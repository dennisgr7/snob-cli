//! Running one `snob` command for a tool call, and reading what it said.
//!
//! **Every call is a process of its own**, the binary this server is, run
//! with the arguments a person would type. Not a function call into the
//! commands, for three reasons that each would be enough: a command reads its
//! confirmation from standard input, which here carries the protocol; the
//! cancellation token is the process's, so one call's Ctrl+C would be every
//! call's; and a stray line on standard output would be a frame of JSON-RPC
//! the client cannot parse. A child has its own of all three, and everything
//! the commands promise (the budget, the cooldown, consent, the write regime)
//! holds for it exactly as it holds for a person, because it is the same code
//! path a person runs.
//!
//! **Stopping is closing its standard input** (`--stop-when-stdin-closes`),
//! which a parent can do on every platform; the child then stops as for
//! Ctrl+C, saving its page and giving its walk back, and is killed only if it
//! has not gone in [`GRACE`]. Killing it outright would hold its walk's claim
//! until the claim's lease ran out.

use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::exit::ExitCode;

/// How long a child asked to stop gets before it is killed. A walk stops
/// between two requests, and a request in flight is raced against the
/// cancellation, so seconds are plenty; a write in flight is the one thing
/// that is not abandoned, and its answer comes back well inside this.
pub const GRACE: Duration = Duration::from_secs(30);

/// The most of standard error kept for a failure that did not end in the JSON
/// error line: enough for the sentences, not a log.
const STDERR_KEPT: usize = 4 * 1024;

/// Environment variables a child never inherits.
///
/// `SNOB_IGNORE_COOLDOWN` is the escape hatch that exists so a bug of ours
/// cannot lock a person out; set in the environment the client was started
/// from, it would hand every agent's call a way past the hard stop. The rest
/// of the environment is the person's own machine (`SNOB_NO_BROWSER`, the
/// proxy, the locale) and is passed on.
const NEVER_INHERITED: &[&str] = &["SNOB_IGNORE_COOLDOWN"];

/// How a child run ended.
#[derive(Debug)]
pub struct Finished {
    pub code: ExitCode,
    /// Its standard output, parsed, when it was JSON.
    pub json: Option<Value>,
    /// Its standard output as text, when it was not.
    pub text: String,
    /// The JSON error object on the last line of standard error, when it
    /// printed one.
    pub error: Option<Value>,
    /// The end of standard error, for a failure with no JSON error.
    pub said: String,
}

/// What every child is started with, ahead of its own arguments: the global
/// flags the server was given (where the files are, which account, what TLS
/// trusts) and the two that make it a well-behaved child.
#[derive(Debug, Clone)]
pub struct Launch {
    pub exe: std::path::PathBuf,
    pub globals: Vec<OsString>,
}

impl Launch {
    /// Runs `snob <globals> <args>` to its end, or until `stop` resolves.
    ///
    /// `tick` is called every `every` while it runs, with how long it has run,
    /// so the caller can tell the client the call is alive.
    pub async fn run(
        &self,
        args: &[String],
        stop: impl std::future::Future<Output = ()>,
        every: Duration,
        mut tick: impl FnMut(Duration),
    ) -> anyhow::Result<Finished> {
        let mut command = Command::new(&self.exe);
        command
            .args(&self.globals)
            .arg("--stop-when-stdin-closes")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Not `kill_on_drop`: a child left behind by a server that went
            // away stops on its own, in order, because its standard input
            // closes with the server; killing it would hold its walk's claim
            // until the lease ran out.
            .stderr(Stdio::piped());
        for name in NEVER_INHERITED {
            command.env_remove(name);
        }
        let mut child = command.spawn()?;
        // Held, never written: dropping it is the request to stop.
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");

        let out = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let err = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });

        let started = tokio::time::Instant::now();
        let mut beat = tokio::time::interval_at(started + every, every);
        tokio::pin!(stop);
        let status = loop {
            tokio::select! {
                status = child.wait() => break status?,
                () = &mut stop, if stdin.is_some() => {
                    drop(stdin.take());
                    match tokio::time::timeout(GRACE, child.wait()).await {
                        Ok(status) => break status?,
                        Err(_) => {
                            child.kill().await?;
                            break child.wait().await?;
                        }
                    }
                }
                _ = beat.tick() => tick(started.elapsed()),
            }
        };

        let out = out.await??;
        let err = String::from_utf8_lossy(&err.await??).into_owned();
        Ok(read(status.code(), &out, &err))
    }
}

/// What a finished child said, read the way `main` and the commands print it:
/// data on standard output, and a failure in JSON as the last line of
/// standard error.
fn read(code: Option<i32>, out: &[u8], err: &str) -> Finished {
    let text = String::from_utf8_lossy(out).into_owned();
    let json = serde_json::from_str::<Value>(text.trim()).ok().or_else(|| {
        // ndjson: one object per line, read as the array it is.
        let lines: Option<Vec<Value>> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).ok())
            .collect();
        lines.filter(|lines| !lines.is_empty()).map(Value::Array)
    });
    let error = err
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("error").is_some());
    let said = match err.len().checked_sub(STDERR_KEPT) {
        Some(from) => {
            let from = (from..err.len())
                .find(|i| err.is_char_boundary(*i))
                .unwrap_or(err.len());
            err[from..].to_string()
        }
        None => err.to_string(),
    };
    Finished {
        code: code.map_or(ExitCode::Interrupted, from_number),
        json,
        text,
        error,
        said,
    }
}

/// The exit code a number stands for. One a child cannot exit with (it was
/// killed by a signal, or it is newer than this server) is a failure.
fn from_number(number: i32) -> ExitCode {
    [
        ExitCode::Ok,
        ExitCode::Error,
        ExitCode::NoSession,
        ExitCode::Challenge,
        ExitCode::RateLimited,
        ExitCode::Interrupted,
    ]
    .into_iter()
    .find(|code| i32::from(crate::exit::code(*code)) == number)
    .unwrap_or(ExitCode::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_and_an_error_line_are_both_read() {
        let finished = read(
            Some(5),
            b"{\"a\":1}\n",
            "a sentence\n{\"viewer\":null,\"error\":{\"code\":\"rate_limited\"}}\n",
        );
        assert_eq!(finished.code, ExitCode::RateLimited);
        assert_eq!(finished.json.unwrap()["a"], 1);
        assert_eq!(finished.error.unwrap()["error"]["code"], "rate_limited");
    }

    #[test]
    fn ndjson_is_read_as_an_array() {
        let finished = read(Some(0), b"{\"a\":1}\n{\"a\":2}\n", "");
        assert_eq!(finished.json.unwrap().as_array().unwrap().len(), 2);
        assert!(finished.error.is_none());
    }

    #[test]
    fn a_sentence_on_standard_error_is_not_taken_for_an_error_object() {
        let finished = read(Some(1), b"", "{\"not\":\"an error\"}\n");
        assert!(finished.error.is_none());
        assert_eq!(finished.code, ExitCode::Error);
    }

    #[test]
    fn a_long_standard_error_keeps_its_end() {
        // Three bytes a character, so the cut has to find a boundary.
        let long = "\u{2192}".repeat(STDERR_KEPT);
        let finished = read(Some(1), b"", &long);
        assert!(finished.said.len() <= STDERR_KEPT);
        assert!(finished.said.ends_with('\u{2192}'));
    }

    #[test]
    fn a_number_no_code_has_is_a_failure() {
        assert_eq!(from_number(130), ExitCode::Interrupted);
        assert_eq!(from_number(2), ExitCode::Error);
        assert_eq!(from_number(-1), ExitCode::Error);
    }
}
