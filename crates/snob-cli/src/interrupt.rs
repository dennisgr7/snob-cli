//! Ctrl+C handling, and the other ways a system asks a program to stop.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use snob_ig::pace::CancelToken;

/// The process's cancellation token. See [`install`].
static TOKEN: OnceLock<CancelToken> = OnceLock::new();

/// Set when the system, rather than a person, asked the process to stop.
static BY_THE_SYSTEM: AtomicBool = AtomicBool::new(false);

/// Set by [`stop_when_stdin_closes`].
static ON_STDIN_EOF: AtomicBool = AtomicBool::new(false);

/// Makes standard input reaching its end one more way of asking the process
/// to stop, taken as the system's requests are (no message, exit 130). Called
/// by `main` for `--stop-when-stdin-closes`, before anything installs the
/// handler.
///
/// **For a parent that cannot send a signal.** Windows has no way for one
/// process to send another the Ctrl+C its handler hears, short of sharing a
/// console, so a parent such as `snob mcp` that wants a walk stopped in order
/// (its page saved, its claim given back) and not killed where it stands
/// closes the pipe instead, which every platform can do. A parent that dies
/// closes it too, so its child does not walk on for nobody.
///
/// Standard input is read for nothing else afterwards: nothing can be typed
/// in answer to a question, so the command has to be told its answers (`-y`).
pub fn stop_when_stdin_closes() {
    ON_STDIN_EOF.store(true, Ordering::SeqCst);
}

/// Says so, once, when standard input has reached its end.
///
/// A thread of its own rather than a blocking task: a read of a pipe cannot be
/// canceled, and a blocking task still in one would hold up the runtime's
/// shutdown for as long as the parent keeps the pipe open. What it reads is
/// thrown away.
fn stdin_closed() -> tokio::sync::mpsc::Receiver<()> {
    let (closed, heard) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        wait_for_the_end_of_stdin();
        let _ = closed.blocking_send(());
    });
    heard
}

/// Reads standard input until it ends: a closed pipe, the end of a file, or
/// anything that will not read.
#[cfg(unix)]
fn wait_for_the_end_of_stdin() {
    use std::io::Read;
    let mut stdin = std::io::stdin().lock();
    let mut sink = [0u8; 256];
    while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
}

/// The same on Windows, **without a read left waiting**.
///
/// A synchronous read pending on a pipe holds the handle, and every other call
/// on it waits behind the read: `is_terminal` asks the handle its file name
/// (the MSYS check), so the first `stdin().is_terminal()` anywhere in the run
/// would hang until the parent wrote or closed. So the pipe is peeked at, which
/// returns at once, and read only for what it already holds; closed, the peek
/// fails with `ERROR_BROKEN_PIPE`. A standard input that is not a pipe (a file,
/// `NUL`, a console) fails the peek too and counts as ended, as the end of a
/// file does on Unix.
#[cfg(windows)]
fn wait_for_the_end_of_stdin() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    // Asked once a quarter of a second: the parent closing the pipe is the
    // run stopping, and a quarter of a second is nothing beside the request
    // in flight it then waits for.
    const EVERY: std::time::Duration = std::time::Duration::from_millis(250);

    let handle = std::io::stdin().as_raw_handle();
    let mut sink = [0u8; 256];
    loop {
        let mut available: u32 = 0;
        // SAFETY: `handle` is this process's standard input, which stays open
        // for the life of the process; the only out-pointer is to a local, and
        // a null buffer of size zero asks for the count alone.
        let peeked = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if peeked == 0 {
            return;
        }
        if available == 0 {
            std::thread::sleep(EVERY);
            continue;
        }
        let mut read: u32 = 0;
        let wanted = available.min(sink.len() as u32);
        // SAFETY: the same handle; `sink` is a local buffer at least `wanted`
        // bytes long, `read` a local, and no overlapped structure is passed,
        // so the read is synchronous and over when this returns. It does not
        // wait: the pipe already holds `wanted` bytes.
        let ok = unsafe {
            ReadFile(
                handle,
                sink.as_mut_ptr(),
                wanted,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            return;
        }
    }
}

/// Whether this run was canceled: a Ctrl+C the handler heard, a stop the
/// system asked for, or a stop key in the interactive browser.
pub fn interrupted() -> bool {
    TOKEN.get().is_some_and(CancelToken::is_canceled)
}

/// Whether the system asked the process to stop: a service manager, the
/// terminal going away, the session ending.
///
/// For the full-screen browsers, which read Ctrl+C as a key and never look at
/// the token between two keys: `ui::browser::input::read` hands them a Ctrl+C
/// when this is set, so each leaves the way it leaves for the key. Not for a
/// token canceled by a stop key, which the browser that read it already acts
/// on, and which must not reach a browser it returns to.
pub fn asked_by_the_system() -> bool {
    BY_THE_SYSTEM.load(Ordering::SeqCst)
}

/// Who asked the process to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    /// A person, at the keyboard: Ctrl+C.
    Person,
    /// The system: a service manager stopping the unit, the terminal going
    /// away, the session ending, the machine shutting down.
    System,
}

/// Installs the handler and returns the cancellation token.
///
/// The double-press pattern is required because `tokio::signal::ctrl_c()`
/// **permanently** disables the process default: from the first call onwards, a
/// Ctrl+C no longer kills the program. If the orderly stop ever got stuck, the
/// user would have no way out.
///
/// **The system's requests to stop take the same road as Ctrl+C.** On Unix
/// that is `SIGTERM`, which `systemctl stop`, `kill` and a shutdown send, and
/// `SIGHUP`, which a closed terminal sends; on Windows it is the console
/// closing, the user signing out and the machine shutting down. Left to their
/// defaults these ended the process where it stood: a walk lost the page in
/// flight and skipped the orderly stop a Ctrl+C gets. Now each cancels the
/// same token, so the walk stops the way it stops for a person, and a second
/// request of either kind ends the process at once. Nothing is printed for
/// them: there may be no terminal left to print to, and the "again" advice is
/// for a person. The exit code is 130, the one an interrupted run has, whoever
/// interrupted it. Windows gives a process a few seconds after the console
/// closes, which is time enough for a walk to stop between two requests.
///
/// **Installed once per process, not once per call.** Interrupting is a
/// property of the process. `snob watch` opens an `App` per tick — so that
/// each tick picks up a rotated session and holds no SQLite connection while it
/// sleeps — and a listener and a token per call would leave one listener per
/// tick alive, thousands of them after a week, with the signal going to
/// whichever won the race and canceling a token that nothing is watching.
pub fn install() -> CancelToken {
    TOKEN
        .get_or_init(|| {
            let token = CancelToken::default();
            let copy = token.clone();

            tokio::spawn(async move {
                let mut asked = Requests::listen();
                let Some(first) = asked.next().await else {
                    return;
                };
                if first == Asked::Person {
                    eprintln!(
                        "\nStopping and saving what has been fetched... (Ctrl+C again to quit now)"
                    );
                } else {
                    BY_THE_SYSTEM.store(true, Ordering::SeqCst);
                }
                copy.cancel();

                if let Some(second) = asked.next().await {
                    if second == Asked::Person {
                        eprintln!("\nForced exit.");
                    }
                    // Nothing below this runs a destructor, so a cursor hidden
                    // by the login menu would stay hidden for the rest of the
                    // user's shell session. A browser this process started
                    // closes as its pipe does, when the process is gone, and
                    // on Windows its job ends it.
                    crate::ui::restore_terminal();
                    std::process::exit(130);
                }
            });

            token
        })
        .clone()
}

/// Every way the process is asked to stop, listened to once for its whole
/// life. A request the system would not let this listen for is `None`, and
/// keeps its default; Ctrl+C is listened for whatever happens to the others.
struct Requests {
    /// Standard input closing, when [`stop_when_stdin_closes`] asked for it.
    stdin: Option<tokio::sync::mpsc::Receiver<()>>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    close: Option<tokio::signal::windows::CtrlClose>,
    #[cfg(windows)]
    logoff: Option<tokio::signal::windows::CtrlLogoff>,
    #[cfg(windows)]
    shutdown: Option<tokio::signal::windows::CtrlShutdown>,
}

/// Waits for `$slot`'s next request, or for ever when it is not listened to
/// or has stopped delivering: one that has stopped must not end every wait at
/// once and spin the loop that waits on it.
macro_rules! next_of {
    ($slot:expr) => {
        async {
            if let Some(slot) = $slot.as_mut()
                && slot.recv().await.is_some()
            {
                return;
            }
            std::future::pending::<()>().await
        }
    };
}

impl Requests {
    fn listen() -> Self {
        let stdin = ON_STDIN_EOF.load(Ordering::SeqCst).then(stdin_closed);
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                stdin,
                terminate: signal(SignalKind::terminate()).ok(),
                hangup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
            Self {
                stdin,
                close: ctrl_close().ok(),
                logoff: ctrl_logoff().ok(),
                shutdown: ctrl_shutdown().ok(),
            }
        }
    }

    /// The next request, or `None` once Ctrl+C can no longer be heard.
    async fn next(&mut self) -> Option<Asked> {
        #[cfg(unix)]
        {
            tokio::select! {
                heard = tokio::signal::ctrl_c() => heard.ok().map(|()| Asked::Person),
                () = next_of!(self.stdin) => Some(Asked::System),
                () = next_of!(self.terminate) => Some(Asked::System),
                () = next_of!(self.hangup) => Some(Asked::System),
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                heard = tokio::signal::ctrl_c() => heard.ok().map(|()| Asked::Person),
                () = next_of!(self.stdin) => Some(Asked::System),
                () = next_of!(self.close) => Some(Asked::System),
                () = next_of!(self.logoff) => Some(Asked::System),
                () = next_of!(self.shutdown) => Some(Asked::System),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs in one process share one token, so a Ctrl+C reaches whatever is
    /// running now rather than a token from a tick that finished hours ago.
    #[tokio::test]
    async fn installing_twice_hands_back_the_same_token() {
        let first = install();
        let second = install();

        assert!(!second.is_canceled());
        first.cancel();
        assert!(
            second.is_canceled(),
            "the second caller must be watching the token the handler cancels"
        );
    }
}
