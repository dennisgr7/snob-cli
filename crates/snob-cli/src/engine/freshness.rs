//! Whether a stored list still answers the question, and the one request that
//! finds out.
//!
//! This is where nearly all of the tool's savings come from. Walking a list of
//! three hundred costs twenty-five requests; asking whether it changed costs one.
//! So the counter is always polled, including before the very first walk —
//! without it the first snapshot would be born with no counter to compare
//! against and the cache could never hit at all.

use anyhow::Result;
use snob_core::clock::now;
use snob_core::model::{ListKind, User};
use snob_ig::error::IgError;
use snob_store::store::{accounts, snapshots};

use crate::app::App;
use crate::engine::target::Target;
use crate::engine::{ListOutcome, ListQuery, Provenance, walk};

/// Polls, compares, and either serves what is stored or walks.
pub async fn decide_and_fetch(
    app: &mut App,
    args: &ListQuery,
    kind: ListKind,
    target: &Target,
    stored: Option<snapshots::Snapshot>,
) -> Result<(Vec<User>, ListOutcome)> {
    let declared = match poll(app, target, kind).await {
        Ok(counter) => counter,
        // A security check or a dead session is the reader's to clear, and
        // nothing stored answers it: said as itself, with its own exit code
        // and, for a challenge, its link. A page that has not shown what the
        // poll is built from sent nothing, so nothing is known to be wrong
        // with Instagram: said as itself too, and the next run asks again.
        Err(e)
            if e.downcast_ref::<IgError>().is_some_and(|e| {
                e.invalidates_session() || matches!(e, IgError::PageNotReady(_))
            }) =>
        {
            return Err(e);
        }
        Err(e) => {
            // Walking the whole list right when Instagram is already having
            // trouble is the worst possible reaction, so anything stored wins.
            //
            // Except under `--refresh`, whose whole help text is "walk the list
            // again": it falls through to where a first-ever run already is, no
            // counter to compare against, so walk. That costs one request
            // against an endpoint that just failed, which is the price of the
            // flag meaning what it says; the pager does not retry a push-back,
            // so it is one and not four.
            if let Some(snapshot) = &stored
                && !args.refresh
            {
                app.warn(&crate::report::poll_failed_serving_stored(&e));
                // Served, but with nothing said about whether it is still
                // true. It is fine to print; it is not fine to cross against
                // another list, and only the provenance can carry that.
                return super::serve_stored(app, snapshot, Provenance::PollFailed);
            }
            app.warn(&crate::report::poll_failed(&e));
            None
        }
    };

    if !args.refresh
        && let Some(snapshot) = &stored
        && is_still_good(snapshot, declared, args.max_age)
    {
        // The counter was polled just now and had not moved, so this describes
        // the account as it is however old the snapshot is. That is what makes
        // it safe to cross.
        return super::serve_stored(app, snapshot, Provenance::CounterVerified);
    }

    // The counter moved, or the capture aged out; either way the list has
    // stopped answering. A caller that walks on a timer can still say it has
    // walked this one recently enough, and then the change waits for a later
    // walk rather than costing a whole list every time a counter ticks.
    if !args.refresh
        && let Some(gap) = args.walk_at_most_every
        && let Some(snapshot) = &stored
        && snapshot.taken_at.unwrap_or_default() + gap > now()
    {
        return super::serve_stored(app, snapshot, Provenance::WalkedRecently);
    }

    walk::fetch(app, args, kind, target, declared).await
}

/// The two conditions a stored list has to meet, both of them necessary.
///
/// Age alone is not enough — a list that changed two minutes ago is wrong
/// however fresh — and an unmoved counter alone is not enough either, because
/// the same number can hide one arrival and one departure. Together they are
/// what makes reusing it honest.
fn is_still_good(
    snapshot: &snapshots::Snapshot,
    declared: Option<u64>,
    max_age: std::time::Duration,
) -> bool {
    // Saturating, so the longest `--max-age` anyone can write reuses rather
    // than walks.
    let fresh = snapshot.taken_at.unwrap_or_default() + max_age >= now();
    // `None` never counts as unchanged: not knowing the counter is not the
    // same as knowing it stayed put.
    let unchanged = declared.is_some() && declared == snapshot.declared_count;
    fresh && unchanged
}

/// A stored list that still answers, if there is one — for a caller that
/// already holds today's counter and wants to know whether opening the list
/// will cost anything before asking anybody about spending.
///
/// The interactive profile view is that caller: the counter came off
/// `web_profile_info` moments ago, so comparing against it is the same
/// honesty [`is_still_good`] gives the poll, without spending the poll. The
/// members are returned rather than the snapshot, because the one thing the
/// caller does with a fresh list is show it.
pub(crate) fn fresh_members(
    app: &App,
    account: snob_core::Pk,
    kind: ListKind,
    declared: Option<u64>,
    max_age: std::time::Duration,
) -> Result<Option<Vec<User>>> {
    let Some(snapshot) = snapshots::latest_complete(app.db().conn(), account, kind)? else {
        return Ok(None);
    };
    if !is_still_good(&snapshot, declared, max_age) {
        return Ok(None);
    }
    Ok(Some(snapshots::members(app.db().conn(), snapshot.id)?))
}

/// Reads the counters. The cheapest request there is, and the one that avoids
/// walking a list that has not changed.
///
/// It costs nothing at all when the target was resolved by name: resolving and
/// polling are both the account's profile, so the answer is already in hand
/// and asking twice would only spend the request that the whole cache policy
/// exists to save. Otherwise it is [`snob_ig::client::IgClient::counters`]:
/// from the browser the hover card, by pk; without one the profile, by a name,
/// and nothing at all when there is no name to ask with.
async fn poll(app: &mut App, target: &Target, kind: ListKind) -> Result<Option<u64>> {
    let counters = match target.counters {
        Some(counters) => counters,
        None => {
            let Some(counters) = app
                .client()
                .counters(target.pk, target.username.as_deref())
                .await?
            else {
                return Ok(None);
            };
            // One answer carries both counters, so the other list of a crossing
            // does not have to ask again.
            app.remember_counters(counters);
            counters
        }
    };

    accounts::record_poll(
        app.db().conn(),
        target.pk,
        counters.followers,
        counters.following,
    )?;
    Ok(counters.of(kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Epoch;

    fn snapshot(taken_at: Epoch, declared: Option<u64>) -> snapshots::Snapshot {
        crate::engine::stored_snapshot(taken_at, taken_at, declared)
    }

    const SIX_HOURS: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

    #[test]
    fn fresh_and_unmoved_is_the_only_case_that_is_reused() {
        let recent = snapshot(now(), Some(300));
        assert!(is_still_good(&recent, Some(300), SIX_HOURS));
    }

    #[test]
    fn a_moved_counter_is_walked_however_fresh_the_list_is() {
        let recent = snapshot(now(), Some(300));
        assert!(!is_still_good(&recent, Some(301), SIX_HOURS));
    }

    #[test]
    fn an_old_list_is_walked_however_still_the_counter_is() {
        let old = snapshot(
            now() - SIX_HOURS - std::time::Duration::from_secs(1),
            Some(300),
        );
        assert!(!is_still_good(&old, Some(300), SIX_HOURS));
    }

    /// The longest maximum age anyone can write must not mean the shortest.
    ///
    /// `duration::parse` refuses what will not fit in an `i64`, and a
    /// `Duration` built any other way still saturates instead of wrapping: a
    /// saturated bound reuses everything rather than walking everything.
    #[test]
    fn an_absurd_maximum_age_reuses_rather_than_walks() {
        let ancient = snapshot(Epoch::default(), Some(300));
        let forever = std::time::Duration::from_secs(u64::MAX);

        assert!(
            is_still_good(&ancient, Some(300), forever),
            "the longest age anyone can write is the one that expires nothing"
        );
    }

    /// Not knowing the counter is not the same as knowing it stayed put. If an
    /// unknown counted as unchanged, a failed poll would freeze the cache.
    #[test]
    fn an_unknown_counter_is_never_taken_as_unchanged() {
        let recent = snapshot(now(), Some(300));
        assert!(!is_still_good(&recent, None, SIX_HOURS));

        let never_counted = snapshot(now(), None);
        assert!(!is_still_good(&never_counted, None, SIX_HOURS));
    }
}
