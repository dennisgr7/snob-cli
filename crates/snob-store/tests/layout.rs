//! The move from the single-account layout, from the start and from every
//! point a run can stop at.
//!
//! A stopped run is the files it leaves, so each test lays those out by hand
//! and checks that the next run finishes the move without losing anything.

use std::path::Path;

use snob_core::Pk;
use snob_core::session::{Session, SessionOrigin};
use snob_store::config::{self, WatchConfig};
use snob_store::layout::{self, LayoutError, Settled};
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::SecretStore;
use snob_store::store::Store;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const SID: &str = "71234567890%3AAbCdEfGhIjKl%3A20";
/// The account `SID` belongs to.
const ME: Pk = Pk::new(71_234_567_890);

/// A data directory and a store whose keyring entries are this test's own.
struct Machine {
    _tmp: tempfile::TempDir,
    paths: AppPaths,
    secrets: SecretStore,
    _keyring: std::sync::MutexGuard<'static, ()>,
}

impl Machine {
    fn me(&self) -> AccountPaths {
        self.paths.account(ME)
    }

    /// The single-account layout's database.
    fn old_db(&self) -> std::path::PathBuf {
        self.paths.data_dir().join("snob.db")
    }

    /// The single-account layout's session file.
    fn old_session_file(&self) -> std::path::PathBuf {
        self.paths.data_dir().join("session.json")
    }

    fn settle(&self) -> Result<Settled, LayoutError> {
        layout::settle(&self.paths, &self.secrets)
    }

    /// The single-account layout's session file, holding `session()`.
    ///
    /// Written as the account's own and moved: the two files are one format.
    fn old_session_at(&self, file: &Path) {
        self.secrets
            .session_of(&self.me())
            .save(&session())
            .unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::rename(self.me().session_file(), file).unwrap();
        // Unless something else is in it already.
        let _ = std::fs::remove_dir(self.me().dir());
    }

    fn old_layout(&self) {
        database(&self.old_db(), "old");
        self.old_session_at(&self.old_session_file());
    }

    /// The account's session, as the next command reads it.
    fn my_session(&self) -> Option<Session> {
        self.secrets.session_of(&self.me()).load().unwrap()
    }

    fn registry(&self) -> Registry {
        Registry::load(&self.paths).unwrap()
    }

    /// Everything that says the move is done and nothing is left over.
    fn assert_settled(&self) {
        assert!(!layout::pending(&self.paths));
        assert!(!self.old_db().exists());
        assert!(!self.old_session_file().exists());
        let registry = self.registry();
        assert_eq!(registry.active, Some(ME));
        assert_eq!(registry.get(ME).unwrap().username, "me");
        assert_eq!(
            self.my_session().unwrap().sessionid.expose(),
            session().sessionid.expose()
        );
        assert_eq!(marker(&self.me().db_file()).as_deref(), Some("old"));
    }
}

fn machine() -> Machine {
    // The keyring belongs to the operating system, and writing it from
    // several threads at once is not reliable.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let held = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::rooted_at(tmp.path());
    let secrets = SecretStore::new(paths.clone(), true)
        .with_service(&format!("snob-ig-test-layout-{}-{n}", std::process::id()));
    paths.ensure_dirs().unwrap();
    Machine {
        _tmp: tmp,
        paths,
        secrets,
        _keyring: held,
    }
}

fn session() -> Session {
    let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    session.username = Some("me".into());
    session
}

/// A database at `path` that remembers `marker`.
fn database(path: &Path, marker: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    Store::open_at(path)
        .unwrap()
        .remember("marker", marker)
        .unwrap();
}

fn marker(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    Store::open_at(path).unwrap().remembered("marker").unwrap()
}

#[test]
fn the_old_layout_moves_under_its_account() {
    let m = machine();
    m.old_layout();
    assert!(layout::pending(&m.paths));

    assert_eq!(m.settle().unwrap(), Settled::default());

    m.assert_settled();
}

#[test]
fn a_second_run_finds_nothing_to_do() {
    let m = machine();
    m.old_layout();
    m.settle().unwrap();
    let before = std::fs::read_to_string(m.paths.registry_file()).unwrap();

    assert_eq!(m.settle().unwrap(), Settled::default());

    assert_eq!(
        std::fs::read_to_string(m.paths.registry_file()).unwrap(),
        before
    );
    m.assert_settled();
}

/// Stopped after the staging directory was made, before the database went in.
#[test]
fn a_run_stopped_while_staging_is_finished() {
    let m = machine();
    m.old_layout();
    std::fs::create_dir_all(m.me().dir().with_extension("moving")).unwrap();
    assert!(layout::pending(&m.paths));

    m.settle().unwrap();

    m.assert_settled();
    assert!(!m.me().dir().with_extension("moving").exists());
}

/// Stopped with the database staged and not yet landed.
#[test]
fn a_run_stopped_before_landing_is_finished() {
    let m = machine();
    m.old_layout();
    let staging = m.me().dir().with_extension("moving");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::rename(m.old_db(), staging.join("snob.db")).unwrap();
    assert!(layout::pending(&m.paths));

    m.settle().unwrap();

    m.assert_settled();
    assert!(!staging.exists());
}

/// Stopped with the database moved and the session not.
#[test]
fn a_run_stopped_before_the_session_is_finished() {
    let m = machine();
    database(&m.me().db_file(), "old");
    m.old_session_at(&m.old_session_file());

    m.settle().unwrap();

    m.assert_settled();
}

/// Stopped with the session saved under its account and the old copy still
/// there.
#[test]
fn a_run_stopped_before_the_old_session_was_deleted_is_finished() {
    let m = machine();
    database(&m.me().db_file(), "old");
    m.old_session_at(&m.old_session_file());
    m.secrets.session_of(&m.me()).save(&session()).unwrap();

    m.settle().unwrap();

    m.assert_settled();
}

/// Stopped with everything moved and no registry written: the account is
/// found by its directory, and its name in its session.
#[test]
fn a_run_stopped_before_the_registry_is_finished() {
    let m = machine();
    database(&m.me().db_file(), "old");
    m.secrets.session_of(&m.me()).save(&session()).unwrap();
    assert!(layout::pending(&m.paths));

    m.settle().unwrap();

    m.assert_settled();
}

/// With no session to say whose it is, the database waits for a login to
/// claim it, and nobody is registered.
#[test]
fn with_no_session_the_database_waits_unclaimed() {
    let m = machine();
    database(&m.old_db(), "old");

    let settled = m.settle().unwrap();

    assert_eq!(settled, Settled::default());
    assert_eq!(
        marker(&m.paths.unclaimed_dir().join("snob.db")).as_deref(),
        Some("old")
    );
    assert!(!m.old_db().exists());
    assert_eq!(m.registry(), Registry::default());
    assert!(!layout::pending(&m.paths));
}

/// With no session, the one account there is takes the database, beside the
/// session file already in its directory.
#[test]
fn with_no_session_the_one_account_there_is_takes_it() {
    let m = machine();
    database(&m.old_db(), "old");
    m.secrets.session_of(&m.me()).save(&session()).unwrap();

    m.settle().unwrap();

    m.assert_settled();
    assert!(m.me().session_file().exists());
}

/// A session that never learned its username is registered with none, not
/// with its id passed off as one.
#[test]
fn a_session_with_no_username_is_registered_with_none() {
    let m = machine();
    database(&m.old_db(), "old");
    let mut nameless = session();
    nameless.username = None;
    m.secrets.session_of(&m.me()).save(&nameless).unwrap();
    std::fs::rename(m.me().session_file(), m.old_session_file()).unwrap();

    m.settle().unwrap();

    let registry = m.registry();
    assert_eq!(registry.active, Some(ME));
    assert_eq!(registry.get(ME).unwrap().username, "");
}

/// An old session that does not read is no session: the move finishes, the
/// database waits unclaimed, and the file is set aside rather than stopping
/// every command.
#[test]
fn an_old_session_that_does_not_read_is_set_aside() {
    let m = machine();
    database(&m.old_db(), "old");
    std::fs::write(m.old_session_file(), b"not a session").unwrap();

    let settled = m.settle().unwrap();

    assert!(settled.unreadable.is_some(), "{settled:?}");
    assert!(!layout::pending(&m.paths));
    assert!(!m.old_session_file().exists());
    assert_eq!(
        std::fs::read(m.old_session_file().with_extension("json.unreadable")).unwrap(),
        b"not a session"
    );
    assert_eq!(
        marker(&m.paths.unclaimed_dir().join("snob.db")).as_deref(),
        Some("old")
    );
    assert_eq!(m.registry(), Registry::default());
}

/// A database an older snob made after the move never replaces the
/// account's: it is set aside, and the account's is left as it was.
#[test]
fn a_database_is_never_written_over() {
    let m = machine();
    m.old_layout();
    m.settle().unwrap();
    database(&m.old_db(), "newer");
    assert!(layout::pending(&m.paths));

    let settled = m.settle().unwrap();

    let parked = settled.parked.expect("the newer database is set aside");
    assert_eq!(marker(&parked.join("snob.db")).as_deref(), Some("newer"));
    m.assert_settled();
}

/// A database another connection is in the middle of using is left where it
/// is, and so is everything else.
#[test]
fn a_database_in_use_stops_the_move_before_anything_moves() {
    let m = machine();
    m.old_layout();
    let db = m.old_db();
    let writer = Store::open_at(&db).unwrap();
    let reader = rusqlite::Connection::open(&db).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM meta", [], |row| row.get(0))
        .unwrap();
    writer.remember("marker", "written meanwhile").unwrap();

    let refused = m.settle();

    assert!(
        matches!(refused, Err(LayoutError::InUse { .. })),
        "{refused:?}"
    );
    assert!(db.exists());
    assert!(m.old_session_file().exists());
    assert!(!m.me().dir().exists());
    assert!(!m.me().dir().with_extension("moving").exists());
    assert!(!m.paths.registry_file().exists());
    assert!(layout::pending(&m.paths));
}

/// A machine snob has never run on is settled at once, with an empty
/// registry that says so.
#[test]
fn a_machine_with_nothing_on_it_is_settled_at_once() {
    let m = machine();
    assert!(layout::pending(&m.paths));

    assert_eq!(m.settle().unwrap(), Settled::default());

    assert_eq!(m.registry(), Registry::default());
    assert!(!layout::pending(&m.paths));
}

/// On Windows a database another process merely has open cannot be moved, and
/// that is said before anything is.
#[cfg(windows)]
#[test]
fn a_database_held_open_stops_the_move_before_anything_moves() {
    let m = machine();
    m.old_layout();
    let _idle = Store::open_at(&m.old_db()).unwrap();

    let refused = m.settle();

    assert!(
        matches!(refused, Err(LayoutError::InUse { .. })),
        "{refused:?}"
    );
    assert!(m.old_db().exists());
    assert!(m.old_session_file().exists());
    assert!(!m.me().dir().with_extension("moving").exists());
    assert!(!m.paths.registry_file().exists());
}

/// A monitor configured before there were several accounts.
const WATCH_1: &str = "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"friend\"\n\
                       [account.consent]\nagreed_at = 1700000000\n";

impl Machine {
    fn watch_file(&self) -> std::path::PathBuf {
        config::path(&self.paths)
    }

    fn backup(&self) -> std::path::PathBuf {
        self.watch_file().with_extension("toml.schema1.bak")
    }

    fn write_watch(&self, text: &str) {
        config::write(&self.paths, text).unwrap();
    }

    fn watch(&self) -> WatchConfig {
        config::load(&self.paths).unwrap().unwrap()
    }
}

/// Every entry of the old file was read as the one account there was, so each
/// names it now, and the old file is kept beside it.
#[test]
fn the_watch_file_names_the_account_it_was_read_as() {
    let m = machine();
    m.old_layout();
    m.write_watch(WATCH_1);

    m.settle().unwrap();

    m.assert_settled();
    let watch = m.watch();
    assert_eq!(watch.schema, config::SCHEMA);
    assert_eq!(watch.every, Some(std::time::Duration::from_secs(21_600)));
    assert_eq!(watch.accounts.len(), 1);
    assert_eq!(watch.accounts[0].target, "friend");
    assert_eq!(watch.accounts[0].viewer, Some(ME));
    assert!(watch.accounts[0].consent.is_some());
    let text = std::fs::read_to_string(m.watch_file()).unwrap();
    assert!(text.contains(&format!("viewer = {ME}  # @me")), "{text}");
    assert_eq!(std::fs::read_to_string(m.backup()).unwrap(), WATCH_1);
}

/// An empty list meant the account's own lists, and now says so.
#[test]
fn an_empty_watch_list_becomes_the_accounts_own() {
    let m = machine();
    m.old_layout();
    m.write_watch("schema = 1\nevery = \"6h\"\n");

    m.settle().unwrap();

    let watch = m.watch();
    assert_eq!(watch.accounts.len(), 1);
    assert!(watch.accounts[0].is_own());
    assert_eq!(watch.accounts[0].viewer, Some(ME));
}

/// Stopped after the old file was copied aside and before the new one was
/// written.
#[test]
fn a_run_stopped_after_the_watch_backup_is_finished() {
    let m = machine();
    m.old_layout();
    m.write_watch(WATCH_1);
    std::fs::copy(m.watch_file(), m.backup()).unwrap();

    m.settle().unwrap();

    assert_eq!(m.watch().accounts[0].viewer, Some(ME));
    assert_eq!(std::fs::read_to_string(m.backup()).unwrap(), WATCH_1);
}

/// With no account to name, and with a file already of this schema, the file
/// is left as it is: an entry without a viewer is read as the account in use.
#[test]
fn a_watch_file_is_left_alone_when_there_is_nothing_to_name() {
    let m = machine();
    database(&m.old_db(), "old");
    m.write_watch(WATCH_1);
    m.settle().unwrap();
    assert_eq!(std::fs::read_to_string(m.watch_file()).unwrap(), WATCH_1);
    assert!(!m.backup().exists());
    drop(m);

    let m = machine();
    m.old_layout();
    let current = "schema = 2\nevery = \"6h\"\n";
    m.write_watch(current);
    m.settle().unwrap();
    assert_eq!(std::fs::read_to_string(m.watch_file()).unwrap(), current);
    assert!(!m.backup().exists());
}

/// A file that does not read is the monitor's to report, and does not stop
/// every other command from moving the data.
#[test]
fn a_watch_file_that_does_not_read_does_not_stop_the_move() {
    let m = machine();
    m.old_layout();
    let broken = "schema = 1\nevry = \"6h\"\n";
    m.write_watch(broken);

    m.settle().unwrap();

    m.assert_settled();
    assert_eq!(std::fs::read_to_string(m.watch_file()).unwrap(), broken);
    assert!(!m.backup().exists());
}

/// A database that waited unclaimed, recorded as `owner`'s when there is one.
fn unclaimed(m: &Machine, owner: Option<Pk>) -> std::path::PathBuf {
    let db = m.paths.unclaimed_dir().join("snob.db");
    database(&db, "old");
    if let Some(owner) = owner {
        Store::open_at(&db)
            .unwrap()
            .conn()
            .execute_batch(&format!(
                "INSERT INTO users (pk, username, first_seen, last_seen) VALUES ({owner}, 'x', 1, 1);
                 INSERT INTO accounts (pk, is_self, added_at) VALUES ({owner}, 1, 1);"
            ))
            .unwrap();
    }
    db
}

/// The first account to sign in takes a database nobody could be named for,
/// and one it already says is its own.
#[test]
fn a_login_takes_the_unclaimed_database() {
    for owner in [None, Some(ME)] {
        let m = machine();
        unclaimed(&m, owner);
        // The account's directory may be there already, holding its session.
        m.secrets.session_of(&m.me()).save(&session()).unwrap();

        assert!(layout::adopt(&m.paths, ME).unwrap(), "{owner:?}");

        assert_eq!(marker(&m.me().db_file()).as_deref(), Some("old"));
        assert!(m.me().session_file().exists());
        assert!(!m.paths.unclaimed_dir().exists());
        assert!(
            !layout::adopt(&m.paths, ME).unwrap(),
            "there is nothing left"
        );
    }
}

/// Another account's history, or a database the account already has, is
/// never what a login takes.
#[test]
fn a_login_leaves_what_is_not_its_to_take() {
    let m = machine();
    let db = unclaimed(&m, Some(Pk::new(5)));
    assert!(!layout::adopt(&m.paths, ME).unwrap());
    assert!(db.exists() && !m.me().db_file().exists());
    drop(m);

    let m = machine();
    let db = unclaimed(&m, None);
    database(&m.me().db_file(), "mine");
    assert!(!layout::adopt(&m.paths, ME).unwrap());
    assert!(db.exists());
    assert_eq!(marker(&m.me().db_file()).as_deref(), Some("mine"));
}

/// A login that did not happen gives back what it took, and leaves no
/// directory behind for an id nobody signed in as.
#[test]
fn a_login_that_did_not_happen_gives_the_database_back() {
    let m = machine();
    let db = unclaimed(&m, None);
    assert!(layout::adopt(&m.paths, ME).unwrap());

    assert!(layout::unadopt(&m.paths, ME).unwrap());

    assert_eq!(marker(&db).as_deref(), Some("old"));
    assert!(!m.me().dir().exists());
    assert!(!m.paths.unclaimed_dir().with_extension("moving").exists());
    assert!(
        !layout::unadopt(&m.paths, ME).unwrap(),
        "there is nothing left to give back"
    );
}
