//! The box a view puts over itself while it waits on the network.
//!
//! A request made from a view may sit behind the pacer for seconds before it
//! is even sent, and a download is as long as the file is large. A view that
//! says nothing meanwhile looks hung, and the one thing a person can do about
//! a hung program is the thing the view least wants: kill it. So every fetch
//! a key starts is run under a [`Busy`], which names what is going on and how
//! to stop it, with a spinner that turns to show the program is alive.
//!
//! **Two steps, because of the borrows.** The view under the box is drawn
//! from the same state the fetch is about to change: a folder's items are
//! read by the frame and filled by the walk. [`Busy::begin`] draws the view
//! once and keeps the cells; [`Busy::run`] then owns the wait, and paints the
//! box over those kept cells without asking the view anything.
//!
//! **Nothing is shown before the first [`input::TICK`].** Most opens are
//! answered from what the session already holds, and a box that flashed for
//! a frame on each of them would be noise. `begin`'s own frame is the view as
//! it already was, which writes nothing.
//!
//! The keyboard stays alive through [`input::watching_cancel_keys`], which
//! this wraps: `q` and Ctrl+C cancel the token the fetch races.

use std::future::Future;
use std::pin::Pin;

use console::measure_text_width;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};
use snob_ig::pace::CancelToken;

use crate::ui::browser::input::{self, Stopped, watching_cancel_keys};
use crate::ui::tui::{self, Tui};

/// A wait about to start: what it is called, and the view it happens over.
pub struct Busy {
    text: String,
    /// The view as it was drawn when the wait began.
    under: Buffer,
    colors: bool,
}

impl Busy {
    /// Draws the view through `under` and keeps what it drew. `text` is what
    /// the box will say: `Opening story 3`, `Reading @someone`.
    pub fn begin(
        tui: &mut Tui,
        text: impl Into<String>,
        under: impl FnOnce(&mut Frame<'_>),
    ) -> std::io::Result<Self> {
        let mut kept = Buffer::default();
        tui.terminal.draw(|frame| {
            under(frame);
            kept = frame.buffer_mut().clone();
        })?;
        Ok(Self::over(text, kept, tui::colors_enabled()))
    }

    /// A wait over cells already drawn.
    fn over(text: impl Into<String>, under: Buffer, colors: bool) -> Self {
        Self {
            text: text.into(),
            under,
            colors,
        }
    }

    /// Runs `work` to its end with the keyboard alive and, once it has
    /// lasted a tick, the box up and its spinner turning.
    ///
    /// A frame that fails to draw is ignored on purpose: the fetch is the
    /// job and the box is the report, and the view's next ordinary frame
    /// repaints whatever this left behind.
    ///
    /// **The wait lives on the heap, and that is not a nicety.** Every
    /// command's future is held inside the one `main` awaits, a view's with
    /// the rest, and a debug build copies that future from frame to frame on
    /// its way into the runtime. With this an `async fn`, each wait a view
    /// makes stood in the view's future by value, the work with the loop
    /// around it and the watcher around that, and took the whole from 13,736
    /// bytes to 22,728 (`size_of_val` of what `main` awaits, a debug build
    /// with `testing`, November 2026). On Windows, where the main thread has
    /// 1 MiB, that was enough for a command that draws nothing to overflow
    /// its stack: the sandbox test `a_job_polled_to_its_end_gives_the_walks_answer`
    /// failed on it every time. Boxed, a wait is a pointer in the view's
    /// future whatever the work is, and the whole is 13,736 bytes again.
    pub fn run<'a, T: 'a>(
        self,
        tui: &'a mut Tui,
        cancel: &'a CancelToken,
        work: impl Future<Output = T> + 'a,
    ) -> Pin<Box<dyn Future<Output = (T, Stopped)> + 'a>> {
        Box::pin(async move {
            let waited = async {
                tokio::pin!(work);
                let mut turn = 0usize;
                // The first wait is a focused tick whoever is looking: a
                // window without the focus is often still on screen, and two
                // seconds of nothing is what this box exists to prevent.
                let mut wait = input::TICK;
                loop {
                    tokio::select! {
                        biased;
                        out = &mut work => break out,
                        () = tokio::time::sleep(wait) => {
                            let _ = tui.terminal.draw(|frame| self.draw(frame, turn));
                            turn = turn.wrapping_add(1);
                            wait = input::tick();
                        }
                    }
                }
            };
            watching_cancel_keys(cancel, waited).await
        })
    }

    /// One frame of the wait: the kept view, dimmed, and the box over it.
    ///
    /// A terminal resized since the view was kept has cells of another
    /// shape, and the kept ones would land in the wrong places: the box is
    /// then drawn alone, and the view comes back with its next frame.
    fn draw(&self, frame: &mut Frame<'_>, turn: usize) {
        let buffer = frame.buffer_mut();
        if buffer.area == self.under.area {
            buffer.content.clone_from(&self.under.content);
        }
        modal(frame, &self.text, turn, self.colors);
    }
}

/// The spinner's frame for `turn`, from the frames a walk's progress line
/// turns through, less the last, which is the one that says "finished".
fn spinner(turn: usize) -> char {
    let frames = crate::progress::tick_chars(crate::progress::rich_glyphs());
    let turning = frames.chars().count().saturating_sub(1).max(1);
    frames.chars().nth(turn % turning).unwrap_or(' ')
}

/// The box itself, over whatever the frame holds: the spinner and what is
/// going on in bold, and on the bottom edge the two keys that stop it.
pub fn modal(frame: &mut Frame<'_>, text: &str, turn: usize, colors: bool) {
    let dim = tui::paint(colors, tui::DIM);
    tui::veil(frame, colors);
    let text = format!("{} {text}", spinner(turn));
    let width = (measure_text_width(&text) as u16 + 8)
        .max(34)
        .min(frame.area().width);
    let area = tui::modal_area(frame.area(), width, 5);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(tui::card_padding())
        .title_bottom(Line::from(Span::styled(" q or ctrl+c cancels ", dim)).centered());
    let inner = tui::open_modal(frame, area, block);
    frame.render_widget(
        Paragraph::new(tui::styled_line(text, tui::BOLD, colors)),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let height = terminal.backend().buffer().area.height;
        (0..height)
            .map(|y| tui::row_text(terminal, y) + "\n")
            .collect()
    }

    /// A view of `width` by `height` with one line of text on its top row.
    fn kept(width: u16, height: u16, text: &str) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut kept = Buffer::default();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(text), frame.area());
                kept = frame.buffer_mut().clone();
            })
            .unwrap();
        kept
    }

    /// The box says what is being waited on and which keys end the wait.
    #[test]
    fn the_busy_box_names_what_is_going_on_and_how_to_stop_it() {
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal
            .draw(|frame| modal(frame, "Opening story 3", 0, false))
            .unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains("Opening story 3"), "{screen}");
        assert!(screen.contains("q or ctrl+c cancels"), "{screen}");
    }

    /// The view the wait began over is still there around the box.
    #[test]
    fn the_busy_box_is_drawn_over_the_view_it_found() {
        let busy = Busy::over("Reading @anna", kept(70, 14, "Followers of @me"), false);
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal.draw(|frame| busy.draw(frame, 0)).unwrap();
        let screen = screen(&terminal);
        assert!(screen.starts_with("Followers of @me"), "{screen}");
        assert!(screen.contains("Reading @anna"), "{screen}");
    }

    /// After a resize the kept cells are of another shape: they are left
    /// out rather than drawn in the wrong places, and the box still shows.
    #[test]
    fn a_resize_during_a_wait_draws_the_box_alone() {
        let busy = Busy::over("Reading @anna", kept(70, 14, "Followers of @me"), false);
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        terminal.draw(|frame| busy.draw(frame, 0)).unwrap();
        let screen = screen(&terminal);
        assert!(!screen.contains("Followers of @me"), "{screen}");
        assert!(screen.contains("Reading @anna"), "{screen}");
    }

    /// The spinner goes round its frames and never lands on the last one,
    /// which a walk's progress line shows only when it is done.
    #[test]
    fn the_spinner_turns_and_never_shows_its_finished_frame() {
        let frames = crate::progress::tick_chars(crate::progress::rich_glyphs());
        let finished = frames.chars().last().unwrap();
        let turning = frames.chars().count() - 1;
        assert_ne!(spinner(0), spinner(1));
        assert_eq!(spinner(0), spinner(turning));
        for turn in 0..turning * 2 {
            assert_ne!(spinner(turn), finished, "turn {turn}");
        }
    }
}
