//! The profile-picture viewer: one row, two verbs.
//!
//! At a terminal a person usually wants to *look* at the full-size picture,
//! and only sometimes to keep it, so the row takes the story browser's verbs
//! exactly: Enter writes the bytes into the session's scratch directory and
//! hands the file to the system viewer, D saves them here under the name the
//! static command writes. The bytes are already in hand when this opens — the fetch
//! happened before, where the cancel check and the cooldown gate live — so
//! nothing in this loop touches the network.
//!
//! A list of one is still drawn as a list, deliberately: the row is where
//! the picture's size and kind are said, and the frame is the same shape a
//! reader already knows from `stories` and `highlights`.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::widgets::{Cell, Row, TableState};
use snob_core::model::printable;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::app::Viewer;
use crate::commands::pfp::Picture;
use crate::exit::ExitCode;
use crate::output;
use crate::ui::accounts::{self, Browsed, Picked};
use crate::ui::browser::input::{self, Action, TICK, next};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui;

/// Drives the row until the user leaves it, or picks another account to see
/// the picture as. `note` is said on the first frame.
pub(crate) async fn browse(
    picture: &Picture,
    viewer: &Viewer,
    paths: &AppPaths,
    secrets: &SecretStore,
    mut note: String,
) -> Result<Browsed> {
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
    let scratch = Scratch::new(paths.story_scratch())?;

    let mut tui = tui::claim_fullscreen("--no-interactive downloads the picture; -o says where")?;

    let colors = tui::colors_enabled();
    let mut receipts: Vec<String> = Vec::new();
    // Enter twice is one write: the scratch file is kept for the session.
    let mut opened: Option<PathBuf> = None;
    let mut switch_to: Option<Viewer> = None;

    // Every way out passes the receipts: see `tui::print_receipts`.
    let outcome: Result<ExitCode> = async {
        loop {
            tui.terminal
                .draw(|frame| draw(frame, picture, viewer, &note, colors))?;

            let Some(action) = next(TICK).map_err(input::unreadable)? else {
                continue;
            };
            note.clear();

            match action {
                Action::Open => {
                    note = tui::opened_note(open(picture, &scratch, &mut opened));
                }
                Action::Download => {
                    note = tui::saved_note(keep(picture), &mut receipts);
                }
                Action::Redraw => tui.terminal.clear()?,
                Action::Quit => break Ok(ExitCode::Ok),
                Action::Interrupt => break Ok(ExitCode::Interrupted),
                Action::Accounts => {
                    let picked = accounts::pick(&mut tui, secrets, paths, viewer, |frame| {
                        draw(frame, picture, viewer, &note, colors);
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
                // One row: there is nowhere for the selection to move, and
                // Back is not a way out — leaving a view and leaving the
                // program are different intentions, the way `input::Action`'s
                // own doc draws the line, and every other flat view lets
                // Back fall through.
                _ => {}
            }
        }
    }
    .await;

    drop(tui);
    tui::print_receipts(&receipts);
    outcome.map(|code| Browsed::of(code, switch_to))
}

/// Draws the one row inside the shared chrome. The row is always the
/// selection — there is nothing else to select.
fn draw(frame: &mut Frame<'_>, picture: &Picture, viewer: &Viewer, note: &str, colors: bool) {
    let area = frame.area();
    let block = tui::top_right(
        tui::view_block(
            format!("Profile picture · @{}", printable(&picture.username)),
            colors,
            tui::list_padding(area),
        ),
        None,
        tui::viewer_line(viewer, colors),
    )
    .title_bottom(tui::footer(
        note,
        "enter open · d save here · a account · q quit",
        colors,
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut list = TableState::default();
    list.select(Some(0));
    frame.render_stateful_widget(
        tui::list_table(
            [picture_row(picture, colors)],
            [
                Constraint::Length(4),
                Constraint::Length(
                    console::measure_text_width(&picture.source.label()).max(9) as u16
                ),
                Constraint::Length(10),
            ],
        ),
        inner,
        &mut list,
    );
}

/// What the picture is: kind, where it came from, and what a save costs —
/// the cost dim, because it is the aside.
fn picture_row(picture: &Picture, colors: bool) -> Row<'static> {
    let size = format!("({})", human_size(picture.bytes.len()));
    let mut cells = vec![
        Cell::from(picture.extension().to_string()),
        Cell::from(picture.source.label()),
    ];
    cells.push(Cell::from(size).style(tui::paint(colors, tui::DIM)));
    Row::new(cells)
}

/// Writes the picture into the scratch directory and hands it to the system
/// viewer. The file, not the URL, for the story browser's reasons.
pub(crate) fn open(
    picture: &Picture,
    scratch: &Scratch,
    opened: &mut Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(existing) = opened.clone()
        && existing.is_file()
    {
        opener::open(&existing).context("the system viewer would not start")?;
        return Ok(existing);
    }
    let name = output::default_path(scratch.dir(), &picture.username, picture.extension())?;
    let path = scratch.dir().join(name);
    output::create_new(&path)?
        .write_all(&picture.bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    opener::open(&path).context("the system viewer would not start")?;
    *opened = Some(path.clone());
    Ok(path)
}

/// Saves the picture where the user is working — the very name and refusal
/// `snob pfp someone` gives: created, never written over.
pub(crate) fn keep(picture: &Picture) -> Result<PathBuf> {
    let path = output::default_path(
        std::path::Path::new("."),
        &picture.username,
        picture.extension(),
    )?;
    output::create_new(&path)?
        .write_all(&picture.bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// "243 KB" / "1.2 MB": enough to know what a save costs, nothing to audit.
fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn the_size_reads_in_kilobytes_until_a_megabyte() {
        assert_eq!(human_size(1), "1 KB");
        assert_eq!(human_size(243 * 1024), "243 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MB");
    }

    #[test]
    fn the_frame_says_what_the_picture_is_and_how_to_take_it() {
        let picture = Picture::for_tests("someone", vec![0u8; 243 * 1024]);
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| draw(frame, &picture, &tui::me(), "", false))
            .unwrap();
        let row = |y| tui::row_text(&terminal, y);
        assert!(row(0).contains("Profile picture · @someone"));
        assert!(row(0).ends_with(" as @me ╮"));
        assert!(row(1).contains("(243 KB)"));
        assert!(row(1).contains("> "));
        assert!(row(5).contains("d save here"));
    }
}
