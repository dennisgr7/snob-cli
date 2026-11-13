//! The interactive profile: the account's card, and everything on it opens.
//!
//! The card is the page reduced to a screenful — who they are, how you stand
//! to each other, the counts, the highlights — with one cursor over it.
//! Up/Down move between rows, Left/Right move within the horizontal ones,
//! Enter opens what is under the cursor: the actions submenu, the stories,
//! one of the lists, a highlight. Sub-views come back to the card with Left,
//! Backspace or Esc; `q` leaves from anywhere.
//!
//! **What a click costs is paid at the click.** The card opens on what one
//! `profile::fetch` with `MutualPolicy::Defer` already holds — about three
//! requests — and everything deeper says its price first: the mutual walk
//! runs when the number is opened, a followers or following walk is an
//! inline y/n question naming the size, and on somebody else's account that
//! question is the consent question, carried into the engine as
//! `ListQuery.yes` with the answer recorded on the [`App`]. The posts tile
//! asks nothing, being no list of people: it reads what `snob posts` opens
//! with, the profile and the first page of the grid, under a box that says
//! so.
//!
//! **Never call `ui::stories::browse` or `ui::highlights::browse` from
//! here.** Each makes a [`Tui`] of its own — a second claim on the one
//! terminal this card already holds — and each creates a [`Scratch`] at the
//! same `story_scratch()` path, which deletes the live session's files. The
//! shared internals (`walk_in`, `keep_folder`, `open`, `keep`) are the
//! supported way in. `ui::people` is entered only through its `browse_in`,
//! and `ui::posts` through its `browse_grid_in`, never `browse_grid`. Both
//! borrow this card's guard and its scratch directory instead of
//! building either: the card a list opens for one of its accounts ([`peek`])
//! opens that account's picture and stories into the same directory.
//!
//! **The card is fitted to the terminal, and never loses a row to it**
//! ([`fitted`]). The bio takes up to six rows and ends in `…` when it has
//! more; `i` shows all the account says about itself, from what was already
//! read. On a short terminal the bio's extra rows go first, then the blank
//! rows, so that everything there is to open is still on screen.
//!
//! Walks are the other thing that cannot run under the browser's terminal
//! state: the progress bars belong on the real screen, where their finished
//! receipts stay in the user's scrollback, and raw mode would stairstep them
//! anyway (it clears OPOST on Unix). [`Tui::suspend`] hands the terminal
//! back before `engine::list` draws its bars; [`Tui::resume`] takes it back
//! and repaints from nothing. Everything terminal-shaped beyond that is
//! `ui::tui`'s — the guard, the chrome, and the receipts said on the real
//! screen after the alternate screen has taken the frame away.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use console::measure_text_width;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Padding, Paragraph, Row, Table, TableState, Wrap};
use snob_core::model::{ListKind, User, printable};
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::app::{App, Viewer};
use crate::commands::common;
use crate::commands::highlights::{Entry, Tray};
use crate::commands::pfp::Picture;
use crate::commands::profile::{Highlight, Profile, Visibility, badges};
use crate::engine;
use crate::exit::ExitCode;
use crate::media::Story;
use crate::posts::Fetched;
use crate::report;
use crate::ui::accounts::{self, Browsed, Picked};
use crate::ui::browser::busy::Busy;
use crate::ui::browser::input::{
    self, Action, Raw, Stopped, action_of, page, watching_cancel_keys,
};
use crate::ui::browser::scratch::{ABANDONED_AFTER, Scratch};
use crate::ui::help;
use crate::ui::highlights::{Folder, empty_note, folder_title, keep_folder, stem_of, walk_in};
use crate::ui::people::Shelf;
use crate::ui::preview;
use crate::ui::theme::Role;
use crate::ui::tui::{self, Tui};

/// How long a stored list may serve a click without a question: the default
/// `--max-age` gives the list commands.
const MAX_AGE: std::time::Duration = engine::DEFAULT_MAX_AGE;

/// Where the browser is, and what the keys therefore drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Card,
    /// The actions submenu: 0 is the picture, 1 is the scan.
    Actions {
        selected: usize,
    },
    Stories,
    /// Inside one highlight; the selection lives in its [`Folder`].
    Inside {
        index: usize,
    },
}

/// The focusable rows of the card, in cursor order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardRow {
    Actions,
    /// The picture, a row of the card a list opens alone ([`viewing_rows`]): on
    /// the card it is under the actions.
    Picture,
    Stories,
    FollowedBy,
    Counts,
    Highlights,
}

/// Which rows this profile has to move over. Absent parts are simply not
/// focusable: a hidden tray is not a row that says "nothing".
fn card_rows(profile: &Profile) -> Vec<CardRow> {
    card_rows_of(profile, false)
}

/// [`card_rows`], with the picture first where the card draws it
/// (`pictured`): then it is on the card to be seen and opened, and no longer
/// only under the actions.
fn card_rows_of(profile: &Profile, pictured: bool) -> Vec<CardRow> {
    let mut rows = Vec::new();
    if pictured {
        rows.push(CardRow::Picture);
    }
    rows.push(CardRow::Actions);
    if matches!(&profile.stories, Visibility::Shown(items) if !items.is_empty()) {
        rows.push(CardRow::Stories);
    }
    if profile.mutual.as_ref().is_some_and(|m| m.count > 0) {
        rows.push(CardRow::FollowedBy);
    }
    rows.push(CardRow::Counts);
    if matches!(&profile.highlights, Visibility::Shown(list) if !list.is_empty()) {
        rows.push(CardRow::Highlights);
    }
    rows
}

/// The inline y/n question in the footer, when one is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    None,
    Scan,
    List(ListKind),
}

/// What a key says to a pending question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    Yes,
    No,
    Interrupt,
    Ignored,
}

/// A pending question owns the keyboard: y runs it, n or Esc declines,
/// Ctrl+C still interrupts, and everything else — `q` and `a` included — is
/// ignored rather than quitting or changing accounts mid-question.
pub(crate) fn answer(key: KeyEvent) -> Answer {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c') if ctrl => Answer::Interrupt,
        KeyCode::Char('y' | 'Y') if plain => Answer::Yes,
        KeyCode::Char('n' | 'N') | KeyCode::Esc if plain => Answer::No,
        _ => Answer::Ignored,
    }
}

struct State {
    level: Level,
    /// Index into [`card_rows`].
    row: usize,
    /// The selected column of the counts row: posts, followers, following.
    counts_col: usize,
    /// The selected highlight chip. Remembered across walks in and out.
    hl_col: usize,
    stories_selected: usize,
    ask: Ask,
}

/// Everything fetched once and kept for the session.
struct Media {
    stories: Vec<Story>,
    stories_opened: Vec<Option<PathBuf>>,
    tray: Tray,
    folders: Vec<Folder>,
    /// The walked mutual list, from the first click on the number.
    mutual: Option<Vec<User>>,
    pfp: Option<Picture>,
    pfp_opened: Option<PathBuf>,
    /// Whether fetching the picture is a request to Instagram and not the
    /// CDN alone ([`picture_is_a_request`]). The read-only card says so on
    /// the row, before the key.
    pfp_is_a_request: bool,
    /// Whether the card draws the account's picture ([`pictured`]), and the
    /// small one it draws once that has arrived.
    pictured: bool,
    avatar: Option<preview::Pixels>,
}

/// Takes the account's small picture from the gallery once it has arrived,
/// for the frames to draw, and hands back its address while it is still to
/// be fetched: what a card's loop tells the gallery it wants, every turn.
fn adopt_avatar<'p>(
    profile: &'p Profile,
    media: &mut Media,
    gallery: &preview::Gallery<'_>,
) -> Option<&'p str> {
    let address = profile.pfp_url.as_deref().filter(|_| media.pictured)?;
    if media.avatar.is_none() {
        media.avatar = gallery.get(address).cloned();
    }
    media.avatar.is_none().then_some(address)
}

/// How a card's gallery fetches an address: a download, which is the tab's
/// or the CDN client's and no request to Instagram, on a client the task
/// holds for itself.
fn fetching(client: &Arc<IgClient>) -> impl FnOnce(String) -> preview::Fetching<'static> + '_ {
    move |address| {
        let client = Arc::clone(client);
        Box::pin(async move { client.download(&address).await.ok() })
    }
}

/// Whether a card draws the account's picture: a run that draws pictures
/// at all (`preview::Gallery::draws`), and an account that has one and says
/// where a small copy of it is.
fn pictured(profile: &Profile) -> bool {
    preview::Gallery::draws() && !profile.pfp_anonymous && profile.pfp_url.is_some()
}

impl Media {
    /// What the card holds before anything is clicked: the stories and the
    /// tray the profile came with, nothing opened yet.
    fn of(profile: &Profile) -> Self {
        let stories = match &profile.stories {
            Visibility::Shown(items) => items.clone(),
            Visibility::Hidden => Vec::new(),
        };
        let tray = Tray {
            username: profile.username.clone(),
            entries: match &profile.highlights {
                Visibility::Shown(list) => list.iter().map(entry_of).collect(),
                Visibility::Hidden => Vec::new(),
            },
        };
        Self {
            stories_opened: vec![None; stories.len()],
            folders: Folder::for_tray(&tray),
            stories,
            tray,
            mutual: None,
            pfp: None,
            pfp_opened: None,
            pfp_is_a_request: false,
            pictured: false,
            avatar: None,
        }
    }
}

/// Whether the picture costs a request beyond the CDN's: only where the
/// profile named no full-size picture and there is no page to have named it,
/// which is when `commands::pfp::picture` asks the by-id endpoint.
fn picture_is_a_request(app: &App, profile: &Profile) -> bool {
    profile.pfp_full_size.is_none() && !app.client().has_page()
}

/// What a suspended piece of work left the loop to do.
enum After {
    /// Back on the card, with this in the note line.
    Stay(String),
    /// The session is over; the terminal is already restored.
    Leave(ExitCode),
}

/// Drives the card until the user leaves it, or picks another account to
/// see it as. `note` is said on the first frame.
pub async fn browse(
    app: &mut App,
    profile: &Profile,
    typed: Option<&str>,
    paths: &AppPaths,
    secrets: &SecretStore,
    mut note: String,
) -> Result<Browsed> {
    snob_store::paths::sweep_old_scratch(&paths.stories_root(), ABANDONED_AFTER);
    let scratch = Scratch::new(paths.story_scratch())?;

    let mut tui =
        tui::claim_fullscreen("--no-interactive prints the profile; --format and -o shape it")?;

    let viewer = app.viewer().clone();
    let own = viewer.pk == profile.pk;
    let pictured = pictured(profile);
    let rows = card_rows_of(profile, pictured);
    let mut state = State {
        level: Level::Card,
        row: 0,
        counts_col: 0,
        hl_col: 0,
        stories_selected: 0,
        ask: Ask::None,
    };
    let mut media = Media::of(profile);
    media.pictured = pictured;
    // The account's small picture, fetched once the card has been up for a
    // moment and while the keys are read (`preview::Gallery`). Through a
    // client of its own to hold: the walks this card starts take the `App`
    // whole.
    let mut gallery = preview::Gallery::new();
    let shared = app.client_shared();

    let colors = tui::colors_enabled();
    let mut receipts: Vec<String> = Vec::new();
    let mut list = TableState::default();
    let mut page_rows = 1usize;
    let mut switch_to: Option<Viewer> = None;

    // The level as it is on screen, for a `Busy` to keep and wait over. A
    // macro and not a closure: a closure made once would hold the media and
    // the list for as long as it lived, and the fetch the wait is for needs
    // them back. Each use borrows for the one frame it draws.
    macro_rules! level_frame {
        () => {
            |frame: &mut Frame<'_>| {
                draw_level(
                    frame,
                    profile,
                    &media,
                    &state,
                    &rows,
                    own,
                    &viewer,
                    &note,
                    colors,
                    &mut list,
                    &mut page_rows,
                );
            }
        };
    }

    // Every way out passes the receipts: see `tui::print_receipts`.
    let outcome: Result<ExitCode> = async {
        loop {
            let wanted = adopt_avatar(profile, &mut media, &gallery);
            gallery.want(wanted);
            match state.level {
                Level::Stories => list.select(Some(state.stories_selected)),
                Level::Inside { index } => list.select(Some(media.folders[index].selected)),
                Level::Card | Level::Actions { .. } => {}
            }
            tui.terminal.draw(|frame| {
                draw_level(
                    frame,
                    profile,
                    &media,
                    &state,
                    &rows,
                    own,
                    &viewer,
                    &note,
                    colors,
                    &mut list,
                    &mut page_rows,
                );
            })?;

            let event = gallery
                .read(fetching(&shared))
                .await
                .map_err(input::unreadable)?;
            let key = match event {
                Raw::Tick | Raw::Resized | Raw::Paste(_) => continue,
                Raw::Key(key) => key,
            };
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            // A note stays up until the user does something else, not until the
            // next timer tick wipes it -- but never under a pending question,
            // whose branch writes its own.
            if state.ask == Ask::None {
                note.clear();
            }

            if state.ask != Ask::None {
                match answer(key) {
                    Answer::Interrupt => break Ok(ExitCode::Interrupted),
                    Answer::Yes => {
                        let pending = std::mem::replace(&mut state.ask, Ask::None);
                        let after = match pending {
                            Ask::List(kind) => {
                                consent_was_given(app, typed, own);
                                walk_and_open(
                                    app,
                                    &mut tui,
                                    &mut receipts,
                                    &scratch,
                                    typed,
                                    profile,
                                    kind,
                                )
                                .await?
                            }
                            Ask::Scan => {
                                consent_was_given(app, typed, own);
                                scan_and_open(
                                    app,
                                    &mut tui,
                                    &mut receipts,
                                    &scratch,
                                    typed,
                                    profile,
                                )
                                .await?
                            }
                            Ask::None => After::Stay(String::new()),
                        };
                        match after {
                            After::Stay(text) => note = text,
                            After::Leave(code) => break Ok(code),
                        }
                    }
                    Answer::No => {
                        state.ask = Ask::None;
                        note = "Nothing walked".to_string();
                    }
                    Answer::Ignored => {}
                }
                continue;
            }

            // The picker opens the same way from every level, over it.
            if action_of(key) == Action::Accounts {
                let picked = accounts::pick(&mut tui, secrets, paths, &viewer, |frame| {
                    draw_level(
                        frame,
                        profile,
                        &media,
                        &state,
                        &rows,
                        own,
                        &viewer,
                        &note,
                        colors,
                        &mut list,
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
                continue;
            }

            // So do the account's page on the web and the keys of the level.
            match action_of(key) {
                Action::Web => {
                    note = tui::web_note(&tui::page_of(&profile.username), &mut receipts);
                    continue;
                }
                Action::Help => {
                    let (title, keys) = match state.level {
                        Level::Card if pictured => ("Profile", preview::with_key(CARD_KEYS)),
                        Level::Card => ("Profile", CARD_KEYS.to_vec()),
                        Level::Actions { .. } => ("Actions", preview::with_key(ACTIONS_KEYS)),
                        Level::Stories => (
                            "Stories",
                            preview::with_key(crate::ui::stories::FOLDER_KEYS),
                        ),
                        Level::Inside { .. } => (
                            "A highlight",
                            preview::with_key(crate::ui::stories::FOLDER_KEYS),
                        ),
                    };
                    help::show(&mut tui, title, &keys, level_frame!())?;
                    continue;
                }
                _ => {}
            }
            // And, from the card, all the account says about itself.
            if state.level == Level::Card && asks_about(key) {
                if says_something(profile) {
                    show_about(&mut tui, profile, level_frame!())?;
                } else {
                    note = nothing_said(profile);
                }
                continue;
            }

            let level_before = state.level;
            // The picture on the card is the first entry of the actions
            // panel put where it can be seen: Enter, `d` and `p` on it do
            // what they do there, through the same code.
            let on_the_picture = state.level == Level::Card
                && rows[state.row.min(rows.len() - 1)] == CardRow::Picture
                && matches!(
                    action_of(key),
                    Action::Open | Action::Download | Action::Preview
                );
            let level_of_the_key = if on_the_picture {
                Level::Actions { selected: 0 }
            } else {
                state.level
            };
            match level_of_the_key {
                Level::Card => {
                    let here = rows[state.row.min(rows.len() - 1)];
                    // Left and Right, and `h` and `l` with them, move the
                    // column on the horizontal rows, and must not reach
                    // `action_of`, whose Left is Back and whose Right is Open.
                    if plain && matches!(here, CardRow::Counts | CardRow::Highlights) {
                        let (col, len) = match here {
                            CardRow::Counts => (&mut state.counts_col, 3),
                            CardRow::Highlights => (&mut state.hl_col, media.tray.entries.len()),
                            _ => unreachable!(),
                        };
                        match key.code {
                            KeyCode::Left | KeyCode::Char('h') => {
                                *col = col.saturating_sub(1);
                                continue;
                            }
                            KeyCode::Right | KeyCode::Char('l') => {
                                *col = (*col + 1).min(len.saturating_sub(1));
                                continue;
                            }
                            _ => {}
                        }
                    }
                    match action_of(key) {
                        Action::Up => state.row = state.row.saturating_sub(1),
                        Action::Down => state.row = (state.row + 1).min(rows.len() - 1),
                        Action::First => state.row = 0,
                        Action::Last => state.row = rows.len() - 1,
                        Action::Open => match here {
                            CardRow::Actions => state.level = Level::Actions { selected: 0 },
                            // Never a row of this card: see `viewing_rows`.
                            CardRow::Picture => {}
                            CardRow::Stories => state.level = Level::Stories,
                            CardRow::FollowedBy => {
                                match followed_by_open(
                                    app,
                                    &mut tui,
                                    &mut receipts,
                                    &scratch,
                                    profile,
                                    &mut media,
                                )
                                .await?
                                {
                                    After::Stay(text) => note = text,
                                    After::Leave(code) => break Ok(code),
                                }
                            }
                            CardRow::Counts => match state.counts_col {
                                // The grid, as `snob posts` opens it: the
                                // profile and the grid's first page, under a
                                // box and with no question, since it is no
                                // list of people. A count of zero is an
                                // answer the card already has.
                                0 if profile.posts == Some(0) => {
                                    note = "No posts".to_string();
                                }
                                0 => {
                                    let busy = Busy::begin(
                                        &mut tui,
                                        "Reading the posts...",
                                        level_frame!(),
                                    )?;
                                    let (read, stopped) = busy
                                        .run(
                                            &mut tui,
                                            app.client().pacer().cancel_token(),
                                            crate::posts::fetch_grid(
                                                app.client(),
                                                &profile.username,
                                                viewer.pk,
                                                Some(profile.pk),
                                                1,
                                                0,
                                            ),
                                        )
                                        .await;
                                    if let Some(code) = stopped.leave() {
                                        break Ok(code);
                                    }
                                    match read {
                                        Err(e) => note = format!("Could not read the posts: {e}"),
                                        Ok(Fetched::Hidden { .. }) => {
                                            note = "The posts are for followers only".to_string();
                                        }
                                        Ok(Fetched::Grid(grid)) if grid.posts.is_empty() => {
                                            note = "No posts".to_string();
                                        }
                                        Ok(Fetched::Grid(grid)) => {
                                            // On the heap, as the card a list
                                            // opens is (`people::run`).
                                            let left = Box::pin(crate::ui::posts::browse_grid_in(
                                                &mut tui,
                                                &scratch,
                                                app.client(),
                                                grid,
                                                &viewer,
                                                &mut receipts,
                                            ))
                                            .await?;
                                            if let Some(code) = left {
                                                break Ok(code);
                                            }
                                            // The grid's frame is what the
                                            // terminal believes is on screen.
                                            tui.terminal.clear()?;
                                        }
                                    }
                                }
                                col => {
                                    let kind = if col == 1 {
                                        ListKind::Followers
                                    } else {
                                        ListKind::Following
                                    };
                                    match list_open(
                                        app,
                                        &mut tui,
                                        &mut receipts,
                                        &scratch,
                                        profile,
                                        kind,
                                        &mut state,
                                    )
                                    .await?
                                    {
                                        After::Stay(text) => note = text,
                                        After::Leave(code) => break Ok(code),
                                    }
                                }
                            },
                            CardRow::Highlights => {
                                let index = state.hl_col;
                                let busy = Busy::begin(
                                    &mut tui,
                                    format!("Opening highlight {}...", index + 1),
                                    level_frame!(),
                                )?;
                                let (result, stopped) = busy
                                    .run(
                                        &mut tui,
                                        app.client().pacer().cancel_token(),
                                        walk_in(
                                            app.client(),
                                            &media.tray,
                                            index,
                                            &mut media.folders,
                                        ),
                                    )
                                    .await;
                                match result {
                                    Ok(true) => state.level = Level::Inside { index },
                                    Ok(false) => note = empty_note(index),
                                    Err(e) => note = format!("Could not open it: {e}"),
                                }
                                if let Some(code) = stopped.leave() {
                                    break Ok(code);
                                }
                            }
                        },
                        Action::Download => {
                            if here == CardRow::Highlights {
                                let (folder_note, stopped) = watching_cancel_keys(
                                    app.client().pacer().cancel_token(),
                                    keep_folder(
                                        app.client(),
                                        &mut tui,
                                        &media.tray,
                                        state.hl_col,
                                        &mut media.folders,
                                        &viewer,
                                        &mut receipts,
                                    ),
                                )
                                .await;
                                note = folder_note;
                                if let Some(code) = stopped.leave() {
                                    break Ok(code);
                                }
                            }
                        }
                        Action::Redraw => tui.terminal.clear()?,
                        Action::Quit => break Ok(ExitCode::Ok),
                        Action::Interrupt => break Ok(ExitCode::Interrupted),
                        // The last three are answered above, for every
                        // level at once.
                        Action::Back
                        | Action::PageUp
                        | Action::PageDown
                        | Action::Accounts
                        | Action::Web
                        | Action::Help
                        | Action::Preview
                        | Action::None => {}
                    }
                }
                Level::Actions { selected } => {
                    // Esc closes the submenu rather than the program; `q` still
                    // quits, through `action_of`.
                    if plain && key.code == KeyCode::Esc {
                        state.level = Level::Card;
                        continue;
                    }
                    match action_of(key) {
                        Action::Up => state.level = Level::Actions { selected: 0 },
                        Action::Down => state.level = Level::Actions { selected: 1 },
                        Action::Open => {
                            if selected == 0 {
                                let busy = Busy::begin(
                                    &mut tui,
                                    "Opening the picture...",
                                    level_frame!(),
                                )?;
                                let (text, stopped) = busy
                                    .run(
                                        &mut tui,
                                        app.client().pacer().cancel_token(),
                                        pfp_open(app, profile, &mut media, &scratch),
                                    )
                                    .await;
                                note = text;
                                if let Some(code) = stopped.leave() {
                                    break Ok(code);
                                }
                            } else {
                                state.ask = Ask::Scan;
                                state.level = Level::Card;
                            }
                        }
                        Action::Download => {
                            if selected == 0 {
                                let busy =
                                    Busy::begin(&mut tui, "Saving the picture...", level_frame!())?;
                                let (text, stopped) = busy
                                    .run(
                                        &mut tui,
                                        app.client().pacer().cancel_token(),
                                        pfp_keep(app, profile, &mut media, &mut receipts),
                                    )
                                    .await;
                                note = text;
                                if let Some(code) = stopped.leave() {
                                    break Ok(code);
                                }
                            }
                        }
                        Action::Preview => {
                            if selected == 0 {
                                if let Some(why) = preview::refused(None) {
                                    note = why;
                                    continue;
                                }
                                let busy = Busy::begin(
                                    &mut tui,
                                    "Fetching the picture...",
                                    level_frame!(),
                                )?;
                                let (fetched, stopped) = busy
                                    .run(
                                        &mut tui,
                                        app.client().pacer().cancel_token(),
                                        ensure_pfp(app, profile, &mut media),
                                    )
                                    .await;
                                if let Some(code) = stopped.leave() {
                                    break Ok(code);
                                }
                                note = match (fetched, media.pfp.as_ref()) {
                                    (Err(e), _) => could_not_fetch_the_picture(&e),
                                    (Ok(()), Some(picture)) => preview::show(
                                        &mut tui,
                                        &picture_name(profile),
                                        &picture.bytes,
                                        level_frame!(),
                                    )?,
                                    (Ok(()), None) => String::new(),
                                };
                            }
                        }
                        Action::Back => state.level = Level::Card,
                        Action::Redraw => tui.terminal.clear()?,
                        Action::Quit => break Ok(ExitCode::Ok),
                        Action::Interrupt => break Ok(ExitCode::Interrupted),
                        _ => {}
                    }
                }
                Level::Stories => {
                    if plain && key.code == KeyCode::Esc {
                        state.level = Level::Card;
                        continue;
                    }
                    let total = media.stories.len();
                    match action_of(key) {
                        Action::Up => {
                            state.stories_selected = state.stories_selected.saturating_sub(1)
                        }
                        Action::Down => {
                            state.stories_selected =
                                (state.stories_selected + 1).min(total.saturating_sub(1));
                        }
                        Action::PageUp => {
                            state.stories_selected =
                                state.stories_selected.saturating_sub(page(page_rows));
                        }
                        Action::PageDown => {
                            state.stories_selected = (state.stories_selected + page(page_rows))
                                .min(total.saturating_sub(1));
                        }
                        Action::First => state.stories_selected = 0,
                        Action::Last => state.stories_selected = total.saturating_sub(1),
                        Action::Open => {
                            let busy = Busy::begin(
                                &mut tui,
                                format!("Opening story {}...", state.stories_selected + 1),
                                level_frame!(),
                            )?;
                            let (text, stopped) = story_open(
                                busy,
                                &mut tui,
                                app,
                                profile,
                                &mut media,
                                state.stories_selected,
                                &scratch,
                            )
                            .await;
                            note = text;
                            if let Some(code) = stopped.leave() {
                                break Ok(code);
                            }
                        }
                        Action::Download => {
                            let busy = Busy::begin(
                                &mut tui,
                                format!("Saving story {}...", state.stories_selected + 1),
                                level_frame!(),
                            )?;
                            let (text, stopped) = story_keep(
                                busy,
                                &mut tui,
                                app,
                                profile,
                                &mut media,
                                state.stories_selected,
                                &mut receipts,
                            )
                            .await;
                            note = text;
                            if let Some(code) = stopped.leave() {
                                break Ok(code);
                            }
                        }
                        Action::Back => state.level = Level::Card,
                        Action::Redraw => tui.terminal.clear()?,
                        Action::Quit => break Ok(ExitCode::Ok),
                        Action::Interrupt => break Ok(ExitCode::Interrupted),
                        Action::Preview => {
                            let at = state.stories_selected;
                            let kind = media.stories.get(at).map(|story| story.kind);
                            if let Some(why) = preview::refused(kind) {
                                note = why;
                                continue;
                            }
                            let busy = Busy::begin(
                                &mut tui,
                                format!("Fetching story {}...", at + 1),
                                level_frame!(),
                            )?;
                            let (file, stopped) = story_fetched(
                                busy, &mut tui, app, profile, &mut media, at, &scratch,
                            )
                            .await;
                            if let Some(code) = stopped.leave() {
                                break Ok(code);
                            }
                            note = preview::show_file(&mut tui, file, level_frame!())?;
                        }
                        Action::Accounts | Action::Web | Action::Help | Action::None => {}
                    }
                }
                Level::Inside { index } => {
                    if plain && key.code == KeyCode::Esc {
                        state.level = Level::Card;
                        continue;
                    }
                    let stem = stem_of(&media.tray, index);
                    let folder = &mut media.folders[index];
                    let items = folder.items.as_deref().unwrap_or_default();
                    let total = items.len();
                    match action_of(key) {
                        Action::Up => folder.selected = folder.selected.saturating_sub(1),
                        Action::Down => {
                            folder.selected = (folder.selected + 1).min(total.saturating_sub(1));
                        }
                        Action::PageUp => {
                            folder.selected = folder.selected.saturating_sub(page(page_rows));
                        }
                        Action::PageDown => {
                            folder.selected =
                                (folder.selected + page(page_rows)).min(total.saturating_sub(1));
                        }
                        Action::First => folder.selected = 0,
                        Action::Last => folder.selected = total.saturating_sub(1),
                        Action::Open => {
                            // The frame under the box reads the folders, so
                            // the folder is let go of for it and taken again.
                            let busy = Busy::begin(
                                &mut tui,
                                format!("Opening item {}...", folder.selected + 1),
                                level_frame!(),
                            )?;
                            let folder = &mut media.folders[index];
                            let items = folder.items.as_deref().unwrap_or_default();
                            let (result, stopped) = busy
                                .run(
                                    &mut tui,
                                    app.client().pacer().cancel_token(),
                                    crate::ui::stories::open(
                                        app.client(),
                                        &stem,
                                        items,
                                        folder.selected,
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
                        Action::Download => {
                            let busy = Busy::begin(
                                &mut tui,
                                format!("Saving item {}...", folder.selected + 1),
                                level_frame!(),
                            )?;
                            let folder = &mut media.folders[index];
                            let items = folder.items.as_deref().unwrap_or_default();
                            let (result, stopped) = busy
                                .run(
                                    &mut tui,
                                    app.client().pacer().cancel_token(),
                                    crate::ui::stories::keep(
                                        app.client(),
                                        &stem,
                                        items,
                                        folder.selected,
                                        &mut folder.opened,
                                    ),
                                )
                                .await;
                            note = tui::saved_note(result, &mut receipts);
                            if let Some(code) = stopped.leave() {
                                break Ok(code);
                            }
                        }
                        Action::Back => state.level = Level::Card,
                        Action::Redraw => tui.terminal.clear()?,
                        Action::Quit => break Ok(ExitCode::Ok),
                        Action::Interrupt => break Ok(ExitCode::Interrupted),
                        Action::Preview => {
                            let at = folder.selected;
                            let kind = items.get(at).map(|item| item.kind);
                            if let Some(why) = preview::refused(kind) {
                                note = why;
                                continue;
                            }
                            let busy = Busy::begin(
                                &mut tui,
                                format!("Fetching item {}...", at + 1),
                                level_frame!(),
                            )?;
                            let folder = &mut media.folders[index];
                            let items = folder.items.as_deref().unwrap_or_default();
                            let (file, stopped) = busy
                                .run(
                                    &mut tui,
                                    app.client().pacer().cancel_token(),
                                    crate::ui::stories::fetched(
                                        app.client(),
                                        &stem,
                                        items,
                                        at,
                                        &scratch,
                                        &mut folder.opened,
                                    ),
                                )
                                .await;
                            if let Some(code) = stopped.leave() {
                                break Ok(code);
                            }
                            note = preview::show_file(&mut tui, file, level_frame!())?;
                        }
                        Action::Accounts | Action::Web | Action::Help | Action::None => {}
                    }
                }
            }
            // The scroll offset belongs to the level it was scrolled at.
            if state.level != level_before {
                list = TableState::default();
            }
        }
    }
    .await;

    drop(tui);
    tui::print_receipts(&receipts);
    outcome.map(|code| Browsed::of(code, switch_to))
}

/// Draws the level the card is at.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_level(
    frame: &mut Frame<'_>,
    profile: &Profile,
    media: &Media,
    state: &State,
    rows: &[CardRow],
    own: bool,
    viewer: &Viewer,
    note: &str,
    colors: bool,
    list: &mut TableState,
    page_rows: &mut usize,
) {
    match state.level {
        Level::Card => draw_card(
            frame, profile, media, state, rows, own, viewer, note, colors,
        ),
        Level::Actions { selected } => {
            // The card stays underneath: the submenu acts on the account the
            // card shows, and losing sight of it helps nobody.
            draw_card(
                frame, profile, media, state, rows, own, viewer, note, colors,
            );
            draw_actions_panel(frame, profile, selected, note, colors);
        }
        Level::Stories => crate::ui::stories::draw_items_view(
            frame,
            stories_title(profile, media),
            crate::ui::stories::FOLDER_HINT,
            &media.stories,
            true,
            viewer,
            list,
            note,
            colors,
            page_rows,
        ),
        Level::Inside { index } => {
            let folder = &media.folders[index];
            let items = folder.items.as_deref().unwrap_or_default();
            crate::ui::stories::draw_items_view(
                frame,
                folder_title(
                    &profile.username,
                    &media.tray.entries[index],
                    index,
                    items.len(),
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
    }
}

/// One highlight of the profile as the tray type the fetchers take.
fn entry_of(h: &Highlight) -> Entry {
    Entry {
        id: h.id.clone(),
        title: h.title.clone(),
        declared_items: h.items,
        created_at: None,
        updated_at: h.updated_at,
    }
}

fn entry_title(entry: &Entry) -> String {
    if entry.title.is_empty() {
        "(untitled)".to_string()
    } else {
        entry.title.clone()
    }
}

/// The `y` was the consent, for the engine and for the next question alike.
///
/// Recorded only for somebody else's account — your own lists have nothing
/// to agree to — and only when a name was typed, which is the only case the
/// engine would ask about.
fn consent_was_given(app: &mut App, typed: Option<&str>, own: bool) {
    if !own && let Some(name) = typed {
        app.record_consent(engine::target::clean(name));
    }
}

fn list_query(typed: Option<&str>) -> engine::ListQuery {
    engine::ListQuery {
        target: typed.map(str::to_string),
        // The inline question was asked and answered before this is built;
        // without it the engine would try to prompt mid-suspension.
        yes: true,
        refresh: false,
        cache: false,
        max_age: MAX_AGE,
        no_resume: false,
        max_pages: None,
        // Pause without the warning: from inside a drawn screen it would draw
        // over it. Pause never goes past the day's ceiling.
        over_budget: Some(snob_ig::pager::OverBudget::Pause),
        walk_at_most_every: None,
    }
}

fn subject_of(profile: &Profile) -> String {
    format!("@{}", printable(&profile.username))
}

/// Enter on followers or following: the stored list when it still answers,
/// the inline question when it would cost a walk.
async fn list_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    scratch: &Scratch,
    profile: &Profile,
    kind: ListKind,
    state: &mut State,
) -> Result<After> {
    let declared = match kind {
        ListKind::Followers => profile.followers,
        ListKind::Following => profile.following,
    };
    // The counter came off the page moments ago, so comparing against it is
    // the same honesty the poll gives the list commands — without the poll.
    if let Some(people) =
        engine::freshness::fresh_members(app, profile.pk, kind, declared, MAX_AGE)?
    {
        // No walk, so no suspension: the list draws on the guard this card
        // already holds, and coming back is just the next frame.
        let shelf = Shelf::flat(
            format!("{kind} of {}", subject_of(profile)),
            &people,
            app.viewer().clone(),
        );
        let code = crate::ui::people::browse_in(tui, app, &shelf, scratch, receipts).await?;
        if code == ExitCode::Interrupted {
            return Ok(After::Leave(code));
        }
        return Ok(After::Stay(format!(
            "{}, from the stored list",
            report::accounts(people.len())
        )));
    }
    state.ask = Ask::List(kind);
    Ok(After::Stay(String::new()))
}

/// The walk a `y` pays for, and the list it opens.
async fn walk_and_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    scratch: &Scratch,
    typed: Option<&str>,
    profile: &Profile,
    kind: ListKind,
) -> Result<After> {
    let _ = tui.suspend();
    let subject = subject_of(profile);
    let query = list_query(typed);
    let result = common::walk_named(app, &query, kind, &subject, |_| Ok(())).await;
    app.progress().finish();
    let (people, outcome) = match result {
        Ok(pair) => pair,
        Err(e) => return stay_or_leave(app, tui, e),
    };
    if app.cancel().is_canceled() {
        return Ok(After::Leave(ExitCode::Interrupted));
    }
    tui.resume()?;
    if people.is_empty() {
        return Ok(After::Stay(format!("The {kind} list is empty")));
    }
    let shelf = Shelf::flat(
        format!("{kind} of {subject}"),
        &people,
        app.viewer().clone(),
    );
    let code = crate::ui::people::browse_in(tui, app, &shelf, scratch, receipts).await?;
    if code == ExitCode::Interrupted {
        return Ok(After::Leave(code));
    }
    Ok(After::Stay(format!(
        "{} · {}",
        report::accounts(people.len()),
        report::requests(outcome.requests)
    )))
}

/// The scan a `y` pays for: both lists, crossed, as the tray `snob scan -i`
/// shows.
async fn scan_and_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    scratch: &Scratch,
    typed: Option<&str>,
    profile: &Profile,
) -> Result<After> {
    let _ = tui.suspend();
    let subject = subject_of(profile);
    let query = list_query(typed);

    let first = common::walk_named(app, &query, ListKind::Followers, &subject, |outcome| {
        if outcome.is_complete() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "the followers list came back incomplete, and a crossing built on it would lie"
            ))
        }
    })
    .await;
    let (followers, followers_outcome) = match first {
        Ok(pair) => pair,
        Err(e) => return stay_or_leave(app, tui, e),
    };
    let second = common::walk_named(app, &query, ListKind::Following, &subject, |_| Ok(())).await;
    app.progress().finish();
    let (following, following_outcome) = match second {
        Ok(pair) => pair,
        Err(e) => return stay_or_leave(app, tui, e),
    };
    if app.cancel().is_canceled() {
        return Ok(After::Leave(ExitCode::Interrupted));
    }
    tui.resume()?;
    if let Err(e) = engine::cooldown::check_same_moment(&followers_outcome, &following_outcome) {
        return Ok(After::Stay(format!("Not crossed: {e}")));
    }

    let sets = crate::ui::people::scan_sets(
        &followers,
        &following,
        &snob_core::filters::Filter::default(),
    );
    let shelf = Shelf::tray(format!("scan of {subject}"), &sets, app.viewer().clone());
    let code = crate::ui::people::browse_in(tui, app, &shelf, scratch, receipts).await?;
    if code == ExitCode::Interrupted {
        return Ok(After::Leave(code));
    }
    Ok(After::Stay(format!(
        "Scanned · {}",
        report::requests(followers_outcome.requests + following_outcome.requests)
    )))
}

/// Enter on the followed-by number: the mutual walk on the first click, the
/// kept list after.
async fn followed_by_open(
    app: &mut App,
    tui: &mut Tui,
    receipts: &mut Vec<String>,
    scratch: &Scratch,
    profile: &Profile,
    media: &mut Media,
) -> Result<After> {
    let Some(mutual) = profile.mutual.as_ref() else {
        return Ok(After::Stay(String::new()));
    };
    if media.mutual.is_none() {
        let _ = tui.suspend();
        // No bar draws for this walk, so one line says why the terminal is
        // quiet while the pacer spaces the pages out.
        eprintln!(
            "Walking the accounts you follow that follow @{}...",
            printable(&profile.username)
        );
        let walked = crate::commands::profile::mutuals(
            app.client(),
            profile.pk,
            &profile.username,
            mutual.count,
            mutual.preview.clone(),
        )
        .await;
        app.progress().finish();
        match walked {
            Ok(list) => media.mutual = Some(list.people),
            Err(e) => return stay_or_leave(app, tui, e),
        }
        if app.cancel().is_canceled() {
            return Ok(After::Leave(ExitCode::Interrupted));
        }
        tui.resume()?;
    }
    let people = media.mutual.as_deref().unwrap_or_default();
    if people.is_empty() {
        return Ok(After::Stay("The mutual list came back empty".to_string()));
    }
    let shelf = Shelf::flat(
        format!("followed by, of {}", subject_of(profile)),
        people,
        app.viewer().clone(),
    );
    let code = crate::ui::people::browse_in(tui, app, &shelf, scratch, receipts).await?;
    if code == ExitCode::Interrupted {
        return Ok(After::Leave(code));
    }
    Ok(After::Stay(format!(
        "{} you follow",
        report::accounts(people.len())
    )))
}

/// A failure while suspended: back on the card with the reason in the note —
/// unless the user interrupted, which ends the session with the terminal
/// already restored.
fn stay_or_leave(app: &App, tui: &mut Tui, error: anyhow::Error) -> Result<After> {
    if app.cancel().is_canceled() {
        return Ok(After::Leave(ExitCode::Interrupted));
    }
    tui.resume()?;
    Ok(After::Stay(format!("Could not walk it: {error}")))
}

/// Enter on the picture row: fetched once, then the story browser's open.
async fn pfp_open(app: &App, profile: &Profile, media: &mut Media, scratch: &Scratch) -> String {
    if let Err(e) = ensure_pfp(app, profile, media).await {
        return could_not_fetch_the_picture(&e);
    }
    let Some(picture) = media.pfp.as_ref() else {
        return String::new();
    };
    tui::opened_note(crate::ui::pfp::open(
        picture,
        scratch,
        &mut media.pfp_opened,
    ))
}

/// D on the picture row: the very file `snob pfp` writes, where the user is.
async fn pfp_keep(
    app: &App,
    profile: &Profile,
    media: &mut Media,
    receipts: &mut Vec<String>,
) -> String {
    if let Err(e) = ensure_pfp(app, profile, media).await {
        return could_not_fetch_the_picture(&e);
    }
    let Some(picture) = media.pfp.as_ref() else {
        return String::new();
    };
    tui::saved_note(crate::ui::pfp::keep(picture), receipts)
}

async fn ensure_pfp(app: &App, profile: &Profile, media: &mut Media) -> Result<()> {
    if media.pfp.is_none() {
        if profile.pfp_anonymous {
            return Err(crate::commands::pfp::no_picture(&profile.username));
        }
        media.pfp = Some(
            crate::commands::pfp::picture(
                app.client(),
                profile.pk,
                &profile.username,
                profile.pfp_url.clone(),
                profile.pfp_full_size.clone(),
            )
            .await?,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What the card and the card a list opens both do.

/// Enter on a story: fetched and handed to the system viewer, under the box
/// `busy` already drew the view for. The note it leaves, and whether a key
/// stopped it.
async fn story_open(
    busy: Busy,
    tui: &mut Tui,
    app: &App,
    profile: &Profile,
    media: &mut Media,
    index: usize,
    scratch: &Scratch,
) -> (String, Stopped) {
    let (result, stopped) = busy
        .run(
            tui,
            app.client().pacer().cancel_token(),
            crate::ui::stories::open(
                app.client(),
                &profile.username,
                &media.stories,
                index,
                scratch,
                &mut media.stories_opened,
            ),
        )
        .await;
    (tui::opened_note(result), stopped)
}

/// D on a story: saved where the user is, under the box `busy` drew.
async fn story_keep(
    busy: Busy,
    tui: &mut Tui,
    app: &App,
    profile: &Profile,
    media: &mut Media,
    index: usize,
    receipts: &mut Vec<String>,
) -> (String, Stopped) {
    let (result, stopped) = busy
        .run(
            tui,
            app.client().pacer().cancel_token(),
            crate::ui::stories::keep(
                app.client(),
                &profile.username,
                &media.stories,
                index,
                &mut media.stories_opened,
            ),
        )
        .await;
    (tui::saved_note(result, receipts), stopped)
}

/// `p` on a story: the file Enter would open, fetched under the box `busy`
/// drew, for `preview::show_file` to draw once the wait is over and the view
/// can be drawn under it again.
async fn story_fetched(
    busy: Busy,
    tui: &mut Tui,
    app: &App,
    profile: &Profile,
    media: &mut Media,
    index: usize,
    scratch: &Scratch,
) -> (Result<PathBuf>, Stopped) {
    busy.run(
        tui,
        app.client().pacer().cancel_token(),
        crate::ui::stories::fetched(
            app.client(),
            &profile.username,
            &media.stories,
            index,
            scratch,
            &mut media.stories_opened,
        ),
    )
    .await
}

/// The note a picture that could not be fetched leaves.
fn could_not_fetch_the_picture(error: &anyhow::Error) -> String {
    format!("Could not fetch the picture: {error}")
}

/// What the box of a drawn profile picture is called.
fn picture_name(profile: &Profile) -> String {
    format!("@{}", printable(&profile.username))
}

/// The title over the stories of the account, from either card.
fn stories_title(profile: &Profile, media: &Media) -> String {
    format!(
        "Stories · @{} · {} up",
        printable(&profile.username),
        media.stories.len()
    )
}

/// The note `i` leaves on an account with no name, category, bio or link.
fn nothing_said(profile: &Profile) -> String {
    format!(
        "@{} says nothing about itself",
        printable(&profile.username)
    )
}

/// Where a key that moves takes the selection of a list of `total`, and
/// `None` for a key that does not move.
fn moved(selected: usize, total: usize, page_rows: usize, action: Action) -> Option<usize> {
    let last = total.saturating_sub(1);
    Some(match action {
        Action::Up => selected.saturating_sub(1),
        Action::Down => (selected + 1).min(last),
        Action::PageUp => selected.saturating_sub(page(page_rows)),
        Action::PageDown => (selected + page(page_rows)).min(last),
        Action::First => 0,
        Action::Last => last,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// The card an account list opens.

/// The bottom edge of the card a list opens, when nothing on it opens.
pub(crate) const PEEK_HINT: &str = "w open on the web · ? keys · esc back";

/// The same, said shorter, after the keys of a row that opens: the line has
/// to fit a card 80 columns wide.
const PEEK_HINT_SHORT: &str = "w web · ? keys · esc back";

/// The bottom edge of the stories that card opens.
const PEEK_STORIES_HINT: &str = "↑↓ move · enter open · d save · ← back · ? keys";

/// `i`, on both cards.
const ABOUT_KEY: help::Key =
    help::key("i", "all the account says about itself", KeyCode::Char('i'));

/// What `?` shows over the card a list opens.
const PEEK_KEYS: &[help::Key] = &[
    help::key("↑ ↓ j k", "move between what opens", KeyCode::Down),
    help::key(
        "enter → l",
        "open the picture, or the stories",
        KeyCode::Enter,
    ),
    help::key("d", "save the picture here", KeyCode::Char('d')),
    ABOUT_KEY,
    help::WEB,
    help::key("esc q ← h backspace", "back to the list", KeyCode::Esc),
    help::REDRAW,
    help::STOP,
];

/// What `?` shows over the stories that card opens. `q` is back here, as it
/// is on the card they were opened from, and there is no account to change.
const PEEK_STORIES_KEYS: &[help::Key] = &[
    help::MOVE,
    help::PAGE,
    help::ENDS,
    crate::ui::stories::OPEN_KEY,
    crate::ui::stories::SAVE_KEY,
    help::key("esc q ← h backspace", "back to the card", KeyCode::Left),
    help::WEB,
    help::REDRAW,
    help::STOP,
];

/// What `?` shows over the card.
const CARD_KEYS: &[help::Key] = &[
    help::key("↑ ↓ j k", "move between the rows", KeyCode::Down),
    help::key("← → h l", "pick within a row", KeyCode::Right),
    help::key("g G home end", "the first row, the last", KeyCode::Home),
    help::key("enter", "open what is under the cursor", KeyCode::Enter),
    help::key(
        "d",
        "save the picture, or all of a highlight",
        KeyCode::Char('d'),
    ),
    ABOUT_KEY,
    help::ACCOUNT,
    help::WEB,
    help::REDRAW,
    help::QUIT,
    help::STOP,
];

/// What `?` shows over the actions panel.
const ACTIONS_KEYS: &[help::Key] = &[
    help::MOVE,
    help::key(
        "enter → l",
        "open the picture, or start the scan",
        KeyCode::Enter,
    ),
    help::key("d", "save the picture here", KeyCode::Char('d')),
    help::key("esc ← h backspace", "back to the card", KeyCode::Left),
    help::ACCOUNT,
    help::WEB,
    help::REDRAW,
    help::QUIT,
    help::STOP,
];

/// What a key did to the card a list opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Peeked {
    /// Esc, `q`, Backspace, Left or `h`: back to the list it came from.
    Back,
    /// Ctrl+C: out of the whole view.
    Interrupt,
    /// `w`, and `o` as it was before `w` meant this everywhere: hand the
    /// profile address to the system browser.
    Browser,
    /// `?`: the keys of this card.
    Help,
    /// `i`: all the account says about itself.
    About,
    /// Up or `k`, among the rows that open.
    Up,
    /// Down or `j`.
    Down,
    /// Enter, Right or `l`: open the row under the cursor.
    Open,
    /// `d`: save what the row under the cursor is.
    Save,
    /// `p`: draw the picture here, in a build that draws them.
    Preview,
    Redraw,
    Ignored,
}

/// Binds one key of the card a list opens. Deliberately not `action_of`:
/// here `q` and Esc both mean "back to the list", where in the card they
/// leave.
pub(crate) fn peek_key(key: KeyEvent) -> Peeked {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c') if ctrl => Peeked::Interrupt,
        KeyCode::Char('l') if ctrl => Peeked::Redraw,
        KeyCode::Esc | KeyCode::Char('q' | 'h') | KeyCode::Backspace | KeyCode::Left if plain => {
            Peeked::Back
        }
        KeyCode::Char('w' | 'o' | 'O') if plain => Peeked::Browser,
        KeyCode::Char('?') if plain => Peeked::Help,
        KeyCode::Char('i') if plain => Peeked::About,
        KeyCode::Char('p') if plain => Peeked::Preview,
        KeyCode::Up | KeyCode::Char('k') if plain => Peeked::Up,
        KeyCode::Down | KeyCode::Char('j') if plain => Peeked::Down,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if plain => Peeked::Open,
        KeyCode::Char('d' | 'D') if plain => Peeked::Save,
        _ => Peeked::Ignored,
    }
}

/// Whether the card a list opens can open this row: only what is a file
/// away. The stories came with the profile, and looking at one is a download
/// from the CDN, which is no request to Instagram; the picture is the same,
/// or says on its row that it is one request ([`picture_is_a_request`]). A
/// highlight is a walk, a list is a walk and a question, and the mutual
/// followers are pages of a sitting: those stay with `snob profile`, which
/// asks before it spends.
fn opens_in_a_list(row: CardRow) -> bool {
    matches!(row, CardRow::Picture | CardRow::Stories)
}

/// The hints under the card a list opens: the keys of the row under the
/// cursor, when one opens, then the ones that are always there. `several`
/// is whether there is more than one row to move between, and `cut` whether
/// the bio is shown short of whole.
fn peek_hint(focus: Option<CardRow>, several: bool, cut: bool) -> String {
    let mut hint = String::new();
    if several {
        hint.push_str("↑↓ move · ");
    }
    match focus {
        Some(CardRow::Picture) => hint.push_str("enter open · d save · "),
        Some(_) => hint.push_str("enter open · "),
        None => {}
    }
    if cut {
        hint.push_str("i bio · ");
    }
    hint.push_str(if focus.is_some() {
        PEEK_HINT_SHORT
    } else {
        PEEK_HINT
    });
    hint
}

/// The card of the account under the cursor of a list, on the guard the list
/// holds: what `snob profile` shows, without the rows that start a walk.
///
/// **It opens what is a file away and nothing else** ([`opens_in_a_list`]):
/// the picture and the stories the profile came with, each fetched when its
/// key is pressed, under a [`Busy`], into `scratch`, which is the caller's:
/// the list's own, or the card's when the list was opened from the card,
/// since two scratch directories on one path is the second deleting the
/// first's files. `i` shows the whole bio, and sends nothing.
///
/// `None` is back to the list; `Some` leaves the view with that code.
pub(crate) async fn peek(
    tui: &mut Tui,
    app: &App,
    profile: &Profile,
    scratch: &Scratch,
    receipts: &mut Vec<String>,
) -> Result<Option<ExitCode>> {
    let colors = tui::colors_enabled();
    let viewer = app.viewer().clone();
    let own = viewer.pk == profile.pk;
    let rows = viewing_rows(profile);
    // The rows the cursor moves between, as places in `rows`.
    let opening: Vec<usize> = (0..rows.len())
        .filter(|at| opens_in_a_list(rows[*at]))
        .collect();
    let mut media = Media::of(profile);
    media.pfp_is_a_request = picture_is_a_request(app, profile);
    media.pictured = pictured(profile);
    let mut gallery = preview::Gallery::new();
    let shared = app.client_shared();
    let mut state = State {
        level: Level::Card,
        row: opening.first().copied().unwrap_or(0),
        counts_col: 0,
        hl_col: 0,
        stories_selected: 0,
        ask: Ask::None,
    };
    let mut note = String::new();
    let mut list = TableState::default();
    let mut page_rows = 1usize;

    // The view as it is on screen: a macro for the reason `browse` has one.
    macro_rules! peek_frame {
        () => {
            |frame: &mut Frame<'_>| {
                draw_peek(
                    frame,
                    profile,
                    &media,
                    &state,
                    &rows,
                    own,
                    &viewer,
                    &note,
                    colors,
                    &mut list,
                    &mut page_rows,
                );
            }
        };
    }

    loop {
        let wanted = adopt_avatar(profile, &mut media, &gallery);
        gallery.want(wanted);
        if state.level == Level::Stories {
            list.select(Some(state.stories_selected));
        }
        tui.terminal.draw(peek_frame!())?;
        let read = gallery
            .read(fetching(&shared))
            .await
            .map_err(input::unreadable)?;
        let key = match read {
            Raw::Tick | Raw::Resized | Raw::Paste(_) => continue,
            Raw::Key(key) => key,
        };
        note.clear();

        if state.level == Level::Stories {
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            if plain && key.code == KeyCode::Esc {
                state.level = Level::Card;
                continue;
            }
            let action = action_of(key);
            let total = media.stories.len();
            if let Some(to) = moved(state.stories_selected, total, page_rows, action) {
                state.stories_selected = to;
                continue;
            }
            match action {
                Action::Open => {
                    let busy = Busy::begin(
                        tui,
                        format!("Opening story {}...", state.stories_selected + 1),
                        peek_frame!(),
                    )?;
                    let (text, stopped) = story_open(
                        busy,
                        tui,
                        app,
                        profile,
                        &mut media,
                        state.stories_selected,
                        scratch,
                    )
                    .await;
                    note = text;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                }
                Action::Download => {
                    let busy = Busy::begin(
                        tui,
                        format!("Saving story {}...", state.stories_selected + 1),
                        peek_frame!(),
                    )?;
                    let (text, stopped) = story_keep(
                        busy,
                        tui,
                        app,
                        profile,
                        &mut media,
                        state.stories_selected,
                        receipts,
                    )
                    .await;
                    note = text;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                }
                // `q` is back here, as it is on the card under this list.
                Action::Back | Action::Quit => state.level = Level::Card,
                Action::Redraw => tui.terminal.clear()?,
                Action::Interrupt => return Ok(Some(ExitCode::Interrupted)),
                Action::Web => {
                    note = tui::web_note(&tui::page_of(&profile.username), receipts);
                }
                Action::Help => {
                    help::show(
                        tui,
                        "Stories",
                        &preview::with_key(PEEK_STORIES_KEYS),
                        peek_frame!(),
                    )?;
                }
                Action::Preview => {
                    let at = state.stories_selected;
                    let kind = media.stories.get(at).map(|story| story.kind);
                    if let Some(why) = preview::refused(kind) {
                        note = why;
                        continue;
                    }
                    let busy =
                        Busy::begin(tui, format!("Fetching story {}...", at + 1), peek_frame!())?;
                    let (file, stopped) =
                        story_fetched(busy, tui, app, profile, &mut media, at, scratch).await;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                    note = preview::show_file(tui, file, peek_frame!())?;
                }
                // The moves were taken above; an account changes from the
                // list this card was opened from.
                Action::Up
                | Action::Down
                | Action::PageUp
                | Action::PageDown
                | Action::First
                | Action::Last
                | Action::Accounts
                | Action::None => {}
            }
            continue;
        }

        let here = opening
            .iter()
            .position(|at| *at == state.row)
            .map(|place| (place, rows[state.row]));
        match peek_key(key) {
            Peeked::Back => return Ok(None),
            Peeked::Interrupt => return Ok(Some(ExitCode::Interrupted)),
            Peeked::Redraw => tui.terminal.clear()?,
            Peeked::Browser => {
                note = tui::web_note(&tui::page_of(&profile.username), receipts);
            }
            Peeked::Help => {
                help::show(tui, "Profile", &preview::with_key(PEEK_KEYS), peek_frame!())?
            }
            Peeked::About => {
                if says_something(profile) {
                    show_about(tui, profile, peek_frame!())?;
                } else {
                    note = nothing_said(profile);
                }
            }
            Peeked::Up => {
                if let Some((place, _)) = here {
                    state.row = opening[place.saturating_sub(1)];
                }
            }
            Peeked::Down => {
                if let Some((place, _)) = here {
                    state.row = opening[(place + 1).min(opening.len() - 1)];
                }
            }
            Peeked::Open => match here {
                Some((_, CardRow::Stories)) => {
                    state.level = Level::Stories;
                    list = TableState::default();
                }
                Some((_, CardRow::Picture)) => {
                    let busy = Busy::begin(tui, "Opening the picture...", peek_frame!())?;
                    let (text, stopped) = busy
                        .run(
                            tui,
                            app.client().pacer().cancel_token(),
                            pfp_open(app, profile, &mut media, scratch),
                        )
                        .await;
                    note = text;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                }
                _ => {}
            },
            Peeked::Save => {
                if let Some((_, CardRow::Picture)) = here {
                    let busy = Busy::begin(tui, "Saving the picture...", peek_frame!())?;
                    let (text, stopped) = busy
                        .run(
                            tui,
                            app.client().pacer().cancel_token(),
                            pfp_keep(app, profile, &mut media, receipts),
                        )
                        .await;
                    note = text;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                }
            }
            Peeked::Preview => {
                if let Some((_, CardRow::Picture)) = here {
                    if let Some(why) = preview::refused(None) {
                        note = why;
                        continue;
                    }
                    let busy = Busy::begin(tui, "Fetching the picture...", peek_frame!())?;
                    let (fetched, stopped) = busy
                        .run(
                            tui,
                            app.client().pacer().cancel_token(),
                            ensure_pfp(app, profile, &mut media),
                        )
                        .await;
                    if let Some(code) = stopped.leave() {
                        return Ok(Some(code));
                    }
                    note = match (fetched, media.pfp.as_ref()) {
                        (Err(e), _) => could_not_fetch_the_picture(&e),
                        (Ok(()), Some(picture)) => preview::show(
                            tui,
                            &picture_name(profile),
                            &picture.bytes,
                            peek_frame!(),
                        )?,
                        (Ok(()), None) => String::new(),
                    };
                }
            }
            Peeked::Ignored => {}
        }
    }
}

/// One frame of [`peek`]: the card, or the stories it opened.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_peek(
    frame: &mut Frame<'_>,
    profile: &Profile,
    media: &Media,
    state: &State,
    rows: &[CardRow],
    own: bool,
    viewer: &Viewer,
    note: &str,
    colors: bool,
    list: &mut TableState,
    page_rows: &mut usize,
) {
    if state.level == Level::Stories {
        crate::ui::stories::draw_items_view(
            frame,
            stories_title(profile, media),
            PEEK_STORIES_HINT,
            &media.stories,
            true,
            viewer,
            list,
            note,
            colors,
            page_rows,
        );
    } else {
        card_frame(
            frame, profile, media, state, rows, own, viewer, note, colors, true,
        );
    }
}

/// The rows the card a list opens draws: the card's, with the picture in
/// the place of the actions button, unless the account never set one.
fn viewing_rows(profile: &Profile) -> Vec<CardRow> {
    let mut rows = card_rows(profile);
    rows.retain(|row| *row != CardRow::Actions);
    if !profile.pfp_anonymous {
        rows.insert(0, CardRow::Picture);
    }
    rows
}

// ---------------------------------------------------------------------------
// Frames. Pure rendering: state in, cells out, no terminal asked anything.

/// The widest the card grows. Past this a line of facts stops being a card
/// and starts being a banner; on a wide terminal the card centers instead.
const CARD_MAX_W: u16 = 96;

/// The most rows of the card a bio takes. Instagram's own form ends a bio
/// at 150 characters, under two lines of a card this wide, so what takes
/// more is the line breaks a bio is laid out with; six shows most of those
/// whole, and `i` shows any bio whole.
const BIO_ROWS: usize = 6;

/// The rows of bio the card holds on to while there is air left to give up.
const BIO_ROWS_KEPT: usize = 2;

/// How the card is fitted to the rows the terminal has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    /// The rows the bio may take.
    bio: usize,
    /// A blank row under the about block and between each pair of rows.
    gaps: bool,
    /// The blank row inside the top border and inside the bottom one.
    padded: bool,
    /// The picture drawn at the left of the about block, the text beside
    /// it. Then the picture's row is that drawing and has no band of its
    /// own ([`banded`]).
    avatar: bool,
}

/// The picture on the card: this many columns, which at a pixel a column
/// and two a row is a square of 16 by 16, and a row under it that says what
/// it is and carries the cursor. Small on purpose: it says who the card is
/// about at a glance, and Enter is one key away from the real thing.
const AVATAR_W: u16 = 16;
const AVATAR_ROWS: u16 = AVATAR_W / 2 + 1;

/// The columns between the picture and the text beside it.
const AVATAR_GAP: u16 = 3;

impl Fit {
    /// The card where there is room for all of it.
    const WHOLE: Fit = Fit {
        bio: BIO_ROWS,
        gaps: true,
        padded: true,
        avatar: false,
    };

    /// The columns the about block's text has, of the card's `text_w`.
    fn about_width(self, text_w: usize) -> usize {
        if self.avatar {
            text_w.saturating_sub(usize::from(AVATAR_W + AVATAR_GAP))
        } else {
            text_w
        }
    }

    fn padding(self) -> Padding {
        if self.padded {
            tui::card_padding()
        } else {
            Padding::horizontal(2)
        }
    }
}

/// The card's width: at most [`CARD_MAX_W`].
fn card_width(area: Rect) -> u16 {
    area.width.min(CARD_MAX_W)
}

/// The columns of text inside the card: its width less the borders and the
/// two columns of padding each side.
fn text_width(area: Rect) -> usize {
    card_width(area).saturating_sub(2 + 4) as usize
}

/// The card's height under `fit`: the borders, the padding, the about
/// block, the rows, and the blank rows between them all.
fn card_height(profile: &Profile, rows: &[CardRow], text_w: usize, fit: Fit) -> u16 {
    2 + if fit.padded { 2 } else { 0 } + about_rows(profile, text_w, fit) + {
        let banded = banded(rows, fit);
        let body: u16 = banded.clone().map(row_height).sum();
        let air = if fit.gaps { banded.count() as u16 } else { 0 };
        body + air
    }
}

/// The rows the about block takes: its text's, and no fewer than the
/// picture's where one is drawn beside it.
fn about_rows(profile: &Profile, text_w: usize, fit: Fit) -> u16 {
    let text = about_lines(profile, fit.about_width(text_w), fit.bio, false).len() as u16;
    if fit.avatar {
        text.max(AVATAR_ROWS)
    } else {
        text
    }
}

/// The rows that get a band of their own under the about block: all of
/// them, less the picture's where it is drawn in the about block.
fn banded(rows: &[CardRow], fit: Fit) -> impl Iterator<Item = &CardRow> + Clone {
    rows.iter()
        .filter(move |row| !(fit.avatar && **row == CardRow::Picture))
}

/// Fits the card to the rows of `area`, giving up first what costs least:
/// the bio's rows past the second, since the whole of it is one `i` away;
/// then the blank rows between the bands; then the ones inside the border;
/// and the bio's second row last. **Never a row of the card**: a row is
/// something to open, and one that fell off the bottom of a short terminal
/// was there to move onto and not to see, which is what this replaced. A
/// terminal too short for even that cuts the card at its bottom edge.
///
/// `pictured` is whether the card draws its picture (`Media::pictured`).
/// The picture is the first thing given up: it is drawn only where the
/// whole card fits with it, since under that it would be taking rows from
/// the bio to show sixteen pixels of a face.
fn fitted(profile: &Profile, rows: &[CardRow], area: Rect, pictured: bool) -> Fit {
    let text_w = text_width(area);
    let fits = |fit: Fit| card_height(profile, rows, text_w, fit) <= area.height;
    let with_the_picture = Fit {
        avatar: true,
        ..Fit::WHOLE
    };
    if pictured && rows.contains(&CardRow::Picture) && fits(with_the_picture) {
        return with_the_picture;
    }
    let mut fit = Fit::WHOLE;
    while fit.bio > BIO_ROWS_KEPT && !fits(fit) {
        fit.bio -= 1;
    }
    if !fits(fit) {
        fit.gaps = false;
    }
    if !fits(fit) {
        fit.padded = false;
    }
    if !fits(fit) {
        fit.bio = 1;
    }
    fit
}

fn row_height(row: &CardRow) -> u16 {
    if *row == CardRow::Counts { 2 } else { 1 }
}

/// The bio as the card shows it, and whether that left any of it out:
/// wrapped, its blank lines dropped, since a row of the card is worth more
/// than a pause, and at most `rows` rows, the last ending in `…` where there
/// was more. `i` shows all of it, as it was written ([`about_text`]).
fn bio_on_the_card(bio: &str, width: usize, rows: usize) -> (Vec<String>, bool) {
    let width = width.max(8);
    let mut lines: Vec<String> = tui::wrapped(bio, width)
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect();
    let cut = lines.len() > rows;
    if cut {
        lines.truncate(rows);
        if let Some(last) = lines.last_mut() {
            while measure_text_width(last) + 1 > width {
                last.pop();
            }
            last.truncate(last.trim_end().len());
            last.push('…');
        }
    }
    (lines, cut)
}

/// Whether the card shows less than the whole bio at this width and fit:
/// what the bottom edge offers `i` for.
fn bio_is_cut(profile: &Profile, width: usize, rows: usize) -> bool {
    profile
        .biography
        .as_deref()
        .is_some_and(|bio| bio_on_the_card(bio, width, rows).1)
}

/// The about block's lines: the full name in bold, the category dim, the
/// bio plain, on at most `bio_rows` rows — it is the one thing here the
/// person wrote, and the least aside-like fact on the card — and the link
/// in the accent, as a link is on the site.
fn about_lines(
    profile: &Profile,
    width: usize,
    bio_rows: usize,
    colors: bool,
) -> Vec<Line<'static>> {
    let dim = tui::paint(colors, tui::DIM);
    let mut lines = Vec::new();
    if let Some(name) = &profile.full_name {
        lines.push(Line::from(Span::styled(
            name.clone(),
            tui::paint(colors, tui::BOLD),
        )));
    }
    if let Some(category) = &profile.category {
        lines.push(Line::from(Span::styled(category.clone(), dim)));
    }
    if let Some(bio) = &profile.biography {
        for line in bio_on_the_card(bio, width, bio_rows).0 {
            lines.push(Line::from(line));
        }
    }
    if let Some(url) = &profile.external_url {
        lines.push(Line::from(Span::styled(
            url.clone(),
            tui::paint(colors, Role::Accent.style()),
        )));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Everything the account says about itself, for the box `i` opens: the
/// name, the category, the bio as it was written, blank lines and all, and
/// the link. Wrapped here, so the box knows how far it can scroll.
fn about_text(profile: &Profile, width: usize, colors: bool) -> Vec<Line<'static>> {
    let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut heading = Vec::new();
    if let Some(name) = &profile.full_name {
        for line in tui::wrapped(name, width) {
            heading.push(Line::from(Span::styled(
                line,
                tui::paint(colors, tui::BOLD),
            )));
        }
    }
    if let Some(category) = &profile.category {
        for line in tui::wrapped(category, width) {
            heading.push(Line::from(Span::styled(line, tui::paint(colors, tui::DIM))));
        }
    }
    blocks.push(heading);
    if let Some(bio) = &profile.biography {
        blocks.push(
            tui::wrapped(bio, width)
                .into_iter()
                .map(Line::from)
                .collect(),
        );
    }
    if let Some(url) = &profile.external_url {
        blocks.push(
            tui::wrapped(url, width)
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(line, tui::paint(colors, Role::Accent.style())))
                })
                .collect(),
        );
    }
    // A blank line between the blocks that are there, and none around them.
    let mut lines = Vec::new();
    for block in blocks.into_iter().filter(|block| !block.is_empty()) {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(block);
    }
    lines
}

/// The widest the box `i` opens grows: a line of prose is read, not scanned.
const ABOUT_MAX_W: u16 = 72;

/// The box `i` opens over the card: [`about_text`], from line `scroll` on.
/// Hands back the furthest it can scroll, which is 0 when all of it fits.
fn about_modal(frame: &mut Frame<'_>, profile: &Profile, scroll: usize, colors: bool) -> usize {
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    tui::veil(frame, colors);

    let screen = frame.area();
    let width = screen.width.min(ABOUT_MAX_W);
    let text = about_text(profile, width.saturating_sub(2 + 4).max(8) as usize, colors);
    let height = (2 + 2 + text.len() as u16).min(screen.height);
    let area = tui::modal_area(screen, width, height);
    let rows = height.saturating_sub(2 + 2) as usize;
    let furthest = text.len().saturating_sub(rows);
    let scroll = scroll.min(furthest);
    let closes = if furthest > 0 {
        " ↑↓ scroll · any other key closes "
    } else {
        " any key closes "
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(tui::card_padding())
        .title_top(Line::from(Span::styled(
            format!(" About @{} ", printable(&profile.username)),
            bold,
        )))
        .title_bottom(Line::from(Span::styled(closes, dim)).centered());
    let inner = tui::open_modal(frame, area, block);
    let shown: Vec<Line<'static>> = text.into_iter().skip(scroll).take(rows).collect();
    frame.render_widget(Paragraph::new(shown), inner);
    if furthest > 0 {
        // The bar's travel is the offsets there are, the first to the last.
        tui::scrollbar(frame, area, furthest + 1, scroll, rows as u16);
    }
    furthest
}

/// Whether the account says anything about itself that `i` could show.
fn says_something(profile: &Profile) -> bool {
    profile.full_name.is_some()
        || profile.category.is_some()
        || profile.biography.is_some()
        || profile.external_url.is_some()
}

/// `i`: all the account says about itself, over the view `under` draws,
/// until a key that does not scroll it is pressed. Nothing is fetched: it is
/// what the card was opened with. The key that closes it is spent doing so,
/// as in [`help::show`].
fn show_about(
    tui: &mut Tui,
    profile: &Profile,
    mut under: impl FnMut(&mut Frame<'_>),
) -> Result<()> {
    let colors = tui::colors_enabled();
    let mut scroll = 0usize;
    let mut furthest = 0usize;
    let mut rows = 1usize;
    loop {
        tui.terminal.draw(|frame| {
            under(frame);
            furthest = about_modal(frame, profile, scroll, colors);
            rows = frame.area().height.saturating_sub(2 + 2).max(1) as usize;
        })?;
        scroll = scroll.min(furthest);
        let key = match input::read(input::tick()).map_err(input::unreadable)? {
            Raw::Tick | Raw::Resized | Raw::Paste(_) => continue,
            Raw::Key(key) => key,
        };
        match action_of(key) {
            Action::Up => scroll = scroll.saturating_sub(1),
            Action::Down => scroll = (scroll + 1).min(furthest),
            Action::PageUp => scroll = scroll.saturating_sub(page(rows)),
            Action::PageDown => scroll = (scroll + page(rows)).min(furthest),
            Action::First => scroll = 0,
            Action::Last => scroll = furthest,
            _ => return Ok(()),
        }
    }
}

/// Whether a key asks for the box of [`show_about`]: `i`, with nothing held.
/// Bound here and not in `action_of`, since only the two cards have one.
fn asks_about(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('i')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Draws the card: identity on the border, an about block, the focusable
/// rows each in its own band with air between them, one voice at the bottom
/// — and, when a question is pending, the question as a modal over all of
/// it, in the middle of the screen where the eyes are.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw_card(
    frame: &mut Frame<'_>,
    profile: &Profile,
    media: &Media,
    state: &State,
    rows: &[CardRow],
    own: bool,
    viewer: &Viewer,
    note: &str,
    colors: bool,
) {
    card_frame(
        frame, profile, media, state, rows, own, viewer, note, colors, false,
    );
}

/// The card, with `viewing` saying it is the one the account list opens
/// ([`peek`]): then only the rows that card can open take the cursor and say
/// they open ([`opens_in_a_list`]), and the bottom edge carries these hints
/// instead of the card's.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn card_frame(
    frame: &mut Frame<'_>,
    profile: &Profile,
    media: &Media,
    state: &State,
    rows: &[CardRow],
    own: bool,
    viewer: &Viewer,
    note: &str,
    colors: bool,
    viewing: bool,
) {
    let area = frame.area();
    let here = rows[state.row.min(rows.len() - 1)];
    let opens = |row: CardRow| !viewing || opens_in_a_list(row);
    let focus = opens(here).then_some(here);
    let dim = tui::paint(colors, tui::DIM);

    // The account acted as goes on the frame's top row rather than on the
    // card's border, whose top-right holds the badges; the card is laid out
    // in the rows under it, so the two never overlap.
    let [label, below] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    frame.render_widget(Paragraph::new(tui::viewer_line(viewer, colors)), label);
    let fit = fitted(profile, rows, below, media.pictured);
    let text_w = text_width(below);
    let h = card_height(profile, rows, text_w, fit).min(below.height);
    let card = below.centered(Constraint::Length(card_width(below)), Constraint::Length(h));
    let cut = bio_is_cut(profile, fit.about_width(text_w), fit.bio);

    // The identity on the border: the handle bold on the left, what the
    // account is and how you stand to it dim on the right.
    let mut attrs: Vec<&str> = badges(profile);
    attrs.extend(relation_words(profile));
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(fit.padding())
        .title_top(Line::from(Span::styled(
            format!(" @{} ", printable(&profile.username)),
            tui::paint(colors, tui::accent_bold()),
        )));
    if !attrs.is_empty() {
        let text = format!(" {} ", attrs.join(" · "));
        block = block.title_top(Line::from(Span::styled(text, dim)).right_aligned());
    }
    block = block.title_bottom(if viewing {
        let several = rows.iter().filter(|row| opens_in_a_list(**row)).count() > 1;
        tui::footer(note, &peek_hint(focus, several, cut), colors)
    } else {
        card_footer_line(here, cut, note, colors)
    });
    let inner = block.inner(card);
    frame.render_widget(block, card);
    let width = inner.width as usize;

    // The vertical bands, built from the rows this profile has: an absent
    // part is not a band that says "nothing". Band 0 is the about block;
    // `at` is where each row landed, which depends on whether this terminal
    // has the rows for a blank one between them.
    let about = about_lines(profile, fit.about_width(width), fit.bio, colors);
    let mut constraints = vec![Constraint::Length(about_rows(profile, width, fit))];
    let mut at = Vec::with_capacity(rows.len());
    for row in banded(rows, fit) {
        if fit.gaps {
            constraints.push(Constraint::Length(1));
        }
        at.push(constraints.len());
        constraints.push(Constraint::Length(row_height(row)));
    }
    constraints.push(Constraint::Fill(1));
    let bands = Layout::vertical(constraints).split(inner);
    let about_at = if fit.avatar {
        let [picture, _, text] = Layout::horizontal([
            Constraint::Length(AVATAR_W),
            Constraint::Length(AVATAR_GAP),
            Constraint::Fill(1),
        ])
        .areas(bands[0]);
        draw_avatar(
            frame,
            picture,
            media.avatar.as_ref(),
            focus == Some(CardRow::Picture),
            colors,
        );
        text
    } else {
        bands[0]
    };
    frame.render_widget(Paragraph::new(about).wrap(Wrap { trim: false }), about_at);

    for (row, at) in banded(rows, fit).zip(at) {
        let band = bands[at];
        match row {
            CardRow::Actions => draw_row_button(
                frame,
                band,
                "actions on this account",
                None,
                focus == Some(CardRow::Actions),
                opens(CardRow::Actions),
                colors,
            ),
            CardRow::Picture => draw_row_button(
                frame,
                band,
                "profile picture",
                media.pfp_is_a_request.then_some("one request"),
                focus == Some(CardRow::Picture),
                opens(CardRow::Picture),
                colors,
            ),
            CardRow::Stories => {
                let n = media.stories.len();
                let label = if n == 1 {
                    "1 story up".to_string()
                } else {
                    format!("{n} stories up")
                };
                draw_row_button(
                    frame,
                    band,
                    &label,
                    None,
                    focus == Some(CardRow::Stories),
                    opens(CardRow::Stories),
                    colors,
                );
            }
            CardRow::FollowedBy => {
                let Some(mutual) = profile.mutual.as_ref() else {
                    continue;
                };
                let aside = if mutual.preview.is_empty() {
                    None
                } else {
                    Some(mutual.preview.join(", "))
                };
                draw_row_button(
                    frame,
                    band,
                    &format!("followed by {} you follow", grouped(mutual.count)),
                    aside.as_deref(),
                    focus == Some(CardRow::FollowedBy),
                    opens(CardRow::FollowedBy),
                    colors,
                );
            }
            CardRow::Counts => draw_counts(
                frame,
                band,
                profile,
                focus == Some(CardRow::Counts),
                state.counts_col,
                colors,
            ),
            CardRow::Highlights => {
                let labels: Vec<String> = media
                    .tray
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, entry)| {
                        let count = media.folders[i]
                            .items
                            .as_ref()
                            .map(|items| items.len() as u64)
                            .or(entry.declared_items);
                        match count {
                            Some(n) => format!("{} · {n}", entry_title(entry)),
                            None => entry_title(entry),
                        }
                    })
                    .collect();
                let selected = (focus == Some(CardRow::Highlights)).then_some(state.hl_col);
                frame.render_widget(
                    Paragraph::new(chip_row("Highlights  ", &labels, selected, width, colors)),
                    band,
                );
            }
        }
    }

    // The question, over everything and where the eyes are. Drawn last: the
    // veil dims what is already on screen, and the modal's Clear keeps the
    // veil out of the modal.
    if state.ask != Ask::None {
        tui::veil(frame, colors);
        question_modal(frame, &ask_wording(state.ask, own, profile), colors);
    }
}

/// One focusable row: a label, an optional dim aside, and a dim `›` at the
/// right edge saying "this opens" (`opens`; the read-only card has no such
/// rows). The selection covers the whole band, the
/// way a row of a list would, not just the text.
fn draw_row_button(
    frame: &mut Frame<'_>,
    band: Rect,
    label: &str,
    aside: Option<&str>,
    focused: bool,
    opens: bool,
    colors: bool,
) {
    let dim = tui::DIM;
    let marker = if focused && !colors { ">" } else { " " };
    let mut spans = vec![Span::raw(format!("{marker} {label}"))];
    if let Some(aside) = aside {
        let text = format!("   {aside}");
        spans.push(if colors {
            Span::styled(text, dim)
        } else {
            Span::raw(text)
        });
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), band);
    if opens {
        frame.render_widget(
            Paragraph::new(
                Line::from(if colors {
                    Span::styled("› ", dim)
                } else {
                    Span::raw("› ")
                })
                .right_aligned(),
            ),
            band,
        );
    }
    if focused && colors {
        frame.buffer_mut().set_style(band, tui::selection());
    }
}

/// The counters as three tiles: the label dim on top, the number bold under
/// it, and the focused tile inverted whole — `column_highlight_style` covers
/// the full height of the rows area, which is exactly the two lines. The
/// focused tile's label is not dim: dim and reverse video together is a
/// faint background with the label lost in it on more than one theme.
fn draw_counts(
    frame: &mut Frame<'_>,
    band: Rect,
    profile: &Profile,
    focused: bool,
    col: usize,
    colors: bool,
) {
    let dim = tui::DIM;
    let tiles = [
        ("posts", profile.posts),
        ("followers", profile.followers),
        ("following", profile.following),
    ];
    // One width for all three: the widest of the six texts, plus a blank
    // cell each side so the inverted tile reads as a button.
    let w = tiles
        .iter()
        .flat_map(|(label, n)| [label.len(), counted(*n).len()])
        .max()
        .unwrap_or(6) as u16
        + 2;

    let row = Row::new(tiles.iter().enumerate().map(|(i, (label, n))| {
        let label_line = if colors && focused && i == col {
            Line::from((*label).to_string()).centered()
        } else if colors {
            Line::from(Span::styled((*label).to_string(), dim)).centered()
        } else {
            let marker = if focused && i == col { ">" } else { " " };
            Line::from(format!("{marker}{label}")).centered()
        };
        let count_line = tui::styled_line(counted(*n), tui::BOLD, colors).centered();
        Cell::from(Text::from(vec![label_line, count_line]))
    }))
    .height(2);

    let mut ts = TableState::default();
    if focused && colors {
        ts.select_column(Some(col));
    }
    frame.render_stateful_widget(
        Table::new([row], [Constraint::Length(w); 3])
            .column_spacing(3)
            .column_highlight_style(tui::selection()),
        band,
        &mut ts,
    );
}

/// "1,234", or "?" when Instagram did not say.
fn counted(n: Option<u64>) -> String {
    n.map(grouped).unwrap_or_else(|| "?".to_string())
}

/// The card's bottom edge: the note when there is one, otherwise the hints
/// for the row under the cursor. A pending question is a modal, not a line
/// here.
fn card_footer_line(here: CardRow, cut: bool, note: &str, colors: bool) -> Line<'static> {
    tui::footer(note, &card_hint(here, cut), colors)
}

/// The account's picture at the left of the about block: the picture, once
/// it has arrived, and under it the row that says what it is and carries
/// the cursor, since a picture cannot be drawn in reverse video and still be
/// the picture. Until it arrives its place is kept, empty, so that nothing
/// on the card moves when it does.
fn draw_avatar(
    frame: &mut Frame<'_>,
    area: Rect,
    picture: Option<&preview::Pixels>,
    focused: bool,
    colors: bool,
) {
    let [place, label] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(
        Rect::new(area.x, area.y, area.width, area.height.min(AVATAR_ROWS)),
    );
    if let Some(picture) = picture {
        picture.draw_in(frame, place);
    }
    let style = if focused {
        tui::paint(colors, tui::selection())
    } else {
        tui::paint(colors, tui::DIM)
    };
    let text = if focused { "picture ›" } else { "picture" };
    frame.render_widget(
        Paragraph::new(Line::from(text).centered()).style(style),
        label,
    );
}

/// The card's hints. `cut` is whether the bio is shown short of whole, which
/// is when `i` is worth naming here. The account key gives its place to the
/// save on the highlights row, so that the line still fits a card 80 columns
/// wide; `?` names every key.
fn card_hint(here: CardRow, cut: bool) -> String {
    let mut hint = String::from("↑↓ move · ←→ pick · enter open");
    hint.push_str(match here {
        CardRow::Highlights => " · d save all",
        CardRow::Picture => " · d save",
        _ => " · a account",
    });
    if cut {
        hint.push_str(" · i bio");
    }
    hint.push_str(" · ? keys · q quit");
    hint
}

/// What the modal needs to say no twice and yes once, for measuring.
const ANSWER_WORDS: &str = "y  walk it now    n  leave it    esc = no"; // layout-allow: the gaps are the columns being measured
const MODAL_MIN_W: u16 = 46;
const MODAL_MAX_W: u16 = 72;

/// The size a question asks for: wide enough for the question or the answer
/// row plus padding and borders, bounded both ways; tall enough for the
/// wrapped question, a blank line, and the answers.
fn question_size(question: &str, area: Rect) -> (u16, u16) {
    let content = measure_text_width(question).max(measure_text_width(ANSWER_WORDS)) as u16;
    let width = (content + 6)
        .clamp(MODAL_MIN_W, MODAL_MAX_W)
        .min(area.width);
    let text_w = width.saturating_sub(6) as usize;
    let lines = tui::wrapped(question, text_w.max(8)).len().clamp(1, 3) as u16;
    let height = (2 + 2 + lines + 1 + 1).min(area.height);
    (width, height)
}

/// A y/n question about a walk, as a modal over the frame: double border —
/// the only double in the program, the stroke that says "on top" without
/// naming a color — over a `Clear`, at the optical center. The card asks its
/// pending question with it, and the account list asks with it before walking
/// again as another account.
///
/// No default answer on purpose: `ui::confirm` prints `[Y/n]`, but a walk of
/// three hundred pages must not have an Enter that quietly starts it.
pub(crate) fn question_modal(frame: &mut Frame<'_>, question: &str, colors: bool) {
    let dim = tui::paint(colors, tui::DIM);
    let bold = tui::paint(colors, tui::BOLD);
    let (w, h) = question_size(question, frame.area());
    let area = tui::modal_area(frame.area(), w, h);
    let block = Block::bordered()
        .border_type(BorderType::Double)
        .padding(tui::card_padding())
        .title_top(Line::from(Span::styled(" a question ", dim)).centered());
    let inner = tui::open_modal(frame, area, block);

    let [text_band, _, answer_band] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(tui::styled_line(question, tui::BOLD, colors)).wrap(Wrap { trim: false }),
        text_band,
    );
    let [yes, no] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(answer_band);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("y", bold),
            Span::raw("  walk it now"),
        ])),
        yes,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("n", bold),
            Span::raw("  leave it"),
            Span::styled("    esc = no", dim),
        ])),
        no,
    );
}

/// What a `y` would spend, named before it is spent.
fn ask_wording(ask: Ask, own: bool, profile: &Profile) -> String {
    let whose = if own {
        "your".to_string()
    } else {
        format!("@{}'s", printable(&profile.username))
    };
    match ask {
        Ask::List(kind) => {
            let count = match kind {
                ListKind::Followers => profile.followers,
                ListKind::Following => profile.following,
            };
            let count = count
                .map(|n| format!("{} ", grouped(n)))
                .unwrap_or_default();
            format!("Walk {whose} {count}{kind} now?")
        }
        Ask::Scan => format!("Walk and cross {whose} followers and following now?"),
        Ask::None => String::new(),
    }
}

/// The actions submenu as a panel over the card: the card stays visible
/// under the veil, because the submenu acts on the account it shows.
fn draw_actions_panel(
    frame: &mut Frame<'_>,
    profile: &Profile,
    selected: usize,
    note: &str,
    colors: bool,
) {
    let dim = tui::paint(colors, tui::DIM);
    tui::veil(frame, colors);

    let rows = [
        ("profile picture", "look at it or save it"),
        ("scan", "followers and following, crossed"),
    ];
    let name_w = 16u16;
    let what_w = rows.iter().map(|(_, what)| what.len()).max().unwrap_or(20) as u16;
    let w = (2 + 4 + 2 + name_w + 2 + what_w)
        .clamp(44, 72)
        .min(frame.area().width);
    let area = tui::modal_area(frame.area(), w, 6);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(tui::card_padding())
        .title_top(
            Line::from(Span::styled(
                format!(" actions on @{} ", printable(&profile.username)),
                dim,
            ))
            .centered(),
        )
        .title_bottom(tui::footer(
            note,
            "enter open · d save the picture · esc back",
            colors,
        ));
    let inner = tui::open_modal(frame, area, block);

    let mut ts = TableState::default();
    ts.select(Some(selected));
    frame.render_stateful_widget(
        tui::list_table(
            rows.iter().map(|(name, what)| {
                Row::new(vec![
                    Cell::from((*name).to_string()),
                    Cell::from((*what).to_string()).style(dim),
                ])
            }),
            [Constraint::Length(name_w), Constraint::Fill(1)],
        ),
        inner,
        &mut ts,
    );
}

/// One chip. Focused: the text with a blank cell each side, inverted — the
/// button a terminal knows how to draw, no brackets needed. Unfocused: the
/// text plain. The `>` is what is left when there is no styling at all.
fn chip(text: &str, focused: bool, colors: bool) -> Span<'static> {
    if focused {
        if colors {
            Span::styled(format!(" {text} "), tui::selection())
        } else {
            Span::raw(format!(">{text} "))
        }
    } else {
        Span::raw(format!(" {text} "))
    }
}

/// Which chips fit: the smallest window that keeps `selected` whole, grown
/// rightward while there is room. Widths are display columns; `sep` is what
/// sits between two chips.
fn chip_window(
    widths: &[usize],
    sep: usize,
    selected: usize,
    width: usize,
) -> std::ops::Range<usize> {
    if widths.is_empty() {
        return 0..0;
    }
    let selected = selected.min(widths.len() - 1);
    let mut start = selected;
    let mut used = widths[selected];
    while start > 0 && used + sep + widths[start - 1] <= width {
        start -= 1;
        used += sep + widths[start];
    }
    let mut end = selected + 1;
    while end < widths.len() && used + sep + widths[end] <= width {
        used += sep + widths[end];
        end += 1;
    }
    start..end
}

/// One physical row of chips, windowed to `width`, with a dim marker at
/// either end that has chips cut off. The plain labels are measured; the
/// styling lives in spans, so it never counts as columns. Three spaces
/// between chips: with the focused chip an inverted block, the gap alone
/// separates, and a glyph would fight the `·` inside the labels.
fn chip_row(
    prefix: &str,
    labels: &[String],
    selected: Option<usize>,
    width: usize,
    colors: bool,
) -> Line<'static> {
    // Each chip costs its label plus the blank cell each side.
    let widths: Vec<usize> = labels.iter().map(|l| measure_text_width(l) + 2).collect();
    // One column for `‹`, two for `. ›`.
    let room = width
        .saturating_sub(measure_text_width(prefix))
        .saturating_sub(3);
    let window = chip_window(&widths, 3, selected.unwrap_or(0), room);

    let dim = tui::paint(colors, tui::DIM);
    let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix.to_string())];
    if window.start > 0 {
        spans.push(Span::styled("‹", dim));
    }
    for index in window.clone() {
        if index > window.start {
            spans.push(Span::raw("   "));
        }
        // A terminal too narrow for even one chip gets that chip cut to fit
        // rather than a row the screen clamps mid-frame.
        let label = if window.len() == 1 && widths[index] > room {
            console::truncate_str(&labels[index], room.saturating_sub(3), "…").into_owned()
        } else {
            labels[index].clone()
        };
        spans.push(chip(&label, selected == Some(index), colors));
    }
    if window.end < labels.len() {
        spans.push(Span::styled(" ›", dim));
    }
    Line::from(spans)
}

/// "1,234" — the counts the way the page groups them.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The header's short words for how the two accounts stand to each other.
///
/// Attribute-shaped on purpose — they sit beside "verified, private" — where
/// the printed document says the same thing as a sentence.
fn relation_words(profile: &Profile) -> Vec<&'static str> {
    let mut words = Vec::new();
    if let Some(r) = profile.relation {
        if r.you_follow {
            words.push("you follow them");
        } else if r.you_requested {
            words.push("you requested");
        }
        if r.follows_you {
            words.push("follows you");
        } else if r.they_requested {
            words.push("they requested");
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use snob_core::{Epoch, Pk};

    use super::*;

    /// A pending question keeps the keyboard: `a` does not open the account
    /// picker under it, and `q` does not quit.
    #[test]
    fn a_pending_question_ignores_the_account_key() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(answer(key(KeyCode::Char('a'))), Answer::Ignored);
        assert_eq!(answer(key(KeyCode::Char('q'))), Answer::Ignored);
        assert_eq!(answer(key(KeyCode::Char('y'))), Answer::Yes);
        assert_eq!(answer(key(KeyCode::Esc)), Answer::No);
        assert_eq!(
            answer(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Answer::Interrupt
        );
    }

    fn bare_profile() -> Profile {
        Profile {
            pk: Pk::new(7),
            username: "someone".to_string(),
            full_name: Some("Some One".to_string()),
            biography: None,
            external_url: None,
            is_private: false,
            is_verified: true,
            category: None,
            followers: Some(1234),
            following: Some(567),
            posts: Some(128),
            relation: Some(crate::commands::profile::Relation {
                you_follow: true,
                follows_you: false,
                you_requested: false,
                they_requested: true,
            }),
            mutual: Some(crate::commands::profile::Mutual {
                count: 33,
                preview: vec!["ana".to_string()],
                people: Vec::new(),
                complete: false,
            }),
            highlights: Visibility::Shown(vec![Highlight {
                id: "highlight:1".to_string(),
                title: "trip".to_string(),
                items: Some(5),
                updated_at: None,
            }]),
            pfp_url: None,
            pfp_full_size: None,
            pfp_anonymous: false,
            stories: Visibility::Shown(vec![]),
            read_at: Epoch::new(0),
        }
    }

    /// Absent parts are not focusable rows: nothing to open is nothing to
    /// land on.
    #[test]
    fn the_card_only_offers_what_the_profile_has() {
        let full = {
            let mut p = bare_profile();
            p.stories = Visibility::Shown(vec![story()]);
            p
        };
        assert_eq!(
            card_rows(&full),
            vec![
                CardRow::Actions,
                CardRow::Stories,
                CardRow::FollowedBy,
                CardRow::Counts,
                CardRow::Highlights
            ]
        );

        let mut hidden = bare_profile();
        hidden.stories = Visibility::Hidden;
        hidden.highlights = Visibility::Hidden;
        hidden.mutual = None;
        assert_eq!(
            card_rows(&hidden),
            vec![CardRow::Actions, CardRow::Counts],
            "a hidden tray and no mutuals leave nothing to open there"
        );

        let mut none_in_common = bare_profile();
        none_in_common.mutual.as_mut().unwrap().count = 0;
        assert!(!card_rows(&none_in_common).contains(&CardRow::FollowedBy));
    }

    fn story() -> Story {
        Story {
            kind: crate::media::Kind::Photo,
            taken_at: Epoch::new(0),
            expiring_at: None,
            url: None,
            mentions: Vec::new(),
        }
    }

    /// The window always holds the selected chip whole, and never overruns
    /// the width it was given.
    #[test]
    fn the_chip_window_keeps_the_selection_visible_and_fits() {
        let widths = [10, 8, 12, 6, 9];
        for selected in 0..widths.len() {
            for width in [12, 20, 30, 100] {
                let window = chip_window(&widths, 1, selected, width);
                assert!(
                    window.contains(&selected),
                    "selected {selected} fell out of {window:?} at width {width}"
                );
                let used: usize = window.clone().map(|i| widths[i]).sum::<usize>()
                    + window.len().saturating_sub(1);
                assert!(
                    used <= width || window.len() == 1,
                    "window {window:?} uses {used} of {width}"
                );
            }
        }
        // A width too small for even one chip still shows the selected one.
        assert_eq!(chip_window(&widths, 1, 2, 3), 2..3);
        assert_eq!(chip_window(&[], 1, 0, 10), 0..0);
    }

    /// The rendered strip never claims more columns than the terminal has:
    /// the labels are measured plain, the styling lives in spans.
    #[test]
    fn the_chip_row_measures_within_the_width() {
        let labels: Vec<String> = (0..8).map(|i| format!("highlight-{i} · 12")).collect();
        for width in [30, 50, 80] {
            let row = chip_row("Highlights: ", &labels, Some(4), width, false);
            assert!(
                row.width() <= width,
                "{} columns into {width}: {row}",
                row.width()
            );
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.contains("highlight"), "the selection fell off: {text}");
        }
        // With room, the selected chip is whole; without, it is cut to fit
        // rather than left to the screen's clamp mid-frame.
        let wide: String = chip_row("Highlights: ", &labels, Some(4), 80, false)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(wide.contains("highlight-4 · 12"));
        let narrow: String = chip_row("Highlights: ", &labels, Some(4), 30, false)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(narrow.contains('…'));
    }

    #[test]
    fn the_counts_group_their_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(128), "128");
        assert_eq!(grouped(1234), "1,234");
        assert_eq!(grouped(1234567), "1,234,567");
    }

    /// The header's relation words are attribute-shaped: the follow that
    /// exists is said, the request stands in only while nothing is settled.
    #[test]
    fn the_relation_reads_as_attributes() {
        let p = bare_profile();
        assert_eq!(
            relation_words(&p),
            vec!["you follow them", "they requested"]
        );

        let mut own = bare_profile();
        own.relation = None;
        assert!(relation_words(&own).is_empty());
    }

    /// The question names whose list and how big before anything is spent.
    #[test]
    fn the_question_names_the_size_and_the_owner() {
        let p = bare_profile();
        assert_eq!(
            ask_wording(Ask::List(ListKind::Followers), false, &p),
            "Walk @someone's 1,234 followers now?"
        );
        assert_eq!(
            ask_wording(Ask::List(ListKind::Following), true, &p),
            "Walk your 567 following now?"
        );
        assert_eq!(
            ask_wording(Ask::Scan, true, &p),
            "Walk and cross your followers and following now?"
        );
        let mut unknown = bare_profile();
        unknown.followers = None;
        assert_eq!(
            ask_wording(Ask::List(ListKind::Followers), false, &unknown),
            "Walk @someone's followers now?"
        );
    }

    /// A profile highlight becomes the tray type without inventing a date.
    #[test]
    fn a_profile_highlight_becomes_a_tray_entry() {
        let entry = entry_of(&Highlight {
            id: "highlight:9".to_string(),
            title: String::new(),
            items: Some(3),
            updated_at: Some(Epoch::new(5)),
        });
        assert_eq!(entry.id, "highlight:9");
        assert_eq!(entry.declared_items, Some(3));
        assert_eq!(entry.created_at, None);
        assert_eq!(entry.updated_at, Some(Epoch::new(5)));
        assert_eq!(entry_title(&entry), "(untitled)");
    }

    fn card_rendered(
        profile: &Profile,
        media: &Media,
        state: &State,
        rows: &[CardRow],
        note: &str,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                draw_card(
                    frame,
                    profile,
                    media,
                    state,
                    rows,
                    false,
                    &tui::me(),
                    note,
                    false,
                )
            })
            .unwrap();
        terminal
    }

    /// The card is a centered column: identity on its top border, the
    /// person's name and the focusable rows inside with air between them,
    /// and one voice on the bottom edge — the note, or the hints.
    #[test]
    fn the_card_is_a_centered_column_with_one_voice_at_the_bottom() {
        let mut profile = bare_profile();
        profile.stories = Visibility::Shown(vec![story()]);
        let rows = card_rows(&profile);
        let media = Media::of(&profile);
        let state = State {
            level: Level::Card,
            row: 0,
            counts_col: 0,
            hl_col: 0,
            stories_selected: 0,
            ask: Ask::None,
        };

        let terminal = card_rendered(&profile, &media, &state, &rows, "");
        // The account acted as takes the top row, right-aligned, and the 16
        // rows of card center in the 23 under it: the border lands on row 5.
        assert!(
            tui::row_text(&terminal, 0).ends_with(" as @me "),
            "{}",
            tui::row_text(&terminal, 0)
        );
        let top = tui::row_text(&terminal, 5);
        assert!(top.contains("@someone"), "{top}");
        assert!(top.contains("verified"), "{top}");
        assert!(tui::row_text(&terminal, 7).contains("Some One"));
        let actions = tui::row_text(&terminal, 9);
        assert!(actions.contains("actions on this account"), "{actions}");
        assert!(actions.contains('›'), "the row says it opens: {actions}");
        // The counters are tiles: labels on one row, numbers under them.
        let labels = tui::row_text(&terminal, 15);
        assert!(labels.contains("posts"), "{labels}");
        assert!(labels.contains("followers"), "{labels}");
        let numbers = tui::row_text(&terminal, 16);
        assert!(numbers.contains("1,234"), "{numbers}");
        assert!(numbers.contains("567"), "{numbers}");
        assert!(tui::row_text(&terminal, 18).contains("trip"));
        let bottom = tui::row_text(&terminal, 20);
        assert!(bottom.contains("enter open"), "{bottom}");
        // Drawn without styling, nothing on the card carries a style.
        let buffer = terminal.backend().buffer();
        for cell in &buffer.content {
            assert!(cell.modifier.is_empty(), "styled without colors: {cell:?}");
            assert_eq!(cell.fg, ratatui::style::Color::Reset, "{cell:?}");
        }

        let terminal = card_rendered(&profile, &media, &state, &rows, "Saved x");
        assert!(tui::row_text(&terminal, 20).contains("Saved x"));
    }

    /// On a terminal shorter than the card, the account keeps the top row
    /// and the card starts under it rather than drawing over it.
    #[test]
    fn a_short_terminal_keeps_the_account_above_the_card() {
        let profile = bare_profile();
        let rows = card_rows(&profile);
        let media = Media::of(&profile);
        let state = State {
            level: Level::Card,
            row: 0,
            counts_col: 0,
            hl_col: 0,
            stories_selected: 0,
            ask: Ask::None,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                draw_card(
                    frame,
                    &profile,
                    &media,
                    &state,
                    &rows,
                    false,
                    &tui::me(),
                    "",
                    false,
                );
            })
            .unwrap();
        assert!(
            tui::row_text(&terminal, 0).ends_with(" as @me "),
            "{}",
            tui::row_text(&terminal, 0)
        );
        assert!(
            tui::row_text(&terminal, 1).contains("@someone"),
            "{}",
            tui::row_text(&terminal, 1)
        );
    }

    /// The question is a modal at the optical center, never a line on the
    /// bottom border — and the card is still there behind it.
    #[test]
    fn the_question_lands_where_the_eyes_are_and_the_card_stays_behind_it() {
        let mut profile = bare_profile();
        profile.stories = Visibility::Shown(vec![story()]);
        let rows = card_rows(&profile);
        let media = Media::of(&profile);
        let state = State {
            level: Level::Card,
            row: 0,
            counts_col: 0,
            hl_col: 0,
            stories_selected: 0,
            ask: Ask::List(ListKind::Followers),
        };

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                draw_card(
                    frame,
                    &profile,
                    &media,
                    &state,
                    &rows,
                    false,
                    &tui::me(),
                    "",
                    true,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text_row =
            |y: u16| -> String { (0..100u16).map(|x| buffer[(x, y)].symbol()).collect() };

        // 1. The question sits in the upper-middle band, not the last row.
        let y = (0..30u16)
            .find(|&y| text_row(y).contains("followers now?"))
            .expect("the question is on screen");
        assert!((5..=15).contains(&y), "the question landed on row {y}");
        assert!(!text_row(29).contains("now?"), "back on the bottom edge");

        // 2. Double border, centered: the question modal is the one double.
        // The question sits two rows under the modal's top border (border,
        // padding, text).
        let corners = text_row(y - 2);
        assert!(corners.contains('╔'), "{corners}");
        assert!(corners.contains('╗'), "{corners}");

        // 3. The veil dims the card; Clear keeps it out of the modal. This
        //    is what proves there is a real modal, not a line painted over.
        assert!(
            buffer[(10, 10)].modifier.contains(Modifier::DIM),
            "the background was not veiled"
        );
        let inside = (0..100u16)
            .find(|&x| text_row(y).contains("followers now?") && buffer[(x, y)].symbol() == "W")
            .unwrap_or(40);
        assert!(
            !buffer[(inside, y)].modifier.contains(Modifier::DIM),
            "the veil leaked into the modal"
        );

        // 4. The answers say what y spends, and there is no default.
        let answers = text_row(y + 2);
        assert!(answers.contains("walk it now"), "{answers}");
        assert!(answers.contains("leave it"), "{answers}");

        // 5. The card is still behind it: a modal, not a new screen.
        assert!(
            (0..30u16).any(|yy| text_row(yy).contains("@someone")),
            "the card vanished"
        );
    }

    /// The modal's size is bounded and its height follows the wrap.
    #[test]
    fn the_question_size_is_bounded_and_wraps() {
        let area = Rect::new(0, 0, 100, 30);
        let (w, h) = question_size("Walk your 567 following now?", area);
        // The answers row is what governs a short question's width.
        assert_eq!(w, 47);
        assert_eq!(h, 7);
        let long =
            "Walk and cross @averyveryverylongaccountname's followers and following right now?";
        let (w, h) = question_size(long, area);
        assert_eq!(w, MODAL_MAX_W, "long questions get the ceiling");
        assert_eq!(h, 8, "and wrap to two lines");
    }

    /// Esc, `q`, Backspace, Left and `h` go back to the list; `w` is the
    /// browser, and `o` still is; `?` asks for the keys and `i` for the bio;
    /// the arrows and `j` `k` move, Enter, Right and `l` open, `d` saves;
    /// Ctrl+C leaves; the rest does nothing.
    #[test]
    fn the_card_a_list_opens_binds_its_own_keys() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for back in [
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('h'),
            KeyCode::Backspace,
            KeyCode::Left,
        ] {
            assert_eq!(peek_key(key(back)), Peeked::Back, "{back:?}");
        }
        assert_eq!(peek_key(key(KeyCode::Char('w'))), Peeked::Browser);
        assert_eq!(peek_key(key(KeyCode::Char('o'))), Peeked::Browser);
        assert_eq!(peek_key(key(KeyCode::Char('?'))), Peeked::Help);
        assert_eq!(peek_key(key(KeyCode::Char('i'))), Peeked::About);
        for up in [KeyCode::Up, KeyCode::Char('k')] {
            assert_eq!(peek_key(key(up)), Peeked::Up, "{up:?}");
        }
        for down in [KeyCode::Down, KeyCode::Char('j')] {
            assert_eq!(peek_key(key(down)), Peeked::Down, "{down:?}");
        }
        for open in [KeyCode::Enter, KeyCode::Right, KeyCode::Char('l')] {
            assert_eq!(peek_key(key(open)), Peeked::Open, "{open:?}");
        }
        assert_eq!(peek_key(key(KeyCode::Char('d'))), Peeked::Save);
        assert_eq!(peek_key(preview::KEY.press), Peeked::Preview);
        assert_eq!(peek_key(key(KeyCode::Char('a'))), Peeked::Ignored);
        assert_eq!(
            peek_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Peeked::Interrupt
        );
        assert_eq!(
            peek_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Peeked::Redraw
        );
        assert_eq!(
            peek_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT)),
            Peeked::Ignored
        );
    }

    /// Every key the boxes of this view name is bound to something: the
    /// card's and the panel's by the shared binding, with `i` by the card's
    /// own; the card a list opens by its own; its stories by the shared one.
    #[test]
    fn every_key_the_help_names_does_something() {
        for key in CARD_KEYS.iter().chain(ACTIONS_KEYS) {
            assert!(
                action_of(key.press) != Action::None || asks_about(key.press),
                "{}",
                key.keys
            );
        }
        for key in PEEK_KEYS {
            assert_ne!(peek_key(key.press), Peeked::Ignored, "{}", key.keys);
        }
        for key in PEEK_STORIES_KEYS {
            assert_ne!(action_of(key.press), Action::None, "{}", key.keys);
        }
        // `i` is the card's, and is no letter of anything else there.
        assert!(asks_about(ABOUT_KEY.press));
        assert_eq!(action_of(ABOUT_KEY.press), Action::None);
        assert!(!asks_about(KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::CONTROL
        )));
    }

    fn at_the_card() -> State {
        State {
            level: Level::Card,
            row: 0,
            counts_col: 0,
            hl_col: 0,
            stories_selected: 0,
            ask: Ask::None,
        }
    }

    fn screen_of(terminal: &Terminal<TestBackend>) -> String {
        let height = terminal.backend().buffer().area.height;
        (0..height)
            .map(|y| tui::row_text(terminal, y) + "\n")
            .collect()
    }

    /// A frame of the card a list opens, at this size, without styling.
    fn peeked(profile: &Profile, state: &State, width: u16, height: u16) -> String {
        let rows = viewing_rows(profile);
        let media = Media::of(profile);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut list = TableState::default();
        let mut page_rows = 1usize;
        terminal
            .draw(|frame| {
                draw_peek(
                    frame,
                    profile,
                    &media,
                    state,
                    &rows,
                    false,
                    &tui::me(),
                    "",
                    false,
                    &mut list,
                    &mut page_rows,
                );
            })
            .unwrap();
        screen_of(&terminal)
    }

    /// The card a list opens shows what the profile has (identity, counts,
    /// who you both know, highlights), has no actions row, and only the
    /// picture says it opens: the lists and the highlights are walks, and
    /// stay with `snob profile`.
    #[test]
    fn the_card_a_list_opens_shows_the_page_and_opens_only_what_is_a_file_away() {
        let profile = bare_profile();
        let rows = viewing_rows(&profile);
        assert_eq!(rows[0], CardRow::Picture);
        assert!(!rows.contains(&CardRow::Actions));
        assert!(!rows.contains(&CardRow::Stories), "it has none up");
        for row in [
            CardRow::Actions,
            CardRow::FollowedBy,
            CardRow::Counts,
            CardRow::Highlights,
        ] {
            assert!(!opens_in_a_list(row), "{row:?}");
        }

        let screen = peeked(&profile, &at_the_card(), 80, 24);
        assert!(screen.contains("@someone"), "{screen}");
        assert!(screen.contains("Some One"), "{screen}");
        assert!(screen.contains("followed by 33 you follow"), "{screen}");
        assert!(screen.contains("1,234"), "{screen}");
        assert!(screen.contains("trip"), "{screen}");
        assert!(!screen.contains("actions on this account"), "{screen}");
        assert!(screen.contains("> profile picture"), "{screen}");
        assert_eq!(
            screen.matches('›').count(),
            1,
            "the picture alone opens: {screen}"
        );
        assert!(screen.contains("enter open · d save"), "{screen}");
        assert!(screen.contains("? keys · esc back"), "{screen}");
        assert!(!screen.contains("↑↓ move"), "one row, nowhere to move");

        // An account that never set a picture, with nothing up: nothing on
        // the card opens, nothing has the cursor, and the edge says so.
        let mut plain = bare_profile();
        plain.pfp_anonymous = true;
        assert!(!viewing_rows(&plain).contains(&CardRow::Picture));
        let screen = peeked(&plain, &at_the_card(), 80, 24);
        assert!(!screen.contains('›'), "nothing here opens: {screen}");
        assert!(
            !screen.contains('>'),
            "and nothing has the cursor: {screen}"
        );
        assert!(screen.contains(PEEK_HINT), "{screen}");
    }

    /// With stories up, the card a list opens has a row for them that opens,
    /// under the picture's, and what it opens is the list of them with its
    /// own keys on the bottom edge. A picture that costs a request says so
    /// on its row.
    #[test]
    fn the_card_a_list_opens_can_open_its_stories() {
        let mut profile = bare_profile();
        profile.stories = Visibility::Shown(vec![story(), story()]);
        let rows = viewing_rows(&profile);
        assert_eq!(&rows[..2], [CardRow::Picture, CardRow::Stories]);
        assert!(opens_in_a_list(CardRow::Picture) && opens_in_a_list(CardRow::Stories));

        let mut state = at_the_card();
        state.row = 1;
        let screen = peeked(&profile, &state, 80, 24);
        assert!(screen.contains("> 2 stories up"), "{screen}");
        assert_eq!(screen.matches('›').count(), 2, "{screen}");
        assert!(screen.contains("↑↓ move · enter open · w web"), "{screen}");
        assert!(!screen.contains("d save"), "a row of stories is opened");

        state.level = Level::Stories;
        let screen = peeked(&profile, &state, 80, 24);
        assert!(screen.contains("Stories · @someone · 2 up"), "{screen}");
        assert!(screen.contains("enter open · d save"), "{screen}");
        assert!(!screen.contains("q quit"), "q is back here: {screen}");

        // The picture's row names its price when it has one.
        let mut media = Media::of(&profile);
        media.pfp_is_a_request = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                card_frame(
                    frame,
                    &profile,
                    &media,
                    &at_the_card(),
                    &rows,
                    false,
                    &tui::me(),
                    "",
                    false,
                    true,
                );
            })
            .unwrap();
        let screen = screen_of(&terminal);
        assert!(screen.contains("profile picture   one request"), "{screen}"); // layout-allow: the gap is the one the row draws
    }

    /// The hints of the card a list opens name the keys of the row under
    /// the cursor and fit the narrowest card with every part there.
    #[test]
    fn the_hints_of_the_card_a_list_opens_fit_eighty_columns() {
        assert_eq!(peek_hint(None, false, false), PEEK_HINT);
        let longest = peek_hint(Some(CardRow::Picture), true, true);
        assert!(longest.contains("i bio"), "{longest}");
        // 80 columns less the two corners and the blank each side of a hint.
        assert!(measure_text_width(&longest) <= 76, "{longest}");
        for here in [
            CardRow::Actions,
            CardRow::Stories,
            CardRow::Counts,
            CardRow::Highlights,
        ] {
            let hint = card_hint(here, true);
            assert!(measure_text_width(&hint) <= 76, "{hint}");
            assert!(hint.contains("? keys"), "{hint}");
        }
        assert!(!card_hint(CardRow::Counts, false).contains("i bio"));
    }

    /// The movement keys take a list's selection where they say, within it.
    #[test]
    fn a_selection_moves_within_its_list() {
        assert_eq!(moved(0, 5, 3, Action::Up), Some(0));
        assert_eq!(moved(3, 5, 3, Action::Down), Some(4));
        assert_eq!(moved(4, 5, 3, Action::Down), Some(4));
        assert_eq!(moved(0, 5, 3, Action::PageDown), Some(2));
        assert_eq!(moved(4, 5, 3, Action::PageUp), Some(2));
        assert_eq!(moved(2, 5, 3, Action::First), Some(0));
        assert_eq!(moved(2, 5, 3, Action::Last), Some(4));
        assert_eq!(moved(0, 0, 3, Action::Last), Some(0));
        assert_eq!(moved(2, 5, 3, Action::Open), None);
    }

    fn with_a_bio(bio: &str) -> Profile {
        let mut profile = bare_profile();
        profile.biography = Some(bio.to_string());
        profile
    }

    /// A bio laid out on several lines is on the card as it was written,
    /// line under line, with only its blank lines left out.
    #[test]
    fn a_bio_of_several_lines_is_shown_whole_when_it_fits() {
        let profile = with_a_bio("climber\n\nbased in Oslo\nprints at the shop");
        let rows = card_rows(&profile);
        let media = Media::of(&profile);
        let terminal = card_rendered(&profile, &media, &at_the_card(), &rows, "");
        let screen = screen_of(&terminal);
        let lines: Vec<&str> = screen.lines().collect();
        let first = lines
            .iter()
            .position(|line| line.contains("climber"))
            .unwrap_or_else(|| panic!("{screen}"));
        assert!(lines[first + 1].contains("based in Oslo"), "{screen}");
        assert!(lines[first + 2].contains("prints at the shop"), "{screen}");
        assert!(!screen.contains('…'), "nothing was left out: {screen}");
        assert!(!screen.contains("i bio"), "so `i` is not offered: {screen}");
        // And the row under it is still a blank one: the card kept its air.
        assert!(
            lines[first + 3].replace('│', "").trim().is_empty(),
            "{screen}"
        );
    }

    /// A bio of more rows than the card gives it ends in an ellipsis, the
    /// bottom edge names `i`, and the box `i` opens has all of it, blank
    /// lines included, scrolling where the terminal is too short for it.
    #[test]
    fn a_long_bio_ends_in_an_ellipsis_and_i_shows_the_rest() {
        let bio = (1..=9)
            .map(|n| format!("line number {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let profile = with_a_bio(&format!("{bio}\n\nthe last one"));
        let (shown, cut) = bio_on_the_card(profile.biography.as_deref().unwrap(), 70, BIO_ROWS);
        assert!(cut);
        assert_eq!(shown.len(), BIO_ROWS);
        assert_eq!(shown[BIO_ROWS - 1], "line number 6…");
        // The ellipsis takes a column of the line, never one past it.
        let (narrow, _) = bio_on_the_card("abcdefgh abcdefgh abcdefgh", 8, 2);
        assert_eq!(narrow, ["abcdefgh", "abcdefg…"]);

        let rows = card_rows(&profile);
        let media = Media::of(&profile);
        let terminal = card_rendered(&profile, &media, &at_the_card(), &rows, "");
        let screen = screen_of(&terminal);
        assert!(screen.contains("line number 1"), "{screen}");
        assert!(!screen.contains("the last one"), "{screen}");
        assert!(screen.contains('…'), "{screen}");
        assert!(screen.contains("i bio"), "{screen}");

        // The box, on a terminal with room for all of it.
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let mut furthest = usize::MAX;
        terminal
            .draw(|frame| furthest = about_modal(frame, &profile, 0, false))
            .unwrap();
        let screen = screen_of(&terminal);
        assert_eq!(furthest, 0, "{screen}");
        assert!(screen.contains("About @someone"), "{screen}");
        assert!(screen.contains("Some One"), "{screen}");
        assert!(screen.contains("line number 9"), "{screen}");
        assert!(screen.contains("the last one"), "{screen}");
        assert!(screen.contains("any key closes"), "{screen}");

        // And on one too short: it scrolls, to the last line and no further.
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| furthest = about_modal(frame, &profile, 0, false))
            .unwrap();
        let screen = screen_of(&terminal);
        assert!(furthest > 0, "{screen}");
        assert!(screen.contains("↑↓ scroll"), "{screen}");
        assert!(!screen.contains("the last one"), "{screen}");
        terminal
            .draw(|frame| {
                about_modal(frame, &profile, usize::MAX, false);
            })
            .unwrap();
        let screen = screen_of(&terminal);
        assert!(screen.contains("the last one"), "{screen}");
        assert!(!screen.contains("Some One"), "{screen}");
    }

    /// An account that says nothing about itself has nothing for `i`.
    #[test]
    fn i_has_something_to_show_only_where_the_account_said_something() {
        let mut profile = bare_profile();
        assert!(says_something(&profile), "it has a name");
        profile.full_name = None;
        assert!(!says_something(&profile));
        assert!(about_text(&profile, 40, false).is_empty());
        assert_eq!(nothing_said(&profile), "@someone says nothing about itself");
        profile.external_url = Some("https://example.com".to_string());
        assert!(says_something(&profile));
    }

    /// On a terminal too short for the card as it is drawn with room, the
    /// card gives up the bio's extra rows, then its blank rows, and keeps
    /// every row there is to open: all of them are on screen, each still
    /// where the cursor can be seen on it.
    #[test]
    fn a_short_terminal_gives_up_the_air_before_a_row() {
        let mut profile = with_a_bio("one\ntwo\nthree\nfour");
        profile.stories = Visibility::Shown(vec![story()]);
        let rows = card_rows(&profile);
        let media = Media::of(&profile);
        let draw = |height: u16, row: usize| {
            let mut state = at_the_card();
            state.row = row;
            let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw_card(
                        frame,
                        &profile,
                        &media,
                        &state,
                        &rows,
                        false,
                        &tui::me(),
                        "",
                        false,
                    );
                })
                .unwrap();
            screen_of(&terminal)
        };
        let every_row = |screen: &str| {
            for part in [
                "actions on this account",
                "1 story up",
                "followed by 33 you follow",
                "followers",
                "1,234",
                "Highlights",
            ] {
                assert!(screen.contains(part), "{part} is missing: {screen}");
            }
        };

        // With room: the whole bio, and air between the bands.
        let area = Rect::new(0, 0, 80, 29);
        assert_eq!(fitted(&profile, &rows, area, false), Fit::WHOLE);
        let screen = draw(30, 0);
        every_row(&screen);
        assert!(screen.contains("four"), "{screen}");

        // The card is 20 rows here: borders and padding 4, the name and the
        // four lines of bio 5, the rows 6, the blank rows 5. A row short,
        // the bio gives up one of its own, and the air stays; two short,
        // another, down to the second.
        assert_eq!(
            fitted(&profile, &rows, Rect::new(0, 0, 80, 20), false),
            Fit::WHOLE
        );
        let fit = fitted(&profile, &rows, Rect::new(0, 0, 80, 19), false);
        assert_eq!((fit.bio, fit.gaps, fit.padded), (3, true, true));
        let fit = fitted(&profile, &rows, Rect::new(0, 0, 80, 18), false);
        assert_eq!((fit.bio, fit.gaps, fit.padded), (2, true, true));

        // 14 rows, where the highlights used to fall off the bottom: the
        // blank rows go, and every row is there.
        let fit = fitted(&profile, &rows, Rect::new(0, 0, 80, 13), false);
        assert_eq!((fit.bio, fit.gaps), (2, false));
        let screen = draw(14, rows.len() - 1);
        every_row(&screen);
        assert!(screen.contains("two…"), "{screen}");
        assert!(screen.contains(">trip"), "the cursor shows: {screen}");

        // Shorter still: the padding, then the bio's second row.
        let fit = fitted(&profile, &rows, Rect::new(0, 0, 80, 10), false);
        assert_eq!((fit.bio, fit.gaps, fit.padded), (1, false, false));
        let screen = draw(11, rows.len() - 1);
        every_row(&screen);
        assert!(screen.contains("one…"), "{screen}");
    }

    /// The tile under the cursor is reverse video, and its label is not
    /// also dim; the other tiles' labels are.
    #[test]
    fn the_focused_tile_keeps_its_label_bright() {
        let profile = bare_profile();
        let mut terminal = Terminal::new(TestBackend::new(60, 2)).unwrap();
        terminal
            .draw(|frame| draw_counts(frame, frame.area(), &profile, true, 1, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = tui::row_text(&terminal, 0);
        // Every character of the row is one column wide, so a byte offset
        // into it is a column.
        let cell_of = |label: &str| {
            let at = row.find(label).unwrap_or_else(|| panic!("{row}")) as u16;
            &buffer[(at, 0)]
        };
        let focused = cell_of("followers");
        assert!(focused.modifier.contains(Modifier::REVERSED), "{focused:?}");
        assert!(!focused.modifier.contains(Modifier::DIM), "{focused:?}");
        let other = cell_of("posts");
        assert!(other.modifier.contains(Modifier::DIM), "{other:?}");
        assert!(!other.modifier.contains(Modifier::REVERSED), "{other:?}");
    }

    /// A card as `viewing` says, at this size, with its picture arrived or
    /// not, and the cursor on `row`.
    fn pictured_card(
        profile: &Profile,
        rows: &[CardRow],
        row: usize,
        arrived: bool,
        viewing: bool,
        height: u16,
    ) -> Terminal<TestBackend> {
        let mut media = Media::of(profile);
        media.pictured = true;
        if arrived {
            media.avatar = Some(preview::Pixels::plain(150, 150, (9, 9, 200)));
        }
        let mut state = at_the_card();
        state.row = row;
        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        terminal
            .draw(|frame| {
                card_frame(
                    frame,
                    profile,
                    &media,
                    &state,
                    rows,
                    false,
                    &tui::me(),
                    "",
                    true,
                    viewing,
                );
            })
            .unwrap();
        terminal
    }

    /// Where the card draws pictures, the account's is at the left of what
    /// the account says, which moves to its right; it is the card's first
    /// row, with the cursor on the label under it, and no row of text
    /// stands for it. The place is kept before the picture arrives.
    #[test]
    fn the_card_draws_the_picture_beside_what_the_account_says() {
        let mut profile = with_a_bio("climber\nbased in Oslo");
        profile.pfp_url = Some("https://x.cdninstagram.com/small.jpg".to_string());
        let rows = card_rows_of(&profile, true);
        assert_eq!(&rows[..2], [CardRow::Picture, CardRow::Actions]);
        assert!(!card_rows(&profile).contains(&CardRow::Picture));

        let terminal = pictured_card(&profile, &rows, 0, true, false, 30);
        let screen = screen_of(&terminal);
        let lines: Vec<&str> = screen.lines().collect();
        let named = lines
            .iter()
            .position(|line| line.contains("Some One"))
            .unwrap_or_else(|| panic!("{screen}"));
        // Sixteen columns of picture, eight rows of it, from the row the
        // name is on; the name and the bio start past it.
        assert_eq!(lines[named].matches('▀').count(), 16, "{screen}");
        assert_eq!(screen.matches('▀').count(), 16 * 8, "{screen}");
        let column = |text: &str| {
            let line = lines.iter().find(|line| line.contains(text)).unwrap();
            line.chars().position(|c| c == text.chars().next().unwrap())
        };
        assert_eq!(column("Some One"), column("climber"), "{screen}");
        assert!(
            lines[named].find('▀') < lines[named].find("Some One"),
            "{screen}"
        );
        assert!(lines[named + 8].contains("picture ›"), "{screen}");
        assert!(!screen.contains("profile picture"), "{screen}");
        assert!(screen.contains("actions on this account"), "{screen}");
        assert!(screen.contains("enter open · d save"), "{screen}");
        // The label under the picture carries the cursor, in reverse video.
        let buffer = terminal.backend().buffer();
        let at = lines[named + 8].find("picture").unwrap();
        let x = lines[named + 8][..at].chars().count() as u16;
        assert!(
            buffer[(x, named as u16 + 8)]
                .modifier
                .contains(Modifier::REVERSED),
            "{screen}"
        );

        // Not arrived yet: the place is kept, and nothing moves when it is.
        let waiting = screen_of(&pictured_card(&profile, &rows, 1, false, false, 30));
        assert_eq!(waiting.matches('▀').count(), 0, "{waiting}");
        assert_eq!(
            waiting.lines().position(|line| line.contains("Some One")),
            Some(named)
        );
        assert!(waiting.contains("picture") && !waiting.contains("picture ›"));
    }

    /// A terminal too short for the card with its picture draws the card
    /// without it, and the picture is a row of text again: still there to
    /// open. The card a list opens draws its picture the same way.
    #[test]
    fn a_short_terminal_gives_up_the_picture_and_keeps_its_row() {
        let mut profile = bare_profile();
        profile.pfp_url = Some("https://x.cdninstagram.com/small.jpg".to_string());
        let rows = card_rows_of(&profile, true);
        let fit = fitted(&profile, &rows, Rect::new(0, 0, 80, 22), true);
        assert!(fit.avatar, "22 rows hold it: {fit:?}");
        let fit = fitted(&profile, &rows, Rect::new(0, 0, 80, 21), true);
        assert!(!fit.avatar, "21 do not: {fit:?}");
        assert!(!fitted(&profile, &rows, Rect::new(0, 0, 80, 40), false).avatar);

        let short = screen_of(&pictured_card(&profile, &rows, 0, true, false, 20));
        assert_eq!(short.matches('▀').count(), 0, "{short}");
        assert!(short.contains("> profile picture") || short.contains("profile picture"));
        assert!(short.contains("actions on this account"), "{short}");

        let viewing = viewing_rows(&profile);
        let peeked = screen_of(&pictured_card(&profile, &viewing, 0, true, true, 30));
        assert_eq!(peeked.matches('▀').count(), 16 * 8, "{peeked}");
        assert!(peeked.contains("picture ›"), "{peeked}");
        assert!(!peeked.contains("profile picture"), "{peeked}");
        assert!(peeked.contains("enter open · d save"), "{peeked}");
    }

    /// The picture is fetched while the card has none, and no longer once
    /// it has arrived; a card that draws no pictures never asks.
    #[test]
    fn the_cards_picture_is_wanted_until_it_arrives() {
        let mut profile = bare_profile();
        profile.pfp_url = Some("https://x.cdninstagram.com/small.jpg".to_string());
        let gallery = preview::Gallery::new();
        let mut media = Media::of(&profile);
        assert_eq!(adopt_avatar(&profile, &mut media, &gallery), None);
        media.pictured = true;
        assert_eq!(
            adopt_avatar(&profile, &mut media, &gallery),
            profile.pfp_url.as_deref()
        );
        media.avatar = Some(preview::Pixels::plain(2, 2, (1, 1, 1)));
        assert_eq!(adopt_avatar(&profile, &mut media, &gallery), None);
    }
}
