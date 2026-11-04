//! How long Instagram's answers took to arrive, kept for a week.
//!
//! One row per answer of the API read (`snob_core::budget::AnswerTime`),
//! written as it arrives by the pacer, through `SqliteRateBudget`. The pace
//! reads the last few back to lengthen its waits when they were slow
//! ([`recent_waits`], `snob_core::budget::SlowRule`), and `snob status` shows
//! the last day ([`last_day`]). What policy there is lives in `snob-core` and
//! `snob-ig`; this module only stores and sums.

use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior, params};
use snob_core::EpochMs;
use snob_core::budget::{AnswerTime, Via};

use super::StoreError;
use super::rate_budget::CLOCK_SKEW_TOLERANCE_MS;

/// The window `snob status` shows.
pub const DAY_MS: i64 = 86_400_000;

/// How long a row is kept: long enough that the answers before a push-back
/// are still there when somebody looks into it days later, short enough
/// that the table stays a few thousand rows (the request budget lets about
/// two thousand through in a day).
const KEPT_MS: i64 = 7 * DAY_MS;

/// Records `answer`, pruning what is older than a week and what is stamped
/// after `now` (a clock that went backwards), in one transaction.
pub fn record(conn: &mut Connection, answer: &AnswerTime, now: EpochMs) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM answers WHERE at_ms <= ?1 OR at_ms > ?2",
        params![now.get() - KEPT_MS, now.get() + CLOCK_SKEW_TOLERANCE_MS],
    )?;
    tx.execute(
        "INSERT INTO answers (at_ms, operation, status, waited_ms, load, via)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            answer.at.get(),
            answer.operation,
            answer.status,
            millis(answer.waited),
            answer.load,
            answer.via.as_str()
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// How long the healthy answers (2xx) received after `since` took, newest
/// first, at most `n` of them.
pub fn recent_waits(
    conn: &Connection,
    since: EpochMs,
    n: usize,
) -> Result<Vec<Duration>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT waited_ms FROM answers
         WHERE at_ms > ?1 AND status BETWEEN 200 AND 299
         ORDER BY at_ms DESC LIMIT ?2",
    )?;
    let limit = i64::try_from(n).unwrap_or(i64::MAX);
    let rows = statement.query_map(params![since.get(), limit], |row| row.get::<_, i64>(0))?;
    rows.map(|ms| Ok(Duration::from_millis(u64::try_from(ms?).unwrap_or(0))))
        .collect()
}

/// The answers of one day, summed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnswersDay {
    /// Every answer read, refusals included.
    pub answers: u32,
    /// Those of them that were not a 2xx.
    pub refused: u32,
    /// How long the healthy ones took: the median, the 95th percentile and
    /// the longest. `None` without a healthy answer.
    pub median: Option<Duration>,
    pub p95: Option<Duration>,
    pub slowest: Option<Duration>,
    /// The last load Instagram declared, and when; `None` if it declared
    /// none all day.
    pub load: Option<(String, EpochMs)>,
    /// How the last answer left.
    pub via: Option<Via>,
}

/// The answers of the day before `now`. Reads only.
pub fn last_day(conn: &Connection, now: EpochMs) -> Result<AnswersDay, StoreError> {
    let since = now.get() - DAY_MS;
    let mut statement = conn
        .prepare_cached("SELECT status, waited_ms FROM answers WHERE at_ms > ?1 AND at_ms <= ?2")?;
    let rows = statement.query_map(params![since, now.get() + CLOCK_SKEW_TOLERANCE_MS], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut day = AnswersDay::default();
    let mut healthy = Vec::new();
    for row in rows {
        let (status, waited) = row?;
        day.answers += 1;
        if (200..300).contains(&status) {
            healthy.push(Duration::from_millis(u64::try_from(waited).unwrap_or(0)));
        } else {
            day.refused += 1;
        }
    }
    healthy.sort_unstable();
    day.median = rank(&healthy, 50);
    day.p95 = rank(&healthy, 95);
    day.slowest = healthy.last().copied();
    day.load = conn
        .query_row(
            "SELECT load, at_ms FROM answers
             WHERE at_ms > ?1 AND load IS NOT NULL ORDER BY at_ms DESC LIMIT 1",
            params![since],
            |row| Ok((row.get::<_, String>(0)?, EpochMs::new(row.get(1)?))),
        )
        .map(Some)
        .or_else(none_if_no_row)?;
    day.via = conn
        .query_row(
            "SELECT via FROM answers WHERE at_ms > ?1 ORDER BY at_ms DESC LIMIT 1",
            params![since],
            |row| row.get::<_, String>(0),
        )
        .map(|via| Via::parse(&via))
        .or_else(none_if_no_row)?;
    Ok(day)
}

fn none_if_no_row<T>(e: rusqlite::Error) -> Result<Option<T>, rusqlite::Error> {
    match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        e => Err(e),
    }
}

/// The value at percentile `p` of `sorted`, by nearest rank.
fn rank(sorted: &[Duration], p: usize) -> Option<Duration> {
    if sorted.is_empty() {
        return None;
    }
    let index = (sorted.len() * p).div_ceil(100).max(1) - 1;
    sorted.get(index.min(sorted.len() - 1)).copied()
}

/// A wait as a column: whole milliseconds, saturating.
fn millis(wait: Duration) -> i64 {
    i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    const MINUTE: i64 = 60_000;

    fn answer(at: EpochMs, status: u16, waited_ms: u64, load: Option<&str>) -> AnswerTime {
        AnswerTime {
            at,
            operation: "followers".to_string(),
            status,
            waited: Duration::from_millis(waited_ms),
            load: load.map(str::to_string),
            via: Via::Browser,
        }
    }

    fn stored(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM answers", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_week_old_answer_and_one_from_the_future_are_pruned() {
        let mut store = Store::in_memory().unwrap();
        let now = EpochMs::new(30 * DAY_MS);
        let conn = store.conn_mut();
        for at in [now.get() - 8 * DAY_MS, now.get() + 60 * MINUTE] {
            conn.execute(
                "INSERT INTO answers (at_ms, operation, status, waited_ms, via)
                 VALUES (?1, 'followers', 200, 300, 'browser')",
                params![at],
            )
            .unwrap();
        }
        assert_eq!(stored(conn), 2);
        record(conn, &answer(now, 200, 300, None), now).unwrap();
        assert_eq!(stored(conn), 1);
    }

    #[test]
    fn recent_waits_are_the_healthy_ones_in_the_window_newest_first() {
        let mut store = Store::in_memory().unwrap();
        let now = EpochMs::new(30 * DAY_MS);
        let conn = store.conn_mut();
        let at = |minutes_ago: i64| EpochMs::new(now.get() - minutes_ago * MINUTE);
        for (ago, status, waited) in [
            (20, 200, 9_000),
            (4, 200, 400),
            (3, 429, 50),
            (2, 200, 600),
            (1, 200, 800),
        ] {
            record(conn, &answer(at(ago), status, waited, None), now).unwrap();
        }
        let waits = recent_waits(conn, at(10), 5).unwrap();
        assert_eq!(waits, [800, 600, 400].map(Duration::from_millis));
        let two = recent_waits(conn, at(10), 2).unwrap();
        assert_eq!(two, [800, 600].map(Duration::from_millis));
    }

    #[test]
    fn the_last_day_is_summed_over_its_healthy_answers() {
        let mut store = Store::in_memory().unwrap();
        let now = EpochMs::new(30 * DAY_MS);
        let conn = store.conn_mut();
        let at = |minutes_ago: i64| EpochMs::new(now.get() - minutes_ago * MINUTE);
        record(conn, &answer(at(25 * 60), 200, 60_000, Some("old")), now).unwrap();
        for (i, waited) in (100..=1_000).step_by(100).enumerate() {
            let load = (i == 3).then_some("x-ig-capacity-level=3");
            record(conn, &answer(at(60 - i as i64), 200, waited, load), now).unwrap();
        }
        record(conn, &answer(at(1), 429, 20, None), now).unwrap();

        let day = last_day(conn, now).unwrap();
        assert_eq!(day.answers, 11);
        assert_eq!(day.refused, 1);
        assert_eq!(day.median, Some(Duration::from_millis(500)));
        assert_eq!(day.p95, Some(Duration::from_millis(1_000)));
        assert_eq!(day.slowest, Some(Duration::from_millis(1_000)));
        assert_eq!(
            day.load,
            Some(("x-ig-capacity-level=3".to_string(), at(57)))
        );
        assert_eq!(day.via, Some(Via::Browser));
    }

    #[test]
    fn a_day_with_no_answers_is_empty() {
        let store = Store::in_memory().unwrap();
        let day = last_day(store.conn(), EpochMs::new(30 * DAY_MS)).unwrap();
        assert_eq!(day, AnswersDay::default());
    }
}
