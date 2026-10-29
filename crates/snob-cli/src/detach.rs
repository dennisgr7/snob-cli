//! Starting a process apart from the command that needed it, to outlive it.
//!
//! The owner of the browsers is started this way (`owner/spawn.rs`), and
//! anything else that must go on after the command that started it ends is
//! started here too, so there is one home for what such a process may take
//! from the command and what it must leave behind.
//!
//! **It takes nothing of the command's output with it.** Its input is the
//! null device, and its output and errors go to the files it is given (its
//! output to the null device when none is): a program reading a command's
//! output — a pipe, a `$(…)`, the test suite — waits for every holder of that
//! output to close it, and a process still holding it would keep that
//! program waiting for as long as it runs. On Unix every descriptor this
//! program opens is close-on-exec, so the three it sets are the only ones of
//! its own the process gets; one snob itself inherited open across `exec` — a
//! `3>file`, a make jobserver's pipe — passes on as it would to any child. On
//! Windows the handles it inherits are listed one by one, as for the browser
//! (`pipe.rs`), because `std::process::Command` would hand it every
//! inheritable handle in this process.
//!
//! **Nor the command's terminal.** A session of its own on Unix, a detached
//! process in a group of its own on Windows: a Ctrl+C typed at the command,
//! or its terminal closing, is not the process's to hear. It leaves the job
//! the command may be in when that job allows it ([`Started::left_job`]), so
//! the job closing does not take it along; where the job does not allow it,
//! the process goes with the job. A cgroup is not left either: a process
//! started from a systemd unit ends with the unit.
//!
//! **It keeps the command's environment, for its whole life.** Whatever the
//! command was started with, a bare one from a cron job or a systemd unit
//! included, is what the process has until it leaves.
//!
//! It runs from the root of the system drive, so it holds no directory
//! anybody may want to remove or unmount — the data directory included, which
//! `snob purge` removes and Windows will not while a process runs in it.
//!
//! Only `std`, `libc` and `windows-sys`, all three in every build: nothing
//! here waits on a feature.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// A process just started, as far as the command that started it can tell.
pub(crate) struct Started {
    /// Whether it left the job the command was in. On Windows, whether the
    /// job allowed `CREATE_BREAKAWAY_FROM_JOB` (a command in no job leaves
    /// none, and counts as having left); where it did not, the process ends
    /// when the job is closed, as it would for a client that ends its own
    /// process tree. On Unix always: a session of its own is all a terminal
    /// or a process group can reach, and a cgroup is never left.
    pub(crate) left_job: bool,
    process: platform::Process,
}

impl Started {
    /// Whether it has already left.
    pub(crate) fn gone(&self) -> bool {
        self.process.gone()
    }
}

/// Starts `program` with `args`, detached, its output appended to `stdout`
/// (the null device when there is none) and its errors to `stderr`. Both
/// files are created when missing, readable by this user alone on Unix (on
/// Windows they take the directory's access list, user-only in the data
/// directory).
pub(crate) fn spawn(
    program: &Path,
    args: &[OsString],
    stdout: Option<&Path>,
    stderr: &Path,
) -> io::Result<Started> {
    platform::spawn(program, args, &neutral_dir(), stdout, stderr)
}

/// A directory that is always there and nobody removes.
fn neutral_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("SystemDrive")
            .map(|mut drive| {
                // `C:` alone is the current directory on that drive.
                drive.push("\\");
                PathBuf::from(drive)
            })
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir)
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/")
    }
}

/// Opens `path` to append to, creating it when missing.
fn append_to(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(unix)]
mod platform {
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Told by the thread that reaps it when it has left.
    pub(super) struct Process(Arc<AtomicBool>);

    impl Process {
        pub(super) fn gone(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    pub(super) fn spawn(
        program: &Path,
        args: &[OsString],
        dir: &Path,
        stdout: Option<&Path>,
        stderr: &Path,
    ) -> io::Result<super::Started> {
        let stdout = match stdout {
            Some(path) => Stdio::from(super::append_to(path)?),
            None => Stdio::null(),
        };
        let stderr = super::append_to(stderr)?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
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
        // outlives it — a monitor — would otherwise keep a finished process
        // as a zombie until it exits itself.
        let gone = Arc::new(AtomicBool::new(false));
        let told = Arc::clone(&gone);
        std::thread::spawn(move || {
            let _ = child.wait();
            told.store(true, Ordering::SeqCst);
        });
        Ok(super::Started {
            left_job: true,
            process: Process(gone),
        })
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
    pub(super) struct Process(HANDLE);

    // SAFETY: a process handle owned by this struct alone; Windows handles
    // have no thread affinity.
    unsafe impl Send for Process {}
    // SAFETY: `gone` only waits on the handle, which any thread may do.
    unsafe impl Sync for Process {}

    impl Process {
        pub(super) fn gone(&self) -> bool {
            // SAFETY: a process handle this struct owns; a zero wait only
            // asks.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: owned by this struct, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) fn spawn(
        program: &Path,
        args: &[OsString],
        dir: &Path,
        stdout: Option<&Path>,
        stderr: &Path,
    ) -> io::Result<super::Started> {
        let null = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")?;
        let stdout = stdout.map(super::append_to).transpose()?;
        let stderr = super::append_to(stderr)?;
        // Each handle once: the list is what the process inherits, and every
        // file above was opened apart, so no two are the same handle even
        // when two paths are.
        let mut inherited: Vec<HANDLE> = vec![null.as_raw_handle()];
        inherited.extend(stdout.as_ref().map(|f| f.as_raw_handle()));
        inherited.push(stderr.as_raw_handle());
        for &handle in &inherited {
            // SAFETY: handles this function owns, through the files above.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let (input, output, errors) = (
            null.as_raw_handle(),
            stdout
                .as_ref()
                .map_or(null.as_raw_handle(), |f| f.as_raw_handle()),
            stderr.as_raw_handle(),
        );
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
        // SAFETY: the handle list outlives `CreateProcessW` below.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_mut_ptr().cast(),
                std::mem::size_of_val(inherited.as_slice()),
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
        startup.StartupInfo.hStdInput = input;
        startup.StartupInfo.hStdOutput = output;
        startup.StartupInfo.hStdError = errors;
        startup.lpAttributeList = attributes;

        let plain = EXTENDED_STARTUPINFO_PRESENT | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        let mut result: io::Result<super::Started> =
            Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32));
        for (flags, left_job) in [(plain | CREATE_BREAKAWAY_FROM_JOB, true), (plain, false)] {
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
                // once; the process handle is kept, in `Process`.
                unsafe { CloseHandle(information.hThread) };
                result = Ok(super::Started {
                    left_job,
                    process: Process(information.hProcess),
                });
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
        drop((null, stdout, stderr));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits for `started` to leave, failing the test past a generous bound.
    fn wait_gone(started: &Started) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !started.gone() {
            assert!(Instant::now() < deadline, "the process never left");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// This very test binary, which lists its tests on standard output and
    /// refuses an option it does not know on standard error, the same on
    /// every platform, and runs no test for either.
    fn this_binary() -> PathBuf {
        std::env::current_exe().expect("the test binary has a path")
    }

    #[test]
    fn a_detached_start_writes_its_output_and_errors_where_it_is_told() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (out, err) = (dir.path().join("out.log"), dir.path().join("err.log"));
        let started = spawn(&this_binary(), &["--list".into()], Some(&out), &err)
            .expect("the process starts");
        wait_gone(&started);
        let listed = std::fs::read_to_string(&out).expect("its output is in the file");
        assert!(
            listed.contains("a_detached_start_writes_its_output_and_errors_where_it_is_told"),
            "{listed}"
        );

        let refused = spawn(
            &this_binary(),
            &["--no-such-option-anywhere".into()],
            Some(&out),
            &err,
        )
        .expect("the process starts");
        wait_gone(&refused);
        let errors = std::fs::read_to_string(&err).expect("its errors are in the file");
        assert!(errors.contains("no-such-option-anywhere"), "{errors}");
        // Appended to, not started again: the first listing is still there.
        let listed_again = std::fs::read_to_string(&out).expect("still there");
        assert!(listed_again.starts_with(&listed), "{listed_again}");
    }

    #[test]
    fn a_detached_start_with_no_output_file_writes_only_its_errors() {
        let dir = tempfile::tempdir().expect("temp dir");
        let err = dir.path().join("err.log");
        let started =
            spawn(&this_binary(), &["--list".into()], None, &err).expect("the process starts");
        wait_gone(&started);
        assert_eq!(
            std::fs::read_to_string(&err).expect("the error file is created"),
            ""
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readable")
            .map(|e| e.expect("an entry").file_name())
            .collect();
        assert_eq!(names, ["err.log"]);
    }

    #[test]
    fn a_detached_start_holds_no_directory_of_the_commands() {
        let dir = neutral_dir();
        assert!(dir.is_dir(), "{}", dir.display());
        assert_eq!(dir.parent(), None, "{}", dir.display());
    }
}
