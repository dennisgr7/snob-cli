//! What the tests in this module's files, and the pager's, build their clients
//! out of: one copy rather than one per file. Anything only one concern's tests
//! reach for stays with those tests.

use std::sync::Arc;

use snob_core::EpochMs;
use snob_core::budget::{RateBudget, RateBudgetError};
use snob_core::session::{Session, SessionOrigin};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::IgClient;
use super::page::{AskFuture, Page, PageError, PageFuture, PageRequest, PageResponse};
use crate::graphql;
use crate::model::web::route_answer;
use crate::pace::Pacer;
use crate::web::{self, AppCalls, Ask, Call, Told};

pub(crate) const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
pub(crate) const SID: &str = "42%3AAbCdEfGh%3A20";

/// The one way every client here is built: pointed at `base`, paying through
/// `pacer`, and with a CSRF token when it has to be able to write.
fn built(base: &str, pacer: Pacer, can_write: bool) -> IgClient {
    let mut session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
    if can_write {
        session.csrftoken = Some("TOKEN".into());
    }
    IgClient::new(session, pacer)
        .unwrap()
        .with_base_url(Url::parse(base).unwrap())
}

/// A client of `server` that pays through `pacer`.
pub(crate) fn client_with(server: &MockServer, pacer: Pacer) -> IgClient {
    built(&server.uri(), pacer, false)
}

pub(super) async fn client(server: &MockServer) -> IgClient {
    client_with(server, Pacer::unlimited())
}

/// A budget that remembers what it was told to write down.
///
/// `Pacer::unlimited()` answers `start_cooldown` with `Ok(0)` and forgets, so
/// without this nothing would observe the recording half of
/// [`IgClient::classify_and_record`]: the rule that decides whether the next
/// run walks back into an account Instagram has just flagged.
#[derive(Default)]
pub(super) struct Recording {
    started: std::sync::Mutex<Vec<(String, std::time::Duration)>>,
}

impl Recording {
    pub(super) fn calls(&self) -> Vec<(String, std::time::Duration)> {
        self.started.lock().unwrap().clone()
    }
}

impl snob_core::budget::RateBudget for Recording {
    fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
        Ok(std::time::Duration::ZERO)
    }
    fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.reserve()
    }
    fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
        Ok(None)
    }
    fn start_cooldown(
        &self,
        reason: &str,
        minimum: std::time::Duration,
    ) -> Result<EpochMs, RateBudgetError> {
        self.started
            .lock()
            .unwrap()
            .push((reason.to_string(), minimum));
        Ok(EpochMs::new(0))
    }
}

/// A client whose budget can be asked afterwards what it was told.
pub(super) fn watching(base: &str) -> (IgClient, Arc<Recording>) {
    watching_as(base, false)
}

/// The same, with the option of a session that can write. Kept as one
/// helper so the two paths are driven through identical wiring and any
/// difference in what gets recorded is the code's rather than the test's.
pub(super) fn watching_as(base: &str, can_write: bool) -> (IgClient, Arc<Recording>) {
    let budget = Arc::new(Recording::default());
    let pacer = Pacer::new(Arc::clone(&budget) as Arc<dyn RateBudget>);
    (built(base, pacer, can_write), budget)
}

pub(super) async fn answering(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

/// A session with a CSRF token, which is what `login --browser` produces
/// and what the write path requires.
pub(super) async fn writer(server: &MockServer) -> IgClient {
    built(&server.uri(), Pacer::unlimited(), true)
}

/// A page that hands out both tokens, served to the session's account (pk
/// 42), which is what a logged-in one does.
pub(super) const LOGGED_IN_PAGE: &str = r#"<html><script>
    {"define":[["DTSGInitData",[],{"token":"DTSG-TOKEN"},258],
               ["LSD",[],{"token":"LSD-TOKEN"},323],
               ["PolarisViewer",[],{"data":{"id":"42","username":"me","fbid":"17841400000000042"},"id":"42"},1508]]}
    </script></html>"#;

/// A cache that already knows the ids.
///
/// **Every write test uses this, and that is a decision worth naming.**
/// Discovery walks `static.cdninstagram.com`, and the host is fixed in
/// `graphql::bundles_in` rather than taken from the document — which is the
/// property that makes walking a page's URLs safe, and which therefore
/// cannot be pointed at a mock server. Weakening it so a test could reach it
/// would be trading the guard for the coverage. The walk's two halves are
/// pure functions and are tested directly in `graphql`; what is exercised
/// here is everything around them.
pub(super) struct Known;

impl graphql::DocIds for Known {
    fn get(&self, name: &str) -> Option<String> {
        Some(match name {
            "usePolarisFollowMutation" => "26508036048874888".into(),
            _ => "27789106940691111".into(),
        })
    }
    fn put(&self, _: &str, _: &str) {}
}

/// A server that answers the page and the mutation, which is the pair every
/// write needs.
pub(super) async fn instagram_that_takes_a_write(body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    server
}

/// What the mutation answers.
pub(super) const FOLLOWED: &str = r#"{"result":"following","status":"ok"}"#;

type Answering = dyn Fn(&PageRequest) -> Result<PageResponse, PageError> + Send + Sync;
type Telling = dyn Fn(&Call) -> Result<Told, PageError> + Send + Sync;

/// What a [`Scripted`] page does with an intent.
enum Intents {
    /// Nothing: it is a page that only sends.
    None,
    /// Whatever the closure says.
    Told(Box<Telling>),
    /// Builds the request as the tab does, from invented page values and
    /// app calls, and sends it through the page's own answer. The stories
    /// tray is the one the page shows, if any.
    Building(Box<std::sync::Mutex<AppCalls>>, Option<Vec<String>>),
}

/// A page with no browser behind it: it answers what `answer` says to each
/// request, and keeps what it was asked to send, and each intent it was
/// asked.
pub(crate) struct Scripted {
    answer: Box<Answering>,
    intents: Intents,
    asked: std::sync::Mutex<Vec<PageRequest>>,
    asks: std::sync::Mutex<Vec<Call>>,
}

impl Scripted {
    pub(super) fn new(
        answer: impl Fn(&PageRequest) -> Result<PageResponse, PageError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::made(Box::new(answer), Intents::None)
    }

    /// A page that answers each intent as `tell` says, and sends nothing.
    pub(crate) fn telling(
        tell: impl Fn(&Call) -> Result<Told, PageError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::made(
            Box::new(|_| Err(PageError::Browser("this page only answers intents".into()))),
            Intents::Told(Box::new(tell)),
        )
    }

    /// A page that builds each intent with [`web::build_call`], the tab's
    /// own step, on the invented page of `web::fixtures` logged in as the
    /// session of [`SID`], and sends it through `answer`. The viewer and a
    /// document are answered from the same page: no stories tray, and no
    /// bundle URLs, so no test downloads one.
    pub(super) fn building(
        answer: impl Fn(&PageRequest) -> Result<PageResponse, PageError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::building_under(None, answer)
    }

    /// [`Self::building`], on a page that shows the stories tray `tray`:
    /// the ids of its reels, in order.
    pub(super) fn building_under(
        tray: Option<&[&str]>,
        answer: impl Fn(&PageRequest) -> Result<PageResponse, PageError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::made(
            Box::new(answer),
            Intents::Building(
                Box::new(std::sync::Mutex::new(web::fixtures::app())),
                tray.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
            ),
        )
    }

    fn made(answer: Box<Answering>, intents: Intents) -> Arc<Self> {
        Arc::new(Self {
            answer,
            intents,
            asked: std::sync::Mutex::new(Vec::new()),
            asks: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Takes in one of the app's calls on the invented page, as the tab's
    /// listener does: for a test of what a call built after it copies.
    pub(super) fn app_saw(&self, path: &str, form: &str) {
        if let Intents::Building(app, _) = &self.intents {
            app.lock().unwrap().saw(path, form);
        }
    }

    /// What it was asked to send, in order.
    pub(super) fn asked(&self) -> Vec<PageRequest> {
        self.asked.lock().unwrap().clone()
    }

    /// The intents it was asked, in order.
    pub(crate) fn asks(&self) -> Vec<Call> {
        self.asks.lock().unwrap().clone()
    }

    fn sent(&self, request: PageRequest) -> Result<PageResponse, PageError> {
        let answer = (self.answer)(&request);
        self.asked.lock().unwrap().push(request);
        answer
    }

    /// The tab's answer to `call`, built on the invented page.
    fn built(
        &self,
        app: &std::sync::Mutex<AppCalls>,
        tray: &Option<Vec<String>>,
        call: &Call,
    ) -> Result<Told, PageError> {
        let page = page_of_the_session();
        let viewer = page.viewer.clone().expect("the invented page has a viewer");
        match &call.ask {
            Ask::Viewer => Ok(Told::Viewer(viewer)),
            Ask::Tray => Ok(Told::Tray(tray.clone())),
            Ask::Document { path } => {
                let request = PageRequest {
                    method: super::page::Method::Get,
                    url: format!("{}{path}", call.origin),
                    headers: Vec::new(),
                    referrer: format!("{}{}", call.origin, call.referrer),
                    body: None,
                    navigate: true,
                    cap: call.cap,
                    timeout_ms: call.timeout_ms,
                };
                let mut answer = self.sent(request)?;
                // As the tab: a document that says whom it was served to is
                // this session's, or logged out. One that says nothing is
                // taken as the invented page's.
                let shown = crate::page_values::PageValues::parse(&answer.body);
                if (200..300).contains(&answer.status)
                    && (shown.signed_out || shown.viewer.is_some())
                {
                    web::viewer_of(&shown, viewer.pk)?;
                }
                answer.body.clear();
                Ok(Told::Document {
                    answer,
                    bundles: Vec::new(),
                })
            }
            ask => {
                let account = viewer.pk;
                let request = {
                    let mut app = app.lock().unwrap();
                    let rest_token = app.rest_token_sent().map(str::to_string);
                    let seen = web::Seen {
                        rest_token: rest_token.as_deref(),
                        ..web::Seen::default()
                    };
                    web::build_call(call, &page, &mut app, account, &seen)?
                };
                let answer = self.sent(request)?;
                Ok(match ask {
                    Ask::Pk { name } => Told::Pk {
                        pk: route_answer(&answer.body, &format!("/{name}/")),
                        answer,
                    },
                    _ => Told::Answer(answer),
                })
            }
        }
    }
}

/// The invented page of `web::fixtures`, served to the session of [`SID`].
fn page_of_the_session() -> crate::page_values::PageValues {
    let mut page = web::fixtures::page();
    if let Some(viewer) = page.viewer.as_mut() {
        viewer.pk = snob_core::Pk::new(42);
    }
    page
}

impl Page for Scripted {
    fn send(&self, request: PageRequest) -> PageFuture<'_> {
        let answer = self.sent(request);
        Box::pin(async move { answer })
    }

    fn ask(&self, call: Call) -> AskFuture<'_> {
        let told = match &self.intents {
            Intents::None => Err(PageError::Browser("this page answers no intents".into())),
            Intents::Told(tell) => tell(&call),
            Intents::Building(app, tray) => self.built(app, tray, &call),
        };
        self.asks.lock().unwrap().push(call);
        Box::pin(async move { told })
    }
}

/// A budget that counts what it was charged, and can be put in a cooldown.
#[derive(Default)]
pub(crate) struct Counting {
    reads: std::sync::atomic::AtomicU32,
    writes: std::sync::atomic::AtomicU32,
    cooling: std::sync::atomic::AtomicBool,
    /// What it was asked, in order: `read`, `write`, and `write wait` for
    /// a write's wait asked without spending.
    order: std::sync::Mutex<Vec<&'static str>>,
}

impl Counting {
    /// What it was asked, in order.
    pub(super) fn order(&self) -> Vec<&'static str> {
        self.order.lock().unwrap().clone()
    }

    /// The reads and the writes charged.
    pub(super) fn reserved(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.reads.load(Relaxed), self.writes.load(Relaxed))
    }

    /// Puts the account in a cooldown from now on.
    pub(super) fn cool_down(&self) {
        self.cooling
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl RateBudget for Counting {
    fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.order.lock().unwrap().push("read");
        Ok(std::time::Duration::ZERO)
    }
    fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.order.lock().unwrap().push("write");
        Ok(std::time::Duration::ZERO)
    }
    fn write_wait(&self) -> Result<std::time::Duration, RateBudgetError> {
        self.order.lock().unwrap().push("write wait");
        Ok(std::time::Duration::ZERO)
    }
    fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
        let cooling = self.cooling.load(std::sync::atomic::Ordering::Relaxed);
        Ok(cooling.then_some(EpochMs::new(i64::MAX)))
    }
    fn start_cooldown(&self, _: &str, _: std::time::Duration) -> Result<EpochMs, RateBudgetError> {
        self.cool_down();
        Ok(EpochMs::new(i64::MAX))
    }
}

/// A client that sends only through `page`, paying through a budget that
/// counts, pointed at an address nothing listens on: nothing it does can
/// reach a server.
pub(crate) fn spending_client(page: Arc<dyn Page>) -> (IgClient, Arc<Counting>) {
    let budget = Arc::new(Counting::default());
    let pacer = Pacer::new(Arc::clone(&budget) as Arc<dyn RateBudget>);
    (built(NOWHERE, pacer, false).through(page), budget)
}

/// A client of `server` whose budget counts what it is charged, for a test
/// that has to show a request cost nothing.
pub(super) fn counting_client_of(server: &MockServer) -> (IgClient, Arc<Counting>) {
    let budget = Arc::new(Counting::default());
    let pacer = Pacer::new(Arc::clone(&budget) as Arc<dyn RateBudget>);
    (built(&server.uri(), pacer, false), budget)
}

/// An address nothing listens on, for a client that must not send.
pub(super) const NOWHERE: &str = "http://127.0.0.1:9/";

/// A page's answer: `status` with `body`, where it was asked.
pub(crate) fn page_said(status: u16, body: &str) -> PageResponse {
    PageResponse {
        status,
        body: body.to_string(),
        ..Default::default()
    }
}
