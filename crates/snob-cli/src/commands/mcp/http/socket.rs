//! `snob mcp --listen socket`: the HTTP server on a Unix socket in snob's
//! data directory, which **only this user can reach and nobody else can
//! take**.
//!
//! A port on loopback is open to every user of the machine, and nothing
//! holds it while the server is not running, so another user who binds it
//! first is handed the key by the next client (`http.rs`, "The server does
//! not prove itself to its clients"). A socket here closes both: the data
//! directory is private (`paths::create_private_dir`, `0700`), so nobody
//! else can connect to it or put a socket of their own at its name, and the
//! socket itself is made `0600`. Each connection is checked again for who
//! is at the other end (`peer_cred`), as the owner's and the relay's are:
//! this user, or root, who can read everything snob keeps anyway (a tunnel
//! run as a system service is often root). A tunnel of the person's own
//! brings it elsewhere: `ssh -L 8750:<path>` from the far end, or
//! cloudflared's `unix:<path>` service.
//!
//! **One server at a time, at a fixed name**, so a tunnel's configuration
//! outlives a restart. Who serves is decided by a lock (`mcp.sock.lock`
//! beside it, held for the server's life), never by connecting: holding
//! it, a socket found at the name is one a server left when it was killed
//! (a release build's panic aborts), and is removed; anything there that is
//! not a socket is left alone, and the server does not start.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use tokio::net::{UnixListener, UnixStream};

use crate::exit::{ExitCode, ExitError};

/// The socket's name in the data directory.
pub const NAME: &str = "mcp.sock";

/// The lock's name beside it, which says whether a server holds the name.
const LOCK: &str = "mcp.sock.lock";

/// The longest socket path taken, short of the 104 bytes macOS allows and
/// the 108 Linux does (the relay's `MOST_SOCKET_PATH`).
const MOST_PATH: usize = 100;

/// The socket a server listens on, and the lock that makes it this
/// server's, held until it is dropped.
pub struct Held {
    listener: UnixListener,
    path: PathBuf,
    /// The socket's device and inode, so that dropping removes this
    /// server's socket and nothing put at its name since.
    made: (u64, u64),
    /// Held, never read: the lock lasts as long as the file is open.
    _lock: std::fs::File,
}

/// Where the socket is, for a data directory.
pub fn path(data_dir: &Path) -> std::io::Result<PathBuf> {
    Ok(std::path::absolute(data_dir)?.join(NAME))
}

/// Takes the name and listens on it: refused when another server holds
/// it, when its path is too long for a socket, or when something that is
/// not a socket is there.
pub fn hold(path: &Path) -> Result<Held> {
    if path.as_os_str().len() > MOST_PATH {
        return Err(ExitError::new(
            ExitCode::Error,
            format!(
                "{} is too long for a socket's path, which may be at most {MOST_PATH} bytes",
                path.display()
            ),
        )
        .with_hint("listen on a loopback address instead, such as --listen 127.0.0.1:8750")
        .into());
    }
    let lock_path = path.with_file_name(LOCK);
    let lock = snob_store::paths::try_lock_file(&lock_path)
        .with_context(|| format!("could not open {}", lock_path.display()))?;
    let Some(lock) = lock else {
        return Err(ExitError::new(
            ExitCode::Error,
            format!(
                "another snob mcp --listen is serving on {} already",
                path.display()
            ),
        )
        .with_hint("stop that one first, or use it")
        .into());
    };
    match std::fs::symlink_metadata(path) {
        // Left by a server that is gone, since the lock was free.
        Ok(found) if found.file_type().is_socket() => std::fs::remove_file(path)
            .with_context(|| format!("could not remove the old {}", path.display()))?,
        Ok(_) => {
            return Err(ExitError::new(
                ExitCode::Error,
                format!(
                    "{} is there and is not a socket, so snob leaves it alone",
                    path.display()
                ),
            )
            .with_hint("remove it if it is not something you keep there")
            .into());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("could not look at {}", path.display())),
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("could not listen on {}", path.display()))?;
    // The data directory is private already; this is the leaf.
    let made = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::symlink_metadata(path));
    let made = match made {
        Ok(made) => (made.dev(), made.ino()),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            return Err(e).with_context(|| format!("could not make {} private", path.display()));
        }
    };
    Ok(Held {
        listener,
        path: path.to_path_buf(),
        made,
        _lock: lock,
    })
}

impl Held {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The next connection, refused unless this user or root is at the
    /// other end.
    pub async fn accept(&self) -> std::io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        let theirs = stream.peer_cred()?.uid();
        // SAFETY: takes nothing and cannot fail.
        let ours = unsafe { libc::geteuid() };
        if theirs == ours || theirs == 0 {
            Ok(stream)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("a connection from user {theirs}, not this one"),
            ))
        }
    }
}

/// Removes the socket, when what is at its name is still this server's.
/// The lock goes with the file it is held on, after.
impl Drop for Held {
    fn drop(&mut self) {
        let ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|found| (found.dev(), found.ino()) == self.made);
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::exit_code_for;

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[tokio::test]
    async fn the_socket_is_private_and_gone_when_the_server_is() {
        let dir = private_dir();
        let path = path(dir.path()).unwrap();
        let held = hold(&path).unwrap();
        let found = std::fs::symlink_metadata(&path).unwrap();
        assert!(found.file_type().is_socket());
        assert_eq!(found.permissions().mode() & 0o777, 0o600);
        // This user is let in.
        let connecting = UnixStream::connect(&path);
        let (accepted, connected) = tokio::join!(held.accept(), connecting);
        accepted.unwrap();
        connected.unwrap();
        drop(held);
        assert!(std::fs::symlink_metadata(&path).is_err());
    }

    #[tokio::test]
    async fn a_second_server_is_refused_and_takes_nothing() {
        let dir = private_dir();
        let path = path(dir.path()).unwrap();
        let first = hold(&path).unwrap();
        let refused = hold(&path).err().expect("the name is held");
        assert_eq!(exit_code_for(&refused), ExitCode::Error);
        assert!(refused.to_string().contains("already"), "{refused}");
        // The first one's socket is untouched, and still answers.
        let connecting = UnixStream::connect(&path);
        let (accepted, connected) = tokio::join!(first.accept(), connecting);
        accepted.unwrap();
        connected.unwrap();
    }

    #[tokio::test]
    async fn a_socket_left_by_a_server_that_is_gone_is_replaced() {
        let dir = private_dir();
        let path = path(dir.path()).unwrap();
        // A server killed: its socket stays, its lock does not.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(std::fs::symlink_metadata(&path).is_ok());
        let held = hold(&path).unwrap();
        let connecting = UnixStream::connect(&path);
        let (accepted, connected) = tokio::join!(held.accept(), connecting);
        accepted.unwrap();
        connected.unwrap();
    }

    #[tokio::test]
    async fn what_is_not_a_socket_is_left_alone() {
        let dir = private_dir();
        let path = path(dir.path()).unwrap();
        std::fs::write(&path, b"somebody's").unwrap();
        let refused = hold(&path).err().expect("not a socket");
        assert!(refused.to_string().contains("not a socket"), "{refused}");
        assert_eq!(std::fs::read(&path).unwrap(), b"somebody's");
        // Nor is a link followed.
        std::fs::remove_file(&path).unwrap();
        let target = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(hold(&path).is_err());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn a_socket_put_at_the_name_since_is_not_removed() {
        let dir = private_dir();
        let path = path(dir.path()).unwrap();
        let held = hold(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let theirs = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(held);
        assert!(std::fs::symlink_metadata(&path).is_ok());
        drop(theirs);
    }

    #[test]
    fn a_path_too_long_for_a_socket_is_refused() {
        let long = PathBuf::from(format!("/{}/{NAME}", "d".repeat(MOST_PATH)));
        let refused = hold(&long).err().expect("too long");
        assert!(refused.to_string().contains("too long"), "{refused}");
    }
}
