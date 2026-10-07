//! One run of the monitor: the accounts it covers, each read as its viewer,
//! and the things that happen once around them.
//!
//! Both modes come through here — `super::scheduled` on a timer and
//! `super::once` by hand — so AGENTS.md's rule that the queue is drained once
//! per run, after every account, is one rule in one place rather than two
//! copies that drift.

use anyhow::Result;
use snob_core::model::printable;
use snob_core::{Epoch, Pk};
use snob_ig::pace::CancelToken;
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::SecretStore;
use snob_store::store::Store;
use snob_store::store::shared::Shared;

use crate::app::{App, Viewer};
use crate::engine::watch::{TickReport, Watched, group_by_viewer};
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui;

use super::delivery::{Delivery, deliver, drain};
use super::say::{describe, refusal_line, say_what_was_given_up};
use super::wire::{failed_tick_json, json_line, tick_json};

/// One run: each viewer's entries read as that viewer, then what every
/// account owes sent, then every account's database settled.
///
/// A fresh `App` per viewer and per run rather than one held open for weeks.
/// It picks up a session that was replaced and a refreshed User-Agent, and —
/// the reason that matters on Windows — it holds no SQLite connection while
/// the loop sleeps, so `snob purge` in another terminal is not blocked by a
/// file this has open.
pub(super) async fn run_viewers(
    paths: &AppPaths,
    secrets: &SecretStore,
    watched: &[Watched],
    delivery: Option<&Delivery>,
    printing: Printing,
    with_progress: bool,
) -> RunSummary {
    let cancel = crate::interrupt::install();
    run_groups(paths, watched, delivery, printing, &cancel, |account| {
        let opened = App::open(&secrets.session_of(account), account, with_progress)?;
        Ok(opened.map(|mut app| {
            app.stops_on_a_critical_battery(crate::power::battery::critical);
            Box::new(app)
        }))
    })
    .await
}

/// Whether the run that is due waits for power: the battery is critical, so
/// the system is about to hibernate or shut down under a run started now
/// (`power::battery`). Written down in `shared.db` the first time, so `snob
/// watch status` says what the monitor is doing instead of calling it late.
pub(super) fn waits_for_power(paths: &AppPaths, now: Epoch) -> bool {
    if !crate::power::battery::critical() {
        return false;
    }
    match Shared::open(paths) {
        Ok(shared) => {
            if let Err(e) = shared.wait_for_power(now) {
                tracing::debug!(error = %e, "could not write down that the run waits for power");
            }
        }
        Err(e) => tracing::debug!(error = %e, "could not open shared.db"),
    }
    true
}

/// The run that waited for power is running: `snob watch status` stops
/// saying it waits.
pub(super) fn power_is_back(paths: &AppPaths) {
    match Shared::open(paths).and_then(|shared| shared.power_is_back()) {
        Ok(()) => {}
        Err(e) => tracing::debug!(error = %e, "could not write down that power is back"),
    }
}

/// [`run_viewers`] with the opening handed in, so a test can open an `App`
/// against a mock server for one viewer and none for another.
async fn run_groups(
    paths: &AppPaths,
    watched: &[Watched],
    delivery: Option<&Delivery>,
    printing: Printing,
    cancel: &CancelToken,
    mut open: impl FnMut(&AccountPaths) -> Result<Option<Box<App>>>,
) -> RunSummary {
    let mut tally = Tally::default();
    // Only to name the viewers a run could not read as.
    let registry = Registry::load(paths).unwrap_or_default();

    for (viewer, entries) in group_by_viewer(watched) {
        // Ctrl+C stops the run rather than only the viewer it landed in.
        if cancel.is_canceled() {
            break;
        }
        let account = viewer.map(|pk| paths.account(pk));
        let opened = match &account {
            Some(account) => open(account),
            // Nobody is signed in to read these as.
            None => Ok(None),
        };
        let reader = viewer.map(|pk| Viewer {
            pk,
            username: crate::account::username(&registry, pk).map(str::to_string),
        });
        let as_whom = reader.as_ref().map(Viewer::label);
        let why = match opened {
            // A viewer in a cooldown or under the brake is read like any
            // other: the engine's own gates refuse each list and spend
            // nothing, and the tick still says so, records its row and sends
            // its heartbeat.
            Ok(Some(mut app)) => {
                // The monitor takes an answer in advance from `watch.toml`,
                // never from a flag, so the refusal when nobody is at a
                // terminal has to say so.
                app.consent_comes_from_the_config();
                run_accounts(&mut app, &entries, delivery, printing, &mut tally).await;
                continue;
            }
            Ok(None) => no_session_for(viewer, &registry, as_whom.as_deref(), &entries),
            Err(e) => e,
        };
        skip(
            account.as_ref(),
            reader.as_ref(),
            &entries,
            why,
            printing,
            &mut tally,
        );
    }

    // Once, after every viewer, and whatever the viewers did — including when
    // none had news of its own, which is the common case. Rows left aging past
    // `MAX_AGE_SECS` are never handed back by `due`, and nothing expires them
    // but a failed attempt.
    //
    // Not after a cancellation: the queue is `DRAIN_LIMIT` POSTs of up to
    // thirty seconds each per account, and nothing is lost by leaving it —
    // what is owed stays owed, and the next run drains it.
    if let Some(delivery) = delivery
        && !cancel.is_canceled()
    {
        drain_all(paths, cancel, delivery).await;
    }
    settle_all(paths);

    tally.summary(printing)
}

/// Every account with a database: the registry's, and any directory left
/// behind. One that never had a database gets none made for it here.
pub(super) fn account_databases(paths: &AppPaths) -> Vec<AccountPaths> {
    Registry::known_accounts(paths)
        .into_iter()
        .map(|pk| paths.account(pk))
        .filter(|account| account.db_file().exists())
        .collect()
}

/// Sends what every account owes, each account's queue once.
async fn drain_all(paths: &AppPaths, cancel: &CancelToken, delivery: &Delivery) {
    for account in account_databases(paths) {
        if cancel.is_canceled() {
            return;
        }
        match Store::open_existing(&account) {
            Ok(Some(db)) => drain(&db, cancel, delivery).await,
            Ok(None) => {}
            Err(e) => ui::warn(&format!(
                "could not read what account {} owes: {e}",
                account.pk()
            )),
        }
    }
}

/// Retention for every account's database, with no session needed.
///
/// Every door a run can close early closes after this or before a session is
/// asked for, so it is what keeps owed reports, old captures and the run log
/// expiring while a session is gone or an address is refused.
pub(super) fn settle_all(paths: &AppPaths) {
    let now = snob_core::clock::now();
    say_what_was_given_up(
        account_databases(paths)
            .iter()
            .map(|account| crate::engine::watch::settle_account(account, now))
            .sum(),
    );
}

/// The refusal for a viewer with no session, or for entries nobody is signed
/// in to read as.
fn no_session_for(
    viewer: Option<Pk>,
    registry: &Registry,
    as_whom: Option<&str>,
    entries: &[Watched],
) -> anyhow::Error {
    let hint = match viewer {
        Some(pk) => format!(
            "run \"snob login --account {}\"",
            crate::account::username(registry, pk).map_or_else(|| pk.to_string(), printable)
        ),
        None => "run \"snob login\"".to_string(),
    };
    let why = if viewer.is_some() {
        "no session is stored"
    } else {
        "nobody is signed in"
    };
    skipped_because(as_whom, why, entries)
        .with_hint(hint)
        .into()
}

/// "@viewer: why, so @a and @b are skipped this run", as a missing session.
fn skipped_because(as_whom: Option<&str>, why: &str, entries: &[Watched]) -> ExitError {
    let names: Vec<String> = entries
        .iter()
        .map(|entry| match (entry.name(), as_whom) {
            (Some(name), _) => format!("@{}", printable(name)),
            (None, Some(viewer)) => viewer.to_string(),
            (None, None) => "your account".to_string(),
        })
        .collect();
    let verb = if names.len() > 1 { "are" } else { "is" };
    let named = crate::report::and_list(&names).unwrap_or_else(|| "nothing".to_string());
    let message = match as_whom {
        Some(viewer) => format!("{viewer}: {why}, so {named} {verb} skipped this run"),
        None => format!("{why}, so {named} {verb} skipped this run"),
    };
    ExitError::new(ExitCode::NoSession, message)
}

/// What a group that was not read leaves behind: a failed run per entry in
/// its viewer's run log, a line per entry in the stream, and the reason
/// among the run's failures.
fn skip(
    account: Option<&AccountPaths>,
    viewer: Option<&Viewer>,
    entries: &[Watched],
    why: anyhow::Error,
    printing: Printing,
    tally: &mut Tally,
) {
    let at = snob_core::clock::now();
    let db = account.and_then(|account| match Store::open_existing(account) {
        Ok(db) => db.map(|db| (db, account.pk())),
        Err(e) => {
            tracing::warn!(error = %e, "the skipped runs could not be recorded");
            None
        }
    });
    for entry in entries {
        if let Some((db, viewer)) = &db {
            record_failed_run(db, *viewer, entry, &why, 0, at);
        }
        if printing.json {
            crate::ui::say!(
                "{}",
                json_line(&failed_tick_json(viewer, entry, &why, at, 0))
            );
        }
    }
    tally.failures.push((viewer.cloned(), why));
}

/// How much a run says out loud about a tick.
///
/// The two modes differ here and nowhere else. A scheduled service writes one
/// line per run down a pipe and stays quiet when there is no news, which is
/// what makes `snob watch >> events.ndjson` and "silence means nothing changed"
/// both true. `once` is looked at while it runs, so it says so either way and
/// lays its JSON out to be read rather than appended to.
#[derive(Clone, Copy)]
pub(super) struct Printing {
    json: bool,
    watching: bool,
}

impl Printing {
    /// How a failure is told, decided by the same flag as the result.
    pub(super) fn wording(self) -> report::Wording {
        if self.json {
            report::Wording::Json
        } else {
            report::Wording::Prose
        }
    }

    pub(super) fn unattended(json: bool) -> Self {
        Self {
            json,
            watching: false,
        }
    }

    pub(super) fn watched(json: bool) -> Self {
        Self {
            json,
            watching: true,
        }
    }
}

/// What a run over several accounts came to.
///
/// Not `snob_core::watch::RunOutcome`, which is what *one* run of *one* account
/// came to and is the vocabulary `watch_runs.outcome` is kept in. This is the
/// summary of a whole pass over the file: the requests it spent, the first
/// reason any of its ticks gave, and the one failure nobody has printed yet.
pub(super) struct RunSummary {
    /// Requests spent by every account that got as far as spending any.
    pub(super) spent: u32,
    /// The first non-`Ok` verdict a tick reported, the accounts being in the
    /// order the file names them. [`first_reason`] is where the choice between
    /// first and worst is made, and why there is no worst to choose.
    pub(super) code: ExitCode,
    /// The last failure, unprinted. Every earlier one has already been printed,
    /// for the reason [`to_print_and_to_return`] gives.
    pub(super) failed: Option<anyhow::Error>,
}

impl RunSummary {
    /// The run as a result: its unprinted failure, if it had one.
    pub(super) fn into_result(self) -> Result<()> {
        match self.failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// A run's summary while it is being added up, across every viewer.
struct Tally {
    spent: u32,
    code: ExitCode,
    /// Each with the account it was read as, for a failure told in JSON to
    /// name: several are opened in one run, so the one acting last is not it.
    failures: Vec<(Option<Viewer>, anyhow::Error)>,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            spent: 0,
            code: ExitCode::Ok,
            failures: Vec::new(),
        }
    }
}

impl Tally {
    /// Prints every failure but the last, which the summary carries, and
    /// leaves the account the last was read as for its caller's print to name.
    fn summary(self, printing: Printing) -> RunSummary {
        let (print_here, failed) = to_print_and_to_return(self.failures);
        for (viewer, earlier) in print_here {
            report::print_error_as(&earlier, printing.wording(), viewer.as_ref());
        }
        let failed = failed.map(|(viewer, e)| {
            report::set_acting_as(viewer);
            e
        });
        RunSummary {
            spent: self.spent,
            code: self.code,
            failed,
        }
    }
}

/// Every account one viewer watches, through that viewer's `App`.
///
/// One `App` for all of them: they share a session and a request budget. A
/// failure on one account does not stop the rest — a private account somebody
/// stopped being allowed to read must not silence the monitor's own.
///
/// `self` in `watched` is whoever `app` is, so this is handed only the entries
/// read as that viewer.
async fn run_accounts(
    app: &mut crate::app::App,
    watched: &[Watched],
    delivery: Option<&Delivery>,
    printing: Printing,
    tally: &mut Tally,
) {
    for account in watched {
        // Ctrl+C stops the run rather than only the account it landed in, not
        // walking every remaining account after the user asked it to stop.
        if app.cancel().is_canceled() {
            break;
        }
        // Read around the tick rather than out of the report it returns.
        //
        // `TickReport.requests` is worked out at the *end* of `tick`, so any
        // `?` on the way out throws away a measurement the pacer has already
        // been charged for — and `tick_one` also `?`s on serializing the body
        // and on `deliver`, both of them after a whole successful two-list
        // walk. What was really spent has its own row in AGENTS.md.
        let before = app.client().pacer().spent();
        let outcome = tick_one(app, account, delivery, printing).await;
        let charged = app.client().pacer().spent().saturating_sub(before);
        tally.spent += charged;

        match outcome {
            Ok(tick) => tally.code = first_reason(tally.code, tick.outcome()),
            Err(e) => {
                // A tick that failed is still a tick that happened, and the
                // run log says "one row per tick, including the ticks that
                // found nothing": without this row `status` goes on printing
                // the last good run, and `prune` keeps that newest row for
                // ever.
                //
                // An account whose *first* tick fails still records nothing:
                // `watch_runs.account_pk` references `accounts(pk)`, and
                // nothing has put a row there yet.
                //
                // The moment is read once and given to both, so the row in the
                // run log and the line in the stream name the same second.
                let at = snob_core::clock::now();
                record_failed_run(app.db(), app.viewer().pk, account, &e, charged, at);
                if printing.json {
                    let line = failed_tick_json(Some(app.viewer()), account, &e, at, charged);
                    crate::ui::say!("{}", json_line(&line));
                }
                tally.failures.push((Some(app.viewer().clone()), e));
            }
        }
    }
}

/// Writes the row a failed tick owes the run log of `viewer`, the account
/// `db` belongs to.
///
/// Best-effort, like `commit`'s own recording: a run log that cannot be written
/// is worth a line in the journal and is not a reason to turn a failure into a
/// different failure.
///
/// The account has to be resolved locally, because a tick that failed at
/// `target::resolve` never learned an id. An account that has never been seen
/// has no `accounts` row for the foreign key to point at, so nothing is written
/// — which is exactly the case the comment at the call site names.
fn record_failed_run(
    db: &Store,
    viewer: Pk,
    watched: &Watched,
    error: &anyhow::Error,
    requests: u32,
    at: Epoch,
) {
    let pk = match watched.name() {
        Some(name) => snob_store::store::accounts::find_pk_by_username(
            db.conn(),
            snob_core::model::printable(name).trim(),
        )
        .ok()
        .flatten(),
        None => Some(viewer),
    };
    let Some(account_pk) = pk else {
        return;
    };

    let outcome = crate::exit::from_chain(error).unwrap_or(ExitCode::Error);
    let record = snob_store::store::watch::record_run(
        db.conn(),
        &snob_store::store::watch::Run {
            account_pk,
            started_at: at,
            finished_at: Some(at),
            requests,
            outcome: Some(outcome.into()),
            changes: 0,
        },
    );
    if let Err(e) = record {
        tracing::warn!(error = %e, "the failed run could not be recorded");
    }
}

/// Splits a run's failures into the ones it prints and the one it hands back.
///
/// Only one can be returned, and whoever gets it prints it — `scheduled` so the
/// service keeps going, `main` so `once` exits with the right code. So the rest
/// are printed here, in the order they happened, and each failure reaches the
/// journal exactly once.
///
/// Printing every failure here *and* returning one would write the whole
/// `error:` / `caused by:` / `hint:` block twice per failing run, and returning
/// `Ok` to avoid that would hide that an account had failed.
fn to_print_and_to_return<T>(failures: Vec<T>) -> (Vec<T>, Option<T>) {
    let mut failures = failures.into_iter();
    let last = failures.next_back();
    (failures.collect(), last)
}

/// The verdict a run answers with, folded over the ticks that succeeded.
///
/// **The first non-`Ok` one, not the worst.** `ExitCode` is a handful of
/// independent reasons rather than a severity scale: it derives no `Ord`, and
/// its numbers are a shell convention — 130 is 128 plus SIGINT — so "the worst"
/// is not something this could compute. Deriving `Ord` and taking the maximum
/// would put `Interrupted` above `NoSession`: a run somebody stopped would
/// outrank a session that has gone, in the value `once` hands back as its
/// process exit code.
///
/// So the rule is the order the accounts are already in, and the earliest reason
/// wins. A tick that failed outright is not here at all — it leaves through
/// `RunSummary::failed`, and `once` returns this only when nothing failed.
fn first_reason(so_far: ExitCode, tick: ExitCode) -> ExitCode {
    match so_far {
        ExitCode::Ok => tick,
        earlier => earlier,
    }
}

/// One account, inside a run that may cover several.
async fn tick_one(
    app: &mut crate::app::App,
    watched: &Watched,
    delivery: Option<&Delivery>,
    printing: Printing,
) -> Result<TickReport> {
    let tick = crate::engine::watch::tick(app, watched).await;
    app.progress().finish();
    let tick = tick?;

    if printing.json {
        crate::ui::say!("{}", json_line(&tick_json(&tick)));
    } else if printing.watching || tick.report.has_changes() {
        for line in describe(&tick.report, tick.lists.iter().any(|l| l.skipped.is_some())) {
            crate::ui::say!("{line}");
        }
    }

    warn_about_refusals(&tick);

    deliver(app, &tick, delivery).await?;
    Ok(tick)
}

/// Says out loud which lists this run could not look at, and why.
///
/// To standard error, so it does not land in the middle of a report something
/// else is parsing — and said even in JSON, where a caller reading `looked`
/// would otherwise have to guess why.
fn warn_about_refusals(tick: &TickReport) {
    for (kind, skipped) in tick
        .lists
        .iter()
        .filter_map(|l| l.skipped.map(|s| (l.kind, s)))
    {
        ui::warn(&refusal_line(kind, skipped));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::{
        app_on, app_posting_to, delivery_to, known_account, owed_long_ago,
    };
    use std::cell::RefCell;

    use snob_store::store::deliveries;

    /// The skipped entries are named as a sentence would, and the verb agrees.
    #[test]
    fn the_skipped_entries_are_named_and_the_verb_agrees() {
        let friend = Watched::consented("friend".into(), crate::engine::watch::Consent);
        let one = skipped_because(Some("@me"), "no session", &[Watched::own()]);
        assert_eq!(
            one.to_string(),
            "@me: no session, so @me is skipped this run"
        );
        let two = skipped_because(None, "no session", &[Watched::own(), friend]);
        assert_eq!(
            two.to_string(),
            "no session, so your account and @friend are skipped this run"
        );
    }

    /// A tick that failed is still a tick that happened, and it still spent.
    ///
    /// `record_run` is reached only through `commit`, the last statement of a
    /// *successful* `tick_one`, so an `Err` out of `tick` needs its own row, or
    /// `status` would go on printing the last good run and `health` a stale
    /// `ok`.
    ///
    /// The count comes from the pacer around the tick rather than out of the
    /// report, because `TickReport.requests` is worked out at the end of `tick`
    /// and any `?` on the way out throws it away — after the pacer has already
    /// been charged.
    #[tokio::test]
    async fn a_run_that_failed_is_recorded_and_counts_what_it_spent() {
        let server = wiremock::MockServer::start().await;
        // The profile poll is refused, so `target::resolve` fails and the `?`
        // carries out of `tick` — one request spent, no report, no commit.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let (mut app, _delivery) = app_posting_to(&server);
        // The account has to be known locally, or the foreign key has nothing
        // to point at — which is its own gap, written down at the call site.
        known_account(app.db(), Pk::new(99), "friend", false);

        let watched = [Watched::consented(
            "friend".into(),
            crate::engine::watch::Consent,
        )];
        let mut outcome = Tally::default();
        run_accounts(
            &mut app,
            &watched,
            None,
            Printing::unattended(false),
            &mut outcome,
        )
        .await;

        assert!(
            !outcome.failures.is_empty(),
            "the account could not be resolved"
        );
        assert!(
            outcome.spent >= 1,
            "the poll was charged, so the run has to say it spent it: {}",
            outcome.spent
        );

        let runs = snob_store::store::watch::last_runs(app.db().conn()).unwrap();
        let recorded = runs
            .iter()
            .find(|r| r.account_pk == Pk::new(99))
            .expect("a tick that failed is a tick that happened");
        assert_ne!(
            recorded.outcome,
            Some(ExitCode::Ok.into()),
            "and it must not read as a run that worked"
        );
        assert!(
            recorded.requests >= 1,
            "the poll was spent and charged, so it has to be counted: {recorded:?}"
        );
    }

    /// Each failure reaches the journal once, and the run still carries one.
    #[test]
    fn every_failure_is_printed_once_and_the_run_still_carries_one() {
        let (printed, returned) = to_print_and_to_return(vec![
            anyhow::anyhow!("first"),
            anyhow::anyhow!("second"),
            anyhow::anyhow!("third"),
        ]);
        assert_eq!(
            printed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["first", "second"],
            "in the order they happened"
        );
        assert_eq!(returned.unwrap().to_string(), "third");

        // One watched account is the default: nothing prints here, so the
        // caller's print is the only one.
        let (printed, returned) = to_print_and_to_return(vec![anyhow::anyhow!("only")]);
        assert!(printed.is_empty());
        assert_eq!(returned.unwrap().to_string(), "only");

        let (printed, returned) = to_print_and_to_return(Vec::<anyhow::Error>::new());
        assert!(printed.is_empty() && returned.is_none());
    }

    /// The run's code is the first reason, and there is no worst to take:
    /// ordering `ExitCode` by number would put `Interrupted` (130) above
    /// `NoSession` (3), and a run somebody stopped would outrank a session that
    /// has gone in what `once` hands back as its process exit code.
    #[test]
    fn the_run_answers_with_the_first_reason_not_the_worst() {
        assert_eq!(
            first_reason(ExitCode::Ok, ExitCode::RateLimited),
            ExitCode::RateLimited,
            "the first account with something to say is what the run says"
        );
        assert_eq!(
            first_reason(ExitCode::RateLimited, ExitCode::NoSession),
            ExitCode::RateLimited,
            "a later account does not overwrite an earlier reason"
        );
        assert_eq!(
            first_reason(ExitCode::Interrupted, ExitCode::Error),
            ExitCode::Interrupted,
            "and not by being higher or lower than it"
        );
        assert_eq!(
            first_reason(ExitCode::Ok, ExitCode::Ok),
            ExitCode::Ok,
            "a run where every account was fine is fine"
        );
    }

    /// A run with no session still settles the queue.
    ///
    /// A monitor whose session was logged out, or whose keyring is locked,
    /// would otherwise expire nothing for as long as that lasts: not the owed
    /// reports, not old captures, not the run log.
    #[tokio::test]
    async fn a_run_with_no_session_still_settles_the_queue() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let account = paths.account(Pk::new(42));
        let now = snob_core::clock::now();
        let id = owed_long_ago(&account, now);

        let secrets = SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-settle-{}", std::process::id()));
        let watched = [Watched::own().read_as(Some(Pk::new(42)))];

        run_viewers(
            &paths,
            &secrets,
            &watched,
            None,
            Printing::unattended(false),
            false,
        )
        .await;

        let db = Store::open(&account).unwrap();
        assert_eq!(
            deliveries::state(db.conn(), id).unwrap().as_deref(),
            Some("expired"),
            "the run had no session, and retention still has to happen"
        );
    }

    /// An account's database, with the account itself known in it.
    fn database(paths: &AppPaths, pk: Pk, name: &str) -> Store {
        let db = Store::open(&paths.account(pk)).unwrap();
        known_account(&db, pk, name, true);
        db
    }

    /// One viewer without a session skips its own entries and no others.
    ///
    /// Each group opens its own viewer's session, so a viewer that was logged
    /// out must cost the others nothing — and its entries must still leave a
    /// row in its run log, or `status` goes on reporting its last good run.
    #[tokio::test]
    async fn a_viewer_without_a_session_skips_only_its_own_entries() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let (signed_in, logged_out) = (Pk::new(42), Pk::new(43));
        drop(database(&paths, signed_in, "me"));
        drop(database(&paths, logged_out, "other"));

        let watched = [
            Watched::own().read_as(Some(signed_in)),
            Watched::own().read_as(Some(logged_out)),
        ];
        let opened = RefCell::new(Vec::new());
        let summary = run_groups(
            &paths,
            &watched,
            None,
            Printing::unattended(false),
            &CancelToken::default(),
            |account| {
                opened.borrow_mut().push(account.pk());
                Ok(if account.pk() == signed_in {
                    Some(Box::new(app_on(Store::open(account).unwrap(), &server)))
                } else {
                    None
                })
            },
        )
        .await;

        assert_eq!(*opened.borrow(), [signed_in, logged_out]);
        assert!(
            !server.received_requests().await.unwrap().is_empty(),
            "the viewer with a session was still read"
        );
        let failed = summary.failed.expect("the skipped viewer is a failure");
        assert_eq!(crate::exit::from_chain(&failed), Some(ExitCode::NoSession));
        assert!(failed.to_string().contains("skipped"), "{failed}");

        let outcomes = |pk: Pk| -> Vec<_> {
            let db = Store::open(&paths.account(pk)).unwrap();
            snob_store::store::watch::last_runs(db.conn())
                .unwrap()
                .into_iter()
                .map(|run| (run.account_pk, run.outcome))
                .collect()
        };
        assert_eq!(
            outcomes(logged_out),
            [(logged_out, Some(ExitCode::NoSession.into()))],
            "the skip is recorded where the viewer's runs are"
        );
        assert!(
            outcomes(signed_in)
                .iter()
                .all(|(_, outcome)| *outcome != Some(ExitCode::NoSession.into())),
            "the viewer with a session was not skipped"
        );
    }

    /// Every account's queue goes out once a run, whoever it watched.
    ///
    /// A report is owed by the account whose run made it, and each account
    /// keeps its own queue, so the drain has to walk every database — once,
    /// even when the registry and the directories both name an account.
    #[tokio::test]
    async fn the_drain_covers_every_account_once() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let delivery = delivery_to(&server);
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let now = snob_core::clock::now();
        Registry::update(&paths, |registry| {
            registry.upsert(Pk::new(42), "me", now);
        })
        .unwrap();
        for (pk, name) in [(Pk::new(42), "me"), (Pk::new(43), "other")] {
            let db = database(&paths, pk, name);
            deliveries::enqueue(
                db.conn(),
                &format!("run-{pk}"),
                pk,
                r#"{"schema":1,"event":"watch.changes"}"#,
                now,
                &delivery.destination,
            )
            .unwrap();
        }

        run_groups(
            &paths,
            &[],
            Some(&delivery),
            Printing::unattended(false),
            &CancelToken::default(),
            |_| unreachable!("nothing is watched"),
        )
        .await;

        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "one report owed by each account, each sent once"
        );
        for pk in [Pk::new(42), Pk::new(43)] {
            let db = Store::open(&paths.account(pk)).unwrap();
            assert_eq!(deliveries::pending(db.conn()).unwrap(), 0);
        }
    }

    /// Ctrl+C stops the run before the next viewer, and before the drain.
    #[tokio::test]
    async fn a_canceled_run_opens_no_further_viewer() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let delivery = delivery_to(&server);
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let (first, second) = (Pk::new(43), Pk::new(44));
        let db = database(&paths, first, "first");
        deliveries::enqueue(
            db.conn(),
            "run-owed",
            first,
            "{}",
            snob_core::clock::now(),
            &delivery.destination,
        )
        .unwrap();
        drop(db);
        drop(database(&paths, second, "second"));

        let watched = [
            Watched::own().read_as(Some(first)),
            Watched::own().read_as(Some(second)),
        ];
        let cancel = CancelToken::default();
        let opened = RefCell::new(Vec::new());
        run_groups(
            &paths,
            &watched,
            Some(&delivery),
            Printing::unattended(false),
            &cancel,
            |account| {
                opened.borrow_mut().push(account.pk());
                cancel.cancel();
                Ok(None)
            },
        )
        .await;

        assert_eq!(
            *opened.borrow(),
            [first],
            "the second viewer was not opened"
        );
        let db = Store::open(&paths.account(second)).unwrap();
        assert!(
            snob_store::store::watch::last_runs(db.conn())
                .unwrap()
                .is_empty()
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "what is owed stays owed, for the next run"
        );
    }
}
