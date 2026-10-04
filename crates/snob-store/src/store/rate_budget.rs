//! Rate control that persists across runs.
//!
//! An in-memory limiter is no good here: the budget has to outlive the process,
//! because running the command twice in a row must not spend twice without
//! anyone noticing. No Rust crate does this over local storage, so it is
//! hand-written.
//!
//! The algorithm is GCRA, an exact token bucket expressed as a single integer:
//! instead of storing how many tokens are left and when they were refilled, it
//! stores the theoretical instant from which the next request is legitimate.
//! Integer arithmetic, no floating-point drift, and no row to update per tick.
//!
//! The daily ceiling on accounts read off lists is not a bucket: it promises
//! at most so many in any 24 hours, so it is a log of what each page carried,
//! summed over the last day (`account_reads`).

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use snob_core::EpochMs;
use snob_core::Pk;
use snob_core::budget::{Brake, RateBudget, RateBudgetError, common_brake};
use snob_core::clock::now_ms;

use super::StoreError;
use super::shared::Shared;
use crate::paths::AccountPaths;

/// A storage failure said in the budget's terms.
///
/// One line per fallible call, because no `From` impl can do it for a `?`:
/// `RateBudgetError` lives in `snob_core::budget` and `rusqlite::Error` in
/// rusqlite, so an impl here would be between two foreign types. What is lost
/// is brevity; what is gained is that the crate boundary is visible at every
/// place a storage failure crosses it.
fn budget_err<E: std::fmt::Display>(e: E) -> RateBudgetError {
    RateBudgetError(e.to_string())
}

/// Bucket names, as stored in `rate_budget.bucket`.
///
/// Constants rather than inline literals: each is read in several places, and
/// missing one of them silently restarts the budget.
const PACE_BUCKET: &str = "pace";
const DAILY_BUCKET: &str = "daily";
const WRITE_BUCKET: &str = "writes";
/// A day, the window the accounts ceiling is stated over.
const DAY_MS: i64 = 86_400_000;
/// The only value of `cooldowns.scope`. A cooldown covers the whole session.
const SESSION_SCOPE: &str = "session";

/// Sustained pace: one request every 3.83 seconds. The number is a reference
/// cadence the bucket keeps, not the cadence snob reads at: it is what
/// InstagramUnfollowers, the project that gave snob the idea, averages per
/// page. That pauses 1,250 ms before a request and 1,150 ms after a page, and
/// then a long 10,000 ms pause every seventh page, so its mean per page is
/// 1250 + 1150 + 10000/7 = 3,829 ms; the long pause belongs in the average.
///
/// snob's own walker steps one to three seconds between pages and rests five
/// to fifteen minutes every forty (`snob_ig::pace::Pace::default`), faster
/// than this within a sitting and far slower across one. The tolerance below
/// absorbs the sitting: from rest, request `k` goes without a wait while it
/// is sent no sooner than `(k - 20)` emissions in, so at a steady two-second
/// step about forty-two go through before the bucket starts pacing, and at a
/// steady one second, the safe direction, about twenty-eight. A sitting's
/// rest refills it. So within one walker this bucket paces only a run of
/// unusually short steps; what it bounds is the combined rate of several
/// processes sharing the budget, the case the project explicitly supports,
/// where it is the only thing holding that rate down. A change to
/// `Pace::default`'s step or sitting means revisiting this constant: the two
/// are one decision written in two files, and
/// `a_sitting_at_two_seconds_a_step_fits_the_pace_bucket` checks them
/// together.
///
/// Revisited for the requests the app sends around a list (2026-10-01):
/// when each page is followed by its `show_many`, which goes only once the
/// page has shown the token it needs, a page is two requests a step apart,
/// and the bucket starts pacing a sitting after about fourteen pages instead
/// of forty. That is slower, the safe direction, and is left so: the
/// constant is not lowered to let the extra request through. The
/// navigation before a list and the reads that open a profile are a few
/// requests an action, which the tolerance absorbs.
pub const PACE_EMISSION_MS: i64 = 3_830;
/// Burst tolerance of the pace bucket: twenty requests.
///
/// Twenty emissions, deliberately: it is what lets a sitting at the walker's
/// mean step through without the bucket pacing it. A walker steps faster
/// than the emission within a sitting, so the tolerance decides how many of
/// its pages go out before the bucket starts pacing them: about forty-two at
/// two seconds a step, against the forty a sitting holds, and about
/// twenty-eight at one second. A change to it moves those numbers, and
/// `a_sitting_at_two_seconds_a_step_fits_the_pace_bucket` with them; a
/// narrower tolerance would start pacing an ordinary sitting, which is a
/// design change rather than a tuning.
pub const PACE_BURST_MS: i64 = PACE_EMISSION_MS * 20;

/// Daily ceiling of roughly two thousand requests.
pub const DAILY_EMISSION_MS: i64 = 43_200;
pub const DAILY_BURST_MS: i64 = 86_400_000;

/// Sustained pace of **writes**: one follow or unfollow every fifteen minutes,
/// which is ninety-six a day if somebody keeps it up around the clock.
///
/// This is a third bucket rather than a smaller emission on the existing two,
/// because a write is a request *and* something else. It pays the pace bucket
/// and the daily bucket like every other request — it costs Instagram the same
/// — and then it pays this one on top, which is the constraint that has nothing
/// to do with volume.
///
/// Where the number comes from, since `pace.rs`'s numbers come from the web
/// app's recorded cadence and the owner's decision, and this one has
/// neither. The public field reports for 2026 put an established account at
/// 100 to 150 follow actions a day and a new account at 10 to 30, and they
/// agree on something more useful than either figure:
/// **what a service reacts to is the burst, not the daily total.** A hundred
/// unfollows inside half an hour is refused on an account whose day's count
/// would have passed without comment. So the design target is not a daily
/// ceiling at all — it is a floor under the gap between two writes, and ninety-
/// six a day is what falls out of it rather than what was aimed at.
///
/// Fifteen minutes is also below the rate a person clicking the button would
/// produce, which is the point: the ceiling that matters is not the one snob
/// enforces on itself but the one the account has already used up elsewhere.
/// This bucket knows nothing about the follows made in the app on the same
/// account today, so it has to leave room for them.
const WRITE_EMISSION_MS: i64 = 900_000;

/// Burst tolerance of the write bucket: **three** writes back to back, and then
/// the fifteen minutes apply.
///
/// Two emissions, not three. A GCRA tolerance of *n* intervals lets *n + 1*
/// through before it throttles — the *n* that fit ahead plus the one emitted on
/// pace, which is the arithmetic `FIT_IN_A_ROW` in the tests below spells out
/// for the pace bucket. Written as three it would allow four, and the sentence
/// above would be wrong about the constant underneath it.
///
/// Deliberately tight, and the reason is the shape of the thing rather than the
/// size of it. Twenty was right for reads, where a burst is a walk going
/// through pages of one list, which is the cost of one question. Three writes in
/// a row is about as many decisions as a person makes at a sitting; past that it
/// is no longer somebody tidying their following list, which is the only use
/// this budget is sized for.
const WRITE_BURST_MS: i64 = WRITE_EMISSION_MS * 2;

/// Slack before deciding the system clock has gone backwards.
const CLOCK_SKEW_TOLERANCE_MS: i64 = 5_000;

const MAX_COOLDOWN_MS: i64 = 24 * 3600 * 1000;

/// Escape hatch environment variable. Deliberately absent from the help: it
/// exists so a bug of ours cannot lock anyone out, not for skipping the limit
/// out of convenience.
const IGNORE_COOLDOWN_ENV: &str = "SNOB_IGNORE_COOLDOWN";

/// Whether the escape hatch is open.
fn ignoring_cooldowns() -> bool {
    std::env::var(IGNORE_COOLDOWN_ENV).is_ok_and(|value| is_affirmative(&value))
}

/// Whether a variable's value means yes.
///
/// The switch takes an answer rather than merely existing. This is the one
/// thing that turns off the protection the whole project is built around, and
/// `SNOB_IGNORE_COOLDOWN=0` meaning "yes, ignore it" is the kind of surprise
/// that only shows up later as an account in trouble.
///
/// Split from the read above so a test can drive it without setting a variable
/// the rest of the suite is reading at the same time.
fn is_affirmative(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// The core of the algorithm, isolated so it can be tested without a database.
///
/// `emission` is what a request costs in time and `burst` how far ahead one may
/// run. Returns the new theoretical instant and the wait.
///
/// Two of the four are moments and two are lengths of time, which is exactly
/// the pair a bare `i64` could not tell apart: typed, `decide(now, tat, burst,
/// emission)` does not compile.
fn decide(tat: EpochMs, now: EpochMs, emission: i64, burst: i64) -> (EpochMs, i64) {
    let tat = tat.max(now);
    let wait = ((tat - Duration::from_millis(burst as u64)) - now).max(0);
    (tat + Duration::from_millis(emission as u64), wait)
}

pub struct SqliteRateBudget {
    /// Behind a `Mutex` because the trait exposes `&self` and opening a
    /// transaction needs `&mut Connection`. There is never real contention:
    /// reservations within a process are sequential, and coordination between
    /// processes is SQLite's job.
    conn: std::sync::Mutex<Connection>,
    /// `shared.db`, and the account this budget is, so that its push-backs
    /// reach the others and theirs reach it. `None` in tests, and
    /// when `shared.db` cannot be opened: the account's own cooldown must still
    /// be read and recorded, by an older build too.
    shared: Option<(Pk, std::sync::Mutex<Shared>)>,
}

impl SqliteRateBudget {
    /// Opens its **own** connection to the same file, with the store's settings.
    ///
    /// Not sharing the store's connection is deliberate: this way the
    /// two-process case is the same as the two-connection case, so what runs is
    /// what gets tested, and the budget's borrow cannot clash with the
    /// transaction that inserts pages.
    ///
    /// But separate must not mean differently configured. `trusted_schema` and
    /// the defensive flag are about the file rather than about a handle, so one
    /// undefended connection leaves the file undefended and cancels what the
    /// store set. The same goes for `secure_delete` and the WAL size bound —
    /// and this is the connection that enforces the bound, because it commits
    /// an `IMMEDIATE` transaction before every single request and is therefore
    /// the one that checkpoints.
    pub fn open(paths: &AccountPaths) -> Result<Self, StoreError> {
        // The store must have been opened first: it is what creates the schema.
        let conn = Connection::open(paths.db_file())?;
        super::configure(&conn)?;

        // The one setting this connection does not want from `configure`.
        // Under WAL, `synchronous = NORMAL` skips the fsync at commit, so a
        // power cut can lose the last transactions. Here those are the cooldown
        // writes, and losing one brings the account out of a block early — the
        // single direction `start_cooldown` must never be wrong in. One fsync
        // against a pace of one request every 3.83 seconds costs nothing.
        conn.pragma_update(None, "synchronous", "FULL")?;
        let shared = match Shared::open(paths) {
            Ok(shared) => Some((paths.pk(), std::sync::Mutex::new(shared))),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "shared.db could not be opened; this account's push-backs will not brake the others"
                );
                None
            }
        };
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
            shared,
        })
    }

    /// A budget over one connection and no other account's push-backs.
    #[cfg(test)]
    fn over(conn: Connection) -> Self {
        Self {
            conn: std::sync::Mutex::new(conn),
            shared: None,
        }
    }

    /// The brake at `now`, from every account's push-backs.
    fn brake_at(&self, now: EpochMs) -> Result<Option<Brake>, RateBudgetError> {
        let Some((_, shared)) = &self.shared else {
            return Ok(None);
        };
        let pushbacks = shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pushbacks(now)
            .map_err(budget_err)?;
        Ok(common_brake(&pushbacks, now))
    }

    /// Tolerates poisoning: if a thread panicked holding the lock, the worst
    /// case here is a half-done reservation the transaction already rolled back.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reserve_bucket(
        tx: &rusqlite::Transaction<'_>,
        bucket: &str,
        emission: i64,
        burst: i64,
        now: EpochMs,
    ) -> Result<i64, RateBudgetError> {
        // The two columns are `INTEGER` and become moments here, at the row
        // boundary, the way an account id does through `pk_from_sql`.
        let row: Option<(EpochMs, EpochMs)> = tx
            .query_row(
                "SELECT tat_ms, updated_at_ms FROM rate_budget WHERE bucket = ?1",
                params![bucket],
                |row| Ok((EpochMs::new(row.get(0)?), EpochMs::new(row.get(1)?))),
            )
            .optional()
            .map_err(budget_err)?;

        let stored_tat = match row {
            // If the clock went backwards the stored instant means nothing any
            // more: the wait would come out as hours. It is reset. That is not
            // a free pass, because it still grants no burst beyond tolerance.
            Some((_, updated))
                if now + Duration::from_millis(CLOCK_SKEW_TOLERANCE_MS as u64) < updated =>
            {
                tracing::warn!(bucket, "the system clock went backwards; budget reset");
                now
            }
            Some((tat, _)) => tat,
            None => now,
        };

        let (new_tat, wait) = decide(stored_tat, now, emission, burst);

        tx.execute(
            "INSERT INTO rate_budget (bucket, tat_ms, emission_ms, burst_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(bucket) DO UPDATE SET
                 tat_ms = excluded.tat_ms,
                 emission_ms = excluded.emission_ms,
                 burst_ms = excluded.burst_ms,
                 updated_at_ms = excluded.updated_at_ms",
            params![bucket, new_tat.get(), emission, burst, now.get()],
        )
        .map_err(budget_err)?;

        Ok(wait)
    }

    /// The body of both reservations, so that the two cannot come apart.
    ///
    /// Every reservation charges the pace bucket and the daily one; a write
    /// charges the write bucket as well. The answer is the longest of the waits
    /// they hand back, and **all of the buckets are charged whichever wait
    /// wins** — a request held back by one budget still spends the others,
    /// because it is still going to be sent.
    ///
    /// One transaction for all of them, and it is `IMMEDIATE` for the reason
    /// spelled out below. Charging the write bucket in a second transaction
    /// would let two processes interleave between the two, which is exactly the
    /// case a shared budget exists for.
    fn reserve_buckets(&self, write: bool) -> Result<Duration, RateBudgetError> {
        let now = now_ms();

        // BEGIN IMMEDIATE rather than the default deferred one: with deferred,
        // two processes read, both try to write, and the second gets
        // SQLITE_BUSY when upgrading the transaction, at which point
        // busy_timeout can no longer help and it fails outright. Taking the
        // write lock up front makes the processes serialize.
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(budget_err)?;

        let pace_wait =
            Self::reserve_bucket(&tx, PACE_BUCKET, PACE_EMISSION_MS, PACE_BURST_MS, now)?;
        let daily_wait =
            Self::reserve_bucket(&tx, DAILY_BUCKET, DAILY_EMISSION_MS, DAILY_BURST_MS, now)?;
        let write_wait = if write {
            Self::reserve_bucket(&tx, WRITE_BUCKET, WRITE_EMISSION_MS, WRITE_BURST_MS, now)?
        } else {
            0
        };

        tx.commit().map_err(budget_err)?;

        Ok(Duration::from_millis(
            pace_wait.max(daily_wait).max(write_wait).max(0) as u64,
        ))
    }

    /// The wait `bucket` would hand back at `now`, charged nothing: what
    /// [`Self::reserve_bucket`] decides, without writing it down.
    fn bucket_wait(
        conn: &Connection,
        bucket: &str,
        emission: i64,
        burst: i64,
        now: EpochMs,
    ) -> Result<i64, RateBudgetError> {
        Ok(decide(Self::stored_tat(conn, bucket, now)?, now, emission, burst).1)
    }

    /// The theoretical instant `bucket` holds at `now`, as the next charge
    /// would read it: `now` for a bucket never charged, and for one written
    /// by a clock that has since gone backwards.
    fn stored_tat(
        conn: &Connection,
        bucket: &str,
        now: EpochMs,
    ) -> Result<EpochMs, RateBudgetError> {
        let row: Option<(EpochMs, EpochMs)> = conn
            .query_row(
                "SELECT tat_ms, updated_at_ms FROM rate_budget WHERE bucket = ?1",
                params![bucket],
                |row| Ok((EpochMs::new(row.get(0)?), EpochMs::new(row.get(1)?))),
            )
            .optional()
            .map_err(budget_err)?;
        let stored_tat = match row {
            // A clock that went backwards resets the bucket when it is next
            // charged, as `reserve_bucket` says.
            Some((_, updated))
                if now + Duration::from_millis(CLOCK_SKEW_TOLERANCE_MS as u64) < updated =>
            {
                now
            }
            Some((tat, _)) => tat,
            None => now,
        };
        Ok(stored_tat)
    }

    /// What `bucket` would let through at `now` without a wait, and when the
    /// next one goes without one. Charges nothing.
    fn bucket_state(
        conn: &Connection,
        bucket: &str,
        emission: i64,
        burst: i64,
        now: EpochMs,
    ) -> Result<BucketState, RateBudgetError> {
        let tat = Self::stored_tat(conn, bucket, now)?.max(now);
        // Request `k` from here waits nothing while `tat + k * emission` is
        // no further ahead of `now` than the tolerance: `decide`, unrolled.
        let room = now.get() + burst - tat.get();
        let left = if room < 0 { 0 } else { room / emission + 1 };
        let wait = decide(tat, now, emission, burst).1;
        Ok(BucketState {
            left: u32::try_from(left).unwrap_or(u32::MAX),
            most: u32::try_from(burst / emission + 1).unwrap_or(u32::MAX),
            free_at: now + Duration::from_millis(wait.max(0) as u64),
        })
    }

    /// The daily ceiling on accounts in force now, from when the last
    /// push-back was. See [`snob_core::budget::accounts_per_day`].
    fn accounts_ceiling(conn: &Connection, now: EpochMs) -> Result<u64, RateBudgetError> {
        let last_push_back = Self::last_cooldown(conn)?.map(|c| c.set_at);
        Ok(u64::from(snob_core::budget::accounts_per_day(
            last_push_back,
            now,
        )))
    }

    /// The account's last cooldown as it was written down, ended or not.
    fn last_cooldown(conn: &Connection) -> Result<Option<CooldownRecord>, RateBudgetError> {
        conn.query_row(
            "SELECT until_ms, set_at_ms, reason, strikes FROM cooldowns WHERE scope = ?1",
            params![SESSION_SCOPE],
            |row| {
                Ok(CooldownRecord {
                    until: EpochMs::new(row.get(0)?),
                    set_at: EpochMs::new(row.get(1)?),
                    reason: row.get(2)?,
                    strikes: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(budget_err)
    }

    /// What was read in the last day, oldest first, as `(at_ms, accounts)`.
    ///
    /// A row stamped in the future is a clock that went backwards; it is not
    /// counted, and [`RateBudget::spend_accounts`] removes it, as the request
    /// buckets reset on the same event.
    fn accounts_read_today(
        conn: &Connection,
        now: EpochMs,
    ) -> Result<Vec<(i64, u64)>, RateBudgetError> {
        let mut statement = conn
            .prepare_cached(
                "SELECT at_ms, accounts FROM account_reads
                 WHERE at_ms > ?1 AND at_ms <= ?2 ORDER BY at_ms",
            )
            .map_err(budget_err)?;
        statement
            .query_map(
                params![now.get() - DAY_MS, now.get() + CLOCK_SKEW_TOLERANCE_MS],
                |row| Ok((row.get(0)?, u64::from(row.get::<_, u32>(1)?))),
            )
            .map_err(budget_err)?
            .collect::<Result<_, _>>()
            .map_err(budget_err)
    }
}

/// A cooldown as the table holds it: the last one written, whether or not it
/// has ended. It is never deleted, because when it was set decides the day's
/// ceiling for a week and whether the next push-back escalates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownRecord {
    pub until: EpochMs,
    pub set_at: EpochMs,
    /// What Instagram said, as `start_cooldown` was told it.
    pub reason: String,
    /// How many push-backs in a row, each within a day of the last.
    pub strikes: u32,
}

impl CooldownRecord {
    /// When it ends, if it still stands at `now`. Checked against `set_at`
    /// too: if the clock went backwards the cooldown still stands even though
    /// `until` looks past.
    pub fn standing_at(&self, now: EpochMs) -> Option<EpochMs> {
        (now < self.until || now < self.set_at).then_some(self.until)
    }

    /// Until when a push-back would escalate this one instead of starting
    /// over.
    pub fn escalates_until(&self) -> EpochMs {
        self.set_at + Duration::from_millis(MAX_COOLDOWN_MS as u64)
    }
}

/// One bucket, read and not charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketState {
    /// How many requests it lets through from now without a wait.
    pub left: u32,
    /// How many it lets through from rest.
    pub most: u32,
    /// When the next one goes without a wait: now, while `left` is not zero.
    pub free_at: EpochMs,
}

/// The whole budget of one account at one moment, read and not charged:
/// what `snob status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetState {
    /// The pace bucket, which every request pays.
    pub pace: BucketState,
    /// The daily bucket, which every request pays.
    pub daily: BucketState,
    /// The write bucket, which a follow or an unfollow pays on top.
    pub writes: BucketState,
    /// When a write would go without a wait: the latest of the three buckets
    /// it pays.
    pub next_write_at: EpochMs,
    /// Accounts read off lists in the last 24 hours.
    pub accounts_read: u64,
    /// The ceiling on them in force now.
    pub accounts_ceiling: u64,
    /// The last cooldown written down, ended or not.
    pub last_cooldown: Option<CooldownRecord>,
    /// The brake on every account, if one stands.
    pub brake: Option<Brake>,
}

impl BudgetState {
    /// When this account may send again, if it may not now: its own cooldown
    /// or the brake, whichever ends later, as [`RateBudget::cooldown`] says.
    pub fn held_until(&self, now: EpochMs) -> Option<EpochMs> {
        let own = self.last_cooldown.as_ref().and_then(|c| c.standing_at(now));
        own.max(self.brake.as_ref().map(|b| b.until))
    }

    /// How many more accounts the day holds.
    pub fn accounts_left(&self) -> u64 {
        self.accounts_ceiling.saturating_sub(self.accounts_read)
    }

    /// How many requests go out from now without a wait: every one pays both
    /// the pace and the day, so the smaller of the two.
    pub fn requests_now(&self) -> u32 {
        self.pace.left.min(self.daily.left)
    }

    /// When the next request goes without a wait: the later of the two
    /// buckets it pays, each of which says now while it has one left.
    pub fn next_request_at(&self) -> EpochMs {
        self.pace.free_at.max(self.daily.free_at)
    }
}

/// The budget of the account whose database `conn` is, at `now`, with the
/// brake read from `shared` when there is one. **Charges nothing and writes
/// nothing**: every value is what the next charge would read.
///
/// The escape hatch is not consulted: this reports what is written down,
/// and a cooldown being ignored does not make it any less there.
pub fn state(
    conn: &Connection,
    shared: Option<&Shared>,
    now: EpochMs,
) -> Result<BudgetState, RateBudgetError> {
    let bucket =
        |name, emission, burst| SqliteRateBudget::bucket_state(conn, name, emission, burst, now);
    let pace = bucket(PACE_BUCKET, PACE_EMISSION_MS, PACE_BURST_MS)?;
    let daily = bucket(DAILY_BUCKET, DAILY_EMISSION_MS, DAILY_BURST_MS)?;
    let writes = bucket(WRITE_BUCKET, WRITE_EMISSION_MS, WRITE_BURST_MS)?;
    let brake = match shared {
        Some(shared) => common_brake(&shared.pushbacks(now).map_err(budget_err)?, now),
        None => None,
    };
    Ok(BudgetState {
        pace,
        daily,
        writes,
        next_write_at: pace.free_at.max(daily.free_at).max(writes.free_at),
        accounts_read: SqliteRateBudget::accounts_read_today(conn, now)?
            .iter()
            .map(|(_, n)| n)
            .sum(),
        accounts_ceiling: SqliteRateBudget::accounts_ceiling(conn, now)?,
        last_cooldown: SqliteRateBudget::last_cooldown(conn)?,
        brake,
    })
}

impl RateBudget for SqliteRateBudget {
    fn reserve(&self) -> Result<Duration, RateBudgetError> {
        self.reserve_buckets(false)
    }

    fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
        self.reserve_buckets(true)
    }

    /// The longest of the three waits a write would be handed now: the
    /// buckets `reserve_buckets` charges for one, read and not charged.
    fn write_wait(&self) -> Result<Duration, RateBudgetError> {
        let now = now_ms();
        let conn = self.conn();
        let mut wait = 0;
        for (bucket, emission, burst) in [
            (PACE_BUCKET, PACE_EMISSION_MS, PACE_BURST_MS),
            (DAILY_BUCKET, DAILY_EMISSION_MS, DAILY_BURST_MS),
            (WRITE_BUCKET, WRITE_EMISSION_MS, WRITE_BURST_MS),
        ] {
            wait = wait.max(Self::bucket_wait(&conn, bucket, emission, burst, now)?);
        }
        Ok(Duration::from_millis(wait.max(0) as u64))
    }

    /// How long until the last day holds room for `accounts` more: the moment
    /// enough of what was read falls out of the window. Asks without spending.
    ///
    /// Asking for more than a whole day holds is asking for an empty day,
    /// which does come; the ceiling is never waited on past that.
    fn accounts_wait(&self, accounts: u32) -> Result<Duration, RateBudgetError> {
        let now = now_ms();
        let conn = self.conn();
        let ceiling = Self::accounts_ceiling(&conn, now)?;
        let wanted = u64::from(accounts).min(ceiling);
        let read = Self::accounts_read_today(&conn, now)?;
        let mut used: u64 = read.iter().map(|(_, n)| n).sum();
        for (at, n) in &read {
            if used + wanted <= ceiling {
                break;
            }
            used -= n;
            if used + wanted <= ceiling {
                return Ok(Duration::from_millis(
                    (at + DAY_MS - now.get()).max(0) as u64
                ));
            }
        }
        Ok(Duration::ZERO)
    }

    /// Charged after the page arrived, for what it actually carried — asking
    /// for twelve and being served eight is eight accounts.
    fn spend_accounts(&self, accounts: u32) -> Result<(), RateBudgetError> {
        if accounts == 0 {
            return Ok(());
        }
        let now = now_ms();
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(budget_err)?;
        tx.execute(
            "DELETE FROM account_reads WHERE at_ms <= ?1",
            params![now.get() - DAY_MS],
        )
        .map_err(budget_err)?;
        let ahead = tx
            .execute(
                "DELETE FROM account_reads WHERE at_ms > ?1",
                params![now.get() + CLOCK_SKEW_TOLERANCE_MS],
            )
            .map_err(budget_err)?;
        if ahead > 0 {
            tracing::warn!("the system clock went backwards; accounts budget reset");
        }
        tx.execute(
            "INSERT INTO account_reads (at_ms, accounts) VALUES (?1, ?2)",
            params![now.get(), accounts],
        )
        .map_err(budget_err)?;
        tx.commit().map_err(budget_err)
    }

    fn accounts_left(&self) -> Result<u32, RateBudgetError> {
        let now = now_ms();
        let conn = self.conn();
        let ceiling = Self::accounts_ceiling(&conn, now)?;
        let used: u64 = Self::accounts_read_today(&conn, now)?
            .iter()
            .map(|(_, n)| n)
            .sum();
        Ok(ceiling.saturating_sub(used) as u32)
    }

    fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
        if ignoring_cooldowns() {
            tracing::warn!("{IGNORE_COOLDOWN_ENV} is set: the cooldown is being ignored");
            return Ok(None);
        }

        let now = now_ms();
        let own = Self::last_cooldown(&self.conn())?.and_then(|c| c.standing_at(now));
        // Whichever ends later: the account's own, or the brake on all of them.
        let brake = self.brake_at(now)?.map(|brake| brake.until);
        Ok(own.max(brake))
    }

    fn brake(&self) -> Result<Option<Brake>, RateBudgetError> {
        if ignoring_cooldowns() {
            return Ok(None);
        }
        self.brake_at(now_ms())
    }

    /// Reads the previous cooldown and writes the next one in **one**
    /// transaction, and never lets the result end sooner than what was already
    /// there.
    ///
    /// Both halves matter for the same reason `reserve` takes the write lock up
    /// front: other snob processes share this file — the monitor, a command in
    /// another terminal, the owner recording a push-back. Two processes reading
    /// `strikes = 1` at the same instant would both write `strikes = 2`, losing
    /// one escalation. And an unconditional write would let a two-hour throttle
    /// recorded ten minutes into a twelve-hour action block replace it,
    /// bringing the account out of the more serious block early, which is the
    /// one direction this table must never be wrong in.
    fn start_cooldown(&self, reason: &str, minimum: Duration) -> Result<EpochMs, RateBudgetError> {
        let now = now_ms();
        let base = minimum.as_millis() as i64;

        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(budget_err)?;

        let previous: Option<(EpochMs, i64, EpochMs)> = tx
            .query_row(
                "SELECT set_at_ms, strikes, until_ms FROM cooldowns WHERE scope = ?1",
                params![SESSION_SCOPE],
                |row| {
                    Ok((
                        EpochMs::new(row.get(0)?),
                        row.get(1)?,
                        EpochMs::new(row.get(2)?),
                    ))
                },
            )
            .optional()
            .map_err(budget_err)?;

        // Reoffending within the next day doubles the penalty.
        let (length, strikes) = match previous {
            Some((set_at, strikes, _)) if now - set_at < MAX_COOLDOWN_MS => {
                let next = strikes + 1;
                let escalated = base.saturating_mul(1 << (next - 1).min(5));
                (escalated.min(MAX_COOLDOWN_MS), next)
            }
            _ => (base.min(MAX_COOLDOWN_MS), 1),
        };

        let standing = previous.map_or(EpochMs::new(0), |(_, _, until)| until);
        let until = (now + Duration::from_millis(length as u64)).max(standing);

        tx.execute(
            "INSERT INTO cooldowns (scope, until_ms, set_at_ms, reason, strikes)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope) DO UPDATE SET
                 until_ms = excluded.until_ms,
                 set_at_ms = excluded.set_at_ms,
                 reason = excluded.reason,
                 strikes = excluded.strikes",
            params![SESSION_SCOPE, until.get(), now.get(), reason, strikes],
        )
        .map_err(budget_err)?;
        tx.commit().map_err(budget_err)?;

        // Best effort: the account's own cooldown is recorded above, and a
        // failure here costs the others their brake, not this one its pause.
        if let Some((pk, shared)) = &self.shared
            && let Err(e) = shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(*pk, now, until, reason)
        {
            tracing::warn!(error = %e, "the push-back could not be shared with the other accounts");
        }

        tracing::warn!(reason, minutes = length / 60_000, "account in cooldown");
        Ok(until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The numbers that decide how fast this program talks to Instagram.**
    ///
    /// Written out rather than derived, because a test written in terms of the
    /// constant it is checking passes whatever that constant becomes. Every
    /// database test here is relative: it counts reservations against a burst
    /// expressed in emissions, and because `PACE_BURST_MS` is
    /// `PACE_EMISSION_MS * 20` the whole suite is scale-invariant — ten requests
    /// a second, or a write every seventy seconds, would leave every other test
    /// green. Nothing else in the workspace looks at these values.
    ///
    /// Where 3,830 comes from, and how it relates to `Pace::default`, is on
    /// `PACE_EMISSION_MS`. This pins the number and cannot see `pace.rs`, so a
    /// change there must update both, and the simulation below.
    #[test]
    fn the_budget_numbers_are_the_documented_ones() {
        assert_eq!(PACE_EMISSION_MS, 3_830, "one request every 3.83 s");
        assert_eq!(PACE_BURST_MS, 76_600, "twenty requests of tolerance");
        assert_eq!(DAILY_EMISSION_MS, 43_200, "2000 requests a day");
        assert_eq!(DAILY_BURST_MS, 86_400_000, "a day of them");
        assert_eq!(
            WRITE_EMISSION_MS, 900_000,
            "one write every fifteen minutes"
        );
        assert_eq!(
            WRITE_BURST_MS, 1_800_000,
            "two emissions of tolerance, which lets three writes through and not four"
        );
    }

    /// How many requests sent `step_ms` apart from rest go through the pace
    /// bucket before the first one waits.
    fn sent_before_a_wait(step_ms: i64, requests: usize) -> usize {
        let start = EpochMs::new(1_000_000);
        let mut tat = start;
        for k in 0..requests {
            let now = start + Duration::from_millis(k as u64 * step_ms as u64);
            let (next, wait) = decide(tat, now, PACE_EMISSION_MS, PACE_BURST_MS);
            if wait > 0 {
                return k;
            }
            tat = next;
        }
        requests
    }

    /// The pace bucket and the walker's sitting, checked together: a sitting
    /// at the walker's mean step goes through without the bucket pacing it,
    /// and a run of the shortest steps is paced, after twenty-eight. Both
    /// edges are pinned from both sides: the simulation is exact, so a looser
    /// bound would let these numbers and the constants' docs drift apart.
    ///
    /// The steps are written out rather than read from `pace.rs`, which this
    /// crate cannot see: two seconds is the middle of `STEP_MS` and one its
    /// shortest, and a change to either belongs here too.
    #[test]
    fn a_sitting_at_two_seconds_a_step_fits_the_pace_bucket() {
        assert_eq!(
            sent_before_a_wait(2_000, 43),
            42,
            "forty-two requests two seconds apart do not wait, and the forty-third does"
        );
        assert_eq!(
            sent_before_a_wait(1_000, 40),
            28,
            "at one second apart the bucket starts pacing after twenty-eight"
        );
    }

    const T: i64 = 2_400;
    const TAU: i64 = 48_000; // twenty requests

    /// A tolerance of twenty intervals lets twenty-one requests through before
    /// throttling: the twenty that fit ahead plus the one emitted on pace.
    const FIT_IN_A_ROW: usize = (TAU / T) as usize + 1;

    #[test]
    fn the_first_request_from_cold_does_not_wait() {
        let (tat, wait) = decide(EpochMs::new(0), EpochMs::new(1_000_000), T, TAU);
        assert_eq!(wait, 0);
        assert_eq!(tat, EpochMs::new(1_000_000 + T));
    }

    #[test]
    fn nothing_waits_within_the_burst() {
        let now = EpochMs::new(1_000_000);
        let mut tat = now;
        for i in 0..FIT_IN_A_ROW {
            let (next, wait) = decide(tat, now, T, TAU);
            assert_eq!(wait, 0, "request {i} should not wait");
            tat = next;
        }
    }

    #[test]
    fn once_the_burst_is_spent_waiting_begins() {
        let now = EpochMs::new(1_000_000);
        let mut tat = now;
        for _ in 0..FIT_IN_A_ROW {
            tat = decide(tat, now, T, TAU).0;
        }
        let (_, wait) = decide(tat, now, T, TAU);
        assert_eq!(wait, T, "past the burst you pay the full interval");
    }

    #[test]
    fn the_budget_refills_over_time() {
        let now = EpochMs::new(1_000_000);
        let mut tat = now;
        for _ in 0..25 {
            tat = decide(tat, now, T, TAU).0;
        }
        // An hour later it is fully refilled.
        let (_, wait) = decide(tat, now + Duration::from_millis(3_600_000), T, TAU);
        assert_eq!(wait, 0);
    }

    #[test]
    fn an_instant_in_the_past_does_not_grant_unlimited_budget() {
        // The old tat is clamped to "now": idleness does not accrue credit.
        let (tat, wait) = decide(EpochMs::new(1), EpochMs::new(1_000_000), T, TAU);
        assert_eq!(wait, 0);
        assert_eq!(tat, EpochMs::new(1_000_000 + T));
    }

    /// Built through the real constructor, not through `over`. The claim above
    /// `open` is that what runs is what gets tested, and a helper that skipped
    /// the configuration would have made that false for every test in here.
    fn temp_budget() -> (tempfile::TempDir, SqliteRateBudget) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::rooted_at(tmp.path()).account(snob_core::Pk::new(42));
        // The store creates the schema; the budget hooks in afterwards.
        let _db = super::super::Store::open(&paths).unwrap();
        (tmp, SqliteRateBudget::open(&paths).unwrap())
    }

    /// Three accounts' budgets under one data directory.
    fn three_accounts() -> (tempfile::TempDir, [SqliteRateBudget; 3]) {
        let tmp = tempfile::tempdir().unwrap();
        let app = crate::paths::AppPaths::rooted_at(tmp.path());
        let budgets = [1, 2, 3].map(|pk| {
            let paths = app.account(Pk::new(pk));
            let _db = super::super::Store::open(&paths).unwrap();
            SqliteRateBudget::open(&paths).unwrap()
        });
        (tmp, budgets)
    }

    #[test]
    fn a_push_back_on_one_account_leaves_the_others_free() {
        let (_tmp, [a, b, c]) = three_accounts();
        a.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        a.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        assert_eq!(b.cooldown().unwrap(), None);
        assert_eq!(c.brake().unwrap(), None);
    }

    /// Two accounts pushed back on within the hour: the one with the shorter
    /// cooldown waits for the longer, and so does the account nobody pushed
    /// back on.
    #[test]
    fn two_accounts_pushed_back_on_stop_every_account_until_the_longer_end() {
        let (_tmp, [a, b, c]) = three_accounts();
        let short = a.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        let long = b
            .start_cooldown("feedback_required", Duration::from_secs(12 * 3600))
            .unwrap();
        assert!(long > short);

        for budget in [&a, &b, &c] {
            assert_eq!(budget.cooldown().unwrap(), Some(long));
        }
        assert_eq!(
            c.brake().unwrap(),
            Some(Brake {
                until: long,
                accounts: vec![Pk::new(1), Pk::new(2)],
            })
        );
    }

    /// A `shared.db` from a newer snob costs the brake, never the account's
    /// own cooldown: an owner of an older build still records the push-back
    /// it heard.
    #[test]
    fn a_newer_shared_db_still_records_the_accounts_own_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let app = crate::paths::AppPaths::rooted_at(tmp.path());
        drop(Shared::open(&app).unwrap());
        Connection::open(app.shared_db_file())
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();

        let paths = app.account(Pk::new(1));
        let _db = super::super::Store::open(&paths).unwrap();
        let budget = SqliteRateBudget::open(&paths).unwrap();
        let until = budget
            .start_cooldown("429", Duration::from_secs(3600))
            .unwrap();
        assert_eq!(budget.cooldown().unwrap(), Some(until));
    }

    #[test]
    fn the_first_reservations_do_not_throttle() {
        let (_tmp, b) = temp_budget();
        for _ in 0..10 {
            assert_eq!(b.reserve().unwrap(), Duration::ZERO);
        }
    }

    /// Far enough past the burst that the disk cannot decide the answer.
    ///
    /// The bucket refills in real time, so every millisecond these reservations
    /// take is a millisecond of throttling they undo. Twenty-one of them leave a
    /// margin of one emission for twenty-one committed transactions, and this
    /// connection runs with `synchronous = FULL`, so each one waits for the
    /// platform to flush: a slow Windows runner can spend that margin and fail
    /// on a correct budget.
    ///
    /// Thirty is the same assertion with ten emissions of slack: past the
    /// burst is past the burst, and a machine slow enough to break this one is
    /// slow enough that it was never going to outrun the pace anyway.
    #[test]
    fn the_budget_runs_out_and_starts_throttling() {
        let (_tmp, b) = temp_budget();
        for _ in 0..30 {
            b.reserve().unwrap();
        }
        assert!(
            b.reserve().unwrap() > Duration::ZERO,
            "past the burst it should throttle"
        );
    }

    /// Three writes fit, and the fourth waits. The tolerance is what decides
    /// that, and it is deliberately much tighter than the one reads get.
    #[test]
    fn three_writes_fit_in_a_row_and_the_fourth_waits() {
        let (_tmp, b) = temp_budget();
        for i in 0..3 {
            assert_eq!(
                b.reserve_write().unwrap(),
                Duration::ZERO,
                "write {i} should not have waited"
            );
        }
        assert!(
            b.reserve_write().unwrap() > Duration::from_secs(60),
            "the fourth write should be held back by minutes, not milliseconds"
        );
    }

    /// The wait a write would be handed is asked without spending: from
    /// rest it is none, and three writes still go in a row after asking;
    /// then it is the fourth's, and the fourth is handed no more.
    #[test]
    fn a_writes_wait_is_asked_without_spending() {
        let (_tmp, b) = temp_budget();
        for _ in 0..5 {
            assert_eq!(b.write_wait().unwrap(), Duration::ZERO);
        }
        for i in 0..3 {
            assert_eq!(
                b.reserve_write().unwrap(),
                Duration::ZERO,
                "write {i} should not have waited"
            );
        }
        let asked = b.write_wait().unwrap();
        assert!(asked > Duration::from_secs(60), "{asked:?}");
        assert!(b.write_wait().unwrap() <= asked, "asking spent nothing");
        let handed = b.reserve_write().unwrap();
        assert!(
            handed <= asked && handed + Duration::from_secs(5) >= asked,
            "{handed:?} against {asked:?}"
        );
    }

    /// A write pays the read buckets too, so its wait counts them: a pace
    /// burst spent by reads is a wait for the next write as well.
    #[test]
    fn a_writes_wait_counts_the_read_buckets() {
        let (_tmp, b) = temp_budget();
        while b.reserve().unwrap().is_zero() {}
        assert!(b.write_wait().unwrap() > Duration::ZERO);
    }

    /// **The buckets do not leak into each other.** A write spends the read
    /// budgets too — it is a request — but an exhausted write bucket must not
    /// stop a walk, which is the failure this would have if the write cost were
    /// expressed as a smaller emission on the pace bucket instead of as a
    /// bucket of its own.
    #[test]
    fn a_spent_write_budget_does_not_hold_up_a_read() {
        let (_tmp, b) = temp_budget();
        for _ in 0..4 {
            b.reserve_write().unwrap();
        }
        assert!(
            b.reserve_write().unwrap() > Duration::ZERO,
            "the write bucket should be spent by now"
        );
        assert_eq!(
            b.reserve().unwrap(),
            Duration::ZERO,
            "a read must not pay for the writes"
        );
    }

    /// A write is a request, so the pace bucket sees it. Without this the write
    /// path would be a way of reaching Instagram that the request count does not
    /// know about, and `Pacer::spent` would stop meaning what it says.
    #[test]
    fn a_write_spends_the_read_budget_as_well() {
        let (_tmp, b) = temp_budget();
        // Three writes are all the write bucket allows in a row, so the rest of
        // the pace bucket has to be spent by reads for the assertion to be
        // about the writes having spent theirs.
        for _ in 0..3 {
            b.reserve_write().unwrap();
        }
        for _ in 0..27 {
            b.reserve().unwrap();
        }
        assert!(
            b.reserve().unwrap() > Duration::ZERO,
            "thirty requests, three of which were writes, should have spent the pace burst"
        );
    }

    /// The connection that writes most often gets everything the store sets on
    /// itself. A second connection to one file, configured differently, is the
    /// same as not configuring the file.
    #[test]
    fn the_budget_connection_is_protected_like_the_store() {
        let (_tmp, budget) = temp_budget();
        let conn = budget.conn();
        let pragma = |name: &str| -> i64 {
            conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
                .unwrap()
        };

        assert_eq!(
            pragma("trusted_schema"),
            0,
            "the schema is executable content"
        );
        assert_eq!(pragma("secure_delete"), 1);
        assert_eq!(pragma("journal_size_limit"), 4 * 1024 * 1024);

        // FULL, not the store's NORMAL: losing the last commit here would bring
        // an account out of a cooldown early.
        assert_eq!(pragma("synchronous"), 2, "cooldown writes must be fsynced");
    }

    #[test]
    fn with_no_cooldown_there_is_no_cooldown() {
        let (_tmp, b) = temp_budget();
        assert_eq!(b.cooldown().unwrap(), None);
    }

    #[test]
    fn a_cooldown_blocks_and_reoffending_lengthens_it() {
        let (_tmp, b) = temp_budget();

        let first = b.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        let active = b.cooldown().unwrap().unwrap();
        assert_eq!(active, first);

        let second = b.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        assert!(
            second - now_ms() > first - now_ms(),
            "reoffending should lengthen the cooldown"
        );
    }

    /// The one direction this table must never be wrong in: a twelve-hour
    /// action block, followed ten minutes later by a two-hour throttle, must not
    /// end at the two-hour mark.
    #[test]
    fn a_shorter_cause_never_cuts_a_standing_cooldown_short() {
        let (_tmp, b) = temp_budget();

        let long = b
            .start_cooldown("feedback_required", Duration::from_secs(12 * 3600))
            .unwrap();
        let after_short = b
            .start_cooldown("rate_limit", Duration::from_secs(2 * 3600))
            .unwrap();

        assert!(
            after_short >= long,
            "the cooldown was cut from {long} to {after_short}"
        );
        assert_eq!(b.cooldown().unwrap().unwrap(), after_short);
    }

    /// The switch that turns off the protection the whole project is built
    /// around takes a yes, not merely a value. Setting it to `0` and getting
    /// "cooldowns ignored" is a surprise that only surfaces later, as an
    /// account in trouble.
    #[test]
    fn the_escape_hatch_needs_an_affirmative_value() {
        for yes in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_affirmative(yes), "{yes:?}");
        }
        for no in ["0", "false", "no", "off", "", "  ", "maybe"] {
            assert!(!is_affirmative(no), "{no:?}");
        }
    }

    #[test]
    fn the_cooldown_has_a_ceiling() {
        let (_tmp, b) = temp_budget();
        for _ in 0..10 {
            b.start_cooldown("429", Duration::from_secs(12 * 3600))
                .unwrap();
        }
        let until = b.cooldown().unwrap().unwrap();
        assert!(until - now_ms() <= MAX_COOLDOWN_MS);
    }

    /// Writes a read `ago` in the past, which is the only way to put a day
    /// into a test that takes milliseconds.
    fn read_at(b: &SqliteRateBudget, ago: Duration, accounts: u32) {
        b.conn()
            .execute(
                "INSERT INTO account_reads (at_ms, accounts) VALUES (?1, ?2)",
                params![now_ms().get() - ago.as_millis() as i64, accounts],
            )
            .unwrap();
    }

    /// A day's accounts go through without waiting, and the next ones wait
    /// until the oldest reads fall out of the day: never more than the
    /// ceiling in any 24 hours, which a bucket with a day of burst allowed
    /// twice over.
    #[test]
    fn the_account_budget_holds_a_day_and_then_waits_for_it_to_pass() {
        let (_tmp, b) = temp_budget();
        assert_eq!(b.accounts_left().unwrap(), 2_000);
        assert_eq!(b.accounts_wait(25).unwrap(), Duration::ZERO);

        for _ in 0..79 {
            b.spend_accounts(25).unwrap();
        }
        assert_eq!(b.accounts_left().unwrap(), 25);
        assert_eq!(b.accounts_wait(25).unwrap(), Duration::ZERO);
        b.spend_accounts(25).unwrap();

        assert_eq!(b.accounts_left().unwrap(), 0);
        let wait = b.accounts_wait(25).unwrap();
        // All of it was read just now, so the room comes back a day from now.
        assert!(
            wait > Duration::from_secs(86_390) && wait <= Duration::from_secs(86_400),
            "{wait:?}"
        );
    }

    /// Room comes back as the oldest reads age out, and only as much as they
    /// carried.
    #[test]
    fn room_comes_back_as_the_oldest_reads_leave_the_day() {
        let (_tmp, b) = temp_budget();
        read_at(&b, Duration::from_secs(23 * 3600), 1_000);
        read_at(&b, Duration::from_secs(3600), 1_000);
        read_at(&b, Duration::from_secs(25 * 3600), 1_000);

        assert_eq!(
            b.accounts_left().unwrap(),
            0,
            "a read older than a day is gone"
        );
        let wait = b.accounts_wait(500).unwrap();
        assert!(
            wait > Duration::from_secs(3590) && wait <= Duration::from_secs(3600),
            "the read of 23 hours ago leaves in an hour: {wait:?}"
        );
        let wait = b.accounts_wait(1_500).unwrap();
        assert!(
            wait > Duration::from_secs(23 * 3600 - 10),
            "fifteen hundred needs the read of an hour ago gone too: {wait:?}"
        );
        assert!(
            b.accounts_wait(5_000).unwrap() <= Duration::from_secs(23 * 3600),
            "more than a day holds waits for an empty day, not forever"
        );
    }

    /// Asking does not spend: the ration is only moved by what was read.
    #[test]
    fn asking_about_accounts_spends_none() {
        let (_tmp, b) = temp_budget();
        for _ in 0..10 {
            b.accounts_wait(2_000).unwrap();
        }
        assert_eq!(b.accounts_left().unwrap(), 2_000);
    }

    /// A push-back halves the day's ceiling, and what was already read counts
    /// against the lower one as it is.
    #[test]
    fn a_push_back_halves_the_ceiling() {
        let (_tmp, b) = temp_budget();
        b.spend_accounts(500).unwrap();
        assert_eq!(b.accounts_left().unwrap(), 1_500);

        b.start_cooldown("feedback_required", Duration::from_secs(60))
            .unwrap();
        assert_eq!(b.accounts_left().unwrap(), 500);
    }

    /// A read stamped ahead of the clock is a clock that went backwards: not
    /// counted, and removed at the next write.
    #[test]
    fn a_read_from_the_future_does_not_hold_the_day_shut() {
        let (_tmp, b) = temp_budget();
        b.conn()
            .execute(
                "INSERT INTO account_reads (at_ms, accounts) VALUES (?1, 2000)",
                params![now_ms().get() + 3_600_000],
            )
            .unwrap();
        assert_eq!(b.accounts_left().unwrap(), 2_000);
        b.spend_accounts(25).unwrap();
        let rows: i64 = b
            .conn()
            .query_row("SELECT count(*) FROM account_reads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// Everything the state reads, as rows, to check that reading it wrote
    /// nothing.
    fn rows(b: &SqliteRateBudget) -> Vec<String> {
        let conn = b.conn();
        let mut all = Vec::new();
        for table in ["rate_budget", "cooldowns", "account_reads"] {
            let mut statement = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                .unwrap();
            let width = statement.column_count();
            let mut rows = statement.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                let cells: Vec<String> = (0..width)
                    .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                    .collect();
                all.push(format!("{table}: {}", cells.join(", ")));
            }
        }
        all
    }

    /// From rest the buckets hold what their constants say: twenty-one, two
    /// thousand and one, three. Each read is one less of the first two, a
    /// write one less of all three, and none of this is a charge.
    #[test]
    fn the_state_counts_what_is_left_and_spends_nothing() {
        let (_tmp, b) = temp_budget();
        let now = now_ms();
        let fresh = state(&b.conn(), None, now).unwrap();
        assert_eq!((fresh.pace.left, fresh.pace.most), (21, 21));
        assert_eq!((fresh.daily.left, fresh.daily.most), (2_001, 2_001));
        assert_eq!((fresh.writes.left, fresh.writes.most), (3, 3));
        assert_eq!(fresh.pace.free_at, now);
        assert_eq!(fresh.next_write_at, now);
        assert_eq!((fresh.accounts_read, fresh.accounts_ceiling), (0, 2_000));
        assert_eq!(fresh.last_cooldown, None);
        assert_eq!(fresh.held_until(now), None);

        for _ in 0..5 {
            b.reserve().unwrap();
        }
        b.reserve_write().unwrap();
        b.spend_accounts(36).unwrap();
        let before = rows(&b);
        let now = now_ms();
        let spent = state(&b.conn(), None, now).unwrap();
        assert_eq!(rows(&b), before, "reading the state wrote something");

        // The bucket refills in real time, so a slow machine may have earned
        // one back in between: the count is at most what was left, and no
        // more than one emission's worth above it.
        assert!((15..=16).contains(&spent.pace.left), "{:?}", spent.pace);
        assert!(
            (1_995..=1_996).contains(&spent.daily.left),
            "{:?}",
            spent.daily
        );
        assert_eq!(spent.writes.left, 2);
        assert_eq!((spent.accounts_read, spent.accounts_left()), (36, 1_964));
    }

    /// Past the tolerance nothing is left, and the next one is free one wait
    /// from now: the wait `reserve` would hand back.
    #[test]
    fn a_spent_bucket_says_when_it_frees_up() {
        let (_tmp, b) = temp_budget();
        for _ in 0..3 {
            b.reserve_write().unwrap();
        }
        let now = now_ms();
        let spent = state(&b.conn(), None, now).unwrap();
        assert_eq!(spent.writes.left, 0);
        let wait = spent.writes.free_at - now;
        assert!(wait > 800_000 && wait <= 900_000, "{wait}");
        assert_eq!(spent.next_write_at, spent.writes.free_at);
        assert!(spent.pace.left > 0, "reads are not held up by writes");
    }

    /// A bucket written by a clock that has since gone backwards reads as
    /// rest, as the next charge would treat it.
    #[test]
    fn a_bucket_from_the_future_reads_as_rest() {
        let (_tmp, b) = temp_budget();
        let ahead = now_ms().get() + 3_600_000;
        b.conn()
            .execute(
                "INSERT INTO rate_budget (bucket, tat_ms, emission_ms, burst_ms, updated_at_ms)
                 VALUES ('pace', ?1, 3830, 76600, ?1)",
                params![ahead + 10_000_000],
            )
            .unwrap();
        assert_eq!(state(&b.conn(), None, now_ms()).unwrap().pace.left, 21);
    }

    /// The reason and the strikes are read back, and a cooldown that has
    /// ended is still reported as the last one, without holding anything.
    #[test]
    fn the_last_cooldown_is_read_back_with_its_reason_and_strikes() {
        let (_tmp, b) = temp_budget();
        b.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        let until = b
            .start_cooldown("feedback_required", Duration::from_secs(3600))
            .unwrap();
        let now = now_ms();
        let held = state(&b.conn(), None, now).unwrap();
        let last = held.last_cooldown.clone().unwrap();
        assert_eq!(
            (last.reason.as_str(), last.strikes),
            ("feedback_required", 2)
        );
        assert_eq!(held.held_until(now), Some(until));
        assert_eq!(held.accounts_ceiling, 1_000, "a push-back halves the day");

        let later = until + Duration::from_secs(1);
        let ended = state(&b.conn(), None, later).unwrap();
        assert_eq!(ended.held_until(later), None);
        assert_eq!(ended.last_cooldown, Some(last));
    }

    /// The brake on every account is read from `shared.db`, and holds an
    /// account that was never pushed back on.
    #[test]
    fn the_state_reports_the_brake_on_every_account() {
        let (tmp, [a, b, c]) = three_accounts();
        a.start_cooldown("429", Duration::from_secs(3600)).unwrap();
        let long = b.start_cooldown("429", Duration::from_secs(7200)).unwrap();
        let shared = Shared::read_existing(&crate::paths::AppPaths::rooted_at(tmp.path()))
            .unwrap()
            .unwrap();
        let now = now_ms();
        let held = state(&c.conn(), Some(&shared), now).unwrap();
        assert_eq!(held.last_cooldown, None);
        assert_eq!(held.held_until(now), Some(long));
        assert_eq!(held.brake.unwrap().accounts, vec![Pk::new(1), Pk::new(2)]);
    }

    /// Proves the budget really is shared between connections, which is what
    /// keeps two snob processes from spending at once.
    #[test]
    fn two_connections_share_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shared.db");
        let _db = super::super::Store::open_at(&path).unwrap();

        let one = SqliteRateBudget::over(Connection::open(&path).unwrap());
        let two = SqliteRateBudget::over(Connection::open(&path).unwrap());

        for _ in 0..15 {
            one.reserve().unwrap();
        }
        for _ in 0..15 {
            two.reserve().unwrap();
        }

        // Thirty reservations comfortably exceed the burst of twenty, so the
        // next one has to throttle even on the first connection.
        assert!(
            one.reserve().unwrap() > Duration::ZERO,
            "both connections should share one budget"
        );
    }
}
