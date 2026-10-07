//! The browser snob sends its requests from.
//!
//! **Why a browser.** A request sent with `reqwest` carries this process's TLS
//! handshake and HTTP/2 settings, cookies the login handed over and nothing
//! updates, and headers computed to agree with a Chrome this process is not.
//! A session a browser created, used by something that is visibly not that
//! browser, is the textbook stolen-session signature. So the requests come
//! from the browser itself: the Chrome, Edge, Brave or Chromium installed on
//! the machine, running without a window against snob's own profile — the
//! same profile the login happens in — and sending each request with
//! `fetch()` from an open instagram.com tab.
//!
//! There is a browser per account, each on the account's own profile,
//! started on the first request sent as that account. They run in the owner
//! of the browsers (`owner`), one process shared by every command of this
//! user's, which closes each a few minutes after its last request; a command
//! that cannot reach the owner, or is told `SNOB_NO_OWNER`, runs them itself,
//! closing each after the same idle time ([`alone`]) and the rest at its end
//! ([`crate::owner::finish`]). Nothing that spends no network starts one.
//!
//! **What a headless Chrome gives away, and what is done about each.** The
//! first four were measured against Chromium 141, the rest against Chromium
//! 153 on Linux ARM64, in September 2026, each from a script on a page and a
//! service worker the way a site would ask:
//!
//! - `navigator.webdriver` is `true` under the debugging pipe, headless or
//!   not. `--disable-blink-features=AutomationControlled`, in `cdp`.
//! - The User-Agent says `HeadlessChrome`. `--user-agent` with the launched
//!   browser's own; it is what covers the few requests that belong to no
//!   target, a service worker's script among them.
//! - That flag blanks the high-entropy client hints — architecture, bitness,
//!   platform version, full version list — which is worse than the name it
//!   hides. They are asked once of a browser started without it, and kept
//!   ([`machine_hints`]); the brands are the ones the browser reports for
//!   itself, so a Chromium does not claim to be Google Chrome.
//! - `setUserAgentOverride` holds for one target, and a worker or the
//!   service worker a site registers is a target of its own: the service
//!   worker called itself `HeadlessChrome`. Every target is paused as it
//!   attaches and handed the same override (`cdp::OnAttach`). Its requests
//!   carry no `Sec-CH-UA` either way — neither does a windowed Chromium's,
//!   measured under Xvfb, so that is Chromium and not a tell.
//! - `document.hasFocus()` was `false`: focus is emulated.
//! - The screen was all work area, `availHeight` equal to `height`, and the
//!   window ran off it from (10,10): a taskbar's strip is kept, and the window
//!   fills the rest from the corner.
//! - `(pointer: fine)` and `(hover: hover)` were both false, a phone's answer:
//!   a mouse is declared through `--blink-settings`.
//! - The requests ran in the page's main world, where a `fetch` the site
//!   wrapped and its resource timing list both see them; they run in an
//!   isolated world ([`tab::isolated_world`]).
//!
//! And two things that are not about looking like a browser but about what
//! a browser does to other people: the feed plays videos on its own, and a
//! play counts, so no video reaches the page; and the site's app sends calls
//! of its own, seen signals among them, so each of its API calls is judged
//! against the same allowlist as snob's before it leaves ([`guard`]).
//!
//! What is left is left knowingly: WebGL is absent — `getContext` returns
//! nothing without a GPU the browser will use headless — and the switch that
//! brings in the software renderer is one Chromium itself calls unsafe, and
//! would only name SwiftShader instead. Whether a headless browser on a
//! Windows desktop reaches the real GPU is to be measured there first.
//! `navigator.languages` and `Accept-Language` are the profile's own, which is
//! what the login sent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::allowlist::{Operation, Rediscovered};
use snob_ig::client::page::{Method, PageError, PageRequest, PageResponse, PushedBack};
use snob_ig::model::web::{RouteAnswer, document_pk, route_answer};
use snob_ig::pace::CancelToken;
use snob_ig::web::{Ask, Call, Seen, SentToken, Told, Unbuilt};
use snob_store::paths::{AccountPaths, AppPaths};

use crate::cdp::Cdp;

mod guard;
mod identity;
mod listen;
pub(crate) mod profile;
mod tab;
pub(crate) mod write_back;

use identity::{machine_hints, metadata};
pub(crate) use profile::ProfileMark;
use profile::sync_cookies;
use tab::{fetch, navigate, navigate_and_read};

/// How long a navigation may take to finish loading.
const LOAD_TIMEOUT: Duration = Duration::from_secs(45);

/// How long after a page finishes loading before the first request is sent
/// from it: the app bootstraps after `load`, and a request from a page that
/// has not started its own is not what a person's first click looks like.
const SETTLE: Duration = Duration::from_millis(1_500);

/// How long a single browser command may take when it is not a fetch: what
/// any browser command gets.
const COMMAND_TIMEOUT: Duration = crate::cdp::CALL_TIMEOUT;

/// How long a browser is kept after its last request, whether or not a
/// command that used it is still connected: an interactive view left open
/// does not hold a browser for as long as it stays open. Until then a browser
/// outlives the commands that used it, which is what spares the next a cold
/// start. A few minutes, since memory is what a browser costs.
pub(crate) const IDLE: Duration = Duration::from_secs(5 * 60);

/// How often a process with no owner looks again at a profile another snob's
/// browser holds.
const PROFILE_RETRY: Duration = Duration::from_secs(5);

/// How long a call waits for the app's own calls on the tab's document to
/// carry what it copies from them, which they do as the document boots:
/// the app sends its first ones within a second or two of the load. Past
/// it the call is not built, and the page is not ready.
const APP_CALLS_PATIENCE: Duration = Duration::from_secs(8);

/// How often a call waiting on the app's calls looks again.
const APP_CALLS_POLL: Duration = Duration::from_millis(250);

/// How long the app's calls on a profile document pause before a write is
/// built beside it: a document's boot calls leave within about a second of
/// the load and of each other in the captures of the real app, and a
/// person's click comes after them.
const APP_CALLS_QUIET: Duration = Duration::from_secs(1);

/// The most a page may hand back, measured the way it travels: as a protocol
/// message on the pipe, where every character outside printable ASCII is six
/// bytes (`\uXXXX`) and a quote or backslash two. Two megabytes short of the
/// pipe's ceiling, for the envelope. A message over that ceiling is dropped on
/// the way in and fails its request as a browser failure, so the cap is
/// applied in the page, before anything is sent back: an answer too large then
/// arrives as `too_large`, which the client reads as what it is.
const PAGE_WIRE_CAP: u64 = (crate::pipe::MAX_MESSAGE_BYTES - 2 * 1024 * 1024) as u64;

/// How much of the app's answers the browser keeps for the listener to read:
/// enough for a page's API calls, which are what it judges, and no more.
const LISTENING_BUFFER: u64 = 1024 * 1024;

/// The screen the browser says it is on: the commonest desktop size.
const SCREEN: (u32, u32) = (1920, 1080);

/// The strip at the bottom of that screen a taskbar keeps for itself.
///
/// A headless screen is all work area, so `screen.availHeight` equaled
/// `screen.height` — measured, 1080 of 1080 — which no desktop with a taskbar
/// or a dock reports. The window fills what is left, from the corner, rather
/// than sitting at (10,10) and running off the bottom edge.
const TASKBAR: u32 = 48;

/// The browsers of this process, once something has asked for them.
static HEADLESS: OnceLock<Arc<Headless>> = OnceLock::new();

/// The browsers this process runs itself, made on first use.
pub(crate) fn local(paths: &AppPaths) -> Arc<Headless> {
    HEADLESS
        .get_or_init(|| Arc::new(Headless::new(paths.clone())))
        .clone()
}

/// The browsers of a process with no owner to send through
/// (`owner::Remote::send_as` sends through this when it has none), each closed once it has been idle
/// for [`IDLE`]: a walk asleep on the day's budget would otherwise hold its
/// profile against every other command for hours. What a browser rotated
/// stays in its profile, where the next one reads it.
///
/// **The reaper wakes when there is something to close, not on a timer.** It
/// sleeps until the first open browser would be idle for [`IDLE`], and with no
/// browser open it sleeps until one starts. Looked at once a second instead, a
/// `snob watch` running without an owner woke eighty-six thousand times a day
/// for a browser that is open a few minutes of it, while the monitor's own
/// loop goes out of its way to wake a few hundred times
/// (`commands::watch::scheduled::nap_for`).
///
/// Such a process waits for a profile another snob's browser holds, rather
/// than failing: with no owner there is nobody to share that browser through.
pub(crate) fn alone(paths: &AppPaths) -> Arc<Headless> {
    static REAPING: std::sync::Once = std::sync::Once::new();
    let headless = local(paths);
    headless.waits.store(true, Ordering::SeqCst);
    REAPING.call_once(|| {
        let reaped = Arc::clone(&headless);
        tokio::spawn(async move {
            loop {
                match reaped.reap(|open| open.idle >= IDLE).await.next {
                    Some(wait) => tokio::time::sleep(wait).await,
                    None => reaped.started.notified().await,
                }
            }
        });
    });
    headless
}

/// What `mutex` guards, whether or not a thread panicked holding it: every
/// value kept behind one here stays whole whatever panicked.
pub(super) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a switch in the environment is on: set, and neither empty nor `0`.
/// `SNOB_NO_BROWSER` and `SNOB_NO_OWNER` are both read this way, so a unit
/// file that writes `=0` to be explicit gets what it wrote.
pub fn env_flag(name: &str) -> bool {
    is_on(std::env::var_os(name).as_deref())
}

fn is_on(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// Closes the account's browser, or every one, among those this process runs
/// itself, if any were started. See `owner::release`.
pub(crate) async fn release_own(pk: Option<Pk>) {
    if let Some(headless) = HEADLESS.get() {
        headless.release(pk).await;
    }
}

/// Closes the browsers this process runs itself, if any were started, the
/// polite way — a browser that is killed can lose the cookies it rotated in
/// the last half minute — and hands back what each held, for
/// [`write_back::keep`] to store.
pub(crate) async fn close_own() -> Vec<write_back::Rotated> {
    match HEADLESS.get() {
        Some(headless) => headless.close().await,
        None => Vec::new(),
    }
}

/// The browsers, each started on first use.
pub(crate) struct Headless {
    paths: AppPaths,
    /// A browser per account, each on the account's own profile and behind a
    /// lock of its own: one request at a time on a tab — a tab is not a
    /// connection pool, and the pacing never asks for two at once anyway —
    /// and one account's slow start never holds up another's requests.
    accounts: std::sync::Mutex<HashMap<Pk, Arc<Account>>>,
    /// Told the first push-back each browser hears, and for which account.
    /// The owner passes it on to every command it serves.
    heard: tokio::sync::broadcast::Sender<(Pk, PushedBack)>,
    /// Set by [`alone`]: a profile another snob holds is waited for.
    waits: AtomicBool,
    /// Poked when a browser starts, for [`alone`]'s reaper asleep with none
    /// open. `notify_one`, so a start that comes between the reaper looking
    /// and the reaper waiting is kept for it rather than lost.
    started: tokio::sync::Notify,
}

/// One account's browser, while it has one.
#[derive(Default)]
struct Account {
    live: tokio::sync::Mutex<Option<Live>>,
    /// Its listener's share, where `Page::heard` can reach it without
    /// waiting on a request in flight. Emptied when the browser closes: what
    /// it heard is recorded, and a browser started after is not stopped by it.
    listened: std::sync::Mutex<Option<Arc<listen::Shared>>>,
    /// When it last finished a request, or started.
    used: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Account {
    fn listened(&self) -> Option<Arc<listen::Shared>> {
        lock(&self.listened).clone()
    }

    fn touch(&self) {
        *lock(&self.used) = Some(std::time::Instant::now());
    }

    fn idle(&self) -> Duration {
        lock(&self.used).map_or(Duration::MAX, |at| at.elapsed())
    }

    /// Closes the browser in `slot`, if there is one, and forgets what its
    /// listener heard.
    async fn close(&self, slot: &mut Option<Live>) {
        if let Some(live) = slot.take() {
            live.cdp.close().await;
        }
        *lock(&self.listened) = None;
    }
}

/// What [`Headless::reap`] leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reaped {
    /// The browsers still open, busy ones included.
    pub(crate) open: usize,
    /// How long until the first of them has been idle for [`IDLE`], when one
    /// is open: the next moment there may be something to close. A busy one
    /// counts as a whole [`IDLE`] away, since it is touched again when its
    /// request ends.
    pub(crate) next: Option<Duration>,
}

/// An open browser, as the owner decides whether to close it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Open {
    pub(crate) pk: Pk,
    /// How long since its last request finished.
    pub(crate) idle: Duration,
    /// Instagram has pushed back on it.
    pub(crate) latched: bool,
}

struct Live {
    cdp: Cdp,
    /// The profile it runs on: the account's own.
    profile: std::path::PathBuf,
    /// The DevTools session of the tab the requests are sent from.
    tab: String,
    /// The origin the tab is on. `about:blank` until the first request.
    origin: String,
    /// The isolated world the requests are sent from, with the loader of the
    /// document it was made in; `None` until one is made. See
    /// [`tab::isolated_world`].
    world: Option<(String, i64)>,
    /// The values the document that world was made in carries, read with it.
    page: snob_ig::page_values::PageValues,
    /// Whether its cookies have been checked ([`sync_cookies`]): from then on
    /// the mark names the session the browser holds ([`Live::held`]).
    checked: bool,
    /// What is known about the profile; see [`ProfileMark`].
    mark: ProfileMark,
    /// What the listener to the app's traffic shares with the requests sent
    /// from the same tab; see [`listen`].
    listened: Arc<listen::Shared>,
    /// Where a push-back heard on it is recorded: the account's own.
    account: AccountPaths,
    /// The loader of the last document whose token the owner log named
    /// ([`Live::build`]), so that each document's is named once.
    token_named: Option<String>,
    /// The document the last [`Ask::Document`] loaded, by its loader, and
    /// the path it was asked for: the only one a write may be built on
    /// ([`Live::build`]).
    asked_document: Option<(String, String)>,
}

impl Live {
    /// The session the browser holds, by its fingerprint, once its cookies
    /// have been checked: the one it was handed, a later login it kept
    /// instead ([`sync_cookies`]), or what a write-back stored from it. A new
    /// login for the same account is a different session, and has to reach a
    /// browser that is still open; the mark's `made` is how two logins of one
    /// account are told apart when two commands hand them in turn.
    fn held(&self) -> Option<&str> {
        self.checked.then_some(())?;
        self.mark.session.as_deref()
    }

    /// What the browser holds for the site it was sending to: read before it
    /// closes, for [`write_back`]. Nothing for a browser that never reached a
    /// site or was never handed a session.
    ///
    /// The site is the one the requests were sent to, not wherever the tab
    /// ended up: a tab stopped on `about:blank` after a push-back, or one a
    /// read left off the site, still holds that site's session.
    async fn cookies(&self) -> Option<snob_ig::login::BrowserCookies> {
        self.held()?;
        let host = lock(&self.listened.site).clone()?;
        let cookies = self
            .cdp
            .cookies()
            .await
            .map_err(|e| tracing::debug!(error = %e, "could not read the browser's cookies"))
            .ok()?;
        crate::cdp::collect_for(&cookies, &host)
    }

    /// Whether the browser holds exactly `session`'s `sessionid`.
    async fn holds(&self, session: &Session) -> bool {
        self.cookies()
            .await
            .is_some_and(|jar| jar.sessionid.expose() == session.sessionid.expose())
    }

    /// The same, with the session it was handed, for this process's own
    /// write-back.
    async fn jar(&self, pk: Pk) -> Option<write_back::Rotated> {
        let handed = self.held()?.to_string();
        let jar = self.cookies().await?;
        Some(write_back::Rotated::new(pk, handed, jar))
    }

    /// The values the tab's current document carries, with the web session
    /// id its app last sent from that document: what a request built like
    /// the app's is built from.
    ///
    /// **The document is checked first**, as for a request sent: the app can
    /// load another one by itself, and values read from the old document
    /// would go out beside the new one's calls. A new document's values are
    /// read here, and the app's calls are taken from the entry of the loader
    /// it came from; one whose app has sent nothing yet has no entry, and a
    /// call that needs one is `Missing`.
    async fn page_values(&mut self) -> Result<snob_ig::page_values::PageValues, PageError> {
        tab::isolated_world(self).await?;
        let mut values = self.page.clone();
        values.web_session = self.world.as_ref().and_then(|(loader, _)| {
            lock(&self.listened.documents)
                .get(loader)
                .and_then(|document| document.web_session.clone())
        });
        Ok(values)
    }

    /// The loader of the document the tab is on, as the world was last made.
    fn loader(&self) -> String {
        self.world
            .as_ref()
            .map(|(loader, _)| loader.clone())
            .unwrap_or_default()
    }

    /// Whether the document `loader` names is the one the last
    /// [`Ask::Document`] loaded at `path`.
    fn made_from(&self, loader: &str, path: &str) -> bool {
        self.asked_document
            .as_ref()
            .is_some_and(|(asked, at)| !loader.is_empty() && asked == loader && at == path)
    }

    /// The request `call` asks for, built as `session` from the tab's
    /// current document: its values, the app's calls on it, and what the
    /// app last sent with the operation asked, from any document
    /// ([`snob_ig::web::build_call`]): its variables, whose flags are copied,
    /// and the number it sent the read under.
    ///
    /// **A value only the app's calls carry is waited for**, up to
    /// [`APP_CALLS_PATIENCE`]: the app sends them as its document boots, a
    /// moment after the load. Each try reads the document again, so a new
    /// one the app loaded is built from with its own calls. Past the
    /// patience, and at once for any other gap, nothing is built and the
    /// page is not ready — never a browser failure, which would close it.
    ///
    /// **A write is built only on the document it is made from**: the one
    /// the last [`Ask::Document`] loaded at `call`'s referrer, still the
    /// tab's. Its route, its referrer and the cold-start navigation chain
    /// describe that document, so one built on any other would carry values
    /// the document it names never gave. The tab moves between the two asks
    /// when another command on the account loads a page, when a newer login
    /// reloads the site, or when an idle browser is closed and the next one
    /// starts on the home page; the write is then not built, and nothing is
    /// sent.
    ///
    /// **A read is made from the page the tab is on**: the queries the app
    /// sends about an account it is not showing (a profile's, a hover
    /// card's, the story viewer's) go out as it sends them, from the page it
    /// is on, on that page's route, so the `Referer` and `__crn` agree with
    /// where the tab really is rather than with a page it never went to
    /// ([`Live::made_from_where_the_tab_is`]).
    ///
    /// **What the live check asks is said in the owner log**, at debug:
    /// once per document, which of its two tokens the app's Relay and Comet
    /// calls carry; and for a query built, whether it took the app's
    /// variables or its own, and which keys its variables and the app's for
    /// the same operation on this document each lack. Names only: never a
    /// token, a form or a variable's value.
    async fn build(&mut self, session: &Session, call: &Call) -> Result<PageRequest, PageError> {
        let deadline = tokio::time::Instant::now() + APP_CALLS_PATIENCE;
        loop {
            let page = self.page_values().await?;
            let loader = self.loader();
            if matches!(call.ask, Ask::Write { .. }) && !self.made_from(&loader, &call.referrer) {
                tracing::debug!("a write asked on a document it was not made from");
                return Err(PageError::NotReady(
                    "the profile this write is made from".into(),
                ));
            }
            let call = self.made_from_where_the_tab_is(call).await;
            // A std lock, released before anything is awaited.
            let (built, token, apps) = {
                let mut documents = lock(&self.listened.documents);
                let asked = match &call.ask {
                    Ask::Query { operation, .. } => Some(*operation),
                    _ => None,
                };
                let variables = asked
                    .and_then(|operation| documents.template(operation))
                    .map(str::to_string);
                let doc_id = asked
                    .and_then(|operation| documents.rediscovered().get(operation))
                    .map(str::to_string);
                let rest_token = documents.rest_token().map(str::to_string);
                let document = documents.entry(&loader);
                let token = document.calls.sent_token(&page);
                let built = snob_ig::web::build_call(
                    &call,
                    &page,
                    &mut document.calls,
                    session.ds_user_id,
                    &Seen {
                        variables: variables.as_deref(),
                        doc_id: doc_id.as_deref(),
                        rest_token: rest_token.as_deref(),
                    },
                );
                let apps = match &call.ask {
                    Ask::Query { operation, .. } => document
                        .calls
                        .variables_sent(*operation)
                        .map(str::to_string),
                    _ => None,
                };
                (built, token, apps)
            };
            if token != SentToken::NotYet && self.token_named.as_deref() != Some(&loader) {
                tracing::debug!(
                    ?token,
                    "the token the app's Relay and Comet calls carry on this document"
                );
                self.token_named = Some(loader);
            }
            match built {
                Err(Unbuilt::Missing(missing))
                    if missing.waits_on_the_app() && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(APP_CALLS_POLL).await;
                }
                Ok(request) => {
                    say_the_variables(&call, &request, apps.as_deref());
                    return Ok(request);
                }
                built => return built.map_err(PageError::from),
            }
        }
    }

    /// The numbers the app was seen to send the registry's reads under:
    /// what a request built from them is held to the allowlist with.
    fn rediscovered(&self) -> Rediscovered {
        lock(&self.listened.documents).rediscovered().clone()
    }

    /// `call`, made from the page the tab is on ([`snob_ig::web::made_from`]),
    /// which is read from the tab itself: a page that could not be read
    /// leaves the call as it was asked.
    async fn made_from_where_the_tab_is(&mut self, call: &Call) -> Call {
        let Some(path) = tab::path_of(self).await else {
            return call.clone();
        };
        let made = snob_ig::web::made_from(call, &path);
        if made.referrer != call.referrer {
            tracing::debug!(
                "a read is made from the page the tab is on, not the one it was asked from"
            );
        }
        made
    }
}

/// Says in the owner log, for a query built as `request`, whether it took
/// the app's variables, with only what is asked made snob's, or its own; and,
/// when the app sent the same operation on the document (`apps`), which keys
/// each side's variables lack. Names only.
fn say_the_variables(call: &Call, request: &PageRequest, apps: Option<&str>) {
    let Ask::Query {
        operation,
        variables: asked,
    } = &call.ask
    else {
        return;
    };
    let Some(ours) = request.body.as_deref().and_then(|body| {
        url::form_urlencoded::parse(body.as_bytes())
            .find(|(name, _)| name == "variables")
            .map(|(_, value)| value.into_owned())
    }) else {
        return;
    };
    let operation = operation.friendly_name();
    let the_apps = ours != *asked;
    tracing::debug!(
        operation,
        the_apps,
        "a query's variables: the app's flags, or snob's own"
    );
    if let Some((ours_lack, apps_lack)) =
        apps.and_then(|apps| snob_ig::web::keys_each_lacks(&ours, apps))
    {
        tracing::debug!(
            operation,
            ?ours_lack,
            ?apps_lack,
            "the keys snob's variables and the app's each lack"
        );
    }
}

impl Headless {
    /// Browsers of a test's own, apart from this process's.
    #[cfg(test)]
    pub(crate) fn apart(paths: &AppPaths) -> Arc<Self> {
        Arc::new(Self::new(paths.clone()))
    }

    fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            accounts: std::sync::Mutex::new(HashMap::new()),
            heard: tokio::sync::broadcast::Sender::new(16),
            waits: AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
        }
    }

    fn account(&self, pk: Pk) -> Arc<Account> {
        Arc::clone(lock(&self.accounts).entry(pk).or_default())
    }

    fn every_account(&self) -> Vec<(Pk, Arc<Account>)> {
        lock(&self.accounts)
            .iter()
            .map(|(pk, account)| (*pk, Arc::clone(account)))
            .collect()
    }

    /// Every push-back heard from now on, with its account.
    pub(crate) fn hear(&self) -> tokio::sync::broadcast::Receiver<(Pk, PushedBack)> {
        self.heard.subscribe()
    }

    /// What the account's browser has heard, if it is open and heard anything.
    pub(crate) fn heard_for(&self, pk: Pk) -> Option<PushedBack> {
        let account = lock(&self.accounts).get(&pk).cloned()?;
        account.listened()?.latch.get()
    }

    async fn close(&self) -> Vec<write_back::Rotated> {
        let mut held = Vec::new();
        for (pk, account) in self.every_account() {
            let mut slot = account.live.lock().await;
            if let Some(live) = slot.as_ref()
                && let Some(rotated) = live.jar(pk).await
            {
                held.push(rotated);
            }
            account.close(&mut slot).await;
        }
        held
    }

    /// What the account's browser holds, for the command that is leaving to
    /// write back: nothing unless the browser holds the session that command
    /// handed it, by its fingerprint, since only that one may replace what is
    /// stored ([`write_back`]). Waits for a request in flight.
    pub(crate) async fn cookies_of(
        &self,
        pk: Pk,
        handed: &str,
    ) -> Option<snob_ig::login::BrowserCookies> {
        let account = self.account(pk);
        let slot = account.live.lock().await;
        let live = slot.as_ref()?;
        if live.held() != Some(handed) {
            return None;
        }
        live.cookies().await
    }

    /// Closes the account's browser, or every one, once the request in flight
    /// on it is answered.
    pub(crate) async fn release(&self, pk: Option<Pk>) {
        for (each, account) in self.every_account() {
            if pk.is_none_or(|pk| pk == each) {
                let mut slot = account.live.lock().await;
                account.close(&mut slot).await;
            }
        }
    }

    /// Closes the open browsers `due` says to, never one answering a request,
    /// and says how many are still open, busy ones included, and when the
    /// first of them idles out.
    pub(crate) async fn reap(&self, due: impl Fn(&Open) -> bool) -> Reaped {
        let mut open = 0;
        let mut next: Option<Duration> = None;
        let mut sooner = |wait: Duration| next = Some(next.map_or(wait, |next| next.min(wait)));
        for (pk, account) in self.every_account() {
            let Ok(mut slot) = account.live.try_lock() else {
                open += 1;
                sooner(IDLE);
                continue;
            };
            let Some(live) = slot.as_ref() else {
                continue;
            };
            let state = Open {
                pk,
                idle: account.idle(),
                latched: live.listened.latch.get().is_some(),
            };
            if due(&state) {
                tracing::debug!(account = %pk, idle = ?state.idle, latched = state.latched, "closing a browser");
                account.close(&mut slot).await;
            } else {
                open += 1;
                sooner(IDLE.saturating_sub(state.idle));
            }
        }
        Reaped { open, next }
    }

    /// Sends `request` as `session`, from the account's own browser
    /// ([`Self::on_ready_tab`]).
    ///
    /// **A navigation is never a plain request.** The tab goes to a document
    /// only as an [`Ask::Document`] or an [`Ask::Pk`], whose answers carry no
    /// HTML, so no document leaves this process; one sent as a request is
    /// refused before a browser is started for it, and nothing is sent.
    pub(crate) async fn send_as(
        &self,
        session: &Session,
        request: PageRequest,
    ) -> Result<PageResponse, PageError> {
        if request.navigate {
            tracing::error!("refused a navigation sent as a request");
            return Err(PageError::NotAllowed(
                "a navigation is asked as a document".into(),
            ));
        }
        let origin = origin_of(&request.url).map_err(broken)?;
        self.on_ready_tab(session, &origin, async |live, world| {
            self.send_on(live, &request, world).await
        })
        .await
    }

    /// Answers `call` as `session`, from the account's own browser
    /// ([`Self::on_ready_tab`]): the request it asks for built in the tab
    /// beside the page's values and sent, or what the tab's document says
    /// itself ([`answer_on`]). A page that is not ready leaves the browser
    /// as it is.
    ///
    /// **The intent is checked here again**, before a browser is started for
    /// it: whatever handed it over, the owner's socket included, it is held
    /// to the allowlist by the process that would send it.
    pub(crate) async fn ask_as(&self, session: &Session, call: Call) -> Result<Told, PageError> {
        if let Some(what) = snob_ig::allowlist::refused_ask(&call.ask) {
            tracing::error!(request = %what, "refused a call snob may not send");
            return Err(PageError::NotAllowed(what));
        }
        let origin = origin_of(&call.origin).map_err(broken)?;
        self.on_ready_tab(session, &origin, async |live, _| {
            let told = answer_on(live, session, &call).await;
            // Whatever failed once a push-back was heard, the push-back is
            // the answer, as in `send_on`.
            told.map_err(|e| live.listened.latch.get().map_or(e, PageError::PushedBack))
        })
        .await
    }

    /// Runs `step` on the account's own browser as `session`, once its tab
    /// is ready on `origin` ([`Self::ready`]), with the world a fetch is
    /// sent from.
    ///
    /// A browser started for this step that fails before the step runs is
    /// closed and started once more: a fresh browser can stall on a profile
    /// the helpers of the one before it still hold, and nothing has been
    /// sent yet. Not once Instagram has pushed back on it: that failure is
    /// the push-back, and nothing more is sent. The step itself runs once,
    /// whatever happens: a write would go out twice.
    async fn on_ready_tab<T>(
        &self,
        session: &Session,
        origin: &str,
        step: impl AsyncFnOnce(&mut Live, i64) -> Result<T, PageError>,
    ) -> Result<T, PageError> {
        let account = self.account(session.ds_user_id);
        let mut slot = account.live.lock().await;
        let mut starts = 0;
        let ready = loop {
            if slot.is_none() {
                let started = self
                    .start_when_free(session, &account)
                    .await
                    .map_err(broken)?;
                starts += 1;
                account.touch();
                *slot = Some(started);
                self.started.notify_one();
            }
            let live = slot.as_mut().expect("a browser was started above");
            let ready = self.ready(live, session, origin).await;
            // Whatever failed once a push-back was heard, the push-back is the
            // answer, as in `send_on`.
            let ready =
                ready.map_err(|e| live.listened.latch.get().map_or(e, PageError::PushedBack));
            match ready {
                Err(PageError::Browser(e)) if starts == 1 => {
                    tracing::warn!(
                        account = %session.ds_user_id,
                        error = %e,
                        "a browser just started failed before sending; starting another"
                    );
                    account.close(&mut slot).await;
                }
                ready => break ready,
            }
        };
        let result = match ready {
            Ok(world) => {
                let live = slot.as_mut().expect("the tab was made ready above");
                step(live, world).await
            }
            Err(e) => Err(e),
        };
        account.touch();
        if let Err(PageError::Browser(_)) = &result {
            // A browser that failed once is not trusted with the next request:
            // it may be gone. The next request starts a fresh one. Only then —
            // a request the network dropped says nothing about the browser,
            // and relaunching it would load the site again for nothing.
            account.close(&mut slot).await;
        }
        result
    }

    /// Everything before a request itself is written: the session in the
    /// browser, the tab on `origin`, and the world a fetch is sent from.
    async fn ready(
        &self,
        live: &mut Live,
        session: &Session,
        origin: &str,
    ) -> Result<i64, PageError> {
        // Nothing more is sent from a browser Instagram has pushed back on.
        if let Some(cause) = live.listened.latch.get() {
            return Err(PageError::PushedBack(cause));
        }
        let origin = origin.to_string();
        let given = session.fingerprint();
        let held_since = live.checked.then(|| live.mark.made.unwrap_or_default());
        let holds_it = live.held() == Some(given.as_str());
        if !holds_it && newer_login(held_since, session) {
            let mut mark = live.mark.clone();
            let carried = sync_cookies(live, session, &origin, &mut mark)
                .await
                .map_err(broken)?;
            if mark != live.mark {
                mark.write(&live.profile);
                live.mark = mark;
            }
            // Not carried, it keeps the later login its mark names, and this
            // session is not written in again.
            if carried {
                // A tab already on the site loaded as whoever was there before.
                live.origin = String::new();
            }
            live.checked = true;
        } else if !holds_it && live.holds(session).await {
            // What a write-back stored from this browser comes back as a
            // session of its own, made when the one it was handed was. The
            // browser holds it already, and it is the one a write-back may
            // replace from now on. Asked of the cookies at each request of a
            // command whose session the browser does not hold.
            live.mark.handed(session);
            live.mark.write(&live.profile);
        }
        if live.origin != origin {
            // The site the listener counts, when it is not Instagram's own.
            *lock(&live.listened.site) = url::Url::parse(&origin)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string));
            navigate(live, &format!("{origin}/")).await?;
            // What the first load handed this profile, for the live check:
            // device cookies are never made up, only kept and noted.
            if let (Ok(jar), Some(host)) = (live.cdp.cookies().await, host_of(&origin)) {
                let (held, missing) = profile::device_cookies(&jar, &host);
                tracing::debug!(?held, ?missing, "the device cookies the profile holds");
            }
            live.origin = origin;
            // The app has been running on the page since it loaded, and may
            // have been told no in the meantime.
            if let Some(cause) = live.listened.latch.get() {
                return Err(PageError::PushedBack(cause));
            }
        }
        tab::isolated_world(live).await
    }

    /// Writes `request` from `world`, and reads its answer.
    ///
    /// Held to the allowlist here as well, whoever handed it over: the
    /// client's page checks it, and a request that reaches the owner some
    /// other way is checked by the process that holds the browser.
    async fn send_on(
        &self,
        live: &mut Live,
        request: &PageRequest,
        world: i64,
    ) -> Result<PageResponse, PageError> {
        refuse(request, &Rediscovered::default())?;
        match fetch(live, world, request).await {
            Ok(response) => own_answer(live, request, response).await,
            // A failure while the listener stopped the tab under it is that
            // push-back, not the network's.
            Err(e) => Err(live.listened.latch.get().map_or(e, PageError::PushedBack)),
        }
    }

    /// Starts the browser, in a process with no owner once no other snob's
    /// holds the profile: looked at again every [`PROFILE_RETRY`], until
    /// Ctrl+C. Said once per wait.
    async fn start_when_free(&self, session: &Session, account: &Account) -> Result<Live> {
        let mut told = false;
        loop {
            let error = match self.start(session, account).await {
                Err(e) if self.waits.load(Ordering::SeqCst) => e,
                started => return started,
            };
            if error.downcast_ref::<crate::cdp::ProfileInUse>().is_none() {
                return Err(error);
            }
            if !told {
                crate::ui::info(&format!(
                    "Another snob is using {}'s browser; waiting for it to finish (Ctrl+C to stop)",
                    crate::app::label(session.ds_user_id, session.username.as_deref())
                ));
                told = true;
            }
            if crate::interrupt::install()
                .sleep_or_cancel(PROFILE_RETRY)
                .await
            {
                return Err(error);
            }
        }
    }

    /// Starts the browser. A second snob already running one on this profile
    /// makes Chrome hand over to it and exit, which `Cdp::connect` recognizes
    /// and says in so many words.
    async fn start(&self, session: &Session, account: &Account) -> Result<Live> {
        let (profile, _) = profile::for_account(&self.paths, session.ds_user_id)?;
        let mut mark = ProfileMark::read(&profile);
        // The browser that made the profile, while it is still installed; then
        // the one the session names; then the first found. See `ProfileMark`
        // for why the first answer is the one that matters.
        let made_it = mark.browser.as_deref().and_then(|path| {
            crate::browser::detect_all()
                .into_iter()
                .find(|b| b.path == path)
        });
        let browser = made_it
            .or_else(|| {
                session
                    .browser
                    .as_deref()
                    .and_then(crate::browser::detect_named)
            })
            .or_else(crate::browser::detect)
            .ok_or_else(|| {
                anyhow!(
                    "snob sends its requests from a browser, and no Chrome, Edge, Brave or \
                     Chromium was found on this machine.\n\
                     Install Chrome, Edge or Brave (or Chromium on Linux), or set \
                     SNOB_NO_BROWSER=1 to send them directly — which Instagram can tell \
                     apart from a browser."
                )
            })?;
        refuse_root()?;
        // A profile nothing has claimed yet, or whose browser is gone: from
        // here on it is this one's. Written once the launch has created the
        // directory it lives in.
        let claim = mark.browser.as_deref() != Some(browser.path.as_path());
        mark.browser = Some(browser.path.clone());
        // The User-Agent of the binary being started, never the one stored
        // with the session, pinned at login or not. The brands below are what
        // this binary reports, and a stored string would put another version
        // or system in the User-Agent than in every client hint beside it. A
        // pinned one is for the requests sent without a browser.
        let user_agent = browser.user_agent();

        // What this browser says about the machine, asked of it once without
        // `--user-agent` (which blanks it) and kept. See `machine_hints`.
        let hints = machine_hints(&browser, &self.paths).await;

        let flags = vec![
            "--headless=new".to_string(),
            // Kept although every target is overridden below: a few requests
            // belong to no target — the script of a service worker is one — and
            // without it they would name `HeadlessChrome`.
            format!("--user-agent={user_agent}"),
            format!(
                "--screen-info={{0,0 {}x{} workAreaBottom={TASKBAR}}}",
                SCREEN.0, SCREEN.1
            ),
            "--window-position=0,0".to_string(),
            format!("--window-size={},{}", SCREEN.0, SCREEN.1 - TASKBAR),
            // A headless browser has no pointer: `(pointer: fine)` and
            // `(hover: hover)` were both false, which is a phone's answer on a
            // desktop's screen. A mouse is what the machine this claims to be
            // has.
            "--blink-settings=primaryPointerType=4,availablePointerTypes=4,\
             primaryHoverType=2,availableHoverTypes=2"
                .to_string(),
        ];
        let cancel = CancelToken::default();
        let launched = crate::cdp::launch_headless(&browser, &profile, &flags)
            .with_context(|| format!("could not start {} without a window", browser.name))?;
        let cdp = Cdp::connect(launched, &cancel).await?;
        if claim {
            mark.write(&profile);
        }

        let version = cdp.browser_call("Browser.getVersion", json!({})).await?;
        let full_version = version
            .get("product")
            .and_then(Value::as_str)
            .and_then(|product| product.split('/').nth(1))
            .unwrap_or_default()
            .to_string();
        // The build that runs, which is not always the one detection read: an
        // update waiting for the browser to close puts the new version beside
        // the old, and the old one is what starts — and what answered the
        // probe. The User-Agent states the running build's major version, as
        // every client hint beside it does; `--user-agent` above, which only
        // the few requests of no target carry, keeps the detected one.
        let running = full_version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .unwrap_or(browser.major_version);
        let user_agent = browser.user_agent_at(running);

        // Attached before the plan below is set, so the dispatcher only
        // registers the tab: the calls after the plan are the one place the
        // tab is given its identity.
        let Tab {
            session: tab,
            target: tab_target,
        } = attach_to_a_tab(&cdp).await?;

        // **Every target, not only the tab.** A worker, a frame and the
        // service worker the site registers are targets of their own, and
        // `setUserAgentOverride` holds for the one it was sent to; measured,
        // a service worker's requests carried no client hints at all. With
        // auto-attach each one is paused before it runs and handed the same
        // override (`cdp::OnAttach`). The language is left alone everywhere:
        // the profile's own, which is what the login sent.
        let metadata = metadata(&user_agent, &full_version, hints.as_ref());
        let identity = json!({ "userAgent": user_agent, "userAgentMetadata": metadata });
        let page_commands = vec![
            ("Emulation.setUserAgentOverride", identity.clone()),
            // A headless tab never has focus, and `document.hasFocus()` said
            // so — measured false — where a page somebody is looking at says
            // true.
            (
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
            ),
            // The guard's pauses, judged by the rule set just below.
            ("Fetch.enable", guard::patterns()),
        ];
        cdp.connection().set_on_attach(crate::cdp::OnAttach {
            page: page_commands.clone(),
            worker: vec![
                ("Network.setUserAgentOverride", identity),
                ("Fetch.enable", guard::patterns()),
            ],
        });
        // Created here so the guard can read what the app is seen to send.
        let listened = Arc::new(listen::Shared::default());
        let judged = Arc::clone(&listened);
        cdp.connection().set_pause_rule(Arc::new(move |params| {
            let documents = lock(&judged.documents);
            guard::judge(params, documents.rediscovered())
        }));
        cdp.browser_call(
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                // The service and shared workers, which belong to the browser
                // rather than to a tab. The tab's own are asked for on it.
                "filter": [
                    { "type": "service_worker", "exclude": false },
                    { "type": "shared_worker", "exclude": false },
                    { "exclude": true },
                ],
            }),
        )
        .await?;

        for (method, params) in page_commands {
            cdp.page_call(&tab, method, params, COMMAND_TIMEOUT).await?;
        }
        cdp.page_call(
            &tab,
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            COMMAND_TIMEOUT,
        )
        .await?;
        // So that a tab that crashes says so, and the request waiting on it
        // fails at once rather than at its timeout: `Inspector.targetCrashed`
        // is only sent to a session that asked for the domain. Nothing about
        // it reaches the page.
        cdp.page_call(&tab, "Inspector.enable", json!({}), COMMAND_TIMEOUT)
            .await?;

        // The calls Instagram's own app makes on this tab are listened to for
        // push-back (`listen`): their answers, with room for a page's worth
        // and no more, and the tab's address as it changes, which is where an
        // in-app route to the challenge shows without a request. Their forms
        // come with them, which is what a request built like theirs copies;
        // the largest the capture shows is a few KiB.
        cdp.page_call(
            &tab,
            "Network.enable",
            json!({
                "maxTotalBufferSize": LISTENING_BUFFER,
                "maxResourceBufferSize": LISTENING_BUFFER / 4,
                "maxPostDataSize": 65_536,
            }),
            COMMAND_TIMEOUT,
        )
        .await?;
        cdp.browser_call(
            "Target.setDiscoverTargets",
            json!({ "discover": true, "filter": [{ "type": "page" }] }),
        )
        .await?;
        *lock(&account.listened) = Some(Arc::clone(&listened));
        // Told on to whoever asked to hear, the moment it happens; ends with
        // the browser, when the latch goes.
        let mut heard = listened.latch.heard();
        let tell = self.heard.clone();
        let pk = session.ds_user_id;
        tokio::spawn(async move {
            let cause = match heard.wait_for(Option::is_some).await {
                Ok(cause) => cause.clone(),
                Err(_) => return,
            };
            if let Some(cause) = cause {
                let _ = tell.send((pk, cause));
            }
        });
        let account_paths = self.paths.account(pk);
        tokio::spawn(listen::listen(
            cdp.connection().clone(),
            tab.clone(),
            tab_target,
            Arc::clone(&listened),
            account_paths.clone(),
        ));

        Ok(Live {
            cdp,
            profile,
            tab,
            origin: "about:blank".to_string(),
            world: None,
            page: snob_ig::page_values::PageValues::default(),
            checked: false,
            mark,
            listened,
            account: account_paths,
            token_named: None,
            asked_document: None,
        })
    }
}

/// `request`, unless the allowlist refuses it, a read under a number the app
/// was seen to send it under (`found`) as well as under the registry's:
/// then nothing is sent, and it is said as the defect of snob's it is.
fn refuse(request: &PageRequest, found: &Rediscovered) -> Result<(), PageError> {
    match snob_ig::allowlist::refused_with(request, found) {
        Some(what) => {
            tracing::error!(request = %what, "refused a call snob may not send");
            Err(PageError::NotAllowed(what))
        }
        None => Ok(()),
    }
}

/// The tab's answer to `call`, asked as `session`: what its document says,
/// or the answer to the request it asks for, built beside the document's
/// values ([`Live::build`]) and sent from the isolated world.
///
/// - [`Ask::Viewer`] and [`Ask::Tray`] are read from the document, and send
///   nothing.
/// - [`Ask::Pk`] asks the route definitions when the app has made a route
///   call on the document to copy, and otherwise loads the profile and reads
///   its route props ([`pk_of_the_document`]). Either way only once the
///   document is `session`'s: a document served to nobody, or to another
///   account, sends nothing.
/// - [`Ask::Asset`] fetches a file from the CDN ([`asset`]).
/// - [`Ask::Document`] loads the page, and hands back the bundles it names
///   and not its HTML, once the document is `session`'s. On a profile, it
///   waits for the app's own profile query first, and then for the app's
///   calls to pause for [`APP_CALLS_QUIET`], each up to
///   [`APP_CALLS_PATIENCE`]: the app sends its calls as the document boots
///   (its profile, highlights-tray and hover-card queries among them), and
///   a write asked next is built after them, as a person's click comes
///   after the page has loaded, with its `__req` past theirs. The document
///   is the one a write asked next is built on, while it is the tab's
///   ([`Live::build`]).
/// - The rest are built and sent.
async fn answer_on(live: &mut Live, session: &Session, call: &Call) -> Result<Told, PageError> {
    match &call.ask {
        Ask::Viewer => signed_in(live, session).await.map(Told::Viewer),
        Ask::Tray => {
            signed_in(live, session).await?;
            tab::tray(live).await.map(Told::Tray)
        }
        Ask::Pk { name } => {
            signed_in(live, session).await?;
            let route = format!("/{name}/");
            let envelope = {
                let loader = live.loader();
                lock(&live.listened.documents)
                    .get(&loader)
                    .is_some_and(|document| document.calls.made_a_route_call())
            };
            if !envelope {
                tracing::debug!(
                    "a name's pk is read from its profile document: the app made no route call to copy"
                );
                return pk_of_the_document(live, call, &route).await;
            }
            tracing::debug!("a name's pk is asked of the route definitions");
            let answer = built_and_sent(live, session, call).await?;
            let pk = route_answer(&answer.body, &route);
            Ok(Told::Pk { answer, pk })
        }
        Ask::Document { path } => {
            live.asked_document = None;
            let mut answer = loaded(live, call, path).await?;
            let bundles = snob_ig::graphql::bundles_in(&answer.body);
            answer.body.clear();
            if (200..300).contains(&answer.status) {
                signed_in(live, session).await?;
                if path != "/" {
                    let sent = the_apps_query(live, Operation::ProfilePage).await;
                    let settled = the_apps_calls_paused(live).await;
                    tracing::debug!(
                        sent = sent.is_some(),
                        settled,
                        "the app's profile query on the profile document, and its calls paused"
                    );
                }
                live.asked_document = Some((live.loader(), path.clone()));
            }
            Ok(Told::Document { answer, bundles })
        }
        Ask::Asset { url } => {
            if let Some(what) = snob_ig::allowlist::refused_asset(&call.origin, url) {
                tracing::error!(request = %what, "refused a download snob may not make");
                return Err(PageError::NotAllowed(what));
            }
            asset(live, call, url).await.map(Told::Asset)
        }
        Ask::Query { .. }
        | Ask::Rest { .. }
        | Ask::Navigation { .. }
        | Ask::Statuses { .. }
        | Ask::Write { .. } => built_and_sent(live, session, call).await.map(Told::Answer),
    }
}

/// The file at `url`, fetched from the isolated world as the app's own page
/// would fetch one from the CDN: the browser's TLS and HTTP/2, `Sec-Fetch-*`,
/// the page's `Referer` and none of the site's cookies, which are not the
/// CDN's. Handed back as base64 and never a video, which the guard fails by
/// address (the client keeps those). Not read as an answer from Instagram: a
/// CDN's refusal is no push-back, so the listener's rule is not applied.
/// Nothing is fetched once a push-back has been heard.
async fn asset(live: &mut Live, call: &Call, url: &str) -> Result<PageResponse, PageError> {
    if let Some(cause) = live.listened.latch.get() {
        return Err(PageError::PushedBack(cause));
    }
    let world = tab::isolated_world(live).await?;
    match tab::fetch_asset(live, world, url, call.cap, call.timeout_ms).await {
        Ok(response) => Ok(response),
        Err(e) => Err(live.listened.latch.get().map_or(e, PageError::PushedBack)),
    }
}

/// Who the tab's document was served to, when that is `session`'s account
/// ([`snob_ig::web::viewer_of`], as a built call checks it).
async fn signed_in(
    live: &mut Live,
    session: &Session,
) -> Result<snob_ig::page_values::Viewer, PageError> {
    let page = live.page_values().await?;
    Ok(snob_ig::web::viewer_of(&page, session.ds_user_id)?.clone())
}

/// The request `call` asks for, built on the tab's document, held to the
/// allowlist, and sent from the isolated world.
async fn built_and_sent(
    live: &mut Live,
    session: &Session,
    call: &Call,
) -> Result<PageResponse, PageError> {
    let request = live.build(session, call).await?;
    refuse(&request, &live.rediscovered())?;
    let world = tab::isolated_world(live).await?;
    let response = fetch(live, world, &request).await?;
    own_answer(live, &request, response).await
}

/// The tab goes to `path` on the call's origin, a navigation the allowlist
/// lets out, and the document it lands on is the answer.
async fn loaded(live: &mut Live, call: &Call, path: &str) -> Result<PageResponse, PageError> {
    let request = PageRequest {
        method: Method::Get,
        url: format!("{}{path}", call.origin),
        headers: Vec::new(),
        referrer: format!("{}{}", call.origin, call.referrer),
        body: None,
        navigate: true,
        cap: call.cap,
        timeout_ms: call.timeout_ms,
    };
    refuse(&request, &Rediscovered::default())?;
    let response = navigate_and_read(live, &request.url).await?;
    own_answer(live, &request, response).await
}

/// The pk behind `route`, read from the profile document the tab loads for
/// it: the id of the route props the document embeds.
///
/// **The app's own profile query is a second witness.** It sends one as a
/// profile document boots, by the id it read from the same props; when its
/// variables arrive on the new document within [`APP_CALLS_PATIENCE`], their
/// id is checked against the document's. Two that disagree are an error,
/// as is a document that names neither: a wrong pk would read another
/// account. The document's HTML is not handed back.
///
/// A document that is not a success, a name nobody owns among them, is
/// handed back at once: the app sends no profile query from it to wait for.
async fn pk_of_the_document(live: &mut Live, call: &Call, route: &str) -> Result<Told, PageError> {
    let mut answer = loaded(live, call, route).await?;
    let from_the_document = document_pk(&answer.body);
    answer.body.clear();
    if !(200..300).contains(&answer.status) {
        tracing::debug!(
            status = answer.status,
            "the profile document is not a success"
        );
        return Ok(Told::Pk {
            answer,
            pk: RouteAnswer::Error,
        });
    }
    let from_the_app = the_apps_query(live, Operation::ProfilePage)
        .await
        .and_then(|sent| id_in(&sent));
    let witnesses = match (from_the_document, from_the_app) {
        (Some(document), Some(app)) if document == app => "both, and they agree",
        (Some(_), Some(_)) => "both, and they name two accounts",
        (Some(_), None) => "the document's props alone",
        (None, Some(_)) => "the app's query alone",
        (None, None) => "neither",
    };
    tracing::debug!(witnesses, "the pk the profile document named");
    let pk = match (from_the_document, from_the_app) {
        (Some(document), Some(app)) if document != app => RouteAnswer::Error,
        (Some(pk), _) | (None, Some(pk)) => RouteAnswer::Pk(pk),
        (None, None) => RouteAnswer::Error,
    };
    Ok(Told::Pk { answer, pk })
}

/// The variables the app sent with its own `operation` on the tab's
/// document, waited for up to [`APP_CALLS_PATIENCE`]; `None` when it sent
/// none by then.
async fn the_apps_query(live: &Live, operation: Operation) -> Option<String> {
    let loader = live.loader();
    let deadline = tokio::time::Instant::now() + APP_CALLS_PATIENCE;
    loop {
        let sent = lock(&live.listened.documents)
            .get(&loader)
            .and_then(|document| document.calls.variables_sent(operation))
            .map(str::to_string);
        if sent.is_some() || tokio::time::Instant::now() >= deadline {
            return sent;
        }
        tokio::time::sleep(APP_CALLS_POLL).await;
    }
}

/// Waits until the app has sent no call from the tab's document for
/// [`APP_CALLS_QUIET`], up to [`APP_CALLS_PATIENCE`]; says whether it
/// paused by then. A call the app makes after, such as a poll, is not
/// waited for.
async fn the_apps_calls_paused(live: &Live) -> bool {
    let loader = live.loader();
    let heard = || {
        lock(&live.listened.documents)
            .get(&loader)
            .map_or(0, |document| document.heard)
    };
    let deadline = tokio::time::Instant::now() + APP_CALLS_PATIENCE;
    let mut last = heard();
    let mut quiet_since = tokio::time::Instant::now();
    loop {
        if quiet_since.elapsed() >= APP_CALLS_QUIET {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(APP_CALLS_POLL).await;
        let now = heard();
        if now != last {
            last = now;
            quiet_since = tokio::time::Instant::now();
        }
    }
}

/// The `id` of a query's variables, as the app sends it: a string of digits.
fn id_in(variables: &str) -> Option<Pk> {
    let variables: Value = serde_json::from_str(variables).ok()?;
    match variables.get("id")? {
        Value::String(id) => id.parse().ok(),
        Value::Number(id) => id.as_u64().map(Pk::new),
        _ => None,
    }
}

/// One of snob's own answers, on its way back to the client.
///
/// A push-back is recorded here, by the process holding the browser, rather
/// than by the client ([`listen::heard`]): the command that asked may have
/// stopped waiting, and the owner's browser still heard it. It is then
/// refused like everything after it, so the client never records it.
/// The row is committed before the refusal is handed back, so the command
/// that asked finds it. Nothing is passed on once the latch is claimed,
/// whatever this answer says: the listener may have sent the tab to
/// `about:blank` under it. The hops of an answer refused as a push-back are
/// not charged: the cooldown is already committed, and the pacer would
/// refuse the charge.
async fn own_answer(
    live: &Live,
    request: &PageRequest,
    response: PageResponse,
) -> Result<PageResponse, PageError> {
    let latch = &live.listened.latch;
    if latch.get().is_none()
        && let Some(error) = listen::pushed_back(&request.url, &response)
    {
        listen::noted_own(&request.url, &response);
        let recording = listen::heard(
            live.cdp.connection(),
            &live.tab,
            &live.listened,
            &live.account,
            &error,
        );
        if let Some(recording) = recording {
            let _ = recording.await;
        }
    }
    match latch.get() {
        Some(cause) => Err(PageError::PushedBack(cause)),
        None => Ok(response),
    }
}

/// Chromium will not run as root with its sandbox on, and snob does not turn
/// the sandbox off for anybody: this browser loads pages and media a server
/// chose, which is what the sandbox is for. Measured: it exits 1 at once, and
/// "exited with code 1" says nothing about why. Root is the ordinary user in
/// a container, which is where this is most likely to be met.
pub(crate) fn refuse_root() -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid` takes nothing, touches no memory and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            anyhow::bail!(
                "snob sends its requests from a browser, and Chromium will not run as root \
                 with its sandbox on.\n\
                 Run snob as an ordinary user, or set SNOB_NO_BROWSER=1 to send the requests \
                 directly — which Instagram can tell apart from a browser."
            );
        }
    }
    Ok(())
}

/// A failure of the browser, or of driving it, as the page reports one.
fn broken(e: anyhow::Error) -> PageError {
    PageError::Browser(format!("{e:#}"))
}

/// Whether `session` is to be written into a browser that already holds
/// another of the account's, made at `held_since`: only a later login is. Two
/// commands on one account can hold two of its sessions — one started before
/// a login in another terminal — and the browser keeps the later rather than
/// trading one for the other at every request, which would also hand back a
/// session the browser has since rotated. A login is authoritative;
/// `write_back` holds the same rule.
///
/// **Later by the wall clock** when each was made, to the second. A login
/// made in the same second as the one before, or while the clock stood
/// behind where it was then, does not reach a browser that is open; a login
/// closes the account's browser first (`owner::release`), so its own check
/// meets a new one, and what is left is a command started before it that
/// sends after it.
fn newer_login(held_since: Option<snob_core::Epoch>, session: &Session) -> bool {
    held_since.is_none_or(|at| session.created_at > at)
}

/// Attaches to the tab the browser opened with, in flat mode so its commands
/// travel on the same pipe.
async fn attach_to_a_tab(cdp: &Cdp) -> Result<Tab> {
    let targets = cdp.browser_call("Target.getTargets", json!({})).await?;
    let page = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .and_then(|all| {
            all.iter()
                .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        })
        .and_then(|t| t.get("targetId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the browser opened no tab"))?
        .to_string();
    let attached = cdp
        .browser_call(
            "Target.attachToTarget",
            json!({ "targetId": page, "flatten": true }),
        )
        .await?;
    let session = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the browser would not attach to its tab"))?;
    Ok(Tab {
        session,
        target: page,
    })
}

/// The tab a browser opened with, as this connection knows it.
struct Tab {
    /// The DevTools session its commands travel on.
    session: String,
    /// The target it is, whose address the browser reports as it changes.
    target: String,
}

/// The host of an origin.
fn host_of(origin: &str) -> Option<String> {
    url::Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

/// Scheme, host and port, with no trailing slash.
fn origin_of(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("not an address: {url}"))?;
    Ok(parsed.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_has_no_path_and_no_trailing_slash() {
        assert_eq!(
            origin_of("https://www.instagram.com/api/v1/x/?a=1").unwrap(),
            "https://www.instagram.com"
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8765/api/v1/x/").unwrap(),
            "http://127.0.0.1:8765"
        );
    }

    /// A login made later than the session the browser holds replaces it; one
    /// made in the same second, or while the clock stood behind, does not.
    #[test]
    fn only_a_login_made_later_by_the_clock_reaches_an_open_browser() {
        let mut session = Session::from_sessionid(
            "42%3Aabc%3A1",
            "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        session.created_at = snob_core::Epoch::new(1_000);
        let at = |secs| Some(snob_core::Epoch::new(secs));
        assert!(newer_login(None, &session), "a browser holding nothing");
        assert!(newer_login(at(999), &session));
        assert!(!newer_login(at(1_000), &session), "the same second");
        assert!(!newer_login(at(1_001), &session), "a clock set back");
    }

    /// What a browser heard goes with it: a browser started after the one a
    /// push-back was heard on is not stopped by it, so a monitor's next run
    /// sends once the cooldown is over.
    #[tokio::test]
    async fn a_push_back_is_forgotten_with_the_browser_that_heard_it() {
        let root = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(root.path()));
        let pk = Pk::new(42);
        let shared = Arc::new(listen::Shared::default());
        assert!(shared.latch.claim(PushedBack::RateLimited));
        *headless.account(pk).listened.lock().unwrap() = Some(shared);
        assert_eq!(headless.heard_for(pk), Some(PushedBack::RateLimited));

        headless.release(Some(pk)).await;
        assert_eq!(headless.heard_for(pk), None);
    }

    /// A navigation sent as a request, to a profile the allowlist lets the
    /// tab go to, is refused before a browser is started for it: a document
    /// is asked as one, and its HTML never leaves the tab.
    #[tokio::test]
    async fn a_navigation_sent_as_a_request_starts_no_browser() {
        let server = wiremock::MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(root.path()));
        let request = PageRequest {
            url: format!("{}/someone/", server.uri()),
            navigate: true,
            ..answer(&server)
        };
        let error = headless
            .send_as(&made("42%3Anavigation%3A1", 1_000), request)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, PageError::NotAllowed(what) if what == "a navigation is asked as a document"),
            "{error:?}"
        );
        assert!(headless.account(Pk::new(42)).live.lock().await.is_none());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// Says why a test that needs a browser does not run, or fails it where
    /// one is required, as the browser tests under `tests/` do.
    fn skip(why: &str) {
        assert!(
            !env_flag("SNOB_TEST_REQUIRE_BROWSER"),
            "{why}, and SNOB_TEST_REQUIRE_BROWSER needs one"
        );
        eprintln!("{why}; skipping");
    }

    /// A site to send to: its page, which sets `cookie` when given one, and
    /// an answer at `/api/v1/friendships/42/following/`.
    async fn site(cookie: Option<&str>) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut page = ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/html")
            .set_body_string("<!doctype html><title>site</title>");
        if let Some(cookie) = cookie {
            page = page.insert_header("Set-Cookie", cookie);
        }
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(page)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        server
    }

    fn answer(server: &wiremock::MockServer) -> PageRequest {
        PageRequest {
            method: snob_ig::client::page::Method::Get,
            url: format!("{}/api/v1/friendships/42/following/", server.uri()),
            headers: Vec::new(),
            referrer: format!("{}/", server.uri()),
            body: None,
            navigate: false,
            cap: 1024 * 1024,
            timeout_ms: 20_000,
        }
    }

    fn made(sessionid: &str, at: i64) -> Session {
        let mut session = Session::from_sessionid(
            sessionid,
            "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        session.created_at = snob_core::Epoch::new(at);
        session
    }

    /// Sends the first request as `session`, which starts the browser, and
    /// says whether it did.
    async fn started(headless: &Headless, session: &Session, request: PageRequest) -> bool {
        if !a_browser_is_installed() {
            return false;
        }
        match headless.send_as(session, request).await {
            Ok(_) => true,
            Err(PageError::Browser(e)) => {
                skip(&format!("the browser found here will not start ({e})"));
                false
            }
            Err(e) => panic!("{e:?}"),
        }
    }

    /// A browser holding a later login keeps it when a command hands it an
    /// older session, and hands its cookies back for a write-back only to a
    /// command that handed it the session it holds: the older one's command
    /// gets nothing, and so writes nothing over the store.
    #[tokio::test]
    async fn a_browser_hands_back_only_the_session_it_holds() {
        let server = site(None).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let later = made("42%3Alater%3A1", 2_000);
        let older = made("42%3Aolder%3A1", 1_000);

        if !started(&headless, &later, answer(&server)).await {
            return;
        }
        headless.send_as(&older, answer(&server)).await.unwrap();
        let cookie = last_sessionid(&server).await;
        assert!(cookie.contains("sessionid=42%3Alater%3A1"), "{cookie}");

        let pk = Pk::new(42);
        assert!(
            headless
                .cookies_of(pk, &older.fingerprint())
                .await
                .is_none()
        );
        let held = headless
            .cookies_of(pk, &later.fingerprint())
            .await
            .expect("the cookies of the session it holds");
        assert_eq!(held.sessionid.expose(), "42%3Alater%3A1");
        headless.release(None).await;
    }

    /// What a write-back stored from a browser is a session the browser
    /// holds: the next command hands it as stored, and gets the cookies back
    /// to write again. The site here renames the session as the browser
    /// meets it, as Instagram does a rotated or undecoded one.
    #[tokio::test]
    async fn a_browser_hands_back_what_was_written_back_from_it() {
        let server = site(Some("sessionid=42%3Arotated%3A2; Path=/; HttpOnly")).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let handed = made("42%3Ahanded%3A1", 1_000);
        let pk = Pk::new(42);

        if !started(&headless, &handed, answer(&server)).await {
            return;
        }
        let jar = headless
            .cookies_of(pk, &handed.fingerprint())
            .await
            .expect("the cookies of the session it was handed");
        let stored = write_back::merged(&handed, &handed.fingerprint(), &jar)
            .expect("the renamed session, to store");
        assert_eq!(stored.sessionid.expose(), "42%3Arotated%3A2");

        headless.send_as(&stored, answer(&server)).await.unwrap();
        let held = headless
            .cookies_of(pk, &stored.fingerprint())
            .await
            .expect("the cookies of the session stored from it");
        assert_eq!(held.sessionid.expose(), "42%3Arotated%3A2");
        headless.release(None).await;
    }

    /// The site's page, renaming the session of whichever account loads it.
    struct RotatesEach;

    impl wiremock::Respond for RotatesEach {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let page = wiremock::ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string("<!doctype html><title>site</title>");
            let cookie = request
                .headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            match ["42", "43"]
                .into_iter()
                .find(|pk| cookie.contains(&format!("sessionid={pk}%3Ahanded%3A1")))
            {
                Some(pk) => page.insert_header(
                    "Set-Cookie",
                    format!("sessionid={pk}%3Arotated%3A2; Path=/; HttpOnly"),
                ),
                None => page,
            }
        }
    }

    /// Two accounts' browsers rotate their sessions in one run: closing them
    /// stores each account's rotated session in its own place, each profile's
    /// mark follows its own, and neither jar answers for the other's session.
    #[tokio::test]
    async fn two_accounts_rotating_in_one_run_each_keep_their_own() {
        use wiremock::Mock;
        use wiremock::matchers::{method, path};

        let server = site(None).await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(RotatesEach)
            .with_priority(1)
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let (paths, headless) = two_accounts(tmp.path());
        let secrets = snob_store::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-rotating-{}", std::process::id()));
        let sessions = [
            made("42%3Ahanded%3A1", 1_000),
            made("43%3Ahanded%3A1", 1_000),
        ];
        for session in &sessions {
            let account = paths.account(session.ds_user_id);
            secrets.session_of(&account).save(session).unwrap();
        }

        if !started(&headless, &sessions[0], answer(&server)).await {
            return;
        }
        headless
            .send_as(&sessions[1], answer(&server))
            .await
            .unwrap();
        // What a command leaving the owner is handed, account by account:
        // each browser's own jar, and none for the other's session.
        for (session, other) in [(&sessions[0], &sessions[1]), (&sessions[1], &sessions[0])] {
            let pk = session.ds_user_id;
            let jar = headless.cookies_of(pk, &session.fingerprint()).await;
            assert_eq!(
                jar.unwrap().sessionid.expose(),
                format!("{pk}%3Arotated%3A2")
            );
            assert!(
                headless
                    .cookies_of(pk, &other.fingerprint())
                    .await
                    .is_none()
            );
        }
        write_back::keep(&secrets, &paths, headless.close().await);

        for pk in [42, 43] {
            let account = paths.account(Pk::new(pk));
            let kept = secrets.session_of(&account).load().unwrap().unwrap();
            assert_eq!(kept.ds_user_id, Pk::new(pk));
            assert_eq!(kept.sessionid.expose(), format!("{pk}%3Arotated%3A2"));
            let mark = ProfileMark::read(&paths.browser_profile_for(Pk::new(pk)));
            assert_eq!(mark.session, Some(kept.fingerprint()), "account {pk}");
        }
    }

    /// Instagram's 429, after `delay`.
    fn refused(delay: Duration) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(429)
            .set_body_raw(
                r#"{"message":"Please wait a few minutes"}"#,
                "application/json",
            )
            .set_delay(delay)
    }

    /// A site whose answer to snob's request is `answer`, the page running
    /// the app's `script`.
    async fn refusing(
        answer: impl wiremock::Respond + 'static,
        script: &str,
    ) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("<!doctype html><title>site</title><script>{script}</script>"),
                "text/html",
            ))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/following/"))
            .respond_with(answer)
            .with_priority(1)
            .mount(&server)
            .await;
        server
    }

    /// Two accounts, each with its database, and the browsers to send as
    /// them from.
    fn two_accounts(root: &std::path::Path) -> (AppPaths, Headless) {
        let paths = AppPaths::rooted_at(root);
        for pk in [42, 43] {
            snob_store::store::Store::open(&paths.account(Pk::new(pk))).unwrap();
        }
        (paths.clone(), Headless::new(paths))
    }

    /// How long the account's cooldown has left, if it has one.
    fn cooldown_left(paths: &AccountPaths) -> Option<Duration> {
        use snob_core::budget::RateBudget;
        let budget = snob_store::store::rate_budget::SqliteRateBudget::open(paths).unwrap();
        let until = budget.cooldown().unwrap()?;
        let left = until.get() - snob_core::clock::now_ms().get();
        Some(Duration::from_millis(u64::try_from(left).unwrap_or(0)))
    }

    /// Waits for a 429 to be recorded in the account's database, which is
    /// done on a task of its own, and checks it was recorded once: a cooldown
    /// recorded twice within a day doubles.
    async fn rate_limit_recorded_once(paths: &AccountPaths) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while cooldown_left(paths).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the push-back was never recorded"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Room for a second recording to land, were there one.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let owed = snob_core::budget::RATE_LIMIT_COOLDOWN;
        let left = cooldown_left(paths).unwrap();
        let slack = Duration::from_secs(120);
        assert!(
            left <= owed + slack && left + slack >= owed,
            "a cooldown of {left:?} where one of {owed:?} was owed"
        );
    }

    /// Whether a browser is here to test with.
    fn a_browser_is_installed() -> bool {
        let found = crate::browser::detect().is_some();
        if !found {
            skip("no browser installed");
        }
        found
    }

    /// How many of snob's requests reached the site.
    async fn asked(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/api/v1/friendships/42/following/")
            .count()
    }

    /// A push-back on snob's own request is recorded by the process holding
    /// the browser, once, in the database of the account it was sent as and
    /// no other, and nothing more is sent from that browser.
    #[tokio::test]
    async fn snobs_own_push_back_is_recorded_once_in_its_accounts_database() {
        if !a_browser_is_installed() {
            return;
        }
        let server = refusing(refused(Duration::ZERO), "").await;
        let tmp = tempfile::tempdir().unwrap();
        let (paths, headless) = two_accounts(tmp.path());
        let session = made("42%3Aown%3A1", 1_000);

        match headless.send_as(&session, answer(&server)).await {
            Err(PageError::PushedBack(PushedBack::RateLimited)) => {}
            Err(PageError::Browser(e)) => {
                skip(&format!("the browser found here will not start ({e})"));
                return;
            }
            other => panic!("{other:?}"),
        }
        rate_limit_recorded_once(&paths.account(Pk::new(42))).await;
        assert_eq!(cooldown_left(&paths.account(Pk::new(43))), None);

        assert!(matches!(
            headless.send_as(&session, answer(&server)).await,
            Err(PageError::PushedBack(PushedBack::RateLimited))
        ));
        assert_eq!(asked(&server).await, 1, "nothing more was sent");
        rate_limit_recorded_once(&paths.account(Pk::new(42))).await;
        headless.release(None).await;
    }

    /// Refuses snob's request slowly, and says at once that it has arrived.
    struct RefusedSlowly(Arc<std::sync::atomic::AtomicBool>);

    impl wiremock::Respond for RefusedSlowly {
        fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            refused(Duration::from_millis(2_500))
        }
    }

    /// Open once snob's request has arrived.
    struct Gate(Arc<std::sync::atomic::AtomicBool>);

    impl wiremock::Respond for Gate {
        fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
            let open = self.0.load(std::sync::atomic::Ordering::SeqCst);
            wiremock::ResponseTemplate::new(200).set_body_string(if open { "open" } else { "shut" })
        }
    }

    /// Heard first by the listener, on a call of the app's refused while
    /// snob's own request was on its way, a push-back is recorded once:
    /// snob's own 429, answered after, finds the latch claimed.
    #[tokio::test]
    async fn a_push_back_the_listener_heard_first_is_recorded_once() {
        use wiremock::Mock;
        use wiremock::matchers::{method, path};

        if !a_browser_is_installed() {
            return;
        }
        let arrived = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = refusing(
            RefusedSlowly(Arc::clone(&arrived)),
            "const wait = () => fetch('/gate').then(r => r.text()).then(t =>
               t === 'open' ? fetch('/api/v1/app-call/') : setTimeout(wait, 50));
             wait();",
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/gate"))
            .respond_with(Gate(Arc::clone(&arrived)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/app-call/"))
            .respond_with(refused(Duration::ZERO))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let (paths, headless) = two_accounts(tmp.path());

        match headless
            .send_as(&made("42%3Aheard%3A1", 1_000), answer(&server))
            .await
        {
            Err(PageError::PushedBack(PushedBack::RateLimited)) => {}
            Err(PageError::Browser(e)) => {
                skip(&format!("the browser found here will not start ({e})"));
                return;
            }
            other => panic!("{other:?}"),
        }
        let app_called = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .any(|r| r.url.path() == "/api/v1/app-call/");
        assert!(
            app_called,
            "the app's call was refused while snob's was on its way"
        );
        rate_limit_recorded_once(&paths.account(Pk::new(42))).await;
        assert_eq!(cooldown_left(&paths.account(Pk::new(43))), None);
        headless.release(None).await;
    }

    /// Answers snob's request after a moment, and says at once that it has
    /// arrived.
    struct AnsweredSlowly(Arc<std::sync::atomic::AtomicBool>);

    impl wiremock::Respond for AnsweredSlowly {
        fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            wiremock::ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(Duration::from_millis(1_500))
        }
    }

    /// The session the site was last sent on snob's request.
    async fn last_sessionid(server: &wiremock::MockServer) -> String {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .rev()
            .find(|r| r.url.path() == "/api/v1/friendships/42/following/")
            .and_then(|r| r.headers.get("cookie"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// Closing an account's browser waits for the request in flight on it,
    /// and the next request, a new login's, starts a browser on the same
    /// profile that sends it.
    #[tokio::test]
    async fn a_browser_closed_mid_request_answers_it_and_the_next_starts_fresh() {
        let arrived = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = refusing(AnsweredSlowly(Arc::clone(&arrived)), "").await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let first = made("42%3Afirst%3A1", 1_000);
        let second = made("42%3Asecond%3A1", 2_000);

        if !started(&headless, &first, answer(&server)).await {
            return;
        }
        arrived.store(false, std::sync::atomic::Ordering::SeqCst);
        let answered = std::sync::atomic::AtomicBool::new(false);
        let (first_sent, released) = tokio::join!(
            async {
                let result = headless.send_as(&first, answer(&server)).await;
                answered.store(true, std::sync::atomic::Ordering::SeqCst);
                result
            },
            async {
                while !arrived.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                headless.release(Some(Pk::new(42))).await;
                let waited = answered.load(std::sync::atomic::Ordering::SeqCst);
                (waited, headless.send_as(&second, answer(&server)).await)
            }
        );
        first_sent.expect("the request in flight is answered");
        let (waited, next) = released;
        assert!(waited, "the browser was closed under its request");
        next.expect("the next request starts a browser that sends it");
        let cookie = last_sessionid(&server).await;
        assert!(cookie.contains("sessionid=42%3Asecond%3A1"), "{cookie}");
        assert_eq!(asked(&server).await, 3);
        headless.release(None).await;
    }

    /// A browser started for a request that stops answering before the
    /// request is sent — as a fresh one on a profile its predecessor's
    /// helpers still held did, on `Page.getFrameTree` — is closed and one
    /// more is started, and the request goes out once.
    #[tokio::test]
    async fn a_fresh_browser_that_stalls_before_sending_is_started_again() {
        let server = site(None).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Astall%3A1", 1_000);

        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        headless.release(Some(Pk::new(42))).await;
        let loaded = loads(&server).await;
        tab::STALL_FRAME_TREE.set(true);
        let sent = headless.send_as(&session, answer(&server)).await;
        assert!(!tab::STALL_FRAME_TREE.get(), "the stall was never met");
        sent.expect("a second browser sends it");
        // Each browser started loads the site; the stalled one again would not.
        assert_eq!(
            loads(&server).await,
            loaded + 2,
            "no second browser started"
        );
        assert_eq!(asked(&server).await, 2, "the request went out once");
        headless.release(None).await;
    }

    /// A fresh browser that fails before sending once Instagram has pushed
    /// back on it is not started again: the push-back is the answer, and
    /// neither the site nor the request is asked for again.
    #[tokio::test]
    async fn a_fresh_browser_pushed_back_on_before_sending_is_not_started_again() {
        let server = site(None).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aheard%3A1", 1_000);

        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        headless.release(Some(Pk::new(42))).await;
        let loaded = loads(&server).await;
        tab::HEARD_BEFORE_STALL.set(Some(PushedBack::RateLimited));
        tab::STALL_FRAME_TREE.set(true);
        let sent = headless.send_as(&session, answer(&server)).await;
        assert!(!tab::STALL_FRAME_TREE.get(), "the stall was never met");
        assert!(
            matches!(sent, Err(PageError::PushedBack(PushedBack::RateLimited))),
            "{sent:?}"
        );
        assert_eq!(loads(&server).await, loaded + 1, "a second browser started");
        assert_eq!(asked(&server).await, 1, "the request went out");
        headless.release(None).await;
    }

    /// A document shaped like a logged-in one as viewer `pk`: the modules the
    /// values are read from, made up, and a boot script that sends calls the
    /// way the app's carry what the document does not — a REST call with the
    /// web session id, a Relay query of the registry's with the rest of its
    /// form and the document's own token, and a route call's envelope.
    fn document(pk: u64, viewer: &str, spin_t: u64, relay: &str, session_id: &str) -> String {
        let hover = snob_ig::allowlist::Operation::HoverCard;
        let boot = [
            format!(
                "fetch('/api/v1/boot/', {{ headers: {{ 'X-Web-Session-ID': '{session_id}' }} }});"
            ),
            app_post(
                "/api/graphql",
                Some(hover.friendly_name()),
                &app_relay_form(hover.friendly_name(), hover.doc_id(), relay, session_id),
            ),
            app_post(
                "/ajax/bulk-route-definitions/",
                None,
                &app_routes_form(relay),
            ),
        ]
        .join("\n");
        shaped(pk, viewer, spin_t, relay, &boot)
    }

    /// A document shaped like a logged-in one as viewer `pk`, which runs
    /// `boot` as it loads.
    fn shaped(pk: u64, viewer: &str, spin_t: u64, relay: &str, boot: &str) -> String {
        format!(
            r#"<!doctype html><title>site</title>
<script type="application/json">{{"require":[["ScheduledServerJS","handle",null,[{{"__bbox":{{"define":[["SiteData",[],{{"client_revision":1000000001,"haste_session":"20000.HYP:instagram_web_pkg.2.1...0","hsi":"7000000000000000001","__spin_r":1000000001,"__spin_b":"trunk","__spin_t":{spin_t}}},317],["SiteData",[],{{"client_revision":1000000001,"haste_session":"20000.HYP:instagram_web_pkg.2.1...0","hsi":"7000000000000000001","__spin_r":1000000001,"__spin_b":"trunk","__spin_t":1}},317],["LSD",[],{{"token":"lsd"}},323],["DTSGInitialData",[],{{"token":"{relay}"}},258],["DTSGInitData",[],{{"token":"session","async_get_token":"async"}},3515],["WebBloksVersioningID",[],{{"versioningID":"0123456789abcdef"}},6013],["PolarisViewer",[],{{"data":{{"id":"{pk}","username":"{viewer}","fbid":"17841400000000001"}},"id":"{pk}"}},1508]]}}}}]]]}}</script>
<script>{boot}</script>"#
        )
    }

    /// A Relay form of the app's for `name` under `doc_id`, made up in the
    /// capture's shape: every field a call built like it copies, its counter
    /// at `c`, and the document's own token and web session id.
    fn app_relay_form(name: &str, doc_id: &str, relay: &str, session_id: &str) -> String {
        let variables = r#"{"id":"2345678901","app_flag":true}"#;
        app_relay_form_with(name, doc_id, relay, session_id, variables)
    }

    /// [`app_relay_form`], with the app's `variables`.
    fn app_relay_form_with(
        name: &str,
        doc_id: &str,
        relay: &str,
        session_id: &str,
        variables: &str,
    ) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("av", "17841400000000001"),
                ("__d", "www"),
                ("__user", "0"),
                ("__a", "1"),
                ("__req", "c"),
                ("__hs", "20000.HYP:instagram_web_pkg.2.1...0"),
                ("dpr", "1"),
                ("__ccg", "EXCELLENT"),
                ("__rev", "1000000001"),
                ("__s", session_id),
                ("__hsi", "7000000000000000001"),
                ("__dyn", "7xeUmwlE"),
                ("__csr", "gP4ll"),
                ("__hsdp", ""),
                ("__hblp", ""),
                ("__sjsp", "g4o"),
                ("__comet_req", "7"),
                ("fb_dtsg", relay),
                ("jazoest", "21234"),
                ("lsd", "lsd"),
                ("__spin_r", "1000000001"),
                ("__spin_b", "trunk"),
                ("__spin_t", "1790000001"),
                ("fb_api_caller_class", "RelayModern"),
                ("fb_api_req_friendly_name", name),
                ("server_timestamps", "true"),
                ("variables", variables),
                ("doc_id", doc_id),
            ])
            .finish()
    }

    /// A route call's form of the app's, its counter at `b`.
    fn app_routes_form(relay: &str) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("route_urls[0]", "/someone/"),
                ("routing_namespace", "igx_www"),
                ("__d", "www"),
                ("__user", "0"),
                ("__a", "1"),
                ("__req", "b"),
                ("fb_dtsg", relay),
                ("jazoest", "21234"),
                ("lsd", "lsd"),
            ])
            .finish()
    }

    /// A line of script that posts `form` to `path` as the app does,
    /// announcing `name` when it is a Relay call.
    fn app_post(path: &str, name: Option<&str>, form: &str) -> String {
        let mut headers = json!({
            "Content-Type": "application/x-www-form-urlencoded",
            "X-FB-LSD": "lsd",
        });
        if let Some(name) = name {
            headers["X-FB-Friendly-Name"] = json!(name);
        }
        format!(
            "fetch({}, {{ method: 'POST', headers: {headers}, body: {} }});",
            json!(path),
            json!(form)
        )
    }

    /// Answers the calls a [`document`]'s boot script sends.
    async fn answer_the_app(server: &wiremock::MockServer) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        Mock::given(method("GET"))
            .and(path("/api/v1/boot/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(server)
            .await;
        for posted in ["/api/graphql", "/ajax/bulk-route-definitions/"] {
            Mock::given(method("POST"))
                .and(path(posted))
                .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
                .mount(server)
                .await;
        }
    }

    /// The values are read from the document the tab loaded, with the web
    /// session id its app sent, and read again when the tab loads another.
    #[tokio::test]
    async fn the_page_values_are_the_current_documents() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        let html = |body: String| ResponseTemplate::new(200).set_body_raw(body, "text/html");
        // Ahead of `site`'s own page.
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(document(
                42,
                "first.viewer",
                1_790_000_001,
                "relay:one",
                "abc123:def456:ghi789",
            )))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/second/"))
            .respond_with(html(document(
                42,
                "second.viewer",
                1_790_000_002,
                "relay:two",
                "abc123:def456:jkl012",
            )))
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Avalues%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let read = || async {
            let account = headless.account(Pk::new(42));
            let mut slot = account.live.lock().await;
            let live = slot.as_mut().expect("the browser");
            live.page_values().await.expect("the page's values")
        };

        let first = read().await;
        let viewer = first.viewer.expect("the viewer");
        assert_eq!(viewer.username, "first.viewer");
        assert_eq!(viewer.pk, Pk::new(42));
        assert_eq!(first.site.expect("SiteData").spin_t, "1790000001");
        assert_eq!(
            first.relay_dtsg.as_ref().map(|t| t.expose()),
            Some("relay:one")
        );
        assert_eq!(first.bloks_version.as_deref(), Some("0123456789abcdef"));
        assert_eq!(first.web_session.as_deref(), Some("abc123:def456:ghi789"));

        let told = headless
            .ask_as(
                &session,
                intent(
                    &server,
                    Ask::Document {
                        path: "/second/".into(),
                    },
                    "/",
                ),
            )
            .await
            .unwrap();
        assert!(matches!(told, Told::Document { .. }), "{told:?}");
        let second = read().await;
        assert_eq!(second.viewer.expect("the viewer").username, "second.viewer");
        assert_eq!(second.site.expect("SiteData").spin_t, "1790000002");
        assert_eq!(
            second.relay_dtsg.as_ref().map(|t| t.expose()),
            Some("relay:two")
        );
        assert_eq!(second.web_session.as_deref(), Some("abc123:def456:jkl012"));
        headless.release(None).await;
    }

    /// A document the app loads by itself, with nothing sent by snob, is the
    /// one the values are read from next: never the old document's values
    /// beside the new one's web session.
    #[tokio::test]
    async fn the_page_values_follow_a_document_the_app_loads_by_itself() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        let html = |body: String| ResponseTemplate::new(200).set_body_raw(body, "text/html");
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(document(
                42,
                "first.viewer",
                1_790_000_001,
                "relay:one",
                "abc123:def456:ghi789",
            )))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(document(
                42,
                "reloaded.viewer",
                1_790_000_002,
                "relay:two",
                "abc123:def456:jkl012",
            )))
            .with_priority(2)
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Areload%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let account = headless.account(Pk::new(42));
        let mut slot = account.live.lock().await;
        let live = slot.as_mut().expect("the browser");
        let first = live.page_values().await.unwrap();
        assert_eq!(first.viewer.expect("the viewer").username, "first.viewer");
        let first_loader = live.world.clone().expect("the first document's world").0;

        tab::evaluate(
            &live.cdp,
            &live.tab,
            "setTimeout(() => location.reload(), 0)",
            COMMAND_TIMEOUT,
        )
        .await
        .unwrap();
        // Until the reloaded document's app has sent its call.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let reloaded = loop {
            let values = live.page_values().await.unwrap();
            if values.web_session.as_deref() == Some("abc123:def456:jkl012") {
                break values;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the page never reloaded"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // The first document's calls are its own still, and not the new one's.
        let loader = &live
            .world
            .as_ref()
            .expect("the reloaded document's world")
            .0;
        assert_ne!(*loader, first_loader);
        let kept = lock(&live.listened.documents)
            .get(&first_loader)
            .and_then(|document| document.web_session.clone());
        assert_eq!(kept.as_deref(), Some("abc123:def456:ghi789"));

        assert_eq!(
            reloaded.viewer.expect("the viewer").username,
            "reloaded.viewer"
        );
        assert_eq!(reloaded.site.expect("SiteData").spin_t, "1790000002");
        assert_eq!(
            reloaded.relay_dtsg.as_ref().map(|t| t.expose()),
            Some("relay:two")
        );
        assert_eq!(
            reloaded.web_session.as_deref(),
            Some("abc123:def456:jkl012")
        );
        drop(slot);
        headless.release(None).await;
    }

    /// Once the tab is on the site, the document it is on holds what the
    /// app's calls on it carried — the Relay form, the route envelope, the
    /// counter after the app's, the variables of a registry query — and a
    /// call built from it copies them: the browser hands each call's form to
    /// the listener.
    #[tokio::test]
    async fn the_apps_calls_are_kept_by_the_document_that_sent_them() {
        use snob_ig::allowlist::Operation;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        let page = document(
            42,
            "some.viewer",
            1_790_000_001,
            "relay:one",
            "abc123:def456:ghi789",
        );
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html"))
            .with_priority(1)
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Acalls%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let account = headless.account(Pk::new(42));
        let mut slot = account.live.lock().await;
        let live = slot.as_mut().expect("the browser");

        let hover = Operation::HoverCard;
        let (relay, routes) = built_from_the_apps_calls(live).await;
        assert_eq!(relay.field("__req"), Some("d"), "after the app's c");
        assert_eq!(relay.field("__dyn"), Some("7xeUmwlE"));
        assert_eq!(relay.field("__sjsp"), Some("g4o"));
        assert_eq!(relay.field("__ccg"), Some("EXCELLENT"));
        assert_eq!(relay.field("__s"), Some("abc123:def456:ghi789"));
        assert_eq!(relay.field("fb_dtsg"), Some("relay:one"));
        assert_eq!(routes.field("routing_namespace"), Some("igx_www"));
        assert_eq!(routes.field("__req"), Some("d"));
        assert_eq!(
            lock(&live.listened.documents).template(hover),
            Some(r#"{"id":"2345678901","app_flag":true}"#)
        );
        drop(slot);
        headless.release(None).await;
    }

    /// A Relay call of the app's that the tab's guard refuses still leaves
    /// its form on the document that sent it: the browser tells of a request
    /// before it pauses it. So a document whose only Relay call is refused
    /// is not one a call can never be built on.
    #[tokio::test]
    async fn a_call_the_guard_refuses_still_leaves_its_form() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let invented = "SomeBadgeQuery";
        let boot = [
            app_post(
                "/api/graphql",
                Some(invented),
                &app_relay_form(
                    invented,
                    "1000000000000001",
                    "relay:one",
                    "abc123:def456:ghi789",
                ),
            ),
            app_post(
                "/ajax/bulk-route-definitions/",
                None,
                &app_routes_form("relay:one"),
            ),
        ]
        .join("\n");
        let page = shaped(42, "some.viewer", 1_790_000_001, "relay:one", &boot);
        let server = site(None).await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html"))
            .with_priority(1)
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Arefused%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let account = headless.account(Pk::new(42));
        let mut slot = account.live.lock().await;
        let live = slot.as_mut().expect("the browser");

        let (relay, _) = built_from_the_apps_calls(live).await;
        assert_eq!(relay.field("__dyn"), Some("7xeUmwlE"));
        assert_eq!(relay.field("__req"), Some("d"));
        let refused = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/api/graphql")
            .count();
        assert_eq!(refused, 0, "the invented call never left");
        drop(slot);
        headless.release(None).await;
    }

    /// A Relay query of the registry's and a route call, built from the
    /// tab's current document and the app's calls kept for it, once both
    /// can be: the app sends its calls as its document boots.
    async fn built_from_the_apps_calls(
        live: &mut Live,
    ) -> (snob_ig::web::Built, snob_ig::web::Built) {
        use snob_ig::web::{AppCalls, Context};

        let hover = snob_ig::allowlist::Operation::HoverCard;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let values = live.page_values().await.unwrap();
            let loader = live.world.clone().expect("the document's world").0;
            let calls = lock(&live.listened.documents)
                .get(&loader)
                .map(|document| document.calls.clone());
            if let Some(calls) = calls {
                let context = |app: &mut AppCalls| -> (Result<_, _>, Result<_, _>) {
                    let mut relay = app.clone();
                    let mut routes = app.clone();
                    let relay = Context {
                        page: &values,
                        app: &mut relay,
                        csrf: None,
                        claim: "0",
                    }
                    .relay(hover, r#"{"id":"2345678901"}"#, None, "/", None);
                    let routes = Context {
                        page: &values,
                        app: &mut routes,
                        csrf: None,
                        claim: "0",
                    }
                    .bulk_route_definitions(&["/someone/"], "/");
                    (relay, routes)
                };
                if let (Ok(relay), Ok(routes)) = context(&mut calls.clone()) {
                    return (relay, routes);
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the app's calls never reached the document it is on"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// An intent for the tab, as `server`'s client would hand it over.
    fn intent(server: &wiremock::MockServer, ask: Ask, referrer: &str) -> Call {
        Call {
            ask,
            origin: server.uri(),
            referrer: referrer.to_string(),
            claim: "0".to_string(),
            cap: 1024 * 1024,
            timeout_ms: 20_000,
        }
    }

    /// The form of a POST `server` received.
    fn form_of(request: &wiremock::Request) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect()
    }

    fn field<'a>(form: &'a [(String, String)], name: &str) -> Option<&'a str> {
        form.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The Relay forms `server` received whose variables are `variables`:
    /// snob's, told from the app's by what they ask.
    async fn relay_sent(
        server: &wiremock::MockServer,
        variables: &str,
    ) -> Vec<Vec<(String, String)>> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/graphql")
            .map(form_of)
            .filter(|form| field(form, "variables") == Some(variables))
            .collect()
    }

    /// A route definitions answer naming `pk` for `route`, invented in the
    /// Comet shape.
    fn routes_answer(route: &str, pk: u64) -> String {
        format!(
            r#"for (;;);{{"payload":{{"payloads":{{"{route}":{{"error":false,"result":{{"type":"route_definition","exports":{{"hostableView":{{"props":{{"id":"{pk}"}}}}}}}}}}}}}}}}"#
        )
    }

    /// Each kind of intent, answered by the tab on a document whose app has
    /// sent its calls: the viewer and the tray from the document; a query
    /// built after the app's counter from the document's own tokens; a REST
    /// read with the app's web session id; a name through the route
    /// definitions; a profile loaded, with its bundles and without its HTML.
    /// And a plain request the allowlist refuses never reaches the site.
    #[tokio::test]
    async fn an_intent_is_built_in_the_tab_beside_the_pages_values() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        // The CSRF token a request built like the app's carries is the
        // cookie's, which the site hands out with its page.
        let html = |body: String| {
            ResponseTemplate::new(200)
                .set_body_raw(body, "text/html")
                .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/")
        };
        let home = document(
            42,
            "some.viewer",
            1_790_000_001,
            "relay:one",
            "abc123:def456:ghi789",
        ) + r#"<script type="application/json">{"preload":{"data":{"xdt_api__v1__feed__reels_tray":{"tray":[{"id":"9001","user":{"username":"someone"}},{"id":"9002"}]}}}}</script>"#;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(home))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/someone/"))
            .respond_with(html(
                shaped(42, "some.viewer", 1_790_000_002, "relay:two", "")
                    + r#"<script type="application/json">{"bundle":"https:\/\/static.cdninstagram.com\/rsrc.php\/v4\/yX\/r\/invented.js"}</script>"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ajax/bulk-route-definitions/"))
            .and(body_string_contains("route_urls%5B0%5D=%2Fsomeone%2F"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(routes_answer("/someone/", 9001)),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/9001/following/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aasks%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let ask =
            |ask: Ask, referrer: &str| headless.ask_as(&session, intent(&server, ask, referrer));

        let told = ask(Ask::Viewer, "/").await.unwrap();
        assert!(
            matches!(&told, Told::Viewer(v) if v.pk == Pk::new(42) && v.username == "some.viewer"),
            "{told:?}"
        );
        let told = ask(Ask::Tray, "/").await.unwrap();
        assert!(
            matches!(&told, Told::Tray(Some(ids)) if ids == &["9001", "9002"]),
            "{told:?}"
        );

        let hover = r#"{"userID":"9001"}"#;
        let told = ask(
            Ask::Query {
                operation: Operation::HoverCard,
                variables: hover.into(),
            },
            "/someone/",
        )
        .await
        .unwrap();
        assert!(
            matches!(&told, Told::Answer(a) if a.status == 200),
            "{told:?}"
        );
        let sent = relay_sent(&server, hover).await;
        assert_eq!(sent.len(), 1);
        let form = &sent[0];
        assert_eq!(field(form, "__req"), Some("d"), "after the app's c");
        assert_eq!(field(form, "av"), Some("17841400000000001"));
        assert_eq!(field(form, "lsd"), Some("lsd"));
        assert_eq!(field(form, "fb_dtsg"), Some("relay:one"));
        assert_eq!(field(form, "__s"), Some("abc123:def456:ghi789"));

        let told = ask(
            Ask::Rest {
                read: snob_ig::allowlist::Rest::Following(Pk::new(9001)),
                query: vec![("count".into(), "12".into())],
            },
            "/someone/",
        )
        .await
        .unwrap();
        assert!(
            matches!(&told, Told::Answer(a) if a.body == r#"{"users":[]}"#),
            "{told:?}"
        );
        let received = server.received_requests().await.unwrap_or_default();
        let rest = received
            .iter()
            .find(|r| r.url.path() == "/api/v1/friendships/9001/following/")
            .expect("the REST read");
        assert_eq!(rest.url.query(), Some("count=12"));
        assert_eq!(
            rest.headers
                .get("x-web-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("abc123:def456:ghi789")
        );

        let told = ask(
            Ask::Pk {
                name: "someone".into(),
            },
            "/",
        )
        .await
        .unwrap();
        assert!(
            matches!(&told, Told::Pk { pk: RouteAnswer::Pk(pk), .. } if *pk == Pk::new(9001)),
            "{told:?}"
        );

        // A plain request the allowlist refuses: nothing reaches the site.
        for refused in [
            PageRequest {
                method: Method::Post,
                url: format!("{}/api/v1/web/something/", server.uri()),
                body: Some("a=b".into()),
                ..answer(&server)
            },
            PageRequest {
                url: format!("{}/stories/someone/", server.uri()),
                navigate: true,
                ..answer(&server)
            },
        ] {
            let error = headless.send_as(&session, refused).await.unwrap_err();
            assert!(matches!(error, PageError::NotAllowed(_)), "{error:?}");
        }
        // A navigation the allowlist lets out, sent as a request rather than
        // asked as a document: refused, and the profile is not loaded.
        let navigation = PageRequest {
            url: format!("{}/someone/", server.uri()),
            navigate: true,
            ..answer(&server)
        };
        let error = headless.send_as(&session, navigation).await.unwrap_err();
        assert!(
            matches!(&error, PageError::NotAllowed(what) if what == "a navigation is asked as a document"),
            "{error:?}"
        );
        let received = server.received_requests().await.unwrap_or_default();
        assert!(
            received.iter().all(|r| r.url.path() != "/someone/"),
            "the profile was loaded"
        );

        let told = ask(
            Ask::Document {
                path: "/someone/".into(),
            },
            "/",
        )
        .await
        .unwrap();
        let Told::Document { answer, bundles } = told else {
            panic!("a document is answered as one: {told:?}");
        };
        assert_eq!(answer.status, 200);
        assert!(answer.body.is_empty(), "the HTML stays in the tab");
        assert_eq!(
            bundles,
            ["https://static.cdninstagram.com/rsrc.php/v4/yX/r/invented.js"]
        );
        // The tab is on the profile, whose document preloads no tray.
        assert!(matches!(
            ask(Ask::Tray, "/").await.unwrap(),
            Told::Tray(None)
        ));

        let received = server.received_requests().await.unwrap_or_default();
        for never in ["/api/v1/web/something/", "/stories/someone/"] {
            assert!(
                received.iter().all(|r| r.url.path() != never),
                "{never} was reached"
            );
        }
        headless.release(None).await;
    }

    /// A read is built the way the app builds it on the page the tab is on:
    /// the `Referer` and the route are the tab's, though it was asked from a
    /// profile it never went to; the app's flags, its identifiers and the
    /// number it sent the read under (which the registry does not hold) are
    /// copied; and of the app's own calls the tab lets out the boot reads it
    /// names and no other.
    #[tokio::test]
    async fn a_read_follows_the_page_the_tab_is_on_and_what_the_app_sent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let moved = "1000000000000777";
        let hover = Operation::HoverCard;
        let promotion = "QuickPromotionSupportIGSchemaBatchFetchQuery";
        let post = |name: &str, doc_id: &str, variables: &str| {
            let form =
                app_relay_form_with(name, doc_id, "relay:one", "abc123:def456:ghi789", variables)
                    + "&__crn=comet.igweb.PolarisFeedRoute";
            format!(
                "fetch('/api/graphql', {{ method: 'POST', headers: {{ \
                 'Content-Type': 'application/x-www-form-urlencoded', 'X-FB-LSD': 'lsd', \
                 'X-FB-Friendly-Name': {}, 'X-IG-App-ID': '111222333444555', \
                 'X-ASBD-ID': '777888' }}, body: {} }});",
                json!(name),
                json!(form)
            )
        };
        let boot = [
            post(
                hover.friendly_name(),
                moved,
                r#"{"userID":"1","app_flag":true}"#,
            ),
            post(promotion, "26673487622279953", r#"{"scale":1}"#),
            post(
                "PolarisInventedBadgeQuery",
                "1000000000000001",
                r#"{"x":1}"#,
            ),
            app_post(
                "/ajax/bulk-route-definitions/",
                None,
                &app_routes_form("relay:one"),
            ),
        ]
        .join("\n");
        let page = shaped(42, "some.viewer", 1_790_000_001, "relay:one", &boot);
        let server = site(None).await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(page, "text/html")
                    .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/"),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Afollows%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let asked = r#"{"userID":"9001"}"#;
        let told = headless
            .ask_as(
                &session,
                intent(
                    &server,
                    Ask::Query {
                        operation: hover,
                        variables: asked.into(),
                    },
                    "/someone/",
                ),
            )
            .await
            .unwrap();
        assert!(
            matches!(&told, Told::Answer(a) if a.status == 200),
            "{told:?}"
        );

        let received = server.received_requests().await.unwrap_or_default();
        let ours: Vec<&wiremock::Request> = received
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/api/graphql")
            .filter(|r| {
                field(&form_of(r), "variables") == Some(r#"{"userID":"9001","app_flag":true}"#)
            })
            .collect();
        assert_eq!(
            ours.len(),
            1,
            "{:?}",
            received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
        );
        let form = form_of(ours[0]);
        assert_eq!(field(&form, "doc_id"), Some(moved));
        assert_eq!(field(&form, "__crn"), Some("comet.igweb.PolarisFeedRoute"));
        let header = |name: &str| ours[0].headers.get(name).and_then(|v| v.to_str().ok());
        assert_eq!(
            header("referer"),
            Some(format!("{}/", server.uri()).as_str()),
            "from the page the tab is on"
        );
        assert_eq!(header("x-ig-app-id"), Some("111222333444555"));
        assert_eq!(header("x-asbd-id"), Some("777888"));

        // The app's own boot reads: the promotion is let out, the invented
        // badge is not.
        let named = |name: &str| {
            received
                .iter()
                .filter(|r| r.method.as_str() == "POST")
                .filter(|r| field(&form_of(r), "fb_api_req_friendly_name") == Some(name))
                .count()
        };
        assert_eq!(named(promotion), 1, "the app's promotion read");
        assert_eq!(
            named("PolarisInventedBadgeQuery"),
            0,
            "a call the app invented"
        );
        headless.release(None).await;
    }

    /// A file is fetched by the tab and comes back whole, bytes of every
    /// value, as base64; one past the cap comes back without a body, one
    /// from nowhere the rule allows is never fetched, and the answer is not
    /// read as Instagram's.
    #[tokio::test]
    async fn an_asset_is_fetched_by_the_tab_and_comes_back_whole() {
        use base64::Engine as _;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        let bytes: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        Mock::given(method("GET"))
            .and(path("/pic.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(bytes.clone(), "image/jpeg"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/big.bin"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(vec![7u8; 1024 * 1024 + 1], "image/jpeg"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone.bin"))
            .respond_with(ResponseTemplate::new(403).set_body_string("expired"))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aassets%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let asset = |name: &str| {
            headless.ask_as(
                &session,
                intent(
                    &server,
                    Ask::Asset {
                        url: format!("{}/{name}", server.uri()),
                    },
                    "/",
                ),
            )
        };

        let Told::Asset(whole) = asset("pic.bin").await.unwrap() else {
            panic!("an asset is answered as one");
        };
        assert_eq!(whole.status, 200);
        assert!(!whole.too_large);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&whole.body)
            .unwrap();
        assert_eq!(decoded, bytes);

        let Told::Asset(big) = asset("big.bin").await.unwrap() else {
            panic!("an asset is answered as one");
        };
        assert!(big.too_large && big.body.is_empty(), "past the call's cap");

        // A status is the answer, and is no push-back of Instagram's.
        let Told::Asset(gone) = asset("gone.bin").await.unwrap() else {
            panic!("an asset is answered as one");
        };
        assert_eq!(gone.status, 403);

        let refused = headless
            .ask_as(
                &session,
                intent(
                    &server,
                    Ask::Asset {
                        url: "https://evil.test/x.jpg".into(),
                    },
                    "/",
                ),
            )
            .await;
        assert!(
            matches!(refused, Err(PageError::NotAllowed(_))),
            "{refused:?}"
        );

        let received = server.received_requests().await.unwrap_or_default();
        let fetched = received
            .iter()
            .find(|r| r.url.path() == "/pic.bin")
            .expect("the file was asked of the server");
        let header = |name: &str| fetched.headers.get(name).and_then(|v| v.to_str().ok());
        assert_eq!(header("sec-fetch-mode"), Some("cors"));
        assert_eq!(header("sec-fetch-dest"), Some("empty"));
        assert_eq!(
            header("referer"),
            Some(format!("{}/", server.uri()).as_str())
        );
        assert!(
            received
                .iter()
                .all(|r| r.url.host_str() != Some("evil.test"))
        );
        headless.release(None).await;
    }

    /// The owner log names, once for the document, the token the app's
    /// Relay and Comet calls carry: on the boot document, `DTSGInitialData`'s.
    /// And a query of an operation the app sent too names the keys each
    /// side's variables lack. Names only, never a token or a value.
    #[tokio::test]
    async fn the_boot_documents_token_is_named_in_the_owner_log() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        #[derive(Clone, Default)]
        struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                lock(&self.0).extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let server = site(None).await;
        let home = document(
            42,
            "some.viewer",
            1_790_000_001,
            "relay:one",
            "abc123:def456:ghi789",
        );
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(home, "text/html")
                    .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/"),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        // Every task of this test runs on its one thread. A second
        // dispatcher, alive for the test, keeps a test logging on another
        // thread meanwhile from caching the token's line as unwanted.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let _logged = tracing::subscriber::set_default(subscriber);
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Atoken%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let hover = Ask::Query {
            operation: Operation::HoverCard,
            variables: r#"{"userID":"9001"}"#.into(),
        };
        for _ in 0..2 {
            let told = headless
                .ask_as(&session, intent(&server, hover.clone(), "/"))
                .await
                .unwrap();
            assert!(matches!(told, Told::Answer(_)), "{told:?}");
        }
        headless.release(None).await;

        let said = String::from_utf8(lock(&lines.0).clone()).unwrap();
        let said_of = |what: &str| -> Vec<String> {
            said.lines()
                .filter(|line| line.contains(what))
                .map(str::to_string)
                .collect()
        };
        let token = said_of("the token the app's Relay and Comet calls carry");
        assert_eq!(token.len(), 1, "once for the document: {said}");
        assert!(token[0].contains("token=Relay"), "{}", token[0]);
        let keys = said_of("the keys snob's variables and the app's each lack");
        assert_eq!(keys.len(), 2, "{said}");
        assert!(
            keys[0].contains(r#"ours_lack=["id", "app_flag"]"#)
                && keys[0].contains(r#"apps_lack=["userID"]"#),
            "{}",
            keys[0]
        );
        for line in token.iter().chain(&keys) {
            for never in ["relay:one", "session", "9001", "2345678901"] {
                assert!(!line.contains(never), "{never} in {line}");
            }
        }
    }

    /// A document shaped like the profile of `pk` as viewer 42, whose app
    /// sends its own profile query by `app_id` as it boots, when given one.
    fn profile_document(pk: Option<u64>, app_id: Option<u64>) -> String {
        let profile = Operation::ProfilePage;
        let boot = app_id
            .map(|id| {
                app_post(
                    "/api/graphql",
                    Some(profile.friendly_name()),
                    &app_relay_form_with(
                        profile.friendly_name(),
                        profile.doc_id(),
                        "relay:two",
                        "abc123:def456:jkl012",
                        &format!(r#"{{"id":"{id}","a_flag_the_app_sends":true}}"#),
                    ),
                )
            })
            .unwrap_or_default();
        let props = pk
            .map(|pk| {
                format!(
                    r#"<script type="application/json">{{"require":[["RouteProps",null,null,[{{"rootView":{{"props":{{"id":"{pk}","page_logging":"profile"}}}}}}]]]}}</script>"#
                )
            })
            .unwrap_or_default();
        shaped(42, "some.viewer", 1_790_000_002, "relay:two", &boot) + &props
    }

    /// A write is built only on the profile document its command loaded,
    /// while the tab is still on it. Asked before any profile, once another
    /// page was loaded in between, as a second command on the account loads
    /// one, or once the browser was closed and a fresh one started on the
    /// home page, it is not built and nothing is sent. On the profile loaded
    /// again, it goes, made from it.
    #[tokio::test]
    async fn a_write_is_built_only_on_the_document_it_is_made_from() {
        use snob_ig::graphql::{Mutation, PROFILE_ROUTE};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        let html = |body: String| {
            ResponseTemplate::new(200)
                .set_body_raw(body, "text/html")
                .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/")
        };
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(document(
                42,
                "some.viewer",
                1_790_000_001,
                "relay:one",
                "abc123:def456:ghi789",
            )))
            .with_priority(1)
            .mount(&server)
            .await;
        for (at, pk) in [("/someone/", 9001), ("/other/", 9003)] {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(html(profile_document(Some(pk), Some(pk))))
                .mount(&server)
                .await;
        }
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Awrites%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let ask =
            |ask: Ask, referrer: &str| headless.ask_as(&session, intent(&server, ask, referrer));
        let follow = || {
            ask(
                Ask::Write {
                    mutation: Mutation::Follow,
                    variables: r#"{"target_user_id":"9001","container_module":"profile"}"#.into(),
                    doc_id: Mutation::Follow.seed_doc_id().into(),
                    route: PROFILE_ROUTE.into(),
                },
                "/someone/",
            )
        };
        let load = |at: &str| {
            ask(
                Ask::Document {
                    path: at.to_string(),
                },
                "/",
            )
        };
        let writes = async || -> Vec<wiremock::Request> {
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| {
                    r.method.as_str() == "POST"
                        && field(&form_of(r), "fb_api_req_friendly_name")
                            == Some(Mutation::Follow.friendly_name())
                })
                .collect()
        };
        let not_built = |told: Result<Told, PageError>| {
            assert!(matches!(&told, Err(PageError::NotReady(_))), "{told:?}");
        };

        not_built(follow().await);
        load("/someone/").await.unwrap();
        load("/other/").await.unwrap();
        not_built(follow().await);
        assert!(writes().await.is_empty(), "nothing was sent");

        load("/someone/").await.unwrap();
        let told = follow().await.unwrap();
        assert!(
            matches!(&told, Told::Answer(a) if a.status == 200),
            "{told:?}"
        );
        let sent = writes().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(
            field(&form_of(&sent[0]), "__crn"),
            Some(PROFILE_ROUTE),
            "on the profile's route"
        );
        assert!(
            sent[0]
                .headers
                .get("referer")
                .and_then(|r| r.to_str().ok())
                .is_some_and(|r| r.ends_with("/someone/")),
            "made from the profile"
        );

        // The browser closed while the command waited: the next starts on
        // the home page, and builds nothing there.
        headless.release(None).await;
        not_built(follow().await);
        assert_eq!(writes().await.len(), 1, "nothing more was sent");
        headless.release(None).await;
    }

    /// A write asked after a profile document comes after the app's calls
    /// on it, one that leaves after the tab's [`SETTLE`] among them: its
    /// `__req` is past every one of theirs.
    #[tokio::test]
    async fn a_write_follows_the_apps_calls_on_the_profile() {
        use snob_ig::graphql::{Mutation, PROFILE_ROUTE};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        let hover = Operation::HoverCard;
        let late = app_relay_form_with(
            hover.friendly_name(),
            hover.doc_id(),
            "relay:two",
            "abc123:def456:jkl012",
            r#"{"userID":"9001"}"#,
        )
        .replace("__req=c", "__req=d");
        let later = format!(
            "setTimeout(() => {{ {} }}, 2000);",
            app_post("/api/graphql", Some(hover.friendly_name()), &late)
        );
        // The app's boot, with a call that leaves half a second after the
        // tab has settled on the document.
        let boot = profile_document(None, Some(9001));
        let profile = format!(
            "{}{later}</script>",
            boot.strip_suffix("</script>")
                .expect("the boot ends the document")
        );
        Mock::given(method("GET"))
            .and(path("/someone/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(profile, "text/html")
                    .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/"),
            )
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Asettled%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let ask =
            |ask: Ask, referrer: &str| headless.ask_as(&session, intent(&server, ask, referrer));

        ask(
            Ask::Document {
                path: "/someone/".into(),
            },
            "/",
        )
        .await
        .unwrap();
        let told = ask(
            Ask::Write {
                mutation: Mutation::Follow,
                variables: r#"{"target_user_id":"9001","container_module":"profile"}"#.into(),
                doc_id: Mutation::Follow.seed_doc_id().into(),
                route: PROFILE_ROUTE.into(),
            },
            "/someone/",
        )
        .await
        .unwrap();
        assert!(
            matches!(&told, Told::Answer(a) if a.status == 200),
            "{told:?}"
        );
        let received = server.received_requests().await.unwrap_or_default();
        let named = |name: &str| {
            received
                .iter()
                .position(|r| field(&form_of(r), "fb_api_req_friendly_name") == Some(name))
        };
        let write = named(Mutation::Follow.friendly_name()).expect("the write");
        let the_late_call = named(hover.friendly_name()).expect("the app's late call");
        assert!(the_late_call < write, "the app's late call came first");
        assert_eq!(
            field(&form_of(&received[write]), "__req"),
            Some("e"),
            "past the app's c and d"
        );
        headless.release(None).await;
    }

    /// With no route call of the app's to copy, a name is read from its
    /// profile document: the route props it embeds, checked against the
    /// id of the app's own profile query when it sends one, and two
    /// sources that disagree are no answer. The profile query snob sends
    /// after it copies the app's variables, with only the id its own.
    #[tokio::test]
    async fn a_name_is_read_from_its_profile_document() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        // The CSRF token a request built like the app's carries is the
        // cookie's, which the site hands out with its page.
        let html = |body: String| {
            ResponseTemplate::new(200)
                .set_body_raw(body, "text/html")
                .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/")
        };
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(shaped(
                42,
                "some.viewer",
                1_790_000_001,
                "relay:one",
                "",
            )))
            .with_priority(1)
            .mount(&server)
            .await;
        for (at, pk, app) in [
            ("/someone/", Some(9001), Some(9001)),
            ("/props.only/", Some(9002), None),
            ("/other/", Some(9003), Some(9004)),
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(html(profile_document(pk, app)))
                .mount(&server)
                .await;
        }
        // Instagram's page for a name nobody owns: the app's, with a 404.
        Mock::given(method("GET"))
            .and(path("/nobody/"))
            .respond_with(ResponseTemplate::new(404).set_body_raw(
                shaped(42, "some.viewer", 1_790_000_003, "relay:three", ""),
                "text/html",
            ))
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Anames%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let ask = |ask: Ask| headless.ask_as(&session, intent(&server, ask, "/"));
        let pk_of = |name: &str| {
            ask(Ask::Pk {
                name: name.to_string(),
            })
        };

        for (name, expected) in [
            ("someone", RouteAnswer::Pk(Pk::new(9001))),
            ("props.only", RouteAnswer::Pk(Pk::new(9002))),
            ("other", RouteAnswer::Error),
        ] {
            let told = pk_of(name).await.unwrap();
            let Told::Pk { answer, pk } = told else {
                panic!("a name is answered with a pk: {told:?}");
            };
            assert_eq!(pk, expected, "{name}");
            assert!(answer.body.is_empty(), "the HTML stays in the tab");
        }

        // A name nobody owns: its 404 is handed back, with no wait for a
        // profile query the app never sends from it.
        let waited = std::time::Instant::now();
        let told = pk_of("nobody").await.unwrap();
        let Told::Pk { answer, pk } = told else {
            panic!("a name is answered with a pk: {told:?}");
        };
        assert_eq!(answer.status, 404);
        assert_eq!(pk, RouteAnswer::Error);
        assert!(waited.elapsed() < APP_CALLS_PATIENCE);

        // Back on a profile whose app sent its query, snob's copies it.
        pk_of("someone").await.unwrap();
        let told = ask(Ask::Query {
            operation: Operation::ProfilePage,
            variables: r#"{"id":"9005"}"#.into(),
        })
        .await
        .unwrap();
        assert!(
            matches!(&told, Told::Answer(a) if a.status == 200),
            "{told:?}"
        );
        let copied = r#"{"id":"9005","a_flag_the_app_sends":true}"#;
        assert_eq!(relay_sent(&server, copied).await.len(), 1);
        headless.release(None).await;
    }

    /// A page the app's calls have not filled sends nothing, says it is not
    /// ready once the wait is over, and keeps its browser; a page signed out
    /// sends nothing either, and says so.
    #[tokio::test]
    async fn a_page_not_ready_or_signed_out_sends_nothing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(None).await;
        // The CSRF token a request built like the app's carries is the
        // cookie's, which the site hands out with its page.
        let html = |body: String| {
            ResponseTemplate::new(200)
                .set_body_raw(body, "text/html")
                .insert_header("Set-Cookie", "csrftoken=from-the-cookie; Path=/")
        };
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(html(shaped(
                42,
                "some.viewer",
                1_790_000_001,
                "relay:one",
                "",
            )))
            .with_priority(1)
            .mount(&server)
            .await;
        let signed_out = shaped(42, "some.viewer", 1_790_000_001, "relay:one", "").replace(
            r#"["PolarisViewer",[],{"data":{"id":"42","username":"some.viewer","fbid":"17841400000000001"},"id":"42"},1508]"#,
            r#"["PolarisViewer",[],{"data":null,"id":null},1508]"#,
        );
        assert!(signed_out.contains(r#"{"data":null,"id":null}"#));
        Mock::given(method("GET"))
            .and(path("/gone/"))
            .respond_with(html(signed_out))
            .mount(&server)
            .await;
        answer_the_app(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aready%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let ask = |ask: Ask| headless.ask_as(&session, intent(&server, ask, "/"));
        let hover = || Ask::Query {
            operation: Operation::HoverCard,
            variables: r#"{"userID":"9001"}"#.into(),
        };

        let waited = std::time::Instant::now();
        let error = ask(hover()).await.unwrap_err();
        assert!(
            matches!(&error, PageError::NotReady(what) if what == "the web session id"),
            "{error:?}"
        );
        assert!(waited.elapsed() >= APP_CALLS_PATIENCE);
        assert!(
            headless.account(Pk::new(42)).live.lock().await.is_some(),
            "the browser is kept"
        );
        assert!(matches!(ask(Ask::Viewer).await.unwrap(), Told::Viewer(_)));

        // A document served to nobody is loaded, and answered as logged out.
        let error = ask(Ask::Document {
            path: "/gone/".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(error, PageError::LoggedOut), "{error:?}");
        let error = ask(hover()).await.unwrap_err();
        assert!(matches!(error, PageError::LoggedOut), "{error:?}");
        let error = ask(Ask::Viewer).await.unwrap_err();
        assert!(matches!(error, PageError::LoggedOut), "{error:?}");
        let error = ask(Ask::Pk {
            name: "someone".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(error, PageError::LoggedOut), "{error:?}");

        let requests = server.received_requests().await.unwrap_or_default();
        assert!(
            !requests.iter().any(|r| r.url.path() == "/someone/"),
            "no profile is loaded for a name"
        );
        let posted = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .count();
        assert_eq!(posted, 0, "nothing was sent");
        headless.release(None).await;
    }

    /// The page fills in the CSRF token and the claim only where a request
    /// names them: a Relay call gets no claim though the page keeps one, and
    /// a route call no CSRF token, as the app sends them.
    #[tokio::test]
    async fn the_page_adds_no_header_a_request_does_not_carry() {
        use snob_ig::client::page::Method::{Get, Post};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = site(Some("csrftoken=from-the-cookie; Path=/")).await;
        for posted in ["/api/graphql", "/ajax/bulk-route-definitions/"] {
            Mock::given(method("POST"))
                .and(path(posted))
                .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
                .mount(&server)
                .await;
        }
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aheaders%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        {
            let account = headless.account(Pk::new(42));
            let slot = account.live.lock().await;
            let live = slot.as_ref().expect("the browser");
            tab::evaluate(
                &live.cdp,
                &live.tab,
                "sessionStorage.setItem('www-claim-v2', 'hmac.kept-by-the-page')",
                COMMAND_TIMEOUT,
            )
            .await
            .unwrap();
        }
        let request = |target: &str, method, headers: &[(&str, &str)], body: Option<String>| {
            let mut request = answer(&server);
            request.url = format!("{}{target}", server.uri());
            request.method = method;
            request.headers = headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect();
            request.body = body;
            request
        };
        let form = ("Content-Type", "application/x-www-form-urlencoded");
        // Forms the tab's guard lets through: one of the registry's queries,
        // and the route definitions.
        let hover = snob_ig::allowlist::Operation::HoverCard;
        let relay = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("fb_api_req_friendly_name", hover.friendly_name()),
                ("variables", r#"{"id":"2345678901"}"#),
                ("doc_id", hover.doc_id()),
            ])
            .finish();
        let routes = "route_urls[0]=%2Fsomeone%2F&routing_namespace=igx_www&__a=1".to_string();
        for sent in [
            request(
                "/api/graphql",
                Post,
                &[
                    ("X-FB-LSD", "lsd"),
                    ("X-CSRFToken", ""),
                    ("X-FB-Friendly-Name", hover.friendly_name()),
                    form,
                ],
                Some(relay),
            ),
            request(
                "/ajax/bulk-route-definitions/",
                Post,
                &[("X-FB-LSD", "lsd"), ("X-IG-D", "www"), form],
                Some(routes),
            ),
            request(
                "/api/v1/friendships/42/following/",
                Get,
                &[("X-CSRFToken", ""), ("X-IG-WWW-Claim", "0")],
                None,
            ),
        ] {
            headless.send_as(&session, sent).await.unwrap();
        }

        let received = server.received_requests().await.unwrap_or_default();
        let last = |at: &str| {
            let request = received
                .iter()
                .rev()
                .find(|r| r.url.path() == at)
                .unwrap_or_else(|| panic!("nothing reached {at}"));
            let header = |name: &str| {
                request
                    .headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            (header("x-csrftoken"), header("x-ig-www-claim"))
        };
        let cookie = Some("from-the-cookie".to_string());
        assert_eq!(last("/api/graphql"), (cookie.clone(), None));
        assert_eq!(last("/ajax/bulk-route-definitions/"), (None, None));
        assert_eq!(
            last("/api/v1/friendships/42/following/"),
            (cookie, Some("hmac.kept-by-the-page".to_string()))
        );
        headless.release(None).await;
    }

    /// The owner log says, for each read that carries a claim, which claim
    /// went out (`0`, the one snob kept, or the one the page keeps under
    /// `www-claim-v2`) and whether its answer handed out a new one. Never the
    /// claim.
    #[tokio::test]
    async fn which_claim_a_read_carried_is_named_in_the_owner_log() {
        use snob_ig::client::page::Method::Get;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        #[derive(Clone, Default)]
        struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                lock(&self.0).extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let server = site(Some("csrftoken=from-the-cookie; Path=/")).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/followers/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/mutual_followers/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{}")
                    .insert_header("x-ig-set-www-claim", "hmac.handed-out"),
            )
            .mount(&server)
            .await;
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        // Every task of this test runs on its one thread. A second
        // dispatcher, alive for the test, keeps a test logging on another
        // thread meanwhile from caching the claim's line as unwanted.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let _logged = tracing::subscriber::set_default(subscriber);
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aclaims%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let read = |list: &str, claim: &str| {
            let mut request = answer(&server);
            request.url = format!("{}/api/v1/friendships/42/{list}/", server.uri());
            request.method = Get;
            request.headers = vec![
                ("X-CSRFToken".to_string(), String::new()),
                ("X-IG-WWW-Claim".to_string(), claim.to_string()),
            ];
            request.body = None;
            request
        };
        for (list, claim) in [
            ("followers", "0"),
            ("followers", "hmac.kept-by-snob"),
            ("mutual_followers", "0"),
            ("followers", "0"),
        ] {
            headless.send_as(&session, read(list, claim)).await.unwrap();
        }
        headless.release(None).await;

        let said = String::from_utf8(lock(&lines.0).clone()).unwrap();
        let claims: Vec<&str> = said
            .lines()
            .filter(|line| line.contains("the claim a read from the tab carried"))
            .collect();
        assert_eq!(claims.len(), 4, "{said}");
        for (line, (sent, set)) in claims.iter().zip([
            ("sent=0", "set=false"),
            ("sent=snob", "set=false"),
            ("sent=0", "set=true"),
            ("sent=page", "set=false"),
        ]) {
            assert!(
                line.contains(sent) && line.contains(set),
                "{sent} {set}: {line}"
            );
            assert!(!line.contains("hmac"), "never the claim: {line}");
        }
    }

    /// The guard on the wire: what the page's own script sends is paused and
    /// judged before it leaves. A Relay call the registry does not name, the
    /// view-count, sync and event-log paths and a piece of video never reach
    /// the site; a query of the registry's and a REST read do.
    #[tokio::test]
    async fn the_tab_lets_out_only_what_the_allowlist_names() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let hover = snob_ig::allowlist::Operation::HoverCard;
        let relay = |name: &str, doc_id: &str| {
            url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("fb_api_req_friendly_name", name),
                    ("variables", r#"{"id":"2345678901"}"#),
                    ("doc_id", doc_id),
                ])
                .finish()
        };
        let post = |to: &str, name: &str, form: &str| {
            format!(
                "['{to}', {{ method: 'POST', headers: {{ 'Content-Type': \
                 'application/x-www-form-urlencoded', 'X-FB-Friendly-Name': '{name}' }}, \
                 body: '{form}' }}]"
            )
        };
        // One after the other, so that once the last has arrived every one
        // before it has been judged.
        let calls = [
            post(
                "/api/graphql",
                "SomeBadgeQuery",
                &relay("SomeBadgeQuery", "1000000000000001"),
            ),
            post("/video/x/", "", "a=b"),
            post("/sync/instagram/", "", "a=b"),
            post("/logging_client_events", "", "a=b"),
            "['/clip.mp4?part=1', {}]".to_string(),
            post(
                "/api/graphql",
                hover.friendly_name(),
                &relay(hover.friendly_name(), hover.doc_id()),
            ),
            "['/api/v1/probe/', {}]".to_string(),
        ];
        let page = format!(
            "<!doctype html><title>site</title><script>(async () => {{ \
             for (const [to, how] of [{}]) {{ try {{ await fetch(to, how); }} catch (e) {{}} }} \
             }})();</script>",
            calls.join(", ")
        );
        let server = site(None).await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page, "text/html"))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let headless = Headless::new(AppPaths::rooted_at(tmp.path()));
        let session = made("42%3Aguard%3A1", 1_000);
        if !started(&headless, &session, answer(&server)).await {
            return;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let received = loop {
            let received = server.received_requests().await.unwrap_or_default();
            if received.iter().any(|r| r.url.path() == "/api/v1/probe/") {
                break received;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the page's last call never arrived"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let reached: Vec<String> = received
            .iter()
            .filter(|r| r.url.path() != "/")
            .map(|r| {
                let form = String::from_utf8_lossy(&r.body);
                let name = url::form_urlencoded::parse(form.as_bytes())
                    .find(|(n, _)| n == "fb_api_req_friendly_name")
                    .map(|(_, v)| format!(" {v}"))
                    .unwrap_or_default();
                format!("{} {}{name}", r.method, r.url.path())
            })
            .collect();
        for out in [
            format!("POST /api/graphql {}", hover.friendly_name()),
            "GET /api/v1/probe/".to_string(),
        ] {
            assert!(reached.contains(&out), "{out} never arrived: {reached:?}");
        }
        for kept in [
            "POST /api/graphql SomeBadgeQuery",
            "POST /video/x/",
            "POST /sync/instagram/",
            "POST /logging_client_events",
            "GET /clip.mp4",
        ] {
            assert!(
                !reached.iter().any(|r| r == kept),
                "{kept} got out: {reached:?}"
            );
        }
        headless.release(None).await;
    }

    /// How many times the site's page was loaded.
    async fn loads(server: &wiremock::MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/")
            .count()
    }

    /// `=0` and an empty value leave a switch off, as unset does.
    #[test]
    fn a_switch_is_on_only_when_it_says_so() {
        let on = |value: Option<&str>| is_on(value.map(std::ffi::OsStr::new));
        assert!(on(Some("1")));
        assert!(on(Some("yes")));
        assert!(!on(Some("0")));
        assert!(!on(Some("")));
        assert!(!on(None));
    }

    /// A protocol URL pattern, matched as the browser matches it: `*` any
    /// run, `?` one character, `\` the next one as itself.
    fn matches(pattern: &[char], url: &[char]) -> bool {
        match pattern {
            [] => url.is_empty(),
            ['*', rest @ ..] => (0..=url.len()).any(|i| matches(rest, &url[i..])),
            ['?', rest @ ..] => !url.is_empty() && matches(rest, &url[1..]),
            ['\\', c, rest @ ..] | [c, rest @ ..] => {
                url.first() == Some(c) && matches(rest, &url[1..])
            }
        }
    }

    fn refused_by_address(url: &str) -> bool {
        let url: Vec<char> = url.chars().collect();
        guard::video_patterns()
            .iter()
            .filter_map(|p| p["urlPattern"].as_str())
            .any(|p| matches(&p.chars().collect::<Vec<_>>(), &url))
    }

    /// Every piece of video on the CDN is refused by its address, and a
    /// username with `.mp4` in it is not: snob asks about names like that.
    #[test]
    fn video_is_refused_by_address_and_a_name_is_not() {
        for video in [
            "https://instagram.fmad3-1.fna.fbcdn.net/o1/v/t16/f2/m86/AQ.mp4?stp=dst&bytestart=0",
            "https://scontent.cdninstagram.com/v/t50/x.m3u8?oh=1",
            "http://127.0.0.1:8080/v/t16/piece.webm?byteend=999",
        ] {
            assert!(refused_by_address(video), "{video}");
        }
        for own in [
            "https://www.instagram.com/clips.mp4/",
            "https://www.instagram.com/api/v1/users/web_profile_info/?username=clips.mp4",
            "https://www.instagram.com/api/v1/users/web_profile_info/?username=x.webm&count=1",
        ] {
            assert!(!refused_by_address(own), "{own}");
        }
    }
}
