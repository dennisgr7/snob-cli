//! The process that started this one, and hearing when it is gone.
//!
//! **A parent dying sends its children nothing.** A closed terminal sends
//! `SIGHUP`, a service manager `SIGTERM`, a console closing its event on
//! Windows, and `interrupt` hears all of them; but a program that runs snob
//! and dies — a script killed by its timeout, an agent's shell tool, a
//! tunnel's launcher, an app that crashed — tells nobody. Its snob was
//! adopted by the system and went on: a walk read page after page for
//! nobody, spending the account's budget and holding its claim on the list,
//! only to fail writing a result nobody would read; `snob mcp --listen` kept
//! its port for ever. So a process that a program drives watches that
//! program, and its going is one more of the system's ways of asking it to
//! stop (`interrupt::stop_when_parent_exits`): the walk saves its page and
//! gives its claim back, exactly as for `SIGTERM`.
//!
//! **Not every snob is watched** ([`worth_watching`]). A command at a
//! terminal is already told by `SIGHUP` when the terminal goes; a command
//! somebody detached on purpose (`nohup`, `setsid`, a service) must outlive
//! whoever started it; and the processes snob itself detaches to outlive the
//! command that started them (the owner of the browsers, a job's supervisor)
//! are what they are for.
//!
//! **Watched as a process, never as a thread.** Linux's `PR_SET_PDEATHSIG`
//! fires when the *thread* that started this process exits, and a parent that
//! starts its children from a short-lived thread (a pool's blocking thread, a
//! test harness's per-test thread) would stop every snob it ran for no reason.
//! So on Linux the parent is watched through a `pidfd`, on macOS through
//! `kqueue`, on Windows through a handle to it; each is opened while the
//! parent is known to be this process's ([`arm`]), so a pid used again by
//! another process later is never mistaken for it.

/// What [`worth_watching`] decides on, read once at the start
/// ([`situation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Situation {
    /// Standard input or output is a pipe or a socket: a program is on the
    /// other end, not a terminal or a file.
    pub(crate) driven: bool,
    /// `SIGHUP` was ignored when this process started, which is what `nohup`
    /// leaves: somebody asked for it to outlive its terminal and its parent.
    pub(crate) hangup_ignored: bool,
    /// This process leads a session of its own, which is what `setsid`
    /// leaves (and what `detach` gives the processes it starts on Unix).
    pub(crate) session_leader: bool,
    /// A process snob starts to outlive the command that started it (the
    /// owner of the browsers, a job's supervisor).
    pub(crate) detached_command: bool,
    /// `SNOB_NO_PARENT_WATCH` is set.
    pub(crate) opted_out: bool,
    /// The parent is the system's first process (pid 1 on Unix), which a
    /// service manager is, and which never goes while anything runs.
    pub(crate) parent_is_init: bool,
}

/// Whether to stop when the parent goes, in `situation`.
///
/// **Only a process a program drives**: at a terminal `SIGHUP` already says
/// it, and a command whose output goes to a file has nobody to wait for its
/// answer, so it keeps what it did before (finishing). Never one detached on
/// purpose, by a person or by snob. `SNOB_NO_PARENT_WATCH` is the way out for
/// a case these rules do not foresee; it stays out of the help, as
/// `SNOB_IGNORE_COOLDOWN` does.
pub(crate) fn worth_watching(situation: Situation) -> bool {
    let Situation {
        driven,
        hangup_ignored,
        session_leader,
        detached_command,
        opted_out,
        parent_is_init,
    } = situation;
    driven
        && !hangup_ignored
        && !session_leader
        && !detached_command
        && !opted_out
        && !parent_is_init
}

/// This process's [`Situation`], `detached_command` being whether the command
/// it runs is one snob starts to outlive its starter.
///
/// **Read before anything installs a signal handler**: `interrupt` listens
/// for `SIGHUP`, which replaces the disposition `nohup` left.
pub(crate) fn situation(detached_command: bool) -> Situation {
    Situation {
        driven: platform::driven(),
        hangup_ignored: hangup_ignored(),
        session_leader: platform::session_leader(),
        detached_command,
        opted_out: std::env::var_os("SNOB_NO_PARENT_WATCH").is_some(),
        parent_is_init: platform::parent_is_init(),
    }
}

/// Whether `SIGHUP` reached this process ignored, which is what `nohup`
/// leaves (never on Windows, which has no such signal). `interrupt` asks it
/// too, before it listens for `SIGHUP`, so as not to undo what `nohup` did.
pub(crate) fn hangup_ignored() -> bool {
    platform::hangup_ignored()
}

/// The parent, held open so that it can be waited for.
pub(crate) enum Parent {
    /// Alive when it was opened, and this process's.
    Open(platform::Process),
    /// Already gone when it was looked for: this process had been adopted.
    Gone,
    /// It could not be opened (another user's process, a kernel too old and
    /// a sandbox that forbids asking): it is watched by asking, now and then,
    /// whether this process still has it.
    #[cfg(unix)]
    Asked(platform::Asked),
    /// Nothing can be known about it; it is not watched. Only on Windows,
    /// where a parent that cannot be opened cannot be asked after either.
    #[cfg(windows)]
    Unknowable,
}

/// Opens the parent this process has now.
pub(crate) fn arm() -> Parent {
    platform::arm()
}

/// How long the parent is left between two questions, where it can only be
/// asked after. A walk stops between two requests, seconds apart, so a second
/// adds nothing anybody would notice, and costs one system call.
#[cfg(unix)]
const ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

impl Parent {
    /// Blocks until the parent is gone. Returns at once when it already is;
    /// never returns when it cannot be known.
    pub(crate) fn wait(self) {
        match self {
            Parent::Open(process) => process.wait(),
            Parent::Gone => {}
            #[cfg(unix)]
            Parent::Asked(asked) => {
                while asked.still_ours() {
                    std::thread::sleep(ASK_EVERY);
                }
            }
            #[cfg(windows)]
            Parent::Unknowable => loop {
                std::thread::park();
            },
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::Parent;
    pub(crate) use super::unix::{Asked, driven, hangup_ignored, parent_is_init, session_leader};

    /// A `pidfd` for the parent.
    pub(crate) struct Process(OwnedFd);

    pub(crate) fn arm() -> Parent {
        // SAFETY: no arguments, cannot fail.
        let pid = unsafe { libc::getppid() };
        match open(pid) {
            Opened::Open(process) => {
                // Opened by pid, so it is this process's parent only if this
                // process has not been adopted meanwhile: asked after the
                // pidfd is held, a pid used again cannot pass for it.
                // SAFETY: no arguments, cannot fail.
                if unsafe { libc::getppid() } == pid {
                    Parent::Open(process)
                } else {
                    Parent::Gone
                }
            }
            Opened::Gone => Parent::Gone,
            Opened::Refused => Parent::Asked(Asked::of(pid)),
        }
    }

    pub(crate) enum Opened {
        Open(Process),
        Gone,
        /// `pidfd_open` is missing (before Linux 5.3) or forbidden.
        Refused,
    }

    /// Opens the process `pid`.
    pub(crate) fn open(pid: libc::pid_t) -> Opened {
        // SAFETY: `pidfd_open(pid, 0)` takes two integers and returns a new
        // descriptor or -1; libc has no wrapper on musl, hence the syscall.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => Opened::Gone,
                _ => Opened::Refused,
            };
        }
        let Ok(fd) = i32::try_from(fd) else {
            return Opened::Refused;
        };
        // SAFETY: a descriptor the kernel just handed this process, owned by
        // nobody else.
        Opened::Open(Process(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    impl Process {
        /// Blocks until the process has exited: a pidfd reads as ready then.
        pub(crate) fn wait(self) {
            let mut ready = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                // SAFETY: one `pollfd`, a local, for a descriptor this struct
                // holds open; -1 waits for as long as it takes.
                let answered = unsafe { libc::poll(&mut ready, 1, -1) };
                if answered > 0 {
                    return;
                }
                if answered < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    // A pidfd that will not poll says nothing more by
                    // waiting on it; treating it as gone would stop a walk
                    // for a reason that is not its parent's, so wait on.
                    loop {
                        std::thread::park();
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::Parent;
    pub(crate) use super::unix::{Asked, driven, hangup_ignored, parent_is_init, session_leader};

    /// A `kqueue` that holds one registration: the parent's exit.
    pub(crate) struct Process(OwnedFd);

    pub(crate) fn arm() -> Parent {
        // SAFETY: no arguments, cannot fail.
        let pid = unsafe { libc::getppid() };
        match open(pid) {
            Opened::Open(process) => {
                // The registration is by pid: the parent is still this
                // process's only if it has not been adopted meanwhile.
                // SAFETY: no arguments, cannot fail.
                if unsafe { libc::getppid() } == pid {
                    Parent::Open(process)
                } else {
                    Parent::Gone
                }
            }
            Opened::Gone => Parent::Gone,
            Opened::Refused => Parent::Asked(Asked::of(pid)),
        }
    }

    pub(crate) enum Opened {
        Open(Process),
        Gone,
        Refused,
    }

    /// A kqueue told when `pid` exits.
    pub(crate) fn open(pid: libc::pid_t) -> Opened {
        // SAFETY: no arguments; returns a new descriptor or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Opened::Refused;
        }
        // SAFETY: a descriptor the kernel just handed this process.
        let kq = unsafe { OwnedFd::from_raw_fd(kq) };
        let Ok(ident) = usize::try_from(pid) else {
            return Opened::Refused;
        };
        // SAFETY: `kevent` is plain data, for which all zeros is valid.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = ident;
        change.filter = libc::EVFILT_PROC;
        change.flags = libc::EV_ADD | libc::EV_ONESHOT;
        change.fflags = libc::NOTE_EXIT;
        // SAFETY: one change, a local, on the kqueue just made; no events
        // asked for, so no timeout is needed and none is waited.
        let added = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if added < 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => Opened::Gone,
                _ => Opened::Refused,
            };
        }
        Opened::Open(Process(kq))
    }

    impl Process {
        /// Blocks until the kqueue reports the exit.
        pub(crate) fn wait(self) {
            loop {
                // SAFETY: as for the registration.
                let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                // SAFETY: no changes; room for one event, a local; a null
                // timeout waits for as long as it takes.
                let answered = unsafe {
                    libc::kevent(
                        self.0.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        &mut event,
                        1,
                        std::ptr::null(),
                    )
                };
                if answered > 0 {
                    return;
                }
                if answered < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    // As on Linux: a queue that will not wait is no news of
                    // the parent.
                    loop {
                        std::thread::park();
                    }
                }
            }
        }
    }
}

/// What every Unix shares: the situation, and asking after the parent where
/// it cannot be waited for.
#[cfg(unix)]
mod unix {
    use std::os::fd::RawFd;

    /// Whether `fd` is a pipe or a socket.
    fn a_program_on(fd: RawFd) -> bool {
        // SAFETY: `stat` is plain data, for which all zeros is valid.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: a standard descriptor, which may be closed (then `fstat`
        // fails and nothing is read), and a local to write into.
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return false;
        }
        let kind = stat.st_mode & libc::S_IFMT;
        kind == libc::S_IFIFO || kind == libc::S_IFSOCK
    }

    pub(crate) fn driven() -> bool {
        a_program_on(libc::STDIN_FILENO) || a_program_on(libc::STDOUT_FILENO)
    }

    pub(crate) fn hangup_ignored() -> bool {
        // SAFETY: `sigaction` is plain data, for which all zeros is valid.
        let mut now: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a null new action only reads the current one into `now`.
        if unsafe { libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut now) } != 0 {
            return false;
        }
        now.sa_sigaction == libc::SIG_IGN
    }

    pub(crate) fn session_leader() -> bool {
        // SAFETY: no pointers; asks about this process.
        unsafe { libc::getsid(0) == libc::getpid() }
    }

    pub(crate) fn parent_is_init() -> bool {
        // SAFETY: no arguments, cannot fail.
        unsafe { libc::getppid() == 1 }
    }

    /// The parent, by pid, asked after: once it is gone this process is
    /// adopted, and its parent pid changes.
    pub(crate) struct Asked(libc::pid_t);

    impl Asked {
        pub(crate) fn of(pid: libc::pid_t) -> Self {
            Self(pid)
        }

        pub(crate) fn still_ours(&self) -> bool {
            // SAFETY: no arguments, cannot fail.
            unsafe { libc::getppid() == self.0 }
        }
    }
}

/// Any other Unix: no way to wait, so the parent is asked after.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod platform {
    use super::Parent;
    pub(crate) use super::unix::{Asked, driven, hangup_ignored, parent_is_init, session_leader};

    pub(crate) enum Process {}

    impl Process {
        pub(crate) fn wait(self) {
            match self {}
        }
    }

    pub(crate) fn arm() -> Parent {
        // SAFETY: no arguments, cannot fail.
        Parent::Asked(Asked::of(unsafe { libc::getppid() }))
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType, SYNCHRONIZE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, INFINITE, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    use super::Parent;

    /// A handle to the parent, opened to be waited on.
    pub(crate) struct Process(HANDLE);

    // SAFETY: a process handle owned by this struct alone; Windows handles
    // have no thread affinity.
    unsafe impl Send for Process {}

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: owned by this struct, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    impl Process {
        pub(crate) fn wait(self) {
            // SAFETY: a process handle opened with `SYNCHRONIZE`, held until
            // this returns.
            unsafe { WaitForSingleObject(self.0, INFINITE) };
        }
    }

    fn a_program_on(handle: HANDLE) -> bool {
        // SAFETY: a standard handle, which may be null or invalid; then the
        // type is unknown, never a pipe.
        unsafe { GetFileType(handle) == FILE_TYPE_PIPE }
    }

    pub(crate) fn driven() -> bool {
        a_program_on(std::io::stdin().as_raw_handle())
            || a_program_on(std::io::stdout().as_raw_handle())
    }

    /// Windows has no `nohup` to ask about.
    pub(crate) fn hangup_ignored() -> bool {
        false
    }

    /// Nor sessions in that sense; the processes snob detaches are named by
    /// their command instead.
    pub(crate) fn session_leader() -> bool {
        false
    }

    /// No process plays init's part: a parent of pid 0 or 4 would be the
    /// kernel's, which `OpenProcess` refuses, and that is `Unknowable`.
    pub(crate) fn parent_is_init() -> bool {
        false
    }

    /// The pid of the process that started this one, from a snapshot of the
    /// processes: the documented way, and the only one outside `ntdll`.
    fn parent_pid() -> Option<u32> {
        // SAFETY: a snapshot of the processes, closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: no arguments.
        let me = unsafe { GetCurrentProcessId() };
        // SAFETY: plain data, for which all zeros is valid.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        // SAFETY: the snapshot just taken and a local entry whose size is
        // set, as both calls require.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32ProcessID == me {
                found = Some(entry.th32ParentProcessID);
                break;
            }
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: the snapshot, closed once.
        unsafe { CloseHandle(snapshot) };
        found
    }

    /// When `process` was created, in the units `FILETIME` counts.
    fn created(process: HANDLE) -> Option<u64> {
        let mut times = [FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }; 4];
        let [born, exited, kernel, user] = &mut times;
        // SAFETY: a handle with at least limited query access, and four
        // locals to write into.
        let ok = unsafe { GetProcessTimes(process, born, exited, kernel, user) };
        (ok != 0)
            .then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }

    pub(crate) fn arm() -> Parent {
        let Some(pid) = parent_pid() else {
            return Parent::Unknowable;
        };
        match open(pid) {
            Opened::Open(process) => {
                // Windows hands a parent's pid to the next process once the
                // parent is gone, and never changes it here: a process that
                // was created after this one cannot have started it, so the
                // parent is gone and the pid is somebody else's.
                // SAFETY: a pseudo-handle for this process, never closed.
                let ours = created(unsafe { GetCurrentProcess() });
                match (created(process.0), ours) {
                    (Some(theirs), Some(ours)) if theirs > ours => Parent::Gone,
                    (Some(_), Some(_)) => Parent::Open(process),
                    _ => Parent::Unknowable,
                }
            }
            Opened::Gone => Parent::Gone,
            Opened::Refused => Parent::Unknowable,
        }
    }

    pub(crate) enum Opened {
        Open(Process),
        Gone,
        /// Another user's process, or a protected one.
        Refused,
    }

    /// Opens the process `pid` to wait on it and to ask when it was created.
    pub(crate) fn open(pid: u32) -> Opened {
        // SAFETY: asks only to wait on it and to read its times.
        let handle =
            unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            // SAFETY: no arguments; the error of the call just made.
            return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
                Opened::Gone
            } else {
                Opened::Refused
            };
        }
        Opened::Open(Process(handle))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A program on the other end, and nobody asked for it to live on.
    const DRIVEN: Situation = Situation {
        driven: true,
        hangup_ignored: false,
        session_leader: false,
        detached_command: false,
        opted_out: false,
        parent_is_init: false,
    };

    #[test]
    fn a_process_a_program_drives_watches_its_parent() {
        assert!(worth_watching(DRIVEN));
    }

    /// At a terminal `SIGHUP` says it, and output to a file has nobody
    /// waiting on it.
    #[test]
    fn a_terminal_or_a_file_is_not_watched() {
        assert!(!worth_watching(Situation {
            driven: false,
            ..DRIVEN
        }));
    }

    #[test]
    fn whatever_was_detached_on_purpose_outlives_its_parent() {
        for detached in [
            Situation {
                hangup_ignored: true,
                ..DRIVEN
            },
            Situation {
                session_leader: true,
                ..DRIVEN
            },
            Situation {
                detached_command: true,
                ..DRIVEN
            },
            Situation {
                parent_is_init: true,
                ..DRIVEN
            },
            Situation {
                opted_out: true,
                ..DRIVEN
            },
        ] {
            assert!(!worth_watching(detached), "{detached:?}");
        }
    }

    /// The process waited on is opened by pid and waited on as each
    /// platform does it; a child stands in for the parent, which a test
    /// cannot kill.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn waiting_on_a_process_returns_once_it_has_exited() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("sleep runs");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping", "-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("cmd runs");

        #[cfg(unix)]
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        #[cfg(windows)]
        let pid = child.id();
        let platform::Opened::Open(process) = platform::open(pid) else {
            panic!("a running child opens");
        };
        let (done, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            process.wait();
            let _ = done.send(());
        });
        assert!(
            heard.recv_timeout(Duration::from_millis(500)).is_err(),
            "the wait returned while the child was still running"
        );

        child.kill().unwrap();
        child.wait().unwrap();
        heard
            .recv_timeout(Duration::from_secs(10))
            .expect("the wait returned once the child exited");
    }
}
