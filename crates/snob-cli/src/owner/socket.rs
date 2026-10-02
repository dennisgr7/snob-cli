//! Where the owner listens and how a command reaches it, per platform, and
//! who may.
//!
//! **Only the user running snob, on both ends.** The session crosses this
//! channel with every request, so reaching it has to be as hard as reading
//! the stored session:
//!
//! - **Unix**: a socket in the data directory, which is `0700`, itself `0600`,
//!   and each end asks the kernel who is at the other (`peer_cred`) and hangs
//!   up on anybody but this user. One owner at a time is a lock on a file
//!   beside it, held while the owner listens; the socket file is only ever
//!   replaced by whoever holds it. A retired owner lets both go while it
//!   finishes with the commands still connected to it.
//! - **Windows**: a named pipe, whose name anybody on the machine can open or
//!   take first, so the name is not trusted at all. The owner creates it
//!   owned by this user, with a DACL naming only this user, as the first
//!   instance of that name — it refuses to join a pipe somebody else made —
//!   and refuses remote clients. A command, before it sends anything, checks
//!   that the pipe's owner is this user; the owner checks that the process
//!   at the other end of each connection runs as this user. A pipe another
//!   user took first is therefore a pipe this user will not talk to, and the
//!   command runs its browser itself. The name lasts while any instance of
//!   it is open, so a retired owner keeps it until its last command leaves.

use std::io;
#[cfg(unix)]
use std::path::PathBuf;

use snob_store::paths::AppPaths;

#[cfg(unix)]
pub(super) use unix::{Listener, Owner, ServerStream, Stream, bind, connect, owner_of};
#[cfg(windows)]
pub(super) use windows::{Listener, Owner, ServerStream, Stream, bind, connect, owner_of};

/// Whether a failed connection means nobody is listening, which a command
/// answers by starting the owner, rather than something that starting one
/// will not fix.
pub(super) fn nobody_there(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// The data directory as an absolute path: the command and the owner it
/// starts must name the same socket, and the owner runs from another working
/// directory.
fn data_dir(paths: &AppPaths) -> io::Result<std::path::PathBuf> {
    std::path::absolute(paths.data_dir())
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;

    use snob_store::paths::AppPaths;

    pub(crate) type Stream = tokio::net::UnixStream;
    pub(crate) type ServerStream = tokio::net::UnixStream;

    /// The socket, and the lock that makes this process the one owner.
    pub(crate) struct Listener {
        listener: tokio::net::UnixListener,
        socket: PathBuf,
        /// Held for as long as this listens; released when it is dropped,
        /// after the socket file is gone.
        _lock: std::fs::File,
    }

    /// Removes the socket while the lock is still held, so it can never
    /// remove one a later owner made.
    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    impl Listener {
        /// The next command of this user's to connect.
        pub(crate) async fn accept(&mut self) -> io::Result<ServerStream> {
            loop {
                let (stream, _) = self.listener.accept().await?;
                match same_user(&stream) {
                    Ok(()) => return Ok(stream),
                    Err(e) => tracing::warn!(error = %e, "refused a connection to the browsers"),
                }
            }
        }
    }

    /// Listens, or `None` when another owner already does.
    pub(crate) fn bind(paths: &AppPaths) -> io::Result<Option<Listener>> {
        let (socket, lock) = super::names(paths)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(lock)?;
        // SAFETY: a descriptor this process owns, for as long as `lock` lives.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock => Ok(None),
                _ => Err(error),
            };
        }
        // Left by an owner that did not get to remove it.
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        Ok(Some(Listener {
            listener,
            socket,
            _lock: lock,
        }))
    }

    /// Reaches the owner, if one is listening and it is this user's.
    pub(crate) async fn connect(paths: &AppPaths) -> io::Result<Stream> {
        let (socket, _) = super::names(paths)?;
        let stream = tokio::net::UnixStream::connect(socket).await?;
        same_user(&stream)?;
        Ok(stream)
    }

    /// The owner's process, to wait for. Only Windows needs it gone before
    /// its files are: here a file that is still open is removed all the same.
    pub(crate) struct Owner;

    impl Owner {
        pub(crate) fn wait_up_to(self, _patience: std::time::Duration) {}
    }

    pub(crate) fn owner_of(_stream: &Stream) -> Option<Owner> {
        None
    }

    fn same_user(stream: &tokio::net::UnixStream) -> io::Result<()> {
        let theirs = stream.peer_cred()?.uid();
        // SAFETY: takes nothing and cannot fail.
        let ours = unsafe { libc::geteuid() };
        if theirs == ours {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the other end runs as user {theirs}, not {ours}"),
            ))
        }
    }
}

/// The socket and the lock beside it.
#[cfg(unix)]
fn names(paths: &AppPaths) -> io::Result<(PathBuf, PathBuf)> {
    let dir = data_dir(paths)?;
    Ok((
        dir.join("browser-owner.sock"),
        dir.join("browser-owner.lock"),
    ))
}

/// The pipe's name: one per data directory, so that each sandbox of the test
/// suite has an owner of its own, and nothing anybody could not work out —
/// which is why the name protects nothing and the checks below do.
#[cfg(windows)]
fn pipe_name(paths: &AppPaths) -> io::Result<String> {
    let dir = data_dir(paths)?;
    // FNV-1a, for the same reason as the sandbox's keyring namespace: stable
    // from one run to the next, which `DefaultHasher` does not promise.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!(r"\\.\pipe\snob-ig-browsers-{hash:016x}"))
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use snob_store::paths::AppPaths;
    use snob_store::windows_user::{OneUserAcl, User};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        EqualSid, InitializeSecurityDescriptor, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl,
        SetSecurityDescriptorOwner,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    pub(crate) type Stream = NamedPipeClient;
    pub(crate) type ServerStream = NamedPipeServer;

    /// `SECURITY_DESCRIPTOR_REVISION`, which lives in a part of the Windows
    /// headers this crate does not otherwise need.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    /// Who the process `pid` runs as.
    fn user_of(pid: u32) -> io::Result<User> {
        // SAFETY: asks for the least access that lets its token be read.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the handle just opened, with that access.
        let user = unsafe { User::of(process) };
        // SAFETY: a process handle this function opened.
        unsafe { CloseHandle(process) };
        user
    }

    fn this_user(sid: PSID, what: impl std::fmt::Display) -> io::Result<()> {
        let ours = User::this_process()?;
        // SAFETY: two SIDs, alive for the call.
        if unsafe { EqualSid(ours.sid(), sid) } != 0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{what} is another user's"),
            ))
        }
    }

    /// The owner's process, to wait for it to be gone before the files it
    /// holds open are removed, which Windows refuses until then.
    pub(crate) struct Owner(HANDLE);

    // SAFETY: a process handle owned by this struct alone; Windows handles
    // have no thread affinity.
    unsafe impl Send for Owner {}
    // SAFETY: nothing reachable through `&self` touches the handle.
    unsafe impl Sync for Owner {}

    impl Owner {
        pub(crate) fn wait_up_to(self, patience: std::time::Duration) {
            let millis = u32::try_from(patience.as_millis()).unwrap_or(u32::MAX);
            // SAFETY: a process handle opened with `SYNCHRONIZE`.
            unsafe { WaitForSingleObject(self.0, millis) };
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            // SAFETY: owned by this struct, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(crate) fn owner_of(stream: &Stream) -> Option<Owner> {
        let mut pid: u32 = 0;
        // SAFETY: a pipe handle `stream` holds, and an out-parameter.
        if unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) } == 0 {
            return None;
        }
        // SAFETY: asks only to wait on it.
        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        (!process.is_null()).then_some(Owner(process))
    }

    /// Whether the command at the other end of `pipe` runs as this user.
    fn the_client_is_this_users(pipe: HANDLE) -> io::Result<()> {
        let mut pid: u32 = 0;
        // SAFETY: a pipe handle the caller holds, and an out-parameter.
        if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let theirs = user_of(pid)?;
        this_user(theirs.sid(), format_args!("process {pid} at the other end"))
    }

    /// Whether the pipe `pipe` reaches belongs to this user: its owner, which
    /// only whoever created it could set, and not the process serving it,
    /// whose id another user can arrange to be one of this user's by letting
    /// it go back to the pool of free ids.
    fn the_pipe_is_this_users(pipe: HANDLE) -> io::Result<()> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a pipe handle the caller holds, opened for reading, which
        // includes reading who owns it; out-parameters, the descriptor freed
        // below and the owner pointing into it.
        let read = unsafe {
            GetSecurityInfo(
                pipe,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if read != 0 {
            return Err(io::Error::from_raw_os_error(read as i32));
        }
        let checked = this_user(owner, "the pipe");
        // SAFETY: the descriptor the read produced, freed once.
        unsafe { LocalFree(descriptor) };
        checked
    }

    /// A security descriptor owned by this user, whose DACL allows this user
    /// and nobody else, and the attributes that carry it. Boxed, since the
    /// attributes point at the descriptor and the descriptor at the ACL and
    /// the SID.
    struct OnlyThisUser {
        user: User,
        acl: OneUserAcl,
        descriptor: Box<SECURITY_DESCRIPTOR>,
        attributes: SECURITY_ATTRIBUTES,
    }

    // SAFETY: the pointers inside point into buffers this struct owns, which
    // nothing else reaches; they are only read, by the call that creates a
    // pipe.
    unsafe impl Send for OnlyThisUser {}
    // SAFETY: the same; nothing reachable through `&self` writes.
    unsafe impl Sync for OnlyThisUser {}

    impl OnlyThisUser {
        fn new() -> io::Result<Box<Self>> {
            let user = User::this_process()?;
            let acl = OneUserAcl::new(&user, 0)?;
            let mut this = Box::new(Self {
                user,
                acl,
                descriptor: Box::new(SECURITY_DESCRIPTOR::default()),
                attributes: SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: std::ptr::null_mut(),
                    bInheritHandle: 0,
                },
            });
            let descriptor: PSECURITY_DESCRIPTOR =
                (&mut *this.descriptor as *mut SECURITY_DESCRIPTOR).cast();
            // SAFETY: a descriptor this function owns, in absolute format.
            if unsafe { InitializeSecurityDescriptor(descriptor, SECURITY_DESCRIPTOR_REVISION) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
            // The owner is what a command checks, so it is set rather than
            // left to the token's default, which for an elevated process is
            // the Administrators group.
            //
            // SAFETY: the SID lives in `this.user`, beside the descriptor.
            if unsafe { SetSecurityDescriptorOwner(descriptor, this.user.sid(), 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the ACL lives in `this.acl`, beside the descriptor.
            if unsafe { SetSecurityDescriptorDacl(descriptor, 1, this.acl.as_ptr(), 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            this.attributes.lpSecurityDescriptor = descriptor;
            Ok(this)
        }
    }

    /// The pipe, with the next instance of it waiting for a command.
    pub(crate) struct Listener {
        name: String,
        security: Box<OnlyThisUser>,
        next: NamedPipeServer,
    }

    fn create(name: &str, security: &OnlyThisUser, first: bool) -> io::Result<NamedPipeServer> {
        let attributes = (&security.attributes as *const SECURITY_ATTRIBUTES).cast_mut();
        // SAFETY: the attributes and what they point at live in `security`,
        // which outlives the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .pipe_mode(PipeMode::Byte)
                .create_with_security_attributes_raw(name, attributes.cast())
        }
    }

    impl Listener {
        /// The next command of this user's to connect.
        pub(crate) async fn accept(&mut self) -> io::Result<ServerStream> {
            loop {
                self.next.connect().await?;
                let fresh = create(&self.name, &self.security, false)?;
                let connected = std::mem::replace(&mut self.next, fresh);
                match the_client_is_this_users(connected.as_raw_handle()) {
                    Ok(()) => return Ok(connected),
                    Err(e) => tracing::warn!(error = %e, "refused a connection to the browsers"),
                }
            }
        }
    }

    /// Listens, or `None` when the name is taken: by another owner, or by
    /// somebody this user's commands will not talk to.
    pub(crate) fn bind(paths: &AppPaths) -> io::Result<Option<Listener>> {
        let name = super::pipe_name(paths)?;
        let security = OnlyThisUser::new()?;
        match create(&name, &security, true) {
            Ok(next) => Ok(Some(Listener {
                name,
                security,
                next,
            })),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Reaches the owner, if one is listening and it is this user's.
    pub(crate) async fn connect(paths: &AppPaths) -> io::Result<Stream> {
        let name = super::pipe_name(paths)?;
        let mut tries = 0;
        let client = loop {
            match ClientOptions::new().open(&name) {
                Ok(client) => break client,
                // Every instance is taken for the moment; the owner makes a
                // new one as it hands each over.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && tries < 50 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        };
        the_pipe_is_this_users(client.as_raw_handle())?;
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Who else is refused is the platform's to check, and not something a
    /// test running as one user can arrange; what is checked here is that one
    /// owner holds the name, that this user gets in, and on Unix the mode.
    #[tokio::test]
    async fn one_owner_listens_and_this_user_reaches_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(root.path());
        snob_store::paths::create_private_dir(paths.data_dir()).unwrap();

        assert!(nobody_there(&connect(&paths).await.unwrap_err()));

        let mut listener = bind(&paths).unwrap().expect("the first owner listens");
        assert!(
            bind(&paths).unwrap().is_none(),
            "a second owner finds the first there"
        );

        #[cfg(unix)]
        let (socket, _) = names(&paths).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the socket is this user's alone");
        }

        // Twice: the second connection is served by the instance made as the
        // first was handed over.
        for _ in 0..2 {
            let (client, server) = tokio::join!(connect(&paths), listener.accept());
            let client = client.expect("this user reaches it");
            server.expect("and is let in");
            #[cfg(windows)]
            assert!(
                owner_of(&client).is_some(),
                "the owner's process can be waited for"
            );
            #[cfg(unix)]
            let _ = owner_of(&client);
        }

        drop(listener);
        #[cfg(unix)]
        assert!(!socket.exists(), "the socket goes with its owner");
        // A pipe instance with an operation pending goes once the runtime has
        // seen that operation canceled, not the moment it is dropped; a new
        // owner is another process, started after this one has gone.
        let mut next = None;
        for _ in 0..200 {
            next = bind(&paths).unwrap();
            if next.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(next.is_some(), "and the next owner can take over");
    }
}
