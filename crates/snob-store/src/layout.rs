//! The move from the single-account layout — one `snob.db` and one `session`
//! in the data directory — to one directory and one session entry per
//! account.
//!
//! **Automatic, and finished by whichever run comes next.** [`pending`] is a
//! look at a few names in the data directory, cheap enough for every command;
//! [`settle`] does the move under a lock, and every step it takes is one the
//! next run can find half done and complete:
//!
//! 1. Whose data it is: the session's account, or — with no session left —
//!    the one account directory there is, or nobody's. A session file that
//!    does not read is no session, and is set aside as `*.unreadable`.
//! 2. The database's log is folded into it, which fails while an older snob
//!    is still using it.
//! 3. The database goes into a staging directory beside its destination, the
//!    database file itself last, and the staging directory then takes the
//!    destination's name. A destination that has a database already keeps it:
//!    what was on its way is set aside as `conflict-<ms>` instead.
//! 4. The account's directory is made, so the account is found by it from
//!    here on; the session is saved under it and read back, and only a copy
//!    that reads back the same lets the old ones be deleted.
//! 5. A schema-1 `watch.toml` is copied aside and rewritten to name that
//!    account as the viewer of every entry.
//! 6. The registry is written, which is what says the move is done.
//!
//! A database nobody could be named for waits in `unclaimed` until a login
//! [`adopt`]s it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

use crate::config::{self, ConfigError};
use crate::paths::{AppPaths, PathError, create_private_dir, rename_patiently};
use crate::registry::{Registry, RegistryError};
use crate::secrets::{Kind, SecretStore, SecretsError, Stored};
use snob_core::Pk;

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("could not {what} {path}: {source}")]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the database at {path} is still in use, most likely by an older snob such as a \
         scheduled \"snob watch\".\nStop it, then run the command again."
    )]
    InUse { path: PathBuf },
    #[error("could not read the database at {path}: {source}")]
    Database {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "{from} was to be moved to {to}, where there is one already.\nMove one of the two \
         away by hand."
    )]
    Occupied { from: PathBuf, to: PathBuf },
    #[error(
        "the session saved for account {pk} did not read back as the one stored before; \
         the earlier copy was kept"
    )]
    SessionChanged { pk: Pk },
    #[error(transparent)]
    Secrets(#[from] SecretsError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Paths(#[from] PathError),
}

/// What a move did that somebody should be told.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// Where a database was set aside because its destination had one.
    pub parked: Option<PathBuf>,
    /// Why the old session did not read, when it was there and did not.
    pub unreadable: Option<String>,
}

/// The database's files, the database itself last: while it is still in the
/// data directory the move is not finished, whatever else has gone.
const DATABASE: [&str; 3] = ["snob.db-wal", "snob.db-shm", "snob.db"];

/// Beside a destination, the name of the directory a database waits in.
const STAGING: &str = "moving";

/// How long an older snob's transaction is waited out before it is reported.
const BUSY: Duration = Duration::from_secs(2);

/// Whether [`settle`] has anything to do.
///
/// The registry is written last, so its absence is also a move that stopped
/// after the old session was deleted. The rest is what an older snob may have
/// left since.
pub fn pending(paths: &AppPaths) -> bool {
    !paths.registry_file().exists()
        || paths.legacy_db_file().exists()
        || paths.legacy_session_file().exists()
        || !stagings(paths).is_empty()
}

/// Moves the single-account layout to the per-account one. Safe to run at
/// any point of an earlier run that stopped, and a no-op once done.
pub fn settle(paths: &AppPaths, secrets: &SecretStore) -> Result<Settled, LayoutError> {
    paths.ensure_dirs()?;
    let _lock = lock(paths)?;
    // Under the lock: another process may have finished it a moment ago.
    if !pending(paths) {
        return Ok(Settled::default());
    }

    // Read before anything is written, and a keyring that refuses stops the
    // move: going on without its session would delete it below. A copy that
    // is there and does not read stops nothing: it is no session, and would
    // otherwise stop every command, `login` and `logout` among them.
    let legacy = secrets.legacy_session();
    let mut unreadable = None;
    let found = match legacy.load_located_strictly() {
        Err(e @ SecretsError::KeyringUnreadable(_)) => return Err(e.into()),
        Err(e) => {
            set_aside_unreadable(paths)?;
            unreadable = Some(e.to_string());
            None
        }
        Ok(found) => found,
    };
    let pk = match &found {
        Some((session, _)) => Some(session.ds_user_id),
        None => lone_account(paths),
    };

    let parked = move_database(paths, pk)?;

    let session = match found {
        Some((session, backend)) => {
            // Made first: once the old copy is gone, a run that stops before
            // the registry is written finds the account by its directory.
            paths.account(session.ds_user_id).ensure_dirs()?;
            let account = secrets.session_of(&paths.account(session.ds_user_id));
            account.save_in(backend, &session)?;
            let back = account.load()?;
            if back.map(|s| s.fingerprint()) != Some(session.fingerprint()) {
                return Err(LayoutError::SessionChanged {
                    pk: session.ds_user_id,
                });
            }
            legacy.delete()?;
            Some(session)
        }
        // A run that stopped after deleting the old copy finds the new one,
        // and a keyring that will not hand it over stops the move rather than
        // leave the account out of the registry for good.
        None => match pk.map(|pk| {
            secrets
                .session_of(&paths.account(pk))
                .load_located_strictly()
        }) {
            Some(Err(e @ SecretsError::KeyringUnreadable(_))) => return Err(e.into()),
            Some(Ok(Some((session, _)))) => Some(session),
            _ => None,
        },
    };

    let account = session.map(|session| (session.ds_user_id, session.username));
    if let Some((pk, username)) = &account {
        name_the_viewer(paths, secrets, *pk, username.as_deref())?;
    }
    Registry::update(paths, |registry| {
        if let Some((pk, username)) = &account {
            if registry.get(*pk).is_none() {
                // No name is better than the id passed off as one.
                let username = username.clone().unwrap_or_default();
                registry.upsert(*pk, &username, snob_core::clock::now());
            }
            registry.active.get_or_insert(*pk);
        }
    })?;

    if let Some(parked) = &parked {
        tracing::warn!(to = %parked.display(), "a database was set aside rather than written over");
    }
    Ok(Settled { parked, unreadable })
}

/// Moves the old session file aside, as `session.json.unreadable`, so the
/// move is not pending on it and it is still there to look at. The old
/// keyring entry is left: nothing reads it, and `purge` removes it.
fn set_aside_unreadable(paths: &AppPaths) -> Result<(), LayoutError> {
    let file = paths.legacy_session_file();
    if file.exists() {
        let aside = file.with_extension("json.unreadable");
        tracing::warn!(to = %aside.display(), "the old session does not read, and was set aside");
        rename(&file, &aside)?;
    }
    Ok(())
}

/// Gives the database no account could be told for to `pk`, the account a
/// login has just signed in as. Returns whether it did.
///
/// Only when nothing in it says it is another account's, and only when `pk`
/// has no database of its own: one account's history is never mixed into
/// another's, nor written over. The move is [`settle`]'s own, so a run that
/// stops halfway is finished by the next command.
pub fn adopt(paths: &AppPaths, pk: Pk) -> Result<bool, LayoutError> {
    let unclaimed = paths.unclaimed_dir();
    let db = unclaimed.join("snob.db");
    if !db.exists() {
        return Ok(false);
    }
    let _lock = lock(paths)?;
    if !db.exists() || paths.account(pk).db_file().exists() {
        return Ok(false);
    }
    if recorded_owner(&db)?.is_some_and(|owner| owner != pk) {
        return Ok(false);
    }
    let staging = paths.account(pk).dir().with_extension(STAGING);
    rename(&unclaimed, &staging)?;
    land(paths, &staging)?;
    Ok(true)
}

/// Puts back what [`adopt`] gave `pk`, for a login that then did not happen:
/// an id nobody signed in as keeps nobody's history. Only while nothing has
/// waited in `unclaimed` since. Returns whether it did.
pub fn unadopt(paths: &AppPaths, pk: Pk) -> Result<bool, LayoutError> {
    let _lock = lock(paths)?;
    let account = paths.account(pk);
    if paths.unclaimed_dir().exists() || !account.db_file().exists() {
        return Ok(false);
    }
    let staging = paths.unclaimed_dir().with_extension(STAGING);
    create_private_dir(&staging)?;
    move_database_files(&account.dir(), &staging)?;
    land(paths, &staging)?;
    // Unless something else is in it, such as a session file.
    let _ = std::fs::remove_dir(account.dir());
    Ok(true)
}

/// The account a database says it belongs to, read without writing to it.
fn recorded_owner(db: &Path) -> Result<Option<Pk>, LayoutError> {
    let conn =
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            LayoutError::Database {
                path: db.to_path_buf(),
                source,
            }
        })?;
    Ok(crate::store::accounts::own(&conn)?)
}

/// Rewrites a schema-1 `watch.toml` so every entry names `pk`, the account it
/// was read as, keeping the old file beside it. A file that does not read is
/// left for the monitor to report: it must not stop every command here.
fn name_the_viewer(
    paths: &AppPaths,
    secrets: &SecretStore,
    pk: Pk,
    username: Option<&str>,
) -> Result<(), LayoutError> {
    let mut watch = match config::load(paths) {
        Ok(Some(watch)) if watch.schema == 1 => watch,
        Ok(_) => return Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "watch.toml was left as it is");
            return Ok(());
        }
    };
    let file = config::path(paths);
    let backup = file.with_extension("toml.schema1.bak");
    std::fs::copy(&file, &backup).map_err(|source| LayoutError::Io {
        what: "copy",
        path: file.clone(),
        source,
    })?;
    watch.upgrade(pk);
    // The note about the signing key is prose, and stays true only if the key
    // is still there.
    let signed = watch.webhook.is_some()
        && matches!(
            secrets.load_secret(Kind::WatchSigningKey),
            Ok(Stored::Found(_))
        );
    let text = config::template(&watch, signed, |viewer| {
        username.filter(|_| viewer == pk).map(str::to_string)
    });
    config::write(paths, &text)?;
    Ok(())
}

/// Held for the whole move, so two runs starting at once make it once.
fn lock(paths: &AppPaths) -> Result<File, LayoutError> {
    let path = paths.data_dir().join(".layout.lock");
    crate::paths::lock_file(&path).map_err(|source| LayoutError::Io {
        what: "lock",
        path,
        source,
    })
}

/// The account whose directory is the only one there, when there is exactly
/// one: whose a database is, once no session says.
fn lone_account(paths: &AppPaths) -> Option<Pk> {
    match paths.account_dirs().as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// The staging directories an earlier run left.
fn stagings(paths: &AppPaths) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(paths.accounts_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.extension().is_some_and(|e| e == STAGING))
        .collect()
}

/// Moves the data directory's database under `pk`, or into `unclaimed`
/// without one, and lands every staging directory there is.
fn move_database(paths: &AppPaths, pk: Option<Pk>) -> Result<Option<PathBuf>, LayoutError> {
    if paths.legacy_db_file().exists() {
        checkpoint(&paths.legacy_db_file())?;
        let destination = pk.map_or_else(|| paths.unclaimed_dir(), |pk| paths.account(pk).dir());
        let staging = destination.with_extension(STAGING);
        create_private_dir(&paths.accounts_dir())?;
        create_private_dir(&staging)?;
        move_database_files(paths.data_dir(), &staging)?;
    }

    let mut parked = None;
    for staging in stagings(paths) {
        if let Some(to) = land(paths, &staging)? {
            parked = Some(to);
        }
    }
    Ok(parked)
}

/// Folds the database's log into it, so the file moved is the whole of it.
///
/// Busy means another connection is inside a transaction: an older snob, whose
/// writes would go on landing in the log beside a database that has gone. On
/// Windows one that merely has it open would stop the move halfway, so that
/// is asked too.
fn checkpoint(db: &Path) -> Result<(), LayoutError> {
    let failed = |source| LayoutError::Database {
        path: db.to_path_buf(),
        source,
    };
    let conn =
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(failed)?;
    conn.busy_timeout(BUSY).map_err(failed)?;
    let busy: i64 = match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)) {
        Ok(busy) => busy,
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) => 1,
        Err(e) => return Err(failed(e)),
    };
    conn.close().map_err(|(_, e)| failed(e))?;
    if busy != 0 || held_elsewhere(db) {
        return Err(LayoutError::InUse {
            path: db.to_path_buf(),
        });
    }
    Ok(())
}

/// Whether another process has the file open, which on Windows keeps it
/// from being moved: opening it with no sharing fails at once if so.
#[cfg(windows)]
fn held_elsewhere(file: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt as _;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(file)
        .is_err_and(|e| e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

#[cfg(not(windows))]
fn held_elsewhere(_: &Path) -> bool {
    false
}

/// Puts a staging directory's database where it was going, or sets it aside
/// when a database is there already. Returns where it was set aside.
fn land(paths: &AppPaths, staging: &Path) -> Result<Option<PathBuf>, LayoutError> {
    let destination = staging.with_extension("");
    let occupied = destination.join("snob.db").exists()
        || DATABASE
            .iter()
            .any(|name| staging.join(name).exists() && destination.join(name).exists());
    if occupied {
        let parked = paths
            .accounts_dir()
            .join(format!("conflict-{}", snob_core::clock::now_ms().get()));
        rename(staging, &parked)?;
        return Ok(Some(parked));
    }

    if !destination.exists() {
        rename(staging, &destination)?;
        return Ok(None);
    }
    // The account's directory is there already, holding its session file.
    move_database_files(staging, &destination)?;
    std::fs::remove_dir(staging).map_err(|source| LayoutError::Io {
        what: "remove",
        path: staging.to_path_buf(),
        source,
    })?;
    Ok(None)
}

/// Moves the database's files that are in `from` to the same names in `to`,
/// in [`DATABASE`]'s order, stopping at the first that will not go.
fn move_database_files(from: &Path, to: &Path) -> Result<(), LayoutError> {
    for name in DATABASE {
        let file = from.join(name);
        if file.exists() {
            move_new(&file, &to.join(name))?;
        }
    }
    Ok(())
}

/// A rename that never replaces what is at the destination.
fn move_new(from: &Path, to: &Path) -> Result<(), LayoutError> {
    if to.exists() {
        return Err(LayoutError::Occupied {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
    }
    rename(from, to)
}

fn rename(from: &Path, to: &Path) -> Result<(), LayoutError> {
    tracing::debug!(from = %from.display(), to = %to.display(), "moving");
    rename_patiently(from, to).map_err(|source| LayoutError::Io {
        what: "move",
        path: from.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Backend;
    use snob_core::session::{Session, SessionOrigin};

    /// A move that stopped once the old keyring entry was gone, with no
    /// database to have made the account's directory, still finds the account
    /// when the registry was never written.
    #[test]
    fn a_keyring_session_moved_without_a_database_is_found_again() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let secrets = SecretStore::new(paths.clone(), false)
            .with_service(&format!("snob-ig-test-layout-unit-{}", std::process::id()));
        if !matches!(secrets.probe_writable(), Ok(Backend::Keyring)) {
            return;
        }
        let session = Session::from_sessionid(
            "71234567890%3AAbCdEfGhIjKl%3A20",
            "Mozilla/5.0",
            SessionOrigin::Paste,
        )
        .unwrap();
        let pk = session.ds_user_id;
        secrets
            .legacy_session()
            .save_in(Backend::Keyring, &session)
            .unwrap();

        settle(&paths, &secrets).unwrap();
        std::fs::remove_file(paths.registry_file()).unwrap();
        let again = settle(&paths, &secrets);

        let registry = Registry::load(&paths).unwrap();
        secrets.delete_all(&[pk]).unwrap();
        again.unwrap();
        assert_eq!(registry.active, Some(pk));
        assert_eq!(registry.get(pk).map(|a| a.username.as_str()), Some(""));
    }
}
