//! Telling that the machine slept.
//!
//! A walk is fifteen minutes to over an hour of pages and rests, which is
//! long enough for a laptop to be shut or put to sleep in the middle of it.
//! Nothing about a walk breaks then — the process is frozen and carries on
//! where it stood — but what it carries on *with* is stale: the network is
//! still coming back, the page the requests are built from is hours old, and
//! a request in flight as the lid closed fails on waking. The walk has to know
//! it slept to treat that as the end of a sitting rather than as three network
//! failures in a row (`pager::ListWalker`).
//!
//! **Two clocks, compared.** The wall clock moves while the machine sleeps,
//! and [`now`] here does not: the difference between how far each moved since
//! the last look is the time spent asleep. Comparing the wall clock against
//! the length of the sleep that was asked for is not enough, because a walk
//! also waits inside the client, for the request budget, for as long as that
//! takes; and comparing it against [`std::time::Instant`] is not either,
//! because what that one does across a sleep is up to the platform, and on
//! Windows it keeps counting.

use std::time::Duration;

use snob_core::EpochMs;

/// How much further the wall clock may run than the awake one between two
/// looks before the difference is taken for a sleep.
///
/// A minute: far above what the time service corrects at a time, which is
/// seconds and done by slewing rather than jumping, and far below the shortest
/// sleep worth calling one: a laptop's lid closed and opened again in a minute
/// comes back to a network that never went.
pub const MARGIN: Duration = Duration::from_secs(60);

/// A clock that stands still while the machine sleeps, counted from an
/// arbitrary point. Only differences between two readings mean anything.
///
/// - **Linux**: [`std::time::Instant`] reads `CLOCK_MONOTONIC`, which the
///   kernel stops during a suspend (`CLOCK_BOOTTIME` is the one that runs on).
/// - **macOS**: it reads `CLOCK_UPTIME_RAW`, which stops while the Mac sleeps.
/// - **Windows**: `Instant` counts through a sleep, so this asks for the
///   *unbiased* interrupt time, which is the one Windows documents as leaving
///   sleep and hibernation out.
pub fn now() -> Duration {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;

        let mut hundreds_of_nanoseconds = 0u64;
        // SAFETY: the pointer is to a local the call writes one value into.
        let read = unsafe { QueryUnbiasedInterruptTime(&mut hundreds_of_nanoseconds) };
        if read != 0 {
            return Duration::from_nanos(hundreds_of_nanoseconds.saturating_mul(100));
        }
        // Documented to fail only on a null pointer; a reading that does not
        // move makes every gap look like sleep, which is the safe mistake.
        Duration::ZERO
    }
    #[cfg(not(windows))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }
}

/// The last look at both clocks, to tell from the next one whether the
/// machine slept in between.
#[derive(Debug, Clone, Copy)]
pub struct Asleep {
    wall: EpochMs,
    awake: Duration,
}

impl Asleep {
    pub fn new(wall: EpochMs, awake: Duration) -> Self {
        Self { wall, awake }
    }

    /// How long the machine slept since the last look, when the wall clock
    /// ran at least [`MARGIN`] further than the awake one; and the look
    /// becomes the last one either way, so a sleep is reported once.
    ///
    /// A wall clock that went backwards is a clock somebody set, not a sleep.
    pub fn check(&mut self, wall: EpochMs, awake: Duration) -> Option<Duration> {
        let on_the_wall = wall - self.wall;
        let awake_for = awake.saturating_sub(self.awake);
        *self = Self::new(wall, awake);
        let on_the_wall = Duration::from_millis(u64::try_from(on_the_wall).ok()?);
        on_the_wall
            .checked_sub(awake_for)
            .filter(|slept| *slept >= MARGIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_700_000_000_000;

    fn at(wall_ms: i64, awake_s: u64) -> (EpochMs, Duration) {
        (EpochMs::new(START + wall_ms), Duration::from_secs(awake_s))
    }

    #[test]
    fn a_machine_that_stayed_awake_did_not_sleep() {
        let (wall, awake) = at(0, 100);
        let mut asleep = Asleep::new(wall, awake);
        let (wall, awake) = at(3_600_000, 3_700);
        assert_eq!(asleep.check(wall, awake), None, "an hour each");
    }

    #[test]
    fn the_wall_running_ahead_is_the_time_asleep() {
        let (wall, awake) = at(0, 100);
        let mut asleep = Asleep::new(wall, awake);
        let (wall, awake) = at(8 * 3_600_000 + 10_000, 110);
        assert_eq!(
            asleep.check(wall, awake),
            Some(Duration::from_secs(8 * 3_600))
        );
        let (wall, awake) = at(8 * 3_600_000 + 20_000, 120);
        assert_eq!(asleep.check(wall, awake), None, "reported once");
    }

    #[test]
    fn a_small_correction_is_not_a_sleep() {
        let (wall, awake) = at(0, 100);
        let mut asleep = Asleep::new(wall, awake);
        let (wall, awake) = at(30_000, 100);
        assert_eq!(asleep.check(wall, awake), None, "under the margin");
    }

    #[test]
    fn a_clock_set_back_is_not_a_sleep() {
        let (wall, awake) = at(0, 100);
        let mut asleep = Asleep::new(wall, awake);
        let (wall, awake) = at(-3_600_000, 160);
        assert_eq!(asleep.check(wall, awake), None);
    }
}
