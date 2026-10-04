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
/// user's real data directory out of it. The five variables removed below do
/// not reach the run when set outside the test: an account, a token, a key, a
/// cooldown override or a log level named in the developer's environment is
/// not this test's.
fn command(root: &Path, instagram: Option<&MockServer>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_snob"));
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
        "SNOB_IGNORE_COOLDOWN",
        "SNOB_LOG",
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
    // The page that hands out the tokens a mutation needs, served to the
    // session's account, and the mutation.
    Mock::given(method("GET"))
        .and(url_path("/someone/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<html>{"define":[["DTSGInitData",[],{"token":"DTSG"},1],
               ["LSD",[],{"token":"LSD"},2],
               ["PolarisViewer",[],{"data":{"id":"42","username":"me","fbid":"17841400000000042"},"id":"42"},3]]}
               <script src="https://static.cdninstagram.com/rsrc.php/v4/a.js"></script></html>"#,
        ))
        .with_priority(1)
        .mount(&instagram)
        .await;
    Mock::given(method("POST"))
        .and(url_path("/api/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"result":"unfollowed","status":"ok"}"#),
        )
        .mount(&instagram)
        .await;
    // The profile has to say the relationship exists, or the command correctly
    // decides there is nothing to do and sends nothing.
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
        .mount(&instagram)
        .await;
    log_in_writing(tmp.path(), &instagram);
    common::remember_doc_ids(tmp.path());

    let out = snob(tmp.path(), Some(&instagram), &["unfollow", "someone", "-y"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));

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
