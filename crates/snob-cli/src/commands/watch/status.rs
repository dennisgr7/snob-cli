//! `snob watch status`: what is configured, and whether it is working.
//!
//! The other half of not having to remember flags. `super::setup` writes the
//! file; this reads it back alongside what the runs actually did, and answers
//! the one question a scheduled thing cannot answer for itself — a monitor that
//! found nothing and a monitor that quietly stopped look identical from outside.
//!
//! `health` is where that answer is decided, in `status`'s output **and** in its
//! exit code, so a machine and a person are told the same thing.

use std::collections::BTreeMap;

use anyhow::Result;
use snob_core::model::{ListKind, printable};
use snob_core::watch::{RunOutcome, schedule};
use snob_core::{Epoch, Pk};
use snob_store::config::{self, WatchConfig};
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::store::{Store, deliveries, watch as watch_store};

use crate::app::Viewer;
use crate::cli::WatchStatusArgs;
use crate::engine::check::Verdict;
use crate::engine::watch::Watched;
use crate::exit::ExitCode;
use crate::report;

use super::schedule::schedule_from;
use super::watched::watched_from;

/// Reports what is configured and what has happened.
pub fn status(
    args: WatchStatusArgs,
    paths: &AppPaths,
    account: Option<&AccountPaths>,
) -> Result<ExitCode> {
    let config = config::load(paths)?;
    let recorded = recorded(paths, config.as_ref(), account.map(|a| a.pk()))?;
    let owed = recorded.owed;

    let health = health(
        config.as_ref(),
        &recorded.runs,
        &recorded.reported,
        owed,
        snob_core::clock::now(),
        waiting_for_power(paths),
    );

    if args.output.json {
        let document = super::wire::status_json(
            config.is_some(),
            &config::path(paths),
            &owed,
            &health,
            &recorded.runs,
            &recorded.marks,
        );
        crate::ui::say!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(health.verdict.exit_code());
    }

    match &config {
        Some(config) => {
            crate::ui::say!("Configured in {}", config::path(paths).display());
            for line in describe_config(config) {
                crate::ui::say!("  {line}");
            }
        }
        None => crate::ui::say!(
            "Nothing is configured. Run \"snob watch setup\", or pass the schedule on the \
             command line."
        ),
    }

    for line in recorded.lines() {
        crate::ui::say!("{line}");
    }

    // Two lines, about two different sets of rows: `pending` is every row in the
    // state, while only the ones addressed here are ever handed back. Each line
    // is true of the rows it counts, and which is which is exactly what the
    // reader needs.
    if owed.waiting > 0 || owed.elsewhere > 0 || owed.given_up > 0 {
        crate::ui::say!();
    }
    if owed.given_up > 0 {
        // `say::given_up_sentence`, so the run's warning and this line cannot
        // drift apart over the one event they both describe.
        crate::ui::say!("{}", super::say::given_up_sentence(owed.given_up));
    }
    if owed.waiting > 0 {
        let (subject, it) = if owed.waiting == 1 {
            ("report is", "it")
        } else {
            ("reports are", "them")
        };
        crate::ui::say!(
            "{} {subject} waiting to be delivered; the next run tries {it}.",
            owed.waiting
        );
    }
    if owed.elsewhere > 0 {
        // The verb is in the tuple with everything else it has to agree with:
        // exactly one report left over after the webhook address moved is the
        // ordinary case.
        let (subject, they, expire, their, it) = if owed.elsewhere == 1 {
            ("report is", "It", "expires", "its", "it")
        } else {
            ("reports are", "They", "expire", "their", "them")
        };
        crate::ui::say!(
            "{} {subject} addressed to a webhook this configuration does not send to, so \
             nothing here will try {it}. {they} {expire} on {their} own.",
            owed.elsewhere
        );
    }

    if !health.notes.is_empty() {
        crate::ui::say!();
        crate::ui::say!("Health: {}", health.verdict.as_str());
        for note in &health.notes {
            crate::ui::say!("  {note}");
        }
    }

    // Non-zero when the monitor is not doing what it was configured to do, so
    // this is usable as a probe rather than only as something to read.
    Ok(health.verdict.exit_code())
}

/// Whether the monitor is doing what it was configured to do.
///
/// The verdict, made of what `status` already reads, so a monitor that stopped
/// working does not have to be read to be known — the same problem the reports
/// themselves have and which `--json` exists to solve.
///
/// Pure, and separate from the printing, because the interesting part is which
/// states count as broken and that is a thing worth pinning.
pub(super) struct Health {
    pub(super) verdict: Verdict,
    pub(super) notes: Vec<String>,
}

/// The accounts the configuration has `viewer` watch right now, as ids, with
/// `db` being `viewer`'s database.
///
/// `None` means it could not be settled, and every run is then treated as
/// watched — the direction that does not fail a probe on a guess. That happens
/// with no configuration at all, and when an entry is read as the account in
/// use and no account is signed in. `self` is the account its entry is read
/// as, and a name is looked up where that account's runs recorded it.
///
/// `watched` must come from `watched_from`, so this and a run agree on which
/// accounts the file names.
fn watched_pks(db: &Store, watched: Option<&[Watched]>, viewer: Pk) -> Result<Option<Vec<Pk>>> {
    let Some(watched) = watched else {
        return Ok(None);
    };
    let mut pks = Vec::new();
    for entry in watched {
        match (entry.viewer(), entry.name()) {
            // Read as the account in use, and nobody is signed in.
            (None, _) => return Ok(None),
            (Some(other), _) if other != viewer => {}
            (Some(_), None) => pks.push(viewer),
            (Some(_), Some(named)) => {
                if let Some(pk) =
                    snob_store::store::accounts::find_pk_by_username(db.conn(), named)?
                {
                    pks.push(pk);
                }
            }
        }
    }
    Ok(Some(pks))
}

/// One account's newest run, with the two things [`health`] cannot work out for
/// itself: what to call the account, and whether the file still names it.
///
/// Built by [`recorded`], which has the store open and is already resolving
/// both for its own lines.
pub(super) struct RunOf {
    pub(super) run: watch_store::Run,
    /// `@name`, or the id when the name was never learned.
    who: String,
    /// Whether `watch.toml` still lists this account. `true` when it cannot be
    /// settled, which is the direction that does not fail a probe on a guess.
    watched: bool,
    /// The account whose database recorded it: the one the run read as.
    pub(super) viewer: Viewer,
}

/// One receipt, with what to call its account and whose database holds it.
pub(super) struct MarkOf {
    pub(super) mark: watch_store::AccountMark,
    who: String,
    pub(super) viewer: Viewer,
}

/// What every account's database says the monitor did.
///
/// **Every database, not the account in use's.** A run reads each entry as its
/// viewer and records it in that viewer's database, so the answer to "is it
/// working" is spread over all of them: this is one of the listed places that
/// open another account's database, and it only reads.
#[derive(Default)]
struct Recorded {
    /// The accounts read, in the order their databases were.
    viewers: Vec<Viewer>,
    /// What every account owes, added up.
    owed: deliveries::Owed,
    runs: Vec<RunOf>,
    marks: Vec<MarkOf>,
    reported: Vec<HalfRead>,
}

/// Reads what every account with a database recorded. None is created for an
/// account that never had one.
fn recorded(
    paths: &AppPaths,
    config: Option<&WatchConfig>,
    in_use: Option<Pk>,
) -> Result<Recorded> {
    // The address this configuration could post to, spelled the way the outbox
    // records it — `status` builds no delivery of its own, so it goes through
    // the same function `delivery_from` does rather than comparing the file's
    // raw string against a normalized URL.
    let destination = config
        .and_then(|c| c.webhook.as_ref())
        .and_then(|w| url::Url::parse(&w.url).ok())
        .map(|url| super::delivery::destination_of(&url));
    let watched = config.map(|config| watched_from(None, Some(config), in_use));
    let registry = Registry::load(paths).unwrap_or_default();

    let mut recorded = Recorded::default();
    for account in super::run::account_databases(paths) {
        let viewer = Viewer {
            pk: account.pk(),
            username: crate::account::username(&registry, account.pk()).map(str::to_string),
        };
        // Gone since it was listed: `purge` removed it meanwhile.
        let Some(db) = Store::open_existing(&account)? else {
            continue;
        };
        recorded.read(&db, viewer, watched.as_deref(), destination.as_deref())?;
    }
    Ok(recorded)
}

impl Recorded {
    /// Adds what `db`, `viewer`'s database, recorded.
    fn read(
        &mut self,
        db: &Store,
        viewer: Viewer,
        watched: Option<&[Watched]>,
        destination: Option<&str>,
    ) -> Result<()> {
        let conn = db.conn();
        let owed = deliveries::owed(conn, destination)?;
        self.owed.waiting += owed.waiting;
        self.owed.elsewhere += owed.elsewhere;
        self.owed.given_up += owed.given_up;

        // Resolved once per account, here where the store is open, so every
        // line names an account the same way.
        let mut labels: BTreeMap<Pk, String> = BTreeMap::new();
        let mut who = |pk: Pk| -> Result<String> {
            if let Some(label) = labels.get(&pk) {
                return Ok(label.clone());
            }
            let name = snob_store::store::users::name(conn, pk)?;
            let label = crate::app::label(pk, name.as_deref());
            labels.insert(pk, label.clone());
            Ok(label)
        };

        // Per account, and reported per account: a run covers every configured
        // one, so one unqualified "last ran" line is whichever account happened
        // to be last in the file.
        //
        // Asked of the run log rather than of the marks. A mark only moves when
        // a list was compared, so an account whose every tick was refused — a
        // fresh setup whose first runs met a cooldown, a stranger who went
        // private — has no mark and plenty of runs.
        let named = watched_pks(db, watched, viewer.pk)?;
        for run in watch_store::last_runs(conn)? {
            self.runs.push(RunOf {
                who: who(run.account_pk)?,
                watched: named
                    .as_ref()
                    .is_none_or(|pks| pks.contains(&run.account_pk)),
                viewer: viewer.clone(),
                run,
            });
        }

        // Which accounts have had exactly one of their two lists reported on.
        let mut seen: BTreeMap<Pk, Vec<ListKind>> = BTreeMap::new();
        for mark in watch_store::all_marks(conn)? {
            seen.entry(mark.account_pk).or_default().push(mark.kind);
            self.marks.push(MarkOf {
                who: who(mark.account_pk)?,
                viewer: viewer.clone(),
                mark,
            });
        }
        for (pk, kinds) in seen {
            self.reported.push(HalfRead {
                who: who(pk)?,
                reported: kinds[0],
                missing: (kinds.len() == 1).then(|| match kinds[0] {
                    ListKind::Followers => ListKind::Following,
                    ListKind::Following => ListKind::Followers,
                }),
            });
        }

        self.viewers.push(viewer);
        Ok(())
    }

    /// The runs and receipts, under the account each was read as.
    fn lines(&self) -> Vec<String> {
        if self.viewers.is_empty() {
            return vec![String::new(), "It has not run yet.".to_string()];
        }
        let mut lines = Vec::new();
        for viewer in &self.viewers {
            lines.push(String::new());
            lines.push(format!("As {}:", viewer.label()));
            // Said before the marks, because it answers the question somebody
            // opening `status` actually has. A run that could not look moves no
            // mark, so a monitor that has been in a cooldown since Monday looks,
            // from the marks alone, exactly like one that was killed on Monday.
            let mut ran = false;
            for RunOf { run, who, .. } in self.runs.iter().filter(|of| of.viewer.pk == viewer.pk) {
                ran = true;
                let mut line = format!("  {who} last ran on {}", report::stored_on(run.started_at));
                if let Some(outcome) = &run.outcome
                    && *outcome != RunOutcome::Ok
                {
                    line.push_str(&format!(" and could not look ({outcome})"));
                } else if run.changes == 0 {
                    line.push_str(" and found nothing");
                } else {
                    line.push_str(&format!(
                        " and found {} change{}",
                        run.changes,
                        plural(run.changes as usize)
                    ));
                }
                lines.push(format!("{line}."));
            }
            if !ran {
                lines.push("  It has not run yet.".to_string());
            }

            let mut reported = false;
            for MarkOf { mark, who, .. } in self.marks.iter().filter(|of| of.viewer.pk == viewer.pk)
            {
                reported = true;
                lines.push(format!(
                    "  {who}: {} last reported on {}",
                    mark.kind,
                    report::stored_on(mark.compared_at)
                ));
            }
            if !reported {
                lines.push("  It has not reported on anything yet.".to_string());
            }
        }
        lines
    }
}

/// How many scheduled runs may be missed before silence is a failure rather
/// than a warning.
///
/// One missed run is a machine that was asleep, a laptop that was shut, a
/// cooldown that ran long. Three is nobody coming back.
const MISSED_BEFORE_FAILED: i64 = 3;

/// How long this configuration means the monitor may be silent for.
///
/// Asked of the schedule rather than guessed at: six hours for `--every 6h`,
/// and a week for `--on mon --at 09:00`, which is the point — a weekly monitor
/// that has not run since Tuesday is not late.
///
/// **The widest gap in a cycle, not the distance between the next two
/// moments.** Those are the same number only on a uniform grid, and the wizard
/// prompts for something that is not one, back to back: "Which days?
/// (mon,thu)" and "At what times? (09:00,21:00)". Answer both with the example
/// and the real gaps are 12h, 60h, 12h, 84h. On a Saturday the next two
/// moments are Monday 09:00 and Monday 21:00, and measuring 12h from them would
/// call a last run on Thursday evening three missed runs: `status` red for 48
/// hours every week, on a monitor doing exactly what it was told.
///
/// Bounded two ways so an `--every 5m` file does not walk a week of moments:
/// a horizon of eight days, which covers any weekly pattern, and a hard cap on
/// iterations. A uniform schedule reaches its widest gap on the first step, so
/// the cap costs it nothing.
///
/// `None` when the file has no schedule this can build, or names one that never
/// fires. Both of those are their own line elsewhere and neither is a reason to
/// call the monitor late as well.
fn expected_gap(config: &WatchConfig, now: Epoch) -> Option<i64> {
    /// Far enough to see a whole week's pattern, and one day over so a weekly
    /// schedule is measured rather than truncated.
    const HORIZON_SECS: i64 = 8 * 24 * 3600;
    /// A backstop for a schedule that fires often enough to make the horizon
    /// expensive. Two hundred steps of `--every 5m` is under a day, and a
    /// uniform grid has already given its answer by step one.
    const MOST_STEPS: usize = 200;

    let schedule = schedule_from(&Default::default(), Some(config)).ok()?;
    let first = schedule::next_after(&schedule, Some(now), now, &chrono::Local)?;

    let mut at = first;
    let mut widest = 0;
    for _ in 0..MOST_STEPS {
        let Some(next) = schedule::next_after(&schedule, Some(at), at, &chrono::Local) else {
            break;
        };
        widest = widest.max(next - at);
        at = next;
        if at - first >= HORIZON_SECS {
            break;
        }
    }
    (widest > 0).then_some(widest)
}

/// An account with exactly one of its two lists ever reported on.
///
/// Built by `status` from the marks it already reads. A mark moves only when a
/// list was actually compared, so a list with none while its sibling has one has
/// never been read — which nothing else in the tool can say.
struct HalfRead {
    who: String,
    reported: ListKind,
    /// The one that never has been, or `None` when both have.
    missing: Option<ListKind>,
}

/// Since when the monitor has held a due run back for a critical battery, as
/// the run loop wrote it down; `None` when it is not, or nothing can tell.
fn waiting_for_power(paths: &AppPaths) -> Option<Epoch> {
    let shared = snob_store::store::shared::Shared::read_existing(paths).ok()??;
    shared.waiting_for_power_since().ok()?
}

fn health(
    config: Option<&WatchConfig>,
    runs: &[RunOf],
    reported: &[HalfRead],
    owed: deliveries::Owed,
    now: Epoch,
    waiting_for_power: Option<Epoch>,
) -> Health {
    let mut notes = Vec::new();
    let mut verdict = Verdict::Ok;
    let mut at_least = |level: Verdict| verdict = verdict.max(level);

    if config.is_none() {
        at_least(Verdict::Warned);
        notes.push(crate::report::NOTHING_CONFIGURED.to_string());
    }

    if runs.is_empty() {
        at_least(Verdict::Warned);
        notes.push("it has not run yet".to_string());
    }

    // **One list being read while the other never is.**
    //
    // `watch_runs.outcome` is one column for a tick that covers two lists, and
    // `TickReport::looked()` asks `any`, not `all` — so a run where followers
    // completed and following was refused records `ok`. Every run, forever, on
    // the account AGENTS.md files under "Known walls": `following` meets the
    // truncation wall on every walk while `followers` completes. Nothing else
    // tells that from a quiet account.
    //
    // Asked of the marks rather than of the run log, because the marks are
    // where the durable answer already is: a mark moves only when a list was
    // actually compared, so a list with none while its sibling has one has
    // never once been read. That needs no new column and no threshold, and it
    // catches the permanent case, which is the one that matters. A single
    // half-blind run is the payload's question, not this one.
    for half in reported.iter().filter(|r| r.missing.is_some()) {
        let (read, never) = (half.reported, half.missing.expect("filtered"));
        at_least(Verdict::Warned);
        notes.push(format!(
            "{}: {read} has been reported on and {never} never has, so one of the two \
             lists is not being read",
            half.who
        ));
    }

    // A schedule the scheduler refuses is a monitor that cannot start.
    // `describe_config` prints the clauses without evaluating anything, so this
    // is where the schedule is built.
    if let Some(config) = config
        && let Err(e) = schedule_from(&Default::default(), Some(config))
    {
        at_least(Verdict::Failed);
        notes.push(format!("the configured schedule cannot be built: {e}"));
    }

    // And an address the delivery refuses is a monitor that cannot finish:
    // `delivery_from` runs before anything is opened or spent, so a bad address
    // kills every run before `record_failed_run` can file one, and the table
    // stays empty. `webhook::problem_with_config` is the same question
    // `snob watch check` asks.
    if let Some(webhook) = config.and_then(|c| c.webhook.as_ref())
        && let Some(problem) =
            crate::watch::webhook::problem_with_config(&webhook.url, &webhook.headers)
    {
        at_least(Verdict::Failed);
        notes.push(problem);
    }

    // **Whether it is still running at all.** An `outcome` that succeeded says
    // nothing about how old it is, and `prune` keeps the newest row per account
    // whatever its age, precisely so this can read it. A unit that was
    // disabled, a container nobody restarted, a process the kernel killed:
    // `002_watch.sql` says `watch_runs` exists because "a monitor that quietly
    // stopped looks exactly like a quiet account", and this is the reader that
    // tells them apart.
    //
    // The gap comes from the schedule rather than from a guess, so a weekly
    // monitor is not called late after two days.
    //
    // **Except while it waits for power.** A monitor on a laptop whose battery
    // is critical holds the run that is due until the machine is on power
    // again (`scheduled::battery_is_critical`); that is the monitor doing
    // what it should, and it is said as such rather than counted late.
    if let Some(since) = waiting_for_power {
        at_least(Verdict::Warned);
        notes.push(format!(
            "the run that is due is waiting for power: the battery has been critical since {}",
            report::stored_on(since)
        ));
    } else if let Some(gap) = config.and_then(|c| expected_gap(c, now))
        && let Some(newest) = runs.iter().map(|r| r.run.started_at).max()
    {
        let silent = now - newest;
        let missed = silent / gap.max(1);
        if missed >= MISSED_BEFORE_FAILED {
            at_least(Verdict::Failed);
            notes.push(format!(
                "it has not run since {}, which is {missed} scheduled runs ago",
                report::stored_on(newest)
            ));
        } else if missed >= 1 {
            at_least(Verdict::Warned);
            notes.push(format!(
                "it has not run since {}, and one was due by now",
                report::stored_on(newest)
            ));
        }
    }

    // What stopped the last run of each account. A cooldown lifts on its own
    // and is worth saying rather than alarming about; a session that has gone
    // will not come back without somebody logging in, and every run until then
    // does nothing.
    //
    // **Only for accounts the file still names.** `last_runs` answers about
    // every account that has ever run, and one old failed run for a stranger
    // since removed from `watch.toml` must not pin the verdict at `Failed` for
    // good: that is how people learn to ignore a probe. It is still worth a
    // line, because a row nobody watches is worth explaining, and the line
    // names the account, so several accounts do not print one sentence N
    // times.
    for RunOf {
        run, who, watched, ..
    } in runs
    {
        let Some(code) = run.outcome.as_ref().filter(|c| **c != RunOutcome::Ok) else {
            continue;
        };
        if !watched {
            notes.push(format!(
                "{who} last ended in {code}, and the configuration no longer names it"
            ));
            continue;
        }
        // **Matched as an outcome, not against literals spelled again here**:
        // `as_str`'s own doc says the vocabulary exists "so nothing has to
        // invent tokens inline, which is how two spellings of one condition get
        // shipped". The row arrives typed, parsed at the store.
        //
        // A token this build does not know is `None` and falls to the arm below:
        // a failure it cannot explain, which is the direction a probe should be
        // wrong in. It is still printed as the word the row holds, because
        // "ended in something this build does not know" is less use to whoever
        // has to look than the word itself.
        match code.known() {
            // A cooldown lifts by itself and an interrupt was the user. Neither
            // is a monitor that needs anybody.
            Some(RunOutcome::RateLimited | RunOutcome::Interrupted) => {
                at_least(Verdict::Warned);
                notes.push(format!("{who}'s last run ended in {code}"));
            }
            _ => {
                at_least(Verdict::Failed);
                notes.push(format!("{who}'s last run ended in {code}"));
            }
        }
    }

    // **Which queue it is, asked of the queue**, not of one `pending` count or
    // of the file: a monitor whose `[webhook] url` moved must not be scored on
    // rows `due` can never return, and a run given the address on the command
    // line from a unit with no `watch.toml` (the README's own example) has rows
    // with a real address the next run drains. A probe that pages for a
    // healthy service is how people learn to ignore a probe.
    if owed.elsewhere > 0 {
        if config.and_then(|c| c.webhook.as_ref()).is_some() {
            // The file names an address and these are for a different one, so
            // nothing will ever post them: they expire where they are, and the
            // changes in them are already marked as reported, which makes them
            // the only copy.
            at_least(Verdict::Failed);
            notes.push(format!(
                "{} queued report(s) are addressed somewhere this configuration does not send \
                 to, so nothing here will try them",
                owed.elsewhere
            ));
        } else {
            // With no address in the file, a run given `--webhook` and a
            // `[webhook]` somebody deleted look identical from here, and
            // guessing in the alarming direction would fail the supported one.
            // Worth a line, not a verdict — the direction `watched_pks` takes
            // when it cannot settle who is watched.
            at_least(Verdict::Warned);
            notes.push(format!(
                "{} queued report(s) are addressed to a webhook this file does not name, so \
                 only a run given --webhook can send them",
                owed.elsewhere
            ));
        }
    }

    if owed.waiting > 0 {
        at_least(Verdict::Warned);
        notes.push(format!(
            "{} report(s) still waiting to be delivered",
            owed.waiting
        ));
    }

    // A report given up on is not a failure of the monitor -- it is usually a
    // receiver that was down for a day -- but it must not be silent, and it
    // must not be the thing that lets the verdict go green: counting only
    // `pending` would turn `warning` into `ok` the moment the change was thrown
    // away.
    if owed.given_up > 0 {
        at_least(Verdict::Warned);
        notes.push(format!(
            "{} report(s) were given up on for being too old to be news",
            owed.given_up
        ));
    }

    Health { verdict, notes }
}

/// The configuration in a few lines, for `status` and for the confirmation
/// `setup` shows before replacing a file.
pub(super) fn describe_config(config: &WatchConfig) -> Vec<String> {
    let mut lines = Vec::new();

    let when =
        report::schedule_clauses(config.every, &config.on, &config.at, config.cron.as_deref());
    lines.push(if when.is_empty() {
        "no schedule: it will not run until one is set".to_string()
    } else {
        format!("Runs {}", when.join(", "))
    });

    // What the file says, not what a `Schedule` would clamp it to: this is the
    // place a person reads their configuration back, a hand-edited
    // `jitter = "1h"` included.
    if let Some(jitter) = config.jitter
        && let Some(sentence) = report::jitter_sentence(jitter)
    {
        lines.push(sentence);
    }

    // Redacted, not just filtered: `webhook::check` refuses an address
    // carrying a password because it would be echoed by `status`. One address,
    // cleaned once, and both sentences below read it.
    let address = config
        .webhook
        .as_ref()
        .map(|webhook| printable(&crate::watch::webhook::shown_str(&webhook.url)));

    match (&config.webhook, &address) {
        (Some(webhook), Some(address)) => {
            lines.push(format!("Reports to {address}"));
            if webhook.heartbeat {
                lines.push("Sends a report even when nothing changed".to_string());
            }
        }
        // What the **file** says, which is not the whole of where a report can
        // go. `--webhook` on the command line is a supported way to run this and
        // leaves nothing here to read back, so a flat "sends nothing" would
        // describe a monitor that delivers on every run as one that does not.
        _ => lines.push(
            "No address here: the report goes to standard output unless a run is given \
             --webhook"
                .to_string(),
        ),
    }

    lines.push(watching_line(config));

    // The two facts above are one fact, not two unrelated lines: somebody
    // else's names leaving this machine on every run is the part of this
    // configuration a person would most want to be reminded of. "Their" rather
    // than a count, because `watching_line` directly above has just said how
    // many.
    if let Some(address) = &address
        && config.accounts.iter().any(|a| !a.is_own())
    {
        lines.push(format!(
            "Their usernames and names go to {address} with every report"
        ));
    }

    lines
}

/// Who a run over this file would actually walk.
///
/// **Asked of `watched_from`, which is the function that decides it**, so the
/// sentence cannot describe a run that does something else. The hard shape is a
/// file naming one stranger and not you: `watched_from` falls back to the
/// viewer only when the account list is empty.
///
/// `None` for the target, because there is no command line here: this is what a
/// scheduled run over this file alone would walk.
fn watching_line(config: &WatchConfig) -> String {
    let watched = watched_from(None, Some(config), None);
    // A `self` is its viewer's own account, so each viewer's counts once.
    let own = watched
        .iter()
        .filter(|w| w.name().is_none())
        .map(Watched::viewer)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let others = watched.iter().filter(|w| w.name().is_some()).count();

    let yours = match own {
        0 => {
            return format!(
                "Watches {others} account{}, and not your own",
                plural(others)
            );
        }
        1 => "your account".to_string(),
        n => format!("{n} of your accounts"),
    };
    match others {
        0 => format!("Watches {yours}"),
        n => format!("Watches {yours} and {n} other{}", plural(n)),
    }
}

pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::{known_account, watch_toml};
    use snob_core::Pk;
    use snob_core::watch::RecordedOutcome;
    use std::time::Duration;

    /// The one address in the file is shown twice when somebody else is
    /// watched, and both showings are the cleaned one. The second was the
    /// raw string, so a hand-written `user:pass@` -- which `check` refuses
    /// and `status` does not run -- reached the terminal after the first
    /// line had taken care to hide it.
    #[test]
    fn a_password_in_the_address_is_hidden_from_every_line() {
        let lines = describe_config(&watch_toml(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://me:hunter2@n8n.local/hook\"

[[account]]
target = \"someone\"
",
        ));
        let text = lines.join(
            "
",
        );
        assert!(text.contains("go to"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("n8n.local/hook"), "{text}");
    }

    #[test]
    fn it_describes_an_interval_and_a_webhook() {
        let lines = describe_config(&watch_toml(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.local/hook\"\nheartbeat = true\n",
        ));
        let text = lines.join("\n");
        assert!(text.contains("every 6h"), "{text}");
        assert!(text.contains("https://n8n.local/hook"), "{text}");
        assert!(text.contains("even when nothing changed"), "{text}");
    }

    /// A file with no webhook is a whole configuration, and saying so is what
    /// stops somebody thinking the delivery is broken.
    #[test]
    fn it_says_when_nothing_is_sent_anywhere() {
        let lines = describe_config(&watch_toml("schema = 1\nevery = \"6h\"\n"));
        assert!(lines.join("\n").contains("standard output"));
    }

    /// A hand-edited file can have neither half of a schedule, and then the
    /// monitor never runs. Saying "Runs" with nothing after it would read as
    /// though it were fine.
    #[test]
    fn a_file_with_no_schedule_says_it_will_not_run() {
        let lines = describe_config(&watch_toml("schema = 1\n"));
        assert!(lines[0].contains("will not run"), "{lines:?}");
    }

    #[test]
    fn it_counts_the_other_accounts() {
        let lines = describe_config(&watch_toml(
            "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n\n\
             [[account]]\ntarget = \"someone\"\n[account.consent]\nagreed_at = 1\n",
        ));
        assert!(
            lines.join("\n").contains("your account and 1 other"),
            "{lines:?}"
        );
    }

    /// A file that names one stranger is walked as that stranger alone:
    /// `watched_from` falls back to the viewer only when the list is empty. Both
    /// of the two places a person reads the configuration back said otherwise,
    /// because this counted the strangers and then claimed the viewer anyway.
    #[test]
    fn it_does_not_claim_your_account_when_the_file_does_not_list_it() {
        let lines = describe_config(&watch_toml(
            "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"friend\"\n\
             [account.consent]\nagreed_at = 1\n",
        ));
        let text = lines.join("\n");

        assert!(!text.contains("your account"), "{text}");
        assert!(text.contains("1 account, and not your own"), "{text}");
    }

    /// The sentence names the accounts a run would actually walk, over every
    /// shape the file can take.
    ///
    /// The rule that makes it hard: the viewer is added only when the account
    /// list is empty, so a file naming one stranger is walked as that stranger
    /// alone.
    ///
    /// The expected sentences are written out rather than computed from
    /// `watched_from`, which would make this agree with itself whatever either
    /// side did. Written out, a change to `watched_from` moves the sentence and
    /// fails here -- which is the whole property.
    #[test]
    fn the_line_names_the_accounts_a_run_would_actually_walk() {
        let friend = "[account.consent]\nagreed_at = 1\n";
        for (body, expected) in [
            (
                "schema = 1\nevery = \"6h\"\n".to_string(),
                "Watches your account",
            ),
            (
                "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n".to_string(),
                "Watches your account",
            ),
            (
                format!("schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"friend\"\n{friend}"),
                "Watches 1 account, and not your own",
            ),
            (
                format!(
                    "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n\n\
                     [[account]]\ntarget = \"friend\"\n{friend}"
                ),
                "Watches your account and 1 other",
            ),
            (
                format!(
                    "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"@friend\"\n{friend}\n\
                     [[account]]\ntarget = \"other\"\n{friend}"
                ),
                "Watches 2 accounts, and not your own",
            ),
            (
                format!(
                    "schema = 2
every = \"6h\"

[[account]]
target = \"self\"
viewer = 1

                     [[account]]
target = \"self\"
viewer = 2

                     [[account]]
target = \"friend\"
viewer = 2
{friend}"
                ),
                "Watches 2 of your accounts and 1 other",
            ),
        ] {
            let file = watch_toml(&body);
            assert_eq!(watching_line(&file), expected, "for {body:?}");
        }
    }

    /// A file with no `[[account]]` at all means the obvious thing, and this is
    /// the one shape where the old wording happened to be right by accident --
    /// it printed nothing.
    #[test]
    fn a_file_naming_nobody_watches_the_viewer() {
        let lines = describe_config(&watch_toml("schema = 1\nevery = \"6h\"\n"));
        assert!(
            lines.join("\n").contains("Watches your account"),
            "{lines:?}"
        );
    }

    /// The one setting somebody could write into the file and never see again.
    ///
    /// `status` and the replace-this-file confirmation are where a person reads
    /// their configuration back, and `WatchConfig.jitter` reached neither.
    #[test]
    fn a_configured_jitter_is_read_back() {
        let lines = describe_config(&watch_toml("schema = 1\nevery = \"6h\"\njitter = \"1h\"\n"));
        assert!(
            lines.join("\n").contains("pushed up to 1h later"),
            "{lines:?}"
        );

        let none = describe_config(&watch_toml("schema = 1\nevery = \"6h\"\njitter = \"0\"\n"));
        assert!(
            !none.join("\n").contains("pushed up to"),
            "a jitter that was turned off has nothing to say: {none:?}"
        );
    }

    /// A name written with an at sign still names the account it watches.
    ///
    /// `status` settles which accounts the file names by looking each one up by
    /// username. Were `target = "@friend"` to find nobody, every run of that
    /// account would be scored as belonging to an account the configuration no
    /// longer names -- deliberately only a note, never a verdict -- and a
    /// monitor whose one watched account fails every run would exit 0 forever.
    #[test]
    fn a_name_written_with_an_at_sign_is_still_an_account_the_file_watches() {
        let db = Store::in_memory().unwrap();
        known_account(&db, Pk::new(7), "friend", false);

        let edited =
            watch_toml("schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"@friend\"\n");
        let watched = watched_from(None, Some(&edited), Some(Pk::new(42)));
        assert_eq!(
            watched_pks(&db, Some(&watched), Pk::new(42)).unwrap(),
            Some(vec![Pk::new(7)]),
            "the file names @friend and the store knows friend; they are one account"
        );
    }

    /// `self` is the account its entry is read as, which is not necessarily the
    /// one this database belongs to.
    #[test]
    fn self_is_the_account_its_entry_is_read_as() {
        let db = Store::in_memory().unwrap();
        let file = watch_toml(
            r#"
schema = 2
every = "6h"

[[account]]
target = "self"
viewer = 1

[[account]]
target = "self"
"#,
        );

        let signed_in = watched_from(None, Some(&file), Some(Pk::new(3)));
        assert_eq!(
            watched_pks(&db, Some(&signed_in), Pk::new(1)).unwrap(),
            Some(vec![Pk::new(1)])
        );
        assert_eq!(
            watched_pks(&db, Some(&signed_in), Pk::new(3)).unwrap(),
            Some(vec![Pk::new(3)]),
            "the second names no viewer, so it is the account in use"
        );
        let nobody = watched_from(None, Some(&file), None);
        assert_eq!(
            watched_pks(&db, Some(&nobody), Pk::new(1)).unwrap(),
            None,
            "with nobody signed in, whose the second is cannot be settled"
        );
    }

    /// A fixed present, so how old a run is is something these tests state
    /// rather than something they inherit from the wall clock.
    const NOW: Epoch = Epoch::new(1_700_000_000);

    /// A run that happened a minute ago, not at the epoch: a run that old under
    /// a six-hourly schedule is a monitor that stopped.
    fn ran(outcome: ExitCode) -> watch_store::Run {
        watch_store::Run {
            account_pk: Pk::new(42),
            started_at: NOW - Duration::from_secs(60),
            finished_at: Some(NOW - Duration::from_secs(60)),
            requests: 1,
            // The conversion `commit` writes through, not a token spelled here:
            // a fixture spelling the literals `health` matched on would move
            // with any misspelling.
            outcome: Some(outcome.into()),
            changes: 0,
        }
    }

    /// A run belonging to an account the file still names.
    fn of(run: &watch_store::Run) -> RunOf {
        RunOf {
            run: run.clone(),
            who: "@me".to_string(),
            watched: true,
            viewer: me(),
        }
    }

    fn me() -> Viewer {
        Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        }
    }

    /// Which states count as a monitor that has stopped doing its job.
    ///
    /// What is pinned here is the line between "working" and "not", because
    /// that is what an exit code is.
    #[test]
    fn a_healthy_monitor_is_told_apart_from_one_that_has_stopped_working() {
        let configured = watch_toml(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://n8n.internal/hook\"
",
        );

        let ok = ran(ExitCode::Ok);
        assert_eq!(
            health(
                Some(&configured),
                &[of(&ok)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Ok,
            "configured, ran just now, nothing owed"
        );

        // A cooldown lifts on its own, and an interrupt was the user. Neither
        // is a monitor that needs attention.
        for lifts in [ExitCode::RateLimited, ExitCode::Interrupted] {
            let run = ran(lifts);
            assert_eq!(
                health(
                    Some(&configured),
                    &[of(&run)],
                    &[],
                    deliveries::Owed::default(),
                    NOW,
                    None
                )
                .verdict,
                Verdict::Warned,
                "{lifts:?} passes on its own"
            );
        }

        // A session that has gone will not come back without somebody logging
        // in, and every run until then does nothing at all.
        let dead = ran(ExitCode::NoSession);
        assert_eq!(
            health(
                Some(&configured),
                &[of(&dead)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Failed
        );

        // Owed reports this configuration could post are a wait.
        assert_eq!(
            health(
                Some(&configured),
                &[of(&ok)],
                &[],
                deliveries::Owed {
                    waiting: 2,
                    elsewhere: 0,
                    given_up: 0,
                },
                NOW,
                None
            )
            .verdict,
            Verdict::Warned
        );
        // Addressed to somewhere this file does not name is a different
        // question: a file with no address cannot tell a run given --webhook
        // from a [webhook] somebody deleted.
        // `a_report_addressed_elsewhere_is_not_owed_to_this_run` holds both
        // directions down.
        let no_webhook = watch_toml(
            "schema = 1
every = \"6h\"
",
        );
        assert_eq!(
            health(
                Some(&no_webhook),
                &[of(&ok)],
                &[],
                deliveries::Owed {
                    waiting: 0,
                    elsewhere: 2,
                    given_up: 0,
                },
                NOW,
                None
            )
            .verdict,
            Verdict::Warned,
            "a file with no address cannot tell --webhook from a deleted section"
        );

        // And a machine with nothing configured is not broken, but a bare
        // `snob watch` there has no schedule to run on.
        let nothing = health(None, &[], &[], deliveries::Owed::default(), NOW, None);
        assert_eq!(nothing.verdict, Verdict::Warned);
        assert_eq!(nothing.notes.len(), 2, "{:?}", nothing.notes);
    }

    /// **An address that kills every run before it starts is a failure, not a
    /// quiet month.**
    ///
    /// `delivery_from` runs before anything is opened or spent, so a bad
    /// address means `record_failed_run` never files a row and `watch_runs`
    /// stays empty, which alone reads as "it has not run yet".
    #[test]
    fn an_address_that_can_never_work_is_a_failure_and_not_a_quiet_monitor() {
        // No scheme. Exactly what a hand-edited file looks like, and the file
        // invites hand-editing on its first line.
        let no_scheme = watch_toml(
            "schema = 1
every = \"6h\"

[webhook]
url = \"n8n.local/hook\"
",
        );
        let found = health(
            Some(&no_scheme),
            &[],
            &[],
            deliveries::Owed::default(),
            NOW,
            None,
        );
        assert_eq!(
            found.verdict,
            Verdict::Failed,
            "an unusable address with an empty run log: {:?}",
            found.notes
        );
        assert_eq!(found.verdict.exit_code(), ExitCode::Error);

        // A credential in the address is the other refusal `webhook::check`
        // makes, and it reaches this probe by the same route.
        let in_the_url = watch_toml(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://user:pw@example.com/hook\"
",
        );
        assert_eq!(
            health(
                Some(&in_the_url),
                &[],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Failed
        );

        // And an address that is merely unreachable is not this probe's
        // question -- it cannot be answered without posting.
        let fine = watch_toml(
            "schema = 1
every = \"6h\"

[webhook]
url = \"https://example.com/hook\"
",
        );
        let ok = health(
            Some(&fine),
            &[],
            &[],
            deliveries::Owed::default(),
            NOW,
            None,
        );
        assert_eq!(ok.verdict, Verdict::Warned, "{:?}", ok.notes);
    }

    /// What stopped the last run is read in the vocabulary exit codes are
    /// written in, not in three literals spelled again inside `health`.
    ///
    /// Respelling one would score a monitor in an ordinary cooldown `Failed`,
    /// so `status` would exit 1 about something that resumes by itself.
    ///
    /// Walked over `ExitCode::ALL`, so a code added later has to be placed
    /// deliberately rather than defaulted into `Failed` by nobody mentioning it.
    #[test]
    fn what_stopped_the_last_run_is_read_in_the_tokens_exit_codes_are_written_in() {
        let configured = watch_toml("schema = 1\nevery = \"6h\"\n");
        let verdict_of = |run: &watch_store::Run| {
            health(
                Some(&configured),
                &[of(run)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None,
            )
            .verdict
        };

        for code in ExitCode::ALL {
            let run = ran(code);
            let expected = match code {
                ExitCode::Ok => Verdict::Ok,
                ExitCode::RateLimited | ExitCode::Interrupted => Verdict::Warned,
                _ => Verdict::Failed,
            };
            assert_eq!(
                verdict_of(&run),
                expected,
                "a run recorded as {code:?} ({})",
                code.as_str()
            );
        }

        // Named on its own as well as inside the walk, because this is the one
        // the mapping can lose without the walk noticing: drop a code from `ALL`
        // and the loop simply stops testing it.
        let cooled = ran(ExitCode::RateLimited);
        assert_eq!(
            verdict_of(&cooled),
            Verdict::Warned,
            "a cooldown lifts by itself; a probe that pages for one is a probe people switch off"
        );

        // And a row spelled by something that is not this build is a failure it
        // cannot explain, rather than a quiet `Ok`. It is an `Unknown` and not a
        // near miss for `rate_limited`: nothing here guesses at a token a newer
        // build wrote.
        let unknown = watch_store::Run {
            outcome: Some(RecordedOutcome::Unknown("rate-limited".into())),
            ..ran(ExitCode::Ok)
        };
        assert_eq!(
            verdict_of(&unknown),
            Verdict::Failed,
            "a token this build does not know is not a healthy run"
        );
    }

    /// The two probes say the same thing about a machine with no `watch.toml`.
    ///
    /// It is the advice a newly installed tool gives, so it is the one somebody
    /// edits, and two probes run one after the other must not disagree about
    /// the same machine.
    ///
    /// One run is given so `status` has nothing else to say: the only note left
    /// is the one under test, which is what makes this an equality rather than a
    /// search for a substring.
    #[test]
    fn both_probes_say_the_same_thing_about_an_unconfigured_machine() {
        let ok = ran(ExitCode::Ok);
        let status = health(
            None,
            &[of(&ok)],
            &[],
            deliveries::Owed::default(),
            NOW,
            None,
        );
        assert_eq!(status.notes.len(), 1, "{:?}", status.notes);

        let check = crate::engine::check::without_a_session(None, None, NOW);
        assert_eq!(check.checked.len(), 1, "{:?}", check.checked);

        assert_eq!(
            status.notes[0],
            super::super::say::problem_line(
                check.checked[0]
                    .problem
                    .as_ref()
                    .expect("the check has to say why nothing is configured")
            ),
            "two probes somebody runs one after the other, disagreeing about one machine"
        );
    }

    /// A queue for an address the file does not name is not a monitor with
    /// nowhere to send, and one for an address it *has* moved away from is.
    ///
    /// A run given the address on the command line from a unit with no
    /// `watch.toml` (the README's own systemd example) has rows with a real
    /// address the next run drains, while a `[webhook] url` moved to a new host
    /// with the old queue still standing really does lose changes.
    #[test]
    fn a_report_addressed_elsewhere_is_not_owed_to_this_run() {
        let ok = ran(ExitCode::Ok);
        let two_elsewhere = deliveries::Owed {
            waiting: 0,
            elsewhere: 2,
            given_up: 0,
        };

        // The address moved. Those rows can never go, and the changes in them
        // are already marked as reported, so they are the only copy.
        let moved = watch_toml(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://n8n.new.local/hook\"\n",
        );
        let stranded = health(Some(&moved), &[of(&ok)], &[], two_elsewhere, NOW, None);
        assert_eq!(stranded.verdict, Verdict::Failed, "{:?}", stranded.notes);

        // The same rows with no `[webhook]` in the file are the shape the README
        // leads with: the address is on the command line and the next run drains
        // them. A guess in the alarming direction is what costs a probe its
        // credibility.
        let from_the_flag = watch_toml("schema = 1\nevery = \"6h\"\n");
        let fine = health(
            Some(&from_the_flag),
            &[of(&ok)],
            &[],
            two_elsewhere,
            NOW,
            None,
        );
        assert_eq!(fine.verdict, Verdict::Warned, "{:?}", fine.notes);
        assert_eq!(fine.verdict.exit_code(), ExitCode::Ok);
        assert!(
            fine.notes.iter().any(|n| n.contains("--webhook")),
            "and it has to say what would send them: {:?}",
            fine.notes
        );

        // And the file that says where reports go says so out loud, because
        // "Sends nothing" was printed about a monitor that delivers on every
        // run.
        assert!(
            !describe_config(&from_the_flag)
                .join("\n")
                .contains("Sends nothing"),
            "the file names no address; that is not the same as sending nothing"
        );
    }

    /// A file whose schedule the scheduler refuses is a monitor that cannot
    /// start, and both read-only probes say so.
    ///
    /// Every one of these gets into the file through a hand-edit, which the
    /// first line of `watch.toml` says is fine, and every one passes
    /// `config::parse` — which reads TOML, the schema number and one key clash.
    #[test]
    fn a_schedule_the_scheduler_refuses_is_not_healthy() {
        for refused in [
            "schema = 1\nevery = \"5m\"\n",
            "schema = 1\ncron = \"0 9 * *\"\n",
            "schema = 1\nat = [\"25:00\"]\n",
            // Not `every = "2w"` beside `on = ["mon"]`: that is the shape the
            // README leads with, and it builds
            // (`one_monday_in_every_two_is_what_the_readme_says_it_is`).
        ] {
            let configured = watch_toml(refused);
            let ok = ran(ExitCode::Ok);
            let health = health(
                Some(&configured),
                &[of(&ok)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None,
            );
            assert_eq!(
                health.verdict,
                Verdict::Failed,
                "{refused:?} kills every run: {:?}",
                health.notes
            );
        }

        // And one that builds is still fine.
        let good = watch_toml("schema = 1\nevery = \"6h\"\n");
        let ok = ran(ExitCode::Ok);
        assert_eq!(
            health(
                Some(&good),
                &[of(&ok)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Ok
        );
    }

    /// One list being read while the other never is.
    ///
    /// `watch_runs.outcome` is one column for a tick covering two lists, and
    /// `TickReport::looked()` asks `any` rather than `all` — so a run where
    /// followers completed and following was refused records `ok`. Every run,
    /// forever, on the account AGENTS.md files under "Known walls".
    ///
    /// Asked of the marks, because that is where the durable answer already
    /// is: a mark moves only when a list was actually compared.
    #[test]
    fn one_list_never_being_read_is_not_a_healthy_monitor() {
        let configured = watch_toml("schema = 1\nevery = \"6h\"\n");
        let ok = ran(ExitCode::Ok);

        let half = HalfRead {
            who: "@me".to_string(),
            reported: ListKind::Followers,
            missing: Some(ListKind::Following),
        };
        let blind = health(
            Some(&configured),
            &[of(&ok)],
            &[half],
            deliveries::Owed::default(),
            NOW,
            None,
        );
        assert_eq!(blind.verdict, Verdict::Warned, "{:?}", blind.notes);
        assert!(
            blind
                .notes
                .iter()
                .any(|n| n.contains("following") && n.contains("@me")),
            "the line has to name the list and the account: {:?}",
            blind.notes
        );

        // Both read is the ordinary case and says nothing.
        let whole = HalfRead {
            who: "@me".to_string(),
            reported: ListKind::Followers,
            missing: None,
        };
        assert_eq!(
            health(
                Some(&configured),
                &[of(&ok)],
                &[whole],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Ok
        );
    }

    /// A monitor that stopped is not a healthy monitor.
    ///
    /// A successful run three weeks old is not `Ok`, and `prune` keeps the
    /// newest row per account whatever its age, so it never ages into "it has
    /// not run yet" either. That is the likeliest real failure of an unattended
    /// service: a unit disabled, a container nobody restarted, a process the
    /// kernel killed.
    #[test]
    fn a_monitor_that_stopped_is_not_healthy() {
        let configured = watch_toml("schema = 1\nevery = \"6h\"\n");

        let recent = ran(ExitCode::Ok);
        assert_eq!(
            health(
                Some(&configured),
                &[of(&recent)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Ok
        );

        // One six-hour gap missed is a laptop that was shut.
        let late = watch_store::Run {
            started_at: NOW - Duration::from_secs(7 * 3_600),
            ..ran(ExitCode::Ok)
        };
        let one = health(
            Some(&configured),
            &[of(&late)],
            &[],
            deliveries::Owed::default(),
            NOW,
            None,
        );
        assert_eq!(one.verdict, Verdict::Warned, "{:?}", one.notes);

        // Three weeks is nobody coming back.
        let gone = watch_store::Run {
            started_at: NOW - Duration::from_secs(21 * 86_400),
            ..ran(ExitCode::Ok)
        };
        let stopped = health(
            Some(&configured),
            &[of(&gone)],
            &[],
            deliveries::Owed::default(),
            NOW,
            None,
        );
        assert_eq!(stopped.verdict, Verdict::Failed, "{:?}", stopped.notes);
        assert!(
            stopped
                .notes
                .iter()
                .any(|n| n.contains("has not run since")),
            "and it has to say so: {:?}",
            stopped.notes
        );
    }

    /// A monitor holding its run for a critical battery is doing what it
    /// should: said as such, and not counted late however long it waits.
    #[test]
    fn a_monitor_waiting_for_power_is_not_late() {
        let configured = watch_toml("schema = 1\nevery = \"6h\"\n");
        let gone = watch_store::Run {
            started_at: NOW - Duration::from_secs(2 * 86_400),
            ..ran(ExitCode::Ok)
        };
        let waiting = health(
            Some(&configured),
            &[of(&gone)],
            &[],
            deliveries::Owed::default(),
            NOW,
            Some(NOW - Duration::from_secs(3_600)),
        );
        assert_eq!(waiting.verdict, Verdict::Warned, "{:?}", waiting.notes);
        assert!(
            waiting
                .notes
                .iter()
                .any(|n| n.contains("waiting for power")),
            "{:?}",
            waiting.notes
        );
        assert!(
            !waiting
                .notes
                .iter()
                .any(|n| n.contains("has not run since")),
            "{:?}",
            waiting.notes
        );
    }

    /// How late is late comes from the schedule, so a weekly monitor is not
    /// called late after two days.
    #[test]
    fn how_late_is_late_depends_on_the_schedule() {
        let two_days_ago = watch_store::Run {
            started_at: NOW - Duration::from_secs(2 * 86_400),
            ..ran(ExitCode::Ok)
        };

        let weekly = watch_toml("schema = 1\non = [\"mon\"]\nat = [\"09:00\"]\n");
        assert_eq!(
            health(
                Some(&weekly),
                &[of(&two_days_ago)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Ok,
            "two days is not late for a weekly schedule"
        );

        let six_hourly = watch_toml("schema = 1\nevery = \"6h\"\n");
        assert_ne!(
            health(
                Some(&six_hourly),
                &[of(&two_days_ago)],
                &[],
                deliveries::Owed::default(),
                NOW,
                None
            )
            .verdict,
            Verdict::Ok,
            "and it very much is for a six-hourly one"
        );
    }

    /// **A schedule that is not a uniform grid, which is what the wizard
    /// suggests.**
    ///
    /// The test above uses one weekly moment and `every = "6h"` — both
    /// uniform, so both have one gap. Answer the two prompts with the examples
    /// they print and the gaps are 12h, 60h, 12h and 84h; reading the first one
    /// would call a working monitor three runs late for two days out of every
    /// seven.
    #[test]
    fn an_uneven_schedule_is_measured_by_its_widest_gap_and_not_its_narrowest() {
        let uneven = watch_toml(
            "schema = 1
on = [\"mon\", \"thu\"]
at = [\"09:00\", \"21:00\"]
",
        );
        let gap = expected_gap(&uneven, NOW).expect("the schedule builds and fires");
        assert!(
            gap >= 80 * 3600,
            "the widest gap in this week is 84h; got {}h",
            gap / 3600
        );

        // Twice a day is still twice a day: the widest gap is the overnight
        // one, not the twelve hours between the two the wizard prints.
        let daily_pair = watch_toml(
            "schema = 1
at = [\"09:00\", \"10:00\"]
",
        );
        let gap = expected_gap(&daily_pair, NOW).expect("fires");
        assert!(
            gap >= 22 * 3600,
            "09:00 and 10:00 leaves 23 hours overnight; got {}h",
            gap / 3600
        );

        // And a uniform grid is unchanged, which is the half that already
        // worked and must keep working.
        let uniform = watch_toml(
            "schema = 1
every = \"6h\"
",
        );
        assert_eq!(expected_gap(&uniform, NOW), Some(6 * 3600));
    }

    /// A run belonging to an account the file no longer names is worth a line,
    /// not a verdict.
    ///
    /// `last_runs` answers about every account that has ever run, and one old
    /// failure for a stranger since removed must not pin the verdict at
    /// `Failed` for good. The note names the account, so several do not print
    /// one sentence N times.
    #[test]
    fn a_run_for_an_account_nobody_watches_any_more_does_not_fail_the_verdict() {
        let configured = watch_toml("schema = 1\nevery = \"6h\"\n");
        let failed = ran(ExitCode::NoSession);

        let orphan = RunOf {
            run: failed,
            who: "@stranger".to_string(),
            watched: false,
            viewer: me(),
        };
        let health = health(
            Some(&configured),
            &[orphan],
            &[],
            deliveries::Owed::default(),
            NOW,
            None,
        );

        assert_eq!(health.verdict, Verdict::Ok, "{:?}", health.notes);
        assert!(
            health.notes.iter().any(|n| n.contains("@stranger")),
            "the line has to say which account: {:?}",
            health.notes
        );
    }

    /// What each account recorded is read from its own database and shown
    /// under it, and the JSON names the account on every row.
    #[test]
    fn status_groups_what_each_account_recorded_by_viewer() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        for (pk, name) in [(1, "one"), (2, "two")] {
            let db = Store::open(&paths.account(Pk::new(pk))).unwrap();
            known_account(&db, Pk::new(pk), name, true);
            watch_store::record_run(
                db.conn(),
                &watch_store::Run {
                    account_pk: Pk::new(pk),
                    ..ran(ExitCode::Ok)
                },
            )
            .unwrap();
        }
        let file = watch_toml(
            "schema = 2\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\nviewer = 1\n\n\
             [[account]]\ntarget = \"self\"\nviewer = 2\n",
        );

        let recorded = recorded(&paths, Some(&file), Some(Pk::new(1))).unwrap();

        assert_eq!(recorded.runs.len(), 2);
        assert!(recorded.runs.iter().all(|of| of.watched), "both are named");
        let lines = recorded.lines();
        let at = |line: &str| {
            lines
                .iter()
                .position(|l| l == line)
                .unwrap_or_else(|| panic!("no {line:?} in {lines:?}"))
        };
        assert!(at("As account 1:") < at("As account 2:"), "{lines:?}");
        assert!(lines[at("As account 1:") + 1].starts_with("  @one last ran"));
        assert!(lines[at("As account 2:") + 1].starts_with("  @two last ran"));

        let health = health(
            Some(&file),
            &recorded.runs,
            &recorded.reported,
            recorded.owed,
            NOW,
            None,
        );
        let document = super::super::wire::status_json(
            true,
            std::path::Path::new("watch.toml"),
            &recorded.owed,
            &health,
            &recorded.runs,
            &recorded.marks,
        );
        let viewers: Vec<_> = document["last_runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| (run["pk"].clone(), run["viewer"]["pk"].clone()))
            .collect();
        assert_eq!(
            viewers,
            [(1.into(), 1.into()), (2.into(), 2.into())],
            "{document}"
        );
    }
}
