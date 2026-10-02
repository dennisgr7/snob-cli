//! Exit codes, so a caller can tell "log in again" apart from "wait a while"
//! without parsing message text.

use snob_core::model::StopReason;
use snob_ig::error::IgError;

/// How a run ended, under the name the process exits with.
///
/// The outcome is the domain's, because `watch_runs.outcome` and the `--json`
/// documents name it too, and its tokens are [`RunOutcome::as_str`]. Which
/// number a shell reads for it is this crate's business, and is [`code`].
///
/// [`RunOutcome::as_str`]: snob_core::watch::RunOutcome::as_str
pub use snob_core::watch::RunOutcome as ExitCode;

/// The number the process exits with for `outcome`.
pub fn code(outcome: ExitCode) -> u8 {
    match outcome {
        ExitCode::Ok => 0,
        ExitCode::Error => 1,
        ExitCode::NoSession => 3,
        ExitCode::Challenge => 4,
        ExitCode::RateLimited => 5,
        // 128 + SIGINT, the shell convention.
        ExitCode::Interrupted => 130,
    }
}

/// The code for what Instagram said, when a walk stopped because it said
/// something.
///
/// It has to agree with [`from_stop_reason`], which is the other
/// road to the same question: the walker records a coarse [`StopReason`] and
/// keeps the error beside it, and whichever of the two a command happens to
/// read must not change the answer.
pub fn from_ig_error(e: &IgError) -> ExitCode {
    match e {
        IgError::SessionExpired | IgError::UserAgentMismatch => ExitCode::NoSession,
        IgError::Challenge { .. } | IgError::Checkpoint { .. } => ExitCode::Challenge,
        // `InCooldown` is the backstop in `Pacer::clear` answering, and it
        // exits the same way the explicit gates in front of it do. They all
        // reach this code through `report::refuse_in_cooldown` and its
        // neighbors; a run that got past them and was stopped here is the same
        // outcome and must not be told apart by a script reading the code.
        IgError::RateLimited | IgError::FeedbackRequired | IgError::InCooldown { .. } => {
            ExitCode::RateLimited
        }
        // Ctrl+C during the budget's owed wait comes back through the
        // client rather than through the token, so it arrives here as an
        // error — and it is still the user stopping, which `from_stop_reason`
        // answers with 130 for the very same event.
        IgError::Canceled => ExitCode::Interrupted,
        _ => ExitCode::Error,
    }
}

/// The code for a result that had to be refused because a walk stopped
/// early for this reason. `PageLimit` maps to `Error` here, unlike in a
/// plain list: the cap was asked for, but the refused result is still not
/// delivered.
pub fn from_stop_reason(reason: StopReason) -> ExitCode {
    match reason {
        StopReason::Canceled => ExitCode::Interrupted,
        StopReason::RateLimit => ExitCode::RateLimited,
        StopReason::SessionInvalid => ExitCode::NoSession,
        StopReason::Completed
        | StopReason::PageLimit
        | StopReason::Truncated
        | StopReason::Network => ExitCode::Error,
    }
}

/// Digs the code out of an error chain.
///
/// `anyhow` wraps as it goes, so by the time an error reaches `main` the
/// thing that knew what happened is several layers down. The tests use this
/// lookup too: a test that reimplements it can pass while the real one is
/// broken.
pub fn from_chain(error: &anyhow::Error) -> Option<ExitCode> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ExitError>())
        .map(|e| e.code)
}

/// The exit code for a failed run, from whatever in the chain knows it.
///
/// An error that already knows its code wins: it was set by whoever refused
/// the result, which is more specific than anything reconstructed from an
/// Instagram error further down. Here rather than in `main`, because the
/// JSON rendering of a failure names the same code and the two must agree.
pub fn exit_code_for(error: &anyhow::Error) -> ExitCode {
    if let Some(code) = from_chain(error) {
        return code;
    }

    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<IgError>()
                .or_else(|| {
                    cause
                        .downcast_ref::<snob_ig::login::LoginError>()
                        .and_then(|e| e.as_instagram())
                })
                .map(from_ig_error)
        })
        .unwrap_or(ExitCode::Error)
}

/// An error that already knows its exit code.
///
/// The walker reports throttling, cancellation and session death as a
/// [`StopReason`] inside an `Ok`, so by the time a command refuses a result
/// there is no [`IgError`] left in the chain for `main` to map. This carries
/// the code instead, keeping "wait a while" and "log in again" tellable apart.
#[derive(Debug)]
pub struct ExitError {
    pub code: ExitCode,
    message: String,
    /// What to do about it, kept apart from what happened, so the printer can
    /// put it on its own `hint:` line rather than under `error:`, where it
    /// would read as more of the complaint.
    hint: Option<String>,
}

impl ExitError {
    pub fn new(code: ExitCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Adds the advice. A builder rather than a third argument to `new`,
    /// because most of the places that construct one of these have no advice
    /// to give and should not have to say so.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl std::fmt::Display for ExitError {
    /// The failure alone. The advice is [`ExitError::hint`], and the printer
    /// puts it back — but anything that only has a `Display`, like an `anyhow`
    /// chain being formatted somewhere else, still reads a complete sentence.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::watch::{RecordedOutcome, RunOutcome};

    /// The token a code writes is the token that reads back as it, and the
    /// run log records it as itself.
    ///
    /// Walked over `ALL` and counted, because a variant dropped from that list
    /// quietly narrows every caller that walks it -- and this is the caller
    /// where it is cheapest to notice. Counted for distinctness too: two codes
    /// sharing one token round-trip perfectly and are still a build that
    /// cannot tell a challenge from a plain failure.
    #[test]
    fn every_exit_code_reads_back_from_the_token_it_writes() {
        assert_eq!(ExitCode::ALL.len(), 6, "a code was added or dropped");
        for code in ExitCode::ALL {
            assert_eq!(
                RunOutcome::from_token(code.as_str()),
                Some(code),
                "{code:?} writes {:?} and does not read back from it",
                code.as_str()
            );
            assert_eq!(
                RecordedOutcome::from(code),
                code,
                "{code:?} records an outcome that is not its own"
            );
        }

        let mut outcomes: Vec<&str> = ExitCode::ALL.iter().map(|c| c.as_str()).collect();
        outcomes.sort_unstable();
        outcomes.dedup();
        assert_eq!(
            outcomes.len(),
            ExitCode::ALL.len(),
            "two codes share one outcome"
        );

        assert_eq!(
            RunOutcome::from_token("rate-limited"),
            None,
            "a spelling this build does not write is not an outcome it knows"
        );
    }

    /// The tokens in `whoami --json` are the ones the README's exit-code table
    /// uses, so `$?` and the object say the same thing by the same name.
    #[test]
    fn every_code_has_a_stable_token() {
        let codes = [
            (ExitCode::Ok, "ok"),
            (ExitCode::Error, "error"),
            (ExitCode::NoSession, "no_session"),
            (ExitCode::Challenge, "challenge"),
            (ExitCode::RateLimited, "rate_limited"),
            (ExitCode::Interrupted, "interrupted"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (code, token) in codes {
            assert_eq!(code.as_str(), token);
            assert!(seen.insert(token), "{token} is used twice");
        }
    }

    /// The numbers the README documents, one per outcome. Nothing but this
    /// match gives them, so nothing else can pin them.
    #[test]
    fn every_outcome_exits_with_its_documented_number() {
        let numbers: Vec<u8> = ExitCode::ALL.into_iter().map(code).collect();
        assert_eq!(numbers, [0, 1, 3, 4, 5, 130]);
    }

    #[test]
    fn a_refused_result_keeps_the_documented_codes() {
        assert_eq!(
            from_stop_reason(StopReason::Canceled),
            ExitCode::Interrupted
        );
        assert_eq!(
            from_stop_reason(StopReason::RateLimit),
            ExitCode::RateLimited
        );
        assert_eq!(
            from_stop_reason(StopReason::SessionInvalid),
            ExitCode::NoSession
        );
        assert_eq!(from_stop_reason(StopReason::PageLimit), ExitCode::Error);
    }

    /// The two roads to the same event have to arrive at the same code.
    ///
    /// A walk stopped by Ctrl+C is recorded as `StopReason::Canceled`, which
    /// maps to 130; a Ctrl+C during the request budget's owed wait comes back
    /// as `IgError::Canceled` instead, and must map to 130 too. The same key
    /// press must not give two exit codes depending on whether the tool
    /// happened to be sleeping at the time.
    #[test]
    fn a_cancellation_is_the_users_code_whichever_path_it_arrives_by() {
        assert_eq!(from_ig_error(&IgError::Canceled), ExitCode::Interrupted);
        assert_eq!(
            from_stop_reason(StopReason::Canceled),
            ExitCode::Interrupted
        );
    }

    /// The rest of the mapping, so a later arm cannot be added over one of
    /// these by accident.
    #[test]
    fn what_instagram_said_decides_the_code() {
        assert_eq!(from_ig_error(&IgError::SessionExpired), ExitCode::NoSession);
        assert_eq!(
            from_ig_error(&IgError::Checkpoint { url: None }),
            ExitCode::Challenge
        );
        assert_eq!(from_ig_error(&IgError::RateLimited), ExitCode::RateLimited);
        assert_eq!(
            from_ig_error(&IgError::Decode("not json".into())),
            ExitCode::Error
        );
    }

    #[test]
    fn the_code_survives_an_anyhow_chain() {
        let error: anyhow::Error =
            ExitError::new(ExitCode::RateLimited, "refused for the test").into();
        assert_eq!(from_chain(&error), Some(ExitCode::RateLimited));

        // Wrapped in context, which is what really happens on the way up.
        let wrapped = error.context("while doing something else");
        assert_eq!(from_chain(&wrapped), Some(ExitCode::RateLimited));
    }

    /// An error that never carried one has none to give, and `main` falls back
    /// to the generic code rather than inventing a specific one.
    #[test]
    fn an_ordinary_error_carries_no_code() {
        assert_eq!(from_chain(&anyhow::anyhow!("plain")), None);
    }
}
