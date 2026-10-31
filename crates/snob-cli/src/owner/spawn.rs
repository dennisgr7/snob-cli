//! Starting the owner of the browsers, apart from the command that needed it.
//!
//! The start itself is `detach.rs`, shared with anything else that outlives
//! its command: what the owner takes from the command and what it leaves —
//! the output, the terminal, the job, the directory — is said there. What is
//! the owner's alone is here: its errors go to a log in the data directory,
//! and its output to the null device.
//!
//! **It keeps the command's environment, for its whole life**, and for the
//! owner that matters beyond itself: `PATH`, and with it which browser is
//! found on Linux; the locale and time zone the browser reports; the desktop
//! session Chromium picks its cookie keyring from. An owner a cron job or a
//! systemd unit started with a bare environment sets these for every command
//! it serves until it leaves; `SNOB_NO_OWNER=1` in that job keeps its
//! browsers its own. Likewise an owner that could not leave the command's job,
//! or that shares its cgroup, ends with it, and with the requests of any other
//! command it was serving; the next command starts another.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// Past this, the log starts again, as the next owner starts. Owners come and
/// go — a scheduled monitor leaves its between runs, which are further apart
/// than the linger — so only an owner some command keeps connected for weeks
/// appends past it, and at the default level it writes little.
const LOG_LIMIT: u64 = 1024 * 1024;

/// The owner just started, as far as the command that started it can tell.
pub(super) use crate::detach::Started;

/// Starts `program` with `args` as the owner, logging to `log`.
pub(super) fn spawn(program: &Path, args: &[OsString], log: &Path) -> io::Result<Started> {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
    crate::detach::spawn(program, args, None, log)
}
