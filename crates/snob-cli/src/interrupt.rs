//! Ctrl+C handling, and the other ways a system asks a program to stop.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use snob_ig::pace::CancelToken;

/// The process's cancellation token. See [`install`].
static TOKEN: OnceLock<CancelToken> = OnceLock::new();

/// Set when the system, rather than a person, asked the process to stop.
static BY_THE_SYSTEM: AtomicBool = AtomicBool::new(false);

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
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                terminate: signal(SignalKind::terminate()).ok(),
                hangup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
            Self {
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
                () = next_of!(self.terminate) => Some(Asked::System),
                () = next_of!(self.hangup) => Some(Asked::System),
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                heard = tokio::signal::ctrl_c() => heard.ok().map(|()| Asked::Person),
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
