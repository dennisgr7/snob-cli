//! HTTP client against Instagram's web API.
//!
//! It uses the `www.instagram.com` endpoints rather than the `i.instagram.com`
//! ones, to stay consistent with a session that originated in a desktop
//! browser. That consistency between cookie, User-Agent and endpoint is what
//! Instagram evaluates, and breaking it is what produces `useragent mismatch`.
//!
//! One `IgClient`, split by concern. What is here is the client itself: how it
//! is built, what it holds, where it is pointed, and the two steps every answer
//! goes through whichever half of the API produced it -- [`IgClient::decode`]
//! and [`IgClient::classify_and_record`]. The files below add to the same
//! `impl`, which is what lets each of them be read on its own.
//!
//! - [`transport`] -- how a request goes out and how the answer comes back:
//!   the origin rule, the redirect loop that pays for every hop it follows,
//!   the ceilings on what will be read, and the races against the cancel
//!   token.
//! - [`headers`] -- what a request says about itself. [`headers::Surface`] is
//!   the difference between the app talking to the API and the browser
//!   navigating to a page, and it decides most of the set.
//! - [`read`] -- the endpoints this tool reads, one method per URL.
//! - [`write`] -- the follow and the unfollow, and nothing else. One file, so
//!   that the rule about which function may send a method other than GET can
//!   be checked by opening it.
//! - [`media`] -- the CDN half: where a picture is allowed to come from, and
//!   the download that carries nothing identifying.
//! - [`page`] -- the browser tab every request to Instagram is sent from by
//!   default, and the allowlist wrapped around it.
//! - [`ask`] -- what the client asks that tab for by name rather than by
//!   request, and what each kind of ask is paid with.
//! - [`posts`] -- a profile's posts, one post, and its comments.

mod ask;
mod headers;
mod media;
pub mod page;
mod posts;
mod read;
mod transport;
mod write;

#[cfg(test)]
pub(crate) mod harness;

pub use self::posts::PostsPage;
pub use self::read::Direction;

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use serde::de::DeserializeOwned;
use snob_core::session::Session;
use url::Url;

use crate::BASE_URL;
use crate::client_hints::ClientHints;
use crate::error::{IgError, classify, refused};
use crate::pace::{Pace, Pacer};

use self::media::cdn_policy;
use self::transport::{Answer, api_policy, build_client, same_origin};

pub struct IgClient {
    /// Talks to Instagram, the reads and the two writes alike. Carries the
    /// session, and follows no redirect on its own: see [`api_policy`].
    api: reqwest::Client,
    /// Talks to the CDN. Carries nothing that identifies the account, and
    /// every hop has to be somewhere pictures come from.
    ///
    /// Two clients rather than one because the two have opposite rules: the
    /// API request must not leave instagram.com, and the asset request has to
    /// be allowed to move between CDN hosts. One client can only have one
    /// redirect policy, so sharing it meant the looser of the two governed the
    /// requests carrying the credentials.
    ///
    /// Built on first use, which is `snob pfp` and nothing else. A
    /// `reqwest::Client` is a connection pool and a TLS configuration — the
    /// platform trust store is read to assemble one — and every other command
    /// paid for that on the startup path to never send a request through it.
    cdn: OnceLock<reqwest::Client>,
    base: Url,
    session: Session,
    /// Reserving budget lives here rather than in each caller, so a request
    /// that is never paid for cannot be written.
    pacer: Pacer,
    hints: ClientHints,
    /// Instagram's session-continuity token. See [`IgClient::claim`].
    claim: Mutex<String>,
    /// The browser tab every request to Instagram is sent from, when there is
    /// one. See [`page`]. `None` sends them with `reqwest`: under
    /// `SNOB_NO_BROWSER`, and to a test server.
    page: Option<std::sync::Arc<dyn page::Page>>,
    /// The `X-Web-Session-ID` the client that sends without a browser puts on
    /// its REST reads: made once, as a page makes one for all of its calls.
    web_session: String,
    /// The highlights tray the last profile opened was read with, for the
    /// command that shows it to take rather than ask again. See
    /// `IgClient::profile_by_pk`.
    held_tray: Mutex<Option<ask::HeldTray>>,
    /// Whether a `show_many` has come back unbuilt for want of the token the
    /// app's REST POSTs carry: the next ones are not paid for. See
    /// `IgClient::statuses`.
    statuses_unbuilt: AtomicBool,
}

/// What a browser sends before the server has told it anything.
const INITIAL_CLAIM: &str = "0";

/// Whether `url` is Instagram itself: the one copy of the question both the
/// page and the pace ask of an address.
fn is_instagram(url: &Url) -> bool {
    static LIVE: std::sync::LazyLock<Url> =
        std::sync::LazyLock::new(|| Url::parse(BASE_URL).expect("BASE_URL parses"));
    same_origin(url, &LIVE)
}

/// Where every client built after this call points, in a testing build.
///
/// **Compiled out of a release build entirely**, feature and all: a shipped
/// binary has neither this function nor the flag that calls it, so there is no
/// way to point it at another server and nothing to disable. That is what the
/// whole Cargo feature buys over a hidden flag, and it is the reason it is a
/// feature.
///
/// A process-global rather than an argument, which is normally the wrong answer
/// and is the right one here. Three separate places build a client — `App`,
/// `whoami`, and `login::validate`, which is in this crate and takes no path
/// from the binary at all — so an argument would have to be threaded through
/// five signatures that exist in the release build, to carry a value that never
/// exists in it. It is set once from `main` before any client exists, and never
/// again: [`OnceLock::set`] returns the value back on a second call rather than
/// replacing it, so a run cannot be redirected halfway through.
///
/// It sets the **base URL**, not a "skip the pace" switch, which is the
/// difference that matters. [`IgClient::is_live`] answers by address, so a
/// client pointed here is genuinely not Instagram and turning the pace off is
/// telling the truth. The failure that shape avoids is the one a loopback-only
/// escape hatch would have created: a proxy on `127.0.0.1` forwarding to
/// Instagram is a test server by address and Instagram by content, and the walk
/// it produces is a real account read with no waits between pages.
///
/// The binary refuses `--ig-base-url` unless `--sandbox-root` is given too, so
/// the session a redirected client carries comes out of a store inside that
/// root. The stored session of the person running it is not reachable from
/// here.
#[cfg(feature = "testing")]
static SANDBOX_BASE: std::sync::OnceLock<Url> = std::sync::OnceLock::new();

/// Points every client built from now on somewhere other than Instagram.
///
/// See [`SANDBOX_BASE`]. `Err` carries the base already set, on a second call.
#[cfg(feature = "testing")]
pub fn point_every_client_at(base: Url) -> Result<(), Url> {
    SANDBOX_BASE.set(base)
}

impl IgClient {
    pub fn new(session: Session, pacer: Pacer) -> Result<Self, IgError> {
        // Read here rather than at each call site, because `login::validate`
        // builds a client inside this crate and never sees the binary's
        // arguments. In a release build this line does not exist.
        #[cfg(feature = "testing")]
        if let Some(base) = SANDBOX_BASE.get() {
            return Self::pointed_at(session, pacer, base.clone());
        }
        Self::pointed_at(session, pacer, Url::parse(BASE_URL)?)
    }

    fn pointed_at(session: Session, pacer: Pacer, base: Url) -> Result<Self, IgError> {
        let page = if is_instagram(&base) || page::used_off_instagram() {
            page::page_for(&session)
        } else {
            None
        };
        Ok(Self {
            page,
            web_session: headers::new_web_session(),
            hints: ClientHints::from_user_agent(&session.user_agent),
            api: build_client(&session.user_agent, api_policy())?,
            cdn: OnceLock::new(),
            base,
            session,
            pacer,
            claim: Mutex::new(INITIAL_CLAIM.to_string()),
            held_tray: Mutex::new(None),
            statuses_unbuilt: AtomicBool::new(false),
        })
    }

    /// The CDN client, built the first time a picture is downloaded.
    ///
    /// Everything the policy needs is already a field, so nothing has to be
    /// captured at construction time, and only `pfp` pays for building it.
    pub(in crate::client) fn cdn(&self) -> Result<&reqwest::Client, IgError> {
        if let Some(cdn) = self.cdn.get() {
            return Ok(cdn);
        }
        let built = build_client(&self.session.user_agent, cdn_policy(self.base.clone()))?;
        Ok(self.cdn.get_or_init(|| built))
    }

    /// Reads Instagram's answer and, if it was a push-back, records the
    /// cooldown before handing the error on.
    ///
    /// It lives here for the same reason the budget does: every caller needs
    /// the same reaction, and a 429 that left no mark would have the next run
    /// knock on the same door straight away. Making the request is what earns
    /// the cooldown, so the place that makes requests is the place that
    /// records it.
    pub(in crate::client) fn classify_and_record(&self, status: u16, body: &str) -> IgError {
        self.record(classify(status, body))
    }

    /// The second half of [`Self::classify_and_record`], for an answer that
    /// was classified some other way: a navigation's landing page, which has
    /// no status or body to classify.
    pub(in crate::client) fn record(&self, error: IgError) -> IgError {
        if let Some((reason, minimum)) = crate::error::cooldown_for(&error) {
            // A cooldown that cannot be written is not worth losing the real
            // error over: the caller still gets told what Instagram said.
            if let Err(e) = self.pacer.start_cooldown(reason, minimum) {
                tracing::warn!(error = %e, "could not record the cooldown");
            }
        }
        error
    }

    /// Points the client at a different server. Tests only.
    ///
    /// Rebuilds rather than assigns: both redirect policies are decided from
    /// the base URL when the client is made, so moving the field alone would
    /// leave them judging every hop against the wrong server.
    ///
    /// **This is also what turns the pace off**, through [`IgClient::is_live`],
    /// so a consumer of this crate that pointed a client at a proxy would walk
    /// Instagram with no waits between pages. `snob-cli` is the only consumer,
    /// and it only does this in tests.
    #[doc(hidden)]
    #[must_use]
    pub fn with_base_url(self, base: Url) -> Self {
        Self::pointed_at(self.session, self.pacer, base)
            .expect("rebuilding a client that already exists cannot fail")
    }

    /// The same client, sending through `page`: for the tests of what the
    /// client makes of a page's answers, with no browser behind it.
    #[cfg(test)]
    pub(crate) fn through(mut self, page: std::sync::Arc<dyn page::Page>) -> Self {
        self.page = Some(page::allowlisted(page));
        self
    }

    /// The same client, asking and sending through `page`, allowlist and
    /// all: for a test that drives the browser path with a page of its own.
    /// After [`Self::with_base_url`], which builds the client again without
    /// one.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_page(mut self, page: std::sync::Arc<dyn page::Page>) -> Self {
        self.page = Some(page::allowlisted(page));
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Whether this client sends from a browser page: which of its two arms
    /// a read takes, told to a caller that never chooses it.
    pub fn has_page(&self) -> bool {
        self.page.is_some()
    }

    /// Whether this client can send a write at all.
    ///
    /// A write needs a CSRF token. Sent with `reqwest`, that is the one the
    /// session was stored with, and `snob login --paste` without
    /// `--csrftoken` stores none. Sent from the browser it is the browser's
    /// own, read from its cookie jar at the moment of sending — Instagram sets
    /// one on every page load — so any session can write. One question with
    /// one answer, asked by every place that needs to know.
    pub fn can_write(&self) -> bool {
        self.page.is_some() || self.session.csrftoken.is_some()
    }

    pub fn pacer(&self) -> &Pacer {
        &self.pacer
    }

    /// Whether this client is pointed at Instagram itself.
    ///
    /// What decides whether the pace is real: a walk against the live host pays
    /// every wait between pages, and a walk against a mock server pays none.
    ///
    /// Asked of the base URL rather than set by a caller, and that is the whole
    /// point: "walk Instagram with no waits" is not a thing anybody can ask
    /// for. A test server cannot be Instagram, and Instagram cannot be a test
    /// server, so the question answers itself.
    pub fn is_live(&self) -> bool {
        is_instagram(&self.base)
    }

    /// Waits a step between two pages or entries of one action, the one a
    /// list walk takes ([`Pace::step`]), and answers whether it was canceled.
    ///
    /// Only against Instagram ([`Self::is_live`]), like every wait in the
    /// walker: a mock server is not somebody's account. For what pages by
    /// hand rather than through the walker, so the rule is kept in one place.
    pub async fn step_between(&self, pace: &Pace) -> bool {
        self.is_live() && self.pacer.cancel_token().sleep_or_cancel(pace.step()).await
    }

    /// Waits as a person looks at a profile before opening one of its lists
    /// ([`Pace::dwell`]), and answers whether it was canceled. Only against
    /// Instagram, as [`Self::step_between`]; for what reads a list by hand
    /// rather than through the walker, which waits it itself.
    pub async fn dwell_before_a_list(&self, pace: &Pace) -> bool {
        self.is_live()
            && self
                .pacer
                .cancel_token()
                .sleep_or_cancel(pace.dwell())
                .await
    }

    /// Turns what Instagram said into either the value asked for or an error,
    /// recording a cooldown on the way if the answer earned one.
    ///
    /// Shared by the read and the write path so that "a 200 can still be a
    /// failure" is one rule rather than two; [`refused`] is that rule, and the
    /// browser's listener asks it too.
    pub(in crate::client) fn decode<T: DeserializeOwned>(
        &self,
        answer: &Answer,
    ) -> Result<T, IgError> {
        let body = answer.body.as_str();
        if refused(answer.status, body) {
            return Err(self.refuse(answer));
        }

        serde_json::from_str(body).map_err(|e| {
            // The same excerpt every other error gets: filtered, since it is
            // printed to a terminal, and aware that a body starting with `<` is
            // a captive portal rather than the API, which is what a body that
            // will not parse usually is.
            IgError::Decode(format!(
                "{e} - response: {}",
                crate::error::body_excerpt(body)
            ))
        })
    }

    /// [`IgClient::decode`], for the answer to a query of the registry's
    /// asked of the page, whose `data` is under `root`, the operation's root
    /// field ([`crate::allowlist::Operation::root_field`]).
    ///
    /// **An answer whose `data` lacks `root` does not decode.** Every field
    /// of the models is optional, so such an answer would otherwise read as
    /// an account with no highlight, no story or no profile, and say nothing
    /// of the field Instagram did answer under.
    ///
    /// One that does not decode says in the log what the live check needs to
    /// tell why: the names of the keys under `data`, the content-type it was
    /// served as, and whether it came behind `for (;;);`. Never a value.
    pub(in crate::client) fn decode_query<T: DeserializeOwned>(
        &self,
        answer: &Answer,
        root: &str,
    ) -> Result<T, IgError> {
        self.decode(answer)
            .and_then(|decoded| match under_another_field(&answer.body, root) {
                Some(fields) => Err(IgError::Decode(format!(
                    "the answer's data is under {fields:?}, not {root:?}"
                ))),
                None => Ok(decoded),
            })
            .inspect_err(|error| {
                if matches!(error, IgError::Decode(_)) {
                    note_undecoded(answer);
                }
            })
    }

    /// The one way a push-back leaves the client: classified and recorded,
    /// then noted, since whether the note is a warning depends on the cause.
    /// One function rather than a pair of calls at each site, so that "every
    /// push-back is measured" is checkable by reading it, the write path
    /// included.
    pub(in crate::client) fn refuse(&self, answer: &Answer) -> IgError {
        let error = self.classify_and_record(answer.status, &answer.body);
        self.note_push_back(answer, &error);
        error
    }

    /// Writes down what a push-back looked like, and changes nothing.
    ///
    /// **This is a measurement, not a mechanism.** Reading `Retry-After` would
    /// make snob the only tool of its class that does, and nobody has
    /// established whether these endpoints send it at all; the honest first
    /// step is to log it on every push-back so that a real run answers the
    /// question. Until it has, inventing behavior on the assumption that the
    /// header arrives is guessing with somebody's account.
    ///
    /// **When it is implemented it is a floor and never a ceiling.** A server
    /// naming thirty seconds must not shorten a local cooldown that is longer:
    /// the cooldown lengths here are about how long an account is left alone
    /// after Instagram has objected, which is a different question from how
    /// soon the endpoint will answer again. Written here because this is where
    /// somebody will come looking when they add it.
    ///
    /// **`warn` for a push-back, `debug` for any other refusal.** The user is
    /// told the cause, "throttling", and not which endpoint, what status or
    /// what body, and a push-back met live leaves nothing else to go on. A run
    /// stops at its first push-back, so this is a line or two, not a stream:
    /// the browser's listener writes its own when it hears one first.
    ///
    /// The value is logged as it arrived. A header value cannot carry a byte
    /// below 0x20 — the HTTP parser refuses one before we ever see it — so
    /// there is nothing here that a terminal would act on.
    pub(in crate::client) fn note_push_back(&self, answer: &Answer, error: &IgError) {
        let endpoint = answer.endpoint.as_str();
        let status = answer.status;
        let retry_after = answer.retry_after.as_deref().unwrap_or("<absent>");
        let load = answer.load.as_deref().unwrap_or("<absent>");
        let served = answer.served.as_deref().unwrap_or("<absent>");
        let body = crate::error::body_excerpt(&answer.body);
        if crate::error::cooldown_for(error).is_some() {
            tracing::warn!(endpoint, status, retry_after, load, served, %body, "Instagram pushed back");
        } else {
            tracing::debug!(endpoint, status, retry_after, load, served, %body, "Instagram refused");
        }
    }
}

/// The names of the fields under `data` when `data` is an object without
/// `root`, and `None` otherwise: a `data` that is `null`, or absent, is the
/// model's to read.
fn under_another_field(body: &str, root: &str) -> Option<Vec<String>> {
    let answer: serde_json::Value = serde_json::from_str(body).ok()?;
    let data = answer.get("data")?.as_object()?;
    (!data.contains_key(root)).then(|| data.keys().cloned().collect())
}

/// Says what a query's answer that did not decode looked like: its
/// endpoint, the content-type it was served as (`served`, with `x-stack`),
/// whether it came behind `for (;;);`, and the names of the keys under its
/// `data`, which say which root field Instagram answered with.
fn note_undecoded(answer: &Answer) {
    let endpoint = answer.endpoint.as_str();
    let served = answer.served.as_deref().unwrap_or("<absent>");
    let guarded = answer.body.starts_with("for (;;);");
    let json = answer
        .body
        .strip_prefix("for (;;);")
        .unwrap_or(&answer.body);
    let data: Option<Vec<String>> = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|answer| Some(answer.get("data")?.as_object()?.keys().cloned().collect()));
    tracing::debug!(
        endpoint,
        served,
        guarded,
        ?data,
        "a query's answer did not decode"
    );
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::{answering, client, watching};

    /// Every push-back is written down, with the length its own cause earns.
    ///
    /// `cooldown_for` is the table and it is tested on its own, but a table
    /// nobody reads is not a rule. This drives the four causes through a real
    /// request and asks the budget what arrived.
    #[tokio::test]
    async fn every_push_back_puts_the_account_in_cooldown() {
        use crate::error::cooldown_for;

        let cases = [
            (429, "", IgError::RateLimited),
            (
                400,
                r#"{"message":"","spam":true,"status":"fail"}"#,
                IgError::RateLimited,
            ),
            (
                400,
                r#"{"message":"feedback_required","status":"fail"}"#,
                IgError::FeedbackRequired,
            ),
            (
                400,
                r#"{"message":"challenge_required","status":"fail"}"#,
                IgError::Challenge { url: None },
            ),
        ];

        for (status, body, expected) in cases {
            let server = answering(status, body).await;
            let (client, budget) = watching(&server.uri());
            client.validate().await.unwrap_err();

            let (reason, minimum) = cooldown_for(&expected).expect("this cause earns a cooldown");
            assert_eq!(
                budget.calls(),
                vec![(reason.to_string(), minimum)],
                "status {status} with body {body:?} should record one cooldown"
            );
        }
    }

    /// A dead session is not push-back, and must not put the account in
    /// cooldown: logging in again is what fixes it, and a cooldown would refuse
    /// the very command that fixes it.
    #[tokio::test]
    async fn a_dead_session_records_nothing() {
        let server = answering(403, r#"{"message":"login_required","status":"fail"}"#).await;
        let (client, budget) = watching(&server.uri());

        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::SessionExpired));
        assert!(budget.calls().is_empty());
    }

    /// Only a `data` object that lacks the root field is another field's
    /// answer; a `data` that is `null` or absent is left to the model.
    #[test]
    fn an_answer_is_under_another_field_only_when_data_lacks_the_root() {
        let root = "user";
        assert_eq!(under_another_field(r#"{"data":{"user":null}}"#, root), None);
        assert_eq!(
            under_another_field(r#"{"data":{"user":{},"viewer":{}}}"#, root),
            None
        );
        assert_eq!(under_another_field(r#"{"data":null}"#, root), None);
        assert_eq!(under_another_field(r#"{"errors":[]}"#, root), None);
        assert_eq!(
            under_another_field(r#"{"data":{"xig_user":{"pk":"1"},"viewer":{}}}"#, root),
            Some(vec!["viewer".to_string(), "xig_user".to_string()])
        );
        assert_eq!(
            under_another_field(r#"{"data":{}}"#, root),
            Some(Vec::new())
        );
    }

    /// A 200 whose body will not parse is a `Decode` error with the same
    /// excerpt as every other error: filtered, and named as an HTML page. A
    /// captive portal answering 200 with a login page is the ordinary way to
    /// reach this.
    #[tokio::test]
    async fn a_body_that_will_not_parse_is_excerpted_like_every_other_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "<html><body>{esc}[2K{esc}[A Sign in to the network</body></html>",
                esc = '\x1b'
            )))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .validate()
            .await
            .expect_err("that is not the API's JSON");
        assert!(matches!(error, IgError::Decode(_)), "{error:?}");
        let message = error.to_string();

        assert!(!message.contains('\x1b'), "{message:?}");
        assert!(message.contains("an HTML page"), "{message}");
    }
}
