//! A list command's `--background`: the walk as a job, started by a person.
//!
//! What an agent's `snob_list` does through `snob __job start`, for somebody
//! at a terminal: the walk runs in a supervisor that outlives this command
//! (`jobs`), and this answers at once with the job's id. **The rules are the
//! job's**: one job at a time per account (`too_many_jobs`, or the same walk
//! already running handed back), the command line checked against what a
//! job may run (`jobs::argv`), and the walk's own budget, cooldown and
//! pacing, which it meets as any walk does.
//!
//! **Consent is asked here, before the job is made**, in the words and
//! order every list command asks it (`engine::ask_consent_with`, before
//! the account is resolved, so a no spends nothing), since nobody is at the
//! job's terminal to ask; the job's command then carries the `-y` that yes
//! is. With no terminal and no `-y` it is refused as any walk would be, and
//! no job is made.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::Result;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{Cli, JobStartArgs, ListArgs};
use crate::exit::ExitCode;
use crate::{engine, ui};

/// Starts `command` (one of the five list commands) with `args` as a job.
pub async fn run(
    cli: &Cli,
    command: &'static str,
    args: &ListArgs,
    store: &SecretStore,
    account: &AccountPaths,
) -> Result<ExitCode> {
    {
        // The App reads the session and the store and sends nothing; it is
        // dropped before the job is made, so nothing of this process's is
        // held while the walk runs.
        let mut app = super::common::open(&args.walk, &store.session_of(account), account)?;
        engine::ask_consent_with(&mut app, &engine::ListQuery::from(args), ui::can_be_asked())
            .await?;
    }

    let start = JobStartArgs {
        wait: Duration::ZERO,
        tool: None,
        arguments: None,
        started_by: None,
        check: false,
        command: job_command(command, args),
    };
    let Some(record) = crate::jobs::start::start(cli, &start, store, account).await? else {
        return Ok(ExitCode::Interrupted);
    };

    if !std::io::stdout().is_terminal() {
        ui::say!("{}", serde_json::to_string(&record)?);
        return Ok(ExitCode::Ok);
    }
    let id = record["job_id"].as_str().unwrap_or_default();
    ui::say!(
        "{} {id}.",
        if record["reused"] == true {
            "That walk is already running, as job"
        } else {
            "Started job"
        }
    );
    ui::say!(
        "\"snob jobs show {id}\" says how far it is and, once it ends, what it found; \
         \"snob jobs cancel {id}\" stops it, keeping what it read."
    );
    Ok(ExitCode::Ok)
}

/// The job's command line: the list command, the account, and the flags a
/// job takes that were given (`--background` refuses every other one at the
/// parser), with `-y` for a yes asked above or typed. No `--format`: a
/// job's walk prints JSON, which is what `snob jobs show` hands back.
fn job_command(command: &str, args: &ListArgs) -> Vec<String> {
    let mut line = vec![command.to_string()];
    if let Some(target) = &args.target {
        line.push(target.clone());
        // Asked and answered above, or answered in advance.
        line.push("-y".into());
    } else if args.walk.consent.yes {
        line.push("-y".into());
    }
    if args.walk.refresh {
        line.push("--refresh".into());
    }
    if args.walk.offline {
        line.push("--offline".into());
    }
    if let Some(limit) = args.limit {
        line.push(format!("--limit={limit}"));
    }
    if let Some(pages) = args.walk.max_pages {
        line.push(format!("--max-pages={pages}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn list_args(line: &[&str]) -> ListArgs {
        match Cli::try_parse_from(line).unwrap().command {
            crate::cli::Command::Followers(args) => args,
            _ => panic!("not followers"),
        }
    }

    /// What it builds is what a job may run, and carries the yes.
    #[test]
    fn the_job_runs_the_walk_that_was_typed_with_the_yes_that_was_given() {
        let args = list_args(&[
            "snob",
            "followers",
            "someone",
            "--background",
            "--refresh",
            "--limit",
            "5",
        ]);
        let line = job_command("followers", &args);
        assert_eq!(
            line,
            ["followers", "someone", "-y", "--refresh", "--limit=5"]
        );
        assert!(crate::jobs::argv::check(&line).is_ok());
    }

    #[test]
    fn a_flag_a_job_would_drop_is_refused_beside_background() {
        for flag in [&["-o", "out.csv"][..], &["--max-age", "1h"], &["--dry-run"]] {
            let mut line = vec!["snob", "followers", "--background"];
            line.extend_from_slice(flag);
            assert!(Cli::try_parse_from(&line).is_err(), "{flag:?}");
        }
    }
}
