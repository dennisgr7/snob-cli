//! Listening to the calls Instagram's own app makes on the page, for
//! push-back.
//!
//! **Why.** The tab snob sends from is on instagram.com, and the site's app
//! runs in it and makes calls of its own. Instagram talks to the session
//! through every one of them: a 429 or a challenge on one of the app's calls is
//! the service saying no, exactly as it would be on one of snob's. Unheard,
//! that answer would reach snob only at its own next request, and not at all
//! when there was none — and a 429 on the site's own page load would be read
//! by nobody.
//! The rule is a hard stop on the first push-back, so the browser listens.
//!
//! **What counts.** Only calls to Instagram's hosts (in a sandbox, to the site
//! the tab is on) that the app sends with `XMLHttpRequest` or `fetch`, and the
//! documents the tab loads. A 429 counts the moment its answer arrives; any
//! other answer that could be one — a refusal, or a JSON answer that might
//! declare a failure under a 200 — is judged when its body has arrived, by
//! [`snob_ig::error::push_back`], the same rule the client applies to its own.
//! A route the app takes to `/challenge/` counts when the tab's address says
//! so, with no request at all. The paths and the thresholds are provisional
//! until a capture of the real app with `tools/capture/record.js` says
//! otherwise; what makes them safe meanwhile is that nothing is counted that
//! the rule does not already call a push-back.
//!
//! **Recorded exactly once, by the process holding the browser.** A cooldown
//! repeated within a day doubles, so one push-back written down twice costs
//! twice. A [`Latch`] shared with the page decides it: the listener, and the
//! page for each of snob's own answers ([`pushed_back`]), claim it through
//! [`heard`], and only the first to claim it records, in the database of the
//! account the browser is for. The command that asked is not needed for it:
//! a push-back is recorded even when that command has stopped waiting. Once it is
//! claimed, every answer of snob's goes back as `PageError::PushedBack`,
//! which the client never records.
//!
//! The listener hears the documents snob reads as well as the app's; the
//! fetches snob sends are skipped only to spare reading their bodies twice,
//! as the page judges their answers itself. Each runs in a script named
//! [`OWN_SCRIPT`], which is the initiator the browser reports for it —
//! measured on Chromium 141, where the app's own `eval`'d code has an empty
//! name, so an empty name alone would have taken those for snob's.
//!
//! The app's calls also carry what the document does not: the web session
//! id, the rest of the app's forms and the variables of its queries, kept
//! for the requests built from the page's values ([`Documents`]). They are
//! kept by the document that sent them, named by its loader, so that a call
//! is built only from its own document's even when an event that said the
//! tab changed documents was dropped or came late.
//!
//! Once claimed, the tab is sent to `about:blank` — nothing more of the app
//! runs — and the page refuses to send anything until the browser is closed.
//!
//! **Where.** In a task of its own, fed by the dispatcher's events. A body is
//! read with `Network.getResponseBody` from yet another task: the dispatcher
//! must never wait on a reply, and this task must not stop listening while it
//! waits for one. `Fetch.enable` is not used for any of this: it pauses every
//! request it matches — the API calls, the paths snob never sends to, and
//! video — and each is judged in the dispatcher ([`super::guard`]), where no
//! event is dropped, rather than here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use snob_ig::allowlist::{Operation, Rediscovered};
use snob_ig::client::page::{PageResponse, PushedBack};
use snob_ig::error::{IgError, landed_on, push_back};
use snob_ig::web::AppCalls;
use snob_store::paths::AccountPaths;

use super::lock;
use crate::cdp::{Connection, Event};

/// The name snob's own fetch script runs under, which the browser reports as
/// the initiator of every request it sends. Seen by nothing but the protocol:
/// the script runs in an isolated world the page cannot reach.
pub(super) const OWN_SCRIPT: &str = "snob-request";

/// How many of the app's requests are followed at once. A request is
/// forgotten when it finishes; this only bounds a page that starts requests
/// and never finishes them.
const FOLLOWED: usize = 512;

/// How long reading one answer's body may take.
const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// How many of the tab's documents the app's calls are kept for: the one
/// the tab is on, and the few before it whose late calls may still arrive
/// and must land on their own.
const KEPT_DOCUMENTS: usize = 4;

/// The first push-back heard on a browser, and who heard it.
///
/// A `watch` rather than a plain lock so that it can be waited on: the owner
/// of the browsers tells every command sending as the account the moment it is
/// claimed, not at the command's next request (`owner`).
#[derive(Debug)]
pub(super) struct Latch {
    claimed: tokio::sync::watch::Sender<Option<PushedBack>>,
}

impl Default for Latch {
    fn default() -> Self {
        Self {
            claimed: tokio::sync::watch::Sender::new(None),
        }
    }
}

impl Latch {
    /// Claims the latch for `cause`. `true` for the first to claim it, who
    /// records it; `false` when it was claimed already.
    pub(super) fn claim(&self, cause: PushedBack) -> bool {
        self.claimed.send_if_modified(|claimed| {
            if claimed.is_some() {
                return false;
            }
            *claimed = Some(cause);
            true
        })
    }

    /// What was heard, once something was.
    pub(super) fn get(&self) -> Option<PushedBack> {
        self.claimed.borrow().clone()
    }

    /// Waits until something is heard, for as long as the browser lives.
    pub(super) fn heard(&self) -> tokio::sync::watch::Receiver<Option<PushedBack>> {
        self.claimed.subscribe()
    }
}

/// What the page's traffic is judged against, shared with the engine that
/// sends from the same tab.
#[derive(Debug, Default)]
pub(super) struct Shared {
    pub(super) latch: Latch,
    /// The host of the site the requests are sent to, set only where the tab
    /// is sent there: counted beside Instagram's own, which is what lets a
    /// sandbox's fake be heard, and the host whose cookies are written back.
    pub(super) site: Mutex<Option<String>>,
    /// What the app's own calls carried, by the document that sent them.
    /// A std lock, never held across an await.
    pub(super) documents: Mutex<Documents>,
}

/// What the app's calls from one of the tab's documents carried.
#[derive(Debug)]
pub(super) struct Document {
    /// The loader the document came from, which names it.
    loader: String,
    /// The web session id the app last sent from it ([`web_session_of`]).
    /// The app makes a new one for each page load.
    pub(super) web_session: Option<String>,
    /// For a request built like the app's to copy.
    pub(super) calls: AppCalls,
    /// How many of the app's calls from it were taken in: what tells when
    /// they have paused.
    pub(super) heard: u64,
}

/// The app's calls, kept by the document that sent them: the last
/// [`KEPT_DOCUMENTS`], most recent first.
#[derive(Debug, Default)]
pub(super) struct Documents {
    kept: VecDeque<Document>,
    /// The latest `variables` the app sent with each of the registry's
    /// reads, from any document. Kept when the document goes: a flag the
    /// app sends is the same on the next document, where the app may not
    /// send that read at all.
    templates: HashMap<Operation, String>,
    /// The numbers the app sent the registry's reads under, from any
    /// document, when its call vouched for them: kept for the same reason.
    rediscovered: Rediscovered,
    /// The token the app's latest REST POST carried, from any document
    /// (`AppCalls::rest_token_sent`). Kept when the document goes: in the
    /// capture of 2026-10-01 one REST token served three document loads,
    /// where the Relay token changed with each.
    rest_token: Option<String>,
}

impl Documents {
    /// The document `loader` names, made when it is new; the oldest goes
    /// when there are more than [`KEPT_DOCUMENTS`].
    pub(super) fn entry(&mut self, loader: &str) -> &mut Document {
        let at = match self.kept.iter().position(|d| d.loader == loader) {
            Some(at) => at,
            None => {
                self.kept.push_front(Document {
                    loader: loader.to_string(),
                    web_session: None,
                    calls: AppCalls::default(),
                    heard: 0,
                });
                self.kept.truncate(KEPT_DOCUMENTS);
                0
            }
        };
        &mut self.kept[at]
    }

    /// The document `loader` names, if its calls are kept.
    pub(super) fn get(&self, loader: &str) -> Option<&Document> {
        self.kept.iter().find(|d| d.loader == loader)
    }

    /// The latest `variables` the app sent with `operation`, a read of the
    /// registry's, from any document.
    pub(super) fn template(&self, operation: Operation) -> Option<&str> {
        self.templates.get(&operation).map(String::as_str)
    }

    /// The numbers the app was seen to send the registry's reads under.
    pub(super) fn rediscovered(&self) -> &Rediscovered {
        &self.rediscovered
    }

    /// The token the app's latest REST POST carried, from any document.
    pub(super) fn rest_token(&self) -> Option<&str> {
        self.rest_token.as_deref()
    }

    /// Takes in one of the app's calls from the document `loader` names: its
    /// web session id, and its form or query, sent to `path` under `headers`.
    fn saw(
        &mut self,
        loader: &str,
        session: Option<String>,
        path: &str,
        form: Option<&str>,
        headers: &[(String, String)],
    ) {
        let document = self.entry(loader);
        document.heard += 1;
        if session.is_some() {
            document.web_session = session;
        }
        let Some(form) = form else {
            return;
        };
        let operation = document.calls.saw_with(path, form, headers);
        if let Some(token) = document.calls.rest_token_sent().map(str::to_string) {
            self.rest_token = Some(token);
        }
        let Some(operation) = operation else {
            return;
        };
        let document = self.entry(loader);
        if operation.write().is_none() {
            let variables = document.calls.variables_sent(operation).map(str::to_string);
            let doc_id = document.calls.doc_id_sent(operation).map(str::to_string);
            if let Some(variables) = variables {
                self.templates.insert(operation, variables);
            }
            if let Some(doc_id) = doc_id {
                self.rediscovered.insert(operation, &doc_id);
            }
        }
    }
}

impl Shared {
    fn counts_host(&self, host: &str) -> bool {
        host == "instagram.com"
            || host.ends_with(".instagram.com")
            || lock(&self.site).as_deref() == Some(host)
    }
}

/// One of the app's requests being followed to its answer.
#[derive(Debug)]
struct Followed {
    /// The path it was sent to, for the line a push-back leaves: which of
    /// the app's calls was refused is the one thing the cause cannot say.
    path: String,
    status: Option<u16>,
    /// Its body has to be read before it can be judged.
    judge_body: bool,
}

/// What an event decided.
#[derive(Debug)]
enum Heard {
    Nothing,
    /// A push-back, decided on the spot.
    PushBack(IgError),
    /// A candidate whose body has to be read first.
    ReadBody {
        request: String,
        status: u16,
        path: String,
    },
}

/// The app's requests, followed from sending to answer: the part of the
/// listener that decides, with no browser and no clock in it.
#[derive(Debug)]
struct Traffic {
    shared: Arc<Shared>,
    /// The tab's own target, whose address changes are watched.
    tab_target: String,
    followed: HashMap<String, Followed>,
    order: VecDeque<String>,
}

impl Traffic {
    fn new(shared: Arc<Shared>, tab_target: String) -> Self {
        Self {
            shared,
            tab_target,
            followed: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// What one event from the tab, or from the browser, decides.
    fn feed(&mut self, event: &Event) -> Heard {
        let params = &event.params;
        match event.method.as_str() {
            "Network.requestWillBeSent" => self.sent(params),
            "Network.responseReceived" => self.answered(params),
            "Network.loadingFinished" => {
                let Some(id) = params.get("requestId").and_then(Value::as_str) else {
                    return Heard::Nothing;
                };
                match self.forget(id) {
                    Some(Followed {
                        judge_body: true,
                        status: Some(status),
                        path,
                        ..
                    }) => Heard::ReadBody {
                        request: id.to_string(),
                        status,
                        path,
                    },
                    _ => Heard::Nothing,
                }
            }
            "Network.loadingFailed" => {
                if let Some(id) = params.get("requestId").and_then(Value::as_str) {
                    self.forget(id);
                }
                Heard::Nothing
            }
            "Target.targetInfoChanged" => {
                let info = params.get("targetInfo").unwrap_or(&Value::Null);
                if info.get("targetId").and_then(Value::as_str) != Some(self.tab_target.as_str()) {
                    return Heard::Nothing;
                }
                // The app routes without loading a document: the address is
                // what says it went to the challenge.
                let address = info.get("url").and_then(Value::as_str).unwrap_or("");
                self.landing(address, None)
            }
            _ => Heard::Nothing,
        }
    }

    fn sent(&mut self, params: &Value) -> Heard {
        let Some(id) = params.get("requestId").and_then(Value::as_str) else {
            return Heard::Nothing;
        };
        let kind = params.get("type").and_then(Value::as_str).unwrap_or("");
        let document = kind == "Document";
        if !(document || kind == "XHR" || kind == "Fetch") {
            return Heard::Nothing;
        }
        let address = params
            .pointer("/request/url")
            .and_then(Value::as_str)
            .unwrap_or("");
        // A hop of a request already followed: the answer that redirected it
        // is judged as its own, and a document sent on to the challenge is a
        // push-back whatever the hop said.
        if params.get("redirectResponse").is_some() {
            if let Some(status) = params
                .pointer("/redirectResponse/status")
                .and_then(Value::as_u64)
                && status == 429
                && self.followed.contains_key(id)
            {
                let path = self.forget(id).map(|f| f.path).unwrap_or_default();
                noted(&path, 429, "");
                return Heard::PushBack(IgError::RateLimited);
            }
            if document {
                let heard = self.landing(address, Some(id));
                if !matches!(heard, Heard::Nothing) {
                    return heard;
                }
            }
        }
        let Ok(url) = url::Url::parse(address) else {
            return Heard::Nothing;
        };
        if !url
            .host_str()
            .is_some_and(|host| self.shared.counts_host(host))
        {
            self.forget(id);
            return Heard::Nothing;
        }
        if !document && sent_by_snob(params) {
            self.forget(id);
            return Heard::Nothing;
        }
        // A frame's document is not a new load of the tab's.
        let frame = params.get("frameId").and_then(Value::as_str);
        let tab_document = document && frame == Some(self.tab_target.as_str());
        let loader = params
            .get("loaderId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if (tab_document || !document) && loader.is_empty() {
            tracing::trace!(path = url.path(), "a call with no document to keep it by");
        } else if tab_document {
            lock(&self.shared.documents).entry(loader);
        } else if !document {
            let posted = super::guard::form_of(params.get("request").unwrap_or(&Value::Null))
                .ok()
                .flatten();
            let session = web_session_of(params, posted.as_deref());
            let form = posted.as_deref().or(url.query());
            let headers = headers_of(params);
            lock(&self.shared.documents).saw(loader, session, url.path(), form, &headers);
        }
        self.follow(
            id,
            Followed {
                path: url.path().to_string(),
                status: None,
                judge_body: false,
            },
        );
        Heard::Nothing
    }

    fn answered(&mut self, params: &Value) -> Heard {
        let Some(id) = params.get("requestId").and_then(Value::as_str) else {
            return Heard::Nothing;
        };
        if !self.followed.contains_key(id) {
            return Heard::Nothing;
        }
        let status = params
            .pointer("/response/status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(0);
        if status == 429 {
            let path = self.forget(id).map(|f| f.path).unwrap_or_default();
            noted(&path, 429, "");
            return Heard::PushBack(IgError::RateLimited);
        }
        let json = params
            .pointer("/response/mimeType")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("json"));
        let refused = !(200..300).contains(&status) && !(300..400).contains(&status);
        match self.followed.get_mut(id) {
            Some(followed) if refused || json => {
                followed.status = Some(status);
                followed.judge_body = true;
            }
            _ => {
                self.forget(id);
            }
        }
        Heard::Nothing
    }

    /// A push-back if `address` is the challenge; only the challenge, as the
    /// login form is a session gone and no push-back.
    fn landing(&mut self, address: &str, request: Option<&str>) -> Heard {
        let Ok(url) = url::Url::parse(address) else {
            return Heard::Nothing;
        };
        if !url
            .host_str()
            .is_some_and(|host| self.shared.counts_host(host))
        {
            return Heard::Nothing;
        }
        match landed_on(url.path()) {
            Some(error) if snob_ig::error::cooldown_for(&error).is_some() => {
                if let Some(id) = request {
                    self.forget(id);
                }
                Heard::PushBack(error)
            }
            _ => Heard::Nothing,
        }
    }

    fn follow(&mut self, id: &str, followed: Followed) {
        if self.followed.insert(id.to_string(), followed).is_none() {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > FOLLOWED {
            if let Some(oldest) = self.order.pop_front() {
                self.followed.remove(&oldest);
            }
        }
    }

    fn forget(&mut self, id: &str) -> Option<Followed> {
        let followed = self.followed.remove(id)?;
        self.order.retain(|kept| kept != id);
        Some(followed)
    }
}

/// Whether one of snob's own answers is a push-back, read the way the client
/// would read it: a landing on the challenge that stayed on the site, or
/// whatever [`push_back`] says of the status and the body — of the status
/// alone for a body too large to keep, as the client then reads it.
pub(super) fn pushed_back(asked: &str, response: &PageResponse) -> Option<IgError> {
    let origin = |address: &str| url::Url::parse(address).ok().map(|u| u.origin());
    if response.redirected
        && origin(&response.url) == origin(asked)
        && let Some(landing) = url::Url::parse(&response.url)
            .ok()
            .and_then(|u| landed_on(&u[url::Position::BeforePath..url::Position::AfterQuery]))
            .filter(|e| snob_ig::error::cooldown_for(e).is_some())
    {
        return Some(landing);
    }
    let body = if response.too_large {
        ""
    } else {
        &response.body
    };
    push_back(response.status, body)
}

/// Whether a request was sent by snob's own fetch script, by the name of the
/// script the browser says it came from.
fn sent_by_snob(params: &Value) -> bool {
    params
        .pointer("/initiator/stack/callFrames")
        .and_then(Value::as_array)
        .and_then(|frames| frames.first())
        .and_then(|frame| frame.get("url"))
        .and_then(Value::as_str)
        == Some(OWN_SCRIPT)
}

/// The headers one of the app's calls was sent with, as far as the browser
/// reports them: the identifiers and the root field a call built like it
/// copies or checks are among them.
fn headers_of(params: &Value) -> Vec<(String, String)> {
    params
        .pointer("/request/headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// The web session id one of the app's calls carries: the `X-Web-Session-ID`
/// header its REST calls send, or the `__s` field of a Relay form, `posted`.
fn web_session_of(params: &Value, posted: Option<&str>) -> Option<String> {
    let header = params
        .pointer("/request/headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-web-session-id"))
        })
        .and_then(|(_, value)| value.as_str())
        .map(str::to_string);
    let field = || {
        url::form_urlencoded::parse(posted?.as_bytes())
            .find(|(name, _)| name == "__s")
            .map(|(_, value)| value.into_owned())
    };
    let value = header.or_else(field)?;
    snob_ig::page_values::web_session_id(&value).map(str::to_string)
}

/// Listens to the tab until the browser goes, and records the first
/// push-back it hears.
pub(super) async fn listen(
    connection: Connection,
    tab: String,
    tab_target: String,
    shared: Arc<Shared>,
    paths: AccountPaths,
) {
    // Room for a page's worth of events: a subscriber that falls behind loses
    // them rather than holding up the replies the dispatcher also carries.
    let mut on_tab = connection.subscribe(Some(&tab), 4096);
    let mut on_browser = connection.subscribe(None, 256);
    let mut traffic = Traffic::new(Arc::clone(&shared), tab_target);
    loop {
        let event = tokio::select! {
            event = on_tab.recv() => match event {
                Some(event) => event,
                // The tab is gone, and the browser with it.
                None => return,
            },
            Some(event) = on_browser.recv() => event,
        };
        match traffic.feed(&event) {
            Heard::Nothing => {}
            Heard::PushBack(error) => {
                heard(&connection, &tab, &shared, &paths, &error);
            }
            Heard::ReadBody {
                request,
                status,
                path,
            } => {
                let (connection, tab, shared, paths) = (
                    connection.clone(),
                    tab.clone(),
                    Arc::clone(&shared),
                    paths.clone(),
                );
                tokio::spawn(async move {
                    let Some(body) = body_of(&connection, &tab, &request).await else {
                        return;
                    };
                    if let Some(error) = push_back(status, &body) {
                        noted(&path, status, &body);
                        heard(&connection, &tab, &shared, &paths, &error);
                    }
                });
            }
        }
    }
}

/// The body of an answer the browser still holds, or nothing: a body it has
/// already let go of, one sent as binary, and a failure to ask all mean
/// there is nothing to judge.
async fn body_of(connection: &Connection, tab: &str, request: &str) -> Option<String> {
    let answer = connection
        .call(
            Some(tab),
            "Network.getResponseBody",
            json!({ "requestId": request }),
            BODY_TIMEOUT,
        )
        .await
        .map_err(|e| tracing::debug!(error = %e, "could not read the body of the app's call"))
        .ok()?;
    if answer.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    answer
        .get("body")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// What a call the page made was refused with, at `warn`, whether or not
/// this is the push-back that gets recorded: which call, and what Instagram
/// said. Only the path, as the query can carry a name; the body through the
/// same excerpt every error gets.
fn noted(path: &str, status: u16, body: &str) {
    tracing::warn!(
        path,
        status,
        body = %snob_ig::error::body_excerpt(body),
        "Instagram pushed back on a call the page made"
    );
}

/// What one of snob's own answers was refused with, at `warn`, as the client
/// logs a push-back it reads: the path without its query, the status, the
/// headers that say why, how loaded Instagram was and which backend answered,
/// and the body excerpt.
pub(super) fn noted_own(asked: &str, response: &PageResponse) {
    let path = url::Url::parse(asked)
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    let body = if response.too_large {
        ""
    } else {
        &response.body
    };
    tracing::warn!(
        %path,
        status = response.status,
        retry_after = response.header("retry-after").unwrap_or("<absent>"),
        load = response.load().as_deref().unwrap_or("<absent>"),
        served = response.served().as_deref().unwrap_or("<absent>"),
        body = %snob_ig::error::body_excerpt(body),
        "Instagram pushed back on snob's request"
    );
}

/// A push-back heard on the page, on the app's traffic or on one of snob's
/// own answers: recorded if nobody has yet, and the tab stopped either way.
/// The recording, when this call claimed it, runs on a task of its own, handed
/// back for a caller that must not answer before it is done.
///
/// Every command sending as the account is told the moment the latch is
/// claimed, which is before the cooldown is committed: a command that reads
/// the store at once can still find none. Nothing is sent in that window —
/// the browser refuses everything once claimed — but a reader of the store
/// should go by the error it was given rather than by the row.
pub(super) fn heard(
    connection: &Connection,
    tab: &str,
    shared: &Shared,
    paths: &AccountPaths,
    error: &IgError,
) -> Option<tokio::task::JoinHandle<()>> {
    let cause = PushedBack::of(error)?;
    if !shared.latch.claim(cause) {
        return None;
    }
    tracing::warn!(%error, "Instagram pushed back on the page; stopping");
    connection.fire(Some(tab), "Page.navigate", json!({ "url": "about:blank" }));
    let (reason, minimum) = snob_ig::error::cooldown_for(error)?;
    let paths = paths.clone();
    let reason = reason.to_string();
    Some(tokio::task::spawn_blocking(move || {
        record(&paths, &reason, minimum);
    }))
}

/// Writes the cooldown down where every process reads it before it spends:
/// in the database of the account whose browser heard it.
///
/// Through the budget's own connection alone, as the client records: the
/// command that opened this browser opened the store first, so the schema is
/// there, and an owner of an older build, still serving after an upgrade,
/// must not fail to write it down because a newer build moved the schema on.
fn record(paths: &AccountPaths, reason: &str, minimum: Duration) {
    use snob_core::budget::RateBudget;
    let recorded = snob_store::store::rate_budget::SqliteRateBudget::open(paths)
        .map_err(|e| e.to_string())
        .and_then(|budget| budget.start_cooldown(reason, minimum).map_err(|e| e.0));
    if let Err(e) = recorded {
        tracing::warn!(error = %e, "could not record the cooldown Instagram's push-back earned");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(method: &str, params: Value) -> Event {
        Event {
            session: Some("tab".to_string()),
            method: method.to_string(),
            params,
        }
    }

    fn sent(id: &str, url: &str, kind: &str, script: &str) -> Event {
        event(
            "Network.requestWillBeSent",
            json!({
                "requestId": id,
                "loaderId": "L",
                "type": kind,
                "request": { "url": url },
                "initiator": { "type": "script", "stack": { "callFrames": [ { "url": script } ] } },
            }),
        )
    }

    fn answered(id: &str, status: u16, mime: &str) -> Event {
        event(
            "Network.responseReceived",
            json!({ "requestId": id, "response": { "status": status, "mimeType": mime } }),
        )
    }

    fn finished(id: &str) -> Event {
        event("Network.loadingFinished", json!({ "requestId": id }))
    }

    fn traffic() -> Traffic {
        Traffic::new(Arc::new(Shared::default()), "the-tab".to_string())
    }

    fn is_rate_limited(heard: &Heard) -> bool {
        matches!(heard, Heard::PushBack(IgError::RateLimited))
    }

    fn is_nothing(heard: &Heard) -> bool {
        matches!(heard, Heard::Nothing)
    }

    /// Whether `heard` asks for the body of `request`, answered `status`, to
    /// `path`.
    fn reads_body(heard: &Heard, request: &str, status: u16, path: &str) -> bool {
        matches!(
            heard,
            Heard::ReadBody { request: r, status: s, path: p }
                if r == request && *s == status && p == path
        )
    }

    #[test]
    fn a_429_on_the_apps_call_is_heard_at_once() {
        let mut t = traffic();
        let url = "https://www.instagram.com/api/v1/feed/timeline/";
        assert!(is_nothing(&t.feed(&sent(
            "1",
            url,
            "XHR",
            "https://static.cdninstagram.com/a.js"
        ))));
        assert!(is_rate_limited(&t.feed(&answered(
            "1",
            429,
            "application/json"
        ))));
        assert!(is_nothing(&t.feed(&finished("1"))), "and only once");
    }

    /// One of the app's calls from the tab's document `L1`, carrying
    /// `headers` and `form`.
    fn sent_with(id: &str, url: &str, kind: &str, headers: Value, form: Option<&str>) -> Event {
        let mut request = json!({ "url": url, "headers": headers });
        if let Some(form) = form {
            request["postData"] = json!(form);
        }
        event(
            "Network.requestWillBeSent",
            json!({
                "requestId": id,
                "frameId": "the-tab",
                "loaderId": "L1",
                "type": kind,
                "request": request,
                "initiator": { "type": "script", "stack": { "callFrames": [ { "url": "https://static.cdninstagram.com/a.js" } ] } },
            }),
        )
    }

    /// The same, from the document `loader`.
    fn sent_from(loader: &str, id: &str, url: &str, kind: &str, form: Option<&str>) -> Event {
        let mut sent = sent_with(id, url, kind, json!({}), form);
        sent.params["loaderId"] = json!(loader);
        sent
    }

    fn web_session(t: &Traffic, loader: &str) -> Option<String> {
        let documents = t.shared.documents.lock().unwrap();
        documents.get(loader)?.web_session.clone()
    }

    fn kept(t: &Traffic, loader: &str) -> bool {
        t.shared.documents.lock().unwrap().get(loader).is_some()
    }

    /// A made-up Relay form of the app's for `name`, whole enough to copy,
    /// sending `variables`.
    fn relay_form(name: &str, variables: &str, session: &str) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("__req", "c"),
                ("dpr", "1"),
                ("__ccg", "EXCELLENT"),
                ("__s", session),
                ("__dyn", "7xeUmwlE"),
                ("__csr", "gP4ll"),
                ("__hsdp", ""),
                ("__hblp", ""),
                ("__sjsp", "g4o"),
                ("fb_dtsg", "relay:token"),
                ("fb_api_req_friendly_name", name),
                ("variables", variables),
                ("doc_id", "1000000000000001"),
            ])
            .finish()
    }

    /// The web session id is taken from the app's REST header or its Relay
    /// form, for the document that sent it: the app makes a new one per
    /// load. A frame's document, or a malformed id, changes nothing.
    #[test]
    fn the_web_session_id_is_the_apps_for_the_document_that_sent_it() {
        let mut t = traffic();
        let api = "https://www.instagram.com/api/v1/web/some/call/";
        t.feed(&sent_with(
            "1",
            api,
            "Fetch",
            json!({ "X-Web-Session-ID": "abc123:def456:ghi789" }),
            None,
        ));
        assert_eq!(
            web_session(&t, "L1").as_deref(),
            Some("abc123:def456:ghi789")
        );

        t.feed(&sent_with(
            "2",
            api,
            "XHR",
            json!({ "X-Web-Session-ID": "not an id" }),
            None,
        ));
        let mut frame = sent_from("F1", "3", "https://www.instagram.com/x/", "Document", None);
        frame.params["frameId"] = json!("a-frame");
        t.feed(&frame);
        assert_eq!(
            web_session(&t, "L1").as_deref(),
            Some("abc123:def456:ghi789")
        );
        assert!(!kept(&t, "F1"), "a frame's document is not the tab's");

        let home = "https://www.instagram.com/";
        t.feed(&sent_from("L2", "4", home, "Document", None));
        assert_eq!(web_session(&t, "L2"), None);

        let mut relay = sent_from(
            "L2",
            "5",
            "https://www.instagram.com/api/graphql",
            "XHR",
            Some("av=17841400000000001&__s=abc123%3Adef456%3Ajkl012&__user=0"),
        );
        relay.params["request"]["headers"] = json!({ "X-FB-LSD": "lsd" });
        t.feed(&relay);
        assert_eq!(
            web_session(&t, "L2").as_deref(),
            Some("abc123:def456:jkl012")
        );
        assert_eq!(
            web_session(&t, "L1").as_deref(),
            Some("abc123:def456:ghi789")
        );
    }

    /// Two documents' calls stay apart, and a call whose document was never
    /// announced — its Document event dropped, or yet to come — is kept by
    /// its own loader, never mixed into the last one announced.
    #[test]
    fn two_documents_calls_stay_apart() {
        let graphql = "https://www.instagram.com/api/graphql";
        let mut t = traffic();
        t.feed(&sent_from(
            "L1",
            "1",
            "https://www.instagram.com/",
            "Document",
            None,
        ));
        t.feed(&sent_from(
            "L1",
            "2",
            graphql,
            "XHR",
            Some(&relay_form("SomeQuery", "{}", "abc123:def456:ghi789")),
        ));
        // No Document event for L2 was heard.
        t.feed(&sent_from(
            "L2",
            "3",
            graphql,
            "XHR",
            Some(&relay_form("SomeQuery", "{}", "abc123:def456:jkl012")),
        ));
        assert_eq!(
            web_session(&t, "L1").as_deref(),
            Some("abc123:def456:ghi789")
        );
        assert_eq!(
            web_session(&t, "L2").as_deref(),
            Some("abc123:def456:jkl012")
        );
    }

    /// A call that names no document is not taken: there is nothing to keep
    /// it by that would not risk mixing two.
    #[test]
    fn a_call_with_no_document_is_not_taken() {
        let mut t = traffic();
        let form = relay_form("SomeQuery", "{}", "abc123:def456:ghi789");
        t.feed(&sent_from(
            "",
            "1",
            "https://www.instagram.com/api/graphql",
            "XHR",
            Some(&form),
        ));
        t.feed(&sent_from(
            "",
            "2",
            "https://www.instagram.com/",
            "Document",
            None,
        ));
        assert!(!kept(&t, ""));
    }

    /// The variables of the registry's reads the app sends are kept, on the
    /// document and across documents; a name the registry does not hold is
    /// not.
    #[test]
    fn the_registrys_variables_are_kept_and_others_are_not() {
        use snob_ig::allowlist::Operation;

        let graphql = "https://www.instagram.com/api/graphql";
        let profile = Operation::ProfilePage;
        let mut t = traffic();
        let form = relay_form(
            profile.friendly_name(),
            r#"{"id":"1","flag":true}"#,
            "a:b:c",
        );
        t.feed(&sent_from("L1", "1", graphql, "XHR", Some(&form)));
        let other = relay_form("SomeQuery", r#"{"x":1}"#, "a:b:c");
        t.feed(&sent_from("L1", "2", graphql, "XHR", Some(&other)));
        let documents = t.shared.documents.lock().unwrap();
        let calls = &documents.get("L1").unwrap().calls;
        assert_eq!(
            calls.variables_sent(profile),
            Some(r#"{"id":"1","flag":true}"#)
        );
        assert_eq!(
            documents.template(profile),
            Some(r#"{"id":"1","flag":true}"#)
        );
        for operation in Operation::ALL {
            if operation != profile {
                assert_eq!(documents.template(operation), None, "{operation:?}");
            }
        }
    }

    /// Only the last few documents are kept; the variables the app sent
    /// outlive the document that sent them.
    #[test]
    fn templates_outlive_their_document() {
        use snob_ig::allowlist::Operation;

        let graphql = "https://www.instagram.com/api/graphql";
        let hover = Operation::HoverCard;
        let mut t = traffic();
        let form = relay_form(hover.friendly_name(), r#"{"id":"7"}"#, "a:b:c");
        t.feed(&sent_from("L0", "0", graphql, "XHR", Some(&form)));
        for n in 1..=KEPT_DOCUMENTS {
            t.feed(&sent_from(
                &format!("L{n}"),
                &n.to_string(),
                "https://www.instagram.com/",
                "Document",
                None,
            ));
        }
        assert!(!kept(&t, "L0"), "the oldest went");
        assert!(kept(&t, "L1") && kept(&t, &format!("L{KEPT_DOCUMENTS}")));
        let documents = t.shared.documents.lock().unwrap();
        assert_eq!(documents.template(hover), Some(r#"{"id":"7"}"#));
    }

    /// The app's route call on a document is what one built like it copies,
    /// with the counter after the app's; another document starts over, and
    /// snob's own calls are not the app's.
    #[test]
    fn the_apps_calls_on_a_document_are_copied() {
        use snob_core::secret::Secret;
        use snob_ig::page_values::PageValues;
        use snob_ig::web::{BULK_ROUTE_DEFINITIONS, Context};

        let page = PageValues {
            lsd: Some(Secret::new("lsd")),
            relay_dtsg: Some(Secret::new("relay:token")),
            ..PageValues::default()
        };
        let routes = |t: &Traffic, loader: &str| {
            let mut documents = t.shared.documents.lock().unwrap();
            let mut context = Context {
                page: &page,
                app: &mut documents.entry(loader).calls,
                csrf: None,
                claim: "0",
            };
            context.bulk_route_definitions(&["/someone/"], "/")
        };
        let address = format!("https://www.instagram.com{BULK_ROUTE_DEFINITIONS}");
        let form =
            "route_urls[0]=%2Fa%2F&routing_namespace=igx_www&__req=a&fb_dtsg=relay%3Atoken&lsd=lsd";

        let mut t = traffic();
        let mut own = sent_with("1", &address, "XHR", json!({}), Some(form));
        own.params["initiator"]["stack"]["callFrames"][0]["url"] = json!(OWN_SCRIPT);
        t.feed(&own);
        assert!(
            routes(&t, "L1").is_err(),
            "snob's own call is not the app's"
        );

        t.feed(&sent_with("2", &address, "XHR", json!({}), Some(form)));
        let built = routes(&t, "L1").unwrap();
        assert_eq!(built.field("routing_namespace"), Some("igx_www"));
        assert_eq!(built.field("__req"), Some("b"));

        let home = "https://www.instagram.com/";
        t.feed(&sent_from("L2", "3", home, "Document", None));
        assert!(routes(&t, "L2").is_err(), "a new document starts over");
        assert!(routes(&t, "L1").is_ok(), "and the old one keeps its own");
    }

    /// snob's own fetches are skipped: the page judges their answers itself.
    #[test]
    fn snobs_own_fetch_is_left_to_the_page() {
        let mut t = traffic();
        let url = "https://www.instagram.com/api/v1/friendships/1/followers/";
        t.feed(&sent("2", url, "Fetch", OWN_SCRIPT));
        assert!(is_nothing(&t.feed(&answered("2", 429, "application/json"))));
    }

    /// A document snob navigates to read is heard like any other, in the
    /// order the browser tells it: the request, then its answer, both before
    /// the navigation has finished loading.
    #[test]
    fn a_document_snob_reads_is_heard() {
        let mut t = traffic();
        t.feed(&sent(
            "3",
            "https://www.instagram.com/someone/",
            "Document",
            "",
        ));
        assert!(is_rate_limited(&t.feed(&answered("3", 429, "text/html"))));

        // Sent on to the challenge by a redirect, heard from the hop.
        t.feed(&sent(
            "4",
            "https://www.instagram.com/someone/",
            "Document",
            "",
        ));
        let hop = event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "4",
                "type": "Document",
                "request": { "url": "https://www.instagram.com/challenge/abc/" },
                "redirectResponse": { "status": 302 },
            }),
        );
        assert!(matches!(
            t.feed(&hop),
            Heard::PushBack(IgError::Challenge { .. })
        ));
    }

    /// The app's `eval`'d code has an empty script name too: it is the app's,
    /// and heard.
    #[test]
    fn an_empty_script_name_is_not_snobs() {
        let mut t = traffic();
        t.feed(&sent(
            "4",
            "https://www.instagram.com/graphql/query",
            "Fetch",
            "",
        ));
        assert!(is_rate_limited(&t.feed(&answered(
            "4",
            429,
            "application/json"
        ))));
    }

    /// A body is read only for a refusal or for JSON, and judged by the same
    /// rule the client applies to its own answers.
    #[test]
    fn a_body_is_read_only_for_a_candidate() {
        let mut t = traffic();
        let url = "https://www.instagram.com/api/v1/x/";
        t.feed(&sent(
            "5",
            url,
            "XHR",
            "https://static.cdninstagram.com/a.js",
        ));
        t.feed(&answered("5", 200, "application/json"));
        assert!(reads_body(&t.feed(&finished("5")), "5", 200, "/api/v1/x/"));
        t.feed(&sent(
            "6",
            url,
            "XHR",
            "https://static.cdninstagram.com/a.js",
        ));
        t.feed(&answered("6", 400, "text/html"));
        assert!(reads_body(&t.feed(&finished("6")), "6", 400, "/api/v1/x/"));
        t.feed(&sent(
            "7",
            url,
            "XHR",
            "https://static.cdninstagram.com/a.js",
        ));
        t.feed(&answered("7", 200, "text/html"));
        assert!(is_nothing(&t.feed(&finished("7"))));
    }

    /// Another host's calls and a stylesheet are nothing to do with this.
    #[test]
    fn another_host_or_another_kind_is_not_counted() {
        let mut t = traffic();
        t.feed(&sent(
            "8",
            "https://graph.facebook.com/logging",
            "XHR",
            "x.js",
        ));
        assert!(is_nothing(&t.feed(&answered("8", 429, "application/json"))));
        t.feed(&sent(
            "9",
            "https://www.instagram.com/static/a.css",
            "Stylesheet",
            "",
        ));
        assert!(is_nothing(&t.feed(&answered("9", 429, "text/css"))));
        // A sandbox's own site counts once it is the one the tab is on.
        *t.shared.site.lock().unwrap() = Some("127.0.0.1".to_string());
        t.feed(&sent(
            "10",
            "http://127.0.0.1:8080/api/v1/x/",
            "Fetch",
            "http://127.0.0.1:8080/",
        ));
        assert!(is_rate_limited(&t.feed(&answered(
            "10",
            429,
            "application/json"
        ))));
    }

    /// The app routing to the challenge is heard from the tab's address, with
    /// no request to see; the login form is a session gone and not this.
    #[test]
    fn a_route_to_the_challenge_is_heard_without_a_request() {
        let mut t = traffic();
        let moved = |url: &str, target: &str| {
            event(
                "Target.targetInfoChanged",
                json!({ "targetInfo": { "targetId": target, "url": url } }),
            )
        };
        assert!(matches!(
            t.feed(&moved(
                "https://www.instagram.com/challenge/abc/",
                "the-tab"
            )),
            Heard::PushBack(IgError::Challenge { .. })
        ));
        assert!(is_nothing(&t.feed(&moved(
            "https://www.instagram.com/challenge/abc/",
            "another-tab"
        ))));
        assert!(is_nothing(&t.feed(&moved(
            "https://www.instagram.com/accounts/login/",
            "the-tab"
        ))));
    }

    fn answer(status: u16, body: &str) -> PageResponse {
        PageResponse {
            status,
            body: body.to_string(),
            url: "https://www.instagram.com/api/v1/x/".to_string(),
            ..Default::default()
        }
    }

    /// snob's own answer is a push-back by the rule the client reads it by.
    #[test]
    fn snobs_own_push_back_is_judged_as_the_client_judges_it() {
        let asked = "https://www.instagram.com/api/v1/x/";
        assert!(pushed_back(asked, &answer(200, r#"{"status":"ok"}"#)).is_none());
        assert!(pushed_back(asked, &answer(404, "")).is_none());
        assert!(matches!(
            pushed_back(asked, &answer(429, "")),
            Some(IgError::RateLimited)
        ));
        let too_large = PageResponse {
            too_large: true,
            ..answer(429, "")
        };
        assert!(
            matches!(pushed_back(asked, &too_large), Some(IgError::RateLimited)),
            "a 429 too large to keep is still a 429"
        );
    }

    /// A document that landed on the challenge is a push-back the way the
    /// client reads it: only when it stayed on the site.
    #[test]
    fn a_landing_on_the_challenge_is_judged_as_the_client_judges_it() {
        let asked = "https://www.instagram.com/someone/";
        let landed = |url: &str| PageResponse {
            status: 200,
            redirected: true,
            url: url.to_string(),
            ..Default::default()
        };
        assert!(matches!(
            pushed_back(asked, &landed("https://www.instagram.com/challenge/x/")),
            Some(IgError::Challenge { .. })
        ));
        assert!(
            pushed_back(asked, &landed("https://example.com/challenge/x/")).is_none(),
            "off the site the client refuses it for another reason, and records nothing"
        );
        assert!(pushed_back(asked, &landed("https://www.instagram.com/accounts/login/")).is_none());
        match pushed_back(
            asked,
            &landed("https://www.instagram.com/challenge/x/?next=%2Fsomeone%2F"),
        ) {
            Some(IgError::Challenge { url: Some(url) }) => {
                assert!(
                    url.contains("?next="),
                    "the challenge keeps its query: {url}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// Collects what a subscriber writes, for a test to read back.
    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Written {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A push-back on snob's own request leaves the line a live push-back is
    /// investigated by: which endpoint, without its query, and what Instagram
    /// said and announced.
    #[test]
    fn snobs_own_push_back_is_noted_with_what_instagram_said() {
        let written = Written::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let written = written.clone();
                move || written.clone()
            })
            .with_ansi(false)
            .finish();
        let response = PageResponse {
            headers: vec![
                ("retry-after".into(), "60".into()),
                ("x-ig-capacity-level".into(), "2".into()),
            ],
            ..answer(429, r#"{"message":"Please wait a few minutes"}"#)
        };
        tracing::subscriber::with_default(subscriber, || {
            noted_own(
                "https://www.instagram.com/api/v1/x/?name=someone",
                &response,
            );
        });

        let line = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
        for field in [
            "path=/api/v1/x/",
            "status=429",
            "retry_after=\"60\"",
            "x-ig-capacity-level=2",
            "Please wait a few minutes",
        ] {
            assert!(line.contains(field), "{field} is missing from {line}");
        }
        assert!(!line.contains("someone"), "the query can carry a name");
    }

    #[test]
    fn the_first_to_claim_the_latch_records() {
        let latch = Latch::default();
        assert!(latch.claim(PushedBack::RateLimited));
        assert!(!latch.claim(PushedBack::FeedbackRequired));
        assert_eq!(latch.get(), Some(PushedBack::RateLimited));
    }

    /// A page that starts requests and never finishes them is followed only
    /// so far.
    #[test]
    fn what_is_followed_is_bounded() {
        let mut t = traffic();
        for n in 0..(FOLLOWED + 50) {
            t.feed(&sent(
                &n.to_string(),
                "https://www.instagram.com/api/",
                "XHR",
                "a.js",
            ));
        }
        assert_eq!(t.followed.len(), FOLLOWED);
        assert_eq!(t.order.len(), FOLLOWED);
    }
}
