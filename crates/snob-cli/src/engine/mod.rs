//! Getting a list, and everything that decides how.
//!
//! This is the part of the tool that has rules rather than opinions: who is
//! being asked about, whether the network has to be touched at all, what a
//! walk costs, and when a stored answer is still true. It returns data and a
//! description of where the data came from; it never decides how any of it
//! looks. That is [`crate::commands`]' job.
//!
//! Every list, crossing and summary the tool prints comes out of [`list`];
//! what only reads captures it stored, without walking, includes
//! `freshness::fresh_members`, [`people::in_common`] and the monitor's
//! comparison in [`watch`].

/// How old a stored capture whose counter has not moved may be and still be
/// served: a day. `--max-age`'s default, the profile browser's and the
/// monitor's.
///
/// Longer than any schedule's interval plus its jitter, so the monitor serves
/// an unchanged capture on its next run instead of walking it again; and short
/// enough to catch, within a day, the change a counter cannot see (one arrival
/// and one departure between two looks).
pub const DEFAULT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

pub mod check;
pub mod cooldown;
pub mod freshness;
pub mod people;
pub mod target;
pub mod walk;
pub mod watch;

use anyhow::Result;
use snob_core::model::{ListKind, StopReason, User};
use snob_core::{Epoch, Pk};
use snob_store::store::{accounts, snapshots, users};

use crate::app::App;
use crate::exit::ExitCode;
use crate::ui;

/// Where a returned list came from, in the sense that decides whether two of
/// them may be crossed against each other.
///
/// The distinction that matters is not how old a stored list is. It is whether
/// anything in **this run** established that it still describes the account:
/// two lists a month apart are fine to cross if both counters were checked just
/// now and neither had moved, and two lists an hour apart are not fine if
/// nobody looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Walked in this run. It is the account as it is.
    Walked,
    /// Stored, and a counter poll in this run said it had not moved.
    CounterVerified,
    /// Stored, served during a cooldown. Nothing may be spent to check.
    Cooldown,
    /// Stored, served because the counter poll failed. No evidence either way.
    PollFailed,
    /// Stored, served because `--offline` said not to look.
    CacheFlag,
    /// Stored, served because the counter moved but the list was walked less
    /// than [`ListQuery::walk_at_most_every`] ago. The change is real and is
    /// left for a later walk to read: a monitor re-walking a list every time a
    /// counter ticks is the volume Instagram judges an account on.
    WalkedRecently,
}

impl Provenance {
    /// Whether this list is known to describe the account as it is now.
    ///
    /// The four that answer `false` are the four where nothing in this run
    /// read the list: during a cooldown, after a failed poll or under
    /// `--offline` nothing was spent finding out, and for `WalkedRecently` the
    /// counter was polled and had moved, but the list was left for a later
    /// walk. That is what makes them safe to *serve* and unsafe to *cross*.
    pub fn describes_now(self) -> bool {
        matches!(self, Self::Walked | Self::CounterVerified)
    }
}

#[derive(Debug)]
pub struct ListOutcome {
    /// How this list was obtained, and therefore what is known about whether
    /// it is still true. Whether it is stored falls out of it
    /// ([`Self::is_stored`]), which is why there is no second field that could
    /// disagree with this one.
    pub provenance: Provenance,
    pub reason: StopReason,
    pub requests: u32,
    /// When the walk that produced this list **began**.
    ///
    /// Half of an interval. A list is not an instant: walking six thousand
    /// accounts at the documented pace takes hours of sittings and rests, and
    /// days under the daily ceiling on accounts, and whether two lists
    /// describe one moment is a question about the time between the two
    /// walks, not between the two moments they happened to finish at.
    pub started_at: Epoch,
    /// When it finished. The date shown to a person, and the one the store
    /// orders by.
    pub taken_at: Epoch,
    /// Whose list this is.
    ///
    /// The caller asked with a name and gets back an id, which is the only
    /// answer that cannot be wrong: a name has to be resolved, may not be the
    /// spelling Instagram uses, and — for your own account — may not be known
    /// at all until something goes and looks it up.
    pub account_pk: Pk,
    /// The stored capture these users came out of.
    ///
    /// Carried rather than looked up afterwards. The monitor has to know which
    /// row this is to compare it against the one it last reported, and asking
    /// the store for "the newest one" after the fact is a different question:
    /// another process sharing this database — the very thing the request
    /// budget is built to expect — can have closed a walk in between, and the
    /// answer would then name a capture these users did not come from.
    pub snapshot_id: i64,
    /// What Instagram actually said, when a walk stopped because it said
    /// something.
    ///
    /// [`StopReason`] is coarser than the error behind it on purpose — the
    /// store only needs to know whether the list is usable. But
    /// `SessionInvalid` covers both "log in again" and "Instagram wants the
    /// account verified", and those are exit code 3 and exit code 4, which a
    /// script reading the exit code has to be able to tell apart without
    /// reading English.
    pub stopped_by: Option<ExitCode>,
    /// Whether a second run would continue this walk rather than start it
    /// again.
    ///
    /// Asked of the store once the snapshot is closed, rather than worked out
    /// from [`Self::reason`], because the reason does not know: `Truncated`
    /// arrives both from the reclassification that happens after pagination has
    /// already ended — no cursor to store — and from the four guards that stop
    /// in the middle of it with one saved. Only the store can tell those apart,
    /// and only the store knows whether the partial has already aged out of the
    /// resume window.
    pub resumable: bool,
}

impl ListOutcome {
    /// What a stored snapshot answers with. Complete by construction: the
    /// store only ever hands back snapshots that are.
    ///
    /// The provenance is not defaulted here: every caller has to say which of
    /// the storage paths it is, because that is what decides whether the list
    /// may be crossed.
    ///
    /// The row itself rather than fields picked out of it, so the two epochs
    /// cannot be transposed and the account cannot come from anywhere but the
    /// row the members come from.
    pub(crate) fn cached(snapshot: &snapshots::Snapshot, provenance: Provenance) -> Self {
        debug_assert!(
            !matches!(provenance, Provenance::Walked),
            "a stored list was not walked"
        );
        Self {
            provenance,
            reason: StopReason::Completed,
            // Filled in by `list`, which is the only place that sees the whole
            // run and can ask the pacer what it really charged.
            requests: 0,
            started_at: snapshot.started_at,
            // The view this comes from cannot return an open snapshot, so the
            // fallback is unreachable.
            taken_at: snapshot.taken_at.unwrap_or_default(),
            account_pk: snapshot.account_pk,
            snapshot_id: snapshot.id,
            stopped_by: None,
            // A stored list is a finished one — the view this comes from cannot
            // return anything else — so there is nothing left to continue.
            resumable: false,
        }
    }

    /// Whether this list came out of storage rather than a walk in this run:
    /// every provenance but `Walked`, `CounterVerified` included.
    pub fn is_stored(&self) -> bool {
        self.provenance != Provenance::Walked
    }

    pub fn is_complete(&self) -> bool {
        self.reason.yields_complete_list()
    }

    /// The code a command should exit with when it has to refuse this result.
    ///
    /// What Instagram said beats what the store had to record, because the
    /// store's vocabulary is about whether the list is usable and the exit
    /// code is about what the caller should do next.
    pub fn exit_code(&self) -> ExitCode {
        self.stopped_by
            .unwrap_or_else(|| crate::exit::from_stop_reason(self.reason))
    }

    /// The code for a result that **was printed** out of this list, short or
    /// not.
    ///
    /// A cap the user asked for is not a failure, so `PageLimit` sits with
    /// `Completed`; everything else keeps the code that says what stopped the
    /// walk, so a script can tell "wait" from "log in again". `lists` and
    /// `sets` both ask it, so a page cap exits alike from either.
    ///
    /// A stored list needs no arm of its own. `ListOutcome::cached` is the
    /// only way to a provenance other than `Walked` and it records
    /// `Completed`, so anything out of storage arrives at the first arm.
    pub fn exit_code_for_a_printed_result(&self) -> ExitCode {
        match self.reason {
            StopReason::Completed | StopReason::PageLimit => ExitCode::Ok,
            _ => self.exit_code(),
        }
    }

    /// A walked-or-stored outcome with every other field at its plainest:
    /// one request, epoch zero, account and capture 1. Tests override what
    /// they assert on.
    #[cfg(test)]
    pub(crate) fn for_test(provenance: Provenance, reason: StopReason) -> Self {
        Self {
            provenance,
            reason,
            requests: 1,
            started_at: Epoch::default(),
            taken_at: Epoch::default(),
            account_pk: Pk::new(1),
            snapshot_id: 1,
            stopped_by: None,
            resumable: false,
        }
    }

    /// Whether this is the list of the account the run acts as.
    pub fn is_own(&self, viewer: &crate::app::Viewer) -> bool {
        self.account_pk == viewer.pk
    }
}

/// What a list is asked for: which account, and how the answer may be got.
///
/// Only what the engine reads: the rest of `cli::ListArgs` is presentation
/// (the format, the output path, the filters, the cap), and the monitor asks
/// for lists without a command line at all. `commands::common` is where a
/// `ListArgs` becomes one.
#[derive(Debug, Clone)]
pub struct ListQuery {
    /// The account, as typed. `None` is the session's own.
    pub target: Option<String>,
    /// Consent given in advance, for a list that is somebody else's.
    pub yes: bool,
    /// Walk even when storage could answer.
    pub refresh: bool,
    /// Answer out of storage and spend nothing.
    pub cache: bool,
    /// How old a stored list may be and still be served.
    pub max_age: std::time::Duration,
    /// Start over rather than continue an interrupted walk.
    pub no_resume: bool,
    /// Stop after this many pages.
    pub max_pages: Option<u32>,
    /// What a walk does when the day's accounts run out before the list
    /// does. `None` pauses, and says so first when the list will not fit;
    /// `Some` is a caller that already knows and wants nothing said.
    pub over_budget: Option<snob_ig::pager::OverBudget>,
    /// The shortest gap between two walks of one list, whatever the counter
    /// says. `None` walks whenever the stored list no longer answers.
    pub walk_at_most_every: Option<std::time::Duration>,
}

/// Gets one list, deciding along the way whether anything needs fetching.
///
/// The order of the checks is the whole policy, and each one exists to stop a
/// request being spent that did not have to be:
///
/// 1. In cooldown nothing may be spent, so only storage can answer.
/// 2. With `--offline` the network is off, resolution included — and with it the
///    consent question, which is about enumerating somebody rather than about
///    reading what was already enumerated.
/// 3. Otherwise, someone else's account needs consent before it is enumerated,
///    and before it is resolved.
/// 4. The cooldown is checked again, because it can land while step 3 waits.
/// 5. One counter poll says whether the list moved at all.
/// 6. If it did not, and what is stored is fresh enough, storage answers.
/// 7. Otherwise, walk.
pub async fn list(
    app: &mut App,
    args: &ListQuery,
    kind: ListKind,
) -> Result<(Vec<User>, ListOutcome)> {
    // Measured rather than added up along the way. Every request goes through
    // the pacer, including the ones a retry makes and the ones spent before
    // the walk begins, so asking it afterwards is the only count that cannot
    // drift from what was really charged.
    let before = app.client().pacer().spent();
    let (users, mut outcome) = decide(app, args, kind).await?;
    outcome.requests = app.client().pacer().spent().saturating_sub(before);
    Ok((users, outcome))
}

async fn decide(
    app: &mut App,
    args: &ListQuery,
    kind: ListKind,
) -> Result<(Vec<User>, ListOutcome)> {
    if let Some(held) = app.held()? {
        return cooldown::serve(app, args, kind, &held);
    }

    // **`--offline` is not asked about**, because there is nothing to agree to:
    // consent governs enumerating somebody else's lists, and this reads a list
    // that was already walked — with permission — off this machine's own disk.
    // Nothing is resolved over the network either, so the rule that consent
    // comes before resolution is not in play. Asking would also make `snob
    // unfollowers someone --offline` from cron or down a pipe exit 130 over an
    // answer that costs nothing, and `cooldown::serve` asks nothing about the
    // same stored lists either.
    if !args.cache {
        ask_consent_with(app, args, ui::can_be_asked()).await?;
    }

    // The check at the top cannot see a cooldown that landed while the
    // confirmation prompt was open — one written by the service sharing this
    // database, for instance — and resolving is itself a request, so the
    // second check comes here, before `target::resolve`: in cooldown nothing
    // may be spent, not even the counter poll.
    if let Some(held) = app.held()? {
        return cooldown::serve(app, args, kind, &held);
    }

    // A crossing asks for two lists, and resolving is a request. Reusing what
    // the first call worked out is what stops the second asking Instagram the
    // identical question about the identical account seconds later.
    let target = match app.resolved_target(args.target.as_deref()) {
        Some(target) => target,
        None => {
            let target = if args.cache {
                target::from_store(app, args.target.as_deref(), kind)?
            } else {
                target::resolve(app, args).await?
            };
            app.remember_target(args.target.as_deref(), target.clone());
            target
        }
    };

    // The user row has to exist before the account row: `accounts.pk`
    // references `users.pk`.
    //
    // Two calls rather than one, because "we know this account exists" and "we
    // know what it is called" are different claims: writing a name never
    // learned would put the numeric id in `users.username`, clobbering a
    // correct stored one and filing a rename that never happened.
    match target.username.as_deref() {
        Some(username) => {
            users::upsert(app.db().conn(), &User::named(target.pk, username)).map(|_| ())?
        }
        None => users::ensure(app.db().conn(), target.pk)?,
    }
    accounts::upsert(app.db().conn(), target.pk, target.is_self)?;

    let stored = snob_store::store::snapshots::latest_complete(app.db().conn(), target.pk, kind)?;

    if args.cache {
        let Some(snapshot) = stored else {
            return Err(crate::report::refuse_nothing_stored(kind));
        };
        // `--offline` is a promise not to spend a request, so nothing here
        // checked whether the stored list is still true. That is exactly what
        // makes it unsafe to cross against another one.
        return serve_stored(app, &snapshot, Provenance::CacheFlag);
    }

    freshness::decide_and_fetch(app, args, kind, &target, stored).await
}

/// A stored capture as the answer: its members, and where they came from.
pub(super) fn serve_stored(
    app: &App,
    snapshot: &snapshots::Snapshot,
    provenance: Provenance,
) -> Result<(Vec<User>, ListOutcome)> {
    Ok((
        snapshots::members(app.db().conn(), snapshot.id)?,
        ListOutcome::cached(snapshot, provenance),
    ))
}

/// Asks before enumerating somebody else's account, at most once per run.
///
/// It happens **before** the account is resolved, because resolving is already
/// a request: asking afterwards means that answering "no" has still spent one
/// on a run the user never authorized.
///
/// The price of asking first is that the name shown is the one typed rather
/// than the one Instagram spells. That costs nothing when it is wrong, and the
/// only account it can wrongly ask about is your own, which needs you to have
/// typed your own name.
///
/// **The third argument is for tests only**; `decide` passes
/// `ui::can_be_asked()`. Being a terminal is a property of the process's
/// streams, which `cargo test` answers differently depending on where the
/// suite was started from — the same reason `Presentation` carries
/// `interactive` as data rather than asking at the point of use.
#[doc(hidden)]
pub async fn ask_consent_with(
    app: &mut App,
    args: &ListQuery,
    someone_is_there: bool,
) -> Result<()> {
    let Some(typed) = args.target.as_deref() else {
        return Ok(()); // your own account, nothing to agree to
    };
    // Cleaned before it is used as a key, so the answer is filed under the
    // account rather than under a spelling of it. `clean` only strips a leading
    // at sign and is idempotent, so this holds whether or not the name arrives
    // already cleaned.
    let name = target::clean(typed);
    if args.yes || app.has_consent(name) {
        return Ok(());
    }

    // Compared raw, deliberately. A typed name with a zero-width character in
    // it is not your own account, and filtering before this comparison would
    // make it match — which skips the question for somebody else's lists.
    let is_own_name = app
        .viewer()
        .username
        .as_deref()
        .is_some_and(|mine| mine.eq_ignore_ascii_case(name));
    if is_own_name {
        return Ok(());
    }

    // How an account is named on screen is `target::label`'s question, and it is
    // the same question here: `args.target` is `Some` at this point, so `label`
    // returns exactly the at sign and the filtered name the sentences in
    // `report` want.
    let shown = target::label(app, args.target.as_deref());

    // Being unable to ask and being told no are two different events:
    // `confirm` answers with its default the moment nobody can answer, which
    // would blame the user for a "no" nobody gave.
    //
    // What is asked here is whether somebody is at the keyboard, and nothing
    // else. The question is written to standard error and the answer read from
    // standard input, so a pipe or a redirect on standard output does not touch
    // it — and `snob scan someone | jq` is a shape the README promises.
    if !someone_is_there {
        // Which way to answer in advance is the *caller's* fact, not this
        // function's, and it is the one thing this hands over: the sentence
        // itself is `report`'s, like every other sentence the tool prints.
        return Err(crate::report::refuse_unconsented(
            &shown,
            app.consent_in_advance(),
        ));
    }

    app.warn(crate::report::READING_SOMEBODY_ELSES_LIST);
    if !ui::confirm_off_thread(
        app.progress(),
        crate::report::ask_to_continue(&shown),
        false,
    )
    .await?
    {
        return Err(crate::report::refuse_declined(&shown));
    }
    // Asked and answered. A crossing wants two lists and a summary four, and
    // asking again about the same account reads as not having listened.
    app.record_consent(name);
    Ok(())
}

/// A complete stored capture of account 1's followers, the row the storage
/// paths serve from.
#[cfg(test)]
pub(crate) fn stored_snapshot(
    started_at: Epoch,
    taken_at: Epoch,
    declared_count: Option<u64>,
) -> snapshots::Snapshot {
    snapshots::Snapshot {
        id: 1,
        account_pk: Pk::new(1),
        kind: ListKind::Followers,
        started_at,
        taken_at: Some(taken_at),
        member_count: 0,
        declared_count,
        next_cursor: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_outcome_is_complete_by_construction() {
        let snapshot = snapshots::Snapshot {
            account_pk: Pk::new(7),
            ..stored_snapshot(Epoch::default(), Epoch::default(), None)
        };
        let outcome = ListOutcome::cached(&snapshot, Provenance::CounterVerified);
        assert!(outcome.is_complete());
        assert!(outcome.is_stored());
        // Both ends of the interval come off the row, so they cannot disagree
        // with the members read from the same one.
        assert_eq!(outcome.account_pk, Pk::new(7));
    }

    /// A cap the user asked for is not a failure, on either side of a
    /// crossing (`cli.rs` promises 0 for it), and neither is anything served
    /// from storage. Everything else keeps the code that says what happened,
    /// so a script can tell "wait" from "log in again".
    #[test]
    fn the_exit_code_reports_what_stopped_the_walk() {
        let exit_code = |provenance, reason| {
            ListOutcome::for_test(provenance, reason).exit_code_for_a_printed_result()
        };
        let walked = |reason| exit_code(Provenance::Walked, reason);
        for reason in [StopReason::Completed, StopReason::PageLimit] {
            assert_eq!(walked(reason), ExitCode::Ok);
        }
        assert_eq!(walked(StopReason::RateLimit), ExitCode::RateLimited);
        assert_eq!(walked(StopReason::Canceled), ExitCode::Interrupted);
        assert_eq!(walked(StopReason::SessionInvalid), ExitCode::NoSession);
        // A plain list prints what it got and still exits with what stopped
        // it: the truncation wall is not a success.
        for stopped in [StopReason::Truncated, StopReason::Network] {
            assert_eq!(
                walked(stopped),
                ExitCode::Error,
                "{stopped:?} is not something the user asked for"
            );
        }
        // A stored list is a stored list, whatever ended the walk that made it.
        assert_eq!(
            exit_code(Provenance::CounterVerified, StopReason::Completed),
            ExitCode::Ok
        );
    }
}
