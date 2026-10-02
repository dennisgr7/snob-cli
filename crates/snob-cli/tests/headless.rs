//! The headless transport, driven end to end: the real binary, a real browser,
//! and a fake Instagram served locally.
//!
//! Every request to Instagram leaves from a browser tab, and nothing else
//! in the suite sends one that way — `sandbox.rs` reaches its mock servers with
//! `reqwest`, on purpose, so it runs on a machine with no browser. This is the
//! one place the path users actually take is exercised: the tab is opened, the
//! pasted session is written into the browser, the page load sets a CSRF
//! cookie, and the API calls go out with the browser's own cookie jar and
//! headers. `--through-the-browser` is what points that path at a local
//! server; like `--ig-base-url` it exists only in a testing build.
//!
//! **Skipped when no browser will start**, for the reason `browser_pipe.rs`
//! gives: a machine where Chromium cannot run is a machine where snob cannot
//! either, and a test that cannot run there should say nothing rather than
//! something false. It asks the browser directly, once per run of this file,
//! so a browser that starts and then misbehaves under snob is a failure, not a
//! skip. Where a browser must start — CI on Windows and macOS sets
//! `SNOB_TEST_REQUIRE_BROWSER` — one that will not fails every test instead of
//! turning them into green no-ops.
#![cfg(feature = "testing")]

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use snob_cli::{browser, cdp};
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;
use wiremock::matchers::{method, path as url_path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

mod common;

use common::ig::{self, World, header, is_the_apps};

const SESSIONID: &str = "42%3Aheadless%3A17";

/// What the fake Instagram sets on the page load. A pasted session carries no
/// CSRF token at all, so seeing this one on an API call means the call went
/// out with the browser's own cookie jar — `reqwest` never learns it.
const SERVED_CSRF: &str = ig::CSRF;

/// Whether a browser will start headless on this machine at all.
///
/// Asked once for the whole file, on a runtime of its own: every test starting
/// a probe browser beside its own doubled the browsers starting at once.
fn a_browser_starts() -> bool {
    static STARTS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let answer = STARTS.get_or_init(|| {
        std::thread::spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the probe")
                .block_on(probe())
        })
        .join()
        .expect("the probe finishes")
    });
    match answer {
        Ok(()) => true,
        Err(why) => {
            let required = snob_cli::headless::env_flag("SNOB_TEST_REQUIRE_BROWSER");
            assert!(!required, "{why}, and SNOB_TEST_REQUIRE_BROWSER needs one");
            eprintln!("{why}; skipping");
            false
        }
    }
}

/// The browser to this test alone, while the guard lives, or `None` where no
/// browser starts.
///
/// The tests run one at a time: several browsers starting and closing at once
/// on a loaded machine hold their profiles past the patience the tests give
/// them.
async fn a_browser_alone() -> Option<tokio::sync::MutexGuard<'static, ()>> {
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let alone = ONE_AT_A_TIME.lock().await;
    a_browser_starts().then_some(alone)
}

async fn probe() -> Result<(), String> {
    let found = browser::detect().ok_or("no browser installed")?;
    let temporary = tempfile::tempdir().expect("a temporary directory");
    let paths = AppPaths::rooted_at(temporary.path());
    let cancel = CancelToken::default();
    let flags = ["--headless=new".to_string()];
    let started = match cdp::launch_headless(&found, &paths.browser_profile(), &flags) {
        Ok(launched) => cdp::Cdp::connect(launched, &cancel).await,
        Err(e) => Err(e),
    };
    match started {
        Ok(cdp) => {
            cdp.close().await;
            Ok(())
        }
        Err(e) => Err(format!("the browser found here will not start ({e})")),
    }
}

/// The fake Instagram, and the world it serves.
async fn fake_instagram() -> (World, MockServer) {
    let world = World::new();
    let server = world.serve_web().await;
    (world, server)
}

fn snob(root: &Path, instagram: &MockServer, args: &[&str], typed: Option<&str>) -> Output {
    snob_with(root, instagram, args, typed, &[])
}

/// [`snob`], with `envs` set for it. Neither switch that turns part of the
/// engine off is inherited from the shell the tests run in.
fn snob_with(
    root: &Path,
    instagram: &MockServer,
    args: &[&str],
    typed: Option<&str>,
    envs: &[(&str, &str)],
) -> Output {
    snob_started(root, instagram, args, typed, envs)
        .wait_with_output()
        .expect("the binary finishes")
}

/// [`snob_with`], left running.
fn snob_started(
    root: &Path,
    instagram: &MockServer,
    args: &[&str],
    typed: Option<&str>,
    envs: &[(&str, &str)],
) -> std::process::Child {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_snob"))
        .arg("--sandbox-root")
        .arg(root)
        .arg("--ig-base-url")
        .arg(instagram.uri())
        .arg("--through-the-browser")
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("SNOB_NO_BROWSER")
        .env_remove("SNOB_NO_OWNER")
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    if let Some(typed) = typed {
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(typed.as_bytes())
            .expect("the binary reads what it is given");
    }
    drop(child.stdin.take());
    child
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// What the command said, and what the owner of the browsers in the sandbox
/// at `root` logged, for a failure message: a browser's trouble is told in
/// the owner's log, not by the command.
fn told(root: &Path, output: &Output) -> String {
    let log = AppPaths::rooted_at(root)
        .data_dir()
        .join("browser-owner.log");
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    format!("{}\n--- {} ---\n{logged}", said(output), log.display())
}

/// The whole promise of the transport, asked of what the server received.
#[tokio::test]
async fn every_request_leaves_from_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;

    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let out = snob(
        tmp.path(),
        &instagram,
        &["unfollowers", "--format", "json"],
        None,
    );
    assert!(
        out.status.success(),
        "the crossing failed: {}",
        told(tmp.path(), &out)
    );

    let received = snobs_requests(&instagram).await;
    let first_api = received
        .iter()
        .position(is_an_api_call)
        .expect("the API was called");
    assert!(
        received[..first_api].iter().any(|r| r.url.path() == "/"),
        "the tab opens the site before it asks the API anything"
    );

    for request in received.iter().filter(|r| is_an_api_call(r)) {
        let what = request.url.to_string();
        let path = request.url.path();
        let agent = header(request, "user-agent").unwrap_or_default();
        assert!(!agent.is_empty(), "{what}: no User-Agent");
        assert!(!agent.contains("Headless"), "{what}: {agent}");
        let brands = header(request, "sec-ch-ua").unwrap_or_default();
        assert!(!brands.contains("Headless"), "{what}: {brands}");

        // Only the browser's own jar holds the token the page load set. The
        // REST and Relay families carry it, and a route call does not.
        let csrf = (!path.starts_with("/ajax/")).then_some(SERVED_CSRF);
        assert_eq!(
            header(request, "x-csrftoken"),
            csrf,
            "{what}: the CSRF token is not the browser's"
        );
        // The claim is a REST header.
        assert_eq!(
            header(request, "x-ig-www-claim").is_some(),
            path.starts_with("/api/v1/"),
            "{what}: the claim"
        );
        let cookie = header(request, "cookie").unwrap_or_default();
        assert!(cookie.contains("sessionid="), "{what}: no session cookie");
        assert!(
            cookie.contains(&format!("csrftoken={SERVED_CSRF}")),
            "{what}: the cookie jar is not the browser's: {cookie}"
        );

        // Set by the browser's network stack on a fetch, and by nothing else
        // snob sends from here.
        assert_eq!(header(request, "sec-fetch-mode"), Some("cors"), "{what}");
        assert_eq!(
            header(request, "sec-fetch-site"),
            Some("same-origin"),
            "{what}"
        );
    }

    // The claim the server handed out comes back on the calls after it.
    let claimed = received
        .iter()
        .filter(|r| r.url.path().starts_with("/api/v1/"))
        .skip_while(|r| header(r, "x-ig-www-claim") != Some(ig::CLAIM))
        .count();
    assert!(claimed > 0, "the claim the server set was never sent back");
}

/// Whether `request` asked Instagram's API: REST, Relay or a route call.
fn is_an_api_call(request: &Request) -> bool {
    let path = request.url.path();
    path.starts_with("/api/") || path == "/graphql/query" || path.starts_with("/ajax/")
}

/// Everything the server was sent but the app's own calls, in order: what
/// snob sent, and the documents and pictures the browser loaded.
async fn snobs_requests(instagram: &MockServer) -> Vec<Request> {
    instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !is_the_apps(r))
        .collect()
}

/// Logs in by pasting `sessionid`, and checks it worked; `label` starts what
/// a failure says.
fn logged_in(root: &Path, instagram: &MockServer, sessionid: &str, label: &str) -> Output {
    let out = snob(
        root,
        instagram,
        &["login", "--paste"],
        Some(&format!("{sessionid}\n")),
    );
    assert!(out.status.success(), "{label}{}", told(root, &out));
    out
}

/// How many requests the server has been sent so far, the app's aside.
async fn requests_so_far(instagram: &MockServer) -> usize {
    snobs_requests(instagram).await.len()
}

/// The paths of the API calls the server was sent since `since`, the app's
/// aside.
async fn api_paths_since(instagram: &MockServer, since: usize) -> Vec<String> {
    snobs_requests(instagram).await[since..]
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| p.starts_with("/api/"))
        .collect()
}

/// The session cookie the last document load carried: a session is checked
/// from the document the browser loads with it.
async fn last_session_seen(instagram: &MockServer) -> String {
    let received = snobs_requests(instagram).await;
    let last = received
        .iter()
        .rev()
        .find(|r| r.url.path() == "/")
        .expect("the site was loaded");
    header(last, "cookie").unwrap_or_default().to_string()
}

/// A User-Agent given at login is not the browser's to send: every request
/// the tab makes, documents and API calls alike, carries the browser's own,
/// and the pinned one is left to the requests sent without a browser.
#[tokio::test]
async fn a_pinned_user_agent_is_not_what_the_browser_sends() {
    const PINNED: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/99.0.0.0 Safari/537.36 Pinned/1";
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;
    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste", "--user-agent", PINNED],
        Some(&format!("{SESSIONID}\n")),
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    let out = snob(tmp.path(), &instagram, &["profile", "someone"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));

    let received = snobs_requests(&instagram).await;
    assert!(received.iter().any(is_an_api_call), "the API was asked");
    for request in &received {
        let what = request.url.to_string();
        let agent = header(request, "user-agent").unwrap_or_default();
        assert!(agent.contains("Chrome/"), "{what}: {agent}");
        assert!(
            !agent.contains("Pinned") && !agent.contains("Chrome/99."),
            "{what}: the pinned User-Agent was sent: {agent}"
        );
        assert!(!agent.contains("Headless"), "{what}: {agent}");
    }
}

/// Logging in and asking who the session is send nothing to the API: both
/// read the account off the document the browser loads.
#[tokio::test]
async fn a_login_and_whoami_ask_the_api_nothing() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");
    let out = snob(tmp.path(), &instagram, &["whoami", "--json"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(said(&out).contains(r#""username": "me""#), "{}", said(&out));
    assert_eq!(api_paths_since(&instagram, 0).await, Vec::<String>::new());
    let found = world
        .audit(
            &instagram,
            &["/api/v1/users/*/info/", "/api/v1/friendships/*/following/"],
        )
        .await;
    assert!(found.is_empty(), "{found:?}");
}

/// `whoami` stores the name the account goes by now, read off the document,
/// when it was renamed since the session stored one.
#[tokio::test]
async fn whoami_stores_a_renamed_accounts_new_name() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");
    world.person(ig::ME, |me| me.username = "me.renamed".into());
    let out = snob(tmp.path(), &instagram, &["whoami", "--json"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(
        said(&out).contains(r#""username": "me.renamed""#),
        "{}",
        said(&out)
    );
    let out = snob(
        tmp.path(),
        &instagram,
        &["whoami", "--offline", "--json"],
        None,
    );
    assert!(
        said(&out).contains(r#""username": "me.renamed""#),
        "the new name was not stored: {}",
        said(&out)
    );
}

/// Instagram pushing back on the document a login loads leaves the session
/// stored unconfirmed, with the one cooldown the browser heard written down,
/// and the next command refuses for it.
#[tokio::test]
async fn a_push_back_on_the_logins_document_is_recorded_once() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(refused())
        .with_priority(1)
        .mount(&instagram)
        .await;

    let out = logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");
    assert!(
        said(&out).contains("could not be confirmed"),
        "{}",
        told(tmp.path(), &out)
    );
    assert_one_rate_limit_was_recorded(tmp.path());

    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(
        said(&out).contains("so the session was not checked"),
        "{}",
        told(tmp.path(), &out)
    );
    assert_one_rate_limit_was_recorded(tmp.path());
}

/// The cookies the last request the browser made carried, the app's own
/// included: those after a document load carry what its answer set.
async fn cookies_after_the_load(instagram: &MockServer) -> String {
    let received = instagram.received_requests().await.unwrap_or_default();
    let last = received.last().expect("the site was loaded");
    header(last, "cookie").unwrap_or_default().to_string()
}

/// A login is authoritative: a fresh paste for the same account reaches the
/// browser, which already carries the old session and has to replace it.
#[tokio::test]
async fn a_new_login_for_the_same_account_reaches_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;

    for token in ["first", "second"] {
        let sessionid = format!("42%3A{token}%3A17");
        logged_in(tmp.path(), &instagram, &sessionid, &format!("{token}: "));
        let cookie = last_session_seen(&instagram).await;
        assert!(
            cookie.contains(&format!("sessionid={sessionid}")),
            "{token}: the browser sent {cookie}"
        );
    }
}

/// A login Instagram refuses leaves nothing behind that keeps the stored
/// session out: the next command sends the stored one, although the refused
/// one was made later and went into the account's browser for its check.
#[tokio::test]
async fn a_refused_login_leaves_the_stored_session_in_charge() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    const REFUSED: &str = "42%3Arefused%3A17";
    // Served a logged-out document.
    world.refuse(REFUSED);

    logged_in(tmp.path(), &instagram, SESSIONID, "");
    let out = snob(
        tmp.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{REFUSED}\n")),
    );
    assert!(!out.status.success(), "the refused login was stored");
    assert!(
        last_session_seen(&instagram).await.contains(REFUSED),
        "the check went out with the pasted session"
    );

    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    let cookie = last_session_seen(&instagram).await;
    assert!(
        cookie.contains(&format!("sessionid={SESSIONID}")),
        "the browser sent {cookie}"
    );
}

/// A profile holding another account's cookies is emptied before this one's
/// session goes in, and that is said: the session and the device cookie the
/// first account's page load set do not travel with the second account's
/// requests. With a profile per account this is a fallback, reached here by
/// moving the first account's profile to where the second's goes.
#[tokio::test]
async fn a_profile_holding_another_account_is_emptied_first() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;

    logged_in(tmp.path(), &instagram, "42%3Afirst%3A17", "");
    assert!(
        cookies_after_the_load(&instagram)
            .await
            .contains("datr=device-of-the-first-account"),
        "the first account's page load set the device cookie"
    );

    let paths = AppPaths::rooted_at(tmp.path());
    owner_gone(&paths).await;
    snob_store::paths::rename_patiently(
        &paths.browser_profile_for(snob_core::Pk::new(42)),
        &paths.browser_profile_for(snob_core::Pk::new(43)),
    )
    .unwrap();

    instagram.reset().await;
    let (_, second) = fake_instagram().await;
    let out = logged_in(tmp.path(), &second, "43%3Asecond%3A17", "");

    // The page load on the second server sets a datr of its own; what must not
    // happen is the first one's arriving at the API before that.
    let received = second.received_requests().await.unwrap_or_default();
    let first_load = received
        .iter()
        .find(|r| r.url.path() == "/")
        .expect("the tab opened the site");
    let carried = header(first_load, "cookie").unwrap_or_default();
    assert!(
        carried.contains("sessionid=43%3Asecond%3A17"),
        "the second account's session went in: {carried}"
    );
    assert!(
        !carried.contains("42%3A") && !carried.contains("device-of-the-first-account"),
        "the first account's cookies went with the second: {carried}"
    );
    let log =
        std::fs::read_to_string(paths.data_dir().join("browser-owner.log")).unwrap_or_default();
    assert!(
        format!("{}{log}", said(&out)).contains("held another account's cookies"),
        "the fallback was not said: {log}"
    );
}

/// `snob logout` deletes the profile of the account in use, and `--all` every
/// account's, with the owner still up and one of them just closed.
#[tokio::test]
async fn logout_deletes_the_accounts_profile_or_every_one() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;
    for sessionid in ["42%3Afirst%3A17", "43%3Asecond%3A17"] {
        logged_in(tmp.path(), &instagram, sessionid, &format!("{sessionid}: "));
    }
    let paths = AppPaths::rooted_at(tmp.path());
    for pk in [42, 43] {
        assert!(paths.browser_profile_for(snob_core::Pk::new(pk)).is_dir());
    }

    // The account in use, the one signed in last, and nobody else's.
    let out = snob(tmp.path(), &instagram, &["logout"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(!paths.browser_profile_for(snob_core::Pk::new(43)).exists());
    assert!(paths.browser_profile_for(snob_core::Pk::new(42)).is_dir());

    let out = snob(tmp.path(), &instagram, &["logout", "--all"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(
        !paths.browser_profile().exists(),
        "a profile was left behind: {}",
        told(tmp.path(), &out)
    );
}

/// What the page and its service worker give away, asked of them from inside.
///
/// The fake site's own page reports what a script can read about the browser
/// it runs in, and registers a service worker that reports its User-Agent.
/// Each reads wrong on a headless Chromium the engine does not cover: no
/// focus, a screen with no taskbar, no mouse, and a service worker calling
/// itself `HeadlessChrome` — a target of its own that the tab's override does
/// not reach.
#[tokio::test]
async fn the_page_and_its_worker_look_like_a_desktop_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    // Mounted first, so it wins over the plain page.
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(world.page(
            "navigator.serviceWorker.register('/sw.js');
            const probe = (form) => fetch('/page-probe?focus=' + document.hasFocus()
              + '&avail=' + (screen.availHeight < screen.height)
              + '&pointer=' + matchMedia('(pointer: fine)').matches
              + '&hover=' + matchMedia('(hover: hover)').matches
              + '&webdriver=' + navigator.webdriver
              + '&form=' + form);
            navigator.userAgentData.getHighEntropyValues(['formFactors'])
              .then(v => probe((v.formFactors || []).join(',')), () => probe('refused'));",
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/sw.js"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "self.addEventListener('install', e => e.waitUntil(fetch('/sw-probe')));",
            "text/javascript",
        ))
        .mount(&instagram)
        .await;
    for probe in ["/page-probe", "/sw-probe"] {
        Mock::given(method("GET"))
            .and(url_path(probe))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&instagram)
            .await;
    }

    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");
    // A second run, so the worker registered by the first has certainly been
    // installed and has asked for what it asks for.
    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));

    let received = instagram.received_requests().await.unwrap_or_default();
    let paths: Vec<String> = received.iter().map(|r| r.url.path().to_string()).collect();
    let page = received
        .iter()
        .find(|r| r.url.path() == "/page-probe")
        .unwrap_or_else(|| panic!("the page ran its script: {paths:?}"));
    let said_by_page: std::collections::HashMap<_, _> = page.url.query_pairs().collect();
    for (what, expected) in [
        ("focus", "true"),
        ("avail", "true"),
        ("pointer", "true"),
        ("hover", "true"),
        ("webdriver", "false"),
        ("form", "Desktop"),
    ] {
        assert_eq!(
            said_by_page.get(what).map(|v| v.as_ref()),
            Some(expected),
            "{what}: {}",
            page.url
        );
    }

    let worker = received
        .iter()
        .find(|r| r.url.path() == "/sw-probe")
        .expect("the service worker was installed and asked");
    let agent = header(worker, "user-agent").unwrap_or_default();
    assert!(
        !agent.is_empty() && !agent.contains("Headless"),
        "the service worker names itself: {agent}"
    );
}

/// No video the site loads reaches the page, so none plays.
///
/// The feed plays a video that scrolls into view, muted, on its own, and a
/// play of a reel counts from its start: every run would add plays nobody
/// watched to other people's videos. Both ways a page loads one are tried
/// here — a `<video>` element, and a piece fetched by script the way the
/// site's player does — and the server must never be asked for either.
#[tokio::test]
async fn no_video_the_site_loads_reaches_the_page() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(world.page(
            "const video = document.createElement('video');
            video.muted = true; video.autoplay = true; video.playsInline = true;
            video.src = '/clip.mp4';
            document.documentElement.appendChild(video);
            fetch('/v/t16/piece.mp4?bytestart=0&byteend=999').catch(() => {});
            fetch('/clips.mp4/').catch(() => {});
            fetch('/page-probe').catch(() => {});",
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;
    for path in [
        "/clip.mp4",
        "/v/t16/piece.mp4",
        "/clips.mp4/",
        "/page-probe",
    ] {
        Mock::given(method("GET"))
            .and(url_path(path))
            .respond_with(ResponseTemplate::new(200).set_body_string("x"))
            .mount(&instagram)
            .await;
    }

    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let paths: Vec<String> = instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p == "/page-probe"),
        "the page ran its script, so the check below means something: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with(".mp4")),
        "a video reached the page: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p == "/clips.mp4/"),
        "a profile named like a video was refused with the videos: {paths:?}"
    );
}

/// A worker the page starts while snob is between requests runs at once.
///
/// A target the browser pauses as it attaches waits for somebody to read the
/// protocol, and a worker that waits for snob's next command starts late,
/// which is itself a timing tell. The page here starts one during the pause
/// after the site loads, when snob has nothing in flight, and reports how long
/// it took to begin.
#[tokio::test]
async fn a_worker_created_while_snob_is_idle_starts_at_once() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(world.page(
            "setTimeout(() => {
              const made = Date.now();
              const code = new Blob(['postMessage(Date.now())'],
                { type: 'text/javascript' });
              const worker = new Worker(URL.createObjectURL(code));
              worker.onmessage = (e) => fetch('/worker-probe?ms=' + (e.data - made));
            }, 50);",
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/worker-probe"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&instagram)
        .await;

    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let received = instagram.received_requests().await.unwrap_or_default();
    let paths: Vec<String> = received.iter().map(|r| r.url.to_string()).collect();
    let probe = received
        .iter()
        .find(|r| r.url.path() == "/worker-probe")
        .unwrap_or_else(|| panic!("the worker ran and reported: {paths:?}"));
    let late: u64 = probe
        .url
        .query_pairs()
        .find(|(k, _)| k == "ms")
        .and_then(|(_, v)| v.parse().ok())
        .expect("the delay is a number of milliseconds");
    eprintln!("the worker started {late} ms after it was created");
    // Held until the next command, the worker would wait out the rest of the
    // page's one-and-a-half-second settling pause, well over a second. The
    // dispatcher lets it go in about ten milliseconds; a macOS runner with
    // every test's browser starting at once took 581, so the line sits
    // between the two.
    assert!(
        late < 1_000,
        "the worker started {late} ms after it was created"
    );
}

/// The fake site's page, handing a device cookie of its own to every browser
/// that does not have one yet — the way Meta's `datr` is set on a first visit.
struct HandsOutDevices(World, std::sync::atomic::AtomicUsize);

impl HandsOutDevices {
    fn of(world: &World) -> Self {
        Self(world.clone(), std::sync::atomic::AtomicUsize::new(0))
    }
}

impl wiremock::Respond for HandsOutDevices {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let answer = self.0.document_answer(request, "", false);
        if header(request, "cookie").is_some_and(|c| c.contains("datr=")) {
            return answer;
        }
        let n = self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        // Kept for a year, as Meta's is: a cookie with no lifetime ends with
        // the browser, and a device that does not outlive the run is not one.
        answer.append_header(
            "Set-Cookie",
            format!("datr=device-{n}; Path=/; Max-Age=31536000"),
        )
    }
}

/// The cookies the first page load of a run carried.
async fn first_page_load_carried(instagram: &MockServer, since: usize) -> String {
    let received = snobs_requests(instagram).await;
    let load = received[since..]
        .iter()
        .find(|r| r.url.path() == "/")
        .expect("the tab opened the site");
    header(load, "cookie").unwrap_or_default().to_string()
}

/// Every account keeps its own device: the browser another account used in
/// between is not this one's, and coming back finds the device it left.
///
/// One profile emptied whenever the account changed would keep two accounts
/// from looking like one person's browser, and throw the device away each
/// time: coming back to the first account would come from a browser Instagram
/// had never seen.
#[tokio::test]
async fn each_account_keeps_a_device_of_its_own() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(HandsOutDevices::of(&world))
        .with_priority(1)
        .mount(&instagram)
        .await;

    let mut seen = 0;
    for (sessionid, carried) in [
        ("42%3Afirst%3A17", None),
        ("43%3Asecond%3A17", None),
        ("42%3Afirst%3A17", Some("datr=device-1")),
    ] {
        logged_in(tmp.path(), &instagram, sessionid, &format!("{sessionid}: "));
        let cookies = first_page_load_carried(&instagram, seen).await;
        match carried {
            Some(device) => assert!(
                cookies.contains(device),
                "{sessionid} came back from another device: {cookies}"
            ),
            None => assert!(
                !cookies.contains("datr="),
                "{sessionid} began with somebody's device: {cookies}"
            ),
        }
        seen = requests_so_far(&instagram).await;
    }

    let paths = AppPaths::rooted_at(tmp.path());
    for pk in [42, 43] {
        assert!(
            paths.browser_profile_for(snob_core::Pk::new(pk)).is_dir(),
            "account {pk} has a profile of its own"
        );
    }
}

/// A profile from before they were kept per account moves under the account
/// it holds, and the browser still finds everything in it: the device it was
/// given comes along.
#[tokio::test]
async fn a_profile_from_before_moves_under_its_account() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(HandsOutDevices::of(&world))
        .with_priority(1)
        .mount(&instagram)
        .await;

    logged_in(tmp.path(), &instagram, SESSIONID, "");

    // Laid out the way an older snob leaves it: the profile is the directory
    // itself.
    let paths = AppPaths::rooted_at(tmp.path());
    let root = paths.browser_profile();
    let account = paths.browser_profile_for(snob_core::Pk::new(42));
    let aside = tmp.path().join("old-layout");
    // Patiently, as snob itself moves a profile: on Windows a browser that
    // has just closed takes a moment to let go of its files.
    snob_store::paths::rename_patiently(&account, &aside).unwrap();
    snob_store::paths::remove_tree_patiently(&root).unwrap();
    snob_store::paths::rename_patiently(&aside, &root).unwrap();
    assert!(root.join("snob-profile.json").is_file());

    let seen = requests_so_far(&instagram).await;
    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(account.join("snob-profile.json").is_file());
    assert!(!root.join("snob-profile.json").exists());
    let cookies = first_page_load_carried(&instagram, seen).await;
    assert!(
        cookies.contains("datr=device-1"),
        "the moved profile kept its device: {cookies}"
    );
}

/// The fake site's page, handing the browser a rotated session.
struct RotatesTheSession(World);

impl wiremock::Respond for RotatesTheSession {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.document_answer(request, "", true).append_header(
            "Set-Cookie",
            "sessionid=42%3Arotated%3A17; Path=/; Max-Age=31536000; HttpOnly",
        )
    }
}

/// What the browser kept current outlives the profile it kept it in.
///
/// Instagram rotates the session through the answers it sends, and the
/// browser keeps what it is sent. A stored copy that learned none of it would
/// bring back the session as it was at the login once the profile is lost —
/// deleted, damaged, restored from an older backup. The site here hands the
/// browser a rotated session on its first page load; after the profile is
/// gone, the next run can only have the rotated one from the store.
#[tokio::test]
async fn a_session_the_browser_rotated_survives_losing_the_profile() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(RotatesTheSession(world.clone()))
        .with_priority(1)
        .mount(&instagram)
        .await;

    logged_in(tmp.path(), &instagram, SESSIONID, "");
    assert!(
        cookies_after_the_load(&instagram)
            .await
            .contains("sessionid=42%3Arotated%3A17"),
        "the browser took the rotated session"
    );

    let profile = AppPaths::rooted_at(tmp.path()).browser_profile_for(snob_core::Pk::new(42));
    snob_store::paths::remove_tree_patiently(&profile).unwrap();

    let seen = requests_so_far(&instagram).await;
    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    let carried = first_page_load_carried(&instagram, seen).await;
    assert!(
        carried.contains("sessionid=42%3Arotated%3A17"),
        "a fresh profile was handed the session as it was at the login: {carried}"
    );
}

/// The site's page, running a script of the app's.
fn page_running(world: &World, script: &str) -> ig::WorldPage {
    world.page(script)
}

/// Instagram's 429.
fn refused() -> ResponseTemplate {
    ResponseTemplate::new(429).set_body_raw(
        r#"{"message":"Please wait a few minutes"}"#,
        "application/json",
    )
}

/// How long the cooldown the sandbox's store holds has left to run.
fn cooldown_left(root: &Path) -> Option<std::time::Duration> {
    use snob_core::budget::RateBudget;
    let paths = AppPaths::rooted_at(root).account(snob_core::Pk::new(42));
    let budget = snob_store::store::rate_budget::SqliteRateBudget::open(&paths).ok()?;
    let until = budget.cooldown().ok()??;
    let now = snob_core::clock::now_ms();
    Some(std::time::Duration::from_millis(
        u64::try_from(until.get() - now.get()).unwrap_or(0),
    ))
}

/// Recorded once: a cooldown written twice within a day doubles, so a push-back
/// recorded twice would leave four hours where two are owed.
fn assert_one_rate_limit_was_recorded(root: &Path) {
    let owed = snob_core::budget::RATE_LIMIT_COOLDOWN;
    let left = cooldown_left(root).expect("the push-back was written down");
    let slack = std::time::Duration::from_secs(120);
    assert!(
        left <= owed + slack && left + slack >= owed,
        "a cooldown of {left:?} where one of {owed:?} was owed"
    );
}

/// Instagram pushing back on one of its own app's calls stops the run before
/// snob sends anything more, and is written down once.
///
/// The app runs on the tab snob sends from, and Instagram talks to the session
/// through every one of its calls. A 429 there that nobody read would let the
/// run go on asking until the next answer to snob stopped it.
#[tokio::test]
async fn a_push_back_on_the_apps_own_call_stops_the_run() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");

    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(page_running(&world, "fetch('/api/v1/app-call/');"))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/app-call/"))
        .respond_with(refused())
        .mount(&instagram)
        .await;

    let seen = requests_so_far(&instagram).await;
    let out = snob(
        tmp.path(),
        &instagram,
        &["unfollowers", "--format", "json"],
        None,
    );
    assert_eq!(out.status.code(), Some(5), "{}", told(tmp.path(), &out));

    let after = api_paths_since(&instagram, seen).await;
    assert_eq!(
        after,
        ["/api/v1/app-call/"],
        "snob sent nothing after the push-back"
    );
    assert_one_rate_limit_was_recorded(tmp.path());
}

/// Refuses snob's request two and a half seconds after it arrives, and says
/// at once that it has.
struct RefusedSlowly(Arc<AtomicBool>);

impl wiremock::Respond for RefusedSlowly {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.0.store(true, Ordering::SeqCst);
        refused().set_delay(std::time::Duration::from_millis(2500))
    }
}

/// Open once snob's request has arrived.
struct Gate(Arc<AtomicBool>);

impl wiremock::Respond for Gate {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let open = self.0.load(Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_string(if open { "open" } else { "shut" })
    }
}

/// Heard while snob's own request is on its way — the app's call refused
/// after snob's was sent and before it was answered — it is still recorded
/// once, and snob's request comes to nothing more.
#[tokio::test]
async fn a_push_back_heard_while_snob_is_asking_is_recorded_once() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");

    // The app's call waits at a gate of the page's own until snob's first
    // request has reached the server, which answers that one slowly, and is
    // refused at once: heard while snob's is on its way, whatever the timing.
    let asking = Arc::new(AtomicBool::new(false));
    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(page_running(
            &world,
            "const wait = () => fetch('/gate').then(r => r.text()).then(t =>
               t === 'open' ? fetch('/api/v1/app-call/') : setTimeout(wait, 50));
             wait();",
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/gate"))
        .respond_with(Gate(Arc::clone(&asking)))
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/app-call/"))
        .respond_with(refused())
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/(users|friendships)/"))
        .respond_with(RefusedSlowly(Arc::clone(&asking)))
        .with_priority(1)
        .mount(&instagram)
        .await;

    let seen = requests_so_far(&instagram).await;
    let out = snob(
        tmp.path(),
        &instagram,
        &["unfollowers", "--format", "json"],
        None,
    );
    assert_eq!(out.status.code(), Some(5), "{}", told(tmp.path(), &out));
    let asked = api_paths_since(&instagram, seen).await;
    assert!(
        asked.first().is_some_and(|p| p != "/api/v1/app-call/")
            && asked.iter().any(|p| p == "/api/v1/app-call/"),
        "snob's request was on its way when the app's was refused: {asked:?}"
    );
    assert_one_rate_limit_was_recorded(tmp.path());
}

/// The app taking the session to the challenge is heard from the tab's
/// address, with no request to see, and stops the run with the challenge's
/// code.
#[tokio::test]
async fn a_route_to_the_challenge_stops_the_run() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");

    Mock::given(method("GET"))
        .and(url_path("/"))
        .respond_with(page_running(
            &world,
            "setTimeout(() => history.pushState({}, '', '/challenge/abc/'), 200);",
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;

    let seen = requests_so_far(&instagram).await;
    let out = snob(tmp.path(), &instagram, &["whoami"], None);
    assert_eq!(out.status.code(), Some(4), "{}", told(tmp.path(), &out));
    assert!(
        said(&out).contains("security check"),
        "{}",
        told(tmp.path(), &out)
    );
    let asked = api_paths_since(&instagram, seen).await.len();
    assert_eq!(asked, 0, "nothing was asked after the challenge");
    assert!(
        cooldown_left(tmp.path()).is_some(),
        "the challenge was written down"
    );
}

/// A write goes out from the page too: the tab loads the profile, and the
/// mutation is built beside that document in the app's form, which the
/// fake's oracle takes, after the app's own calls on it, and sent once,
/// with the browser's own CSRF token and the headers its network stack adds
/// to a POST. Nothing of snob's is sent after it: only what the browser
/// loads for the page by itself.
#[tokio::test]
async fn a_follow_is_sent_once_from_the_page() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");
    // The world's `/someone/` names no other host: the tab would ask it.
    common::remember_doc_ids(tmp.path());

    let out = snob(tmp.path(), &instagram, &["follow", "someone", "-y"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));

    let received = snobs_requests(&instagram).await;
    let writes: Vec<&Request> = received.iter().filter(|r| is_a_write(r)).collect();
    assert_eq!(writes.len(), 1, "one follow, one write");
    let write = writes[0];
    assert_eq!(write.url.path(), "/api/graphql");
    assert_eq!(header(write, "x-csrftoken"), Some(SERVED_CSRF));
    assert_eq!(header(write, "sec-fetch-mode"), Some("cors"));
    assert_eq!(
        header(write, "origin"),
        Some(instagram.uri().trim_end_matches('/'))
    );
    assert!(
        header(write, "referer").is_some_and(|r| r.ends_with("/someone/")),
        "{:?}",
        header(write, "referer")
    );
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&write.body)
        .into_owned()
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(form.len(), 29, "{form:?}");
    assert_eq!(
        field("fb_api_req_friendly_name"),
        Some("usePolarisFollowMutation")
    );
    assert_eq!(field("doc_id"), Some("26508036048874888"), "the one known");
    assert_eq!(field("av"), Some(ig::fbid(42).as_str()));
    assert_eq!(field("fb_dtsg"), Some(ig::relay_token(42).as_str()));
    assert_eq!(
        field("__crn"),
        Some("comet.igweb.PolarisProfilePostsTabRoute")
    );

    // After the app's own calls on the profile's document, and nothing of
    // snob's after the write.
    let req = |request: &Request| {
        url::form_urlencoded::parse(&request.body)
            .find(|(n, _)| n == "__req")
            .and_then(|(_, v)| u32::from_str_radix(&v, 36).ok())
    };
    let session = field("__s").unwrap_or_default();
    let all = instagram.received_requests().await.unwrap_or_default();
    let apps_on_the_document: Vec<u32> = all
        .iter()
        .filter(|r| is_the_apps(r))
        .filter(|r| url::form_urlencoded::parse(&r.body).any(|(n, v)| n == "__s" && v == session))
        .filter_map(req)
        .collect();
    assert!(!apps_on_the_document.is_empty(), "the app's calls on it");
    let ours = req(write).expect("a __req");
    assert!(
        apps_on_the_document.iter().all(|app| *app < ours),
        "{ours} after {apps_on_the_document:?}"
    );
    // The browser's own loads for the page, such as its icon, are no-cors
    // GETs, which the audit leaves out for the same reason.
    let at = received.iter().position(is_a_write).unwrap();
    let after: Vec<String> = received[at + 1..]
        .iter()
        .filter(|r| {
            r.method != wiremock::http::Method::GET
                || header(r, "sec-fetch-mode") != Some("no-cors")
        })
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert!(after.is_empty(), "sent after the write: {after:?}");
    let found = world.audit(&instagram, &[]).await;
    assert!(found.is_empty(), "{found:?}");
}

/// Whether `request` is one of the two writes: a POST naming a mutation,
/// where the reads snob posts name queries.
fn is_a_write(request: &Request) -> bool {
    request.method == wiremock::http::Method::POST
        && header(request, "x-fb-friendly-name").is_some_and(|name| name.ends_with("Mutation"))
}

/// The profile a write is made from, sent on to the challenge, is said as
/// the challenge: the listener hears the document's redirect and stops the
/// tab, and what snob read from the tab it stopped is not taken for an
/// answer that left the site. Nothing is written, and the challenge is
/// recorded.
#[tokio::test]
async fn a_challenge_on_the_page_a_write_reads_is_said_as_one() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");

    Mock::given(method("GET"))
        .and(url_path("/someone/"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", "/challenge/abc/"))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/challenge/abc/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><title>Help us confirm it's you</title>",
            "text/html",
        ))
        .mount(&instagram)
        .await;

    let out = snob(tmp.path(), &instagram, &["follow", "someone", "-y"], None);
    assert_eq!(out.status.code(), Some(4), "{}", told(tmp.path(), &out));
    let written = snobs_requests(&instagram)
        .await
        .iter()
        .filter(|r| is_a_write(r))
        .count();
    assert_eq!(written, 0, "nothing was written");
    assert!(
        cooldown_left(tmp.path()).is_some(),
        "the challenge was written down"
    );
    // The challenge is where Instagram sent the profile's load, the one hop
    // the browser follows; nothing else is.
    let found = world.audit(&instagram, &[]).await;
    assert_eq!(found, ["refused: a navigation to /challenge/abc/"]);
}

/// Another account's profile and followers, read through the browser: the
/// name through the route definitions, the profile by its pk, and the list
/// in the app's pages of twelve; never the REST profile.
#[tokio::test]
async fn profile_and_followers_of_someone_through_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let out = snob(tmp.path(), &instagram, &["profile", "someone"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(said(&out).contains("someone"), "{}", said(&out));
    let out = snob(
        tmp.path(),
        &instagram,
        &["followers", "someone", "--yes", "--format", "json"],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(said(&out).contains("user24"), "{}", said(&out));

    let asked = api_paths_since(&instagram, 0).await;
    assert!(
        asked.iter().any(|p| p == "/api/graphql"),
        "the profile was read by its pk: {asked:?}"
    );
    assert_eq!(world.profile_queries().len(), 2, "{asked:?}");
    // The list, twelve at a time, asked as the app asks it from the
    // account's page.
    let pages: Vec<Request> = snobs_requests(&instagram)
        .await
        .into_iter()
        .filter(|r| r.url.path() == "/api/v1/friendships/9001/followers/")
        .collect();
    assert_eq!(pages.len(), 3, "{asked:?}");
    for (page, query) in pages.iter().zip([
        "count=12&search_surface=follow_list_page",
        "count=12&max_id=12&search_surface=follow_list_page",
        "count=12&max_id=24&search_surface=follow_list_page",
    ]) {
        assert_eq!(page.url.query(), Some(query));
        assert_eq!(
            header(page, "referer"),
            Some(format!("{}/someone/", instagram.uri()).as_str())
        );
    }
    let found = world
        .audit(
            &instagram,
            &[
                "/api/v1/users/web_profile_info/",
                "/api/v1/users/9001/info/",
                "/api/v1/highlights/*/highlights_tray/",
                "/api/v1/feed/reels_media/",
            ],
        )
        .await;
    assert!(found.is_empty(), "{found:?}");
}

/// Another account's highlights through the browser: the tray is the web
/// client's tray query, whose entries know no count and no dates, and an
/// entry's items are its highlights query, downloaded from the picture's
/// address; neither REST read is sent.
#[tokio::test]
async fn highlights_of_someone_through_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let out = snob(
        tmp.path(),
        &instagram,
        &["highlights", "someone", "--format", "json"],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    let tray: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", told(tmp.path(), &out)));
    assert_eq!(tray["username"], "someone", "{tray}");
    let rows = tray["highlights"].as_array().expect("the tray's rows");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["title"], "Trips");
    for row in rows {
        for unknown in ["items", "created_at", "updated_at"] {
            assert!(row[unknown].is_null(), "{unknown}: {row}");
        }
    }

    let saved = tmp.path().join("one.jpg");
    let out = snob(
        tmp.path(),
        &instagram,
        &[
            "highlights",
            "someone",
            "1",
            "-d",
            "1",
            "-o",
            saved.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert_eq!(std::fs::read(&saved).unwrap(), b"\xff\xd8\xff\xe0 invented");
    let asked: Vec<String> = snobs_requests(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path() == "/graphql/query")
        .filter_map(|r| {
            url::form_urlencoded::parse(&r.body)
                .find(|(n, _)| n == "fb_api_req_friendly_name")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        asked,
        [snob_ig::allowlist::Operation::HighlightsPage.friendly_name()]
    );

    let found = world
        .audit(
            &instagram,
            &[
                "/api/v1/highlights/*/highlights_tray/",
                "/api/v1/feed/reels_media/",
                "/api/v1/users/web_profile_info/",
            ],
        )
        .await;
    assert!(found.is_empty(), "{found:?}");
}

/// Another account's stories through the browser: the gallery query, over
/// the tray the home page shows, and a story downloaded from its address;
/// the tab never loads a story's own page, and `reels_media` is not sent.
#[tokio::test]
async fn stories_of_someone_through_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let out = snob(
        tmp.path(),
        &instagram,
        &["stories", "someone", "--format", "json"],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    let reel: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", told(tmp.path(), &out)));
    assert_eq!(reel["username"], "someone", "{reel}");
    assert_eq!(reel["stories"].as_array().map(Vec::len), Some(2), "{reel}");

    let saved = tmp.path().join("one.jpg");
    let out = snob(
        tmp.path(),
        &instagram,
        &[
            "stories",
            "someone",
            "-d",
            "1",
            "-o",
            saved.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert_eq!(std::fs::read(&saved).unwrap(), b"\xff\xd8\xff\xe0 invented");
    let asked: Vec<String> = snobs_requests(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path() == "/graphql/query")
        .filter_map(|r| {
            url::form_urlencoded::parse(&r.body)
                .find(|(n, _)| n == "fb_api_req_friendly_name")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    let gallery = snob_ig::allowlist::Operation::ReelGallery.friendly_name();
    assert_eq!(asked, [gallery, gallery]);
    let documents: Vec<String> = snobs_requests(&instagram)
        .await
        .iter()
        .filter(|r| header(r, "sec-fetch-mode") == Some("navigate"))
        .map(|r| r.url.path().to_string())
        .collect();
    assert!(
        documents.iter().all(|p| !p.starts_with("/stories/")),
        "{documents:?}"
    );

    let found = world
        .audit(
            &instagram,
            &[
                "/api/v1/feed/reels_media/",
                "/api/v1/users/web_profile_info/",
            ],
        )
        .await;
    assert!(found.is_empty(), "{found:?}");
}

/// Another account's profile picture through the browser: the full size the
/// profile query carries, downloaded from its address, with no lookup by id.
#[tokio::test]
async fn pfp_of_someone_through_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let saved = tmp.path().join("face.jpg");
    let out = snob(
        tmp.path(),
        &instagram,
        &["pfp", "someone", "-o", saved.to_str().unwrap()],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(!said(&out).contains("Full size"), "{}", said(&out));
    assert_eq!(std::fs::read(&saved).unwrap(), b"\xff\xd8\xff\xe0 invented");
    let pictures: Vec<String> = snobs_requests(&instagram)
        .await
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| p.starts_with("/media/"))
        .collect();
    assert_eq!(pictures, ["/media/9001-full.jpg"]);

    let found = world
        .audit(
            &instagram,
            &["/api/v1/users/*/info/", "/api/v1/users/web_profile_info/"],
        )
        .await;
    assert!(found.is_empty(), "{found:?}");
}

/// The Relay operations snob sent since `since`, by the name each form
/// carries.
async fn relay_since(instagram: &MockServer, since: usize) -> Vec<String> {
    snobs_requests(instagram).await[since..]
        .iter()
        .filter(|r| r.url.path() == "/api/graphql")
        .filter_map(|r| {
            url::form_urlencoded::parse(&r.body)
                .find(|(n, _)| n == "fb_api_req_friendly_name")
                .map(|(_, v)| v.into_owned())
        })
        .collect()
}

/// The own account's followers, and a run of the monitor on it, through the
/// browser: its counters are its hover card's, read by pk, and a run prints
/// its report as JSON.
#[tokio::test]
async fn the_own_counters_come_from_the_hover_card_through_the_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "the login failed: ");

    let out = snob(
        tmp.path(),
        &instagram,
        &["followers", "--format", "json"],
        None,
    );
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    assert!(said(&out).contains("user14"), "{}", said(&out));
    let hover = snob_ig::allowlist::Operation::HoverCard.friendly_name();
    assert_eq!(relay_since(&instagram, 0).await, [hover]);

    let config = tmp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("watch.toml"), "schema = 1\nevery = \"6h\"\n").unwrap();
    let since = requests_so_far(&instagram).await;
    let out = snob(tmp.path(), &instagram, &["watch", "once", "--json"], None);
    assert!(out.status.success(), "{}", told(tmp.path(), &out));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", told(tmp.path(), &out)));
    assert_eq!(report["run"]["looked"], true, "{report}");
    assert_eq!(relay_since(&instagram, since).await, [hover]);

    let found = world
        .audit(&instagram, &["/api/v1/users/web_profile_info/"])
        .await;
    assert!(found.is_empty(), "{found:?}");
}

/// Two accounts ask through the one owner, each from its own browser: every
/// Relay form carries its own account's fbid, tokens and counter, which the
/// fake refuses to see crossed.
#[tokio::test]
async fn two_accounts_ask_through_one_owner_without_crossing() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    for sessionid in ["42%3Afirst%3A17", "43%3Asecond%3A17"] {
        logged_in(tmp.path(), &instagram, sessionid, &format!("{sessionid}: "));
    }
    let (one, two) = std::thread::scope(|both| {
        let one = both.spawn(|| {
            snob(
                tmp.path(),
                &instagram,
                &["profile", "someone", "--account", "42"],
                None,
            )
        });
        let two = both.spawn(|| {
            snob(
                tmp.path(),
                &instagram,
                &["profile", "someone", "--account", "43"],
                None,
            )
        });
        (one.join().unwrap(), two.join().unwrap())
    });
    assert!(one.status.success(), "{}", told(tmp.path(), &one));
    assert!(two.status.success(), "{}", told(tmp.path(), &two));
    assert_eq!(world.profile_queries().len(), 2);
    let fbids: std::collections::BTreeSet<String> = snobs_requests(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path() == "/api/graphql")
        .filter_map(|r| {
            url::form_urlencoded::parse(&r.body)
                .find(|(n, _)| n == "av")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        fbids,
        [ig::fbid(42), ig::fbid(43)].into_iter().collect(),
        "each account sent as itself"
    );
    let found = world.audit(&instagram, &[]).await;
    assert!(found.is_empty(), "{found:?}");
}

/// How many times the site was loaded since `since`: once per browser that
/// started, since a tab already on the site is not sent to it again.
async fn page_loads(instagram: &MockServer, since: usize) -> usize {
    snobs_requests(instagram).await[since..]
        .iter()
        .filter(|r| r.url.path() == "/")
        .count()
}

/// Two commands on one account at once: a second browser on the profile would
/// find it held and fail, so both send from the one browser the owner runs,
/// and the site is loaded once.
#[tokio::test]
async fn two_commands_at_once_share_one_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");

    let seen = requests_so_far(&instagram).await;
    // Started together: the first to ask starts the browser, which takes
    // seconds, and the other's request waits its turn on the tab meanwhile.
    let (one, two) = std::thread::scope(|both| {
        let one = both.spawn(|| snob(tmp.path(), &instagram, &["whoami"], None));
        let two = both.spawn(|| snob(tmp.path(), &instagram, &["whoami"], None));
        (one.join().unwrap(), two.join().unwrap())
    });
    assert!(one.status.success(), "{}", told(tmp.path(), &one));
    assert!(two.status.success(), "{}", told(tmp.path(), &two));
    assert_eq!(
        page_loads(&instagram, seen).await,
        1,
        "each command started a browser of its own"
    );
}

/// Waits for the owner to leave, which it does once no command is connected
/// and its browsers are closed.
async fn owner_gone(paths: &AppPaths) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while snob_cli::owner::is_running(paths).await {
        assert!(
            std::time::Instant::now() < deadline,
            "the owner stayed with nothing to do"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// The owner is gone once no command is left and it holds no browser, and a
/// run told to keep its browser to itself starts no owner at all.
#[tokio::test]
async fn the_owner_leaves_when_nothing_is_left() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;
    let paths = AppPaths::rooted_at(tmp.path());
    logged_in(tmp.path(), &instagram, SESSIONID, "");
    let log = paths.data_dir().join("browser-owner.log");
    assert!(log.exists(), "the login's request went through an owner");

    owner_gone(&paths).await;

    let other = tempfile::tempdir().unwrap();
    let out = snob_with(
        other.path(),
        &instagram,
        &["login", "--paste"],
        Some(&format!("{SESSIONID}\n")),
        &[("SNOB_NO_OWNER", "1")],
    );
    assert!(out.status.success(), "{}", told(other.path(), &out));
    assert!(
        !AppPaths::rooted_at(other.path())
            .data_dir()
            .join("browser-owner.log")
            .exists(),
        "SNOB_NO_OWNER started an owner"
    );
}

/// Returns once a crossing of [`CROSSING_42`] has one of its list requests
/// after `since` on the way. Each is answered only after a few seconds, so the
/// crossing is then in the middle of using the account's browser.
async fn a_list_of_42_asked_for(instagram: &MockServer, since: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let received = snobs_requests(instagram).await;
        if received[since..]
            .iter()
            .any(|r| r.url.path().starts_with("/api/v1/friendships/42/"))
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the crossing never asked for a list"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// What `R` answers, a few seconds late.
struct Slowly<R>(R);

impl<R: wiremock::Respond> wiremock::Respond for Slowly<R> {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0
            .respond(request)
            .set_delay(std::time::Duration::from_secs(5))
    }
}

/// A crossing of account 42's own lists, fetched afresh.
const CROSSING_42: &[&str] = &[
    "unfollowers",
    "--account",
    "42",
    "--refresh",
    "--format",
    "json",
];

/// How many times account 42's browser loaded the site since `since`.
async fn page_loads_of_42(instagram: &MockServer, since: usize) -> usize {
    snobs_requests(instagram).await[since..]
        .iter()
        .filter(|r| {
            r.url.path() == "/"
                && header(r, "cookie").is_some_and(|c| c.contains("sessionid=42%3A"))
        })
        .count()
}

/// Logging another account in or out, or purging it, closes that account's
/// browser alone: a command using another account's goes on in the same one.
#[tokio::test]
async fn another_accounts_login_logout_or_purge_leaves_this_ones_browser() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (world, instagram) = fake_instagram().await;
    for sessionid in ["42%3Afirst%3A17", "43%3Asecond%3A17"] {
        logged_in(tmp.path(), &instagram, sessionid, &format!("{sessionid}: "));
    }
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api/v1/friendships/42/(followers|following)/$",
        ))
        .respond_with(Slowly(world.lists()))
        .with_priority(1)
        .mount(&instagram)
        .await;

    let beside: [(&[&str], Option<&str>); 3] = [
        (&["login", "--paste", "--add"], Some("43%3Aagain%3A18\n")),
        (&["logout"], None),
        (&["purge", "--account", "43", "--yes"], None),
    ];
    for (args, typed) in beside {
        let since = requests_so_far(&instagram).await;
        let crossing = snob_started(tmp.path(), &instagram, CROSSING_42, None, &[]);
        a_list_of_42_asked_for(&instagram, since).await;
        let out = snob(tmp.path(), &instagram, args, typed);
        assert!(out.status.success(), "{args:?}: {}", told(tmp.path(), &out));
        let crossed = crossing.wait_with_output().unwrap();
        assert!(
            crossed.status.success(),
            "{args:?}: {}",
            told(tmp.path(), &crossed)
        );
        assert_eq!(
            page_loads_of_42(&instagram, since).await,
            1,
            "{args:?} closed account 42's browser under its crossing"
        );
    }
}

/// With no owner to share a browser through, a command on an account whose
/// profile another browser holds waits for it, says so once, and goes on when
/// it is free.
#[tokio::test]
async fn a_command_with_no_owner_waits_for_a_profile_in_use() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (_, instagram) = fake_instagram().await;
    logged_in(tmp.path(), &instagram, SESSIONID, "");
    let paths = AppPaths::rooted_at(tmp.path());
    owner_gone(&paths).await;

    let found = browser::detect().expect("a browser starts, so one is installed");
    let cancel = CancelToken::default();
    let profile = paths.browser_profile_for(snob_core::Pk::new(42));
    let flags = ["--headless=new".to_string()];
    let launched = cdp::launch_headless(&found, &profile, &flags).unwrap();
    let holding = cdp::Cdp::connect(launched, &cancel).await.unwrap();

    let mut waiting = snob_started(
        tmp.path(),
        &instagram,
        &["unfollowers", "--refresh", "--format", "json"],
        None,
        &[("SNOB_NO_OWNER", "1")],
    );
    // The profile is let go of only once the command has said it waits, so
    // a slow start cannot find it free already.
    let (told, notice) = std::sync::mpsc::channel();
    let stderr = waiting.stderr.take().expect("stderr was piped");
    let reading = std::thread::spawn(move || {
        use std::io::Read;
        let (mut stderr, mut gathered, mut chunk) = (stderr, Vec::new(), [0u8; 4096]);
        while let Ok(read @ 1..) = stderr.read(&mut chunk) {
            gathered.extend_from_slice(&chunk[..read]);
            if String::from_utf8_lossy(&gathered).contains("Another snob is using") {
                let _ = told.send(());
            }
        }
        String::from_utf8_lossy(&gathered).into_owned()
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while notice.try_recv().is_err() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    holding.close().await;
    let out = waiting.wait_with_output().unwrap();
    let said = format!("{}{}", said(&out), reading.join().unwrap());
    assert!(out.status.success(), "{said}");
    assert_eq!(
        said.matches("Another snob is using").count(),
        1,
        "the wait is said once: {said}"
    );
    assert!(
        said.contains("waiting for it to finish (Ctrl+C to stop)"),
        "{said}"
    );
}

/// A run of the binary on the REST path: the sandbox pointed at `instagram`
/// with no browser, sending with `reqwest` as `SNOB_NO_BROWSER` does.
fn snob_directly(
    root: &Path,
    instagram: &MockServer,
    args: &[&str],
    typed: Option<&str>,
) -> Output {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_snob"))
        .arg("--sandbox-root")
        .arg(root)
        .arg("--ig-base-url")
        .arg(instagram.uri())
        .args(args)
        .env("NO_COLOR", "1")
        .env("SNOB_NO_BROWSER", "1")
        .env_remove("SNOB_NO_OWNER")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    if let Some(typed) = typed {
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(typed.as_bytes())
            .expect("the binary reads what it is given");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("the binary finishes")
}

/// What one command of the parity matrix did: its exit code, what it
/// printed, and the file it wrote, each with the run's own paths and times
/// made the same on both sides.
#[derive(Debug)]
struct Shown {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    file: Option<String>,
}

/// The commands both paths are compared on, several per login, in order:
/// a name, its arguments, and the file it writes, if any. `{file}` in an
/// argument is that file's path.
const MATRIX: &[(&str, &[&str], Option<&str>)] = &[
    ("whoami", &["whoami", "--json"], None),
    ("profile", &["profile", "someone", "--format", "json"], None),
    (
        "followers",
        &["followers", "someone", "--yes", "--format", "json"],
        None,
    ),
    (
        "following",
        &["following", "someone", "--yes", "--format", "json"],
        None,
    ),
    ("unfollowers", &["unfollowers", "--format", "json"], None),
    ("fans", &["fans", "--format", "json"], None),
    ("friends", &["friends", "--format", "json"], None),
    ("scan", &["scan", "--format", "json"], None),
    ("stories", &["stories", "someone", "--format", "json"], None),
    (
        "stories -d",
        &["stories", "someone", "-d", "1", "-o", "{file}"],
        Some("story.jpg"),
    ),
    (
        "highlights",
        &["highlights", "someone", "--format", "json"],
        None,
    ),
    (
        "highlights -d",
        &["highlights", "someone", "1", "-d", "1", "-o", "{file}"],
        Some("highlight.jpg"),
    ),
    ("pfp", &["pfp", "someone", "-o", "{file}"], Some("face.jpg")),
    ("follow", &["follow", "someone", "-y"], None),
    ("unfollow", &["unfollow", "someone", "-y"], None),
    ("watch check", &["watch", "check"], None),
    ("watch once", &["watch", "once", "--json"], None),
];

/// Runs [`MATRIX`] in a sandbox of its own, logged in on `instagram` with
/// `run`, one path's way of running the binary.
fn shown_by(
    instagram: &MockServer,
    run: impl Fn(&Path, &MockServer, &[&str], Option<&str>) -> Output,
) -> Vec<(&'static str, Shown)> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // The REST path writes with the token the session was stored with; the
    // browser's is its own, whatever login stored.
    let login = ["login", "--paste", "--csrftoken", ig::CSRF];
    let out = run(root, instagram, &login, Some(&format!("{SESSIONID}\n")));
    assert!(
        out.status.success(),
        "the login failed: {}",
        told(root, &out)
    );
    let from = now();
    common::remember_doc_ids(root);
    let config = root.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("watch.toml"), "schema = 1\nevery = \"6h\"\n").unwrap();

    // The root as printed, and as a JSON string writes it, which on Windows
    // doubles every backslash.
    let shown_root = root.display().to_string();
    let json_root = serde_json::to_string(&shown_root).unwrap();
    let json_root = json_root.trim_matches('"');
    let normal = |text: &str| {
        let text = text
            .replace(json_root, "<root>")
            .replace(&shown_root, "<root>")
            .replace(instagram.uri().trim_end_matches('/'), "<instagram>");
        without_the_runs_times(&text, from - 60, now() + 60)
    };
    MATRIX
        .iter()
        .map(|(name, args, file)| {
            let path = file.map(|file| root.join(file));
            let args: Vec<String> = args
                .iter()
                .map(|arg| match (&path, *arg) {
                    (Some(path), "{file}") => path.display().to_string(),
                    _ => arg.to_string(),
                })
                .collect();
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = run(root, instagram, &args, None);
            let shown = Shown {
                code: out.status.code(),
                stdout: normal(&String::from_utf8_lossy(&out.stdout)),
                stderr: normal(&String::from_utf8_lossy(&out.stderr)),
                file: path.map(|path| match std::fs::read(&path) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(e) => format!("not written: {e}"),
                }),
            };
            (*name, shown)
        })
        .collect()
}

/// The clock, in seconds.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `text` with what depends on when it ran made the same on every run: a
/// moment between `from` and `to` in seconds, written as [`A_MOMENT`] so
/// that JSON stays JSON; a date and time of day as the listings write them
/// (`Sep 30 at 18:12`).
fn without_the_runs_times(text: &str, from: u64, to: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut digits = String::new();
    let mut out = String::new();
    for c in text.chars().chain(std::iter::once('\0')) {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        match digits.parse::<u64>() {
            Ok(moment) if digits.len() == 10 && (from..=to).contains(&moment) => {
                out.push_str(A_MOMENT);
            }
            _ => out.push_str(&digits),
        }
        digits.clear();
        if c != '\0' {
            out.push(c);
        }
    }
    let dated = |words: &[&str]| -> Option<String> {
        let [month, day, at, clock, ..] = words else {
            return None;
        };
        let time = clock.get(..5)?;
        (MONTHS.contains(month)
            && day.trim_end_matches(',').parse::<u8>().is_ok()
            && *at == "at"
            && time.as_bytes()[2] == b':'
            && time.bytes().filter(u8::is_ascii_digit).count() == 4)
            .then(|| format!("<when>{}", &clock[5..]))
    };
    let lines: Vec<String> = out
        .lines()
        .map(|line| {
            let words: Vec<&str> = line.split(' ').collect();
            let mut kept = Vec::new();
            let mut at = 0;
            while at < words.len() {
                match dated(&words[at..]) {
                    Some(when) => {
                        kept.push(when);
                        at += 4;
                    }
                    None => {
                        kept.push(words[at].to_string());
                        at += 1;
                    }
                }
            }
            kept.join(" ")
        })
        .collect();
    let trailing = if text.ends_with('\n') { "\n" } else { "" };
    lines.join("\n") + trailing
}

/// `text` with each count of requests it prints made `<n>`: what a path
/// spends is its own, so only runs on one path compare it.
fn without_request_counts(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let mut line = line.to_string();
            for unit in [" requests", " request"] {
                if let Some(end) = line.find(unit) {
                    let start = line[..end].rfind(' ').map_or(0, |space| space + 1);
                    if start < end && line[start..end].bytes().all(|b| b.is_ascii_digit()) {
                        line.replace_range(start..end, "<n>");
                        break;
                    }
                }
            }
            line
        })
        .collect();
    let trailing = if text.ends_with('\n') { "\n" } else { "" };
    lines.join("\n") + trailing
}

/// A run of [`MATRIX`] with its request counts made `<n>`
/// ([`without_request_counts`]), to compare with a run on the other path.
fn uncounted(run: &[(&'static str, Shown)]) -> Vec<(&'static str, Shown)> {
    run.iter()
        .map(|(name, shown)| {
            let shown = Shown {
                code: shown.code,
                stdout: without_request_counts(&shown.stdout),
                stderr: without_request_counts(&shown.stderr),
                file: shown.file.clone(),
            };
            (*name, shown)
        })
        .collect()
}

/// What a moment of the run itself is written as, once made the same on
/// both paths.
const A_MOMENT: &str = "1000000000";

/// Each place where two JSON documents differ, by JSON pointer.
fn json_differences(
    rest: &serde_json::Value,
    web: &serde_json::Value,
    at: &str,
    found: &mut Vec<String>,
) {
    use serde_json::Value;
    match (rest, web) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let null = Value::Null;
                let at = format!("{at}/{key}");
                json_differences(
                    a.get(key).unwrap_or(&null),
                    b.get(key).unwrap_or(&null),
                    &at,
                    found,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (n, (a, b)) in a.iter().zip(b).enumerate() {
                json_differences(a, b, &format!("{at}/{n}"), found);
            }
        }
        _ if rest != web => found.push(format!("{at}: {rest} | {web}")),
        _ => {}
    }
}

/// Each place where two outputs differ: by JSON pointer when both are JSON
/// documents (or lines of them, as many on each side), and otherwise by
/// the lines of an in-order diff ([`line_diff`]).
fn differences(stream: &str, rest: &str, web: &str) -> Vec<String> {
    let mut found = Vec::new();
    let documents = |text: &str| -> Option<Vec<serde_json::Value>> {
        let text = text.trim();
        if text.is_empty() {
            return Some(Vec::new());
        }
        serde_json::from_str(text)
            .map(|one| vec![one])
            .ok()
            .or_else(|| {
                text.lines()
                    .map(|line| serde_json::from_str(line).ok())
                    .collect()
            })
    };
    if let (Some(a), Some(b)) = (documents(rest), documents(web))
        && a.len() == b.len()
    {
        for (n, (a, b)) in a.iter().zip(&b).enumerate() {
            let at = if n == 0 {
                String::new()
            } else {
                format!("[{n}]")
            };
            json_differences(a, b, &at, &mut found);
        }
        return found.into_iter().map(|d| format!("{stream} {d}")).collect();
    }
    let (a, b): (Vec<&str>, Vec<&str>) = (rest.lines().collect(), web.lines().collect());
    line_diff(&a, &b)
        .into_iter()
        .map(|d| format!("{stream} {d}"))
        .collect()
}

/// The lines to take out of `a` (`-`) and put in (`+`) to make `b`, in
/// order: a line moved, repeated or dropped on one side is a difference.
fn line_diff(a: &[&str], b: &[&str]) -> Vec<String> {
    // The longest common subsequence, by the lengths of the suffixes'.
    let mut common = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            common[i][j] = if a[i] == b[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut found) = (0, 0, Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j == b.len() || (i < a.len() && common[i + 1][j] >= common[i][j + 1]) {
            found.push(format!("-{}", a[i]));
            i += 1;
        } else {
            found.push(format!("+{}", b[j]));
            j += 1;
        }
    }
    found
}

/// The line diff the parity check reads holds order and count.
#[test]
fn a_line_moved_or_repeated_is_a_difference() {
    assert!(line_diff(&["a", "b"], &["a", "b"]).is_empty());
    assert_eq!(line_diff(&["a", "b"], &["b", "a"]), ["-a", "+a"]);
    assert_eq!(line_diff(&["a"], &["a", "a"]), ["+a"]);
    assert_eq!(line_diff(&["a", "x", "b"], &["a", "b", "y"]), ["-x", "+y"]);
}

/// What differs between the REST path and the browser's by design, per
/// command: a start of the difference, a JSON pointer or a line.
const EXPECTED_DIFFERENCES: &[(&str, &str)] = &[
    // The pace of each path's requests, which is when the budget says so.
    ("*", "stderr -the request budget is rationing"),
    ("*", "stderr +the request budget is rationing"),
    // The tray query knows no item count and no dates.
    ("profile", "stdout /highlights/0/items"),
    ("profile", "stdout /highlights/0/updated_at"),
    ("profile", "stdout /highlights/1/items"),
    ("profile", "stdout /highlights/1/updated_at"),
    ("highlights", "stdout /highlights/0/items"),
    ("highlights", "stdout /highlights/0/created_at"),
    ("highlights", "stdout /highlights/0/updated_at"),
    ("highlights", "stdout /highlights/1/items"),
    ("highlights", "stdout /highlights/1/created_at"),
    ("highlights", "stdout /highlights/1/updated_at"),
    // The profile query carries the full-size picture with no size.
    ("pfp", "stderr -Full size: "),
];

/// Whether `difference`, in what `command` showed, is one
/// [`EXPECTED_DIFFERENCES`] lists between the REST path and the browser's.
fn between_the_paths(command: &str, difference: &str) -> bool {
    EXPECTED_DIFFERENCES.iter().any(|(listed, start)| {
        (*listed == "*" || *listed == command) && difference.starts_with(start)
    })
}

/// Every difference between two runs of [`MATRIX`], command by command: the
/// exit code, what each stream printed, and the file written.
fn matrix_differences(
    before: &[(&'static str, Shown)],
    after: &[(&'static str, Shown)],
) -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    for ((name, a), (_, b)) in before.iter().zip(after) {
        if a.code != b.code {
            found.push((*name, format!("exit {:?} | {:?}", a.code, b.code)));
        }
        let streams = differences("stdout", &a.stdout, &b.stdout)
            .into_iter()
            .chain(differences("stderr", &a.stderr, &b.stderr));
        found.extend(streams.map(|d| (*name, d)));
        if a.file != b.file {
            found.push((*name, format!("file {:?} | {:?}", a.file, b.file)));
        }
    }
    found
}

/// Nothing is the same by failing the same way: every command of a run
/// exits 0, and every file it writes holds the fake's picture.
fn every_command_worked(run: &[(&'static str, Shown)]) {
    for (name, shown) in run {
        assert_eq!(shown.code, Some(0), "{name}: {shown:#?}");
        assert!(
            shown
                .file
                .as_deref()
                .is_none_or(|file| file.contains("invented")),
            "{name}: {shown:#?}"
        );
    }
}

/// One world, the whole command matrix, through both paths: without a
/// browser on the REST face, and through Chromium on the web face. What the
/// two show differs only where [`EXPECTED_DIFFERENCES`] says it does.
#[tokio::test]
async fn the_two_paths_show_the_same() {
    let Some(_alone) = a_browser_alone().await else {
        return;
    };
    let rest_world = World::new();
    let rest_face = rest_world.serve_rest().await;
    let rest = shown_by(&rest_face, snob_directly);
    let (web_world, web_face) = fake_instagram().await;
    let web = shown_by(&web_face, snob);

    every_command_worked(&rest);
    every_command_worked(&web);
    let unexpected: Vec<String> = matrix_differences(&uncounted(&rest), &uncounted(&web))
        .into_iter()
        .filter(|(name, d)| !between_the_paths(name, d))
        .map(|(name, d)| format!("{name}: {d}"))
        .collect();
    let found = web_world.audit(&web_face, &[]).await;
    assert!(found.is_empty(), "{found:?}");
    assert!(
        unexpected.is_empty(),
        "{}\n\nrest: {rest:#?}\n\nweb: {web:#?}",
        unexpected.join("\n")
    );
}

/// Serves both faces of the fake Instagram, each over a world of its own,
/// and prints their addresses, for `SNOB_TEST_SERVE_SECS` seconds (ten
/// minutes by default): a person points a sandboxed snob at one to look.
#[tokio::test]
#[ignore = "serves the fake Instagram for a person to use; run by hand"]
async fn serve_the_fake_world() {
    let web = World::new();
    let web_face = web.serve_web().await;
    let rest = World::new();
    let rest_face = rest.serve_rest().await;
    println!("web face:  {}", web_face.uri());
    println!("REST face: {}", rest_face.uri());
    let seconds = std::env::var("SNOB_TEST_SERVE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
}
