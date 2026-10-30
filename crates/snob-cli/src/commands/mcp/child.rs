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
/// that is not abandoned, and its answer comes back well inside this. The
/// same grace a job's walk gets, from the one home it has (`jobs`).
pub const GRACE: Duration = crate::jobs::GRACE;

/// The longest one heartbeat may take to reach the client before the call
/// goes on without it. A client that has stopped reading would otherwise hold
/// the call, and the child's stop with it, for as long as it does not read.
const TICK_WITHIN: Duration = Duration::from_secs(5);

/// The most of standard error kept for a failure that did not end in the JSON
/// error line: enough for the sentences, not a log.
const STDERR_KEPT: usize = 4 * 1024;

/// Environment variables a child never inherits, from the one home they
/// have (`jobs`), which says why.
const NEVER_INHERITED: &[&str] = crate::jobs::NEVER_INHERITED;

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
    /// Where each list it answered with came from, as a walk run with
    /// `--progress-json` says it (`Progress::served`): a job's walk.
    pub served: Vec<Value>,
}

/// What every child is started with, ahead of its own arguments: the global
/// flags the server was given (where the files are, which account, what TLS
/// trusts) and the two that make it a well-behaved child.
#[derive(Debug, Clone)]
pub struct Launch {
    pub exe: std::path::PathBuf,
    pub globals: Vec<OsString>,
    /// What `exe` was when the server started, to tell an upgrade by.
    started_from: Option<Stamp>,
}

/// A binary as an upgrade would change it: its size and modification time,
/// the same two facts the browser owner tells builds apart by
/// (`owner::wire::build`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl Stamp {
    fn of(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// Whether the binary a server started from is gone or has changed, given
/// what it was then and what is at its path now (`None`: nothing is).
///
/// Every installer does one of the two: Homebrew's and Scoop's cleanup
/// remove the old version's directory, WinGet and `install.ps1` move the
/// running file aside and put the new one in its place, and `install.sh`
/// and a package replace the file at the same path.
fn upgraded(then: Option<&Stamp>, now: Option<&Stamp>) -> bool {
    match (then, now) {
        (_, None) => true,
        (Some(then), Some(now)) => then != now,
        // Unreadable at the start and readable now: nothing to compare.
        (None, Some(_)) => false,
    }
}

impl Launch {
    /// Children of `exe`, started with `globals` in front of their own
    /// arguments; `exe` is read now, to tell later whether it was upgraded.
    pub fn new(exe: std::path::PathBuf, globals: Vec<OsString>) -> Self {
        let started_from = Stamp::of(&exe);
        Self {
            exe,
            globals,
            started_from,
        }
    }

    /// Whether the binary this server runs its children from is no longer
    /// the one it started from.
    ///
    /// **Then nothing is started**, rather than a child of whatever is at the
    /// path now: a newer child may read the arguments this server built in a
    /// way this server never checked, and a removed file starts nothing (nor
    /// could a child started through `/proc/self/exe` start the browser
    /// owner, which reads `current_exe` again). Restarting the server, which
    /// the client does when it reconnects it, is the whole fix.
    pub fn upgraded(&self) -> bool {
        upgraded(self.started_from.as_ref(), Stamp::of(&self.exe).as_ref())
    }

    /// The child `run` starts, built and not started.
    fn command(&self, args: &[String]) -> Command {
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
        // **In a process group of its own, so closing its standard input is
        // the only stop it gets.** Sharing the server's, the Ctrl+C a person
        // presses to stop `snob mcp --listen` reached every child too, which
        // took it as its first request to stop; the server then closed its
        // input, a second request, and the child exited at once (`interrupt`:
        // a second request does not wait), with no JSON failure and the
        // terminal's reset sequences written into the pipe, and a write in
        // flight was abandoned. Seen in 4 runs of 9 against a fake Instagram
        // that never answered. On Windows a new group has Ctrl+C disabled and
        // still hears the console closing, signing out and shutting down; on
        // Unix a group other than the terminal's foreground one is not sent
        // its SIGINT, and SIGTERM and SIGHUP still reach the child by pid.
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
        #[cfg(unix)]
        command.process_group(0);
        command
    }

    /// Runs `snob <globals> <args>` to its end, or until `stop` resolves.
    ///
    /// `tick` is called every `every` while it runs, with how long it has run,
    /// so the caller can tell the client the call is alive, and **its future
    /// is awaited here**, in the call's own task, before the loop goes on. So
    /// a heartbeat has reached the client before the result can: the
    /// specification has progress notifications stop once a request is
    /// answered, and a send spawned off carries no order with the result.
    pub async fn run<F>(
        &self,
        args: &[String],
        stop: impl std::future::Future<Output = ()>,
        every: Duration,
        mut tick: impl FnMut(Duration) -> F,
    ) -> anyhow::Result<Finished>
    where
        F: std::future::Future<Output = ()>,
    {
        let mut child = self.command(args).spawn()?;
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
        // A beat that was late, because the one before it waited on the
        // client, is not made up for with a burst of them.
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                // **Never send to the client from a spawned task**: the send
                // would race the result (see above), and rmcp keeps which
                // request a message belongs to in a task-local
                // (`ORIGINATING_REQUEST`) that does not cross `tokio::spawn`.
                // Bounded, so a client that stopped reading cannot keep the
                // child's exit or its stop from being seen.
                _ = beat.tick() => {
                    let _ = tokio::time::timeout(TICK_WITHIN, tick(started.elapsed())).await;
                }
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
    let served = err
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter_map(|value| value.get("progress").cloned())
        .filter(|progress| progress["event"] == "served")
        .collect();
    Finished {
        code: code.map_or(ExitCode::Interrupted, from_number),
        json,
        text,
        error,
        said,
        served,
    }
}

/// The exit code a number stands for. One a child cannot exit with (it was
/// killed by a signal, or it is newer than this server) is a failure.
fn from_number(number: i32) -> ExitCode {
    crate::exit::outcome_of(number).unwrap_or(ExitCode::Error)
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
    fn a_deleted_binary_is_reported_as_an_upgrade() {
        let at = |len, secs| Stamp {
            len,
            modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(secs)),
        };
        // Removed, or moved aside.
        assert!(upgraded(Some(&at(10, 1)), None));
        assert!(upgraded(None, None));
        // Replaced at the same path.
        assert!(upgraded(Some(&at(10, 1)), Some(&at(10, 2))));
        assert!(upgraded(Some(&at(10, 1)), Some(&at(11, 1))));
        // Untouched.
        assert!(!upgraded(Some(&at(10, 1)), Some(&at(10, 1))));
        assert!(!upgraded(None, Some(&at(10, 1))));
    }

    #[test]
    fn a_binary_moved_aside_is_an_upgrade_and_one_left_alone_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("snob");
        std::fs::write(&exe, b"one").unwrap();
        let launch = Launch::new(exe.clone(), Vec::new());
        assert!(!launch.upgraded());
        std::fs::rename(&exe, tmp.path().join("snob.old")).unwrap();
        assert!(launch.upgraded());
    }

    /// The key of the server over the network, and a client's copy, are
    /// taken out of what a child is started with, and so out of the browser
    /// owner and the browsers it would start; so is the escape hatch.
    #[test]
    fn the_mcp_key_never_reaches_a_child() {
        let launch = Launch::new(std::path::PathBuf::from("snob"), Vec::new());
        let command = launch.command(&["status".to_string()]);
        let removed: Vec<&std::ffi::OsStr> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name)
            .collect();
        for name in ["SNOB_MCP_KEY", "SNOB_MCP_TOKEN", "SNOB_IGNORE_COOLDOWN"] {
            assert!(
                removed.contains(&std::ffi::OsStr::new(name)),
                "{name} reaches the child: {removed:?}"
            );
        }
    }

    #[test]
    fn a_number_no_code_has_is_a_failure() {
        assert_eq!(from_number(130), ExitCode::Interrupted);
        assert_eq!(from_number(2), ExitCode::Error);
        assert_eq!(from_number(-1), ExitCode::Error);
    }
}
