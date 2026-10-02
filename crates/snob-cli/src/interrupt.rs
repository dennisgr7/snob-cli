//! Ctrl+C handling.

use std::sync::OnceLock;

use snob_ig::pace::CancelToken;

/// The process's cancellation token. See [`install`].
static TOKEN: OnceLock<CancelToken> = OnceLock::new();

/// Whether this run was canceled: a Ctrl+C the handler heard, or a stop key
/// in the interactive browser.
pub fn interrupted() -> bool {
    TOKEN.get().is_some_and(CancelToken::is_canceled)
}

/// Installs the handler and returns the cancellation token.
///
/// The double-press pattern is required because `tokio::signal::ctrl_c()`
/// **permanently** disables the process default: from the first call onwards, a
/// Ctrl+C no longer kills the program. If the orderly stop ever got stuck, the
/// user would have no way out.
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
                if tokio::signal::ctrl_c().await.is_err() {
                    return;
                }
                eprintln!(
                    "\nStopping and saving what has been fetched... (Ctrl+C again to quit now)"
                );
                copy.cancel();

                if tokio::signal::ctrl_c().await.is_ok() {
                    eprintln!("\nForced exit.");
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
