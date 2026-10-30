//! `snob __job start`: checks a walk, makes it a job, starts its supervisor,
//! and answers with **exactly one compact JSON document** on standard output.
//!
//! The document is the job's record (`show::record`), with `reused` beside
//! it, and the walk's answer under `finished` when it ended within `--wait`.
//! One document and nothing before it, so a caller reads it the way it reads
//! any command's output, once it has ended; a first line with the id would be
//! read as a document of its own (C2-8 in the plan's review).
//!
//! What it does, in order:
//! - checks the command (`argv::check`) and that a session is stored, which
//!   sends nothing (exit 3 without one);
//! - estimates the walk out of what is stored (`commands::dry_run`), which
//!   sends nothing either;
//! - under the account's jobs lock, so two starts at once make one job:
//!   sweeps (`jobs::sweep`); answers with the job already running when it
//!   runs the same command (`reused`), and refuses when it runs another
//!   (`too_many_jobs`: one at a time per account); refuses a walk of a list a
//!   lost job may still hold, saying until when (`walked_elsewhere`); and
//!   makes the job's directory and record;
//! - starts the supervisor, detached (`detach.rs`), with the global flags of
//!   this run that decide where its files are and nothing else
//!   (`argv::globals`); never `--stop-when-stdin-closes`, which this process
//!   may carry;
//! - waits up to 10 s for the supervisor, then up to `--wait` for the walk.
//!
//! **A job is left running only once its id has been handed over.** Asked to
//! stop before it has answered (Ctrl+C, or, under `--stop-when-stdin-closes`,
//! its caller closing its standard input because the tool call was canceled
//! or the client went away), it asks the job it made to stop, and exits 130
//! with nothing on standard output. A job it only found and reused is left
//! alone: its id was handed to whoever started it.

use std::ffi::OsString;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use snob_store::paths::{self, AccountPaths, JobId};
use snob_store::secrets::SecretStore;

use super::{Detached, Job, SCHEMA, Status, argv, show};
use crate::cli::{Cli, JobStartArgs};
use crate::exit::{ExitCode, ExitError};
use crate::report;

/// The hidden command a supervisor is started as.
pub const COMMAND: &str = "__job";

/// How long the start waits for the supervisor to take the job, before it
/// waits `--wait` for the walk (`commands::mcp::longest_call` counts both).
pub const SUPERVISOR_WITHIN: Duration = Duration::from_secs(10);

/// How long the start waits for the job it asks to stop to have stopped,
/// before it exits anyway; the job stops on its own either way.
const STOPPED_WITHIN: Duration = Duration::from_secs(10);

/// What the jobs lock was taken for.
enum Made {
    New(JobId),
    Reused(JobId),
}

fn refused(why: String) -> anyhow::Error {
    ExitError::new(ExitCode::Error, format!("{why}; nothing was started"))
        .with_reason("refused_argv")
        .into()
}

pub async fn run(
    cli: &Cli,
    args: &JobStartArgs,
    store: &SecretStore,
    account: &AccountPaths,
) -> Result<ExitCode> {
    let checked = argv::check(&args.command).map_err(refused)?;
    let arguments = match &args.arguments {
        None => json!({}),
        Some(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| refused("--arguments is not a JSON object".into()))?,
    };
    let secrets = store.session_of(account);
    match secrets.load()? {
        None => return Err(crate::commands::common::no_session()),
        Some(session) if session.ds_user_id != account.pk() => anyhow::bail!(
            "the session stored for account {} is account {}'s; log in again with \
             \"snob login\"",
            account.pk(),
            session.ds_user_id
        ),
        Some(_) => {}
    }
    // What it would cost, said with the job; nothing is sent to know it, and
    // a job is still started when it cannot be told.
    let estimate = crate::commands::dry_run::estimate_for(
        &checked.list.walk,
        &checked.list.target,
        &secrets,
        account,
        checked.kinds,
    )
    .ok()
    .map(|(_, estimate)| crate::commands::dry_run::as_json(&estimate));

    let globals = argv::globals(cli, account.pk())?;
    let now = snob_core::clock::now_ms().get();
    let job = Job {
        schema: SCHEMA,
        id: String::new(),
        created_at: now,
        viewer_pk: account.pk(),
        tool: args
            .tool
            .as_deref()
            .map(|tool| snob_core::model::printable(tool).chars().take(64).collect()),
        arguments,
        argv: checked.args.clone(),
        globals: globals
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        estimate,
        detached: None,
        started_by: args
            .started_by
            .as_deref()
            .and_then(|by| crate::commands::common::client_label(by, "")),
        max_wait_seconds: cli.max_wait.map(|wait| wait.as_secs()),
    };
    let supervisor = argv::supervisor(cli, account.pk())?;
    let made = {
        let account = account.clone();
        tokio::task::spawn_blocking(move || make(&account, job, &checked, supervisor)).await??
    };
    let (id, ours) = match made {
        Made::New(id) => (id, true),
        Made::Reused(id) => (id, false),
    };

    let token = crate::interrupt::install();
    let look = || -> Result<super::Seen> {
        super::look(account, &id, snob_core::clock::now_ms().get())?
            .ok_or_else(|| show::no_such_job(id.as_str()))
    };
    let mut seen = look()?;
    // The supervisor taking the job, then the walk ending.
    let taken: fn(Status) -> bool = |status| status != Status::Queued;
    for (within, enough) in [(SUPERVISOR_WITHIN, taken), (args.wait, Status::is_over)] {
        let deadline = tokio::time::Instant::now() + within;
        while !enough(seen.status) && tokio::time::Instant::now() < deadline {
            let pause = Duration::from_millis(100).min(deadline - tokio::time::Instant::now());
            if token.sleep_or_cancel(pause).await {
                if ours {
                    // Not handed over yet: nobody would ever ask for it.
                    super::ask_to_stop(&seen)?;
                    // Waited for without the token, which is canceled now
                    // and would end every wait at once.
                    let until = tokio::time::Instant::now() + STOPPED_WITHIN;
                    while !seen.status.is_over() && tokio::time::Instant::now() < until {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        seen = look()?;
                    }
                }
                return Ok(ExitCode::Interrupted);
            }
            seen = look()?;
        }
    }

    let mut out = show::whole(&seen, snob_core::clock::now_ms().get());
    out["reused"] = Value::Bool(!ours);
    crate::ui::say!("{}", serde_json::to_string(&out)?);
    Ok(ExitCode::Ok)
}

/// Under the account's jobs lock: sweeps, finds a job to reuse or a reason to
/// refuse, or makes the job and starts its supervisor.
fn make(
    account: &AccountPaths,
    mut job: Job,
    checked: &argv::Checked,
    supervisor_globals: Vec<OsString>,
) -> Result<Made> {
    let _lock = super::lock_jobs(account)?;
    let now = snob_core::clock::now_ms().get();
    super::sweep(account, now);
    let jobs = super::all(account, now)?;
    if let Some(running) = jobs.iter().find(|seen| !seen.status.is_over()) {
        if running
            .job
            .as_ref()
            .is_some_and(|other| other.argv == job.argv)
        {
            return Ok(Made::Reused(running.id.clone()));
        }
        return Err(ExitError::new(
            ExitCode::Error,
            format!(
                "a job is already running for this account ({}, {}), and one runs at a time; \
                 nothing was started",
                running.id,
                show::walks(running)
            ),
        )
        .with_reason("too_many_jobs")
        .with_hint("wait for it to finish, or stop it with \"snob jobs cancel\"")
        .into());
    }
    // A job that died without letting go of its list: another walk of the
    // list is refused until its claim runs out, and saying when is the one
    // useful answer (C1-11 in the plan's review).
    if let Some(lost) = jobs.iter().find(|seen| {
        seen.status == Status::Lost
            && seen.claim_frees_at() > now
            && seen
                .job
                .as_ref()
                .and_then(|other| argv::check(&other.argv).ok())
                .is_some_and(|other| argv::overlap(&other, checked))
    }) {
        let frees = snob_core::EpochMs::new(lost.claim_frees_at()).to_epoch_not_before();
        return Err(ExitError::new(
            ExitCode::Error,
            format!(
                "{} stopped without letting go of {}, which another walk may not read until \
                 {}; nothing was started",
                lost.id,
                show::walks(lost),
                report::stored_on(frees)
            ),
        )
        .with_reason("walked_elsewhere")
        .with_hint("ask again after that, and the walk continues where it stopped")
        .into());
    }

    let id = super::id::new(u64::try_from(now).unwrap_or(0))?;
    let dir = account.job_dir(&id);
    paths::create_private_dir(&dir)?;
    // The lock the supervisor holds, there before it is: a job whose lock
    // file is missing reads as nobody running it.
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(super::file(&dir, "lock"))?;
    job.id = id.to_string();
    super::write(&dir, "job.json", &job)?;

    let mut args = supervisor_globals;
    args.extend([COMMAND.into(), "run".into(), id.as_str().into()]);
    // The program is this one, never a path read from anywhere (C1-5).
    let exe = std::env::current_exe()?;
    let started = crate::detach::spawn(&exe, &args, None, &super::file(&dir, "supervisor.log"));
    let started = match started {
        Ok(started) => started,
        Err(e) => {
            // Nobody will run it, so its ending is written here: the one
            // state.json no supervisor writes.
            let _ = super::write(
                &dir,
                "state.json",
                &super::State {
                    status: Status::Failed,
                    started_at: now,
                    updated_at: now,
                    finished_at: Some(now),
                    exit_number: None,
                    cancel: false,
                    failure: Some("not_started".into()),
                    supervisor_pid: None,
                    child_pid: None,
                },
            );
            return Err(anyhow::Error::new(e).context("the job's supervisor could not be started"));
        }
    };
    job.detached = Some(if started.left_job {
        Detached::LeftJob
    } else {
        Detached::InClientJob
    });
    super::write(&dir, "job.json", &job)?;
    Ok(Made::New(id))
}
