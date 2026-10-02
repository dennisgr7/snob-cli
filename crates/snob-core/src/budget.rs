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
