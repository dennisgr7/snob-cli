//! Where the person running snob sits: at this machine, or at another one
//! connected to it over SSH, and whether a window opened here is one they
//! would see.
//!
//! It decides one thing, which login to offer first. A window opened by a
//! process in an SSH session goes nowhere on a Linux machine with no display —
//! Chromium exits 1 before it reads its pipe — and onto the machine's own
//! screen on a Mac or a Windows PC, where the person typing at the other end
//! of the connection never sees it. Either way `snob login --browser` would
//! fail, or sit for ten minutes waiting for a login in a window nobody can
//! reach, and the login that works from there is `snob login --remote`: the
//! same browser on this machine, without a window, shown in the browser of the
//! machine the person is at through a tunnel (`crate::relay`).
//!
//! **Read from the environment, which can be stale.** A `tmux` or `screen`
//! session started over SSH and attached later at the desk keeps the SSH
//! variables of the connection it was started from; the reverse case keeps
//! none. That is why the answer only orders the menu and words the advice:
//! `--browser` and `--remote` are always there to say outright, and are taken
//! as said. The one refusal is a window asked for on a Linux machine with no
//! display at all, which cannot open whatever the environment got wrong.

/// The SSH connection this process runs in, as `SSH_CONNECTION` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssh {
    /// The address of this machine the client connected to, and its port:
    /// what to give `ssh` from the other end. `None` when the variable is not
    /// there to read, which a `mosh` session or an older server can leave.
    pub server: Option<(String, u16)>,
    /// The account logged in, as `ssh` would be told it.
    pub user: Option<String>,
}

/// What a window opened here amounts to, for the person running snob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// It opens in front of them: a desktop session, or X11 forwarded over
    /// the SSH connection.
    Seen,
    /// It opens on this machine's own screen, which is not where they are: a
    /// Mac or a Windows PC reached over SSH.
    Elsewhere,
    /// It does not open at all: Linux with neither an X11 nor a Wayland
    /// display.
    Nowhere,
}

/// Where the person running snob sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seat {
    pub ssh: Option<Ssh>,
    pub window: Window,
}

/// The operating systems whose answer differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// Linux and the BSDs: a window needs `DISPLAY` or `WAYLAND_DISPLAY`.
    Unix,
    /// macOS and Windows: a window always opens, on the machine's screen.
    Desktop,
}

impl System {
    /// The system this was built for.
    pub fn here() -> Self {
        if cfg!(any(target_os = "macos", windows)) {
            Self::Desktop
        } else {
            Self::Unix
        }
    }
}

impl Seat {
    /// What the environment says. `var` is `std::env::var` outside the tests.
    pub fn from_env(var: impl Fn(&str) -> Option<String>, system: System) -> Self {
        let set = |name: &str| var(name).filter(|value| !value.trim().is_empty());
        let ssh = (set("SSH_CONNECTION").is_some()
            || set("SSH_CLIENT").is_some()
            || set("SSH_TTY").is_some())
        .then(|| Ssh {
            server: set("SSH_CONNECTION").as_deref().and_then(server_end),
            user: set("USER")
                .or_else(|| set("LOGNAME"))
                .or_else(|| set("USERNAME")),
        });
        let window = match system {
            // X11 forwarded over the connection sets `DISPLAY` to the
            // client's display, so a window there is in front of the person.
            System::Unix if set("DISPLAY").is_some() || set("WAYLAND_DISPLAY").is_some() => {
                Window::Seen
            }
            System::Unix => Window::Nowhere,
            System::Desktop if ssh.is_some() => Window::Elsewhere,
            System::Desktop => Window::Seen,
        };
        Self { ssh, window }
    }

    /// Where this process sits, read once.
    pub fn here() -> &'static Self {
        static HERE: std::sync::OnceLock<Seat> = std::sync::OnceLock::new();
        HERE.get_or_init(|| Self::from_env(|name| std::env::var(name).ok(), System::here()))
    }

    /// Whether the login to offer first is the one shown through a tunnel:
    /// a window opened here would not be in front of the person, or would
    /// not open.
    pub fn prefers_remote(&self) -> bool {
        self.window != Window::Seen
    }
}

/// The server's address and port out of `SSH_CONNECTION`, which is
/// `client-address client-port server-address server-port`.
fn server_end(connection: &str) -> Option<(String, u16)> {
    let parts: Vec<&str> = connection.split_whitespace().collect();
    let [_, _, address, port] = parts.as_slice() else {
        return None;
    };
    Some((address.to_string(), port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(vars: &[(&str, &str)], system: System) -> Seat {
        Seat::from_env(
            |name| {
                vars.iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.to_string())
            },
            system,
        )
    }

    /// Over SSH to a Linux machine with no display, nothing a window shows is
    /// seen, and the address the client reached is what `ssh` is given back.
    #[test]
    fn ssh_to_a_machine_with_no_display() {
        let found = seat(
            &[
                ("SSH_CONNECTION", "192.168.1.20 52344 192.168.1.50 22"),
                ("SSH_TTY", "/dev/pts/0"),
                ("USER", "someone"),
            ],
            System::Unix,
        );
        assert_eq!(found.window, Window::Nowhere);
        assert!(found.prefers_remote());
        let ssh = found.ssh.expect("an SSH session");
        assert_eq!(ssh.server, Some(("192.168.1.50".to_string(), 22)));
        assert_eq!(ssh.user.as_deref(), Some("someone"));
    }

    /// X11 forwarded over the connection puts the window in front of the
    /// person, so the window is still offered first.
    #[test]
    fn forwarded_x11_is_a_window_that_is_seen() {
        let found = seat(
            &[
                ("SSH_CONNECTION", "10.0.0.2 40000 10.0.0.1 2222"),
                ("DISPLAY", "localhost:10.0"),
            ],
            System::Unix,
        );
        assert_eq!(found.window, Window::Seen);
        assert!(!found.prefers_remote());
        assert_eq!(
            found.ssh.and_then(|s| s.server),
            Some(("10.0.0.1".to_string(), 2222))
        );
    }

    /// A desktop at the machine itself: nothing changes from before.
    #[test]
    fn a_desktop_session_is_not_remote() {
        let found = seat(&[("WAYLAND_DISPLAY", "wayland-0")], System::Unix);
        assert_eq!(found.ssh, None);
        assert_eq!(found.window, Window::Seen);
        assert_eq!(seat(&[], System::Desktop).window, Window::Seen);
    }

    /// A text console, or a service, on Linux: no SSH, and no window either.
    #[test]
    fn a_console_with_no_display_has_nowhere_for_a_window() {
        let found = seat(&[("DISPLAY", "  ")], System::Unix);
        assert_eq!(found.ssh, None);
        assert_eq!(found.window, Window::Nowhere);
    }

    /// A Mac or a Windows PC over SSH opens the window on its own screen.
    #[test]
    fn a_desktop_reached_over_ssh_shows_the_window_elsewhere() {
        let found = seat(
            &[("SSH_CLIENT", "fe80::1 50000 22"), ("USERNAME", "someone")],
            System::Desktop,
        );
        assert_eq!(found.window, Window::Elsewhere);
        assert!(found.prefers_remote());
        let ssh = found.ssh.expect("an SSH session");
        assert_eq!(ssh.server, None, "SSH_CLIENT does not name the server");
        assert_eq!(ssh.user.as_deref(), Some("someone"));
    }

    /// IPv6 addresses carry colons and no brackets in the variable.
    #[test]
    fn the_server_end_is_read_whatever_the_address() {
        assert_eq!(
            server_end("2001:db8::2 50000 2001:db8::1 22"),
            Some(("2001:db8::1".to_string(), 22))
        );
        assert_eq!(server_end("only three parts"), None);
        assert_eq!(server_end("a 1 b not-a-port"), None);
    }
}
