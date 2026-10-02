//! Fixtures shared by the integration tests.
//!
//! The credentials, the default `ListArgs` and the `App` builder live here once,
//! so adding a flag to `ListArgs` touches this file and not every suite that is
//! testing something else.
//!
//! Not every binary uses every item, and a `tests/common/mod.rs` is compiled
//! separately into each one that declares it, so anything one of them does not
//! touch is reported as dead code there. `allow` rather than `expect`: whether
//! anything is in fact unused differs per binary, and `expect` warns about
//! itself in the binaries that happen to use all of it.
#![allow(dead_code)]

/// The fake Instagram in the web app's shapes, and a tab on it with no
/// browser. Only a `testing` build can hand a client a page of its own
/// (`IgClient::with_page`), and most of the binaries that compile this file
/// are not one.
#[cfg(feature = "testing")]
pub mod ig;
#[cfg(feature = "testing")]
pub mod tab;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use snob_cli::app::{App, Viewer};
use snob_cli::cli::{
    BrowseArgs, ConsentArgs, FilterArgs, ListArgs, OutputArgs, ProgressArgs, WalkArgs,
};
use snob_core::Pk;
use snob_core::budget::{RateBudget, UnlimitedRateBudget};
use snob_core::model::User;
use snob_core::session::{Session, SessionOrigin};
use snob_ig::client::IgClient;
use snob_ig::pace::Pacer;
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::secrets::SecretStore;
use snob_store::store::Store;
use snob_store::store::rate_budget::SqliteRateBudget;
use url::Url;
use wiremock::MockServer;

/// A plausible desktop Chrome User-Agent. The session is tied to one, and
/// Instagram checks that the two agree.
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// A `sessionid` in the shape Instagram issues: the account id, a token and a
/// version, percent-encoded as the cookie carries them.
pub const SID: &str = "42%3AAbCdEfGh%3A20";

/// The arguments a list command gets with no flags given.
///
/// `no_progress` and `yes` are the two that differ from the real defaults, and
/// deliberately: a bar drawing into the test harness is noise, and a test that
/// stops to ask for consent hangs.
pub fn args() -> ListArgs {
    ListArgs {
        target: None,
        filter: FilterArgs::default(),
        output: OutputArgs::default(),
        limit: None,
        browse: BrowseArgs::default(),
        walk: WalkArgs {
            progress: ProgressArgs { no_progress: true },
            consent: ConsentArgs { yes: true },
            ..WalkArgs::default()
        },
    }
}

/// The same arguments, aimed at somebody else's account.
pub fn args_for(target: &str) -> ListArgs {
    ListArgs {
        target: Some(target.to_string()),
        ..args()
    }
}

/// The account every fixture signs in as: the id inside [`SID`].
pub const ME: Pk = Pk::new(42);

/// A client of the mock server, signed in with the pasted [`SID`] and spending
/// from `budget`.
pub fn client_with(server: &MockServer, budget: Arc<dyn RateBudget>) -> IgClient {
    let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    IgClient::new(session, Pacer::new(budget))
        .unwrap()
        .with_base_url(Url::parse(&server.uri()).unwrap())
}

/// [`client_with`] on an unlimited budget.
pub fn client(server: &MockServer) -> IgClient {
    client_with(server, Arc::new(UnlimitedRateBudget))
}

/// An `App` over [`client_with`], viewing as [`ME`] under the name "me".
pub fn app_with(server: &MockServer, db: Store, budget: Arc<dyn RateBudget>) -> App {
    App::for_test(
        client_with(server, budget),
        db,
        Viewer {
            pk: ME,
            username: Some("me".into()),
        },
    )
}

/// [`app_with`] on an unlimited budget: what almost every test wants.
pub fn app(server: &MockServer, db: Store) -> App {
    app_with(server, db, Arc::new(UnlimitedRateBudget))
}

/// An `App` of `server`, as [`app_with`] builds one, that asks and sends
/// through a [`tab::TestTab`] on `world`: the browser path, with no browser.
#[cfg(feature = "testing")]
pub fn app_web(
    server: &MockServer,
    world: &ig::World,
    db: Store,
    budget: Arc<dyn RateBudget>,
    viewer: Viewer,
) -> App {
    let client = client_with(server, budget);
    let tab = tab::TestTab::new(world, client.session());
    App::for_test(client.with_page(Arc::new(tab)), db, viewer)
}

/// The database a test keeps under its own temporary root.
pub fn open_db(root: &Path) -> Store {
    Store::open_at(&root.join("test.db")).unwrap()
}

/// Account [`ME`]'s files under a test's own root.
pub fn account_at(root: &Path) -> AccountPaths {
    AppPaths::rooted_at(root).account(ME)
}

/// A budget over account [`ME`]'s database under `root`, and where it is.
///
/// Opening the store first is not optional: it is what creates the schema, and
/// the budget opens its own connection to the same file expecting tables.
pub fn budget_at(root: &Path) -> (AccountPaths, Arc<SqliteRateBudget>) {
    let paths = account_at(root);
    let _schema = Store::open(&paths).unwrap();
    let budget = Arc::new(SqliteRateBudget::open(&paths).unwrap());
    (paths, budget)
}

pub const HOUR: Duration = Duration::from_secs(3600);

/// Puts the account in a rate-limit cooldown of `length`, as a push-back does.
pub fn start_cooldown(budget: &SqliteRateBudget, length: Duration) {
    budget.start_cooldown("rate_limit", length).unwrap();
}

/// A secret store beside `paths`, on a keyring service named after the test
/// and this process: the real service belongs to the operating system.
pub fn secrets(paths: &AccountPaths, name: &str) -> SecretStore {
    SecretStore::new(AppPaths::clone(paths), false)
        .with_service(&format!("snob-ig-test-{name}-{}", std::process::id()))
}

/// A user as a list page names one: an id and a name, nothing else known.
pub fn user(pk: u64, name: &str) -> User {
    User {
        pk: Pk::new(pk),
        username: name.into(),
        full_name: None,
        is_private: None,
        is_verified: None,
        pfp_url: None,
    }
}

/// How many requests the mock server has answered.
pub async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

/// Puts the follow and unfollow mutation ids in account [`ME`]'s database
/// under a sandbox `root`, as a machine that has already discovered them
/// would have.
///
/// **Discovery cannot be exercised by the binary tests, deliberately.** It
/// walks `static.cdninstagram.com`, and the host is fixed in
/// `graphql::bundles_in` rather than taken from the page — the property that
/// makes walking a document's URLs safe at all, and therefore one that cannot
/// be pointed at a mock server without giving it up. The walk's two halves
/// are pure functions with tests of their own; what the binary tests exercise
/// is the request built from what they found.
///
/// Written through the same `Store` the binary uses, so the row lands where the
/// run will look for it rather than where the test thinks it should.
pub fn remember_doc_ids(root: &Path) {
    let db = Store::open(&account_at(root)).expect("the sandbox database opens");
    for (name, id) in [
        ("usePolarisFollowMutation", "26508036048874888"),
        ("usePolarisUnfollowMutation", "27789106940691111"),
    ] {
        db.remember(&format!("graphql.doc_id.{name}"), id)
            .expect("the sandbox database is writable");
    }
}
