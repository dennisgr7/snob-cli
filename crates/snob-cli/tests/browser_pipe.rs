//! The login browser, against a real browser.
//!
//! Everything else about `cdp` and `pipe` is tested without one — the framing,
//! the reply matching, the inheritance blob, the argument quoting. What cannot
//! be tested that way is what matters most about the transport: that Chromium
//! finds the protocol where we put it, and that no loopback port exists.
//!
//! **Skipped when no browser is installed**, rather than failed. A machine with
//! no Chromium is a machine where `snob login --browser` is not an option
//! anyway, and a test that cannot run there should say nothing rather than
//! something false. Where one must start — CI on Windows and macOS sets
//! `SNOB_TEST_REQUIRE_BROWSER` — a browser that will not fails the test.
//!
//! Nothing here logs into anything. The browser is pointed at its own empty
//! profile under a temporary directory, asked one question about itself, and
//! closed.

use std::path::PathBuf;

// Only the job-object test below uses this, and that test is Windows-only, so
// on every other platform the import is dead and `-D warnings` says so.
#[cfg(windows)]
use std::time::Duration;

use snob_cli::{browser, cdp};
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;

/// Says why a test that needs a browser does not run, or fails it where one
/// is required.
fn skip(why: &str) {
    let required = snob_cli::headless::env_flag("SNOB_TEST_REQUIRE_BROWSER");
    assert!(!required, "{why}, and SNOB_TEST_REQUIRE_BROWSER needs one");
    eprintln!("{why}; skipping");
}

/// A browser on an empty profile under a temporary directory, in a window on
/// the login page or headless, with the directory and the profile, which
/// have to outlive it. `None`, said, when there is none to start.
///
/// **A browser that will not start is skipped, not failed.** The Ubuntu CI
/// runner has Chrome installed and cannot run it: no display, and no user
/// namespaces for its sandbox, so it exits 1 before reading the pipe. Making
/// that green would mean passing `--no-sandbox` from `cdp::launch`, which is
/// production code and would hand every real user a browser with its sandbox
/// off to satisfy a test. The Windows and macOS runners start a browser and
/// run every assertion.
async fn started(headless: bool) -> Option<(tempfile::TempDir, PathBuf, cdp::Cdp)> {
    let Some(found) = browser::detect() else {
        skip("no browser installed");
        return None;
    };

    let temporary = tempfile::tempdir().expect("a temporary directory");
    let profile = AppPaths::rooted_at(temporary.path()).browser_profile_for(snob_core::Pk::new(1));
    let launched = if headless {
        cdp::launch_headless(&found, &profile, &["--headless=new".to_string()])
    } else {
        cdp::launch(&found, &profile)
    };
    let started = match launched {
        Ok(launched) => cdp::Cdp::connect(launched, &CancelToken::default()).await,
        Err(e) => Err(e),
    };
    match started {
        Ok(cdp) => Some((temporary, profile, cdp)),
        Err(e) => {
            skip(&format!("the browser found here will not start ({e})"));
            None
        }
    }
}

/// The one thing that has to be true of the transport: a browser we started
/// speaks the protocol over the pipe, and there is no port anywhere. Why a
/// port is the danger is told in `pipe.rs`.
#[tokio::test]
async fn the_browser_answers_on_the_pipe_and_opens_no_port() {
    let Some((_temporary, profile, cdp)) = started(false).await else {
        return;
    };

    let user_agent = cdp.user_agent().await.expect("it reports its User-Agent");
    assert!(
        user_agent.contains("Mozilla/5.0"),
        "that is not a User-Agent: {user_agent}"
    );

    // The file a browser listening on a port writes, and the one anything
    // looking for the port would read. `launch` removes a stale copy before
    // starting, so its absence here means this browser did not write one.
    let active_port = profile.join("DevToolsActivePort");
    assert!(
        !active_port.exists(),
        "the browser wrote {} , so it is listening on a port after all",
        active_port.display()
    );

    // And the port itself, asked of the operating system rather than inferred
    // from the absence of a file. The browser process is the one that would
    // listen; its renderers do not.
    #[cfg(windows)]
    assert_eq!(
        listening_ports(cdp.browser_pid()),
        Vec::<String>::new(),
        "the browser is listening on a socket, which the pipe is there to prevent"
    );

    cdp.close().await;
}

/// Closing the browser waits for every process it started, not only its own:
/// a helper left running holds the profile, and a browser started next on it
/// can stall.
#[cfg(unix)]
#[tokio::test]
async fn closing_the_browser_leaves_none_of_its_processes() {
    let Some((_temporary, _, cdp)) = started(true).await else {
        return;
    };

    let info = cdp
        .browser_call("SystemInfo.getProcessInfo", serde_json::json!({}))
        .await
        .expect("it lists its processes");
    let pids: Vec<libc::pid_t> = info["processInfo"]
        .as_array()
        .expect("a list of processes")
        .iter()
        .filter_map(|process| process["id"].as_i64())
        .filter_map(|id| libc::pid_t::try_from(id).ok())
        .collect();
    assert!(pids.len() > 1, "a browser runs helpers: {info}");

    cdp.close().await;
    // SAFETY: signal 0 sends nothing; it asks whether the process is there.
    let running = |pid: libc::pid_t| unsafe { libc::kill(pid, 0) } == 0;
    let left: Vec<_> = pids.into_iter().filter(|pid| running(*pid)).collect();
    assert!(left.is_empty(), "still running once closed: {left:?}");
}

/// Every TCP socket this pid is listening on.
///
/// Read from `netstat` rather than from a crate, because what is being checked
/// is what an attacker would see: the operating system's own list of what can
/// be connected to.
#[cfg(windows)]
fn listening_ports(pid: u32) -> Vec<String> {
    let mut netstat = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
    );
    netstat.push(r"System32\netstat.exe");
    let output = std::process::Command::new(netstat)
        .args(["-ano", "-p", "TCP"])
        .output()
        .expect("netstat runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("LISTENING"))
        .filter(|line| line.split_whitespace().last() == Some(&pid.to_string()))
        .map(|line| line.split_whitespace().nth(1).unwrap_or("?").to_string())
        .collect()
}

/// Snob killed from outside takes the browser with it.
///
/// This is the case no handler of ours can catch — Task Manager, `taskkill /F`,
/// a service manager's stop timeout — and it is what the job object is for.
/// Dropping the `Cdp`, and with it the process handle, is the same kernel
/// event that a killed snob produces: the last handle to the job closes, and
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` empties it. Doing it by drop is what
/// makes the test deterministic rather than a race against a second process.
#[cfg(windows)]
#[tokio::test]
async fn losing_the_job_handle_takes_the_browser_down() {
    let Some(found) = browser::detect() else {
        return skip("no browser installed");
    };

    let temporary = tempfile::tempdir().expect("a temporary directory");
    let profile = temporary.path().join("profile");
    let flags = ["--headless=new".to_string()];
    let launched = cdp::launch_headless(&found, &profile, &flags).expect("the browser starts");
    let cdp = cdp::Cdp::connect(launched, &CancelToken::default())
        .await
        .expect("it answers on the pipe");
    let pid = cdp.browser_pid();
    assert!(pid != 0);
    assert!(is_running(pid), "it should be up before we let go of it");

    // Held so the pipe stays open: the browser closing on its pipe would pass
    // this test without the job doing anything.
    let _pipe = cdp.connection().clone();
    // No kill, no close, no polite request: just letting go, which is what
    // being killed from outside amounts to.
    drop(cdp);

    for _ in 0..100 {
        if !is_running(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the browser outlived the job it was in");
}

/// Whether a process id still names a live process.
///
/// Asked through the job's own effect rather than by opening the process,
/// because a killed process keeps its id until every handle to it is closed and
/// `OpenProcess` would still succeed on that zombie. `tasklist` reads the live
/// list.
#[cfg(windows)]
fn is_running(pid: u32) -> bool {
    let mut tasklist = std::path::PathBuf::from(
        std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
    );
    tasklist.push(r"System32\tasklist.exe");
    let output = std::process::Command::new(tasklist)
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist runs");
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}
