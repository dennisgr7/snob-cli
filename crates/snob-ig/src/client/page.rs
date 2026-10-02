//! Requests sent from inside a browser page, rather than by this process.
//!
//! **Why this exists.** This is the default last hop for every request to
//! Instagram. The other one, `reqwest`, is what runs under `SNOB_NO_BROWSER`
//! and against a test server: this process's own TLS handshake, its own HTTP/2
//! settings, the cookies it was handed at login and never updates, and a set
//! of headers computed to agree with a User-Agent this process is not. Every
//! one of those is a way for Instagram to tell that the session a browser
//! created is being used by something that is not that browser — a textbook
//! stolen-session signature. A request sent by `fetch()` from an open
//! instagram.com tab has none of them: the handshake, the cookie jar, the
//! rotating `rur` and `csrftoken`, the client hints and the `Referer` are the
//! browser's own, because the browser is the one sending it.
//!
//! So this crate defines only the shape of such a request, and of an intent
//! the tab builds one from ([`crate::web::Ask`]), and the one place a client
//! picks it up. Launching and driving the browser is `snob-cli`'s
//! (`headless/`), which already owns the DevTools pipe for the login; this
//! crate still compiles no browser code at all.
//!
//! The pacing, the budgets, the cooldowns and the classification of every
//! answer are untouched: a request from the page is paid for, charged and read
//! exactly as a `reqwest` one is. Only the last hop changes hands.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use snob_core::session::Session;

use crate::web::{Call, Told, Unbuilt};

/// The two methods a page sends with. Nothing else is sent from here either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Method {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
}

/// One request for the page to send.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PageRequest {
    pub method: Method,
    /// Absolute, on the page's own origin.
    pub url: String,
    /// Only the headers the site's own script adds. Everything a browser adds
    /// by itself — `Cookie`, `User-Agent`, the client hints, `Sec-Fetch-*`,
    /// `Accept-Encoding`, `Origin` — is the browser's to add, and it does.
    pub headers: Vec<(String, String)>,
    /// The page the request is made from, absolute. `fetch()` sends it as the
    /// `Referer`.
    pub referrer: String,
    /// A form-encoded body, for the POSTs built in the tab: the registry's
    /// reads and the two writes.
    pub body: Option<String>,
    /// A navigation rather than a fetch: the tab goes to `url` and the answer
    /// is the document it lands on. Only the process holding the browser
    /// sends one, for an `Ask::Document` or an `Ask::Pk`; one handed to it
    /// as a request is refused.
    pub navigate: bool,
    /// The most bytes of body worth reading.
    pub cap: u64,
    /// How long the page may take before giving up.
    pub timeout_ms: u64,
}

/// What came back.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PageResponse {
    pub status: u16,
    /// Lowercase names. `Set-Cookie` is never among them — the browser keeps
    /// it, which is the point.
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Where the request ended up, after any redirect the browser followed.
    pub url: String,
    pub redirected: bool,
    /// How many redirects the browser followed to get there. Only a
    /// navigation follows any; it can also land elsewhere with none counted.
    pub hops: u32,
    /// The body was longer than [`PageRequest::cap`] and was not kept.
    pub too_large: bool,
}

impl PageResponse {
    /// A header, by lowercase name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// What Instagram said about its own load while answering, as the
    /// client logs it.
    pub fn load(&self) -> Option<String> {
        super::transport::load_from(|name| self.header(name))
    }

    /// Which backend answered, and in what form: `x-stack` and
    /// `content-type`, as the client logs them.
    pub fn served(&self) -> Option<String> {
        super::transport::served_from(|name| self.header(name))
    }
}

/// Why a page handed back no answer. Three different things, read three
/// different ways — which is why this is not a string.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum PageError {
    /// The request got no answer: the network, or the page's own timeout.
    /// Worth a retry, as the same failure through `reqwest` is.
    #[error("{0}")]
    Unreachable(String),
    /// A write, and the browser holds no CSRF token to send it with.
    #[error("the browser holds no CSRF token to write with")]
    NoCsrfToken,
    /// The browser itself: it would not start, went away, or stopped
    /// answering its protocol.
    #[error("{0}")]
    Browser(String),
    /// Instagram has pushed back on this session, and it was already written
    /// down: the page refuses to send anything more. See [`PushedBack`].
    #[error("{}", crate::error::IgError::from(.0.clone()))]
    PushedBack(PushedBack),
    /// Not one of the calls snob may send ([`crate::allowlist`]), so it was
    /// never sent. Carries the method and the path.
    #[error("{}", crate::error::IgError::NotAllowed(.0.clone()))]
    NotAllowed(String),
    /// The page has not shown a value the call is built from, and nothing
    /// was sent. The browser is fine: this is never [`PageError::Browser`],
    /// which would close it.
    #[error("{}", crate::error::IgError::PageNotReady(.0.clone()))]
    NotReady(String),
    /// The tab's document is signed out, or served to another account, so
    /// nothing was sent as this one.
    #[error("{}", crate::error::IgError::SessionExpired)]
    LoggedOut,
}

impl From<PageError> for crate::error::IgError {
    fn from(error: PageError) -> Self {
        match error {
            PageError::Unreachable(why) => Self::Unreachable(why),
            PageError::NoCsrfToken => Self::NoCsrfToken,
            PageError::Browser(why) => Self::Browser(why),
            PageError::PushedBack(cause) => cause.into(),
            PageError::NotAllowed(what) => Self::NotAllowed(what),
            PageError::NotReady(what) => Self::PageNotReady(what),
            PageError::LoggedOut => Self::SessionExpired,
        }
    }
}

impl From<Unbuilt> for PageError {
    fn from(unbuilt: Unbuilt) -> Self {
        match unbuilt {
            Unbuilt::Missing(missing) => Self::NotReady(missing.0.into()),
            Unbuilt::LoggedOut => Self::LoggedOut,
            Unbuilt::NotAllowed(what) => Self::NotAllowed(what),
        }
    }
}

/// A push-back the browser heard and has already written down.
///
/// **Why it exists.** Instagram talks to the session through every call its
/// app makes on the page, not only through snob's own, and a 429 or a
/// challenge on one of the app's calls is heard by the browser before snob's
/// next request would hear it. The process holding the browser records every
/// push-back heard on the page — the app's, and the answers to snob's own
/// requests — once, and from then on its page refuses with this, carrying the
/// cause, so a command stops with the right code and the challenge's link.
///
/// **Never recorded by the client.** It becomes the matching [`IgError`]
/// through `From`, which is not the path that records a cooldown. A cooldown
/// repeated within a day doubles, so one push-back written down twice would
/// cost twice.
///
/// [`IgError`]: crate::error::IgError
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PushedBack {
    RateLimited,
    FeedbackRequired,
    Challenge { url: Option<String> },
    Checkpoint { url: Option<String> },
}

impl PushedBack {
    /// The push-back an error is, if it is one.
    pub fn of(error: &crate::error::IgError) -> Option<Self> {
        use crate::error::IgError;
        match error {
            IgError::RateLimited => Some(Self::RateLimited),
            IgError::FeedbackRequired => Some(Self::FeedbackRequired),
            IgError::Challenge { url } => Some(Self::Challenge { url: url.clone() }),
            IgError::Checkpoint { url } => Some(Self::Checkpoint { url: url.clone() }),
            _ => None,
        }
    }
}

impl From<PushedBack> for crate::error::IgError {
    fn from(cause: PushedBack) -> Self {
        match cause {
            PushedBack::RateLimited => Self::RateLimited,
            PushedBack::FeedbackRequired => Self::FeedbackRequired,
            PushedBack::Challenge { url } => Self::Challenge { url },
            PushedBack::Checkpoint { url } => Self::Checkpoint { url },
        }
    }
}

/// What sending returns: boxed, because the one implementation is behind a
/// trait object and lives in another crate.
pub type PageFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PageResponse, PageError>> + Send + 'a>>;

/// What asking returns, boxed for the same reason.
pub type AskFuture<'a> = Pin<Box<dyn Future<Output = Result<Told, PageError>> + Send + 'a>>;

/// A browser tab on Instagram that sends what it is given.
pub trait Page: Send + Sync {
    fn send(&self, request: PageRequest) -> PageFuture<'_>;

    /// Answers an intent: the tab builds the request from its document's
    /// values right before it sends it ([`crate::web::build_call`]), or
    /// answers from the document itself, and hands back what came of it.
    fn ask(&self, call: Call) -> AskFuture<'_> {
        let _ = call;
        Box::pin(std::future::ready(Err(PageError::Browser(
            "this page answers no intents".into(),
        ))))
    }

    /// A push-back the browser has heard and written down already, if it has.
    /// Asked before anything is paid for, so a command learns of it without
    /// spending a request — and learns the cause, which the cooldown alone
    /// does not say.
    fn heard(&self) -> Option<PushedBack> {
        None
    }
}

/// Builds the page a client for this session sends through.
///
/// Called once per client; an implementation shares one browser between them,
/// because two browsers on one profile cannot both run.
pub type PageFactory = Arc<dyn Fn(&Session) -> Arc<dyn Page> + Send + Sync>;

/// Where every client built after this is set sends its requests from.
///
/// A process-global for the reason [`super::SANDBOX_BASE`] and `http::TRUST`
/// are: three separate places build a client, one of them inside this crate
/// with no path from the binary's arguments, and the answer is the same for
/// the whole process. Set once from `main`, before any client exists.
static FACTORY: OnceLock<PageFactory> = OnceLock::new();

/// Sends every request of every client built from now on from a browser page.
///
/// `Err` hands the factory back on a second call.
pub fn send_every_request_from(factory: PageFactory) -> Result<(), PageFactory> {
    FACTORY.set(factory)
}

/// The page a client for `session` should use, when one was set up.
pub(crate) fn page_for(session: &Session) -> Option<Arc<dyn Page>> {
    FACTORY.get().map(|factory| allowlisted(factory(session)))
}

/// `page`, refusing whatever is not one of the calls snob may send before it
/// leaves. Every page a client sends through is one of these.
pub(crate) fn allowlisted(page: Arc<dyn Page>) -> Arc<dyn Page> {
    Arc::new(Allowlisted(page))
}

struct Allowlisted(Arc<dyn Page>);

impl Page for Allowlisted {
    fn send(&self, request: PageRequest) -> PageFuture<'_> {
        if let Some(what) = crate::allowlist::refused(&request) {
            tracing::error!(request = %what, "refused a call snob may not send");
            return Box::pin(std::future::ready(Err(PageError::NotAllowed(what))));
        }
        self.0.send(request)
    }

    fn ask(&self, call: Call) -> AskFuture<'_> {
        if let Some(what) = crate::allowlist::refused_ask(&call.ask) {
            tracing::error!(request = %what, "refused a call snob may not send");
            return Box::pin(std::future::ready(Err(PageError::NotAllowed(what))));
        }
        self.0.ask(call)
    }

    fn heard(&self) -> Option<PushedBack> {
        self.0.heard()
    }
}

/// Whether a client pointed somewhere other than Instagram uses the page too.
///
/// Off by default, so a test server is reached the way the rest of the suite
/// reaches it. A testing build turns it on to drive the page path end to end
/// against a fake Instagram served locally.
#[cfg(feature = "testing")]
static OFF_INSTAGRAM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Uses the page even for a client that is not pointed at Instagram.
#[cfg(feature = "testing")]
pub fn use_the_page_off_instagram() {
    OFF_INSTAGRAM.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn used_off_instagram() -> bool {
    #[cfg(feature = "testing")]
    {
        OFF_INSTAGRAM.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(feature = "testing"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness::{Scripted, page_said};
    use super::*;

    fn request(method: Method, path: &str) -> PageRequest {
        PageRequest {
            method,
            url: format!("https://www.instagram.com{path}"),
            headers: Vec::new(),
            referrer: "https://www.instagram.com/".into(),
            body: Some("fb_dtsg=relay%3Atoken".into()),
            navigate: false,
            cap: 1 << 20,
            timeout_ms: 30_000,
        }
    }

    /// An answer from the backend that sends no load still says which one it
    /// was, so the absent load reads as that backend's ordinary answer.
    #[test]
    fn an_answer_says_which_backend_served_it() {
        let mut answer = page_said(500, "");
        answer.headers = vec![
            ("x-stack".into(), "www".into()),
            ("content-type".into(), "text/html; charset=utf-8".into()),
        ];
        assert_eq!(answer.load(), None);
        assert_eq!(
            answer.served().as_deref(),
            Some("x-stack=www content-type=text/html; charset=utf-8")
        );
        assert_eq!(page_said(200, "").served(), None);
    }

    #[tokio::test]
    async fn a_call_snob_may_not_send_never_reaches_the_browser() {
        let browser = Scripted::new(|_| Ok(page_said(200, "{}")));
        let page = allowlisted(browser.clone());

        let error = page
            .send(request(Method::Post, "/api/v1/web/something/"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, PageError::NotAllowed(what) if what == "POST /api/v1/web/something/"),
            "{error:?}"
        );
        assert!(browser.asked().is_empty());
        let said = crate::error::IgError::from(error).to_string();
        assert!(said.contains("POST /api/v1/web/something/"), "{said}");
        assert!(!said.contains("relay:token"), "{said}");

        // A GET is a friendship list's, or it is not sent either.
        let error = page
            .send(request(Method::Get, "/api/v1/friendships/pending/"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, PageError::NotAllowed(what) if what == "GET /api/v1/friendships/pending/"),
            "{error:?}"
        );
        assert!(browser.asked().is_empty());
        let answer = page
            .send(request(
                Method::Get,
                "/api/v1/friendships/2345678901/following/",
            ))
            .await
            .unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(browser.asked().len(), 1);
    }
}
