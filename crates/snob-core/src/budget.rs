//! The request budget, as an interface rather than as a database.
//!
//! **This is a port, kept apart from the persistence module that implements
//! it.** `snob-ig` needs the trait — `Pacer` cannot be built without one — and
//! must not depend on the crate that opens SQLite to get it.
//!
//! What is here is everything a caller has to know: how to ask for a slot, how
//! to ask whether the account is in cooldown, and how long each kind of
//! cooldown lasts. How any of that is stored is `snob_store::store::rate_budget`.
//!
//! The cooldown lengths are here rather than there because they are policy and
//! not storage: `snob-ig` decides *which* of the three a refusal earns, from a
//! status code and a body, and it does that without knowing there is a database
//! at all.

use std::time::Duration;

use crate::{EpochMs, Pk};

/// Cooldown after Instagram throttles the account.
pub const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(2 * 3600);

/// Cooldown after Instagram blocks an action on the account.
pub const ACTION_BLOCK_COOLDOWN: Duration = Duration::from_secs(12 * 3600);

/// Cooldown after Instagram asks for the account to be verified.
///
/// Deliberately much shorter than the other two, because it is the only one
/// waiting does not fix: a challenge is cleared by the user opening the link,
/// and the account is usable again the moment they do. Twelve hours would
/// strand someone who cleared it in thirty seconds, and would do nothing extra
/// about the case this exists for — a scheduled run knocking again on an
/// account Instagram has just asked to verify itself. Half an hour stops the
/// second without stranding the first, and repeats still escalate.
pub const CHALLENGE_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// How long to wait before firing a request.
pub trait RateBudget: Send + Sync {
    /// Reserves a request and returns how long to wait before sending it. Zero
    /// means go ahead.
    ///
    /// The reservation is committed even if the request is never made:
    /// overcharging is the safe direction to be wrong in.
    fn reserve(&self) -> Result<Duration, RateBudgetError>;

    /// Reserves a **write** — a follow or an unfollow — and returns how long to
    /// wait before sending it.
    ///
    /// A write pays everything a read pays and then the write bucket on top, so
    /// this can never come back with a shorter wait than [`Self::reserve`]
    /// would have. That ordering is the whole point of it being a separate
    /// method: a caller cannot reach the cheaper one by mistake, because
    /// `IgClient::post` calls this one and `IgClient::get` calls the other, and
    /// neither takes an argument that could pick the wrong one.
    fn reserve_write(&self) -> Result<Duration, RateBudgetError>;

    /// How long [`Self::reserve_write`] would wait if it were asked now.
    /// Zero means now. Asks without spending.
    ///
    /// A write from the browser is built on the profile the tab loads just
    /// before it, and a quarter of an hour's wait between the two outlasts
    /// the browser: its wait comes before the profile is loaded, and the
    /// reservation after it finds nothing more to wait for.
    ///
    /// The default is none, which is what every test double wants.
    fn write_wait(&self) -> Result<Duration, RateBudgetError> {
        Ok(Duration::ZERO)
    }

    /// How long [`Self::reserve`] would wait if it were asked now. Zero means
    /// now. Asks without spending, so a caller with a limit on how long it
    /// may wait can refuse before the reservation is committed rather than
    /// after (`Pacer::waiting_at_most`).
    ///
    /// The default is none, which is what every test double wants.
    fn read_wait(&self) -> Result<Duration, RateBudgetError> {
        Ok(Duration::ZERO)
    }

    /// Until when the account is in cooldown.
    fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError>;

    /// Puts the account in cooldown and returns until when.
    fn start_cooldown(&self, reason: &str, minimum: Duration) -> Result<EpochMs, RateBudgetError>;

    /// How long until `accounts` more may be read off a list without the last
    /// 24 hours holding more than [`accounts_per_day`]. Zero means now. Asks
    /// without spending.
    ///
    /// **The second budget, and the one the account is judged on.** The
    /// buckets above count requests; this counts what came back in them.
    /// Meta's paper on the system that issues the automated-activity warning
    /// (arXiv 2502.17693) weighs each request by how many accounts it returns,
    /// and a list page is nothing but accounts. See [`accounts_per_day`].
    ///
    /// The default grants everything, which is what every test double wants;
    /// the real budget, `SqliteRateBudget`, overrides every default here.
    fn accounts_wait(&self, accounts: u32) -> Result<Duration, RateBudgetError> {
        let _ = accounts;
        Ok(Duration::ZERO)
    }

    /// Records that `accounts` were read off a list.
    fn spend_accounts(&self, accounts: u32) -> Result<(), RateBudgetError> {
        let _ = accounts;
        Ok(())
    }

    /// How many accounts may still be read today without waiting.
    fn accounts_left(&self) -> Result<u32, RateBudgetError> {
        Ok(u32::MAX)
    }

    /// The brake on every account, when one stands. See [`common_brake`].
    ///
    /// [`Self::cooldown`] already counts it; this says why, so the refusal can
    /// name the accounts. The default is none, which is what a budget that
    /// knows only one account can say.
    fn brake(&self) -> Result<Option<Brake>, RateBudgetError> {
        Ok(None)
    }

    /// Whether this budget keeps the sitting ([`Sitting`]), so that it
    /// outlives the process: [`Self::sitting_page`], [`Self::rest_owed`]
    /// and [`Self::rest_paid`] then answer for it.
    ///
    /// The default is no, which is what every test double wants: the pacer
    /// then counts the sitting in the process, with the same arithmetic
    /// ([`Sitting::after_page`]). The real budget, `SqliteRateBudget`, keeps
    /// it in the account's database, so a run stopped during its rest, or a
    /// page cap reached on the page that filled a sitting, leaves the rest
    /// owed to whatever walks that account next.
    fn keeps_sitting(&self) -> bool {
        false
    }

    /// Records a page of a list walked at `at`, and answers the sitting it
    /// leaves: one transaction, so two processes walking the same account
    /// count each other's pages. See [`Sitting::after_page`].
    ///
    /// The default keeps nothing and answers as if this were the first page
    /// ever walked; nothing reads it unless [`Self::keeps_sitting`] says yes.
    fn sitting_page(&self, at: EpochMs, rule: SittingRule) -> Result<Sitting, RateBudgetError> {
        Ok(Sitting::after_page(None, at, rule))
    }

    /// Until when the rest the sitting owes runs, if it is still owed at
    /// `now`. Asks without writing.
    fn rest_owed(&self, now: EpochMs) -> Result<Option<EpochMs>, RateBudgetError> {
        let _ = now;
        Ok(None)
    }

    /// Records that the rest running until `until` was paid: the sitting
    /// starts again. A rest that is not that one any more (another process
    /// paid it, and a new sitting filled since) is left as it is.
    fn rest_paid(&self, until: EpochMs) -> Result<(), RateBudgetError> {
        let _ = until;
        Ok(())
    }
}

/// The pages of lists walked on one account since its last rest, and the
/// rest they owe once they fill a sitting.
///
/// **A rest is owed, not taken on the spot.** The page that fills a sitting
/// draws its rest once and writes down when it ends (`rest_until`), whether
/// or not the walk that read it goes on; and before any page of any walk of
/// the account, the walker pays what is left of it. Paid only by a walk
/// that went on, as the rest once was, it was skipped by a walk that ended
/// on its page cap, by a run stopped during it, and by every caller that
/// reads a list a page at a time, which is how an agent reads one.
///
/// The arithmetic is here, over plain numbers, so that the database that
/// keeps it (`snob-store`) and the pacer that counts it in a process for a
/// budget that keeps nothing (`snob-ig`) are the same rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sitting {
    /// Pages walked in it.
    pub pages: u32,
    /// When the last of them was walked.
    pub last_page_at: EpochMs,
    /// When the rest it owes ends, once it is full; `None` before.
    pub rest_until: Option<EpochMs>,
}

/// What makes a sitting, from the pace (`snob_ig::pace::Pace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SittingRule {
    /// How many pages fill it.
    pub pages: u32,
    /// The rest the page that fills it owes, drawn by the caller: kept only
    /// when this page is the one that fills it.
    pub rest: Duration,
    /// How long with no page a sitting that is not full ends on its own: the
    /// longest rest. Left alone that long, the reading has rested by any
    /// account of it; a shorter gap, even the shortest rest, would let a run
    /// stopped and started again just past it walk a second sitting with no
    /// rest drawn for the first.
    pub idle: Duration,
}

impl Sitting {
    /// The sitting after a page walked at `at`, from the one `before` it
    /// (`None`: no page was ever walked here).
    ///
    /// - A rest that has ended by `at` ends its sitting: this page is the
    ///   first of the next.
    /// - A rest still owed stays owed, and the page is counted in the full
    ///   sitting: walked by somebody who did not wait for it, it is no
    ///   reason to forget the rest.
    /// - A sitting that is not full and was left alone for longer than
    ///   [`SittingRule::idle`] ends too.
    /// - The page that brings the count to [`SittingRule::pages`] owes
    ///   [`SittingRule::rest`] from `at`.
    ///
    /// A clock set back is not idleness: a page "before" the last one counts
    /// on in the same sitting, the direction that rests sooner.
    pub fn after_page(before: Option<Sitting>, at: EpochMs, rule: SittingRule) -> Sitting {
        let idle = i64::try_from(rule.idle.as_millis()).unwrap_or(i64::MAX);
        let (pages, owed) = match before {
            None => (0, None),
            Some(s) => match s.rest_until {
                Some(until) if at >= until => (0, None),
                Some(until) => (s.pages, Some(until)),
                None if at - s.last_page_at > idle => (0, None),
                None => (s.pages, None),
            },
        };
        let pages = pages.saturating_add(1);
        let rest_until = owed.or_else(|| (pages >= rule.pages.max(1)).then(|| at + rule.rest));
        Sitting {
            pages,
            last_page_at: at,
            rest_until,
        }
    }

    /// When the rest ends, if it is still owed at `now`.
    pub fn rest_owed(&self, now: EpochMs) -> Option<EpochMs> {
        self.rest_until.filter(|until| *until > now)
    }

    /// The sitting once the rest running until `until` is paid: a new one,
    /// with no page in it yet. Any other rest stays owed.
    #[must_use]
    pub fn rested(self, until: EpochMs) -> Sitting {
        if self.rest_until == Some(until) {
            Sitting {
                pages: 0,
                rest_until: None,
                ..self
            }
        } else {
            self
        }
    }

    /// How many pages into a sitting the next page walked at `now` would
    /// come after: none once a rest has ended or the sitting was left alone
    /// past `idle`, as [`Self::after_page`] counts. For an estimate.
    pub fn pages_at(&self, now: EpochMs, idle: Duration) -> u32 {
        let rule = SittingRule {
            pages: u32::MAX,
            rest: Duration::ZERO,
            idle,
        };
        Self::after_page(Some(*self), now, rule).pages - 1
    }
}

/// How close together push-backs on two different accounts have to be for
/// every account to stop.
///
/// Two accounts refused within the hour, from one machine, is Instagram
/// answering the machine rather than either account, and the next account to
/// ask would be the third.
pub const COMMON_BRAKE_WINDOW: Duration = Duration::from_secs(3600);

/// One push-back, as every account's budget records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushBack {
    pub pk: Pk,
    pub at: EpochMs,
    /// When the cooldown it started on that account ends.
    pub until: EpochMs,
}

/// Every account paused until `until`, because of push-backs on `accounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brake {
    pub until: EpochMs,
    /// In the order they were first pushed back on.
    pub accounts: Vec<Pk>,
}

/// The brake the push-backs impose at `now`, if any.
///
/// Two push-backs on **different** accounts less than [`COMMON_BRAKE_WINDOW`]
/// apart stop every account until the later of their two cooldowns. Several
/// on one account are that account's own cooldown, which it escalates by
/// itself.
pub fn common_brake(pushbacks: &[PushBack], now: EpochMs) -> Option<Brake> {
    let window = COMMON_BRAKE_WINDOW.as_millis() as i64;
    let mut sorted: Vec<&PushBack> = pushbacks.iter().collect();
    sorted.sort_by_key(|p| p.at);

    let mut brake: Option<Brake> = None;
    for (i, a) in sorted.iter().enumerate() {
        for b in &sorted[i + 1..] {
            let until = a.until.max(b.until);
            if a.pk == b.pk || b.at - a.at >= window || until <= now {
                continue;
            }
            let brake = brake.get_or_insert(Brake {
                until,
                accounts: Vec::new(),
            });
            brake.until = brake.until.max(until);
            for pk in [a.pk, b.pk] {
                if !brake.accounts.contains(&pk) {
                    brake.accounts.push(pk);
                }
            }
        }
    }
    brake
}

/// Accounts a day read off follower and following lists, in the ordinary case.
///
/// The closest thing to a measured number anybody has published: the careful
/// monitor in this category (misiektoja/instagram_monitor, 4.0, September
/// 2026) defaults to it, and its collaborators' testing puts the flags that
/// arrive within a day at list walks past it. InstagramUnfollowers users were
/// logged out for automated activity at about 1,600 in one sitting in the same
/// month. It is a ceiling, not a target.
const ACCOUNTS_PER_DAY: u32 = 2_000;

/// The same after Instagram has pushed back on this account recently.
const ACCOUNTS_PER_DAY_AFTER_PUSH_BACK: u32 = 1_000;

/// How long a push-back keeps the lower ceiling in force.
const PUSH_BACK_MEMORY: Duration = Duration::from_secs(7 * 24 * 3600);

/// The daily ceiling on accounts read, given when the last cooldown began.
///
/// Policy rather than storage, so it lives here next to the cooldown lengths;
/// the store only remembers when.
pub fn accounts_per_day(last_push_back: Option<EpochMs>, now: EpochMs) -> u32 {
    match last_push_back {
        Some(at) if now - at < PUSH_BACK_MEMORY.as_millis() as i64 => {
            ACCOUNTS_PER_DAY_AFTER_PUSH_BACK
        }
        _ => ACCOUNTS_PER_DAY,
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RateBudgetError(pub String);

// No `From<StoreError>` or `From<rusqlite::Error>`: both types live in
// `snob-store`, and the orphan rule refuses an impl where neither side is local.
// `budget_err` in `snob_store::store::rate_budget` converts at the crate
// boundary instead, one line per fallible call.

/// Grants everything and counts nothing. **Tests only**: using it against
/// Instagram skips rate control entirely.
#[doc(hidden)]
pub struct UnlimitedRateBudget;

impl RateBudget for UnlimitedRateBudget {
    fn reserve(&self) -> Result<Duration, RateBudgetError> {
        Ok(Duration::ZERO)
    }
    fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
        Ok(Duration::ZERO)
    }
    fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
        Ok(None)
    }
    fn start_cooldown(
        &self,
        _reason: &str,
        _minimum: Duration,
    ) -> Result<EpochMs, RateBudgetError> {
        Ok(EpochMs::new(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    fn pushed(pk: u64, at_min: i64, for_min: i64) -> PushBack {
        PushBack {
            pk: Pk::new(pk),
            at: EpochMs::new(at_min * MINUTE),
            until: EpochMs::new((at_min + for_min) * MINUTE),
        }
    }

    #[test]
    fn two_push_backs_on_one_account_are_no_brake() {
        let now = EpochMs::new(1_000 * MINUTE);
        let pushbacks = [pushed(1, 990, 120), pushed(1, 995, 240)];
        assert_eq!(common_brake(&pushbacks, now), None);
    }

    #[test]
    fn two_accounts_an_hour_and_a_minute_apart_are_no_brake() {
        let now = EpochMs::new(1_000 * MINUTE);
        let pushbacks = [pushed(1, 900, 240), pushed(2, 961, 240)];
        assert_eq!(common_brake(&pushbacks, now), None);
    }

    #[test]
    fn a_brake_that_has_ended_is_none() {
        let now = EpochMs::new(1_000 * MINUTE);
        let pushbacks = [pushed(1, 700, 120), pushed(2, 730, 180)];
        assert_eq!(common_brake(&pushbacks, now), None);
    }

    /// The account with the shorter cooldown waits for the longer one, and so
    /// does every account that was not pushed back on at all.
    #[test]
    fn two_accounts_within_the_hour_stop_until_the_longer_cooldown() {
        let now = EpochMs::new(1_000 * MINUTE);
        let pushbacks = [pushed(2, 990, 720), pushed(1, 960, 120)];
        assert_eq!(
            common_brake(&pushbacks, now),
            Some(Brake {
                until: EpochMs::new((990 + 720) * MINUTE),
                accounts: vec![Pk::new(1), Pk::new(2)],
            })
        );
    }

    #[test]
    fn the_window_is_the_documented_one() {
        assert_eq!(COMMON_BRAKE_WINDOW, Duration::from_secs(3600));
    }

    /// The daily ceiling on accounts, and the week it stays halved for.
    #[test]
    fn the_account_ceiling_halves_for_a_week_after_a_push_back() {
        let now = EpochMs::new(10 * 86_400_000);
        let day = 86_400_000_i64;
        assert_eq!(accounts_per_day(None, now), 2_000);
        assert_eq!(
            accounts_per_day(Some(EpochMs::new(now.get() - day)), now),
            1_000
        );
        assert_eq!(
            accounts_per_day(Some(EpochMs::new(now.get() - 7 * day + 1)), now),
            1_000
        );
        assert_eq!(
            accounts_per_day(Some(EpochMs::new(now.get() - 7 * day)), now),
            2_000
        );
    }

    const RULE: SittingRule = SittingRule {
        pages: 3,
        rest: Duration::from_secs(600),
        idle: Duration::from_secs(900),
    };

    /// Walks `pages` pages a second apart from `from`, and answers the
    /// sitting they leave.
    fn walked(before: Option<Sitting>, from: i64, pages: i64) -> Sitting {
        let mut sitting = before;
        for n in 0..pages {
            sitting = Some(Sitting::after_page(
                sitting,
                EpochMs::new(from + n * 1_000),
                RULE,
            ));
        }
        sitting.expect("at least one page")
    }

    /// The page that fills a sitting owes its rest from when it was walked,
    /// and the rest stays owed however many pages come before it ends.
    #[test]
    fn the_page_that_fills_a_sitting_owes_its_rest() {
        let two = walked(None, 0, 2);
        assert_eq!((two.pages, two.rest_until), (2, None));

        let full = walked(Some(two), 2_000, 1);
        assert_eq!(full.pages, 3);
        assert_eq!(full.rest_until, Some(EpochMs::new(2_000 + 600_000)));
        assert_eq!(
            full.rest_owed(EpochMs::new(3_000)),
            Some(EpochMs::new(602_000))
        );

        let unwaited = walked(Some(full), 3_000, 1);
        assert_eq!(unwaited.pages, 4, "a page walked anyway still counts");
        assert_eq!(
            unwaited.rest_until, full.rest_until,
            "and owes the same rest"
        );
    }

    /// Once the rest has ended, or once it is paid, the next page is the
    /// first of a new sitting.
    #[test]
    fn a_rest_ended_or_paid_starts_the_sitting_again() {
        let full = walked(None, 0, 3);
        let until = full.rest_until.expect("full");
        assert_eq!(full.rest_owed(until), None, "a rest has ended at its end");

        let after = Sitting::after_page(Some(full), until, RULE);
        assert_eq!((after.pages, after.rest_until), (1, None));

        let paid = full.rested(until);
        assert_eq!((paid.pages, paid.rest_until), (0, None));
        let after = Sitting::after_page(Some(paid), EpochMs::new(3_000), RULE);
        assert_eq!(after.pages, 1);

        let other = full.rested(EpochMs::new(until.get() + 1));
        assert_eq!(other, full, "another rest is not this one paid");
    }

    /// A sitting left alone for longer than the longest rest ends; one left
    /// alone for less, or a clock set back, counts on.
    #[test]
    fn a_sitting_left_alone_past_the_longest_rest_ends() {
        let two = walked(None, 0, 2);
        let soon = Sitting::after_page(Some(two), EpochMs::new(1_000 + 900_000), RULE);
        assert_eq!(
            soon.pages, 3,
            "fifteen minutes to the millisecond is not past"
        );

        let late = Sitting::after_page(Some(two), EpochMs::new(1_000 + 900_001), RULE);
        assert_eq!((late.pages, late.rest_until), (1, None));

        let back = Sitting::after_page(Some(two), EpochMs::new(-50_000), RULE);
        assert_eq!(back.pages, 3, "a clock set back rests sooner, not later");

        assert_eq!(two.pages_at(EpochMs::new(2_000), RULE.idle), 2);
        assert_eq!(two.pages_at(EpochMs::new(1_000 + 900_001), RULE.idle), 0);
    }

    /// A sitting of zero pages is a sitting of one, rather than a rest owed
    /// before anything was walked or never.
    #[test]
    fn a_sitting_of_no_pages_is_one_of_one() {
        let rule = SittingRule { pages: 0, ..RULE };
        let first = Sitting::after_page(None, EpochMs::new(0), rule);
        assert_eq!(first.rest_until, Some(EpochMs::new(600_000)));
    }

    /// The three cooldowns, written out rather than derived.
    ///
    /// `AGENTS.md` promises that an action block earns twelve hours rather than
    /// the throttle's two. Everything else refers to the constants by name, so
    /// without this both could become one second with the suite green.
    ///
    /// It sits beside the constants rather than in a crate that only reads
    /// them.
    #[test]
    fn the_cooldowns_are_the_documented_ones() {
        assert_eq!(RATE_LIMIT_COOLDOWN, Duration::from_secs(2 * 3600));
        assert_eq!(ACTION_BLOCK_COOLDOWN, Duration::from_secs(12 * 3600));
        assert_eq!(CHALLENGE_COOLDOWN, Duration::from_secs(30 * 60));
        assert!(
            ACTION_BLOCK_COOLDOWN > RATE_LIMIT_COOLDOWN,
            "an action block is not a throttle and must not be treated as the lighter one"
        );
    }
}
