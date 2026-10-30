//! `snob __job run <id>`: the supervisor, which runs a job's walk and writes
//! what it came to.
//!
//! Started detached by `__job start` (`detach.rs`), with the null device for
//! its input and output, so nothing it does reaches whoever started the job.
//! It holds the job's `lock` for its whole life: a held lock is how every
//! other process knows the job is running (`jobs::derive`), and the
//! operating system lets go of it when the process ends, however it ends.
//!
//! **What it runs is checked here, again**, and is this binary
//! (`current_exe`): `job.json` names no program, and its command line passes
//! `argv::check` or the job ends `failed` with `refused_argv` and nothing run.
//! The global flags are this process's own (`argv::globals`), never read from
//! the record.
//!
//! The walk is run with `--spend-alone`, so it refuses while another agent's
//! run holds the account; `--stop-when-stdin-closes`, on a pipe this process
//! holds; and `--progress-json`, which `show` reads the walk's progress from.
//! Its output goes to `out.json.partial`, renamed `out.json` once it has
//! ended, and its errors to `err.log`.
//!
//! **Stopping is closing that pipe.** Every second it asks whether a `stop`
//! file is there, or whether it was itself asked to stop (`SIGTERM`, `SIGHUP`,
//! the session ending: `interrupt::install`); then it closes the walk's
//! standard input, which stops the walk as Ctrl+C does, its page saved and its
//! claim given back, and kills it through its own handle only after
//! [`GRACE`]. It never signals a process id it read from a file. **A walk it
//! killed ended as asked** ([`status_for`]), whatever exit the kill left it
//! with: none on Unix, 1 on Windows, where a kill is `TerminateProcess`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Result;
use snob_store::paths::{self, AccountPaths, JobId};

use super::{GRACE, HEARTBEAT, Job, SCHEMA, State, Status, argv, file, read, show, write};
use crate::cli::Cli;
use crate::exit::ExitCode;

/// How often it asks whether it should stop, and whether the walk ended.
const POLL: Duration = Duration::from_secs(1);

/// How long it tries to take the job's lock: a moment's holder is a reader
/// asking whether the job runs (`paths::lock_is_held`); a holder for longer is
/// another supervisor, and this one leaves it the job.
const LOCK_WITHIN: Duration = Duration::from_secs(5);

/// `CREATE_NO_WINDOW`: a console program started by a process with no
/// console of its own (this one is detached) is otherwise given a window.
#[cfg(windows)]
const NO_WINDOW: u32 = windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub async fn run(cli: &Cli, account: &AccountPaths, id: &str) -> Result<ExitCode> {
    let Some(id) = JobId::parse(id) else {
        return Err(show::no_such_job(id));
    };
    let dir = account.job_dir(&id);
    if !dir.is_dir() {
        return Err(show::no_such_job(id.as_str()));
    }
    let Some(_lock) = take_lock(&file(&dir, "lock")).await? else {
        tracing::warn!(job = %id, "another supervisor holds this job");
        return Ok(ExitCode::Error);
    };
    if read::<State>(&dir, "state.json").is_some_and(|state| state.status.is_over()) {
        return Ok(ExitCode::Ok);
    }

    let now = now();
    let mut state = State {
        status: Status::Running,
        started_at: now,
        updated_at: now,
        finished_at: None,
        exit_number: None,
        cancel: false,
        failure: None,
        supervisor_pid: Some(std::process::id()),
        child_pid: None,
    };
    let job: Option<Job> = read(&dir, "job.json");
    let checked = match &job {
        Some(job)
            if job.schema == SCHEMA && job.id == id.as_str() && job.viewer_pk == account.pk() =>
        {
            argv::check(&job.argv)
        }
        _ => Err("the job's record is not one this build runs".to_string()),
    };
    let checked = match checked {
        Ok(checked) => checked,
        Err(why) => {
            tracing::warn!(job = %id, %why, "the job's command line was refused");
            return end(&dir, state, Status::Failed, None, Some("refused_argv"));
        }
    };
    if file(&dir, "stop").exists() {
        state.cancel = true;
        return end(&dir, state, Status::Canceled, None, None);
    }
    write(&dir, "state.json", &state)?;

    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(argv::walk(cli, account.pk(), &checked.args)?)
        .stdin(Stdio::piped())
        .stdout(private(&file(&dir, "out.json.partial"), false)?)
        .stderr(private(&file(&dir, "err.log"), true)?);
    // The escape hatch from the cooldown is for a bug of ours, typed by a
    // person, and the MCP key is the server's; a job is an agent's.
    for name in super::NEVER_INHERITED {
        command.env_remove(name);
    }
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, NO_WINDOW);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(job = %id, error = %e, "the walk could not be started");
            return end(&dir, state, Status::Failed, None, Some("not_started"));
        }
    };
    // Its copies of the files, so the output can be renamed once the walk is
    // done with it: Windows renames no file a handle is open on.
    drop(command);
    state.child_pid = Some(child.id());
    write(&dir, "state.json", &state)?;

    let token = crate::interrupt::install();
    // Held, never written: dropping it is the request to stop.
    let mut stdin = child.stdin.take();
    let mut asked: Option<Instant> = None;
    let mut killed = false;
    let mut beat = Instant::now();
    let exited = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
        }
        let stop_file = file(&dir, "stop").exists();
        if asked.is_none() && (stop_file || token.is_canceled()) {
            state.cancel = stop_file;
            drop(stdin.take());
            asked = Some(Instant::now());
        }
        if asked.is_some_and(|at| at.elapsed() > GRACE) {
            killed = child.kill().is_ok();
            break child.wait()?.code();
        }
        if beat.elapsed() >= HEARTBEAT {
            state.updated_at = self::now();
            let _ = write(&dir, "state.json", &state);
            beat = Instant::now();
        }
        tokio::time::sleep(POLL).await;
    };
    // Asked to stop by a file the walk never saw, because it ended first,
    // is still a stop that was asked for.
    state.cancel = state.cancel || file(&dir, "stop").exists();

    let partial = file(&dir, "out.json.partial");
    if partial.exists() {
        paths::rename_patiently(&partial, &file(&dir, "out.json"))?;
    }
    let status = status_for(exited, killed, state.cancel);
    end(&dir, state, status, exited, None)
}

/// How a walk ended, from its exit, whether this supervisor killed it past
/// the grace, and whether its stop was asked for (`cancel`, the `stop`
/// file). A walk that finished is a success, whatever came after; one killed
/// was stopped, as it was asked to be, whatever the kill left it exiting
/// with, which differs by platform; otherwise 130 is a stop, and nothing at
/// all (a signal from elsewhere, on Unix) a stop only when one was asked for.
fn status_for(exited: Option<i32>, killed: bool, cancel: bool) -> Status {
    match exited {
        Some(0) => Status::Succeeded,
        _ if killed && cancel => Status::Canceled,
        _ if killed => Status::Stopped,
        Some(130) | None if cancel => Status::Canceled,
        Some(130) => Status::Stopped,
        _ => Status::Failed,
    }
}

/// Writes the job's ending. The lock is let go of after this, by the caller
/// returning.
fn end(
    dir: &std::path::Path,
    mut state: State,
    status: Status,
    exit_number: Option<i32>,
    failure: Option<&str>,
) -> Result<ExitCode> {
    let now = now();
    state.status = status;
    state.updated_at = now;
    state.finished_at = Some(now);
    state.exit_number = exit_number;
    state.failure = failure.map(String::from);
    write(dir, "state.json", &state)?;
    Ok(ExitCode::Ok)
}

fn now() -> i64 {
    snob_core::clock::now_ms().get()
}

/// The job's lock, tried for [`LOCK_WITHIN`]; `None` when somebody keeps it.
async fn take_lock(path: &std::path::Path) -> Result<Option<std::fs::File>> {
    let deadline = Instant::now() + LOCK_WITHIN;
    loop {
        if let Some(lock) = paths::try_lock_file(path)? {
            return Ok(Some(lock));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A file only this user can read, emptied first or appended to.
fn private(path: &std::path::Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A walk killed past its grace ended as it was asked to, on every
    /// platform: Windows leaves a killed process exiting 1, Unix with none.
    #[test]
    fn a_walk_killed_past_its_grace_ended_as_it_was_asked_to() {
        for left_with in [Some(1), None, Some(137)] {
            assert_eq!(status_for(left_with, true, true), Status::Canceled);
            assert_eq!(status_for(left_with, true, false), Status::Stopped);
        }
        // One that finished before the kill landed finished.
        assert_eq!(status_for(Some(0), true, true), Status::Succeeded);
        // Not killed, its exit decides.
        assert_eq!(status_for(Some(0), false, false), Status::Succeeded);
        assert_eq!(status_for(Some(130), false, true), Status::Canceled);
        assert_eq!(status_for(None, false, true), Status::Canceled);
        assert_eq!(status_for(Some(130), false, false), Status::Stopped);
        assert_eq!(status_for(Some(1), false, true), Status::Failed);
        assert_eq!(status_for(None, false, false), Status::Failed);
    }
}
