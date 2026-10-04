//! SQLite storage.
//!
//! One file per account in the local data directory, and `shared.db` for what
//! every account shares. Synchronous on purpose:
//! transactions take microseconds and it is not worth dragging in an async
//! wrapper, especially since none of them is currently kept up to date with the
//! version of `rusqlite` we use.

pub mod accounts;
pub mod deliveries;
mod migrations;
pub mod rate_budget;
pub mod shared;
pub mod snapshots;
pub mod users;
pub mod watch;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::Migrations;
use thiserror::Error;

use crate::paths::{AccountPaths, PathError};
use snob_core::Pk;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not apply migrations: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error(transparent)]
    Paths(#[from] PathError),
    /// A downgrade -- or a leftover `~/.cargo/bin/snob` earlier on the `PATH`
    /// -- meets a database written by a build with more migrations than it
    /// has. Without this the user gets `rusqlite_migration`'s own wording,
    /// "Attempt to migrate a database with a migration number that is too
    /// high", about a file they have no reason to think is broken; the
    /// plausible response is deleting it, which throws away every capture and
    /// every mark.
    #[error(
        "the database at {path} was written by a newer version of snob: it is at schema \
         {found} and this build knows {known}.\nUpgrade snob, or point this one at a \
         different data directory."
    )]
    SchemaFromNewerSnob {
        path: String,
        found: i64,
        known: usize,
    },
    /// A page arrived for a walk this process no longer holds.
    ///
    /// The one store error that is not a fault: another process decided this
    /// walk had been abandoned and adopted it, and the honest response is to
    /// stop rather than to write a second stream of pages into one capture.
    #[error("another process took over this walk")]
    ClaimTaken,
}

/// SQLite only has signed 64-bit integers, and `rusqlite` stopped converting
/// `u64` in 0.38. Instagram ids are at most thirteen digits, eight orders of
/// magnitude below the ceiling, so the round trip is exact for any value this
/// tool will ever see.
///
/// **These two functions are the only road, and nothing can be written that
/// goes around them.** [`Pk`] implements neither `ToSql` nor `FromSql`, so
/// `params![pk]` and `row.get::<_, Pk>(0)` do not compile at all: the
/// discipline is the compiler's rather than code review's. `rusqlite`'s
/// `fallible_uint` feature stays off as well, deliberately, for every other
/// `u64` in the schema; the manifest says so next to the dependency.
///
/// Implementing the two traits on [`Pk`] would be better still, because the
/// bit-cast would live in one place instead of two functions that have to
/// agree. It is not available: `snob-core` does **no I/O** and must not
/// compile SQLite — that is what the crate split is for — and the orphan rule
/// stops this crate writing an impl of `rusqlite`'s trait for a type it does
/// not own. Two functions in one module is the closest reachable shape.
#[inline]
pub(crate) fn pk_to_sql(pk: Pk) -> i64 {
    pk.get() as i64
}

#[inline]
pub(crate) fn pk_from_sql(value: i64) -> Pk {
    Pk::new(value as u64)
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// The account's database, created with its directory on first use.
    pub fn open(paths: &AccountPaths) -> Result<Self, StoreError> {
        paths.ensure_dirs()?;
        Self::open_at(&paths.db_file())
    }

    /// The account's database if it has one, never creating it or its
    /// directory: a pass over other accounts must not bring back one that
    /// `purge` removed while it ran.
    pub fn open_existing(paths: &AccountPaths) -> Result<Option<Self>, StoreError> {
        let flags =
            rusqlite::OpenFlags::default().difference(rusqlite::OpenFlags::SQLITE_OPEN_CREATE);
        match Connection::open_with_flags(paths.db_file(), flags) {
            Ok(conn) => Self::opened(conn, &paths.db_file()).map(Some),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::CannotOpen && !paths.db_file().exists() =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The account's database **for a report**: read, and nothing stored
    /// changed. `None` when it has none.
    ///
    /// [`Store::open_existing`] migrates a database from an older snob, and
    /// compacts it after, and its last connection checkpoints the log into
    /// the file when it closes: three writes, which a command that says it
    /// writes nothing must not make. See [`read_only`].
    pub fn read_existing(paths: &AccountPaths) -> Result<Option<Self>, StoreError> {
        let conn = read_only(&paths.db_file(), &migrations::MIGRATIONS, migrations::COUNT)?;
        Ok(conn.map(|conn| Self { conn }))
    }

    pub fn open_at(path: &Path) -> Result<Self, StoreError> {
        Self::opened(Connection::open(path)?, path)
    }

    fn opened(mut conn: Connection, path: &Path) -> Result<Self, StoreError> {
        configure(&conn)?;
        reject_newer_schema(&conn, path, migrations::COUNT)?;
        migrate(&mut conn, &migrations::MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// Throwaway database for tests.
    #[doc(hidden)]
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::opened(Connection::open_in_memory()?, Path::new(":memory:"))
    }

    /// Read access for the child modules and the CLI. Writes that span several
    /// tables go through functions that manage their own transaction.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// One remembered fact, by name, from the `meta` table: facts about the
    /// database itself rather than about an account.
    ///
    /// A method so that a caller with a `Store` does not need `rusqlite` in its
    /// own manifest to reach the table. `snob-cli` is that caller, and adding a
    /// database crate to it for two lines would put the connection type in a
    /// place that has no business naming it.
    pub fn remembered(&self, key: &str) -> Result<Option<String>, StoreError> {
        meta_get(&self.conn, key)
    }

    /// Writes one.
    pub fn remember(&self, key: &str, value: &str) -> Result<(), StoreError> {
        meta_set(&self.conn, key, value)
    }
}

/// Runs the migration chain with foreign keys out of the way.
///
/// This is the **only** place they can be turned off for a migration.
/// `rusqlite_migration` runs the whole chain inside one transaction, and SQLite
/// documents `PRAGMA foreign_keys` as a no-op inside one — so a migration that
/// writes the pragma itself, the way SQLite's own table-rebuild recipe says to,
/// would look correct and do nothing.
///
/// What that costs is not a failed migration, it is silent data loss. The first
/// migration to rebuild `snapshots` by create-copy-drop-rename would have its
/// `DROP TABLE` fire `ON DELETE CASCADE` on `snapshot_members` and empty it.
/// The rebuilt snapshots still read `complete = 1`, so `usable_snapshots` keeps
/// serving them, `members()` returns nothing, and `snob unfollowers` reports
/// everyone you follow as an unfollower.
///
/// It takes the migrations rather than reaching for the static so that a test
/// can run its own chain through the very function production uses. Checking
/// this against a hand-written copy of the wrapping would prove nothing.
fn migrate(conn: &mut Connection, migrations: &Migrations<'_>) -> Result<(), StoreError> {
    let before = user_version(conn).unwrap_or(0);

    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = migrations.to_latest(conn);
    // Back on even if the chain failed: the connection is handed back to the
    // caller either way, and `open_at` only stops on the `?` below.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // **Another process may have got there first.** `rusqlite_migration`
    // reads the version before it opens its transaction, so two snobs opening
    // the database for the first time after an upgrade — the monitor and a
    // command typed by hand — both decide to migrate, and the second waits out
    // the first's lock and then fails applying the same chain again ("table
    // already exists"). The chain is one transaction, so nothing is
    // half-applied; what is left to ask is whether the database is now where
    // it needed to be, and if it is, the error was only about who did it.
    if let Err(e) = outcome {
        if migrations
            .pending_migrations(conn)
            .map_or(true, |pending| pending > 0)
        {
            return Err(e.into());
        }
        tracing::debug!(error = %e, "another process migrated the database first");
    }

    // **Once, and only when a migration actually ran.**
    //
    // `DROP INDEX` moves the index's pages to the freelist; the file only
    // shrinks on `VACUUM`, and on the index 010 removes that is 42% of it.
    // `VACUUM` cannot run inside a transaction, so it cannot live in the
    // migration, and it rewrites the whole file, so it must not run on every
    // open — hence the version comparison rather than a flag somebody has to
    // remember to clear.
    //
    // Best-effort. A database that could not be compacted is a database that
    // works and takes more disk, which is not worth failing an open over: the
    // space comes back on the next migration, or never, and either way the
    // user's command runs.
    if user_version(conn).unwrap_or(0) > before
        && let Err(e) = conn.execute_batch("VACUUM")
    {
        tracing::debug!(error = %e, "the database could not be compacted after migrating");
    }

    Ok(())
}

/// One remembered value, from the `meta` table of this connection's database.
fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// What the migration runner counts up to. `0` on a database that has none.
fn user_version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Settings that have to be applied on every open.
///
/// `journal_mode` is the exception: it is written into the file header and
/// persists across opens. The rest are per connection.
///
/// `foreign_keys` is deliberately **not** here: it belongs around the migration
/// run, which is the one thing that needs it off, and [`migrate`] leaves it on
/// afterwards. (The bundled SQLite is compiled with
/// `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`, so in this binary it is on before anyone
/// asks — but the schema's foreign keys are load-bearing, so it is set rather
/// than assumed.)
///
/// Also called by `SqliteRateBudget::open`, which opens its own connection to
/// the same file and would otherwise miss every protection below, and by
/// `shared::Shared::open`. Both override `synchronous` afterwards, and say
/// there why.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(5_000))?;

    // `PRAGMA journal_mode` returns a row with the resulting mode, so it has to
    // be queried. With `pragma_update` it fails with "Execute returned results".
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

    conn.pragma_update(None, "synchronous", "NORMAL")?;

    // Not only a speed setting. Without it SQLite may spill a temporary
    // b-tree into TMPDIR, which is outside every directory `purge` knows
    // about — so a query's working copy of the follower list would outlive
    // the command whose whole job is to leave nothing behind.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -8_000)?; // 8 MiB

    // Deleted rows are overwritten rather than merely unlinked from the page.
    // This is what any pruning inside the file needs — `delete_partials` on
    // every walk, and `watch::prune`'s sweeps: they remove content without
    // removing the file, and the default leaves it legible in the freed pages.
    // A database of a few megabytes does not notice the cost.
    conn.pragma_update(None, "secure_delete", "ON")?;

    // In WAL mode the log is reused rather than truncated, so it keeps the
    // pre-image of everything `secure_delete` just scrubbed from the database
    // proper. Bounding it bounds how much of that history survives.
    conn.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;

    // Nothing here uses a virtual table or a function inside the schema, so
    // this costs nothing — and it is SQLite's own advice for any application
    // that can manage without them, because the schema of a database file is
    // executable content and this file sits at a fixed, guessable path.
    conn.pragma_update(None, "trusted_schema", "OFF")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;

    Ok(())
}

/// The database at `path`, opened to be read and never changed: `None` when
/// there is no file, and never one created.
///
/// - **Nothing written through it**: `query_only` refuses any statement
///   that would, and the connection does not checkpoint the log into the
///   file when it closes, which the last connection otherwise does.
/// - **A schema from an older snob is not migrated in place.** It is copied
///   into memory and brought up to date there, so the report reads what the
///   newest schema says; the file itself is migrated by the next command
///   that writes, as ever.
/// - One from a newer snob is refused, as every open refuses it.
pub(crate) fn read_only(
    path: &Path,
    migrations: &Migrations<'_>,
    known: usize,
) -> Result<Option<Connection>, StoreError> {
    if !path.exists() {
        return Ok(None);
    }
    // Opened read-write, not `SQLITE_OPEN_READ_ONLY`: a read-only connection
    // cannot set up the WAL index when the log files are absent, which is
    // how a database another connection closed cleanly is left.
    // `query_only` is what keeps it from writing.
    let flags = rusqlite::OpenFlags::default().difference(rusqlite::OpenFlags::SQLITE_OPEN_CREATE);
    let conn = match Connection::open_with_flags(path, flags) {
        Ok(conn) => conn,
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::CannotOpen && !path.exists() =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    conn.busy_timeout(Duration::from_millis(5_000))?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
        true,
    )?;
    conn.pragma_update(None, "trusted_schema", "OFF")?;
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.pragma_update(None, "query_only", "ON")?;
    reject_newer_schema(&conn, path, known)?;
    if usize::try_from(user_version(&conn)?).is_ok_and(|found| found == known) {
        return Ok(Some(conn));
    }

    let mut copy = Connection::open_in_memory()?;
    // Every page in one step: the copy is of a quiet file, and a step that
    // met a writer is retried after the pause.
    rusqlite::backup::Backup::new(&conn, &mut copy)?.run_to_completion(
        i32::MAX,
        Duration::from_millis(50),
        None,
    )?;
    drop(conn);
    configure(&copy)?;
    migrate(&mut copy, migrations)?;
    copy.pragma_update(None, "query_only", "ON")?;
    Ok(Some(copy))
}

/// Refuses a database written by a **newer** build, before the migration runner
/// says so in its own words; [`StoreError::SchemaFromNewerSnob`] says why.
///
/// `known` is how many migrations this build has for the file: `shared.db` has
/// a chain of its own.
fn reject_newer_schema(conn: &Connection, path: &Path, known: usize) -> Result<(), StoreError> {
    let found = user_version(conn)?;
    if found as usize > known {
        return Err(StoreError::SchemaFromNewerSnob {
            path: path.display().to_string(),
            found,
            known,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite_migration::M;

    use super::*;

    #[test]
    fn an_id_survives_the_round_trip() {
        for n in [0, 1, 4_340_136_074, 71_234_567_890, i64::MAX as u64] {
            let pk = Pk::new(n);
            assert_eq!(pk_from_sql(pk_to_sql(pk)), pk);
        }
    }

    #[test]
    fn migrations_are_applied_on_open() {
        let db = Store::in_memory().unwrap();
        let tables: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'users'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    /// Proves STRICT really applied: without it SQLite would accept the text in
    /// an INTEGER column thanks to its loose typing.
    #[test]
    fn tables_reject_the_wrong_types() {
        let db = Store::in_memory().unwrap();
        let result = db.conn().execute(
            "INSERT INTO users (pk, username, first_seen, last_seen)
             VALUES ('not a number', 'x', 0, 0)",
            [],
        );
        assert!(result.is_err(), "STRICT is not active");
    }

    /// Proves the foreign keys are live once the store is open. `migrate` turns
    /// them off for the chain and has to turn them back on.
    #[test]
    fn foreign_keys_are_enforced() {
        let db = Store::in_memory().unwrap();
        let result = db.conn().execute(
            "INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (999, 999, 0)",
            [],
        );
        assert!(result.is_err(), "foreign_keys is not active");
    }

    /// A migration that rebuilds a table must not take the rows of the tables
    /// that reference it; `migrate` says why only it can see to that. So this
    /// drives the real `migrate`, with a second migration shaped like the one
    /// somebody will eventually write.
    #[test]
    fn a_migration_that_rebuilds_a_table_keeps_its_children() {
        // The view has to go first and come back afterwards: SQLite checks
        // every view when a table is renamed, and `usable_snapshots` selects
        // from `snapshots`. Worth knowing before writing a real rebuild.
        let rebuild_snapshots = "
            DROP VIEW usable_snapshots;
            CREATE TABLE snapshots_new (
              id             INTEGER PRIMARY KEY,
              account_pk     INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
              kind           TEXT    NOT NULL,
              source         TEXT    NOT NULL DEFAULT 'live',
              started_at     INTEGER NOT NULL,
              taken_at       INTEGER,
              complete       INTEGER NOT NULL DEFAULT 0,
              member_count   INTEGER NOT NULL DEFAULT 0,
              declared_count INTEGER,
              pages          INTEGER NOT NULL DEFAULT 0,
              requests       INTEGER NOT NULL DEFAULT 0,
              next_cursor    TEXT,
              resumes        INTEGER NOT NULL DEFAULT 0,
              stopped_by     TEXT
            ) STRICT;
            INSERT INTO snapshots_new SELECT
              id, account_pk, kind, source, started_at, taken_at, complete,
              member_count, declared_count, pages, requests, next_cursor,
              resumes, stopped_by FROM snapshots;
            DROP TABLE snapshots;
            ALTER TABLE snapshots_new RENAME TO snapshots;
            CREATE VIEW usable_snapshots AS
              SELECT * FROM snapshots WHERE complete = 1 AND taken_at IS NOT NULL;";

        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();

        // The database as it is today, with one snapshot and one member in it.
        let first = Migrations::new(vec![M::up(include_str!("sql/001_initial.sql"))]);
        migrate(&mut conn, &first).unwrap();
        conn.execute_batch(
            "INSERT INTO users (pk, username, first_seen, last_seen) VALUES (1, 'someone', 0, 0);
             INSERT INTO accounts (pk, is_self, added_at) VALUES (1, 1, 0);
             INSERT INTO snapshots (id, account_pk, kind, source, started_at, taken_at, complete)
               VALUES (1, 1, 'followers', 'live', 0, 0, 1);
             INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (1, 1, 0);",
        )
        .unwrap();

        let second = Migrations::new(vec![
            M::up(include_str!("sql/001_initial.sql")),
            M::up(rebuild_snapshots),
        ]);
        migrate(&mut conn, &second).unwrap();

        let members: i64 = conn
            .query_row("SELECT count(*) FROM snapshot_members", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            members, 1,
            "the rebuild cascaded the members away and left the snapshot claiming to be complete"
        );

        // And the keys are live again afterwards, or the next write is unguarded.
        let orphan = conn.execute(
            "INSERT INTO snapshot_members (snapshot_id, user_pk, ordinal) VALUES (999, 999, 0)",
            [],
        );
        assert!(orphan.is_err(), "migrate left foreign_keys off");
    }

    #[test]
    fn the_file_uses_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Store::open_at(&tmp.path().join("test.db")).unwrap();
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    /// Opening an account that has no database leaves nothing behind.
    #[test]
    fn an_account_without_a_database_is_not_given_one() {
        let tmp = tempfile::tempdir().unwrap();
        let account = crate::paths::AppPaths::rooted_at(tmp.path()).account(Pk::new(42));

        assert!(Store::open_existing(&account).unwrap().is_none());
        assert!(!account.dir().exists(), "nothing was created for it");

        drop(Store::open(&account).unwrap());
        assert!(Store::open_existing(&account).unwrap().is_some());
    }

    /// Reading for a report changes nothing in the file: not a database at
    /// the current schema, which refuses a write through it, and not one an
    /// older snob left, which is read migrated without being migrated.
    #[test]
    fn a_report_reads_the_database_and_leaves_it_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let account = crate::paths::AppPaths::rooted_at(tmp.path()).account(Pk::new(42));
        assert!(Store::read_existing(&account).unwrap().is_none());
        assert!(!account.dir().exists(), "nothing was created for it");

        // Two migrations short of the current schema, as an older snob left it.
        account.ensure_dirs().unwrap();
        let path = account.db_file();
        {
            let mut conn = Connection::open(&path).unwrap();
            configure(&conn).unwrap();
            let older = Migrations::new(
                migrations::CHAIN[..migrations::COUNT - 2]
                    .iter()
                    .map(|sql| rusqlite_migration::M::up(sql))
                    .collect(),
            );
            migrate(&mut conn, &older).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let read = Store::read_existing(&account).unwrap().unwrap();
        assert_eq!(
            user_version(read.conn()).unwrap() as usize,
            migrations::COUNT,
            "the report reads the current schema"
        );
        drop(read);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the file was changed"
        );

        drop(Store::open(&account).unwrap());
        let read = Store::read_existing(&account).unwrap().unwrap();
        assert!(
            read.remember("key", "value").is_err(),
            "a write went through a report's connection"
        );
    }

    /// A database from a newer snob says so, rather than letting the migration
    /// runner say it in its own words; `StoreError::SchemaFromNewerSnob` says
    /// why that matters.
    #[test]
    fn a_database_from_a_newer_snob_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ahead.db");

        // A real database, then wound forward the way a newer build would leave
        // it. The view has to be there or the *older*-schema check fires first.
        Store::open_at(&path).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", (migrations::COUNT + 3) as i64)
                .unwrap();
        }

        let Err(error) = Store::open_at(&path) else {
            panic!("a database from a newer snob should be refused");
        };
        assert!(
            matches!(error, StoreError::SchemaFromNewerSnob { .. }),
            "{error}"
        );
        let said = error.to_string();
        assert!(said.contains("newer version of snob"), "{said}");
        assert!(
            !said.contains("Delete"),
            "deleting it throws away every capture: {said}"
        );
    }
}
