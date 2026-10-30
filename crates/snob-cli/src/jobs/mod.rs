//! A list walk that outlives the call that started it: a job.
//!
//! An agent's tool call is bounded by its client, a minute or a few, and a
//! walk of a few thousand accounts takes most of an hour at a human pace. So a
//! walk can be started as a job ([`start`]), which goes on in a process of its
//! own after the call has answered, and asked about later by its id
//! ([`show`]), from this process or another, by an agent or by the person
//! (`snob jobs`).
//!
//! **Three processes, and the job's state is files.** `snob __job start`
//! checks the command, writes the job's record and starts the supervisor,
//! detached (`detach.rs`), then answers with the record, or with the result
//! when the walk finished in the time it was told to wait. The supervisor,
//! `snob __job run <id>`, holds the job's lock for its whole life, runs the
//! walk as a child of its own with the flags every agent's run gets
//! (`--spend-alone`, `--stop-when-stdin-closes`) and `--progress-json`, and
//! writes what it came to. Everything else (`show`, `list`, `cancel`,
//! `snob status`, the sweep) reads the files and asks the lock, and writes
//! nothing but a `stop` file.
//!
//! **One job running per account, and nothing in one runs that was not
//! checked where it runs** (`argv.rs`): the supervisor runs itself, never a
//! program named on disk, and checks the stored command line again.
//!
//! **Stopping is a file, never a signal.** `cancel`, `purge` and `logout` write
//! `stop` into the job's directory; the supervisor, which asks for it every
//! second, closes the walk's standard input, which stops it as Ctrl+C does,
//! and kills it through its own handle only past [`GRACE`]. No process id read
//! from a file is ever signaled: a pid outlives its process and is reused.
//!
//! **Everything here is `std` and threads**, called from the async runtime
//! where waiting is involved, so jobs build without the `mcp` feature and
//! `snob jobs` and `purge` work in every build (`tokio`'s `process` is the
//! `mcp` feature's).
//!
//! Files in a job's directory, each `0600` on Unix (created so, by
//! `paths::replace_private`, `supervise::private`, `detach` and
//! [`private_marker`]) and under the account's user-only directory on
//! Windows:
//! - `job.json`, written by `start`, and once more with how it was detached;
//! - `state.json`, written by the supervisor alone, or by `start` for a
//!   supervisor that could not be started;
//! - `lock`, held by the supervisor;
//! - `stop`, written by whoever asks it to stop;
//! - `out.json.partial`, the walk's standard output, renamed `out.json` once
//!   it has ended;
//! - `err.log`, the walk's standard error, the progress lines among it;
//! - `supervisor.log`, the supervisor's own errors.

pub(crate) mod argv;
mod id;
pub mod show;
pub mod start;
pub mod supervise;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snob_core::Pk;
use snob_store::paths::{self, AccountPaths, JobId};

/// The version of `job.json`'s shape. A record of another one is shown, as
/// what it is, and never run.
pub const SCHEMA: u32 = 1;

/// How long a walk asked to stop gets before it is killed: the same as for
/// a call's child (`commands::mcp::child`), for the same reason. A walk stops
/// between two requests and a request in flight races the stop, so seconds
/// are plenty; killing it holds its claim on the list until the claim's lease
/// runs out.
pub const GRACE: Duration = Duration::from_secs(30);

/// Environment variables an agent's run never inherits: a call's child
/// (`commands::mcp::child`) and a job's walk (`supervise`).
///
/// `SNOB_IGNORE_COOLDOWN` is the escape hatch that exists so a bug of ours
/// cannot lock a person out; set in the environment the client was started
/// from, it would hand every agent's call a way past the hard stop.
/// `SNOB_MCP_KEY` and `SNOB_MCP_TOKEN` are the key of `snob mcp` served
/// over the network, the server's and a client's copy (`commands::mcp::key`):
/// no command a call runs reads either, and every process it starts would
/// carry them on, the browser owner and its browsers among them, which keep
/// the environment they were started with. The rest of the environment is
/// the person's own machine (`SNOB_NO_BROWSER`, the proxy, the locale) and
/// is passed on.
pub const NEVER_INHERITED: &[&str] = &["SNOB_IGNORE_COOLDOWN", "SNOB_MCP_KEY", "SNOB_MCP_TOKEN"];

/// How long a job may wait for its supervisor before it counts as lost: it
/// is started at once, so a job still queued after this never will be.
pub const LOST_AFTER: Duration = Duration::from_secs(30);

/// How often a running supervisor says it is still there (`updated_at`). A
/// job found lost is taken to have held its claim on the list until at most
/// this long after, which [`Seen::claim_frees_at`] counts on.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(30);

/// How long a finished job's result is kept: a day, which is longer than
/// anybody waits to ask for it, and short enough that lists do not pile up.
const KEEP_RESULT: Duration = Duration::from_secs(24 * 3600);

/// How long a finished job's directory is kept, its record and its log.
pub(crate) const KEEP_JOB: Duration = Duration::from_secs(7 * 24 * 3600);

/// The most jobs kept for an account, however recent: the oldest finished
/// ones go first.
const MOST_KEPT: usize = 50;

/// What a job is, written once by `start`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Job {
    pub schema: u32,
    pub id: String,
    /// Milliseconds since the epoch.
    pub created_at: i64,
    pub viewer_pk: Pk,
    /// The tool call it was started for, when it was one, and its arguments,
    /// without those that only shaped how the call was answered.
    pub tool: Option<String>,
    pub arguments: Value,
    /// The command it runs, as checked (`argv::check`).
    pub argv: Vec<String>,
    /// The global flags its processes run with, **for showing only**: the
    /// supervisor takes its own from its command line (`argv::globals`).
    pub globals: Vec<String>,
    /// What the walk was estimated to cost when it was started
    /// (`commands::dry_run`), as `--dry-run`'s JSON says it.
    pub estimate: Option<Value>,
    pub detached: Option<Detached>,
    /// The client that started it, in the few characters `client_label`
    /// keeps.
    pub started_by: Option<String>,
    /// The `--max-wait` its walk runs with, in seconds.
    pub max_wait_seconds: Option<u64>,
}

/// Whether the job left the process group, or the job object, of whatever
/// started it (`detach::Started::left_job`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Detached {
    /// It goes on when its client closes.
    LeftJob,
    /// It ends when its client closes: the client's job object did not let it
    /// out (Windows), and closing the client ends the walk with it.
    InClientJob,
}

impl Detached {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeftJob => "left_job",
            Self::InClientJob => "in_client_job",
        }
    }
}

/// How a job stands, written by its supervisor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    /// `Running`, or how it ended.
    pub status: Status,
    pub started_at: i64,
    /// The last time the supervisor said it was there.
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    /// What the walk exited with, when it exited.
    pub exit_number: Option<i32>,
    /// Whether it was asked to stop.
    pub cancel: bool,
    /// Why it ended without running, as a token (`refused_argv`).
    pub failure: Option<String>,
    /// For showing only: nothing signals a pid read from a file.
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
}

/// How a job stands, as everything but its supervisor reads it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Made, and its supervisor not yet there.
    Queued,
    Running,
    /// Not finished, and nobody running it: its supervisor was killed, or
    /// never started.
    Lost,
    /// The walk exited 0.
    Succeeded,
    /// It exited otherwise, or did not run.
    Failed,
    /// It was asked to stop, and stopped.
    Canceled,
    /// It stopped as for Ctrl+C without being asked to, at a limit on a wait
    /// or because the system asked.
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Lost => "lost",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Stopped => "stopped",
        }
    }

    /// Whether it is over and will not change again.
    pub fn is_over(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    /// Whether the walk ran and ended.
    pub fn finished(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::Stopped
        )
    }
}

/// The status a job is read as, from its state, whether its lock is held,
/// when it was made and the time now.
///
/// An ending its supervisor wrote is the answer, even while it still holds
/// the lock on its way out. Otherwise a held lock is a running job. A
/// supervisor that wrote it was running and holds nothing is gone; so is one
/// that never came, after [`LOST_AFTER`].
pub fn derive(state: Option<&State>, held: bool, created_at: i64, now: i64) -> Status {
    if let Some(state) = state
        && state.status.is_over()
    {
        return state.status;
    }
    if held {
        return Status::Running;
    }
    match state {
        Some(_) => Status::Lost,
        None if now.saturating_sub(created_at) > LOST_AFTER.as_millis() as i64 => Status::Lost,
        None => Status::Queued,
    }
}

/// A job as it was read.
#[derive(Debug, Clone)]
pub struct Seen {
    pub id: JobId,
    pub dir: PathBuf,
    /// `None` when `job.json` did not read: it is then shown, and swept, and
    /// never run.
    pub job: Option<Job>,
    pub state: Option<State>,
    pub held: bool,
    pub stop_asked: bool,
    pub status: Status,
}

impl Seen {
    /// When the job was made, or when its directory was, for a record that
    /// did not read.
    pub fn created_at(&self) -> i64 {
        self.job
            .as_ref()
            .map(|job| job.created_at)
            .unwrap_or_else(|| modified_ms(&self.dir).unwrap_or(0))
    }

    /// The last moment anything of it is known to have been alive.
    pub fn last_alive(&self) -> i64 {
        let state = self.state.as_ref().map_or(0, |s| s.updated_at);
        state.max(self.created_at())
    }

    /// For a lost job: the moment its walk's claim on the list frees at the
    /// latest, which is how long another walk of that list is refused
    /// (`report::walked_elsewhere`). A walk refreshes its claim with every
    /// page, and the supervisor said it was there until [`HEARTBEAT`] before
    /// it was not, so the claim was last refreshed before then.
    pub fn claim_frees_at(&self) -> i64 {
        let ttl = snob_store::store::snapshots::CLAIM_TTL_SECS * 1000;
        self.last_alive() + HEARTBEAT.as_millis() as i64 + ttl
    }

    /// When it ended, or was last seen, for the retention.
    fn ended_at(&self) -> i64 {
        self.state
            .as_ref()
            .and_then(|s| s.finished_at)
            .unwrap_or_else(|| self.last_alive())
    }
}

/// A file of a job's directory.
pub(crate) fn file(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// Writes `value` as `name` in `dir`, whole or not at all
/// (`paths::replace_private`), so a reader sees the old record or the new.
pub(crate) fn write(dir: &Path, name: &str, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    paths::replace_private(&file(dir, name), &file(dir, &format!("{name}.tmp")), &bytes)
        .map_err(|(_, e)| e)
}

/// Reads `name` in `dir`: `None` when it is not there, or does not read as
/// what it should be.
pub(crate) fn read<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Option<T> {
    let bytes = std::fs::read(file(dir, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn modified_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Reads one job of `account`'s, **without writing anything**: `None` when
/// the account has no job of that name.
pub fn look(account: &AccountPaths, id: &JobId, now: i64) -> io::Result<Option<Seen>> {
    let dir = account.job_dir(id);
    if !dir.is_dir() {
        return Ok(None);
    }
    let job: Option<Job> = read(&dir, "job.json");
    let state: Option<State> = read(&dir, "state.json");
    let held = paths::lock_is_held(&file(&dir, "lock"))?.unwrap_or(false);
    let stop_asked = file(&dir, "stop").exists();
    let created_at = job
        .as_ref()
        .map(|job| job.created_at)
        .unwrap_or_else(|| modified_ms(&dir).unwrap_or(0));
    let status = derive(state.as_ref(), held, created_at, now);
    Ok(Some(Seen {
        id: id.clone(),
        dir,
        job,
        state,
        held,
        stop_asked,
        status,
    }))
}

/// Every job of `account`'s, oldest first. A directory whose name is not a
/// job's is not one of ours and is left out, and alone.
pub fn all(account: &AccountPaths, now: i64) -> io::Result<Vec<Seen>> {
    let entries = match std::fs::read_dir(account.jobs_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut ids: Vec<JobId> = entries
        .flatten()
        .filter_map(|entry| JobId::parse(entry.file_name().to_str()?))
        .collect();
    ids.sort();
    let mut seen = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(job) = look(account, &id, now)? {
            seen.push(job);
        }
    }
    Ok(seen)
}

/// Takes the lock every start and every sweep of `account`'s jobs holds
/// (`jobs/.lock`), creating the directory, private, when it is not there.
/// Held until the file is dropped.
pub(crate) fn lock_jobs(account: &AccountPaths) -> io::Result<std::fs::File> {
    account.ensure_dirs().map_err(io::Error::other)?;
    paths::create_private_dir(&account.jobs_dir()).map_err(io::Error::other)?;
    paths::lock_file(&account.jobs_dir().join(".lock"))
}

/// Asks a job to stop. Nothing else is touched: its supervisor reads the
/// file.
pub(crate) fn ask_to_stop(seen: &Seen) -> io::Result<()> {
    private_marker(&file(&seen.dir, "stop"))
}

/// An empty file whose being there is what it says (`lock`, `stop`), made
/// `0600` on Unix like every other file of a job's, and left as it is when
/// it is there already.
pub(crate) fn private_marker(path: &Path) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).map(drop)
}

/// Forgets what `account`'s jobs no longer need, under the jobs lock the
/// caller holds: a finished job's result after [`KEEP_RESULT`], the whole job
/// after [`KEEP_JOB`], and the oldest past [`MOST_KEPT`]. **A job whose lock
/// is held is never touched**, however old.
pub(crate) fn sweep(account: &AccountPaths, now: i64) {
    let Ok(jobs) = all(account, now) else {
        return;
    };
    let mut kept = Vec::new();
    for seen in jobs {
        if seen.held || !seen.status.is_over() {
            kept.push(seen);
            continue;
        }
        let age = now.saturating_sub(seen.ended_at());
        if age > KEEP_JOB.as_millis() as i64 {
            let _ = paths::remove_tree(&seen.dir);
            continue;
        }
        if age > KEEP_RESULT.as_millis() as i64 {
            let _ = std::fs::remove_file(file(&seen.dir, "out.json"));
            let _ = std::fs::remove_file(file(&seen.dir, "out.json.partial"));
        }
        kept.push(seen);
    }
    // Oldest first, so the ones past the count are at the front.
    let over = kept.len().saturating_sub(MOST_KEPT);
    for seen in kept
        .iter()
        .filter(|seen| !seen.held && seen.status.is_over())
        .take(over)
    {
        let _ = paths::remove_tree(&seen.dir);
    }
}

/// `account`'s jobs being stopped, and the jobs lock held so no other starts
/// until this is dropped (`purge`, `logout`).
pub struct Stopping {
    pub ids: Vec<JobId>,
    account: AccountPaths,
    _lock: Option<std::fs::File>,
}

impl Stopping {
    /// Asks every job of `account`'s that is not over to stop, holding the
    /// jobs lock. An account with no jobs directory has none, and nothing is
    /// made for it.
    pub fn every_job_of(account: &AccountPaths) -> io::Result<Self> {
        if !account.jobs_dir().is_dir() {
            return Ok(Self {
                ids: Vec::new(),
                account: account.clone(),
                _lock: None,
            });
        }
        let lock = lock_jobs(account)?;
        let now = snob_core::clock::now_ms().get();
        let mut ids = Vec::new();
        for seen in all(account, now)? {
            if !seen.status.is_over() {
                ask_to_stop(&seen)?;
                ids.push(seen.id);
            }
        }
        Ok(Self {
            ids,
            account: account.clone(),
            _lock: Some(lock),
        })
    }

    /// Waits, at most `within`, for every job asked to stop to be over and
    /// let go of its lock. Blocks the thread: a caller on the runtime runs
    /// it off it.
    pub fn wait(&self, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        loop {
            let now = snob_core::clock::now_ms().get();
            let settled = self
                .ids
                .iter()
                .all(|id| match look(&self.account, id, now) {
                    Ok(Some(seen)) => seen.status.is_over() && !seen.held,
                    _ => true,
                });
            if settled {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// How long [`Stopping::wait`] waits: the grace a walk gets, and a little for
/// the supervisor to write its ending.
pub const STOPPING_WITHIN: Duration = Duration::from_secs(GRACE.as_secs() + 5);

/// For `purge` and `logout`: stops the jobs of `accounts`, holding each one's jobs lock so no other
/// starts, and waits for them to have stopped (`jobs::Stopping`). Said when
/// there were any, and warned when one did not stop in time; the removal goes
/// on either way.
pub async fn stop_before_leaving(accounts: Vec<AccountPaths>) -> anyhow::Result<Vec<Stopping>> {
    let (stopping, settled) = tokio::task::spawn_blocking(move || {
        let stopping = accounts
            .iter()
            .map(Stopping::every_job_of)
            .collect::<std::io::Result<Vec<_>>>()?;
        // Every one waited for, whether or not one before it settled.
        let late = stopping
            .iter()
            .filter(|jobs| !jobs.wait(STOPPING_WITHIN))
            .count();
        let settled = late == 0;
        Ok::<_, std::io::Error>((stopping, settled))
    })
    .await??;
    let count: usize = stopping.iter().map(|jobs| jobs.ids.len()).sum();
    if count > 0 {
        crate::ui::say!(
            "Stopped {count} {} first.",
            if count == 1 { "job" } else { "jobs" }
        );
    }
    if !settled {
        crate::ui::warn("a job did not stop in time; what it holds may not all go");
    }
    Ok(stopping)
}

/// Runs `snob __job …` and `snob jobs …`, as the account this run acts as and
/// on that account's jobs alone.
///
/// Called by `main` after the layout is settled and before the owner of the
/// browsers is installed: nothing here sends a request, and the walk a
/// supervisor runs is a process of its own, which installs its own.
pub async fn dispatch(
    cli: &crate::cli::Cli,
    store: &snob_store::secrets::SecretStore,
    app: &paths::AppPaths,
) -> anyhow::Result<crate::exit::ExitCode> {
    use crate::cli::{Command, JobCommand, JobsArgs, JobsCommand};

    let env = std::env::var("SNOB_ACCOUNT").ok();
    let registry = snob_store::registry::Registry::load(app)?;
    let viewer = crate::account::resolve_if_any(cli.account.as_deref(), env.as_deref(), &registry)?
        .ok_or_else(crate::commands::common::no_session)?;
    crate::report::act_as(viewer.clone());
    let account = app.account(viewer.pk);

    match &cli.command {
        Command::Job(JobCommand::Start(args)) => start::run(cli, args, store, &account).await,
        Command::Job(JobCommand::Run(args)) => supervise::run(cli, &account, &args.id).await,
        Command::Job(JobCommand::Show(args))
        | Command::Jobs(JobsArgs {
            command: Some(JobsCommand::Show(args)),
            ..
        }) => show::show(&account, &args.id, args.wait, args.output.json).await,
        Command::Job(JobCommand::Cancel(args))
        | Command::Jobs(JobsArgs {
            command: Some(JobsCommand::Cancel(args)),
            ..
        }) => show::cancel(&account, &args.id, args.wait, args.output.json).await,
        Command::Job(JobCommand::List(args))
        | Command::Jobs(JobsArgs {
            command: Some(JobsCommand::List(args)),
            ..
        })
        | Command::Jobs(JobsArgs {
            command: None,
            list: args,
        }) => show::list(&account, args.all, args.output.json),
        _ => unreachable!("only the jobs' commands are dispatched here"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: Status) -> State {
        State {
            status,
            started_at: 0,
            updated_at: 0,
            finished_at: None,
            exit_number: None,
            cancel: false,
            failure: None,
            supervisor_pid: None,
            child_pid: None,
        }
    }

    #[test]
    fn a_job_whose_lock_is_free_and_state_unfinished_is_lost() {
        let running = state(Status::Running);
        assert_eq!(derive(Some(&running), false, 0, 1), Status::Lost);
        assert_eq!(derive(Some(&running), true, 0, 1), Status::Running);
        // Queued for longer than a supervisor takes to start.
        let late = LOST_AFTER.as_millis() as i64 + 1;
        assert_eq!(derive(None, false, 0, late), Status::Lost);
        // An ending is the answer, held or not.
        for over in [
            Status::Succeeded,
            Status::Failed,
            Status::Canceled,
            Status::Stopped,
        ] {
            assert_eq!(derive(Some(&state(over)), true, 0, 1), over);
            assert_eq!(derive(Some(&state(over)), false, 0, late), over);
        }
    }

    #[test]
    fn a_young_queued_job_is_not_lost() {
        assert_eq!(derive(None, false, 1_000, 1_000), Status::Queued);
        assert_eq!(
            derive(None, false, 1_000, 1_000 + LOST_AFTER.as_millis() as i64),
            Status::Queued
        );
        assert_eq!(derive(None, true, 1_000, 1_000), Status::Running);
    }

    fn account(tmp: &tempfile::TempDir) -> AccountPaths {
        paths::AppPaths::rooted_at(tmp.path()).account(Pk::new(42))
    }

    /// A job directory as `start` and a supervisor leave it.
    fn made(account: &AccountPaths, at: i64, over: Option<(Status, i64)>) -> JobId {
        let id = id::new(at as u64).unwrap();
        let dir = account.job_dir(&id);
        paths::create_private_dir(&dir).unwrap();
        let job = Job {
            schema: SCHEMA,
            id: id.to_string(),
            created_at: at,
            viewer_pk: account.pk(),
            tool: None,
            arguments: Value::Null,
            argv: vec!["followers".into()],
            globals: Vec::new(),
            estimate: None,
            detached: None,
            started_by: None,
            max_wait_seconds: None,
        };
        write(&dir, "job.json", &job).unwrap();
        std::fs::write(file(&dir, "out.json"), b"[]").unwrap();
        if let Some((status, finished)) = over {
            let mut ended = state(status);
            ended.finished_at = Some(finished);
            write(&dir, "state.json", &ended).unwrap();
        }
        id
    }

    #[test]
    fn retention_keeps_running_jobs_and_drops_old_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let account = account(&tmp);
        let day = 24 * 3600 * 1000;
        let now = 100 * day;
        let ancient_running = made(&account, now - 30 * day, None);
        let held = paths::try_lock_file(&file(&account.job_dir(&ancient_running), "lock"))
            .unwrap()
            .unwrap();
        let week_old = made(
            &account,
            now - 9 * day,
            Some((Status::Succeeded, now - 8 * day)),
        );
        let two_days = made(
            &account,
            now - 2 * day,
            Some((Status::Failed, now - 2 * day)),
        );
        let fresh = made(&account, now - 1000, Some((Status::Succeeded, now - 1000)));

        sweep(&account, now);
        assert!(account.job_dir(&ancient_running).join("out.json").exists());
        assert!(!account.job_dir(&week_old).exists());
        assert!(account.job_dir(&two_days).exists());
        assert!(!account.job_dir(&two_days).join("out.json").exists());
        assert!(account.job_dir(&fresh).join("out.json").exists());

        // Past the count, the oldest finished go and the running one stays.
        for n in 0..MOST_KEPT {
            made(
                &account,
                now - 500 + n as i64,
                Some((Status::Succeeded, now - 10)),
            );
        }
        sweep(&account, now);
        let left = all(&account, now).unwrap();
        assert_eq!(left.len(), MOST_KEPT);
        assert!(left.iter().any(|seen| seen.id == ancient_running));
        assert!(!left.iter().any(|seen| seen.id == two_days));
        drop(held);
    }

    #[test]
    fn stopping_writes_a_stop_file_into_every_job_that_is_not_over() {
        let tmp = tempfile::tempdir().unwrap();
        let account = account(&tmp);
        let now = snob_core::clock::now_ms().get();
        let queued = made(&account, now, None);
        let done = made(&account, now, Some((Status::Succeeded, now)));
        let stopping = Stopping::every_job_of(&account).unwrap();
        assert_eq!(stopping.ids, std::slice::from_ref(&queued));
        assert!(account.job_dir(&queued).join("stop").exists());
        assert!(!account.job_dir(&done).join("stop").exists());
        // A queued job nobody runs is not over until it is lost.
        assert!(!stopping.wait(Duration::from_millis(200)));
    }

    #[test]
    fn an_account_with_no_jobs_has_none_to_stop_and_gets_no_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let account = account(&tmp);
        let stopping = Stopping::every_job_of(&account).unwrap();
        assert!(stopping.ids.is_empty());
        assert!(stopping.wait(Duration::ZERO));
        assert!(!account.jobs_dir().exists());
    }
}
