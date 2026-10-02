//! The account picker: which of the accounts signed in here a view acts as.
//!
//! A modal over whatever view opened it, drawn like the profile's actions
//! panel. One row per account in the registry, the active one and any with no
//! session stored marked, and "Add an account" last. Enter picks an account
//! for this view only; `u` makes the highlighted one the active account, the
//! one commands act as when none is named, and leaves the view as it is.
//!
//! It reads the registry and whether each account has a session stored, and
//! never another account's database.
//!
//! Adding an account is the browser login `snob login --add` runs, on the
//! real screen with the view suspended. The Ctrl+C that cancels it is the
//! process's (`interrupt::install`), so it cancels every request the view
//! would send after it too, and the view ends with it: the hint says so.

use anyhow::Result;
use console::measure_text_width;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Row, TableState};
use snob_core::Pk;
use snob_store::paths::AppPaths;
use snob_store::registry::{Registry, RegistryError};
use snob_store::secrets::SecretStore;

use crate::app::Viewer;
use crate::exit::ExitCode;
use crate::ui::browser::input::{self, Action, Raw, TICK, action_of};
use crate::ui::tui::{self, Tui};

/// One account, as the picker shows it.
#[derive(Debug, Clone)]
pub struct Account {
    pub viewer: Viewer,
    pub active: bool,
    /// Whether a session is stored for it. One without cannot be picked.
    pub session: bool,
}

/// The registry's accounts, in its order. `stored` says whether one has a
/// session stored.
pub fn accounts(registry: &Registry, stored: impl Fn(Pk) -> bool) -> Vec<Account> {
    registry
        .accounts
        .iter()
        .map(|account| Account {
            viewer: Viewer::from(account),
            active: registry.active == Some(account.pk),
            session: stored(account.pk),
        })
        .collect()
}

/// What a key asked of the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Stay,
    /// Back to the view, as the account it was.
    Close,
    /// The view as this account, for this session only.
    Switch(Pk),
    /// This account made the active one.
    MakeActive(Pk),
    /// A browser login for another account.
    Add,
    Redraw,
    Leave(ExitCode),
}

/// The picker's state: the rows, the selection and the note under them.
pub struct Picker {
    accounts: Vec<Account>,
    /// Into `accounts`; one past the last is "Add an account".
    selected: usize,
    /// The account the view acts as.
    current: Pk,
    note: String,
}

impl Picker {
    /// The selection starts on the account the view acts as.
    pub fn new(accounts: Vec<Account>, current: Pk) -> Self {
        let mut picker = Self {
            accounts: Vec::new(),
            selected: 0,
            current,
            note: String::new(),
        };
        picker.refresh(accounts, Some(current));
        picker
    }

    /// The rows read again, the selection on `on` when it is listed.
    fn refresh(&mut self, accounts: Vec<Account>, on: Option<Pk>) {
        self.accounts = accounts;
        self.selected = on
            .and_then(|pk| self.accounts.iter().position(|a| a.viewer.pk == pk))
            .unwrap_or(self.selected.min(self.accounts.len()));
    }

    fn account(&self, pk: Pk) -> Option<&Account> {
        self.accounts.iter().find(|a| a.viewer.pk == pk)
    }

    fn on_add(&self) -> bool {
        self.selected == self.accounts.len()
    }

    /// One key. Esc, Left and `a` close the picker; `q` and Ctrl+C leave the
    /// view, as they do from the profile's actions panel.
    pub fn key(&mut self, key: KeyEvent) -> Step {
        self.note.clear();
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if plain && key.code == KeyCode::Char('u') {
            return match self.accounts.get(self.selected) {
                Some(account) if account.active => {
                    self.note = format!("{} is the active account", account.viewer.label());
                    Step::Stay
                }
                Some(account) => Step::MakeActive(account.viewer.pk),
                None => Step::Stay,
            };
        }
        // `action_of` reads Esc as leaving; here it only closes the picker.
        if plain && key.code == KeyCode::Esc {
            return Step::Close;
        }
        let last = self.accounts.len();
        match action_of(key) {
            Action::Up => self.selected = self.selected.saturating_sub(1),
            Action::Down => self.selected = (self.selected + 1).min(last),
            Action::First | Action::PageUp => self.selected = 0,
            Action::Last | Action::PageDown => self.selected = last,
            Action::Open => return self.open(),
            Action::Back | Action::Accounts => return Step::Close,
            Action::Redraw => return Step::Redraw,
            Action::Quit => return Step::Leave(ExitCode::Ok),
            Action::Interrupt => return Step::Leave(ExitCode::Interrupted),
            Action::Download | Action::None => {}
        }
        Step::Stay
    }

    fn open(&mut self) -> Step {
        match self.accounts.get(self.selected) {
            None => Step::Add,
            Some(account) if account.viewer.pk == self.current => Step::Close,
            Some(account) if !account.session => {
                self.note = format!(
                    "{} has no session here; \"snob login --account {}\" signs it in",
                    account.viewer.label(),
                    account.viewer.pk
                );
                Step::Stay
            }
            Some(account) => Step::Switch(account.viewer.pk),
        }
    }
}

/// What the picker came back with.
#[derive(Debug)]
pub enum Picked {
    /// Back to the view as the account it was.
    Stay,
    /// The view as this account, for this session only.
    Switch(Viewer),
    /// The view is over.
    Leave(ExitCode),
}

/// How a view that can change accounts ended.
#[derive(Debug)]
pub enum Browsed {
    Done(ExitCode),
    /// The user picked this account: the command shows the same thing again,
    /// read as it.
    SwitchTo(Viewer),
}

impl Browsed {
    /// The code the view's loop ended with, unless it ended to switch.
    pub fn of(code: ExitCode, switch_to: Option<Viewer>) -> Self {
        match switch_to {
            Some(viewer) => Self::SwitchTo(viewer),
            None => Self::Done(code),
        }
    }
}

/// Runs the picker over the view `under` draws, until an account is picked
/// or the picker is closed.
pub async fn pick(
    tui: &mut Tui,
    secrets: &SecretStore,
    paths: &AppPaths,
    current: &Viewer,
    mut under: impl FnMut(&mut Frame<'_>),
) -> Result<Picked> {
    let colors = tui::colors_enabled();
    let stored = |pk| secrets.session_of(&paths.account(pk)).something_is_stored();
    let mut picker = Picker::new(Vec::new(), current.pk);
    match Registry::load(paths) {
        Ok(registry) => picker.refresh(accounts(&registry, stored), Some(current.pk)),
        Err(e) => picker.note = format!("Could not read the accounts: {e}"),
    }

    loop {
        tui.terminal.draw(|frame| {
            under(frame);
            draw(frame, &picker, current, colors);
        })?;
        let Raw::Key(key) = input::read(TICK).map_err(input::unreadable)? else {
            continue;
        };
        match picker.key(key) {
            Step::Stay => {}
            Step::Close => return Ok(Picked::Stay),
            Step::Switch(pk) => {
                if let Some(account) = picker.account(pk) {
                    return Ok(Picked::Switch(account.viewer.clone()));
                }
            }
            Step::MakeActive(pk) => {
                picker.note = match make_active(paths, pk) {
                    Ok(Some(registry)) => {
                        picker.refresh(accounts(&registry, stored), Some(pk));
                        let now = picker.account(pk).map(|a| a.viewer.label());
                        let now = now.unwrap_or_else(|| pk.to_string());
                        if pk == current.pk {
                            format!("{now} is the active account now")
                        } else {
                            format!(
                                "{now} is the active account now; this view stays as {}",
                                current.label()
                            )
                        }
                    }
                    Ok(None) => "That account is no longer signed in here".to_string(),
                    Err(e) => format!("Could not make it active: {e}"),
                };
            }
            Step::Add => {
                tui.suspend()?;
                let added = crate::commands::login::add_by_browser(secrets.clone(), paths).await;
                tui.resume()?;
                if crate::interrupt::interrupted() {
                    return Ok(Picked::Leave(ExitCode::Interrupted));
                }
                picker.note = match added {
                    Ok(ExitCode::Ok) => match Registry::load(paths) {
                        // A login leaves the account it signed in as active.
                        Ok(registry) => {
                            let added = registry.active;
                            picker.refresh(accounts(&registry, stored), added);
                            match added.and_then(|pk| picker.account(pk)) {
                                Some(account) => format!("Signed in as {}", account.viewer.label()),
                                None => "Signed in".to_string(),
                            }
                        }
                        Err(e) => format!("Could not read the accounts: {e}"),
                    },
                    Ok(_) => "No account was added".to_string(),
                    Err(e) if crate::exit::from_chain(&e) == Some(ExitCode::Interrupted) => {
                        "No account was added".to_string()
                    }
                    Err(e) => format!("Could not add an account: {e}"),
                };
            }
            Step::Redraw => tui.terminal.clear()?,
            Step::Leave(code) => return Ok(Picked::Leave(code)),
        }
    }
}

/// Makes `pk` the active account, under the registry's lock. `None` when the
/// registry no longer lists it.
fn make_active(paths: &AppPaths, pk: Pk) -> Result<Option<Registry>, RegistryError> {
    let mut listed = false;
    let registry = Registry::update(paths, |registry| {
        listed = registry.get(pk).is_some();
        if listed {
            registry.active = Some(pk);
        }
    })?;
    Ok(listed.then_some(registry))
}

/// What a row says beside the account's name.
fn marks(account: &Account) -> String {
    match (account.active, account.session) {
        (true, true) => "active".to_string(),
        (true, false) => "active · no session".to_string(),
        (false, true) => String::new(),
        (false, false) => "no session".to_string(),
    }
}

/// The picker, veiling the view drawn under it.
fn draw(frame: &mut Frame<'_>, picker: &Picker, current: &Viewer, colors: bool) {
    let dim = tui::paint(colors, tui::DIM);
    tui::veil(frame, colors);

    let rows: Vec<(String, String)> = picker
        .accounts
        .iter()
        .map(|account| (account.viewer.label(), marks(account)))
        .chain(std::iter::once((
            "Add an account".to_string(),
            "log in with a browser".to_string(),
        )))
        .collect();
    let hint = if picker.on_add() {
        "enter log in · Ctrl+C during the login also ends this view · esc back"
    } else {
        "enter use in this view · u make active · esc back"
    };
    let name_w = rows
        .iter()
        .map(|(name, _)| measure_text_width(name))
        .max()
        .unwrap_or(0)
        .max(16) as u16;
    let what_w = rows
        .iter()
        .map(|(_, what)| measure_text_width(what))
        .max()
        .unwrap_or(0) as u16;
    let w = (2 + 4 + 2 + name_w + 2 + what_w)
        .max(measure_text_width(hint) as u16 + 4)
        .clamp(44, 80)
        .min(frame.area().width);
    let area = tui::modal_area(frame.area(), w, rows.len() as u16 + 4);

    let title = Line::from(Span::styled(" accounts ", dim)).centered();
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(tui::card_padding())
        .title_top(title)
        .title_bottom(tui::footer(&picker.note, hint, colors));
    block = tui::top_right(block, None, tui::viewer_line(current, colors)).border_style(dim);
    let inner = tui::open_modal(frame, area, block);

    let mut state = TableState::default();
    state.select(Some(picker.selected));
    frame.render_stateful_widget(
        tui::list_table(
            rows.into_iter()
                .map(|(name, what)| Row::new(vec![Cell::from(name), Cell::from(what).style(dim)])),
            [Constraint::Length(name_w), Constraint::Fill(1)],
        ),
        inner,
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use snob_core::Epoch;
    use snob_store::registry::Registered;

    use super::*;

    fn registry() -> Registry {
        let registered = |pk: u64, name: &str| Registered {
            pk: Pk::new(pk),
            username: name.to_string(),
            added_at: Epoch::new(0),
        };
        Registry {
            active: Some(Pk::new(7)),
            accounts: vec![
                registered(7, "me"),
                registered(8, "work"),
                registered(9, "old"),
            ],
        }
    }

    /// `@me` active, `@work` signed in, `@old` with no session.
    fn picker() -> Picker {
        Picker::new(accounts(&registry(), |pk| pk != Pk::new(9)), Pk::new(7))
    }

    fn press(picker: &mut Picker, code: KeyCode) -> Step {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn rendered(picker: &Picker) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, picker, &tui::me(), false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_rows_come_from_the_registry_and_the_stored_sessions() {
        let rows = accounts(&registry(), |pk| pk != Pk::new(9));
        let seen: Vec<(String, bool, bool)> = rows
            .iter()
            .map(|a| (a.viewer.label(), a.active, a.session))
            .collect();
        assert_eq!(
            seen,
            [
                ("@me".to_string(), true, true),
                ("@work".to_string(), false, true),
                ("@old".to_string(), false, false),
            ]
        );
    }

    #[test]
    fn it_opens_on_the_account_the_view_acts_as() {
        let picker = Picker::new(accounts(&registry(), |_| true), Pk::new(8));
        assert_eq!(picker.selected, 1);
    }

    /// Enter on another account switches this view to it; on the account
    /// the view already is, it only closes; on one with no session it says
    /// how to sign it in; on the last row it adds one.
    #[test]
    fn enter_switches_closes_explains_or_adds() {
        let mut picker = picker();
        assert_eq!(press(&mut picker, KeyCode::Enter), Step::Close);

        press(&mut picker, KeyCode::Down);
        assert_eq!(press(&mut picker, KeyCode::Enter), Step::Switch(Pk::new(8)));

        press(&mut picker, KeyCode::Down);
        assert_eq!(press(&mut picker, KeyCode::Enter), Step::Stay);
        assert!(
            picker.note.contains("@old has no session"),
            "{}",
            picker.note
        );

        press(&mut picker, KeyCode::Down);
        assert!(picker.on_add());
        assert_eq!(press(&mut picker, KeyCode::Enter), Step::Add);
        // The last row is as far as the selection goes.
        press(&mut picker, KeyCode::Down);
        assert!(picker.on_add());
    }

    /// `u` makes the highlighted account active, and does nothing on the
    /// account already active or on the row that adds one.
    #[test]
    fn u_makes_the_highlighted_account_active() {
        let mut picker = picker();
        assert_eq!(press(&mut picker, KeyCode::Char('u')), Step::Stay);
        assert!(picker.note.contains("is the active account"));

        press(&mut picker, KeyCode::Down);
        assert_eq!(
            press(&mut picker, KeyCode::Char('u')),
            Step::MakeActive(Pk::new(8))
        );
        assert!(picker.note.is_empty());

        press(&mut picker, KeyCode::End);
        assert_eq!(press(&mut picker, KeyCode::Char('u')), Step::Stay);
    }

    /// Esc, Left and `a` close the picker; `q` and Ctrl+C leave the view.
    #[test]
    fn the_ways_out() {
        let mut picker = picker();
        assert_eq!(press(&mut picker, KeyCode::Esc), Step::Close);
        assert_eq!(press(&mut picker, KeyCode::Left), Step::Close);
        assert_eq!(press(&mut picker, KeyCode::Char('a')), Step::Close);
        assert_eq!(
            press(&mut picker, KeyCode::Char('q')),
            Step::Leave(ExitCode::Ok)
        );
        assert_eq!(
            picker.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Step::Leave(ExitCode::Interrupted)
        );
    }

    #[test]
    fn making_an_account_active_writes_the_registry_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        Registry::update(&paths, |registry| {
            registry.upsert(Pk::new(7), "me", Epoch::new(0));
            registry.upsert(Pk::new(8), "work", Epoch::new(0));
            registry.active = Some(Pk::new(7));
        })
        .unwrap();

        let written = make_active(&paths, Pk::new(8)).unwrap().unwrap();
        assert_eq!(written.active, Some(Pk::new(8)));
        assert_eq!(Registry::load(&paths).unwrap(), written);

        assert!(make_active(&paths, Pk::new(9)).unwrap().is_none());
        assert_eq!(Registry::load(&paths).unwrap().active, Some(Pk::new(8)));
    }

    #[test]
    fn the_modal_marks_the_active_account_and_the_ones_with_no_session() {
        let picker = picker();
        let screen = rendered(&picker);
        assert!(screen.contains(" accounts "), "{screen}");
        assert!(screen.contains(" as @me "), "{screen}");
        let line = |name: &str| {
            screen
                .lines()
                .find(|line| line.contains(name))
                .unwrap_or_else(|| panic!("no {name} in\n{screen}"))
                .to_string()
        };
        assert!(line("> @me").contains("active"), "{screen}");
        assert!(!line("@work").contains("active"), "{screen}");
        assert!(line("@old").contains("no session"), "{screen}");
        assert!(line("Add an account").contains("log in with a browser"));
        assert!(screen.contains("u make active"), "{screen}");
    }

    /// On the row that adds an account, the hint says a Ctrl+C during the
    /// login ends the view too.
    #[test]
    fn the_add_row_warns_that_ctrl_c_ends_the_view() {
        let mut picker = picker();
        press(&mut picker, KeyCode::End);
        let screen = rendered(&picker);
        assert!(screen.contains("Ctrl+C during the login"), "{screen}");
    }

    /// A view that ended on a pick switches to it, whatever its code; one
    /// that did not ends with its code.
    #[test]
    fn a_view_ends_by_switching_only_when_an_account_was_picked() {
        let work = Viewer {
            pk: Pk::new(8),
            username: Some("work".to_string()),
        };
        assert!(matches!(
            Browsed::of(ExitCode::Ok, Some(work)),
            Browsed::SwitchTo(to) if to.pk == Pk::new(8)
        ));
        assert!(matches!(
            Browsed::of(ExitCode::Interrupted, None),
            Browsed::Done(ExitCode::Interrupted)
        ));
    }
}
