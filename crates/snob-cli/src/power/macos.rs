//! `caffeinate`, the system's own way to hold off idle sleep. See the module
//! above.

use std::process::{Child, Command, Stdio};

/// The `caffeinate` holding the machine awake: ended on drop, and ending by
/// itself when this process does (`-w`).
pub(super) struct Request(Child);

/// `reason` is not passed on: `caffeinate` takes none, and `pmset -g
/// assertions` names it, with snob's pid beside it.
pub(super) fn request(_reason: &'static str) -> Option<Request> {
    // One start at a time with the browsers' (`pipe`), so that none of their
    // pipes is caught open across this one's `exec`.
    let spawned = {
        let _starting = crate::pipe::one_start_at_a_time();
        Command::new("/usr/bin/caffeinate")
            .arg("-i")
            .arg("-w")
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    match spawned {
        Ok(child) => Some(Request(child)),
        Err(e) => {
            tracing::debug!(error = %e, "caffeinate could not be started");
            None
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
