//! Requests built the way instagram.com's own app builds them.
//!
//! The app sends each call in one of four families, and the family decides
//! the headers and the form, not the path:
//!
//! - **REST**, `GET /api/v1/…`: the app's identifiers, the CSRF token, the
//!   `X-IG-WWW-Claim` cycle and the web session id. Of the app's REST POSTs
//!   snob sends one, `friendships/show_many`, a read ([`SHOW_MANY`]), with
//!   `X-Instagram-AJAX` and a form of its own.
//! - **Relay**, `POST /api/graphql`: a form of 29 fields, the page's values
//!   among them, with the operation's name and `lsd` in the headers. Never
//!   the claim, `X-Requested-With` or the web session id.
//! - **Relay on `/graphql/query`**: the same, plus the answer's root field
//!   and the Bloks version.
//! - **Comet**, `POST /ajax/bulk-route-definitions/` and
//!   `POST /ajax/navigation/` here: route addresses in the app's own
//!   envelope.
//!
//! Every value is the page's: read from the document ([`PageValues`]) or
//! copied from the app's own calls on it ([`AppCalls`]). A value neither has
//! shown is [`Missing`] and the call is not built; nothing is made up.
//!
//! Only the calls of [`crate::allowlist`] are built, named by its values.
//!
//! Only the headers the app's script sets are listed. The browser adds the
//! rest (`Cookie`, `User-Agent`, the client hints, `Origin`, `Sec-Fetch-*`).
//! Where a call carries `X-CSRFToken` or `X-IG-WWW-Claim`, the page writes
//! in the ones it holds as it sends; it adds neither to a call without it,
//! so the family decides which go out.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use snob_core::Pk;

use crate::IG_APP_ID;
use crate::allowlist::{self, Family, Operation, Rediscovered, Rest};
use crate::client::page::{Method, PageRequest, PageResponse};
use crate::client_hints::ASBD_ID;
use crate::graphql::{Mutation, PROFILE_ROUTE, jazoest};
use crate::model::web::RouteAnswer;
use crate::page_values::{PageValues, Viewer};

/// The Comet call that asks for the definitions of routes.
pub const BULK_ROUTE_DEFINITIONS: &str = "/ajax/bulk-route-definitions/";

/// The Comet call the app's router makes when it moves to a route.
pub const NAVIGATION: &str = "/ajax/navigation/";

/// The REST read the app sends after every list page, a POST; why it is a
/// read is said in [`crate::allowlist`], where it is let out.
pub const SHOW_MANY: &str = "/api/v1/friendships/show_many/";

const FORM: &str = "application/x-www-form-urlencoded";

/// What a command may ask the tab for: every one of them a member of
/// [`crate::allowlist`], and none of them a request. The process holding
/// the browser builds the request, from the page's values, right before it
/// is sent ([`build_call`]); the command never sees a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ask {
    /// Who the tab's document was served to. Nothing is sent.
    Viewer,
    /// The reels of the home document's stories tray, in the order it shows
    /// them. Nothing is sent.
    Tray,
    /// The pk behind a name: the route definitions, or the profile document
    /// when the app has made no route call to copy.
    Pk { name: String },
    /// The tab loads `path`, the home page or a profile. The answer carries
    /// the document's bundles, never its HTML: the first half of a write,
    /// which is made from the profile it loads, and finds its `doc_id` in
    /// those bundles when the one it has is refused. A document served to
    /// nobody, or to another account, is logged out.
    Document { path: String },
    /// A read of the registry's. A write here is refused.
    Query {
        operation: Operation,
        variables: String,
    },
    /// A REST read of the registry's, with the query the app sends.
    Rest {
        read: Rest,
        query: Vec<(String, String)>,
    },
    /// The app's router moving to `route`, a [`crate::allowlist::list_route`]:
    /// what it sends as a profile opens, and as a profile's mutual followers
    /// open. The tab itself does not move.
    Navigation { route: String },
    /// How the viewer stands with `pks`, the accounts a list page just
    /// listed: `friendships/show_many`, which the app sends after every list
    /// page. A read.
    Statuses { pks: Vec<Pk> },
    /// One of the two writes, under the `doc_id` found for it, on the route
    /// it is made from: what `follow` and `unfollow` ask of the page, right
    /// after the [`Ask::Document`] of the profile.
    Write {
        mutation: Mutation,
        variables: String,
        doc_id: String,
        route: String,
    },
    /// A file from the CDN, which the tab fetches as the app's own page
    /// would: the browser's TLS and HTTP/2, its headers and its cookie rules
    /// for that host. The answer carries the bytes as base64 ([`Told::Asset`]).
    /// Never a video: the tab lets none reach it.
    Asset { url: String },
}

impl Ask {
    /// The path the ask is answered from, where its answer is read as
    /// having come from.
    pub fn endpoint(&self) -> String {
        match self {
            Self::Viewer | Self::Tray => "/".into(),
            Self::Pk { .. } => BULK_ROUTE_DEFINITIONS.into(),
            Self::Document { path } => path.clone(),
            Self::Query { operation, .. } => operation.path().into(),
            Self::Rest { read, .. } => read.path(),
            Self::Navigation { .. } => NAVIGATION.into(),
            Self::Statuses { .. } => SHOW_MANY.into(),
            Self::Write { mutation, .. } => Operation::of_write(*mutation).path().into(),
            Self::Asset { url } => url.clone(),
        }
    }
}

/// An [`Ask`], with what the request built for it takes from the client
/// that asks: where it goes, the page it is made from, the claim, and the
/// ceilings on the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub ask: Ask,
    /// The site's address, such as `https://www.instagram.com`.
    pub origin: String,
    /// The page the call is made from, as a path: `/<user>/`.
    pub referrer: String,
    /// The current `X-IG-WWW-Claim`, for a REST read.
    pub claim: String,
    /// The most bytes of body worth reading.
    pub cap: u64,
    /// How long the page may take before giving up.
    pub timeout_ms: u64,
}

/// What the tab answered an [`Ask`] with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Told {
    /// The answer to a request built and sent.
    Answer(PageResponse),
    /// Who the document was served to: no token.
    Viewer(Viewer),
    /// The tray's reels, or `None` when the document holds no tray.
    Tray(Option<Vec<String>>),
    /// The pk behind a name. A document's answer comes without its body.
    Pk {
        answer: PageResponse,
        pk: RouteAnswer,
    },
    /// The document loaded, without its body, and the bundles it names.
    Document {
        answer: PageResponse,
        bundles: Vec<String>,
    },
    /// A file from the CDN. **The body is base64**, as the bytes cannot travel
    /// as text; a file past the cap is `too_large`, with no body.
    Asset(PageResponse),
}

/// Why no request was built for a [`Call`]. Nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unbuilt {
    /// A value the page has not shown.
    #[error(transparent)]
    Missing(Missing),
    /// The tab's document is signed out, or served to another account.
    #[error("the page is not logged in as this account")]
    LoggedOut,
    /// Not one of the calls snob may send. Carries the method and the path.
    #[error("{0}")]
    NotAllowed(String),
}

/// The request `call` asks for, built from the tab's current document: its
/// values (`page`), the app's calls on it (`app`), and what the app last sent
/// with the operation asked (`seen`, from any document). `account` is the pk
/// of the session the call is sent as.
///
/// The one pure step between an intent and the wire, in this order, and
/// the first that fails decides:
///
/// 0. the intent is checked again ([`allowlist::refused_ask`]), whoever
///    sent it;
/// 1. a signed-out document, and
/// 2. one whose viewer was not read, send nothing;
/// 3. a document served to another account is logged out as far as this
///    session goes;
/// 4. what the tab answers itself is never built;
/// 5. the call is built ([`Context::build`]);
/// 6. made a request on `call.origin`;
/// 7. which is checked as every request from the page is
///    ([`allowlist::refused_with`]), a `doc_id` the app sent standing in for
///    the registry's only when [`Seen::doc_id`] names it.
pub fn build_call(
    call: &Call,
    page: &PageValues,
    app: &mut AppCalls,
    account: Pk,
    seen: &Seen<'_>,
) -> Result<PageRequest, Unbuilt> {
    if let Some(what) = allowlist::refused_ask(&call.ask) {
        return Err(Unbuilt::NotAllowed(what));
    }
    viewer_of(page, account)?;
    if matches!(
        call.ask,
        Ask::Viewer | Ask::Tray | Ask::Document { .. } | Ask::Asset { .. }
    ) {
        return Err(Unbuilt::NotAllowed(ANSWERED_BY_THE_TAB.into()));
    }
    let mut context = Context {
        page,
        app,
        csrf: None,
        claim: &call.claim,
    };
    let request = context
        .build(&call.ask, &call.referrer, seen)?
        .into_request(&call.origin, call.cap, call.timeout_ms);
    let found = match (&call.ask, seen.doc_id) {
        (Ask::Query { operation, .. }, Some(doc_id)) => Rediscovered::of(*operation, doc_id),
        _ => Rediscovered::default(),
    };
    match allowlist::refused_with(&request, &found) {
        Some(what) => Err(Unbuilt::NotAllowed(what)),
        None => Ok(request),
    }
}

/// `call`, for a read the app sends about an account it is not showing, made
/// from `path`, the page the tab is on: the app prefetches a profile, a hover
/// card and a story tray from whichever page it is on, so that page's address
/// is the `Referer` and its route the `__crn` the call carries. A page that
/// is neither the home page nor a profile leaves the call as it was asked,
/// and so do the reads that belong to a profile of their own (a list, the
/// highlights viewer) and everything that is not a read of the registry's, a
/// write above all, which is made on the document it loaded.
pub fn made_from(call: &Call, path: &str) -> Call {
    let prefetched = matches!(
        &call.ask,
        Ask::Query {
            operation: Operation::ProfilePage
                | Operation::HoverCard
                | Operation::HighlightsTray
                | Operation::NoteBubble
                | Operation::SchoolPartnerBadge
                | Operation::ReelGallery,
            ..
        }
    );
    if !prefetched || path == call.referrer || !allowlist::document(path) {
        return call.clone();
    }
    Call {
        referrer: path.to_string(),
        ..call.clone()
    }
}

/// What the app sent, on any document, with the operation a call asks for:
/// copied into the call rather than invented.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seen<'a> {
    /// The latest `variables` the app sent with it, whose flags a call
    /// copies: only the keys that name what is asked are made snob's
    /// ([`Operation::asked_keys`]).
    pub variables: Option<&'a str>,
    /// The `doc_id` the app sent it under when it is not the registry's,
    /// which [`allowlist::Rediscovered`] vouched for.
    pub doc_id: Option<&'a str>,
    /// The token the app's own REST POSTs carry ([`AppCalls::rest_token_sent`]),
    /// from any document: what a [`Ask::Statuses`] is sent with.
    pub rest_token: Option<&'a str>,
}

/// Who the tab's document was served to, when that is `account`: steps 1
/// to 3 of [`build_call`], and what the tab checks before it answers from
/// the document itself. A signed-out document, and one served to another
/// account, are logged out as far as this session goes; one whose viewer
/// was not read is not ready.
pub fn viewer_of(page: &PageValues, account: Pk) -> Result<&Viewer, Unbuilt> {
    if page.signed_out {
        return Err(Unbuilt::LoggedOut);
    }
    let viewer = page
        .viewer
        .as_ref()
        .ok_or(Unbuilt::Missing(Missing("the viewer")))?;
    if viewer.pk != account {
        return Err(Unbuilt::LoggedOut);
    }
    Ok(viewer)
}

/// Why an ask that sends nothing is not built.
const ANSWERED_BY_THE_TAB: &str = "answered by the tab, not sent";

/// A value the call needs that the page has not shown yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the page has not shown {0} yet")]
pub struct Missing(pub &'static str);

impl Missing {
    pub const RELAY_CALL: Self = Self("a Relay call of the app's");
    pub const ROUTE_CALL: Self = Self("a route call of the app's");
    pub const WEB_SESSION: Self = Self("the web session id");
    pub const RELAY_TOKEN_SENT: Self = Self("a Relay token the app sent");
    /// Not waited for: the app sends a REST POST with a token only where a
    /// person does something (opens the activity, scrolls a list), which on
    /// the documents snob loads may never come.
    pub const REST_TOKEN_SENT: Self = Self("a REST token the app sent");

    /// Whether only the app's own calls on the document can fill the gap,
    /// which they may still do a moment later. A value of the document's
    /// own is read when the document is, and is there or never will be.
    pub fn waits_on_the_app(&self) -> bool {
        [
            Self::RELAY_CALL,
            Self::ROUTE_CALL,
            Self::WEB_SESSION,
            Self::RELAY_TOKEN_SENT,
        ]
        .contains(self)
    }
}

/// One request, as the app would send it.
#[derive(Clone, PartialEq, Eq)]
pub struct Built {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Only the ones the app's script sets, in its order.
    pub headers: Vec<(String, String)>,
    /// The page the call is made from, as a path: `/<user>/`.
    pub referrer: String,
    /// The form of a POST, in the app's order. Empty is an empty body.
    pub form: Option<Vec<(String, String)>>,
}

impl Built {
    /// A header, by name in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// A field of the form.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.form
            .as_ref()?
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The path with its query.
    pub fn target(&self) -> String {
        if self.query.is_empty() {
            return self.path.clone();
        }
        format!("{}?{}", self.path, encoded(&self.query))
    }

    /// The form, encoded.
    pub fn body(&self) -> Option<String> {
        self.form.as_deref().map(encoded)
    }

    /// The request the page sends for it, on `origin`: the site's address,
    /// such as `https://www.instagram.com`.
    pub fn into_request(self, origin: &str, cap: u64, timeout_ms: u64) -> PageRequest {
        let origin = origin.trim_end_matches('/');
        PageRequest {
            method: self.method,
            url: format!("{origin}{}", self.target()),
            body: self.body(),
            headers: self.headers,
            referrer: format!("{origin}{}", self.referrer),
            navigate: false,
            cap,
            timeout_ms,
        }
    }
}

/// Names only: the form and the headers carry tokens.
impl fmt::Debug for Built {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn names(pairs: &[(String, String)]) -> Vec<&str> {
            pairs.iter().map(|(n, _)| n.as_str()).collect()
        }
        f.debug_struct("Built")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers", &names(&self.headers))
            .field("form", &self.form.as_deref().map(names))
            .finish_non_exhaustive()
    }
}

/// What the app's own calls on one document carried that the document does
/// not: the module bitmaps and the rest of the Relay form, the Relay token it
/// sent, the envelope of its route calls, its request counter, the variables
/// and `doc_id`s of the registry's operations it sent, and the identifiers it
/// puts in its headers.
///
/// **The counter is the app's, and the app does not see snob's.** A built
/// call takes the number after the highest the app has sent on this
/// document, and the app's next call sends that same number again: one
/// document then carries a `__req` twice, which the app alone never does.
/// Whether that draws anything is for the live check to say.
///
/// Made new with each document, as the app's own state is.
#[derive(Debug, Clone, Default)]
pub struct AppCalls {
    relay: Option<RelayCopy>,
    /// The `fb_dtsg` of the app's latest Relay or Comet form.
    dtsg: Option<String>,
    /// The `fb_dtsg` of the app's latest REST POST that carried one whose
    /// `jazoest` it matches ([`Self::rest_token_sent`]).
    rest_token: Option<String>,
    comet: Option<Vec<(String, Option<String>)>>,
    /// The next `__req` snob takes: one past the highest the app has sent,
    /// or snob has taken.
    next_req: u32,
    /// The first `__req` snob took on this document, to tell when the app
    /// repeats one.
    first_taken: Option<u32>,
    /// Whether the app has sent a call on this document that names the route
    /// it was made on (`__crn`). Its queries right after a cold document
    /// name none; every one after does.
    route_named: bool,
    /// The latest `variables` the app sent with each of the registry's
    /// operations: its own flags, with their names and values as it sends
    /// them, for a call of the same operation to copy rather than invent.
    variables: HashMap<Operation, String>,
    /// The `doc_id` the app sent each of the registry's reads under, when
    /// [`allowlist::vouched`] holds for the call.
    doc_ids: HashMap<Operation, String>,
    /// `X-IG-App-ID` and `X-ASBD-ID`, as the app's own calls carry them.
    app_id: Option<String>,
    asbd_id: Option<String>,
}

/// The fields of the app's latest Relay form that describe the page's
/// state rather than the call: `dpr`, `__ccg`, and the bitmaps of the
/// modules loaded, which only the app can know.
#[derive(Debug, Clone)]
struct RelayCopy {
    dpr: String,
    ccg: String,
    bitmaps: [(&'static str, String); 5],
}

/// The fields of a Comet envelope the page fills in for each call: the
/// counter and the per-load tokens.
const FILLED: [&str; 4] = ["__req", "fb_dtsg", "jazoest", "lsd"];

impl AppCalls {
    /// Takes in one of the app's calls: the form of a POST, or the query of
    /// a GET, sent to `path`. The registry's operation whose variables it
    /// kept, if it was one.
    pub fn saw(&mut self, path: &str, form: &str) -> Option<Operation> {
        self.saw_with(path, form, &[])
    }

    /// [`Self::saw`], with the headers the call went out under: the
    /// identifiers the app puts in them are kept, and so is the number a read
    /// of the registry's went out under, when the call vouches for it.
    pub fn saw_with(
        &mut self,
        path: &str,
        form: &str,
        headers: &[(String, String)],
    ) -> Option<Operation> {
        let fields: Vec<(String, String)> = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect();
        let field = |name: &str| fields.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        let header = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        if let Some(req) = field("__req").and_then(|r| u32::from_str_radix(r, 36).ok()) {
            if self
                .first_taken
                .is_some_and(|first| (first..self.next_req).contains(&req))
            {
                tracing::debug!(req = %base36(req), "the app sent a __req snob had sent on this document");
            }
            self.next_req = self.next_req.max(req.saturating_add(1));
        }
        if field("__crn").is_some_and(|route| !route.is_empty()) {
            self.route_named = true;
        }
        if path.starts_with("/api/") || path == "/graphql/query" {
            let identifier = |name: &str| {
                header(name)
                    .filter(|id| !id.is_empty() && id.len() <= 20)
                    .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
                    .map(str::to_string)
            };
            if let Some(id) = identifier("x-ig-app-id") {
                self.app_id = Some(id);
            }
            if let Some(id) = identifier("x-asbd-id") {
                self.asbd_id = Some(id);
            }
        }
        let carries_the_relay_token =
            matches!(path, "/api/graphql" | "/graphql/query") || path.starts_with("/ajax/");
        if carries_the_relay_token && let Some(dtsg) = field("fb_dtsg") {
            self.dtsg = Some(dtsg.clone());
        }
        if path.starts_with("/api/v1/")
            && let (Some(dtsg), Some(sent)) = (field("fb_dtsg"), field("jazoest"))
            && !dtsg.is_empty()
            && jazoest(dtsg) == *sent
        {
            self.rest_token = Some(dtsg.clone());
        }
        match path {
            "/api/graphql" | "/graphql/query" => {
                let copied = (|| {
                    let bitmap = |name: &'static str| Some((name, field(name)?.clone()));
                    Some(RelayCopy {
                        dpr: field("dpr")?.clone(),
                        ccg: field("__ccg")?.clone(),
                        bitmaps: [
                            bitmap("__dyn")?,
                            bitmap("__csr")?,
                            bitmap("__hsdp")?,
                            bitmap("__hblp")?,
                            bitmap("__sjsp")?,
                        ],
                    })
                })();
                if copied.is_some() {
                    self.relay = copied;
                }
                let operation = field("fb_api_req_friendly_name").and_then(|n| Operation::named(n));
                if let (Some(operation), Some(variables)) = (operation, field("variables")) {
                    self.variables.insert(operation, variables.clone());
                    if let Some(doc_id) = field("doc_id")
                        && allowlist::vouched(
                            operation,
                            path,
                            doc_id,
                            variables,
                            header("x-root-field-name"),
                        )
                    {
                        self.doc_ids.insert(operation, doc_id.clone());
                    }
                    return Some(operation);
                }
            }
            BULK_ROUTE_DEFINITIONS if field("routing_namespace").is_some() => {
                self.comet = Some(
                    fields
                        .into_iter()
                        .filter(|(name, _)| !name.starts_with("route_urls["))
                        .map(|(name, value)| {
                            let value = (!FILLED.contains(&name.as_str())).then_some(value);
                            (name, value)
                        })
                        .collect(),
                );
            }
            _ => {}
        }
        None
    }

    /// The latest `variables` the app sent with `operation` on this document.
    pub fn variables_sent(&self, operation: Operation) -> Option<&str> {
        self.variables.get(&operation).map(String::as_str)
    }

    /// The `doc_id` the app sent the registry's read `operation` under on
    /// this document, when the call vouched for it ([`allowlist::vouched`]).
    pub fn doc_id_sent(&self, operation: Operation) -> Option<&str> {
        self.doc_ids.get(&operation).map(String::as_str)
    }

    /// The token the app's REST POSTs on this document carried, when one
    /// did, with the `jazoest` it makes.
    ///
    /// **Not the page's Relay token, and not known to be any value the
    /// document shows.** In the capture of 2026-10-01 every REST POST of the
    /// app (`show_many`, `news/inbox`, `discover/ayml`) carried one `jazoest`,
    /// 22858, across three document loads, while the Relay and Comet calls'
    /// changed with each load (26068, 26433, 26511) and matched the token the
    /// document wrote into its `/ajax/qm/` call. The REST token is longer
    /// lived, and its source in the document is one of the two the recorder
    /// redacts (`DTSGInitData`'s token, or `MRequestConfig`'s `dtsg`, which
    /// says it is valid for a day); which one cannot be told from a redacted
    /// recording. So it is taken only from the app's own REST POSTs, never
    /// guessed from the page, and a call that needs it is not built until the
    /// app has sent one.
    pub fn rest_token_sent(&self) -> Option<&str> {
        self.rest_token.as_deref()
    }

    /// `X-IG-App-ID`, as the app's calls on this document carry it.
    pub fn app_id(&self) -> Option<&str> {
        self.app_id.as_deref()
    }

    /// `X-ASBD-ID`, as the app's calls on this document carry it.
    pub fn asbd_id(&self) -> Option<&str> {
        self.asbd_id.as_deref()
    }

    /// Whether the app has made a route call on this document, whose
    /// envelope a route call built here would copy.
    pub fn made_a_route_call(&self) -> bool {
        self.comet.is_some()
    }

    /// Which of `page`'s two tokens the app's Relay and Comet calls on this
    /// document carry: what the live check reads to settle which one a call
    /// built here takes. Never the token itself.
    pub fn sent_token(&self, page: &PageValues) -> SentToken {
        let Some(sent) = self.dtsg.as_deref() else {
            return SentToken::NotYet;
        };
        let is = |token: &Option<snob_core::secret::Secret>| {
            token.as_ref().is_some_and(|token| token.expose() == sent)
        };
        if is(&page.relay_dtsg) {
            SentToken::Relay
        } else if is(&page.session_dtsg) {
            SentToken::Session
        } else {
            SentToken::Neither
        }
    }

    /// The `__req` of a call about to be built.
    fn next_req(&mut self) -> String {
        let req = self.next_req.max(1);
        self.first_taken.get_or_insert(req);
        self.next_req = req.saturating_add(1);
        base36(req)
    }
}

/// Which of a document's two tokens the app's Relay and Comet calls carry
/// ([`AppCalls::sent_token`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentToken {
    /// `DTSGInitialData`'s, the one a call built here takes.
    Relay,
    /// `DTSGInitData`'s.
    Session,
    /// A token the document does not carry.
    Neither,
    /// The app has sent no Relay or Comet call on the document yet.
    NotYet,
}

/// What a call is built from, besides the call itself.
pub struct Context<'a> {
    pub page: &'a PageValues,
    pub app: &'a mut AppCalls,
    /// The CSRF token the browser holds, when known. REST and Relay calls
    /// carry the header either way, empty without one: the page writes its
    /// cookie's in as it sends.
    pub csrf: Option<&'a str>,
    /// The current `X-IG-WWW-Claim`: `0` until the server hands one out.
    pub claim: &'a str,
}

impl<'a> Context<'a> {
    /// The call `ask` names, made from `referrer`, a path.
    ///
    /// A [`Ask::Query`] of a write is refused here too, whatever checked the
    /// intent before: a write goes out only as [`Ask::Write`]. A query takes
    /// the variables the app sent with the same operation (`seen`), with only
    /// the keys that name what is asked made snob's, so its flags are the
    /// app's rather than invented; without them, the ask's own. It is made on
    /// the route of the page it is made from ([`route_of`]).
    pub fn build(&mut self, ask: &Ask, referrer: &str, seen: &Seen<'_>) -> Result<Built, Unbuilt> {
        let built = match ask {
            Ask::Query {
                operation,
                variables,
            } => {
                if operation.write().is_some() {
                    return Err(Unbuilt::NotAllowed(format!("POST {}", operation.path())));
                }
                let variables = match seen.variables {
                    Some(seen) => with_the_keys_of(seen, variables, operation.asked_keys())
                        .unwrap_or_else(|| variables.clone()),
                    None => variables.clone(),
                };
                let route = self.route_on(referrer);
                self.relay(*operation, &variables, route, referrer, seen.doc_id)
            }
            Ask::Rest { read, query } => {
                let query: Vec<(&str, &str)> = query
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect();
                Ok(self.rest_get(*read, &query, referrer))
            }
            Ask::Write {
                mutation,
                variables,
                doc_id,
                route,
            } => self.relay(
                Operation::of_write(*mutation),
                variables,
                Some(route),
                referrer,
                Some(doc_id),
            ),
            Ask::Pk { name } => self.bulk_route_definitions(&[&format!("/{name}/")], referrer),
            Ask::Navigation { route } => self.navigation(route),
            Ask::Statuses { pks } => self.show_many(pks, referrer, seen.rest_token),
            Ask::Viewer | Ask::Tray | Ask::Document { .. } | Ask::Asset { .. } => {
                return Err(Unbuilt::NotAllowed(ANSWERED_BY_THE_TAB.into()));
            }
        };
        built.map_err(Unbuilt::Missing)
    }

    /// The route a Relay read made from `referrer` is made on, as the app
    /// names it with `__crn`. **None for the first call built on a document
    /// whose app has named none**: the queries the app sends as a cold
    /// document boots go out without one, and the ones after carry the route.
    fn route_on(&self, referrer: &str) -> Option<&'static str> {
        if !self.app.route_named && self.app.first_taken.is_none() {
            return None;
        }
        route_of(referrer)
    }

    /// `X-IG-App-ID`, the app's own where its calls carried one.
    fn app_id(&self) -> &str {
        self.app.app_id().unwrap_or(IG_APP_ID)
    }

    /// `X-ASBD-ID`, the app's own where its calls carried one.
    fn asbd_id(&self) -> &str {
        self.app.asbd_id().unwrap_or(ASBD_ID)
    }

    /// A REST read.
    pub fn rest_get(&self, read: Rest, query: &[(&str, &str)], referrer: &str) -> Built {
        Built {
            method: Method::Get,
            path: read.path(),
            query: owned(query),
            headers: self.rest_headers(),
            referrer: referrer.into(),
            form: None,
        }
    }

    fn rest_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("X-IG-App-ID", self.app_id()),
            ("X-ASBD-ID", self.asbd_id()),
            ("X-CSRFToken", self.csrf()),
            ("X-IG-WWW-Claim", self.claim),
            ("X-Requested-With", "XMLHttpRequest"),
        ];
        // The app sends it once it has made one for the page, which is not
        // on its very first calls.
        if let Some(session) = self.page.web_session.as_deref() {
            headers.push(("X-Web-Session-ID", session));
        }
        headers.push(("X-IG-Max-Touch-Points", "0"));
        owned(&headers)
    }

    fn csrf(&self) -> &'a str {
        self.csrf.unwrap_or_default()
    }

    /// The per-load token a Relay or Comet POST carries: the page's, and
    /// only when the app itself sent it on this document, since which of the
    /// document's two tokens that is has not been confirmed.
    fn relay_token(&self) -> Result<&'a str, Missing> {
        let token = self
            .page
            .relay_dtsg
            .as_ref()
            .ok_or(Missing("the Relay token"))?
            .expose();
        match &self.app.dtsg {
            Some(sent) if sent == token => Ok(token),
            _ => Err(Missing::RELAY_TOKEN_SENT),
        }
    }

    /// A Relay call of `operation`, which takes `variables` as JSON. `route`
    /// is `__crn`, the route the call is made on; the app leaves it out of
    /// the queries it sends right after a document loads. `doc_id` stands in
    /// for the registry's, for a number the app was seen to send (a read) or
    /// that was rediscovered (a write).
    pub fn relay(
        &mut self,
        operation: Operation,
        variables: &str,
        route: Option<&str>,
        referrer: &str,
        doc_id: Option<&str>,
    ) -> Result<Built, Missing> {
        let page = self.page;
        let viewer = page.viewer.as_ref().ok_or(Missing("the viewer"))?;
        let site = page.site.as_ref().ok_or(Missing("SiteData"))?;
        let lsd = page.lsd.as_ref().ok_or(Missing("LSD"))?.expose();
        page.relay_dtsg.as_ref().ok_or(Missing("the Relay token"))?;
        let session = page.web_session.as_deref().ok_or(Missing::WEB_SESSION)?;
        let copy = self.app.relay.clone().ok_or(Missing::RELAY_CALL)?;
        let dtsg = self.relay_token()?;
        let xdt = match operation.family() {
            Family::RelayQuery => {
                let bloks = page
                    .bloks_version
                    .as_deref()
                    .ok_or(Missing("the Bloks version"))?;
                Some((operation.root_field(), bloks))
            }
            _ => None,
        };
        let req = self.app.next_req();

        let mut form = owned(&[
            ("av", viewer.fbid.as_str()),
            ("__d", "www"),
            ("__user", "0"),
            ("__a", "1"),
            ("__req", req.as_str()),
            ("__hs", site.haste_session.as_str()),
            ("dpr", copy.dpr.as_str()),
            ("__ccg", copy.ccg.as_str()),
            ("__rev", site.revision.as_str()),
            ("__s", session),
            ("__hsi", site.hsi.as_str()),
        ]);
        form.extend(copy.bitmaps.map(|(name, value)| (name.to_string(), value)));
        form.extend(owned(&[
            ("__comet_req", "7"),
            ("fb_dtsg", dtsg),
            ("jazoest", jazoest(dtsg).as_str()),
            ("lsd", lsd),
            ("__spin_r", site.spin_r.as_str()),
            ("__spin_b", site.spin_b.as_str()),
            ("__spin_t", site.spin_t.as_str()),
        ]));
        if let Some(route) = route {
            form.push(("__crn".into(), route.into()));
        }
        // The operation's five, in the order the app sends them in every
        // Relay POST of both captures (2026-09-29 and 2026-10-01): the
        // timestamps flag before the variables.
        form.extend(owned(&[
            ("fb_api_caller_class", "RelayModern"),
            ("fb_api_req_friendly_name", operation.friendly_name()),
            ("server_timestamps", "true"),
            ("variables", variables),
            ("doc_id", doc_id.unwrap_or(operation.doc_id())),
        ]));

        let mut headers = vec![
            ("X-FB-Friendly-Name", operation.friendly_name()),
            ("X-FB-LSD", lsd),
            ("X-CSRFToken", self.csrf()),
            ("X-IG-App-ID", self.app_id()),
            ("X-ASBD-ID", self.asbd_id()),
            ("X-IG-Max-Touch-Points", "0"),
            ("Content-Type", FORM),
        ];
        if let Some((root_field, bloks)) = xdt {
            headers.extend([
                ("X-Root-Field-Name", root_field),
                ("X-Bloks-Version-Id", bloks),
            ]);
        }

        Ok(Built {
            method: Method::Post,
            path: operation.path().into(),
            query: Vec::new(),
            headers: owned(&headers),
            referrer: referrer.into(),
            form: Some(form),
        })
    }

    /// The route definitions of `routes`, each a path on the site such as
    /// `/<user>/`: what the app asks before it links to a page, and where
    /// the ids the page will need are.
    ///
    /// The envelope is the app's latest one on this document, with the
    /// counter and the tokens filled in from the page.
    pub fn bulk_route_definitions(
        &mut self,
        routes: &[&str],
        referrer: &str,
    ) -> Result<Built, Missing> {
        let leading = routes
            .iter()
            .enumerate()
            .map(|(i, route)| (format!("route_urls[{i}]"), route.to_string()))
            .collect();
        self.comet(BULK_ROUTE_DEFINITIONS, leading, None, referrer)
    }

    /// The app's router moving to `route`, a path such as `/<user>/` or
    /// `/<user>/followers/mutualOnly`: what it sends as a profile opens, and
    /// as the mutual-followers tab of one opens (the capture of 2026-10-01
    /// holds one before every list that was read).
    ///
    /// The form is the route definitions' envelope after the viewer's
    /// `fbid` (`client_previous_actor_id`) and the route, with `__crn` the
    /// route moved to, as the app's navigations carry it: a profile's posts
    /// tab for a profile and for its mutual followers alike, the feed's for
    /// the home page. Made from the route itself, which is the page's address
    /// once the app has pushed it, so the `Referer` is the route.
    pub fn navigation(&mut self, route: &str) -> Result<Built, Missing> {
        let viewer = self.page.viewer.as_ref().ok_or(Missing("the viewer"))?;
        let leading = owned(&[
            ("client_previous_actor_id", viewer.fbid.as_str()),
            ("route_url", route),
        ]);
        let profile = route.strip_suffix("followers/mutualOnly").unwrap_or(route);
        self.comet(NAVIGATION, leading, route_of(profile), route)
    }

    /// A Comet call to `path`: `leading` and then the app's envelope, with
    /// the counter and the tokens filled in from the page and `__crn` made
    /// `crn` where one is given.
    fn comet(
        &mut self,
        path: &str,
        leading: Vec<(String, String)>,
        crn: Option<&str>,
        referrer: &str,
    ) -> Result<Built, Missing> {
        let envelope = self.app.comet.clone().ok_or(Missing::ROUTE_CALL)?;
        let lsd = self.page.lsd.as_ref().ok_or(Missing("LSD"))?.expose();
        let dtsg = self.relay_token()?;
        let req = self.app.next_req();
        // The app sends the flows its envelope names in a header as well.
        let flows = envelope
            .iter()
            .find(|(name, _)| name == "qpl_active_flow_ids")
            .and_then(|(_, value)| value.clone())
            .filter(|flows| !flows.is_empty());
        let mut form = leading;
        form.extend(envelope.into_iter().map(|(name, value)| {
            let value = match (name.as_str(), value) {
                ("__crn", Some(theirs)) => crn.map_or(theirs, str::to_string),
                (_, Some(value)) => value,
                ("__req", None) => req.clone(),
                ("fb_dtsg", None) => dtsg.into(),
                ("jazoest", None) => jazoest(dtsg),
                // `lsd`, the last of `FILLED`.
                (_, None) => lsd.into(),
            };
            (name, value)
        }));

        let mut headers = vec![("X-FB-LSD", lsd)];
        if let Some(flows) = flows.as_deref() {
            headers.push(("X-FB-QPL-Active-Flows", flows));
        }
        headers.extend([
            ("X-IG-D", "www"),
            ("X-ASBD-ID", self.asbd_id()),
            ("X-IG-Max-Touch-Points", "0"),
            ("Content-Type", FORM),
        ]);

        Ok(Built {
            method: Method::Post,
            path: path.into(),
            query: Vec::new(),
            headers: owned(&headers),
            referrer: referrer.into(),
            form: Some(form),
        })
    }

    /// `friendships/show_many` for `pks`, the accounts of the list page just
    /// read, made from `referrer`, the page the list was read from: what the
    /// app sends the moment each list page answers, and before it asks for
    /// the next (every one of the 16 list pages of the capture of
    /// 2026-10-01, 0.2 to 0.3 seconds after).
    ///
    /// The REST family's headers and `X-Instagram-AJAX`, the revision the
    /// app's REST POSTs carry (`SiteData`'s), with a form of three fields in
    /// the app's order: the pks, comma-separated, then `jazoest` and
    /// `fb_dtsg`. The token is `token`, the one the app's own REST POSTs were
    /// seen to carry ([`AppCalls::rest_token_sent`]); without one the call is
    /// not built, since no token the document shows is known to be it.
    pub fn show_many(
        &mut self,
        pks: &[Pk],
        referrer: &str,
        token: Option<&str>,
    ) -> Result<Built, Missing> {
        let site = self.page.site.as_ref().ok_or(Missing("SiteData"))?;
        let token = token.ok_or(Missing::REST_TOKEN_SENT)?;
        let ids = pks
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut headers = self.rest_headers();
        headers.extend(owned(&[
            ("X-Instagram-AJAX", site.revision.as_str()),
            ("Content-Type", FORM),
        ]));
        Ok(Built {
            method: Method::Post,
            path: SHOW_MANY.into(),
            query: Vec::new(),
            headers,
            referrer: referrer.into(),
            form: Some(owned(&[
                ("user_ids", ids.as_str()),
                ("jazoest", jazoest(token).as_str()),
                ("fb_dtsg", token),
            ])),
        })
    }
}

/// The route the app names for a page, in `__crn`: its home page is the
/// feed's, and a profile is the profile's posts tab. Another page has none
/// here, as snob is made from no other.
pub fn route_of(referrer: &str) -> Option<&'static str> {
    if referrer == "/" {
        Some(FEED_ROUTE)
    } else if allowlist::document(referrer) {
        Some(PROFILE_ROUTE)
    } else {
        None
    }
}

/// The route of the home page.
pub const FEED_ROUTE: &str = "comet.igweb.PolarisFeedRoute";

/// The variables of the app's profile query for `pk`, as the capture shows it
/// sending them: the integrity filter, the id, and five flags of its own, in
/// its order. What a call sends before the app has sent a query to copy
/// them from; [`Seen::variables`] replaces the flags with the app's own.
pub fn profile_variables(pk: Pk) -> String {
    format!(
        concat!(
            r#"{{"enable_integrity_filters":true,"id":"{}","#,
            r#""__relay_internal__pv__PolarisCannesGuardianExperienceEnabledrelayprovider":true,"#,
            r#""__relay_internal__pv__PolarisCASB976ProfileEnabledrelayprovider":false,"#,
            r#""__relay_internal__pv__PolarisWebSchoolsEnabledrelayprovider":false,"#,
            r#""__relay_internal__pv__PolarisRepostsConsumptionEnabledrelayprovider":true,"#,
            r#""__relay_internal__pv__PolarisShortDramaEnabledrelayprovider":false}}"#
        ),
        pk
    )
}

/// The variables of the app's story and highlight viewers, which open `reel`
/// over `reel_ids` with a window of three after and two before, and a flag
/// of their own.
pub fn reel_variables(reel: &str, reel_ids: &[String]) -> String {
    format!(
        concat!(
            r#"{{"initial_reel_id":{},"reel_ids":{},"first":3,"last":2,"#,
            r#""__relay_internal__pv__PolarisCommunityNoteStoriesLabelEnabledrelayprovider":true}}"#
        ),
        serde_json::Value::from(reel),
        serde_json::Value::from(reel_ids.to_vec()),
    )
}

/// The flags the app sends with both of the grid's queries, under `data`:
/// twelve posts a page, and what to say about the owner's stories beside
/// them (captures of 2026-10-01, on every profile opened).
const POSTS_DATA: &str = concat!(
    r#"{"count":12,"include_reel_media_seen_timestamp":true,"#,
    r#""include_relationship_info":true,"latest_besties_reel_media":true,"#,
    r#""latest_reel_media":true}"#
);

/// The three flags of the app's own that close both of the grid's queries.
const POSTS_FLAGS: &str = concat!(
    r#""__relay_internal__pv__PolarisMultiCaptionCarouselEnabledrelayprovider":true,"#,
    r#""__relay_internal__pv__PolarisShortDramaEnabledrelayprovider":false,"#,
    r#""__relay_internal__pv__PolarisReelsRecoDebugOverlayEnabledrelayprovider":false"#
);

/// The variables of the grid's first page for `username`, as the app sends
/// them as a profile opens: the page's flags, the name, and its own three.
pub fn posts_variables(username: &str) -> String {
    format!(
        r#"{{"data":{POSTS_DATA},"username":{},{POSTS_FLAGS}}}"#,
        serde_json::Value::from(username)
    )
}

/// The variables of the grid's next page, from where the page before ended
/// (`after`), as the app sends them as the grid is scrolled: twelve again,
/// with the caption flag the first page does not carry.
pub fn posts_page_variables(username: &str, after: &str) -> String {
    format!(
        concat!(
            r#"{{"after":{},"before":null,"data":{},"first":12,"#,
            r#""include_multi_captions":true,"last":null,"username":{},{}}}"#
        ),
        serde_json::Value::from(after),
        POSTS_DATA,
        serde_json::Value::from(username),
        POSTS_FLAGS
    )
}

/// `template`, a JSON object the app sent, with the values of `keys` made
/// the ones of `variables`: the app's keys, in its order, and every other
/// value as it sent it. `None` when either is not an object, when none of
/// `keys` is among `variables`', or when `variables` names one that the app's
/// object lacks.
fn with_the_keys_of(template: &str, variables: &str, keys: &[&str]) -> Option<String> {
    let Ordered::Object(mut fields) = serde_json::from_str(template).ok()? else {
        return None;
    };
    let Ordered::Object(asked) = serde_json::from_str(variables).ok()? else {
        return None;
    };
    let mut replaced = false;
    for (name, value) in asked {
        if !keys.contains(&name.as_str()) {
            continue;
        }
        fields.iter_mut().find(|(have, _)| *have == name)?.1 = value;
        replaced = true;
    }
    replaced.then(|| serde_json::to_string(&Ordered::Object(fields)).ok())?
}

/// The top-level keys each of two JSON objects of variables lacks that the
/// other has, `ours` first and then the app's (`apps`), each in its own
/// order: names only, for a log line, since a value can name an account.
/// `None` when either is not an object.
pub fn keys_each_lacks(ours: &str, apps: &str) -> Option<(Vec<String>, Vec<String>)> {
    let keys = |json: &str| -> Option<Vec<String>> {
        let Ordered::Object(fields) = serde_json::from_str(json).ok()? else {
            return None;
        };
        Some(fields.into_iter().map(|(name, _)| name).collect())
    };
    let (ours, apps) = (keys(ours)?, keys(apps)?);
    let lacking = |these: &[String], those: &[String]| -> Vec<String> {
        those
            .iter()
            .filter(|name| !these.contains(name))
            .cloned()
            .collect()
    };
    Some((lacking(&ours, &apps), lacking(&apps, &ours)))
}

/// A JSON value that keeps its objects' keys in the order they came in,
/// which `serde_json::Value` does not.
enum Ordered {
    Object(Vec<(String, Ordered)>),
    Array(Vec<Ordered>),
    Scalar(serde_json::Value),
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Ordered;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Ordered, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry()? {
                    fields.push(field);
                }
                Ok(Ordered::Object(fields))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered::Array(items))
            }

            fn visit_bool<E>(self, v: bool) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(v.into()))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(v.into()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(v.into()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(v.into()))
            }

            fn visit_str<E>(self, v: &str) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(v.into()))
            }

            fn visit_string<E>(self, v: String) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(v.into()))
            }

            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Scalar(serde_json::Value::Null))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for Ordered {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Self::Object(fields) => {
                let mut map = serializer.serialize_map(Some(fields.len()))?;
                for (name, value) in fields {
                    map.serialize_entry(name, value)?;
                }
                map.end()
            }
            Self::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Scalar(value) => value.serialize(serializer),
        }
    }
}

/// `__req` as the app writes it: base 36, lowercase.
fn base36(mut n: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).expect("ASCII digits")
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

fn encoded(pairs: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// A logged-in page and the app's calls on it, invented in the capture's
/// shapes: what the tests here, and the client's, build calls from.
#[cfg(test)]
pub(crate) mod fixtures {
    use snob_core::secret::Secret;

    use super::*;
    use crate::page_values::Site;

    /// A logged-in page, with made-up values shaped like the capture's.
    pub(crate) fn page() -> PageValues {
        PageValues {
            viewer: Some(Viewer {
                pk: Pk::new(1_234_567_890),
                username: "some.viewer".into(),
                fbid: "17841400000000001".into(),
            }),
            site: Some(Site {
                revision: "1000000001".into(),
                hsi: "7000000000000000001".into(),
                haste_session: "20000.HYP:instagram_web_pkg.2.1...0".into(),
                spin_r: "1000000001".into(),
                spin_b: "trunk".into(),
                spin_t: "1790000001".into(),
            }),
            lsd: Some(Secret::new("lsdTokenAAAAAAAAAAAAAA")),
            relay_dtsg: Some(Secret::new("relay:token")),
            session_dtsg: Some(Secret::new("session:token")),
            bloks_version: Some("0123456789abcdef".repeat(4)),
            web_session: Some("abc123:def456:ghi789".into()),
            signed_out: false,
        }
    }

    /// The app's own Relay form on the page, with its counter at `c`.
    pub(crate) const APP_RELAY: &str = concat!(
        "av=17841400000000001&__d=www&__user=0&__a=1&__req=c",
        "&__hs=20000.HYP%3Ainstagram_web_pkg.2.1...0&dpr=1&__ccg=EXCELLENT",
        "&__rev=1000000001&__s=abc123%3Adef456%3Aghi789&__hsi=7000000000000000001",
        "&__dyn=7xeUmwlE&__csr=gP4ll&__hsdp=&__hblp=&__sjsp=g4o",
        "&__comet_req=7&fb_dtsg=relay%3Atoken&jazoest=21234&lsd=lsdTokenAAAAAAAAAAAAAA",
        "&__spin_r=1000000001&__spin_b=trunk&__spin_t=1790000001",
        "&fb_api_caller_class=RelayModern&fb_api_req_friendly_name=SomeQuery",
        "&server_timestamps=true&variables=%7B%7D&doc_id=1000000000000001",
    );

    /// The app's own route call on the page.
    pub(crate) const APP_ROUTES: &str = concat!(
        "route_urls[0]=%2Fsomeone%2F&route_urls[1]=%2Fsomeone.else%2F",
        "&routing_namespace=igx_www&__d=www&__user=0&__a=1&__req=d",
        "&__hs=20000.HYP%3Ainstagram_web_pkg.2.1...0&dpr=1&__ccg=EXCELLENT",
        "&__rev=1000000001&__s=abc123%3Adef456%3Aghi789&__hsi=7000000000000000001",
        "&__dyn=7xeUmwlE&__csr=gP4ll&__comet_req=7&fb_dtsg=relay%3Atoken",
        "&jazoest=21234&lsd=lsdTokenAAAAAAAAAAAAAA&__spin_r=1000000001",
        "&__spin_b=trunk&__spin_t=1790000001&__crn=comet.igweb.PolarisProfilePostsTabRoute",
    );

    pub(crate) fn app() -> AppCalls {
        let mut app = AppCalls::default();
        app.saw("/api/graphql", APP_RELAY);
        app.saw(BULK_ROUTE_DEFINITIONS, APP_ROUTES);
        app
    }
}

#[cfg(test)]
mod tests {
    use snob_core::secret::Secret;

    use super::fixtures::{APP_RELAY, APP_ROUTES, app, page};
    use super::*;

    fn context<'a>(page: &'a PageValues, app: &'a mut AppCalls) -> Context<'a> {
        Context {
            page,
            app,
            csrf: Some("csrfTokenBBBBBBBBBBBBBBBBBBBBBBB"),
            claim: "0",
        }
    }

    const PROFILE: Operation = Operation::ProfilePage;
    const BY_PK: &str = r#"{"id":"2345678901"}"#;

    #[test]
    fn a_rest_read_carries_the_apps_headers() {
        let (page, mut app) = (page(), app());
        let built = context(&page, &mut app).rest_get(
            Rest::Followers(Pk::new(2_345_678_901)),
            &[("count", "12"), ("search_surface", "follow_list_page")],
            "/someone/",
        );
        assert_eq!(built.method, Method::Get);
        assert_eq!(
            built.target(),
            "/api/v1/friendships/2345678901/followers/?count=12&search_surface=follow_list_page"
        );
        assert_eq!(
            built.headers,
            owned(&[
                ("X-IG-App-ID", "936619743392459"),
                ("X-ASBD-ID", "359341"),
                ("X-CSRFToken", "csrfTokenBBBBBBBBBBBBBBBBBBBBBBB"),
                ("X-IG-WWW-Claim", "0"),
                ("X-Requested-With", "XMLHttpRequest"),
                ("X-Web-Session-ID", "abc123:def456:ghi789"),
                ("X-IG-Max-Touch-Points", "0"),
            ])
        );
        assert_eq!(built.referrer, "/someone/");
        assert_eq!(built.body(), None);
    }

    /// The claim is whatever the cycle is at, and the web session id is left
    /// out until the app has made one, as the app's first calls leave it.
    #[test]
    fn a_rest_read_echoes_the_claim_and_waits_for_the_web_session() {
        let mut page = page();
        page.web_session = None;
        let mut app = app();
        let mut context = context(&page, &mut app);
        context.claim = "hmac.AR2claimCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
        let built = context.rest_get(Rest::Following(Pk::new(2_345_678_901)), &[], "/");
        assert_eq!(
            built.header("x-ig-www-claim"),
            Some("hmac.AR2claimCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC")
        );
        assert_eq!(built.header("x-web-session-id"), None);
        assert_eq!(built.target(), "/api/v1/friendships/2345678901/following/");
    }

    /// REST and Relay calls carry `X-CSRFToken` even with no token known,
    /// for the page to write its cookie's in; a route call carries none, as
    /// the app's do not.
    #[test]
    fn the_csrf_header_goes_on_the_families_that_carry_it() {
        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        context.csrf = None;
        let rest = context.rest_get(Rest::Followers(Pk::new(2_345_678_901)), &[], "/");
        let relay = context.relay(PROFILE, BY_PK, None, "/", None).unwrap();
        let routes = context.bulk_route_definitions(&["/a/"], "/").unwrap();
        assert_eq!(rest.header("x-csrftoken"), Some(""));
        assert_eq!(relay.header("x-csrftoken"), Some(""));
        assert_eq!(routes.header("x-csrftoken"), None);
    }

    /// The whole form, field by field, in the order the app sends it.
    #[test]
    fn a_relay_call_sends_the_apps_twenty_nine_fields() {
        let (page, mut app) = (page(), app());
        let built = context(&page, &mut app)
            .relay(
                PROFILE,
                BY_PK,
                Some("comet.igweb.PolarisProfilePostsTabRoute"),
                "/someone/",
                None,
            )
            .unwrap();
        assert_eq!(built.method, Method::Post);
        assert_eq!(built.target(), "/api/graphql");
        let expected = owned(&[
            ("av", "17841400000000001"),
            ("__d", "www"),
            ("__user", "0"),
            ("__a", "1"),
            ("__req", "e"),
            ("__hs", "20000.HYP:instagram_web_pkg.2.1...0"),
            ("dpr", "1"),
            ("__ccg", "EXCELLENT"),
            ("__rev", "1000000001"),
            ("__s", "abc123:def456:ghi789"),
            ("__hsi", "7000000000000000001"),
            ("__dyn", "7xeUmwlE"),
            ("__csr", "gP4ll"),
            ("__hsdp", ""),
            ("__hblp", ""),
            ("__sjsp", "g4o"),
            ("__comet_req", "7"),
            ("fb_dtsg", "relay:token"),
            ("jazoest", jazoest("relay:token").as_str()),
            ("lsd", "lsdTokenAAAAAAAAAAAAAA"),
            ("__spin_r", "1000000001"),
            ("__spin_b", "trunk"),
            ("__spin_t", "1790000001"),
            ("__crn", "comet.igweb.PolarisProfilePostsTabRoute"),
            ("fb_api_caller_class", "RelayModern"),
            ("fb_api_req_friendly_name", "PolarisProfilePageContentQuery"),
            ("server_timestamps", "true"),
            ("variables", r#"{"id":"2345678901"}"#),
            ("doc_id", "28036671149327607"),
        ]);
        assert_eq!(expected.len(), 29);
        assert_eq!(built.form, Some(expected));
        assert_eq!(
            built.headers,
            owned(&[
                ("X-FB-Friendly-Name", "PolarisProfilePageContentQuery"),
                ("X-FB-LSD", "lsdTokenAAAAAAAAAAAAAA"),
                ("X-CSRFToken", "csrfTokenBBBBBBBBBBBBBBBBBBBBBBB"),
                ("X-IG-App-ID", "936619743392459"),
                ("X-ASBD-ID", "359341"),
                ("X-IG-Max-Touch-Points", "0"),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ])
        );
        // Relay never announces itself as the REST client does.
        for rest_only in [
            "X-IG-WWW-Claim",
            "X-Requested-With",
            "X-Web-Session-ID",
            "X-Instagram-AJAX",
        ] {
            assert_eq!(built.header(rest_only), None, "{rest_only}");
        }
    }

    /// Right after a document loads, the app's queries name no route.
    #[test]
    fn a_relay_call_on_no_route_leaves_the_route_out() {
        let (page, mut app) = (page(), app());
        let built = context(&page, &mut app)
            .relay(PROFILE, BY_PK, None, "/", None)
            .unwrap();
        assert_eq!(built.field("__crn"), None);
        assert_eq!(built.form.unwrap().len(), 28);
    }

    #[test]
    fn a_query_on_graphql_query_names_its_root_field_and_the_bloks_version() {
        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        let variables = r#"{"initial_reel_id":"2345678901"}"#;
        let query = context
            .relay(Operation::ReelGallery, variables, None, "/", None)
            .unwrap();
        let graphql = context
            .relay(Operation::HoverCard, variables, None, "/", None)
            .unwrap();
        assert_eq!(query.target(), "/graphql/query");
        assert_eq!(graphql.target(), "/api/graphql");
        assert_eq!(
            query.header("x-root-field-name"),
            Some("xdt_api__v1__feed__reels_media__connection")
        );
        assert_eq!(
            query.header("x-bloks-version-id"),
            Some("0123456789abcdef".repeat(4).as_str())
        );
        assert_eq!(graphql.header("x-root-field-name"), None);
        let names = |built: &Built| -> Vec<String> {
            built.headers.iter().map(|(n, _)| n.clone()).collect()
        };
        assert_eq!(names(&query)[..graphql.headers.len()], names(&graphql)[..]);
        assert_eq!(query.headers.len(), graphql.headers.len() + 2);

        // The same form, but for the operation and the counter each call
        // takes.
        let shared = |built: &Built| {
            let mut form = built.form.clone().unwrap();
            form.retain(|(name, _)| {
                !["__req", "fb_api_req_friendly_name", "doc_id"].contains(&name.as_str())
            });
            form
        };
        assert_eq!(shared(&query), shared(&graphql));
        assert_eq!(query.field("doc_id"), Some("28262315486766731"));
        assert_eq!(
            query.field("fb_api_req_friendly_name"),
            Some("PolarisStoriesV3ReelPageGalleryQuery")
        );
        assert_eq!(query.field("__req"), Some("e"));
        assert_eq!(graphql.field("__req"), Some("f"));
    }

    /// In base 36, continuing after the highest `__req` the app has sent on
    /// the page. The app's own counter does not see these, so its next call
    /// repeats one (see [`AppCalls`]).
    #[test]
    fn the_request_counter_continues_the_apps_in_base_36() {
        let page = page();
        let mut app = AppCalls::default();
        app.saw("/api/graphql", APP_RELAY);
        app.saw(BULK_ROUTE_DEFINITIONS, APP_ROUTES);
        app.saw("/ajax/bootloader-endpoint/", "modules=x&__req=z");
        app.saw("/api/graphql", "__req=3");
        let mut context = context(&page, &mut app);
        let next = |context: &mut Context<'_>| {
            context
                .relay(PROFILE, BY_PK, None, "/", None)
                .unwrap()
                .field("__req")
                .unwrap()
                .to_string()
        };
        assert_eq!(next(&mut context), "10");
        assert_eq!(next(&mut context), "11");
        let routes = context.bulk_route_definitions(&["/a/"], "/").unwrap();
        assert_eq!(routes.field("__req"), Some("12"));
    }

    #[test]
    fn a_fresh_page_counts_from_one() {
        let mut app = AppCalls::default();
        assert_eq!(app.next_req(), "1");
        assert_eq!(app.next_req(), "2");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36 * 36), "100");
        assert_eq!(base36(0), "0");
    }

    /// A value the page has not shown is not made up: the call is not
    /// built, and it takes no number from the counter.
    #[test]
    fn a_relay_call_the_page_cannot_fill_is_not_built() {
        type Remove = fn(&mut PageValues, &mut AppCalls);
        let cases: [(Remove, &str); 8] = [
            (|p, _| p.viewer = None, "the viewer"),
            (|p, _| p.site = None, "SiteData"),
            (|p, _| p.lsd = None, "LSD"),
            (|p, _| p.relay_dtsg = None, "the Relay token"),
            (|p, _| p.web_session = None, "the web session id"),
            (|_, a| *a = AppCalls::default(), "a Relay call of the app's"),
            (
                |_, a| a.dtsg = Some("another:token".into()),
                "a Relay token the app sent",
            ),
            (|p, _| p.bloks_version = None, "the Bloks version"),
        ];
        for (remove, what) in cases {
            let (mut page, mut app) = (page(), app());
            remove(&mut page, &mut app);
            let before = app.next_req;
            let mut context = context(&page, &mut app);
            let built = context.relay(Operation::ReelGallery, BY_PK, None, "/", None);
            assert_eq!(built.unwrap_err(), Missing(what));
            assert_eq!(app.next_req, before, "{what}");
        }
    }

    /// The gaps only the app's calls fill are the ones worth waiting for;
    /// the document's own values are there or never will be.
    #[test]
    fn only_the_apps_calls_are_waited_on() {
        for waits in [
            Missing::RELAY_CALL,
            Missing::ROUTE_CALL,
            Missing::WEB_SESSION,
            Missing::RELAY_TOKEN_SENT,
        ] {
            assert!(waits.waits_on_the_app(), "{waits}");
        }
        for now in [
            "the viewer",
            "SiteData",
            "LSD",
            "the Relay token",
            "the Bloks version",
        ] {
            assert!(!Missing(now).waits_on_the_app(), "{now}");
        }
    }

    /// The variables of the registry's operations the app sent are kept,
    /// the latest of each; another operation's, or a route call's, are not.
    #[test]
    fn the_variables_the_app_sent_are_kept_for_the_registrys_operations() {
        let form = |name: &str, variables: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("fb_api_req_friendly_name", name),
                    ("variables", variables),
                    ("doc_id", "1000000000000001"),
                ])
                .finish()
        };
        let mut app = AppCalls::default();
        assert_eq!(app.saw("/api/graphql", APP_RELAY), None, "SomeQuery");
        let profile = PROFILE.friendly_name();
        assert_eq!(
            app.saw("/api/graphql", &form(profile, r#"{"id":"1","flag":true}"#)),
            Some(PROFILE)
        );
        assert_eq!(
            app.saw("/api/graphql", &form(profile, r#"{"id":"2","flag":false}"#)),
            Some(PROFILE)
        );
        assert_eq!(
            app.variables_sent(PROFILE),
            Some(r#"{"id":"2","flag":false}"#)
        );
        assert_eq!(app.variables_sent(Operation::HoverCard), None);
        let routes = form(Operation::HoverCard.friendly_name(), "{}");
        assert_eq!(app.saw(BULK_ROUTE_DEFINITIONS, &routes), None);
        assert_eq!(app.variables_sent(Operation::HoverCard), None);
    }

    /// Only a whole Relay form is copied: one missing a bitmap leaves the
    /// last whole one in place.
    #[test]
    fn a_relay_form_is_copied_whole_or_not_at_all() {
        let mut app = AppCalls::default();
        app.saw(
            "/api/graphql",
            "dpr=1&__ccg=GOOD&__dyn=a&__csr=b&__hsdp=&__hblp=",
        );
        assert!(app.relay.is_none());
        app.saw("/graphql/query", APP_RELAY);
        app.saw("/api/graphql", "dpr=2&__ccg=GOOD&__dyn=z");
        let copy = app.relay.unwrap();
        assert_eq!(copy.ccg, "EXCELLENT");
        assert_eq!(copy.bitmaps[0], ("__dyn", "7xeUmwlE".to_string()));
        // A route call is not a Relay form, and a Relay form is not a route
        // call.
        let mut app = AppCalls::default();
        app.saw(BULK_ROUTE_DEFINITIONS, APP_RELAY);
        app.saw("/ajax/navigation/", APP_ROUTES);
        assert!(app.relay.is_none());
        assert!(app.comet.is_none());
    }

    #[test]
    fn route_definitions_go_in_the_apps_envelope() {
        let (page, mut app) = (page(), app());
        let built = context(&page, &mut app)
            .bulk_route_definitions(&["/another/", "/third.one/"], "/someone/")
            .unwrap();
        assert_eq!(built.method, Method::Post);
        assert_eq!(built.target(), "/ajax/bulk-route-definitions/");
        assert_eq!(
            built.form,
            Some(owned(&[
                ("route_urls[0]", "/another/"),
                ("route_urls[1]", "/third.one/"),
                ("routing_namespace", "igx_www"),
                ("__d", "www"),
                ("__user", "0"),
                ("__a", "1"),
                ("__req", "e"),
                ("__hs", "20000.HYP:instagram_web_pkg.2.1...0"),
                ("dpr", "1"),
                ("__ccg", "EXCELLENT"),
                ("__rev", "1000000001"),
                ("__s", "abc123:def456:ghi789"),
                ("__hsi", "7000000000000000001"),
                ("__dyn", "7xeUmwlE"),
                ("__csr", "gP4ll"),
                ("__comet_req", "7"),
                ("fb_dtsg", "relay:token"),
                ("jazoest", jazoest("relay:token").as_str()),
                ("lsd", "lsdTokenAAAAAAAAAAAAAA"),
                ("__spin_r", "1000000001"),
                ("__spin_b", "trunk"),
                ("__spin_t", "1790000001"),
                ("__crn", "comet.igweb.PolarisProfilePostsTabRoute"),
            ]))
        );
        assert_eq!(
            built.headers,
            owned(&[
                ("X-FB-LSD", "lsdTokenAAAAAAAAAAAAAA"),
                ("X-IG-D", "www"),
                ("X-ASBD-ID", "359341"),
                ("X-IG-Max-Touch-Points", "0"),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ])
        );
        assert!(
            built
                .body()
                .unwrap()
                .starts_with("route_urls%5B0%5D=%2Fanother%2F&route_urls%5B1%5D=%2Fthird.one%2F&")
        );
    }

    /// The token in the envelope is the page's current one, and only when
    /// the app itself sent it on this document: which of the page's two
    /// tokens Relay takes is not confirmed.
    #[test]
    fn the_envelope_takes_the_relay_token_the_app_sent() {
        let mut page = page();
        page.relay_dtsg = Some(Secret::new("fresh:token"));
        let mut app = app();
        app.saw("/graphql/query", "fb_dtsg=fresh%3Atoken&__req=1");
        let built = context(&page, &mut app)
            .bulk_route_definitions(&["/a/"], "/")
            .unwrap();
        assert_eq!(built.field("fb_dtsg"), Some("fresh:token"));
        assert_eq!(
            built.field("jazoest"),
            Some(jazoest("fresh:token").as_str())
        );

        // The app's calls sent the other one.
        let mut app = self::app();
        let missing = context(&page, &mut app).bulk_route_definitions(&["/a/"], "/");
        assert_eq!(missing.unwrap_err(), Missing("a Relay token the app sent"));
        // A REST POST's token is not the Relay one.
        let mut app = self::app();
        app.dtsg = None;
        app.saw("/api/v1/web/something/", "fb_dtsg=fresh%3Atoken");
        let missing = context(&page, &mut app).bulk_route_definitions(&["/a/"], "/");
        assert_eq!(missing.unwrap_err(), Missing("a Relay token the app sent"));

        page.relay_dtsg = None;
        let mut app = self::app();
        let missing = context(&page, &mut app).bulk_route_definitions(&["/a/"], "/");
        assert_eq!(missing.unwrap_err(), Missing("the Relay token"));
        let mut app = AppCalls::default();
        assert!(!app.made_a_route_call());
        let missing = context(&self::page(), &mut app).bulk_route_definitions(&["/a/"], "/");
        assert_eq!(missing.unwrap_err(), Missing("a route call of the app's"));
        assert!(self::app().made_a_route_call());
    }

    /// Which token the app's calls carry is told by which of the page's
    /// two it matches, and not before the app has sent one.
    #[test]
    fn the_token_the_app_sent_is_named_and_not_shown() {
        let page = page();
        assert_eq!(app().sent_token(&page), SentToken::Relay);

        let mut app = AppCalls::default();
        assert_eq!(app.sent_token(&page), SentToken::NotYet);
        // A REST call's token is not a Relay or Comet call's.
        app.saw("/api/v1/web/something/", "fb_dtsg=session%3Atoken");
        assert_eq!(app.sent_token(&page), SentToken::NotYet);
        app.saw("/api/graphql", "fb_dtsg=session%3Atoken&__req=1");
        assert_eq!(app.sent_token(&page), SentToken::Session);
        app.saw(BULK_ROUTE_DEFINITIONS, "fb_dtsg=another%3Atoken&__req=2");
        assert_eq!(app.sent_token(&page), SentToken::Neither);

        let printed = format!("{:?}", app.sent_token(&page));
        assert!(!printed.contains("token:"), "{printed}");
    }

    /// The keys each side's variables lack are named, in each side's order,
    /// and no value is.
    #[test]
    fn the_keys_each_side_lacks_are_names_only() {
        let (ours, apps) = keys_each_lacks(
            r#"{"id":"2345678901","render_surface":"PROFILE"}"#,
            r#"{"enable_integrity_filters":true,"id":"3456789012","app_flag":false}"#,
        )
        .unwrap();
        assert_eq!(ours, ["enable_integrity_filters", "app_flag"]);
        assert_eq!(apps, ["render_surface"]);
        let printed = format!("{ours:?}{apps:?}");
        for value in ["2345678901", "3456789012", "PROFILE", "true", "false"] {
            assert!(!printed.contains(value), "{printed}");
        }

        let (ours, apps) = keys_each_lacks(BY_PK, BY_PK).unwrap();
        assert!(ours.is_empty() && apps.is_empty());
        assert_eq!(keys_each_lacks(BY_PK, "[]"), None);
        assert_eq!(keys_each_lacks("not json", BY_PK), None);
    }

    /// What is built is what the page lets out.
    #[test]
    fn every_call_built_here_passes_the_allowlist() {
        use crate::allowlist::refused;

        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        let mut built = Vec::new();
        for operation in Operation::ALL {
            built.push(
                context
                    .relay(operation, BY_PK, None, "/someone/", None)
                    .unwrap(),
            );
        }
        built.push(context.bulk_route_definitions(&["/a/"], "/").unwrap());
        built.push(context.navigation("/someone/").unwrap());
        built.push(context.navigation("/someone/followers/mutualOnly").unwrap());
        let pk = Pk::new(2_345_678_901);
        built.push(
            context
                .show_many(&[pk, VIEWER], "/someone/", Some("rest:token"))
                .unwrap(),
        );
        for read in [
            Rest::Followers(pk),
            Rest::Following(pk),
            Rest::MutualFollowers(pk),
        ] {
            built.push(context.rest_get(read, &[("count", "12")], "/someone/"));
        }
        for built in built {
            let target = built.target();
            let request = built.into_request(ORIGIN, 1 << 20, 30_000);
            assert_eq!(refused(&request), None, "{target}");
        }
    }

    const ORIGIN: &str = "https://www.instagram.com";
    const VIEWER: Pk = Pk::new(1_234_567_890);

    fn call(ask: Ask) -> Call {
        Call {
            ask,
            origin: ORIGIN.into(),
            referrer: "/someone/".into(),
            claim: "0".into(),
            cap: 1 << 20,
            timeout_ms: 30_000,
        }
    }

    fn query(operation: Operation, variables: &str) -> Ask {
        Ask::Query {
            operation,
            variables: variables.into(),
        }
    }

    fn write(mutation: Mutation) -> Ask {
        Ask::Write {
            mutation,
            variables: r#"{"target_user_id":"2345678901"}"#.into(),
            doc_id: "1000000000000003".into(),
            route: "comet.igweb.PolarisProfilePostsTabRoute".into(),
        }
    }

    /// Every ask that sends something, sent from the page as it was asked:
    /// on the origin, from the referrer, and past the allowlist.
    #[test]
    fn every_ask_that_sends_builds_a_request_the_allowlist_lets_out() {
        use crate::allowlist::refused;

        let pk = Pk::new(2_345_678_901);
        let asks = [
            query(PROFILE, BY_PK),
            query(Operation::HoverCard, r#"{"userID":"2345678901"}"#),
            query(Operation::ReelGallery, BY_PK),
            Ask::Rest {
                read: Rest::Followers(pk),
                query: vec![
                    ("count".into(), "12".into()),
                    ("search_surface".into(), "follow_list_page".into()),
                ],
            },
            Ask::Rest {
                read: Rest::MutualFollowers(pk),
                query: vec![("page_size".into(), "12".into())],
            },
            Ask::Pk {
                name: "someone".into(),
            },
            write(Mutation::Follow),
            write(Mutation::Unfollow),
        ];
        let (page, mut app) = (page(), app());
        for ask in asks {
            let endpoint = ask.endpoint();
            let request =
                build_call(&call(ask), &page, &mut app, VIEWER, &Seen::default()).unwrap();
            assert_eq!(refused(&request), None, "{endpoint}");
            let url = url::Url::parse(&request.url).unwrap();
            assert_eq!(url.path(), endpoint);
            assert_eq!(request.referrer, "https://www.instagram.com/someone/");
            assert!(!request.navigate);
        }
    }

    #[test]
    fn a_name_is_asked_as_its_route() {
        let (page, mut app) = (page(), app());
        let ask = Ask::Pk {
            name: "some.one_else".into(),
        };
        let request = build_call(&call(ask), &page, &mut app, VIEWER, &Seen::default()).unwrap();
        assert_eq!(
            request.url,
            "https://www.instagram.com/ajax/bulk-route-definitions/"
        );
        let body = request.body.unwrap();
        assert!(
            body.starts_with("route_urls%5B0%5D=%2Fsome.one_else%2F&routing_namespace="),
            "{body}"
        );
    }

    /// A write goes out under the number found for it, on the route it is
    /// made from.
    #[test]
    fn a_write_carries_its_doc_id_and_route() {
        let (page, mut app) = (page(), app());
        let request = build_call(
            &call(write(Mutation::Follow)),
            &page,
            &mut app,
            VIEWER,
            &Seen::default(),
        )
        .unwrap();
        let form: Vec<(String, String)> =
            url::form_urlencoded::parse(request.body.unwrap().as_bytes())
                .into_owned()
                .collect();
        let field = |name: &str| {
            form.iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(form.len(), 29);
        assert_eq!(field("doc_id"), Some("1000000000000003"));
        assert_eq!(
            field("__crn"),
            Some("comet.igweb.PolarisProfilePostsTabRoute")
        );
        assert_eq!(
            field("fb_api_req_friendly_name"),
            Some("usePolarisFollowMutation")
        );
        assert_eq!(
            field("variables"),
            Some(r#"{"target_user_id":"2345678901"}"#)
        );
        assert_eq!(request.url, "https://www.instagram.com/api/graphql");
    }

    /// A write named as a query is refused where the intent is checked, and
    /// where it is built, whichever is reached: nothing builds it without
    /// the other.
    #[test]
    fn a_query_of_a_write_is_never_built() {
        for mutation in Mutation::ALL {
            let operation = Operation::of_write(mutation);
            let ask = query(operation, BY_PK);
            let (page, mut app) = (page(), app());
            let refused = build_call(
                &call(ask.clone()),
                &page,
                &mut app,
                VIEWER,
                &Seen::default(),
            );
            assert_eq!(
                refused.unwrap_err(),
                Unbuilt::NotAllowed("POST /api/graphql".into())
            );
            let before = app.next_req;
            let built = context(&page, &mut app).build(&ask, "/someone/", &Seen::default());
            assert_eq!(
                built.unwrap_err(),
                Unbuilt::NotAllowed("POST /api/graphql".into())
            );
            assert_eq!(app.next_req, before, "no number taken");
        }
    }

    /// What the tab answers itself is never a request.
    #[test]
    fn the_asks_the_tab_answers_are_not_built() {
        for ask in [
            Ask::Viewer,
            Ask::Tray,
            Ask::Document {
                path: "/someone/".into(),
            },
            Ask::Asset {
                url: "https://scontent.cdninstagram.com/v/a.jpg".into(),
            },
        ] {
            let (page, mut app) = (page(), app());
            let built = build_call(
                &call(ask.clone()),
                &page,
                &mut app,
                VIEWER,
                &Seen::default(),
            );
            assert!(matches!(built, Err(Unbuilt::NotAllowed(_))), "{ask:?}");
            let built = context(&page, &mut app).build(&ask, "/", &Seen::default());
            assert!(matches!(built, Err(Unbuilt::NotAllowed(_))), "{ask:?}");
        }
    }

    /// The profile query takes the app's own variables, flags and all,
    /// with only the id snob's; with none seen, the ask's.
    #[test]
    fn the_profile_query_copies_the_apps_variables_but_the_id() {
        let seen = r#"{"id":"1000000001","render_surface":"PROFILE","a_flag":true,"count":3,"nested":{"z":1,"a":[null,"x"]}}"#;
        let (page, mut app) = (page(), app());
        let request = build_call(
            &call(query(PROFILE, BY_PK)),
            &page,
            &mut app,
            VIEWER,
            &Seen {
                variables: Some(seen),
                doc_id: None,
                rest_token: None,
            },
        )
        .unwrap();
        let variables = |request: &PageRequest| {
            url::form_urlencoded::parse(request.body.as_deref().unwrap().as_bytes())
                .into_owned()
                .find(|(n, _)| n == "variables")
                .unwrap()
                .1
        };
        assert_eq!(
            variables(&request),
            r#"{"id":"2345678901","render_surface":"PROFILE","a_flag":true,"count":3,"nested":{"z":1,"a":[null,"x"]}}"#
        );

        let request = build_call(
            &call(query(PROFILE, BY_PK)),
            &page,
            &mut app,
            VIEWER,
            &Seen::default(),
        )
        .unwrap();
        assert_eq!(variables(&request), BY_PK);
        // A template that is not an object, or names no id, is not one.
        for seen in ["[1]", r#"{"userID":"1"}"#, "not json"] {
            let request = build_call(
                &call(query(PROFILE, BY_PK)),
                &page,
                &mut app,
                VIEWER,
                &Seen {
                    variables: Some(seen),
                    doc_id: None,
                    rest_token: None,
                },
            )
            .unwrap();
            assert_eq!(variables(&request), BY_PK, "{seen}");
        }
        // Another query keeps its own variables whatever was seen.
        let hover = r#"{"userID":"2345678901"}"#;
        let request = build_call(
            &call(query(Operation::HoverCard, hover)),
            &page,
            &mut app,
            VIEWER,
            &Seen {
                variables: Some(seen),
                doc_id: None,
                rest_token: None,
            },
        )
        .unwrap();
        assert_eq!(variables(&request), hover);
    }

    fn variables_of(request: &PageRequest) -> String {
        url::form_urlencoded::parse(request.body.as_deref().unwrap().as_bytes())
            .into_owned()
            .find(|(n, _)| n == "variables")
            .unwrap()
            .1
    }

    fn field_of(request: &PageRequest, name: &str) -> Option<String> {
        url::form_urlencoded::parse(request.body.as_deref().unwrap().as_bytes())
            .into_owned()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// Before the app has sent a query to copy, the profile query goes with
    /// the capture's seven variables, in the app's order; once it has, its
    /// flags are the app's and only the id is snob's.
    #[test]
    fn the_profile_query_goes_with_the_apps_seven_variables() {
        let pk = Pk::new(2_345_678_901);
        let ours = profile_variables(pk);
        let keys: Vec<String> =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&ours)
                .unwrap()
                .keys()
                .cloned()
                .collect();
        assert_eq!(keys.len(), 7);
        assert_eq!(
            ours,
            concat!(
                r#"{"enable_integrity_filters":true,"id":"2345678901","#,
                r#""__relay_internal__pv__PolarisCannesGuardianExperienceEnabledrelayprovider":true,"#,
                r#""__relay_internal__pv__PolarisCASB976ProfileEnabledrelayprovider":false,"#,
                r#""__relay_internal__pv__PolarisWebSchoolsEnabledrelayprovider":false,"#,
                r#""__relay_internal__pv__PolarisRepostsConsumptionEnabledrelayprovider":true,"#,
                r#""__relay_internal__pv__PolarisShortDramaEnabledrelayprovider":false}"#
            )
        );

        let (page, mut app) = (page(), app());
        let request = build_call(
            &call(query(PROFILE, &ours)),
            &page,
            &mut app,
            VIEWER,
            &Seen::default(),
        )
        .unwrap();
        assert_eq!(variables_of(&request), ours);

        // The app's flags win, with its order, and the id stays the asked one.
        let theirs = r#"{"enable_integrity_filters":true,"id":"1","__relay_internal__pv__PolarisNewFlagrelayprovider":true}"#;
        let request = build_call(
            &call(query(PROFILE, &ours)),
            &page,
            &mut app,
            VIEWER,
            &Seen {
                variables: Some(theirs),
                doc_id: None,
                rest_token: None,
            },
        )
        .unwrap();
        assert_eq!(
            variables_of(&request),
            r#"{"enable_integrity_filters":true,"id":"2345678901","__relay_internal__pv__PolarisNewFlagrelayprovider":true}"#
        );
    }

    /// The gallery and the highlights page go with the viewer's flag, and
    /// copy the app's own flags as the profile query does: only the keys
    /// that say what is opened are snob's.
    #[test]
    fn the_viewers_queries_carry_their_flag_and_copy_the_apps() {
        let tray = vec!["2345678901".to_string(), "7".to_string()];
        let ours = reel_variables("2345678901", &tray);
        assert_eq!(
            ours,
            concat!(
                r#"{"initial_reel_id":"2345678901","reel_ids":["2345678901","7"],"first":3,"last":2,"#,
                r#""__relay_internal__pv__PolarisCommunityNoteStoriesLabelEnabledrelayprovider":true}"#
            )
        );
        let theirs = concat!(
            r#"{"initial_reel_id":"1","reel_ids":["1"],"first":4,"last":1,"#,
            r#""__relay_internal__pv__PolarisCommunityNoteStoriesLabelEnabledrelayprovider":false}"#
        );
        let (page, mut app) = (page(), app());
        for operation in [Operation::ReelGallery, Operation::HighlightsPage] {
            let request = build_call(
                &call(query(operation, &ours)),
                &page,
                &mut app,
                VIEWER,
                &Seen {
                    variables: Some(theirs),
                    doc_id: None,
                    rest_token: None,
                },
            )
            .unwrap();
            // The reel and the tray are asked ones, the window and the flag
            // the app's.
            assert_eq!(
                variables_of(&request),
                concat!(
                    r#"{"initial_reel_id":"2345678901","reel_ids":["2345678901","7"],"first":3,"last":2,"#,
                    r#""__relay_internal__pv__PolarisCommunityNoteStoriesLabelEnabledrelayprovider":false}"#
                ),
                "{operation:?}"
            );
        }
        // Another operation's template is not this one's: the hover card
        // names a user, not a reel.
        let request = build_call(
            &call(query(Operation::HoverCard, r#"{"userID":"2345678901"}"#)),
            &page,
            &mut app,
            VIEWER,
            &Seen {
                variables: Some(theirs),
                doc_id: None,
                rest_token: None,
            },
        )
        .unwrap();
        assert_eq!(variables_of(&request), r#"{"userID":"2345678901"}"#);
    }

    /// A read names the route of the page it is made from, `__crn`, the one
    /// the app's own calls on that page carry; none for a page that is
    /// neither the home page nor a profile.
    #[test]
    fn a_read_names_the_route_of_the_page_it_is_made_from() {
        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        let built = |context: &mut Context<'_>, referrer: &str| {
            context
                .build(&query(PROFILE, BY_PK), referrer, &Seen::default())
                .unwrap()
        };
        let home = built(&mut context, "/");
        let profile = built(&mut context, "/someone/");
        let other = built(&mut context, "/explore/");
        assert_eq!(home.field("__crn"), Some("comet.igweb.PolarisFeedRoute"));
        assert_eq!(
            profile.field("__crn"),
            Some("comet.igweb.PolarisProfilePostsTabRoute")
        );
        assert_eq!(other.field("__crn"), None);
        assert_eq!(route_of("/"), Some(FEED_ROUTE));
        assert_eq!(route_of("/some.one/"), Some(PROFILE_ROUTE));
        assert_eq!(route_of("/stories/"), None);
        assert_eq!(route_of("/someone/followers/"), None);
        // The field sits where the app puts it: after `__spin_t`, before
        // the caller class.
        let names: Vec<&str> = home
            .form
            .as_ref()
            .unwrap()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        let at = names.iter().position(|n| *n == "__crn").unwrap();
        assert_eq!(names[at - 1], "__spin_t");
        assert_eq!(names[at + 1], "fb_api_caller_class");
    }

    /// The first call built on a document whose app has named no route yet
    /// leaves it out, as the app's own queries right after a cold document
    /// do; the next, and any after the app names one, carry it.
    #[test]
    fn the_first_query_on_a_cold_document_names_no_route() {
        let page = page();
        let mut cold = AppCalls::default();
        cold.saw("/api/graphql", APP_RELAY);
        let mut context = context(&page, &mut cold);
        let first = context
            .build(&query(PROFILE, BY_PK), "/someone/", &Seen::default())
            .unwrap();
        let second = context
            .build(&query(PROFILE, BY_PK), "/someone/", &Seen::default())
            .unwrap();
        assert_eq!(first.field("__crn"), None);
        assert_eq!(first.form.unwrap().len(), 28);
        assert_eq!(
            second.field("__crn"),
            Some("comet.igweb.PolarisProfilePostsTabRoute")
        );

        // The app has named one: the first call carries it.
        let mut warm = AppCalls::default();
        warm.saw(
            "/api/graphql",
            &format!("{APP_RELAY}&__crn=comet.igweb.PolarisFeedRoute"),
        );
        let first = context_of(&page, &mut warm)
            .build(&query(PROFILE, BY_PK), "/", &Seen::default())
            .unwrap();
        assert_eq!(first.field("__crn"), Some("comet.igweb.PolarisFeedRoute"));
    }

    fn context_of<'a>(page: &'a PageValues, app: &'a mut AppCalls) -> Context<'a> {
        context(page, app)
    }

    /// The route call carries `X-FB-QPL-Active-Flows` when the envelope it
    /// copies names its flows, as the app sends it, and none when it does
    /// not.
    #[test]
    fn a_route_call_carries_the_flows_its_envelope_names() {
        let page = page();
        let mut app = AppCalls::default();
        app.saw("/api/graphql", APP_RELAY);
        app.saw(
            BULK_ROUTE_DEFINITIONS,
            &format!("{APP_ROUTES}&qpl_active_flow_ids=175125627%2C516759801"),
        );
        let built = context(&page, &mut app)
            .bulk_route_definitions(&["/a/"], "/")
            .unwrap();
        assert_eq!(
            built.header("x-fb-qpl-active-flows"),
            Some("175125627,516759801")
        );
        assert_eq!(
            built.field("qpl_active_flow_ids"),
            Some("175125627,516759801")
        );
        let names: Vec<&str> = built.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "X-FB-LSD",
                "X-FB-QPL-Active-Flows",
                "X-IG-D",
                "X-ASBD-ID",
                "X-IG-Max-Touch-Points",
                "Content-Type"
            ]
        );

        let mut plain = self::app();
        let built = context(&page, &mut plain)
            .bulk_route_definitions(&["/a/"], "/")
            .unwrap();
        assert_eq!(built.header("x-fb-qpl-active-flows"), None);
    }

    /// The app's identifiers are copied from its own calls when they carry
    /// them, and the constants stand in until then. Only digits are taken,
    /// and only from the app's API calls.
    #[test]
    fn the_apps_identifiers_are_copied_where_its_calls_carry_them() {
        let page = page();
        let headers = |app_id: &str, asbd: &str| {
            vec![
                ("X-IG-App-ID".to_string(), app_id.to_string()),
                ("x-asbd-id".to_string(), asbd.to_string()),
            ]
        };
        let mut app = self::app();
        app.saw_with(
            "/api/graphql",
            APP_RELAY,
            &headers("936619743392460", "359342"),
        );
        let (rest, relay, routes) = {
            let mut context = context(&page, &mut app);
            (
                context.rest_get(Rest::Following(Pk::new(2_345_678_901)), &[], "/"),
                context.relay(PROFILE, BY_PK, None, "/", None).unwrap(),
                context.bulk_route_definitions(&["/a/"], "/").unwrap(),
            )
        };
        for built in [&rest, &relay, &routes] {
            assert_eq!(
                built.header("x-asbd-id"),
                Some("359342"),
                "{:?}",
                built.path
            );
        }
        assert_eq!(rest.header("x-ig-app-id"), Some("936619743392460"));
        assert_eq!(relay.header("x-ig-app-id"), Some("936619743392460"));
        assert_eq!(routes.header("x-ig-app-id"), None);

        // Not digits, or not the app's API: not taken.
        let mut app = self::app();
        app.saw_with("/api/graphql", APP_RELAY, &headers("abc", "<script>"));
        app.saw_with("/ajax/bz", "a=b", &headers("1", "2"));
        let rest =
            context(&page, &mut app).rest_get(Rest::Following(Pk::new(2_345_678_901)), &[], "/");
        assert_eq!(rest.header("x-ig-app-id"), Some(IG_APP_ID));
        assert_eq!(rest.header("x-asbd-id"), Some(ASBD_ID));
    }

    /// The number the app sends a read under is kept when the call vouches
    /// for it, and goes into a call built beside it; one that does not, a
    /// write's, and one for another operation are not.
    #[test]
    fn a_reads_number_follows_the_app_when_its_call_vouches_for_it() {
        let form = |name: &str, doc_id: &str, variables: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("fb_api_req_friendly_name", name),
                    ("variables", variables),
                    ("doc_id", doc_id),
                ])
                .finish()
        };
        let moved = "1000000000000777";
        let mut app = AppCalls::default();
        // On /api/graphql the app names no root field.
        assert_eq!(
            app.saw_with(
                "/api/graphql",
                &form(PROFILE.friendly_name(), moved, r#"{"id":"1"}"#),
                &[],
            ),
            Some(PROFILE)
        );
        assert_eq!(app.doc_id_sent(PROFILE), Some(moved));
        // Where the operation is sent on /graphql/query the app names it.
        let gallery = Operation::ReelGallery;
        let theirs = form(gallery.friendly_name(), moved, r#"{"initial_reel_id":"1"}"#);
        app.saw_with("/graphql/query", &theirs, &[]);
        assert_eq!(app.doc_id_sent(gallery), None, "no root field named");
        let root = vec![(
            "X-Root-Field-Name".to_string(),
            "something_else".to_string(),
        )];
        app.saw_with("/graphql/query", &theirs, &root);
        assert_eq!(app.doc_id_sent(gallery), None, "another root field");
        let root = vec![(
            "X-Root-Field-Name".to_string(),
            gallery.root_field().to_string(),
        )];
        app.saw_with("/graphql/query", &theirs, &root);
        assert_eq!(app.doc_id_sent(gallery), Some(moved));
        // The variables must carry what the operation asks by; a number
        // that is not one; a write; an endpoint that is not the operation's.
        let hover = Operation::HoverCard;
        app.saw_with(
            "/api/graphql",
            &form(hover.friendly_name(), moved, r#"{"id":"1"}"#),
            &[],
        );
        app.saw_with(
            "/api/graphql",
            &form(
                Operation::HighlightsTray.friendly_name(),
                "12ab",
                r#"{"user_id":"1"}"#,
            ),
            &[],
        );
        app.saw_with(
            "/api/graphql",
            &form(
                Operation::Follow.friendly_name(),
                moved,
                r#"{"target_user_id":"1"}"#,
            ),
            &[],
        );
        app.saw_with(
            "/graphql/query",
            &form(
                Operation::StoriesTray.friendly_name(),
                moved,
                r#"{"data":{}}"#,
            ),
            &[],
        );
        for operation in [
            hover,
            Operation::HighlightsTray,
            Operation::Follow,
            Operation::StoriesTray,
        ] {
            assert_eq!(app.doc_id_sent(operation), None, "{operation:?}");
        }

        // Built beside it, the read goes under the app's number, and the
        // allowlist lets it out under that number and no other.
        let page = page();
        let mut app = self::app();
        let ask = call(query(PROFILE, BY_PK));
        let own = build_call(&ask, &page, &mut app, VIEWER, &Seen::default()).unwrap();
        assert_eq!(field_of(&own, "doc_id").as_deref(), Some(PROFILE.doc_id()));
        let seen = Seen {
            variables: None,
            doc_id: Some(moved),
            rest_token: None,
        };
        let found = build_call(&ask, &page, &mut app, VIEWER, &seen).unwrap();
        assert_eq!(field_of(&found, "doc_id").as_deref(), Some(moved));
        assert_eq!(
            allowlist::refused(&found).as_deref(),
            Some("POST /api/graphql")
        );
        assert_eq!(
            allowlist::refused_with(&found, &Rediscovered::of(PROFILE, moved)),
            None
        );
        assert!(
            allowlist::refused_with(&found, &Rediscovered::of(PROFILE, "1000000000000778"))
                .is_some()
        );
        // A number is taken for a read only: it never makes a write's own.
        let follow = call(write(Mutation::Follow));
        let sent = build_call(&follow, &page, &mut app, VIEWER, &seen).unwrap();
        assert_eq!(
            field_of(&sent, "doc_id").as_deref(),
            Some("1000000000000003")
        );
    }

    /// A document signed out, or served to another account, sends nothing
    /// as this one; one whose viewer was not read waits on nothing.
    #[test]
    fn a_page_not_logged_in_as_the_account_builds_nothing() {
        let ask = query(Operation::HoverCard, BY_PK);
        let mut signed_out = page();
        signed_out.viewer = None;
        signed_out.signed_out = true;
        let mut app = app();
        let before = app.next_req;
        assert_eq!(
            build_call(
                &call(ask.clone()),
                &signed_out,
                &mut app,
                VIEWER,
                &Seen::default()
            )
            .unwrap_err(),
            Unbuilt::LoggedOut
        );
        assert_eq!(
            build_call(
                &call(ask.clone()),
                &page(),
                &mut app,
                Pk::new(43),
                &Seen::default()
            )
            .unwrap_err(),
            Unbuilt::LoggedOut
        );
        let mut unread = page();
        unread.viewer = None;
        let missing =
            build_call(&call(ask), &unread, &mut app, VIEWER, &Seen::default()).unwrap_err();
        assert_eq!(missing, Unbuilt::Missing(Missing("the viewer")));
        assert!(!Missing("the viewer").waits_on_the_app());
        assert_eq!(app.next_req, before);
    }

    /// What the owner's socket carries reads back as it was written.
    #[test]
    fn asks_and_answers_read_back_as_written() {
        let pk = Pk::new(2_345_678_901);
        for ask in [
            Ask::Viewer,
            Ask::Tray,
            Ask::Pk {
                name: "someone".into(),
            },
            Ask::Document {
                path: "/someone/".into(),
            },
            Ask::Asset {
                url: "https://scontent.cdninstagram.com/v/a.jpg".into(),
            },
            query(Operation::HighlightsTray, BY_PK),
            Ask::Rest {
                read: Rest::Following(pk),
                query: vec![("count".into(), "12".into())],
            },
            write(Mutation::Unfollow),
        ] {
            let call = call(ask);
            let json = serde_json::to_string(&call).unwrap();
            assert_eq!(serde_json::from_str::<Call>(&json).unwrap(), call);
        }

        let answer = PageResponse {
            status: 200,
            body: "{}".into(),
            ..PageResponse::default()
        };
        let told = [
            Told::Answer(answer.clone()),
            Told::Viewer(page().viewer.unwrap()),
            Told::Tray(Some(vec!["1".into(), "2".into()])),
            Told::Tray(None),
            Told::Pk {
                answer: answer.clone(),
                pk: RouteAnswer::Pk(pk),
            },
            Told::Pk {
                answer: answer.clone(),
                pk: RouteAnswer::NoProfile,
            },
            Told::Document {
                answer: answer.clone(),
                bundles: vec!["https://static.cdninstagram.com/rsrc.php/a.js".into()],
            },
            Told::Asset(answer),
        ];
        for told in told {
            let json = serde_json::to_string(&told).unwrap();
            let back: Told = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }

    /// The navigation is the route definitions' envelope after the viewer's
    /// `fbid` and the route, on the route moved to, made from the route:
    /// the capture's 26 fields when the envelope is the app's.
    #[test]
    fn a_navigation_goes_in_the_apps_envelope_on_the_route_moved_to() {
        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        let built = context.navigation("/someone/").unwrap();
        assert_eq!(built.method, Method::Post);
        assert_eq!(built.target(), "/ajax/navigation/");
        assert_eq!(built.referrer, "/someone/");
        let names: Vec<&str> = built
            .form
            .as_ref()
            .unwrap()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(
            names[..3],
            ["client_previous_actor_id", "route_url", "routing_namespace"]
        );
        assert_eq!(
            built.field("client_previous_actor_id"),
            Some("17841400000000001")
        );
        assert_eq!(built.field("route_url"), Some("/someone/"));
        assert_eq!(built.field("__crn"), Some(PROFILE_ROUTE));
        assert_eq!(built.field("__req"), Some("e"));
        assert_eq!(built.field("fb_dtsg"), Some("relay:token"));
        assert!(built.field("route_urls[0]").is_none());
        let routes = context.bulk_route_definitions(&["/a/"], "/").unwrap();
        assert_eq!(built.headers, routes.headers, "the family's headers");

        // The mutual tab is the profile's route; the home page the feed's.
        let mutual = context.navigation("/someone/followers/mutualOnly").unwrap();
        assert_eq!(mutual.field("__crn"), Some(PROFILE_ROUTE));
        assert_eq!(mutual.referrer, "/someone/followers/mutualOnly");
        let home = context.navigation("/").unwrap();
        assert_eq!(home.field("__crn"), Some(FEED_ROUTE));

        // Without a route call of the app's to copy, nothing is built.
        let mut cold = AppCalls::default();
        cold.saw("/api/graphql", APP_RELAY);
        let missing = super::tests::context(&page, &mut cold).navigation("/someone/");
        assert_eq!(missing.unwrap_err(), Missing::ROUTE_CALL);
    }

    /// `show_many` is the REST family's headers and the app's revision, and
    /// a form of the pks, `jazoest` and the REST token, in that order; with
    /// no token the app sent, nothing is built and nothing waits for one.
    #[test]
    fn show_many_is_a_rest_post_with_the_token_the_app_sent() {
        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        let pks = [Pk::new(2_345_678_901), Pk::new(7)];
        let built = context
            .show_many(&pks, "/someone/", Some("rest:token"))
            .unwrap();
        assert_eq!(built.method, Method::Post);
        assert_eq!(built.target(), "/api/v1/friendships/show_many/");
        assert_eq!(built.referrer, "/someone/");
        assert_eq!(
            built.form,
            Some(owned(&[
                ("user_ids", "2345678901,7"),
                ("jazoest", jazoest("rest:token").as_str()),
                ("fb_dtsg", "rest:token"),
            ]))
        );
        assert_eq!(
            built.headers,
            owned(&[
                ("X-IG-App-ID", "936619743392459"),
                ("X-ASBD-ID", "359341"),
                ("X-CSRFToken", "csrfTokenBBBBBBBBBBBBBBBBBBBBBBB"),
                ("X-IG-WWW-Claim", "0"),
                ("X-Requested-With", "XMLHttpRequest"),
                ("X-Web-Session-ID", "abc123:def456:ghi789"),
                ("X-IG-Max-Touch-Points", "0"),
                ("X-Instagram-AJAX", "1000000001"),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ])
        );
        let missing = context.show_many(&pks, "/someone/", None).unwrap_err();
        assert_eq!(missing, Missing::REST_TOKEN_SENT);
        assert!(!missing.waits_on_the_app());
        assert!(!format!("{built:?}").contains("rest:token"));
    }

    /// The REST token is the one the app's REST POSTs carried, and only
    /// when the `jazoest` beside it is the one it makes; a Relay or Comet
    /// call's token is not it.
    #[test]
    fn the_rest_token_is_taken_from_the_apps_rest_posts() {
        let mut app = app();
        assert_eq!(app.rest_token_sent(), None, "the Relay token is not it");
        let rest = |token: &str, sent: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([("user_ids", "1"), ("jazoest", sent), ("fb_dtsg", token)])
                .finish()
        };
        app.saw("/api/v1/news/inbox/", &rest("rest:token", "1"));
        assert_eq!(app.rest_token_sent(), None, "a jazoest it does not make");
        app.saw(
            "/api/v1/news/inbox/",
            &rest("rest:token", &jazoest("rest:token")),
        );
        assert_eq!(app.rest_token_sent(), Some("rest:token"));
        app.saw("/api/graphql", "fb_dtsg=relay%3Atoken&__req=z");
        assert_eq!(app.rest_token_sent(), Some("rest:token"));
        assert_eq!(app.sent_token(&page()), SentToken::Relay);
    }

    /// Both are asked by intent, and an intent the tab cannot fill sends
    /// nothing: the token comes from what the app sent, through [`Seen`].
    #[test]
    fn the_list_asks_are_built_from_the_intent() {
        let (page, mut app) = (page(), app());
        let navigation = Ask::Navigation {
            route: "/someone/".into(),
        };
        let request =
            build_call(&call(navigation), &page, &mut app, VIEWER, &Seen::default()).unwrap();
        assert_eq!(request.url, "https://www.instagram.com/ajax/navigation/");
        assert_eq!(request.referrer, "https://www.instagram.com/someone/");

        let statuses = Ask::Statuses {
            pks: vec![Pk::new(7)],
        };
        let unbuilt = build_call(
            &call(statuses.clone()),
            &page,
            &mut app,
            VIEWER,
            &Seen::default(),
        );
        assert_eq!(
            unbuilt.unwrap_err(),
            Unbuilt::Missing(Missing::REST_TOKEN_SENT)
        );
        let seen = Seen {
            rest_token: Some("rest:token"),
            ..Seen::default()
        };
        let request = build_call(&call(statuses), &page, &mut app, VIEWER, &seen).unwrap();
        assert_eq!(
            request.url,
            "https://www.instagram.com/api/v1/friendships/show_many/"
        );
        assert_eq!(request.referrer, "https://www.instagram.com/someone/");
        assert!(allowlist::refused(&request).is_none());

        // A route snob does not read from is refused before it is built.
        let elsewhere = Ask::Navigation {
            route: "/stories/someone/".into(),
        };
        let refused = build_call(&call(elsewhere), &page, &mut app, VIEWER, &Seen::default());
        assert!(matches!(refused, Err(Unbuilt::NotAllowed(_))));
    }

    #[test]
    fn a_built_request_does_not_print_its_tokens() {
        let (page, mut app) = (page(), app());
        let mut context = context(&page, &mut app);
        let built = [
            context.relay(PROFILE, BY_PK, None, "/", None).unwrap(),
            context.bulk_route_definitions(&["/a/"], "/").unwrap(),
        ];
        for built in built {
            let printed = format!("{built:?}");
            assert!(printed.contains("fb_dtsg"), "{printed}");
            for token in ["relay:token", "lsdToken", "csrfToken"] {
                assert!(!printed.contains(token), "{printed}");
            }
        }
    }

    /// The grid's variables are the captures' (2026-10-01), key for key and
    /// in their order, with only the name and the cursor made snob's.
    #[test]
    fn the_grids_variables_are_the_apps() {
        assert_eq!(
            posts_variables("some.one"),
            concat!(
                r#"{"data":{"count":12,"include_reel_media_seen_timestamp":true,"#,
                r#""include_relationship_info":true,"latest_besties_reel_media":true,"#,
                r#""latest_reel_media":true},"username":"some.one","#,
                r#""__relay_internal__pv__PolarisMultiCaptionCarouselEnabledrelayprovider":true,"#,
                r#""__relay_internal__pv__PolarisShortDramaEnabledrelayprovider":false,"#,
                r#""__relay_internal__pv__PolarisReelsRecoDebugOverlayEnabledrelayprovider":false}"#
            )
        );
        let page: serde_json::Value =
            serde_json::from_str(&posts_page_variables("some.one", "AQH\"x")).unwrap();
        assert_eq!(page["after"], "AQH\"x");
        assert_eq!(page["first"], 12);
        assert_eq!(page["username"], "some.one");
        assert_eq!(page["data"]["count"], 12);
        let keys: Vec<String> = keys_each_lacks("{}", &posts_page_variables("a", "b"))
            .unwrap()
            .0;
        assert_eq!(
            keys[..8],
            [
                "after",
                "before",
                "data",
                "first",
                "include_multi_captions",
                "last",
                "username",
                "__relay_internal__pv__PolarisMultiCaptionCarouselEnabledrelayprovider"
            ]
        );
    }
}
