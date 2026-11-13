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
//! post the arrows across, and `h` and `l` with them, move between its items,
//! as the app's arrows do, and Esc or Backspace walk back out. `?` shows the
//! keys of the level, `w` hands the post's page to the system's browser, and
//! `p` draws the item on screen in a build that draws pictures
//! (`ui::preview`). Not bound, and kept for what comes next: `r` and `L`.
//!
//! **Every wait is under a box that says what it is** (`ui::browser::busy`):
//! opening a post, an item, a page of the grid or of the comments, saving.
//! The post a command asked for is read that way too, on the first frame,
//! so that nothing is fetched with the screen saying nothing.
//!
//! **What does not fit is said where there is room for it.** A narrow grid
//! drops its columns from the right, comments first; a wide one puts all the
//! selected post says in a panel beside the list, from what the grid already
//! read, and Space puts the same in a box at any width. Inside a post the
//! text is wrapped here and not by the paragraph, so that the comments
//! scroll to their last row and no further ([`visual`]), and so that the
//! item on screen stays in view under a caption taller than the terminal.
//!
//! **It can be entered on a terminal somebody else holds**: the profile
//! card's posts tile comes in through [`browse_grid_in`], with the card's
//! guard and its scratch directory, and Esc at the grid goes back to the
//! card.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Padding, Paragraph, Row, TableState};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;

use crate::app::Viewer;
use crate::commands::post::comment_line;
use crate::commands::posts::{about, clipped, size_of};
use crate::exit::ExitCode;
use crate::media::Kind;
use crate::posts::{self, Comments, Grid, Opened, Post};
use crate::report;
use crate::ui::browser::busy::Busy;
use crate::ui::browser::input::{self, Action, Raw, page};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::help;
use crate::ui::preview;
use crate::ui::tui::{self, Tui};

/// What a key does in this browser: the shared bindings, and the few that
/// mean something here alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Shared(Action),
    /// `←` or `h`: the item before, inside a post; at a grid the card
    /// opened, back to the card.
    Previous,
    /// `→` or `l`: the item after, inside a post; at the grid it opens one.
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
    /// Space, at the grid: all the selected post says, in a box.
    About,
    /// Esc and Backspace: walk back out, or leave where there is no out.
    Back,
}

/// Binds one key. Plain, or with shift, and nothing else, as every browser
/// binds them (`input::action_of` says why). A key this browser gives a
/// meaning of its own is one line here and one in the table `?` shows.
fn key_of(key: KeyEvent) -> Key {
    let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT);
    if !plain {
        return Key::Shared(input::action_of(key));
    }
    match key.code {
        KeyCode::Left | KeyCode::Char('h') => Key::Previous,
        KeyCode::Right | KeyCode::Char('l') => Key::Next,
        KeyCode::Enter | KeyCode::Char('o') => Key::Open,
        KeyCode::Char('d') => Key::SaveOne,
        KeyCode::Char('D') => Key::SaveAll,
        KeyCode::Char('c') => Key::Comments,
        KeyCode::Char('n') => Key::MoreComments,
        KeyCode::Char(' ') => Key::About,
        KeyCode::Esc | KeyCode::Backspace => Key::Back,
        _ => Key::Shared(input::action_of(key)),
    }
}

/// What `?` shows over the grid.
const GRID_KEYS: &[help::Key] = &[
    help::MOVE,
    help::PAGE,
    help::ENDS,
    help::key("enter o → l", "open the post", KeyCode::Enter),
    help::key("d", "save every item of the post here", KeyCode::Char('d')),
    help::key("space", "all the post says, in a box", KeyCode::Char(' ')),
    help::key("w", "open the post on the web", KeyCode::Char('w')),
    help::key(
        "esc backspace",
        "leave, or back where it came from",
        KeyCode::Esc,
    ),
    help::REDRAW,
    help::QUIT,
    help::STOP,
];

/// What `?` shows inside a post.
const POST_KEYS: &[help::Key] = &[
    help::key("← → h l", "the item before, the item after", KeyCode::Right),
    help::key("↑ ↓ j k", "the same, or scroll the comments", KeyCode::Down),
    help::key(
        "pgup pgdn g G",
        "the comments a page, or to an end",
        KeyCode::PageDown,
    ),
    help::key(
        "enter o",
        "open the item in the system viewer",
        KeyCode::Enter,
    ),
    help::key("d", "save the item here", KeyCode::Char('d')),
    help::key("D", "save every item of the post here", KeyCode::Char('D')),
    help::key("c", "show or hide the comments", KeyCode::Char('c')),
    help::key("n", "read more comments", KeyCode::Char('n')),
    help::key("w", "open the post on the web", KeyCode::Char('w')),
    help::key("esc backspace", "back to the grid, or leave", KeyCode::Esc),
    help::REDRAW,
    help::QUIT,
    help::STOP,
];

/// One post as the session holds it once opened.
struct Detail {
    post: Post,
    /// Read as it opened; `None` when reading them failed.
    comments: Option<Comments>,
    showing_comments: bool,
    /// The item on screen.
    item: usize,
    /// How far the comments are scrolled, in rows of the screen.
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

/// What is read on the first frame, under a box, for the command that asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum First {
    /// `snob posts someone 3`: open that post of the grid.
    Open(usize),
    /// `snob post LINK`: the comments of the one post.
    Comments,
}

/// How the keys ended a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Leave(ExitCode),
    /// Out of a grid another view opened, back to that view.
    Back,
}

/// What the last frame measured, for the keys that move by what is on
/// screen: a page of rows, and the columns a post's text is wrapped to.
#[derive(Debug, Default)]
struct Seen {
    page_rows: usize,
    text_width: usize,
    /// Whether the frame had room for a small picture beside its text: one
    /// is fetched only where it would be drawn.
    beside: bool,
}

/// Browses `grid`, opening post `start` (zero-based) on the first frame when
/// one was asked for.
pub async fn browse_grid(
    client: &IgClient,
    grid: Grid,
    start: Option<usize>,
    viewer: &Viewer,
    paths: &AppPaths,
) -> Result<ExitCode> {
    let mut session = Session::at_a_grid(client, grid, false);
    session.selected = start.unwrap_or(0);
    session.first = start.map(First::Open);
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
    let session = Session {
        client,
        grid: None,
        details: vec![Some(Detail::new(post, None))],
        level: Level::Post(0),
        selected: 0,
        note: String::new(),
        receipts: Vec::new(),
        lent: false,
        first: Some(First::Comments),
        gallery: preview::Gallery::new(),
        pictures: preview::Gallery::draws(),
    };
    session.run(viewer, paths).await
}

/// The same grid on a terminal somebody else holds.
///
/// The profile card enters here with its own guard and its own scratch
/// directory: a second of either is a second claim on the one terminal, and
/// a directory made on the path of the first, which takes the first one's
/// files with it when it goes. The receipts are the card's to print, after
/// its guard drops. `None` is back to the card; `Some` leaves the view with
/// that code.
pub(crate) async fn browse_grid_in(
    tui: &mut Tui,
    scratch: &Scratch,
    client: &IgClient,
    grid: Grid,
    viewer: &Viewer,
    receipts: &mut Vec<String>,
) -> Result<Option<ExitCode>> {
    let mut session = Session::at_a_grid(client, grid, true);
    let ended = session.run_in(tui, scratch, viewer).await;
    receipts.append(&mut session.receipts);
    Ok(match ended? {
        Ended::Back => None,
        Ended::Leave(code) => Some(code),
    })
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
    /// Whether the terminal is another view's, to go back to.
    lent: bool,
    first: Option<First>,
    /// The small pictures drawn beside the text, where this run draws any.
    gallery: preview::Gallery<'a>,
    pictures: bool,
}

impl<'a> Session<'a> {
    fn at_a_grid(client: &'a IgClient, grid: Grid, lent: bool) -> Self {
        let details = grid.posts.iter().map(|_| None).collect();
        Self {
            client,
            grid: Some(grid),
            details,
            level: Level::Grid,
            selected: 0,
            note: String::new(),
            receipts: Vec::new(),
            lent,
            first: None,
            gallery: preview::Gallery::new(),
            pictures: preview::Gallery::draws(),
        }
    }

    /// The address of the small picture of what the cursor is on: at the
    /// grid the selected post's first item, inside a post the item on
    /// screen, a video's being its cover (`posts::Item::thumb`).
    fn thumb(&self) -> Option<&str> {
        self.item_under_the_cursor()?.thumb.as_deref()
    }

    fn item_under_the_cursor(&self) -> Option<&posts::Item> {
        match self.level {
            Level::Grid => self.grid.as_ref()?.posts.get(self.selected)?.items.first(),
            Level::Post(index) => {
                let detail = self.detail(index)?;
                detail.post.items.get(detail.item)
            }
        }
    }

    /// What this frame draws beside its text.
    fn beside(&self) -> preview::Beside<'_> {
        preview::Beside {
            on: self.pictures,
            picture: self.thumb().and_then(|address| self.gallery.get(address)),
            cover: self
                .item_under_the_cursor()
                .is_some_and(|item| item.kind == Kind::Video),
        }
    }

    /// The session on a terminal and a scratch directory of its own.
    async fn run(mut self, viewer: &Viewer, paths: &AppPaths) -> Result<ExitCode> {
        snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
        let scratch = Scratch::new(paths.story_scratch())?;
        let mut tui = tui::claim_fullscreen(
            "--no-interactive prints the listing; --download saves without one",
        )?;

        // Every way out passes the receipts: see `tui::print_receipts`.
        let ended = self.run_in(&mut tui, &scratch, viewer).await;
        drop(tui);
        tui::print_receipts(&self.receipts);
        Ok(match ended? {
            Ended::Leave(code) => code,
            // Nothing lent this terminal, so there is nowhere back to.
            Ended::Back => ExitCode::Ok,
        })
    }

    /// The loop, on whichever terminal it was given.
    async fn run_in(&mut self, tui: &mut Tui, scratch: &Scratch, viewer: &Viewer) -> Result<Ended> {
        let colors = tui::colors_enabled();
        let mut list = TableState::default();
        let mut seen = Seen::default();

        if let Some(first) = self.first.take() {
            let left = match first {
                First::Open(index) => {
                    self.open_post(index, tui, &mut list, viewer, colors, &mut seen)
                        .await?
                }
                First::Comments => {
                    self.first_comments(tui, &mut list, viewer, colors, &mut seen)
                        .await?
                }
            };
            if let Some(code) = left {
                return Ok(Ended::Leave(code));
            }
        }

        let client = self.client;
        loop {
            self.draw(tui, &mut list, viewer, colors, &mut seen)?;
            // The small picture of what the cursor is on, where the frame
            // just drawn had room for one: fetched once the cursor has
            // rested, while the keys are read (`preview::Gallery`).
            let wanted = self
                .thumb()
                .filter(|_| self.pictures && seen.beside)
                .map(str::to_string);
            self.gallery.want(wanted.as_deref());
            let raw = self
                .gallery
                .read(|address| Box::pin(async move { client.download(&address).await.ok() }))
                .await
                .map_err(input::unreadable)?;
            let key = match raw {
                Raw::Key(key) => key_of(key),
                Raw::Resized | Raw::Paste(_) | Raw::Tick => continue,
            };
            self.note.clear();
            // On the heap, as every wait a view makes is (`Busy::run`): what
            // a key does is the largest future here, and this loop is inside
            // a command's.
            let acted =
                Box::pin(self.act(key, tui, &mut list, viewer, colors, &mut seen, scratch)).await?;
            if let Some(ended) = acted {
                return Ok(ended);
            }
        }
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
        seen: &mut Seen,
        scratch: &Scratch,
    ) -> Result<Option<Ended>> {
        match key {
            Key::Shared(Action::Interrupt) => {
                return Ok(Some(Ended::Leave(ExitCode::Interrupted)));
            }
            Key::Shared(Action::Quit) => return Ok(Some(Ended::Leave(ExitCode::Ok))),
            Key::Shared(Action::Redraw) => {
                tui.terminal.clear()?;
                return Ok(None);
            }
            Key::Shared(Action::Help) => {
                let (title, keys) = match self.level {
                    Level::Grid => ("Posts", GRID_KEYS.to_vec()),
                    Level::Post(_) => ("A post", preview::with_key(POST_KEYS)),
                };
                help::show(tui, title, &keys, |frame| {
                    self.frame(frame, list, viewer, colors, seen);
                })?;
                return Ok(None);
            }
            Key::Shared(Action::Web) => {
                let post = match self.level {
                    Level::Grid => self.grid.as_ref().and_then(|g| g.posts.get(self.selected)),
                    Level::Post(index) => self.detail(index).map(|detail| &detail.post),
                };
                if let Some(link) = post.map(Post::link) {
                    self.note = tui::web_note(&link, &mut self.receipts);
                }
                return Ok(None);
            }
            _ => {}
        }
        let left = match self.level {
            Level::Grid => {
                return self
                    .act_at_the_grid(key, tui, list, viewer, colors, seen)
                    .await;
            }
            Level::Post(index) => {
                self.act_in_a_post(index, key, tui, list, viewer, colors, seen, scratch)
                    .await?
            }
        };
        Ok(left)
    }

    async fn act_at_the_grid(
        &mut self,
        key: Key,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
    ) -> Result<Option<Ended>> {
        let total = self.grid.as_ref().map_or(0, |g| g.posts.len());
        let last = total.saturating_sub(1);
        let before = self.selected;
        match key {
            Key::Shared(Action::Up) => self.selected = self.selected.saturating_sub(1),
            Key::Shared(Action::Down) => self.selected = (self.selected + 1).min(last),
            Key::Shared(Action::PageUp) => {
                self.selected = self.selected.saturating_sub(page(seen.page_rows));
            }
            Key::Shared(Action::PageDown) => {
                self.selected = (self.selected + page(seen.page_rows)).min(last);
            }
            Key::Shared(Action::First) => self.selected = 0,
            Key::Shared(Action::Last) => self.selected = last,
            Key::Open | Key::Next | Key::Shared(Action::Open) => {
                let index = self.selected;
                let left = self
                    .open_post(index, tui, list, viewer, colors, seen)
                    .await?;
                if let Some(code) = left {
                    return Ok(Some(Ended::Leave(code)));
                }
            }
            Key::SaveOne | Key::SaveAll | Key::Shared(Action::Download) => {
                let Some(post) = self.grid.as_ref().and_then(|g| g.posts.get(self.selected)) else {
                    return Ok(None);
                };
                let post = post.clone();
                let left = self
                    .keep_all(&post, tui, list, viewer, colors, seen)
                    .await?;
                if let Some(code) = left {
                    return Ok(Some(Ended::Leave(code)));
                }
            }
            Key::About => {
                if let Some(post) = self.grid.as_ref().and_then(|g| g.posts.get(self.selected)) {
                    show_about(tui, post, |frame| {
                        self.frame(frame, list, viewer, colors, seen);
                    })?;
                }
            }
            Key::Back => {
                return Ok(Some(if self.lent {
                    Ended::Back
                } else {
                    Ended::Leave(ExitCode::Ok)
                }));
            }
            // Left walks out of a sub-view of the card, as it does out of
            // its stories; a grid of its own has nowhere out but leaving,
            // which is not Left's to do.
            Key::Previous if self.lent => return Ok(Some(Ended::Back)),
            _ => {}
        }
        // Reaching the last row reads the next page, as scrolling the
        // app's grid to its end does.
        let at_the_end = total > 0 && self.selected + 1 == total && before + 1 != total;
        if at_the_end && self.grid.as_ref().is_some_and(|g| g.next.is_some()) {
            let busy = Busy::begin(tui, "Reading the next page...", |frame| {
                self.frame(frame, list, viewer, colors, seen);
            })?;
            let client = self.client;
            let Some(grid) = self.grid.as_mut() else {
                return Ok(None);
            };
            let (read, stopped) = busy
                .run(
                    tui,
                    client.pacer().cancel_token(),
                    posts::more(client, grid),
                )
                .await;
            if let Some(code) = stopped.leave() {
                return Ok(Some(Ended::Leave(code)));
            }
            match read {
                Ok(_) => {
                    let read = self.grid.as_ref().map_or(0, |g| g.posts.len());
                    self.details.resize_with(read, || None);
                }
                Err(e) => self.note = format!("Could not read more: {e}"),
            }
        }
        Ok(None)
    }

    #[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
    async fn act_in_a_post(
        &mut self,
        index: usize,
        key: Key,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
        scratch: &Scratch,
    ) -> Result<Option<Ended>> {
        let leave = |code| Ok(Some(Ended::Leave(code)));
        let client = self.client;
        let Some(detail) = self.detail_mut(index) else {
            self.level = Level::Grid;
            return Ok(None);
        };
        let last_item = detail.post.items.len().saturating_sub(1);
        let at = detail.item;
        // The comments scroll until their last row is the screen's last.
        let furthest = furthest(detail, seen);
        let a_page = page(seen.page_rows);
        match key {
            Key::Previous => detail.item = detail.item.saturating_sub(1),
            Key::Next => detail.item = (detail.item + 1).min(last_item),
            Key::Shared(Action::Up) if detail.showing_comments => {
                detail.scroll = detail.scroll.min(furthest).saturating_sub(1);
            }
            Key::Shared(Action::Down) if detail.showing_comments => {
                detail.scroll = (detail.scroll + 1).min(furthest);
            }
            Key::Shared(Action::PageUp) if detail.showing_comments => {
                detail.scroll = detail.scroll.min(furthest).saturating_sub(a_page);
            }
            Key::Shared(Action::PageDown) if detail.showing_comments => {
                detail.scroll = (detail.scroll + a_page).min(furthest);
            }
            Key::Shared(Action::First) if detail.showing_comments => detail.scroll = 0,
            Key::Shared(Action::Last) if detail.showing_comments => detail.scroll = furthest,
            Key::Shared(Action::Up) => detail.item = detail.item.saturating_sub(1),
            Key::Shared(Action::Down) => detail.item = (detail.item + 1).min(last_item),
            Key::Shared(Action::First) => detail.item = 0,
            Key::Shared(Action::Last) => detail.item = last_item,
            Key::Open | Key::Shared(Action::Open) => {
                let busy = Busy::begin(tui, format!("Opening item {}...", at + 1), |frame| {
                    self.frame(frame, list, viewer, colors, seen);
                })?;
                let Some(detail) = self.detail_mut(index) else {
                    return Ok(None);
                };
                let (result, stopped) = busy
                    .run(
                        tui,
                        client.pacer().cancel_token(),
                        posts::open_item(
                            client,
                            &detail.post,
                            at,
                            scratch.dir(),
                            &mut detail.opened,
                        ),
                    )
                    .await;
                self.note = tui::opened_note(result);
                if let Some(code) = stopped.leave() {
                    return leave(code);
                }
            }
            Key::SaveOne | Key::Shared(Action::Download) => {
                let busy = Busy::begin(tui, format!("Saving item {}...", at + 1), |frame| {
                    self.frame(frame, list, viewer, colors, seen);
                })?;
                let Some(detail) = self.detail_mut(index) else {
                    return Ok(None);
                };
                let (result, stopped) = busy
                    .run(
                        tui,
                        client.pacer().cancel_token(),
                        posts::keep_item(client, &detail.post, at, &detail.opened),
                    )
                    .await;
                let said = result.as_ref().ok().and_then(|kept| kept.said.clone());
                self.note = tui::saved_note(result.map(|kept| kept.path), &mut self.receipts);
                if let Some(said) = said {
                    self.note = format!("{}; {said}", self.note);
                    self.receipts.push(said);
                }
                if let Some(code) = stopped.leave() {
                    return leave(code);
                }
            }
            Key::SaveAll => {
                let post = detail.post.clone();
                let left = self
                    .keep_all(&post, tui, list, viewer, colors, seen)
                    .await?;
                if let Some(code) = left {
                    return leave(code);
                }
            }
            Key::Shared(Action::Preview) => {
                let kind = detail.post.items.get(at).map(|item| item.kind);
                if let Some(why) = preview::refused(kind) {
                    self.note = why;
                    return Ok(None);
                }
                let busy = Busy::begin(tui, format!("Fetching item {}...", at + 1), |frame| {
                    self.frame(frame, list, viewer, colors, seen);
                })?;
                let Some(detail) = self.detail_mut(index) else {
                    return Ok(None);
                };
                let (file, stopped) = busy
                    .run(
                        tui,
                        client.pacer().cancel_token(),
                        posts::fetched_item(
                            client,
                            &detail.post,
                            at,
                            scratch.dir(),
                            &mut detail.opened,
                        ),
                    )
                    .await;
                if let Some(code) = stopped.leave() {
                    return leave(code);
                }
                let note = preview::show_file(tui, file, |frame| {
                    self.frame(frame, list, viewer, colors, seen);
                })?;
                self.note = note;
            }
            Key::Comments => {
                detail.showing_comments = !detail.showing_comments;
                detail.scroll = 0;
                if detail.comments.is_none() {
                    self.note = "The comments could not be read when it opened".into();
                }
            }
            Key::MoreComments => {
                // Said, as `c` says it: a key that answers nothing reads as
                // one that did not arrive.
                let Some(comments) = detail.comments.as_ref() else {
                    self.note = "The comments could not be read when it opened".into();
                    return Ok(None);
                };
                if comments.next.is_none() {
                    self.note = "No more comments".into();
                    return Ok(None);
                }
                detail.showing_comments = true;
                let busy = Busy::begin(tui, "Reading more comments...", |frame| {
                    self.frame(frame, list, viewer, colors, seen);
                })?;
                let Some(detail) = self.detail_mut(index) else {
                    return Ok(None);
                };
                let Some(comments) = detail.comments.as_mut() else {
                    return Ok(None);
                };
                let (read, stopped) = busy
                    .run(
                        tui,
                        client.pacer().cancel_token(),
                        posts::more_comments(client, &detail.post, comments),
                    )
                    .await;
                if let Some(code) = stopped.leave() {
                    return leave(code);
                }
                if let Err(e) = read {
                    self.note = format!("Could not read more comments: {e}");
                }
            }
            Key::Back | Key::Shared(Action::Back) => {
                if self.grid.is_none() {
                    return leave(ExitCode::Ok);
                }
                self.level = Level::Grid;
                *list = TableState::default();
            }
            _ => {}
        }
        Ok(None)
    }

    fn detail(&self, index: usize) -> Option<&Detail> {
        self.details.get(index).and_then(Option::as_ref)
    }

    fn detail_mut(&mut self, index: usize) -> Option<&mut Detail> {
        self.details.get_mut(index).and_then(Option::as_mut)
    }

    /// Opens post `index` of the grid: the app's two reads the first time,
    /// under a box that says so, nothing after. A post that could not be
    /// read stays at the grid, the reason in the note line. `Some` is the
    /// way out.
    async fn open_post(
        &mut self,
        index: usize,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
    ) -> Result<Option<ExitCode>> {
        if self.detail(index).is_some() {
            self.level = Level::Post(index);
            return Ok(None);
        }
        let Some(post) = self.grid.as_ref().and_then(|g| g.posts.get(index)).cloned() else {
            return Ok(None);
        };
        let busy = Busy::begin(tui, format!("Opening post {}...", index + 1), |frame| {
            self.frame(frame, list, viewer, colors, seen);
        })?;
        let client = self.client;
        let (opened, stopped) = busy
            .run(
                tui,
                client.pacer().cancel_token(),
                posts::open(client, &post),
            )
            .await;
        if let Some(code) = stopped.leave() {
            return Ok(Some(code));
        }
        match opened {
            Ok((post, comments)) => {
                if self.details.len() <= index {
                    self.details.resize_with(index + 1, || None);
                }
                self.details[index] = Some(Detail::new(post, Some(comments)));
                self.level = Level::Post(index);
            }
            Err(e) => self.note = format!("Could not open it: {e}"),
        }
        Ok(None)
    }

    /// The comments of the one post a link named, read under a box over the
    /// post, which is already on screen without them. `Some` is the way out.
    async fn first_comments(
        &mut self,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
    ) -> Result<Option<ExitCode>> {
        let busy = Busy::begin(tui, "Reading the comments...", |frame| {
            self.frame(frame, list, viewer, colors, seen);
        })?;
        let client = self.client;
        let Some(detail) = self.detail_mut(0) else {
            return Ok(None);
        };
        let (comments, stopped) = busy
            .run(
                tui,
                client.pacer().cancel_token(),
                posts::first_comments(client, &detail.post),
            )
            .await;
        if let Some(code) = stopped.leave() {
            return Ok(Some(code));
        }
        match comments {
            Ok(comments) => detail.comments = Some(comments),
            Err(e) => self.note = format!("Could not read the comments: {e}"),
        }
        Ok(None)
    }

    /// D: every item of `post` where the user works. `Some` is the way out.
    async fn keep_all(
        &mut self,
        post: &Post,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
    ) -> Result<Option<ExitCode>> {
        let busy = Busy::begin(tui, "Saving every item of the post...", |frame| {
            self.frame(frame, list, viewer, colors, seen);
        })?;
        let client = self.client;
        let (kept, stopped) = busy
            .run(
                tui,
                client.pacer().cancel_token(),
                posts::keep_post(client, post),
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
        Ok(stopped.leave())
    }

    fn draw(
        &self,
        tui: &mut Tui,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
    ) -> Result<()> {
        tui.terminal
            .draw(|frame| self.frame(frame, list, viewer, colors, seen))?;
        Ok(())
    }

    /// One frame of the level the browser is at.
    fn frame(
        &self,
        frame: &mut Frame<'_>,
        list: &mut TableState,
        viewer: &Viewer,
        colors: bool,
        seen: &mut Seen,
    ) {
        match self.level {
            Level::Grid => {
                if let Some(grid) = &self.grid {
                    list.select(Some(self.selected));
                    draw_grid(
                        frame,
                        grid,
                        self.lent,
                        viewer,
                        list,
                        &self.note,
                        colors,
                        self.beside(),
                        seen,
                    );
                }
            }
            Level::Post(index) => {
                if let Some(detail) = self.detail(index) {
                    let out = match (&self.grid, self.lent) {
                        (Some(_), _) => "esc back",
                        (None, _) => "esc leave",
                    };
                    draw_post(
                        frame,
                        detail,
                        out,
                        viewer,
                        &self.note,
                        colors,
                        self.beside(),
                        seen,
                    );
                }
            }
        }
    }
}

/// The least width the caption keeps before a column is given up for it.
const CAPTION_MIN: u16 = 20;

/// Which of the grid's columns a width has room for. The number, the kind
/// and the caption are always there; the rest go from the right, the
/// comments first, then the likes, then the date, as the account list's do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    posted: bool,
    likes: bool,
    comments: bool,
}

/// The selection's marker, then each column with the two blank columns
/// before the next: the number, the kind.
const ALWAYS_W: u16 = 2 + (4 + 2) + (9 + 2);
const POSTED_W: u16 = 13 + 2;
const COUNT_W: u16 = 9 + 2;

fn columns(width: u16) -> Columns {
    let all = ALWAYS_W + POSTED_W + 2 * COUNT_W + CAPTION_MIN;
    Columns {
        comments: width >= all,
        likes: width >= all - COUNT_W,
        posted: width >= all - 2 * COUNT_W,
    }
}

/// From this many columns the grid has a panel beside it that says all the
/// selected post does. Under it the list needs the room, and Space opens
/// the same in a box.
const PANEL_FROM: u16 = 110;

/// The panel's width, its left border and padding in it: about a third of
/// the least terminal that has one.
const PANEL_W: u16 = 40;

/// Draws the grid: one post a row, in named columns, and on a wide terminal
/// what the selected one says beside it.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_grid(
    frame: &mut Frame<'_>,
    grid: &Grid,
    lent: bool,
    viewer: &Viewer,
    list: &mut TableState,
    note: &str,
    colors: bool,
    beside: preview::Beside<'_>,
    seen: &mut Seen,
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
        // `Grid::username` is kept as Instagram spells it, for the requests
        // that page on; on screen it goes through `printable` like every
        // other string from another person.
        format!(
            "Posts · @{} · {total} read{more}",
            printable(&grid.username)
        ),
        colors,
        tui::list_padding(area),
    );
    let inner = block.inner(area);
    let viewport = (inner.height as usize).saturating_sub(1).max(1);
    seen.page_rows = viewport;
    let fits = total <= viewport;
    block = tui::top_right(
        block,
        (!fits).then(|| tui::position_line(selected, total, colors)),
        tui::viewer_line(viewer, colors),
    );
    let out = if lent { "esc back" } else { "q quit" };
    block = block.title_bottom(tui::footer(
        note,
        &format!("↑↓ move · enter open · d save all · space about · ? keys · {out}"),
        colors,
    ));
    frame.render_widget(block, area);

    let (table, panel) = if area.width >= PANEL_FROM {
        let [table, panel] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(PANEL_W)]).areas(inner);
        (table, Some(panel))
    } else {
        (inner, None)
    };
    let shown = columns(table.width);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), selected, total, viewport, 2);
    let dim = tui::paint(colors, tui::DIM);
    let rows = grid.posts.iter().enumerate().map(|(index, post)| {
        let likes = if post.likes_hidden {
            "-".to_string()
        } else {
            post.likes.map_or("-".into(), posts::grouped)
        };
        let mut cells = vec![
            Cell::from(
                Line::from(format!("{}.", index + 1))
                    .right_aligned()
                    .style(dim),
            ),
            Cell::from(post.kind.label()),
        ];
        if shown.posted {
            cells.push(Cell::from(report::dated(post.taken_at)).style(dim));
        }
        if shown.likes {
            cells.push(Cell::from(Line::from(likes).right_aligned()));
        }
        if shown.comments {
            cells.push(Cell::from(
                Line::from(post.comments.map_or("-".into(), posts::grouped)).right_aligned(),
            ));
        }
        cells.push(Cell::from(clipped(post.headline(), 80)));
        Row::new(cells)
    });
    let mut widths = vec![Constraint::Length(4), Constraint::Length(9)];
    let mut names = vec!["", "kind"];
    for (there, width, name) in [
        (shown.posted, 13, "posted"),
        (shown.likes, 9, "likes"),
        (shown.comments, 9, "comments"),
    ] {
        if there {
            widths.push(Constraint::Length(width));
            names.push(name);
        }
    }
    widths.push(Constraint::Fill(1));
    names.push("caption");
    frame.render_stateful_widget(
        tui::list_table(rows, widths).header(tui::header_row(&names, colors)),
        table,
        list,
    );
    if !fits {
        tui::scrollbar(frame, area, total, selected, viewport as u16);
    }

    if let (Some(panel), Some(post)) = (panel, grid.posts.get(selected)) {
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(dim)
            .padding(Padding::horizontal(1));
        let within = block.inner(panel);
        frame.render_widget(block, panel);
        // The picture has a place of its own at the top of the panel, kept
        // whether or not it has arrived, so that the text under it does not
        // jump as the cursor moves: as tall as a square picture the panel's
        // width, and never more than half the panel.
        let slot = (within.width / 2).min(within.height / 2);
        seen.beside = beside.on && slot >= PICTURE_MIN_ROWS;
        let text = if seen.beside {
            let [picture, _, text] = Layout::vertical([
                Constraint::Length(slot),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(within);
            if let Some(pixels) = beside.picture {
                pixels.draw_in(frame, picture);
            }
            text
        } else {
            within
        };
        frame.render_widget(
            Paragraph::new(about_lines(
                post,
                text.width as usize,
                text.height as usize,
                colors,
            )),
            text,
        );
    } else {
        seen.beside = false;
    }
}

/// The fewest rows a small picture is drawn in: under it, at two pixels a
/// row, there is nothing left to recognize.
const PICTURE_MIN_ROWS: u16 = 6;

/// The fewest columns a picture beside a post is drawn in: a column is a
/// pixel, and under two dozen across nothing is recognized.
const PICTURE_MIN_W: u16 = 24;

/// The fewest columns a post's text keeps when a picture is drawn beside
/// it: a line of a caption is still read as a line.
const TEXT_MIN_W: u16 = 60;

/// What a post says, a line each, styled as the post's own view styles it:
/// whose it is in bold, when and where it is on the web dim.
fn about_styled(post: &Post, colors: bool) -> Vec<Line<'static>> {
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    about(post)
        .into_iter()
        .enumerate()
        .map(|(i, line)| match i {
            0 => Line::from(line).style(bold),
            1 | 2 => Line::from(line).style(dim),
            _ => Line::from(line),
        })
        .collect()
}

/// [`about_styled`] wrapped to `width` and cut to `rows`, the last of them
/// an ellipsis when there was more: the panel beside the grid and the box
/// Space opens. From what the grid read; nothing is fetched for it.
fn about_lines(post: &Post, width: usize, rows: usize, colors: bool) -> Vec<Line<'static>> {
    let mut lines = visual(about_styled(post, colors), width);
    // A caption ends with a blank line when nothing follows it.
    while lines.last().is_some_and(|line| line.width() == 0) {
        lines.pop();
    }
    if lines.len() > rows {
        lines.truncate(rows);
        if let Some(last) = lines.last_mut() {
            *last = Line::from("…").style(tui::paint(colors, tui::DIM));
        }
    }
    lines
}

/// The widest the box Space opens grows: prose is read, not scanned.
const ABOUT_MAX_W: u16 = 72;

/// The box Space opens over the grid: all the selected post says.
fn about_modal(frame: &mut Frame<'_>, post: &Post, colors: bool) {
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    tui::veil(frame, colors);
    let screen = frame.area();
    let width = screen.width.min(ABOUT_MAX_W);
    let text_w = width.saturating_sub(2 + 4).max(8) as usize;
    let most = screen.height.saturating_sub(2 + 2) as usize;
    let lines = about_lines(post, text_w, most, colors);
    let area = tui::modal_area(screen, width, lines.len() as u16 + 2 + 2);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(tui::card_padding())
        .title_top(Line::from(Span::styled(format!(" {} ", post.code), bold)))
        .title_bottom(Line::from(Span::styled(" any key closes ", dim)).centered());
    let inner = tui::open_modal(frame, area, block);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Space: [`about_modal`] over the view `under` draws, until a key is
/// pressed. The key is spent closing it, as in `help::show`.
fn show_about(tui: &mut Tui, post: &Post, mut under: impl FnMut(&mut Frame<'_>)) -> Result<()> {
    let colors = tui::colors_enabled();
    loop {
        tui.terminal.draw(|frame| {
            under(frame);
            about_modal(frame, post, colors);
        })?;
        if let Raw::Key(_) = input::read(input::tick()).map_err(input::unreadable)? {
            return Ok(());
        }
    }
}

/// The rows `lines` take at `width` columns, each line that is too wide
/// wrapped at its blanks under its own indent, in its own style.
///
/// **Wrapped here, and not by the paragraph that draws them**: a paragraph
/// wraps as it draws and tells nobody how many rows that made, so a scroll
/// bounded by the lines that went in stops short of the bottom as soon as
/// one comment takes two rows. With the rows counted before they are drawn
/// the comments scroll to their last row and stop ([`furthest`]).
fn visual(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            rows.push(line);
            continue;
        }
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // A line of one span keeps the span's style; the content here never
        // builds another kind, and a line that did would keep its own.
        let style = match line.spans.as_slice() {
            [only] => line.style.patch(only.style),
            _ => line.style,
        };
        let body = text.trim_start_matches(' ');
        let indent = " ".repeat(text.len() - body.len());
        let room = width.saturating_sub(indent.len()).max(1);
        for piece in tui::wrapped(body, room) {
            rows.push(Line::from(format!("{indent}{piece}")).style(style));
        }
    }
    rows
}

/// How far a post's text scrolls: until its last row is the screen's last.
/// By what the last frame measured, which is what the person is looking at.
fn furthest(detail: &Detail, seen: &Seen) -> usize {
    visual(post_lines(detail, false), seen.text_width)
        .len()
        .saturating_sub(seen.page_rows.max(1))
}

/// Draws one post: what it says, its items with the one on screen marked,
/// and its comments when they are shown. `out` is what Esc is called here.
///
/// With the comments hidden nothing scrolls by a key, and the arrows move
/// between the items: the item on screen is kept in view, even under a
/// caption taller than the terminal, which would otherwise push the items
/// under the bottom edge with no key to reach them.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_post(
    frame: &mut Frame<'_>,
    detail: &Detail,
    out: &str,
    viewer: &Viewer,
    note: &str,
    colors: bool,
    beside: preview::Beside<'_>,
    seen: &mut Seen,
) {
    let post = &detail.post;
    let area = frame.area();
    let mut block = tui::view_block(
        format!("@{} · {}", post.owner, post.code),
        colors,
        tui::card_padding(),
    );
    let whole = block.inner(area);
    // The item on screen is drawn to the right of the text where there is
    // room for both: as wide as a square picture the view's height, half
    // the view at most, and never so wide that the text has under
    // `TEXT_MIN_W` columns left.
    let picture_w = (whole.height * 2)
        .min(whole.width / 2)
        .min(whole.width.saturating_sub(TEXT_MIN_W + 2));
    seen.beside = beside.on && picture_w >= PICTURE_MIN_W;
    let (inner, picture) = if seen.beside {
        let [text, _, picture] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Length(picture_w),
        ])
        .areas(whole);
        (text, Some(picture))
    } else {
        (whole, None)
    };
    seen.page_rows = inner.height as usize;
    seen.text_width = inner.width as usize;

    // Wrapped a line at a time, so that the row the item on screen starts
    // on is known: its line comes after what the post says and the blank
    // under it (`post_lines`).
    let item_line = about(post).len() + 1 + detail.item;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut item_row = 0usize;
    for (index, line) in post_lines(detail, colors).into_iter().enumerate() {
        if index == item_line {
            item_row = lines.len();
        }
        lines.extend(visual(vec![line], inner.width as usize));
    }
    let viewport = inner.height as usize;
    let overflow = lines.len().saturating_sub(viewport);
    let scroll = if detail.showing_comments {
        detail.scroll.min(overflow)
    } else {
        item_row
            .saturating_sub(viewport.saturating_sub(1))
            .min(overflow)
    };
    let scrolls = detail.showing_comments && overflow > 0;
    block = tui::top_right(
        block,
        scrolls.then(|| tui::position_line(scroll, overflow + 1, colors)),
        tui::viewer_line(viewer, colors),
    );
    block = block.title_bottom(tui::footer(
        note,
        &format!("←→ item · o open · d save · c comments · n more · ? keys · {out}"),
        colors,
    ));
    frame.render_widget(block, area);

    let shown: Vec<Line<'static>> = lines
        .into_iter()
        .skip(scroll)
        .take(inner.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
    if scrolls {
        // The bar's travel is the offsets there are, the first to the last.
        tui::scrollbar(frame, area, overflow + 1, scroll, inner.height);
    }
    if let Some(area) = picture {
        beside.draw_in(frame, area, colors);
    }
}

/// The lines of one post as [`draw_post`] shows it.
fn post_lines(detail: &Detail, colors: bool) -> Vec<Line<'static>> {
    let post = &detail.post;
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    let mut lines = about_styled(post, colors);
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

    fn comment(author: &str, text: &str) -> CommentLine {
        CommentLine {
            author: author.into(),
            text: text.into(),
            at: Epoch::new(1_790_510_400),
            likes: 0,
            replies_count: 0,
            replies: Vec::new(),
            pk: None,
            author_pk: None,
        }
    }

    fn comments(lines: Vec<CommentLine>) -> Comments {
        Comments {
            total: Some(lines.len() as u64),
            lines,
            next: None,
        }
    }

    fn screen_of(terminal: &Terminal<TestBackend>) -> String {
        let height = terminal.backend().buffer().area.height;
        (0..height)
            .map(|y| tui::row_text(terminal, y) + "\n")
            .collect()
    }

    fn grid_drawn(width: u16, height: u16, lent: bool) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut seen = Seen::default();
        terminal
            .draw(|frame| {
                draw_grid(
                    frame,
                    &grid(),
                    lent,
                    &tui::me(),
                    &mut list,
                    "",
                    false,
                    preview::Beside::default(),
                    &mut seen,
                );
            })
            .unwrap();
        terminal
    }

    #[test]
    fn the_grid_draws_a_post_a_row() {
        let terminal = grid_drawn(100, 8, false);
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
        let edge = tui::row_text(&terminal, 7);
        assert!(edge.contains("enter open"), "{edge}");
        assert!(edge.contains("? keys · q quit"), "{edge}");
        // On a terminal the card lent, Esc is back and says so.
        let edge = tui::row_text(&grid_drawn(100, 8, true), 7);
        assert!(edge.contains("esc back"), "{edge}");
    }

    /// The grid keeps the name as Instagram spells it, for the requests; the
    /// title it draws is the name through `printable`, so a bidirectional
    /// override in it cannot turn the frame's text around.
    #[test]
    fn the_grid_title_draws_the_name_through_printable() {
        let mut grid = grid();
        grid.username = "some\u{202e}one".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut seen = Seen::default();
        terminal
            .draw(|frame| {
                draw_grid(
                    frame,
                    &grid,
                    false,
                    &tui::me(),
                    &mut list,
                    "",
                    false,
                    preview::Beside::default(),
                    &mut seen,
                );
            })
            .unwrap();
        let title = tui::row_text(&terminal, 0);
        assert!(title.contains("Posts · @someone · 2 read"), "{title}");
    }

    /// A narrow grid gives up its columns from the right, the comments
    /// first, and keeps the number, the kind and the caption to the end.
    #[test]
    fn a_narrow_grid_gives_up_its_columns_from_the_right() {
        let all = Columns {
            posted: true,
            likes: true,
            comments: true,
        };
        // 80 columns of terminal are 76 of table: everything, as before.
        assert_eq!(columns(76), all);
        assert_eq!(columns(200), all);
        assert_eq!(
            columns(75),
            Columns {
                comments: false,
                ..all
            }
        );
        assert_eq!(
            columns(64),
            Columns {
                posted: true,
                likes: false,
                comments: false
            }
        );
        assert_eq!(
            columns(53),
            Columns {
                posted: false,
                likes: false,
                comments: false
            }
        );

        let header = tui::row_text(&grid_drawn(80, 8, false), 1);
        assert!(header.contains("comments"), "{header}");
        let terminal = grid_drawn(54, 8, false);
        let header = tui::row_text(&terminal, 1);
        assert!(header.contains("kind"), "{header}");
        assert!(header.contains("caption"), "{header}");
        assert!(!header.contains("likes"), "{header}");
        assert!(!header.contains("posted"), "{header}");
        let first = tui::row_text(&terminal, 2);
        assert!(
            first.contains("2 items") && first.contains("sun"),
            "{first}"
        );
    }

    /// A wide terminal says all the selected post does beside the list, from
    /// what the grid read; a narrower one has the list alone.
    #[test]
    fn a_wide_grid_says_what_the_selected_post_does_beside_it() {
        let wide = screen_of(&grid_drawn(120, 12, false));
        assert!(wide.contains("@someone · 2 items"), "{wide}");
        assert!(wide.contains("instagram.com/p/"), "{wide}");
        assert!(wide.contains("Liked by close and 79 others"), "{wide}");
        let narrow = screen_of(&grid_drawn(109, 12, false));
        assert!(!narrow.contains("Liked by close"), "{narrow}");

        // Space says the same in a box, at any width.
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| about_modal(frame, &post(), false))
            .unwrap();
        let boxed = screen_of(&terminal);
        assert!(boxed.contains("DdzEShhEWCR"), "{boxed}");
        assert!(boxed.contains("Liked by close and 79 others"), "{boxed}");
        assert!(boxed.contains("any key closes"), "{boxed}");
        // And what does not fit the rows it has ends in an ellipsis.
        let cut = about_lines(&post(), 40, 2, false);
        assert_eq!(cut.len(), 2);
        assert_eq!(cut[1].to_string(), "…");
    }

    #[test]
    fn a_post_shows_what_it_says_its_items_and_its_comments() {
        let mut detail = Detail::new(post(), Some(comments(vec![comment("friend", "nice")])));
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
        let mut seen = Seen::default();
        terminal
            .draw(|frame| {
                draw_post(
                    frame,
                    &detail,
                    "esc back",
                    &tui::me(),
                    "",
                    false,
                    preview::Beside::default(),
                    &mut seen,
                );
            })
            .unwrap();
        assert!(tui::row_text(&terminal, 0).contains("@someone · DdzEShhEWCR"));
        assert!(tui::row_text(&terminal, 19).contains("esc back"));
        assert!(tui::row_text(&terminal, 19).contains("? keys"));
        assert_eq!((seen.page_rows, seen.text_width), (16, 94));
    }

    /// With the comments hidden the item on screen stays in view under a
    /// caption taller than the screen, and the first item of a post that
    /// fits is drawn from its top as before.
    #[test]
    fn the_item_on_screen_stays_in_view_under_a_long_caption() {
        let draw = |detail: &Detail, seen: &mut Seen| {
            let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
            terminal
                .draw(|frame| {
                    draw_post(
                        frame,
                        detail,
                        "esc back",
                        &tui::me(),
                        "",
                        false,
                        preview::Beside::default(),
                        seen,
                    );
                })
                .unwrap();
            screen_of(&terminal)
        };
        let mut long = post();
        long.caption = (0..40).map(|i| format!("caption line {i} ")).collect();
        let mut detail = Detail::new(long, None);
        detail.item = 1;
        let mut seen = Seen::default();
        let screen = draw(&detail, &mut seen);
        assert!(screen.contains("> 2. video 720×1280"), "{screen}");
        assert!(
            furthest(&detail, &seen) > 0,
            "forty lines of caption do not fit in sixteen rows"
        );
        // The first item is in view too, and so is the one before the last.
        detail.item = 0;
        let screen = draw(&detail, &mut seen);
        assert!(screen.contains("> 1. photo 1080×1350"), "{screen}");

        // A post that fits is drawn from its top, whichever item is marked.
        let mut short = Detail::new(post(), None);
        short.item = 1;
        let screen = draw(&short, &mut seen);
        assert!(screen.contains("@someone · 2 items"), "{screen}");
        assert!(screen.contains("> 2. video 720×1280"), "{screen}");
    }

    /// A line too wide for the screen is wrapped under its own indent, in
    /// its own style, and a line that fits is left exactly as it was.
    #[test]
    fn a_long_line_wraps_under_its_own_indent() {
        let dim = tui::DIM;
        let lines = vec![
            Line::from("short"),
            Line::from("    ↳ @friend: one two three four five").style(dim),
            Line::from("> 1. photo"),
        ];
        let rows = visual(lines, 20);
        let text: Vec<String> = rows.iter().map(Line::to_string).collect();
        assert_eq!(
            text,
            [
                "short",
                "    ↳ @friend: one",
                "    two three four",
                "    five",
                "> 1. photo"
            ]
        );
        assert_eq!(rows[2].style, dim, "the rows it became keep its style");
        assert_eq!(rows[0].style, ratatui::style::Style::default());
        for row in &rows {
            assert!(row.width() <= 20, "{row}");
        }
    }

    /// The comments scroll until their last row is on the screen's last
    /// row, and no further. Counted in lines as they went in, which is what
    /// bounded the scroll before, the bottom was out of reach as soon as
    /// the comments wrapped.
    #[test]
    fn the_comments_scroll_to_their_last_row_and_no_further() {
        let long = "word ".repeat(60);
        let said: Vec<CommentLine> = (1..=6)
            .map(|n| comment(&format!("friend{n}"), long.trim()))
            .chain([comment("last", "the end of it")])
            .collect();
        let mut detail = Detail::new(post(), Some(comments(said)));
        detail.showing_comments = true;

        let draw = |detail: &Detail, seen: &mut Seen| {
            let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
            terminal
                .draw(|frame| {
                    draw_post(
                        frame,
                        detail,
                        "esc back",
                        &tui::me(),
                        "",
                        false,
                        preview::Beside::default(),
                        seen,
                    );
                })
                .unwrap();
            screen_of(&terminal)
        };
        let mut seen = Seen::default();
        let top = draw(&detail, &mut seen);
        assert!(top.contains("@someone · 2 items"), "{top}");
        assert!(!top.contains("the end of it"), "{top}");

        let furthest_row = furthest(&detail, &seen);
        let unwrapped = post_lines(&detail, false).len();
        assert!(
            furthest_row > unwrapped,
            "more rows to scroll than there are lines: {furthest_row} > {unwrapped}"
        );

        // Scrolled as far as it goes, the last comment is on the last row
        // of text; scrolled "further", it is the same frame.
        detail.scroll = furthest_row;
        let bottom = draw(&detail, &mut seen);
        let rows: Vec<&str> = bottom.lines().collect();
        assert!(rows[11].contains("@last: the end of it"), "{bottom}");
        detail.scroll = usize::MAX;
        assert_eq!(draw(&detail, &mut seen), bottom);
    }

    #[test]
    fn the_keys_inside_a_post_are_its_own() {
        let key = |code| key_of(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(key(KeyCode::Left), Key::Previous);
        assert_eq!(key(KeyCode::Right), Key::Next);
        assert_eq!(key(KeyCode::Char('h')), Key::Previous);
        assert_eq!(key(KeyCode::Char('l')), Key::Next);
        assert_eq!(key(KeyCode::Char('o')), Key::Open);
        assert_eq!(key(KeyCode::Enter), Key::Open);
        assert_eq!(key(KeyCode::Char('d')), Key::SaveOne);
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)),
            Key::SaveAll
        );
        assert_eq!(key(KeyCode::Char('c')), Key::Comments);
        assert_eq!(key(KeyCode::Char('n')), Key::MoreComments);
        assert_eq!(key(KeyCode::Char(' ')), Key::About);
        assert_eq!(key(KeyCode::Esc), Key::Back);
        assert_eq!(key(KeyCode::Char('q')), Key::Shared(Action::Quit));
        assert_eq!(key(KeyCode::Char('w')), Key::Shared(Action::Web));
        assert_eq!(key(KeyCode::Char('?')), Key::Shared(Action::Help));
        assert_eq!(key(KeyCode::Char('p')), Key::Shared(Action::Preview));
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Key::Shared(Action::Interrupt)
        );
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Key::Shared(Action::Redraw)
        );
        assert_eq!(
            key_of(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)),
            Key::Shared(Action::None),
            "a modified arrow belongs to somebody else"
        );
        // Kept for what comes next, and bound to nothing meanwhile.
        for free in ['r', 'L'] {
            assert_eq!(key(KeyCode::Char(free)), Key::Shared(Action::None));
        }
    }

    /// Every key the two boxes name is bound to something here.
    #[test]
    fn every_key_the_help_names_does_something() {
        for named in GRID_KEYS.iter().chain(POST_KEYS).chain([&preview::KEY]) {
            assert_ne!(
                key_of(named.press),
                Key::Shared(Action::None),
                "{}",
                named.keys
            );
        }
    }

    fn blocks(terminal: &Terminal<TestBackend>) -> usize {
        screen_of(terminal).matches('▀').count()
    }

    /// A wide grid keeps a place for the selected post's small picture over
    /// what the post says, drawn once it has arrived; the text is where it
    /// was either way. A run that draws no pictures has the text alone, as
    /// before, and so has a grid with no panel.
    #[test]
    fn a_wide_grid_draws_the_selected_posts_picture_over_what_it_says() {
        let picture = preview::Pixels::plain(320, 400, (200, 10, 10));
        let draw = |width: u16, beside: preview::Beside<'_>| {
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            let mut list = TableState::default();
            list.select(Some(0));
            let mut seen = Seen::default();
            terminal
                .draw(|frame| {
                    draw_grid(
                        frame,
                        &grid(),
                        false,
                        &tui::me(),
                        &mut list,
                        "",
                        true,
                        beside,
                        &mut seen,
                    );
                })
                .unwrap();
            (terminal, seen.beside)
        };
        let row_of = |terminal: &Terminal<TestBackend>, text: &str| {
            screen_of(terminal)
                .lines()
                .position(|line| line.contains(text))
                .unwrap_or_else(|| panic!("{}", screen_of(terminal)))
        };

        let on = preview::Beside {
            on: true,
            picture: None,
            cover: false,
        };
        let (waiting, room) = draw(120, on);
        assert!(room, "the frame has room for a picture");
        assert_eq!(blocks(&waiting), 0, "and none has arrived");
        let (arrived, _) = draw(
            120,
            preview::Beside {
                picture: Some(&picture),
                ..on
            },
        );
        assert!(blocks(&arrived) > 0, "{}", screen_of(&arrived));
        assert_eq!(
            row_of(&waiting, "@someone · 2 items"),
            row_of(&arrived, "@someone · 2 items"),
            "the text does not move when the picture arrives"
        );

        let (plain, room) = draw(120, preview::Beside::default());
        assert!(!room);
        assert_eq!(blocks(&plain), 0);
        assert!(
            row_of(&plain, "@someone · 2 items") < row_of(&waiting, "@someone · 2 items"),
            "with no pictures the text starts at the top"
        );
        let (narrow, room) = draw(100, on);
        assert!(!room, "no panel, no picture to fetch");
        assert_eq!(blocks(&narrow), 0);
    }

    /// Inside a post the item on screen is drawn to the right of the text
    /// where there is room for both, and the text is wrapped to what is
    /// left; a video's picture says it is its cover. A narrow terminal, or
    /// a run with no pictures, has the text alone at its whole width.
    #[test]
    fn a_post_draws_the_item_on_screen_beside_its_text() {
        let picture = preview::Pixels::plain(320, 400, (10, 200, 10));
        let mut detail = Detail::new(post(), None);
        let draw = |detail: &Detail, width: u16, beside: preview::Beside<'_>| {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let mut seen = Seen::default();
            terminal
                .draw(|frame| {
                    draw_post(
                        frame,
                        detail,
                        "esc back",
                        &tui::me(),
                        "",
                        true,
                        beside,
                        &mut seen,
                    );
                })
                .unwrap();
            (terminal, seen)
        };
        let shown = preview::Beside {
            on: true,
            picture: Some(&picture),
            cover: false,
        };

        let (wide, seen) = draw(&detail, 160, shown);
        assert!(seen.beside);
        assert!(blocks(&wide) > 0, "{}", screen_of(&wide));
        // 26 rows of text: a picture of 52 columns, and two between them.
        assert_eq!((seen.page_rows, seen.text_width), (26, 154 - 2 - 52));
        assert!(!screen_of(&wide).contains("the video's cover"));

        detail.item = 1;
        let (video, _) = draw(
            &detail,
            160,
            preview::Beside {
                cover: true,
                ..shown
            },
        );
        assert!(
            screen_of(&video).contains("the video's cover"),
            "{}",
            screen_of(&video)
        );

        let (narrow, seen) = draw(&detail, 80, shown);
        assert!(!seen.beside, "the text keeps its columns first");
        assert_eq!(blocks(&narrow), 0);
        assert_eq!(seen.text_width, 74);
        let (plain, seen) = draw(&detail, 160, preview::Beside::default());
        assert!(!seen.beside);
        assert_eq!(blocks(&plain), 0);
        assert_eq!(seen.text_width, 154);
    }
}
