//! What snob asks of the operating system's power management.
//!
//! **Keeping the machine awake while a walk sends requests** ([`KeepAwake`]).
//! A walk is fifteen minutes to over an hour, most of it rests in which
//! nobody touches the machine, and a laptop left alone that long sleeps by
//! itself: the walk stops where it stood and picks up hours later, when the
//! lid opens. So while a walk is reading, snob tells the system it is busy, the
//! way a download or a backup does.
//!
//! **Only idle sleep is held off.** Closing the lid, pressing the power
//! button, choosing Sleep from a menu or `systemctl suspend` still put the
//! machine to sleep at once: those are a person's decision, and the walk
//! picks itself up after them (`snob_ig::awake`). The three backends are
//! chosen for exactly that:
//!
//! - **Windows**: a power request (`PowerSetRequest`, `SystemRequired`), whose
//!   reason `powercfg /requests` shows. Windows documents that sleeping on
//!   purpose overrides it, and on a laptop on battery in modern standby it
//!   gives up on its own five minutes after the sleep timeout.
//! - **macOS**: `caffeinate -i -w <pid>`, the system's own tool, which holds
//!   off idle sleep only, and ends by itself if snob dies without letting go.
//! - **Linux**: an `idle` inhibitor from logind, in `block` mode. Not a
//!   `sleep` one, which would also refuse a suspend somebody asks for by hand.
//!   Where there is no system bus, a Raspberry Pi or a server, there is no
//!   idle sleep to hold off either, and nothing is done.
//!
//! **What failing costs is nothing.** Each backend that cannot be had is
//! logged and left alone: the walk is not worse off than it was before this
//! existed.

pub mod battery;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
pub mod qos;
#[cfg(windows)]
mod windows;

/// Something the system holds for as long as it lives: dropping it lets go.
type Held = Box<dyn Send>;

/// How a hold is taken: the platform's own, or a test's.
type Take = fn(&'static str) -> Option<Held>;

/// Keeps the machine from sleeping on its own while held. See the module.
pub struct KeepAwake {
    /// What the system is told the machine is kept awake for, where it shows
    /// one: `powercfg /requests`, `pmset -g assertions`,
    /// `systemd-inhibit --list`.
    reason: &'static str,
    held: Option<Held>,
    take: Take,
    /// Set once the system said no, so that a walk asking again on every page
    /// does not ask the system again on every page.
    refused: bool,
}

impl KeepAwake {
    /// A guard that holds nothing yet. `wanted` false makes one that never
    /// holds: a walk against a test server sleeps through nothing, and asks
    /// the system for nothing.
    pub fn new(reason: &'static str, wanted: bool) -> Self {
        Self {
            reason,
            held: None,
            take: platform,
            refused: !wanted,
        }
    }

    /// Keeps the machine awake from now on, if it is not already.
    pub fn hold(&mut self) {
        if self.held.is_some() || self.refused {
            return;
        }
        self.held = (self.take)(self.reason);
        self.refused = self.held.is_none();
    }

    /// Lets the machine sleep on its own again: a walk waiting hours for the
    /// day's accounts has no reason to keep it up.
    pub fn let_go(&mut self) {
        self.held = None;
    }

    /// Follows a walk's events: let go while it waits for the day's
    /// accounts, held through everything else, the rests between sittings
    /// included, since a rest is a quarter of an hour of the idleness a
    /// laptop sleeps on.
    pub fn follow(&mut self, event: &snob_ig::pager::Event) {
        use snob_ig::pager::{Event, WaitKind};
        match event {
            Event::Waiting {
                kind: WaitKind::Day,
                ..
            } => self.let_go(),
            Event::Finished { .. } => self.let_go(),
            _ => self.hold(),
        }
    }
}

/// The platform's hold, or `None` where it cannot be had.
#[cfg(windows)]
fn platform(reason: &'static str) -> Option<Held> {
    windows::request(reason).map(|held| Box::new(held) as Held)
}

#[cfg(target_os = "macos")]
fn platform(reason: &'static str) -> Option<Held> {
    macos::request(reason).map(|held| Box::new(held) as Held)
}

#[cfg(target_os = "linux")]
fn platform(reason: &'static str) -> Option<Held> {
    linux::request(reason).map(|held| Box::new(held) as Held)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn platform(_: &'static str) -> Option<Held> {
    None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use snob_core::model::StopReason;
    use snob_ig::pager::{Event, WaitKind};

    use super::*;

    /// A system's hold, counted: how many are held right now, and how many
    /// were ever asked for.
    struct Counted;

    static HELD: AtomicUsize = AtomicUsize::new(0);
    static ASKED: AtomicUsize = AtomicUsize::new(0);

    impl Drop for Counted {
        fn drop(&mut self) {
            HELD.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn counted(_: &'static str) -> Option<Held> {
        ASKED.fetch_add(1, Ordering::SeqCst);
        HELD.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Counted))
    }

    fn refusing(_: &'static str) -> Option<Held> {
        None
    }

    fn waiting(kind: WaitKind) -> Event {
        Event::Waiting {
            kind,
            duration: Duration::from_secs(60),
        }
    }

    /// One test, because the counters are the process's: held through the
    /// walk and its rests, let go for the day's wait and at the end, asked of
    /// the system once per stretch rather than once per page.
    #[test]
    fn a_walk_is_kept_awake_except_while_it_waits_for_the_day() {
        let mut awake = KeepAwake::new("a test", true);
        awake.take = counted;
        awake.follow(&Event::Started {
            estimated: None,
            resumed: false,
        });
        awake.follow(&waiting(WaitKind::Step));
        awake.follow(&waiting(WaitKind::Sitting));
        assert_eq!(HELD.load(Ordering::SeqCst), 1);
        assert_eq!(
            ASKED.load(Ordering::SeqCst),
            1,
            "asked once for the stretch"
        );

        awake.follow(&waiting(WaitKind::Day));
        assert_eq!(
            HELD.load(Ordering::SeqCst),
            0,
            "not held through the day's wait"
        );

        awake.follow(&waiting(WaitKind::Step));
        assert_eq!(HELD.load(Ordering::SeqCst), 1);
        assert_eq!(ASKED.load(Ordering::SeqCst), 2);

        awake.follow(&Event::Finished {
            pages: 3,
            users: 30,
            reason: StopReason::Completed,
        });
        assert_eq!(HELD.load(Ordering::SeqCst), 0);

        awake.hold();
        drop(awake);
        assert_eq!(HELD.load(Ordering::SeqCst), 0, "dropping lets go");
    }

    /// A system that said no is not asked again on every page, and one never
    /// wanted is never asked.
    #[test]
    fn a_refusal_is_not_asked_again() {
        let mut awake = KeepAwake::new("a test", true);
        awake.take = refusing;
        awake.hold();
        assert!(awake.refused);
        awake.take = |_| panic!("asked again");
        awake.hold();

        let mut unwanted = KeepAwake::new("a test", false);
        unwanted.take = |_| panic!("a walk against a test server asks nothing");
        unwanted.hold();
    }
}
