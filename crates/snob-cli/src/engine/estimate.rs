//! What a list command would cost, worked out from what is stored here and
//! sent nowhere: `--dry-run`.
//!
//! An estimate, labeled as one. What it cannot know without asking is said as
//! a range: a stored list still fresh costs one request if its counter has not
//! moved and a walk if it has, and only the request that reads the counter can
//! tell which. A list whose size was never read here is said to be unknown
//! rather than guessed.
//!
//! [`compute`] is the arithmetic, over plain numbers, so every case is a unit
//! test; [`gather`] reads those numbers out of the store and the budget.
//! Neither decides how anything looks.

use std::time::Duration;

use anyhow::Result;
use snob_core::model::ListKind;
use snob_core::{Epoch, EpochMs};
use snob_store::store::{accounts, rate_budget, snapshots};

use crate::app::App;
use crate::engine::{ListQuery, target, walk};

/// What the web app asks for in a page of a list.
const ACCOUNTS_PER_PAGE: u64 = snob_ig::pace::ACCOUNTS_PER_PAGE as u64;

/// How many pages of a sitting the pace bucket lets through at the walker's
/// step before it starts pacing them, and how long a page takes after that:
/// from the browser each page is two requests (the page and its
/// `show_many`), so about fourteen pages at two seconds and then two
/// emissions of 3.83 s each; without it, one request a page, so about
/// forty-two and then one emission. The reasoning is at `PACE_EMISSION_MS`
/// in `rate_budget.rs`.
const FREE_PAGES_FROM_THE_BROWSER: u64 = 14;
const PACED_PAGE_FROM_THE_BROWSER_MS: u64 = 7_660;
const FREE_PAGES_DIRECT: u64 = 42;
const PACED_PAGE_DIRECT_MS: u64 = 3_830;
/// The walker's mean step between pages, and its mean dwell before a list.
const STEP_MS: u64 = 2_000;
const DWELL_MS: u64 = 2_750;

/// One list as stored, the inputs of its estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub kind: ListKind,
    /// How many accounts it holds, as its counter last said or as its last
    /// walk found. `None` when it was never read here.
    pub size: Option<u64>,
    /// When its last complete walk finished.
    pub taken_at: Option<Epoch>,
    /// What an interrupted walk the next run would continue has stored.
    pub resumable: Option<u64>,
    /// Whether another process is walking it right now.
    pub elsewhere: bool,
}

/// Everything [`compute`] needs.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// The viewer's own lists, whose account needs no finding.
    pub own: bool,
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
    pub requests_left: u64,
    pub accounts_left: u64,
    pub held: Option<EpochMs>,
}

/// What would happen to one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Stored recently enough: reused if its counter has not moved, walked
    /// if it has.
    ReusedUnlessMoved,
    /// Walked: nothing stored, too old, or `--refresh`.
    Walked,
    /// Never read here, so how big it is is not known.
    Unknown,
    /// Another process is walking it, and a run would refuse.
    Refused,
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
    pub pages: u64,
    /// The fewest and the most requests it could take.
    pub requests: (u64, u64),
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
    /// Seconds, the shortest and the longest, of the walks alone; a pause for
    /// the day's accounts is not in it.
    pub seconds: (u64, u64),
    pub requests_left: u64,
    pub accounts_left: u64,
    /// Whether it would pause for the day's accounts, which `--same-day`
    /// overrides.
    pub pauses: bool,
    pub same_day: bool,
    pub held: Option<EpochMs>,
}

/// The arithmetic.
pub fn compute(inputs: &Inputs) -> Estimate {
    // From the browser, another account is found by its profile: the
    // query and the three reads the app sends beside it, plus the name's
    // lookup the first time. Without it, one read either way. Those reads
    // carry the counters, so the first list is polled for nothing.
    let to_find = match (inputs.own, inputs.browser, inputs.pk_known) {
        (true, _, _) => 0,
        (false, true, true) => 4,
        (false, true, false) => 5,
        (false, false, _) => 1,
    };

    let mut lists = Vec::new();
    let (mut low, mut high) = (to_find, to_find);
    let (mut accounts, mut seconds_low, mut seconds_high) = (0, 0, 0);
    // Whether a list before this one may have been walked, which moves the
    // action on and makes the next list's counters a request again.
    let (mut walked_before_low, mut walked_before_high) = (false, false);
    for (at, stored) in inputs.lists.iter().enumerate() {
        let first = at == 0;
        let poll = |walked_before: bool| u64::from(first && inputs.own || !first && walked_before);
        let fresh = !inputs.refresh
            && stored
                .taken_at
                .is_some_and(|taken| taken + inputs.max_age >= inputs.now);
        let fate = if stored.elsewhere {
            Fate::Refused
        } else if stored.size.is_none() {
            Fate::Unknown
        } else if fresh {
            Fate::ReusedUnlessMoved
        } else {
            Fate::Walked
        };
        let size = stored.size.unwrap_or(0);
        let already = if inputs.no_resume {
            0
        } else {
            stored.resumable.unwrap_or(0)
        };
        let to_read = size.saturating_sub(already);
        let mut pages = walk::pages_to_walk(to_read);
        if let Some(most) = inputs.max_pages {
            pages = pages.min(u64::from(most));
        }
        let walk = walk::requests_for_pages(pages, inputs.browser);
        let (list_low, list_high) = match fate {
            Fate::Refused => (0, 0),
            Fate::Unknown => (poll(walked_before_low), poll(walked_before_high)),
            Fate::ReusedUnlessMoved => (poll(walked_before_low), poll(walked_before_high) + walk),
            Fate::Walked => (
                poll(walked_before_low) + walk,
                poll(walked_before_high) + walk,
            ),
        };
        low += list_low;
        high += list_high;
        let walks_at_most = matches!(fate, Fate::Walked | Fate::ReusedUnlessMoved);
        if walks_at_most {
            accounts += to_read.min(pages * ACCOUNTS_PER_PAGE);
            let time = walk_seconds(pages, inputs.browser);
            if fate == Fate::Walked {
                seconds_low += time.0;
            }
            seconds_high += time.1;
        }
        walked_before_low |= fate == Fate::Walked;
        walked_before_high |= walks_at_most;
        lists.push(ListEstimate {
            kind: stored.kind,
            fate,
            size: stored.size,
            taken_at: stored.taken_at,
            to_read,
            pages,
            requests: (list_low, list_high),
        });
    }

    Estimate {
        to_find,
        lists,
        requests: (low, high),
        accounts,
        seconds: (seconds_low, seconds_high),
        requests_left: inputs.requests_left,
        accounts_left: inputs.accounts_left,
        pauses: !inputs.same_day && accounts > inputs.accounts_left,
        same_day: inputs.same_day,
        held: inputs.held,
    }
}

/// How long a walk of `pages` takes, the shortest and the longest: the dwell,
/// the steps the pace bucket lets through, the paced pages after them, and a
/// rest of five to fifteen minutes every sitting it does not end on.
fn walk_seconds(pages: u64, browser: bool) -> (u64, u64) {
    let (free, paced) = if browser {
        (FREE_PAGES_FROM_THE_BROWSER, PACED_PAGE_FROM_THE_BROWSER_MS)
    } else {
        (FREE_PAGES_DIRECT, PACED_PAGE_DIRECT_MS)
    };
    let sitting = u64::from(snob_ig::pace::PAGES_PER_SITTING);
    let rests = pages.saturating_sub(1) / sitting;
    let mut ms = DWELL_MS;
    for page in 0..pages {
        ms += if page % sitting < free {
            STEP_MS
        } else {
            paced
        };
    }
    (ms / 1000 + rests * 5 * 60, ms / 1000 + rests * 15 * 60)
}

/// Reads [`Inputs`] for `query` and the lists `kinds`, in order, out of the
/// store and the budget. Sends nothing.
pub fn gather(app: &App, query: &ListQuery, kinds: &[ListKind]) -> Result<Inputs> {
    let typed = query.target.as_deref().unwrap_or_default();
    let pk = target::known_pk_of(app, query.target.as_deref(), typed)?;
    let own = pk == Some(app.viewer().pk);
    let conn = app.db().conn();
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
            resumable: snapshots::resumable_progress(conn, pk, kind)?,
            elsewhere: snapshots::walked_elsewhere(conn, pk, kind)?,
        });
    }
    let now_ms = snob_core::clock::now_ms();
    let budget = rate_budget::state(conn, None, now_ms)?;
    Ok(Inputs {
        own,
        pk_known: pk.is_some(),
        browser: app.client().has_page(),
        lists,
        refresh: query.refresh,
        no_resume: query.no_resume,
        max_age: query.max_age,
        max_pages: query.max_pages,
        same_day: query.over_budget.is_some(),
        now: now_ms.to_epoch(),
        requests_left: u64::from(budget.daily.left),
        accounts_left: budget.accounts_left(),
        held: app.held()?.map(|held| held.until_ms),
    })
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
            resumable: None,
            elsewhere: false,
        }
    }

    fn inputs(own: bool, browser: bool, lists: Vec<Stored>) -> Inputs {
        Inputs {
            own,
            pk_known: true,
            browser,
            lists,
            refresh: false,
            no_resume: false,
            max_age: 24 * HOUR,
            max_pages: None,
            same_day: false,
            now: NOW,
            requests_left: 2_001,
            accounts_left: 2_000,
            held: None,
        }
    }

    /// Your own list, never walked: one poll, a navigation and two requests
    /// a page from the browser; one a page and the poll without it.
    #[test]
    fn a_list_to_walk_is_its_pages_and_what_goes_with_them() {
        let list = || vec![stored(ListKind::Followers, Some(120), None)];
        let from_the_browser = compute(&inputs(true, true, list()));
        assert_eq!(from_the_browser.to_find, 0);
        assert_eq!(from_the_browser.lists[0].fate, Fate::Walked);
        assert_eq!(from_the_browser.lists[0].pages, 10);
        assert_eq!(from_the_browser.requests, (22, 22), "1 + 1 + 2 x 10");
        assert_eq!(from_the_browser.accounts, 120);

        let direct = compute(&inputs(true, false, list()));
        assert_eq!(direct.requests, (11, 11), "the poll and ten pages");
    }

    /// A fresh list is one request if nothing moved and a walk if it did; a
    /// stale one, or `--refresh`, is a walk for certain.
    #[test]
    fn a_fresh_list_is_a_range_and_a_stale_one_is_a_walk() {
        let fresh = compute(&inputs(
            true,
            true,
            vec![stored(ListKind::Followers, Some(24), Some(HOUR))],
        ));
        assert_eq!(fresh.lists[0].fate, Fate::ReusedUnlessMoved);
        assert_eq!(fresh.requests, (1, 1 + 1 + 4));
        assert_eq!(fresh.seconds.0, 0, "reused, it takes no time");

        let stale = compute(&inputs(
            true,
            true,
            vec![stored(ListKind::Followers, Some(24), Some(48 * HOUR))],
        ));
        assert_eq!(stale.lists[0].fate, Fate::Walked);
        assert_eq!(stale.requests, (6, 6));

        let mut refreshed = inputs(
            true,
            true,
            vec![stored(ListKind::Followers, Some(24), Some(HOUR))],
        );
        refreshed.refresh = true;
        assert_eq!(compute(&refreshed).lists[0].fate, Fate::Walked);
    }

    /// Somebody else's account costs its finding, which carries the first
    /// list's counter; the second list is polled again only if the first was
    /// walked. An account never seen here is a list of unknown size.
    #[test]
    fn another_account_costs_its_finding_and_a_crossing_its_second_poll() {
        let both = vec![
            stored(ListKind::Followers, Some(12), None),
            stored(ListKind::Following, Some(12), None),
        ];
        let crossing = compute(&inputs(false, true, both));
        assert_eq!(crossing.to_find, 4);
        assert_eq!(crossing.lists[0].requests, (3, 3), "no poll, 1 + 2 x 1");
        assert_eq!(crossing.lists[1].requests, (4, 4), "a poll, 1 + 2 x 1");
        assert_eq!(crossing.requests, (11, 11));

        let mut unknown = inputs(
            false,
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
        assert_eq!(unknown.accounts, 0);
    }

    /// A walk left to resume is what is left of it; `--no-resume` is the
    /// whole list; `--max-pages` caps the pages.
    #[test]
    fn what_a_walk_already_has_and_a_page_cap_are_taken_off() {
        let mut list = stored(ListKind::Following, Some(240), None);
        list.resumable = Some(120);
        let resumed = compute(&inputs(true, false, vec![list.clone()]));
        assert_eq!(
            (resumed.lists[0].to_read, resumed.lists[0].pages),
            (120, 10)
        );

        let mut again = inputs(true, false, vec![list.clone()]);
        again.no_resume = true;
        assert_eq!(compute(&again).lists[0].pages, 20);

        let mut capped = inputs(true, false, vec![list]);
        capped.max_pages = Some(3);
        let capped = compute(&capped);
        assert_eq!(capped.lists[0].pages, 3);
        assert_eq!(capped.accounts, 36);
    }

    /// More than the day's accounts pauses, unless `--same-day`.
    #[test]
    fn more_than_the_days_accounts_pauses_unless_finished_the_same_day() {
        let big = || vec![stored(ListKind::Followers, Some(5_000), None)];
        let paused = compute(&inputs(true, true, big()));
        assert!(paused.pauses);
        let mut same_day = inputs(true, true, big());
        same_day.same_day = true;
        assert!(!compute(&same_day).pauses);
    }

    /// Another process walking the list: a run would refuse, for nothing.
    #[test]
    fn a_list_walked_elsewhere_is_refused() {
        let mut list = stored(ListKind::Followers, Some(500), None);
        list.elsewhere = true;
        let refused = compute(&inputs(true, true, vec![list]));
        assert_eq!(refused.lists[0].fate, Fate::Refused);
        assert_eq!(refused.requests, (0, 0));
    }

    /// A sitting rests between sittings and not after the last.
    #[test]
    fn a_walk_rests_between_sittings() {
        let (one_low, one_high) = walk_seconds(40, false);
        assert_eq!(one_low, one_high, "one sitting has no rest");
        let (two_low, two_high) = walk_seconds(41, false);
        assert_eq!(
            two_high - two_low,
            600,
            "one rest of five to fifteen minutes"
        );
        assert!(
            walk_seconds(40, true).0 > one_low,
            "the browser's pages pace sooner"
        );
    }
}
