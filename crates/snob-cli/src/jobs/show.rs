//! Reading jobs back: `snob jobs` and `snob __job show|list|cancel`.
//!
//! **Only the account's own jobs**, the account this run acts as: an id is
//! looked for in that account's jobs directory and nowhere else, so an agent
//! served for one account cannot read, or stop, a walk of another's
//! (AGENTS.md, "The MCP server", the jobs). An id of another account's, or
//! one that is not an id, is "no such job".
//!
//! Nothing here writes but `cancel`'s `stop` file, so `snob jobs` and
//! `snob status` keep the promise that reading costs nothing.
//!
//! The record is one JSON object, the same one `start` answers with, and the
//! walk's own answer, once there is one, under `finished`: its exit, its
//! output as it printed it, and the JSON error line it ended with
//! (`main::wording_for`), which is how the MCP server reads a walk it ran
//! itself (`commands::mcp::child`).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use snob_core::model::printable;
use snob_core::{Epoch, EpochMs};
use snob_store::paths::{AccountPaths, JobId};

use super::{Seen, Status, argv, file};
use crate::exit::{ExitCode, ExitError};
use crate::report;

/// How much of the end of `err.log` is read for the newest progress line.
const TAIL: u64 = 8 * 1024;

/// The most of the walk's standard error kept for a failure with no JSON
/// error line, as for a call's child (`commands::mcp::child`).
const SAID: usize = 4 * 1024;

/// The refusal for an id this account has no job of.
pub fn no_such_job(id: &str) -> anyhow::Error {
    ExitError::new(
        ExitCode::Error,
        format!(
            "there is no job \"{}\" for this account",
            printable(&id.chars().take(40).collect::<String>())
        ),
    )
    .with_reason("no_such_job")
    .with_hint("\"snob jobs\" lists them")
    .into()
}

/// The job named `id`, read now.
fn find(account: &AccountPaths, id: &str) -> Result<Seen> {
    let parsed = JobId::parse(id).ok_or_else(|| no_such_job(id))?;
    super::look(account, &parsed, now())?.ok_or_else(|| no_such_job(id))
}

fn now() -> i64 {
    snob_core::clock::now_ms().get()
}

/// Seconds, as every other document gives a moment.
fn seconds(ms: i64) -> Epoch {
    EpochMs::new(ms).to_epoch()
}

/// Whether `line` is a record of the log, as `main::init_tracing` writes
/// one (an optional time, a level, the spans and the module it came from,
/// each ending in a colon), and not a sentence a command wrote for its
/// reader. Such a record may carry what Instagram answered, and is no
/// answer to hand an agent or a person reading a job's failure. Colors, when
/// the log has them, are left out first.
pub(crate) fn is_log_record(line: &str) -> bool {
    let plain = without_colors(line);
    let mut words = plain.split_whitespace();
    let mut word = words.next();
    // The time `--verbose` puts first: `2026-10-30T05:01:02.123456Z`.
    if word.is_some_and(|w| w.starts_with(|c: char| c.is_ascii_digit())) {
        word = words.next();
    }
    if !word.is_some_and(|w| matches!(w, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")) {
        return false;
    }
    // Spans, `name{field=value}:`, before the module.
    let Some(target) = words.find(|w| !(w.contains('{') && w.ends_with(':'))) else {
        return false;
    };
    let Some(path) = target.strip_suffix(':') else {
        return false;
    };
    path.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && path
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == ':')
}

/// `line` without its terminal's color sequences (`ESC [ … letter`).
fn without_colors(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// What a command said, less the records of the log among it
/// ([`is_log_record`]): the lines its reader is meant to read. All of them
/// when nothing else is left, so a failure that said nothing else is not
/// told as having said nothing.
pub(crate) fn sentences(lines: Vec<&str>) -> Vec<&str> {
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !is_log_record(line))
        .collect();
    if kept.is_empty() { lines } else { kept }
}

/// The newest progress line a walk wrote (`--progress-json`), from the end of
/// its log: the object under `progress`. Not a `served` line, which says
/// where a list came from once it is done with (`finished` reads those), and
/// nothing of how far the walk is.
pub fn newest_progress(dir: &Path) -> Option<Value> {
    let tail = tail_of(&file(dir, "err.log"), TAIL)?;
    tail.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| value.get("progress").cloned())
        .find(|progress| progress["event"] != "served")
}

/// The last `bytes` of a file, as text.
fn tail_of(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(bytes)))
        .ok()?;
    let mut read = Vec::new();
    file.read_to_end(&mut read).ok()?;
    Some(String::from_utf8_lossy(&read).into_owned())
}

/// How long the walk has left, the fewest and the most seconds, from where
/// its newest progress line puts it: the pages left of the list it is on,
/// timed by the pace alone (`engine::estimate::walk_seconds`), and what is
/// left of a wait it is in. Before its first line, the estimate it was
/// started with, less the time since. `None` when there is nothing to tell it
/// by, which is also the case between the two lists of a crossing.
pub fn eta(seen: &Seen, progress: Option<&Value>, now: i64) -> Option<(u64, u64)> {
    if seen.status.is_over() {
        return None;
    }
    let Some(progress) = progress else {
        let estimate = seen.job.as_ref()?.estimate.as_ref()?;
        let since = seen
            .state
            .as_ref()
            .map_or(seen.created_at(), |s| s.started_at);
        let gone = u64::try_from(now.saturating_sub(since) / 1000).unwrap_or(0);
        let least = estimate["seconds"]["least"].as_u64()?;
        let most = estimate["seconds"]["most"].as_u64()?;
        return Some((least.saturating_sub(gone), most.saturating_sub(gone)));
    };
    if progress["event"] == "finished" {
        return None;
    }
    let of = progress["of"].as_u64()?;
    let read = progress["accounts"].as_u64().unwrap_or(0);
    let pages = of
        .saturating_sub(read)
        .div_ceil(u64::from(snob_ig::pace::ACCOUNTS_PER_PAGE));
    let into_sitting =
        progress["page"].as_u64().unwrap_or(0) % u64::from(snob_ig::pace::PAGES_PER_SITTING);
    let waited = progress["at"]
        .as_i64()
        .map_or(0, |at| u64::try_from(now.saturating_sub(at)).unwrap_or(0));
    let wait_left = if progress["event"] == "waiting" {
        (progress["seconds"].as_u64().unwrap_or(0) * 1000).saturating_sub(waited)
    } else {
        0
    };
    // A rest is what the walk owes before its next page, which the pace
    // already counts; any other wait is on top of it.
    let (rest, other) = if progress["kind"] == "sitting" {
        (wait_left, 0)
    } else {
        (0, wait_left.div_ceil(1000))
    };
    let (least, most) = crate::engine::estimate::walk_seconds((pages, pages), into_sitting, rest);
    Some((least + other, most + other))
}

/// The record of a job, as one JSON object.
pub fn record(seen: &Seen, now: i64) -> Value {
    let job = seen.job.as_ref();
    let state = seen.state.as_ref();
    let checked = job.and_then(|job| argv::check(&job.argv).ok());
    let progress = newest_progress(&seen.dir);
    let eta = eta(seen, progress.as_ref(), now);
    json!({
        "job_id": seen.id.as_str(),
        "status": seen.status.as_str(),
        "command": checked.as_ref().map(|c| c.command),
        "account": checked.as_ref().and_then(|c| c.target.as_deref()).map(printable),
        "tool": job.and_then(|job| job.tool.as_deref()),
        "arguments": job.map_or(Value::Null, |job| job.arguments.clone()),
        "created_at": seconds(seen.created_at()),
        "started_at": state.map(|s| seconds(s.started_at)),
        "updated_at": state.map(|s| seconds(s.updated_at)),
        "finished_at": state.and_then(|s| s.finished_at).map(seconds),
        "exit_number": state.and_then(|s| s.exit_number),
        "failure": state.and_then(|s| s.failure.as_deref()),
        "stop_asked": seen.stop_asked,
        "estimate": job.and_then(|job| job.estimate.clone()),
        "progress": progress,
        "eta_seconds": eta.map(|(least, most)| json!({ "least": least, "most": most })),
        "detached": job.and_then(|job| job.detached).map(super::Detached::as_str),
        "started_by": job.and_then(|job| job.started_by.as_deref()).map(printable),
        // Until when a lost job may still be holding the list it walked, so
        // another walk of it is refused (`start`).
        "claim_frees_at": (seen.status == Status::Lost).then(|| seconds(seen.claim_frees_at())),
    })
}

/// What the walk answered, once it has ended: its exit, what it printed and
/// the JSON error line it ended with, as a call's child is read
/// (`commands::mcp::child`), and `served`, where each list it answered with
/// came from (`Progress::served`: a walk, or what is stored, and when that
/// was read). `output` is `null` a day after the walk ended, swept or not
/// (`jobs::sweep`).
pub fn finished(seen: &Seen) -> Option<Value> {
    if !seen.status.finished() {
        return None;
    }
    let exit_number = seen.state.as_ref().and_then(|s| s.exit_number);
    let exit = match exit_number {
        Some(number) => crate::exit::outcome_of(number).unwrap_or(ExitCode::Error),
        None => ExitCode::Error,
    };
    // Past its keeping it is gone, whether or not a sweep has come by yet
    // (`snob status` sweeps nothing): the day is what is promised.
    let past = snob_core::clock::now_ms()
        .get()
        .saturating_sub(seen.ended_at())
        > super::KEEP_RESULT.as_millis() as i64;
    let output = std::fs::read(file(&seen.dir, "out.json"))
        .ok()
        .filter(|_| !past)
        .map(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            serde_json::from_str::<Value>(text.trim())
                .unwrap_or_else(|_| Value::String(text.into()))
        });
    let log = tail_of(&file(&seen.dir, "err.log"), 4 * TAIL).unwrap_or_default();
    // Where each list came from, as the walk said it (`Progress::served`).
    let served: Vec<Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| value.get("progress").cloned())
        .filter(|progress| progress["event"] == "served")
        .collect();
    let lines = sentences(
        log.lines()
            .filter(|line| !line.trim().is_empty())
            .filter(|line| {
                serde_json::from_str::<Value>(line.trim())
                    .ok()
                    .is_none_or(|value| value.get("progress").is_none())
            })
            .collect(),
    );
    let error = lines
        .last()
        .and_then(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|value| value.get("error").is_some());
    let mut said = lines.join("\n");
    if let Some(from) = said.len().checked_sub(SAID) {
        let from = (from..said.len())
            .find(|i| said.is_char_boundary(*i))
            .unwrap_or(said.len());
        said = said[from..].to_string();
    }
    Some(json!({
        "exit": exit.as_str(),
        "exit_number": exit_number,
        "output": output,
        "error": error,
        "said": said,
        "served": served,
    }))
}

/// The record with the walk's answer under `finished` when there is one.
pub fn whole(seen: &Seen, now: i64) -> Value {
    let mut out = record(seen, now);
    out["finished"] = finished(seen).unwrap_or(Value::Null);
    out
}

/// Waits, at most `wait`, for the job to be over, and reads it again.
pub async fn settle(account: &AccountPaths, seen: Seen, wait: Duration) -> Result<Seen> {
    let deadline = tokio::time::Instant::now() + wait;
    let token = crate::interrupt::install();
    let mut seen = seen;
    while !seen.status.is_over() && tokio::time::Instant::now() < deadline {
        if token
            .sleep_or_cancel(Duration::from_millis(250).min(deadline - tokio::time::Instant::now()))
            .await
        {
            break;
        }
        seen =
            super::look(account, &seen.id, now())?.ok_or_else(|| no_such_job(seen.id.as_str()))?;
    }
    Ok(seen)
}

/// `show <id>`: the job, and its result once it has one.
pub async fn show(
    account: &AccountPaths,
    id: &str,
    wait: Duration,
    json: bool,
) -> Result<ExitCode> {
    let seen = settle(account, find(account, id)?, wait).await?;
    let now = now();
    if json {
        crate::ui::say!("{}", serde_json::to_string(&whole(&seen, now))?);
    } else {
        crate::ui::say!("{}", line(&seen, now));
        if let Some(finished) = finished(&seen) {
            crate::ui::say!(
                "  It exited {}.",
                finished["exit"].as_str().unwrap_or("error")
            );
        }
    }
    Ok(ExitCode::Ok)
}

/// `list`: the jobs not yet finished, or every one kept with `all`.
pub fn list(account: &AccountPaths, all: bool, json: bool) -> Result<ExitCode> {
    let now = now();
    let jobs: Vec<Seen> = super::all(account, now)?
        .into_iter()
        .filter(|seen| all || !seen.status.finished())
        .collect();
    if json {
        let records: Vec<Value> = jobs.iter().map(|seen| record(seen, now)).collect();
        crate::ui::say!("{}", serde_json::to_string(&json!({ "jobs": records }))?);
    } else if jobs.is_empty() {
        crate::ui::say!(
            "{}",
            if all {
                "No jobs are kept for this account."
            } else {
                "No job is running for this account."
            }
        );
    } else {
        for seen in &jobs {
            crate::ui::say!("{}", line(seen, now));
        }
    }
    Ok(ExitCode::Ok)
}

/// `cancel <id>`: asks the job to stop, as Ctrl+C stops a walk, keeping what
/// it read, and waits at most `wait` for it to have.
pub async fn cancel(
    account: &AccountPaths,
    id: &str,
    wait: Duration,
    json: bool,
) -> Result<ExitCode> {
    let seen = find(account, id)?;
    let asked = !seen.status.is_over();
    if asked {
        super::ask_to_stop(&seen)?;
    }
    let seen = settle(account, seen, wait).await?;
    let now = now();
    if json {
        crate::ui::say!("{}", serde_json::to_string(&whole(&seen, now))?);
    } else if asked {
        crate::ui::say!("{} was asked to stop; it keeps what it read.", seen.id);
        crate::ui::say!("{}", line(&seen, now));
    } else {
        crate::ui::say!("{}", line(&seen, now));
    }
    Ok(ExitCode::Ok)
}

/// What a job walks, in words: "followers of @someone", "your unfollowers".
pub fn walks(seen: &Seen) -> String {
    let checked = seen
        .job
        .as_ref()
        .and_then(|job| argv::check(&job.argv).ok());
    match checked {
        Some(checked) => match &checked.target {
            Some(who) => format!("{} of @{}", checked.command, printable(who)),
            None => format!("your {}", checked.command),
        },
        None => "a walk this build does not read".to_string(),
    }
}

/// One line about a job, for a person.
pub fn line(seen: &Seen, now: i64) -> String {
    let mut line = format!(
        "{}  {:<9}  {}, since {}",
        seen.id,
        seen.status.as_str(),
        walks(seen),
        report::stored_on(seconds(seen.created_at()))
    );
    if let Some(by) = seen.job.as_ref().and_then(|job| job.started_by.as_deref()) {
        line.push_str(&format!(", started by {}", printable(by)));
    }
    if let Some(progress) = newest_progress(&seen.dir)
        && !seen.status.is_over()
        && let Some(read) = progress["accounts"].as_u64()
    {
        match progress["of"].as_u64() {
            Some(of) => line.push_str(&format!("; {read} of about {of} read")),
            None => line.push_str(&format!("; {read} read")),
        }
    }
    if let Some((least, most)) = eta(seen, newest_progress(&seen.dir).as_ref(), now) {
        line.push_str(&format!("; {} left", report::time_left(least, most)));
    }
    if seen.status == Status::Lost {
        line.push_str(&format!(
            "; its walk lets go of the list by {}",
            report::stored_on(seconds(seen.claim_frees_at()))
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The records of the log are told from the sentences around them,
    /// with or without a time, spans or colors.
    #[test]
    fn a_log_record_is_told_from_a_sentence() {
        for record in [
            " WARN snob_store::store::rate_budget: account in cooldown reason=\"rate_limit\"",
            " WARN snob_ig::client: Instagram pushed back status=429 body={\"message\":\"x\"}",
            "ERROR snob_cli::owner: gone",
            "2026-10-30T05:01:02.123456Z DEBUG snob_ig::pager: page 3",
            "  INFO walk{list=followers}: snob_ig::pager: page",
            "\u{1b}[33m WARN\u{1b}[0m \u{1b}[2msnob_ig::client\u{1b}[0m\u{1b}[2m:\u{1b}[0m pushed",
        ] {
            assert!(is_log_record(record), "{record}");
        }
        for sentence in [
            "warning: Instagram asked to slow down; the account is in cooldown for 2 hours",
            "error: the account is in cooldown until Oct 30 at 05:01",
            "WARN this is not a module",
            "hint: run it again later",
            "",
        ] {
            assert!(!is_log_record(sentence), "{sentence}");
        }
        assert_eq!(
            sentences(vec![" WARN snob_ig::client: x", "warning: the sentence"]),
            ["warning: the sentence"]
        );
        assert_eq!(
            sentences(vec![" WARN snob_ig::client: x"]),
            [" WARN snob_ig::client: x"],
            "all of it, when nothing else is left"
        );
    }

    #[test]
    fn the_newest_progress_line_is_read_from_the_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("err.log");
        let mut text = String::new();
        // Far more than the tail, so only the end can have been read.
        for page in 0..2000 {
            text.push_str(&format!(
                "{{\"progress\":{{\"event\":\"page\",\"page\":{page},\"accounts\":{}}}}}\n",
                page * 12
            ));
        }
        text.push_str("warning: a sentence between two lines\n");
        text.push_str(
            "{\"progress\":{\"event\":\"waiting\",\"kind\":\"sitting\",\"seconds\":300}}\n",
        );
        // Where a list came from, which says nothing of how far a walk is.
        text.push_str("{\"progress\":{\"event\":\"served\",\"from\":\"walked\"}}\n");
        text.push_str("{\"error\":{\"code\":\"rate_limited\"}}\n");
        std::fs::write(&log, text).unwrap();
        let newest = newest_progress(tmp.path()).expect("a line is there");
        assert_eq!(newest["event"], "waiting");
        assert_eq!(newest["seconds"], 300);
        assert_eq!(newest_progress(&tmp.path().join("nowhere")), None);
    }
}
