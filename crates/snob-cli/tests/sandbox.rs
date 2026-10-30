//! The whole binary, driven end to end, with no Instagram and no receiver of
//! anybody's.
//!
//! The library suites call the engine directly: `watch_tick.rs` runs the
//! comparison through `engine::watch::tick`, `watch_webhook.rs` the outbox
//! through `WebhookClient`. This one runs the program: `main`, the dispatch,
//! `AppPaths::discover`, the secret store choosing a backend and the exit code a
//! timer reads, so the parts of `snob watch` a person actually configures and a
//! receiver actually hears from are exercised as a whole. It reaches its mock
//! servers with `reqwest` and needs no browser; `headless.rs` drives the same
//! binary through a real one.
//!
//! **What makes it possible, and what makes it safe.** Three flags, all behind
//! the `testing` Cargo feature so a released binary contains none of them nor
//! the code they reach. `crates/snob-core/tests/sandbox.rs` reads the source to
//! hold that down for `--sandbox-root` and `--ig-base-url`; the third,
//! `--through-the-browser`, is gated by the same feature and requires the
//! second (below). `--sandbox-root` puts every file this run touches under
//! one temporary directory and forces the file backend, so the keyring is not
//! opened at all: the rule `crates/snob-core/tests/keyring.rs` holds the test
//! suite to, applied to the binary. `--ig-base-url` requires `--sandbox-root`,
//! which is the whole safety argument for it existing: a redirected client can
//! only carry a session out of a store inside that root, so the real stored
//! session of whoever runs this is not reachable. `--through-the-browser`,
//! which `headless.rs` adds, requires `--ig-base-url` in turn.
//!
//! The pace is off for the same reason it is off in every other test, and it is
//! honest about it: `IgClient::is_live` answers by address, and the address here
//! really is a local mock server.
//!
//! Every test gets its own sandbox root, its own fake Instagram and its own
//! fake receiver, so nothing shares state and the file cannot pass by accident
//! because a neighbor ran first.
#![cfg(feature = "testing")]

use std::path::Path;
use std::process::{Command, Output};

use snob_core::Pk;
use wiremock::matchers::{method, path as url_path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

mod common;

/// A session cookie's worth of digits, which is all the shape anything checks.
const SESSIONID: &str = "42%3Asandbox%3A17";

/// Given explicitly so nothing goes looking for an installed browser: this has
/// to run the same on a laptop with three of them and on a CI container with
/// none.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// The account the fake Instagram answers about, and the one the session is.
const PK: Pk = Pk::new(42);

/// The real binary, aimed at one sandbox and, when given, one fake Instagram.
///
/// `CARGO_BIN_EXE_snob` is the binary this test's own build produced, so it
/// carries the `testing` feature and nothing else has to be arranged. Every
/// invocation gets `--sandbox-root`, which is what keeps the keyring and the
/// user's real data directory out of it. The variables removed below do not
/// reach the run when set outside the test: an account, a token, a key, a
/// cooldown override or a log level named in the developer's environment is
/// not this test's, and neither is the SSH session the suite may be run from,
/// which changes what `login` offers (`seat`).
fn command(root: &Path, instagram: Option<&MockServer>) -> Command {
    command_of(Path::new(env!("CARGO_BIN_EXE_snob")), root, instagram)
}

/// The same, for a copy of the binary at `program`.
fn command_of(program: &Path, root: &Path, instagram: Option<&MockServer>) -> Command {
    let mut command = Command::new(program);
    command.arg("--sandbox-root").arg(root);
    if let Some(server) = instagram {
        command.arg("--ig-base-url").arg(server.uri());
    }
    // A run that inherits a terminal would try to draw a progress bar and, in
    // the wizard's case, ask a question. Neither is what is under test here.
    command.env("NO_COLOR", "1");
    for inherited in [
        "SNOB_ACCOUNT",
        "SNOB_CSRFTOKEN",
        "SNOB_SIGNING_KEY",
        "SNOB_MCP_KEY",
        "SNOB_MCP_TOKEN",
        "SNOB_IGNORE_COOLDOWN",
        "SNOB_LOG",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
    ] {
        command.env_remove(inherited);
    }
    command
}

/// Runs the binary against one sandbox, and says what it did.
fn snob(root: &Path, instagram: Option<&MockServer>, args: &[&str]) -> Output {
    snob_with(root, instagram, &[], args)
}

/// The same, with these variables set.
fn snob_with(
    root: &Path,
    instagram: Option<&MockServer>,
    env: &[(&str, &str)],
    args: &[&str],
) -> Output {
    command(root, instagram)
        .args(args)
        .envs(env.iter().copied())
        .output()
        .expect("the binary runs")
}

/// The same, with something on standard input — which is how `--paste` reads a
/// sessionid when nobody is at a terminal.
fn snob_typing(root: &Path, instagram: &MockServer, args: &[&str], typed: &str) -> Output {
    use std::io::Write;

    let mut child = command(root, Some(instagram))
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(typed.as_bytes())
        .expect("the binary reads what it is given");
    child.wait_with_output().expect("the binary finishes")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// What the run printed, read as the JSON document it has to be.
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}{}", stdout(output), stderr(output)))
}

/// A directory of the test's own to run the binary from, which is where a
/// download lands when `-o` says nothing.
fn here(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let here = tmp.path().join("here");
    std::fs::create_dir(&here).unwrap();
    here
}

/// An Instagram that answers everything a run needs and nothing it does not.
///
/// `followers` and `following` are the counters the profile declares; the lists
/// themselves come back with that many made-up accounts, so a walk has
/// something real to compare.
async fn fake_instagram(followers: u64, following: u64) -> MockServer {
    let server = MockServer::start().await;

    // `validate()`: the session works. Told apart from a walk of the same list
    // by `count=1`, which is the whole point of that endpoint being the cheap
    // one -- without the constraint this answers the walk as well, and every
    // run comes back with a `following` list that declared two accounts and
    // served none.
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/friendships/{PK}/following/")))
        .and(query_param("count", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
        .mount(&server)
        .await;

    // Who the session belongs to, for `whoami` and for a run with no stored
    // name.
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/users/{PK}/info/")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"user":{"pk":42,"username":"me","full_name":"Me"}}"#),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"data":{{"user":{{"id":"{PK}","username":"me",
                "edge_followed_by":{{"count":{followers}}},
                "edge_follow":{{"count":{following}}}}}}}}}"#
        )))
        .mount(&server)
        .await;

    for (kind, count) in [("followers", followers), ("following", following)] {
        let users: Vec<String> = (0..count)
            .map(|n| format!(r#"{{"pk":{},"username":"user{n}"}}"#, 1_000 + n))
            .collect();
        Mock::given(method("GET"))
            .and(path_regex(format!(r"^/api/v1/friendships/\d+/{kind}/$")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
            )
            .mount(&server)
            .await;
    }

    server
}

/// Logs a sandbox in, so the tests below start from a machine that has a
/// session.
///
/// This is itself the first thing worth asserting: `--paste` falls back to
/// reading a line off standard input when nobody is at a terminal, the session
/// is validated against the server the run was pointed at, and it lands in a
/// file inside the sandbox rather than in the operating system's keyring.
fn log_in(root: &Path, instagram: &MockServer) {
    let out = paste(root, instagram, SESSIONID, &[]);
    assert!(
        out.status.success(),
        "the sandbox could not log in: {}{}",
        stdout(&out),
        stderr(&out)
    );
}

/// Moves every capture in the sandbox a day and an hour into the past.
///
/// The monitor walks a list at most once a day however its counter moves, so
/// a test about what the second run's walk finds has to let the day go by.
fn a_day_passes(root: &Path) {
    snob_store::store::Store::open_at(&database(root))
        .expect("the sandbox has a database once something has run")
        .conn()
        .execute(
            "UPDATE snapshots SET started_at = started_at - 90000,
                                  taken_at = taken_at - 90000",
            [],
        )
        .expect("the captures can be moved");
}

/// The signed-in account's database in the sandbox.
fn database(root: &Path) -> std::path::PathBuf {
    snob_store::paths::AppPaths::rooted_at(root)
        .account(PK)
        .db_file()
}

/// A `watch.toml` in the sandbox, written the way a hand-edit would.
fn configure(root: &Path, body: &str) {
    let dir = root.join("config");
    std::fs::create_dir_all(&dir).expect("the sandbox is writable");
    std::fs::write(dir.join("watch.toml"), body).expect("the sandbox is writable");
}

/// A `watch.toml` that runs every six hours and reports to `receiver`.
fn configure_webhook(root: &Path, receiver: &MockServer) {
    configure(
        root,
        &format!(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"{}/webhook/snob\"\n",
            receiver.uri()
        ),
    );
}

/// A receiver that answers with this status, and remembers what it was sent.
async fn receiver(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(url_path("/webhook/snob"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

async fn posted(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

/// The session goes into the sandbox and the keyring is never opened.
///
/// The rule `crates/snob-core/tests/keyring.rs` holds every test to, asserted of
/// the binary rather than of a test: a run under `--sandbox-root` writes its
/// session to a file inside that root. Only a test of the program can say this:
/// `SecretStore` choosing a backend is `main`'s decision, and no library test
/// reaches it.
#[tokio::test]
async fn a_sandbox_run_keeps_its_session_in_the_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    let stored = snob_store::paths::AppPaths::rooted_at(tmp.path())
        .account(PK)
        .session_file();
    assert!(
        stored.is_file(),
        "the session has to land in the sandbox, not in the keyring"
    );

    let out = snob(tmp.path(), Some(&instagram), &["whoami", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["pk"], PK.get(), "{said}");
    assert_eq!(
        said["storage"], "file",
        "a sandbox run must not reach the operating system's store: {said}"
    );
    assert_eq!(
        said["storage_path"].as_str().map(std::path::Path::new),
        Some(stored.as_path()),
        "and it says where, which is the line somebody checks: {said}"
    );
}

/// **A push-back says which endpoint and what Instagram answered**, without
/// `--verbose`: the cause alone ("throttling") does not say what was refused,
/// and a run stops at its first push-back, so this one line is all there is.
#[tokio::test]
async fn a_push_back_names_its_endpoint_and_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "busy"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "30")
                .set_body_string(r#"{"message":"Please wait a few minutes","status":"fail"}"#),
        )
        .with_priority(1)
        .mount(&instagram)
        .await;

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["profile", "busy", "--no-interactive"],
    );
    let said = stderr(&out);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("Instagram pushed back"), "{said}");
    assert!(said.contains("/api/v1/users/web_profile_info/"), "{said}");
    assert!(said.contains("429"), "{said}");
    assert!(said.contains("retry_after=\"30\""), "{said}");
    assert!(said.contains("Please wait a few minutes"), "{said}");
    assert!(
        !said.contains("username=busy"),
        "only the path, as the query can carry a name: {said}"
    );
}

/// Under a JSON format, a push-back still ends in the one error object, as
/// the last line of standard error: the line naming the endpoint and the
/// answer comes before it, for a person reading the log.
#[tokio::test]
async fn a_push_back_in_json_ends_with_the_error_object() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "busy"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(r#"{"message":"Please wait a few minutes","status":"fail"}"#),
        )
        .with_priority(1)
        .mount(&instagram)
        .await;

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["profile", "busy", "--format", "json"],
    );
    let said = stderr(&out);
    assert_eq!(out.status.code(), Some(5), "{said}");
    assert!(said.contains("Instagram pushed back"), "{said}");
    let last = said
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let told: serde_json::Value =
        serde_json::from_str(last).expect("the last line of standard error is the error object");
    assert_eq!(told["error"]["code"], "rate_limited", "{said}");
    assert_eq!(told["error"]["exit"], 5, "{said}");
    assert_eq!(
        stdout(&out),
        "",
        "nothing on standard output to mistake for a result"
    );
}

/// **Logging the stored account in again does not ask its name again.**
///
/// A login hands over no username, so the first one asks
/// `/api/v1/users/{pk}/info/` — the request a real login met a 429 on, right
/// after the check. The second login of the same account already has the
/// name in the session it replaces.
#[tokio::test]
async fn logging_the_same_account_in_again_does_not_ask_its_name() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let asked = |requests: Vec<Request>| {
        requests
            .iter()
            .filter(|r| r.url.path() == format!("/api/v1/users/{PK}/info/"))
            .count()
    };

    log_in(tmp.path(), &instagram);
    let first = asked(instagram.received_requests().await.unwrap());
    assert_eq!(first, 1, "the first login learns the name");

    log_in(tmp.path(), &instagram);
    let second = asked(instagram.received_requests().await.unwrap());
    assert_eq!(second, first, "the second one already knew it");

    let out = snob(tmp.path(), Some(&instagram), &["whoami", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["username"], "me", "{said}");
}

/// `snob watch check` is a probe, and this is the whole of what it probes.
///
/// A mistake in the state matrix behind this command exits 0 just like a
/// healthy run, so it is driven here through the real binary: the exit code
/// is the entire point, and a monitoring system reads `$?` and nothing else.
#[tokio::test]
async fn the_probe_answers_for_a_machine_that_would_work() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let n8n = receiver(200).await;
    log_in(tmp.path(), &instagram);
    configure_webhook(tmp.path(), &n8n);

    let out = snob(tmp.path(), Some(&instagram), &["watch", "check", "--json"]);
    let said = json(&out);
    assert!(
        out.status.success(),
        "a machine that would run is not a failure: {said}\n{}",
        stderr(&out)
    );

    let checks = said["checks"].as_array().expect("there are checks");
    for what in ["schedule", "session", "account", "webhook"] {
        assert!(
            checks.iter().any(|c| c["what"] == what),
            "nothing checked the {what}: {said}"
        );
    }

    // And the receiver really was posted to, with the message that says it is
    // not a report.
    let sent = posted(&n8n).await;
    assert_eq!(sent.len(), 1, "the probe posts exactly one preflight");
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("the body is JSON");
    assert_eq!(body["event"], "watch.preflight", "{body}");
    assert_eq!(body["schema"], 1, "{body}");
    assert!(body["run"]["id"].as_str().is_some(), "{body}");
    assert!(body["run"]["at"].as_i64().is_some(), "{body}");
}

/// A run that has news posts it, signed, and says so on standard output.
///
/// The one path this whole branch is about, from the file on disk to the bytes
/// a receiver reads. The second run is what makes it a monitor rather than a
/// dump: the first lays the baseline and reports nothing, by design.
#[tokio::test]
async fn a_change_reaches_the_receiver_signed_and_only_once() {
    let tmp = tempfile::tempdir().unwrap();
    let n8n = receiver(200).await;
    let first = fake_instagram(3, 2).await;
    log_in(tmp.path(), &first);
    configure_webhook(tmp.path(), &n8n);

    let out = snob(tmp.path(), Some(&first), &["watch", "once"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        posted(&n8n).await.is_empty(),
        "the first run lays a baseline and has nothing to report"
    );

    // Somebody left, a day later.
    a_day_passes(tmp.path());
    let second = fake_instagram(2, 2).await;
    let out = snob(tmp.path(), Some(&second), &["watch", "once"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let sent = posted(&n8n).await;
    assert_eq!(sent.len(), 1, "a change is reported once");
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("the body is JSON");
    assert_eq!(body["event"], "watch.changes", "{body}");
    assert_eq!(body["counts"]["followers_lost"], 1, "{body}");
    assert_eq!(
        sent[0]
            .headers
            .get("x-snob-event")
            .map(|v| v.to_str().unwrap_or_default()),
        Some("watch.changes"),
        "the header has to agree with the body"
    );
    assert!(
        sent[0].headers.contains_key("x-snob-delivery"),
        "delivery is at-least-once, so a receiver needs the id to deduplicate on"
    );

    // A third run with nothing new sends nothing, which is the promise that
    // makes every message that arrives mean something.
    let out = snob(tmp.path(), Some(&second), &["watch", "once"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        posted(&n8n).await.len(),
        1,
        "nothing changed, so nothing is sent"
    );
}

/// A receiver that is down costs nothing, and the next run picks it up.
///
/// The queue is the reason a restarting receiver does not lose a change, and
/// this is the only test in the tree that watches a whole process fail to
/// deliver, exit, and a second process deliver the same bytes under the same
/// id.
#[tokio::test]
async fn a_report_a_receiver_refused_is_owed_and_then_delivered() {
    let tmp = tempfile::tempdir().unwrap();
    let down = receiver(500).await;
    let first = fake_instagram(3, 2).await;
    log_in(tmp.path(), &first);
    configure_webhook(tmp.path(), &down);

    snob(tmp.path(), Some(&first), &["watch", "once"]);
    a_day_passes(tmp.path());
    let second = fake_instagram(2, 2).await;
    let out = snob(tmp.path(), Some(&second), &["watch", "once"]);
    assert!(
        out.status.success(),
        "a receiver that is down does not fail the run: {}",
        stderr(&out)
    );
    let refused = posted(&down).await;
    assert_eq!(refused.len(), 1, "it was tried once and queued");

    let said = json(&snob(tmp.path(), None, &["watch", "status", "--json"]));
    assert_eq!(
        said["deliveries"]["waiting"], 1,
        "what is owed is what a run could post: {said}"
    );
    assert_eq!(
        said["deliveries"]["elsewhere"], 0,
        "and it is addressed here: {said}"
    );
    assert!(
        said.get("pending_deliveries").is_none(),
        "the queue is told once, under `deliveries`: {said}"
    );

    // A second receiver, on an address of its own.
    //
    // **Started before the first is dropped, and that ordering is the test.**
    // Taking `down` away first frees its port, and an operating system is
    // entitled to hand the very same one straight back — macOS does, reliably.
    // Both servers would then have one address, the queued row would be
    // addressed to it after all, and the assertion below would read
    // `waiting: 1` where it wants `elsewhere: 1`: a test about two addresses,
    // quietly run against one.
    let up = MockServer::start().await;
    drop(down);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&up)
        .await;
    configure_webhook(tmp.path(), &up);

    // A queued report belongs to the address it was addressed to, so pointing
    // somewhere else must not flush it -- which is what the count says.
    let said = json(&snob(tmp.path(), None, &["watch", "status", "--json"]));
    assert_eq!(
        said["deliveries"]["elsewhere"], 1,
        "a report made for one address is not owed to another: {said}"
    );
    assert_eq!(said["deliveries"]["waiting"], 0, "{said}");
    assert!(
        posted(&up).await.is_empty(),
        "and nothing was sent to the new address"
    );
}

/// A stranger's lists are not read on an answer nobody gave.
///
/// The domain rule, asserted of the program rather than of a function: an
/// unattended run refuses before it spends anything, and it says which command
/// fixes it. Written with the at sign, a spelling the lookup has to strip to
/// match anything.
#[tokio::test]
async fn an_unattended_run_refuses_a_stranger_nobody_answered_for() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    configure(
        tmp.path(),
        "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"@stranger\"\n",
    );

    let before = posted(&instagram).await.len();
    let out = snob(tmp.path(), Some(&instagram), &["watch", "once"]);
    assert!(!out.status.success(), "it has to refuse: {}", stdout(&out));

    let said = stderr(&out);
    assert!(
        said.contains("stranger"),
        "the refusal has to name the account: {said}"
    );
    assert_eq!(
        posted(&instagram).await.len(),
        before,
        "and it refuses before spending anything"
    );
}

/// The scheduled mode writes one line per tick, and a line jq can read.
///
/// The recipe the README offers is `snob watch --json >> events.ndjson`, and a
/// consumer of that file reads it a line at a time. One English sentence on
/// standard output ends the pipeline. Nothing drove the loop before this: the
/// two shapes of `json_line` are pinned by unit tests, but which mode gets
/// which, whether a tick prints at all, and what the loop says about the runs
/// it was not running for are all decided in the loop.
///
/// **Why the run log is edited.** The tightest schedule this tool accepts is
/// every fifteen minutes, so no schedule makes a fresh install tick inside a
/// test's patience: with an interval the first run is a whole interval away,
/// and with a calendar it is the next moment the grid names. Winding one
/// recorded run back an hour is what a machine that was switched off looks
/// like, and it is the one state that makes the loop work immediately -- so it
/// drives the missed-run fold at the same time, which nothing else does.
#[tokio::test]
async fn the_scheduled_stream_is_one_json_line_a_tick() {
    use std::io::BufRead;

    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    // One run, so there is a past; then an hour ago, so the grid has moments in
    // it that were missed.
    let first = snob(tmp.path(), Some(&instagram), &["watch", "once"]);
    assert!(first.status.success(), "{}", stderr(&first));
    {
        let db = snob_store::store::Store::open_at(&database(tmp.path()))
            .expect("the sandbox has a database by now");
        db.conn()
            .execute(
                "UPDATE watch_runs SET started_at = started_at - 3600,
                 finished_at = finished_at - 3600",
                [],
            )
            .expect("the run log is writable");
    }

    let mut child = command(tmp.path(), Some(&instagram))
        .args(["watch", "--cron", "*/15 * * * *", "--json", "--no-progress"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");

    // Read on another thread, because the loop does not end and a blocking read
    // here would hang the suite rather than fail it.
    let out = child.stdout.take().expect("stdout was piped");
    let (say, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if say.send(line).is_err() {
                return;
            }
        }
    });

    let first = heard.recv_timeout(std::time::Duration::from_secs(30));
    let _ = child.kill();
    let rest = child.wait_with_output().expect("the binary finishes");
    let said_aloud = String::from_utf8_lossy(&rest.stderr).to_string();

    let first =
        first.unwrap_or_else(|_| panic!("the loop wrote nothing in thirty seconds: {said_aloud}"));
    let said: serde_json::Value = serde_json::from_str(&first)
        .unwrap_or_else(|e| panic!("the stream wrote a line jq cannot read: {first:?} ({e})"));
    assert_eq!(said["schema"], 2, "{said}");
    assert!(
        said["run"]["at"].as_i64().is_some(),
        "a file like this is queried by time, so every line needs one: {said}"
    );
    assert!(
        said["run"]["lists"]
            .as_array()
            .is_some_and(|l| l.len() == 2),
        "and it says which lists it read, so a refusal is not a quiet run: {said}"
    );

    // The runs it was not running for are folded into this one, and the
    // sentence about them reads as a sentence.
    assert!(
        said_aloud.contains("missed while this was not running"),
        "a machine that was off has to be told what it missed: {said_aloud}"
    );
    assert!(
        !said_aloud.contains("1 scheduled runs were"),
        "and the sentence has to agree with its own number: {said_aloud}"
    );
}

/// A run that could not see says so, rather than saying nothing changed.
///
/// The distinction the whole `run.lists` field exists for, end to end. An
/// Instagram that answers nothing useful produces zeros in `counts` -- exactly
/// the zeros a quiet run produces -- so a receiver branching on
/// `counts.followers_lost` would call it a quiet morning. `run.looked` and
/// `run.lists` are what tell the two apart, and the exit code is what a timer
/// reads.
#[tokio::test]
async fn a_run_that_could_not_see_is_not_a_run_that_saw_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    configure(tmp.path(), "schema = 1\nevery = \"6h\"\n");

    let blind = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&blind)
        .await;

    let out = snob(tmp.path(), Some(&blind), &["watch", "once", "--json"]);
    let said: serde_json::Value = serde_json::from_str(&stdout(&out))
        .unwrap_or_else(|e| panic!("{:?} ({e})\n{}", stdout(&out), stderr(&out)));

    assert_eq!(
        said["counts"]["followers_lost"], 0,
        "a blind run and a quiet one have the same counts, which is the problem: {said}"
    );
    assert_eq!(
        said["run"]["looked"], false,
        "and this is what tells them apart: {said}"
    );
    let lists = said["run"]["lists"]
        .as_array()
        .expect("both lists are named");
    assert!(
        lists.iter().all(|l| !l["skipped"].is_null()),
        "neither list was read, and each has to say so: {said}"
    );
    assert!(
        !out.status.success(),
        "a run that saw nothing is not a run that succeeded: {said}"
    );
}

/// The first push-back is a hard stop, and a second process obeys it.
///
/// The most expensive rule this tool has, asserted across two processes, which
/// is the only way it can be: the cooldown is written to the database and the
/// point of it is that the *next* run reads it. A library test shares a
/// connection and an in-memory budget with the code it is testing, so it cannot
/// tell a cooldown that was honored from a cooldown that was merely recorded.
#[tokio::test]
async fn a_hard_stop_is_obeyed_by_the_process_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    configure(tmp.path(), "schema = 1\nevery = \"6h\"\n");

    let throttled = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"message":"","spam":true}"#))
        .mount(&throttled)
        .await;

    let refused = snob(tmp.path(), Some(&throttled), &["watch", "once", "--json"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    let said: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap_or_else(|e| {
        panic!(
            "a tick that failed still leaves a line: {:?} ({e})\n{}",
            stdout(&refused),
            stderr(&refused)
        )
    });
    let lists = said["run"]["lists"]
        .as_array()
        .expect("both lists are named");
    assert!(
        lists.iter().all(|l| !l["skipped"].is_null()),
        "a throttled run read nothing, and every list has to say so: {said}"
    );
    let spent = posted(&throttled).await.len();
    assert_eq!(spent, 1, "the first push-back is the last request");

    // A whole new process, reading the cooldown out of the database rather than
    // out of anything it remembers.
    let again = snob(tmp.path(), Some(&throttled), &["watch", "once"]);
    assert_eq!(
        again.status.code(),
        Some(5),
        "a cooldown is exit 5: {}",
        stderr(&again)
    );
    assert_eq!(
        posted(&throttled).await.len(),
        spent,
        "the run after a hard stop knocks again: {}",
        stderr(&again)
    );
}

/// A machine with nothing configured is not a machine that is broken, and a
/// machine whose schedule cannot be built is.
///
/// The two ends of the probe's verdict, through the real exit code. Both used
/// to be 0.
#[tokio::test]
async fn the_probe_tells_unconfigured_from_unstartable() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    let bare = snob(tmp.path(), Some(&instagram), &["watch", "check"]);
    assert!(
        bare.status.success(),
        "nothing configured is not broken: {}",
        stderr(&bare)
    );

    // Accepted by the parser -- it is TOML, the schema is right, no key clashes
    // -- and refused by the evaluator that actually decides when a run happens.
    configure(tmp.path(), "schema = 1\nevery = \"5m\"\n");
    let unstartable = snob(tmp.path(), Some(&instagram), &["watch", "check"]);
    assert!(
        !unstartable.status.success(),
        "a schedule no run can be built from stops every run: {}{}",
        stdout(&unstartable),
        stderr(&unstartable)
    );
}

/// A reel with one photo and one video, mounted on an existing fake Instagram.
///
/// Two candidate sizes on the photo so the listing has something to choose
/// wrongly: the client is told to take the largest, and taking the first is the
/// mistake that would otherwise pass every assertion.
async fn with_stories(server: &MockServer) {
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"reels_media":[{"items":[
                {"pk":"1","media_type":1,"taken_at":1000,"expiring_at":99999999999,
                 "image_versions2":{"candidates":[
                    {"url":"https://scontent.cdninstagram.com/small.jpg","width":320,"height":320},
                    {"url":"https://scontent.cdninstagram.com/big.jpg","width":1080,"height":1920}]}},
                {"pk":"2","media_type":2,"taken_at":2000,"expiring_at":99999999999,
                 "video_versions":[
                    {"url":"https://scontent.cdninstagram.com/clip.mp4","width":720,"height":1280}]}
            ]}]}"#,
        ))
        .mount(server)
        .await;
}

/// The listing numbers the stories, and those numbers are what `--download`
/// takes.
#[tokio::test]
async fn stories_are_listed_with_the_numbers_download_takes() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    let listed = json(&out);
    let stories = listed["stories"].as_array().expect("an array");
    assert_eq!(stories.len(), 2);
    assert_eq!(stories[0]["number"], 1);
    assert_eq!(stories[0]["kind"], "photo");
    assert_eq!(stories[1]["kind"], "video");
    assert_eq!(
        stories[0]["url"], "https://scontent.cdninstagram.com/big.jpg",
        "the largest candidate wins, not the first"
    );
}

/// **Nothing that would register a view goes out.** The whole reason the
/// command exists in the shape it does, asserted against what the server
/// actually received rather than against what the code appears to do.
#[tokio::test]
async fn listing_stories_sends_nothing_that_marks_them_seen() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);

    snob(tmp.path(), Some(&instagram), &["stories", "me"]);

    assert_nothing_marks_seen(&instagram, "stories").await;
}

/// Every request the server received was a GET to a path that does not say
/// "seen": reading `what` marks nothing as seen.
async fn assert_nothing_marks_seen(instagram: &MockServer, what: &str) {
    for request in posted(instagram).await {
        assert!(
            !request.url.path().contains("seen"),
            "a request went to {} while only reading {what}",
            request.url.path()
        );
        assert_eq!(
            request.method,
            wiremock::http::Method::GET,
            "reading {what} sent a {} to {}",
            request.method,
            request.url.path()
        );
    }
}

/// A number nobody has is refused by name rather than by panic, and an
/// out-of-range one does not wrap round to the last story.
///
/// Zero is refused earlier than nine, and differently: the value parser
/// answers it, where a typo costs nothing, so it is exit 2 with nothing
/// fetched, while nine parses and is refused against the tray.
#[tokio::test]
async fn a_story_number_nobody_has_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--download", "0"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "zero is a parse error, answered before anything was spent"
    );
    assert!(
        stderr(&out).contains("the listing starts at 1"),
        "{}",
        stderr(&out)
    );

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--download", "9"],
    );
    assert!(
        !out.status.success(),
        "story 9 should not have been downloaded"
    );
    assert!(
        stderr(&out).contains("there is no story"),
        "{}",
        stderr(&out)
    );

    // A set with one bad number downloads nothing: the refusal comes before
    // the first request, not after two good stories are already on disk.
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--download", "1,9"],
    );
    assert!(!out.status.success(), "1,9 should refuse as a whole");
    assert!(
        stderr(&out).contains("there is no story 9"),
        "{}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("Saved") && !stdout(&out).contains("Saved"),
        "nothing may be saved when part of the set is refused"
    );
}

/// A reel whose media the fake Instagram itself serves, so a download has
/// somewhere to go. The client downloads from the origin it was pointed at
/// as readily as from the CDN, which is what makes this reachable offline.
async fn with_downloadable_stories(server: &MockServer) {
    let base = server.uri();
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[
                {{"pk":"1","media_type":1,"taken_at":1000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/big.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"2","media_type":2,"taken_at":2000,"expiring_at":99999999999,
                 "video_versions":[
                    {{"url":"{base}/clip.mp4","width":720,"height":1280}}]}}
            ]}}]}}"#
        )))
        .mount(server)
        .await;
    serve_media(server).await;
}

/// A picture at `/big.jpg` and a clip at `/clip.mp4`, served by the fake
/// Instagram itself.
async fn serve_media(server: &MockServer) {
    Mock::given(method("GET"))
        .and(url_path("/big.jpg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8\xff\xe0 a picture".to_vec()),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/clip.mp4"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(b"\x00\x00\x00\x18ftypisom a clip".to_vec()),
        )
        .mount(server)
        .await;
}

/// A tray of two highlights whose media the fake Instagram itself serves,
/// with the items matched on `reel_ids` -- so an ask under the wrong id, or
/// under a bare one, finds no mock and fails loudly.
async fn with_downloadable_highlights(server: &MockServer) {
    let base = server.uri();
    Mock::given(method("GET"))
        .and(url_path(format!(
            "/api/v1/highlights/{PK}/highlights_tray/"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tray":[
                {"id":"highlight:100","title":"trip","media_count":2,
                 "created_at":1600000000,"updated_timestamp":1700000000},
                {"id":"highlight:200","title":"food","media_count":1}
            ],"status":"ok"}"#,
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", "highlight:100"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[
                {{"pk":"1","media_type":1,"taken_at":1600000000,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/big.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"2","media_type":2,"taken_at":1600001000,
                 "video_versions":[
                    {{"url":"{base}/clip.mp4","width":720,"height":1280}}]}}
            ]}}]}}"#
        )))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", "highlight:200"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[
                {{"pk":"3","media_type":1,"taken_at":1600002000,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/big.jpg","width":1080,"height":1920}}]}}}}
            ]}}]}}"#
        )))
        .mount(server)
        .await;
    serve_media(server).await;
}

/// The tray listing numbers the highlights, and those numbers are what the
/// second positional and a tray-level `-d` take.
#[tokio::test]
async fn the_tray_is_listed_with_the_numbers_the_command_takes() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["highlights", "me", "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    let listed = json(&out);
    let tray = listed["highlights"].as_array().expect("an array");
    assert_eq!(tray.len(), 2);
    assert_eq!(tray[0]["number"], 1);
    assert_eq!(tray[0]["title"], "trip");
    assert_eq!(tray[0]["items"], 2);
    assert_eq!(
        tray[0]["id"], "highlight:100",
        "the id keeps the tray's own spelling, the one the items are fetched with"
    );
    // The tray costs the profile and the tray, and opens no highlight: the
    // items of a listing nobody asked into are requests nobody asked for.
    let opened = posted(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path().contains("reels_media"))
        .count();
    assert_eq!(opened, 0, "listing the tray opened a highlight");
}

/// `snob highlights me 1` numbers the items, and `-d` inside takes those
/// numbers: the file lands under `me-<highlight>-<item>`, so the two indexes
/// a person read are the two in the name.
#[tokio::test]
async fn a_highlight_item_is_saved_under_both_numbers() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["highlights", "me", "1", "--download", "2"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        std::fs::read(here.join("me-1-2.mp4")).expect("item 2 of highlight 1, written here"),
        b"\x00\x00\x00\x18ftypisom a clip"
    );

    // Asked again, the file on disk answers: nothing is fetched and nothing
    // is replaced -- the story rule, holding across the second index.
    let again = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["highlights", "me", "1", "--download", "2"],
    );
    assert!(
        again.status.success(),
        "{}{}",
        stdout(&again),
        stderr(&again)
    );
    assert!(
        stderr(&again).contains("Already saved"),
        "{}",
        stderr(&again)
    );
}

/// Without an item number, `-d` takes a whole highlight by its tray number
/// and saves everything in it.
#[tokio::test]
async fn a_whole_highlight_is_saved_by_its_tray_number() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["highlights", "me", "--download", "1"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        std::fs::read(here.join("me-1-1.jpg")).expect("the photo"),
        b"\xff\xd8\xff\xe0 a picture"
    );
    assert_eq!(
        std::fs::read(here.join("me-1-2.mp4")).expect("the video"),
        b"\x00\x00\x00\x18ftypisom a clip"
    );
    assert!(
        !here.join("me-2-1.jpg").exists(),
        "highlight 2 was not asked for and must not be fetched"
    );
}

/// A tray number nobody has is refused by name, before anything is opened --
/// at both places a number is taken.
#[tokio::test]
async fn a_highlight_number_nobody_has_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);

    for args in [
        &["highlights", "me", "9"][..],
        &["highlights", "me", "--download", "9"][..],
    ] {
        let out = snob(tmp.path(), Some(&instagram), args);
        assert!(!out.status.success(), "{args:?} should have been refused");
        assert!(
            stderr(&out).contains("there is no highlight 9"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let opened = posted(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path().contains("reels_media"))
        .count();
    assert_eq!(opened, 0, "a refused number still opened a highlight");
}

/// An item number the entry does not hold is refused by its own sentence,
/// and no media is fetched for the part of the set that exists.
#[tokio::test]
async fn an_item_number_nobody_has_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["highlights", "me", "1", "-d", "1,9"],
    );
    assert!(!out.status.success(), "item 9 should have been refused");
    assert!(
        stderr(&out).contains("there is no item 9: highlight 1 of @me holds 2 items"),
        "{}",
        stderr(&out)
    );
    let media = posted(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path() == "/big.jpg" || r.url.path() == "/clip.mp4")
        .count();
    assert_eq!(media, 0, "a refused set still fetched media");
}

/// One number with `-o` is the single-download contract: `-o` names the
/// file, and naming it again replaces it, because the name was typed.
#[tokio::test]
async fn one_story_goes_to_the_file_that_was_named() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    for _ in 0..2 {
        let out = snob_from(
            &here,
            tmp.path(),
            &instagram,
            &["stories", "me", "-d", "1", "-o", "one.jpg"],
        );
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
        assert!(stderr(&out).contains("Saved one.jpg"), "{}", stderr(&out));
        assert!(!stderr(&out).contains("Already"), "{}", stderr(&out));
        assert_eq!(
            std::fs::read(here.join("one.jpg")).unwrap(),
            b"\xff\xd8\xff\xe0 a picture"
        );
        std::fs::write(here.join("one.jpg"), b"replaced by the next run").unwrap();
    }
    assert!(
        !here.join("me-1.jpg").exists(),
        "the story also landed under the listing's name"
    );
}

/// The same contract one level down, for an item of a highlight.
#[tokio::test]
async fn one_highlight_item_goes_to_the_file_that_was_named() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    for _ in 0..2 {
        let out = snob_from(
            &here,
            tmp.path(),
            &instagram,
            &["highlights", "me", "1", "-d", "2", "-o", "one.mp4"],
        );
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
        assert!(stderr(&out).contains("Saved one.mp4"), "{}", stderr(&out));
        assert!(!stderr(&out).contains("Already"), "{}", stderr(&out));
        assert_eq!(
            std::fs::read(here.join("one.mp4")).unwrap(),
            b"\x00\x00\x00\x18ftypisom a clip"
        );
        std::fs::write(here.join("one.mp4"), b"replaced by the next run").unwrap();
    }
    assert!(
        !here.join("me-1-2.mp4").exists(),
        "the item also landed under the listing's name"
    );
}

/// What one highlight holds, as JSON: the entry it is, then its items.
#[tokio::test]
async fn a_highlight_is_listed_with_the_entry_it_is() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["highlights", "me", "1", "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

    let listed = json(&out);
    let base = instagram.uri();
    assert_eq!(
        listed,
        serde_json::json!({
            "username": "me",
            "highlight": {"number": 1, "id": "highlight:100", "title": "trip"},
            "items": [
                {"number": 1, "kind": "photo", "taken_at": 1_600_000_000,
                 "mentions": [], "url": format!("{base}/big.jpg")},
                {"number": 2, "kind": "video", "taken_at": 1_600_001_000,
                 "mentions": [], "url": format!("{base}/clip.mp4")},
            ],
        })
    );
}

/// The four reads that name one account refuse during a cooldown, and spend
/// nothing to find out.
#[tokio::test]
async fn a_cooldown_stops_every_read_of_one_account() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    let throttled = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"message":"","spam":true}"#))
        .mount(&throttled)
        .await;
    let first = snob(tmp.path(), Some(&throttled), &["profile", "someone"]);
    assert!(!first.status.success(), "{}", stderr(&first));
    let spent = posted(&throttled).await.len();
    assert_eq!(spent, 1, "the push-back is the last request");

    for command in ["profile", "pfp", "stories", "highlights"] {
        let out = snob(tmp.path(), Some(&throttled), &[command, "someone"]);
        assert_eq!(
            out.status.code(),
            Some(5),
            "{command} during a cooldown: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("no request can be made"),
            "{command}: {}",
            stderr(&out)
        );
        assert_eq!(
            posted(&throttled).await.len(),
            spent,
            "{command} spent a request during a cooldown"
        );
    }

    // The refusal says when the cooldown lifts as a moment a program reads,
    // for a profile and for a list with nothing stored to serve, both.
    for args in [
        &["profile", "someone", "--format", "json"][..],
        &["followers", "--format", "json"][..],
    ] {
        let out = snob(tmp.path(), Some(&throttled), args);
        let said = stderr(&out);
        assert_eq!(out.status.code(), Some(5), "{args:?}: {said}");
        let last = said
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        let told: serde_json::Value = serde_json::from_str(last)
            .expect("the last line of standard error is the error object");
        assert_eq!(told["error"]["code"], "rate_limited", "{args:?}: {said}");
        assert!(
            told["error"]["cooldown_until"].as_i64().is_some(),
            "{args:?}: {said}"
        );
    }
    assert_eq!(posted(&throttled).await.len(), spent);
}

/// **Nothing that would register a view goes out**, walking the whole
/// feature: the tray, one highlight's items, and a download.
#[tokio::test]
async fn reading_highlights_sends_nothing_that_marks_them_seen() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    snob(tmp.path(), Some(&instagram), &["highlights", "me"]);
    snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["highlights", "me", "2", "-d", "all"],
    );

    assert_nothing_marks_seen(&instagram, "highlights").await;
}

/// The code of the post [`with_a_post`] serves, and its pk.
const POST_CODE: &str = "DTYHCKvDNxy";
const POST_PK: &str = "3807824420233075826";

/// A post of `me`: a carousel of a photo and a clip, whose files the fake
/// Instagram itself serves, and a page of its comments. The info read is
/// matched with no query at all, so a link's own query sent along finds no
/// mock and fails loudly.
async fn with_a_post(server: &MockServer) {
    let base = server.uri();
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/media/{POST_PK}/info/")))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"items":[{{"pk":"{POST_PK}","code":"{POST_CODE}","media_type":8,"taken_at":1790000000,
                "user":{{"username":"me"}},"caption":{{"text":"a day with @friend"}},
                "like_count":12,"comment_count":1,"top_likers":["close"],
                "carousel_media":[
                  {{"pk":"1","media_type":1,"image_versions2":{{"candidates":[
                     {{"url":"{base}/big.jpg","width":1080,"height":1350}}]}}}},
                  {{"pk":"2","media_type":2,"video_versions":[
                     {{"url":"{base}/clip.mp4","width":720,"height":1280}}]}}
                ]}}],"status":"ok"}}"#
        )))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/media/{POST_PK}/comments/")))
        .and(query_param("can_support_threading", "true"))
        .and(query_param("permalink_enabled", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"comments":[{"pk":"1","text":"lovely","created_at":1790000100,
                "user":{"username":"friend"},"comment_like_count":2,"child_comment_count":0}],
               "comment_count":1,"has_more_comments":false,"has_more_headload_comments":false,
               "status":"ok"}"#,
        ))
        .mount(server)
        .await;
    serve_media(server).await;
}

/// The requests the run sent about the post, as path, query and referrer.
async fn asked_about_the_post(server: &MockServer) -> Vec<(String, String, String)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path().starts_with("/api/v1/media/"))
        .map(|r| {
            (
                r.url.path().to_string(),
                r.url.query().unwrap_or_default().to_string(),
                r.headers
                    .get("referer")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

/// A link copied from the app is read for its code: the post is its info and
/// its comments, asked with nothing of the link's own query, from the page it
/// opens on, and `-d all` saves every item under `<owner>-<code>-<n>`.
#[tokio::test]
async fn a_post_is_read_by_its_link_and_saved_under_its_code() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_a_post(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let link =
        format!("https://www.instagram.com/p/{POST_CODE}/?utm_source=ig_web_copy_link&igsh=x");
    let out = snob_from(&here, tmp.path(), &instagram, &["post", &link, "-d", "all"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        std::fs::read(here.join(format!("me-{POST_CODE}-1.jpg"))).expect("the photo, here"),
        b"\xff\xd8\xff\xe0 a picture"
    );
    assert_eq!(
        std::fs::read(here.join(format!("me-{POST_CODE}-2.mp4"))).expect("the clip, here"),
        b"\x00\x00\x00\x18ftypisom a clip"
    );
    let asked = asked_about_the_post(&instagram).await;
    assert_eq!(asked.len(), 2, "the info and its comments: {asked:?}");
    assert_eq!(asked[0].0, format!("/api/v1/media/{POST_PK}/info/"));
    assert_eq!(asked[0].1, "", "nothing of the link's query is sent");
    assert_eq!(asked[1].0, format!("/api/v1/media/{POST_PK}/comments/"));
    assert!(
        asked[0].2.ends_with(&format!("/p/{POST_CODE}/")),
        "{}",
        asked[0].2
    );

    // Asked again, the files on disk answer.
    let again = snob_from(&here, tmp.path(), &instagram, &["post", &link, "-d", "2"]);
    assert!(
        again.status.success(),
        "{}{}",
        stdout(&again),
        stderr(&again)
    );
    assert!(
        stderr(&again).contains("Already saved"),
        "{}",
        stderr(&again)
    );
}

/// `snob reel` is the same command, and the listing always carries the
/// first page of comments, asked as the app asks it, from the reel's page.
#[tokio::test]
async fn a_reel_is_listed_with_what_it_says_and_its_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_a_post(&instagram).await;
    log_in(tmp.path(), &instagram);

    let link = format!("instagram.com/reel/{POST_CODE}");
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["reel", &link, "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let listed = json(&out);
    assert_eq!(listed["post"]["code"], POST_CODE);
    assert_eq!(listed["post"]["owner"], "me");
    assert_eq!(listed["post"]["mentions"][0], "friend");
    assert_eq!(listed["post"]["liked_by"]["name"], "close");
    assert_eq!(listed["post"]["liked_by"]["others"], 11);
    assert_eq!(listed["post"]["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(listed["comments"][0]["author"], "friend");
    assert_eq!(listed["more_comments"], false);
    let asked = asked_about_the_post(&instagram).await;
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(
        asked
            .iter()
            .all(|(_, _, referer)| referer.ends_with(&format!("/reel/{POST_CODE}/"))),
        "{asked:?}"
    );

    let table = snob(
        tmp.path(),
        Some(&instagram),
        &["post", POST_CODE, "--format", "table"],
    );
    assert!(table.status.success(), "{}", stderr(&table));
    let text = stdout(&table);
    assert!(text.contains("@me · 2 items"), "{text}");
    assert!(text.contains("Mentions: @friend"), "{text}");
    assert!(text.contains("Liked by close and 11 others"), "{text}");
}

/// An address that names no post is refused before anything is asked.
#[tokio::test]
async fn a_link_that_names_no_post_costs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let before = instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    for link in [
        "https://example.com/p/DTYHCKvDNxy/",
        "https://www.instagram.com/someone/",
    ] {
        let out = snob(tmp.path(), Some(&instagram), &["post", link]);
        assert_eq!(out.status.code(), Some(1), "{link}: {}", stderr(&out));
    }
    let after = instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    assert_eq!(before, after, "nothing was asked");
}

/// The binary, run from a directory of the test's choosing -- which is where a
/// story lands when `-o` says nothing.
fn snob_from(cwd: &Path, root: &Path, instagram: &MockServer, args: &[&str]) -> Output {
    command(root, Some(instagram))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("the binary runs")
}

/// The whole tool, end to end, in the order a person meets it.
///
/// One session, one fake Instagram, every read command the binary has, each
/// asserted on what it printed, what it wrote and what it exited with -- and
/// then the two commands that take it all away again. The other tests here
/// each pin one behavior; this one pins that the commands agree with each
/// other over one store: the crossings add up to the lists, the export on
/// disk is the list on screen, and `purge` leaves nothing of any of it.
///
/// The fixture serves the same made-up accounts for both lists: followers
/// `user0..user2`, following `user0..user1`. So `fans` is `user2`, `friends`
/// is `user0` and `user1`, and nobody is an unfollower.
/// A walk asked to stop by the system, the way `systemctl stop` or a closed
/// terminal asks, stops the way a Ctrl+C stops it: promptly, mid-request, and
/// with the code an interrupted run exits with, rather than being killed where
/// it stood.
#[cfg(unix)]
#[tokio::test]
async fn a_walk_asked_to_stop_by_the_system_stops_as_for_ctrl_c() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    // The list never answers, so the walk is mid-request when it is asked.
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(120)))
        .with_priority(1)
        .mount(&instagram)
        .await;
    let walks = || async {
        instagram
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with("/followers/"))
            .count()
    };

    for signal in [libc::SIGTERM, libc::SIGHUP] {
        let before = walks().await;
        let mut child = command(tmp.path(), Some(&instagram))
            .arg("followers")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the binary runs");
        let asked = std::time::Instant::now();
        while walks().await == before {
            assert!(
                asked.elapsed() < std::time::Duration::from_secs(30),
                "the walk never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        // SAFETY: `pid` is this test's own child, still running: it has not
        // been waited for.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);

        let stopped = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                stopped.elapsed() < std::time::Duration::from_secs(15),
                "signal {signal} did not stop the walk"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert_eq!(status.code(), Some(130), "signal {signal}");
    }
}

/// With `--stop-when-stdin-closes`, the parent closing the pipe stops a walk
/// mid-request the way a signal does, on every platform: the one way a parent
/// on Windows has to stop it in order rather than kill it.
#[tokio::test]
async fn a_walk_stops_as_for_ctrl_c_when_its_stdin_closes() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(120)))
        .with_priority(1)
        .mount(&instagram)
        .await;
    let walks = || async {
        instagram
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with("/followers/"))
            .count()
    };

    let mut child = command(tmp.path(), Some(&instagram))
        .args(["followers", "--stop-when-stdin-closes"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the binary runs");
    let asked = std::time::Instant::now();
    while walks().await == 0 {
        assert!(
            asked.elapsed() < std::time::Duration::from_secs(30),
            "the walk never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    drop(child.stdin.take());

    let stopped = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            stopped.elapsed() < std::time::Duration::from_secs(15),
            "closing standard input did not stop the walk"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(status.code(), Some(130));
}

#[tokio::test]
async fn the_whole_tool_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let here = here(&tmp);
    let run = |args: &[&str]| snob_from(&here, tmp.path(), &instagram, args);
    let names = |value: &serde_json::Value| -> Vec<String> {
        value
            .as_array()
            .expect("a list")
            .iter()
            .map(|u| u["username"].as_str().unwrap().to_string())
            .collect()
    };

    // Nothing yet: every command that needs a session says so, with the code.
    let out = run(&["followers"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));

    log_in(tmp.path(), &instagram);

    let out = run(&["whoami", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json(&out)["username"], "me");

    // The two lists, walked. Down a pipe the format is JSON on its own.
    let out = run(&["followers"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user0", "user1", "user2"]);
    let out = run(&["following"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user0", "user1"]);

    // The crossings, served out of what was just stored. The arithmetic is
    // the one AGENTS.md says a test asserts: unfollowers + friends is
    // everyone you follow, fans + friends everyone who follows you.
    let out = run(&["unfollowers", "--cache"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(names(&json(&out)).is_empty(), "{}", stdout(&out));
    let out = run(&["fans", "--cache"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user2"]);
    let out = run(&["friends", "--cache"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(names(&json(&out)), ["user0", "user1"]);

    let out = run(&["scan", "--cache", "--format", "json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let scan = json(&out);
    assert_eq!(scan["counts"]["followers"], 3, "{scan}");
    assert_eq!(scan["counts"]["following"], 2, "{scan}");
    assert_eq!(scan["counts"]["fans"], 1, "{scan}");
    assert_eq!(scan["counts"]["friends"], 2, "{scan}");
    assert_eq!(scan["counts"]["unfollowers"], 0, "{scan}");
    assert_eq!(scan["lists"]["followers"]["source"], "cached", "{scan}");

    // The filters, on the list with the most in it.
    let out = run(&["followers", "--cache", "--limit", "1"]);
    assert_eq!(names(&json(&out)), ["user0"]);
    std::fs::write(here.join("skip.txt"), "@ user1\n# a comment\nUSER0\n").unwrap();
    let out = run(&["followers", "--cache", "--exclude-list", "skip.txt"]);
    assert_eq!(names(&json(&out)), ["user2"], "{}", stderr(&out));

    // Every export format, to a file whose extension chooses it.
    for (file, expect) in [
        ("list.csv", "user0"),
        ("list.md", "| [user0](https://www.instagram.com/user0/)"),
        ("list.ndjson", "\"username\":\"user0\""),
        ("list.json", "\"username\": \"user0\""),
    ] {
        let out = run(&["followers", "--cache", "-o", file]);
        assert!(out.status.success(), "{file}: {}", stderr(&out));
        let written = std::fs::read_to_string(here.join(file)).expect(file);
        assert!(written.contains(expect), "{file}: {written}");
        assert_eq!(
            stdout(&out),
            "",
            "{file}: the result went to the file, not the screen"
        );
    }
    let out = run(&["followers", "--cache", "-o", "list.xlsx"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let workbook = std::fs::read(here.join("list.xlsx")).unwrap();
    assert_eq!(&workbook[..2], b"PK", "an xlsx is a zip");
    // A name this program did not choose is not written over twice: the
    // user named it, so the second run replaces it without complaint.
    assert!(
        run(&["followers", "--cache", "-o", "list.csv"])
            .status
            .success()
    );

    // The session, taken away, and the command that needed it says so again.
    let out = run(&["logout"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Session deleted"), "{}", stdout(&out));
    let out = run(&["followers", "--cache"]);
    assert_eq!(out.status.code(), Some(3));

    // And everything else: shown first, then removed, only on --yes.
    let out = run(&["purge", "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Nothing was deleted"),
        "{}",
        stdout(&out)
    );
    assert!(
        tmp.path().join("data").is_dir(),
        "a dry run deletes nothing"
    );
    let out = run(&["purge"]);
    assert_eq!(
        out.status.code(),
        Some(130),
        "a question nobody could answer is not a yes: {}",
        stderr(&out)
    );
    assert!(tmp.path().join("data").is_dir());
    let out = run(&["purge", "--yes"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        !tmp.path().join("data").exists(),
        "purge left the data directory behind"
    );
    // The exports are the user's, in the user's directory, and stay.
    assert!(here.join("list.csv").is_file());
}

/// A failure is told in the language the answer was going to be in.
///
/// `snob followers --format json` against nothing stored exits 3 with a JSON
/// object on standard error, so a program gets the code, the message and the
/// hint (and the address a challenge carries) rather than an English sentence
/// it cannot parse. The same run asked for prose gets the prose, with the
/// advice set apart as a hint.
#[test]
fn a_failure_is_json_when_the_answer_would_have_been() {
    let tmp = tempfile::tempdir().unwrap();

    let out = snob(tmp.path(), None, &["followers", "--format", "json"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let told: serde_json::Value =
        serde_json::from_str(stderr(&out).trim()).expect("standard error is one JSON object");
    assert_eq!(told["error"]["code"], "no_session");
    assert_eq!(told["error"]["exit"], 3);
    assert_eq!(told["error"]["message"], "no session is stored");
    assert_eq!(told["error"]["hint"], "run \"snob login\"");
    assert_eq!(
        stdout(&out),
        "",
        "nothing on standard output to mistake for a result"
    );

    let out = snob(tmp.path(), None, &["followers", "--format", "md"]);
    assert_eq!(out.status.code(), Some(3));
    let said = stderr(&out);
    assert!(said.contains("error: no session is stored"), "{said}");
    assert!(said.contains("hint:  run \"snob login\""), "{said}");
}

/// With no other account's to keep, `snob logout` takes every browser
/// profile, a login's own and a move that did not finish: each can hold a
/// live session.
#[test]
fn logout_removes_every_browser_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let profiles = data.join("browser-profile");
    let seeded = [
        profiles.join("42").join("Default").join("Cookies"),
        profiles.join("43").join("Local State"),
        profiles.join("login-7").join("Local State"),
        data.join("browser-profile.moving").join("Local State"),
    ];
    for file in &seeded {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"x").unwrap();
    }

    let out = snob(tmp.path(), None, &["logout"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Browser profiles deleted"),
        "{}",
        stdout(&out)
    );
    assert!(!profiles.exists());
    assert!(!data.join("browser-profile.moving").exists());
}

/// A reader that leaves early is not an error of this program's.
///
/// `println!` panics on a closed pipe, and with `panic = "abort"` a release
/// binary would die with a message and whatever status an abort gets the moment
/// `head` had read its line, so the prose has to go through a writer that
/// tolerates it, as the lists do.
#[test]
fn a_closed_pipe_ends_the_output_and_not_the_program() {
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    let mut child = command(tmp.path(), None)
        .args(["watch", "status"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    // Close the reading end before the program has written anything.
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("the binary finishes");

    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!said.contains("panicked"), "{said}");
    assert!(
        out.status.code().is_some_and(|code| code <= 1),
        "a closed pipe exited {:?}: {said}",
        out.status
    );
}

/// **A story can actually be saved.** The name this command invents carries
/// a hyphen, so the allowlist the name is checked against has to take one, or
/// every `--download` fetches the bytes and then refuses its own name. Only
/// driving the command to the write shows that.
#[tokio::test]
async fn a_story_is_downloaded_under_the_name_the_listing_implies() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--download", "1"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        std::fs::read(here.join("me-1.jpg")).expect("the story was written where the command ran"),
        b"\xff\xd8\xff\xe0 a picture"
    );

    // A second download of the same story neither replaces the file -- the
    // name was invented here, not typed -- nor fetches it: the look at the
    // directory comes before the request, so the CDN sees nothing.
    let fetched_before = instagram
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/big.jpg")
        .count();
    let again = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--download", "1"],
    );
    assert!(
        again.status.success(),
        "{}{}",
        stdout(&again),
        stderr(&again)
    );
    assert!(
        stderr(&again).contains("Already saved"),
        "{}",
        stderr(&again)
    );
    let fetched_after = instagram
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/big.jpg")
        .count();
    assert_eq!(
        fetched_before, fetched_after,
        "the story was fetched again for a file already on disk"
    );
    assert_eq!(
        std::fs::read(here.join("me-1.jpg")).unwrap(),
        b"\xff\xd8\xff\xe0 a picture",
        "the file on disk was left alone"
    );
}

/// A download that stopped halfway leaves a `.part`, never a truncated file
/// under the real name -- which the look-before-fetch above would otherwise
/// take for a finished one. The next ask for the same story sweeps one old
/// enough to be abandoned; it is removed **by age**, not blindly, because a
/// fresh `.part` may be a sibling process still streaming -- a blind remove
/// would let two runs publish each other's truncated bytes.
#[tokio::test]
async fn a_leftover_part_file_is_replaced_and_never_taken_for_the_story() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);
    let leftover = here.join("me-1.part");
    std::fs::write(&leftover, b"half of").unwrap();
    // Aged past ABANDONED_AFTER, the way a killed run's leftover really is by
    // the time somebody asks again. A fresh one is deliberately left alone.
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(&leftover)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(stale))
        .unwrap();

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--download", "1"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        !here.join("me-1.part").exists(),
        "the leftover was not cleaned up"
    );
    assert_eq!(
        std::fs::read(here.join("me-1.jpg")).unwrap(),
        b"\xff\xd8\xff\xe0 a picture"
    );
    assert!(
        stderr(&out).contains("Saved ") && !stderr(&out).contains("Already"),
        "{}",
        stderr(&out)
    );
}

/// `--all -o somewhere` puts every story in `somewhere`, and not in the
/// working directory next to an empty folder of that name.
#[tokio::test]
async fn all_stories_land_in_the_directory_that_was_named() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--all", "-o", "saved"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(here.join("saved").join("me-1.jpg").is_file());
    assert!(here.join("saved").join("me-2.mp4").is_file());
    assert!(
        !here.join("me-1.jpg").exists(),
        "a story leaked into the working directory"
    );

    // Run again into the same directory: both are already there, both are
    // said to be, nothing is fetched, and the run is a success -- a second
    // `-d all` is the ordinary way of asking "anything new?", and must not
    // download the whole tray again only to refuse every name.
    let fetched_before = instagram.received_requests().await.unwrap().len();
    let again = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "--all", "-o", "saved"],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    let said = stderr(&again);
    assert_eq!(
        said.matches("Already saved").count(),
        2,
        "both stories should be reported as already saved: {said}"
    );
    let fetched_after = instagram.received_requests().await.unwrap().len();
    // The tray listing itself is one request; the two media files are none.
    assert!(
        fetched_after - fetched_before <= 2,
        "media was fetched again for files already on disk: {} requests",
        fetched_after - fetched_before
    );
    assert!(
        !instagram
            .received_requests()
            .await
            .unwrap()
            .iter()
            .skip(fetched_before)
            .any(|r| r.url.path() == "/big.jpg" || r.url.path() == "/clip.mp4"),
        "a media URL was requested on the second run"
    );
}

/// Stories download several at a time, and the report still reads in the
/// listing's order: one that the CDN refuses is named by its number among the
/// ones that were saved, not by whichever finished first.
#[tokio::test]
async fn a_failed_story_is_reported_by_number_among_the_saved_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let base = instagram.uri();
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"reels_media":[{{"items":[
                {{"pk":"1","media_type":1,"taken_at":1000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/one.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"2","media_type":1,"taken_at":2000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/gone.jpg","width":1080,"height":1920}}]}}}},
                {{"pk":"3","media_type":1,"taken_at":3000,"expiring_at":99999999999,
                 "image_versions2":{{"candidates":[
                    {{"url":"{base}/three.jpg","width":1080,"height":1920}}]}}}}
            ]}}]}}"#
        )))
        .mount(&instagram)
        .await;
    for name in ["/one.jpg", "/three.jpg"] {
        Mock::given(method("GET"))
            .and(url_path(name))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"\xff\xd8\xff\xe0 a picture".to_vec()),
            )
            .mount(&instagram)
            .await;
    }
    Mock::given(method("GET"))
        .and(url_path("/gone.jpg"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&instagram)
        .await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "-d", "all"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(here.join("me-1.jpg").is_file(), "{said}");
    assert!(here.join("me-3.jpg").is_file(), "{said}");
    assert!(!here.join("me-2.jpg").exists());
    assert!(
        !here.join("me-2.part").exists(),
        "a refused download left its partial behind: {said}"
    );
    assert!(
        said.contains("1 of 3 stories could not be downloaded"),
        "{said}"
    );
    // Down a pipe the failure is the JSON shape, so the newline is escaped.
    assert!(
        said.contains("\\n2: ") || said.contains("\n2: "),
        "the failure is named by number: {said}"
    );
    assert!(
        said.contains("Saved 1: ") && said.contains("Saved 3: "),
        "{said}"
    );
}

/// The same sandbox login, with a CSRF token, which is what
/// `snob login --browser` produces and what a write needs.
fn log_in_writing(root: &Path, instagram: &MockServer) {
    let out = paste(root, instagram, SESSIONID, &["--csrftoken", "SANDBOXTOKEN"]);
    assert!(
        out.status.success(),
        "the sandbox could not log in to write: {}{}",
        stdout(&out),
        stderr(&out)
    );
}

/// **A session that cannot write refuses before it spends anything.**
///
/// `log_in` pastes a sessionid and nothing else, which is exactly the session
/// `snob login --paste` produces, so this is the ordinary case rather than a
/// contrived one. The assertion that matters is the second: no request was
/// made, so no budget was charged and nothing reached Instagram.
#[tokio::test]
async fn a_write_without_a_csrf_token_never_reaches_instagram() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let before = posted(&instagram).await.len();

    let out = snob(tmp.path(), Some(&instagram), &["unfollow", "someone", "-y"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("CSRF"), "{}", stderr(&out));
    assert_eq!(
        posted(&instagram).await.len(),
        before,
        "the refusal must happen before any request goes out"
    );
}

/// With no terminal and no -y, a write is not made and the exit code says who
/// decided: 130, stopped by the user, rather than 1, failed.
#[tokio::test]
async fn a_write_nobody_could_confirm_is_not_made() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "someone"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"9001","username":"someone","followed_by_viewer":false}}}"#,
        ))
        // Ahead of the catch-all mounted by `fake_instagram`, which answers for
        // any username with the session's own account. At equal priority
        // wiremock takes the first matching mount, and that one is first.
        .with_priority(1)
        .mount(&instagram)
        .await;
    log_in_writing(tmp.path(), &instagram);

    let out = snob(tmp.path(), Some(&instagram), &["follow", "someone"]);
    assert_eq!(
        out.status.code(),
        Some(130),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    // Asked for JSON, the refusal is the JSON error a program can read.
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["follow", "someone", "--json"],
    );
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    let told: serde_json::Value =
        serde_json::from_str(stderr(&out).lines().last().unwrap_or_default())
            .expect("the last line of standard error is JSON");
    assert_eq!(told["error"]["code"], "interrupted", "{told}");
    assert!(
        posted(&instagram)
            .await
            .iter()
            .all(|r| r.method != wiremock::http::Method::POST),
        "nothing may be sent without an answer"
    );
}

/// A confirmed unfollow sends one POST, to the right path, with the token.
#[tokio::test]
async fn a_confirmed_unfollow_sends_one_post() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    serve_an_unfollow(&instagram).await;
    log_in_writing(tmp.path(), &instagram);
    common::remember_doc_ids(tmp.path());

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollow", "someone", "-y", "--json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    // `--json` says what happened in a word, on standard output.
    let told = json(&out);
    assert_eq!(told["action"], "unfollow", "{told}");
    assert_eq!(told["outcome"], "unfollowed", "{told}");
    assert_eq!(told["changed"], true, "{told}");
    assert_eq!(told["target"]["username"], "someone", "{told}");

    let writes: Vec<_> = posted(&instagram)
        .await
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(writes.len(), 1, "one account, one write");
    assert_eq!(writes[0].url.path(), "/api/graphql");
    assert_eq!(
        writes[0].headers.get("x-csrftoken").unwrap(),
        "SANDBOXTOKEN"
    );
    let sent = String::from_utf8_lossy(&writes[0].body);
    assert!(
        sent.contains("fb_api_req_friendly_name=usePolarisUnfollowMutation"),
        "{sent}"
    );
    // The tokens really came off the page the run fetched, rather than from
    // anywhere else.
    assert!(sent.contains("fb_dtsg=DTSG"), "{sent}");
    assert!(sent.contains("target_user_id"), "{sent}");
}

/// A relationship that already holds costs no request at all, which matters
/// when a write slot is a quarter of an hour.
#[tokio::test]
async fn unfollowing_somebody_you_do_not_follow_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "stranger"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            // Both facts said, on purpose: an absent `requested_by_viewer` is
            // unknown, and unknown sends the request rather than assuming --
            // there may be a pending request to withdraw. Only the fully
            // known "not following, nothing pending" costs no request.
            r#"{"data":{"user":{"id":"9002","username":"stranger","followed_by_viewer":false,"requested_by_viewer":false}}}"#,
        ))
        // Ahead of the catch-all mounted by `fake_instagram`, which answers for
        // any username with the session's own account. At equal priority
        // wiremock takes the first matching mount, and that one is first.
        .with_priority(1)
        .mount(&instagram)
        .await;
    log_in_writing(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollow", "stranger", "-y"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("do not follow"), "{}", stderr(&out));

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollow", "stranger", "-y", "--json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let told = json(&out);
    assert_eq!(told["outcome"], "not_following", "{told}");
    assert_eq!(told["changed"], false, "{told}");

    assert!(
        posted(&instagram)
            .await
            .iter()
            .all(|r| r.method != wiremock::http::Method::POST),
        "nothing needed changing, so nothing should have been sent"
    );
}

/// An empty story tray is an empty document in JSON, not an empty stream: a
/// script reading the pipe gets something to parse.
#[tokio::test]
async fn an_empty_story_tray_prints_its_empty_document() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"reels_media":[]}"#))
        .mount(&instagram)
        .await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["stories", "me", "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let listed = json(&out);
    assert_eq!(listed["username"], "me");
    assert_eq!(listed["stories"], serde_json::json!([]));
}

/// A tray the account keeps back is `null` in JSON, as `profile` writes it:
/// not "has none", and not an empty stream either.
#[tokio::test]
async fn a_hidden_highlight_tray_is_null_in_json() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "stranger"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"9002","username":"stranger","is_private":true,"followed_by_viewer":false}}}"#,
        ))
        // Ahead of the catch-all `fake_instagram` mounts for any username.
        .with_priority(1)
        .mount(&instagram)
        .await;
    log_in(tmp.path(), &instagram);

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["highlights", "stranger", "--format", "json"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let listed = json(&out);
    assert_eq!(listed["username"], "stranger");
    assert!(listed["highlights"].is_null(), "{listed}");
}

/// A refusal on one highlight answers the ones after it too: the downloads
/// stop there, every entry not saved is counted, and the exit code is the
/// refusal's own, so a script can tell a dead session from a failed file.
#[tokio::test]
async fn a_refusal_stops_the_highlight_downloads_with_its_own_code() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    Mock::given(method("GET"))
        .and(url_path(format!(
            "/api/v1/highlights/{PK}/highlights_tray/"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"tray":[
                {"id":"highlight:100","title":"a","media_count":1},
                {"id":"highlight:200","title":"b","media_count":1},
                {"id":"highlight:300","title":"c","media_count":1}
            ],"status":"ok"}"#,
        ))
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/feed/reels_media/"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string(r#"{"message":"login_required","status":"fail"}"#),
        )
        .mount(&instagram)
        .await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["highlights", "me", "-d", "all"],
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let said = stderr(&out);
    assert!(said.contains("3 of 3 highlights"), "{said}");
    assert!(said.contains("not tried after that: 2, 3"), "{said}");
    let opened = posted(&instagram)
        .await
        .iter()
        .filter(|r| r.url.path().contains("reels_media"))
        .count();
    assert_eq!(
        opened, 1,
        "every entry after the refusal was asked for anyway"
    );
}

/// A machine from before several accounts could be signed in is moved under
/// its account by the first command that runs, and that command then acts as
/// it.
#[tokio::test]
async fn an_old_layout_moves_under_its_account() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let first = snob(
        tmp.path(),
        Some(&instagram),
        &["followers", "--format", "json"],
    );
    assert!(first.status.success(), "{}", stderr(&first));

    // Put back the way an older snob left it: one database and one session
    // file in the data directory, and no list of accounts.
    let data = tmp.path().join("data");
    let mine = snob_store::paths::AppPaths::rooted_at(tmp.path()).account(PK);
    for name in ["snob.db", "snob.db-wal", "snob.db-shm", "session.json"] {
        let from = mine.dir().join(name);
        if from.exists() {
            std::fs::rename(&from, data.join(name)).unwrap();
        }
    }
    std::fs::remove_file(data.join("accounts.toml")).unwrap();

    let out = snob(tmp.path(), None, &["whoami", "--offline", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["pk"], PK.get(), "{said}");
    assert_eq!(
        said["storage_path"].as_str().map(Path::new),
        Some(mine.session_file().as_path())
    );
    assert!(!data.join("snob.db").exists() && !data.join("session.json").exists());
    assert!(mine.db_file().exists());

    // What was stored came with it: the list answers out of storage.
    let out = snob(
        tmp.path(),
        None,
        &["followers", "--offline", "--format", "json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

/// `--account` names the account a command acts as, then `SNOB_ACCOUNT`; a
/// name nobody signed in as is a mistake, and nobody signed in at all is no
/// session.
#[tokio::test]
async fn the_account_is_named_by_the_flag_or_the_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;

    let nobody = snob(tmp.path(), None, &["followers", "--account", "me"]);
    assert_eq!(nobody.status.code(), Some(3), "{}", stderr(&nobody));

    log_in(tmp.path(), &instagram);
    let whoami = |env: &[(&str, &str)], args: &[&str]| {
        let mut all = vec!["whoami", "--offline", "--json"];
        all.extend_from_slice(args);
        snob_with(tmp.path(), None, env, &all)
    };

    for (env, args) in [
        (&[][..], &["--account", "42"][..]),
        (&[][..], &["--account", "ME"][..]),
        (&[("SNOB_ACCOUNT", "me")][..], &[][..]),
        (&[("SNOB_ACCOUNT", "nobody")][..], &["--account", "me"][..]),
    ] {
        let out = whoami(env, args);
        assert!(out.status.success(), "{env:?} {args:?}: {}", stderr(&out));
        let said = json(&out);
        assert_eq!(said["pk"], PK.get(), "{said}");
    }

    for (env, args) in [
        (&[][..], &["--account", "nobody"][..]),
        (&[("SNOB_ACCOUNT", "nobody")][..], &[][..]),
    ] {
        let out = whoami(env, args);
        assert_eq!(out.status.code(), Some(1), "{env:?} {args:?}");
        let told: serde_json::Value = serde_json::from_str(stderr(&out).trim())
            .expect("the refusal is JSON, as the answer would have been");
        assert!(
            told["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("@nobody")),
            "{told}"
        );
    }
}

/// Push-backs on two accounts within the hour stop every account, this one
/// included: nothing is spent, the refusal names both and exits 5, and
/// `whoami` says the same.
#[tokio::test]
async fn push_backs_on_two_accounts_stop_every_account() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);

    const OTHER: Pk = Pk::new(7);
    const THIRD: Pk = Pk::new(8);
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    snob_store::registry::Registry::update(&paths, |registry| {
        registry.upsert(OTHER, "other", snob_core::Epoch::new(1_700_000_000));
        registry.upsert(THIRD, "third", snob_core::Epoch::new(1_700_000_000));
    })
    .unwrap();
    let now = snob_core::clock::now_ms();
    let hour = std::time::Duration::from_secs(3600);
    {
        let shared = snob_store::store::shared::Shared::open(&paths).unwrap();
        shared.record(OTHER, now, now + hour, "429").unwrap();
        shared.record(THIRD, now, now + 2 * hour, "429").unwrap();
    }
    let asked = instagram.received_requests().await.unwrap().len();

    let out = snob(tmp.path(), Some(&instagram), &["followers"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("every account is paused until"), "{said}");
    assert!(said.contains("@other and @third"), "{said}");

    let out = snob(tmp.path(), Some(&instagram), &["whoami"]);
    let said = stderr(&out);
    assert!(said.contains("Every account is paused until"), "{said}");

    assert_eq!(
        instagram.received_requests().await.unwrap().len(),
        asked,
        "nothing is sent while the brake stands"
    );
}

/// `account list` shows every account signed in here and which is active;
/// `account use` moves the mark, says from where, and warns when the account
/// it moved to has no session to act with.
#[tokio::test]
async fn the_active_account_is_listed_and_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;

    let empty = snob(tmp.path(), None, &["account", "list", "--json"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    let said = json(&empty);
    assert_eq!(said["accounts"], serde_json::json!([]), "{said}");

    log_in(tmp.path(), &instagram);
    // A second account, signed in once and since without a session.
    const SIGNED_OUT: Pk = Pk::new(7);
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    snob_store::registry::Registry::update(&paths, |registry| {
        registry.upsert(SIGNED_OUT, "gone", snob_core::Epoch::new(1_700_000_000));
    })
    .unwrap();

    let list = |args: &[&str]| {
        let mut all = vec!["account", "list", "--json"];
        all.extend_from_slice(args);
        let out = snob(tmp.path(), None, &all);
        assert!(out.status.success(), "{}", stderr(&out));
        json(&out)
    };
    let said = list(&[]);
    assert_eq!(said["active"], PK.get(), "{said}");
    assert_eq!(said["viewer"]["pk"], PK.get(), "{said}");
    let accounts = said["accounts"].as_array().unwrap();
    let me = &accounts[0];
    assert_eq!((&me["pk"], &me["active"]), (&PK.get().into(), &true.into()));
    assert_eq!(me["session"], "file", "{said}");
    assert!(me["last_used"].is_i64(), "{said}");
    let other = &accounts[1];
    assert_eq!(other["username"], "gone");
    assert!(
        other["session"].is_null() && other["last_used"].is_null(),
        "{said}"
    );
    // The viewer is whoever the run acts as; the mark stays put.
    let said = list(&["--account", "gone"]);
    assert_eq!(
        (said["viewer"]["pk"].clone(), said["active"].clone()),
        (SIGNED_OUT.get().into(), PK.get().into())
    );

    let prose = snob(tmp.path(), None, &["account", "list"]);
    let table = stdout(&prose);
    assert!(
        table.lines().any(|line| line.starts_with("* @me ")),
        "{table}"
    );
    assert!(
        table
            .lines()
            .any(|line| line.starts_with("  @gone ") && line.contains("no session")),
        "{table}"
    );

    let moved = snob(tmp.path(), None, &["account", "use", "@GONE"]);
    assert!(moved.status.success(), "{}", stderr(&moved));
    assert_eq!(stdout(&moved).trim(), "Active account: @gone (was @me)");
    assert!(stderr(&moved).contains("no session"), "{}", stderr(&moved));
    assert_eq!(list(&[])["active"], SIGNED_OUT.get());
    // Acting as it now is acting with no session.
    let whoami = snob(tmp.path(), None, &["whoami", "--offline"]);
    assert_eq!(whoami.status.code(), Some(3), "{}", stderr(&whoami));

    let back = snob(tmp.path(), None, &["account", "use", "42"]);
    assert_eq!(stdout(&back).trim(), "Active account: @me (was @gone)");
    assert!(!stderr(&back).contains("no session"), "{}", stderr(&back));

    let nobody = snob(tmp.path(), None, &["account", "use", "nobody"]);
    assert_eq!(nobody.status.code(), Some(1), "{}", stderr(&nobody));
    assert!(
        stderr(&nobody).contains("snob account list"),
        "{}",
        stderr(&nobody)
    );
    assert_eq!(list(&[])["active"], PK.get());
}

/// A second account the fake Instagram knows, `@other`.
const OTHER: Pk = Pk::new(43);
const OTHER_SESSIONID: &str = "43%3Asandbox%3A17";

/// Teaches the fake Instagram who `OTHER` is; its lists are anybody's.
async fn knows_the_other_account(server: &MockServer) {
    Mock::given(method("GET"))
        .and(url_path(format!("/api/v1/users/{OTHER}/info/")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"user":{"pk":43,"username":"other","full_name":"Other"}}"#),
        )
        .mount(server)
        .await;
}

/// Pastes `sessionid` into `snob login --paste`, with `args` besides.
fn paste(root: &Path, instagram: &MockServer, sessionid: &str, args: &[&str]) -> Output {
    let mut all = vec!["login", "--paste", "--user-agent", UA];
    all.extend_from_slice(args);
    snob_typing(root, instagram, &all, &format!("{sessionid}\n"))
}

/// `me` signed in, and then `other`, each with a browser profile.
async fn two_accounts(root: &Path) -> MockServer {
    let instagram = fake_instagram(3, 2).await;
    knows_the_other_account(&instagram).await;
    log_in(root, &instagram);
    let out = paste(root, &instagram, OTHER_SESSIONID, &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let paths = snob_store::paths::AppPaths::rooted_at(root);
    for pk in [PK, OTHER] {
        let profile = paths.browser_profile_for(pk);
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Local State"), b"x").unwrap();
    }
    instagram
}

/// The rows of a database's request budget, as text.
fn budget(db: &Path) -> String {
    let store = snob_store::store::Store::open_at(db).unwrap();
    let mut statement = store
        .conn()
        .prepare("SELECT bucket, tat_ms FROM rate_budget ORDER BY bucket")
        .unwrap();
    let rows: Vec<String> = statement
        .query_map([], |row| {
            Ok(format!(
                "{}={}",
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.join(",")
}

/// A second account signs in beside the first, becomes the active one, and
/// its login's check is charged to its own database.
#[tokio::test]
async fn a_second_account_signs_in_beside_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    knows_the_other_account(&instagram).await;
    log_in(tmp.path(), &instagram);
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    let mine = budget(&paths.account(PK).db_file());

    // Nobody to ask and nothing named: the session pasted says it is
    // another account, which is added beside the one in use and replaces
    // nothing of it.
    let out = paste(tmp.path(), &instagram, OTHER_SESSIONID, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !stderr(&out).contains("already a session"),
        "{}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("Active account: @other (was @me)"),
        "{}",
        stdout(&out)
    );

    assert_eq!(budget(&paths.account(PK).db_file()), mine);
    assert!(!budget(&paths.account(OTHER).db_file()).is_empty());

    let out = snob(tmp.path(), None, &["whoami", "--offline", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["pk"], OTHER.get(), "{said}");
    assert_eq!(said["viewer"]["pk"], OTHER.get(), "{said}");
    assert_eq!(said["viewer"]["username"], "other", "{said}");
    let out = snob(
        tmp.path(),
        None,
        &["--account", "me", "whoami", "--offline", "--json"],
    );
    let said = json(&out);
    assert_eq!(said["viewer"]["pk"], PK.get(), "{said}");

    // Logging the account in use in again moves nothing, and says nothing
    // about it.
    let out = paste(tmp.path(), &instagram, OTHER_SESSIONID, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stdout(&out).contains("Active account"), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("already a session for @other"),
        "{}",
        stderr(&out)
    );
}

/// `--account` names the account logged in again; `--add` is for one not
/// here yet, and so is about none of them.
#[tokio::test]
async fn a_login_is_for_the_account_named_or_for_a_new_one() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = two_accounts(tmp.path()).await;

    let out = paste(tmp.path(), &instagram, SESSIONID, &["--account", "me"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("already a session for @me"),
        "{}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("Active account: @me (was @other)"),
        "{}",
        stdout(&out)
    );

    // A paste is whichever account its sessionid is, `--add` or not.
    let out = paste(tmp.path(), &instagram, OTHER_SESSIONID, &["--add"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("already a session for @other"),
        "{}",
        stderr(&out)
    );

    for both in [
        &["login", "--add", "--account", "me"][..],
        &["--account", "me", "login", "--add"][..],
    ] {
        let out = snob(tmp.path(), None, both);
        assert_eq!(out.status.code(), Some(2), "{both:?}: {}", stderr(&out));
    }
}

/// Logging out is one account's: its session and its browser profile go, its
/// data and its place among the accounts stay, and the other account is left
/// as it was. `--all` is every account's.
#[tokio::test]
async fn logging_one_account_out_leaves_the_other() {
    let tmp = tempfile::tempdir().unwrap();
    let _instagram = two_accounts(tmp.path()).await;
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    let (me, other) = (paths.account(PK), paths.account(OTHER));

    let out = snob(tmp.path(), None, &["logout"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Session deleted"), "{}", stdout(&out));
    assert!(!other.session_file().exists() && !other.browser_profile().exists());
    assert!(other.db_file().exists());
    assert!(me.session_file().exists() && me.browser_profile().exists());

    let out = snob(tmp.path(), None, &["account", "list", "--json"]);
    let said = json(&out);
    assert_eq!(said["active"], OTHER.get(), "{said}");
    assert!(said["accounts"][1]["session"].is_null(), "{said}");
    assert_eq!(said["accounts"][0]["session"], "file", "{said}");

    // Acting as it now is acting with no session, and says who.
    let out = snob(tmp.path(), None, &["whoami", "--offline", "--json"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["viewer"]["pk"], OTHER.get(), "{said}");

    let out = snob(tmp.path(), None, &["logout", "--all"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!me.session_file().exists());
    assert!(!paths.browser_profile().exists());
    assert!(me.db_file().exists() && other.db_file().exists());
}

/// With no list of accounts to resolve one from, a logout is every account
/// found's, sessions as well as browser profiles.
#[tokio::test]
async fn a_logout_with_no_account_resolved_takes_every_session() {
    let tmp = tempfile::tempdir().unwrap();
    let _instagram = two_accounts(tmp.path()).await;
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    std::fs::remove_file(paths.registry_file()).unwrap();

    let out = snob(tmp.path(), None, &["logout"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Session deleted"), "{}", stdout(&out));
    for pk in [PK, OTHER] {
        assert!(!paths.account(pk).session_file().exists(), "{pk}");
    }
    assert!(!paths.browser_profile().exists());
}

/// With several accounts and none active there is none to log in again as,
/// so a login adds the one it is and makes it active.
#[tokio::test]
async fn a_login_with_none_active_adds_its_account() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = two_accounts(tmp.path()).await;
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    snob_store::registry::Registry::update(&paths, |registry| registry.active = None).unwrap();

    let out = paste(tmp.path(), &instagram, OTHER_SESSIONID, &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let registry = snob_store::registry::Registry::load(&paths).unwrap();
    assert_eq!(registry.active, Some(OTHER));
}

/// `purge --account` takes one account's session, data, browser profile and
/// place among the accounts, and leaves the other's; the monitor's entries
/// read as it are pointed out. Only a typed flag narrows a purge.
#[tokio::test]
async fn purging_one_account_leaves_the_other() {
    let tmp = tempfile::tempdir().unwrap();
    let _instagram = two_accounts(tmp.path()).await;
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
    let (me, other) = (paths.account(PK), paths.account(OTHER));
    configure(
        tmp.path(),
        "schema = 2\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\nviewer = 43\n",
    );

    // The environment names the account every other command acts as, and a
    // purge is still of everything.
    let everything = snob_with(
        tmp.path(),
        None,
        &[("SNOB_ACCOUNT", "other")],
        &["purge", "--dry-run"],
    );
    assert!(everything.status.success(), "{}", stderr(&everything));
    let data = paths.data_dir().display().to_string();
    assert!(
        stdout(&everything).lines().any(|line| line.trim() == data),
        "{}",
        stdout(&everything)
    );

    let out = snob(tmp.path(), None, &["--account", "other", "purge", "--yes"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("watch.toml"), "{}", stderr(&out));
    assert!(!other.dir().exists() && !other.browser_profile().exists());
    assert!(me.session_file().exists() && me.db_file().exists());
    assert!(me.browser_profile().exists());
    assert!(tmp.path().join("config").join("watch.toml").exists());

    let registry = snob_store::registry::Registry::load(&paths).unwrap();
    assert!(registry.get(OTHER).is_none() && registry.get(PK).is_some());
    let out = snob(tmp.path(), None, &["whoami", "--offline", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(
        said["pk"],
        PK.get(),
        "the one account left is the one in use"
    );

    let unknown = snob(tmp.path(), None, &["--account", "other", "purge", "--yes"]);
    assert_eq!(unknown.status.code(), Some(1), "{}", stderr(&unknown));
}

/// `status` with no session says so, in both shapes, and exits 3.
#[test]
fn status_with_no_session_exits_three() {
    let tmp = tempfile::tempdir().unwrap();
    let out = snob(tmp.path(), None, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["error"]["code"], "no_session");
    for section in ["session", "budget", "cooldown", "lists", "watch"] {
        assert_eq!(said[section], serde_json::Value::Null, "{said}");
    }
    let out = snob(tmp.path(), None, &["status", "--budget", "--json"]);
    let said = json(&out);
    assert!(
        said.get("budget").is_some() && said.get("lists").is_none(),
        "{said}"
    );
}

/// Signed in and at rest: the whole budget, every section, exit 0, and not
/// one request to the fake Instagram.
#[tokio::test]
async fn status_reports_the_budget_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let before = instagram.received_requests().await.unwrap().len();

    let out = snob(tmp.path(), Some(&instagram), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = json(&out);
    for section in ["session", "budget", "cooldown", "lists", "watch"] {
        assert!(said.get(section).is_some(), "{section} missing: {said}");
    }
    assert_eq!(said["session"]["pk"], PK.get());
    assert_eq!(
        said["session"]["storage"], "file",
        "where the session was found, which a sandbox keeps in a file"
    );
    assert_eq!(said["budget"]["writes"]["left"], 3);
    assert_eq!(said["budget"]["accounts_left"], 2_000);
    assert_eq!(said["cooldown"]["active"], false);
    assert_eq!(
        said["lists"]["followers"]["taken_at"],
        serde_json::Value::Null
    );

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["status", "--budget", "--json"],
    );
    let said = json(&out);
    let keys: Vec<&String> = said.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["budget", "viewer"]);

    let text = snob(tmp.path(), Some(&instagram), &["status"]);
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(stdout(&text).contains("Requests"), "{}", stdout(&text));

    assert_eq!(
        instagram.received_requests().await.unwrap().len(),
        before,
        "status sent a request"
    );
}

/// In a cooldown it says until when and why, and exits 5 whatever section
/// was asked for, so a script can ask "may I send" with any of them.
#[tokio::test]
async fn status_in_a_cooldown_exits_five_and_says_why() {
    use snob_core::budget::RateBudget;

    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path()).account(PK);
    snob_store::store::rate_budget::SqliteRateBudget::open(&paths)
        .unwrap()
        .start_cooldown("429", std::time::Duration::from_secs(3600))
        .unwrap();

    let out = snob(tmp.path(), Some(&instagram), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["cooldown"]["active"], true);
    assert_eq!(said["cooldown"]["last"]["reason"], "429");
    assert_eq!(said["cooldown"]["last"]["strikes"], 1);
    assert!(said["cooldown"]["until"].as_i64().is_some());
    assert_eq!(said["budget"]["accounts_ceiling"], 1_000);

    let out = snob(tmp.path(), Some(&instagram), &["status", "--lists"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
}

/// A signed-in account with no database is reported at rest, and none is
/// made for it.
#[tokio::test]
async fn status_makes_no_database() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let db = database(tmp.path());
    for suffix in ["", "-wal", "-shm"] {
        let file = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if file.exists() {
            std::fs::remove_file(file).unwrap();
        }
    }

    let out = snob(tmp.path(), None, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["budget"]["day"]["left"], 2_001);
    assert_eq!(json(&out)["budget"]["requests_now"], 21);
    assert!(!db.exists(), "status made a database");
}

/// `-o -` writes the one file itself to standard output and says nothing
/// about where it went: a post's item, a story.
#[tokio::test]
async fn one_file_goes_to_standard_output_with_a_dash() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_a_post(&instagram).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let link = format!("https://www.instagram.com/p/{POST_CODE}/");
    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["post", &link, "-d", "2", "-o", "-"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"\x00\x00\x00\x18ftypisom a clip");
    assert!(!stderr(&out).contains("Saved"), "{}", stderr(&out));

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["stories", "me", "-d", "1", "-o", "-"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"\xff\xd8\xff\xe0 a picture");

    assert_eq!(
        std::fs::read_dir(&here).unwrap().count(),
        0,
        "a file named - or anything else was written"
    );
}

/// `-o -` takes one file: several are refused before any is fetched, and
/// nothing reaches standard output.
#[tokio::test]
async fn standard_output_takes_one_file_and_refuses_several() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_a_post(&instagram).await;
    with_downloadable_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let link = format!("https://www.instagram.com/p/{POST_CODE}/");
    for args in [
        vec!["post", link.as_str(), "-d", "all", "-o", "-"],
        vec!["stories", "me", "-d", "all", "-o", "-"],
    ] {
        let out = snob_from(&here, tmp.path(), &instagram, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(stderr(&out).contains("-o -"), "{}", stderr(&out));
    }
    let media = instagram
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/big.jpg" || r.url.path() == "/clip.mp4")
        .count();
    assert_eq!(media, 0, "a refused download fetched a file");
}

/// `fetch` needs nobody signed in: the file comes from the CDN, and no
/// database, session or browser profile is made for it.
#[tokio::test]
async fn fetch_downloads_a_cdn_file_with_no_account() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = MockServer::start().await;
    serve_media(&instagram).await;
    let here = here(&tmp);

    let clip = format!("{}/clip.mp4", instagram.uri());
    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["fetch", &clip, "--user-agent", UA, "-o", "-"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"\x00\x00\x00\x18ftypisom a clip");

    let picture = format!("{}/big.jpg?oh=signed", instagram.uri());
    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["fetch", &picture, "--user-agent", UA, "-o", "face.jpg"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(here.join("face.jpg")).unwrap(),
        b"\xff\xd8\xff\xe0 a picture"
    );

    let requests = instagram.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the two files were asked for");
    for request in &requests {
        assert!(request.headers.get("cookie").is_none());
        assert_eq!(request.headers.get("user-agent").unwrap(), UA);
    }
    let accounts = snob_store::paths::AppPaths::rooted_at(tmp.path())
        .data_dir()
        .join("accounts");
    assert!(!accounts.exists(), "fetch made an account's directory");
}

/// An address off the CDN is refused before anything is sent or created.
#[tokio::test]
async fn fetch_refuses_an_address_off_the_cdn() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = MockServer::start().await;
    let here = here(&tmp);
    for address in [
        "https://evil.test/x.jpg",
        "http://scontent.cdninstagram.com/x.jpg",
        "file:///etc/passwd",
        // Instagram's own site is not its CDN: an address on it is the API.
        "https://www.instagram.com/api/v1/users/web_profile_info/?username=someone",
    ] {
        let out = snob_from(
            &here,
            tmp.path(),
            &instagram,
            &["fetch", address, "--user-agent", UA, "-o", "x.jpg"],
        );
        assert_eq!(out.status.code(), Some(1), "{address}: {}", stderr(&out));
    }
    assert!(!here.join("x.jpg").exists());
    assert!(instagram.received_requests().await.unwrap().is_empty());
}

/// A download that fails does not take the file the user named with it: a
/// good earlier copy stays as it was, and no scratch file is left beside it.
#[tokio::test]
async fn fetch_keeps_the_named_file_when_the_download_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/old.jpg"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&instagram)
        .await;
    serve_media(&instagram).await;
    let here = here(&tmp);
    std::fs::write(here.join("keep.jpg"), b"a good earlier copy").unwrap();

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &[
            "fetch",
            &format!("{}/old.jpg", instagram.uri()),
            "--user-agent",
            UA,
            "-o",
            "keep.jpg",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(here.join("keep.jpg")).unwrap(),
        b"a good earlier copy"
    );
    let names: Vec<String> = std::fs::read_dir(&here)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["keep.jpg"], "a scratch file was left behind");

    // One that arrives whole replaces it.
    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &[
            "fetch",
            &format!("{}/big.jpg", instagram.uri()),
            "--user-agent",
            UA,
            "-o",
            "keep.jpg",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(here.join("keep.jpg")).unwrap(),
        b"\xff\xd8\xff\xe0 a picture"
    );
}

/// `-o -` on a listing prints it, as no `-o` down a pipe does, and never
/// writes a file named `-`.
#[tokio::test]
async fn a_listing_with_a_dash_goes_to_standard_output() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_stories(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    let out = snob_from(&here, tmp.path(), &instagram, &["stories", "me", "-o", "-"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed = json(&out);
    assert_eq!(listed["stories"].as_array().unwrap().len(), 2, "{listed}");
    assert!(!stderr(&out).contains("Written to"), "{}", stderr(&out));

    assert_eq!(
        std::fs::read_dir(&here).unwrap().count(),
        0,
        "a file named - or anything else was written"
    );
}

/// `-o -` with numbers that name several files is refused before anything
/// is asked of Instagram.
#[tokio::test]
async fn several_files_to_standard_output_are_refused_before_a_request() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    with_downloadable_stories(&instagram).await;
    with_downloadable_highlights(&instagram).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);
    let before = instagram.received_requests().await.unwrap().len();

    for args in [
        vec!["stories", "me", "-d", "1,2", "-o", "-"],
        vec!["highlights", "me", "-d", "1", "-o", "-"],
    ] {
        let out = snob_from(&here, tmp.path(), &instagram, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("-o -"), "{}", stderr(&out));
    }
    assert_eq!(
        instagram.received_requests().await.unwrap().len(),
        before,
        "a refused -o - asked Instagram something"
    );
}

/// `--dry-run` refuses what it would ignore, and the forms it has not got,
/// before anything is read or written.
#[tokio::test]
async fn a_dry_run_refuses_what_it_would_ignore() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let here = here(&tmp);

    for flags in [
        &["-y"][..],
        &["--no-progress"],
        &["--no-interactive"],
        &["--hide", "verified"],
        &["--limit", "3"],
    ] {
        let mut args = vec!["followers", "--dry-run"];
        args.extend_from_slice(flags);
        let out = snob_from(&here, tmp.path(), &instagram, &args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
    }
    for args in [
        vec!["followers", "--dry-run", "--format", "csv"],
        vec!["followers", "--dry-run", "-o", "list.xlsx"],
    ] {
        let out = snob_from(&here, tmp.path(), &instagram, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
    }
    assert_eq!(std::fs::read_dir(&here).unwrap().count(), 0);

    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &["followers", "--dry-run", "--format", "ndjson"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).lines().count(), 1, "{}", stdout(&out));
}

/// A command that walks the page that fills the account's sitting leaves
/// the rest owed in the account's database, where the next command finds
/// it: a dry run counts what is left of it, and the next walk pays it
/// before its first page and begins a new sitting. The sitting it fills is
/// one earlier commands left there, thirty-nine pages in.
///
/// Against a test server nothing is slept, so the rest is paid the moment
/// it is found; that the next command found it is what this holds down.
/// The walker's own tests (`pager.rs`) hold down that, against Instagram, a
/// walk started during the rest sends nothing until it is over.
#[tokio::test]
async fn a_rest_owed_when_a_command_ends_is_kept_for_the_next_one() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let store = || snob_store::store::Store::open_at(&database(tmp.path())).unwrap();
    let sitting = || snob_store::store::rate_budget::sitting(store().conn()).unwrap();
    store()
        .conn()
        .execute(
            "INSERT INTO sitting (id, pages, last_page_at, rest_until) VALUES (1, 39, ?1, NULL)",
            [snob_core::clock::now_ms().get()],
        )
        .expect("the sitting can be written");
    let minutes = |n: u64| std::time::Duration::from_secs(60 * n);

    let before = snob_core::clock::now_ms();
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["followers", "--refresh", "--format", "json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let after = snob_core::clock::now_ms();
    let owed = sitting().expect("a page was walked");
    assert_eq!(owed.pages, 40, "the page counts in the stored sitting");
    let until = owed.rest_until.expect("the fortieth page owes a rest");
    assert!(
        until >= before + minutes(5) && until <= after + minutes(15),
        "a rest of five to fifteen minutes: {until} after {before}"
    );

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["followers", "--dry-run", "--refresh", "--format", "json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let rest = json(&out)["rest_owed_seconds"].as_u64();
    assert!(
        rest.is_some_and(|rest| (1..=900).contains(&rest)),
        "the dry run counts the rest owed: {rest:?}"
    );

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["followers", "--refresh", "--format", "json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        sitting().map(|s| (s.pages, s.rest_until)),
        Some((1, None)),
        "the next command found the rest owed, paid it, and began a new sitting"
    );
}

/// A dry run writes nothing: no database is made for an account that has
/// none, as `status` makes none.
#[tokio::test]
async fn a_dry_run_makes_no_database() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let db = database(tmp.path());
    for suffix in ["", "-wal", "-shm"] {
        let file = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        if file.exists() {
            std::fs::remove_file(file).unwrap();
        }
    }

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollowers", "--dry-run", "--format", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(json(&out)["sends_now"], true);
    assert!(!db.exists(), "a dry run made a database");
}

/// An expired signed address is said to be one.
#[tokio::test]
async fn fetch_says_an_expired_address_is_one() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&instagram)
        .await;
    let here = here(&tmp);
    let out = snob_from(
        &here,
        tmp.path(),
        &instagram,
        &[
            "fetch",
            &format!("{}/old.jpg", instagram.uri()),
            "--user-agent",
            UA,
            "-o",
            "old.jpg",
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("expires"), "{}", stderr(&out));
    assert!(
        !here.join("old.jpg").exists(),
        "a refused file was left behind"
    );
}

/// A reader that leaves halfway through a file ends the run quietly.
#[tokio::test]
async fn fetch_to_a_reader_that_left_is_not_a_failure() {
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    let instagram = MockServer::start().await;
    Mock::given(method("GET"))
        .and(url_path("/large.mp4"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 4 << 20]))
        .mount(&instagram)
        .await;
    let large = format!("{}/large.mp4", instagram.uri());
    let mut child = command(tmp.path(), Some(&instagram))
        .args(["fetch", &large, "--user-agent", UA, "-o", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("the binary finishes");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!said.contains("panicked"), "{said}");
    assert_eq!(out.status.code(), Some(0), "{said}");
}

/// `--dry-run` says what the walks would cost, against what today has left,
/// and sends nothing: before anything is stored, and after.
#[tokio::test]
async fn a_dry_run_estimates_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let before = instagram.received_requests().await.unwrap().len();

    // Nothing stored yet: your own lists are known by nothing here.
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollowers", "--dry-run", "--format", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["dry_run"], true);
    assert_eq!(said["to_find"], 0, "your own account needs no finding");
    let lists: Vec<&str> = said["lists"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["list"].as_str().unwrap())
        .collect();
    assert_eq!(lists, ["followers", "following"], "the order it walks them");
    let status = json(&snob(
        tmp.path(),
        Some(&instagram),
        &["status", "--budget", "--json"],
    ));
    assert_eq!(
        said["requests_left"], status["budget"]["day"]["left"],
        "what is left is what status says"
    );
    assert_eq!(
        instagram.received_requests().await.unwrap().len(),
        before,
        "a dry run sent a request"
    );

    // Walked once, the lists are stored and fresh: a range, one poll at
    // the least.
    let out = snob(tmp.path(), Some(&instagram), &["scan", "--format", "json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let before = instagram.received_requests().await.unwrap().len();
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["scan", "--dry-run", "--format", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = json(&out);
    assert_eq!(said["lists"][0]["fate"], "reused_unless_moved", "{said}");
    assert_eq!(said["lists"][0]["size"], 3, "{said}");
    assert_eq!(said["requests"]["least"], 1, "{said}");
    assert!(said["requests"]["most"].as_u64().unwrap() > 1, "{said}");

    let text = snob(
        tmp.path(),
        Some(&instagram),
        &["followers", "--dry-run", "--refresh", "--format", "table"],
    );
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(
        stdout(&text).contains("nothing was sent"),
        "{}",
        stdout(&text)
    );
    assert!(stdout(&text).contains("a walk"), "{}", stdout(&text));
    assert_eq!(
        instagram.received_requests().await.unwrap().len(),
        before,
        "a dry run sent a request"
    );
}

/// Somebody else's lists: no consent is asked, since nothing is enumerated,
/// and an account never seen here is said to be of unknown size.
#[tokio::test]
async fn a_dry_run_of_a_stranger_asks_nothing_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let before = instagram.received_requests().await.unwrap().len();

    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["followers", "someone", "--dry-run", "--format", "json"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let said = json(&out);
    // The sandbox reaches its fake without a browser, where finding an
    // account is one read; from the browser it is the profile's five.
    assert_eq!(said["to_find"], 1, "{said}");
    assert_eq!(said["lists"][0]["fate"], "unknown", "{said}");
    assert_eq!(
        instagram.received_requests().await.unwrap().len(),
        before,
        "a dry run sent a request"
    );
}

/// In a cooldown a dry run says so and exits 5, as `status` does.
#[tokio::test]
async fn a_dry_run_in_a_cooldown_exits_five() {
    use snob_core::budget::RateBudget;

    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path()).account(PK);
    snob_store::store::rate_budget::SqliteRateBudget::open(&paths)
        .unwrap()
        .start_cooldown("429", std::time::Duration::from_secs(3600))
        .unwrap();

    let out = snob(tmp.path(), Some(&instagram), &["fans", "--dry-run"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    assert!(stdout(&out).contains("cooldown"), "{}", stdout(&out));
}

/// The `SSH_CONNECTION` of a session to a machine at 192.168.1.50.
const OVER_SSH: (&str, &str) = ("SSH_CONNECTION", "192.168.1.20 52344 192.168.1.50 22");

/// Over SSH to a Linux machine with no display, a window cannot open, and
/// `--browser` says so and what works instead, before any browser is looked
/// for — rather than Chromium exiting 1 and the advice being a guess.
#[cfg(not(any(target_os = "macos", windows)))]
#[test]
fn a_window_over_ssh_with_no_display_points_at_the_tunnel() {
    let tmp = tempfile::tempdir().unwrap();
    let out = command(tmp.path(), None)
        .env(OVER_SSH.0, OVER_SSH.1)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(["login", "--browser"])
        .output()
        .expect("the binary runs");
    assert!(!out.status.success());
    let said = stderr(&out);
    assert!(said.contains("SSH session"), "{said}");
    assert!(said.contains("snob login --remote"), "{said}");
    assert!(said.contains("snob login --paste"), "{said}");
    assert!(
        !snob_store::paths::AppPaths::rooted_at(tmp.path())
            .browser_profile()
            .exists(),
        "no profile was made for a browser that could not open"
    );
}

/// With nobody to show the menu to, over SSH, the methods named are the two
/// that work from there.
#[test]
fn a_login_with_no_terminal_over_ssh_names_the_tunnel() {
    let tmp = tempfile::tempdir().unwrap();
    // A display would put the person at this machine (`seat`), and then only
    // `--paste` is named.
    let out = command(tmp.path(), None)
        .env(OVER_SSH.0, OVER_SSH.1)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("login")
        .output()
        .expect("the binary runs");
    assert!(!out.status.success());
    let said = stderr(&out);
    assert!(said.contains("snob login --remote"), "{said}");
    assert!(said.contains("snob login --paste"), "{said}");
}

/// One method per login: `--remote` with either other is refused by the
/// command line itself.
#[test]
fn the_tunnel_is_one_method_among_three() {
    let tmp = tempfile::tempdir().unwrap();
    for both in [
        &["login", "--remote", "--paste"][..],
        &["login", "--remote", "--browser"][..],
    ] {
        let out = snob(tmp.path(), None, both);
        assert_eq!(out.status.code(), Some(2), "{both:?}: {}", stderr(&out));
    }
}

/// One `snob mcp` over its standard input and output, spoken to as a client
/// on the 2025-11-25 revision does.
#[cfg(feature = "mcp")]
struct McpClient {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next: u64,
    /// What the person answers when the server asks with
    /// `elicitation/create`; `None` is a client that never expects to be asked.
    /// An object with an `error` key is sent back as a JSON-RPC error rather
    /// than a result, as a client that declared elicitation and cannot do it
    /// answers.
    answer: Option<serde_json::Value>,
    /// The `_meta` every request carries on the stateless revision, which
    /// starts with no `initialize`.
    meta: Option<serde_json::Value>,
}

#[cfg(feature = "mcp")]
impl McpClient {
    async fn start(root: &Path, instagram: &MockServer, args: &[&str]) -> Self {
        Self::start_with(root, instagram, args, serde_json::json!({})).await
    }

    /// The same, declaring `capabilities` at `initialize`.
    async fn start_with(
        root: &Path,
        instagram: &MockServer,
        args: &[&str],
        capabilities: serde_json::Value,
    ) -> Self {
        let client = Self::spawn(root, instagram, args);
        client.initialize(capabilities).await
    }

    /// A server started from a copy of the binary at `program`.
    async fn start_from(program: &Path, root: &Path, instagram: &MockServer) -> Self {
        let client = Self::spawn_from(program, root, instagram, &[]);
        client.initialize(serde_json::json!({})).await
    }

    /// The handshake of a client on 2025-11-25, declaring `capabilities`.
    async fn initialize(mut self, capabilities: serde_json::Value) -> Self {
        let hello = self
            .ask(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": capabilities,
                    "clientInfo": { "name": "test", "version": "0" }
                }),
            )
            .await;
        assert_eq!(hello["result"]["serverInfo"]["name"], "snob", "{hello}");
        self.send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        self
    }

    /// A client on 2026-07-28: no `initialize`, and the revision and the
    /// capabilities in the `_meta` of every request.
    fn stateless(root: &Path, instagram: &MockServer, capabilities: serde_json::Value) -> Self {
        let mut client = Self::spawn(root, instagram, &[]);
        client.meta = Some(serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": capabilities,
            "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" }
        }));
        client
    }

    fn spawn(root: &Path, instagram: &MockServer, args: &[&str]) -> Self {
        Self::spawn_from(Path::new(env!("CARGO_BIN_EXE_snob")), root, instagram, args)
    }

    fn spawn_from(program: &Path, root: &Path, instagram: &MockServer, args: &[&str]) -> Self {
        use tokio::io::AsyncBufReadExt;
        let mut command = tokio::process::Command::from(command_of(program, root, Some(instagram)));
        command
            .arg("mcp")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("the binary runs");
        let stdin = child.stdin.take().unwrap();
        let lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
            next: 0,
            answer: None,
            meta: None,
        }
    }

    async fn send(&mut self, message: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Sends a request and waits for its answer, stepping over notifications.
    async fn ask(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.send_raw(method, params).await;
        self.read_until(id, method).await
    }

    /// Sends a request and returns its id, without waiting for the answer:
    /// for a call that is to be left asking while another is made.
    async fn send_raw(&mut self, method: &str, params: serde_json::Value) -> u64 {
        self.next += 1;
        let id = self.next;
        let mut params = params;
        if let Some(meta) = &self.meta {
            params["_meta"] = meta.clone();
        }
        self.send(
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .await;
        id
    }

    /// Reads until the server asks a question with `elicitation/create`, and
    /// hands it back unanswered, stepping over everything else.
    async fn question(&mut self) -> serde_json::Value {
        let asked = async {
            while let Some(line) = self.lines.next_line().await.unwrap() {
                let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                if message["method"] == "elicitation/create" {
                    return message;
                }
            }
            panic!("the server went away before asking");
        };
        tokio::time::timeout(std::time::Duration::from_secs(60), asked)
            .await
            .expect("a question within a minute")
    }

    /// Reads until the answer to request `id` (sent as `method`), answering
    /// a question the server asks on the way with [`McpClient::answer`].
    async fn read_until(&mut self, id: u64, method: &str) -> serde_json::Value {
        let answer = async {
            while let Some(line) = self.lines.next_line().await.unwrap() {
                let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                if message["method"] == "elicitation/create" {
                    let answer = self.answer.clone().expect("the server asked, unexpectedly");
                    let reply = match answer.get("error") {
                        Some(error) => serde_json::json!({
                            "jsonrpc": "2.0", "id": message["id"].clone(), "error": error
                        }),
                        None => serde_json::json!({
                            "jsonrpc": "2.0", "id": message["id"].clone(), "result": answer
                        }),
                    };
                    self.send(reply).await;
                    continue;
                }
                if message["id"] == id {
                    return message;
                }
            }
            panic!("the server went away before answering {method}");
        };
        tokio::time::timeout(std::time::Duration::from_secs(60), answer)
            .await
            .expect("an answer within a minute")
    }

    async fn call(&mut self, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        let answer = self
            .ask(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
            )
            .await;
        answer["result"].clone()
    }
}

/// The server offers the reads and not the writes, answers each call with
/// what the command it runs printed, runs nothing that needs a yes before it
/// has one, and answers a tool it does not offer as the protocol says.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_runs_the_commands_and_nothing_it_was_not_told_yes_to() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let listed = client.ask("tools/list", serde_json::json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"snob_status") && names.contains(&"snob_list"),
        "{names:?}"
    );
    assert!(!names.contains(&"snob_follow"), "{names:?}");

    let status = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(status["isError"], false, "{status}");

    let followers = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(followers["isError"], false, "{followers}");
    assert_eq!(followers["structuredContent"]["count"], 3, "{followers}");

    // Somebody else's list needs a yes, and this client declared no way to
    // ask for one: the question comes back to be asked in the chat, and no
    // walk of that account was started.
    let before = posted(&instagram).await.len();
    let theirs = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone" }),
        )
        .await;
    assert_eq!(
        theirs["structuredContent"]["status"], "confirmation_required",
        "{theirs}"
    );
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // A tool that was never listed is no tool at all: a protocol error, as
    // for a name made up, and not a result the model reads as the tool's.
    for name in ["snob_follow", "snob_nothing"] {
        let write = client
            .ask(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": { "account": "someone" } }),
            )
            .await;
        assert_eq!(write["error"]["code"], -32602, "{write}");
        assert!(write.get("result").is_none(), "{write}");
    }
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // And the server answers the next call: none of it took it down.
    let again = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(again["isError"], false, "{again}");
    assert!(client.child.id().is_some(), "the server is still up");
}

/// The mocks a confirmed unfollow of `someone` needs: the page that hands
/// out the tokens a mutation needs, served to the session's account, the
/// mutation, and a profile saying the relationship exists (without it the
/// command correctly decides there is nothing to do and sends nothing).
async fn serve_an_unfollow(instagram: &MockServer) {
    Mock::given(method("GET"))
        .and(url_path("/someone/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html>{"define":[["DTSGInitData",[],{"token":"DTSG"},1],
               ["LSD",[],{"token":"LSD"},2],
               ["PolarisViewer",[],{"data":{"id":"42","username":"me","fbid":"17841400000000042"},"id":"42"},3]]}
               <script src="https://static.cdninstagram.com/rsrc.php/v4/a.js"></script></html>"#,
        ))
        .with_priority(1)
        .mount(instagram)
        .await;
    Mock::given(method("POST"))
        .and(url_path("/api/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"result":"unfollowed","status":"ok"}"#),
        )
        .mount(instagram)
        .await;
    Mock::given(method("GET"))
        .and(url_path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "someone"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{"user":{"id":"9001","username":"someone","followed_by_viewer":true}}}"#,
        ))
        // Ahead of the catch-all mounted by `fake_instagram`, which answers for
        // any username with the session's own account. At equal priority
        // wiremock takes the first matching mount, and that one is first.
        .with_priority(1)
        .mount(instagram)
        .await;
}

/// The POSTs Instagram was sent, which are the writes.
#[cfg(feature = "mcp")]
async fn writes_sent(instagram: &MockServer) -> usize {
    posted(instagram)
        .await
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .count()
}

/// A client that cannot show a form is handed the question to put to the
/// person in the chat, with a token good once, for that call; the call runs
/// when the token comes back with it, and never twice.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_asks_in_the_chat_when_the_client_cannot_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let with = |token: &str, account: &str| serde_json::json!({ "list": "followers", "account": account, "confirmation": token });
    let before = posted(&instagram).await.len();

    // Asked, and nothing done: an answer, not an error.
    let asking = client.call("snob_list", theirs.clone()).await;
    assert_eq!(asking["isError"], false, "{asking}");
    let told = &asking["structuredContent"];
    assert_eq!(told["status"], "confirmation_required", "{asking}");
    assert_eq!(told["done"], false, "{asking}");
    assert!(told["previous"].is_null(), "{asking}");
    let question = told["question"].as_str().unwrap();
    assert!(question.contains("@someone's followers"), "{question}");
    let token = told["confirmation"].as_str().expect("a token").to_string();
    let said = asking["content"][0]["text"].as_str().unwrap();
    assert!(said.contains(question) && said.contains(&token), "{said}");
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // The token for another account's call, or no token at all, is no yes:
    // that call is asked again, and says why.
    for (offered, account) in [(token.as_str(), "somebody_else"), ("garbage", "someone")] {
        let other = client.call("snob_list", with(offered, account)).await;
        assert_eq!(
            other["structuredContent"]["status"], "confirmation_required",
            "{other}"
        );
        assert_eq!(
            other["structuredContent"]["previous"], "not_for_this_call",
            "{other}"
        );
        assert_ne!(other["structuredContent"]["confirmation"], token.as_str());
    }
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // Brought back with its own call, spelled with the at sign or not, it
    // is the yes: the walk runs.
    let done = client.call("snob_list", with(&token, "@someone")).await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(done["structuredContent"]["count"], 3, "{done}");
    let sent = posted(&instagram).await.len();
    assert!(sent > before, "the walk ran");

    // Once.
    let again = client.call("snob_list", with(&token, "someone")).await;
    assert_eq!(again["isError"], true, "{again}");
    assert_eq!(
        again["structuredContent"]["error"]["code"], "confirmation_used",
        "{again}"
    );
    assert_eq!(
        posted(&instagram).await.len(),
        sent,
        "nothing more was sent"
    );

    // A call that asks nothing takes no confirmation.
    let status = client
        .call("snob_status", serde_json::json!({ "confirmation": token }))
        .await;
    assert_eq!(
        status["structuredContent"]["error"]["code"], "invalid_arguments",
        "{status}"
    );
    let own = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "confirmation": "x" }),
        )
        .await;
    assert_eq!(
        own["structuredContent"]["error"]["code"], "invalid_arguments",
        "{own}"
    );
    let not_text = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone", "confirmation": 7 }),
        )
        .await;
    assert_eq!(
        not_text["structuredContent"]["error"]["code"], "invalid_arguments",
        "{not_text}"
    );

    // On the stateless revision, a client with no form is asked in the
    // chat the same way.
    let mut stateless = McpClient::stateless(tmp.path(), &instagram, serde_json::json!({}));
    let asking = stateless.call("snob_list", theirs).await;
    assert_eq!(
        asking["structuredContent"]["status"], "confirmation_required",
        "{asking}"
    );
}

/// A yes given in the chat sends one unfollow, and the same token again
/// sends nothing more.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_yes_given_in_the_chat_sends_one_unfollow() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    serve_an_unfollow(&instagram).await;
    log_in_writing(tmp.path(), &instagram);
    common::remember_doc_ids(tmp.path());
    let mut client = McpClient::start(tmp.path(), &instagram, &["--allow-writes"]).await;

    let asking = client
        .call("snob_unfollow", serde_json::json!({ "account": "someone" }))
        .await;
    let told = &asking["structuredContent"];
    assert_eq!(told["status"], "confirmation_required", "{asking}");
    assert_eq!(told["question"], "Unfollow @someone on Instagram?");
    let token = told["confirmation"].as_str().unwrap().to_string();
    assert_eq!(writes_sent(&instagram).await, 0, "nothing was sent");

    let yes = serde_json::json!({ "account": "someone", "confirmation": token });
    let done = client.call("snob_unfollow", yes.clone()).await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(done["structuredContent"]["outcome"], "unfollowed", "{done}");
    assert_eq!(writes_sent(&instagram).await, 1, "one yes, one write");

    let again = client.call("snob_unfollow", yes).await;
    assert_eq!(
        again["structuredContent"]["error"]["code"], "confirmation_used",
        "{again}"
    );
    assert_eq!(writes_sent(&instagram).await, 1, "and never a second");
}

/// `--confirm-in-chat` says what a yes in the chat may cover, and a call it
/// does not cover, from a client that cannot show a form, runs nothing.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn confirm_in_chat_none_refuses_a_client_that_cannot_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    // A session that can write: one that cannot is refused before it is
    // asked about (`follow --check-ahead`).
    log_in_writing(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let follow = serde_json::json!({ "account": "someone" });
    let before = posted(&instagram).await.len();

    let mut none = McpClient::start(
        tmp.path(),
        &instagram,
        &["--allow-writes", "--confirm-in-chat", "none"],
    )
    .await;
    for (tool, arguments) in [("snob_list", &theirs), ("snob_follow", &follow)] {
        let refused = none.call(tool, arguments.clone()).await;
        assert_eq!(refused["isError"], true, "{tool}: {refused}");
        assert_eq!(
            refused["structuredContent"]["error"]["code"], "cannot_confirm",
            "{tool}: {refused}"
        );
    }

    // `lists`: the walk is asked in the chat, the write is not.
    let mut lists = McpClient::start(
        tmp.path(),
        &instagram,
        &["--allow-writes", "--confirm-in-chat", "lists"],
    )
    .await;
    let walk = lists.call("snob_list", theirs).await;
    assert_eq!(
        walk["structuredContent"]["status"], "confirmation_required",
        "{walk}"
    );
    let write = lists.call("snob_follow", follow.clone()).await;
    let refusal = &write["structuredContent"]["error"];
    assert_eq!(refusal["code"], "cannot_confirm", "{write}");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("--confirm-in-chat all"),
        "{write}"
    );
    // And a token from the server that took it is none here.
    let token = walk["structuredContent"]["confirmation"].as_str().unwrap();
    let forged = lists
        .call(
            "snob_follow",
            serde_json::json!({ "account": "someone", "confirmation": token }),
        )
        .await;
    assert_eq!(
        forged["structuredContent"]["error"]["code"], "cannot_confirm",
        "{forged}"
    );
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");
}

/// A client on 2025-11-25 that declared elicitation and answers the request
/// with "no such method" is asked in the chat instead; one that declines is
/// not.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn an_older_client_that_cannot_answer_the_form_is_asked_in_the_chat() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start_with(
        tmp.path(),
        &instagram,
        &[],
        serde_json::json!({ "elicitation": {} }),
    )
    .await;
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let before = posted(&instagram).await.len();

    client.answer = Some(serde_json::json!({
        "error": { "code": -32601, "message": "Method not found" }
    }));
    let asking = client.call("snob_list", theirs.clone()).await;
    assert_eq!(
        asking["structuredContent"]["status"], "confirmation_required",
        "{asking}"
    );

    client.answer = Some(serde_json::json!({ "action": "decline" }));
    let declined = client.call("snob_list", theirs).await;
    assert_eq!(
        declined["structuredContent"]["error"]["code"], "not_confirmed",
        "{declined}"
    );
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");
}

/// With nobody signed in, `snob_status` still answers (that is what it was
/// asked), and every answer that comes of having no session tells the agent
/// that signing in is the person's, in their own terminal.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_status_without_a_session_is_an_answer_with_a_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let hello = client.ask("tools/list", serde_json::json!({})).await;
    assert!(hello["result"]["tools"].is_array(), "{hello}");

    let status = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(status["isError"], false, "{status}");
    let told = &status["structuredContent"];
    assert_eq!(told["exit"], "no_session", "{status}");
    assert_eq!(told["error"]["code"], "no_session", "{status}");
    let hint = told["error"]["hint"].as_str().expect("a hint");
    assert!(
        hint.contains("snob login") && hint.contains("their own terminal"),
        "{hint}"
    );

    // A list asked for with no session is a failure, with the same hint in
    // place of the one meant for a person at the terminal.
    let followers = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(followers["isError"], true, "{followers}");
    assert_eq!(
        followers["structuredContent"]["error"]["hint"], told["error"]["hint"],
        "{followers}"
    );
    assert!(posted(&instagram).await.is_empty(), "nothing was sent");
}

/// The usernames on a page a list call handed back.
#[cfg(feature = "mcp")]
fn usernames(told: &serde_json::Value) -> Vec<String> {
    told["items"]
        .as_array()
        .unwrap_or_else(|| panic!("a page: {told}"))
        .iter()
        .map(|item| item["username"].as_str().unwrap().to_string())
        .collect()
}

/// A list longer than a page comes back a page at a time, and every page
/// after the first walks nothing, asks nothing and sends nothing: it is
/// handed back from what the first call read, or, by a server that never
/// read it, from the store.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_hands_a_long_list_back_a_page_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(450, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let first = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(first["isError"], false, "{first}");
    let told = &first["structuredContent"];
    assert_eq!(told["count"], 450, "the whole list's: {told}");
    assert_eq!(told["offset"], 0, "{told}");
    let mut read = usernames(told);
    assert!(read.len() < 450, "one page of it");
    let mut next = told["next_offset"].as_u64().expect("a next page");
    // Said in words too, for a client that shows the model only the text.
    let says = |result: &serde_json::Value, next: u64| {
        result["content"].as_array().unwrap().iter().any(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains(&format!("\"offset\": {next}")))
        })
    };
    assert!(says(&first, next), "{first}");

    let sent = posted(&instagram).await.len();
    loop {
        let later = client
            .call(
                "snob_list",
                serde_json::json!({ "list": "followers", "offset": next }),
            )
            .await;
        assert_eq!(later["isError"], false, "{later}");
        let told = &later["structuredContent"];
        assert_eq!(told["count"], 450, "{told}");
        assert_eq!(told["offset"], next, "{told}");
        read.extend(usernames(told));
        match told["next_offset"].as_u64() {
            Some(after) => {
                assert!(says(&later, after), "{later}");
                next = after;
            }
            None => break,
        }
    }
    let every: Vec<String> = (0..450).map(|n| format!("user{n}")).collect();
    assert_eq!(read, every, "every account once, in the order served");
    assert_eq!(
        posted(&instagram).await.len(),
        sent,
        "no page after the first sent anything"
    );

    // A server that never read the list hands the same page back from the
    // store, and sends nothing for it either.
    let mut other = McpClient::start(tmp.path(), &instagram, &[]).await;
    let stored = other
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "offset": 200 }),
        )
        .await;
    assert_eq!(stored["isError"], false, "{stored}");
    let page = usernames(&stored["structuredContent"]);
    assert!(!page.is_empty());
    assert_eq!(page[..], every[200..200 + page.len()]);

    // Another account's list at a later page is not asked about and not
    // walked: nothing of it is stored, and the answer says so.
    let theirs = other
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone", "offset": 200 }),
        )
        .await;
    assert_eq!(theirs["isError"], true, "{theirs}");
    assert!(
        theirs["structuredContent"].get("status").is_none(),
        "nothing asked: {theirs}"
    );
    // Told in its own tools' words: no command to type, which would read the
    // person's own list, and no --offline, which the agent never passed.
    let error = &theirs["structuredContent"]["error"];
    assert_eq!(error["code"], "nothing_stored", "{theirs}");
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("mode \"auto\""), "{theirs}");
    assert!(!hint.contains("snob followers"), "{theirs}");
    assert!(!hint.contains("--offline"), "{theirs}");
    // A later page of a refresh is the same later page, which walks
    // nothing: "the same arguments and an offset" works whatever the mode.
    // Read from the store, it says so and when that was.
    let refresh = other
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "mode": "refresh", "offset": 200 }),
        )
        .await;
    assert_eq!(refresh["isError"], false, "{refresh}");
    assert_eq!(
        usernames(&refresh["structuredContent"])[0],
        "user200",
        "{refresh}"
    );
    assert_eq!(
        refresh["structuredContent"]["served"][0]["from"], "stored",
        "{refresh}"
    );
    assert!(
        refresh["structuredContent"]["served"][0]["taken_at"]
            .as_i64()
            .is_some(),
        "{refresh}"
    );
    assert_eq!(posted(&instagram).await.len(), sent, "and nothing was sent");

    // So is a list asked for as stored, on its first page.
    let stored = other
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "mode": "stored", "limit": 2 }),
        )
        .await;
    assert_eq!(stored["isError"], false, "{stored}");
    let told = &stored["structuredContent"];
    assert_eq!(told["count"], 2, "at most the limit: {stored}");
    assert_eq!(told["served"][0]["from"], "stored", "{stored}");
    let sentence = stored["content"][1]["text"].as_str().unwrap_or_default();
    assert!(sentence.contains("Read from what is stored"), "{stored}");

    // A walk given max_pages may stop short, and what it read is then not
    // stored as a whole list: its later pages come from the server that
    // read it, and another server says it cannot hand them back.
    let capped = serde_json::json!({ "list": "followers", "max_pages": 40 });
    let first = client.call("snob_list", capped.clone()).await;
    assert_eq!(first["isError"], false, "{first}");
    let mut later = capped.clone();
    later["offset"] = serde_json::json!(200);
    let held = client.call("snob_list", later.clone()).await;
    assert_eq!(held["isError"], false, "{held}");
    assert_eq!(usernames(&held["structuredContent"])[0], "user200");
    let gone = other.call("snob_list", later).await;
    assert_eq!(
        gone["structuredContent"]["error"]["code"], "page_unavailable",
        "{gone}"
    );

    // A refresh's later page is held where the page looks for it.
    let refreshed = serde_json::json!({ "list": "followers", "mode": "refresh", "max_pages": 40 });
    let first = client.call("snob_list", refreshed.clone()).await;
    assert_eq!(first["isError"], false, "{first}");
    let mut later = refreshed;
    later["offset"] = serde_json::json!(200);
    let held = client.call("snob_list", later).await;
    assert_eq!(held["isError"], false, "{held}");
    assert_eq!(usernames(&held["structuredContent"])[0], "user200");
}

/// A walk cut short by `max_pages` exits 0 with what it read, as a list cut
/// by the cap a person asked for does; the agent is told it is not the whole
/// list, how much of it was read and of how many, and that `count` is not
/// the account's total.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_walk_cut_short_by_max_pages_says_it_is_not_the_whole_list() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(25, 2).await;
    // Two pages of followers, ahead of the one-page list.
    let page = |from: u64, to: u64, next: Option<u64>| {
        let users: Vec<String> = (from..to)
            .map(|n| format!(r#"{{"pk":{},"username":"user{n}"}}"#, 1_000 + n))
            .collect();
        let next = next.map_or(String::new(), |n| format!(r#","next_max_id":"{n}""#));
        ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"users":[{}],"big_list":true{next},"status":"ok"}}"#,
            users.join(",")
        ))
    };
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .and(query_param("max_id", "12"))
        .respond_with(page(12, 25, None))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(page(0, 12, Some(12)))
        .with_priority(2)
        .mount(&instagram)
        .await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let short = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "max_pages": 1 }),
        )
        .await;
    assert_eq!(short["isError"], false, "{short}");
    let told = &short["structuredContent"];
    assert_eq!(told["complete"], false, "{short}");
    assert_eq!(told["count"], 12, "{short}");
    assert_eq!(told["served"][0]["stopped"], "page_limit", "{short}");
    assert_eq!(told["served"][0]["read"], 12, "{short}");
    assert_eq!(told["served"][0]["of"], 25, "{short}");
    let said: Vec<&str> = short["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert!(
        said.iter()
            .any(|text| text.contains("Not the whole list") && text.contains("12 of about 25")),
        "{short}"
    );

    // Walked whole, it says nothing of the kind.
    let whole = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "mode": "refresh" }),
        )
        .await;
    assert_eq!(whole["isError"], false, "{whole}");
    assert_eq!(whole["structuredContent"]["count"], 25, "{whole}");
    assert!(
        whole["structuredContent"].get("complete").is_none(),
        "{whole}"
    );
}

/// A push-back met by the call itself starts a cooldown its failure does not
/// date: the agent is pointed at `snob_status`, which does, rather than told
/// to run it again once the cooldown lifts.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_push_back_met_by_a_walk_points_the_agent_at_snob_status() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(25, 2).await;
    log_in(tmp.path(), &instagram);
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(r#"{"message":"Please wait a few minutes"}"#),
        )
        .with_priority(1)
        .mount(&instagram)
        .await;
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;
    let met = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "mode": "refresh" }),
        )
        .await;
    assert_eq!(met["isError"], true, "{met}");
    let error = &met["structuredContent"]["error"];
    assert_eq!(error["code"], "rate_limited", "{met}");
    let hint = error["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("snob_status"), "{met}");

    let status = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(
        status["structuredContent"]["exit"], "rate_limited",
        "{status}"
    );
}

/// A server whose binary is moved aside under it, as WinGet and
/// `install.ps1` do on an upgrade and Homebrew's cleanup does by removing it,
/// refuses every call from then on and starts nothing, rather than run a
/// child of whatever is at its path now.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_refuses_calls_once_its_binary_is_upgraded() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let original = Path::new(env!("CARGO_BIN_EXE_snob"));
    let program = bin.path().join(original.file_name().unwrap());
    std::fs::copy(original, &program).unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut client = McpClient::start_from(&program, tmp.path(), &instagram).await;

    let before = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(before["isError"], false, "{before}");
    assert_eq!(
        before["structuredContent"]["exit"], "no_session",
        "{before}"
    );

    // Renaming a running program is allowed on every platform, Windows
    // included; removing it is not, there.
    std::fs::rename(&program, bin.path().join("snob.old")).unwrap();
    for (tool, arguments) in [
        ("snob_status", serde_json::json!({})),
        ("snob_list", serde_json::json!({ "list": "followers" })),
    ] {
        let after = client.call(tool, arguments).await;
        assert_eq!(after["isError"], true, "{tool}: {after}");
        assert_eq!(
            after["structuredContent"]["error"]["code"], "upgraded",
            "{tool}: {after}"
        );
        let message = after["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap();
        assert!(message.contains("restart the MCP server"), "{message}");
    }
    assert!(posted(&instagram).await.is_empty(), "nothing was sent");

    // Stopped before the directory goes, which Windows would refuse while
    // the program in it runs.
    let McpClient {
        mut child, stdin, ..
    } = client;
    drop(stdin);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait())
        .await
        .expect("the server stops when its input closes")
        .unwrap();
    assert!(ended.success(), "{ended:?}");
}

/// On 2025-11-25 the yes is asked with `elicitation/create` while the call
/// waits: no is nothing run, yes is the command run with `-y`.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_asks_an_older_client_while_the_call_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start_with(
        tmp.path(),
        &instagram,
        &[],
        serde_json::json!({ "elicitation": {} }),
    )
    .await;
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });

    client.answer = Some(serde_json::json!({ "action": "decline" }));
    let declined = client.call("snob_list", theirs.clone()).await;
    assert_eq!(
        declined["structuredContent"]["error"]["code"], "not_confirmed",
        "{declined}"
    );

    client.answer = Some(serde_json::json!({ "action": "accept", "content": { "confirm": true } }));
    let accepted = client.call("snob_list", theirs).await;
    assert_eq!(accepted["isError"], false, "{accepted}");
}

/// While a write waits for the person's answer, it holds the server's one
/// call that may reach Instagram: a second write, or a walk, is refused as
/// busy at once, before anything is asked, and nothing is queued; the status
/// and a stored list go on beside it. Once the first is answered, the next
/// call that spends runs.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_second_write_while_one_is_asked_is_refused_as_busy() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    // A session that can write: one that cannot is refused before it is
    // asked about (`follow --check-ahead`).
    log_in_writing(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start_with(
        tmp.path(),
        &instagram,
        &["--allow-writes"],
        serde_json::json!({ "elicitation": {} }),
    )
    .await;
    // Stored first, so the stored list below has something to answer with.
    let walked = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(walked["isError"], false, "{walked}");

    let follow = client
        .send_raw(
            "tools/call",
            serde_json::json!({ "name": "snob_follow", "arguments": { "account": "someone" } }),
        )
        .await;
    // Asked, and left unanswered: the call is holding its permits now.
    let question = client.question().await;

    for (tool, arguments) in [
        ("snob_unfollow", serde_json::json!({ "account": "other" })),
        ("snob_follow", serde_json::json!({ "account": "other" })),
        ("snob_list", serde_json::json!({ "list": "following" })),
        ("snob_profile", serde_json::json!({})),
    ] {
        // `call` would answer a question with `answer`, which is `None`
        // here: a second question would fail the test, not be answered.
        let busy = client.call(tool, arguments).await;
        assert_eq!(busy["isError"], true, "{tool}: {busy}");
        assert_eq!(
            busy["structuredContent"]["error"]["code"], "busy",
            "{tool}: {busy}"
        );
    }
    let status = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(status["isError"], false, "{status}");
    let stored = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "mode": "stored" }),
        )
        .await;
    assert_eq!(stored["isError"], false, "{stored}");
    assert_eq!(stored["structuredContent"]["count"], 3, "{stored}");

    client
        .send(serde_json::json!({
            "jsonrpc": "2.0", "id": question["id"].clone(), "result": { "action": "decline" }
        }))
        .await;
    let declined = client.read_until(follow, "tools/call").await;
    assert_eq!(
        declined["result"]["structuredContent"]["error"]["code"], "not_confirmed",
        "{declined}"
    );
    assert_eq!(writes_sent(&instagram).await, 0, "nothing was written");

    // Given back: the next call that spends runs.
    let again = client
        .call("snob_list", serde_json::json!({ "list": "following" }))
        .await;
    assert_eq!(again["isError"], false, "{again}");
}

/// While a job of the account walks, a call it would refuse is refused
/// before anything is asked, as the call itself would have been: another
/// walk as `too_many_jobs`, a write as `busy`. Nobody says yes to a call
/// that then does not run, though the job's walk holds none of the
/// server's permits once its call has answered.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn nothing_is_asked_for_a_call_a_running_job_would_refuse() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    // `answer` stays `None`: a question would fail the test.
    let mut client = McpClient::start_with(
        tmp.path(),
        &instagram,
        &["--allow-writes"],
        serde_json::json!({ "elicitation": {} }),
    )
    .await;
    let started = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 1 }),
        )
        .await;
    assert_eq!(
        started["structuredContent"]["status"], "running",
        "{started}"
    );
    let id = started["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_string();

    let theirs = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone" }),
        )
        .await;
    assert_eq!(theirs["isError"], true, "{theirs}");
    let error = &theirs["structuredContent"]["error"];
    assert_eq!(error["code"], "too_many_jobs", "{theirs}");
    assert!(
        error["hint"].as_str().unwrap().contains("snob_job_cancel"),
        "{theirs}"
    );

    let follow = client
        .call("snob_follow", serde_json::json!({ "account": "someone" }))
        .await;
    assert_eq!(follow["isError"], true, "{follow}");
    assert_eq!(
        follow["structuredContent"]["error"]["code"], "busy",
        "{follow}"
    );
    assert_eq!(writes_sent(&instagram).await, 0, "nothing was written");

    let stopped = client
        .call("snob_job_cancel", serde_json::json!({ "job_id": id }))
        .await;
    assert_eq!(stopped["isError"], false, "{stopped}");
}

/// A child given `--spend-alone` refuses, before any request, while another
/// run holds the account, with exit 1 and `busy` as the reason; the server
/// gives the flag to the children that may reach Instagram, and an agent is
/// told `busy`. A stored list, and a command a person types, take no lock.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_child_given_spend_alone_refuses_while_another_holds_the_account() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let followers = ["followers", "--format", "json", "--no-interactive"];
    let first = snob(tmp.path(), Some(&instagram), &followers);
    assert!(first.status.success(), "{}", stderr(&first));

    let lock = snob_store::paths::AppPaths::rooted_at(tmp.path())
        .account(PK)
        .spend_lock();
    let held = snob_store::paths::try_lock_file(&lock)
        .unwrap()
        .expect("nothing holds the account between runs");
    let before = posted(&instagram).await.len();

    let alone: Vec<&str> = std::iter::once("--spend-alone").chain(followers).collect();
    let refused = snob(tmp.path(), Some(&instagram), &alone);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    let told: serde_json::Value =
        serde_json::from_str(stderr(&refused).lines().last().unwrap_or_default())
            .expect("the last line of standard error is JSON");
    assert_eq!(told["error"]["code"], "error", "{told}");
    assert_eq!(told["error"]["reason"], "busy", "{told}");
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // Through the server: the walk is refused as busy, the stored list is
    // read beside the run that holds the account.
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;
    let busy = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(busy["isError"], true, "{busy}");
    assert_eq!(busy["structuredContent"]["error"]["code"], "busy", "{busy}");
    assert_eq!(busy["structuredContent"]["exit"], "error", "{busy}");
    let stored = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "mode": "stored" }),
        )
        .await;
    assert_eq!(stored["isError"], false, "{stored}");
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // A person's own command does not take the lock in this release.
    let typed = snob(tmp.path(), Some(&instagram), &followers);
    assert!(typed.status.success(), "{}", stderr(&typed));

    drop(held);
    let freed = snob(tmp.path(), Some(&instagram), &alone);
    assert!(freed.status.success(), "{}", stderr(&freed));
    let walked = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(walked["isError"], false, "{walked}");
}

/// On 2026-07-28 the call answers with the question and a ticket; the
/// answer counts only when it comes back with that ticket for that same
/// command, and only once.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_asks_a_stateless_client_in_the_call() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::stateless(
        tmp.path(),
        &instagram,
        serde_json::json!({ "elicitation": {} }),
    );
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let before = posted(&instagram).await.len();

    let asking = client.call("snob_list", theirs.clone()).await;
    assert_eq!(asking["resultType"], "input_required", "{asking}");
    let state = asking["requestState"]
        .as_str()
        .expect("a signed state")
        .to_string();
    assert!(asking["inputRequests"]["confirm"].is_object(), "{asking}");
    let yes =
        serde_json::json!({ "confirm": { "action": "accept", "content": { "confirm": true } } });

    // The same yes, offered for another account, is no answer.
    let elsewhere = client
        .ask(
            "tools/call",
            serde_json::json!({
                "name": "snob_list",
                "arguments": { "list": "followers", "account": "somebody_else" },
                "inputResponses": yes.clone(),
                "requestState": state.clone(),
            }),
        )
        .await;
    assert_eq!(elsewhere["result"]["isError"], true, "{elsewhere}");

    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // Spelled with the at sign, it is the same command, so the same yes.
    let answered = serde_json::json!({
        "name": "snob_list",
        "arguments": { "list": "followers", "account": "@someone" },
        "inputResponses": yes,
        "requestState": state,
    });
    let done = client.ask("tools/call", answered.clone()).await;
    assert_eq!(done["result"]["isError"], false, "{done}");
    let sent = posted(&instagram).await.len();
    assert!(sent > before, "the walk ran");

    // The identical answered call again, as a client that retries a request
    // it heard nothing back from: the yes was used, and nothing runs twice.
    let again = client.ask("tools/call", answered).await;
    assert_eq!(again["result"]["isError"], true, "{again}");
    assert_eq!(
        again["result"]["structuredContent"]["error"]["code"], "confirmation_used",
        "{again}"
    );
    assert_eq!(
        posted(&instagram).await.len(),
        sent,
        "nothing more was sent"
    );
}

/// A call that comes back with the ticket and no answer is asked again, with
/// a fresh ticket, and the old one is retired: one question open, not two.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_stateless_retry_without_an_answer_is_asked_again() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::stateless(
        tmp.path(),
        &instagram,
        serde_json::json!({ "elicitation": { "form": {} } }),
    );
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let yes =
        serde_json::json!({ "confirm": { "action": "accept", "content": { "confirm": true } } });
    let before = posted(&instagram).await.len();

    let first = client.call("snob_list", theirs.clone()).await;
    let old = first["requestState"]
        .as_str()
        .expect("a ticket")
        .to_string();

    let retried = client
        .ask(
            "tools/call",
            serde_json::json!({
                "name": "snob_list",
                "arguments": theirs.clone(),
                "requestState": old.clone(),
            }),
        )
        .await;
    assert_eq!(
        retried["result"]["resultType"], "input_required",
        "{retried}"
    );
    let fresh = retried["result"]["requestState"]
        .as_str()
        .expect("a fresh ticket")
        .to_string();
    assert_ne!(fresh, old);

    let with_old = client
        .ask(
            "tools/call",
            serde_json::json!({
                "name": "snob_list",
                "arguments": theirs.clone(),
                "inputResponses": yes.clone(),
                "requestState": old,
            }),
        )
        .await;
    assert_eq!(
        with_old["result"]["structuredContent"]["error"]["code"], "confirmation_used",
        "{with_old}"
    );
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    let done = client
        .ask(
            "tools/call",
            serde_json::json!({
                "name": "snob_list",
                "arguments": theirs,
                "inputResponses": yes,
                "requestState": fresh,
            }),
        )
        .await;
    assert_eq!(done["result"]["isError"], false, "{done}");
}

/// Asks `tool` in the chat and calls it again with the token, as an agent
/// does once the person said yes; the second call's result.
#[cfg(feature = "mcp")]
async fn confirmed_in_chat(
    client: &mut McpClient,
    tool: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let asking = client.call(tool, arguments.clone()).await;
    let told = &asking["structuredContent"];
    assert_eq!(told["status"], "confirmation_required", "{asking}");
    let mut yes = arguments;
    yes["confirmation"] = told["confirmation"].clone();
    client.call(tool, yes).await
}

/// The acts recorded in the sandbox account's database, of any age.
#[cfg(feature = "mcp")]
fn acts_recorded(root: &Path) -> i64 {
    let db = snob_store::store::Store::open_at(&database(root)).unwrap();
    snob_store::store::mcp_acts::count(db.conn()).unwrap()
}

/// Ten follows and unfollows agents were told yes to in the last day are as
/// many as a server allows when it was given no `--max-writes-per-day`: the
/// eleventh is refused with nothing sent and nothing recorded, **before the
/// person is asked**, and says when the next is free as a moment. A write the
/// person types is not counted, and a server given a higher cap lets it
/// through.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_eleventh_agent_write_in_a_day_is_refused() {
    use snob_store::store::mcp_acts::{NewAct, Recorded, record};
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    serve_an_unfollow(&instagram).await;
    log_in_writing(tmp.path(), &instagram);
    common::remember_doc_ids(tmp.path());
    // Ten earlier today, as the children of a server record them.
    let mut db = snob_store::store::Store::open_at(&database(tmp.path())).unwrap();
    for _ in 0..10 {
        let act = NewAct {
            action: "follow",
            target: Some("somebody"),
            via: "chat",
            client: Some("test 0"),
        };
        let recorded = record(&mut db, &act, None, snob_core::clock::now_ms()).unwrap();
        assert!(matches!(recorded, Recorded::Done(_)));
    }
    drop(db);
    let unfollow = serde_json::json!({ "account": "someone" });

    let mut client = McpClient::start(tmp.path(), &instagram, &["--allow-writes"]).await;
    let refused = client.call("snob_unfollow", unfollow.clone()).await;
    assert_eq!(refused["isError"], true, "{refused}");
    let error = &refused["structuredContent"]["error"];
    assert_eq!(error["code"], "mcp_write_cap", "{refused}");
    let now = snob_core::clock::now_ms().to_epoch().get();
    let retry_at = error["retry_at"].as_i64().expect("a moment");
    // A day after the first of the ten, rounded up to the second.
    assert!(retry_at > now && retry_at <= now + 86_401, "{refused}");
    assert!(
        error["hint"].as_str().unwrap().contains("retry_at"),
        "{refused}"
    );
    assert_eq!(writes_sent(&instagram).await, 0, "nothing was sent");
    assert_eq!(acts_recorded(tmp.path()), 10, "nor recorded");

    // Typed by the person, it is theirs: sent, and not recorded.
    let typed = snob(
        tmp.path(),
        Some(&instagram),
        &["unfollow", "someone", "-y", "--json"],
    );
    assert_eq!(typed.status.code(), Some(0), "{}", stderr(&typed));
    assert_eq!(writes_sent(&instagram).await, 1);
    assert_eq!(acts_recorded(tmp.path()), 10);

    // A server allowed eleven lets this one through, once.
    let mut eleven = McpClient::start(
        tmp.path(),
        &instagram,
        &["--allow-writes", "--max-writes-per-day", "11"],
    )
    .await;
    let done = confirmed_in_chat(&mut eleven, "snob_unfollow", unfollow.clone()).await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(writes_sent(&instagram).await, 2);
    assert_eq!(acts_recorded(tmp.path()), 11);
    let twelfth = eleven.call("snob_unfollow", unfollow).await;
    assert_eq!(
        twelfth["structuredContent"]["error"]["code"], "mcp_write_cap",
        "{twelfth}"
    );
    assert_eq!(writes_sent(&instagram).await, 2);

    // And no server takes a cap past what the write bucket allows in a day.
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["mcp", "--allow-writes", "--max-writes-per-day", "100"],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// In a cooldown a write, a refresh and a list with nothing stored to serve
/// are refused **before the person is asked**, as their own runs would
/// refuse them, with when the cooldown lifts; a list with something stored
/// is still asked about and served, and none of it is recorded as an act,
/// since nothing was read or sent.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_cooldown_refuses_a_call_before_anybody_is_asked() {
    use snob_core::budget::RateBudget;
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    serve_an_unfollow(&instagram).await;
    log_in_writing(tmp.path(), &instagram);
    common::remember_doc_ids(tmp.path());
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let walked = snob(
        tmp.path(),
        Some(&instagram),
        &[
            "followers",
            "someone",
            "-y",
            "--format",
            "json",
            "--no-interactive",
        ],
    );
    assert!(walked.status.success(), "{}", stderr(&walked));
    let paths = snob_store::paths::AppPaths::rooted_at(tmp.path()).account(PK);
    snob_store::store::rate_budget::SqliteRateBudget::open(&paths)
        .unwrap()
        .start_cooldown("429", std::time::Duration::from_secs(3600))
        .unwrap();
    let before = posted(&instagram).await.len();

    let mut client = McpClient::start(tmp.path(), &instagram, &["--allow-writes"]).await;
    for (tool, arguments) in [
        ("snob_unfollow", serde_json::json!({ "account": "someone" })),
        (
            "snob_list",
            serde_json::json!({ "list": "following", "account": "someone" }),
        ),
        (
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone", "mode": "refresh" }),
        ),
    ] {
        let refused = client.call(tool, arguments).await;
        assert_eq!(refused["isError"], true, "{refused}");
        let told = &refused["structuredContent"];
        assert_eq!(told["exit"], "rate_limited", "{refused}");
        assert!(told.get("confirmation").is_none(), "asked: {refused}");
        assert!(
            told["error"]["cooldown_until"].as_i64().is_some(),
            "{refused}"
        );
    }

    let served = confirmed_in_chat(
        &mut client,
        "snob_list",
        serde_json::json!({ "list": "followers", "account": "someone" }),
    )
    .await;
    assert_eq!(served["isError"], false, "{served}");
    let told = &served["structuredContent"];
    assert_eq!(told["count"], 3, "{served}");
    // Handed over as the stored list it is, with its date and the
    // cooldown's end, in the JSON and in a sentence.
    assert_eq!(told["served"][0]["from"], "stored_in_cooldown", "{served}");
    assert!(told["served"][0]["taken_at"].as_i64().is_some(), "{served}");
    assert!(told["cooldown_until"].as_i64().is_some(), "{served}");
    let sentence = served["content"][1]["text"].as_str().unwrap_or_default();
    assert!(sentence.contains("Not read now"), "{served}");

    // Asked for as stored, the same: dated, with the cooldown's end.
    let stored = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone", "mode": "stored" }),
        )
        .await;
    assert_eq!(stored["isError"], false, "{stored}");
    let told = &stored["structuredContent"];
    assert!(told["served"][0]["taken_at"].as_i64().is_some(), "{stored}");
    assert!(told["cooldown_until"].as_i64().is_some(), "{stored}");
    assert!(stored["content"][1]["text"].is_string(), "{stored}");

    // An estimate in the cooldown is the answer asked for, not an error.
    let estimate = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "dry_run": true }),
        )
        .await;
    assert_eq!(estimate["isError"], false, "{estimate}");
    assert_eq!(
        estimate["structuredContent"]["sends_now"], false,
        "{estimate}"
    );
    assert_eq!(acts_recorded(tmp.path()), 0, "nothing was read or sent");
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");
}

/// `snob status` shows what agents were told yes to in the last day: what,
/// how the yes was given and through which client, an attempt that came to
/// nothing included; and the agent's own `snob_status` says the same.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn status_shows_what_an_agent_was_told_yes_to() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    serve_an_unfollow(&instagram).await;
    log_in_writing(tmp.path(), &instagram);
    common::remember_doc_ids(tmp.path());
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };

    let mut chat = McpClient::start(tmp.path(), &instagram, &["--allow-writes"]).await;
    let done = confirmed_in_chat(
        &mut chat,
        "snob_unfollow",
        serde_json::json!({ "account": "@someone" }),
    )
    .await;
    assert_eq!(done["isError"], false, "{done}");
    // Any other name is the session's own account to the fake Instagram, and
    // the command refuses to unfollow yourself after its first request: an
    // attempt, recorded all the same.
    let own = confirmed_in_chat(
        &mut chat,
        "snob_unfollow",
        serde_json::json!({ "account": "nobody" }),
    )
    .await;
    assert_eq!(own["isError"], true, "{own}");
    assert_eq!(writes_sent(&instagram).await, 1);

    // Asked in a form, on the older revision, while the call waits.
    let mut form = McpClient::start_with(
        tmp.path(),
        &instagram,
        &[],
        serde_json::json!({ "elicitation": {} }),
    )
    .await;
    form.answer = Some(serde_json::json!({ "action": "accept", "content": { "confirm": true } }));
    let walked = form
        .call(
            "snob_list",
            serde_json::json!({ "list": "fans", "account": "other" }),
        )
        .await;
    assert_eq!(walked["isError"], false, "{walked}");
    // What needs no yes is not recorded.
    let own_list = form
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(own_list["isError"], false, "{own_list}");

    let out = snob(tmp.path(), Some(&instagram), &["status", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let agents = &json(&out)["agents"];
    assert_eq!(agents["writes"], 2, "{agents}");
    let acts = agents["acts"].as_array().unwrap();
    assert_eq!(acts.len(), 3, "{agents}");
    assert_eq!(acts[0]["action"], "unfollow");
    assert_eq!(acts[0]["write"], true);
    assert_eq!(acts[0]["target"]["username"], "someone");
    assert_eq!(acts[0]["target"]["pk"], 9001, "resolved once looked up");
    assert_eq!(acts[0]["via"], "chat");
    assert_eq!(acts[0]["client"], "test 0");
    assert_eq!(acts[1]["target"]["username"], "nobody");
    assert_eq!(acts[2]["action"], "fans");
    assert_eq!(acts[2]["write"], false);
    assert_eq!(acts[2]["target"]["username"], "other");
    assert_eq!(acts[2]["via"], "elicitation");

    let out = snob(tmp.path(), Some(&instagram), &["status", "--agents"]);
    let said = stdout(&out);
    assert!(said.starts_with("Agents\n"), "{said}");
    assert!(said.contains("2 in the last 24 hours"), "{said}");
    assert!(
        said.contains("unfollow @someone, yes given in the chat, through test 0"),
        "{said}"
    );
    assert!(
        said.contains("read @other's fans, yes given in a form, through test 0"),
        "{said}"
    );

    let status = chat.call("snob_status", serde_json::json!({})).await;
    assert_eq!(
        status["structuredContent"]["agents"]["writes"], 2,
        "{status}"
    );
}

// ---------------------------------------------------------------------------
// Jobs: a walk that outlives the call that started it (`jobs`).
//
// **Every test here stops its jobs and waits for their locks before its
// `TempDir` drops** (`Jobs`, below, on drop, a panic included): a job is a
// detached process, and one left running would go on writing into a
// directory being removed, and on Windows keep it from being removed.
// ---------------------------------------------------------------------------

/// A fake Instagram whose followers list answers after `delay`, the first
/// `times` it is asked, and at once after that: a walk that is still going
/// when it is asked about, and one that finishes when it is walked again.
async fn fake_instagram_slow(followers: u64, delay: std::time::Duration, times: u64) -> MockServer {
    let server = fake_instagram(followers, 2).await;
    let users: Vec<String> = (0..followers)
        .map(|n| format!(r#"{{"pk":{},"username":"user{n}"}}"#, 1_000 + n))
        .collect();
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(",")))
                .set_delay(delay),
        )
        .up_to_n_times(times)
        .with_priority(1)
        .mount(&server)
        .await;
    server
}

/// The walk the tests start as a job, as the MCP server will start it.
const FOLLOWERS_JOB: [&str; 6] = [
    "--",
    "followers",
    "--format",
    "json",
    "--no-interactive",
    "--no-progress",
];

/// How many times the walk asked for a page of followers.
async fn followers_asked(instagram: &MockServer) -> usize {
    instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path().ends_with("/followers/"))
        .count()
}

/// Waits until the walk has asked for a page `at_least` times.
async fn until_asked(instagram: &MockServer, at_least: usize) {
    let since = std::time::Instant::now();
    while followers_asked(instagram).await < at_least {
        assert!(
            since.elapsed() < std::time::Duration::from_secs(60),
            "the job's walk never asked for its page"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The jobs of one sandbox, stopped and waited for when this goes.
struct Jobs<'a> {
    root: &'a Path,
    instagram: &'a MockServer,
}

impl Jobs<'_> {
    /// `snob __job start --wait <wait> <command>`, and the one document it
    /// answers with.
    fn start(&self, wait: &str, command: &[&str]) -> (Output, Option<serde_json::Value>) {
        let mut args = vec!["__job", "start", "--wait", wait, "--tool", "snob_list"];
        args.extend_from_slice(command);
        let out = snob(self.root, Some(self.instagram), &args);
        let told = serde_json::from_str(&stdout(&out)).ok();
        (out, told)
    }

    /// A start that has to have answered: its record.
    fn started(&self, wait: &str, command: &[&str]) -> serde_json::Value {
        let (out, told) = self.start(wait, command);
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
        let told = told.unwrap_or_else(|| panic!("one JSON document: {}", stdout(&out)));
        assert_eq!(
            stdout(&out).trim().lines().count(),
            1,
            "exactly one compact document: {}",
            stdout(&out)
        );
        told
    }

    /// `snob __job <verb> …`, read as its JSON.
    fn ask(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["__job"];
        all.extend_from_slice(args);
        let out = snob(self.root, Some(self.instagram), &all);
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
        json(&out)
    }

    /// The job, once it is over, within a minute.
    fn finished(&self, id: &str) -> serde_json::Value {
        let told = self.ask(&["show", id, "--wait", "60s", "--json"]);
        assert!(
            !matches!(told["status"].as_str(), Some("running" | "queued")),
            "still going after a minute: {told}"
        );
        told
    }

    /// Every job of every account of the sandbox's, by its directory.
    fn dirs(&self) -> Vec<std::path::PathBuf> {
        let app = snob_store::paths::AppPaths::rooted_at(self.root);
        app.account_dirs()
            .into_iter()
            .flat_map(|pk| {
                std::fs::read_dir(app.account(pk).jobs_dir())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.path())
            })
            .collect()
    }
}

impl Drop for Jobs<'_> {
    fn drop(&mut self) {
        for dir in self.dirs() {
            let _ = std::fs::write(dir.join("stop"), b"");
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while self.dirs().iter().any(|dir| {
            snob_store::paths::lock_is_held(&dir.join("lock"))
                .ok()
                .flatten()
                .unwrap_or(false)
        }) {
            if std::time::Instant::now() > deadline {
                // Not a panic inside a drop that may be unwinding already.
                eprintln!("a job did not stop within a minute");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// A walk that ends within the wait is answered with its result, inline,
/// as one document: the record, and the walk's own answer under `finished`.
#[tokio::test]
async fn a_job_that_finishes_within_the_wait_answers_inline() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };

    let told = jobs.started("60s", &FOLLOWERS_JOB);
    assert_eq!(told["status"], "succeeded", "{told}");
    assert_eq!(told["reused"], false);
    assert_eq!(told["command"], "followers");
    assert_eq!(told["tool"], "snob_list");
    assert!(told["detached"].is_string(), "{told}");
    assert!(told["estimate"]["dry_run"] == true, "{told}");
    assert_eq!(told["finished"]["exit"], "ok", "{told}");
    let output = told["finished"]["output"].to_string();
    for user in ["user0", "user1", "user2"] {
        assert!(output.contains(user), "{output}");
    }
    let id = told["job_id"].as_str().unwrap();
    assert!(snob_store::paths::JobId::parse(id).is_some(), "{id}");
    // Its walk said how it went, a line a page, and asked once.
    assert!(told["progress"]["event"] == "finished", "{told}");
    assert_eq!(followers_asked(&instagram).await, 1);
    // `--no-progress` was taken out of what it ran.
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(jobs.dirs()[0].join("job.json")).unwrap()).unwrap();
    assert!(
        !record["argv"].to_string().contains("no-progress"),
        "{record}"
    );
}

/// A walk that takes longer than the wait is answered with its record while
/// it runs, and asked about later gives what the walk answered.
#[tokio::test]
async fn a_job_polled_to_its_end_gives_the_walks_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(3), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };

    let told = jobs.started("0s", &FOLLOWERS_JOB);
    assert!(
        matches!(told["status"].as_str(), Some("running" | "queued")),
        "{told}"
    );
    assert!(told["finished"].is_null(), "{told}");
    let id = told["job_id"].as_str().unwrap().to_string();

    // The person sees it, in `snob jobs` and in `snob status`.
    let listed = jobs.ask(&["list", "--json"]);
    assert_eq!(listed["jobs"][0]["job_id"], id.as_str(), "{listed}");
    let out = snob(tmp.path(), Some(&instagram), &["jobs"]);
    assert!(stdout(&out).contains(&id), "{}", stdout(&out));
    assert!(stdout(&out).contains("your followers"), "{}", stdout(&out));
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["status", "--jobs", "--json"],
    );
    assert_eq!(json(&out)["jobs"]["running"][0]["job_id"], id.as_str());

    let done = jobs.finished(&id);
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(done["finished"]["exit"], "ok");
    assert!(done["finished"]["output"].to_string().contains("user2"));
    // And `snob jobs show` says the same to a person.
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["jobs", "show", &id, "--json"],
    );
    assert_eq!(json(&out)["status"], "succeeded");
    // Over, it is no longer one `snob jobs` lists, and is with `--all`.
    assert_eq!(jobs.ask(&["list", "--json"])["jobs"], serde_json::json!([]));
    assert_eq!(
        jobs.ask(&["list", "--all", "--json"])["jobs"][0]["job_id"],
        id.as_str()
    );
}

/// Canceling a job stops its walk as Ctrl+C does, which lets go of the list,
/// so the next walk of it is not refused as walked elsewhere.
#[tokio::test]
async fn canceling_a_job_stops_it_like_ctrl_c_and_the_walk_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };

    let id = jobs.started("0s", &FOLLOWERS_JOB)["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    until_asked(&instagram, 1).await;
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["jobs", "cancel", &id, "--wait", "60s", "--json"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let canceled = json(&out);
    assert_eq!(canceled["status"], "canceled", "{canceled}");
    assert_eq!(canceled["exit_number"], 130, "{canceled}");
    assert_eq!(canceled["finished"]["exit"], "interrupted");
    // Asked again, nothing changes: it is over.
    let again = jobs.ask(&["cancel", &id, "--json"]);
    assert_eq!(again["status"], "canceled");

    // The list was let go of, so it is walked again, and this time answers.
    let told = jobs.started("60s", &FOLLOWERS_JOB);
    assert_eq!(told["status"], "succeeded", "{told}");
    assert_ne!(told["job_id"], id.as_str());
}

/// The process that started a job is gone long before the walk ends, and the
/// walk goes on without it.
#[tokio::test]
async fn a_job_outlives_the_process_that_started_it() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(5), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };

    let since = std::time::Instant::now();
    let told = jobs.started("0s", &FOLLOWERS_JOB);
    assert!(
        since.elapsed() < std::time::Duration::from_secs(5),
        "the start waited for the walk"
    );
    let id = told["job_id"].as_str().unwrap().to_string();
    assert_eq!(jobs.finished(&id)["status"], "succeeded");
}

/// A start whose standard input closes before it has answered, as when the
/// tool call is canceled or the client goes away, stops the job it made:
/// nobody would ever be told its id.
#[tokio::test]
async fn closing_starts_stdin_before_it_answered_stops_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };

    let mut args = vec![
        "--stop-when-stdin-closes",
        "__job",
        "start",
        "--wait",
        "600s",
    ];
    args.extend_from_slice(&FOLLOWERS_JOB);
    let mut child = command(tmp.path(), Some(&instagram))
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the binary runs");
    until_asked(&instagram, 1).await;
    drop(child.stdin.take());
    let out = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    assert_eq!(stdout(&out), "", "no id was handed over");

    let listed = jobs.ask(&["list", "--all", "--json"]);
    let id = listed["jobs"][0]["job_id"].as_str().unwrap().to_string();
    // Already, without waiting: the start saw it stop before it left.
    let over = jobs.ask(&["show", &id, "--json"]);
    assert_eq!(over["status"], "canceled", "{over}");
    assert_eq!(over["exit_number"], 130, "{over}");
}

/// `purge` stops the jobs before anything goes: they are listed in what it
/// will do, stopped, and only then is everything removed. (That the owner of
/// the browsers is released after them is the order of `purge.rs`; no
/// browser runs here to tell.)
#[tokio::test]
async fn purge_lists_then_stops_running_jobs_before_removing_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let id = jobs.started("0s", &FOLLOWERS_JOB)["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    until_asked(&instagram, 1).await;

    // Listed, and a dry run stops nothing.
    let out = snob(tmp.path(), Some(&instagram), &["purge", "--dry-run"]);
    assert!(stdout(&out).contains(&id), "{}", stdout(&out));
    assert_eq!(jobs.ask(&["show", &id, "--json"])["status"], "running");

    let out = snob(tmp.path(), Some(&instagram), &["purge", "--yes"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("Stopped 1 job first."),
        "{}",
        stdout(&out)
    );
    assert!(
        !tmp.path().join("data").exists(),
        "the data directory, the job's with it, is gone"
    );
}

/// A job start that waited on the jobs lock while the session was deleted,
/// as a purge and a logout delete it holding that lock, makes no job and
/// says there is no session, rather than starting a walk in what is going.
#[tokio::test]
async fn a_start_that_waited_while_the_session_went_makes_no_job() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let account = snob_store::paths::AppPaths::rooted_at(tmp.path()).account(PK);
    snob_store::paths::create_private_dir(&account.jobs_dir()).unwrap();
    let lock = snob_store::paths::try_lock_file(&account.jobs_dir().join(".lock"))
        .unwrap()
        .expect("nothing holds it yet");

    let mut start = command(tmp.path(), Some(&instagram));
    start
        .args(["__job", "start", "--wait=0s"])
        .args(FOLLOWERS_JOB)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = start.spawn().unwrap();
    // Long enough for it to pass its first look at the session and wait on
    // the lock; a start that has not got there yet finds no session either.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let session = account.session_file();
    assert!(session.exists(), "the sandbox keeps the session in a file");
    std::fs::remove_file(&session).unwrap();
    drop(lock);

    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let made: Vec<_> = std::fs::read_dir(account.jobs_dir())
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .collect();
    assert!(made.is_empty(), "no job was made: {made:?}");
}

/// A jobs directory that cannot be made keeps neither `logout` nor `purge`
/// from deleting the session: there are no jobs to stop there, and none
/// could start.
#[tokio::test]
async fn a_jobs_directory_that_cannot_be_made_keeps_no_session() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mine = snob_store::paths::AppPaths::rooted_at(tmp.path()).account(PK);

    log_in(tmp.path(), &instagram);
    std::fs::write(mine.jobs_dir(), b"x").unwrap();
    assert!(mine.session_file().exists());
    let out = snob(tmp.path(), Some(&instagram), &["logout"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(!mine.session_file().exists(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("could not hold the jobs"),
        "{}",
        stderr(&out)
    );

    std::fs::remove_file(mine.jobs_dir()).unwrap();
    log_in(tmp.path(), &instagram);
    std::fs::write(mine.jobs_dir(), b"x").unwrap();
    let pk = PK.get().to_string();
    let out = snob(
        tmp.path(),
        Some(&instagram),
        &["purge", "--account", &pk, "--yes"],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(!mine.session_file().exists(), "{}", stderr(&out));
    assert!(!mine.db_file().exists(), "{}", stderr(&out));
}

/// `logout` stops the account's jobs too: a walk would otherwise go on with
/// a session being deleted.
#[tokio::test]
async fn logout_stops_running_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let id = jobs.started("0s", &FOLLOWERS_JOB)["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    until_asked(&instagram, 1).await;

    let out = snob(tmp.path(), Some(&instagram), &["logout"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("Stopped 1 job first."),
        "{}",
        stdout(&out)
    );
    // Read straight from its files: with nobody signed in, no command acts as
    // the account any more.
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(jobs.dirs()[0].join("state.json")).unwrap()).unwrap();
    assert_eq!(state["status"], "canceled", "{state}");
    assert!(jobs.dirs()[0].ends_with(&id));
}

/// One job runs at a time per account: the same walk again is the job already
/// running, and another walk is refused with nothing started.
#[tokio::test]
async fn a_second_job_for_the_account_is_refused_or_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let first = jobs.started("0s", &FOLLOWERS_JOB);
    let again = jobs.started("0s", &FOLLOWERS_JOB);
    assert_eq!(again["job_id"], first["job_id"]);
    assert_eq!(again["reused"], true);

    let (out, _) = jobs.start(
        "0s",
        &["--", "following", "--format", "json", "--no-interactive"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    let told: serde_json::Value =
        serde_json::from_str(stderr(&out).trim().lines().last().unwrap()).unwrap();
    assert_eq!(told["error"]["reason"], "too_many_jobs", "{told}");
    assert_eq!(jobs.dirs().len(), 1);
}

/// A job asked to stop that has not stopped yet is never handed to a new
/// start of its walk, which would be given a walk about to end `canceled`:
/// the start is refused as `job_stopping` with nothing started, and its
/// `--check` says the same, writing nothing.
#[tokio::test]
async fn a_job_asked_to_stop_is_not_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let walk = &FOLLOWERS_JOB;
    // Free to start, and the check says so.
    let (out, told) = jobs.start("0s", &[&["--check"][..], &walk[..]].concat());
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(told.unwrap()["would"], "start");
    assert!(jobs.dirs().is_empty(), "the check made nothing");

    // A finished job made to stand as one still running and asked to stop:
    // its ending taken away, its lock held here as its supervisor's would
    // be, and a stop file written.
    let id = jobs.started("60s", walk)["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dir = jobs.dirs()[0].clone();
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    state["status"] = "running".into();
    state["finished_at"] = serde_json::Value::Null;
    std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
    std::fs::write(dir.join("stop"), b"").unwrap();
    let held = snob_store::paths::try_lock_file(&dir.join("lock"))
        .unwrap()
        .expect("nobody holds the job's lock");
    let standing = jobs.ask(&["show", &id, "--json"]);
    assert_eq!(standing["status"], "running", "{standing}");
    assert_eq!(standing["stop_asked"], true, "{standing}");

    for check in [true, false] {
        let mut line: Vec<&str> = if check { vec!["--check"] } else { Vec::new() };
        line.extend_from_slice(walk);
        let (out, told) = jobs.start("0s", &line);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(told.is_none(), "{}", stdout(&out));
        let told: serde_json::Value =
            serde_json::from_str(stderr(&out).trim().lines().last().unwrap()).unwrap();
        assert_eq!(told["error"]["reason"], "job_stopping", "{told}");
        assert!(
            told["error"]["message"].as_str().unwrap().contains(&id),
            "{told}"
        );
    }
    assert_eq!(jobs.dirs().len(), 1, "nothing was started");
    drop(held);
}

/// Two starts at once make one job: the count and the making are one step,
/// under the account's jobs lock.
#[tokio::test]
async fn two_concurrent_starts_make_one_job() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let spawn = || {
        let mut args = vec!["__job", "start", "--wait", "0s"];
        args.extend_from_slice(&FOLLOWERS_JOB);
        command(tmp.path(), Some(&instagram))
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the binary runs")
    };
    let (one, two) = (spawn(), spawn());
    let one = json(&one.wait_with_output().unwrap());
    let two = json(&two.wait_with_output().unwrap());
    assert_eq!(one["job_id"], two["job_id"], "{one} {two}");
    assert_ne!(one["reused"], two["reused"], "one made it, one found it");
    assert_eq!(jobs.dirs().len(), 1);
}

/// A job is looked for in the account's own jobs alone: an agent served for
/// one account cannot read, or stop, another account's walk by its id.
#[tokio::test]
async fn a_job_of_another_account_is_no_such_job() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = two_accounts(tmp.path()).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(120)))
        .with_priority(1)
        .mount(&instagram)
        .await;
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut args = vec!["--account", "other", "__job", "start", "--wait", "0s"];
    args.extend_from_slice(&FOLLOWERS_JOB);
    let out = snob(tmp.path(), Some(&instagram), &args);
    assert!(out.status.success(), "{}", stderr(&out));
    let id = json(&out)["job_id"].as_str().unwrap().to_string();

    for verb in ["show", "cancel"] {
        let out = snob(
            tmp.path(),
            Some(&instagram),
            &["--account", "me", "__job", verb, &id, "--json"],
        );
        assert_eq!(out.status.code(), Some(1), "{verb}: {}", stdout(&out));
        let told: serde_json::Value =
            serde_json::from_str(stderr(&out).trim().lines().last().unwrap()).unwrap();
        assert_eq!(told["error"]["reason"], "no_such_job", "{told}");
    }
    let theirs = snob(
        tmp.path(),
        Some(&instagram),
        &["--account", "other", "__job", "show", &id, "--json"],
    );
    let theirs = json(&theirs);
    assert!(
        matches!(theirs["status"].as_str(), Some("running" | "queued")),
        "the cancel as another account stopped nothing: {theirs}"
    );
    assert!(!jobs.dirs()[0].join("stop").exists());
}

/// A job's record is a file anybody running as this user can edit; what it
/// says to run is checked again where it runs, and a command no job may run
/// runs nothing.
#[tokio::test]
async fn a_hand_edited_job_json_runs_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let told = jobs.started("60s", &FOLLOWERS_JOB);
    assert_eq!(told["status"], "succeeded");
    let dir = jobs.dirs()[0].clone();
    let before = instagram
        .received_requests()
        .await
        .unwrap_or_default()
        .len();

    for argv in [
        serde_json::json!(["unfollow", "someone", "-y", "--json"]),
        serde_json::json!(["followers", "-o", "elsewhere.json"]),
        serde_json::json!(["followers", "--sandbox-root", "elsewhere"]),
    ] {
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("job.json")).unwrap()).unwrap();
        record["argv"] = argv.clone();
        std::fs::write(dir.join("job.json"), record.to_string()).unwrap();
        for name in ["state.json", "out.json"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let id = dir.file_name().unwrap().to_str().unwrap();
        let out = snob(tmp.path(), Some(&instagram), &["__job", "run", id]);
        assert!(out.status.success(), "{argv}: {}", stderr(&out));
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(state["status"], "failed", "{argv}: {state}");
        assert_eq!(state["failure"], "refused_argv", "{argv}: {state}");
    }
    assert_eq!(
        instagram
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        before,
        "nothing was sent"
    );
    assert!(!tmp.path().join("elsewhere").exists());
}

/// A job that died without letting go of its list, killed with its client,
/// holds the list until its claim runs out; a start of a walk of that list
/// says until when, and starts nothing. Another list is walked.
#[tokio::test]
async fn a_lost_job_with_a_live_claim_says_when_it_frees() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    // A finished job, made lost by hand: its ending taken away, as though
    // its supervisor had been killed while it ran.
    let told = jobs.started("60s", &FOLLOWERS_JOB);
    let id = told["job_id"].as_str().unwrap().to_string();
    let dir = jobs.dirs()[0].clone();
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
    state["status"] = "running".into();
    state["finished_at"] = serde_json::Value::Null;
    state["updated_at"] = snob_core::clock::now_ms().get().into();
    std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
    assert_eq!(jobs.ask(&["show", &id, "--json"])["status"], "lost");

    let (out, _) = jobs.start("0s", &["--", "unfollowers", "--format", "json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let told: serde_json::Value =
        serde_json::from_str(stderr(&out).trim().lines().last().unwrap()).unwrap();
    assert_eq!(told["error"]["reason"], "walked_elsewhere", "{told}");
    let said = told["error"]["message"].as_str().unwrap();
    assert!(said.contains(&id) && said.contains("until"), "{said}");
    // The same moment for a program, which the lost job's record names too.
    // `retry_at` is rounded up to the second, so it is never too early.
    let frees = jobs.ask(&["show", &id, "--json"])["claim_frees_at"]
        .as_i64()
        .unwrap();
    let retry_at = told["error"]["retry_at"].as_i64().expect("retry_at");
    assert!((frees..=frees + 1).contains(&retry_at), "{told}");
    assert_eq!(jobs.dirs().len(), 1, "nothing was started");

    // A list it never walked is another matter.
    let told = jobs.started(
        "60s",
        &["--", "following", "--format", "json", "--no-interactive"],
    );
    assert_eq!(told["status"], "succeeded", "{told}");
}

// ---------------------------------------------------------------------------
// The jobs through `snob mcp`: a list call starts its walk as a job, and the
// job tools read it back. The same cleanup rule as above: a `Jobs` guard,
// declared after the `TempDir`, stops them before it goes.
// ---------------------------------------------------------------------------

/// The text blocks of a tool result, in one string.
#[cfg(feature = "mcp")]
fn said(result: &serde_json::Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A list walk that ends within the call's wait is answered as it always
/// was, with the job it ran as beside it: the walk is a job all the same,
/// kept, and seen by the person.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_answers_a_quick_walk_inline_with_its_job_id() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let followers = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(followers["isError"], false, "{followers}");
    let told = &followers["structuredContent"];
    assert_eq!(told["count"], 3, "{told}");
    assert_eq!(usernames(told), ["user0", "user1", "user2"]);
    let id = told["job_id"]
        .as_str()
        .expect("the job it ran as")
        .to_string();
    assert!(snob_store::paths::JobId::parse(&id).is_some(), "{id}");

    // Kept as a job of the account's, with the call it was started for.
    let listed = jobs.ask(&["list", "--all", "--json"]);
    let job = &listed["jobs"][0];
    assert_eq!(job["job_id"], id.as_str(), "{listed}");
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(job["tool"], "snob_list", "{job}");
    assert_eq!(
        job["arguments"],
        serde_json::json!({ "list": "followers" }),
        "{job}"
    );
    assert_eq!(job["started_by"], "test 0", "{job}");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(jobs.dirs()[0].join("job.json")).unwrap()).unwrap();
    // A job waits for the budget an hour when the server was given no wait.
    assert_eq!(record["max_wait_seconds"], 3600, "{record}");
    assert_eq!(followers_asked(&instagram).await, 1);
}

/// A walk that outlasts the call's wait is handed back as a job that is
/// still running, and `snob_job` gives its result once it has ended, as the
/// list call would have; `snob_jobs` lists it while it runs.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn the_mcp_server_hands_back_a_job_for_a_slow_walk_and_snob_job_gives_its_result() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(8), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let started = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 0 }),
        )
        .await;
    assert_eq!(started["isError"], false, "{started}");
    let told = &started["structuredContent"];
    assert!(
        matches!(told["status"].as_str(), Some("running" | "queued")),
        "{told}"
    );
    assert_eq!(told["done"], false, "{told}");
    assert_eq!(told["poll_with"], "snob_job", "{told}");
    assert_eq!(told["command"], "followers", "{told}");
    let id = told["job_id"].as_str().unwrap().to_string();
    let text = said(&started);
    assert!(
        text.contains(&format!("snob_job with \"job_id\": \"{id}\"")),
        "{text}"
    );

    let running = client.call("snob_jobs", serde_json::json!({})).await;
    assert_eq!(running["isError"], false, "{running}");
    assert_eq!(
        running["structuredContent"]["jobs"][0]["job_id"],
        id.as_str(),
        "{running}"
    );

    // The same call again is the same job, not a second walk.
    let again = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 0 }),
        )
        .await;
    assert_eq!(again["structuredContent"]["job_id"], id.as_str(), "{again}");
    assert_eq!(again["structuredContent"]["reused"], true, "{again}");

    let done = client
        .call(
            "snob_job",
            serde_json::json!({ "job_id": id, "wait_seconds": 50 }),
        )
        .await;
    assert_eq!(done["isError"], false, "{done}");
    let told = &done["structuredContent"];
    assert_eq!(told["count"], 3, "{told}");
    assert_eq!(usernames(told), ["user0", "user1", "user2"]);
    assert_eq!(told["job_id"], id.as_str(), "{told}");
    assert_eq!(followers_asked(&instagram).await, 1, "walked once");

    // Ended, it is listed only with all.
    let listed = client.call("snob_jobs", serde_json::json!({})).await;
    assert_eq!(
        listed["structuredContent"]["jobs"],
        serde_json::json!([]),
        "{listed}"
    );
    let every = client
        .call("snob_jobs", serde_json::json!({ "all": true }))
        .await;
    assert_eq!(
        every["structuredContent"]["jobs"][0]["status"], "succeeded",
        "{every}"
    );
}

/// `snob_job_cancel` stops the walk as Ctrl+C does, keeping what it read and
/// letting go of the list, so the next call walks it again.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn snob_job_cancel_stops_the_walk_and_it_resumes_on_the_next_call() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let started = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 0 }),
        )
        .await;
    let id = started["structuredContent"]["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a job: {started}"))
        .to_string();
    until_asked(&instagram, 1).await;

    let canceled = client
        .call("snob_job_cancel", serde_json::json!({ "job_id": id }))
        .await;
    assert_eq!(canceled["isError"], false, "{canceled}");
    assert_eq!(
        canceled["structuredContent"]["stop_asked"], true,
        "{canceled}"
    );
    // Stopped as Ctrl+C stops a walk: interrupted, not a failure of its own.
    let stopped = client
        .call(
            "snob_job",
            serde_json::json!({ "job_id": id, "wait_seconds": 50 }),
        )
        .await;
    assert_eq!(stopped["isError"], true, "{stopped}");
    assert_eq!(
        stopped["structuredContent"]["exit"], "interrupted",
        "{stopped}"
    );
    assert_eq!(stopped["structuredContent"]["job_id"], id.as_str());
    // Canceled again, it says it had stopped and changes nothing.
    let again = client
        .call("snob_job_cancel", serde_json::json!({ "job_id": id }))
        .await;
    assert_eq!(again["structuredContent"]["status"], "canceled", "{again}");
    assert_eq!(again["structuredContent"]["done"], true, "{again}");

    // The list was let go of: walked again, and this time it answers.
    let walked = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(walked["isError"], false, "{walked}");
    assert_eq!(walked["structuredContent"]["count"], 3, "{walked}");
    assert_ne!(walked["structuredContent"]["job_id"], id.as_str());
}

/// A job outlives the server that started it, and another server, of the
/// same account, reads it back by its id.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_job_started_by_one_server_is_read_by_another() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(5), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut first = McpClient::start(tmp.path(), &instagram, &[]).await;
    let started = first
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 0 }),
        )
        .await;
    let id = started["structuredContent"]["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a job: {started}"))
        .to_string();
    // The server, and the client with it, gone.
    drop(first);

    let mut second = McpClient::start(tmp.path(), &instagram, &[]).await;
    let done = second
        .call(
            "snob_job",
            serde_json::json!({ "job_id": id, "wait_seconds": 50 }),
        )
        .await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(done["structuredContent"]["count"], 3, "{done}");
}

/// A list call canceled while it waits for its walk stops the job it
/// started: its id was never handed over, so nobody could ask for it.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_canceled_call_cancels_the_job_it_had_not_handed_over() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let call = client
        .send_raw(
            "tools/call",
            serde_json::json!({
                "name": "snob_list",
                "arguments": { "list": "followers", "wait_seconds": 50 }
            }),
        )
        .await;
    until_asked(&instagram, 1).await;
    client
        .send(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled", // spelling-allow: the protocol's name
            "params": { "requestId": call, "reason": "the person moved on" }
        }))
        .await;

    let since = std::time::Instant::now();
    let over = loop {
        let listed = jobs.ask(&["list", "--all", "--json"]);
        let job = listed["jobs"][0].clone();
        if !matches!(job["status"].as_str(), Some("running" | "queued")) {
            break job;
        }
        assert!(
            since.elapsed() < std::time::Duration::from_secs(60),
            "the job was not stopped: {job}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    assert_eq!(over["status"], "canceled", "{over}");
    // And the server goes on.
    let status = client.call("snob_status", serde_json::json!({})).await;
    assert_eq!(status["isError"], false, "{status}");
}

/// `snob_job` takes a job's id and nothing else, and a well-formed id this
/// account has no job of is "no such job", from the child that looked.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn snob_job_refuses_an_id_that_is_not_one() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;
    let before = posted(&instagram).await.len();

    for id in ["--all", "../x", "j1"] {
        for tool in ["snob_job", "snob_job_cancel"] {
            let refused = client.call(tool, serde_json::json!({ "job_id": id })).await;
            assert_eq!(refused["isError"], true, "{tool} {id}: {refused}");
            assert_eq!(
                refused["structuredContent"]["error"]["code"], "invalid_arguments",
                "{tool} {id}: {refused}"
            );
        }
    }
    let nobody = format!("j{}", "0".repeat(26));
    for tool in ["snob_job", "snob_job_cancel"] {
        let unknown = client
            .call(tool, serde_json::json!({ "job_id": nobody }))
            .await;
        assert_eq!(unknown["isError"], true, "{tool}: {unknown}");
        assert_eq!(
            unknown["structuredContent"]["error"]["code"], "no_such_job",
            "{tool}: {unknown}"
        );
    }
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");
}

/// A finished job's result is handed back a page at a time from the job's
/// own file, by a server that never held it, and sends nothing.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn snob_job_pages_a_finished_result_from_its_file() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(450, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut first = McpClient::start(tmp.path(), &instagram, &[]).await;
    let walked = first
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(walked["isError"], false, "{walked}");
    let id = walked["structuredContent"]["job_id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(first);

    let sent = posted(&instagram).await.len();
    let mut other = McpClient::start(tmp.path(), &instagram, &[]).await;
    let mut read = Vec::new();
    let mut offset = 0;
    loop {
        let page = other
            .call(
                "snob_job",
                serde_json::json!({ "job_id": id, "offset": offset }),
            )
            .await;
        assert_eq!(page["isError"], false, "{page}");
        let told = &page["structuredContent"];
        assert_eq!(told["count"], 450, "{told}");
        assert_eq!(told["offset"], offset, "{told}");
        read.extend(usernames(told));
        match told["next_offset"].as_u64() {
            Some(next) => {
                let text = said(&page);
                assert!(
                    text.contains("call snob_job again")
                        && text.contains(&format!("\"offset\": {next}")),
                    "{text}"
                );
                offset = next;
            }
            None => break,
        }
    }
    let every: Vec<String> = (0..450).map(|n| format!("user{n}")).collect();
    assert_eq!(read, every, "every account once, in the order served");
    assert_eq!(posted(&instagram).await.len(), sent, "nothing was sent");
}

/// What a client that takes MCP Tasks declares, at `initialize` or in the
/// `_meta` of a stateless request.
#[cfg(feature = "mcp")]
fn takes_tasks() -> serde_json::Value {
    serde_json::json!({ "extensions": { "io.modelcontextprotocol/tasks": {} } })
}

#[cfg(feature = "mcp")]
impl McpClient {
    /// A client on 2025-11-25 that takes tasks, having checked that the
    /// server says it hands them out.
    async fn taking_tasks(root: &Path, instagram: &MockServer, args: &[&str]) -> Self {
        let mut client = Self::spawn(root, instagram, args);
        let hello = client
            .ask(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": takes_tasks(),
                    "clientInfo": { "name": "test", "version": "0" }
                }),
            )
            .await;
        assert!(
            hello["result"]["capabilities"]["extensions"]["io.modelcontextprotocol/tasks"]
                .is_object(),
            "the server declares tasks: {hello}"
        );
        client
            .send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        client
    }

    /// `tasks/get` until the task is no longer working, within a minute and
    /// a half: the task as it ended.
    async fn task_over(&mut self, id: &str) -> serde_json::Value {
        let since = std::time::Instant::now();
        loop {
            let got = self
                .ask("tasks/get", serde_json::json!({ "taskId": id }))
                .await;
            let task = got["result"].clone();
            assert!(task.is_object(), "a task: {got}");
            assert_eq!(task["taskId"], id, "{task}");
            if task["status"] != "working" {
                return task;
            }
            assert!(
                task["pollIntervalMs"]
                    .as_u64()
                    .is_some_and(|ms| ms >= 5_000),
                "a working task says when to ask again: {task}"
            );
            assert!(
                since.elapsed() < std::time::Duration::from_secs(90),
                "still working after a minute and a half: {task}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

/// A client that takes tasks is answered, for a walk that has not ended in a
/// few seconds, with a task whose id is the job's, whatever wait it asked
/// for, and `tasks/get` hands back the walk's result once it has ended, as
/// the call would have answered it.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_tasks_client_gets_a_task_for_a_slow_walk_and_its_result_from_tasks_get() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(20), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::taking_tasks(tmp.path(), &instagram, &[]).await;

    let asked = std::time::Instant::now();
    let started = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 50 }),
        )
        .await;
    assert!(
        asked.elapsed() < std::time::Duration::from_secs(19),
        "answered before the walk could end, not after the wait it asked: {:?}",
        asked.elapsed()
    );
    assert_eq!(started["resultType"], "task", "{started}");
    assert_eq!(started["status"], "working", "{started}");
    let id = started["taskId"].as_str().unwrap().to_string();
    assert!(
        snob_store::paths::JobId::parse(&id).is_some(),
        "the task is the job: {id}"
    );
    assert_eq!(started["ttlMs"], 7 * 24 * 3600 * 1000, "{started}");
    assert!(started["pollIntervalMs"].as_u64().is_some(), "{started}");
    assert!(
        started["statusMessage"]
            .as_str()
            .is_some_and(|said| said.contains("followers list")),
        "{started}"
    );
    assert!(chrono::DateTime::parse_from_rfc3339(started["createdAt"].as_str().unwrap()).is_ok());

    let ended = client.task_over(&id).await;
    assert_eq!(ended["status"], "completed", "{ended}");
    let result = &ended["result"];
    assert_eq!(result["isError"], false, "{ended}");
    let told = &result["structuredContent"];
    assert_eq!(told["count"], 3, "{told}");
    assert_eq!(usernames(told), ["user0", "user1", "user2"]);
    assert_eq!(told["job_id"], id.as_str(), "{told}");
    assert_eq!(followers_asked(&instagram).await, 1, "walked once");

    // The task is the job: snob_job reads it too, and nothing more is walked.
    let read = client
        .call("snob_job", serde_json::json!({ "job_id": id }))
        .await;
    assert_eq!(read["structuredContent"]["count"], 3, "{read}");
    assert_eq!(followers_asked(&instagram).await, 1);
}

/// `tasks/cancel` stops the walk as `snob_job_cancel` does: the task ends
/// canceled, what was read is kept, and the list is let go of, so the next
/// call walks it again. On the stateless revision, the client saying it
/// takes tasks in every request.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn tasks_cancel_stops_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::stateless(tmp.path(), &instagram, takes_tasks());

    let started = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    assert_eq!(started["resultType"], "task", "{started}");
    let id = started["taskId"].as_str().unwrap().to_string();
    until_asked(&instagram, 1).await;

    let canceled = client
        .ask("tasks/cancel", serde_json::json!({ "taskId": id }))
        .await;
    assert!(canceled["error"].is_null(), "{canceled}");
    assert!(canceled["result"].is_object(), "an empty ack: {canceled}");
    let ended = client.task_over(&id).await;
    let canceled = "cancelled"; // spelling-allow: the extension's word for it
    assert_eq!(ended["status"], canceled, "{ended}");
    assert!(ended.get("result").is_none(), "{ended}");

    // The list was let go of: walked again, and this time it ends.
    let again = client
        .call("snob_list", serde_json::json!({ "list": "followers" }))
        .await;
    let told = if again["resultType"] == "task" {
        let next = again["taskId"].as_str().unwrap().to_string();
        assert_ne!(next, id, "a walk of its own");
        let ended = client.task_over(&next).await;
        assert_eq!(ended["status"], "completed", "{ended}");
        ended["result"]["structuredContent"].clone()
    } else {
        again["structuredContent"].clone()
    };
    assert_eq!(told["count"], 3, "{told}");
}

/// A client that did not say it takes tasks gets the job's id, as before,
/// and is refused the task methods.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_client_without_tasks_gets_a_job_handle_instead() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(8), 1).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::start(tmp.path(), &instagram, &[]).await;

    let started = client
        .call(
            "snob_list",
            serde_json::json!({ "list": "followers", "wait_seconds": 0 }),
        )
        .await;
    assert!(started.get("taskId").is_none(), "{started}");
    assert_eq!(started["isError"], false, "{started}");
    let told = &started["structuredContent"];
    assert_eq!(told["poll_with"], "snob_job", "{told}");
    let id = told["job_id"].as_str().unwrap().to_string();

    let refused = client
        .ask("tasks/get", serde_json::json!({ "taskId": id }))
        .await;
    assert!(refused["error"].is_object(), "{refused}");

    let done = client
        .call(
            "snob_job",
            serde_json::json!({ "job_id": id, "wait_seconds": 50 }),
        )
        .await;
    assert_eq!(done["structuredContent"]["count"], 3, "{done}");
}

/// An id that names no job of the account, well formed or not, is an
/// unknown task to every task method, and runs nothing.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn an_unknown_task_is_invalid_params() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut client = McpClient::taking_tasks(tmp.path(), &instagram, &[]).await;

    let none_such = format!("j{}", "0".repeat(26));
    assert!(snob_store::paths::JobId::parse(&none_such).is_some());
    for id in ["nope", "--all", none_such.as_str()] {
        for (method, params) in [
            ("tasks/get", serde_json::json!({ "taskId": id })),
            ("tasks/cancel", serde_json::json!({ "taskId": id })),
            (
                "tasks/update",
                serde_json::json!({ "taskId": id, "inputResponses": {} }),
            ),
        ] {
            let answer = client.ask(method, params).await;
            assert_eq!(answer["error"]["code"], -32602, "{method} {id}: {answer}");
            assert!(
                answer["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.starts_with("unknown task")),
                "{answer}"
            );
        }
    }
    assert_eq!(followers_asked(&instagram).await, 0);
    assert!(jobs.dirs().is_empty(), "no job was made");
}

/// A server acting as one account does not know another account's job as a
/// task: it cannot read it or stop it by its id.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn a_task_of_another_account_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = two_accounts(tmp.path()).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v1/friendships/\d+/followers/$"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(120)))
        .with_priority(1)
        .mount(&instagram)
        .await;
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut args = vec!["--account", "other", "__job", "start", "--wait", "0s"];
    args.extend_from_slice(&FOLLOWERS_JOB);
    let out = snob(tmp.path(), Some(&instagram), &args);
    assert!(out.status.success(), "{}", stderr(&out));
    let id = json(&out)["job_id"].as_str().unwrap().to_string();

    let mut mine = McpClient::taking_tasks(tmp.path(), &instagram, &["--account", "me"]).await;
    for method in ["tasks/get", "tasks/cancel"] {
        let answer = mine.ask(method, serde_json::json!({ "taskId": id })).await;
        assert_eq!(answer["error"]["code"], -32602, "{method}: {answer}");
    }
    assert!(
        !jobs.dirs()[0].join("stop").exists(),
        "the cancel as another account stopped nothing"
    );

    let mut theirs = McpClient::taking_tasks(tmp.path(), &instagram, &["--account", "other"]).await;
    let got = theirs
        .ask("tasks/get", serde_json::json!({ "taskId": id }))
        .await;
    assert_eq!(got["result"]["status"], "working", "{got}");
}

/// A key in snob's form, as `snob mcp key new` makes one. A fixed one, since
/// these tests hand it over through `SNOB_MCP_KEY` and never make one: that
/// would write the system keyring.
#[cfg(feature = "mcp-http")]
const MCP_KEY: &str = "snob_mcp_N2u3l9q0aYyK0w7mQxk1vR4cT8bZp5sJdE6fGhIoUjA";

/// `show` says where the key comes from and names it by a fingerprint, and
/// with the key in `SNOB_MCP_KEY` warns that the variable overrides the
/// keyring. The key itself is printed only on request, and then only to a
/// terminal unless `--print` says otherwise; a key not in snob's form is
/// refused without being repeated.
///
/// Through the variable only, and so it reads nothing from the keyring: the
/// variable wins before the keyring is asked (`commands::mcp::key`).
#[cfg(feature = "mcp-http")]
#[test]
fn mcp_key_show_names_the_environment_and_prints_a_fingerprint() {
    let tmp = tempfile::tempdir().unwrap();
    let with_key = [("SNOB_MCP_KEY", MCP_KEY)];

    let out = snob_with(tmp.path(), None, &with_key, &["mcp", "key", "show"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(said.contains("SNOB_MCP_KEY"), "{said}");
    let fingerprint = said
        .split("fingerprint ")
        .nth(1)
        .map(|rest| rest.trim_end().trim_end_matches('.'))
        .unwrap_or_else(|| panic!("no fingerprint: {said}"));
    assert_eq!(fingerprint.len(), 12, "{said}");
    assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()), "{said}");
    assert!(!said.contains(MCP_KEY), "the key was printed: {said}");
    assert!(
        stderr(&out).contains("warning: SNOB_MCP_KEY is set"),
        "{}",
        stderr(&out)
    );

    // The same key gives the same fingerprint, every run.
    let again = snob_with(tmp.path(), None, &with_key, &["mcp", "key", "show"]);
    assert_eq!(stdout(&again), said);

    // Standard output here is a pipe, so the key is not shown without
    // --print, and nothing is printed instead of it.
    let refused = snob_with(
        tmp.path(),
        None,
        &with_key,
        &["mcp", "key", "show", "--reveal"],
    );
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(stdout(&refused).is_empty(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("--print"), "{}", stderr(&refused));
    assert!(!stderr(&refused).contains(MCP_KEY));

    let revealed = snob_with(
        tmp.path(),
        None,
        &with_key,
        &["mcp", "key", "show", "--reveal", "--print"],
    );
    assert!(revealed.status.success(), "{}", stderr(&revealed));
    assert_eq!(stdout(&revealed).trim_end(), MCP_KEY);
    assert!(
        stderr(&revealed).contains(fingerprint),
        "{}",
        stderr(&revealed)
    );

    let typo = format!("{MCP_KEY}x");
    let malformed = snob_with(
        tmp.path(),
        None,
        &[("SNOB_MCP_KEY", &typo)],
        &["mcp", "key", "show"],
    );
    assert_eq!(malformed.status.code(), Some(1), "{}", stderr(&malformed));
    assert!(stderr(&malformed).contains("SNOB_MCP_KEY"));
    assert!(
        !stderr(&malformed).contains(MCP_KEY) && !stdout(&malformed).contains(MCP_KEY),
        "the value was repeated: {}",
        stderr(&malformed)
    );

    let unparsed = snob(tmp.path(), None, &["mcp", "key", "show", "--print"]);
    assert_eq!(unparsed.status.code(), Some(2), "--print needs --reveal");
}

/// `revoke` with the key in `SNOB_MCP_KEY` says that a server started with
/// the variable still takes it: removing the keyring's changes nothing for
/// that server. The sandbox's keyring holds no key, so it says there was
/// none, and removes nothing anywhere else.
#[cfg(feature = "mcp-http")]
#[test]
fn mcp_key_revoke_warns_when_the_environment_holds_the_key() {
    let tmp = tempfile::tempdir().unwrap();

    let out = snob_with(
        tmp.path(),
        None,
        &[("SNOB_MCP_KEY", MCP_KEY)],
        &["mcp", "key", "revoke"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(said.contains("no MCP key"), "{said}");
    let warned = stderr(&out);
    assert!(
        warned.contains("warning: SNOB_MCP_KEY is set")
            && warned.contains("a server started with it takes that key"),
        "{warned}"
    );
    assert!(!said.contains(MCP_KEY) && !warned.contains(MCP_KEY));

    // Without the variable there is nothing to warn about.
    let out = snob(tmp.path(), None, &["mcp", "key", "revoke"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));
}

/// One `snob mcp --listen` on a loopback port of its own, with
/// [`MCP_KEY`] in its environment, spoken to over HTTP as a client does.
///
/// Started with `--stop-when-stdin-closes`, so closing its standard input
/// asks it to stop as Ctrl+C would, on every platform.
#[cfg(feature = "mcp-http")]
struct HttpMcp {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    port: u16,
    http: reqwest::Client,
    next: u64,
    /// What the server said on standard error, as it said it.
    said: std::sync::Arc<std::sync::Mutex<String>>,
}

/// What an HTTP request to the server came back with.
#[cfg(feature = "mcp-http")]
struct Answered {
    status: u16,
    session: Option<String>,
    /// The JSON-RPC messages in the body, from its events or as it is.
    messages: Vec<serde_json::Value>,
}

#[cfg(feature = "mcp-http")]
impl HttpMcp {
    async fn start(root: &Path, instagram: &MockServer, args: &[&str]) -> Self {
        use tokio::io::AsyncBufReadExt;
        let mut command = tokio::process::Command::from(command(root, Some(instagram)));
        command
            .args(["--stop-when-stdin-closes", "mcp", "--listen", "127.0.0.1:0"])
            .args(args)
            .env("SNOB_MCP_KEY", MCP_KEY)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("the binary runs");
        let stdin = child.stdin.take();
        let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
        let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let first = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the server went away before it listened");
                said.lock().unwrap().push_str(&format!("{line}\n"));
                if line.contains("serving on http://") {
                    return line;
                }
            }
        })
        .await
        .expect("listening within a minute");
        let port = first
            .split("http://127.0.0.1:")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("a port in {first}"));
        // Read on, so that nothing the server says can fill the pipe.
        let more = std::sync::Arc::clone(&said);
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                more.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
        Self {
            child,
            stdin,
            port,
            http: reqwest::Client::new(),
            next: 0,
            said,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Posts `message` with the key, in `session` when given, on the
    /// revision a session speaks (2025-11-25) or else the stateless one.
    async fn post(&self, session: Option<&str>, message: &serde_json::Value) -> Answered {
        post_to(&self.http, self.url(), session, message).await
    }
}

/// [`HttpMcp::post`], with what it needs of the server, for a request made
/// from a task of its own.
#[cfg(feature = "mcp-http")]
async fn post_to(
    http: &reqwest::Client,
    url: String,
    session: Option<&str>,
    message: &serde_json::Value,
) -> Answered {
    let mut request = http
        .post(url)
        .header("Authorization", format!("Bearer {MCP_KEY}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(message.to_string());
    match session {
        Some(id) => {
            request = request
                .header("Mcp-Session-Id", id)
                .header("MCP-Protocol-Version", "2025-11-25");
        }
        None if message["method"] != "initialize" => {
            request = request
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", message["method"].as_str().unwrap_or_default());
            if let Some(name) = message["params"]["name"].as_str() {
                request = request.header("Mcp-Name", name);
            }
        }
        None => {}
    }
    let response = tokio::time::timeout(std::time::Duration::from_secs(90), request.send())
        .await
        .expect("an answer within a minute and a half")
        .expect("the server answers");
    let status = response.status().as_u16();
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|id| id.to_str().ok())
        .map(str::to_string);
    let json = response
        .headers()
        .get("content-type")
        .and_then(|kind| kind.to_str().ok())
        .is_some_and(|kind| kind.starts_with("application/json"));
    let body = response.text().await.unwrap_or_default();
    let messages = if json {
        serde_json::from_str(&body).into_iter().collect()
    } else {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str(data.trim()).ok())
            .collect()
    };
    Answered {
        status,
        session,
        messages,
    }
}

#[cfg(feature = "mcp-http")]
impl HttpMcp {
    /// A request, and the message that answers it.
    async fn ask(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        self.next += 1;
        let id = self.next;
        let mut params = params;
        if session.is_none() {
            params["_meta"] = serde_json::json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" }
            });
        }
        let message =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let answered = self.post(session, &message).await;
        assert_eq!(answered.status, 200, "{method}");
        answered
            .messages
            .into_iter()
            .find(|message| message["id"] == id)
            .unwrap_or_else(|| panic!("no answer to {method}"))
    }

    /// The handshake of a client on 2025-11-25, which opens a session: its
    /// id.
    async fn open(&mut self) -> String {
        self.open_with(serde_json::json!({})).await
    }

    /// [`Self::open`], for a client that declares `capabilities`.
    async fn open_with(&mut self, capabilities: serde_json::Value) -> String {
        self.next += 1;
        let hello = serde_json::json!({
            "jsonrpc": "2.0", "id": self.next, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": capabilities,
                "clientInfo": { "name": "test", "version": "0" }
            }
        });
        let answered = self.post(None, &hello).await;
        assert_eq!(answered.status, 200);
        assert_eq!(
            answered.messages[0]["result"]["serverInfo"]["name"], "snob",
            "{:?}",
            answered.messages
        );
        let session = answered.session.expect("a session id");
        let done = self
            .post(
                Some(&session),
                &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .await;
        assert_eq!(done.status, 202);
        session
    }

    async fn call(
        &mut self,
        session: Option<&str>,
        tool: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let answer = self
            .ask(
                session,
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
            )
            .await;
        answer["result"].clone()
    }

    /// Closes the server's standard input, the stop `--stop-when-stdin-
    /// closes` hears, and waits for it to exit.
    async fn stop(&mut self) -> std::process::ExitStatus {
        self.stdin = None;
        tokio::time::timeout(std::time::Duration::from_secs(90), self.child.wait())
            .await
            .expect("the server stopped within a minute and a half")
            .unwrap()
    }

    fn said(&self) -> String {
        self.said.lock().unwrap().clone()
    }

    /// Whether the server has said `what` on standard error, waiting a few
    /// seconds for the lines after the first.
    async fn has_said(&self, what: &str) -> bool {
        let since = std::time::Instant::now();
        while since.elapsed() < std::time::Duration::from_secs(10) {
            if self.said().contains(what) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }
}

/// Sends `request` as it is on a connection of its own, and reads what comes
/// back until the server closes the connection or `within` passes: what it
/// read, and whether the server closed it.
#[cfg(feature = "mcp-http")]
async fn exchange(port: u16, request: &[u8], within: std::time::Duration) -> (String, bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // A refusal may close the connection before all of it is written.
    let _ = stream.write_all(request).await;
    let mut read = Vec::new();
    let mut buffer = [0u8; 4096];
    let closed = tokio::time::timeout(within, async {
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(n) => read.extend_from_slice(&buffer[..n]),
            }
        }
    })
    .await
    .is_ok();
    (String::from_utf8_lossy(&read).into_owned(), closed)
}

/// A request's head, by hand: `extra` lines go in as they are.
#[cfg(feature = "mcp-http")]
fn head(port: u16, extra: &[&str], length: usize) -> String {
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {length}\r\n"
    );
    for line in extra {
        head.push_str(line);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    head
}

/// An `initialize` as a client on 2025-11-25 sends it.
#[cfg(feature = "mcp-http")]
const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"raw","version":"0"}}}"#;

/// No key, no server: it does not start listening, and says how to make
/// one.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn the_http_server_refuses_to_start_without_a_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mut command = tokio::process::Command::from(command(tmp.path(), None));
    command.args(["mcp", "--listen", "127.0.0.1:0"]);
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
        .await
        .expect("it did not stay to serve")
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("snob mcp key new"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("serving on"), "{}", stderr(&out));
}

/// Loopback only, and a public address only over https and with --listen.
#[cfg(feature = "mcp-http")]
#[test]
fn the_http_server_refuses_a_non_loopback_address() {
    let tmp = tempfile::tempdir().unwrap();
    let key = [("SNOB_MCP_KEY", MCP_KEY)];
    for refused in [
        &["mcp", "--listen", "0.0.0.0:8750"][..],
        &["mcp", "--listen", "[::]:8750"][..],
        &["mcp", "--listen", "192.168.1.10:8750"][..],
        &["mcp", "--listen", "example.com:8750"][..],
        &[
            "mcp",
            "--listen",
            "127.0.0.1:0",
            "--public-url",
            "http://snob.example.com",
        ][..],
        &["mcp", "--public-url", "https://snob.example.com"][..],
    ] {
        let out = snob_with(tmp.path(), None, &key, refused);
        assert_eq!(out.status.code(), Some(2), "{refused:?}: {}", stderr(&out));
        assert!(!stderr(&out).contains("serving on"), "{}", stderr(&out));
    }
    let out = snob_with(tmp.path(), None, &key, &["mcp", "--listen", "0.0.0.0:8750"]);
    assert!(stderr(&out).contains("tunnel"), "{}", stderr(&out));
}

/// A request with no key is refused as soon as its head has arrived: the
/// body it announced is never waited for, and the connection is closed.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_request_without_the_key_is_refused_before_its_body_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let server = HttpMcp::start(tmp.path(), &instagram, &[]).await;

    // A body far past the limit, announced and never sent: a server that
    // read it first would wait, or answer 413.
    let since = std::time::Instant::now();
    let (said, closed) = exchange(
        server.port,
        head(server.port, &[], 10_000_000).as_bytes(),
        std::time::Duration::from_secs(20),
    )
    .await;
    assert!(said.starts_with("HTTP/1.1 401"), "{said}");
    assert!(
        said.contains("www-authenticate: Bearer realm=\"snob\""),
        "{said}"
    );
    assert!(closed, "the connection was closed: {said}");
    assert!(since.elapsed() < std::time::Duration::from_secs(10));

    // Another path is not found, key or no key.
    let (said, _) = exchange(
        server.port,
        b"GET /.well-known/oauth-protected-resource HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        std::time::Duration::from_secs(20),
    )
    .await;
    assert!(said.starts_with("HTTP/1.1 404"), "{said}");
    assert!(posted(&instagram).await.is_empty());
}

/// Not the key, or the key under another scheme: 401, and the connection
/// closed, so a guess costs a connection.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_wrong_key_is_refused_and_the_connection_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let wrong = format!("{}E", &MCP_KEY[..MCP_KEY.len() - 1]);
    for authorization in [
        format!("Authorization: Bearer {wrong}"),
        format!("Authorization: Basic {MCP_KEY}"),
        format!("Authorization: {MCP_KEY}"),
        "Authorization: Bearer ".to_string(),
    ] {
        let mut request = head(server.port, &[&authorization], HELLO.len());
        request.push_str(HELLO);
        let (said, closed) = exchange(
            server.port,
            request.as_bytes(),
            std::time::Duration::from_secs(20),
        )
        .await;
        assert!(said.starts_with("HTTP/1.1 401"), "{authorization}: {said}");
        assert!(said.contains("connection: close"), "{said}");
        assert!(closed, "{authorization}: the connection was closed");
    }
    // The key, in any case of its scheme, is let in.
    let key = format!("Authorization: bEaReR {MCP_KEY}");
    let mut request = head(server.port, &[&key, "Connection: close"], HELLO.len());
    request.push_str(HELLO);
    let (said, _) = exchange(
        server.port,
        request.as_bytes(),
        std::time::Duration::from_secs(20),
    )
    .await;
    assert!(said.starts_with("HTTP/1.1 200"), "{said}");
    assert_eq!(server.stop().await.code(), Some(130));
}

/// Connections that never show the key hold no place a client with it
/// needs: past the cap, the oldest of them is closed for the new one, so a
/// local process holding idle sockets locks nobody out.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn connections_without_the_key_do_not_lock_out_a_client_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    // More than the cap, sending nothing, all open at once.
    let mut idle = Vec::new();
    for _ in 0..40 {
        idle.push(
            tokio::net::TcpStream::connect(("127.0.0.1", server.port))
                .await
                .unwrap(),
        );
    }
    let since = std::time::Instant::now();
    let session = server.open().await;
    let tools = server
        .ask(Some(&session), "tools/list", serde_json::json!({}))
        .await;
    assert!(tools["result"]["tools"].is_array(), "{tools}");
    assert!(
        since.elapsed() < std::time::Duration::from_secs(10),
        "not by waiting for the idle ones to be cut: {:?}",
        since.elapsed()
    );

    // And one that sends nothing is closed within seconds, not left to hold
    // its place for the ten an idle connection between requests gets.
    let (said, closed) = exchange(server.port, b"", std::time::Duration::from_secs(8)).await;
    assert!(closed, "closed for sending nothing: {said}");
    drop(idle);
    assert_eq!(server.stop().await.code(), Some(130));
}

/// A `Host` that is neither loopback nor the public URL's is refused, as a
/// page that reached the port through DNS rebinding would send.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_foreign_host_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let server = HttpMcp::start(
        tmp.path(),
        &instagram,
        &["--public-url", "https://snob.example.com/mcp"],
    )
    .await;
    let key = format!("Authorization: Bearer {MCP_KEY}");
    for (host, status) in [
        ("evil.example", "403"),
        ("snob.example.com", "200"),
        ("localhost", "200"),
    ] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\n{key}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{HELLO}",
            HELLO.len()
        );
        let (said, _) = exchange(
            server.port,
            request.as_bytes(),
            std::time::Duration::from_secs(20),
        )
        .await;
        assert!(
            said.starts_with(&format!("HTTP/1.1 {status}")),
            "{host}: {said}"
        );
    }
}

/// An `Origin` other than the public URL's is refused; a request with none,
/// which is what a client that is not a browser sends, is not.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_foreign_origin_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let server = HttpMcp::start(
        tmp.path(),
        &instagram,
        &["--public-url", "https://snob.example.com"],
    )
    .await;
    let key = format!("Authorization: Bearer {MCP_KEY}");
    for (origin, status) in [
        (Some("https://evil.example"), "403"),
        (Some("http://localhost:8080"), "403"),
        (Some("https://snob.example.com"), "200"),
        (None, "200"),
    ] {
        let origin_line = origin.map(|origin| format!("Origin: {origin}"));
        let mut extra = vec![key.as_str(), "Connection: close"];
        if let Some(line) = &origin_line {
            extra.push(line);
        }
        let mut request = head(server.port, &extra, HELLO.len());
        request.push_str(HELLO);
        let (said, _) = exchange(
            server.port,
            request.as_bytes(),
            std::time::Duration::from_secs(20),
        )
        .await;
        assert!(
            said.starts_with(&format!("HTTP/1.1 {status}")),
            "{origin:?}: {said}"
        );
    }
}

/// A body past the quarter of a megabyte is refused, announced or not.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_body_past_the_limit_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let key = format!("Authorization: Bearer {MCP_KEY}");

    // Announced: refused before any of it is read.
    let (said, closed) = exchange(
        server.port,
        head(server.port, &[&key], 300_000).as_bytes(),
        std::time::Duration::from_secs(20),
    )
    .await;
    assert!(said.starts_with("HTTP/1.1 413"), "{said}");
    assert!(closed);

    // In chunks, which announce nothing: refused once past it.
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{key}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        server.port
    );
    let chunk = "x".repeat(64 * 1024);
    for _ in 0..5 {
        request.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
    }
    request.push_str("0\r\n\r\n");
    let (said, _) = exchange(
        server.port,
        request.as_bytes(),
        std::time::Duration::from_secs(20),
    )
    .await;
    assert!(said.starts_with("HTTP/1.1 413"), "{said}");
}

/// A body that does not arrive is given half a minute, and then the request
/// is answered 408 and its connection closed.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_body_that_never_arrives_is_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let key = format!("Authorization: Bearer {MCP_KEY}");
    let mut request = head(server.port, &[&key], HELLO.len());
    request.push_str(&HELLO[..10]);
    let since = std::time::Instant::now();
    let (said, closed) = exchange(
        server.port,
        request.as_bytes(),
        std::time::Duration::from_secs(60),
    )
    .await;
    assert!(said.starts_with("HTTP/1.1 408"), "{said}");
    assert!(closed);
    assert!(since.elapsed() >= std::time::Duration::from_secs(25));
}

/// `initialize` opens a session, which later requests name; a session the
/// server never opened is not found. The key is named at start by its
/// fingerprint, never itself.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn initialize_over_http_opens_a_session() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let session = server.open().await;
    let tools = server
        .ask(Some(&session), "tools/list", serde_json::json!({}))
        .await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"snob_list"), "{names:?}");
    assert!(
        !names.contains(&"snob_follow"),
        "no writes without --allow-writes"
    );

    let unknown = server
        .post(
            Some("not-a-session"),
            &serde_json::json!({"jsonrpc": "2.0", "id": 99, "method": "tools/list", "params": {}}),
        )
        .await;
    assert_eq!(unknown.status, 404);
    assert!(server.said().contains("fingerprint"), "{}", server.said());
    assert!(!server.said().contains(MCP_KEY), "the key is never said");
}

/// What `snob mcp config --remote` prints reaches a real `snob mcp --listen`
/// as it is: the address, and the header with the token filled in as the
/// client fills it in from `SNOB_MCP_TOKEN`. The key is never printed, even
/// with the server's variable set where it runs, and the server's flags and
/// an address that would carry the key in plain text or hold it are
/// refused, without saying it back.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_remote_config_reaches_the_server_it_names() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let with_key = [("SNOB_MCP_KEY", MCP_KEY)];
    let config = |client: &str, more: &[&str]| {
        let mut args = vec!["mcp", "config", client];
        args.extend_from_slice(more);
        snob_with(tmp.path(), None, &with_key, &args)
    };
    let url = server.url();

    let out = config("claude-code", &["--remote", &url]);
    assert!(out.status.success(), "{}", stderr(&out));
    for said in [stdout(&out), stderr(&out)] {
        assert!(!said.contains(MCP_KEY), "the key was printed: {said}");
    }
    assert!(stderr(&out).contains("SNOB_MCP_TOKEN"), "{}", stderr(&out));
    let snippet: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let remote = &snippet["mcpServers"]["snob"];
    assert_eq!(remote["url"], url.as_str());
    assert!(remote.get("command").is_none(), "{snippet}");
    let header = remote["headers"]["Authorization"].as_str().unwrap();
    let hello = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        }
    });
    let send = |authorization: String| {
        reqwest::Client::new()
            .post(remote["url"].as_str().unwrap())
            .header("Authorization", authorization)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(hello.to_string())
            .send()
    };
    let opened = send(header.replace("${SNOB_MCP_TOKEN}", MCP_KEY))
        .await
        .unwrap();
    assert_eq!(opened.status(), 200);
    assert!(opened.headers().contains_key("mcp-session-id"));
    // Sent as it is printed, before the client fills it in: no key.
    let unfilled = send(header.to_string()).await.unwrap();
    assert_eq!(unfilled.status(), 401);

    let out = config("codex", &["--remote", &url]);
    assert!(out.status.success(), "{}", stderr(&out));
    let toml = stdout(&out);
    assert!(toml.contains(&format!("url = \"{url}\"")), "{toml}");
    assert!(
        toml.contains("bearer_token_env_var = \"SNOB_MCP_TOKEN\""),
        "{toml}"
    );
    assert!(!toml.contains("command"), "{toml}");

    // An address with no path gets the one the server serves.
    let bare = format!("http://127.0.0.1:{}", server.port);
    let out = config("cursor", &["--remote", &bare]);
    assert!(out.status.success(), "{}", stderr(&out));
    let snippet: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(snippet["mcpServers"]["snob"]["url"], url.as_str());

    // The server's flags belong to the server.
    for flags in [
        &["--allow-writes"][..],
        &["--confirm-in-chat", "all"],
        &["--allow-writes", "--max-writes-per-day", "3"],
    ] {
        let mut args = vec!["--remote", url.as_str()];
        args.extend_from_slice(flags);
        let refused = config("codex", &args);
        assert_eq!(refused.status.code(), Some(2), "{flags:?}");
        assert!(stdout(&refused).is_empty());
    }
    // Plain http off this machine, and a key in the address.
    let refused = config("codex", &["--remote", "http://snob.example.com/mcp"]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("https"), "{}", stderr(&refused));
    let holding = format!("https://snob.example.com/{MCP_KEY}/mcp");
    let refused = config("codex", &["--remote", &holding]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    assert!(stdout(&refused).is_empty());
    assert!(
        !stderr(&refused).contains(MCP_KEY),
        "the key was said back: {}",
        stderr(&refused)
    );

    assert_eq!(server.stop().await.code(), Some(130));
}

/// On the stateless revision a call carries everything it needs, and no
/// session is opened for it.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_stateless_call_over_http_needs_no_session() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let status = server
        .call(None, "snob_status", serde_json::json!({}))
        .await;
    assert_eq!(status["isError"], false, "{status}");
    assert_eq!(
        status["structuredContent"]["error"]["code"], "no_session",
        "{status}"
    );
}

/// Eight sessions at once, left by clients that went away without a
/// `DELETE`: a ninth `initialize` closes the one left idle longest rather
/// than being refused, and that client is told its session is gone; the
/// others go on, and a stateless call, which opens none, is answered too.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_ninth_session_closes_the_one_left_idle_longest() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let mut sessions = Vec::new();
    for _ in 0..8 {
        sessions.push(server.open().await);
    }
    // The first is asked something again, so the second is the idlest.
    let status = server
        .call(Some(&sessions[0]), "snob_status", serde_json::json!({}))
        .await;
    assert_eq!(status["isError"], false, "{status}");

    let ninth = server
        .post(None, &serde_json::from_str(HELLO).unwrap())
        .await;
    assert_eq!(ninth.status, 200);
    assert!(ninth.session.is_some());
    let ping = serde_json::json!({"jsonrpc": "2.0", "id": 900, "method": "ping"});
    let closed = server.post(Some(&sessions[1]), &ping).await;
    assert_eq!(closed.status, 404, "the idlest session is gone");
    let kept = server.post(Some(&sessions[0]), &ping).await;
    assert_eq!(kept.status, 200, "the one asked lately is kept");

    let status = server
        .call(None, "snob_status", serde_json::json!({}))
        .await;
    assert_eq!(status["isError"], false, "{status}");
}

/// Over HTTP, whoever holds the key may be the one saying yes in the chat,
/// so by default a yes in the chat covers reading lists and not the writes.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_remote_server_takes_no_chat_yes_for_a_write_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    serve_an_unfollow(&instagram).await;
    log_in_writing(tmp.path(), &instagram);
    let mut server = HttpMcp::start(tmp.path(), &instagram, &["--allow-writes"]).await;
    assert!(
        server
            .has_said("whoever holds the key can call follow and unfollow")
            .await,
        "{}",
        server.said()
    );
    let session = server.open().await;
    let before = posted(&instagram).await.len();
    for tool in ["snob_follow", "snob_unfollow"] {
        let refused = server
            .call(
                Some(&session),
                tool,
                serde_json::json!({ "account": "someone" }),
            )
            .await;
        assert_eq!(refused["isError"], true, "{refused}");
        assert_eq!(
            refused["structuredContent"]["error"]["code"], "cannot_confirm",
            "{refused}"
        );
    }
    let walk = server
        .call(
            Some(&session),
            "snob_list",
            serde_json::json!({ "list": "followers", "account": "someone" }),
        )
        .await;
    assert_eq!(
        walk["structuredContent"]["status"], "confirmation_required",
        "{walk}"
    );
    assert_eq!(writes_sent(&instagram).await, 0);
    assert_eq!(posted(&instagram).await.len(), before, "nothing was sent");

    // Given --confirm-in-chat all, the writes are asked in the chat too.
    let mut all = HttpMcp::start(
        tmp.path(),
        &instagram,
        &["--allow-writes", "--confirm-in-chat", "all"],
    )
    .await;
    assert!(
        all.has_said("confirm them in the chat").await,
        "{}",
        all.said()
    );
    let session = all.open().await;
    let asked = all
        .call(
            Some(&session),
            "snob_unfollow",
            serde_json::json!({ "account": "someone" }),
        )
        .await;
    assert_eq!(
        asked["structuredContent"]["status"], "confirmation_required",
        "{asked}"
    );
    assert_eq!(writes_sent(&instagram).await, 0);
}

/// A token handed out in one session is no yes in another: that call is
/// asked again, and nothing is walked until the session it was given in
/// brings it back.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_chat_ticket_from_one_session_is_refused_in_another() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let here = server.open().await;
    let there = server.open().await;
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let asking = server.call(Some(&here), "snob_list", theirs.clone()).await;
    let token = asking["structuredContent"]["confirmation"]
        .as_str()
        .unwrap_or_else(|| panic!("a token: {asking}"))
        .to_string();
    let mut with_token = theirs.clone();
    with_token["confirmation"] = serde_json::json!(token);

    let elsewhere = server
        .call(Some(&there), "snob_list", with_token.clone())
        .await;
    assert_eq!(
        elsewhere["structuredContent"]["status"], "confirmation_required",
        "{elsewhere}"
    );
    assert_eq!(
        elsewhere["structuredContent"]["previous"], "not_for_this_call",
        "{elsewhere}"
    );
    assert_eq!(followers_asked(&instagram).await, 0, "nothing was walked");

    let done = server.call(Some(&here), "snob_list", with_token).await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(done["structuredContent"]["count"], 3, "{done}");
}

/// The question in one request and the yes in the next, as a stateless
/// client makes them: the walk runs on the second, once.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn a_chat_confirmation_of_a_list_works_over_two_http_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let theirs = serde_json::json!({ "list": "followers", "account": "someone" });
    let asking = server.call(None, "snob_list", theirs.clone()).await;
    assert_eq!(asking["isError"], false, "{asking}");
    let token = asking["structuredContent"]["confirmation"]
        .as_str()
        .unwrap_or_else(|| panic!("a token: {asking}"))
        .to_string();
    assert_eq!(followers_asked(&instagram).await, 0);

    let mut with_token = theirs;
    with_token["confirmation"] = serde_json::json!(token);
    let done = server.call(None, "snob_list", with_token.clone()).await;
    assert_eq!(done["isError"], false, "{done}");
    assert_eq!(done["structuredContent"]["count"], 3, "{done}");
    let walked = followers_asked(&instagram).await;
    assert!(walked > 0);

    let again = server.call(None, "snob_list", with_token).await;
    assert_eq!(
        again["structuredContent"]["error"]["code"], "confirmation_used",
        "{again}"
    );
    assert_eq!(followers_asked(&instagram).await, walked);
}

/// Asked to stop while a call runs, the server stops that call's child in
/// order, as a canceled call would (here, the start of a job, which stops
/// the job it had not handed over yet), waits for it, **hands the call its
/// answer**, and exits 130.
///
/// Stopped through standard input closing (`--stop-when-stdin-closes`),
/// which takes the same road as Ctrl+C and can be sent on every platform.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn ctrl_c_gives_running_calls_their_grace() {
    stopped_during_a_call(true).await;
}

/// The same for a call on the stateless revision, whose answer the SDK
/// drops if its own token is canceled first.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn ctrl_c_gives_a_stateless_call_its_answer() {
    stopped_during_a_call(false).await;
}

/// Asked to stop while a call waits for the person's answer in a form, the
/// server answers that call (with a refusal: nobody said yes) before it
/// ends the streams, rather than drop it with the form still on screen.
#[cfg(feature = "mcp-http")]
#[tokio::test]
async fn ctrl_c_answers_a_call_waiting_on_a_form() {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram(3, 2).await;
    log_in(tmp.path(), &instagram);
    let _jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let session = server
        .open_with(serde_json::json!({ "elicitation": { "form": {} } }))
        .await;
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 600, "method": "tools/call",
        "params": { "name": "snob_list", "arguments": { "list": "followers", "account": "someone" } }
    });
    let response = server
        .http
        .post(server.url())
        .header("Authorization", format!("Bearer {MCP_KEY}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session)
        .header("MCP-Protocol-Version", "2025-11-25")
        .body(request.to_string())
        .send()
        .await
        .expect("the server answers");
    assert_eq!(response.status().as_u16(), 200);
    let (asked_tx, asked) = tokio::sync::oneshot::channel();
    let read = tokio::spawn(async move {
        let mut response = response;
        let mut body = String::new();
        let mut asked_tx = Some(asked_tx);
        while let Ok(Some(chunk)) = response.chunk().await {
            body.push_str(&String::from_utf8_lossy(&chunk));
            if body.contains("elicitation/create")
                && let Some(tx) = asked_tx.take()
            {
                let _ = tx.send(());
            }
        }
        body
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), asked)
        .await
        .expect("asked within a minute")
        .expect("asked");

    let exited = server.stop().await;
    assert_eq!(exited.code(), Some(130), "{}", server.said());
    let body = tokio::time::timeout(std::time::Duration::from_secs(30), read)
        .await
        .expect("the stream ended")
        .unwrap();
    let answer = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
        .find(|message| message["id"] == 600)
        .unwrap_or_else(|| panic!("the call was answered: {body}"));
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert_eq!(followers_asked(&instagram).await, 0, "nothing was read");
}

#[cfg(feature = "mcp-http")]
async fn stopped_during_a_call(in_a_session: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let instagram = fake_instagram_slow(3, std::time::Duration::from_secs(120), 1).await;
    log_in(tmp.path(), &instagram);
    let jobs = Jobs {
        root: tmp.path(),
        instagram: &instagram,
    };
    let mut server = HttpMcp::start(tmp.path(), &instagram, &[]).await;
    let session = if in_a_session {
        Some(server.open().await)
    } else {
        None
    };
    let mut params = serde_json::json!({
        "name": "snob_list", "arguments": { "list": "followers", "wait_seconds": 50 }
    });
    if !in_a_session {
        params["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0" }
        });
    }
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 500, "method": "tools/call", "params": params
    });
    let call = {
        let http = server.http.clone();
        let url = server.url();
        tokio::spawn(async move { post_to(&http, url, session.as_deref(), &request).await })
    };
    until_asked(&instagram, 1).await;

    let since = std::time::Instant::now();
    let exited = server.stop().await;
    assert_eq!(exited.code(), Some(130), "{}", server.said());
    // Within the grace a stopped child gets, and the wait for it.
    assert!(
        since.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        since.elapsed()
    );
    let listed = jobs.ask(&["list", "--all", "--json"]);
    let job = &listed["jobs"][0];
    assert!(
        matches!(job["status"].as_str(), Some("canceled" | "stopped")),
        "the walk was stopped in order: {job}"
    );
    let answered = call.await.unwrap();
    assert_eq!(answered.status, 200, "{:?}", answered.messages);
    let answer = answered
        .messages
        .iter()
        .find(|message| message["id"] == 500)
        .unwrap_or_else(|| panic!("the call was answered: {:?}", answered.messages));
    assert!(
        answer.get("result").is_some() || answer.get("error").is_some(),
        "{answer}"
    );
    assert!(
        !server.said().contains("empty response"),
        "{}",
        server.said()
    );
}
