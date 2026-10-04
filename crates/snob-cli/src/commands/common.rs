//! What the commands share: settling the filter and where the result goes,
//! opening the session, walking a list under a named bar, switching a view to
//! another account, and the cooldown refusal.
//!
//! The order between the first two and the session matters: for the list
//! commands, the destination is checked **before** the session is opened and
//! long before a request is spent, so `-o` pointing somewhere impossible costs
//! nothing to find out. The media listings, `stories` and `highlights`, check
//! their format and destination through [`checked_format`] once the tray has
//! been fetched.

use std::path::PathBuf;

use anyhow::{Context, Result};
use snob_core::Pk;
use snob_core::filters::{Attribute, Filter, parse_username_list};
use snob_core::model::{ListKind, User};
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::secrets::{SecretStore, SessionStore};

use crate::app::{App, Viewer};
use crate::cli::{Attr, BrowseArgs, FilterArgs, Format, ListArgs, OutputArgs, ScanArgs, WalkArgs};
use crate::engine::{self, ListOutcome};
use crate::exit::{ExitCode, ExitError};
use crate::output::{self, Presentation, Rendered};
use crate::report;
use crate::ui::accounts::Browsed;

/// The three forms either media listing has, and the refusal for the others —
/// so `-o out.xlsx` cannot fall through to a table with a spreadsheet's name
/// on it. One copy for `stories` and both levels of `highlights`, so the
/// refusal is spelled once.
pub fn checked_format(
    format: Option<crate::cli::StoryFormat>,
    destination: Option<&std::path::Path>,
    what: &str,
) -> Result<Format> {
    let format = output::effective_format(format.map(Into::into), destination);
    if matches!(format, Format::Csv | Format::Xlsx | Format::Md) {
        anyhow::bail!(
            "{what} has no {} form; it can be a table, json or ndjson",
            format!("{format:?}").to_ascii_lowercase()
        );
    }
    output::check_destination(format, destination)?;
    Ok(format)
}

/// Opens the app for a list command, or refuses because there is no session.
pub fn open(walk: &WalkArgs, secrets: &SessionStore, paths: &AccountPaths) -> Result<Box<App>> {
    app(secrets, paths, shows_progress(walk))
}

/// Whether a list command draws a bar. None when the answer comes out of
/// storage: there is nothing to watch.
pub fn shows_progress(walk: &WalkArgs) -> bool {
    !walk.progress.no_progress && !walk.offline
}

/// The account whose lists a list view shows, by the name another account
/// walks them again with: the one typed, or else the one stored for `pk`,
/// the account the walk was of. `None` when neither is known.
pub fn walked_name(app: &App, typed: Option<&str>, pk: Pk) -> Result<Option<String>> {
    Ok(match typed {
        Some(typed) => Some(engine::target::clean(typed).to_string()),
        None => snob_store::store::users::name(app.db().conn(), pk)?,
    })
}

/// About how many requests walking these lists of `pk` again takes as
/// another account, counted as `--dry-run` counts them
/// (`engine::estimate::rewalk_requests`): finding the account by its name,
/// and each list's walk by the counter this account last polled, or by how
/// many its walk found.
pub fn rewalk_cost(app: &App, pk: Pk, lists: &[(ListKind, usize)]) -> Result<u32> {
    let counted = snob_store::store::accounts::find(app.db().conn(), pk)?;
    let sizes: Vec<u64> = lists
        .iter()
        .map(|&(kind, found)| {
            counted
                .as_ref()
                .and_then(|a| a.counter(kind))
                .unwrap_or(found as u64)
        })
        .collect();
    let requests = engine::estimate::rewalk_requests(app.client().has_page(), true, &sizes);
    Ok(u32::try_from(requests).unwrap_or(u32::MAX))
}

/// Walks a list view's lists again as the account `to` it switched to: the
/// same lists of the same account, read with `walk`, under `to`'s consent
/// question and budget. A name nobody knows is refused before anything opens.
pub async fn rewalk<T>(
    secrets: &SecretStore,
    paths: &AppPaths,
    from: &Viewer,
    name: Option<&str>,
    to: &Viewer,
    with_progress: bool,
    walk: impl AsyncFnOnce(&mut App, String) -> Result<T>,
) -> Result<Switched<(Box<App>, T)>> {
    let Some(name) = name else {
        return Ok(Switched::Refused(format!(
            "Could not switch to {}: the name of the account walked is not known",
            to.label()
        )));
    };
    let name = name.to_string();
    switch(
        secrets,
        paths,
        from,
        to,
        with_progress,
        async |app: &mut App| walk(app, name).await,
    )
    .await
}

/// Opens the app, or refuses because there is no session.
///
/// An error with the code and the hint every other refusal carries, not a
/// line printed before an `Ok`: it goes through `report::print_error`, so a
/// caller that asked for JSON gets JSON for the one failure it is likeliest
/// to meet.
pub fn app(secrets: &SessionStore, paths: &AccountPaths, with_progress: bool) -> Result<Box<App>> {
    App::open(secrets, paths, with_progress)?
        .map(Box::new)
        .ok_or_else(no_session)
}

/// The refusal every command gives when there is no session.
pub fn no_session() -> anyhow::Error {
    ExitError::new(ExitCode::NoSession, "no session is stored")
        .with_hint("run \"snob login\"")
        .into()
}

/// The part of the command line the engine is asked with.
///
/// Here and not in `engine`, so the engine knows nothing about clap: this is
/// the one place the parser's structs are read for what the engine needs.
/// Two commands carry a walk, and both become the same query.
pub(crate) fn query(target: &Option<String>, walk: &WalkArgs) -> engine::ListQuery {
    engine::ListQuery {
        target: target.clone(),
        yes: walk.consent.yes,
        refresh: walk.refresh,
        cache: walk.offline,
        max_age: walk.max_age,
        no_resume: walk.no_resume,
        max_pages: walk.max_pages,
        over_budget: walk
            .same_day
            .then_some(snob_ig::pager::OverBudget::Continue),
        walk_at_most_every: None,
    }
}

impl From<&ListArgs> for engine::ListQuery {
    fn from(args: &ListArgs) -> Self {
        query(&args.target, &args.walk)
    }
}

impl From<&ScanArgs> for engine::ListQuery {
    fn from(args: &ScanArgs) -> Self {
        query(&args.target, &args.walk)
    }
}

/// What is left of a list after the filter and the cap, and how many there
/// were at each step -- the three numbers the summary line is built from.
///
/// The numbers have to be taken in this order -- total before the filter,
/// kept after it, shown after the cap -- and both commands that print a list
/// take them here.
pub struct Narrowed {
    pub shown: Vec<User>,
    /// After the filter, before the cap.
    pub kept: usize,
    /// Before the filter.
    pub total: usize,
}

pub fn narrow(users: Vec<User>, filter: &Filter, limit: Option<usize>) -> Narrowed {
    let total = users.len();
    let mut shown = filter.apply(users);
    let kept = shown.len();
    if let Some(cap) = limit {
        shown.truncate(cap);
    }
    Narrowed { shown, kept, total }
}

/// Opens the session of a command that reads one account's page: `profile`,
/// `pfp`, `stories` and `highlights`.
///
/// `-i` is refused before the session opens and before anything is spent,
/// like a bad `-o` -- the expensive order to find out in is the other one.
/// Then nothing is spent during a cooldown: there is nothing stored to serve
/// instead.
pub fn reader(
    secrets: &SecretStore,
    paths: &AccountPaths,
    interactive: bool,
    with_progress: bool,
) -> Result<Box<App>> {
    if interactive {
        crate::ui::people::check_drawable()?;
    }
    let app = app(&secrets.session_of(paths), paths, with_progress)?;
    refuse_during_cooldown(&app, "no request can be made")?;
    Ok(app)
}

/// The account a read names, or with no name the viewer's own, like the list
/// commands. The viewer's username is known from the session, or, when the
/// session never learned it, from the browser's document, so this costs
/// nothing extra.
///
/// A name read from the document is not stored: `whoami` is the one command
/// that writes the session back, and the document says it again for nothing.
/// Without a browser, a session with no name is refused.
pub async fn target_or_own(app: &App, target: Option<&str>) -> Result<String> {
    if let Some(target) = target {
        return Ok(target.to_string());
    }
    if let Some(name) = &app.viewer().username {
        return Ok(name.clone());
    }
    let refused =
        || anyhow::anyhow!("this session does not know its own username; name an account");
    if !app.client().has_page() {
        return Err(refused());
    }
    app.client().whoami().await?.username.ok_or_else(refused)
}

/// Refuses a command outright while the account is in cooldown.
///
/// For the commands that have nothing stored to serve instead -- a picture,
/// a story, a write. The list commands do not come here: `engine::cooldown`
/// answers them out of storage, which a refusal cannot. The gate is still
/// explicit in each command (the readers' is in [`reader`]), before anything
/// is asked of a person and before anything is spent; what is shared is the
/// sentence.
pub fn refuse_during_cooldown(app: &App, doing: &str) -> Result<()> {
    if let Some(held) = app.held()? {
        return Err(ExitError::new(
            ExitCode::RateLimited,
            format!("{}, so {doing}", report::held_until(&held)),
        )
        .into());
    }
    Ok(())
}

/// What switching a view to another account came to.
pub enum Switched<S> {
    /// That account's session, and what the view shows read as it.
    To(S),
    /// The view stays the account it was; this says why.
    Refused(String),
}

/// Opens the session of the account `to` that a view switched to, and reads
/// with `read` what the view shows as that account.
///
/// Nothing of the account the view was, `from`, is closed first, so a refusal
/// goes back to it as it was, for no request: no session stored, a cooldown
/// or the pause on every account, and a read that is declined, fails or has
/// nothing to show. Only what would have ended the run as either account ends
/// it: an interruption, a push-back or a challenge on the read. A declined
/// question carries the interruption's code without being one, so an
/// interruption is told by the process's token.
pub async fn switch<T>(
    secrets: &SecretStore,
    paths: &AppPaths,
    from: &Viewer,
    to: &Viewer,
    with_progress: bool,
    read: impl AsyncFnOnce(&mut App) -> Result<T>,
) -> Result<Switched<(Box<App>, T)>> {
    let refused = |e: anyhow::Error| {
        // `App::open` made `to` the account a failure is told as.
        report::act_as(from.clone());
        Switched::Refused(format!("Could not switch to {}: {e}", to.label()))
    };
    let account = paths.account(to.pk);
    let opened = app(&secrets.session_of(&account), &account, with_progress).and_then(|app| {
        refuse_during_cooldown(&app, "no request can be made")?;
        Ok(app)
    });
    let mut app = match opened {
        Ok(app) => app,
        Err(e) => return Ok(refused(e)),
    };
    match read(&mut app).await {
        Ok(read) => Ok(Switched::To((app, read))),
        Err(e)
            if app.cancel().is_canceled()
                || matches!(
                    crate::exit::exit_code_for(&e),
                    ExitCode::RateLimited | ExitCode::Challenge
                ) =>
        {
            Err(e)
        }
        Err(e) => Ok(refused(e)),
    }
}

/// Runs an interactive view over `session`, and again over each session it
/// switches to, until the view is left.
///
/// `browse` draws the view, opening with the note it is handed; `switch`
/// opens the account picked, usually through [`switch`]; `leave` is called
/// on each session as it is left, told whether the view acted as more than
/// one account, and before a failed switch ends the run. A refused switch
/// draws the view again over the same session with the reason as its note.
pub async fn switching<S>(
    mut session: S,
    mut browse: impl AsyncFnMut(&mut S, String) -> Result<Browsed>,
    mut switch: impl AsyncFnMut(&S, Viewer) -> Result<Switched<S>>,
    mut leave: impl FnMut(&S, bool),
) -> Result<ExitCode> {
    let mut note = String::new();
    let mut switched = false;
    loop {
        match browse(&mut session, std::mem::take(&mut note)).await? {
            Browsed::Done(code) => {
                leave(&session, switched);
                return Ok(code);
            }
            Browsed::SwitchTo(to) => match switch(&session, to).await {
                Ok(Switched::To(next)) => {
                    switched = true;
                    leave(&session, switched);
                    session = next;
                }
                Ok(Switched::Refused(text)) => note = text,
                Err(e) => {
                    leave(&session, switched);
                    return Err(e);
                }
            },
        }
    }
}

/// Where the result goes and what shape it takes.
///
/// Worked out once, up front, and carried around: a destination that cannot
/// hold the format must not cost a walk — or two — to find out about.
pub struct Destination {
    format: Format,
    presentation: Presentation,
    path: Option<PathBuf>,
}

impl Destination {
    pub fn format(&self) -> Format {
        self.format
    }

    /// Whether a person is watching this appear, which is what advice and
    /// decoration are for. A file and a pipe are neither.
    pub fn is_interactive(&self) -> bool {
        self.presentation.interactive
    }

    pub fn write(&self, users: &[User]) -> Result<()> {
        output::write(users, self.format, self.presentation, self.path.as_deref())
    }

    pub fn write_rendered(&self, rendered: &Rendered) -> Result<()> {
        output::write_rendered(rendered, self.path.as_deref())
    }
}

/// Settles the destination and refuses up front what would only fail later.
pub fn destination(args: &OutputArgs) -> Result<Destination> {
    let path = args.path.clone();
    let format = output::effective_format(args.format, path.as_deref());
    output::check_destination(format, path.as_deref())?;

    Ok(Destination {
        format,
        presentation: Presentation::detect(path.as_deref()),
        path,
    })
}

/// What a list command settles before its session is opened: the filter,
/// where the result goes, and whether it is browsed instead of printed.
pub fn prepare(
    filter: &FilterArgs,
    output: &OutputArgs,
    browse: &BrowseArgs,
) -> Result<(Filter, Destination, bool)> {
    let filter = filter_from(filter)?;
    let destination = destination(output)?;
    if browse.interactive {
        crate::ui::people::check_drawable()?;
    }
    // Decided before anything is spent, like the destination: what was said
    // first, detection only for a run that asked for nothing. The matrix is
    // `BrowseArgs::browses`.
    let browses = browse.browses(
        output.format.is_some() || output.path.is_some(),
        crate::ui::a_human_would_watch_the_listing_scroll_by(),
    );
    Ok((filter, destination, browses))
}

/// Builds the filter from the arguments.
pub fn filter_from(args: &FilterArgs) -> Result<Filter> {
    let mut hide: Vec<Attribute> = args.hide.iter().copied().map(attribute).collect();
    if args.no_verified && !hide.contains(&Attribute::Verified) {
        hide.push(Attribute::Verified);
    }

    let excluded = match &args.exclude_list {
        Some(path) => {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            parse_username_list(&contents)
        }
        None => Default::default(),
    };

    Ok(Filter {
        hide,
        only: args.only.iter().copied().map(attribute).collect(),
        excluded,
    })
}

fn attribute(a: Attr) -> Attribute {
    match a {
        Attr::Verified => Attribute::Verified,
        Attr::Private => Attribute::Private,
        Attr::NoPfp => Attribute::NoPfp,
    }
}

/// Refuses a list that could not be read in full where a missing account
/// makes the answer wrong rather than short; `misreading` is what such an
/// account would be made to look like.
pub fn require_complete(kind: ListKind, outcome: &ListOutcome, misreading: &str) -> Result<()> {
    if outcome.is_complete() {
        return Ok(());
    }
    Err(report::refuse_incomplete(kind, outcome, misreading))
}

/// One walk, with the bar named while it runs and taken down before anything
/// gives up.
///
/// The rule this holds is "finish the bar before the `?`". `indicatif` leaves
/// its last line on screen when it is dropped, so a run ending in a cooldown
/// refusal, a private account or an incomplete list would print the error
/// underneath a spinner that had stopped spinning. Held here, in the one place
/// every walk goes through, so no caller can forget it.
///
/// `check` runs inside the guarded region rather than after it. A crossing has
/// to know its first list is complete before spending the second walk, and that
/// refusal leaves through the same door as any other.
///
/// It does **not** finish on success: a crossing walks two lists through one
/// bar, and clearing it in between would make the second half start from a
/// blank line. The caller ends it when the run is over.
pub async fn walk_named(
    app: &mut App,
    args: &engine::ListQuery,
    kind: ListKind,
    subject: &str,
    check: impl FnOnce(&ListOutcome) -> Result<()>,
) -> Result<(Vec<User>, ListOutcome)> {
    app.progress().begin(&report::walking(kind, subject));

    // Both failures leave through one door, so the rule this function exists to
    // hold is written once inside it too. Two copies of `finish()` here would
    // make a third failure point added between them one more place to remember.
    let result = engine::list(app, args, kind).await.and_then(|pair| {
        check(&pair.1)?;
        Ok(pair)
    });
    if result.is_err() {
        app.progress().finish();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> FilterArgs {
        FilterArgs::default()
    }

    fn viewer(pk: u64, name: &str) -> Viewer {
        Viewer {
            pk: snob_core::Pk::new(pk),
            username: Some(name.to_string()),
        }
    }

    /// A switch the command could open draws the view again as that
    /// account, and each session is left once, told the view acted as more
    /// than one.
    #[tokio::test]
    async fn a_switch_draws_the_view_again_as_the_account_picked() {
        let mut script = vec![
            Browsed::SwitchTo(viewer(8, "work")),
            Browsed::Done(ExitCode::Ok),
        ]
        .into_iter();
        let mut drawn = Vec::new();
        let mut left = Vec::new();
        let code = switching(
            7u64,
            async |session: &mut u64, note: String| {
                drawn.push((*session, note));
                Ok(script.next().expect("drawn once too often"))
            },
            async |_: &u64, to: Viewer| Ok(Switched::To(to.pk.get())),
            |session: &u64, several| left.push((*session, several)),
        )
        .await
        .unwrap();
        assert_eq!(code, ExitCode::Ok);
        assert_eq!(drawn, [(7, String::new()), (8, String::new())]);
        assert_eq!(left, [(7, true), (8, true)]);
    }

    /// A refused switch draws the view again over the session it was, with
    /// the reason as its note, and leaves nothing.
    #[tokio::test]
    async fn a_refused_switch_goes_back_to_the_account_it_was() {
        let mut script = vec![
            Browsed::SwitchTo(viewer(8, "work")),
            Browsed::Done(ExitCode::Interrupted),
        ]
        .into_iter();
        let mut drawn = Vec::new();
        let mut left = Vec::new();
        let code = switching(
            7u64,
            async |session: &mut u64, note: String| {
                drawn.push((*session, note));
                Ok(script.next().expect("drawn once too often"))
            },
            async |_: &u64, _: Viewer| Ok(Switched::Refused("no session".to_string())),
            |session: &u64, several| left.push((*session, several)),
        )
        .await
        .unwrap();
        assert_eq!(code, ExitCode::Interrupted);
        assert_eq!(drawn, [(7, String::new()), (7, "no session".to_string())]);
        assert_eq!(left, [(7, false)]);
    }

    /// A switch that fails ends the run, and the session the view was is
    /// still left first, so its closing line is told.
    #[tokio::test]
    async fn a_failed_switch_leaves_the_account_it_was_before_ending() {
        let mut left = Vec::new();
        let ended = switching(
            7u64,
            async |_: &mut u64, _: String| Ok(Browsed::SwitchTo(viewer(8, "work"))),
            async |_: &u64, _: Viewer| -> Result<Switched<u64>> {
                Err(ExitError::new(ExitCode::RateLimited, "pushed back").into())
            },
            |session: &u64, several| left.push((*session, several)),
        )
        .await;
        assert!(ended.is_err());
        assert_eq!(left, [(7, false)]);
    }

    /// Saying no to the question a walk as the other account asks is a
    /// refusal that goes back to the view, not an interruption that ends it.
    #[tokio::test]
    async fn a_declined_question_on_the_read_is_refused_rather_than_ending_the_run() {
        use snob_core::session::{Session, SessionOrigin};
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let secrets = SecretStore::new(paths.clone(), true).with_service(&format!(
            "snob-ig-test-switch-declined-{}",
            std::process::id()
        ));
        let store = secrets.session_of(&paths.account(snob_core::Pk::new(8)));
        store
            .save(
                &Session::from_sessionid("8%3Aabc%3A1", "Mozilla/5.0", SessionOrigin::Paste)
                    .unwrap(),
            )
            .unwrap();
        let switched = switch(
            &secrets,
            &paths,
            &viewer(7, "me"),
            &viewer(8, "work"),
            false,
            async |_: &mut App| -> Result<()> { Err(report::refuse_declined("@me")) },
        )
        .await;
        store.delete().unwrap();
        match switched.unwrap() {
            Switched::Refused(note) => assert_eq!(
                note, "Could not switch to @work: nothing was done: @me was not confirmed",
                "{note}"
            ),
            Switched::To(_) => panic!("a declined read was taken as a switch"),
        }
    }

    /// An account with no session stored is refused before anything is
    /// opened or read, with a note that names it.
    #[tokio::test]
    async fn a_switch_to_an_account_with_no_session_is_refused_before_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let secrets = SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-switch-{}", std::process::id()));
        let mut read = false;
        let switched = switch(
            &secrets,
            &paths,
            &viewer(7, "me"),
            &viewer(8, "work"),
            false,
            async |_: &mut App| {
                read = true;
                Ok(())
            },
        )
        .await
        .unwrap();
        match switched {
            Switched::Refused(note) => assert_eq!(
                note, "Could not switch to @work: no session is stored",
                "{note}"
            ),
            Switched::To(_) => panic!("an account with no session was opened"),
        }
        assert!(!read);
        assert!(!paths.account(snob_core::Pk::new(8)).db_file().exists());
    }

    /// The cost of walking again counts pages by the counter this account
    /// polled, else by what its walk found, plus the request that finds the
    /// account.
    #[test]
    fn the_cost_of_walking_again_reads_the_known_counter_first() {
        let session = snob_core::session::Session::from_sessionid(
            "7%3AAbCdEfGh%3A20",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/138.0.0.0 Safari/537.36",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        let client =
            snob_ig::client::IgClient::new(session, snob_ig::pace::Pacer::unlimited()).unwrap();
        let app = App::for_test(
            client,
            snob_store::store::Store::in_memory().unwrap(),
            viewer(7, "me"),
        );
        let them = Pk::new(42);
        let lists = [(ListKind::Followers, 60), (ListKind::Following, 10)];
        // Nothing polled: 60 and 10 accounts are five pages of twelve and
        // one. This client sends directly, a request a page: the finding,
        // the two walks, and the second list's poll.
        assert!(!app.client().has_page());
        assert_eq!(rewalk_cost(&app, them, &lists).unwrap(), 1 + 5 + 1 + 1);

        let user = User {
            pk: them,
            username: "them".to_string(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        };
        snob_store::store::users::upsert(app.db().conn(), &user).unwrap();
        snob_store::store::accounts::upsert(app.db().conn(), them, false).unwrap();
        snob_store::store::accounts::record_poll(app.db().conn(), them, Some(500), None).unwrap();
        assert_eq!(rewalk_cost(&app, them, &lists).unwrap(), 1 + 42 + 1 + 1);
    }

    /// `--same-day` is the one flag that reads past the day's accounts, and
    /// no flag leaves the choice to the engine, which pauses.
    #[test]
    fn same_day_is_the_only_way_past_the_day() {
        let walk = WalkArgs::default();
        assert_eq!(query(&None, &walk).over_budget, None);
        let same_day = WalkArgs {
            same_day: true,
            ..WalkArgs::default()
        };
        assert_eq!(
            query(&None, &same_day).over_budget,
            Some(snob_ig::pager::OverBudget::Continue)
        );
    }

    /// An incomplete list blocks the answer, whatever stopped it, and the
    /// refusal names the list and the misreading it would have caused.
    #[test]
    fn an_incomplete_list_blocks_the_answer() {
        use snob_core::model::StopReason;
        let outcome = |reason| ListOutcome::for_test(engine::Provenance::Walked, reason);
        let missing = "they were not there at all";
        assert!(
            require_complete(
                ListKind::Followers,
                &outcome(StopReason::Completed),
                missing
            )
            .is_ok()
        );
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            let error = require_complete(ListKind::Followers, &outcome(reason), missing)
                .expect_err(&format!("with {reason:?} no answer can be given"))
                .to_string();
            assert!(error.contains("wrong"), "{error}");
            assert!(error.contains("the followers list"), "{error}");
            assert!(error.contains(missing), "{error}");
        }
        let error = require_complete(
            ListKind::Following,
            &outcome(StopReason::Truncated),
            missing,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("the following list"), "{error}");
    }

    #[test]
    fn with_no_flags_the_filter_is_empty() {
        assert!(filter_from(&args()).unwrap().is_empty());
    }

    #[test]
    fn no_verified_is_shorthand_for_hiding_verified() {
        let mut a = args();
        a.no_verified = true;
        let f = filter_from(&a).unwrap();
        assert_eq!(f.hide, vec![Attribute::Verified]);
    }

    #[test]
    fn the_shorthand_does_not_duplicate_what_was_already_there() {
        let mut a = args();
        a.no_verified = true;
        a.hide = vec![Attr::Verified];
        assert_eq!(filter_from(&a).unwrap().hide.len(), 1);
    }

    #[test]
    fn it_translates_all_three_attributes() {
        let mut a = args();
        a.hide = vec![Attr::Verified, Attr::Private, Attr::NoPfp];
        let f = filter_from(&a).unwrap();
        assert_eq!(
            f.hide,
            vec![Attribute::Verified, Attribute::Private, Attribute::NoPfp]
        );
    }

    #[test]
    fn an_exclusion_file_that_does_not_exist_gives_a_clear_error() {
        let mut a = args();
        a.exclude_list = Some(PathBuf::from("no-such-file.txt"));
        let error = filter_from(&a).unwrap_err().to_string();
        assert!(error.contains("could not read"));
    }

    /// The extension decides the format when nothing else did, and it has to
    /// be settled before the first request rather than after the walk.
    #[test]
    fn the_destination_is_settled_from_the_arguments() {
        let to_file = OutputArgs {
            format: None,
            path: Some(PathBuf::from("result.csv")),
        };
        assert_eq!(destination(&to_file).unwrap().format(), Format::Csv);

        // A spreadsheet on standard output is refused here, before anything is
        // spent finding out.
        let binary = OutputArgs {
            format: Some(Format::Xlsx),
            path: None,
        };
        assert!(destination(&binary).is_err());
    }
}
