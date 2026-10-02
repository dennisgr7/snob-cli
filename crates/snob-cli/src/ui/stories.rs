//! The interactive story list: arrow keys, Enter to look, D to keep.
//!
//! Keys come through `ui::browser::input`, whose header gives the two defects
//! it closes. The guard, the chrome, standard error as the only stream drawn
//! on, and the receipts said after the alternate screen are `ui::tui`'s.
//!
//! What it does **not** do is render the picture in the terminal; AGENTS.md
//! ("Stories are handed to the system viewer") gives the numbers and the
//! argument.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, Row, TableState};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::app::Viewer;
use crate::exit::ExitCode;
use crate::media::{Stories, Story, bytes_of, default_name, extension_of, left_of, posted_of};
use crate::output;
use crate::ui::accounts::{self, Browsed, Picked};
use crate::ui::browser::input::{self, Action, TICK, next, page, watching_cancel_keys};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui;

/// Drives the list until the user leaves it, or picks another account to see
/// it as. `note` is said on the first frame.
///
/// Downloads are made once and kept: moving up and down a list of ten stories
/// and opening three of them twice is three requests to the CDN, not six.
pub async fn browse(
    client: &IgClient,
    stories: &Stories,
    viewer: &Viewer,
    paths: &AppPaths,
    secrets: &SecretStore,
    mut note: String,
) -> Result<Browsed> {
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);

    let scratch = Scratch::new(paths.story_scratch())?;

    let mut tui =
        tui::claim_fullscreen("--no-interactive prints the listing; --download saves without one")?;

    let colors = tui::colors_enabled();
    let mut selected = 0usize;
    let mut list = TableState::default();
    let mut opened: Vec<Option<PathBuf>> = vec![None; stories.items.len()];
    let mut receipts: Vec<String> = Vec::new();
    let mut page_rows = 1usize;
    let mut switch_to: Option<Viewer> = None;

    // Every way out passes the receipts: see `tui::print_receipts`.
    let outcome: Result<ExitCode> = async {
        loop {
            // The selection is this loop's; the state keeps only the scroll
            // offset, which is what makes the list follow the selection with two
            // rows of margin. Layout is against the terminal's *current* size on
            // every draw, so a resize event that was coalesced, delivered late or
            // missed entirely cannot leave the list wrong.
            list.select(Some(selected));
            tui.terminal.draw(|frame| {
                draw(
                    frame,
                    stories,
                    viewer,
                    &mut list,
                    &note,
                    colors,
                    &mut page_rows,
                );
            })?;

            let Some(action) = next(TICK).map_err(input::unreadable)? else {
                continue;
            };
            // A note stays up until the user does something else, not until the
            // next timer tick wipes it.
            note.clear();

            match action {
                Action::Up => selected = selected.saturating_sub(1),
                Action::Down => selected = (selected + 1).min(stories.items.len() - 1),
                Action::PageUp => selected = selected.saturating_sub(page(page_rows)),
                Action::PageDown => {
                    selected = (selected + page(page_rows)).min(stories.items.len() - 1);
                }
                Action::First => selected = 0,
                Action::Last => selected = stories.items.len() - 1,
                Action::Open => {
                    let (result, stopped) = watching_cancel_keys(
                        client.pacer().cancel_token(),
                        open(
                            client,
                            &stories.username,
                            &stories.items,
                            selected,
                            &scratch,
                            &mut opened,
                        ),
                    )
                    .await;
                    note = tui::opened_note(result);
                    if let Some(code) = stopped.leave() {
                        break Ok(code);
                    }
                }
                Action::Download => {
                    let (result, stopped) = watching_cancel_keys(
                        client.pacer().cancel_token(),
                        keep(
                            client,
                            &stories.username,
                            &stories.items,
                            selected,
                            &mut opened,
                        ),
                    )
                    .await;
                    note = tui::saved_note(result, &mut receipts);
                    if let Some(code) = stopped.leave() {
                        break Ok(code);
                    }
                }
                // A flat list has no level to go up to.
                Action::Back => {}
                // For after anything else has written to the terminal behind the
                // renderer's back: a diffing renderer is only ever as right as its
                // belief about what is on screen, and on Windows ConPTY coalesces
                // positioned writes into fragments no renderer can predict.
                Action::Redraw => tui.terminal.clear()?,
                Action::Quit => break Ok(ExitCode::Ok),
                // Raw mode is what makes this reachable. Outside it, Ctrl+C either
                // raises `SIGINT` or fires the console control handler, and the
                // browser never hears about it.
                Action::Interrupt => break Ok(ExitCode::Interrupted),
                Action::Accounts => {
                    let picked = accounts::pick(&mut tui, secrets, paths, viewer, |frame| {
                        draw(
                            frame,
                            stories,
                            viewer,
                            &mut list,
                            &note,
                            colors,
                            &mut page_rows,
                        );
                    })
                    .await?;
                    match picked {
                        Picked::Stay => {}
                        Picked::Switch(to) => {
                            switch_to = Some(to);
                            break Ok(ExitCode::Ok);
                        }
                        Picked::Leave(code) => break Ok(code),
                    }
                }
                Action::None => {}
            }
        }
    }
    .await;

    drop(tui);
    tui::print_receipts(&receipts);
    outcome.map(|code| Browsed::of(code, switch_to))
}

/// Draws one frame: the shared chrome, the table, and the scrollbar when the
/// table does not fit.
///
/// `page_rows` is written with how many rows the list got, which is what Page
/// Up and Page Down move by -- the keyboard arms cannot see the layout, so the
/// draw leaves the one number they need behind.
fn draw(
    frame: &mut Frame<'_>,
    stories: &Stories,
    viewer: &Viewer,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    draw_items_view(
        frame,
        format!(
            "Stories · @{} · {} up",
            printable(&stories.username),
            stories.items.len()
        ),
        "↑↓ move · enter open · d download · a account · q quit",
        &stories.items,
        true,
        viewer,
        list,
        note,
        colors,
        page_rows,
    );
}

/// The hints of a list of items inside something: a highlight folder, and
/// the profile card's stories and folders, which `←` leaves.
pub(crate) const FOLDER_HINT: &str =
    "↑↓ move · enter open · d download · ← back · a account · q quit";

/// The one items view, drawn under whatever title and hints its owner gives
/// it. The story browser, the inside of a highlight folder, and the profile
/// card's sub-views are all this function, so they cannot drift apart.
///
/// `with_left` is what varies: a live story says what is left of it, a kept
/// one only when it was taken.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
pub(crate) fn draw_items_view(
    frame: &mut Frame<'_>,
    title: String,
    hint: &str,
    items: &[Story],
    with_left: bool,
    viewer: &Viewer,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let total = items.len();
    let selected = list.selected().unwrap_or(0);
    let area = frame.area();

    let mut block = tui::view_block(title, colors, tui::list_padding(area));
    let inner = block.inner(area);
    // One row of the interior belongs to the header.
    let viewport = (inner.height as usize).saturating_sub(1).max(1);
    *page_rows = viewport;
    // Only when some of the list is off screen. On a list that fits, a counter
    // is one more thing to read that says nothing.
    let fits = total <= viewport;
    block = tui::top_right(
        block,
        (!fits).then(|| tui::position_line(selected, total, colors)),
        tui::viewer_line(viewer, colors),
    );
    block = block.title_bottom(tui::footer(note, hint, colors));
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), selected, total, viewport, 2);
    let header: &[&str] = if with_left {
        &["", "kind", "posted", "left", "mentions"]
    } else {
        &["", "kind", "taken", "mentions"]
    };
    let rows = items.iter().enumerate().map(|(index, story)| {
        let (when, left) = if with_left {
            (posted_of(story), Some(left_of(story)))
        } else {
            (crate::report::dated(story.taken_at), None)
        };
        item_row(
            index,
            story.kind.label(),
            when,
            left,
            &story.mentions,
            colors,
        )
    });
    frame.render_stateful_widget(
        tui::list_table(rows, story_widths(items, with_left))
            .header(tui::header_row(header, colors)),
        inner,
        list,
    );
    if !fits {
        tui::scrollbar(frame, area, total, selected, viewport as u16);
    }
}

/// The columns a list of story items needs, measured from the items rather
/// than guessed: a `photo` and an `unknown` are different widths, and a date
/// column sized for August is wrong in September. Only `Length` -- the spare
/// width stays unused on the right, keeping a rail on the left instead of
/// scattering three facts across a two-hundred-column terminal.
fn story_widths(items: &[Story], with_left: bool) -> Vec<Constraint> {
    let kind = items
        .iter()
        .map(|s| s.kind.label().len())
        .max()
        .unwrap_or(5) as u16;
    let when = if with_left {
        (items.iter().map(|s| posted_of(s).len()).max().unwrap_or(6) as u16).max(6)
    } else {
        // A kept story's date is `report::dated`, not the story browser's
        // wording, so its column is measured from that.
        items
            .iter()
            .map(|s| crate::report::dated(s.taken_at).len())
            .max()
            .unwrap_or(6)
            .max(5) as u16
    };
    let mut widths = vec![
        Constraint::Length(3),
        Constraint::Length(kind.max(4)),
        Constraint::Length(when),
    ];
    if with_left {
        let left = items.iter().map(|s| left_of(s).len()).max().unwrap_or(4) as u16;
        widths.push(Constraint::Length(left.max(4)));
    }
    widths.push(Constraint::Fill(1));
    widths
}

/// The row of every items view. What varies between the views is only the
/// time columns: a story says
/// when it was posted and what is left of it, a highlight item only when it
/// was taken (`left` is `None` and the column does not exist). The number is
/// dim and right-aligned, what is left dim, the mentions cyan.
fn item_row(
    index: usize,
    kind: &str,
    posted: String,
    left: Option<String>,
    mentions: &[String],
    colors: bool,
) -> Row<'static> {
    let dim = tui::paint(colors, tui::DIM);
    let number = Line::from(format!("{}.", index + 1)).right_aligned();
    let mut cells = vec![
        Cell::from(number.style(dim)),
        Cell::from(kind.to_string()),
        Cell::from(posted),
    ];
    if let Some(left) = left {
        cells.push(Cell::from(left).style(dim));
    }
    if !mentions.is_empty() {
        let joined = mentions
            .iter()
            .map(|m| format!("@{m}"))
            .collect::<Vec<_>>()
            .join(" ");
        cells.push(Cell::from(joined).style(tui::paint(colors, Style::new().fg(Color::Cyan))));
    }
    Row::new(cells)
}

/// Writes the story into the scratch directory and hands it to the system
/// viewer.
///
/// The file, not the URL. Handing the CDN address to a browser puts a signed
/// link to somebody else's story in a browser history — and it opens a
/// browser to look at a picture, which is not what the user asked for.
/// `stem_base` is what comes before the number in the file's name — the
/// username for a story, the username and the highlight's number for a
/// highlight item — so the two browsers write the very names their commands
/// write.
pub(crate) async fn open(
    client: &IgClient,
    stem_base: &str,
    items: &[Story],
    index: usize,
    scratch: &Scratch,
    cache: &mut [Option<PathBuf>],
) -> Result<PathBuf> {
    if let Some(existing) = cache[index].clone() {
        // Still there: a viewer may have been closed, but nothing deletes
        // these until the session ends.
        if existing.is_file() {
            opener::open(&existing).context("the system viewer would not start")?;
            return Ok(existing);
        }
    }

    let bytes = bytes_of(client, &items[index]).await?;
    // Through the same gate as `keep` and `snob stories --download`, and for
    // the same reason: the name came off the server. `printable` strips what
    // a terminal must not draw and leaves everything a path reads -- a `..`,
    // a drive letter, a UNC share -- and `Path::join` hands an absolute name
    // the whole path. `create_new` then refuses a name that already exists,
    // a link included.
    let name = default_name(scratch.dir(), stem_base, index + 1, extension_of(&bytes))?;
    let path = scratch.dir().join(name);
    output::create_new(&path)?
        .write_all(&bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    opener::open(&path).context("the system viewer would not start")?;
    cache[index] = Some(path.clone());
    Ok(path)
}

/// Saves the story where the user is working, rather than in the scratch
/// directory that gets deleted.
pub(crate) async fn keep(
    client: &IgClient,
    stem_base: &str,
    items: &[Story],
    index: usize,
    cache: &mut [Option<PathBuf>],
) -> Result<PathBuf> {
    // Reuse what was already fetched to look at it. Pressing Enter and then D
    // on the same story is one request, not two.
    let bytes = match cache[index].as_ref().filter(|p| p.is_file()) {
        Some(path) => std::fs::read(path)?,
        None => bytes_of(client, &items[index]).await?,
    };

    let path = default_name(Path::new("."), stem_base, index + 1, extension_of(&bytes))?;
    // Created, not written over: the name is one this program invented, and
    // `snob stories --download` refuses to replace a file under such a name,
    // so this key refuses too.
    output::create_new(&path)?
        .write_all(&bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Epoch;

    use super::*;
    use crate::media::Kind;

    fn story(mentions: &[&str]) -> Story {
        Story {
            kind: Kind::Photo,
            taken_at: Epoch::new(1_700_000_000),
            expiring_at: None,
            url: None,
            mentions: mentions.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    fn listing(count: usize) -> Stories {
        Stories {
            username: "someone".into(),
            items: (0..count).map(|_| story(&[])).collect(),
        }
    }

    /// Draws once into a buffer nobody sees, which is what makes a frame a
    /// value somebody can assert on.
    fn rendered(
        stories: &Stories,
        selected: usize,
        note: &str,
        colors: bool,
        size: (u16, u16),
    ) -> (Terminal<TestBackend>, usize) {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
        let mut list = TableState::default();
        list.select(Some(selected));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    stories,
                    &tui::me(),
                    &mut list,
                    note,
                    colors,
                    &mut page_rows,
                );
            })
            .unwrap();
        (terminal, page_rows)
    }

    /// At 50x10 the frame is: border, padding row, header, five data rows,
    /// padding row is absorbed by the bottom border. The columns have names
    /// because a date and a countdown do not explain themselves.
    #[test]
    fn the_frame_says_whose_stories_and_how_to_drive_them() {
        let (terminal, page_rows) = rendered(&listing(3), 0, "", false, (50, 10));
        assert!(tui::row_text(&terminal, 0).contains("Stories · @someone · 3 up"));
        assert!(tui::row_text(&terminal, 0).ends_with(" as @me ╮"));
        let header = tui::row_text(&terminal, 2);
        assert!(header.contains("kind"), "{header}");
        assert!(header.contains("posted"), "{header}");
        let first = tui::row_text(&terminal, 3);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("photo"), "{first}");
        assert!(tui::row_text(&terminal, 9).contains("enter open"));
        // Borders, the padding row and the header leave six rows of page.
        assert_eq!(page_rows, 6);
    }

    #[test]
    fn a_note_takes_the_hints_place() {
        let (terminal, _) = rendered(&listing(3), 0, "Saved ./someone-1.jpg", false, (50, 10));
        let bottom = tui::row_text(&terminal, 9);
        assert!(bottom.contains("Saved ./someone-1.jpg"));
        assert!(!bottom.contains("enter open"));
    }

    #[test]
    fn the_position_appears_only_when_the_list_overflows() {
        let (overflowing, _) = rendered(&listing(20), 4, "", false, (50, 8));
        assert!(tui::row_text(&overflowing, 0).contains("5/20"));
        let (fitting, _) = rendered(&listing(3), 0, "", false, (50, 10));
        assert!(!tui::row_text(&fitting, 0).contains("1/3"));
    }

    #[test]
    fn the_selected_row_is_reverse_video_when_styling_is_on() {
        let (terminal, _) = rendered(&listing(3), 1, "", true, (50, 10));
        let buffer = terminal.backend().buffer();
        // Data rows start at y 3 (border, padding, header); the second story
        // is y 4, and the selection covers the row band.
        assert!(
            buffer[(4, 4)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(
            !buffer[(4, 3)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
    }

    #[test]
    fn a_mention_rides_on_its_story_row() {
        let stories = Stories {
            username: "someone".into(),
            items: vec![story(&["ana", "bob"])],
        };
        // Short terminal: the padding is waived, rows start under the header.
        let (terminal, _) = rendered(&stories, 0, "", false, (60, 6));
        assert!(tui::row_text(&terminal, 2).contains("@ana @bob"));
    }

    /// The widths come off the items: a countdown column is as wide as its
    /// widest countdown, never a guess.
    #[test]
    fn the_columns_are_measured_from_the_items() {
        let widths = story_widths(&listing(2).items, true);
        assert_eq!(widths.len(), 5);
        assert_eq!(widths[1], Constraint::Length(5), "photo is five columns");
        let without = story_widths(&listing(2).items, false);
        assert_eq!(without.len(), 4, "no left column for highlight items");
        let taken = listing(2).items[0].taken_at;
        assert_eq!(
            without[2],
            Constraint::Length(crate::report::dated(taken).len().max(5) as u16),
            "a kept story's date is measured as `report::dated` writes it"
        );
    }
}
