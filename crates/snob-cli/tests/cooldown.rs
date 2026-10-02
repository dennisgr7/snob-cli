//! What the list commands do while the account is in cooldown.
//!
//! The contract: nothing is spent, not even the counter poll. A stored
//! complete list is served with a warning; with nothing stored the command
//! refuses with the throttling exit code and names when the cooldown ends.

use std::sync::Arc;

use snob_core::Pk;
use snob_core::budget::{RateBudget, RateBudgetError, UnlimitedRateBudget};
use snob_core::model::ListKind;
use snob_store::store::Store;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use snob_cli::cli::ListArgs;
use snob_cli::engine::{self, ListOutcome, Provenance};
use snob_cli::exit::ExitCode;

mod common;
use common::{HOUR, account_at, args, budget_at, requests, start_cooldown};

/// Reopens the database so each invocation is a separate run, the way two
/// consecutive commands really are.
fn reopen(root: &std::path::Path) -> Store {
    Store::open(&account_at(root)).unwrap()
}

async fn mount_profile(server: &MockServer, user: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!(r#"{{"data":{{"user":{user}}}}}"#)),
        )
        .mount(server)
        .await;
}

async fn mount_list(server: &MockServer, pk: Pk, how_many: u64) {
    let users: Vec<String> = (0..how_many)
        .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/friendships/{pk}/followers/")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"users":[{}]}}"#, users.join(","))),
        )
        .mount(server)
        .await;
}

async fn execute_with(
    server: &MockServer,
    db: Store,
    budget: Arc<dyn RateBudget>,
    args: &ListArgs,
) -> anyhow::Result<(Vec<snob_core::model::User>, ListOutcome)> {
    let mut app = common::app_with(server, db, budget);
    engine::list(
        &mut app,
        &engine::ListQuery::from(args),
        ListKind::Followers,
    )
    .await
}

/// Reports no cooldown for a fixed number of calls, then an active one: the
/// shape of a cooldown another process sets while a run is underway.
struct LateCooldown {
    calls_before: u32,
    seen: std::sync::atomic::AtomicU32,
}

impl LateCooldown {
    fn after(calls_before: u32) -> Self {
        Self {
            calls_before,
            seen: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl RateBudget for LateCooldown {
    fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
        Ok(std::time::Duration::ZERO)
    }

    fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.reserve()
    }

    fn cooldown(&self) -> Result<Option<snob_core::EpochMs>, RateBudgetError> {
        let seen = self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok((seen >= self.calls_before)
            .then(|| snob_core::clock::now_ms() + std::time::Duration::from_millis(3_600_000)))
    }

    fn start_cooldown(
        &self,
        _: &str,
        _: std::time::Duration,
    ) -> Result<snob_core::EpochMs, RateBudgetError> {
        Ok(snob_core::EpochMs::new(0))
    }
}

fn assert_rate_limited(error: &anyhow::Error) {
    assert_eq!(
        snob_cli::exit::from_chain(error),
        Some(ExitCode::RateLimited)
    );
}

#[tokio::test]
async fn during_a_cooldown_the_stored_list_is_served_without_requests() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;
    mount_list(&server, Pk::new(42), 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());

    execute_with(
        &server,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &args(),
    )
    .await
    .unwrap();
    let seeded = requests(&server).await;

    start_cooldown(&budget, 2 * HOUR);
    let (found, outcome) = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap();

    assert_eq!(found.len(), 30);
    assert!(outcome.is_stored());
    assert_eq!(outcome.requests, 0);
    assert_eq!(outcome.provenance, Provenance::Cooldown);
    assert_eq!(
        requests(&server).await,
        seeded,
        "a cooldown must not spend a single request, not even the poll"
    );
}

/// The documented contract: served however old, ignoring --max-age. This is
/// what tells the cooldown path apart from the normal cache policy.
#[tokio::test]
async fn an_old_snapshot_is_still_served_during_the_cooldown() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;
    mount_list(&server, Pk::new(42), 30).await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());

    execute_with(
        &server,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &args(),
    )
    .await
    .unwrap();

    // Age the snapshot far past the default --max-age of a day.
    reopen(tmp.path())
        .conn()
        .execute("UPDATE snapshots SET taken_at = taken_at - 1000000", [])
        .unwrap();

    start_cooldown(&budget, 2 * HOUR);
    let (found, outcome) = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap();

    assert_eq!(found.len(), 30);
    assert!(outcome.is_stored());
    assert_eq!(outcome.requests, 0);
}

#[tokio::test]
async fn with_nothing_stored_a_cooldown_refuses_with_the_rate_limited_code() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());
    start_cooldown(&budget, 2 * HOUR);

    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap_err();

    assert!(error.to_string().contains("in cooldown until"), "{error}");
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn refresh_is_refused_while_the_cooldown_lasts() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());
    start_cooldown(&budget, 2 * HOUR);

    let mut args = args();
    args.walk.refresh = true;
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("--refresh"), "{error}");
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn a_named_target_is_resolved_locally_and_case_insensitively() {
    let seed = MockServer::start().await;
    mount_profile(
        &seed,
        r#"{"id":99,"username":"ghost","edge_followed_by":{"count":5},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&seed, Pk::new(99), 5).await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());

    let mut named = args();
    named.target = Some("@ghost".into());
    execute_with(
        &seed,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &named,
    )
    .await
    .unwrap();

    start_cooldown(&budget, 2 * HOUR);

    // A server with nothing mounted: any request would fail loudly.
    let empty = MockServer::start().await;
    let mut cased = args();
    cased.target = Some("@Ghost".into());
    let (found, outcome) = execute_with(&empty, reopen(tmp.path()), budget.clone(), &cased)
        .await
        .unwrap();

    assert_eq!(found.len(), 5);
    assert!(outcome.is_stored());
    assert_eq!(requests(&empty).await, 0);
}

#[tokio::test]
async fn a_named_target_never_walked_is_refused() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());
    start_cooldown(&budget, 2 * HOUR);

    let mut named = args();
    named.target = Some("@ghost".into());
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &named)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("@ghost"), "{error}");
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0);
}

/// A cooldown that lands after the entry check — set by another process
/// sharing the database — must still stop the poll from firing.
#[tokio::test]
async fn a_cooldown_landing_after_the_entry_check_still_stops_the_poll() {
    let server = MockServer::start().await;

    let tmp = tempfile::tempdir().unwrap();
    let _schema = Store::open(&account_at(tmp.path())).unwrap();

    // Visible at the second look (before the poll), not at the entry check.
    let budget: Arc<dyn RateBudget> = Arc::new(LateCooldown::after(1));
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap_err();

    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 0, "the poll must never fire");
}

/// A cooldown that only becomes visible at the walker's own check still has
/// to come out as throttling, not as a generic failure.
#[tokio::test]
async fn a_cooldown_landing_after_the_poll_still_exits_throttled() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let _schema = Store::open(&account_at(tmp.path())).unwrap();

    // Visible only at the fourth look: entry check, pre-poll check, **the
    // pacer's own**, walker.
    //
    // The third of those is the backstop in `Pacer::clear`, which reads the
    // cooldown before every request rather than trusting the caller to have
    // asked. With `after(2)` the pacer would see the cooldown first and the
    // poll would never leave, which is a different test: this one is about the
    // walk stopping after a request that had already left, so the fixture has
    // to let that request leave.
    let budget: Arc<dyn RateBudget> = Arc::new(LateCooldown::after(3));
    let error = execute_with(&server, reopen(tmp.path()), budget.clone(), &args())
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("nothing can be walked"),
        "{error}"
    );
    assert_rate_limited(&error);
    assert_eq!(requests(&server).await, 1, "only the poll was spent");
}

/// `--cache` with a named target spends no profile request on resolving it
/// during a cooldown: the pre-check answers before the name is resolved.
#[tokio::test]
async fn cache_during_a_cooldown_skips_the_resolve_request() {
    let seed = MockServer::start().await;
    mount_profile(
        &seed,
        r#"{"id":99,"username":"ghost","edge_followed_by":{"count":5},"edge_follow":{"count":1}}"#,
    )
    .await;
    mount_list(&seed, Pk::new(99), 5).await;

    let tmp = tempfile::tempdir().unwrap();
    let (_, budget) = budget_at(tmp.path());

    let mut named = args();
    named.target = Some("@ghost".into());
    execute_with(
        &seed,
        reopen(tmp.path()),
        Arc::new(UnlimitedRateBudget),
        &named,
    )
    .await
    .unwrap();

    start_cooldown(&budget, 2 * HOUR);

    let empty = MockServer::start().await;
    let mut cached = args();
    cached.target = Some("@ghost".into());
    cached.walk.offline = true;
    let (found, outcome) = execute_with(&empty, reopen(tmp.path()), budget.clone(), &cached)
        .await
        .unwrap();

    assert_eq!(found.len(), 5);
    assert_eq!(outcome.requests, 0);
    assert_eq!(requests(&empty).await, 0);
}

/// A cooldown that lands while the confirmation prompt is open costs nothing.
///
/// The second check has to come before `target::resolve`, because resolving is
/// a request: past it, a named target would spend the counter poll
/// `engine::cooldown` says must never be spent. The test beside this one cannot
/// see that, since its fixture leaves `target` as `None`, the one shape that
/// resolves without asking Instagram anything.
///
/// `after(1)`: the entry check sees nothing, and the cooldown is there by the
/// time the second one looks. That is the window the second check exists for.
#[tokio::test]
async fn a_cooldown_landing_before_the_resolve_spends_nothing() {
    let server = MockServer::start().await;
    mount_profile(
        &server,
        r#"{"id":"7","username":"someone","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
    )
    .await;

    let tmp = tempfile::tempdir().unwrap();
    let _schema = Store::open(&account_at(tmp.path())).unwrap();

    let named = ListArgs {
        target: Some("someone".to_string()),
        ..args()
    };
    let budget: Arc<dyn RateBudget> = Arc::new(LateCooldown::after(1));
    let error = execute_with(&server, reopen(tmp.path()), budget, &named)
        .await
        .unwrap_err();

    assert_rate_limited(&error);
    assert_eq!(
        requests(&server).await,
        0,
        "resolving the name is a request, and a cooldown is a cooldown"
    );
}

/// What Instagram answers when it wants the account to pass a security check.
const CHALLENGE: &str = r#"{"message":"challenge_required","challenge":{"url":"https://i.instagram.com/challenge/424242/Ch4LLeNGe1/"},"status":"fail"}"#;

/// The older form of the same request, with its link beside the message.
const CHECKPOINT: &str = r#"{"message":"checkpoint_required","checkpoint_url":"/challenge/424242/Ch4LLeNGe2/","lock":false,"status":"fail"}"#;

/// A security check on the counter poll, a challenge or a checkpoint, is
/// reported as one — its own exit code, its link — whether or not a list is
/// stored, and nothing more is sent. Taken for a failed poll, a stored list
/// would be served as if nothing had happened, and with none the walk would
/// find the cooldown the check recorded and end as throttled.
#[tokio::test]
async fn a_challenge_on_the_counter_poll_is_reported_as_one() {
    for (check, body) in [("challenge", CHALLENGE), ("checkpoint", CHECKPOINT)] {
        for stored in [true, false] {
            let server = MockServer::start().await;
            mount_profile(
                &server,
                r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
            )
            .await;
            mount_list(&server, Pk::new(42), 30).await;
            let tmp = tempfile::tempdir().unwrap();
            if stored {
                execute_with(
                    &server,
                    reopen(tmp.path()),
                    Arc::new(UnlimitedRateBudget),
                    &args(),
                )
                .await
                .unwrap();
            }

            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/users/web_profile_info/"))
                .respond_with(ResponseTemplate::new(400).set_body_string(body))
                .mount(&server)
                .await;
            mount_list(&server, Pk::new(42), 30).await;

            let (_, budget) = budget_at(tmp.path());
            let error = execute_with(&server, reopen(tmp.path()), budget, &args())
                .await
                .unwrap_err();
            assert_eq!(
                snob_cli::exit::exit_code_for(&error),
                ExitCode::Challenge,
                "{check}, stored: {stored}: {error:#}"
            );
            assert!(
                format!("{error:#}").contains("/challenge/424242/"),
                "{check}, stored: {stored}: the link was not given: {error:#}"
            );
            assert_eq!(
                requests(&server).await,
                1,
                "{check}, stored: {stored}: only the poll was sent"
            );
        }
    }
}

/// A session Instagram no longer takes, met on the counter poll, ends the run
/// as one — exit code 3, log in again — whether or not a list is stored:
/// nothing stored answers it. So does a session refused for the User-Agent it
/// is sent with, which a new login is also what fixes.
#[tokio::test]
async fn an_expired_session_on_the_counter_poll_is_reported_as_one() {
    for (status, body) in [
        (401, r#"{"message":"","status":"fail"}"#),
        (403, r#"{"message":"login_required","status":"fail"}"#),
        (401, r#"{"message":"useragent mismatch","status":"fail"}"#),
    ] {
        for stored in [true, false] {
            let server = MockServer::start().await;
            mount_profile(
                &server,
                r#"{"id":"42","username":"me","edge_followed_by":{"count":30},"edge_follow":{"count":10}}"#,
            )
            .await;
            mount_list(&server, Pk::new(42), 30).await;
            let tmp = tempfile::tempdir().unwrap();
            if stored {
                execute_with(
                    &server,
                    reopen(tmp.path()),
                    Arc::new(UnlimitedRateBudget),
                    &args(),
                )
                .await
                .unwrap();
            }

            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/users/web_profile_info/"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            mount_list(&server, Pk::new(42), 30).await;

            let (_, budget) = budget_at(tmp.path());
            let error = execute_with(&server, reopen(tmp.path()), budget, &args())
                .await
                .unwrap_err();
            assert_eq!(
                snob_cli::exit::exit_code_for(&error),
                ExitCode::NoSession,
                "{status}, stored: {stored}: {error:#}"
            );
            assert_eq!(
                requests(&server).await,
                1,
                "{status}, stored: {stored}: only the poll was sent"
            );
        }
    }
}
