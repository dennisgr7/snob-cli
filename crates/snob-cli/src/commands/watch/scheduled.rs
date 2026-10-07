//! The scheduled mode: `snob watch` with no subcommand.
//!
//! The loop, and what it starts its clock from. One turn of it is
//! `super::run::run_viewers`, which is the same run `once` makes; what is left
//! here is the waiting, the seed the waiting measures from, and the two
//! sentences a loop no test can drive still has to get right.

use anyhow::Result;
use snob_core::model::printable;
use snob_core::watch::schedule::{self, Due, Schedule};
use snob_core::{Epoch, Pk};
use snob_store::config;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;
use snob_store::store::shared::Shared;

use crate::cli::WatchRunArgs;
use crate::exit::{ExitCode, ExitError};
use crate::report;
use crate::ui;

use super::delivery::delivery_from;
use super::run::{Printing, account_databases, run_viewers, settle_all};
use super::schedule::{describe_schedule, schedule_from, when_from};
use super::watched::{watched_from, watching_label};

/// Stays up and runs on a schedule until it is stopped.
///
/// The loop is deliberately thin. Everything with a rule in it — when the next
/// run is due, how many were missed, how far jitter may push one — is
/// `snob_core::watch::schedule`, which reads no clock and is tested with
/// literal timestamps. What is left here is sleeping and asking again, which
/// is the part no test can usefully drive.
///
/// An entry that names no viewer is read as `in_use`, the account this
/// command resolved.
pub(super) async fn scheduled(
    args: WatchRunArgs,
    secrets: SecretStore,
    paths: &AppPaths,
    in_use: Pk,
) -> Result<ExitCode> {
    // Before anything that can refuse to start. Reading the file, building the
    // schedule and checking the address can each end this process, and a
    // service that dies at startup on a hand-edited file would otherwise expire
    // nothing for as long as nobody notices.
    settle_all(paths);

    // Read first, so the flags can override it. A flag beats the file because
    // somebody typing one is saying something about this run in particular.
    let configured = config::load(paths)?;

    let schedule = schedule_from(&args, configured.as_ref())?;
    let watched = watched_from(args.target.clone(), configured.as_ref(), Some(in_use));
    // Built once and reused, so a service that runs for months holds one
    // connection pool rather than building a TLS stack every few hours. Checked
    // here for the same reason the schedule is: a bad address should stop this
    // at the moment somebody is watching it start.
    let delivery = delivery_from(&args.delivery, configured.as_ref(), &secrets)?;

    // Refused here rather than at the first tick. A service that starts, waits
    // six hours and then exits because it was never allowed to read that
    // account is a service that looked healthy all afternoon.
    if let Some(unallowed) = watched.iter().find(|w| !w.may_run_unattended()) {
        return Err(refuse_unattended(unallowed.name().unwrap_or_default()));
    }

    ui::info(&format!(
        "Watching {}. {} Stop with Ctrl+C.",
        watching_label(&watched),
        describe_schedule(&when_from(&args, configured.as_ref()), &schedule, args.now),
    ));

    // Installed once for the process, which is what lets this open an `App` per
    // run without leaving a signal listener behind on each one.
    let cancel = crate::interrupt::install();
    let printing = Printing::unattended(args.output.json);
    let run = || {
        run_viewers(
            paths,
            &secrets,
            &watched,
            delivery.as_ref(),
            printing,
            !args.progress.no_progress,
        )
    };

    let mut last_run = seed_for(paths, &schedule, snob_core::clock::now())?;

    if args.now {
        // Run one, here, rather than by pretending nothing has ever run:
        // `last_run = None` means "due immediately" only for an interval, and a
        // calendar would answer with its next moment on the grid. It also means
        // the run happens with no jitter, which is right — jitter exists so a
        // *schedule* does not land on the same second every day, and delaying a
        // run somebody just asked for would only look broken.
        last_run = Some(snob_core::clock::now());
        if let Err(e) = run().await.into_result() {
            report::print_error(&e, printing.wording());
        }
    }

    // The moment being waited for, the number of runs it stands for, and the
    // jitter roll that shifted it.
    //
    // Held across iterations rather than recomputed: `next_after` searches from
    // `max(floor, now)`, so once `now` has passed the grid minute it would
    // answer with the *next* one and the wake-up would creep forward instead of
    // arriving. Recomputed when the clock stops ticking and starts jumping —
    // see `CLOCK_JUMP_SECS`.
    let mut waiting_for: Option<(Epoch, u32)> = None;
    // The previous time round's clock reading, which is how a clock that jumped
    // is told from one that ticked.
    let mut clock_was: Option<Epoch> = None;
    // Whether the run that is due is being held for a critical battery, so it
    // is said once rather than at every look.
    let mut waiting_for_power = false;

    loop {
        let now = snob_core::clock::now();

        // A clock that moved by more than a nap did not tick: it was corrected,
        // or the machine was suspended. Either way the moment being waited for
        // was computed against a reading that no longer applies, and `due` is
        // asked again. A machine that booted a year ahead and then had its clock
        // corrected inwards would otherwise park the monitor for the whole year.
        if let Some(before) = clock_was
            && !(0..=CLOCK_JUMP_SECS).contains(&(now - before))
        {
            waiting_for = None;
        }
        clock_was = Some(now);

        let (wake_at, missed) = match waiting_for {
            Some(pending) => pending,
            None => {
                let pending = match schedule::due(&schedule, last_run, now, &chrono::Local) {
                    // Already owed, so it goes now. Jitter is not applied to a
                    // run that is already late: it is off the grid by however
                    // late it is, and shifting it further would move the target
                    // every time round the loop, since the target would be
                    // computed from a `now` that keeps advancing.
                    Due::Now { missed } => (now, missed),
                    // A variant rather than a sentinel moment, so the compiler
                    // asks for this case instead of an arm order keeping
                    // `wake_at` from saturating.
                    Due::Never => {
                        return Err(anyhow::anyhow!(
                            "this schedule can never come round: nothing matches it"
                        ));
                    }
                    // Rolled once per due moment. The roll is made here rather
                    // than inside `wake_at` so that function reads no randomness
                    // and its bounds stay testable.
                    //
                    // `wake_at` and not `jittered`: the jitter the banner
                    // printed is measured against the grid, in seconds-of-day,
                    // and a day a zone springs forward through is an hour
                    // shorter than that. `wake_at` asks the calendar in the zone
                    // from the moment actually due, so the roll cannot reach
                    // past the next moment or spill onto a day the calendar
                    // forbids. It only ever narrows, so the banner stays true.
                    Due::At(at) => (
                        schedule::wake_at(&schedule, at, fastrand::f64(), &chrono::Local),
                        0,
                    ),
                };
                waiting_for = Some(pending);
                pending
            }
        };

        if now >= wake_at {
            // **A critical battery holds the run back**, and only that: a run
            // started now would be cut off by the system hibernating under
            // it. The moment waited for is kept, so the run goes as soon as
            // the machine is on power or charged again, as an overdue run, with
            // no jitter. Looked at again at the near nap, which is soon enough
            // for a charger plugged in and rare enough for a battery that
            // stays empty.
            if super::run::waits_for_power(paths, now) {
                if !waiting_for_power {
                    ui::warn(report::BATTERY_CRITICAL_HOLD);
                    waiting_for_power = true;
                }
                if cancel
                    .sleep_or_cancel(std::time::Duration::from_secs(NAP_NEAR_SECS.unsigned_abs()))
                    .await
                {
                    // Nobody is waiting any more, so `snob watch status` must
                    // not go on saying somebody is.
                    super::run::power_is_back(paths);
                    ui::info("Stopped.");
                    return Ok(ExitCode::Interrupted);
                }
                continue;
            }
            if std::mem::take(&mut waiting_for_power) {
                super::run::power_is_back(paths);
            }
            if missed > 0 {
                ui::warn(&missed_warning(missed));
            }
            // Recorded before the work, so a run that fails cannot turn into a
            // tight loop retrying it.
            last_run = Some(now);
            waiting_for = None;

            // A scheduled service does not exit because one run failed. A
            // cooldown lifts, a network comes back, and a session that is gone
            // gets reported every time until somebody fixes it — which is the
            // point of something that watches.
            if let Err(e) = run().await.into_result() {
                report::print_error(&e, printing.wording());
            }
            // The browser does not sleep through the hours to the next run:
            // what it rotated is written back now, and the browser is left to
            // its owner, which closes it once idle (or closed here, when this
            // process runs its own).
            crate::owner::finish(&secrets, paths).await;
            continue;
        }

        // Slept in bounded stretches and re-checked against the wall clock each
        // time round, rather than in one long sleep. A laptop that suspends for
        // eight hours makes a single long timer wrong by eight hours, and
        // asking "is it time yet?" costs nothing -- asking it every minute
        // for six hours costs a little, which is what `nap_for` is about.
        let nap = nap_for(wake_at - now);
        if cancel
            .sleep_or_cancel(std::time::Duration::from_secs(nap))
            .await
        {
            ui::info("Stopped.");
            return Ok(ExitCode::Interrupted);
        }
    }
}

/// The refusal a scheduled run raises for an account nobody confirmed.
///
/// Its own function so a test can read it. The name lands in two slots of one
/// sentence — the account being refused, and the command that answers for it —
/// and both are filtered: it comes from `watch.toml`, where nothing validates a
/// username.
fn refuse_unattended(name: &str) -> anyhow::Error {
    let shown = printable(name);
    ExitError::new(
        ExitCode::Interrupted,
        format!(
            "reading @{shown}'s lists needs confirmation, and a scheduled run has nobody to \
             ask.\nRun \"snob watch setup\" to answer it once, or \"snob watch once {shown}\" \
             while you are here."
        ),
    )
    .into()
}

/// What the monitor says about the runs it was not running for.
///
/// **One is the ordinary case**, so the sentence has a singular form: a machine
/// off for a day on a daily schedule misses exactly one, and `missed_since`
/// takes one off the end because the moment being served is itself in the past,
/// so one is what a laptop that was shut overnight produces.
///
/// Its own function so a test can read it, like [`refuse_unattended`]. The
/// sentence is only ever printed from inside the loop, which no test can drive.
fn missed_warning(missed: u32) -> String {
    if missed == 1 {
        "1 scheduled run was missed while this was not running. It is not replayed: there is \
         only one present state, so there is nothing to catch up on"
            .to_string()
    } else {
        format!(
            "{missed} scheduled runs were missed while this was not running. They are reported \
             as one: there is only one present state, so there is nothing to catch up on"
        )
    }
}

/// How long to sleep before looking at the clock again, given how far the
/// next run is.
///
/// Short near the moment and longer far from it. Each wake-up is cheap in CPU
/// but a timer the operating system cannot coalesce, on a laptop where that is
/// the thing that costs battery: a one-minute nap wakes three hundred and sixty
/// times between two runs on `--every 6h`, five minutes about seventy. The last
/// ten minutes are walked in one-minute steps so the run still lands within a
/// minute of its moment.
///
/// What detects a suspend does not depend on this: it is the jump in the wall
/// clock between two turns, read against [`CLOCK_JUMP_SECS`], not how often
/// the clock is read. A laptop shut for eight hours is noticed on the first
/// turn after it wakes, whichever nap it was in. And a nap can never run past
/// the moment: it is bounded by the distance, so the wake-up for a run ten
/// minutes out is never later than the run.
fn nap_for(remaining: i64) -> u64 {
    const FAR: i64 = 10 * 60;
    let ceiling = if remaining > FAR {
        NAP_FAR_SECS
    } else {
        NAP_NEAR_SECS
    };
    remaining.clamp(1, ceiling) as u64
}

/// The two nap ceilings [`nap_for`] chooses between. Both stay under the
/// fifteen-minute floor between runs, so neither can nap through one.
const NAP_NEAR_SECS: i64 = 60;
const NAP_FAR_SECS: i64 = 5 * 60;

/// How far the clock may move between two turns of the loop and still be
/// ticking.
///
/// Twice the longest nap: a turn that took longer than that did not sleep
/// and wake, it was corrected, suspended, or resumed -- none of which the
/// moment being waited for was computed against. Tied to [`NAP_FAR_SECS`]
/// rather than written as a number, because the two have to move together: a
/// five-minute nap against a two-minute jump would call every ordinary turn a
/// suspend.
const CLOCK_JUMP_SECS: i64 = 2 * NAP_FAR_SECS;

// The two orderings the loop leans on, held where they cannot be argued
// with: the jump threshold sits clear above the longest nap (or every
// ordinary turn reads as a suspend), and no nap may cross the floor between
// runs (or the loop could sleep through one).
const _: () = assert!(CLOCK_JUMP_SECS > NAP_FAR_SECS);
const _: () = assert!(NAP_FAR_SECS < snob_core::watch::schedule::MIN_GAP_SECS);

/// When the monitor last started a run, for any account: the latest across
/// every account's database.
///
/// Opened and closed here rather than held: the loop deliberately keeps no
/// SQLite connection while it sleeps, so `snob purge` in another terminal is not
/// blocked by a file this has open.
fn last_started(paths: &AppPaths) -> Result<Option<Epoch>> {
    let mut latest = None;
    for account in account_databases(paths) {
        let Some(store) = snob_store::store::Store::open_existing(&account)? else {
            continue;
        };
        latest = latest.max(snob_store::store::watch::last_started(store.conn())?);
    }
    Ok(latest)
}

/// The seed when the run log is empty: none on a calendar, `now` otherwise,
/// because the answer depends on what kind of schedule it is:
///
/// - **An interval** has no moments of its own, so `None` means "due now" and a
///   fresh install would walk the second it was set up. `Some(now)` is what
///   makes it wait one interval, and `--now` is how somebody asks for the walk
///   at start.
/// - **A calendar** has its own moments, and inventing a run for it is harmful.
///   The invented value is a claim that a run happened this second, and
///   `next_after` believes it: the floor becomes `now + MIN_GAP_SECS`, stepping
///   over every moment in the next quarter of an hour, so `snob watch --at
///   09:00` started at 08:50 would sleep for a day and ten minutes. `None` is
///   what the schedule module's own contract asks for here — there is no past
///   to wait from, and no run to be too close to.
fn seed_last_run(schedule: &Schedule, now: Epoch) -> Option<Epoch> {
    (!schedule.is_on_a_calendar()).then_some(now)
}

/// What the loop starts its clock from.
///
/// Seeded from the run log rather than from this process's start. A clock that
/// began again on every start would never reach the first run of `--every 24h`
/// on a machine powered on from eight to six, or under a supervisor with
/// `Restart=always` restarting more often than the interval — while
/// `snob watch status`, reading the very same table, said "It has not run yet."
///
/// With nothing in the log it is [`seed_last_run`]'s answer, remembered across
/// process starts. `last_started` reads `watch_runs`, whose only writer is a
/// tick that finished, so an invented `now` held only in a variable would,
/// while that log stays empty, **measure every start's interval from that
/// start**. `snob watch` in a login item with `--every 1d` (the wizard's own
/// suggestion) on a laptop up eight hours a day would be due at hour
/// twenty-four and shut down at hour eight, every day, for ever.
///
/// A calendar seeds nothing, for the reason [`seed_last_run`] gives: inventing
/// a run for it steps over every moment in the next quarter of an hour.
///
/// Kept in `shared.db`, because the monitor is one for every account. A seed an
/// account's own database kept from before is copied there once, the earliest
/// if several did.
fn seed_for(paths: &AppPaths, schedule: &Schedule, now: Epoch) -> Result<Option<Epoch>> {
    if let Some(recorded) = last_started(paths)? {
        return Ok(Some(recorded));
    }
    let Some(invented) = seed_last_run(schedule, now) else {
        return Ok(None);
    };

    let shared = Shared::open(paths)?;
    if let Some(seeded) = shared.interval_seeded_at()? {
        return Ok(Some(seeded));
    }
    let mut kept_before = None;
    for account in account_databases(paths) {
        let Some(store) = snob_store::store::Store::open_existing(&account)? else {
            continue;
        };
        if let Some(at) = snob_store::store::watch::interval_seeded_at(store.conn())? {
            kept_before = Some(kept_before.map_or(at, |earliest: Epoch| earliest.min(at)));
        }
    }
    Ok(Some(shared.seed_interval(kept_before.unwrap_or(invented))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Near the moment the loop steps by the minute; far from it, by five.
    /// Neither step can carry the loop past the moment itself.
    #[test]
    fn the_nap_is_short_near_the_moment_and_long_far_from_it() {
        assert_eq!(nap_for(0), 1, "never a zero-second sleep");
        assert_eq!(nap_for(-5), 1, "a moment already past is looked at now");
        assert_eq!(nap_for(30), 30, "bounded by the distance when close");
        assert_eq!(nap_for(90), 60, "a minute at most inside the last ten");
        assert_eq!(nap_for(10 * 60), 60, "the last ten minutes are near");
        assert_eq!(nap_for(10 * 60 + 1), 5 * 60, "past them, five minutes");
        assert_eq!(nap_for(6 * 3600), 5 * 60);
    }

    /// An interval measures from the first start, not from this one, while
    /// the run log is still empty.
    #[test]
    fn an_interval_measures_from_the_first_start_and_not_from_this_one() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let every = Schedule::every(std::time::Duration::from_secs(24 * 3_600)).unwrap();

        let first = seed_for(&paths, &every, Epoch::new(1_700_000_000)).unwrap();
        assert_eq!(first, Some(Epoch::new(1_700_000_000)));

        // A second start two hours later, with the run log still empty.
        let second = seed_for(&paths, &every, Epoch::new(1_700_007_200)).unwrap();
        assert_eq!(
            second, first,
            "a restart must not put the interval back to zero"
        );
    }

    /// A seed an account's database kept from before is the one kept, and it
    /// is copied once: the monitor's clock is not put back to zero by moving
    /// to one seed for every account.
    #[test]
    fn a_seed_an_account_kept_before_is_carried_over() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let every = Schedule::every(std::time::Duration::from_secs(24 * 3_600)).unwrap();
        for (pk, seeded) in [(42, 1_700_000_500), (43, 1_700_000_000)] {
            let store = snob_store::store::Store::open(&paths.account(Pk::new(pk))).unwrap();
            store
                .conn()
                .execute(
                    "INSERT INTO watch_state (key, value) VALUES ('interval_seeded_at', ?1)",
                    [seeded],
                )
                .unwrap();
        }

        assert_eq!(
            seed_for(&paths, &every, Epoch::new(1_700_007_200)).unwrap(),
            Some(Epoch::new(1_700_000_000)),
            "the earliest seed an account kept"
        );
        assert_eq!(
            Shared::open(&paths).unwrap().interval_seeded_at().unwrap(),
            Some(Epoch::new(1_700_000_000)),
            "and it is kept where every account reads it"
        );
    }

    /// The monitor last ran when any account last ran.
    #[test]
    fn the_last_run_is_the_latest_across_every_account() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        for (pk, started) in [(42, 1_700_000_000), (43, 1_700_003_600)] {
            let pk = Pk::new(pk);
            let store = snob_store::store::Store::open(&paths.account(pk)).unwrap();
            crate::commands::watch::fixtures::known_account(&store, pk, "me", true);
            snob_store::store::watch::record_run(
                store.conn(),
                &snob_store::store::watch::Run {
                    account_pk: pk,
                    started_at: Epoch::new(started),
                    finished_at: Some(Epoch::new(started)),
                    requests: 0,
                    outcome: None,
                    changes: 0,
                },
            )
            .unwrap();
        }

        assert_eq!(
            last_started(&paths).unwrap(),
            Some(Epoch::new(1_700_003_600))
        );
    }

    /// A recorded run wins over the seed, for an interval and for a calendar.
    #[test]
    fn a_recorded_run_wins_over_the_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let pk = Pk::new(42);
        let store = snob_store::store::Store::open(&paths.account(pk)).unwrap();
        crate::commands::watch::fixtures::known_account(&store, pk, "me", true);
        snob_store::store::watch::record_run(
            store.conn(),
            &snob_store::store::watch::Run {
                account_pk: pk,
                started_at: Epoch::new(1_700_000_000),
                finished_at: Some(Epoch::new(1_700_000_000)),
                requests: 0,
                outcome: None,
                changes: 0,
            },
        )
        .unwrap();

        let every = Schedule::every(std::time::Duration::from_secs(24 * 3_600)).unwrap();
        let at_nine = Schedule::calendar(&[], &[schedule::parse_time("09:00").unwrap()]).unwrap();
        for schedule in [every, at_nine] {
            assert_eq!(
                seed_for(&paths, &schedule, Epoch::new(1_700_007_200)).unwrap(),
                Some(Epoch::new(1_700_000_000)),
                "{schedule:?}"
            );
        }
    }

    /// And a calendar still seeds nothing, because inventing a run for one
    /// steps over every moment in the next quarter of an hour.
    #[test]
    fn a_calendar_is_not_seeded() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let at_nine = Schedule::calendar(&[], &[schedule::parse_time("09:00").unwrap()]).unwrap();

        assert_eq!(
            seed_for(&paths, &at_nine, Epoch::new(1_700_000_000)).unwrap(),
            None
        );
    }

    /// Both slots of the refusal take the same name, so both must be filtered.
    ///
    /// The account comes from `watch.toml`, which nothing validates, and the
    /// refusal is the one place the monitor prints it before any client
    /// exists — the earliest a hostile name can reach a terminal.
    #[test]
    fn the_unattended_refusal_filters_the_name_in_both_slots() {
        let message = refuse_unattended("gh\u{1b}[2K\u{1b}[A").to_string();

        assert!(
            !message.chars().any(|c| c.is_control() && c != '\n'),
            "{message:?} is printed to a terminal and written to the journal"
        );
        assert_eq!(
            message.matches("gh[2K[A").count(),
            2,
            "the account and the command that answers for it name the same person: {message}"
        );
    }

    /// A fresh install does not step over the first moment its calendar names:
    /// a seed of `Some(now)` would put the floor at `now + MIN_GAP_SECS`.
    #[test]
    fn a_fresh_install_keeps_the_first_moment_its_calendar_names() {
        // Midnight UTC on a Monday, so the wall clock is checkable by hand.
        const MONDAY_0000: i64 = 1_786_924_800;
        let at = |secs: i64| Epoch::new(MONDAY_0000 + secs);

        let calendar = Schedule::cron("0 9 * * *").unwrap();
        let ten_to_nine = at(8 * 3600 + 50 * 60);

        assert_eq!(
            seed_last_run(&calendar, ten_to_nine),
            None,
            "a calendar has its own moments and needs no invented run"
        );
        assert_eq!(
            schedule::due(&calendar, None, ten_to_nine, &chrono::Utc),
            schedule::Due::At(at(9 * 3600)),
            "nine o'clock is ten minutes away, which is inside the minimum gap"
        );

        // An interval is the other way round: with no past to measure from it
        // would be due immediately, and a fresh install must not walk the second
        // it is set up.
        let interval = Schedule::every(std::time::Duration::from_secs(6 * 3600)).unwrap();
        assert_eq!(seed_last_run(&interval, ten_to_nine), Some(ten_to_nine));
    }

    /// The monitor's own sentence about its gaps reads as a sentence for one,
    /// the case it prints most often.
    #[test]
    fn the_missed_warning_reads_as_a_sentence_for_one() {
        let one = missed_warning(1);
        assert!(one.contains("1 scheduled run was missed"), "{one}");
        assert!(!one.contains("runs were"), "{one}");
        assert!(!one.contains("They are"), "{one}");

        let several = missed_warning(4);
        assert!(
            several.contains("4 scheduled runs were missed"),
            "{several}"
        );

        // Both have to say the part that matters, which is that they are not
        // replayed: firing twelve to catch up is the burst the pacing exists to
        // prevent, and they would all report the same present state anyway.
        for text in [one, several] {
            assert!(text.contains("nothing to catch up on"), "{text}");
        }
    }
}
