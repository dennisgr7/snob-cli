//! Whether any other snob is running, which is what "a cold start" means.
//!
//! **Every snob that counts holds `running.lock` shared for its whole life**,
//! and the one that would apply a version asks for it alone, without
//! waiting: granted, nobody else holds it, so nobody runs from the binary
//! about to be replaced or is halfway through a write. The system lets go of
//! a lock when its process ends however it ends, so a crash leaves nothing
//! held, and after a restart nobody holds it at all: that is the "or after a
//! restart" of the notice, with no mechanism of its own.
//!
//! **What does not count**: `snob mcp`, which a client may keep running for
//! days and which already refuses every call once its binary changed
//! (`commands::mcp::child::Launch::upgraded`), so the client starts it
//! again; and the owner of the browsers, which lingers minutes after the
//! last command and is retired by the next command of a newer build
//! (`owner::wire::build`). A job's supervisor counts: its walk is a command.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

fn open(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

/// This process counts as running until the returned file is dropped.
/// `None` when it cannot hold the lock: the one applying a version holds it
/// alone for the moment it takes, and a command does not wait for that.
pub(crate) fn count_me(path: &Path) -> Option<File> {
    let file = open(path).ok()?;
    match file.try_lock_shared() {
        Ok(()) => Some(file),
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => None,
    }
}

/// The lock alone, when no other snob holds it: `None` when one runs.
pub(crate) fn alone(path: &Path) -> Option<File> {
    let file = open(path).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nobody_is_alone_while_another_counts_and_anybody_is_once_it_goes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("running.lock");
        let one = count_me(&path).expect("counted");
        let two = count_me(&path).expect("many count at once");
        assert!(alone(&path).is_none());
        drop(one);
        assert!(alone(&path).is_none());
        drop(two);
        let applying = alone(&path).expect("nobody runs");
        assert!(count_me(&path).is_none(), "nobody counts while one applies");
        drop(applying);
        assert!(count_me(&path).is_some());
    }
}
