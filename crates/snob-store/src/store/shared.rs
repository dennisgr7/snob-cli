//! What every account shares: `data/shared.db`.
//!
//! The push-backs any account received, so that each account's budget can see
//! the others' and stop with them ([`snob_core::budget::common_brake`]). A file
//! of its own, with a migration chain of its own, because an account's
//! database is only ever opened as that account and this one is read by all
//! of them.

use std::sync::LazyLock;
use std::time::Duration;

use rusqlite::{Connection, params};
use rusqlite_migration::{M, Migrations};

use snob_core::budget::PushBack;
use snob_core::{Epoch, EpochMs, Pk};

use super::{StoreError, pk_from_sql, pk_to_sql};
use crate::paths::AppPaths;

/// Every migration of `shared.db`, in the order they are applied. The rules at
/// the top of [`super::migrations`] hold here too.
const CHAIN: [&str; 1] = [include_str!("sql/shared/001_pushbacks.sql")];

static MIGRATIONS: LazyLock<Migrations<'static>> =
    LazyLock::new(|| Migrations::new(CHAIN.iter().map(|sql| M::up(sql)).collect()));

/// How long a push-back is kept: longer than any cooldown, which is capped at
/// a day, plus the brake's window.
const KEPT: Duration = Duration::from_secs(2 * 24 * 3600);

/// The `meta` key the interval seed is kept under.
const INTERVAL_SEED: &str = "interval_seeded_at";

pub struct Shared {
    conn: Connection,
}

impl Shared {
    /// Opens `shared.db`, creating it on first use.
    ///
    /// With the store's settings, and `synchronous = FULL` for the reason
    /// `SqliteRateBudget::open` gives: losing a push-back lets the other
    /// accounts out of a brake early.
    pub fn open(paths: &AppPaths) -> Result<Self, StoreError> {
        paths.ensure_dirs()?;
        let path = paths.shared_db_file();
        let mut conn = Connection::open(&path)?;
        super::configure(&conn)?;
        super::reject_newer_schema(&conn, &path, CHAIN.len())?;
        super::migrate(&mut conn, &MIGRATIONS)?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self { conn })
    }

    /// `shared.db` for a command that only reports: read, never created and
    /// nothing in it changed (`store::read_only`). `None` when there is no
    /// file yet, which is before any account's budget was first opened, or
    /// when the file is from a newer snob.
    pub fn read_existing(paths: &AppPaths) -> Result<Option<Self>, StoreError> {
        match super::read_only(&paths.shared_db_file(), &MIGRATIONS, CHAIN.len()) {
            Ok(conn) => Ok(conn.map(|conn| Self { conn })),
            Err(StoreError::SchemaFromNewerSnob { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Records a push-back on `pk`, and forgets the ones too old to matter.
    pub fn record(
        &self,
        pk: Pk,
        at: EpochMs,
        until: EpochMs,
        reason: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO pushbacks (pk, at_ms, until_ms, reason) VALUES (?1, ?2, ?3, ?4)",
            params![pk_to_sql(pk), at.get(), until.get(), reason],
        )?;
        self.conn.execute(
            "DELETE FROM pushbacks WHERE at_ms < ?1",
            params![(at - KEPT).get()],
        )?;
        Ok(())
    }

    /// The push-backs kept at `now`: every one that can still be part of a
    /// brake.
    pub fn pushbacks(&self, now: EpochMs) -> Result<Vec<PushBack>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT pk, at_ms, until_ms FROM pushbacks WHERE at_ms >= ?1")?;
        let rows = statement.query_map(params![(now - KEPT).get()], |row| {
            Ok(PushBack {
                pk: pk_from_sql(row.get(0)?),
                at: EpochMs::new(row.get(1)?),
                until: EpochMs::new(row.get(2)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// When an interval schedule started counting, if it was written down.
    ///
    /// One machine, one monitor: every account's runs count from it.
    pub fn interval_seeded_at(&self) -> Result<Option<Epoch>, StoreError> {
        let at = super::meta_get(&self.conn, INTERVAL_SEED)?;
        Ok(at.and_then(|at| at.parse().ok()).map(Epoch::new))
    }

    /// Writes the seed down unless one already is, and answers with the one
    /// kept: a later start must never move the moment the interval counts
    /// from, and of two starts racing, either is right.
    pub fn seed_interval(&self, at: Epoch) -> Result<Epoch, StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params![INTERVAL_SEED, at.get().to_string()],
        )?;
        Ok(self.interval_seeded_at()?.unwrap_or(at))
    }

    /// Removes every push-back on `pk`, for `purge --account`.
    pub fn forget(&self, pk: Pk) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM pushbacks WHERE pk = ?1",
            params![pk_to_sql(pk)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::clock::now_ms;

    #[test]
    fn the_migration_chain_is_valid() {
        MIGRATIONS.validate().unwrap();
    }

    fn shared() -> (tempfile::TempDir, Shared) {
        let tmp = tempfile::tempdir().unwrap();
        let shared = Shared::open(&AppPaths::rooted_at(tmp.path())).unwrap();
        (tmp, shared)
    }

    #[test]
    fn a_push_back_is_read_back() {
        let (_tmp, shared) = shared();
        let now = now_ms();
        let until = now + Duration::from_secs(3600);
        shared.record(Pk::new(1), now, until, "429").unwrap();
        assert_eq!(
            shared.pushbacks(now).unwrap(),
            vec![PushBack {
                pk: Pk::new(1),
                at: now,
                until
            }]
        );
    }

    #[test]
    fn old_push_backs_are_pruned_as_new_ones_arrive() {
        let (_tmp, shared) = shared();
        let now = now_ms();
        let old = now - KEPT - Duration::from_secs(1);
        shared.record(Pk::new(1), old, old, "429").unwrap();
        shared.record(Pk::new(2), now, now, "429").unwrap();
        let left: i64 = shared
            .conn
            .query_row("SELECT count(*) FROM pushbacks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn forgetting_an_account_leaves_the_others() {
        let (_tmp, shared) = shared();
        let now = now_ms();
        shared.record(Pk::new(1), now, now, "429").unwrap();
        shared.record(Pk::new(2), now, now, "429").unwrap();
        shared.forget(Pk::new(1)).unwrap();
        let left: Vec<Pk> = shared
            .pushbacks(now)
            .unwrap()
            .into_iter()
            .map(|p| p.pk)
            .collect();
        assert_eq!(left, vec![Pk::new(2)]);
    }

    #[test]
    fn the_interval_seed_is_written_once() {
        let (_tmp, shared) = shared();
        assert_eq!(shared.interval_seeded_at().unwrap(), None);
        assert_eq!(
            shared.seed_interval(Epoch::new(1_000)).unwrap(),
            Epoch::new(1_000)
        );
        assert_eq!(
            shared.seed_interval(Epoch::new(2_000)).unwrap(),
            Epoch::new(1_000),
            "a later start keeps the first seed"
        );
        assert_eq!(
            shared.interval_seeded_at().unwrap(),
            Some(Epoch::new(1_000))
        );
    }

    /// STRICT, like every table in the accounts' databases.
    #[test]
    fn the_table_rejects_the_wrong_types() {
        let (_tmp, shared) = shared();
        let result = shared.conn.execute(
            "INSERT INTO pushbacks (pk, at_ms, until_ms, reason) VALUES ('x', 1, 1, '429')",
            [],
        );
        assert!(result.is_err());
    }
}
