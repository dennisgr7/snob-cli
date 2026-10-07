//! A logind inhibitor. See the module above.

/// The inhibitor's file descriptor: logind holds the lock for as long as it
/// is open, and closing it on drop lets go.
pub(super) struct Request(
    #[expect(dead_code, reason = "held for its drop")] zbus::zvariant::OwnedFd,
);

/// An `idle` lock in `block` mode, which delays automatic sleep and leaves a
/// suspend somebody asks for alone. Asked over the system bus with a blocking
/// call, which takes milliseconds and happens at most once per stretch of a
/// walk.
pub(super) fn request(reason: &'static str) -> Option<Request> {
    let asked = || -> zbus::Result<zbus::zvariant::OwnedFd> {
        let bus = zbus::blocking::Connection::system()?;
        let reply = bus.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &("idle", "snob", reason, "block"),
        )?;
        reply.body().deserialize()
    };
    match asked() {
        Ok(fd) => Some(Request(fd)),
        Err(e) => {
            tracing::debug!(error = %e, "logind would not hold off idle sleep");
            None
        }
    }
}
