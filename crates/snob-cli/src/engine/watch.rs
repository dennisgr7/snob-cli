//! What the monitor found, and what it was allowed to conclude from it.
//!
//! Two ways in. [`from_store`] answers out of what is already on disk and
//! spends nothing, which is what makes `snob watch diff` free to run as often
//! as anybody likes. [`tick`] goes and looks first.
//!
//! `tick` does not have a caching policy of its own, and that is deliberate:
//! it calls [`crate::engine::list`], which is the one place every list in this
//! tool comes out of and which already holds the whole order — cooldown,
//! consent, counter poll, freshness, walk. A second policy here would be a
//! second policy to keep in agreement with the first.
//!
//! Both return data and the interval it covers. What any of it looks like is
//! [`crate::commands::watch`]'s question.

use anyhow::Result;
use snob_core::model::{ListKind, StopReason};
use snob_core::watch::{Basis, Changes, ListDiff, Rename};
use snob_core::{Epoch, Pk};
use snob_store::store::{accounts, snapshots, users, watch as store};

use crate::app::{App, Viewer};
use crate::engine::{self, ListOutcome, ListQuery, Provenance, target};
use crate::exit::ExitCode;

/// What one list has to report.
#[derive(Debug, Clone)]
pub struct ListReport {
    pub kind: ListKind,
    pub basis: Basis,
    /// When the last report about this list was made. `None` when there has
    /// never been one.
    ///
    /// The receipt's moment, not the marked capture's `taken_at`. Those come
    /// apart the moment somebody types `snob followers` between two runs, and
    /// this is the one that says what has actually been said out loud.
    pub since: Option<Epoch>,
    /// When the newest capture was taken.
    pub until: Epoch,
    pub diff: ListDiff,
    /// How many accounts the newest capture holds, so a report can say "three
    /// left, of a hundred and forty" without the caller counting again.
    pub total: usize,
}

impl ListReport {
    /// Whether this run established that the list is still true, which is what
    /// makes a rename among its members worth reporting.
    ///
    /// `Unchanged` counts: a rename moves nobody in or out of a list, so a
    /// capture whose counter was checked and had not moved is exactly as good a
    /// set of members to look for renames among as one that was walked. Only a
    /// baseline does not count -- announcing renames against one would report
    /// moves from before anything was ever reported.
    pub fn verified(&self) -> bool {
        matches!(self.basis, Basis::Compare { .. } | Basis::Unchanged { .. })
    }
}

/// Everything storage can say about one account.
#[derive(Debug, Clone)]
pub struct WatchReport {
    pub account_pk: Pk,
    /// The name, when one was ever learned. Filtered by whoever draws it, not
    /// here: this module returns data.
    pub username: Option<String>,
    pub is_self: bool,
    /// `None` when nothing of that list has ever been walked to completion.
    pub followers: Option<ListReport>,
    pub following: Option<ListReport>,
    pub renamed: Vec<Rename>,
}

impl WatchReport {
    /// Whether the tick found anything, without cloning the diffs.
    ///
    /// [`WatchReport::changes`] clones both lists' diffs and every rename to
    /// answer, and a caller that wants only this bool or the count should not
    /// pay for a copy of every arrival and departure.
    pub fn has_changes(&self) -> bool {
        self.change_count() > 0
    }

    /// How many individual changes, counted the way `Changes::len` counts --
    /// both lists plus the renames -- without building a `Changes`.
    pub fn change_count(&self) -> usize {
        let of = |kind| self.report(kind).map_or(0, |r| r.diff.len());
        of(ListKind::Followers) + of(ListKind::Following) + self.renamed.len()
    }

    /// The changes, in the shape the payload and the printer both want.
    pub fn changes(&self) -> Changes {
        Changes {
            followers: self.diff_of(ListKind::Followers),
            following: self.diff_of(ListKind::Following),
            renamed: self.renamed.clone(),
        }
    }

    fn diff_of(&self, kind: ListKind) -> ListDiff {
        self.report(kind)
            .map(|r| r.diff.clone())
            .unwrap_or_default()
    }

    pub fn report(&self, kind: ListKind) -> Option<&ListReport> {
        match kind {
            ListKind::Followers => self.followers.as_ref(),
            ListKind::Following => self.following.as_ref(),
        }
    }

    /// Whether anything at all has ever been walked for this account.
    ///
    /// Told apart from "nothing changed", which is what the caller would
    /// otherwise print at somebody who has never run the tool on this account.
    pub fn has_anything_stored(&self) -> bool {
        self.followers.is_some() || self.following.is_some()
    }
}

/// How old a stored capture may be and still be served when the counter polled
/// this run says it has not moved, and the shortest gap between two walks of
/// one list when it has: a counter that moves is walked by the first run once
/// the day is up, not at once.
///
/// **It has to be longer than the time between two runs, or it never serves
/// anything**: the next run is due one interval after the previous one
/// finished, plus a jitter that only moves it later. A monitor that re-walks
/// lists on a timer is the most expensive shape this tool has, and Meta's paper
/// on its anti-scraping system (arXiv 2502.17693) weighs a request by the
/// accounts it returns. Not derived from the schedule, because `watch once` is
/// also driven by cron and systemd timers, whose interval this program never
/// sees.
const REUSE_WINDOW: std::time::Duration = super::DEFAULT_MAX_AGE;

/// An account this monitor watches.
///
/// The consent is a field rather than a flag the caller passes, because it is
/// the one thing that decides whether a run may go ahead with nobody at the
/// keyboard. A person typing `snob watch once someone` gets asked, the way
/// every other command asks. An unattended run cannot be asked, so it needs an
/// answer that was already given — and [`Watched::may_run_unattended`] is what
/// says whether it has one. A `yes` the program grants itself is not consent,
/// and there is no constructor here that produces one.
#[derive(Debug, Clone)]
pub struct Watched {
    /// The signed-in account this entry is read as. `None` is the account in
    /// use, which a run with nobody signed in does not have.
    viewer: Option<Pk>,
    /// `None` is the viewer's own account: nothing to agree to.
    target: Option<String>,
    consent: Option<Consent>,
}

/// A recorded answer to "may this walk somebody else's lists?".
///
/// Deliberately not a `bool`. A bool is set to `true` by whatever needs it to be
/// true, and the whole rule is about where the `true` came from — so what
/// reaches the walk is a named type, and `grep Consent` is the complete list of
/// the places one can be produced. Today that list is one:
/// [`Watched::consented`], called only where an `[account.consent]` table was
/// read out of `watch.toml`.
///
/// It carries no moment. A number nothing validates and nothing surfaces is an
/// invitation to build a staleness rule on it, and there is no such rule. The
/// record of *when* stays in `watch.toml`, where `AccountConfig::consent`'s own
/// doc explains why it is a table and not a bool; what this type stands for is
/// that an answer was given, which is what [`Watched::may_run_unattended`] asks
/// and what `check` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consent;

impl Watched {
    /// The account the session belongs to.
    pub fn own() -> Self {
        Self {
            viewer: None,
            target: None,
            consent: None,
        }
    }

    /// Somebody else, with the question still to be asked. What a person
    /// typing the command gets: the prompt is where the answer comes from.
    pub fn asking(username: String) -> Self {
        Self {
            viewer: None,
            target: Some(username),
            consent: None,
        }
    }

    /// Somebody else, with an answer already on record.
    pub fn consented(username: String, consent: Consent) -> Self {
        Self {
            viewer: None,
            target: Some(username),
            consent: Some(consent),
        }
    }

    /// The same entry, read as `viewer`.
    #[must_use]
    pub fn read_as(self, viewer: Option<Pk>) -> Self {
        Self { viewer, ..self }
    }

    /// The signed-in account this is read as; `None` is the account in use.
    /// `self` means whoever this is, never the account in use as such.
    pub fn viewer(&self) -> Option<Pk> {
        self.viewer
    }

    /// Whose account this is, when it is not the viewer's.
    pub fn name(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// Whether this may run with nobody there to answer a question.
    ///
    /// Your own account always may. Somebody else's may only when the answer
    /// was already given, because the alternative is a scheduled service
    /// enumerating a stranger's lists on nobody's say-so.
    pub fn may_run_unattended(&self) -> bool {
        self.target.is_none() || self.consent.is_some()
    }

    /// The arguments a tick runs a list command with.
    ///
    /// The only place `yes` is ever set, and only when a [`Consent`] is on
    /// record. `refresh` stays off because the counter poll is what decides
    /// whether to walk, and `cache` stays off because a promise not to look is
    /// not a monitor.
    fn list_args(&self) -> ListQuery {
        ListQuery {
            target: self.target.clone(),
            yes: self.consent.is_some(),
            refresh: false,
            cache: false,
            // A stored capture whose counter has not moved is still current, so
            // reusing it is right and costs nothing. It reads as `Unchanged`
            // against the mark, which is exactly the answer.
            max_age: REUSE_WINDOW,
            no_resume: false,
            max_pages: None,
            // Pause without the warning: nobody is at a terminal to read it,
            // and a monitor never goes past the day's ceiling.
            over_budget: Some(snob_ig::pager::OverBudget::Pause),
            // One walk a day per list at most. A moved counter between two
            // walks is reported by the next one, a day late at worst.
            walk_at_most_every: Some(REUSE_WINDOW),
        }
    }
}

/// The entries each viewer reads, in the order the file first names that
/// viewer and, within one, in the file's order.
///
/// A run opens one session per group, so the accounts one viewer watches are
/// walked together and `self` in each group is that viewer's own account.
pub fn group_by_viewer(watched: &[Watched]) -> Vec<(Option<Pk>, Vec<Watched>)> {
    let mut groups: Vec<(Option<Pk>, Vec<Watched>)> = Vec::new();
    for entry in watched {
        match groups
            .iter_mut()
            .find(|(viewer, _)| *viewer == entry.viewer)
        {
            Some((_, group)) => group.push(entry.clone()),
            None => groups.push((entry.viewer, vec![entry.clone()])),
        }
    }
    groups
}

/// One list, as this run found it.
#[derive(Debug)]
pub struct TickList {
    pub kind: ListKind,
    /// Why this list could not be compared, when it could not. The provenance
    /// and the stop reason are inside `Skipped`, which is what every caller
    /// matches on.
    pub skipped: Option<Skipped>,
}

/// Why a run declined to draw a conclusion from a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Nothing in this run established that the list still describes the
    /// account. That is the three storage paths where no request was spent
    /// finding out: a cooldown, a failed poll, and `--offline`.
    NobodyLooked(Provenance),
    /// The walk did not finish, so accounts are missing from it — and every one
    /// of them would be reported as somebody who left.
    ///
    /// The second field is what Instagram actually said, when it said
    /// something (`ListOutcome::stopped_by`). `StopReason` is coarser than the
    /// error behind it on purpose, because the store only needs to know whether
    /// the list is usable — but `SessionInvalid` covers both "log in again" and
    /// "the account has to be verified", which are exit code 3 and exit code 4,
    /// so the code cannot be rebuilt from the reason alone. A checkpoint read as
    /// 3 would tell an unattended operator to run `snob login`, which during
    /// the cooldown a challenge causes stores a session without validating it
    /// and says "Session stored"; the same response through `snob followers`
    /// exits 4.
    Incomplete(StopReason, Option<ExitCode>),
}

/// What a tick did.
#[derive(Debug)]
pub struct TickReport {
    /// The signed-in account this run read as.
    pub viewer: Viewer,
    pub report: WatchReport,
    pub requests: u32,
    pub lists: Vec<TickList>,
    /// The captures this report spoke about, and the moment it covers.
    ///
    /// Held rather than recomputed because [`commit`] has to move exactly these
    /// marks and no others: a list that was refused is absent, and so is one the
    /// comparison could not build a report for, and asking the store again
    /// afterwards would find both and mark them anyway.
    committable: Vec<(ListKind, i64)>,
    at: Epoch,
    /// How far along the rename history this report has covered, when it read a
    /// window at all. `None` means the cursor must not move.
    rename_cursor: Option<i64>,
    /// The `username_history` rows this tick is announcing, so no later one
    /// repeats them. A different question from the cursor, for the reason
    /// `007_renames_sent.sql` sets out.
    renames_sent: Vec<i64>,
}

/// Records that this report has been reported.
///
/// Split from [`tick`] so the body can be built in between, which is where it
/// belongs: what a report looks like on the wire is presentation, and `engine`
/// does not decide how anything looks. What is not split is the writing —
/// queueing the report and retiring the marks happen in one transaction, for
/// the reason `store::watch::commit_report` sets out.
pub fn commit(
    app: &mut App,
    tick: &TickReport,
    delivery: Option<store::Queued<'_>>,
) -> Result<Option<i64>> {
    let pk = tick.report.account_pk;
    let at = tick.at;
    let (_, db, _) = app.parts();

    let queued = store::commit_report(
        db,
        pk,
        &tick.committable,
        at,
        tick.rename_cursor,
        &tick.renames_sent,
        // The store's own type, not a copy mapped across: the compiler would
        // say nothing about a field added here that a mapper drops.
        delivery,
    )?;

    // Recorded whatever came of it, including a run that concluded nothing.
    // The marks only move when a list was compared, so a monitor sitting in a
    // cooldown for two days moves none of them — and from outside that looks
    // exactly like a monitor that was killed on Monday. This is what lets
    // `status` tell them apart.
    //
    // Not part of the transaction above: that one exists so a report cannot be
    // retired without being queued, and a bookkeeping row has no business being
    // able to fail it.
    let record = store::record_run(
        db.conn(),
        &store::Run {
            account_pk: pk,
            started_at: at,
            finished_at: Some(snob_core::clock::now()),
            requests: tick.requests,
            outcome: Some(tick.outcome().into()),
            changes: tick.report.change_count() as u32,
        },
    );
    if let Err(e) = record {
        tracing::warn!(error = %e, "the run could not be recorded");
    }

    Ok(queued)
}

/// Expires what nothing needs any more: old captures, reports too old to be
/// news, and finished runs.
///
/// **Once per run, beside the queue drain**, not at the end of a comparison:
/// `commit` is reached only through the delivery step, so a run that ended
/// earlier (a session gone, a third party gone private) would leave a report
/// past `deliveries::MAX_AGE_SECS` sitting `pending` for ever. `due` will not
/// hand back an over-age row and `failed` is the only other thing that expires
/// one.
///
/// Takes the store rather than the `App`, because that is all it touches, and
/// because the caller that most needs it has no session to build an `App`
/// from: `snob watch once` on a machine whose session has gone returns before
/// anything is opened.
///
/// A failure here does not fail the run. Whatever the run did is already
/// recorded; a database that could not be tidied is worth a line in the log and
/// nothing more.
///
/// Answers with the number of reports the sweep gave up on, because that one
/// is not housekeeping: the change it carried is gone, and this is the last
/// moment anybody can be told. Everything else it did stays in a trace. It
/// returns the count rather than printing it for the reason the module header
/// gives -- `engine` says what happened and `commands` decides how it reads.
pub fn settle(db: &snob_store::store::Store, at: Epoch) -> usize {
    match store::prune(db.conn(), at) {
        Ok(swept) => {
            if swept.captures > 0 {
                tracing::debug!(
                    removed = swept.captures,
                    "expired what nothing needs any more"
                );
            }
            swept.given_up
        }
        Err(e) => {
            tracing::warn!(error = %e, "old captures could not be expired");
            0
        }
    }
}

/// Retention for every command that is not the monitor, at most once a day.
///
/// Without it somebody who never sets the monitor up has no retention at all:
/// every walk stores a capture, nothing expires one, and the database grows
/// without end -- modeled at 22 MB after a year of daily use against the
/// ~8 MB plateau a monitor user settles at, and rising linearly after that.
///
/// **The gate is a stored timestamp rather than a flag**, so an ordinary
/// invocation costs one `SELECT` on a table with one row and nothing else. It
/// is checked here, in `App::open`, because that is the one moment every
/// command passes through -- the same reasoning that already puts
/// `refresh_user_agent` here.
///
/// The count of reports given up on is deliberately dropped. Outside the
/// monitor there is nobody it would mean anything to: `snob followers` did not
/// queue it and cannot say anything useful about it, and the monitor's own
/// settle reports it the next time it runs.
pub fn settle_daily(db: &snob_store::store::Store) {
    const KEY: &str = "settled_at";
    const A_DAY: i64 = 24 * 3_600;

    let now = snob_core::clock::now();
    let last = db
        .remembered(KEY)
        .ok()
        .flatten()
        // The one row this is kept in is TEXT, so the moment is parsed back
        // out of it here, at the boundary, and nothing further in handles a
        // number.
        .and_then(|v| v.parse::<i64>().ok())
        .map_or(Epoch::default(), Epoch::new);
    if now - last < A_DAY {
        return;
    }

    let _ = settle(db, now);
    if let Err(e) = db.remember(KEY, &now.to_string()) {
        // Not worth failing an ordinary command over. The worst case is that
        // the sweep runs again on the next one.
        tracing::debug!(error = %e, "when retention last ran could not be recorded");
    }
}

/// Best-effort retention for one account's database, opened here, so it needs
/// no session: a run settles every account's this way whatever its viewers
/// did. One that is gone is left gone, and one that cannot be opened is not
/// worth turning into a different failure.
pub fn settle_account(paths: &snob_store::paths::AccountPaths, at: Epoch) -> usize {
    match snob_store::store::Store::open_existing(paths) {
        Ok(Some(db)) => settle(&db, at),
        Ok(None) => 0,
        Err(e) => {
            tracing::warn!(error = %e, "the database could not be opened to settle it");
            0
        }
    }
}

impl TickReport {
    /// A report that never ran, for a test that only cares what it looks like,
    /// read as the account it reports on.
    ///
    /// The three fields it leaves empty are the ones that say what to commit,
    /// and they are private precisely so nothing outside this module can decide
    /// that — a caller that could set `committable` could retire the mark of a
    /// list the run refused.
    #[cfg(test)]
    pub(crate) fn for_test(report: WatchReport, requests: u32, at: Epoch) -> Self {
        Self {
            viewer: Viewer {
                pk: report.account_pk,
                username: report.username.clone(),
            },
            report,
            requests,
            lists: Vec::new(),
            committable: Vec::new(),
            at,
            rename_cursor: None,
            renames_sent: Vec::new(),
        }
    }

    /// When this run concluded.
    ///
    /// Read once, inside [`tick`], just before the comparison, and handed out
    /// rather than read again: this is the moment `commit_report` files the
    /// mark at, and a body or a stream line that asked the clock a second time
    /// would put a different moment on the same event. The field stays
    /// private, so only a real tick can decide it.
    pub fn at(&self) -> Epoch {
        self.at
    }

    /// Whether this run established anything at all.
    ///
    /// A caller that sends the result somewhere has to be able to say "nothing
    /// changed" apart from "I could not look", because an automation watching
    /// for silence reads them as the same thing and they are opposites.
    pub fn looked(&self) -> bool {
        self.lists.iter().any(|l| l.skipped.is_none())
    }

    /// What this run amounts to, in the vocabulary `$?` and the README's table
    /// already use.
    ///
    /// Recorded rather than reconstructed later, because the reasons a list was
    /// refused are held on the run and nothing else keeps them. A run that
    /// could not look at either list is reported as what stopped it, so
    /// `status` can say "in cooldown" rather than only "quiet since Monday".
    pub fn outcome(&self) -> ExitCode {
        if self.looked() {
            return ExitCode::Ok;
        }
        refused_outcome(&self.lists)
    }
}

/// The most specific reason every list on a run was refused. A cooldown is
/// something that lifts, and saying so is more use than "error".
///
/// Shared between [`TickReport::outcome`] and the refusal `tick` raises when
/// it has no stored account to report against, so the two cannot drift:
/// `snob watch once someone` during a cooldown exits the same way whether or
/// not that account had ever been walked before.
fn refused_outcome(lists: &[TickList]) -> ExitCode {
    lists
        .iter()
        .find_map(|list| match list.skipped {
            Some(Skipped::NobodyLooked(Provenance::Cooldown)) => Some(ExitCode::RateLimited),
            // What Instagram said beats what the store had to record, which
            // is the rule `ListOutcome::stopped_by` exists for and the one
            // `exit::from_stop_reason` names in its own doc: whichever of
            // the two a command happens to read must not change the answer.
            Some(Skipped::Incomplete(reason, said)) => {
                Some(said.unwrap_or_else(|| crate::exit::from_stop_reason(reason)))
            }
            _ => None,
        })
        .unwrap_or(ExitCode::Error)
}

/// Goes and looks, then reports what changed since the last time it did.
///
/// The order is the whole of it:
///
/// 1. Get both lists through [`crate::engine::list`], which decides on its own
///    whether anything has to be fetched. One request when nothing moved: one
///    profile answer carries both counters.
/// 2. Refuse to conclude anything from a list this run did not verify, or one
///    that came back short. **The mark does not move for a refused list**: what
///    was never reported stays unreported, and the next run says it.
/// 3. Compare what is left against the receipt, and move it.
pub async fn tick(app: &mut App, watched: &Watched) -> Result<TickReport> {
    let args = watched.list_args();
    let before = app.client().pacer().spent();
    // A tick is an action: counters a previous tick read, on this App, are
    // not this tick's.
    app.client().pacer().begin_action();

    let mut lists = Vec::new();
    let mut usable = Vec::new();
    // Whose account this turned out to be, taken from the engine's answer
    // rather than from the viewer: on a third party they are different, and the
    // id the engine reports is the one that cannot be wrong about it.
    //
    // `None` until a list answers, and not the viewer's id as a stand-in. The
    // two arms below that `continue` learn nothing about the account, and when
    // both lists take one of them (a cooldown on a freshly added account with
    // no capture, a Ctrl+C before the first page) a stand-in would file the
    // comparison, the report's `account.pk` and the `watch_runs` row under the
    // viewer's id, displacing that account's own newest row in `status`. What
    // is stored about the name is the fallback, and when nothing is, the run
    // says so instead of guessing.
    let mut pk: Option<Pk> = None;

    for kind in [ListKind::Followers, ListKind::Following] {
        // A canceled walk comes back `Ok`, so without this the loop would go
        // on to the second list, and `App::resolved_target` drops the
        // remembered counters once the first walk has begun, so it would ask
        // Instagram again after the user was told "Stopping and saving what has
        // been fetched…".
        //
        // Recorded as a refusal rather than dropped: a list this run never
        // looked at must not be compared or marked, which is exactly what
        // `Skipped` means, and saying so keeps the report honest about why it
        // is short.
        if app.cancel().is_canceled() {
            lists.push(TickList {
                kind,
                skipped: Some(Skipped::Incomplete(StopReason::Canceled, None)),
            });
            continue;
        }

        // A cooldown stops *this list*; everything else stops the run. Were a
        // cooldown on the second list an `Err` of the run, a completed
        // followers walk whose diff names three departures would be reported a
        // whole interval late, and no `watch_runs` row written.
        //
        // Classified rather than blanket-caught. Mapping every `Err` to a
        // refusal would swallow a consent refusal, a session that has gone and
        // a challenge as "one list was skipped", turning the failures a run
        // must surface into a short report nobody notices. `RateLimited` is the
        // one genuinely scoped to what could be read now that lifts on its own,
        // which is what `Skipped` describes, and it is what both
        // `refuse_in_cooldown` and `refuse_cooldown_mid_walk` carry.
        //
        // A page that has not shown what a call is built from yet
        // (`IgError::PageNotReady`) is one of the everything else: it ends
        // the run with exit 1, writes no cooldown and marks nothing, and the
        // next run tries again. A document served logged out is an expired
        // session, exit 3.
        let outcome = match engine::list(app, &args, kind).await {
            Ok((_, outcome)) => outcome,
            Err(e) if crate::exit::from_chain(&e) == Some(ExitCode::RateLimited) => {
                lists.push(TickList {
                    kind,
                    skipped: Some(Skipped::Incomplete(
                        StopReason::RateLimit,
                        Some(ExitCode::RateLimited),
                    )),
                });
                continue;
            }
            Err(e) => return Err(e),
        };
        pk = Some(outcome.account_pk);

        let skipped = refusal(&outcome);
        if skipped.is_none() {
            usable.push((kind, outcome.snapshot_id));
        }
        lists.push(TickList { kind, skipped });
    }

    let pk = match (pk, watched.name()) {
        (Some(pk), _) => pk,
        (None, None) => app.viewer().pk,
        (None, Some(name)) => {
            let name = target::clean(name);
            // The refusal carries the code `outcome()` would have produced,
            // not a bare error, so a first tick during a cooldown exits 5 like
            // the same tick on a walked account, and the `--json` stream does
            // not say `"error"` about a state that lifts on its own.
            accounts::find_pk_by_username(app.db().conn(), name)?.ok_or_else(|| {
                crate::report::refuse_nothing_looked_at(name, refused_outcome(&lists))
            })?
        }
    };

    // Read before the comparison, and both handed back, so that whatever
    // commits this report writes the same two numbers the renames were read
    // against. Read again at commit time, a rename filed in between would be
    // marked as reported without having been.
    let at = snob_core::clock::now();
    let head = store::history_head(app.db().conn())?;

    // Nothing is written here. The marks and the cursor move in `commit`,
    // together with the report being queued — reporting and recording having
    // reported are one event.
    let compared = compare(app, pk, &usable, head)?;

    Ok(TickReport {
        viewer: app.viewer().clone(),
        report: compared.report,
        requests: app.client().pacer().spent().saturating_sub(before),
        lists,
        committable: compared.marks,
        at,
        rename_cursor: compared.rename_cursor,
        renames_sent: compared.renames_sent,
    })
}

/// Whether a list may be the basis of a comparison, and why not when it may not.
///
/// Both questions are asked of the outcome rather than worked out here.
/// `describes_now` is the existing answer to "did anything in this run
/// establish that this list is still true", and the three provenances that say
/// no are the three where no request was spent finding out — which is what
/// makes them safe to print and unsafe to compare.
fn refusal(outcome: &ListOutcome) -> Option<Skipped> {
    if !outcome.provenance.describes_now() {
        return Some(Skipped::NobodyLooked(outcome.provenance));
    }
    if !outcome.is_complete() {
        // `stopped_by` rather than the reason alone. It is `None` when nothing
        // Instagram said stopped the walk — a `--max-pages` cut, say — and the
        // reason is the whole answer there.
        return Some(Skipped::Incomplete(outcome.reason, outcome.stopped_by));
    }
    None
}

/// Reads the report without touching the network, and without recording it.
///
/// **It takes `&App`, and that is the guard rather than a convention.**
/// Recording a report means `commit_report`, which needs a `&mut Store`; there
/// is no way to reach one from a shared borrow, so this entry point is
/// incapable of moving a mark.
///
/// A separate entry point rather than a flag on a shared one, because
/// `snob watch diff`'s whole contract is that asking twice gives the same
/// answer, and a flag is one literal away from breaking it.
pub fn from_store(app: &App, typed: Option<&str>) -> Result<WatchReport> {
    Ok(look(app, typed)?.1.report)
}

/// Reads the report and records having made it, the way a tick does.
///
/// **Tests only.** Nothing in production wants this: a report that was recorded
/// but never sent anywhere is a window nobody will ever hear about again. It
/// exists so a test can put an account into "already reported" state without a
/// network to fetch a walk from, and it writes through the same `commit_report`
/// a tick writes through, so there is one writer of marks in the program rather
/// than two that can come to disagree about which lists a report spoke about.
#[doc(hidden)]
pub fn record_from_store(app: &mut App, typed: Option<&str>) -> Result<WatchReport> {
    let (pk, compared) = look(app, typed)?;

    let (_, db, _) = app.parts();
    store::commit_report(
        db,
        pk,
        &compared.marks,
        snob_core::clock::now(),
        compared.rename_cursor,
        &compared.renames_sent,
        None,
    )?;

    Ok(compared.report)
}

/// The comparison both entry points make, and what committing it would write.
fn look(app: &App, typed: Option<&str>) -> Result<(Pk, Compared)> {
    let (pk, username) = resolve(app, typed)?;

    // From the view, so an interrupted walk cannot become a basis for
    // comparison. It is the same guard the static crossings already lean on,
    // and it matters more here: the accounts missing from a half-walked list
    // would be reported as people who left.
    let mut usable = Vec::new();
    for kind in [ListKind::Followers, ListKind::Following] {
        if let Some(latest) = snapshots::latest_complete(app.db().conn(), pk, kind)? {
            usable.push((kind, latest.id));
        }
    }

    // Read before the comparison, so a rename filed while it runs lands on the
    // next report's side rather than being marked as said without having been
    // said.
    let head = store::history_head(app.db().conn())?;
    let mut compared = compare(app, pk, &usable, head)?;
    compared.report.username = username;
    Ok((pk, compared))
}

/// Compares each named capture against its receipt.
///
/// The one place a comparison is made, so that a tick and a plain look cannot
/// disagree about what "since the last report" means. The captures are named by
/// the caller because the two callers know them differently: a tick has the id
/// the walk it just ran produced, which is the only id that is certainly the
/// one those users came from, while a look asks the store for the newest.
///
/// This writes nothing. It hands back what a commit would have to write, and
/// the two callers commit it differently — a tick together with the report it
/// queues, a plain look not at all.
///
/// `head` is the rename history's newest row, read by the caller **before** this
/// runs: a rename filed while the comparison is in progress then lands on the
/// next report's side rather than being filed as said without having been said.
fn compare(app: &App, pk: Pk, usable: &[(ListKind, i64)], head: i64) -> Result<Compared> {
    let mut followers = None;
    let mut following = None;
    for &(kind, snapshot_id) in usable {
        let report = list_report(app, pk, kind, snapshot_id)?;
        match kind {
            ListKind::Followers => followers = report,
            ListKind::Following => following = report,
        }
    }

    // Renames are looked for among the members of **every list this run
    // verified**: somebody only in `following` is exactly the `unfollowers`
    // set. `verified` rather than `compared`, so an `Unchanged` list counts,
    // for the reason [`ListReport::verified`] gives; that is the common case of
    // two unmoved counters and one request.
    //
    // One cursor for the account, not one per list, so there are no two numbers
    // to pick between. Anything that turns up is deduplicated by `pk`: a friend
    // is in both lists and is one person.
    let verified: Vec<&ListReport> = [followers.as_ref(), following.as_ref()]
        .into_iter()
        .flatten()
        .filter(|report| report.verified())
        .collect();

    // What this run announces, and the history rows behind them.
    //
    // **Already-sent is asked per row, not per watermark** (`007_renames_sent`).
    // Whether a rename is reported and whether the cursor may move are
    // independent conditions: a tick with one list refused announces what it
    // can see and must not move the cursor, so the next tick re-reads the same
    // window and must not announce the same rename again under a fresh
    // `run_id`; and a rename of somebody only in the refused list sits inside
    // that same window, so any watermark that suppressed the first would bury
    // the second for ever.
    let mut renamed: Vec<Rename> = Vec::new();
    let mut announcing: Vec<i64> = Vec::new();
    if !verified.is_empty() {
        let since = store::rename_cursor(app.db().conn(), pk)?;
        let already = store::renames_already_sent(app.db().conn(), pk)?;
        let mut seen = std::collections::HashSet::new();
        for report in &verified {
            for rename in
                store::renames_since(app.db().conn(), report.basis.mark_to(), since, head)?
            {
                if already.contains(&rename.history_id) {
                    continue;
                }
                if seen.insert(rename.pk) {
                    announcing.push(rename.history_id);
                    renamed.push(rename);
                }
            }
        }
    }

    // Whether the cursor may move, which is a different question from whether
    // there was anything to report.
    //
    // Only when **every list this account has a capture of** is accounted for.
    // `renames_since` sees only the members of the lists that were verified,
    // and there is one cursor, so moving it past an unverified list would step
    // over a rename of somebody only in that list for good: a rename moves
    // nobody in or out of a list, so no later run would surface it
    // (`006_rename_cursor.sql`).
    //
    // - A list never captured does not hold the cursor back: there are no
    //   members to have missed a rename among. "Captured" is `any_capture`, not
    //   `latest_complete`: a walk that ends `Truncated` is no basis for a
    //   comparison, but its `save_page` ran `users::upsert` and filed history
    //   rows for the members it did see.
    // - A baseline accounts for its list only on the account's **first**
    //   report: the monitor starts now, and history from before it is not news.
    //   A baseline later on is a walled list finally completing, and the run
    //   that can see the renames owed to it is the one *after* it, so counting
    //   it would close the window first. "First" is asked of the marks, not of
    //   the captures: `delete_partials` clears a list's partials whenever a new
    //   walk starts, so counting captures says "one" on the fifth attempt too.
    // - A run that reported on no list read no window, and moves nothing.
    //
    // A permanently walled list therefore holds the window open, and it grows.
    // That is the honest direction, since the alternative is losing what is in
    // it, and it is affordable because `007_renames_sent` means a window read
    // twice sends nothing twice.
    let verified_kinds: Vec<ListKind> = verified.iter().map(|report| report.kind).collect();
    // The lists this report actually spoke about: what the marks name, and the
    // one answer to "which lists did this run report on" that the cursor below
    // reads too.
    //
    // Built here rather than by the caller, because `list_report` can still
    // answer with nothing (a capture that turns out not to be usable, a
    // baseline another process pruned in between), and a mark moved over a
    // list nothing was said about loses that window for good.
    //
    // **The cursor hangs off this list and not off a second copy of it.**
    // Narrowing one copy alone would leave the cursor closing a window over a
    // list no mark was written for; narrowed here, the cursor declines to move
    // and the next run says what this one did not.
    let marks: Vec<(ListKind, i64)> = [followers.as_ref(), following.as_ref()]
        .into_iter()
        .flatten()
        .map(|report| (report.kind, report.basis.mark_to()))
        .collect();

    let first_report = store::mark(app.db().conn(), pk, ListKind::Followers)?.is_none()
        && store::mark(app.db().conn(), pk, ListKind::Following)?.is_none();

    let mut every_list_accounted_for = true;
    for kind in [ListKind::Followers, ListKind::Following] {
        if verified_kinds.contains(&kind) {
            continue;
        }
        if !snapshots::any_capture(app.db().conn(), pk, kind)? {
            continue; // nothing was ever captured, so nothing was missed
        }
        if first_report && marks.iter().any(|&(marked, _)| marked == kind) {
            continue; // the seeding baseline
        }
        every_list_accounted_for = false;
    }

    // Nothing filed after `head` is inside the window: `renames_since` enforces
    // it.
    let covered = (!marks.is_empty() && every_list_accounted_for).then_some(head);

    Ok(Compared {
        report: WatchReport {
            account_pk: pk,
            username: users::name(app.db().conn(), pk)?,
            is_self: app.viewer().pk == pk,
            followers,
            following,
            renamed,
        },
        marks,
        rename_cursor: covered,
        renames_sent: announcing,
    })
}

/// A comparison, and what committing it would have to write.
///
/// The three come out together because they have to agree: the marks name the
/// lists the report spoke about, and the cursor is only set when the report read
/// a rename window. Working either out again at the point of writing would let
/// them come apart.
struct Compared {
    report: WatchReport,
    marks: Vec<(ListKind, i64)>,
    rename_cursor: Option<i64>,
    renames_sent: Vec<i64>,
}

fn list_report(app: &App, pk: Pk, kind: ListKind, snapshot_id: i64) -> Result<Option<ListReport>> {
    let conn = app.db().conn();
    // `find_usable`, not `find`: an id that names a walk which stopped short
    // answers `None` here rather than handing back a capture with accounts
    // missing from it.
    let Some(latest) = snapshots::find_usable(conn, snapshot_id)? else {
        return Ok(None);
    };

    let mark = store::mark(conn, pk, kind)?;
    // A receipt whose capture has been pruned carries no baseline, so it is
    // handed to `decide` as the absence it is and the next report starts over.
    // What survives is the history cursor, so the renames are not re-announced.
    let Some(basis) = Basis::decide(mark.and_then(|m| m.snapshot_id), Some(latest.id)) else {
        return Ok(None);
    };

    let diff = match basis {
        // Nothing is reported and nothing is read: a baseline has no earlier
        // capture, and an unchanged list has nothing new in it. Not reading the
        // members is the whole reason the common case is cheap.
        Basis::Baseline { .. } | Basis::Unchanged { .. } => ListDiff::default(),
        Basis::Compare { before, after } => {
            // The baseline is read through `find_usable` too, not just the
            // newer capture. `members` answers `Ok([])` for an id that is no
            // longer there, which is indistinguishable from a capture that was
            // genuinely empty — so a baseline pruned by another process between
            // reading the mark and reading its members would make every member
            // of the newer capture look like an arrival.
            //
            // Within one process this is impossible: pruning a marked capture
            // sets the mark to NULL and `Basis::decide` answers `Baseline`. It
            // takes a second process's `prune` landing in between.
            let Some(_) = snapshots::find_usable(conn, before)? else {
                return Ok(None);
            };
            ListDiff::between(
                &snapshots::members(conn, before)?,
                &snapshots::members(conn, after)?,
            )
        }
    };

    Ok(Some(ListReport {
        kind,
        basis,
        since: mark.map(|m| m.compared_at),
        until: latest.taken_at.unwrap_or_default(),
        diff,
        total: latest.member_count as usize,
    }))
}

/// Which account this is about, from storage alone.
///
/// Deliberately not `target::from_store`: that one refuses with
/// `report::refuse_nothing_stored`, a sentence about `--offline`, which is a flag
/// this command does not have. `report::refuse_never_walked` is the one that
/// belongs here, and it carries the difference between the two.
fn resolve(app: &App, typed: Option<&str>) -> Result<(Pk, Option<String>)> {
    let Some(typed) = typed else {
        let viewer = app.viewer();
        return Ok((viewer.pk, viewer.username.clone()));
    };

    let name = target::clean(typed);
    let Some(pk) = accounts::find_pk_by_username(app.db().conn(), name)? else {
        return Err(crate::report::refuse_never_walked(name));
    };

    Ok((pk, users::name(app.db().conn(), pk)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(provenance: Provenance, reason: StopReason) -> ListOutcome {
        ListOutcome {
            requests: 0,
            started_at: Epoch::new(1_000),
            taken_at: Epoch::new(1_100),
            account_pk: Pk::new(42),
            snapshot_id: 7,
            ..ListOutcome::for_test(provenance, reason)
        }
    }

    /// A capture with an unchanged counter is served on the schedule people
    /// actually write: the next run is due an interval after the previous one
    /// plus a jitter that only adds, so a window equal to the interval would
    /// always be just too short. Written against the schedule itself, so a
    /// longer jitter or a shorter window fails here.
    #[test]
    fn an_unchanged_capture_outlives_the_gap_to_the_next_run() {
        assert_eq!(REUSE_WINDOW, std::time::Duration::from_secs(24 * 3600));
        assert_eq!(
            watched_own_max_age(),
            REUSE_WINDOW,
            "the tick is what reads it"
        );

        let every = std::time::Duration::from_secs(6 * 3600);
        let schedule = snob_core::watch::schedule::Schedule::every(every).unwrap();
        // A list's capture is taken when its walk ends, and the run then walks
        // the other list before it finishes. The longest that second walk can
        // be without pausing for the day is a day's accounts at the slowest
        // the default pace allows: 2,000 at 12 a page is 167 pages of up to
        // 3 s, plus four sitting rests of up to 15 minutes and the look at
        // the profile before the first page, about 70 minutes.
        let pace = snob_ig::pace::Pace::default();
        let accounts = snob_core::budget::accounts_per_day(None, snob_core::clock::EpochMs::new(0));
        let pages = u64::from(accounts.div_ceil(pace.per_page));
        let rests = pages / u64::from(pace.pages_per_sitting);
        let longest_walk = std::time::Duration::from_millis(
            pace.dwell_ms.1 + pages * pace.step_ms.1 + rests * pace.sitting_pause_ms.1,
        );
        let worst_gap = every + schedule.jitter() + longest_walk;
        assert!(
            REUSE_WINDOW > worst_gap,
            "a capture taken on one run has to still be servable on the next: {worst_gap:?}"
        );
    }

    fn watched_own_max_age() -> std::time::Duration {
        Watched::own().list_args().max_age
    }

    /// A walk that came back short is refused, and refused as *incomplete*
    /// rather than as unverified.
    ///
    /// The two halves of `refusal` mean different things to the caller: nobody
    /// looked is something the next run fixes, while a truncated walk is a list
    /// whose missing accounts would read as people who left.
    #[test]
    fn a_walk_that_came_back_short_is_refused_as_incomplete() {
        for reason in [
            StopReason::RateLimit,
            StopReason::Truncated,
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            assert!(
                matches!(
                    refusal(&outcome(Provenance::Walked, reason)),
                    Some(Skipped::Incomplete(got, _)) if got == reason
                ),
                "a walk that ended {reason:?} was accepted as a basis for comparison"
            );
        }

        // A list nothing verified is the other refusal, and it wins: the run
        // never looked, so what the stored capture says about completeness is
        // beside the point.
        assert!(matches!(
            refusal(&outcome(Provenance::Cooldown, StopReason::Completed)),
            Some(Skipped::NobodyLooked(Provenance::Cooldown))
        ));

        assert!(
            refusal(&outcome(Provenance::Walked, StopReason::Completed)).is_none(),
            "a complete walk this run made is exactly what may be compared"
        );
    }

    /// And the run says so in the one place a timer can read without parsing
    /// English.
    #[test]
    fn a_run_whose_lists_all_came_back_short_does_not_exit_zero() {
        let report = WatchReport {
            account_pk: Pk::new(42),
            username: None,
            is_self: true,
            followers: None,
            following: None,
            renamed: Vec::new(),
        };
        let mut tick = TickReport::for_test(report, 1, Epoch::default());
        tick.lists = vec![TickList {
            kind: ListKind::Followers,
            skipped: Some(Skipped::Incomplete(
                StopReason::RateLimit,
                Some(ExitCode::RateLimited),
            )),
        }];

        assert!(!tick.looked());
        assert_eq!(
            tick.outcome(),
            crate::exit::from_stop_reason(StopReason::RateLimit)
        );
    }

    /// And a checkpoint is reported as a checkpoint, not as "log in again":
    /// `StopReason::SessionInvalid` covers exit codes 3 and 4, so the code
    /// Instagram's answer carried wins over the one rebuilt from the reason. A 3
    /// sends an unattended operator to `snob login`, which during the cooldown a
    /// challenge causes stores a session without validating it and says
    /// "Session stored".
    #[test]
    fn a_challenge_keeps_its_own_code_through_a_tick() {
        let short = |said| {
            let report = WatchReport {
                account_pk: Pk::new(42),
                username: None,
                is_self: true,
                followers: None,
                following: None,
                renamed: Vec::new(),
            };
            let mut tick = TickReport::for_test(report, 1, Epoch::default());
            tick.lists = vec![TickList {
                kind: ListKind::Followers,
                skipped: Some(Skipped::Incomplete(StopReason::SessionInvalid, said)),
            }];
            tick
        };

        assert_eq!(
            short(Some(ExitCode::Challenge)).outcome(),
            ExitCode::Challenge,
            "what Instagram said beats what the store had to record"
        );
        assert_eq!(
            short(Some(ExitCode::NoSession)).outcome(),
            ExitCode::NoSession,
            "and the other half of the same reason still answers 3"
        );
        assert_eq!(
            short(None).outcome(),
            crate::exit::from_stop_reason(StopReason::SessionInvalid),
            "with nothing said, the reason is the whole answer"
        );
    }
}
