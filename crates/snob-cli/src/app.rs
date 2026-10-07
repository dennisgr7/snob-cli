//! What every command hangs off.
//!
//! One place assembles the four things the tool is made of — who we are talking
//! as, where the data is kept, what the requests cost, and how the run reports
//! itself — and hands them out already wired together. Commands take an `App`
//! and orchestrate; they never build a client or open a database themselves.
//!
//! The order below is not arbitrary. The progress bar has to exist before the
//! pacer, because the pacer is what announces a wait and the bar is where that
//! announcement goes; and the pacer has to exist before the client, because a
//! client without one cannot be built at all.

use std::sync::Arc;

use anyhow::Result;
use snob_core::{EpochMs, Pk};
use snob_ig::client::IgClient;
use snob_ig::pace::{CancelToken, Pacer};
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::SessionStore;
use snob_store::store::Store;
use snob_store::store::rate_budget::SqliteRateBudget;

use crate::engine::target;
use crate::interrupt;
use crate::progress::Progress;

/// The account the run is acting as, when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub pk: Pk,
    /// Absent until the first run resolves it. Cosmetic: nothing blocks on it.
    pub username: Option<String>,
}

impl Viewer {
    /// The name, filtered for anything that is going to draw it.
    ///
    /// It came from Instagram — `resolve_username`, or the browser handing the
    /// session over — not from the person running the tool, so it is treated
    /// like any other name off the wire, as `engine::target::label` treats the
    /// name that was typed.
    pub fn safe_username(&self) -> Option<String> {
        self.username.as_deref().map(snob_core::model::printable)
    }

    /// How to name this account to a person: `@someone`, or the id when the
    /// name has not been learned yet.
    pub fn label(&self) -> String {
        label(self.pk, self.username.as_deref())
    }

    /// The `viewer` object a JSON answer names the account with.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({ "pk": self.pk, "username": self.safe_username() })
    }
}

/// How to name any account to a person: `@someone`, or the id when the name has
/// not been learned yet.
///
/// A free function because the monitor names accounts it never has a `Viewer`
/// for, and one spelling of the rule is one place to remember `printable`.
pub fn label(pk: Pk, username: Option<&str>) -> String {
    match username {
        Some(name) => format!("@{}", snob_core::model::printable(name)),
        None => format!("account {pk}"),
    }
}

/// The same rule where the absent case means the viewer rather than an account
/// whose name is not known yet.
///
/// [`label`] answers "which account is this" and falls back to an id; this
/// answers "whose lists are we talking about" and falls back to "your account".
/// Two different questions, which is why they are two functions. This one names
/// the target in the banner a scheduled run opens with, the line `watch check`
/// prints per account, and the context on a failed walk.
pub fn target_label(target: Option<&str>) -> String {
    match target {
        Some(name) => format!("@{}", snob_core::model::printable(name)),
        None => "your account".to_string(),
    }
}

/// The one place a [`Pacer`] is assembled.
///
/// Three things have to be true of every one of them: the store is opened
/// first, because it creates the schema the budget then opens its own
/// connection to; the process's cancellation token is attached, so Ctrl+C
/// reaches a request waiting on the budget; and somebody is told when a wait is
/// imposed, because a command that stops dead for twenty minutes with no
/// explanation reads as a hang.
///
/// `announce` stays with the caller: what a wait looks like is presentation, and
/// a progress bar is right for a walk while a line on standard error is right
/// for a single request. The wiring is what is shared.
fn pacer(
    paths: &AccountPaths,
    announce: Arc<dyn Fn(std::time::Duration) + Send + Sync>,
) -> Result<Pacer> {
    // The store goes first: it is what creates the schema, and the budget opens
    // its own connection to a file that has to have tables already.
    Store::open(paths)?;
    Ok(Pacer::new(Arc::new(SqliteRateBudget::open(paths)?))
        .with_cancel(interrupt::install())
        .announcing(announce))
}

/// `pacer` with the announcement the single-request commands, `login` and
/// `whoami`, want: one line on standard error, because there is nothing for a
/// bar to count.
pub fn pacer_saying_a_line(paths: &AccountPaths) -> Result<Pacer> {
    pacer(
        paths,
        Arc::new(|waited: std::time::Duration| {
            crate::ui::info(&format!(
                "The request budget is rationing; waiting {}.",
                snob_core::duration::format(waited)
            ));
        }),
    )
}

/// Until when this account may send nothing, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub until_ms: EpochMs,
    /// The accounts whose push-backs stopped every account, named. Empty when
    /// what holds this account is its own cooldown.
    pub braked: Vec<String>,
}

/// [`Pacer::cooldown`], and whether it is the brake on every account.
///
/// `paths` is where the braked accounts' names are looked up; without it, or
/// with the registry unreadable, they are named by id.
pub fn held(pacer: &Pacer, paths: Option<&AppPaths>) -> Result<Option<Held>> {
    let Some(until_ms) = pacer.cooldown()? else {
        return Ok(None);
    };
    let braked = match pacer.brake()? {
        Some(brake) if brake.until >= until_ms => {
            let registry = paths
                .and_then(|paths| Registry::load(paths).ok())
                .unwrap_or_default();
            brake
                .accounts
                .iter()
                .map(|&pk| label(pk, registry.get(pk).map(|r| r.username.as_str())))
                .collect()
        }
        _ => Vec::new(),
    };
    Ok(Some(Held { until_ms, braked }))
}

/// How a run could have been given consent before it started.
///
/// The refusal printed when nobody is at a terminal names the way *this*
/// command takes an answer in advance, and the two commands do not take it the
/// same way. The list commands have `-y`. `snob watch once` deliberately does
/// not — the reasoning is written at `WatchOnceArgs` in `cli.rs`, and it is
/// that consent handed over on a command line is consent from whoever wrote
/// the cron entry — so advice naming `-y` there would fail to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConsentInAdvance {
    /// `-y` on the command line.
    #[default]
    Flag,
    /// An `[[account]]` in `watch.toml` carrying the answer somebody gave once,
    /// which is what `snob watch setup` writes.
    WatchConfig,
}

pub struct App {
    /// Shared, for the one place that fans work out across tasks: the story
    /// downloads. Everywhere else reads it through [`App::client`].
    client: Arc<IgClient>,
    db: Store,
    progress: Progress,
    consent_in_advance: ConsentInAdvance,
    viewer: Viewer,
    /// Where the other accounts are named, for [`App::held`]. `None` in a test
    /// app, which names them by id.
    app_paths: Option<AppPaths>,
    consented: Option<String>,
    /// What was asked for, what it resolved to, and the action it was resolved
    /// in.
    ///
    /// A crossing calls `engine::list` twice, and without this each call would
    /// resolve from scratch: two identical `web_profile_info` requests about the
    /// same account seconds apart, or four on a session whose username has never
    /// been resolved, because resolving the name and polling the counters are
    /// separate requests there.
    ///
    /// **The question is part of the key, not just the answer**, because `snob
    /// watch` walks several configured accounts through a single `App`: keyed
    /// on the target alone, the second account would be handed the first one's
    /// target, and its changes committed under the first account's marks.
    /// Keying on what was asked makes reuse mean "the same question".
    ///
    /// The action is what makes reuse safe in time. Requests are paced per
    /// user action (`snob_ig::pace`): the requests that open a profile, its
    /// hover card among them, go out within seconds of each other, while a
    /// walk takes minutes in which the account may have changed. So the
    /// counters are only good while `Pacer::actions()` has not moved, which a
    /// walk, each sitting of one and a monitor tick move. "N requests" and the
    /// JSON `requests` still read `Pacer::spent()`, the truth about requests.
    resolved: Option<Memo>,
    /// Asked between every two pages of a walk: `true` stops it there, kept
    /// for the next run. Set by the monitor alone, to the battery being
    /// critical ([`App::stops_on_a_critical_battery`]).
    stop_between_pages: Option<fn() -> bool>,
}

/// One resolution, and the question it answers.
struct Memo {
    /// The `--target` this was resolved for, exactly as it was given. `None` is
    /// the viewer's own account, which is a different question from any name.
    asked: Option<String>,
    target: target::Target,
    /// `Pacer::actions()` when it was resolved, or its counters were read.
    action: u32,
}

impl App {
    /// Opens everything for a run that acts as the account `paths` is.
    ///
    /// `None` means there is no session stored, which every command turns into
    /// the same message and the same exit code. A session that is another
    /// account's is refused: its requests would be paid for, and recorded, in
    /// this account's database.
    pub fn open(
        secrets: &SessionStore,
        paths: &AccountPaths,
        with_progress: bool,
    ) -> Result<Option<Self>> {
        let Some(mut session) = secrets.load()? else {
            return Ok(None);
        };
        if session.ds_user_id != paths.pk() {
            anyhow::bail!(
                "the session stored for account {} is account {}'s; log in again with \
                 \"snob login\"",
                paths.pk(),
                session.ds_user_id
            );
        }

        // Done here rather than at login because a browser updates long after
        // the session was created, and this is the only moment every command
        // passes through. Failing to store the fresher one is not worth an
        // error: the session still works, and the next run tries again.
        if crate::browser::refresh_user_agent(&mut session)
            && let Err(e) = secrets.save(&session)
        {
            tracing::debug!(error = %e, "could not store the refreshed User-Agent");
        }

        let viewer = Viewer {
            pk: session.ds_user_id,
            username: session.username.clone(),
        };
        crate::report::act_as(viewer.clone());

        let db = Store::open(paths)?;
        // Retention, for the commands that are not the monitor. At most once a
        // day, and here because this is the only moment every command passes
        // through -- the same reason the User-Agent refresh above is here.
        crate::engine::watch::settle_daily(&db);
        let progress = Progress::new(with_progress);
        // Armed before the pacer opens the store and the budget, so a Ctrl+C
        // during those opens stops the run in order rather than killing it.
        interrupt::install();

        let pacer = pacer(paths, {
            let progress = progress.clone();
            // `waiting` rather than `note`: the number counts down on the bar
            // instead of being frozen into the message at the moment the wait
            // began.
            Arc::new(move |waited: std::time::Duration| {
                progress.waiting("the request budget is rationing", waited);
            })
        })?;

        Ok(Some(Self {
            client: Arc::new(IgClient::new(session, pacer)?),
            db,
            progress,
            viewer,
            app_paths: Some(AppPaths::clone(paths)),
            consented: None,
            consent_in_advance: ConsentInAdvance::default(),
            resolved: None,
            stop_between_pages: None,
        }))
    }

    /// An app wired by hand, so the engine can be driven against a mock server.
    ///
    /// **Tests only.** It skips the signal handler and the progress bar, which
    /// are the two things a test has no use for and one of which would spawn a
    /// task per test. Its cancellation is the client's pacer token, as in
    /// [`App::open`], so canceling a test app reaches its requests.
    #[doc(hidden)]
    pub fn for_test(client: IgClient, db: Store, viewer: Viewer) -> Self {
        Self {
            client: Arc::new(client),
            db,
            progress: Progress::new(false),
            viewer,
            app_paths: None,
            consented: None,
            consent_in_advance: ConsentInAdvance::default(),
            resolved: None,
            stop_between_pages: None,
        }
    }

    pub fn client(&self) -> &IgClient {
        &self.client
    }

    /// The client, to be held by a task.
    ///
    /// `JoinSet` wants `'static`, and `IgClient` is deliberately not `Clone`
    /// -- it owns the pacer and the cancel token, and two of it would be two
    /// budgets. One `Arc` is what lets several story downloads share the one
    /// client and its one CDN connection pool; nothing else needs this.
    pub fn client_shared(&self) -> Arc<IgClient> {
        Arc::clone(&self.client)
    }

    /// The three pieces a walk needs at once: it reads through the client and
    /// writes through the store while both are borrowed. Handing them out
    /// together is what lets the compiler see they are different fields.
    pub fn parts(&mut self) -> (&IgClient, &mut Store, &Progress) {
        (&self.client, &mut self.db, &self.progress)
    }

    pub fn db(&self) -> &Store {
        &self.db
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// The token the client's pacer watches, so canceling it reaches a request.
    pub fn cancel(&self) -> &CancelToken {
        self.client.pacer().cancel_token()
    }

    /// Until when this account may send nothing, and why. See [`held`].
    pub fn held(&self) -> Result<Option<Held>> {
        held(self.client.pacer(), self.app_paths.as_ref())
    }

    /// The account this run acts as. There is always one: an `App` cannot be
    /// built without a session.
    pub fn viewer(&self) -> &Viewer {
        &self.viewer
    }

    /// Whether the user has already agreed, in this run, to enumerate somebody
    /// else's account.
    ///
    /// It lives here rather than in the arguments because a crossing asks the
    /// engine for two lists and a summary for two more, and being asked the
    /// same question twice about the same account reads as the tool not having
    /// listened. Consent is a property of the run, so it belongs to the thing
    /// that *is* the run.
    ///
    /// Only a real answer sets it. The cooldown path never asks, so it can
    /// never vouch for one.
    ///
    /// **The account is part of the key, not just the answer**, as in the
    /// `resolved` memo: `run_accounts` walks every account one viewer reads
    /// through one `App`, on purpose, and a yes about @alice must not let @bob's
    /// lists be enumerated with no question printed.
    ///
    /// Compared case-insensitively, because Instagram treats two spellings that
    /// differ only in case as one account, and one `Option` is enough because
    /// the accounts of a run are ticked one after another.
    pub fn has_consent(&self, asked: &str) -> bool {
        self.consented
            .as_deref()
            .is_some_and(|given| given.eq_ignore_ascii_case(asked))
    }

    pub fn record_consent(&mut self, asked: &str) {
        self.consented = Some(asked.to_string());
    }

    /// How this run could have been given consent before it started.
    ///
    /// Read only by the refusal in `engine::ask_consent_with`, which is shared
    /// by every command that enumerates somebody else and therefore cannot know
    /// on its own which of the two answers applies.
    pub fn consent_in_advance(&self) -> ConsentInAdvance {
        self.consent_in_advance
    }

    /// Said by the monitor's two walking entry points, and nothing else.
    ///
    /// One process is one command, so this is a property of the run rather
    /// than of a call.
    pub fn consent_comes_from_the_config(&mut self) {
        self.consent_in_advance = ConsentInAdvance::WatchConfig;
    }

    /// Said by the monitor: a walk stops between two pages once `critical`
    /// says the battery is, rather than being cut off by the system
    /// hibernating under it, and the next run picks it up. A person's own
    /// command is not stopped for it; the monitor is, because nobody is there
    /// to decide (`power::battery`).
    pub fn stops_on_a_critical_battery(&mut self, critical: fn() -> bool) {
        self.stop_between_pages = Some(critical);
    }

    /// What a walk asks between two pages, if anything: see
    /// [`App::stops_on_a_critical_battery`].
    pub fn stop_between_pages(&self) -> Option<fn() -> bool> {
        self.stop_between_pages
    }

    /// What `asked` resolved to earlier in this run, if it was the same
    /// question.
    ///
    /// Compared raw, on the string that was given. Two spellings of one account
    /// simply miss the memo and resolve again, which costs a request; a memo
    /// handed to the wrong account costs correctness, and that is not a trade.
    ///
    /// Who the account is does not go stale inside one run: the id is stable,
    /// and a rename mid-run would not change which account was meant. So the
    /// identity comes back whatever has happened since.
    ///
    /// **The counters do go stale**, and they are handed back only while no
    /// new action has begun — which is to say, only while no time has passed
    /// that the account could have moved in. A walk takes minutes. Reusing a
    /// number read before it, to decide a stored list is still current, would
    /// serve a snapshot that missed everything those minutes contained and call
    /// it counter-verified: the exact shape of failure `Provenance` exists to
    /// stop. Cheaper is not worth wrong.
    ///
    /// One consequence worth naming: the private-account refusal in
    /// `target::resolve` runs once per run rather than once per list. That is
    /// fine — the first list already passed it, and the account cannot have
    /// become private in between in a way that matters — but it is a skip, not
    /// an oversight.
    pub fn resolved_target(&self, asked: Option<&str>) -> Option<target::Target> {
        let memo = self.resolved.as_ref()?;
        if memo.asked.as_deref() != asked {
            return None;
        }
        let mut target = memo.target.clone();
        if memo.action != self.client.pacer().actions() {
            target.counters = None;
        }
        Some(target)
    }

    /// Remembers what a question resolved to, stamped with the action it was
    /// resolved in.
    pub fn remember_target(&mut self, asked: Option<&str>, target: target::Target) {
        self.resolved = Some(Memo {
            asked: asked.map(str::to_string),
            target,
            action: self.client.pacer().actions(),
        });
    }

    /// Adds the counters a poll just obtained, and re-stamps.
    ///
    /// Re-stamping is the point. A memo resolved in an earlier action has had
    /// its counters dropped; the poll reads them anew, in this one, and the
    /// second list of a crossing must not poll all over again. What must
    /// invalidate them is an action begun **after** they were read — a walk,
    /// a sitting, a monitor tick — because that is time in which the account
    /// can have moved.
    pub fn remember_counters(&mut self, counters: target::Counters) {
        let action = self.client.pacer().actions();
        if let Some(memo) = &mut self.resolved {
            memo.target.counters = Some(counters);
            memo.action = action;
        }
    }

    /// A warning, through the progress bar when there is one so it does not
    /// land in the middle of a drawn line.
    pub fn warn(&self, text: &str) {
        self.progress.warn(text);
    }
}

#[cfg(test)]
mod tests {
    use super::{App, Viewer, target_label};
    use snob_core::Pk;

    /// An account's paths with another account's session in them is refused
    /// before anything is opened: its requests would be paid for, and its
    /// cooldowns recorded, in the wrong account's database.
    #[test]
    fn a_session_that_is_another_accounts_is_refused() {
        use snob_core::session::{Session, SessionOrigin};
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
        let secrets = snob_store::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-app-open-{}", std::process::id()));
        let other = paths.account(Pk::new(43));
        let store = secrets.session_of(&other);
        store
            .save(
                &Session::from_sessionid("42%3Aabc%3A1", "Mozilla/5.0", SessionOrigin::Paste)
                    .unwrap(),
            )
            .unwrap();

        let refused = App::open(&store, &other, false)
            .err()
            .expect("another account's session is refused");
        assert!(refused.to_string().contains("account 42"), "{refused}");
        assert!(!other.db_file().exists(), "nothing was opened");
        store.delete().unwrap();
    }

    /// Two of the targets this names come from `watch.toml`, which validates a
    /// username not at all, and one of those is the banner a scheduled service
    /// opens with — the first thing a monitor ever prints.
    #[test]
    fn naming_a_target_takes_out_what_a_terminal_would_obey() {
        let shown = target_label(Some("friend\u{1b}[2K"));
        assert!(!shown.contains('\x1b'), "{shown:?}");
        assert_eq!(shown, "@friend[2K");

        assert_eq!(target_label(None), "your account");
    }

    /// The name here came from Instagram — `whoami` writes it out of
    /// `resolve_username` — not from the command line, and it ends up as the
    /// progress bar's prefix, redrawn several times a second.
    ///
    /// What is left is the brackets as text, which is `printable`'s contract:
    /// it removes what a terminal obeys, not what it prints. Without the escape
    /// in front of them they are three characters in a name.
    #[test]
    fn a_viewer_label_does_not_carry_what_a_terminal_would_obey() {
        let viewer = Viewer {
            pk: Pk::new(42),
            username: Some(format!("me{esc}[2K{esc}[A", esc = '\x1b')),
        };

        let label = viewer.label();
        assert!(!label.contains('\x1b'), "{label:?}");
        assert_eq!(label, "@me[2K[A");
    }

    /// Without a name there is nothing to filter and the id stands in.
    #[test]
    fn an_unnamed_viewer_is_still_nameable() {
        let viewer = Viewer {
            pk: Pk::new(42),
            username: None,
        };

        assert_eq!(viewer.label(), "account 42");
    }

    /// An app over a fake that answers every request, so a test can spend
    /// requests without beginning an action.
    async fn app_spending_on(server: &wiremock::MockServer) -> App {
        use wiremock::{Mock, ResponseTemplate, matchers::any};
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(server)
            .await;
        let session = snob_core::session::Session::from_sessionid(
            "42%3Aabc%3A1",
            "Mozilla/5.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        let client = snob_ig::client::IgClient::new(session, snob_ig::pace::Pacer::unlimited())
            .unwrap()
            .with_base_url(url::Url::parse(&server.uri()).unwrap());
        App::for_test(
            client,
            snob_store::store::Store::in_memory().unwrap(),
            Viewer {
                pk: Pk::new(42),
                username: Some("me".into()),
            },
        )
    }

    /// Spends one request or more, whatever the fake answers: the requests
    /// that open a profile, or a poll, as far as the memo can tell.
    async fn spend(app: &App) {
        let before = app.client().pacer().spent();
        let _ = app.client().counters(Pk::new(7), Some("someone")).await;
        assert!(app.client().pacer().spent() > before, "a request was spent");
    }

    fn someone() -> crate::engine::target::Target {
        crate::engine::target::Target {
            pk: Pk::new(7),
            username: Some("someone".into()),
            is_self: false,
            counters: None,
        }
    }

    const COUNTERS: crate::engine::target::Counters = crate::engine::target::Counters {
        followers: Some(25),
        following: Some(11),
    };

    /// The counters a poll read survive the requests of the same action: the
    /// hover card that read them, and whatever else opening the profile asks.
    #[tokio::test]
    async fn the_counters_survive_the_requests_of_one_action() {
        let server = wiremock::MockServer::start().await;
        let mut app = app_spending_on(&server).await;

        spend(&app).await;
        app.remember_target(Some("someone"), someone());
        spend(&app).await;
        app.remember_counters(COUNTERS);
        spend(&app).await;

        let target = app
            .resolved_target(Some("someone"))
            .expect("the same question");
        assert_eq!(target.counters, Some(COUNTERS));
    }

    /// Once a walk has begun, the counters read before it are dropped; who
    /// the account is is not.
    #[tokio::test]
    async fn the_counters_drop_once_a_walk_has_begun() {
        let server = wiremock::MockServer::start().await;
        let mut app = app_spending_on(&server).await;

        app.remember_target(Some("someone"), someone());
        app.remember_counters(COUNTERS);
        app.client().pacer().begin_action();

        let target = app
            .resolved_target(Some("someone"))
            .expect("the identity outlives the action");
        assert_eq!(target.pk, Pk::new(7));
        assert_eq!(target.counters, None);

        // A poll in the new action is good for the rest of it.
        app.remember_counters(COUNTERS);
        spend(&app).await;
        assert_eq!(
            app.resolved_target(Some("someone")).unwrap().counters,
            Some(COUNTERS)
        );
    }

    /// The memo answers only the question it was resolved for, whatever the
    /// action.
    #[tokio::test]
    async fn a_different_question_never_reuses_the_memo() {
        let server = wiremock::MockServer::start().await;
        let mut app = app_spending_on(&server).await;

        app.remember_target(Some("someone"), someone());
        app.remember_counters(COUNTERS);

        assert!(app.resolved_target(None).is_none(), "your own account");
        assert!(app.resolved_target(Some("Someone")).is_none());
        assert!(app.resolved_target(Some("another")).is_none());
    }
}
