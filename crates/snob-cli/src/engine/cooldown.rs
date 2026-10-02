//! What the tool can still answer while the account is in cooldown, and the
//! rule that stops two stored lists from being crossed when they should not be.
//!
//! A cooldown does not blind the tool. Nothing may be spent — not even the
//! counter poll — so storage is the only thing that can answer, and it answers
//! however old it is: stale beats nothing, and the warning names the date.

use anyhow::Result;
use snob_core::model::{ListKind, User};
use snob_store::store::snapshots;

use crate::app::{App, Held};
use crate::engine::{ListOutcome, ListQuery, Provenance, target};
use crate::report::{self, Blocked};

/// Serves what is stored, or explains why nothing can be.
///
/// No confirmation is asked because nothing is enumerated, and `--max-age` is
/// ignored for the same reason `--offline` ignores it.
pub fn serve(
    app: &App,
    args: &ListQuery,
    kind: ListKind,
    held: &Held,
) -> Result<(Vec<User>, ListOutcome)> {
    if args.refresh {
        return Err(report::refuse_in_cooldown(held, Blocked::RefreshWanted));
    }

    let pk = match args.target.as_deref() {
        None => app.viewer().pk,
        Some(typed) => {
            let username = target::clean(typed);
            match snob_store::store::accounts::find_pk_by_username(app.db().conn(), username)? {
                Some(pk) => pk,
                None => {
                    return Err(report::refuse_in_cooldown(
                        held,
                        Blocked::AccountUnknown(username),
                    ));
                }
            }
        }
    };

    let Some(snapshot) = snapshots::latest_complete(app.db().conn(), pk, kind)? else {
        return Err(report::refuse_in_cooldown(
            held,
            Blocked::NothingStored(kind),
        ));
    };

    let taken_at = snapshot.taken_at.unwrap_or_default();
    app.warn(&report::serving_stored_in_cooldown(held, kind, taken_at));

    super::serve_stored(app, &snapshot, Provenance::Cooldown)
}

/// Two lists may only be crossed if they describe roughly the same moment.
///
/// Only two of the five provenances guarantee that on their own: a walked list
/// is the account as it is, and a counter-verified one was checked against it in
/// this run, so its age is known to be harmless. The other three are stored
/// lists that nothing looked at — during a cooldown nothing may be spent, on a
/// failed poll nothing could be, and with `--offline` nothing was meant to be.
///
/// Stitching two distant moments together invents arrivals and departures that
/// never happened, which is the failure this whole tool is built not to have.
///
/// What is measured is the time **between** the two walks, not between the two
/// moments they finished at. See [`gap_between`].
pub fn check_same_moment(a: &ListOutcome, b: &ListOutcome) -> Result<()> {
    // Both sides have to carry evidence, not just neither side being a
    // cooldown: `--offline` and a failed poll are not cooldowns either, and
    // nothing checked their lists.
    if a.provenance.describes_now() && b.provenance.describes_now() {
        return Ok(());
    }
    if gap_between(a, b) <= SAME_MOMENT_GAP_SECS {
        return Ok(());
    }

    Err(report::refuse_different_moments(
        a.provenance,
        b.provenance,
        a.taken_at,
        b.taken_at,
    ))
}

/// How much dead time there may be between two walks and still be one moment:
/// fifteen minutes of an account's life, the drift this tool treats as a
/// single instant.
///
/// Deliberately not [`snapshots::RESUME_WINDOW_SECS`]. That decides how long
/// one interrupted walk may be continued, this what two finished ones may
/// leave unobserved between them: two questions, so two numbers, and changing
/// how long a walk may be paused must not quietly change what may be crossed.
const SAME_MOMENT_GAP_SECS: i64 = 15 * 60;

/// The seconds during which neither walk was looking.
///
/// Each list covers an interval — first page to last — rather than an instant,
/// and what can invent an arrival is a stretch of time one list saw and the
/// other did not. So this is the distance **between** the intervals: zero when
/// they overlap or touch, however long either of them took.
///
/// That difference is the whole point. Two lists walked back to back in one
/// correct run *finish* far apart by definition — on an account following six
/// thousand people the second walk alone takes hours of sittings and rests at
/// the documented pace, and days under the daily ceiling on accounts — so
/// comparing finishing times against a fifteen-minute bound would refuse
/// precisely the pair that is most obviously one moment.
///
/// It is not a license, either. A walk that genuinely took an hour, crossed
/// against a snapshot from two hours later, still has an hour of gap and is
/// still refused.
///
/// A paused walk keeps the moment its first page was asked for, whether a
/// later run resumed it within [`snapshots::RESUME_WINDOW_SECS`] or it slept
/// on the day's accounts inside one run, so its interval can span a day or
/// more. That is accepted on purpose: this rule is about time neither list
/// saw, a change during a long walk is misread the same whether or not it is
/// crossed, and refusing would leave a list larger than a day's accounts
/// impossible to cross at all.
fn gap_between(a: &ListOutcome, b: &ListOutcome) -> i64 {
    (b.started_at - a.taken_at)
        .max(a.started_at - b.taken_at)
        .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitCode;
    use snob_core::Epoch;

    /// The drift two stored lists may have between them, written out.
    ///
    /// Every other test refers to it by name, so it could be set to a day and
    /// the suite would still pass — while two walks half a day apart were
    /// crossed as though they described one moment, which is what invents an
    /// arrival that never happened.
    ///
    /// Deliberately not tied to `snapshots::RESUME_WINDOW_SECS`: the
    /// constant's own doc says they are two questions and two numbers.
    #[test]
    fn the_gap_two_lists_may_have_is_the_documented_one() {
        assert_eq!(SAME_MOMENT_GAP_SECS, 15 * 60);
    }

    /// A stored row, which is what the outcomes under test are built from.
    ///
    /// The two moments arrive as plain seconds and become [`Epoch`] here, at
    /// the edge, so every assertion below reads as the number of seconds
    /// between two walks rather than as a constructor repeated forty times.
    fn stored(started_at: i64, taken_at: i64) -> snapshots::Snapshot {
        crate::engine::stored_snapshot(Epoch::new(started_at), Epoch::new(taken_at), None)
    }

    /// A capture with no duration, which real ones never are.
    fn outcome(provenance: Provenance, taken_at: i64) -> ListOutcome {
        walked(provenance, taken_at, taken_at)
    }

    fn walked(provenance: Provenance, started_at: i64, taken_at: i64) -> ListOutcome {
        ListOutcome::cached(&stored(started_at, taken_at), provenance)
    }

    /// A counter checked in this run is what makes age harmless, so any skew
    /// passes — that is the whole point of the cache.
    #[test]
    fn a_verified_pair_may_be_any_distance_apart() {
        let a = outcome(Provenance::CounterVerified, 0);
        let b = outcome(Provenance::CounterVerified, 999_999);
        assert!(check_same_moment(&a, &b).is_ok());
    }

    /// What `Provenance` exists for: `snob unfollowers --offline` must not
    /// cross a followers list from June against a following list from August
    /// and call the difference unfollowers.
    #[test]
    fn an_unverified_pair_far_apart_is_refused() {
        let june = 0;
        let august = 5_000_000;
        for provenance in [
            Provenance::CacheFlag,
            Provenance::PollFailed,
            Provenance::Cooldown,
        ] {
            let error = check_same_moment(&outcome(provenance, june), &outcome(provenance, august))
                .expect_err(&format!("{provenance:?} carries no evidence"));
            assert!(error.to_string().contains("different moments"), "{error}");
        }
    }

    /// Close enough together and it is still one moment, whatever the reason
    /// nobody checked.
    #[test]
    fn an_unverified_pair_within_the_window_still_describes_one_moment() {
        let a = outcome(Provenance::CacheFlag, 1_000);
        let b = outcome(Provenance::CacheFlag, 1_800);
        assert!(check_same_moment(&a, &b).is_ok());
    }

    /// The shape of every correct crossing on an account of any size.
    ///
    /// `taken_at` is when a walk **finished**. Walking six thousand accounts
    /// takes hours of sittings and rests at the documented pace, and days
    /// under the daily ceiling on accounts, so the two lists of one perfectly
    /// good `snob unfollowers` run finish far more than fifteen minutes apart,
    /// and reading that pair back with `--offline`, where neither side carries
    /// evidence, must still be one moment.
    #[test]
    fn two_walks_run_back_to_back_are_one_moment_however_long_they_took() {
        let followers = walked(Provenance::CacheFlag, 0, 1_200);
        let following = walked(Provenance::CacheFlag, 1_260, 3_600);

        assert!(
            (following.taken_at - followers.taken_at).abs() > SAME_MOMENT_GAP_SECS,
            "the finishing times are far apart; that is the point"
        );
        assert!(check_same_moment(&followers, &following).is_ok());
    }

    /// Two walks that were running at the same time left no unobserved stretch
    /// at all.
    #[test]
    fn overlapping_walks_have_no_gap_at_all() {
        let a = walked(Provenance::CacheFlag, 0, 2_000);
        let b = walked(Provenance::CacheFlag, 1_000, 3_000);
        assert_eq!(gap_between(&a, &b), 0);
        assert!(check_same_moment(&a, &b).is_ok());
    }

    /// Measuring the gap rather than the distance must not become permission.
    /// A long walk widens its own interval; it does not excuse a partner from
    /// hours later.
    #[test]
    fn a_long_walk_is_not_a_license_for_a_stale_partner() {
        let hour_long = walked(Provenance::CacheFlag, 0, 3_600);
        let much_later = outcome(Provenance::CacheFlag, 10_000);
        assert!(check_same_moment(&hour_long, &much_later).is_err());
    }

    /// A resumed walk keeps the moment its first page was asked for, so its
    /// interval is wider by however long it was paused. That is not a second
    /// concession: `RESUME_WINDOW_SECS` has already decided a pause of that
    /// length leaves one capture, and this treats exactly that span as one
    /// moment. It still expires.
    #[test]
    fn a_resumed_walk_is_one_interval_from_its_first_page() {
        let resumed = walked(Provenance::CacheFlag, 0, 2_000);

        let just_after = walked(Provenance::CacheFlag, 2_010, 2_100);
        assert!(check_same_moment(&resumed, &just_after).is_ok());

        let much_later = outcome(Provenance::CacheFlag, 20_000);
        assert!(check_same_moment(&resumed, &much_later).is_err());
    }

    /// One side without evidence is enough to lose it, even against a walk.
    #[test]
    fn one_unverified_side_is_enough_to_refuse() {
        let walked = ListOutcome {
            provenance: Provenance::Walked,
            ..outcome(Provenance::CacheFlag, 0)
        };
        let stale = outcome(Provenance::CacheFlag, 5_000_000);
        assert!(check_same_moment(&walked, &stale).is_err());
    }

    /// The code has to match the cause, because `exit.rs` says these exist so a
    /// service can tell "wait a while" from everything else without reading the
    /// sentence. What the sentence says is `report`'s test: this module hands
    /// over the provenances and stops there.
    #[test]
    fn the_code_matches_why_nobody_checked() {
        let throttled = check_same_moment(
            &outcome(Provenance::Cooldown, 0),
            &outcome(Provenance::Cooldown, 5_000_000),
        )
        .unwrap_err();
        assert_eq!(
            crate::exit::from_chain(&throttled),
            Some(ExitCode::RateLimited)
        );

        let asked_for = check_same_moment(
            &outcome(Provenance::CacheFlag, 0),
            &outcome(Provenance::CacheFlag, 5_000_000),
        )
        .unwrap_err();
        assert_eq!(crate::exit::from_chain(&asked_for), Some(ExitCode::Error));
    }
}
