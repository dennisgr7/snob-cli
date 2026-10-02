//! Starting the owner of the browsers, apart from the command that needed it.
//!
//! **It outlives the command, so it must take nothing of the command's with
//! it.** Its input and output are the null device and its errors go to a log
//! in the data directory: a program reading a command's output — a pipe, a
//! `$(…)`, the test suite — waits for every holder of that output to close
//! it, and an owner still holding it would keep that program waiting for
//! minutes. On Unix every descriptor this program opens is close-on-exec, so
//! the three it sets are the only ones of its own the owner gets; one snob
//! itself inherited open across `exec` — a `3>file`, a make jobserver's pipe
//! — passes on to the owner as it would to any child. On Windows the handles
//! it inherits are listed one by one, as for the browser (`pipe.rs`),
//! because `std::process::Command` would hand it every inheritable handle in
//! this process.
//!
//! **Nor the command's terminal.** A session of its own on Unix, a detached
//! process in a group of its own on Windows: a Ctrl+C typed at the command,
//! or its terminal closing, is not the owner's to hear. It leaves the job the
//! command may be in when that job allows it, so the job closing does not
//! take the browsers of every other command with it; where it does not, the
//! owner goes with the job and the next command starts another. A cgroup is
//! not left either: an owner started from a systemd unit ends with the unit,
//! and so do the requests of any other command it was serving.
//!
//! **It keeps the command's environment, for its whole life.** `PATH`, and
//! with it which browser is found on Linux; the locale and time zone the
//! browser reports; the desktop session Chromium picks its cookie keyring
//! from. An owner a cron job or a systemd unit started with a bare
//! environment sets these for every command it serves until it leaves;
//! `SNOB_NO_OWNER=1` in that job keeps its browsers its own.
//!
//! It runs from the root of the system drive, so it holds no directory
//! anybody may want to remove or unmount — the data directory included, which
//! `snob purge` removes and Windows will not while a process runs in it.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// Past this, the log starts again, as the next owner starts. Owners come and
/// go — a scheduled monitor leaves its between runs, which are further apart
/// than the linger — so only an owner some command keeps connected for weeks
/// appends past it, and at the default level it writes little.
const LOG_LIMIT: u64 = 1024 * 1024;

/// The owner just started, as far as the command that started it can tell.
pub(super) use platform::Started;

/// Starts `program` with `args` as the owner, logging to `log`.
pub(super) fn spawn(program: &Path, args: &[OsString], log: &Path) -> io::Result<Started> {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
    platform::spawn(program, args, &neutral_dir(), log)
}

/// A directory that is always there and nobody removes.
fn neutral_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("SystemDrive")
            .map(|mut drive| {
                // `C:` alone is the current directory on that drive.
                drive.push("\\");
                std::path::PathBuf::from(drive)
            })
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir)
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from("/")
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Told by the thread that reaps it when it has left.
    pub(crate) struct Started(Arc<AtomicBool>);

    impl Started {
        /// Whether it has already left.
        pub(crate) fn gone(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    pub(super) fn spawn(
        program: &Path,
        args: &[OsString],
        dir: &Path,
        log: &Path,
    ) -> io::Result<Started> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(log)?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log);
        // SAFETY: `setsid` is async-signal-safe and touches no memory, which
        // is the whole of what `pre_exec` asks.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        // Reaped by whoever is still here when it leaves: a command that
        // outlives it — a monitor — would otherwise keep a finished owner as
        // a zombie until it exits itself.
        let gone = Arc::new(AtomicBool::new(false));
        let told = Arc::clone(&gone);
        std::thread::spawn(move || {
            let _ = child.wait();
            told.store(true, Ordering::SeqCst);
        });
        Ok(Started(gone))
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    /// Its process handle, to ask whether it has left.
    pub(crate) struct Started(HANDLE);

    // SAFETY: a process handle owned by this struct alone; Windows handles
    // have no thread affinity.
    unsafe impl Send for Started {}
    // SAFETY: `gone` only waits on the handle, which any thread may do.
    unsafe impl Sync for Started {}

    impl Started {
        /// Whether it has already left.
        pub(crate) fn gone(&self) -> bool {
            // SAFETY: a process handle this struct owns; a zero wait only
            // asks.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for Started {
        fn drop(&mut self) {
            // SAFETY: owned by this struct, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) fn spawn(
        program: &Path,
        args: &[OsString],
        dir: &Path,
        log: &Path,
    ) -> io::Result<Started> {
        let null = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut inherited: [HANDLE; 2] = [null.as_raw_handle(), log.as_raw_handle()];
        for handle in inherited {
            // SAFETY: handles this function owns, through `null` and `log`.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let dir = crate::pipe::wide(&dir.display().to_string());

        let mut attribute_size: usize = 0;
        // SAFETY: asks for the size; fails by design.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_size)
        };
        let mut attribute_storage = vec![0u8; attribute_size];
        let attributes = attribute_storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: `attributes` points at the size just asked for.
        if unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut attribute_size) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the handle array outlives `CreateProcessW` below.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_mut_ptr().cast(),
                std::mem::size_of_val(&inherited),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if updated == 0 {
            let failure = io::Error::last_os_error();
            // SAFETY: initialized above.
            unsafe { DeleteProcThreadAttributeList(attributes) };
            return Err(failure);
        }

        // SAFETY: all-zero is the documented empty value; what matters is set
        // below.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited[0];
        startup.StartupInfo.hStdOutput = inherited[0];
        startup.StartupInfo.hStdError = inherited[1];
        startup.lpAttributeList = attributes;

        let plain = EXTENDED_STARTUPINFO_PRESENT | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        let mut result: io::Result<Started> =
            Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
        for flags in [plain | CREATE_BREAKAWAY_FROM_JOB, plain] {
            let mut command_line = crate::pipe::command_line(program, &args);
            // SAFETY: an out parameter; all-zero is what it is handed.
            let mut information: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: every pointer is to storage that outlives the call, and
            // `command_line` is the writable buffer the call requires.
            let created = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    command_line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    flags,
                    std::ptr::null(),
                    dir.as_ptr(),
                    &startup.StartupInfo,
                    &mut information,
                )
            };
            if created != 0 {
                // SAFETY: a handle `CreateProcessW` gave this process, closed
                // once; the process handle is kept, in `Started`.
                unsafe { CloseHandle(information.hThread) };
                result = Ok(Started(information.hProcess));
                break;
            }
            result = Err(io::Error::last_os_error());
            // A job that does not allow leaving it refuses the first flags,
            // and only those.
            if result.as_ref().err().and_then(io::Error::raw_os_error)
                != Some(ERROR_ACCESS_DENIED as i32)
            {
                break;
            }
        }
        // SAFETY: initialized above, and no longer needed either way.
        unsafe { DeleteProcThreadAttributeList(attributes) };
        drop((null, log));
        result
    }
}
