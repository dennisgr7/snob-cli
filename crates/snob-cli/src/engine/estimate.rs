//! What a list command would cost, worked out from what is stored here and
//! sent nowhere: `--dry-run`.
//!
//! An estimate, labeled as one. What it cannot know without asking is said as
//! a range: a stored list still fresh costs one request if its counter has not
//! moved and a walk if it has, and only the request that reads the counter can
//! tell which; a page brings back eight to twelve accounts, so a list is
//! between two numbers of pages. A list whose size was never read here is said
//! to be unknown rather than guessed.
//!
//! **The time is the pacer's, run forward on paper.** Every request pays the
//! pace and the day buckets of `rate_budget.rs`, from where they stand now; a
//! sitting of [`PAGES_PER_SITTING`] pages rests, counted across the walks of
//! the one process as `Pacer::page_walked` counts it; the steps and dwells are
//! `pace.rs`'s, the shortest for the least time and the longest for the most.
//! Nothing is a copy of those numbers, so a change to one moves this with it.
//!
//! [`compute`] is the arithmetic, over plain numbers, so every case is a unit
//! test; [`gather`] reads those numbers out of the store and the budget.
//! Neither decides how anything looks.

use std::time::Duration;

use anyhow::Result;
use snob_core::model::ListKind;
use snob_core::{Epoch, EpochMs};
use snob_ig::pace::{ACCOUNTS_PER_PAGE, DWELL_MS, PAGES_PER_SITTING, SITTING_PAUSE_MS, STEP_MS};
use snob_ig::pager::HARD_PAGE_CAP;
use snob_store::store::rate_budget::{
    BucketState, BudgetState, DAILY_BURST_MS, DAILY_EMISSION_MS, PACE_BURST_MS, PACE_EMISSION_MS,
};
use snob_store::store::{Store, accounts, snapshots};

use crate::app::Viewer;
use crate::engine::{ListQuery, target, walk};

/// One list as stored, the inputs of its estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub kind: ListKind,
    /// How many accounts it holds, as its counter last said or as its last
    /// walk found. `None` when it was never read here.
    pub size: Option<u64>,
    /// When its last complete walk finished.
    pub taken_at: Option<Epoch>,
    /// What the counter said when that walk was made, which the next run's
    /// counter is compared with: a list stored without one is walked again
    /// however fresh it is (`freshness::is_still_good`).
    pub declared: Option<u64>,
    /// What an interrupted walk the next run would continue has stored.
    pub resumable: Option<u64>,
    /// Whether another process is walking it right now.
    pub elsewhere: bool,
}

/// A bucket as it stands: how many it lets through now, and when the next
/// goes once none is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub left: u32,
    /// Milliseconds from now until it lets one through, while `left` is 0.
    pub free_in_ms: u64,
}

impl Bucket {
    /// A bucket at rest.
    pub const fn at_rest(most: u32) -> Self {
        Self {
            left: most,
            free_in_ms: 0,
        }
    }

    fn of(state: &BucketState, now: EpochMs) -> Self {
        Self {
            left: state.left,
            free_in_ms: u64::try_from(state.free_at.get() - now.get()).unwrap_or(0),
        }
    }
}

/// Everything [`compute`] needs.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// A name was typed, the viewer's own included: the account is found by
    /// its profile, whose answer carries the counters, so the first list is
    /// not polled. With none, the viewer's own lists need no finding and the
    /// first is polled.
    pub typed: bool,
    /// Whether the account's id is known here; finding it costs one more.
    pub pk_known: bool,
    /// Whether requests go out from the browser, which adds a navigation
    /// before a list and a `show_many` after each page.
    pub browser: bool,
    /// In the order the command walks them.
    pub lists: Vec<Stored>,
    pub refresh: bool,
    pub no_resume: bool,
    pub max_age: Duration,
    pub max_pages: Option<u32>,
    pub same_day: bool,
    pub now: Epoch,
    /// The two buckets every request pays.
    pub pace: Bucket,
    pub daily: Bucket,
    pub accounts_left: u64,
    pub held: Option<EpochMs>,
}

/// What would happen to one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Stored recently enough: reused if its counter has not moved, walked
    /// if it has.
    ReusedUnlessMoved,
    /// Walked: nothing stored, too old, no counter to compare, or
    /// `--refresh`.
    Walked,
    /// Never read here, so how big it is is not known; walked.
    Unknown,
    /// Another process is walking it: a run spends what comes before the
    /// walk and stops there.
    Refused,
    /// After a list the run stops at, so never reached.
    NotReached,
}

/// One list's estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEstimate {
    pub kind: ListKind,
    pub fate: Fate,
    pub size: Option<u64>,
    pub taken_at: Option<Epoch>,
    /// Accounts a walk would read: the size, less what a walk to resume has.
    pub to_read: u64,
    /// The fewest and the most pages a walk of it takes.
    pub pages: (u64, u64),
    /// The fewest and the most requests it could take, its poll included.
    pub requests: (u64, u64),
    /// Past [`HARD_PAGE_CAP`] pages: the walk stops there as truncated, and a
    /// crossing refuses it.
    pub truncated: bool,
}

/// The whole command's estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    /// Requests to find the account and open its profile, before any list.
    pub to_find: u64,
    pub lists: Vec<ListEstimate>,
    pub requests: (u64, u64),
    /// The most accounts it would read.
    pub accounts: u64,
    /// Seconds, the shortest and the longest, of the requests and the walks;
    /// a pause for the day's accounts is not in it.
    pub seconds: (u64, u64),
    /// What the day's bucket has left now.
    pub requests_left: u64,
    pub accounts_left: u64,
    /// Whether it would pause for the day's accounts, which `--same-day`
    /// overrides.
    pub pauses: bool,
    pub same_day: bool,
    pub held: Option<EpochMs>,
}

impl Estimate {
    /// Whether a list of unknown size is in it, whose pages past the first
    /// are not counted.
    pub fn unknown(&self) -> bool {
        self.lists.iter().any(|list| list.fate == Fate::Unknown)
    }

    /// Whether a list in it would stop as truncated.
    pub fn truncated(&self) -> bool {
        self.lists.iter().any(|list| list.truncated)
    }
}

/// What finding an account by its name costs, before any list: from the
/// browser its profile, the query and the three reads the app sends beside it
/// (`IgClient::profile_burst`), plus the name's lookup when its id is not
/// known here; without it, one read either way.
pub fn finding(browser: bool, pk_known: bool) -> u64 {
    match (browser, pk_known) {
        (true, true) => 4,
        (true, false) => 5,
        (false, _) => 1,
    }
}

/// About how many requests walking lists of these `sizes` takes, of an
/// account found by its name: its finding, then each walk, every list after
/// the first polled again since the first one's walk began an action. What
/// the full-screen views offer to spend switching to another account.
pub fn rewalk_requests(browser: bool, pk_known: bool, sizes: &[u64]) -> u64 {
    let walks: u64 = sizes
        .iter()
        .map(|&size| walk::requests_to_walk(size, browser))
        .sum();
    finding(browser, pk_known) + walks + sizes.len().saturating_sub(1) as u64
}

/// The arithmetic.
pub fn compute(inputs: &Inputs) -> Estimate {
    let to_find = if inputs.typed {
        finding(inputs.browser, inputs.pk_known)
    } else {
        0
    };
    let plans = plan(inputs);
    let least = Run::through(inputs, &plans, to_find, Scenario::Least);
    let most = Run::through(inputs, &plans, to_find, Scenario::Most);

    let lists = plans
        .iter()
        .enumerate()
        .map(|(at, plan)| ListEstimate {
            kind: plan.stored.kind,
            fate: plan.fate,
            size: plan.stored.size,
            taken_at: plan.stored.taken_at,
            to_read: plan.to_read,
            pages: plan.pages,
            requests: (least.per_list[at], most.per_list[at]),
            truncated: plan.truncated,
        })
        .collect();
    Estimate {
        to_find,
        lists,
        requests: (least.requests, most.requests),
        accounts: most.accounts,
        seconds: (least.ms / 1000, most.ms.div_ceil(1000)),
        requests_left: u64::from(inputs.daily.left),
        accounts_left: inputs.accounts_left,
        pauses: !inputs.same_day && most.accounts > inputs.accounts_left,
        same_day: inputs.same_day,
        held: inputs.held,
    }
}

/// What each list would be, before anything is timed.
struct Plan<'a> {
    stored: &'a Stored,
    fate: Fate,
    to_read: u64,
    pages: (u64, u64),
    truncated: bool,
}

fn plan(inputs: &Inputs) -> Vec<Plan<'_>> {
    let cap = |pages: u64| {
        let pages = pages.min(u64::from(HARD_PAGE_CAP));
        match inputs.max_pages {
            Some(most) => pages.min(u64::from(most)),
            None => pages,
        }
    };
    let mut stopped = false;
    let mut plans = Vec::new();
    for stored in &inputs.lists {
        let fresh = !inputs.refresh
            && stored
                .taken_at
                .is_some_and(|taken| taken + inputs.max_age >= inputs.now);
        let fate = if stopped {
            Fate::NotReached
        } else if stored.elsewhere {
            stopped = true;
            Fate::Refused
        } else if stored.size.is_none() {
            Fate::Unknown
        } else if fresh && stored.declared.is_some() {
            Fate::ReusedUnlessMoved
        } else {
            Fate::Walked
        };
        let already = if inputs.no_resume {
            0
        } else {
            stored.resumable.unwrap_or(0)
        };
        let to_read = stored.size.unwrap_or(0).saturating_sub(already);
        let (pages, truncated) = match fate {
            // At least the first page; the rest is not known.
            Fate::Unknown => ((cap(1), cap(1)), false),
            Fate::Refused | Fate::NotReached => ((0, 0), false),
            Fate::Walked | Fate::ReusedUnlessMoved => {
                let fewest = walk::pages_to_walk(to_read);
                let truncated = fewest > u64::from(HARD_PAGE_CAP)
                    && inputs
                        .max_pages
                        .is_none_or(|most| u64::from(most) > u64::from(HARD_PAGE_CAP));
                (
                    (cap(fewest), cap(walk::pages_to_walk_at_most(to_read))),
                    truncated,
                )
            }
        };
        plans.push(Plan {
            stored,
            fate,
            to_read,
            pages,
            truncated,
        });
    }
    plans
}

/// Which end of the range a run works out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// A fresh list reused, the fewest pages, the shortest waits.
    Least,
    /// A fresh list walked, the most pages, the longest waits.
    Most,
}

/// One bucket of the request budget, run forward: GCRA, as `rate_budget.rs`
/// decides a request. A request may go at `t` once `tat - burst <= t`, and
/// moves `tat` on by one emission from whichever is later.
#[derive(Debug, Clone, Copy)]
struct Gcra {
    tat: i64,
    emission: i64,
    burst: i64,
}

impl Gcra {
    fn starting(bucket: Bucket, emission: i64, burst: i64) -> Self {
        let tat = if bucket.left == 0 {
            i64::try_from(bucket.free_in_ms).unwrap_or(i64::MAX / 4) + burst
        } else {
            burst - (i64::from(bucket.left) - 1) * emission
        };
        Self {
            tat,
            emission,
            burst,
        }
    }

    fn free_at(&self) -> i64 {
        self.tat - self.burst
    }

    fn pay(&mut self, at: i64) {
        self.tat = self.tat.max(at) + self.emission;
    }
}

/// The command, run on paper: the requests it sends, in order, each paid
/// from both buckets, and the waits between them.
struct Run {
    scenario: Scenario,
    ms: u64,
    pace: Gcra,
    daily: Gcra,
    /// Pages walked since the last rest, across walks, as the pacer counts.
    sitting: u32,
    requests: u64,
    accounts: u64,
    per_list: Vec<u64>,
}

impl Run {
    fn through(inputs: &Inputs, plans: &[Plan<'_>], to_find: u64, scenario: Scenario) -> Self {
        let mut run = Self {
            scenario,
            ms: 0,
            pace: Gcra::starting(inputs.pace, PACE_EMISSION_MS, PACE_BURST_MS),
            daily: Gcra::starting(inputs.daily, DAILY_EMISSION_MS, DAILY_BURST_MS),
            sitting: 0,
            requests: 0,
            accounts: 0,
            per_list: Vec::new(),
        };
        for _ in 0..to_find {
            run.send();
        }
        // Whether a list before this one was walked, which begins an action
        // and makes the next list's counters a request again.
        let mut walked_before = false;
        for (at, plan) in plans.iter().enumerate() {
            let before = run.requests;
            if plan.fate != Fate::NotReached {
                let poll = if at == 0 {
                    !inputs.typed
                } else {
                    walked_before
                };
                if poll {
                    run.send();
                }
                let walks = match plan.fate {
                    Fate::Walked | Fate::Unknown => true,
                    Fate::ReusedUnlessMoved => scenario == Scenario::Most,
                    Fate::Refused | Fate::NotReached => false,
                };
                if walks {
                    let pages = match scenario {
                        Scenario::Least => plan.pages.0,
                        Scenario::Most => plan.pages.1,
                    };
                    run.walk(pages, inputs.browser);
                    run.accounts += plan.to_read.min(pages * u64::from(ACCOUNTS_PER_PAGE));
                    walked_before = true;
                }
            }
            run.per_list.push(run.requests - before);
        }
        run
    }

    /// One of the two ends of a pace range.
    fn pick(&self, (least, most): (u64, u64)) -> u64 {
        match self.scenario {
            Scenario::Least => least,
            Scenario::Most => most,
        }
    }

    fn wait(&mut self, ms: u64) {
        self.ms += ms;
    }

    /// One request, sent once both buckets let it through.
    fn send(&mut self) {
        let now = i64::try_from(self.ms).unwrap_or(i64::MAX / 4);
        let at = now.max(self.pace.free_at()).max(self.daily.free_at());
        self.ms = u64::try_from(at).unwrap_or(self.ms);
        self.pace.pay(at);
        self.daily.pay(at);
        self.requests += 1;
    }

    /// A walk of `pages`, as the pager makes it: the dwell, the navigation
    /// from the browser, then each page (and its `show_many`) a step apart,
    /// resting after a full sitting unless the walk ends there.
    fn walk(&mut self, pages: u64, browser: bool) {
        if pages == 0 {
            return;
        }
        self.wait(self.pick(DWELL_MS));
        if browser {
            self.send();
        }
        for page in 0..pages {
            if page > 0 {
                self.wait(self.pick(STEP_MS));
            }
            self.send();
            if browser {
                self.send();
            }
            self.sitting += 1;
            if self.sitting >= PAGES_PER_SITTING && page + 1 < pages {
                self.wait(self.pick(SITTING_PAUSE_MS));
                self.sitting = 0;
            }
        }
    }
}

/// What the command stands on now, read and not charged.
pub struct Standing<'a> {
    pub viewer: &'a Viewer,
    pub store: &'a Store,
    /// Whether requests would go out from the browser.
    pub browser: bool,
    pub budget: &'a BudgetState,
    pub held: Option<EpochMs>,
    pub now: EpochMs,
}

/// Reads [`Inputs`] for `query` and the lists `kinds`, in order, out of the
/// store and the budget. Sends nothing and writes nothing.
pub fn gather(standing: &Standing<'_>, query: &ListQuery, kinds: &[ListKind]) -> Result<Inputs> {
    let pk = match query.target.as_deref() {
        None => Some(standing.viewer.pk),
        Some(typed) => target::known_pk_in(standing.viewer, standing.store, typed)?,
    };
    let conn = standing.store.conn();
    let counters = match pk {
        Some(pk) => accounts::find(conn, pk)?,
        None => None,
    };
    let mut lists = Vec::new();
    for &kind in kinds {
        let Some(pk) = pk else {
            lists.push(Stored {
                kind,
                size: None,
                taken_at: None,
                declared: None,
                resumable: None,
                elsewhere: false,
            });
            continue;
        };
        let latest = snapshots::latest_complete(conn, pk, kind)?;
        let size = counters
            .as_ref()
            .and_then(|c| c.counter(kind))
            .or_else(|| latest.as_ref().and_then(|s| s.declared_count))
            .or_else(|| latest.as_ref().map(|s| s.member_count));
        lists.push(Stored {
            kind,
            size,
            taken_at: latest.as_ref().and_then(|s| s.taken_at),
            declared: latest.as_ref().and_then(|s| s.declared_count),
            resumable: snapshots::resumable_progress(conn, pk, kind)?,
            elsewhere: snapshots::walked_elsewhere(conn, pk, kind)?,
        });
    }
    let budget = standing.budget;
    Ok(Inputs {
        typed: query.target.is_some(),
        pk_known: pk.is_some(),
        browser: standing.browser,
        lists,
        refresh: query.refresh,
        no_resume: query.no_resume,
        max_age: query.max_age,
        max_pages: query.max_pages,
        same_day: query.over_budget.is_some(),
        now: standing.now.to_epoch(),
        pace: Bucket::of(&budget.pace, standing.now),
        daily: Bucket::of(&budget.daily, standing.now),
        accounts_left: budget.accounts_left(),
        held: standing.held,
    })
}

/// The budget of an account with nothing spent, for a test or a caller with
/// no database: both buckets at rest.
pub fn at_rest() -> (Bucket, Bucket) {
    let most = |emission: i64, burst: i64| u32::try_from(burst / emission + 1).unwrap_or(u32::MAX);
    (
        Bucket::at_rest(most(PACE_EMISSION_MS, PACE_BURST_MS)),
        Bucket::at_rest(most(DAILY_EMISSION_MS, DAILY_BURST_MS)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Epoch = Epoch::new(1_800_000_000);
    const HOUR: Duration = Duration::from_secs(3600);

    fn stored(kind: ListKind, size: Option<u64>, age: Option<Duration>) -> Stored {
        Stored {
            kind,
            size,
            taken_at: age.map(|age| NOW - age),
            declared: size,
            resumable: None,
            elsewhere: false,
        }
    }

    fn inputs(typed: bool, browser: bool, lists: Vec<Stored>) -> Inputs {
        let (pace, daily) = at_rest();
        Inputs {
            typed,
            pk_known: true,
            browser,
            lists,
            refresh: false,
            no_resume: false,
            max_age: 24 * HOUR,
            max_pages: None,
            same_day: false,
            now: NOW,
            pace,
            daily,
            accounts_left: 2_000,
            held: None,
        }
    }

    /// Your own list, never walked: one poll, a navigation and two requests
    /// a page from the browser; one a page and the poll without it. At eight
    /// accounts a page it is half as many pages again.
    #[test]
    fn a_list_to_walk_is_its_pages_and_what_goes_with_them() {
        let list = || vec![stored(ListKind::Followers, Some(120), None)];
        let from_the_browser = compute(&inputs(false, true, list()));
        assert_eq!(from_the_browser.to_find, 0);
        assert_eq!(from_the_browser.lists[0].fate, Fate::Walked);
        assert_eq!(from_the_browser.lists[0].pages, (10, 15));
        assert_eq!(
            from_the_browser.requests,
            (22, 32),
            "1 + 1 + 2 x 10, and 1 + 1 + 2 x 15"
        );
        assert_eq!(from_the_browser.accounts, 120);

        let direct = compute(&inputs(false, false, list()));
        assert_eq!(
            direct.requests,
            (11, 16),
            "the poll and ten to fifteen pages"
        );
    }

    /// A fresh list is one request if nothing moved and a walk if it did; a
    /// stale one, one stored with no counter to compare, or `--refresh`, is
    /// a walk for certain.
    #[test]
    fn a_fresh_list_is_a_range_and_a_stale_one_is_a_walk() {
        let fresh = compute(&inputs(
            false,
            true,
            vec![stored(ListKind::Followers, Some(24), Some(HOUR))],
        ));
        assert_eq!(fresh.lists[0].fate, Fate::ReusedUnlessMoved);
        assert_eq!(fresh.requests, (1, 1 + 1 + 2 * 3));
        assert_eq!(fresh.seconds.0, 0, "reused, it takes no time");

        let stale = compute(&inputs(
            false,
            true,
            vec![stored(ListKind::Followers, Some(24), Some(48 * HOUR))],
        ));
        assert_eq!(stale.lists[0].fate, Fate::Walked);
        assert_eq!(stale.requests, (6, 8));

        let mut uncounted = stored(ListKind::Followers, Some(24), Some(HOUR));
        uncounted.declared = None;
        let uncounted = compute(&inputs(false, true, vec![uncounted]));
        assert_eq!(
            uncounted.lists[0].fate,
            Fate::Walked,
            "with no counter stored, the poll cannot match it"
        );

        let mut refreshed = inputs(
            false,
            true,
            vec![stored(ListKind::Followers, Some(24), Some(HOUR))],
        );
        refreshed.refresh = true;
        assert_eq!(compute(&refreshed).lists[0].fate, Fate::Walked);
    }

    /// A typed name costs its finding, which carries the first list's
    /// counter; the second list is polled again only if the first was walked.
    /// The viewer's own name typed is found the same way.
    #[test]
    fn a_typed_name_costs_its_finding_and_a_crossing_its_second_poll() {
        // Eight each: one page however few a page brings back.
        let both = || {
            vec![
                stored(ListKind::Followers, Some(8), None),
                stored(ListKind::Following, Some(8), None),
            ]
        };
        let crossing = compute(&inputs(true, true, both()));
        assert_eq!(crossing.to_find, 4);
        assert_eq!(crossing.lists[0].requests, (3, 3), "no poll, 1 + 2 x 1");
        assert_eq!(crossing.lists[1].requests, (4, 4), "a poll, 1 + 2 x 1");
        assert_eq!(crossing.requests, (11, 11));

        let mut unknown = inputs(
            true,
            true,
            vec![
                stored(ListKind::Followers, None, None),
                stored(ListKind::Following, None, None),
            ],
        );
        unknown.pk_known = false;
        let unknown = compute(&unknown);
        assert_eq!(unknown.to_find, 5);
        assert!(unknown.lists.iter().all(|l| l.fate == Fate::Unknown));
        assert!(unknown.unknown());
        assert_eq!(
            unknown.lists[1].requests,
            (4, 4),
            "a list never read is walked, so the next one is polled"
        );
    }

    /// A walk left to resume is what is left of it; `--no-resume` is the
    /// whole list; `--max-pages` caps the pages, and none sends nothing.
    #[test]
    fn what_a_walk_already_has_and_a_page_cap_are_taken_off() {
        let mut list = stored(ListKind::Following, Some(240), None);
        list.resumable = Some(120);
        let resumed = compute(&inputs(false, false, vec![list.clone()]));
        assert_eq!(
            (resumed.lists[0].to_read, resumed.lists[0].pages),
            (120, (10, 15))
        );

        let mut again = inputs(false, false, vec![list.clone()]);
        again.no_resume = true;
        assert_eq!(compute(&again).lists[0].pages, (20, 30));

        let mut capped = inputs(false, false, vec![list.clone()]);
        capped.max_pages = Some(3);
        let capped = compute(&capped);
        assert_eq!(capped.lists[0].pages, (3, 3));
        assert_eq!(capped.accounts, 36);

        let mut none = inputs(false, true, vec![list]);
        none.max_pages = Some(0);
        assert_eq!(compute(&none).requests, (1, 1), "the poll, and no page");
    }

    /// More than the day's accounts pauses, unless `--same-day`.
    #[test]
    fn more_than_the_days_accounts_pauses_unless_finished_the_same_day() {
        let big = || vec![stored(ListKind::Followers, Some(5_000), None)];
        let paused = compute(&inputs(false, true, big()));
        assert!(paused.pauses);
        let mut same_day = inputs(false, true, big());
        same_day.same_day = true;
        assert!(!compute(&same_day).pauses);
    }

    /// Another process walking the list: a run spends its poll and stops,
    /// and the list after it is never reached.
    #[test]
    fn a_list_walked_elsewhere_stops_the_run() {
        let mut first = stored(ListKind::Followers, Some(500), None);
        first.elsewhere = true;
        let second = stored(ListKind::Following, Some(500), None);
        let refused = compute(&inputs(false, true, vec![first, second]));
        assert_eq!(refused.lists[0].fate, Fate::Refused);
        assert_eq!(refused.lists[1].fate, Fate::NotReached);
        assert_eq!(refused.requests, (1, 1), "the poll, and nothing after");
    }

    /// Past the hard page cap a walk is truncated.
    #[test]
    fn a_list_past_the_page_cap_is_truncated() {
        let huge = compute(&inputs(
            false,
            false,
            vec![stored(ListKind::Followers, Some(30_000), None)],
        ));
        assert!(huge.truncated());
        assert_eq!(huge.lists[0].pages, (2_000, 2_000));
    }

    /// A sitting rests between sittings and not after the last, and the
    /// sitting is the process's: a second walk carries on the first one's.
    #[test]
    fn a_rest_falls_where_the_pacer_puts_it() {
        let seconds = |lists: Vec<Stored>| compute(&inputs(false, false, lists)).seconds;
        let one = seconds(vec![stored(ListKind::Followers, Some(40 * 12), None)]);
        assert!(
            one.0 < 300,
            "forty pages, one sitting that the walk ends on: no rest ({one:?})"
        );
        assert!(
            one.1 >= 900,
            "sixty pages at eight a page: one rest of up to fifteen minutes ({one:?})"
        );

        // Two lists of thirty pages each: a rest inside the second walk.
        let crossing = seconds(vec![
            stored(ListKind::Followers, Some(30 * 12), None),
            stored(ListKind::Following, Some(30 * 12), None),
        ]);
        assert!(
            crossing.0 >= 300,
            "sixty pages across two walks rest once: {crossing:?}"
        );
    }

    /// The day's bucket moves the time once it runs out.
    #[test]
    fn a_spent_day_spaces_the_requests() {
        let list = || vec![stored(ListKind::Followers, Some(12), Some(48 * HOUR))];
        let rested = compute(&inputs(false, false, list())).seconds;
        let mut spent = inputs(false, false, list());
        spent.daily = Bucket {
            left: 0,
            free_in_ms: 600_000,
        };
        let spent = compute(&spent).seconds;
        assert!(spent.0 >= rested.0 + 600, "{spent:?} against {rested:?}");
    }

    /// Re-walking as another account counts as the estimate counts.
    #[test]
    fn a_rewalk_costs_its_finding_its_walks_and_a_second_poll() {
        assert_eq!(rewalk_requests(true, true, &[24]), 4 + 5);
        assert_eq!(rewalk_requests(true, false, &[24, 24]), 5 + 5 + 5 + 1);
        assert_eq!(rewalk_requests(false, true, &[24]), 1 + 2);
    }
}
