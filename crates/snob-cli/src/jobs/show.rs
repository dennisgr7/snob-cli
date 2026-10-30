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

use std::io::{BufRead, Read, Seek, SeekFrom};
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

/// The lists a walk has answered with so far, by name, from the `served`
/// lines of its log ([`served_lines`]): for a crossing, the lists it is
/// done with.
pub fn served_lists(dir: &Path) -> Vec<String> {
    served_lines(dir)
        .into_iter()
        .filter_map(|progress| progress["list"].as_str().map(str::to_string))
        .collect()
}

/// Every `served` line of the walk's log, oldest first, the object under
/// `progress` (`Progress::served`). **Read through the whole log, not its
/// tail**: a crossing's first list says it before the second list's pages,
/// a line each, which a walk that waits out the day's accounts may write
/// by the thousand, and a tail would then lose the first list, which the
/// time left would count again and `finished` would not say was stored. A
/// line at a time, keeping only those, so what is held is what they are.
fn served_lines(dir: &Path) -> Vec<Value> {
    let Ok(log) = std::fs::File::open(file(dir, "err.log")) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(log);
    let mut line = Vec::new();
    let mut served = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Most lines are pages, which need no parsing to be passed over.
        if !line.windows(8).any(|w| w == b"\"served\"") {
            continue;
        }
        if let Some(progress) = serde_json::from_slice::<Value>(&line)
            .ok()
            .and_then(|value| value.get("progress").cloned())
            .filter(|progress| progress["event"] == "served")
        {
            served.push(progress);
        }
    }
    served
}

/// How long the walk has left, the fewest and the most seconds, from where
/// its newest progress line puts it, timed by the pace alone
/// (`engine::estimate::walks_seconds`):
///
/// - the pages left of the list it is on, **no more than its `--max-pages`
///   and `HARD_PAGE_CAP` let it read**, and **of the lists of a crossing it
///   has not reached**, from the estimate it was started with (a list that
///   may be reused counts none at least; one of a size not known when it
///   started, at least a page, and at most as many as the list under way),
///   each with its dwell;
/// - **from where the account's sitting is**, as the newest line says it
///   (`sitting`, `rest_until`), not from where the walk began: a walk that
///   starts on a sitting earlier work left part full rests sooner;
/// - and what is left of a wait it is in.
///
/// Before its first line, and before its first page while the lines do not
/// yet say where the sitting is, the estimate it was started with, which
/// read the sitting, less the time since. `None` when there is nothing to
/// tell it by. `served` names the lists it is done with ([`served_lists`]).
pub fn eta(
    seen: &Seen,
    progress: Option<&Value>,
    served: &[String],
    now: i64,
) -> Option<(u64, u64)> {
    if seen.status.is_over() {
        return None;
    }
    let job = seen.job.as_ref();
    let estimate = job.and_then(|job| job.estimate.as_ref());
    let from_estimate = || {
        let estimate = estimate?;
        let since = seen
            .state
            .as_ref()
            .map_or(seen.created_at(), |s| s.started_at);
        let gone = u64::try_from(now.saturating_sub(since) / 1000).unwrap_or(0);
        let least = estimate["seconds"]["least"].as_u64()?;
        let most = estimate["seconds"]["most"].as_u64()?;
        Some((least.saturating_sub(gone), most.saturating_sub(gone)))
    };
    let Some(progress) = progress else {
        return from_estimate();
    };
    // The most pages a walk of one list reads in this run, as the pager
    // stops it (`ListRequest::max_pages`, `HARD_PAGE_CAP`); a crossing walks
    // each of its lists with the same.
    let cap = u64::from(
        job.and_then(|job| argv::check(&job.argv).ok())
            .and_then(|checked| checked.list.walk.max_pages)
            .map_or(snob_ig::pager::HARD_PAGE_CAP, |most| {
                most.min(snob_ig::pager::HARD_PAGE_CAP)
            }),
    );
    let per_page = u64::from(snob_ig::pace::ACCOUNTS_PER_PAGE);
    // The pages of the list under way, from its size: what a list of a size
    // not known when the walk started is guessed at most, since an
    // account's two lists are more often than not of an order.
    let under_way = progress["of"]
        .as_u64()
        .map_or(0, |of| of.div_ceil(per_page).min(cap));
    let current = progress["list"].as_str();
    // The lists of the estimate not yet reached: neither answered with nor
    // the one under way. A crossing's estimate names both, in the order they
    // are read.
    let to_come: Vec<(u64, u64)> = estimate
        .and_then(|estimate| estimate["lists"].as_array())
        .into_iter()
        .flatten()
        .filter(|list| {
            let name = list["list"].as_str();
            name != current && !name.is_some_and(|name| served.iter().any(|s| s == name))
        })
        .filter_map(|list| {
            let least = list["pages"]["least"].as_u64()?;
            let most = list["pages"]["most"].as_u64()?;
            match list["fate"].as_str()? {
                "walked" => Some((least, most)),
                "reused_unless_moved" => Some((0, most)),
                // Never read here when the walk started (no counter
                // stored), so the estimate counted its first page alone:
                // that at least, and at most the list under way's.
                "unknown" => Some((least, most.max(under_way))),
                _ => None,
            }
        })
        .collect();
    let page = progress["page"].as_u64().unwrap_or(0);
    let pages = if progress["event"] == "finished" {
        // Between two lists: nothing left of the one that ended.
        if to_come.is_empty() {
            return None;
        }
        0
    } else {
        let of = progress["of"].as_u64()?;
        let read = progress["accounts"].as_u64().unwrap_or(0);
        // The walk stops at its cap whatever the list's size.
        of.saturating_sub(read)
            .div_ceil(per_page)
            .min(cap.saturating_sub(page))
    };
    let waited = progress["at"]
        .as_i64()
        .map_or(0, |at| u64::try_from(now.saturating_sub(at)).unwrap_or(0));
    let wait_left = if progress["event"] == "waiting" {
        (progress["seconds"].as_u64().unwrap_or(0) * 1000).saturating_sub(waited)
    } else {
        0
    };
    let full = u64::from(snob_ig::pace::PAGES_PER_SITTING);
    // A rest is what the walk owes before its next page, which the pace
    // already counts; any other wait is on top of it. A rest the sitting
    // owes and the walk has not begun to wait is the sitting's own.
    let owed = progress["rest_until"].as_i64().map_or(0, |until| {
        u64::try_from(until.saturating_sub(now)).unwrap_or(0)
    });
    let (rest, other) = if progress["kind"] == "sitting" {
        (wait_left, 0)
    } else {
        (owed, wait_left.div_ceil(1000))
    };
    // Where the account's sitting is, a line first says once a page has
    // left it (`Event::Page`): before the walk's first page, `started` and
    // the waits before it say nothing, and a sitting earlier work left part
    // full would be timed as a fresh one. The estimate read the sitting, so
    // it is what tells, with a wait for the budget on top, which it leaves
    // out (a rest owed is in it).
    if progress["sitting"].is_null()
        && page == 0
        && progress["event"] != "finished"
        && let Some((least, most)) = from_estimate()
    {
        return Some((least + other, most + other));
    }
    // The account's sitting, as the newest page left it; an older line that
    // does not say it is read as the walk's own pages. A full one whose rest
    // is over is a new sitting.
    let into_sitting = match progress["sitting"].as_u64() {
        Some(pages) if pages >= full && rest == 0 => 0,
        Some(pages) => pages,
        None => page % full,
    };
    let mut walks = vec![(pages, pages)];
    walks.extend(to_come);
    let (least, most) = crate::engine::estimate::walks_seconds(&walks, into_sitting, rest);
    Some((least + other, most + other))
}

/// The record of a job, as one JSON object.
pub fn record(seen: &Seen, now: i64) -> Value {
    let job = seen.job.as_ref();
    let state = seen.state.as_ref();
    let checked = job.and_then(|job| argv::check(&job.argv).ok());
    let progress = newest_progress(&seen.dir);
    let eta = eta(seen, progress.as_ref(), &served_lists(&seen.dir), now);
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
        // another walk of it is refused (`start`); null for a lost job that
        // never started its walk, whose lists are free.
        "claim_frees_at": seen.claim_frees_at().map(seconds),
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
    // Where each list came from, as the walk said it, from the whole log.
    let served = served_lines(&seen.dir);
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
    let served = served_lists(&seen.dir);
    if let Some((least, most)) = eta(seen, newest_progress(&seen.dir).as_ref(), &served, now) {
        line.push_str(&format!("; {} left", report::time_left(least, most)));
    }
    if let Some(frees_at) = seen.claim_frees_at() {
        line.push_str(&format!(
            "; its walk lets go of the list by {}",
            report::stored_on(seconds(frees_at))
        ));
    } else if seen.status == Status::Lost {
        line.push_str("; its walk never started, and its lists are free");
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

    /// A running job of a crossing whose estimate names `lists`.
    fn crossing(lists: Value) -> Seen {
        let now = snob_core::clock::now_ms().get();
        Seen {
            id: JobId::parse("j01M661CR224ZNFPRANE9AWWZDM").unwrap(),
            dir: std::path::PathBuf::from("nowhere"),
            job: Some(super::super::Job {
                schema: super::super::SCHEMA,
                id: "j01M661CR224ZNFPRANE9AWWZDM".into(),
                created_at: now,
                viewer_pk: snob_core::Pk::new(42),
                tool: None,
                arguments: Value::Null,
                argv: vec![],
                globals: vec![],
                estimate: Some(json!({ "lists": lists })),
                detached: None,
                started_by: None,
                max_wait_seconds: None,
            }),
            state: None,
            held: true,
            stop_asked: false,
            status: Status::Running,
        }
    }

    /// The time a walk takes, by the pace alone, from a fresh sitting.
    fn walking(pages: &[(u64, u64)], into_sitting: u64) -> (u64, u64) {
        crate::engine::estimate::walks_seconds(pages, into_sitting, 0)
    }

    /// In the first list of a crossing, the time left counts the second
    /// list too, which it has not reached; once the first is answered with,
    /// only the second.
    #[test]
    fn a_crossing_in_its_first_list_counts_the_second() {
        let now = snob_core::clock::now_ms().get();
        let seen = crossing(json!([
            {"list": "followers", "fate": "walked", "pages": {"least": 20, "most": 20}},
            {"list": "following", "fate": "walked", "pages": {"least": 30, "most": 30}},
        ]));
        // Ten pages into the first list of 240 accounts: ten left of it.
        let line = json!({
            "event": "page", "at": now, "page": 10, "accounts": 120, "of": 240,
            "list": "followers", "sitting": 10, "rest_until": null,
        });
        let (least, most) = eta(&seen, Some(&line), &[], now).unwrap();
        assert_eq!((least, most), walking(&[(10, 10), (30, 30)], 10));
        let alone = walking(&[(10, 10)], 10);
        assert!(most > alone.1, "{most} against {alone:?}");

        // Between the lists, the first answered with: the second alone.
        let finished = json!({
            "event": "finished", "at": now, "page": 20, "accounts": 240, "of": 240,
            "list": "followers", "sitting": 20, "rest_until": null,
        });
        let served = ["followers".to_string()];
        assert_eq!(
            eta(&seen, Some(&finished), &served, now).unwrap(),
            walking(&[(0, 0), (30, 30)], 20)
        );
    }

    /// A walk that began on a sitting earlier work left part full rests
    /// sooner than its own pages say: the time left counts from the
    /// account's sitting, which the newest page line carries.
    #[test]
    fn a_part_full_sitting_brings_the_rest_forward() {
        let now = snob_core::clock::now_ms().get();
        let seen = crossing(json!([
            {"list": "followers", "fate": "walked", "pages": {"least": 20, "most": 20}},
        ]));
        // Its own fifth page, the sitting's thirty-fifth: twenty pages left,
        // and a rest after five of them.
        let line = json!({
            "event": "page", "at": now, "page": 5, "accounts": 60, "of": 300,
            "list": "followers", "sitting": 35, "rest_until": null,
        });
        let (_, most) = eta(&seen, Some(&line), &[], now).unwrap();
        let from_its_own_pages = walking(&[(20, 20)], 5).1;
        let longest_rest = snob_ig::pace::SITTING_PAUSE_MS.1 / 1000;
        assert!(
            most >= from_its_own_pages + longest_rest - 1,
            "{most} against {from_its_own_pages}"
        );

        // A full sitting whose rest is owed counts what is left of it.
        let full = json!({
            "event": "page", "at": now, "page": 5, "accounts": 60, "of": 300,
            "list": "followers", "sitting": 40, "rest_until": now + 600_000,
        });
        let (least, _) = eta(&seen, Some(&full), &[], now).unwrap();
        assert!(least >= 600, "{least}");
    }

    /// A walk given `--max-pages` stops there whatever the list's size, so
    /// the time left counts no page past it.
    #[test]
    fn the_time_left_counts_no_page_past_the_walks_cap() {
        let now = snob_core::clock::now_ms().get();
        let mut seen = crossing(json!([
            {"list": "followers", "fate": "walked", "pages": {"least": 40, "most": 40}},
        ]));
        seen.job.as_mut().unwrap().argv = ["followers", "--format", "json", "--max-pages", "40"]
            .map(String::from)
            .to_vec();
        let line = json!({
            "event": "page", "at": now, "page": 5, "accounts": 60, "of": 5000,
            "list": "followers", "sitting": 5, "rest_until": null,
        });
        assert_eq!(
            eta(&seen, Some(&line), &[], now).unwrap(),
            walking(&[(35, 35)], 5)
        );
        seen.job.as_mut().unwrap().argv = ["followers", "--max-pages=1"].map(String::from).to_vec();
        let first = json!({
            "event": "page", "at": now, "page": 1, "accounts": 12, "of": 5000,
            "list": "followers", "sitting": 1, "rest_until": null,
        });
        assert_eq!(
            eta(&seen, Some(&first), &[], now).unwrap(),
            walking(&[(0, 0)], 1)
        );
    }

    /// The second list of a crossing on an account never read here is of a
    /// size nobody knew when the job started: it is counted all the same,
    /// at least a page and at most as many as the list under way.
    #[test]
    fn a_second_list_of_unknown_size_is_counted() {
        let now = snob_core::clock::now_ms().get();
        let seen = crossing(json!([
            {"list": "followers", "fate": "unknown", "size": null, "pages": {"least": 1, "most": 1}},
            {"list": "following", "fate": "unknown", "size": null, "pages": {"least": 1, "most": 1}},
        ]));
        let line = json!({
            "event": "page", "at": now, "page": 10, "accounts": 120, "of": 240,
            "list": "followers", "sitting": 10, "rest_until": null,
        });
        assert_eq!(
            eta(&seen, Some(&line), &[], now).unwrap(),
            walking(&[(10, 10), (1, 20)], 10)
        );
    }

    /// Before the walk's first page no line says where the account's
    /// sitting is, so the estimate it was started with, which counted a
    /// rest owed after that page, is what the time left is.
    #[test]
    fn before_the_first_page_the_estimate_tells_the_time_left() {
        let now = snob_core::clock::now_ms().get();
        let mut seen = crossing(json!([
            {"list": "followers", "fate": "walked", "pages": {"least": 3, "most": 3}},
        ]));
        seen.job.as_mut().unwrap().estimate.as_mut().unwrap()["seconds"] =
            json!({"least": 302, "most": 913});
        let started = json!({
            "event": "started", "at": now, "page": 0, "accounts": 0, "of": 36,
            "list": "followers", "sitting": null, "rest_until": null,
        });
        let (least, most) = eta(&seen, Some(&started), &[], now).unwrap();
        assert!(least >= 301 && most >= 912, "{least}..{most}");
        // Once a page says where the sitting is, it is timed from there.
        let page = json!({
            "event": "page", "at": now, "page": 1, "accounts": 12, "of": 36,
            "list": "followers", "sitting": 1, "rest_until": null,
        });
        assert_eq!(
            eta(&seen, Some(&page), &[], now).unwrap(),
            walking(&[(2, 2)], 1)
        );
    }

    /// A crossing's first list is said to be done with before the second
    /// list's pages, which may run far past any tail of the log: it is still
    /// known, to the time left and to what the walk answered with.
    #[test]
    fn a_crossing_remembers_its_first_list_past_a_long_second() {
        let tmp = tempfile::tempdir().unwrap();
        let now = snob_core::clock::now_ms().get();
        let mut text = String::new();
        text.push_str(
            "{\"progress\":{\"event\":\"served\",\"list\":\"followers\",\"from\":\"stored\"}}\n",
        );
        for page in 1..=400 {
            text.push_str(&format!(
                "{{\"progress\":{{\"event\":\"page\",\"at\":{now},\"list\":\"following\",\
                 \"page\":{page},\"accounts\":{},\"of\":5000,\"sitting\":{},\
                 \"rest_until\":null}}}}\n",
                page * 12,
                page % 40
            ));
        }
        assert!(text.len() as u64 > 4 * TAIL);
        std::fs::write(tmp.path().join("err.log"), &text).unwrap();
        assert_eq!(served_lists(tmp.path()), ["followers"]);

        let mut seen = crossing(json!([
            {"list": "followers", "fate": "walked", "pages": {"least": 417, "most": 417}},
            {"list": "following", "fate": "walked", "pages": {"least": 417, "most": 417}},
        ]));
        seen.dir = tmp.path().to_path_buf();
        let newest = newest_progress(tmp.path()).unwrap();
        assert_eq!(
            eta(&seen, Some(&newest), &served_lists(tmp.path()), now).unwrap(),
            walking(&[(17, 17)], 400 % 40)
        );

        seen.status = Status::Succeeded;
        seen.state = Some(super::super::State {
            status: Status::Succeeded,
            started_at: now,
            updated_at: now,
            finished_at: Some(now),
            exit_number: Some(0),
            cancel: false,
            failure: None,
            supervisor_pid: None,
            child_pid: None,
        });
        let finished = finished(&seen).unwrap();
        assert_eq!(finished["served"][0]["list"], "followers");
        assert_eq!(finished["served"][0]["from"], "stored");
    }
}
