//! Where the tool keeps its data, following each platform's conventions.
//!
//! **These are per user, never per directory.** Everything here comes from
//! `directories`, which reads the user's profile, so `snob` run from two
//! different folders is the same session, the same database and the same cache.
//! The only thing the working directory decides is where an exported file lands
//! when no `-o` was given, which is what any command-line tool does.
//!
//! Data goes to the **local** directory, not the one that roams with the user
//! profile. On Windows that is the difference between Roaming and Local, and it
//! matters here: the database uses WAL, and WAL on a directory synced by
//! OneDrive or a roaming profile is a documented cause of SQLite corruption. It
//! also keeps megabytes of snapshots out of the roaming profile. Configuration
//! stays in the syncable directory, where it belongs.
//!
//! On Linux and macOS both paths are the same, so the distinction only shows on
//! Windows.

use std::path::{Path, PathBuf};

use thiserror::Error;

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "snob-ig";

#[derive(Debug, Error)]
pub enum PathError {
    #[error("could not determine the user's data directory")]
    NoHome,
    #[error("could not create the directory {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The directory exists and could not be limited to this account.
    ///
    /// Its own variant rather than a [`PathError::Create`], because the
    /// directory was created perfectly well and blaming the creation sends
    /// somebody looking in the wrong place. What failed is the half that makes
    /// it private, and that is the half worth naming: the database inside it is
    /// the whole follower history in the clear.
    #[error("could not restrict {path} to your account: {detail}")]
    NotPrivate { path: PathBuf, detail: String },
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    config: PathBuf,
    data: PathBuf,
    /// The data directory that roams with the profile, on Windows only.
    /// Nothing reads or writes it; [`Self::owned_dirs`] lists it because a
    /// build older than the first release may have left a session there, and
    /// `purge` leaves nothing behind.
    legacy_data: Option<PathBuf>,
    /// Set by [`Self::rooted_at`], and the reason [`Self::stories_root`] is not
    /// simply the system temporary directory.
    ///
    /// Every other location this type hands out is already under the root a
    /// test or `--sandbox-root` gave it. The scratch directory is the one that
    /// would otherwise escape, because it is the one that comes from the
    /// environment rather than from `directories`. "Every file this run
    /// touches is under one directory" is a promise the sandbox seam makes in
    /// `AGENTS.md`, and a hole in it is a test writing into the real
    /// `%TEMP%`.
    sandbox: Option<PathBuf>,
}

impl AppPaths {
    pub fn discover() -> Result<Self, PathError> {
        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .ok_or(PathError::NoHome)?;

        let data = dirs.data_local_dir().to_path_buf();
        let previous = dirs.data_dir().to_path_buf();

        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            legacy_data: (previous != data).then_some(previous),
            data,
            sandbox: None,
        })
    }

    /// For tests: puts every directory under one root.
    pub fn rooted_at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config: root.join("config"),
            data: root.join("data"),
            // Always defined so that `purge` removing it can be tested on any
            // platform, not just Windows.
            legacy_data: Some(root.join("legacy-data")),
            sandbox: Some(root.to_path_buf()),
        }
    }

    /// Where configuration lives: `watch.toml`, written by `snob watch setup`.
    /// Created by what writes there, never by [`Self::ensure_dirs`].
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    /// The accounts' own directories, one per account snob has signed in as.
    pub fn accounts_dir(&self) -> PathBuf {
        self.data.join("accounts")
    }

    /// The accounts snob holds a session for, and which one is active.
    pub fn registry_file(&self) -> PathBuf {
        self.data.join("accounts.toml")
    }

    /// What every account shares: the push-backs any of them received.
    pub fn shared_db_file(&self) -> PathBuf {
        self.data.join("shared.db")
    }

    /// Where a database whose account could not be told goes, until a login
    /// claims it.
    pub fn unclaimed_dir(&self) -> PathBuf {
        self.accounts_dir().join("unclaimed")
    }

    /// The accounts that have a directory, named after their id, by id: the
    /// order a directory is read in differs between systems.
    pub fn account_dirs(&self) -> Vec<snob_core::Pk> {
        let Ok(entries) = std::fs::read_dir(self.accounts_dir()) else {
            return Vec::new();
        };
        let mut pks: Vec<snob_core::Pk> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        pks.sort();
        pks
    }

    /// Everything that belongs to one account.
    pub fn account(&self, pk: snob_core::Pk) -> AccountPaths {
        AccountPaths {
            app: self.clone(),
            pk,
        }
    }

    // The single-account layout, which the per-account one replaced. Only the
    // migration and the removal of every secret read them.

    /// The single-account layout's database.
    pub(crate) fn legacy_db_file(&self) -> PathBuf {
        self.data.join("snob.db")
    }

    /// The single-account layout's session file, used when the system keyring
    /// is unavailable.
    pub(crate) fn legacy_session_file(&self) -> PathBuf {
        self.data.join("session.json")
    }

    /// The directory every browser profile snob uses lives under. Never the
    /// user's real profile. `snob logout --all` removes every profile in it:
    /// each holds a live session.
    pub fn browser_profile(&self) -> PathBuf {
        self.data.join("browser-profile")
    }

    /// The browser profile of one account: the device its requests are sent
    /// from, and nobody else's.
    ///
    /// **One per account**, because one account is only ever sent from one
    /// browser and two accounts never share one: a profile shared by two
    /// would have to be emptied at every switch of account, throwing the
    /// device away each time, and a session coming back to it would come back
    /// from a browser Instagram had never seen.
    pub fn browser_profile_for(&self, pk: snob_core::Pk) -> PathBuf {
        self.browser_profile().join(pk.to_string())
    }

    /// What the browsers said about this machine, kept once for every profile:
    /// it is about the machine, not about any account.
    pub fn browser_hints_file(&self) -> PathBuf {
        self.data.join("browser-hints.json")
    }

    /// Where `snob stories --interactive` puts a story it is about to hand to
    /// the system viewer.
    ///
    /// **Under the operating system's temporary directory, one directory per
    /// process**, because the data directory buys nothing here, as measured on
    /// Windows 11:
    ///
    /// - Photos with the picture on screen and Media Player with the video
    ///   playing hold **no** handle on the file: Restart Manager reports
    ///   nobody, and `DeleteFileW` succeeds while the window is still up. A
    ///   viewer that opens without `FILE_SHARE_DELETE` exists, and nothing can
    ///   delete underneath it, but it is not the default.
    /// - `%TEMP%` and `%LOCALAPPDATA%` carry **identical** ACLs -- SYSTEM,
    ///   Administrators, the user -- both inherited. On Linux
    ///   `$XDG_RUNTIME_DIR` is better than either: 0700 *by specification*, on
    ///   tmpfs, and gone with the session.
    /// - Windows Search crawls `%LOCALAPPDATA%`, so the data directory is not
    ///   the more private place either.
    ///
    /// `std::env::temp_dir()` does the right thing on all three platforms, and
    /// on Linux [`runtime_dir`] prefers `$XDG_RUNTIME_DIR` when there is one.
    ///
    /// **The process id is in the name on purpose**, twice over: two `snob`
    /// runs must not share a directory one of them will delete, and a
    /// fixed-name directory in a shared temporary space is a name anybody can
    /// predict and create first.
    ///
    /// [`Self::owned_dirs`] carries the parent, so `snob purge` still reaches
    /// whatever a run left behind. The system's own cleaner is not a mechanism
    /// to lean on: the oldest thing in one Windows machine's `%TEMP%` had been
    /// there forty-nine days.
    pub fn story_scratch(&self) -> PathBuf {
        self.stories_root()
            .join(format!("run-{}", std::process::id()))
    }

    /// The parent of every [`Self::story_scratch`], which is what `purge`
    /// removes and what the age sweep walks.
    pub fn stories_root(&self) -> PathBuf {
        // A sandbox root replaces every other location a run touches, and this
        // is one of them: a test must not be able to reach into the real
        // temporary directory, and `--sandbox-root` promising "every file this
        // run touches" has to keep being true.
        match &self.sandbox {
            Some(root) => root.join("stories"),
            None => runtime_dir().join("snob-ig-stories"),
        }
    }

    /// Every directory this tool may have created, for `snob purge` to remove.
    ///
    /// Assembled here rather than by the command so that a directory added
    /// later cannot be forgotten by the one command whose whole job is to leave
    /// nothing behind -- the configuration directory included, which is where
    /// `watch.toml` goes.
    ///
    /// Deduplicated, because on macOS the configuration and data directories
    /// are the same path: listed twice, the second removal would fail on a
    /// directory the first one had already taken and be reported as a problem.
    pub fn owned_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let stories = self.stories_root();
        for dir in [
            Some(&self.data),
            Some(&self.config),
            self.legacy_data.as_ref(),
            // The one location outside the data directory this tool writes to,
            // here rather than left to the system's cleaner for the reason
            // [`Self::story_scratch`] gives.
            Some(&stories),
        ]
        .into_iter()
        .flatten()
        {
            if !dirs.contains(dir) {
                dirs.push(dir.clone());
            }
        }
        dirs
    }

    /// Creates what every run writes to: the data directory, and only that.
    ///
    /// The configuration directory is created by what writes configuration
    /// (`config::write`), because creating it for everybody leaves an empty
    /// folder in the roaming profile of every machine this runs on.
    pub fn ensure_dirs(&self) -> Result<(), PathError> {
        create_private_dir(&self.data)
    }
}

/// Where one account's own files are: its database, its session file and its
/// browser profile.
///
/// A type of its own so that a function that acts as an account says so in its
/// signature: the database and the session file are only reachable from here,
/// never from [`AppPaths`], so a caller cannot open one account's data while
/// holding nothing but the machine's. Everything that is not the account's is
/// the [`AppPaths`] it derefs to.
#[derive(Debug, Clone)]
pub struct AccountPaths {
    app: AppPaths,
    pk: snob_core::Pk,
}

impl AccountPaths {
    pub fn pk(&self) -> snob_core::Pk {
        self.pk
    }

    /// The account's directory, named after its id: a username can change,
    /// the id cannot.
    pub fn dir(&self) -> PathBuf {
        self.app.accounts_dir().join(self.pk.to_string())
    }

    /// Its lists, walks, monitor history, budget and cooldowns.
    pub fn db_file(&self) -> PathBuf {
        self.dir().join("snob.db")
    }

    /// Its session, when the system keyring is unavailable.
    pub fn session_file(&self) -> PathBuf {
        self.dir().join("session.json")
    }

    /// The browser profile its requests are sent from.
    pub fn browser_profile(&self) -> PathBuf {
        self.app.browser_profile_for(self.pk)
    }

    /// Creates the data directory and the account's, both private.
    pub fn ensure_dirs(&self) -> Result<(), PathError> {
        self.app.ensure_dirs()?;
        create_private_dir(&self.dir())
    }
}

impl std::ops::Deref for AccountPaths {
    type Target = AppPaths;

    fn deref(&self) -> &AppPaths {
        &self.app
    }
}

/// The folder name every path here is built from.
///
/// Exposed so that a caller recognizing one of our own directories compares
/// against the value `ProjectDirs` was given rather than against a copy of it.
/// `snob purge` needs exactly that, and a second spelling of this string is one
/// that stops matching the day the application is renamed — silently, because
/// the guard simply never fires again.
pub fn app_dir_name() -> &'static str {
    APPLICATION
}

/// Whether a directory is plausible as one of ours, and so may be deleted whole.
///
/// `snob purge` removes directories recursively, which is the only destructive
/// thing this tool does anywhere. It does not take the path on trust: a
/// `ProjectDirs` that resolved oddly — an empty `HOME`, a profile variable that
/// never expanded — turns "remove the data directory" into something far worse,
/// and the check that rules it out costs nothing. Anything this tool creates
/// sits at least two levels below the root and is never the home directory
/// itself.
pub fn is_safe_to_remove(dir: &Path) -> bool {
    // Rejects `/`, `C:\`, `/data` and `C:\data` alike.
    if dir.parent().is_none_or(|parent| parent.parent().is_none()) {
        return false;
    }

    match directories::BaseDirs::new() {
        Some(base) => dir != base.home_dir(),
        // With no home to compare against, the depth check above is all there
        // is — and it is the one that matters.
        None => true,
    }
}

/// Where a file that should not outlive the session goes.
///
/// `$XDG_RUNTIME_DIR` when there is one, and [`std::env::temp_dir`] otherwise.
/// The preference is Linux's alone in practice, and it is worth the three
/// lines: the XDG base directory specification requires that directory to be
/// owned by the user, `0700`, on a filesystem that is not shared, and **removed
/// when the session ends** — which is every property wanted here and none of
/// which `/tmp` promises. `std::env::temp_dir` reads `GetTempPath2W` on
/// Windows and `$TMPDIR` on macOS, both of which are already per-user.
///
/// The variable is checked for being an absolute path that exists, because it
/// arrives from the environment and a relative one would put somebody else's
/// photograph in the working directory.
fn runtime_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(runtime);
        if path.is_absolute() && path.is_dir() {
            return path;
        }
    }
    std::env::temp_dir()
}

/// Removes what earlier runs left in the scratch directory.
///
/// **The one mechanism here that does not depend on anything having gone
/// right.** Everything else — the viewer releasing the file, the session
/// reaching its own cleanup — is a thing that usually happens. A process killed
/// mid-view, a viewer that opened the file without sharing delete, a machine
/// restarted: none of those clean up after themselves, and on Windows the
/// temporary directory is not emptied on boot.
///
/// It walks by age rather than by process id, because a process id is reused
/// and a directory named after a run that ended last week may be named after a
/// run in progress today. `older_than` is deliberately generous: a browsing
/// session is minutes, so hours is far past anything live.
///
/// Every failure is ignored on purpose. This is housekeeping, and a run that
/// cannot tidy up after an earlier one still has a story to show.
pub fn sweep_old_scratch(root: &Path, older_than: std::time::Duration) {
    // Never walk through a link at the root. `read_dir` follows one, and the
    // root sits at a predictable name in a directory anybody can write to, so
    // "the root" could be a link into somebody's home with everything in it
    // older than the cutoff. A link is left alone here; creating the scratch
    // afterwards removes it as a link, the same as at the leaf.
    let Ok(found) = std::fs::symlink_metadata(root) else {
        return; // nothing has ever run, which is the common case
    };
    if !found.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > older_than)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Replaces `path` with `contents` by way of `temporary`: written whole with
/// `write_new_private`, then renamed over `path`, so a reader sees the old
/// file or the new one and never half of one. `fs::write` truncates first, so
/// a process that dies mid-write would leave a half-written file behind;
/// a rename is atomic on every platform snob runs on.
///
/// The error carries the path of the step that failed: `temporary` when the
/// write did, `path` when the rename did. The caller maps it into its own
/// vocabulary.
pub fn replace_private(
    path: &Path,
    temporary: &Path,
    contents: &[u8],
) -> Result<(), (PathBuf, std::io::Error)> {
    write_new_private(temporary, contents).map_err(|e| (temporary.to_path_buf(), e))?;
    std::fs::rename(temporary, path).map_err(|e| (path.to_path_buf(), e))
}

/// Opens `path`, creating it empty when it is not there, and takes an
/// exclusive lock on it that lasts until the returned file is dropped.
///
/// Bind the result to a name for as long as the lock has to hold: `let _ =`
/// drops the file, and the lock with it, at once.
pub fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

/// Creates a file only its owner can read, at exactly this name, and syncs
/// its contents to disk before returning.
///
/// The pieces, each load-bearing:
/// - anything left over from a failed run is removed first, which is what
///   makes `create_new` usable at a fixed name;
/// - `create_new`, not `create`: never open something that already exists,
///   never follow a link. `create(true)` would open an existing file with
///   whatever permissions it already had, since the mode only applies at
///   creation, and [`create_private_dir`] chmods a directory to 0700 without
///   removing anything already inside it, so a link planted while it was lax
///   outlives the tightening;
/// - on Unix the mode is set **at creation** -- a later chmod leaves a window
///   in which the file is readable by others;
/// - `sync_all` before returning: a file that is renamed into place with its
///   contents still in the page cache is not the protection against a
///   half-written file that writing to a temporary is for.
fn write_new_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let _ = std::fs::remove_file(path);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Creates the directory and restricts it to its owner.
///
/// On Unix it chmods 0700. On Windows the ACL inherited from `%LOCALAPPDATA%`
/// is not enough: on a machine whose profile ACL is not the default, nothing
/// inherited limits anything, and the database and the browser profile are the
/// entire follower history and a live session in the clear.
///
/// So on Windows this writes a DACL of its own, with
/// `PROTECTED_DACL_SECURITY_INFORMATION`, which is the flag that stops
/// inheritance rather than merely adding to it. One access-allowed ACE, for the
/// user this process is running as, and nothing else — the faithful reading of
/// 0700. Administrators and `SYSTEM` are deliberately not listed: on Unix root
/// is not in a 0700 mode either, and on Windows both hold the privileges that
/// let them take ownership regardless, so naming them would widen the written
/// rule without narrowing what anybody can actually reach.
///
/// **A failure is an error, not a warning**: a directory this could not
/// restrict is a directory holding the follower history where the check said
/// it would not be, and a caller that is told "fine" cannot act on it. It gets
/// its own variant so the sentence names what really went wrong instead of
/// blaming the creation.
pub fn create_private_dir(dir: &Path) -> Result<(), PathError> {
    std::fs::create_dir_all(dir).map_err(|source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    })?;
    restrict_to_this_account(dir)
}

/// Creates and restricts a directory at a name somebody else can predict,
/// keeping a real directory that is already there and never following a link.
///
/// For the scratch **root** under the world-writable temporary directory --
/// the parent every [`create_fresh_private_dir`] leaf sits in, and the
/// directory [`sweep_old_scratch`] walks with `remove_dir_all`.
/// [`create_private_dir`] cannot be trusted with that name: `create_dir_all`
/// answers `Ok` over a planted link, and [`restrict_to_this_account`] then
/// lands the `0700` on **whatever the link points at** -- so whoever planted
/// it chooses which directory of this account's the age sweep then deletes
/// out of. A link or a stray file found at the name is removed as itself --
/// the same recipe as at the leaf, through [`remove_entry`] on the one reading
/// of it -- while a real directory is kept, because it holds other runs'
/// scratch. A real directory that belongs to somebody else fails in
/// `restrict_to_this_account`, which is the refusal wanted.
fn create_private_root(dir: &Path) -> Result<(), PathError> {
    let create = |source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    };
    if let Err(e) = std::fs::create_dir(dir) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(create(e));
        }
        // `symlink_metadata` does not follow, so a link is seen as a link.
        let found = std::fs::symlink_metadata(dir).map_err(create)?;
        if !found.is_dir() {
            remove_entry(dir, &found).map_err(create)?;
            std::fs::create_dir(dir).map_err(create)?;
        }
    }
    restrict_to_this_account(dir)
}

/// Creates a private directory that **did not exist a moment ago**, under a
/// parent that is made private first.
///
/// For a directory whose name somebody else can predict, which is what a
/// scratch directory named after the process id is. [`create_private_dir`]
/// adopts whatever is already at the path: `create_dir_all` answers `Ok` when
/// the entry exists, a symbolic link to a directory included, and the
/// permissions are then set on **whatever it points at**. On a machine with
/// other users, a `/tmp/snob-ig-stories/run-<pid>` planted ahead of time as a
/// link would choose where a run writes every story it fetches, and have a
/// directory of the victim's own made `0700`. That needs a parent anybody can
/// create entries in, which an unrestricted one under `/tmp` is.
///
/// Two things close it. The parent is created and restricted first -- with
/// `create_private_root`, which refuses to follow a link planted at *its*
/// predictable name too -- so on a run that is the first to use it nobody else
/// can put an entry inside; and the leaf is created with `create_dir`, which
/// fails rather than adopts when something is already there. What is already
/// there is removed if it is something this tool could have left -- an
/// earlier run's directory under a reused process id, which Windows hands out
/// again freely -- and the creation is tried once more. [`remove_tree`] takes
/// a link as the link itself, never following it.
pub fn create_fresh_private_dir(dir: &Path) -> Result<(), PathError> {
    let create = |source| PathError::Create {
        path: dir.to_path_buf(),
        source,
    };
    if let Some(parent) = dir.parent() {
        create_private_root(parent)?;
    }
    if let Err(e) = std::fs::create_dir(dir) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(create(e));
        }
        remove_tree(dir).map_err(create)?;
        std::fs::create_dir(dir).map_err(create)?;
    }
    restrict_to_this_account(dir)
}

/// Removes a directory tree, or a link found at the name as the link itself.
///
/// For `snob purge`, whose list includes the scratch root in the shared
/// temporary directory. `remove_dir_all` already refuses a link at the top --
/// it opens without following -- which is safe, but it reports somebody
/// else's planted link as our failure to clean up. Taking the link as a link
/// leaves what it points at alone and still leaves nothing of snob's behind.
pub fn remove_tree(dir: &Path) -> std::io::Result<()> {
    let found = std::fs::symlink_metadata(dir)?;
    if found.is_dir() {
        std::fs::remove_dir_all(dir)
    } else {
        remove_entry(dir, &found)
    }
}

/// Removes an entry already judged not to be a real directory, as itself: a
/// link as the link, whatever it points at, and a file as the file.
///
/// It goes by the metadata the caller read rather than reading it again, so
/// whatever appears at the name in between, no directory's contents are
/// removed: `remove_dir` takes only an empty directory and `remove_file` none.
/// At the scratch root that is another run's live scratch, created between
/// the two readings a second one would make.
fn remove_entry(dir: &Path, found: &std::fs::Metadata) -> std::io::Result<()> {
    if is_link_to_a_directory(found) {
        std::fs::remove_dir(dir)
    } else {
        std::fs::remove_file(dir)
    }
}

/// [`remove_tree`], tried a few times over a few seconds before giving up; a
/// tree that is already gone is removed.
///
/// For a browser profile. Windows keeps a directory while any handle inside
/// it is open, and a browser that has just been told to close takes a moment
/// to let go of them all.
pub fn remove_tree_patiently(dir: &Path) -> std::io::Result<()> {
    patiently(|| match remove_tree(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        done => done,
    })
}

/// A rename, tried the way [`remove_tree_patiently`] tries a removal.
pub fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    patiently(|| std::fs::rename(from, to))
}

/// Ten attempts over about five seconds. Blocks the thread, so a caller on an
/// async runtime whose workers are all needed runs it off them. Nothing is
/// retried once the name is not there: that does not wait itself away.
fn patiently(mut attempt: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut tries = 0u64;
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(e) if tries == 9 || e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100 * tries));
            }
        }
    }
}

/// Whether a directory entry is a link whose target is a directory.
///
/// Only Windows tells the two kinds of link apart, and only there does it
/// matter: a directory link is a directory to the call that removes it, so it
/// goes with `RemoveDirectory`, and `DeleteFile` answers `ERROR_ACCESS_DENIED`
/// on it. On Unix every link is a file and `remove_file` takes it.
fn is_link_to_a_directory(found: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        found.file_type().is_symlink_dir()
    }
    #[cfg(not(windows))]
    {
        let _ = found;
        false
    }
}

/// The half of [`create_private_dir`] that makes the directory private.
fn restrict_to_this_account(dir: &Path) -> Result<(), PathError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir, perms).map_err(|source| PathError::Create {
            path: dir.to_path_buf(),
            source,
        })?;
    }

    #[cfg(windows)]
    windows_acl::restrict_to_owner(dir)?;
    // The ACL says who may read it; this says who may copy it into a database
    // that outlives it. Two separate findings, one function.
    #[cfg(windows)]
    keep_out_of_the_search_index(dir);

    Ok(())
}

/// Marks a directory so that Windows Search does not index what is inside it.
///
/// The crawl scope includes `AppData\Local`, and a query against the live
/// index returned files out of `%LOCALAPPDATA%\snob-ig\data\browser-profile`,
/// the profile that holds a second copy of the session while a login is in
/// progress. Unmarked, everything this tool writes is read by the indexer and
/// copied into its database, where deleting the original does not remove it.
///
/// `FILE_ATTRIBUTE_NOT_CONTENT_INDEXED` is inherited by files created inside
/// afterwards: a file created in a marked directory carries the attribute
/// without anything setting it. So this is set once, on the directory, at
/// creation.
///
/// Failure is ignored deliberately. The attribute is a defense in depth on top
/// of the ACL, not the thing keeping anybody out, and a tool that refuses to
/// run because an attribute would not set is worse than one that is indexed.
#[cfg(windows)]
fn keep_out_of_the_search_index(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;

    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);

    // SAFETY: `SetFileAttributesW` reads a null-terminated wide string and
    // returns a boolean. The buffer above is null-terminated and outlives the
    // call, and the attribute value is a documented constant. Microsoft
    // documents that it does not clear other attributes when the value is
    // combined, so the directory bit is preserved by reading first.
    unsafe {
        let existing = windows_sys::Win32::Storage::FileSystem::GetFileAttributesW(wide.as_ptr());
        if existing == windows_sys::Win32::Storage::FileSystem::INVALID_FILE_ATTRIBUTES {
            return;
        }
        windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(
            wide.as_ptr(),
            existing | windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        );
    }
}

/// Giving a directory a DACL that names only the user running this process.
///
/// Written against `windows-sys` rather than through a crate because it is one
/// call each to five documented functions, and because the shape of the answer
/// — a protected DACL with exactly one ACE — is what
/// `the_data_directory_is_not_readable_by_other_accounts` reads back. The user
/// and the ACL are [`crate::windows_user`]'s, which the owner's pipe shares.
#[cfg(windows)]
mod windows_acl {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };

    use super::PathError;
    use crate::windows_user::{OneUserAcl, User};

    fn failed(dir: &Path, what: &str, error: std::io::Error) -> PathError {
        PathError::NotPrivate {
            path: dir.to_path_buf(),
            detail: format!("{what} failed: {error}"),
        }
    }

    pub(super) fn restrict_to_owner(dir: &Path) -> Result<(), PathError> {
        let user = User::this_process().map_err(|e| failed(dir, "reading this user", e))?;
        // Inherited by what is created inside, because the point is the
        // database and the browser profile rather than the folder itself.
        let mut acl = OneUserAcl::new(&user, CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE)
            .map_err(|e| failed(dir, "building the ACL", e))?;

        let mut wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // `PROTECTED_DACL_SECURITY_INFORMATION` is the half that matters. Set
        // the DACL without it and the inherited entries stay, which is the
        // situation this exists to end.
        //
        // SAFETY: a null-terminated path, and an ACL that outlives the call.
        let set = unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl.as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if set != 0 {
            return Err(PathError::NotPrivate {
                path: dir.to_path_buf(),
                detail: format!("SetNamedSecurityInfo failed: Windows error {set}"),
            });
        }
        Ok(())
    }

    /// What the directory's DACL actually says, for the test that reads it
    /// back. Returns whether the DACL is protected, and every SID in it.
    #[cfg(test)]
    pub(super) fn describe(dir: &Path) -> (bool, Vec<String>) {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR_CONTROL,
        };

        let mut wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut acl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();

        // SAFETY: a null-terminated path and out-parameters; the descriptor is
        // freed below.
        let read = unsafe {
            GetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(read, 0, "GetNamedSecurityInfo failed");

        let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
        let mut revision: u32 = 0;
        // SAFETY: a descriptor the call above produced.
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        let protected = control & SE_DACL_PROTECTED != 0;

        // SAFETY: an out parameter of three integers, which `GetAclInformation`
        // fills below; all-zero is a valid starting value for every one of them.
        let mut sizes: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the ACL points into the descriptor, which is still alive.
        unsafe {
            GetAclInformation(
                acl,
                &mut sizes as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };

        let mut sids = Vec::new();
        for index in 0..sizes.AceCount {
            let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
            // SAFETY: index is below the count the ACL just reported.
            if unsafe { GetAce(acl, index, &mut ace) } == 0 {
                continue;
            }
            // Every ACE type this can produce puts its SID immediately after
            // the access mask, which is one `u32` past the header.
            //
            // SAFETY: the layout of an access-allowed ACE.
            let sid = unsafe {
                (ace as *const u8)
                    .add(std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>())
                    as PSID
            };
            let mut text: *mut u16 = std::ptr::null_mut();
            // SAFETY: a SID inside the descriptor, and an out-parameter freed
            // immediately after it is read.
            unsafe {
                if ConvertSidToStringSidW(sid, &mut text) != 0 {
                    let mut length = 0;
                    while *text.add(length) != 0 {
                        length += 1;
                    }
                    sids.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                        text, length,
                    )));
                    LocalFree(text as *mut std::ffi::c_void);
                }
            }
        }

        // SAFETY: the descriptor the read produced, freed once.
        unsafe { LocalFree(descriptor) };
        (protected, sids)
    }

    /// This process's user, as a string SID, so a test can compare.
    #[cfg(test)]
    pub(super) fn current_user_sid() -> String {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

        let user = crate::windows_user::User::this_process().expect("this process has a token");
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: a SID this function owns, and an out-parameter freed after
        // it is read.
        unsafe {
            assert_ne!(ConvertSidToStringSidW(user.sid(), &mut text), 0);
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            LocalFree(text as *mut std::ffi::c_void);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A sandboxed run must not reach the real temporary directory.**
    /// The scratch directory is the one location that comes from the
    /// environment rather than from `directories`, so it is the one that would
    /// escape a root — and "every file this run touches is under one
    /// directory" is what `--sandbox-root` promises in `AGENTS.md`.
    #[test]
    fn a_rooted_run_keeps_its_scratch_under_the_root() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.stories_root().starts_with("/tmp/test"));
        assert!(paths.story_scratch().starts_with(paths.stories_root()));
    }

    /// Two runs at once must not share a directory one of them will delete.
    #[test]
    fn each_run_gets_a_scratch_directory_of_its_own() {
        let paths = AppPaths::rooted_at("/tmp/test");
        let mine = paths.story_scratch();
        assert!(
            mine.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(&std::process::id().to_string())),
            "the run has to be named in the path: {}",
            mine.display()
        );
    }

    /// The scratch root is on the list `purge` reads. It is the only thing this
    /// tool writes outside the data directory, so it is the only one that could
    /// be forgotten there.
    #[test]
    fn the_scratch_root_is_something_purge_removes() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.owned_dirs().contains(&paths.stories_root()));
    }

    /// The sweep takes what is old and leaves what is not. Both halves matter:
    /// deleting a live run's directory takes the file out from under the viewer
    /// it was just handed to.
    #[test]
    fn the_sweep_takes_the_abandoned_and_leaves_the_living() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("run-old");
        let fresh = tmp.path().join("run-fresh");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(old.join("story.jpg"), b"x").unwrap();

        // Zero, so everything already on disk counts as abandoned; then a
        // generous one, so nothing does. Testing it this way rather than by
        // waiting keeps the suite off the wall clock.
        sweep_old_scratch(tmp.path(), std::time::Duration::ZERO);
        assert!(!old.exists(), "an abandoned directory should have gone");

        std::fs::create_dir_all(&fresh).unwrap();
        sweep_old_scratch(tmp.path(), std::time::Duration::from_secs(3600));
        assert!(fresh.exists(), "a live directory must be left alone");
    }

    /// A root that does not exist is not an error. It is the common case: the
    /// first run of `snob stories` on a machine.
    #[test]
    fn sweeping_a_root_nothing_has_created_is_quiet() {
        sweep_old_scratch(
            Path::new("/nonexistent-snob-scratch"),
            std::time::Duration::ZERO,
        );
    }

    #[test]
    fn files_hang_off_their_directories() {
        let paths = AppPaths::rooted_at("/tmp/test");
        assert!(paths.browser_profile().starts_with(paths.data_dir()));
        let account = paths.browser_profile_for(snob_core::Pk::new(42));
        assert_eq!(account.parent(), Some(paths.browser_profile().as_path()));
        assert!(paths.browser_hints_file().starts_with(paths.data_dir()));
        for global in [paths.registry_file(), paths.shared_db_file()] {
            assert_eq!(global.parent(), Some(paths.data_dir()));
        }
        assert!(paths.accounts_dir().starts_with(paths.data_dir()));
        assert_eq!(
            paths.unclaimed_dir().parent(),
            Some(paths.accounts_dir().as_path())
        );
    }

    /// An account's files are its own: under a directory named after its id,
    /// and nowhere another account's are.
    #[test]
    fn an_accounts_files_are_under_its_own_directory() {
        let paths = AppPaths::rooted_at("/tmp/test");
        let a = paths.account(snob_core::Pk::new(42));
        let b = paths.account(snob_core::Pk::new(43));

        assert_eq!(a.pk(), snob_core::Pk::new(42));
        assert_eq!(a.dir(), paths.accounts_dir().join("42"));
        for file in [a.db_file(), a.session_file()] {
            assert_eq!(file.parent(), Some(a.dir().as_path()));
            assert!(!file.starts_with(b.dir()));
        }
        assert_eq!(
            a.browser_profile(),
            paths.browser_profile_for(snob_core::Pk::new(42))
        );
        assert_ne!(a.db_file(), paths.legacy_db_file());
        assert_ne!(a.session_file(), paths.legacy_session_file());
        // What is not the account's is the machine's, through the deref.
        assert_eq!(a.data_dir(), paths.data_dir());
    }

    /// Both directories are created, and the account's is private too.
    #[test]
    fn an_account_creates_its_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let account = AppPaths::rooted_at(tmp.path()).account(snob_core::Pk::new(42));
        account.ensure_dirs().unwrap();
        assert!(account.data_dir().is_dir());
        assert!(account.dir().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(account.dir())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    /// Only what gets written to, for the reason [`AppPaths::ensure_dirs`]
    /// gives: somebody who never runs the monitor gets no empty folder in
    /// their roaming profile.
    #[test]
    fn only_the_data_directory_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        paths.ensure_dirs().unwrap();

        assert!(paths.data_dir().is_dir());
        assert!(
            !paths.config_dir().exists(),
            "the directory belongs to whatever writes configuration, not to every run"
        );
    }

    /// The data directory is limited to this account, and says so to the
    /// operating system rather than in a comment.
    ///
    /// Two things are read back, and the first is the one that is easy to lose:
    /// the DACL has to be **protected**, because a DACL set without that flag
    /// keeps every inherited entry and the exposure is exactly those entries.
    /// The second is that the only account named is this one.
    #[cfg(windows)]
    #[test]
    fn the_data_directory_is_not_readable_by_other_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        paths.ensure_dirs().unwrap();

        let (protected, sids) = windows_acl::describe(paths.data_dir());
        assert!(
            protected,
            "the DACL is not protected, so whatever the profile grants still applies"
        );
        assert_eq!(
            sids,
            vec![windows_acl::current_user_sid()],
            "somebody other than this account is named in the directory's DACL"
        );
    }

    /// Where the data goes must not depend on the directory the command was
    /// run from: it is the user's data, not the folder's.
    #[test]
    fn the_paths_do_not_depend_on_the_working_directory() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        assert!(
            paths
                .account(snob_core::Pk::new(42))
                .db_file()
                .is_absolute(),
            "a relative path would follow whoever ran the command around"
        );
        assert!(paths.data_dir().is_absolute());
    }

    /// A scratch directory with a guessable name is never adopted.
    ///
    /// The planted entry is a symbolic link to a directory of the victim's
    /// own. `create_dir_all` answers `Ok` on it and the permissions land on the
    /// target, and every story fetched afterwards would be written wherever the
    /// link points. The fresh variant removes the link as a link -- the target
    /// is untouched -- and makes a real directory.
    #[test]
    fn a_planted_link_under_the_scratch_name_is_replaced_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("theirs.txt"), b"untouched").unwrap();

        let root = tmp.path().join("snob-ig-stories");
        std::fs::create_dir(&root).unwrap();
        let leaf = root.join("run-1234");
        #[cfg(unix)]
        let planted = std::os::unix::fs::symlink(&elsewhere, &leaf).is_ok();
        // A link to a directory needs a privilege on Windows that a test
        // runner does not always have; without it the case is a leftover
        // directory, which the second half covers.
        #[cfg(windows)]
        let planted = std::os::windows::fs::symlink_dir(&elsewhere, &leaf).is_ok();

        create_fresh_private_dir(&leaf).unwrap();

        let made = std::fs::symlink_metadata(&leaf).unwrap();
        assert!(made.is_dir() && !made.is_symlink(), "a link was adopted");
        assert!(
            elsewhere.join("theirs.txt").exists(),
            "removing the link must not reach through it"
        );
        let _ = planted;

        // An earlier run's directory under a reused process id is replaced,
        // and what it held does not survive into the new run.
        std::fs::write(leaf.join("stale.jpg"), b"x").unwrap();
        create_fresh_private_dir(&leaf).unwrap();
        assert!(!leaf.join("stale.jpg").exists());
        assert!(leaf.is_dir());
    }

    /// The parent is made private before the leaf, so on a first run nobody
    /// else can put an entry inside it.
    #[cfg(unix)]
    #[test]
    fn the_scratch_root_is_private_too() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let leaf = tmp.path().join("snob-ig-stories").join("run-1");
        create_fresh_private_dir(&leaf).unwrap();
        for dir in [leaf.parent().unwrap(), leaf.as_path()] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }

    /// A failed replacement names the step that failed: the temporary when it
    /// could not be written, the file itself when the rename could not land.
    #[test]
    fn a_failed_replacement_names_the_path_that_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file.toml");
        let temporary = tmp.path().join("file.toml.new");

        std::fs::create_dir(&temporary).unwrap();
        let (failed, _) = replace_private(&path, &temporary, b"x").unwrap_err();
        assert_eq!(failed, temporary);
        std::fs::remove_dir(&temporary).unwrap();

        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), b"x").unwrap();
        let (failed, _) = replace_private(&path, &temporary, b"x").unwrap_err();
        assert_eq!(failed, path);
    }

    /// A link or a stray file planted at the scratch root's name is taken as
    /// itself and replaced by a private directory; what a link pointed at is
    /// left alone.
    #[cfg(unix)]
    #[test]
    fn a_planted_entry_at_the_scratch_root_is_replaced_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();

        let root = tmp.path().join("snob-ig-stories");
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        create_fresh_private_dir(&root.join("run-1")).unwrap();
        let made = std::fs::symlink_metadata(&root).unwrap();
        assert!(made.is_dir() && !made.is_symlink(), "the link was followed");
        assert_eq!(made.permissions().mode() & 0o777, 0o700);
        let theirs = std::fs::metadata(&elsewhere).unwrap().permissions().mode();
        assert_eq!(theirs & 0o777, 0o755, "the link's target was made private");
        assert!(!elsewhere.join("run-1").exists());

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::write(&root, b"stray").unwrap();
        create_fresh_private_dir(&root.join("run-1")).unwrap();
        let made = std::fs::symlink_metadata(&root).unwrap();
        assert!(made.is_dir(), "the stray file is still there");
        assert_eq!(made.permissions().mode() & 0o777, 0o700);
    }

    /// Everything `purge` has to take away has to be under something this
    /// returns, or the command quietly stops leaving nothing behind.
    #[test]
    fn every_directory_written_to_is_on_the_purge_list() {
        let paths = AppPaths::rooted_at("/tmp/test");
        let owned = paths.owned_dirs();
        let account = paths.account(snob_core::Pk::new(42));

        for path in [
            account.db_file(),
            account.session_file(),
            account.browser_profile(),
            paths.registry_file(),
            paths.shared_db_file(),
            paths.unclaimed_dir(),
            paths.legacy_db_file(),
            paths.legacy_session_file(),
            paths.browser_profile(),
            paths.browser_hints_file(),
            // The configuration file, which lives outside the data directory.
            crate::config::path(&paths),
        ] {
            assert!(
                owned.iter().any(|dir| path.starts_with(dir)),
                "{} is under no directory purge would remove",
                path.display()
            );
        }
    }

    /// On macOS the configuration and data directories are one path. Listed
    /// twice, the second removal fails on what the first already took.
    #[test]
    fn the_purge_list_has_no_repeats() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        let owned = paths.owned_dirs();
        let mut unique = owned.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), owned.len(), "{owned:?}");
    }

    /// The real directories have to survive the guard, or purge removes
    /// nothing at all — a failure that would look exactly like success.
    #[test]
    fn the_real_directories_are_removable() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        for dir in paths.owned_dirs() {
            assert!(is_safe_to_remove(&dir), "{}", dir.display());
        }
    }

    #[test]
    fn nothing_near_the_root_is_removable() {
        for dangerous in ["/", "/home", "/data", "C:\\", "C:\\Users"] {
            assert!(
                !is_safe_to_remove(Path::new(dangerous)),
                "{dangerous} should never be removable"
            );
        }
    }

    /// A `ProjectDirs` that resolved to the home directory itself would take
    /// everything the user owns with it.
    #[test]
    fn the_home_directory_is_not_removable() {
        let Some(base) = directories::BaseDirs::new() else {
            return;
        };
        assert!(!is_safe_to_remove(base.home_dir()));
    }

    /// The database must never end up in a directory that syncs. On Windows
    /// that means Local rather than Roaming; elsewhere both coincide and the
    /// check is trivially true.
    #[test]
    fn data_goes_to_the_local_directory() {
        let Ok(paths) = AppPaths::discover() else {
            return; // no HOME in the test environment
        };
        let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).unwrap();
        assert_eq!(paths.data_dir(), dirs.data_local_dir());
        #[cfg(windows)]
        assert!(!paths.accounts_dir().to_string_lossy().contains("Roaming"));
    }
}
