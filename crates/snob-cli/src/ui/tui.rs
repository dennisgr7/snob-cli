//! The terminal guard every interactive view draws through.
//!
//! One `Tui` is one claim on the terminal: raw mode so keys arrive one at a
//! time, bracketed paste so a pasted `q` is text and not a command, the cursor
//! hidden, and -- for the browsers -- the alternate screen. The claim is made
//! in the constructor and undone in `Drop`, so an early `return` cannot leave
//! a shell in raw mode with no cursor. The two exits that run no destructors,
//! the panic hook and the forced quit, go through `ui::restore_terminal`,
//! which undoes the same four things unconditionally.
//!
//! Everything is drawn on **standard error**. Standard output belongs to the
//! listing: `snob stories someone -i > out.txt` must leave the file empty, the
//! same contract every printed form keeps.
//!
//! The browsers take the **alternate screen**, so leaving one takes the last
//! frame with it. What the user came for must therefore be said again on the
//! way out: each browser collects receipt lines -- `Saved ./someone-3.jpg`,
//! `Opened https://...` -- and hands them to [`print_receipts`] after its
//! guard has dropped, which puts them on the real screen where the shell
//! prompt lands. The menu stays inline instead ([`Tui::inline`]): it lives in
//! the middle of a wizard's questions, and three rows of alternate screen
//! around a two-entry choice would be theater.
//!
//! There is never more than one `Tui`. The profile card reaches the people
//! browser by lending its own guard (`people::browse_in` takes `&mut Tui`)
//! rather than letting a second one be built, and [`Tui::suspend`] /
//! [`Tui::resume`] are how the card steps aside for indicatif's progress bars:
//! the walk draws on the real screen, its finished bars stay in the user's
//! scrollback where receipts belong, and `resume` repaints from nothing --
//! after somebody else has written to the terminal, a renderer's belief about
//! what is on screen is wrong, and `Terminal::clear` is how it says so.

use std::borrow::Cow;
use std::io::{Stderr, stderr};
use std::path::PathBuf;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, Padding, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table,
};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::app::Viewer;

/// The claim on the terminal, and the `ratatui` terminal that draws under it.
pub struct Tui {
    pub terminal: Terminal<CrosstermBackend<Stderr>>,
    /// Whether this guard took the alternate screen, so `suspend` knows
    /// whether there is one to leave.
    fullscreen: bool,
    /// False while suspended, so `Drop` after `suspend` undoes nothing twice
    /// and a failed `resume` does not leave `Drop` believing modes are on.
    active: bool,
}

impl Tui {
    /// Raw mode, the alternate screen, bracketed paste, focus reporting,
    /// cursor hidden. For the browsers.
    ///
    /// Focus reporting is what tells a view whether anybody is looking
    /// (`browser::input::focused`), and it is handed back with the rest on
    /// [`Tui::suspend`]: left on under a cooked terminal, the terminal's
    /// reports would be typed into the shell.
    pub fn fullscreen() -> std::io::Result<Self> {
        Self::enter(true, Viewport::Fullscreen)
    }

    /// The same claim without the alternate screen, drawing in `height` rows
    /// at the cursor. For the menu, which lives between a wizard's questions.
    pub fn inline(height: u16) -> std::io::Result<Self> {
        Self::enter(false, Viewport::Inline(height))
    }

    fn enter(fullscreen: bool, viewport: Viewport) -> std::io::Result<Self> {
        enable_raw_mode()?;
        let modes = if fullscreen {
            crossterm::execute!(
                stderr(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange,
                Hide
            )
        } else {
            crossterm::execute!(stderr(), EnableBracketedPaste, EnableFocusChange, Hide)
        };
        let terminal = modes.and_then(|()| {
            Terminal::with_options(
                CrosstermBackend::new(stderr()),
                TerminalOptions { viewport },
            )
        });
        match terminal {
            Ok(terminal) => Ok(Self {
                terminal,
                fullscreen,
                active: true,
            }),
            // Raw mode is already on and half the modes may be too; undo the
            // whole claim rather than leave a terminal nobody owns.
            Err(e) => {
                crate::ui::restore_terminal();
                Err(e)
            }
        }
    }

    /// Hands the terminal back -- cooked, real screen, cursor shown -- so
    /// somebody else may write to it. Idempotent.
    ///
    /// `active` moves only on success: cleared first, an escape write that
    /// failed would return before `disable_raw_mode` with the flag already
    /// saying "nothing to undo", and `Drop`'s own `suspend` would then take
    /// the early exit, handing the shell back still in raw mode.
    /// Raw mode is also dropped whatever became of the escape writes: it is
    /// the one mode that makes the shell unusable, and a write that failed
    /// is no reason to keep it.
    pub fn suspend(&mut self) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let modes = if self.fullscreen {
            crossterm::execute!(
                stderr(),
                DisableFocusChange,
                DisableBracketedPaste,
                LeaveAlternateScreen,
                Show
            )
        } else {
            crossterm::execute!(stderr(), DisableFocusChange, DisableBracketedPaste, Show)
        };
        let raw = disable_raw_mode();
        // Back to a command in a terminal, which somebody waits on, whatever
        // the view's focus last said (`owner::attended`).
        crate::owner::attended(true);
        crate::ui::browser::input::focus_unknown();
        modes.and(raw)?;
        self.active = false;
        Ok(())
    }

    /// Takes the claim back and repaints from nothing: whatever was drawn
    /// while suspended, the renderer's belief about the screen is wrong now.
    pub fn resume(&mut self) -> std::io::Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        let modes = if self.fullscreen {
            crossterm::execute!(
                stderr(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableFocusChange,
                Hide
            )
        } else {
            crossterm::execute!(stderr(), EnableBracketedPaste, EnableFocusChange, Hide)
        };
        if let Err(e) = modes {
            // Raw mode is on and half the modes may be too. `active` is still
            // false, so `Drop` would undo nothing: hand the whole terminal
            // back now, the same recovery `enter` runs.
            crate::ui::restore_terminal();
            return Err(e);
        }
        self.active = true;
        self.terminal.clear()
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

/// The whole entry, shared by every browser: the terminal check and the
/// claim, each refusal carrying the view's own hint about the flags that
/// work without one.
///
/// The check reads standard error, like everything the views draw; standard
/// output is deliberately not consulted, because the listing it would
/// protect is exactly what the browser withholds.
pub fn claim_fullscreen(hint: &str) -> anyhow::Result<Tui> {
    use crate::exit::{ExitCode, ExitError};

    if !console::Term::stderr().is_term() {
        return Err(
            ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
                .with_hint(hint)
                .into(),
        );
    }
    // The whole claim on the terminal -- raw mode, alternate screen, paste,
    // cursor -- in one guard, so an early return cannot leave any of it on.
    Ok(Tui::fullscreen().map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("the terminal would not go into raw mode: {e}"),
        )
        .with_hint(hint)
    })?)
}

/// The chrome's two modifiers: dim for asides, bold for what a row is about.
pub const DIM: Style = Style::new().add_modifier(Modifier::DIM);
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

/// `style` where the views may style, no style at all where they may not.
pub fn paint(colors: bool, style: Style) -> Style {
    if colors { style } else { Style::new() }
}

/// A line in `style`, or the plain text. The plain form is `Line::raw`,
/// which splits at newlines -- a note built from an error's text may carry
/// one -- so it is kept apart from one span with an empty style.
pub fn styled_line<'a>(text: impl Into<Cow<'a, str>>, style: Style, colors: bool) -> Line<'a> {
    if colors {
        Line::from(Span::styled(text, style))
    } else {
        Line::raw(text)
    }
}

/// Whether the views may style at all. `console` decides -- `NO_COLOR`,
/// `TERM`, whether anyone is attending -- so the browsers agree with every
/// other line this program prints. When it says no, the textual markers
/// (`> `, `‹`, `›`) are the whole interface.
pub fn colors_enabled() -> bool {
    console::Term::stderr().features().colors_supported()
}

/// Says on the real screen what the alternate screen took away.
///
/// Called after the guard has dropped, never before: printed under a live
/// viewport these would land inside the frame, and printed inside the
/// alternate screen they would vanish with it. Stderr, like everything the
/// views draw, and tolerant of it being gone.
///
/// Each view runs its loop inside a block so that every way out -- a draw
/// that fails included -- passes through the guard's drop and then here: an
/// error returned straight through `?` would take every `Saved ...` line
/// down with the alternate screen.
pub fn print_receipts(lines: &[String]) {
    let term = console::Term::stderr();
    for line in lines {
        let _ = term.write_line(line);
    }
}

/// The frame every view sits in: rounded corners, the view's name bold on the
/// top edge, and the breathing room its content asked for.
///
/// No color is ever named for selection or for the border -- the selected row
/// is reverse video and the chrome is dim/bold, so the user's own palette
/// decides what everything looks like. Accents elsewhere stay within the 16
/// ANSI colors for the same reason. And no italics anywhere: several
/// terminals -- conhost among them -- render italic as reverse video, which
/// would collide with the one meaning reverse video has here.
pub fn view_block(title: String, colors: bool, padding: Padding) -> Block<'static> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(padding);
    if colors {
        block
            .border_style(DIM)
            .title_top(Line::from(Span::styled(format!(" {title} "), BOLD)))
    } else {
        block.title_top(Line::from(format!(" {title} ")))
    }
}

/// The breathing room a list gets: one blank row under the border, one cell
/// on the left so rows do not touch the frame. Waived on a terminal too
/// short to give rows away to air.
pub fn list_padding(area: Rect) -> Padding {
    if area.height < 10 {
        Padding::ZERO
    } else {
        Padding::new(1, 1, 1, 0)
    }
}

/// The card's padding: `proportional` because a terminal cell is twice as
/// tall as it is wide, so equal visual margins are 2 columns and 1 row.
pub fn card_padding() -> Padding {
    Padding::proportional(1)
}

/// Where the selection sits in a list that does not fit, for the top-right of
/// the border. A list that fits gets nothing: a counter on it is one more
/// thing to read that says nothing.
pub fn position_line(selected: usize, total: usize, colors: bool) -> Line<'static> {
    styled_line(format!(" {}/{total} ", selected + 1), DIM, colors).right_aligned()
}

/// The account a view acts as, dim, for the top-right of the border.
pub fn viewer_line(viewer: &Viewer, colors: bool) -> Line<'static> {
    styled_line(format!(" as {} ", viewer.label()), DIM, colors).right_aligned()
}

/// The top-right of a view's border: the position when there is one, then
/// the account in the corner. ratatui lays right-aligned titles out in the
/// order they were added, the last one in the corner and the first one
/// cut short when the row runs out, so the account is added last.
pub fn top_right<'a>(block: Block<'a>, position: Option<Line<'a>>, viewer: Line<'a>) -> Block<'a> {
    match position {
        Some(position) => block.title_top(position),
        None => block,
    }
    .title_top(viewer)
}

/// The key hints, dim on the bottom edge.
pub fn hint_line(text: String, colors: bool) -> Line<'static> {
    styled_line(format!(" {text} "), DIM, colors)
}

/// What just happened -- `Saved ./x.jpg`, `Could not open it` -- in the
/// hints' place. Yellow because it must win against a row of dim text, and
/// yellow is an ANSI color every theme has already decided how to show.
pub fn note_line(text: String, colors: bool) -> Line<'static> {
    styled_line(format!(" {text} "), Style::new().fg(Color::Yellow), colors)
}

/// What went wrong -- `Could not open it: ...` -- in the hints' place. Red,
/// because a failure and a receipt are two different sentences and yellow
/// was saying both.
pub fn problem_line(text: String, colors: bool) -> Line<'static> {
    styled_line(format!(" {text} "), Style::new().fg(Color::Red), colors)
}

/// The right note for what a verb reported: failures are problems, the rest
/// are notes. The views build their messages with `Could not` in front, so
/// the prefix is the seam.
pub fn outcome_line(text: String, colors: bool) -> Line<'static> {
    if text.starts_with("Could not") {
        problem_line(text, colors)
    } else {
        note_line(text, colors)
    }
}

/// The bottom edge: the note when there is one, the hints otherwise.
pub fn footer(note: &str, hint: &str, colors: bool) -> Line<'static> {
    if note.is_empty() {
        hint_line(hint.to_string(), colors)
    } else {
        outcome_line(note.to_string(), colors)
    }
}

/// The note for a file handed to the system viewer. Failures start with
/// `Could not`, the prefix [`outcome_line`] keys on.
pub fn opened_note(result: anyhow::Result<PathBuf>) -> String {
    match result {
        Ok(path) => format!("Opened {}", path.display()),
        Err(e) => format!("Could not open it: {e}"),
    }
}

/// The note for a file saved where the user works, and its receipt. Twice on
/// purpose: the note is for now, the receipt is for after the alternate
/// screen has taken the note away. Failures start with `Could not`, as in
/// [`opened_note`], and leave no receipt.
pub fn saved_note(result: anyhow::Result<PathBuf>, receipts: &mut Vec<String>) -> String {
    match result {
        Ok(path) => {
            let line = format!("Saved {}", path.display());
            receipts.push(line.clone());
            line
        }
        Err(e) => format!("Could not save it: {e}"),
    }
}

/// The scroll position, drawn over the right border. Only called when the
/// list overflows; a scrollbar on a list that fits is furniture. The thumb
/// is sized against the viewport, and the arrow caps are dropped: they would
/// be drawn over the frame's corners.
pub fn scrollbar(frame: &mut Frame<'_>, outer: Rect, total: usize, selected: usize, viewport: u16) {
    let mut state = ScrollbarState::new(total)
        .position(selected)
        .viewport_content_length(viewport as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        // Inside the corners, so ╮ and ╯ survive.
        outer.inner(Margin {
            horizontal: 0,
            vertical: 1,
        }),
        &mut state,
    );
}

/// The one selection style: reverse video, never a color. There is no color
/// that is legible on every background; the user already told their terminal
/// what its foreground and background are, and this borrows them.
pub fn selection() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// A table's header row: dim and underlined, no color and no blank row after
/// it -- the underline already separates, and a spare blank row on a list of
/// a hundred accounts is a row of data lost.
///
/// Only for tables whose columns do not explain themselves: a number and a
/// date need naming, a username and a badge do not.
pub fn header_row(labels: &[&str], colors: bool) -> Row<'static> {
    Row::new(
        labels
            .iter()
            .map(|l| Cell::from((*l).to_string()))
            .collect::<Vec<_>>(),
    )
    .style(paint(colors, DIM.add_modifier(Modifier::UNDERLINED)))
}

/// A list's table: two columns between columns, the selection in reverse
/// video, and a `> ` beside it -- what is left of the selection when there
/// is no styling at all.
pub fn list_table<'a, R, C>(rows: R, widths: C) -> Table<'a>
where
    R: IntoIterator,
    R::Item: Into<Row<'a>>,
    C: IntoIterator,
    C::Item: Into<Constraint>,
{
    Table::new(rows, widths)
        .column_spacing(2)
        .row_highlight_style(selection())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always)
}

/// Where a modal lands: the optical center, one third from the top rather
/// than the geometric middle. The eye rests above center, and a question
/// dropped into the exact middle of a fifty-row screen sits below where
/// anyone was looking.
pub fn modal_area(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let [_, band, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(2),
    ])
    .areas(area);
    band.centered_horizontally(Constraint::Length(width))
}

/// Dims everything drawn so far, so the modal on top of it reads as on top.
/// `Buffer::set_style` patches -- it adds DIM without touching a single
/// color, so the theme still decides what the background looks like. The
/// modal itself is rendered over a `Clear`, which resets its cells, so the
/// veil never reaches it.
pub fn veil(frame: &mut Frame<'_>, colors: bool) {
    if colors {
        let area = frame.area();
        frame.buffer_mut().set_style(area, DIM);
    }
}

/// Clears the modal's cells and draws its frame, handing back the interior.
/// The order is ratatui's own recipe: `Clear` first, so no style or symbol
/// underneath leaks through.
pub fn open_modal(frame: &mut Frame<'_>, area: Rect, block: Block<'_>) -> Rect {
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

/// What `List::scroll_padding` does and `Table` does not: keep `padding`
/// rows visible on each side of the selection. `Table` only ever scrolls
/// just enough, so the selection rides the edge; this recomputes the offset
/// the way ratatui's own `apply_scroll_padding_to_selected_index` does,
/// including clamping the padding to what a small viewport can honor.
pub fn scrolled_offset(
    offset: usize,
    selected: usize,
    total: usize,
    viewport: usize,
    padding: usize,
) -> usize {
    if viewport == 0 || total == 0 {
        return 0;
    }
    let pad = padding.min(viewport.saturating_sub(1) / 2);
    let max = total.saturating_sub(viewport);
    let top = selected.saturating_sub(pad);
    let bottom = (selected + pad).min(total - 1);
    offset
        .min(max)
        .min(top)
        .max(bottom.saturating_sub(viewport.saturating_sub(1)))
        .min(max)
}

/// One row of a test terminal, its whole width, as drawn.
#[cfg(test)]
pub(crate) fn row_text(terminal: &Terminal<ratatui::backend::TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// The account the views' tests act as: `@me`.
#[cfg(test)]
pub(crate) fn me() -> Viewer {
    Viewer {
        pk: 7.into(),
        username: Some("me".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walking the selection from end to end keeps two rows of margin
    /// visible wherever the list allows it, and the offset never overshoots.
    #[test]
    fn the_offset_keeps_the_margin_and_never_overshoots() {
        let (total, viewport, pad) = (50usize, 10usize, 2usize);
        let mut offset = 0usize;
        for selected in 0..total {
            offset = scrolled_offset(offset, selected, total, viewport, pad);
            assert!(offset <= total - viewport, "offset {offset} past the end");
            assert!(
                selected >= offset && selected < offset + viewport,
                "selection {selected} fell out of [{offset}, {})",
                offset + viewport
            );
            if selected >= pad && selected < total - pad {
                assert!(
                    selected >= offset + pad && selected <= offset + viewport - 1 - pad,
                    "margin lost at {selected} with offset {offset}"
                );
            }
        }
    }

    /// Without styling a style is dropped whole; with it, kept as given.
    #[test]
    fn paint_keeps_a_style_only_where_the_views_may_style() {
        let accent = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        assert_eq!(paint(true, accent), accent);
        assert_eq!(paint(false, accent), Style::new());
        assert_eq!(paint(false, DIM), Style::new());
    }

    /// The notes the verbs leave: what was opened or saved, or why not, with
    /// a receipt only for what was saved.
    #[test]
    fn the_notes_say_what_happened_and_only_a_save_leaves_a_receipt() {
        let path = || Ok(PathBuf::from("./someone-1.jpg"));
        let failed = || Err(anyhow::anyhow!("the viewer would not start"));
        assert_eq!(opened_note(path()), "Opened ./someone-1.jpg");
        assert_eq!(
            opened_note(failed()),
            "Could not open it: the viewer would not start"
        );

        let mut receipts = Vec::new();
        assert_eq!(saved_note(path(), &mut receipts), "Saved ./someone-1.jpg");
        assert_eq!(receipts, ["Saved ./someone-1.jpg"]);
        assert_eq!(
            saved_note(failed(), &mut receipts),
            "Could not save it: the viewer would not start"
        );
        assert_eq!(receipts.len(), 1, "a failure leaves no receipt");

        // Only the top context reaches the note, as `{e}` gives it.
        let layered = Err(anyhow::anyhow!("disk full").context("could not write x"));
        assert_eq!(opened_note(layered), "Could not open it: could not write x");
    }

    /// A viewport too small for the margin still shows the selection.
    #[test]
    fn a_tiny_viewport_still_follows_the_selection() {
        let mut offset = 0usize;
        for selected in 0..10 {
            offset = scrolled_offset(offset, selected, 10, 3, 2);
            assert!(selected >= offset && selected < offset + 3);
        }
    }

    fn top_row(block: Block<'_>, width: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, 3)).expect("a test terminal");
        terminal
            .draw(|frame| frame.render_widget(block, frame.area()))
            .expect("a frame");
        row_text(&terminal, 0)
    }

    /// The account sits in the corner and the position to its left.
    #[test]
    fn the_account_takes_the_corner_after_the_position() {
        let block = top_right(
            view_block("People".to_string(), false, Padding::ZERO),
            Some(position_line(2, 40, false)),
            viewer_line(&me(), false),
        );
        let row = top_row(block, 40);
        assert!(row.ends_with(" 3/40 ─ as @me ╮"), "got {row:?}");
        assert!(row.starts_with("╭ People "), "got {row:?}");
    }

    /// Without a position the account alone is in the corner, and an account
    /// whose name is not known yet is named by its id.
    #[test]
    fn the_account_alone_takes_the_corner() {
        let nameless = Viewer {
            pk: 7.into(),
            username: None,
        };
        let block = top_right(
            view_block("People".to_string(), false, Padding::ZERO),
            None,
            viewer_line(&nameless, false),
        );
        let row = top_row(block, 40);
        assert!(row.ends_with("─ as account 7 ╮"), "got {row:?}");
    }

    /// On a row too narrow for both, the account keeps its place and the
    /// position is what gets cut.
    #[test]
    fn a_narrow_row_keeps_the_account() {
        let block = top_right(
            Block::bordered(),
            Some(position_line(2, 40, false)),
            viewer_line(&me(), false),
        );
        let row = top_row(block, 12);
        assert!(row.ends_with(" as @me ┐"), "got {row:?}");
    }

    /// The modal band sits above the geometric middle, centered across.
    #[test]
    fn the_modal_lands_on_the_optical_center() {
        let area = Rect::new(0, 0, 100, 30);
        let modal = modal_area(area, 46, 7);
        assert_eq!(modal.width, 46);
        assert_eq!(modal.height, 7);
        assert_eq!(modal.x, 27, "centered: (100 - 46) / 2");
        assert!(
            modal.y + modal.height / 2 < 15,
            "the middle of the modal is above the middle of the screen, got y {}",
            modal.y
        );
    }
}
