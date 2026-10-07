//! The interactive posts browser: a profile's grid as a list, and inside
//! each post what it says, its items and its comments.
//!
//! The highlights browser's shape, two levels in one loop, so that the
//! terminal and the scratch directory last the session. At the grid a row is
//! a post: Enter opens it, D keeps all of it, and reaching the last row reads
//! the grid's next page, as scrolling the app's grid does. Inside, the post is
//! what the app shows under it: whose it is and with whom, when and where, the
//! caption, who is tagged and mentioned, "Liked by ...", and its items, with
//! its comments a key away. `snob post LINK` opens straight inside.
//!
//! **A post is opened with the two reads the app opens it with** (its info
//! and its first page of comments, [`crate::posts::open`]), once a session:
//! walking out and back in costs nothing. More comments are one read a page,
//! on `n`, never on their own.
//!
//! The keys are the other browsers' where they mean the same thing; inside a
//! post the arrows across move between its items, as the app's arrows do,
//! and Esc or Backspace walk back out. A fetch blocks the keys, as in the
//! highlights browser, but q and Ctrl+C still stop it.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::text::Line;
use ratatui::widgets::{Cell, Paragraph, Row, TableState, Wrap};
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;

use crate::app::Viewer;
use crate::commands::post::comment_line;
use crate::commands::posts::{about, clipped, size_of};
use crate::exit::ExitCode;
use crate::posts::{self, Comments, Grid, Opened, Post};
use crate::report;
use crate::ui::browser::input::{self, Action, Raw, page, watching_cancel_keys};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::tui::{self, Tui};

/// What a key does in this browser: the shared bindings, and the few that
/// mean something here alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Shared(Action),
    /// `←`: the item before, inside a post.
    Previous,
    /// `→`: the item after, inside a post; at the grid it opens one.
    Next,
    /// `o` or Enter: open.
    Open,
    /// `d`: save the item on screen, or at the grid the whole post.
    SaveOne,
    /// `D`: save the whole post.
    SaveAll,
    /// `c`: show or hide the comments.
    Comments,
    /// `n`: the next page of comments.
    MoreComments,
    /// Esc and Backspace: walk back out, or leave where there is no out.
    Back,
}

/// Binds one key. Plain, or with shift, and nothing else, as every browser
/// binds them (`input::action_of` says why).
fn key_of(key: KeyEvent) -> Key {
    let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT);
    if !plain {
        return Key::Shared(input::action_of(key));
    }
    match key.code {
        KeyCode::Left => Key::Previous,
        KeyCode::Right => Key::Next,
        KeyCode::Enter | KeyCode::Char('o') => Key::Open,
        KeyCode::Char('d') => Key::SaveOne,
        KeyCode::Char('D') => Key::SaveAll,
        KeyCode::Char('c') => Key::Comments,
        KeyCode::Char('n') => Key::MoreComments,
        KeyCode::Esc | KeyCode::Backspace => Key::Back,
        _ => Key::Shared(input::action_of(key)),
    }
}

/// One post as the session holds it once opened.
struct Detail {
    post: Post,
    /// Read as it opened; `None` when reading them failed.
    comments: Option<Comments>,
    showing_comments: bool,
    /// The item on screen.
    item: usize,
    /// How far the comments are scrolled, in lines.
    scroll: usize,
    opened: Opened,
}

impl Detail {
    fn new(post: Post, comments: Option<Comments>) -> Self {
        let opened = vec![None; post.items.len()];
        Self {
            post,
            comments,
            showing_comments: false,
            item: 0,
            scroll: 0,
            opened,
        }
    }
}

/// Where the browser is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Grid,
    /// Inside the post at this index of the grid, or of the one post a link
    /// named.
    Post(usize),
}

/// Browses `grid`, opening post `start` (zero-based) before the first frame
/// when one was asked for.
pub async fn browse_grid(
    client: &IgClient,
    grid: Grid,
    start: Option<usize>,
    viewer: &Viewer,
    paths: &AppPaths,
) -> Result<ExitCode> {
    let details = grid.posts.iter().map(|_| None).collect();
    let mut session = Session {
        client,
        grid: Some(grid),
        details,
        level: Level::Grid,
        selected: start.unwrap_or(0),
        note: String::new(),
        receipts: Vec::new(),
    };
    if let Some(index) = start {
        session.walk_in(index).await;
    }
    session.run(viewer, paths).await
}

/// Browses the one post a link named, inside it from the first frame. Its
/// comments are read as it opens, as the app reads them.
pub async fn browse_post(
    client: &IgClient,
    post: Post,
    viewer: &Viewer,
    paths: &AppPaths,
) -> Result<ExitCode> {
    let mut session = Session {
        client,
        grid: None,
        details: vec![None],
        level: Level::Post(0),
        selected: 0,
        note: String::new(),
        receipts: Vec::new(),
    };
    let (comments, stopped) = watching_cancel_keys(
        client.pacer().cancel_token(),
        posts::first_comments(client, &post),
    )
    .await;
    if let Some(code) = stopped.leave() {
        return Ok(code);
    }
    let comments = match comments {
        Ok(comments) => Some(comments),
        Err(e) => {
            session.note = format!("Could not read the comments: {e}");
            None
        }
    };
    session.details[0] = Some(Detail::new(post, comments));
    session.run(viewer, paths).await
}

struct Session<'a> {
    client: &'a IgClient,
    /// `None` for a post a link named, which has no grid to walk back to.
    grid: Option<Grid>,
    /// Each post of the grid once opened, by index.
    details: Vec<Option<Detail>>,
    level: Level,
    /// The grid's row.
    selected: usize,
    note: String,
    receipts: Vec<String>,
}

impl Session<'_> {
    async fn run(mut self, viewer: &Viewer, paths: &AppPaths) -> Result<ExitCode> {
        snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
        let scratch = Scratch::new(paths.story_scratch())?;
        let mut tui = tui::claim_fullscreen(
            "--no-interactive prints the listing; --download saves without one",
        )?;
        let colors = tui::colors_enabled();
        let mut list = TableState::default();
        let mut page_rows = 1usize;

        // Every way out passes the receipts: see `tui::print_receipts`.
        let outcome: Result<ExitCode> = async {
            loop {
                self.draw(&mut tui, &mut list, viewer, colors, &mut page_rows)?;
                let raw = input::read(input::tick()).map_err(input::unreadable)?;
                let key = match raw {
                    Raw::Key(key) => key_of(key),
                    Raw::Resized | Raw::Paste(_) | Raw::Tick => continue,
                };
                self.note.clear();
                if let Some(code) = self
                    .act(
                        key,
                        &mut tui,
                        &mut list,
                        viewer,
                        colors,
                        &mut page_rows,
                        &scratch,
                    )
                    .await?
                {
                    break Ok(code);
                }
            }
        }
        .await;

        drop(tui);
        tui::print_receipts(&self.receipts);
        outcome
    }

    /// What `key` does where the browser is. `Some` is the way out.
    #[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
    async fn act(
        &mut self,
        key: Key,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        page_rows: &mut usize,
        scratch: &Scratch,
    ) -> Result<Option<ExitCode>> {
        match key {
            Key::Shared(Action::Interrupt) => return Ok(Some(ExitCode::Interrupted)),
            Key::Shared(Action::Quit) => return Ok(Some(ExitCode::Ok)),
            Key::Shared(Action::Redraw) => {
                tui.terminal.clear()?;
                return Ok(None);
            }
            _ => {}
        }
        match self.level {
            Level::Grid => {
                let total = self.grid.as_ref().map_or(0, |g| g.posts.len());
                let before = self.selected;
                match key {
                    Key::Shared(Action::Up) => self.selected = self.selected.saturating_sub(1),
                    Key::Shared(Action::Down) => {
                        self.selected = (self.selected + 1).min(total.saturating_sub(1));
                    }
                    Key::Shared(Action::PageUp) => {
                        self.selected = self.selected.saturating_sub(page(*page_rows));
                    }
                    Key::Shared(Action::PageDown) => {
                        self.selected =
                            (self.selected + page(*page_rows)).min(total.saturating_sub(1));
                    }
                    Key::Shared(Action::First) => self.selected = 0,
                    Key::Shared(Action::Last) => self.selected = total.saturating_sub(1),
                    Key::Open | Key::Next | Key::Shared(Action::Open) => {
                        let index = self.selected;
                        if let Some(code) = self.walk_in_watched(index).await {
                            return Ok(Some(code));
                        }
                    }
                    Key::SaveOne | Key::SaveAll | Key::Shared(Action::Download) => {
                        let Some(post) =
                            self.grid.as_ref().and_then(|g| g.posts.get(self.selected))
                        else {
                            return Ok(None);
                        };
                        let post = post.clone();
                        if let Some(code) = self.keep_all(&post).await {
                            return Ok(Some(code));
                        }
                    }
                    Key::Back => return Ok(Some(ExitCode::Ok)),
                    _ => {}
                }
                // Reaching the last row reads the next page, as scrolling the
                // app's grid to its end does.
                let at_the_end = total > 0 && self.selected + 1 == total && before + 1 != total;
                if at_the_end && self.grid.as_ref().is_some_and(|g| g.next.is_some()) {
                    self.note = "Reading the next page · q stops".into();
                    self.draw(tui, list, viewer, colors, page_rows)?;
                    self.note.clear();
                    let client = self.client;
                    let Some(grid) = self.grid.as_mut() else {
                        return Ok(None);
                    };
                    let (read, stopped) = watching_cancel_keys(
                        client.pacer().cancel_token(),
                        posts::more(client, grid),
                    )
                    .await;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                    match read {
                        Ok(_) => {
                            let read = self.grid.as_ref().map_or(0, |g| g.posts.len());
                            self.details.resize_with(read, || None);
                        }
                        Err(e) => self.note = format!("Could not read more: {e}"),
                    }
                }
            }
            Level::Post(index) => {
                let Some(detail) = self.details.get_mut(index).and_then(Option::as_mut) else {
                    self.level = Level::Grid;
                    return Ok(None);
                };
                let items = detail.post.items.len();
                // The comments scroll no further than their last line.
                let last_line = post_lines(detail, false).len().saturating_sub(1);
                match key {
                    Key::Previous => detail.item = detail.item.saturating_sub(1),
                    Key::Next => detail.item = (detail.item + 1).min(items.saturating_sub(1)),
                    Key::Shared(Action::Up) if detail.showing_comments => {
                        detail.scroll = detail.scroll.saturating_sub(1);
                    }
                    Key::Shared(Action::Down) if detail.showing_comments => {
                        detail.scroll = (detail.scroll + 1).min(last_line);
                    }
                    Key::Shared(Action::PageUp) if detail.showing_comments => {
                        detail.scroll = detail.scroll.saturating_sub(page(*page_rows));
                    }
                    Key::Shared(Action::PageDown) if detail.showing_comments => {
                        detail.scroll = (detail.scroll + page(*page_rows)).min(last_line);
                    }
                    Key::Shared(Action::Up) => detail.item = detail.item.saturating_sub(1),
                    Key::Shared(Action::Down) => {
                        detail.item = (detail.item + 1).min(items.saturating_sub(1));
                    }
                    Key::Open | Key::Shared(Action::Open) => {
                        let (result, stopped) = watching_cancel_keys(
                            self.client.pacer().cancel_token(),
                            posts::open_item(
                                self.client,
                                &detail.post,
                                detail.item,
                                scratch.dir(),
                                &mut detail.opened,
                            ),
                        )
                        .await;
                        self.note = tui::opened_note(result);
                        if let Some(code) = stopped.leave() {
                            return Ok(Some(code));
                        }
                    }
                    Key::SaveOne | Key::Shared(Action::Download) => {
                        let (result, stopped) = watching_cancel_keys(
                            self.client.pacer().cancel_token(),
                            posts::keep_item(
                                self.client,
                                &detail.post,
                                detail.item,
                                &detail.opened,
                            ),
                        )
                        .await;
                        self.note = tui::saved_note(result, &mut self.receipts);
                        if let Some(code) = stopped.leave() {
                            return Ok(Some(code));
                        }
                    }
                    Key::SaveAll => {
                        let post = detail.post.clone();
                        if let Some(code) = self.keep_all(&post).await {
                            return Ok(Some(code));
                        }
                    }
                    Key::Comments => {
                        detail.showing_comments = !detail.showing_comments;
                        detail.scroll = 0;
                        if detail.comments.is_none() {
                            self.note = "The comments could not be read when it opened".into();
                        }
                    }
                    Key::MoreComments => {
                        let Some(comments) = detail.comments.as_mut() else {
                            return Ok(None);
                        };
                        if comments.next.is_none() {
                            self.note = "No more comments".into();
                            return Ok(None);
                        }
                        detail.showing_comments = true;
                        let (read, stopped) = watching_cancel_keys(
                            self.client.pacer().cancel_token(),
                            posts::more_comments(self.client, &detail.post, comments),
                        )
                        .await;
                        if let Some(code) = stopped.leave() {
                            return Ok(Some(code));
                        }
                        if let Err(e) = read {
                            self.note = format!("Could not read more comments: {e}");
                        }
                    }
                    Key::Back | Key::Shared(Action::Back) => {
                        if self.grid.is_none() {
                            return Ok(Some(ExitCode::Ok));
                        }
                        self.level = Level::Grid;
                        *list = TableState::default();
                    }
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Opens post `index` of the grid, reading it the first time, with q and
    /// Ctrl+C still stopping the read. `Some` is the way out.
    async fn walk_in_watched(&mut self, index: usize) -> Option<ExitCode> {
        let client = self.client;
        let cancel = client.pacer().cancel_token().clone();
        let (_, stopped) = watching_cancel_keys(&cancel, self.walk_in(index)).await;
        stopped.leave()
    }

    /// Opens post `index` of the grid: the app's two reads the first time,
    /// nothing after. A post that could not be read stays at the grid, the
    /// reason in the note line.
    async fn walk_in(&mut self, index: usize) {
        let Some(post) = self.grid.as_ref().and_then(|g| g.posts.get(index)).cloned() else {
            return;
        };
        if self.details.get(index).is_some_and(Option::is_some) {
            self.level = Level::Post(index);
            return;
        }
        match posts::open(self.client, &post).await {
            Ok((post, comments)) => {
                if self.details.len() <= index {
                    self.details.resize_with(index + 1, || None);
                }
                self.details[index] = Some(Detail::new(post, Some(comments)));
                self.level = Level::Post(index);
            }
            Err(e) => self.note = format!("Could not open it: {e}"),
        }
    }

    /// D: every item of `post` where the user works. `Some` is the way out.
    async fn keep_all(&mut self, post: &Post) -> Option<ExitCode> {
        let (kept, stopped) = watching_cancel_keys(
            self.client.pacer().cancel_token(),
            posts::keep_post(self.client, post),
        )
        .await;
        let (note, saved) = kept;
        if saved {
            self.receipts.push(note.clone());
        }
        // Said outside the window to somebody who went to another one while
        // it downloaded (`ui::notify`).
        if stopped == input::Stopped::No && !input::focused() {
            crate::ui::notify::finished(&note, true);
        }
        self.note = note;
        stopped.leave()
    }

    fn draw(
        &self,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        page_rows: &mut usize,
    ) -> Result<()> {
        tui.terminal.draw(|frame| match self.level {
            Level::Grid => {
                if let Some(grid) = &self.grid {
                    list.select(Some(self.selected));
                    draw_grid(frame, grid, viewer, list, &self.note, colors, page_rows);
                }
            }
            Level::Post(index) => {
                if let Some(detail) = self.details.get(index).and_then(Option::as_ref) {
                    draw_post(
                        frame,
                        detail,
                        self.grid.is_some(),
                        viewer,
                        &self.note,
                        colors,
                        page_rows,
                    );
                }
            }
        })?;
        Ok(())
    }
}

/// Draws the grid: one post a row, in named columns.
fn draw_grid(
    frame: &mut Frame<'_>,
    grid: &Grid,
    viewer: &Viewer,
    list: &mut TableState,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let total = grid.posts.len();
    let selected = list.selected().unwrap_or(0);
    let area = frame.area();
    let more = if grid.next.is_some() {
        ", more below"
    } else {
        ""
    };
    let mut block = tui::view_block(
        format!("Posts · @{} · {total} read{more}", grid.username),
        colors,
        tui::list_padding(area),
    );
    let inner = block.inner(area);
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
        "↑↓ move · enter open · d save all of it · q quit",
        colors,
    ));
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), selected, total, viewport, 2);
    let dim = tui::paint(colors, tui::DIM);
    let rows = grid.posts.iter().enumerate().map(|(index, post)| {
        let likes = if post.likes_hidden {
            "-".to_string()
        } else {
            post.likes.map_or("-".into(), posts::grouped)
        };
        Row::new(vec![
            Cell::from(
                Line::from(format!("{}.", index + 1))
                    .right_aligned()
                    .style(dim),
            ),
            Cell::from(post.kind.label()),
            Cell::from(report::dated(post.taken_at)).style(dim),
            Cell::from(Line::from(likes).right_aligned()),
            Cell::from(
                Line::from(post.comments.map_or("-".into(), posts::grouped)).right_aligned(),
            ),
            Cell::from(clipped(post.headline(), 80)),
        ])
    });
    frame.render_stateful_widget(
        tui::list_table(
            rows,
            [
                Constraint::Length(4),
                Constraint::Length(9),
                Constraint::Length(13),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Fill(1),
            ],
        )
        .header(tui::header_row(
            &["", "kind", "posted", "likes", "comments", "caption"],
            colors,
        )),
        inner,
        list,
    );
    if !fits {
        tui::scrollbar(frame, area, total, selected, viewport as u16);
    }
}

/// Draws one post: what it says, its items with the one on screen marked,
/// and its comments when they are shown.
fn draw_post(
    frame: &mut Frame<'_>,
    detail: &Detail,
    from_a_grid: bool,
    viewer: &Viewer,
    note: &str,
    colors: bool,
    page_rows: &mut usize,
) {
    let post = &detail.post;
    let area = frame.area();
    let mut block = tui::view_block(
        format!("@{} · {}", post.owner, post.code),
        colors,
        tui::card_padding(),
    );
    let inner = block.inner(area);
    *page_rows = inner.height as usize;
    block = tui::top_right(block, None, tui::viewer_line(viewer, colors));
    let back = if from_a_grid { "esc back" } else { "esc leave" };
    block = block.title_bottom(tui::footer(
        note,
        &format!("←→ item · o open · d save it · D save the post · c comments · n more · {back}"),
        colors,
    ));
    frame.render_widget(block, area);

    let lines = post_lines(detail, colors);
    let scroll = if detail.showing_comments {
        detail.scroll.min(lines.len().saturating_sub(1))
    } else {
        0
    };
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
    frame.render_widget(paragraph, inner);
}

/// The lines of one post as [`draw_post`] shows it.
fn post_lines(detail: &Detail, colors: bool) -> Vec<Line<'static>> {
    let post = &detail.post;
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    let mut lines: Vec<Line<'static>> = about(post)
        .into_iter()
        .enumerate()
        .map(|(i, line)| match i {
            0 => Line::from(line).style(bold),
            1 | 2 => Line::from(line).style(dim),
            _ => Line::from(line),
        })
        .collect();
    lines.push(Line::default());
    for (index, item) in post.items.iter().enumerate() {
        let here = index == detail.item;
        let marker = if here { "> " } else { "  " };
        let line = Line::from(format!(
            "{marker}{}. {} {}",
            index + 1,
            item.kind.label(),
            size_of(item.width, item.height)
        ));
        lines.push(if here {
            line.style(tui::paint(colors, tui::selection()))
        } else {
            line
        });
    }
    if detail.showing_comments {
        lines.push(Line::default());
        match &detail.comments {
            None => lines.push(Line::from("The comments could not be read").style(dim)),
            Some(comments) => {
                let total = comments
                    .total
                    .map(|n| format!(" ({})", posts::grouped(n)))
                    .unwrap_or_default();
                lines.push(Line::from(format!("Comments{total}")).style(bold));
                if comments.lines.is_empty() {
                    lines.push(Line::from("none").style(dim));
                }
                for comment in &comments.lines {
                    lines.push(Line::from(comment_line(comment)));
                    for reply in &comment.replies {
                        lines.push(Line::from(format!("    ↳ {}", comment_line(reply))).style(dim));
                    }
                }
                if comments.next.is_some() {
                    lines.push(Line::from("n reads more").style(dim));
                }
            }
        }
    } else if detail
        .comments
        .as_ref()
        .is_some_and(|c| !c.lines.is_empty())
    {
        lines.push(Line::default());
        lines.push(Line::from("c shows the comments").style(dim));
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Epoch;
    use snob_ig::model::post::Media;

    use super::*;
    use crate::posts::CommentLine;

    fn post() -> Post {
        let media: Media = serde_json::from_str(
            r#"{"pk":"3995556159532654737","code":"DdzEShhEWCR","media_type":8,"taken_at":1790510400,
            "caption":{"text":"sun"},"like_count":80,"comment_count":2,"top_likers":["close"],
            "carousel_media":[{"pk":"1","media_type":1,"original_width":1080,"original_height":1350},
                              {"pk":"2","media_type":2,"video_versions":[{"url":"https://x.cdninstagram.com/b.mp4","width":720,"height":1280}]}]}"#,
        )
        .unwrap();
        Post::from_media(&media, "someone")
    }

    fn grid() -> Grid {
        Grid {
            username: "someone".into(),
            posts: vec![post(), post()],
            next: Some("x".into()),
            declared: Some(30),
        }
    }

    #[test]
    fn the_grid_draws_a_post_a_row() {
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut rows = 0usize;
        terminal
            .draw(|frame| draw_grid(frame, &grid(), &tui::me(), &mut list, "", false, &mut rows))
            .unwrap();
        let title = tui::row_text(&terminal, 0);
        assert!(
            title.contains("Posts · @someone · 2 read, more below"),
            "{title}"
        );
        let header = tui::row_text(&terminal, 1);
        assert!(header.contains("caption"), "{header}");
        let first = tui::row_text(&terminal, 2);
        assert!(first.contains("1."), "{first}");
        assert!(first.contains("2 items"), "{first}");
        assert!(first.contains("80"), "{first}");
        assert!(first.contains("sun"), "{first}");
        assert!(tui::row_text(&terminal, 7).contains("enter open"));
    }

    #[test]
    fn a_post_shows_what_it_says_its_items_and_its_comments() {
        let mut detail = Detail::new(
            post(),
            Some(Comments {
                lines: vec![CommentLine {
                    author: "friend".into(),
                    text: "nice".into(),
                    at: Epoch::new(1_790_510_400),
                    likes: 0,
                    replies_count: 0,
                    replies: Vec::new(),
                }],
                next: None,
                total: Some(1),
            }),
        );
        detail.item = 1;
        let text = |detail: &Detail| -> Vec<String> {
            post_lines(detail, false)
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let shown = text(&detail);
        assert_eq!(shown[0], "@someone · 2 items");
        assert!(shown.contains(&"Liked by close and 79 others · 2 comments".to_string()));
        assert!(
            shown.contains(&"  1. photo 1080×1350".to_string()),
            "{shown:?}"
        );
        assert!(
            shown.contains(&"> 2. video 720×1280".to_string()),
            "{shown:?}"
        );
        assert_eq!(shown.last().unwrap(), "c shows the comments");

        detail.showing_comments = true;
        let shown = text(&detail);
        assert!(shown.contains(&"Comments (1)".to_string()), "{shown:?}");
        assert!(
            shown.last().unwrap().starts_with("@friend: nice"),
            "{shown:?}"
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        let mut rows = 0usize;
        terminal
            .draw(|frame| draw_post(frame, &detail, true, &tui::me(), "", false, &mut rows))
            .unwrap();
        assert!(tui::row_text(&terminal, 0).contains("@someone · DdzEShhEWCR"));
        assert!(tui::row_text(&terminal, 19).contains("esc back"));
    }

    #[test]
    fn the_keys_inside_a_post_are_its_own() {
        let key = |code| key_of(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Left), Key::Previous);
        assert_eq!(key(KeyCode::Right), Key::Next);
        assert_eq!(key(KeyCode::Char('o')), Key::Open);
        assert_eq!(key(KeyCode::Enter), Key::Open);
        assert_eq!(key(KeyCode::Char('d')), Key::SaveOne);
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)),
            Key::SaveAll
        );
        assert_eq!(key(KeyCode::Char('c')), Key::Comments);
        assert_eq!(key(KeyCode::Char('n')), Key::MoreComments);
        assert_eq!(key(KeyCode::Esc), Key::Back);
        assert_eq!(key(KeyCode::Char('q')), Key::Shared(Action::Quit));
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Key::Shared(Action::Interrupt)
        );
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            Key::Shared(Action::None),
            "a modified arrow belongs to somebody else"
        );
    }
}
