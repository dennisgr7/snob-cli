//! How many tool calls `snob mcp` runs at once, and how many it takes in a
//! minute.
//!
//! An agent makes its calls in parallel by habit; a person does not walk one
//! account from two windows at once. The persistent budget bounds the average
//! rate across processes, but the pause between two pages and the dwell
//! before a list are waited per process (`snob_ig::pace`), so two walks of one
//! account would interleave their requests and drain the budget's burst at
//! once. So a server runs a few calls at a time, and **only one that may reach
//! Instagram**; reading what is stored, an estimate and the status go on
//! beside it.
//!
//! **Taken before anything is asked**, so nobody says yes to a call that is
//! then refused, and no ticket is spent on one; so is every other refusal
//! that costs nothing to know, the account's jobs among them, whose walks
//! run in processes of their own and hold none of these permits once the
//! call that started them has answered (`mod.rs`, `Server::refused_ahead`,
//! which lists what can still refuse after the yes). **Never queued**: a
//! call over a limit answers `busy` at once, and the agent calls again when
//! the other has finished. A queue would hold a call for as long as another
//! walks, past what the client waits for. Permits are held until the call returns, which
//! is after its child has exited, the grace a stopped child gets included.
//!
//! This holds within one server. The other half is the child's: every child
//! that may reach Instagram is run with `--spend-alone` (`tools::Plan::
//! command_line`) and takes the account's lock before it sends anything
//! (`app::spend_alone`), which holds across servers, clients and processes.
//! What holds that lock in another process, another server's call, can
//! still refuse a call after its yes, as `busy`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::tools::Spends;

/// Calls running at once, of any kind. Each is a process with its own
/// connections to the account's database; four lets the status, a stored
/// list and an estimate answer while a walk runs, and keeps an agent that
/// fires twenty calls at once from starting twenty processes.
const CALLS: usize = 4;

/// Calls running at once that may reach Instagram: one, as a person would.
const SPENDING: usize = 1;

/// Follows and unfollows running at once. [`SPENDING`] already admits one,
/// writes included; this states the write regime's one at a time (AGENTS.md,
/// "The write regime") where a change to that number could not loosen it.
const WRITES: usize = 1;

/// Calls taken in one [`WINDOW`]. Generous for a person's conversation, and a
/// stop for an agent looping on a call. It also bounds the tickets alive at
/// once: one is handed out per call at most, each good for ten minutes, so at
/// most 600 are, under the 4096 `seal.rs` keeps.
const RATE: u32 = 60;

/// The window [`RATE`] is counted in.
const WINDOW: Duration = Duration::from_secs(60);

/// The limits one server holds every call to. Shared, behind an `Arc`, by
/// every handle on the server.
pub struct Limits {
    calls: Arc<Semaphore>,
    spending: Arc<Semaphore>,
    writes: Arc<Semaphore>,
    rate: Mutex<Window>,
}

/// What a call holds while it runs; dropping it lets the next one in.
pub struct Permits {
    _call: OwnedSemaphorePermit,
    _spending: Option<OwnedSemaphorePermit>,
    _write: Option<OwnedSemaphorePermit>,
}

/// Which limit refused a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Calls,
    Spending,
    Writes,
    Rate,
}

impl Busy {
    /// What the agent is told, beside the code `busy`.
    pub fn why(self) -> String {
        match self {
            Self::Calls => format!(
                "{CALLS} calls are already running, so nothing was run; wait for one to \
                 finish, then call again"
            ),
            Self::Spending => "another call that may reach Instagram is running, and only one \
                               runs at a time, so nothing was run; wait for it to finish, then \
                               call again"
                .to_string(),
            Self::Writes => "another follow or unfollow is running, so nothing was run; wait \
                             for it to finish, then call again"
                .to_string(),
            Self::Rate => format!(
                "more than {RATE} calls in a minute, so nothing was run; wait a minute, then \
                 call again"
            ),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            calls: Arc::new(Semaphore::new(CALLS)),
            spending: Arc::new(Semaphore::new(SPENDING)),
            writes: Arc::new(Semaphore::new(WRITES)),
            rate: Mutex::new(Window::default()),
        }
    }
}

impl Limits {
    /// The permits a call that `spends` as said needs, or the limit that
    /// refuses it. Nothing is waited for.
    pub fn take(&self, spends: Spends) -> Result<Permits, Busy> {
        self.take_at(spends, Instant::now())
    }

    /// [`Limits::take`] at `now`. **The rate is counted last**, once every
    /// permit is held, so a call refused as busy is not counted against it,
    /// and an agent that calls again a moment later is not refused for having
    /// been refused.
    fn take_at(&self, spends: Spends, now: Instant) -> Result<Permits, Busy> {
        let call = Arc::clone(&self.calls)
            .try_acquire_owned()
            .map_err(|_| Busy::Calls)?;
        let spending = match spends {
            Spends::Nothing => None,
            Spends::Reads | Spends::Writes => Some(
                Arc::clone(&self.spending)
                    .try_acquire_owned()
                    .map_err(|_| Busy::Spending)?,
            ),
        };
        let write = match spends {
            Spends::Writes => Some(
                Arc::clone(&self.writes)
                    .try_acquire_owned()
                    .map_err(|_| Busy::Writes)?,
            ),
            Spends::Nothing | Spends::Reads => None,
        };
        let admitted = self
            .rate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit(now);
        if !admitted {
            return Err(Busy::Rate);
        }
        Ok(Permits {
            _call: call,
            _spending: spending,
            _write: write,
        })
    }
}

/// A fixed window: [`RATE`] calls from the first one, then a new window.
/// Fixed rather than sliding, because it only has to stop a loop, and a loop
/// meets it in either.
#[derive(Default)]
struct Window {
    started: Option<Instant>,
    taken: u32,
}

impl Window {
    /// Counts a call at `now`, or says it is one too many.
    fn admit(&mut self, now: Instant) -> bool {
        let current = self
            .started
            .is_some_and(|started| now.saturating_duration_since(started) < WINDOW);
        if !current {
            self.started = Some(now);
            self.taken = 0;
        }
        if self.taken >= RATE {
            return false;
        }
        self.taken += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_refuse_past_their_count_and_free_on_drop() {
        let limits = Limits::default();
        let now = Instant::now();

        // One call that may reach Instagram at a time, whatever the other
        // wants to send; the status and a stored list go on beside it.
        let walk = limits.take_at(Spends::Reads, now).unwrap();
        assert_eq!(
            limits.take_at(Spends::Reads, now).err(),
            Some(Busy::Spending)
        );
        assert_eq!(
            limits.take_at(Spends::Writes, now).err(),
            Some(Busy::Spending)
        );
        let stored = limits.take_at(Spends::Nothing, now).unwrap();
        drop(walk);

        // A write holds both.
        let write = limits.take_at(Spends::Writes, now).unwrap();
        assert_eq!(
            limits.take_at(Spends::Writes, now).err(),
            Some(Busy::Spending)
        );
        assert_eq!(
            limits.take_at(Spends::Reads, now).err(),
            Some(Busy::Spending)
        );

        // Four calls in all.
        let more: Vec<Permits> = (0..CALLS - 2)
            .map(|_| limits.take_at(Spends::Nothing, now).unwrap())
            .collect();
        assert_eq!(
            limits.take_at(Spends::Nothing, now).err(),
            Some(Busy::Calls)
        );
        drop(more);
        drop(stored);
        drop(write);

        // Every one of them is free again.
        let all: Vec<Permits> = (0..CALLS)
            .map(|i| {
                let spends = if i == 0 {
                    Spends::Writes
                } else {
                    Spends::Nothing
                };
                limits.take_at(spends, now).unwrap()
            })
            .collect();
        assert_eq!(all.len(), CALLS);
    }

    #[test]
    fn the_rate_counts_calls_a_minute_at_a_time_and_not_the_busy_ones() {
        let limits = Limits::default();
        let start = Instant::now();
        // A busy refusal is not counted: here, every call but the first
        // finds the walk running.
        let walk = limits.take_at(Spends::Reads, start).unwrap();
        for _ in 0..RATE * 2 {
            assert_eq!(
                limits.take_at(Spends::Reads, start).err(),
                Some(Busy::Spending)
            );
        }
        drop(walk);

        // One taken above, so RATE - 1 more fit in the window.
        for _ in 1..RATE {
            assert!(limits.take_at(Spends::Nothing, start).is_ok());
        }
        assert_eq!(
            limits.take_at(Spends::Nothing, start).err(),
            Some(Busy::Rate)
        );
        assert_eq!(
            limits
                .take_at(Spends::Nothing, start + WINDOW - Duration::from_millis(1))
                .err(),
            Some(Busy::Rate)
        );
        // A minute on, a new window.
        assert!(limits.take_at(Spends::Nothing, start + WINDOW).is_ok());
    }

    #[test]
    fn every_refusal_says_nothing_was_run_and_to_call_again() {
        for busy in [Busy::Calls, Busy::Spending, Busy::Writes, Busy::Rate] {
            let why = busy.why();
            assert!(why.contains("nothing was run"), "{why}");
            assert!(why.contains("call again"), "{why}");
        }
    }
}
