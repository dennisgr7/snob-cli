//! The interactive highlights browser: the tray as a list of folders, and
//! inside each one the story browser's own moves.
//!
//! One loop over two levels rather than two loops, because the terminal
//! machinery — the `Tui` guard and the scratch directory — is per *session*,
//! and leaving a folder must not tear any of it down. Everything
//! terminal-shaped is `ui::tui`'s, and the items view is
//! `stories::draw_items_view`.
//!
//! What the levels change is only what a row is and what the three verbs do.
//! At the tray a row is a folder: Enter walks in, D keeps everything in it,
//! and the items arrive on the first walk-in and are kept for the session —
//! reopening a folder is free, like reopening a story. Inside, a row is a
//! story in all but expiry: Enter hands it to the system viewer, D keeps it
//! under the very name `snob highlights someone 2 -d 3` would write, and
//! Left or Backspace walks back out with the folder's selection remembered.
//!
//! Fetching inside the loop blocks the keys, deliberately: the story browser
//! already sits on the CDN for megabytes between keystrokes, one `reels_media`
//! request is smaller than that, and a browser that queues keystrokes against
//! a request in flight answers them against a list the user cannot see yet.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::text::Line;
use ratatui::widgets::{Cell, Row, TableState};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::app::Viewer;
use crate::commands::highlights::{Entry, Tray, items_of_entry};
use crate::exit::ExitCode;
use crate::media::{Saved, Story, save_story};
use crate::report;
use crate::ui::accounts::{self, Browsed, Picked};
use crate::ui::browser::input::{self, Action, TICK, next, page, watching_cancel_keys};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui::{self, Tui};

/// Where the browser is, and what the arrow keys therefore move.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Tray,
    /// Inside one entry, by its index into the tray.
    Inside(usize),
}

/// Everything remembered about one entry across the session.
#[derive(Default)]
pub(crate) struct Folder {
    /// `None` until first opened; kept after, so walking out and back in
    /// costs nothing.
    pub(crate) items: Option<Vec<Story>>,
    /// What the system viewer was handed, per item, so Enter twice is one
    /// request. Sized with `items`.
    pub(crate) opened: Vec<Option<PathBuf>>,
    /// The row the selection was on when the user walked out.
    pub(crate) selected: usize,
}

impl Folder {
    /// One unopened folder per entry of the tray.
    pub(crate) fn for_tray(tray: &Tray) -> Vec<Self> {
        tray.entries.iter().map(|_| Self::default()).collect()
    }
}

/// Drives the two lists until the user leaves them, or picks another account
/// to see them as. `note` is said on the first frame.
///
/// `start` is an entry to open before the first frame — `snob highlights
/// someone 2 -i` — already checked against the tray by the caller.
pub async fn browse(
    client: &IgClient,
    tray: &Tray,
    start: Option<usize>,
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
    let mut folders = Folder::for_tray(tray);
    let mut level = Level::Tray;
    let mut tray_selected = start.unwrap_or(0);
    let mut receipts: Vec<String> = Vec::new();
    let mut list = TableState::default();
    let mut page_rows = 1usize;
    let mut switch_to: Option<Viewer> = None;

    // `-i` on a numbered entry opens it before the first frame, and a folder
    // that cannot be opened leaves the user at the tray with the reason in
    // the note line rather than exiting an interface that just appeared.
    if let Some(index) = start {
        match walk_in(client, tray, index, &mut folders).await {
            Ok(true) => level = Level::Inside(index),
            Ok(false) => note = empty_note(index),
            Err(e) => note = format!("Could not open it: {e}"),
        }
    }

    // Every way out passes the receipts: see `tui::print_receipts`.
    let outcome: Result<ExitCode> = async {
        loop {
            let total = match level {
                Level::Tray => tray.entries.len(),
                Level::Inside(index) => folders[index].items.as_ref().map_or(0, Vec::len),
            };
            // A copy, moved by the arms below and written back only while the
            // level it belongs to is still the one on screen -- walking into a
            // folder must not write the tray's row over the folder's.
            let mut selected = match level {
                Level::Tray => tray_selected,
                Level::Inside(index) => folders[index].selected,
            };

            list.select(Some(selected));
            tui.terminal.draw(|frame| match level {
                Level::Tray => {
                    draw_tray(
                        frame,
                        tray,
                        &folders,
                        viewer,
                        &mut list,
                        &note,
                        colors,
                        &mut page_rows,
                    );
                }
                Level::Inside(index) => {
                    draw_items(
                        frame,
                        tray,
                        index,
                        &folders[index],
                        viewer,
                        &mut list,
                        &note,
                        colors,
                        &mut page_rows,
                    );
                }
            })?;

            let Some(action) = next(TICK).map_err(input::unreadable)? else {
                continue;
            };
            note.clear();

            let level_before = level;
            match action {
                Action::Up => selected = selected.saturating_sub(1),
                Action::Down => selected = (selected + 1).min(total.saturating_sub(1)),
                Action::PageUp => selected = selected.saturating_sub(page(page_rows)),
                Action::PageDown => {
                    selected = (selected + page(page_rows)).min(total.saturating_sub(1));
                }
                Action::First => selected = 0,
                Action::Last => selected = total.saturating_sub(1),
                Action::Open => match level {
                    Level::Tray => {
                        let (result, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            walk_in(client, tray, selected, &mut folders),
                        )
                        .await;
                        match result {
                            Ok(true) => level = Level::Inside(selected),
                            Ok(false) => note = empty_note(selected),
                            Err(e) => note = format!("Could not open it: {e}"),
                        }
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                    Level::Inside(index) => {
                        let folder = &mut folders[index];
                        let items = folder.items.as_deref().unwrap_or_default();
                        let (result, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            crate::ui::stories::open(
                                client,
                                &stem_of(tray, index),
                                items,
                                selected,
                                &scratch,
                                &mut folder.opened,
                            ),
                        )
                        .await;
                        note = tui::opened_note(result);
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                },
                Action::Download => match level {
                    Level::Tray => {
                        let (folder_note, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            keep_folder(
                                client,
                                &mut tui,
                                tray,
                                selected,
                                &mut folders,
                                viewer,
                                &mut receipts,
                            ),
                        )
                        .await;
                        note = folder_note;
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                    Level::Inside(index) => {
                        let folder = &mut folders[index];
                        let items = folder.items.as_deref().unwrap_or_default();
                        let (result, stopped) = watching_cancel_keys(
                            client.pacer().cancel_token(),
                            crate::ui::stories::keep(
                                client,
                                &stem_of(tray, index),
                                items,
                                selected,
                                &mut folder.opened,
                            ),
                        )
                        .await;
                        note = tui::saved_note(result, &mut receipts);
                        if let Some(code) = stopped.leave() {
                            break Ok(code);
                        }
                    }
                },
                Action::Back => {
                    // At the tray there is nowhere further out that is not
                    // leaving, and leaving is q's job alone.
                    if let Level::Inside(_) = level {
                        level = Level::Tray;
                    }
                }
                Action::Redraw => tui.terminal.clear()?,
                Action::Quit => break Ok(ExitCode::Ok),
                Action::Interrupt => break Ok(ExitCode::Interrupted),
                Action::Accounts => {
                    let picked =
                        accounts::pick(&mut tui, secrets, paths, viewer, |frame| match level {
                            Level::Tray => draw_tray(
                                frame,
                                tray,
                                &folders,
                                viewer,
                                &mut list,
                                &note,
                                colors,
                                &mut page_rows,
                            ),
                            Level::Inside(index) => draw_items(
                                frame,
                                tray,
                                index,
                                &folders[index],
                                viewer,
                                &mut list,
                                &note,
                                colors,
                                &mut page_rows,
                            ),
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
            // The write-back. Skipped when the arm changed levels: `selected`
            // still belongs to the level the keys were read at. The scroll offset
            // starts over with the level, and the first draw pulls the remembered
            // selection back into view.
            if level == level_before {
                match level {
                    Level::Tray => tray_selected = selected,
                    Level::Inside(index) => folders[index].selected = selected,
                }
            } else {
                list = TableState::default();
            }
        }
    }
    .await;

    drop(tui);
    tui::print_receipts(&receipts);
    outcome.map(|code| Browsed::of(code, switch_to))
}

/// `someone-2` for the entry at `index`: the command's own stem, so the
/// browser and `-d` write the very same names. The command counts from one
/// and the views from zero, and this is where that difference is written.
pub(crate) fn stem_of(tray: &Tray, index: usize) -> String {
    crate::commands::highlights::stem_of(tray, index + 1)
}

/// Fetches an entry's items on first opening; the session keeps them after.
///
/// `Ok(false)` is a folder with nothing in it — deleted since the tray was
/// fetched, or genuinely empty — which is not worth walking into.
pub(crate) async fn walk_in(
    client: &IgClient,
    tray: &Tray,
    index: usize,
    folders: &mut [Folder],
) -> Result<bool> {
    if folders[index].items.is_none() {
        let items = items_of_entry(client, tray, &tray.entries[index]).await?;
        folders[index].opened = vec![None; items.len()];
        folders[index].selected = 0;
        folders[index].items = Some(items);
    }
    Ok(folders[index]
        .items
        .as_ref()
        .is_some_and(|items| !items.is_empty()))
}

/// D on a folder: everything in it, into the working directory, one at a
/// time.
///
/// One at a time rather than through `download_many`, which reports each
/// file on standard error as it lands — lines that would tear the frame this
/// browser is holding. Between items this draws its own frame with the
/// running tally, because a folder of forty items is minutes, and a screen
/// frozen on its last frame reads as a hang. The note line gets the final
/// tally; when anything was saved, the tally also joins the receipts,
/// because the alternate screen takes the note away on exit.
pub(crate) async fn keep_folder(
    client: &IgClient,
    tui: &mut Tui,
    tray: &Tray,
    index: usize,
    folders: &mut [Folder],
    viewer: &Viewer,
    receipts: &mut Vec<String>,
) -> String {
    match walk_in(client, tray, index, folders).await {
        Ok(true) => {}
        Ok(false) => return empty_note(index),
        Err(e) => return format!("Could not fetch it: {e}"),
    }
    let items = folders[index].items.as_deref().unwrap_or_default();
    let stem = stem_of(tray, index);
    let total = items.len();
    let mut kept = 0usize;
    let mut failed = 0usize;
    let mut stopped = false;
    for number in 1..=total {
        // The watcher wrapping this call cancels the pacer token on q and
        // Ctrl+C; read here, it turns "every remaining item fails fast" into
        // an honest early stop that keeps what already landed.
        if client.pacer().cancel_token().is_canceled() {
            stopped = true;
            break;
        }
        draw_saving(tui, &tray.username, viewer, index, number, total);
        match save_story(client, &stem, items, number, std::path::Path::new(".")).await {
            Ok(Saved::Now(_) | Saved::Already(_)) => kept += 1,
            Err(_) => failed += 1,
        }
    }
    let note = if stopped {
        format!(
            "Stopped after {kept} of {total} from highlight {}",
            index + 1
        )
    } else if failed == 0 {
        format!("Saved {kept} of highlight {} here", index + 1)
    } else {
        format!(
            "Saved {kept} of {total} from highlight {}; {failed} failed",
            index + 1
        )
    };
    if kept > 0 {
        receipts.push(note.clone());
    }
    note
}

/// One frame between downloads: the chrome with the count moving on the
/// bottom edge.
///
/// Drawn by the loop itself rather than by the view, because the view's own
/// frame reads the folders the loop holds mutably. A draw failure is
/// ignored on purpose — the save is the job, the frame is the report — and
/// the caller's next ordinary frame repaints whatever this left behind.
fn draw_saving(
    tui: &mut Tui,
    username: &str,
    viewer: &Viewer,
    index: usize,
    number: usize,
    total: usize,
) {
    let colors = tui::colors_enabled();
    let _ = tui.terminal.draw(|frame| {
        let area = frame.area();
        let block = tui::top_right(
            tui::view_block(
                format!("Highlights · @{}", printable(username)),
                colors,
                tui::list_padding(area),
            ),
            None,
            tui::viewer_line(viewer, colors),
        )
        .title_bottom(tui::outcome_line(
            format!(
                "Saving {number} of {total} from highlight {} · q stops",
                index + 1
            ),
            colors,
        ));
        frame.render_widget(block, area);
    });
}

/// The note for a folder with nothing to show.
pub(crate) fn empty_note(index: usize) -> String {
    format!("Highlight {} is empty", index + 1)
}

/// Draws the tray: folders in named columns, because a count and a date do
/// not explain themselves the way a title does.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_tray(
    frame: &mut Frame<'_>,
    tray: &Tray,
    folders: &[Folder],
    viewer: &Viewer,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let total = tray.entries.len();
    let selected = list.selected().unwrap_or(0);
    let area = frame.area();

    let mut block = tui::view_block(
        format!("Highlights · @{} · {total} kept", printable(&tray.username)),
        colors,
        tui::list_padding(area),
    );
    let inner = block.inner(area);
    // One row of the interior belongs to the header.
    let viewport = (inner.height as usize).saturating_sub(1).max(1);
    *page_rows = viewport;
    let fits = total <= viewport;
    block = tui::top_right(
        block,
        (!fits).then(|| tui::position_line(selected, total, colors)),
        tui::viewer_line(viewer, colors),
    );
    block = block.title_bottom(tui::footer(
        note,
        "↑↓ move · enter open · d save all of it · a account · q quit",
        colors,
    ));
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), selected, total, viewport, 2);
    // Never narrower than its own header, or the word "highlight" clips.
    let title_w = tray
        .entries
        .iter()
        .map(|e| console::measure_text_width(&e.title))
        .max()
        .unwrap_or(1)
        .clamp(9, 28) as u16;
    frame.render_stateful_widget(
        tui::list_table(
            tray.entries
                .iter()
                .zip(folders)
                .enumerate()
                .map(|(index, (entry, folder))| tray_row(index, entry, folder, colors)),
            [
                Constraint::Length(3),
                Constraint::Length(title_w),
                Constraint::Length(8),
                Constraint::Length(14),
            ],
        )
        .header(tui::header_row(
            &["", "highlight", "items", "updated"],
            colors,
        )),
        inner,
        list,
    );
    if !fits {
        tui::scrollbar(frame, area, total, selected, viewport as u16);
    }
}

/// Draws one folder's items, under the folder's own name, through the one
/// items view in `ui::stories` — minus the column a highlight item does not
/// have: a kept story does not expire, so there is no `left`.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_items(
    frame: &mut Frame<'_>,
    tray: &Tray,
    index: usize,
    folder: &Folder,
    viewer: &Viewer,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let items = folder.items.as_deref().unwrap_or_default();
    let entry = &tray.entries[index];
    let name = if entry.title.is_empty() {
        format!("highlight {}", index + 1)
    } else {
        format!("\"{}\"", entry.title)
    };
    crate::ui::stories::draw_items_view(
        frame,
        format!(
            "@{} · {name} · {} {}",
            printable(&tray.username),
            items.len(),
            if items.len() == 1 { "item" } else { "items" }
        ),
        crate::ui::stories::FOLDER_HINT,
        items,
        false,
        viewer,
        list,
        note,
        colors,
        page_rows,
    );
}

/// One folder as a table row: number dim, title, the best count known
/// right-aligned under its name, and when it last grew, dim as an aside.
///
/// The count the session has seen beats the count the tray declared, and
/// the tray's number is honestly a declaration until the folder has been
/// opened.
fn tray_row(index: usize, entry: &Entry, folder: &Folder, colors: bool) -> Row<'static> {
    let dim = tui::paint(colors, tui::DIM);
    let count = match folder.items.as_ref() {
        Some(items) => items.len().to_string(),
        None => match entry.declared_items {
            Some(n) => n.to_string(),
            None => "-".to_string(),
        },
    };
    let updated = entry.updated_at.map(report::dated).unwrap_or_default();
    let number = Line::from(format!("{}.", index + 1)).right_aligned();
    let title = if entry.title.is_empty() {
        "-".to_string()
    } else {
        entry.title.clone()
    };
    let count = Line::from(count).right_aligned();
    Row::new(vec![
        Cell::from(number.style(dim)),
        Cell::from(title),
        Cell::from(count),
        Cell::from(updated).style(dim),
    ])
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Epoch;

    use super::*;
    use crate::media::Kind;

    fn entry(title: &str, declared: Option<u64>) -> Entry {
        Entry {
            id: "highlight:1".into(),
            title: title.into(),
            declared_items: declared,
            created_at: None,
            updated_at: None,
        }
    }

    fn folder(items: Option<usize>) -> Folder {
        Folder {
            items: items.map(|n| {
                (0..n)
                    .map(|_| Story {
                        kind: Kind::Photo,
                        taken_at: Epoch::new(1_700_000_000),
                        expiring_at: None,
                        url: None,
                        mentions: Vec::new(),
                    })
                    .collect()
            }),
            ..Folder::default()
        }
    }

    /// At 60x8 the padding is waived: border, header, data rows. The count
    /// column carries the number the session has seen, not the declaration.
    #[test]
    fn the_tray_prefers_the_count_the_session_has_seen() {
        let tray = Tray {
            username: "someone".into(),
            entries: vec![entry("trip", Some(9)), entry("", Some(3))],
        };
        let folders = vec![folder(Some(2)), folder(None)];
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw_tray(
                    frame,
                    &tray,
                    &folders,
                    &tui::me(),
                    &mut list,
                    "",
                    false,
                    &mut page_rows,
                );
            })
            .unwrap();
        assert!(tui::row_text(&terminal, 0).contains("Highlights · @someone · 2 kept"));
        assert!(tui::row_text(&terminal, 0).ends_with(" as @me ╮"));
        let header = tui::row_text(&terminal, 1);
        assert!(header.contains("highlight"), "{header}");
        assert!(header.contains("items"), "{header}");
        // Opened once this session: the real count, not the declaration.
        let first = tui::row_text(&terminal, 2);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("trip"), "{first}");
        assert!(first.contains('2'), "{first}");
        // Never opened, no title: the declaration and a dash.
        let second = tui::row_text(&terminal, 3);
        assert!(second.contains("2."), "{second}");
        assert!(second.contains('-'), "{second}");
        assert!(second.contains('3'), "{second}");
        assert!(tui::row_text(&terminal, 7).contains("save all of it"));
    }

    #[test]
    fn a_folder_draws_under_its_own_name() {
        let tray = Tray {
            username: "someone".into(),
            entries: vec![entry("trip", Some(1))],
        };
        let opened = folder(Some(1));
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw_items(
                    frame,
                    &tray,
                    0,
                    &opened,
                    &tui::me(),
                    &mut list,
                    "",
                    false,
                    &mut page_rows,
                );
            })
            .unwrap();
        assert!(tui::row_text(&terminal, 0).contains("@someone · \"trip\" · 1 item"));
        assert!(tui::row_text(&terminal, 0).ends_with(" as @me ╮"));
        assert!(
            tui::row_text(&terminal, 1).contains("taken"),
            "the header names it"
        );
        let first = tui::row_text(&terminal, 2);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("photo"), "{first}");
        assert!(tui::row_text(&terminal, 7).contains("← back"));
    }
}
