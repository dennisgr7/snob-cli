//! The keys of the view on screen, in a box over it.
//!
//! A view's bottom edge is one row, and what does not fit on it is cut. The
//! hints there name the keys a person needs first; the rest, `g` and `G`, a
//! page at a time, `h j k l`, Ctrl+L, were known to whoever had read
//! `browser::input`. `?` puts all of them on screen, for the view and the
//! level it is pressed at, and any key takes them away again.
//!
//! **A table sits beside the binding it describes.** Each view keeps its own
//! (`KEYS`), built from the entries below that mean the same everywhere and
//! from its own. An entry carries one of the keys it names, so a test in the
//! view can press it and hold the table to what the view really does: a key
//! named here that does nothing is a failing test, not a stale sentence.

use anyhow::Result;
use console::measure_text_width;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Row, Table};

use crate::ui::browser::input::{self, Raw};
use crate::ui::tui::{self, Tui};

/// One row of the box: the keys as they are named, and what they do.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub keys: &'static str,
    pub does: &'static str,
    /// One of the keys named, for the tests that press it.
    pub press: KeyEvent,
}

/// An entry whose key is pressed with nothing held.
pub const fn key(keys: &'static str, does: &'static str, press: KeyCode) -> Key {
    Key {
        keys,
        does,
        press: KeyEvent::new(press, KeyModifiers::NONE),
    }
}

/// An entry whose key is a letter with Ctrl held.
pub const fn ctrl(keys: &'static str, does: &'static str, letter: char) -> Key {
    Key {
        keys,
        does,
        press: KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL),
    }
}

// The entries that mean the same in every view, so that no table retypes
// them and no two views word them differently.
pub const MOVE: Key = key("↑ ↓ j k", "move", KeyCode::Down);
pub const PAGE: Key = key("pgup pgdn", "move a page", KeyCode::PageDown);
pub const ENDS: Key = key("g G home end", "the first, the last", KeyCode::Home);
// Esc with them: wherever a level has one above it, Esc walks out to it
// and only `q` leaves (`highlights::bound_at`, `people::moving`, the card).
pub const BACK: Key = key("esc ← h backspace", "back", KeyCode::Left);
pub const ACCOUNT: Key = key("a", "see it as another account", KeyCode::Char('a'));
pub const WEB: Key = key("w", "open the profile on the web", KeyCode::Char('w'));
pub const REDRAW: Key = ctrl("ctrl+l", "draw the screen again", 'l');
pub const QUIT: Key = key("q", "quit", KeyCode::Char('q'));
pub const STOP: Key = ctrl("ctrl+c", "stop and leave", 'c');

/// Shows `keys` over the view `under` draws until a key is pressed.
///
/// Any key closes it and is spent doing so: the box is in the way of what
/// the key would have acted on. A stop the system asked for is read as a
/// key like any other here, and as Ctrl+C again by the view's own loop.
pub fn show(
    tui: &mut Tui,
    title: &str,
    keys: &[Key],
    mut under: impl FnMut(&mut Frame<'_>),
) -> Result<()> {
    let colors = tui::colors_enabled();
    loop {
        tui.terminal.draw(|frame| {
            under(frame);
            modal(frame, title, keys, colors);
        })?;
        if let Raw::Key(_) = input::read(input::tick()).map_err(input::unreadable)? {
            return Ok(());
        }
    }
}

/// The box: one row a key, the keys on the left in the accent, bold, and
/// what they do beside them. On a terminal too short for all of it the last
/// rows are cut, which is the first rows' good luck: they are the ones every
/// view puts first.
pub fn modal(frame: &mut Frame<'_>, title: &str, keys: &[Key], colors: bool) {
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    let key = tui::paint(colors, tui::accent_bold());
    tui::veil(frame, colors);

    let keys_w = keys
        .iter()
        .map(|k| measure_text_width(k.keys))
        .max()
        .unwrap_or(0) as u16;
    let does_w = keys
        .iter()
        .map(|k| measure_text_width(k.does))
        .max()
        .unwrap_or(0) as u16;
    // Borders, the card's padding each side, and the gap between columns.
    let width = (2 + 4 + keys_w + 3 + does_w).max(30);
    let height = 2 + 2 + keys.len() as u16;
    let area = tui::modal_area(frame.area(), width, height);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(tui::card_padding())
        .title_top(Line::from(Span::styled(format!(" Keys · {title} "), bold)))
        .title_bottom(Line::from(Span::styled(" any key closes ", dim)).centered());
    let inner = tui::open_modal(frame, area, block);
    let rows = keys.iter().map(|k| {
        Row::new(vec![
            Cell::from(Span::styled(k.keys, key)),
            Cell::from(k.does),
        ])
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(keys_w), Constraint::Fill(1)]).column_spacing(3),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::ui::browser::input::{Action, action_of};

    const SOME: &[Key] = &[MOVE, PAGE, ENDS, BACK, ACCOUNT, WEB, REDRAW, QUIT, STOP];

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let height = terminal.backend().buffer().area.height;
        (0..height)
            .map(|y| tui::row_text(terminal, y) + "\n")
            .collect()
    }

    /// The box names the view, every key it was given and how to close it.
    #[test]
    fn the_box_names_every_key_and_how_to_close_it() {
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal
            .draw(|frame| modal(frame, "Stories", SOME, false))
            .unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains("Keys · Stories"), "{screen}");
        assert!(screen.contains("any key closes"), "{screen}");
        for key in SOME {
            assert!(screen.contains(key.keys), "{}: {screen}", key.keys);
            assert!(screen.contains(key.does), "{}: {screen}", key.does);
        }
    }

    /// A terminal too small for the table draws what fits and nothing else
    /// happens: the first keys are still there.
    #[test]
    fn a_short_terminal_shows_the_first_keys() {
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        terminal
            .draw(|frame| modal(frame, "Stories", SOME, false))
            .unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains(MOVE.does), "{screen}");
        assert!(!screen.contains(STOP.does), "{screen}");
    }

    /// The shared entries are bound by the shared binding, each to
    /// something.
    #[test]
    fn every_shared_key_the_help_names_does_something() {
        for key in SOME {
            assert_ne!(action_of(key.press), Action::None, "{}", key.keys);
        }
    }
}
