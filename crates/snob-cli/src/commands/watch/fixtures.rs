//! What the tests of every file here build their reports out of.
//!
//! Gathered in one place rather than left loose in one test module, because
//! every file here has a test module of its own and a fixture private to a
//! sibling is a fixture that gets written twice. `pub(in ...watch)` and no
//! further: they are shapes for tests, and nothing outside this module has any
//! business with them.

use snob_core::Pk;
use std::time::Duration;

use snob_core::Epoch;
use snob_core::model::{ListKind, User};
use snob_core::watch::{Basis, ListDiff, Rename};
use snob_store::config::{self, WatchConfig};
use snob_store::store::deliveries;
use url::Url;

use crate::engine::watch::{ListReport, WatchReport};
use crate::watch::webhook::{Webhook, WebhookClient};

use super::delivery::Delivery;

pub(in crate::commands::watch) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
pub(in crate::commands::watch) const SID: &str = "42%3AAbCdEfGh%3A20";

pub(in crate::commands::watch) fn user(pk: Pk, name: &str) -> User {
    User {
        pk,
        username: name.into(),
        full_name: None,
        is_private: None,
        is_verified: None,
        pfp_url: None,
    }
}

pub(in crate::commands::watch) fn report_with(
    followers: Option<ListReport>,
    renamed: Vec<Rename>,
) -> WatchReport {
    WatchReport {
        account_pk: Pk::new(42),
        username: Some("me".into()),
        is_self: true,
        followers,
        following: None,
        renamed,
    }
}

pub(in crate::commands::watch) fn list(
    basis: Basis,
    diff: ListDiff,
    since: Option<Epoch>,
) -> ListReport {
    ListReport {
        kind: ListKind::Followers,
        basis,
        since,
        until: Epoch::new(2_000),
        diff,
        total: 10,
    }
}

/// `pk` known in `db` as `name`, so rows that point at the account have
/// something to point at.
pub(in crate::commands::watch) fn known_account(
    db: &snob_store::store::Store,
    pk: Pk,
    name: &str,
    is_self: bool,
) {
    snob_store::store::users::upsert(db.conn(), &user(pk, name)).unwrap();
    snob_store::store::accounts::upsert(db.conn(), pk, is_self).unwrap();
}

/// An app and a webhook pointed at the same mock server.
pub(in crate::commands::watch) fn app_posting_to(
    server: &wiremock::MockServer,
) -> (crate::app::App, Delivery) {
    let db = snob_store::store::Store::in_memory().unwrap();
    known_account(&db, Pk::new(42), "me", true);
    (app_on(db, server), delivery_to(server))
}

/// An app acting as account 42 over `db`, asking `server`.
pub(in crate::commands::watch) fn app_on(
    db: snob_store::store::Store,
    server: &wiremock::MockServer,
) -> crate::app::App {
    let session = snob_core::session::Session::from_sessionid(
        SID,
        UA,
        snob_core::session::SessionOrigin::Paste,
    )
    .unwrap();
    let client = snob_ig::client::IgClient::new(session, snob_ig::pace::Pacer::unlimited())
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap());

    crate::app::App::for_test(
        client,
        db,
        crate::app::Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        },
    )
}

/// A webhook pointed at `server`'s `/hook`.
pub(in crate::commands::watch) fn delivery_to(server: &wiremock::MockServer) -> Delivery {
    let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
    Delivery {
        destination: super::delivery::destination_of(&url),
        signed: false,
        client: WebhookClient::new(Webhook {
            url,
            headers: vec![],
            key: None,
        })
        .unwrap(),
        heartbeat: false,
    }
}

/// A `watch.toml` as the tool would read one.
pub(in crate::commands::watch) fn watch_toml(body: &str) -> WatchConfig {
    config::parse(body, std::path::Path::new("watch.toml")).expect("the fixture parses")
}

/// A report queued longer ago than it can be news for, in a store at
/// `paths`. Nothing but a settle can move it: `due` will not hand back an
/// over-age row, and only a failed attempt expires one.
pub(in crate::commands::watch) fn owed_long_ago(
    paths: &snob_store::paths::AccountPaths,
    now: Epoch,
) -> i64 {
    let db = snob_store::store::Store::open(paths).unwrap();
    known_account(&db, Pk::new(42), "me", true);
    deliveries::enqueue(
        db.conn(),
        "run-old",
        Pk::new(42),
        "{}",
        now - Duration::from_secs(deliveries::MAX_AGE_SECS as u64 + 1),
        "https://receiver.example",
    )
    .unwrap()
}
