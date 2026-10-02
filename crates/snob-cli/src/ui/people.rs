//! The interactive account list: arrow keys, Enter to open a profile, `/` to
//! filter.
//!
//! Like the media browsers, this is the default at a human terminal: the
//! rule is `BrowseArgs::browses` in `cli.rs`, which applies
//! `cli::browse_decision`, and a pipe, a redirect, `--format`, `-o` and
//! `--no-interactive` all print the listing. Open, it earns its keep with the
//! two things scrollback cannot do: it narrows a long list as you type, and
//! it shows the account under the cursor without anybody retyping a username
//! into a command.
//!
//! It can be entered with a guard somebody else already holds: the profile
//! card lends its own `Tui` through [`browse_in`], because two guards would
//! be two claims on one terminal; `browse` is the standalone door that makes
//! a guard and prints the receipts. Like the highlights browser it is one loop over
//! two levels — `snob scan -i` starts at a tray of the five crossings and
//! Enter walks into one — and a single list is the same loop with the tray
//! skipped.
//!
//! **Changing accounts walks the list again.** A list is what one account was
//! shown, so `a` picks another account and then asks whether to walk the same
//! list as it, saying about what that costs. Yes leaves the view for the
//! command to walk it as that account, under its own consent question and
//! budget, and draw the view again; no stays here as the account it was. The
//! lists the profile card opens change accounts from the card.
//!
//! **Enter opens the account's profile inside the view.** It reads the profile
//! the way `snob profile` does (`commands::profile::fetch`, deferring the
//! mutual walk), through the client the command already holds, so the read is
//! paced and charged like any other and a cooldown or a rate limit comes back
//! as an error to say, never as a request sent anyway. One Enter is one such
//! read: nothing is fetched ahead of it, and nothing is kept for the next
//! Enter on the same account. While it runs the list stays on screen under a
//! "reading" box and `q` or Ctrl+C cancels, as in the card. A failure (not
//! found, private, cooldown, rate limit) is a red note on the list's bottom
//! edge, and the list is where it was. The profile itself is the read-only
//! card of `ui::profile::peek`: Esc, `q`, Backspace or Left come back to the
//! list with its selection and its filter untouched, and `o` hands the
//! address to the system browser, which is `User::profile_url` the way the
//! printed table already puts it behind every username: encoded, never
//! filtered, so a name with an odd character in it opens the account it
//! names. That differs from the story browser's refusal to hand over CDN
//! addresses because a signed link to somebody's story is a credential in a
//! browser history, and a profile address is public and carries nothing.
//!
//! **The filter is the one text mode in the tool**, and it is why
//! `input::read` exists beside `input::next`: while a query is being typed a
//! `q` is a letter of somebody's name, so the browser binds keys itself here
//! rather than through `action_of`. A paste lands in the query — text typed
//! fast and text pasted mean the same thing in a search box — with anything
//! below space dropped, so a multi-line paste cannot smuggle an Enter. While
//! the query owns the keyboard the frame shows a real cursor at the end of
//! it, which is the terminal's own way of saying where typing goes.

use anyhow::Result;
use console::measure_text_width;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Position};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Paragraph, Row, TableState};
use snob_core::filters::Filter;
use snob_core::model::User;
use snob_core::sets;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::app::{App, Viewer};
use crate::commands::profile::{self as profile_read, MutualPolicy};
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui::accounts::{self, Browsed, Picked};
use crate::ui::browser::input::Action;
use crate::ui::browser::input::{self, Raw, TICK, action_of, page, watching_cancel_keys};
use crate::ui::profile::{self, Answer, answer, question_modal};
use crate::ui::tui::{self, Tui};

/// What the browser shows: one list, or a tray of them.
///
/// The lists are borrowed, not owned — the commands already hold them, and
/// the browser only reads them.
pub struct Shelf<'a> {
    /// "unfollowers of @someone" — what the heading says the rows are.
    pub title: String,
    pub sets: Vec<Set<'a>>,
    /// The account the lists were walked as.
    pub viewer: Viewer,
}

pub struct Set<'a> {
    /// What the tray calls this list. Unused when the shelf holds one set.
    pub label: String,
    pub people: &'a [User],
}

impl<'a> Shelf<'a> {
    /// A tray of labeled lists, in the order given.
    pub fn tray(title: String, sets: &'a [(&str, Vec<User>)], viewer: Viewer) -> Self {
        Self {
            title,
            sets: sets
                .iter()
                .map(|(label, people)| Set {
                    label: (*label).to_string(),
                    people,
                })
                .collect(),
            viewer,
        }
    }

    /// One list, no tray: the shape every list command hands over.
    pub fn flat(title: String, people: &'a [User], viewer: Viewer) -> Self {
        Self {
            title,
            sets: vec![Set {
                label: String::new(),
                people,
            }],
            viewer,
        }
    }
}

/// The five lists a scan's tray shows, crossings first because they are what
/// the command exists to answer, each cut by `filter` after the crossing, as
/// the counts are, so a tray row and its summary line never disagree.
pub fn scan_sets(
    followers: &[User],
    following: &[User],
    filter: &Filter,
) -> [(&'static str, Vec<User>); 5] {
    [
        (
            "unfollowers",
            filter.apply(sets::difference(following, followers)),
        ),
        ("fans", filter.apply(sets::difference(followers, following))),
        (
            "friends",
            filter.apply(sets::intersection(followers, following)),
        ),
        ("followers", filter.apply(followers.to_vec())),
        ("following", filter.apply(following.to_vec())),
    ]
}

/// What changing accounts from a list walks again, and where the picker
/// reads the accounts.
pub struct Rewalk<'a> {
    pub secrets: &'a SecretStore,
    pub paths: &'a AppPaths,
    /// What is walked again, as the question names it: "unfollowers of @x".
    pub what: String,
    /// About how many requests that walk takes.
    pub requests: u32,
}

impl Rewalk<'_> {
    fn question(&self, to: &Viewer) -> String {
        format!(
            "Walk {} as {}? (about {})",
            self.what,
            to.label(),
            report::requests(self.requests)
        )
    }
}

/// Where the browser is, and what the arrow keys therefore move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Tray,
    /// Inside one set, by its index into the shelf.
    Inside(usize),
}

/// Whether keys move the selection or edit the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Moving,
    Filtering,
}

/// Everything the loop reads to draw one frame, kept apart from the terminal
/// so [`draw`] stays a function a test can point at a buffer.
struct State {
    level: Level,
    mode: Mode,
    query: String,
    /// Indices into the current set's people that the query lets through.
    /// The whole list when the query is empty.
    shown: Vec<usize>,
    /// The shown rows, ready to draw: one entry per entry of `shown`, built
    /// by `refilter` and read on every frame. `printable` walks every
    /// character and allocates, and the width scan is another walk, so doing
    /// both for the whole list inside `draw` would be most of what a frame
    /// costs.
    prepared: Vec<Prepared>,
    /// The selected row of the current level's list.
    selected: usize,
    /// The tray row to come back to after walking out of a set.
    tray_selected: usize,
}

/// The refusal `-i` earns where no browser can be drawn, for every command
/// that browses, not only the lists.
///
/// Asked at the top of the command, before the session is opened and long
/// before a request is spent — the same order `-o` is checked in, and for the
/// same reason: a walk that succeeds and then cannot be shown is the
/// expensive order to find out in. The predicate is [`crate::ui::can_show_a_menu`],
/// because a browser needs what a menu needs: keys from standard input,
/// drawing on standard error. Standard output is deliberately not consulted —
/// `-i` was said, so nothing is being detected, and the listing it would
/// protect is exactly what `-i` asks to withhold.
pub fn check_drawable() -> Result<()> {
    if crate::ui::can_show_a_menu() {
        return Ok(());
    }
    Err(
        ExitError::new(ExitCode::Error, "--interactive needs a terminal to draw on")
            .with_hint("--no-interactive prints the result instead")
            .into(),
    )
}

/// Drives the list until the user leaves it, or agrees to walk it as another
/// account, with a terminal of its own. `note` is said on the first frame.
/// `app` is the account the list was walked as, and what Enter reads a
/// profile with.
pub async fn browse(
    app: &mut App,
    shelf: &Shelf<'_>,
    rewalk: &Rewalk<'_>,
    note: String,
) -> Result<Browsed> {
    let mut tui =
        tui::claim_fullscreen("--no-interactive prints the listing; --format and -o shape it")?;

    let mut receipts: Vec<String> = Vec::new();
    let outcome = run(&mut tui, app, shelf, Some(rewalk), &mut receipts, note).await;
    drop(tui);
    tui::print_receipts(&receipts);
    outcome.map(|(code, switch_to)| Browsed::of(code, switch_to))
}

/// The same list on a guard somebody else holds.
///
/// The profile card enters here: it already owns the terminal, and two `Tui`s
/// would be two claims on it. Receipts belong to the guard's owner too — they
/// are printed after *that* guard drops, not here.
pub(crate) async fn browse_in(
    tui: &mut Tui,
    app: &mut App,
    shelf: &Shelf<'_>,
    receipts: &mut Vec<String>,
) -> Result<ExitCode> {
    Ok(run(tui, app, shelf, None, receipts, String::new()).await?.0)
}

/// The loop, and the account the user agreed to walk the list as, if any.
/// Without `rewalk` the list cannot change accounts.
async fn run(
    tui: &mut Tui,
    app: &mut App,
    shelf: &Shelf<'_>,
    rewalk: Option<&Rewalk<'_>>,
    receipts: &mut Vec<String>,
    mut note: String,
) -> Result<(ExitCode, Option<Viewer>)> {
    let colors = tui::colors_enabled();
    let flat = shelf.sets.len() == 1;
    let mut state = State {
        level: if flat { Level::Inside(0) } else { Level::Tray },
        mode: Mode::Moving,
        query: String::new(),
        shown: Vec::new(),
        prepared: Vec::new(),
        selected: 0,
        tray_selected: 0,
    };
    if flat {
        // An empty query lets everything through, so this is "show the whole
        // set" — and it is the one place the prepared rows are built from.
        refilter(shelf, &mut state);
    }

    let mut list = TableState::default();
    let mut page_rows = 1usize;
    let mut redraw = true;
    // The account picked, while the question about walking as it is open.
    let mut asking: Option<Viewer> = None;

    loop {
        if redraw {
            let rows_here = rows_in(shelf, &state);
            state.selected = state.selected.min(rows_here.saturating_sub(1));
            list.select(Some(state.selected));
            tui.terminal.draw(|frame| {
                draw(
                    frame,
                    shelf,
                    &state,
                    &note,
                    rewalk.is_some(),
                    colors,
                    &mut list,
                    &mut page_rows,
                );
                if let (Some(to), Some(rewalk)) = (&asking, rewalk) {
                    question_modal(frame, &rewalk.question(to), colors);
                }
            })?;
        }

        let event = input::read(TICK).map_err(input::unreadable)?;
        // A note stays up until the user does something else, not until the
        // next timer tick wipes it.
        if !matches!(event, Raw::Tick | Raw::Resized) {
            note.clear();
        }
        // Nothing here animates, so a timer tick with nothing new to say is
        // the idle case, and it draws nothing.
        redraw = !matches!(event, Raw::Tick);

        if let Some(to) = asking.take() {
            match asked(to, &shelf.viewer, event) {
                Asked::Wait(to) => asking = Some(to),
                Asked::Walk(to) => return Ok((ExitCode::Ok, Some(to))),
                Asked::Declined(text) => note = text,
                Asked::Leave(code) => return Ok((code, None)),
            }
            continue;
        }

        let level_before = state.level;
        let step = match state.mode {
            Mode::Moving => moving(shelf, &mut state, flat, page_rows, event),
            Mode::Filtering => filtering(shelf, &mut state, event),
        };
        // The scroll offset belongs to the level it was scrolled at; the
        // first draw of the next level pulls its remembered selection back
        // into view.
        if state.level != level_before {
            list = TableState::default();
        }
        match step {
            Step::Go => {}
            Step::Redraw => tui.terminal.clear()?,
            Step::Leave(code) => return Ok((code, None)),
            Step::Profile(person) => {
                // Said before the read, which may wait on the pacer for a
                // while: the list stays under a box that names what is going
                // on and how to stop it.
                let reading = format!("Reading @{}...", person.safe_username());
                tui.terminal.draw(|frame| {
                    draw(
                        frame,
                        shelf,
                        &state,
                        &note,
                        rewalk.is_some(),
                        colors,
                        &mut list,
                        &mut page_rows,
                    );
                    reading_modal(frame, &reading, colors);
                })?;
                let (read, stopped) = watching_cancel_keys(
                    app.client().pacer().cancel_token(),
                    read_profile(app, &person),
                )
                .await;
                if let Some(code) = stopped.leave() {
                    return Ok((code, None));
                }
                match read {
                    Ok(profile) => {
                        let viewer = app.viewer().clone();
                        if let Some(code) = profile::peek(tui, &profile, &viewer, receipts)? {
                            return Ok((code, None));
                        }
                        // Back on the list as it was left; the card's frame
                        // is what the terminal believes is on screen.
                        tui.terminal.clear()?;
                    }
                    Err(e) => note = could_not_read(&person, &e),
                }
            }
            Step::Accounts => {
                let Some(rewalk) = rewalk else {
                    note = "Accounts change from the profile card".to_string();
                    continue;
                };
                let under = |frame: &mut Frame<'_>| {
                    draw(
                        frame,
                        shelf,
                        &state,
                        &note,
                        true,
                        colors,
                        &mut list,
                        &mut page_rows,
                    );
                };
                let picked =
                    accounts::pick(tui, rewalk.secrets, rewalk.paths, &shelf.viewer, under).await?;
                match picked {
                    Picked::Stay => {}
                    Picked::Switch(to) => asking = Some(to),
                    Picked::Leave(code) => return Ok((code, None)),
                }
            }
        }
    }
}

/// What one key did to the loop.
enum Step {
    Go,
    /// Enter on an account: read its profile and show it.
    Profile(User),
    Redraw,
    Leave(ExitCode),
    /// The account picker, over the list.
    Accounts,
}

/// What a key did to the question about walking the list as `to`.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    /// Still asking.
    Wait(Viewer),
    /// Yes: leave the view to walk the list as this account.
    Walk(Viewer),
    /// No: the list stays as the account it was, with this note.
    Declined(String),
    Leave(ExitCode),
}

/// A key while the question is open. Like the profile card's, it owns the
/// keyboard: y walks, n or Esc declines, Ctrl+C still interrupts, and nothing
/// else does anything.
fn asked(to: Viewer, from: &Viewer, event: Raw) -> Asked {
    let Raw::Key(key) = event else {
        return Asked::Wait(to);
    };
    match answer(key) {
        Answer::Yes => Asked::Walk(to),
        Answer::No => Asked::Declined(format!("Nothing walked; still as {}", from.label())),
        Answer::Interrupt => Asked::Leave(ExitCode::Interrupted),
        Answer::Ignored => Asked::Wait(to),
    }
}

/// How many rows the current level has to move over.
fn rows_in(shelf: &Shelf<'_>, state: &State) -> usize {
    match state.level {
        Level::Tray => shelf.sets.len(),
        Level::Inside(_) => state.shown.len(),
    }
}

/// Recomputes which rows the query lets through, from the top.
///
/// The selection goes back to the first match rather than trying to follow a
/// row that may no longer be shown: while somebody is typing, the first match
/// is the row they are steering toward.
fn refilter(shelf: &Shelf<'_>, state: &mut State) {
    let Level::Inside(index) = state.level else {
        return;
    };
    let needle = state.query.to_lowercase();
    state.shown = shelf.sets[index]
        .people
        .iter()
        .enumerate()
        .filter(|(_, person)| {
            needle.is_empty()
                || person.username.to_lowercase().contains(&needle)
                || person
                    .full_name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
        })
        .map(|(i, _)| i)
        .collect();
    state.prepared = state
        .shown
        .iter()
        .map(|&i| prepare(&shelf.sets[index].people[i]))
        .collect();
    state.selected = 0;
}

/// One shown account, ready to draw. See `State::prepared` for why this is
/// done here and not in `draw`.
struct Prepared {
    /// `@username`, already through `printable`.
    username: String,
    /// The full name through `printable`; empty when there is none.
    full_name: String,
    /// Display columns of `username`, the `@` included.
    user_width: usize,
    name_width: usize,
    badges: Option<&'static str>,
}

fn prepare(person: &User) -> Prepared {
    let username = format!("@{}", person.safe_username());
    let full_name = person.safe_full_name().unwrap_or_default();
    Prepared {
        user_width: measure_text_width(&username),
        name_width: measure_text_width(&full_name),
        badges: attributes_of(person),
        username,
        full_name,
    }
}

/// One key while the arrows own the list.
fn moving(shelf: &Shelf<'_>, state: &mut State, flat: bool, page_rows: usize, event: Raw) -> Step {
    let key = match event {
        Raw::Tick | Raw::Resized | Raw::Paste(_) => return Step::Go,
        Raw::Key(key) => key,
    };

    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

    // The two keys `action_of` cannot answer for this browser: `/` starts a
    // query, and Esc clears one before it means leave.
    if let Level::Inside(_) = state.level {
        if key.code == KeyCode::Char('/') && plain {
            state.mode = Mode::Filtering;
            state.query.clear();
            refilter(shelf, state);
            return Step::Go;
        }
        if key.code == KeyCode::Esc && !state.query.is_empty() {
            state.query.clear();
            refilter(shelf, state);
            return Step::Go;
        }
    }

    let rows = rows_in(shelf, state);
    match action_of(key) {
        Action::Up => state.selected = state.selected.saturating_sub(1),
        Action::Down => state.selected = (state.selected + 1).min(rows.saturating_sub(1)),
        Action::PageUp => state.selected = state.selected.saturating_sub(page(page_rows)),
        Action::PageDown => {
            state.selected = (state.selected + page(page_rows)).min(rows.saturating_sub(1));
        }
        Action::First => state.selected = 0,
        Action::Last => state.selected = rows.saturating_sub(1),
        Action::Open => return open(shelf, state),
        Action::Back => {
            if matches!(state.level, Level::Inside(_)) && !flat {
                state.level = Level::Tray;
                state.selected = state.tray_selected;
                state.query.clear();
                state.shown = Vec::new();
            }
        }
        Action::Redraw => return Step::Redraw,
        Action::Quit => return Step::Leave(ExitCode::Ok),
        Action::Interrupt => return Step::Leave(ExitCode::Interrupted),
        Action::Accounts => return Step::Accounts,
        Action::Download | Action::None => {}
    }
    Step::Go
}

/// Enter, wherever the selection is.
fn open(shelf: &Shelf<'_>, state: &mut State) -> Step {
    match state.level {
        Level::Tray => {
            state.tray_selected = state.selected;
            state.level = Level::Inside(state.selected);
            state.query.clear();
            refilter(shelf, state);
            state.selected = 0;
            Step::Go
        }
        Level::Inside(index) => {
            let Some(&person) = state.shown.get(state.selected) else {
                return Step::Go;
            };
            Step::Profile(shelf.sets[index].people[person].clone())
        }
    }
}

/// The profile read behind an Enter: `snob profile`'s own, with the mutual
/// walk left for a click that this view does not have. The id comes from the
/// list, so the name is not resolved again.
pub async fn read_profile(app: &App, person: &User) -> Result<profile_read::Profile> {
    profile_read::fetch(
        app.client(),
        &person.username,
        app.viewer().pk,
        MutualPolicy::Defer,
        Some(person.pk),
    )
    .await
}

/// The note a failed read leaves on the list. The first line of the error
/// only: the footer is one row, and the reason is on it.
fn could_not_read(person: &User, error: &anyhow::Error) -> String {
    let why = error.to_string();
    let why = why.lines().next().unwrap_or_default();
    format!("Could not open @{}: {why}", person.safe_username())
}

/// The box over the list while a profile is being read.
fn reading_modal(frame: &mut Frame<'_>, text: &str, colors: bool) {
    let dim = tui::paint(colors, tui::DIM);
    tui::veil(frame, colors);
    let width = (measure_text_width(text) as u16 + 8)
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

/// One key while the query owns the keyboard.
fn filtering(shelf: &Shelf<'_>, state: &mut State, event: Raw) -> Step {
    match event {
        Raw::Tick | Raw::Resized => Step::Go,
        // Pasting into a search box is typing fast. Anything below space is
        // dropped so a multi-line paste cannot carry an Enter in it.
        Raw::Paste(text) => {
            state.query.extend(text.chars().filter(|c| !c.is_control()));
            refilter(shelf, state);
            Step::Go
        }
        Raw::Key(key) => filtering_key(shelf, state, key),
    }
}

fn filtering_key(shelf: &Shelf<'_>, state: &mut State, key: KeyEvent) -> Step {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('c') {
        return Step::Leave(ExitCode::Interrupted);
    }
    if ctrl && key.code == KeyCode::Char('l') {
        return Step::Redraw;
    }
    let rows = rows_in(shelf, state);
    match key.code {
        // Esc gives the whole list back; Enter keeps what the query narrowed
        // it to. Both put the arrows back in charge.
        KeyCode::Esc => {
            state.mode = Mode::Moving;
            state.query.clear();
            refilter(shelf, state);
        }
        KeyCode::Enter => state.mode = Mode::Moving,
        KeyCode::Backspace => {
            if state.query.pop().is_some() {
                refilter(shelf, state);
            } else {
                state.mode = Mode::Moving;
            }
        }
        // The arrows keep working mid-query, so narrowing and picking are one
        // motion rather than a mode change apart.
        KeyCode::Up => state.selected = state.selected.saturating_sub(1),
        KeyCode::Down => state.selected = (state.selected + 1).min(rows.saturating_sub(1)),
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            state.query.push(c);
            refilter(shelf, state);
        }
        _ => {}
    }
    Step::Go
}

/// Draws one frame: the shared chrome, whichever level's table, and — while
/// the query owns the keyboard — a real cursor at the end of it.
#[expect(clippy::too_many_arguments, reason = "one frame's worth of state")]
fn draw(
    frame: &mut Frame<'_>,
    shelf: &Shelf<'_>,
    state: &State,
    note: &str,
    switches: bool,
    colors: bool,
    list: &mut TableState,
    page_rows: &mut usize,
) {
    let area = frame.area();
    let total = rows_in(shelf, state);

    let mut block = tui::view_block(title_of(shelf, state), colors, tui::list_padding(area));
    let inner = block.inner(area);
    let viewport = (inner.height as usize).max(1);
    *page_rows = viewport;
    let fits = total <= viewport;
    block = tui::top_right(
        block,
        (!fits).then(|| tui::position_line(state.selected, total, colors)),
        tui::viewer_line(&shelf.viewer, colors),
    );
    block = block.title_bottom(footer_line(shelf, state, note, switches, colors));
    frame.render_widget(block, area);

    *list.offset_mut() = tui::scrolled_offset(list.offset(), state.selected, total, viewport, 2);
    match state.level {
        Level::Tray => {
            let label_w = shelf
                .sets
                .iter()
                .map(|set| measure_text_width(&set.label))
                .max()
                .unwrap_or(1) as u16;
            frame.render_stateful_widget(
                tui::list_table(
                    shelf.sets.iter().map(|set| {
                        // A tray of five is a menu, not an enumeration: a
                        // blank row between entries gives each one a hand.
                        Row::new(vec![
                            Cell::from(set.label.clone()),
                            Cell::from(Line::from(set.people.len().to_string()).right_aligned()),
                        ])
                        .bottom_margin(1)
                    }),
                    [Constraint::Length(label_w), Constraint::Length(9)],
                ),
                inner,
                list,
            );
        }
        Level::Inside(_) if state.shown.is_empty() => {
            let text = if state.query.is_empty() {
                "(nobody)"
            } else {
                "(nobody matches)"
            };
            frame.render_widget(
                Paragraph::new(tui::styled_line(text, tui::DIM, colors)),
                inner,
            );
        }
        Level::Inside(_) => {
            frame.render_stateful_widget(
                tui::list_table(
                    state
                        .prepared
                        .iter()
                        .enumerate()
                        .map(|(row, person)| person_row(person, row == state.selected, colors)),
                    columns(&state.prepared, inner.width),
                ),
                inner,
                list,
            );
        }
    }
    if !fits {
        tui::scrollbar(frame, area, total, state.selected, viewport as u16);
    }

    // The cursor is the terminal's own way of saying where typing goes, and
    // it is shown only while there is somewhere for typing to go. Drawn last,
    // over the bottom border, at the end of the query.
    if state.mode == Mode::Filtering {
        let x = area.x
            + 1
            + measure_text_width(FILTER_PREFIX) as u16
            + measure_text_width(&state.query) as u16;
        let y = area.bottom().saturating_sub(1);
        if x < area.right().saturating_sub(1) {
            frame.set_cursor_position(Position { x, y });
        }
    }
}

/// What the top border calls the current level.
fn title_of(shelf: &Shelf<'_>, state: &State) -> String {
    match state.level {
        Level::Tray => shelf.title.clone(),
        Level::Inside(index) => {
            let set = &shelf.sets[index];
            let title = if shelf.sets.len() == 1 {
                shelf.title.clone()
            } else {
                format!("{} — {}", shelf.title, set.label)
            };
            if state.query.is_empty() {
                format!("{title} — {} accounts", set.people.len())
            } else {
                format!(
                    "{title} — {} of {} match \"{}\"",
                    state.shown.len(),
                    set.people.len(),
                    state.query
                )
            }
        }
    }
}

/// What the filter footer starts with; the cursor position is computed
/// against it — in display columns, like everything else — so the two
/// cannot drift apart.
const FILTER_PREFIX: &str = " / ";

/// The bottom border: the note when there is one, the query while it is being
/// typed, the hints otherwise. `switches` says whether `a` changes accounts.
fn footer_line(
    shelf: &Shelf<'_>,
    state: &State,
    note: &str,
    switches: bool,
    colors: bool,
) -> Line<'static> {
    if !note.is_empty() {
        return tui::outcome_line(note.to_string(), colors);
    }
    if state.mode == Mode::Filtering {
        let hint = "  enter keep · esc clear ";
        return Line::from(vec![
            Span::raw(FILTER_PREFIX),
            Span::raw(state.query.clone()),
            Span::styled(hint, tui::paint(colors, tui::DIM)),
        ]);
    }

    let mut hint = String::from(match state.level {
        Level::Tray => "↑↓ move · enter open",
        Level::Inside(_) if shelf.sets.len() > 1 => {
            "↑↓ move · enter open profile · / filter · ← back"
        }
        Level::Inside(_) => "↑↓ move · enter open profile · / filter",
    });
    if !state.query.is_empty() {
        hint.push_str(" · esc clear filter");
    }
    if switches {
        hint.push_str(" · a account");
    }
    hint.push_str(" · q quit");
    tui::hint_line(hint, colors)
}

/// The account list's columns, measured in display columns from what is
/// shown. Only `Length`: under `Flex::Start` the spare width stays unused on
/// the right, which is exactly what is wanted — a rail on the left, not
/// three facts scattered to the ends of a two-hundred-column terminal.
///
/// Narrow terminals drop whole columns rather than clip them: the badge
/// falls away first, then the full name, and the username column is the one
/// that never goes.
fn columns(prepared: &[Prepared], width: u16) -> Vec<Constraint> {
    let user_w = prepared
        .iter()
        .map(|p| p.user_width)
        .max()
        .unwrap_or(1)
        .clamp(12, 26) as u16;
    let name_w = prepared
        .iter()
        .map(|p| p.name_width)
        .max()
        .unwrap_or(0)
        .min(32) as u16;
    // The widest badge is "verified, private". Left-aligned: badges are
    // words, not numbers.
    let badge_w = if prepared.iter().any(|p| p.badges.is_some()) {
        17u16
    } else {
        0
    };

    let gutter = 2 + 2; // "> " and the spacing after the username column
    let mut out = vec![Constraint::Length(user_w)];
    if badge_w > 0 && width >= gutter + user_w + 2 + name_w + 2 + badge_w {
        out.push(Constraint::Length(name_w.max(1)));
        out.push(Constraint::Length(badge_w));
    } else if width >= gutter + user_w + 2 + 12 {
        out.push(Constraint::Length(width - gutter - user_w - 2));
    }
    out
}

/// One account as a table row: the username, the full name, and the badges
/// dim at the end — dim except on the selection, where the reverse video is
/// the emphasis and a dim run inside it would mute it.
fn person_row(person: &Prepared, selected: bool, colors: bool) -> Row<'static> {
    let mut cells = vec![
        Cell::from(person.username.clone()),
        Cell::from(person.full_name.clone()),
    ];
    if let Some(attributes) = person.badges {
        cells.push(if colors && !selected {
            Cell::from(attributes).style(tui::DIM)
        } else {
            Cell::from(attributes)
        });
    }
    Row::new(cells)
}

/// The printed table's words for verified and private
/// (`output::table::attributes`), so the browser and the table never
/// describe those two ways. The table's `no-pfp` is not a badge here.
fn attributes_of(person: &User) -> Option<&'static str> {
    match (
        person.is_verified.unwrap_or(false),
        person.is_private.unwrap_or(false),
    ) {
        (true, true) => Some("verified, private"),
        (true, false) => Some("verified"),
        (false, true) => Some("private"),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Pk;

    use super::*;

    /// The tray's five lists, in order, each crossed the right way round and
    /// cut by the filter after the crossing.
    #[test]
    fn a_scan_tray_holds_the_five_lists_crossed_the_right_way_round() {
        use snob_core::filters::Attribute;
        let famous = |pk, name| User {
            is_verified: Some(true),
            ..person(pk, name, None)
        };
        let followers = vec![
            person(1, "friend", None),
            famous(2, "famous_friend"),
            person(5, "fan", None),
            famous(6, "famous_fan"),
        ];
        let following = vec![
            person(1, "friend", None),
            famous(2, "famous_friend"),
            person(3, "snob", None),
            famous(4, "famous_snob"),
        ];
        let pks = |people: &[User]| people.iter().map(|u| u.pk.get()).collect::<Vec<_>>();

        let all = scan_sets(&followers, &following, &Filter::default());
        let labels: Vec<&str> = all.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            ["unfollowers", "fans", "friends", "followers", "following"]
        );
        assert_eq!(
            pks(&all[0].1),
            [3, 4],
            "unfollowers: followed, not following back"
        );
        assert_eq!(pks(&all[1].1), [5, 6], "fans: following you, not followed");
        assert_eq!(pks(&all[2].1), [1, 2]);
        assert_eq!(pks(&all[3].1), [1, 2, 5, 6]);
        assert_eq!(pks(&all[4].1), [1, 2, 3, 4]);

        let hide_verified = Filter {
            hide: vec![Attribute::Verified],
            ..Default::default()
        };
        let cut = scan_sets(&followers, &following, &hide_verified);
        let sizes: Vec<usize> = cut.iter().map(|(_, people)| people.len()).collect();
        assert_eq!(sizes, [1, 1, 1, 2, 2]);
        assert_eq!(pks(&cut[0].1), [3]);
    }

    fn person(pk: u64, username: &str, full_name: Option<&str>) -> User {
        User {
            pk: Pk::new(pk),
            username: username.to_string(),
            full_name: full_name.map(str::to_string),
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    fn shelf_of(people: &[User]) -> Shelf<'_> {
        Shelf::flat("unfollowers of @someone".to_string(), people, tui::me())
    }

    fn inside(shelf: &Shelf<'_>) -> State {
        let mut state = State {
            level: Level::Inside(0),
            mode: Mode::Moving,
            query: String::new(),
            shown: Vec::new(),
            prepared: Vec::new(),
            selected: 0,
            tray_selected: 0,
        };
        refilter(shelf, &mut state);
        state
    }

    fn rendered(shelf: &Shelf<'_>, state: &State) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        let mut list = TableState::default();
        list.select(Some(state.selected));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    shelf,
                    state,
                    "",
                    true,
                    false,
                    &mut list,
                    &mut page_rows,
                )
            })
            .unwrap();
        terminal
    }

    /// The filter reads the username and the full name, ignores case, and an
    /// emptied query gives the whole list back.
    #[test]
    fn the_query_narrows_by_name_and_by_full_name() {
        let people = [
            person(1, "anna", Some("Anna Banana")),
            person(2, "bob", None),
            person(3, "carol", Some("Anna's Friend")),
        ];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        assert_eq!(state.shown, vec![0, 1, 2]);

        state.query = "ANNA".to_string();
        refilter(&shelf, &mut state);
        assert_eq!(state.shown, vec![0, 2], "case must not matter");

        state.query.clear();
        refilter(&shelf, &mut state);
        assert_eq!(state.shown, vec![0, 1, 2]);
    }

    /// Narrowing the list moves the selection to the first match: while
    /// somebody is typing, that is the row they are steering toward.
    #[test]
    fn narrowing_resets_the_selection() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.selected = 1;

        state.query = "b".to_string();
        refilter(&shelf, &mut state);
        assert_eq!(state.selected, 0);
        assert_eq!(state.shown, vec![1]);
    }

    /// While a query is being typed, `q` is a letter of somebody's name.
    #[test]
    fn typing_a_q_into_the_filter_does_not_quit() {
        let people = [person(1, "quentin", None), person(2, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        let step = filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert!(matches!(step, Step::Go));
        assert_eq!(state.query, "q");
        assert_eq!(state.shown, vec![0]);
        assert_eq!(state.mode, Mode::Filtering);
    }

    /// While a query is being typed, `a` is a letter too, not the account
    /// picker.
    #[test]
    fn typing_an_a_into_the_filter_does_not_open_the_accounts() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        let step = filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(matches!(step, Step::Go));
        assert_eq!(state.query, "a");
        assert_eq!(state.shown, vec![0]);
        assert_eq!(state.mode, Mode::Filtering);
    }

    /// Out of the filter, `a` opens the account picker.
    #[test]
    fn a_opens_the_accounts_while_moving() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        let step = moving(
            &shelf,
            &mut state,
            true,
            10,
            Raw::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        );
        assert!(matches!(step, Step::Accounts));
    }

    fn work() -> Viewer {
        Viewer {
            pk: Pk::new(8),
            username: Some("work".to_string()),
        }
    }

    fn key(code: KeyCode) -> Raw {
        Raw::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// The question names the list, the account and about what it costs.
    #[test]
    fn the_question_names_the_list_the_account_and_the_cost() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let secrets = SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-rewalk-{}", std::process::id()));
        let rewalk = Rewalk {
            secrets: &secrets,
            paths: &paths,
            what: "unfollowers of @someone".to_string(),
            requests: 31,
        };
        assert_eq!(
            rewalk.question(&work()),
            "Walk unfollowers of @someone as @work? (about 31 requests)"
        );
    }

    /// No goes back to the list as the account it was, with a note that
    /// says so; yes leaves to walk it as the account picked; any other key
    /// keeps asking, and Ctrl+C still interrupts.
    #[test]
    fn the_answer_walks_as_the_account_picked_or_stays_as_the_one_it_was() {
        let me = tui::me();
        assert_eq!(
            asked(work(), &me, key(KeyCode::Char('n'))),
            Asked::Declined("Nothing walked; still as @me".to_string())
        );
        assert_eq!(
            asked(work(), &me, key(KeyCode::Esc)),
            Asked::Declined("Nothing walked; still as @me".to_string())
        );
        assert_eq!(
            asked(work(), &me, key(KeyCode::Char('y'))),
            Asked::Walk(work())
        );
        for other in [KeyCode::Char('q'), KeyCode::Char('a'), KeyCode::Enter] {
            assert_eq!(asked(work(), &me, key(other)), Asked::Wait(work()));
        }
        assert_eq!(asked(work(), &me, Raw::Tick), Asked::Wait(work()));
        assert_eq!(
            asked(
                work(),
                &me,
                Raw::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            ),
            Asked::Leave(ExitCode::Interrupted)
        );
    }

    /// Esc empties the query and hands the keys back; Enter keeps the
    /// narrowed list. Both put the arrows back in charge.
    #[test]
    fn esc_clears_and_enter_keeps() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;
        state.query = "b".to_string();
        refilter(&shelf, &mut state);

        filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(state.mode, Mode::Moving);
        assert_eq!(state.shown, vec![1], "Enter keeps what was narrowed");

        state.mode = Mode::Filtering;
        filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(state.mode, Mode::Moving);
        assert_eq!(state.shown, vec![0, 1], "Esc gives the whole list back");
    }

    /// A paste is typing fast — it lands in the query, minus anything below
    /// space, so a multi-line paste cannot carry an Enter into the list.
    #[test]
    fn a_paste_lands_in_the_query_without_its_control_characters() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        filtering(&shelf, &mut state, Raw::Paste("an\r\nna".to_string()));
        assert_eq!(state.query, "anna");
    }

    /// Ctrl+C stays an interrupt even while the query owns the keyboard.
    #[test]
    fn ctrl_c_still_interrupts_mid_query() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;

        let step = filtering_key(
            &shelf,
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(matches!(step, Step::Leave(ExitCode::Interrupted)));
    }

    /// In a shelf with a tray, Enter walks in and Left walks back out, with
    /// the tray row remembered; on a flat shelf Back does nothing.
    #[test]
    fn the_tray_opens_and_closes_like_a_folder() {
        let a = [person(1, "anna", None)];
        let b = [person(2, "bob", None), person(3, "carol", None)];
        let shelf = Shelf {
            title: "scan of @someone".to_string(),
            sets: vec![
                Set {
                    label: "unfollowers".to_string(),
                    people: &a,
                },
                Set {
                    label: "fans".to_string(),
                    people: &b,
                },
            ],
            viewer: tui::me(),
        };
        let mut state = State {
            level: Level::Tray,
            mode: Mode::Moving,
            query: String::new(),
            shown: Vec::new(),
            prepared: Vec::new(),
            selected: 1,
            tray_selected: 0,
        };

        open(&shelf, &mut state);
        assert_eq!(state.level, Level::Inside(1));
        assert_eq!(state.shown, vec![0, 1], "the set is shown unfiltered");
        assert_eq!(state.selected, 0);

        moving(
            &shelf,
            &mut state,
            false,
            10,
            Raw::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        );
        assert_eq!(state.level, Level::Tray);
        assert_eq!(state.selected, 1, "the tray remembers where it was");
    }

    /// The heading carries the count, and the query rewrites it into a
    /// fraction so the narrowing is legible without reading the rows.
    #[test]
    fn the_heading_counts_and_the_query_rewrites_it() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);

        let terminal = rendered(&shelf, &state);
        assert!(
            tui::row_text(&terminal, 0).contains("unfollowers of @someone — 2 accounts"),
            "{}",
            tui::row_text(&terminal, 0)
        );
        assert!(
            tui::row_text(&terminal, 0).ends_with(" as @me ╮"),
            "{}",
            tui::row_text(&terminal, 0)
        );

        state.query = "b".to_string();
        refilter(&shelf, &mut state);
        let terminal = rendered(&shelf, &state);
        assert!(
            tui::row_text(&terminal, 0).contains("1 of 2 match \"b\""),
            "{}",
            tui::row_text(&terminal, 0)
        );
    }

    /// While the query owns the keyboard the bottom border carries it, and
    /// the cursor sits at its end — a real cursor, not a drawn one.
    #[test]
    fn the_filter_footer_carries_the_query_and_a_real_cursor() {
        let people = [person(1, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.mode = Mode::Filtering;
        state.query = "bo".to_string();
        refilter(&shelf, &mut state);

        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        let mut list = TableState::default();
        list.select(Some(0));
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &shelf,
                    &state,
                    "",
                    true,
                    false,
                    &mut list,
                    &mut page_rows,
                )
            })
            .unwrap();
        assert!(tui::row_text(&terminal, 9).contains("/ bo"));
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position { x: 6, y: 9 },
            "one past the query: border, prefix, two letters"
        );
    }

    /// The badge words are the printed table's for verified and private.
    #[test]
    fn the_badges_speak_the_tables_words() {
        let mut flagged = person(1, "anna", None);
        flagged.is_verified = Some(true);
        flagged.is_private = Some(true);
        assert_eq!(attributes_of(&flagged), Some("verified, private"));
        assert_eq!(attributes_of(&person(2, "bob", None)), None);
    }

    /// Column widths count display columns, not characters: a double-width
    /// name does not push the next column out of line, and the badge starts
    /// at the same x on every row.
    #[test]
    fn the_columns_line_up_even_with_double_width_names() {
        let mut wide = person(1, "大大大", None);
        wide.is_private = Some(true);
        let mut narrow = person(2, "ab", None);
        narrow.is_private = Some(true);
        let people = [wide, narrow];
        let shelf = shelf_of(&people);
        let state = inside(&shelf);
        let terminal = rendered(&shelf, &state);
        let buffer = terminal.backend().buffer();
        let column_of = |y: u16| {
            (0..buffer.area.width).find(|&x| {
                let mut text = String::new();
                for dx in 0..7u16 {
                    if x + dx < buffer.area.width {
                        text.push_str(buffer[(x + dx, y)].symbol());
                    }
                }
                text.starts_with("private")
            })
        };
        // Rows sit under border, padding and no header; both carry a badge.
        let ys: Vec<u16> = (1..9).filter(|&y| column_of(y).is_some()).collect();
        assert_eq!(ys.len(), 2, "two badge rows");
        assert_eq!(
            column_of(ys[0]),
            column_of(ys[1]),
            "the badge column moved between rows"
        );
    }

    /// Narrow terminals drop whole columns rather than clip them: the badge
    /// goes first, the username never goes.
    #[test]
    fn narrow_terminals_drop_whole_columns() {
        let mut flagged = person(1, "somebody", Some("Some Body"));
        flagged.is_private = Some(true);
        let prepared = vec![prepare(&flagged)];
        let wide = columns(&prepared, 80);
        assert_eq!(wide.len(), 3, "username, name, badge");
        let narrow = columns(&prepared, 30);
        assert!(narrow.len() < 3, "something was dropped whole");
        assert!(!narrow.is_empty(), "the username survives");
    }

    /// Enter on an account asks for its profile, and for the one the filter
    /// is showing under the cursor, not the one at that index of the whole
    /// list; on an empty list it asks for nothing.
    #[test]
    fn enter_asks_for_the_profile_under_the_cursor() {
        let people = [person(1, "anna", None), person(2, "bob", None)];
        let shelf = shelf_of(&people);
        let mut state = inside(&shelf);
        state.query = "b".to_string();
        refilter(&shelf, &mut state);
        match open(&shelf, &mut state) {
            Step::Profile(who) => assert_eq!(who.username, "bob"),
            _ => panic!("Enter on a row must ask for its profile"),
        }
        // The list is untouched by asking: position and filter stay.
        assert_eq!(state.query, "b");
        assert_eq!(state.shown, vec![1]);

        state.query = "zzz".to_string();
        refilter(&shelf, &mut state);
        assert!(matches!(open(&shelf, &mut state), Step::Go));
    }

    /// The failure a read leaves on the list names the account and the
    /// reason, on one line, in the failure's own register.
    #[test]
    fn a_failed_read_is_one_red_line_on_the_list() {
        let who = person(1, "anna", None);
        let note = could_not_read(
            &who,
            &anyhow::anyhow!(
                "rate limited
second line"
            ),
        );
        assert_eq!(note, "Could not open @anna: rate limited");
        assert!(
            tui::outcome_line(note, false)
                .to_string()
                .contains("Could not")
        );
    }

    /// While the profile is read the list stays behind a box that names the
    /// account and how to stop.
    #[test]
    fn the_reading_box_names_the_account_and_the_way_out() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let state = inside(&shelf);
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        let mut list = TableState::default();
        let mut page_rows = 0usize;
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &shelf,
                    &state,
                    "",
                    true,
                    false,
                    &mut list,
                    &mut page_rows,
                );
                reading_modal(frame, "Reading @anna...", false);
            })
            .unwrap();
        let screen: String = (0..14).map(|y| tui::row_text(&terminal, y)).collect();
        assert!(screen.contains("Reading @anna..."), "{screen}");
        assert!(screen.contains("q or ctrl+c cancels"), "{screen}");
    }

    /// The hint says what Enter does now.
    #[test]
    fn the_hint_says_enter_opens_the_profile() {
        let people = [person(1, "anna", None)];
        let shelf = shelf_of(&people);
        let state = inside(&shelf);
        let line = footer_line(&shelf, &state, "", false, false).to_string();
        assert!(line.contains("enter open profile"), "{line}");
    }
}
