//! What an agent was told yes to through `snob mcp`, and the cap on an
//! agent's writes in a day.
//!
//! **snob cannot prove who said yes** (AGENTS.md, "The MCP server"): a client
//! declares what it can show, and an agent can type a yes in the chat itself.
//! So what was done in the person's name is kept where they can see it
//! (`snob status`), and the number of writes an agent may make in a day stays
//! a person's number, whoever answered.
//!
//! One row per command a child of `snob mcp` ran on a yes, written by that
//! child **before its first request**, in a transaction of its own: a write
//! Instagram then refused still counts, which is the conservative way round
//! for a cap. The cap and the record are one transaction, so two runs cannot
//! both take the last write of the day.
//!
//! Only follow and unfollow are writes ([`is_write`]), and only acts recorded
//! here are counted: a write the person types in a terminal is theirs, and no
//! row is written for it.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{Store, StoreError, pk_from_sql, pk_to_sql};
use snob_core::{EpochMs, Pk};

/// The writes an agent may make in any 24 hours when the server was given no
/// `--max-writes-per-day`: about as many follows and unfollows as a person
/// tidying their list by hand makes in a sitting, and a tenth of what the
/// write bucket would let through.
pub const WRITES_A_DAY: u32 = 10;

/// The most `--max-writes-per-day` may be: what the write bucket lets
/// through in a day anyway.
pub const MOST_WRITES_A_DAY: u32 = super::rate_budget::WRITES_IN_A_DAY;

/// The window the cap counts and `snob status` shows.
pub const DAY_MS: i64 = 86_400_000;

/// How long a row is kept: past the day anything reads, so that a look at
/// the database by hand finds the recent past, and short enough that the
/// table stays a few hundred rows at most.
const KEPT_MS: i64 = 7 * DAY_MS;

/// The commands that change something on Instagram.
pub fn is_write(action: &str) -> bool {
    matches!(action, "follow" | "unfollow")
}

/// An act about to be run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewAct<'a> {
    /// The command: `follow`, `unfollow`, or the list walked.
    pub action: &'a str,
    /// The username the call named, if it named one.
    pub target: Option<&'a str>,
    /// How the yes was given: `form`, `elicitation` or `chat`.
    pub via: &'a str,
    /// The client, as the server reduced its name and version.
    pub client: Option<&'a str>,
}

/// An act as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Act {
    pub at: EpochMs,
    pub action: String,
    pub target_pk: Option<Pk>,
    pub target_name: Option<String>,
    pub via: String,
    pub client: Option<String>,
}

impl Act {
    pub fn is_write(&self) -> bool {
        is_write(&self.action)
    }
}

/// What came of recording an act.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Recorded, under this id ([`resolved`]).
    Done(i64),
    /// Not recorded, and not to be run: `writes` writes in the last day
    /// already reach the cap, and the next one is free at `free_at`.
    Capped { writes: u32, free_at: EpochMs },
}

/// Records `act` at `now`, unless it is a write and `cap` writes have been
/// recorded in the last day. `cap` is `None` for an act the cap does not
/// apply to.
///
/// A row stamped in the future is a clock that went backwards, and is
/// counted: a cap that a clock set back could empty would not be one.
pub fn record(
    store: &mut Store,
    act: &NewAct<'_>,
    cap: Option<u32>,
    now: EpochMs,
) -> Result<Recorded, StoreError> {
    let tx = store
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(cap) = cap.filter(|_| is_write(act.action))
        && let Some(capped) = over_the_cap(&tx, cap, now)?
    {
        return Ok(capped);
    }
    tx.execute(
        "DELETE FROM mcp_acts WHERE at_ms <= ?1",
        params![now.get() - KEPT_MS],
    )?;
    tx.execute(
        "INSERT INTO mcp_acts (at_ms, action, target_name, via, client)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now.get(), act.action, act.target, act.via, act.client],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(Recorded::Done(id))
}

/// Whether one more write at `now` would be past `cap`, **writing nothing**:
/// [`Recorded::Capped`] when it would. What [`record`] decides inside its
/// transaction, and what `snob mcp` asks before it asks the person for a yes
/// to a write (`follow --check-ahead`), so nobody says yes to one the cap
/// then refuses. Between the two another write may still take the last one,
/// which `record` then refuses.
pub fn over_the_cap(
    conn: &Connection,
    cap: u32,
    now: EpochMs,
) -> Result<Option<Recorded>, StoreError> {
    let writes = writes_since(conn, now.get() - DAY_MS)?;
    let counted = u32::try_from(writes.len()).unwrap_or(u32::MAX);
    if counted < cap {
        return Ok(None);
    }
    // The day frees a write when the oldest of those that would still be
    // over the cap leaves it. `get`, though `counted >= cap` keeps the index
    // in range: a cap of 0 would otherwise index past the end.
    let index = writes.len() - cap as usize;
    let oldest = writes.get(index).copied().unwrap_or(now.get());
    Ok(Some(Recorded::Capped {
        writes: counted,
        free_at: EpochMs::new(oldest.max(now.get() - DAY_MS) + DAY_MS),
    }))
}

/// The moments of the writes recorded after `after_ms`, oldest first.
fn writes_since(conn: &Connection, after_ms: i64) -> Result<Vec<i64>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT at_ms FROM mcp_acts
         WHERE at_ms > ?1 AND action IN ('follow', 'unfollow') ORDER BY at_ms",
    )?;
    let rows = statement.query_map(params![after_ms], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Fills in the account an act was about, once the command resolved it.
pub fn resolved(conn: &Connection, id: i64, pk: Pk) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE mcp_acts SET target_pk = ?1 WHERE id = ?2",
        params![pk_to_sql(pk), id],
    )?;
    Ok(())
}

/// The acts of the day before `now`, oldest first.
pub fn last_day(conn: &Connection, now: EpochMs) -> Result<Vec<Act>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT at_ms, action, target_pk, target_name, via, client FROM mcp_acts
         WHERE at_ms > ?1 ORDER BY at_ms, id",
    )?;
    let rows = statement.query_map(params![now.get() - DAY_MS], |row| {
        Ok(Act {
            at: EpochMs::new(row.get(0)?),
            action: row.get(1)?,
            target_pk: row.get::<_, Option<i64>>(2)?.map(pk_from_sql),
            target_name: row.get(3)?,
            via: row.get(4)?,
            client: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// How many acts are stored, of any age; for the tests that check one was
/// or was not written.
#[doc(hidden)]
pub fn count(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn
        .query_row("SELECT count(*) FROM mcp_acts", [], |row| row.get(0))
        .optional()?
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(action: &str) -> NewAct<'_> {
        NewAct {
            action,
            target: Some("someone"),
            via: "chat",
            client: Some("test 0"),
        }
    }

    #[test]
    fn an_act_is_recorded_before_the_request() {
        // Committed by `record` itself: a second connection to the file sees
        // it before the caller does anything else, so a run killed at its
        // first request has already left it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.db");
        let mut store = Store::open_at(&path).unwrap();
        let now = EpochMs::new(1_000 * DAY_MS);
        let Recorded::Done(id) = record(&mut store, &act("unfollow"), Some(10), now).unwrap()
        else {
            panic!("under the cap");
        };
        let other = Store::open_at(&path).unwrap();
        let seen = last_day(other.conn(), now).unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].action, "unfollow");
        assert_eq!(seen[0].target_name.as_deref(), Some("someone"));
        assert_eq!(seen[0].target_pk, None, "not resolved yet");
        assert_eq!(seen[0].via, "chat");
        assert_eq!(seen[0].client.as_deref(), Some("test 0"));
        assert!(seen[0].is_write());

        resolved(store.conn(), id, Pk::new(9001)).unwrap();
        assert_eq!(
            last_day(other.conn(), now).unwrap()[0].target_pk,
            Some(Pk::new(9001))
        );
    }

    #[test]
    fn the_write_cap_counts_only_the_last_day() {
        let mut store = Store::in_memory().unwrap();
        let start = EpochMs::new(1_000 * DAY_MS);
        let at = |ms: i64| EpochMs::new(start.get() + ms);
        // Three writes an hour apart, and walks, which are never counted.
        for hour in 0..3 {
            let now = at(hour * 3_600_000);
            assert!(matches!(
                record(&mut store, &act("follow"), Some(3), now).unwrap(),
                Recorded::Done(_)
            ));
            assert!(matches!(
                record(&mut store, &act("followers"), Some(3), now).unwrap(),
                Recorded::Done(_)
            ));
        }
        // The fourth, the same day, is refused and not recorded; a walk is not.
        let later = at(5 * 3_600_000);
        assert_eq!(
            record(&mut store, &act("unfollow"), Some(3), later).unwrap(),
            Recorded::Capped {
                writes: 3,
                free_at: at(DAY_MS)
            },
            "free when the first of the three leaves the day"
        );
        assert_eq!(count(store.conn()).unwrap(), 6);
        assert!(matches!(
            record(&mut store, &act("fans"), Some(3), later).unwrap(),
            Recorded::Done(_)
        ));
        // With no cap, a write is recorded however many came before.
        assert!(matches!(
            record(&mut store, &act("follow"), None, later).unwrap(),
            Recorded::Done(_)
        ));
        // A day after the first, it has left the window and there is room.
        // A day after the first, it has left the window: three are counted
        // (one, two and five hours in), and the next frees an hour on.
        let next_day = at(DAY_MS + 1);
        assert_eq!(
            record(&mut store, &act("unfollow"), Some(3), next_day).unwrap(),
            Recorded::Capped {
                writes: 3,
                free_at: at(DAY_MS + 3_600_000)
            }
        );
        assert!(matches!(
            record(&mut store, &act("unfollow"), Some(4), next_day).unwrap(),
            Recorded::Done(_)
        ));
        let shown = last_day(store.conn(), next_day).unwrap();
        assert_eq!(
            shown.iter().filter(|act| act.is_write()).count(),
            4,
            "the first write is a day old"
        );
    }

    /// Asked ahead of the question, the cap answers as `record` will, and
    /// writes nothing.
    #[test]
    fn the_cap_can_be_asked_about_without_recording_anything() {
        let mut store = Store::in_memory().unwrap();
        let now = EpochMs::new(1_000 * DAY_MS);
        assert_eq!(over_the_cap(store.conn(), 1, now).unwrap(), None);
        record(&mut store, &act("follow"), Some(1), now).unwrap();
        let later = EpochMs::new(now.get() + 60_000);
        let asked = over_the_cap(store.conn(), 1, later).unwrap();
        assert_eq!(
            asked,
            Some(Recorded::Capped {
                writes: 1,
                free_at: EpochMs::new(now.get() + DAY_MS)
            })
        );
        assert_eq!(count(store.conn()).unwrap(), 1, "asking wrote nothing");
        assert_eq!(
            Some(record(&mut store, &act("unfollow"), Some(1), later).unwrap()),
            asked
        );
    }

    #[test]
    fn a_week_old_act_is_pruned_and_a_future_one_still_counts() {
        let mut store = Store::in_memory().unwrap();
        let start = EpochMs::new(1_000 * DAY_MS);
        record(&mut store, &act("follow"), None, start).unwrap();
        // A clock set back an hour: the write it recorded is still counted.
        let back = EpochMs::new(start.get() - 3_600_000);
        assert!(matches!(
            record(&mut store, &act("follow"), Some(1), back).unwrap(),
            Recorded::Capped { writes: 1, .. }
        ));
        let week_on = EpochMs::new(start.get() + KEPT_MS + 1);
        record(&mut store, &act("fans"), None, week_on).unwrap();
        assert_eq!(count(store.conn()).unwrap(), 1, "only the new one is kept");
    }

    #[test]
    fn the_largest_cap_is_what_the_write_bucket_allows_in_a_day() {
        assert_eq!(MOST_WRITES_A_DAY, 99);
        const { assert!(WRITES_A_DAY < MOST_WRITES_A_DAY) };
    }
}
