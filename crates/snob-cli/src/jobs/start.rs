//! `snob __job start`: checks a walk, makes it a job, starts its supervisor,
//! and answers with **exactly one compact JSON document** on standard output.
//!
//! The document is the job's record (`show::record`), with `reused` beside
//! it, and the walk's answer under `finished` when it ended within `--wait`.
//! One document and nothing before it, so a caller reads it the way it reads
//! any command's output, once it has ended; a first line with the id would be
//! read as a document of its own.
//!
//! What it does, in order:
//! - checks the command (`argv::check`) and that a session is stored, which
//!   sends nothing (exit 3 without one);
//! - estimates the walk out of what is stored (`commands::dry_run`), which
//!   sends nothing either;
//! - under the account's jobs lock, so two starts at once make one job:
//!   sweeps (`jobs::sweep`); answers with the job already running when it
//!   runs the same command (`reused`), and refuses when it runs another
//!   (`too_many_jobs`: one at a time per account), or when it was asked to
//!   stop and has not yet (`job_stopping`: **a job on its way out is never
//!   handed to a new start**, which would be given a walk about to end
//!   `canceled`); refuses a walk of a list a lost job may still hold, saying
//!   until when (`walked_elsewhere`); and makes the job's directory and
//!   record;
//! - starts the supervisor, detached (`detach.rs`), with the global flags of
//!   this run that decide where its files are and nothing else
//!   (`argv::globals`); never `--stop-when-stdin-closes`, which this process
//!   may carry;
//! - waits up to 10 s for the supervisor, then up to `--wait` for the walk.
//!
//! **`--check` makes nothing**: past the command's and the session's checks
//! it answers `{"would": "start"}`, or `{"would": "reuse", "job_id": …}`, or
//! the refusal a start would meet now, reading the jobs and writing nothing,
//! and in a cooldown the refusal the walk itself would meet then (a refresh,
//! or nothing stored to serve). It is what the MCP server asks before it asks
//! the person for a yes, so nobody says yes to a walk a running job or a
//! cooldown then refuses.
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

/// What the account's jobs leave a start of a walk: nothing in its way, or
/// the job already running it to reuse ([`vet`]).
enum Found {
    Free,
    Reuse(JobId),
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
    if args.check {
        let account = account.clone();
        let found = tokio::task::spawn_blocking(move || {
            let now = snob_core::clock::now_ms();
            let found = vet(&super::all(&account, now.get())?, &checked, now.get())?;
            refused_in_cooldown(&account, &checked, now)?;
            Ok::<_, anyhow::Error>(found)
        })
        .await??;
        let out = match found {
            Found::Free => json!({ "would": "start" }),
            Found::Reuse(id) => json!({ "would": "reuse", "job_id": id.as_str() }),
        };
        crate::ui::say!("{}", serde_json::to_string(&out)?);
        return Ok(ExitCode::Ok);
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

/// Whether the walk `checked` may start now among the account's `jobs`, read
/// at `now`: the job already running it, to reuse; or a refusal, for another
/// job running, one asked to stop that has not stopped yet, or a lost job's
/// claim on a list it walks. It writes nothing: a start calls it under the
/// jobs lock, and `--check` without it.
fn vet(jobs: &[super::Seen], checked: &argv::Checked, now: i64) -> Result<Found> {
    if let Some(running) = jobs.iter().find(|seen| !seen.status.is_over()) {
        // On its way out: reused, it would hand this start a walk about to
        // end `canceled`, and another may not start beside it.
        if running.stop_asked {
            return Err(ExitError::new(
                ExitCode::Error,
                format!(
                    "{} ({}) was asked to stop and is stopping, and one job runs at a time; \
                     nothing was started",
                    running.id,
                    show::walks(running)
                ),
            )
            .with_reason("job_stopping")
            .with_hint(format!(
                "ask again once it has stopped, which \"snob jobs show {}\" tells",
                running.id
            ))
            .into());
        }
        if running
            .job
            .as_ref()
            .is_some_and(|other| same_walk(&other.argv, &checked.args))
        {
            return Ok(Found::Reuse(running.id.clone()));
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
    // useful answer.
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
    Ok(Found::Free)
}

/// For `--check`: the refusal the walk `checked` would meet now in a
/// cooldown, **reading only**: a refresh, an account never resolved here, or
/// a list with nothing stored, each refused as the walk's own run refuses it
/// (`engine::cooldown::stored`). With something stored, the walk serves it
/// and this answers nothing: the call is then asked about as any other, its
/// command carrying `-y`, which a question skipped would hand a walk with no
/// yes once the cooldown had lifted.
fn refused_in_cooldown(
    account: &AccountPaths,
    checked: &argv::Checked,
    now: snob_core::EpochMs,
) -> Result<()> {
    let (store, budget) = crate::commands::common::standing(account, now)?;
    let Some(held) = crate::commands::common::held_in(&budget, account, now) else {
        return Ok(());
    };
    let query = crate::commands::common::query(&checked.list.target, &checked.list.walk);
    for &kind in checked.kinds {
        crate::engine::cooldown::stored(&store, account.pk(), &query, kind, &held)?;
    }
    Ok(())
}

/// Whether two command lines walk the same list in the same way: the same
/// but for how a yes to them was given (`--confirmed-via`, `--confirmed-for`),
/// which shapes what is recorded, not what is walked. A yes given in a form
/// and one given in the chat are for the same walk.
fn same_walk(one: &[String], other: &[String]) -> bool {
    fn walk(line: &[String]) -> Vec<&str> {
        let mut kept = Vec::with_capacity(line.len());
        let mut tokens = line.iter();
        while let Some(token) = tokens.next() {
            if token.starts_with("--confirmed-") {
                // Its value, when it was given apart from the flag.
                if !token.contains('=') {
                    tokens.next();
                }
                continue;
            }
            kept.push(token.as_str());
        }
        kept
    }
    walk(one) == walk(other)
}

/// Under the account's jobs lock: sweeps, finds a job to reuse or a reason to
/// refuse ([`vet`]), or makes the job and starts its supervisor.
fn make(
    account: &AccountPaths,
    mut job: Job,
    checked: &argv::Checked,
    supervisor_globals: Vec<OsString>,
) -> Result<Made> {
    let _lock = super::lock_jobs(account)?;
    let now = snob_core::clock::now_ms().get();
    super::sweep(account, now);
    if let Found::Reuse(id) = vet(&super::all(account, now)?, checked, now)? {
        return Ok(Made::Reused(id));
    }

    let id = super::id::new(u64::try_from(now).unwrap_or(0))?;
    let dir = account.job_dir(&id);
    paths::create_private_dir(&dir)?;
    // The lock the supervisor holds, there before it is: a job whose lock
    // file is missing reads as nobody running it.
    super::private_marker(&super::file(&dir, "lock"))?;
    job.id = id.to_string();
    super::write(&dir, "job.json", &job)?;

    let mut args = supervisor_globals;
    args.extend([COMMAND.into(), "run".into(), id.as_str().into()]);
    // The program is this one, never a path read from anywhere (AGENTS.md,
    // "The MCP server": a supervisor runs itself).
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

#[cfg(test)]
mod tests {
    use super::*;

    const FOLLOWERS: [&str; 4] = ["followers", "--format", "json", "--no-interactive"];

    fn checked(line: &[&str]) -> argv::Checked {
        let line: Vec<String> = line.iter().map(|arg| arg.to_string()).collect();
        argv::check(&line).unwrap()
    }

    /// A job of the account, read as `status`, running `line`.
    fn seen(status: Status, stop_asked: bool, line: &[&str]) -> super::super::Seen {
        let id = super::super::id::new(1).unwrap();
        super::super::Seen {
            id: id.clone(),
            dir: std::path::PathBuf::new(),
            job: Some(Job {
                schema: SCHEMA,
                id: id.to_string(),
                created_at: 1,
                viewer_pk: snob_core::Pk::new(42),
                tool: None,
                arguments: Value::Null,
                argv: line.iter().map(|arg| arg.to_string()).collect(),
                globals: Vec::new(),
                estimate: None,
                detached: None,
                started_by: None,
                max_wait_seconds: None,
            }),
            state: None,
            held: true,
            stop_asked,
            status,
        }
    }

    fn reason(refused: Result<Found>) -> Option<&'static str> {
        match refused {
            Ok(_) => None,
            Err(e) => e.downcast_ref::<ExitError>().and_then(ExitError::reason),
        }
    }

    /// **A job asked to stop is never handed to a new start** of its walk,
    /// which would be given a walk about to end `canceled`; nor may another
    /// start beside it.
    #[test]
    fn a_job_asked_to_stop_is_never_reused() {
        for status in [Status::Queued, Status::Running] {
            let stopping = [seen(status, true, &FOLLOWERS)];
            assert_eq!(
                reason(vet(&stopping, &checked(&FOLLOWERS), 2)),
                Some("job_stopping")
            );
            assert_eq!(
                reason(vet(
                    &stopping,
                    &checked(&["following", "--format", "json"]),
                    2
                )),
                Some("job_stopping")
            );
        }
        // Once it has stopped, a new job may start.
        let stopped = [seen(Status::Canceled, true, &FOLLOWERS)];
        assert!(matches!(
            vet(&stopped, &checked(&FOLLOWERS), 2),
            Ok(Found::Free)
        ));
    }

    /// The same walk is the job already running it, however its yes was
    /// given; another walk is refused.
    #[test]
    fn the_same_walk_is_reused_whatever_its_yes_and_another_is_refused() {
        let running = [seen(
            Status::Running,
            false,
            &[
                "followers",
                "someone",
                "-y",
                "--format",
                "json",
                "--confirmed-via=elicitation",
                "--confirmed-for=test 0",
            ],
        )];
        for line in [
            &["followers", "someone", "-y", "--format", "json"][..],
            &[
                "followers",
                "someone",
                "-y",
                "--format",
                "json",
                "--confirmed-via=chat",
            ][..],
        ] {
            assert!(
                matches!(vet(&running, &checked(line), 2), Ok(Found::Reuse(_))),
                "{line:?}"
            );
        }
        assert_eq!(
            reason(vet(&running, &checked(&FOLLOWERS), 2)),
            Some("too_many_jobs")
        );
    }
}
