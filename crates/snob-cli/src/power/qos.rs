//! How much of the processor the browser gets: the system's normal judgment,
//! or the efficient end of it while nobody is waiting on the browser.
//!
//! **What a browser is doing when nobody waits on it.** It lingers five
//! minutes after its last request with the site's app still running in it
//! (`owner::server`), it serves the monitor in the middle of the night, it
//! answers a script with no terminal. None of that is anybody's wait, and on
//! a machine with efficiency cores, or one that can run a core slower, it is
//! the work they exist for. A command typed in a terminal, or a full-screen
//! view somebody is looking at, gets the system's normal treatment: the
//! person decides how they spend their machine's time (`owner::attended`).
//!
//! **What each system is asked**, chosen for being reversible, since the same
//! browser goes from one to the other as commands come and go:
//!
//! - **Windows**: EcoQoS (`SetProcessInformation`, `ProcessPowerThrottling`,
//!   execution speed throttled) on every process of the browser's job, and
//!   back to the system's own choice. It is what Task Manager's efficiency mode
//!   sets beside a lower priority: Windows runs such a process at the most
//!   efficient frequency and on efficiency cores, always, on battery or not.
//! - **Linux**: `SCHED_BATCH`, and a utilization clamp (`uclamp_max`) where the
//!   kernel has one, on every thread of the browser's process group; and back
//!   to `SCHED_OTHER` and no clamp. The clamp is what steers work to the
//!   little cores and the low frequencies, on the machines whose scheduler
//!   places work by energy (ARM, and recent Intel parts without
//!   hyper-threading). **Not `nice`**: an unprivileged process can raise its
//!   niceness and never lower it again, and the browser has to come back.
//! - **macOS**: nothing yet. The background policy, the one a running process
//!   can be put under, throttles disk and network too, and left a browser
//!   unable to answer in time (see `macos::in_group`).
//!
//! **Whatever the system refuses is left alone**, and logged: the browser is
//! then as it was before any of this existed.

/// What the browser runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The system's own judgment, as for any program somebody started.
    Normal,
    /// The efficient end of what the system offers.
    Eco,
}

impl Mode {
    /// What a browser runs as with `attended` commands on it: normal while
    /// anybody waits on it, efficient otherwise.
    pub fn for_attention(attended: bool) -> Self {
        if attended { Self::Normal } else { Self::Eco }
    }
}

#[cfg(windows)]
pub(crate) use windows::{in_job, own};

#[cfg(target_os = "linux")]
pub(crate) use linux::{in_group, own};

#[cfg(target_os = "macos")]
pub(crate) use macos::{in_group, own};

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicProcessIdList, QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        PROCESS_SET_INFORMATION, ProcessPowerThrottling, SetProcessInformation,
    };

    use super::Mode;

    /// More processes than a browser with one tab runs, many times over.
    const MOST_PROCESSES: usize = 128;

    /// `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for [`MOST_PROCESSES`]
    /// ids: the binding declares the list one long, as the C header does.
    #[repr(C)]
    struct Listed {
        assigned: u32,
        listed: u32,
        ids: [usize; MOST_PROCESSES],
    }

    /// Every process of the browser's job.
    pub(crate) fn in_job(job: HANDLE, mode: Mode) {
        let mut list = Listed {
            assigned: 0,
            listed: 0,
            ids: [0; MOST_PROCESSES],
        };
        // SAFETY: `job` is a job handle the caller holds, and the buffer is a
        // `JOBOBJECT_BASIC_PROCESS_ID_LIST` with the room its size says.
        let read = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                (&raw mut list).cast(),
                std::mem::size_of::<Listed>() as u32,
                std::ptr::null_mut(),
            )
        };
        if read == 0 {
            tracing::debug!(error = %std::io::Error::last_os_error(), "could not list the browser's processes");
            return;
        }
        let listed = (list.listed as usize).min(MOST_PROCESSES);
        for &pid in &list.ids[..listed] {
            let Ok(pid) = u32::try_from(pid) else {
                continue;
            };
            // SAFETY: opening a process by id for one right; a null handle is
            // checked before use.
            let process = unsafe { OpenProcess(PROCESS_SET_INFORMATION, 0, pid) };
            if process.is_null() {
                continue;
            }
            set(process, mode);
            // SAFETY: the handle was opened above and is not used after this.
            unsafe { CloseHandle(process) };
        }
    }

    /// This process.
    pub(crate) fn own(mode: Mode) {
        // SAFETY: the pseudo-handle of the current process, which needs no
        // closing.
        set(unsafe { GetCurrentProcess() }, mode);
    }

    /// EcoQoS on, or back to the system's choice: both masks clear.
    fn set(process: HANDLE, mode: Mode) {
        let speed = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: if mode == Mode::Eco { speed } else { 0 },
            StateMask: if mode == Mode::Eco { speed } else { 0 },
        };
        // SAFETY: `process` is a handle with `PROCESS_SET_INFORMATION`, and
        // the structure and its size are the ones this class takes.
        let set = unsafe {
            SetProcessInformation(
                process,
                ProcessPowerThrottling,
                (&raw const state).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if set == 0 {
            tracing::debug!(error = %std::io::Error::last_os_error(), ?mode, "the system would not set a process's power throttling");
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::Mode;

    /// The clamp on how much of the biggest core the browser's threads ask
    /// for, out of 1024, while efficient: half, which keeps them off the big
    /// cores and the top frequencies where the scheduler places work by energy,
    /// and still leaves a page room to load.
    const ECO_CLAMP: u32 = 512;
    /// No clamp: the kernel's own maximum.
    const NO_CLAMP: u32 = 1024;

    // From `include/uapi/linux/sched.h`.
    const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
    const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
    const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;

    /// `struct sched_attr`, which neither libc binds.
    #[repr(C)]
    struct SchedAttr {
        size: u32,
        sched_policy: u32,
        sched_flags: u64,
        sched_nice: i32,
        sched_priority: u32,
        sched_runtime: u64,
        sched_deadline: u64,
        sched_period: u64,
        sched_util_min: u32,
        sched_util_max: u32,
    }

    /// Set once the kernel has said it has no clamps, so it is not asked for
    /// every thread again.
    static NO_CLAMPS: AtomicBool = AtomicBool::new(false);

    /// Every thread of every process in the process group `group`: the
    /// browser and its helpers, which it starts in its own group
    /// (`pipe::spawn`).
    pub(crate) fn in_group(group: i32, mode: Mode) {
        let Ok(processes) = std::fs::read_dir("/proc") else {
            return;
        };
        for process in processes.flatten() {
            let Some(pid) = process
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            if group_of(pid) != Some(group) {
                continue;
            }
            each_thread(pid, mode);
        }
    }

    /// This process.
    pub(crate) fn own(mode: Mode) {
        each_thread(std::process::id().cast_signed(), mode);
    }

    /// The process group `pid` is in, read from `/proc/<pid>/stat`: the third
    /// field after the command name, which is in parentheses and may hold
    /// anything, so it is found from the last `)`.
    fn group_of(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat_group(&stat)
    }

    pub(super) fn stat_group(stat: &str) -> Option<i32> {
        let after = &stat[stat.rfind(')')? + 1..];
        after.split_whitespace().nth(2)?.parse().ok()
    }

    fn each_thread(pid: i32, mode: Mode) {
        let Ok(threads) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return;
        };
        for thread in threads.flatten() {
            if let Some(tid) = thread
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            {
                set(tid, mode);
            }
        }
    }

    fn set(tid: i32, mode: Mode) {
        let policy = match mode {
            Mode::Eco => libc::SCHED_BATCH,
            Mode::Normal => libc::SCHED_OTHER,
        };
        // SAFETY: all-zero is a valid `sched_param`, priority 0 included,
        // which is the only one these two policies take. Zeroed rather than
        // written out: musl's has fields glibc's does not.
        let param: libc::sched_param = unsafe { std::mem::zeroed() };
        // Through the system call rather than the C library's function, which
        // musl declines to implement. Between `SCHED_OTHER` and `SCHED_BATCH`
        // in either direction is allowed unprivileged, and leaves the nice
        // value alone.
        // SAFETY: `param` is a valid `sched_param` that outlives the call.
        let set = unsafe {
            libc::syscall(
                libc::SYS_sched_setscheduler,
                libc::c_long::from(tid),
                libc::c_long::from(policy),
                &raw const param,
            )
        };
        if set != 0 {
            tracing::debug!(tid, error = %std::io::Error::last_os_error(), "could not set a thread's policy");
        }
        if NO_CLAMPS.load(Ordering::Relaxed) {
            return;
        }
        let attr = SchedAttr {
            size: std::mem::size_of::<SchedAttr>() as u32,
            sched_policy: 0,
            sched_flags: SCHED_FLAG_KEEP_POLICY
                | SCHED_FLAG_KEEP_PARAMS
                | SCHED_FLAG_UTIL_CLAMP_MAX,
            sched_nice: 0,
            sched_priority: 0,
            sched_runtime: 0,
            sched_deadline: 0,
            sched_period: 0,
            sched_util_min: 0,
            sched_util_max: if mode == Mode::Eco {
                ECO_CLAMP
            } else {
                NO_CLAMP
            },
        };
        // SAFETY: `attr` is a `sched_attr` whose `size` says how long it is,
        // valid for the call; the last argument is the flags, which are zero.
        let clamped = unsafe {
            libc::syscall(
                libc::SYS_sched_setattr,
                libc::c_long::from(tid),
                &raw const attr,
                0 as libc::c_long,
            )
        };
        if clamped != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS | libc::E2BIG)
            ) {
                tracing::debug!(%error, "this kernel has no utilization clamps");
                NO_CLAMPS.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Mode;

    /// Left as it is, on macOS.
    ///
    /// The one policy a running process can be put under from outside is the
    /// background one (`PRIO_DARWIN_BG`, what `taskpolicy -b -p` sets), and it
    /// is not a lower gear: it throttles the process's disk and network as
    /// well as its processor. Measured on the macOS CI runner, a browser
    /// started under it stopped answering the debugging protocol within the
    /// twenty seconds a command is given, before it had loaded a page. The
    /// level that would fit, a utility clamp, can only be set as a process is
    /// spawned (`posix_spawnattr_set_qos_class_np`), which `std` does not
    /// offer; until the browser is started that way, the browser runs as any
    /// program does.
    pub(crate) fn in_group(_group: i32, _mode: Mode) {}

    /// This process: left as it is, for the same reason, and because all the
    /// owner does is pass messages.
    pub(crate) fn own(_mode: Mode) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whoever_waits_on_the_browser_gets_the_normal_treatment() {
        assert_eq!(Mode::for_attention(true), Mode::Normal);
        assert_eq!(Mode::for_attention(false), Mode::Eco);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_group_is_read_past_a_name_with_parentheses() {
        let stat = "4242 (chrome (renderer) x) S 4200 4100 4100 0 -1 4194560";
        assert_eq!(linux::stat_group(stat), Some(4100));
    }

    /// A child moved to efficient and back, as the system shows it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_group_goes_efficient_and_back() {
        use std::os::unix::process::CommandExt;

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().cast_signed();
        // The 41st field of `/proc/<pid>/stat`, counted from the state, which
        // is the third.
        let policy = || {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let after = &stat[stat.rfind(')').unwrap() + 1..];
            after.split_whitespace().nth(41 - 3).unwrap().to_string()
        };

        in_group(pid, Mode::Eco);
        assert_eq!(policy(), "3", "SCHED_BATCH");
        in_group(pid, Mode::Normal);
        assert_eq!(policy(), "0", "SCHED_OTHER");

        child.kill().unwrap();
        child.wait().unwrap();
    }
}
