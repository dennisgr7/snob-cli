//! The protocol connection: one reader, any number of callers.
//!
//! One task, the dispatcher, owns the read side and does nothing but route. A
//! reply goes to the caller registered under its id; an event goes to whoever
//! subscribed to its session; an attach and a paused request are answered
//! right there. That decides four things, each measured:
//!
//! - A target the browser attaches is let go the moment the attach is read,
//!   whether or not any call is waiting: a worker created one second into a
//!   six-second gap between calls, whose attach waited for the next caller to
//!   read the pipe, started 5.1 s late, where the dispatcher starts it in
//!   about ten milliseconds. A late worker is itself a timing tell.
//! - Answering an attach is never part of a caller's future, so a timeout or a
//!   Ctrl+C cannot leave a target paused for good.
//! - A crashed tab fails the calls waiting on it at once.
//! - A message past the ceiling fails the one call it answered.
//!
//! **The dispatcher never waits for a reply of its own** — it would be
//! waiting on itself — so what it sends in answer is written and not awaited,
//! and a failure among those is logged when its reply comes. It never panics
//! either: the release profile aborts on a panic, and everything it reads came
//! from a browser.
//!
//! A caller registers its id **before** the command is written, and the entry
//! is removed when its future is dropped, whatever the reason: a reply to a
//! call nobody waits for any more finds nothing and is dropped. So calls are
//! safe to cancel and may run at the same time, which is what several tabs on
//! one browser need.
//!
//! **No crate**, because none fits: `chromiumoxide` speaks only WebSocket and
//! enables `Runtime` on every frame, `headless_chrome` only a port, not
//! flattened, and `expect`s in its reader. Neither knows the inherited pipe
//! descriptors or the Windows job object, which are the hard part here.
//! Puppeteer's `Connection` and Playwright's `crConnection.ts` are the
//! structure this follows.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::pipe::{Frame, Message, PipeTransport};

/// Chromium's answer to a command for a session it does not know: the target
/// went away between the command being written and read.
const SESSION_NOT_FOUND: i64 = -32001;

/// Why a command came back without a result.
#[derive(Debug, Clone)]
pub enum CallError {
    /// The browser answered, with an error.
    Refused { method: String, error: Value },
    /// The target the command was for has gone: detached, crashed, or
    /// unknown to the browser by the time the command arrived.
    TargetGone { method: String },
    /// The answer was larger than the pipe carries, and was skipped.
    TooLarge { method: String },
    /// No answer in the time the caller gave it.
    TimedOut { method: String },
    /// The connection is over, and why.
    Closed { method: String, reason: String },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { method, error } => write!(f, "the browser refused {method}: {error}"),
            Self::TargetGone { method } => {
                write!(f, "the tab or worker {method} was sent to has gone")
            }
            Self::TooLarge { method } => write!(
                f,
                "the browser's answer to {method} was larger than {} MiB, and was dropped",
                crate::pipe::MAX_MESSAGE_BYTES / (1024 * 1024)
            ),
            Self::TimedOut { method } => write!(f, "the browser stopped answering ({method})"),
            Self::Closed { method, reason } => write!(
                f,
                "the browser closed the connection before answering {method}: {reason}"
            ),
        }
    }
}

impl std::error::Error for CallError {}

/// Something the browser said without being asked.
#[derive(Debug, Clone)]
pub struct Event {
    /// The session it came from; `None` for the browser's own.
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

/// The commands a newly attached target gets, by what kind of target it is.
///
/// **Why a browser engine needs this.** `Emulation.setUserAgentOverride` holds
/// for the one tab it was sent to. A worker the page starts, a frame from
/// another site and the service worker the site registers are targets of their
/// own, and each of them went out as the bare headless browser: measured on
/// Chromium 153, a service worker called itself `HeadlessChrome`. With
/// auto-attach, the browser pauses every such target before its first line
/// runs and tells this connection; the target is given these commands and then
/// let go.
#[derive(Debug, Clone, Default)]
pub struct OnAttach {
    /// For a page or a frame, which has the `Emulation` domain.
    pub page: Vec<(&'static str, Value)>,
    /// For a worker of any kind, which has `Network` and not `Emulation`.
    pub worker: Vec<(&'static str, Value)>,
}

/// What becomes of a request the browser paused for this connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paused {
    /// It goes on as it was written.
    Continue,
    /// It is failed as a content blocker fails one, for the reason given.
    Fail(String),
}

impl Paused {
    /// The reason every video the site would load is refused for. Too
    /// common to warn about: a feed asks for a piece of one at every scroll.
    pub const VIDEO: &'static str = "video";
}

/// Judges each request the browser pauses, from the `Fetch.requestPaused`
/// event's `params`. Run by the dispatcher, synchronously, so it answers at
/// once and never waits.
pub type PauseRule = Arc<dyn Fn(&Value) -> Paused + Send + Sync>;

/// An open connection to a browser. Cheap to clone; every clone talks to the
/// same dispatcher.
///
/// Dropping the last clone drops the sending half, which ends the writer
/// thread and closes the browser's command pipe; the dispatcher holds only a
/// weak reference, so it does not keep a connection nobody uses alive.
#[derive(Clone)]
pub struct Connection {
    shared: Arc<Shared>,
}

struct Shared {
    to_browser: mpsc::UnboundedSender<Message>,
    next_id: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    pending: HashMap<u64, Pending>,
    /// Why the connection ended, once it has. Every call from then on fails
    /// at once with it.
    closed: Option<String>,
    /// Sessions the browser has said are gone. A call on one fails at once
    /// rather than waiting out a timeout for an answer that cannot come.
    gone: HashSet<String>,
    subscribers: Vec<Subscriber>,
    on_attach: Option<OnAttach>,
    /// What a paused request is judged by; with none, every one is failed.
    pause_rule: Option<PauseRule>,
    /// How much of each reason for a refusal has been written to the log.
    refusals: RefusalLog,
    /// Every attached target, by id, and which of its sessions carries the
    /// identity.
    targets: HashMap<String, Target>,
    /// Which target each attached session belongs to.
    target_of: HashMap<String, String>,
}

/// How often a refusal is written to the log, per reason. A page that retries
/// a refused call (a player does, endlessly) would otherwise write the line
/// for every attempt: measured, 120 a second.
const REFUSAL_LOG_EVERY: Duration = Duration::from_secs(30);

/// The most reasons kept apart; the rest share one entry, so a reason made of
/// an address cannot grow the table without end.
const REFUSAL_REASONS: usize = 64;

/// What the log has been told about one reason for a refusal.
struct Told {
    at: std::time::Instant,
    /// Refusals since the line last written, that one included.
    since: u64,
}

/// Aggregates the refusals the log is told about: the first of a reason is
/// written at once, and then one line per reason every `REFUSAL_LOG_EVERY`
/// with how many there were in between.
#[derive(Default)]
struct RefusalLog {
    told: HashMap<String, Told>,
}

/// A line the log is to get.
struct Said {
    /// Refusals this line stands for.
    refused: u64,
}

impl RefusalLog {
    /// Counts one refusal; `Some` when a line is due for it.
    fn note(&mut self, reason: &str, now: std::time::Instant) -> Option<Said> {
        let key = if self.told.contains_key(reason) || self.told.len() < REFUSAL_REASONS {
            reason
        } else {
            "other reasons"
        };
        let Some(told) = self.told.get_mut(key) else {
            self.told
                .insert(key.to_string(), Told { at: now, since: 0 });
            return Some(Said { refused: 1 });
        };
        told.since += 1;
        if now.saturating_duration_since(told.at) < REFUSAL_LOG_EVERY {
            return None;
        }
        let refused = std::mem::take(&mut told.since);
        told.at = now;
        Some(Said { refused })
    }
}

/// A target and the sessions attached to it.
struct Target {
    kind: String,
    sessions: Vec<String>,
    /// The session the identity was given on.
    carrier: Option<String>,
}

/// A command written and not yet answered.
struct Pending {
    session: Option<String>,
    method: String,
    /// `None` for a command nobody waits for: its reply is only looked at to
    /// log a failure.
    reply: Option<oneshot::Sender<Result<Value, CallError>>>,
}

struct Subscriber {
    session: Option<String>,
    sender: mpsc::Sender<Event>,
}

/// What the dispatcher reads out of a message. Only the envelope; `params`
/// and `result` stay as they came.
#[derive(serde::Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<Value>,
}

impl Connection {
    /// Starts the dispatcher on a launched browser's pipe, or on the pair of
    /// channels the tests drive with a scripted browser.
    pub(crate) fn start(transport: PipeTransport) -> Self {
        let PipeTransport {
            to_browser,
            from_browser,
        } = transport;
        let shared = Arc::new(Shared {
            to_browser,
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
        });
        tokio::spawn(dispatch(Arc::downgrade(&shared), from_browser));
        Self { shared }
    }

    /// Sends one command and waits up to `timeout` for its answer.
    ///
    /// Safe to drop at any point: the id is registered before the command is
    /// written and forgotten when this future goes, so a late answer finds
    /// nobody and is dropped.
    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let (reply, answer) = oneshot::channel();
        let id = self.shared.register(session, method, Some(reply))?;
        let _forget = Forget {
            shared: &self.shared,
            id,
        };
        self.shared.write(id, session, method, params)?;
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(result)) => result,
            // The dispatcher is gone, and took the sender with it.
            Ok(Err(_)) => Err(CallError::Closed {
                method: method.to_string(),
                reason: self.shared.closed_reason(),
            }),
            Err(_) => Err(CallError::TimedOut {
                method: method.to_string(),
            }),
        }
    }

    /// Sends a command nobody waits for. A failure is logged when its reply
    /// comes; a command that could not even be written is logged now.
    pub fn fire(&self, session: Option<&str>, method: &str, params: Value) {
        self.shared.fire(session, method, params);
    }

    /// Everything the browser says on `session` (`None`: its own) from now
    /// on, up to `capacity` events behind.
    ///
    /// **Lossy on purpose.** An event is offered with `try_send`, and one that
    /// does not fit is dropped rather than holding up the dispatcher, which is
    /// also what carries every reply. A subscriber that must not miss
    /// anything keeps up, or asks for room. The channel ends when the session
    /// or the connection does.
    pub fn subscribe(&self, session: Option<&str>, capacity: usize) -> mpsc::Receiver<Event> {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let mut state = self.shared.lock();
        let over = state.closed.is_some() || session.is_some_and(|s| state.gone.contains(s));
        if !over {
            state.subscribers.push(Subscriber {
                session: session.map(str::to_string),
                sender,
            });
        }
        receiver
    }

    /// What every target attached from now on is given before it runs.
    pub fn set_on_attach(&self, plan: OnAttach) {
        self.shared.lock().on_attach = Some(plan);
    }

    /// What every request paused from now on is judged by.
    pub fn set_pause_rule(&self, rule: PauseRule) {
        self.shared.lock().pause_rule = Some(rule);
    }

    /// Why the connection ended, if it has.
    #[cfg(test)]
    fn closed(&self) -> Option<String> {
        self.shared.lock().closed.clone()
    }

    /// How many calls are waiting for an answer. For the tests, which check
    /// that a call given up on leaves nothing behind.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.shared.lock().pending.len()
    }
}

/// Removes a call's entry when its future goes, answered or not.
struct Forget<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.shared.lock().pending.remove(&self.id);
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn closed_reason(&self) -> String {
        self.lock()
            .closed
            .clone()
            .unwrap_or_else(|| "the connection ended".to_string())
    }

    /// Takes an id for a command about to be written, or says why there is no
    /// point writing it.
    fn register(
        &self,
        session: Option<&str>,
        method: &str,
        reply: Option<oneshot::Sender<Result<Value, CallError>>>,
    ) -> Result<u64, CallError> {
        let mut state = self.lock();
        if let Some(reason) = &state.closed {
            return Err(CallError::Closed {
                method: method.to_string(),
                reason: reason.clone(),
            });
        }
        if session.is_some_and(|s| state.gone.contains(s)) {
            return Err(CallError::TargetGone {
                method: method.to_string(),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.pending.insert(
            id,
            Pending {
                session: session.map(str::to_string),
                method: method.to_string(),
                reply,
            },
        );
        Ok(id)
    }

    fn write(
        &self,
        id: u64,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(), CallError> {
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.to_browser
            .send(message.to_string().into_bytes())
            .map_err(|_| CallError::Closed {
                method: method.to_string(),
                reason: "the pipe to the browser is closed".to_string(),
            })
    }

    fn fire(&self, session: Option<&str>, method: &str, params: Value) {
        let sent = self
            .register(session, method, None)
            .and_then(|id| self.write(id, session, method, params));
        if let Err(e) = sent {
            tracing::debug!(error = %e, "a command nobody waits for was not sent");
        }
    }

    /// One whole message off the pipe.
    fn handle(&self, bytes: &[u8]) {
        let Ok(message) = serde_json::from_slice::<Incoming>(bytes) else {
            tracing::debug!("the browser sent something that is not a protocol message");
            return;
        };
        if let Some(id) = message.id {
            self.answered(id, message.result, message.error);
            return;
        }
        let Some(method) = message.method else {
            return;
        };
        match method.as_str() {
            "Target.attachedToTarget" => self.attached(&message.params),
            "Target.detachedFromTarget" => {
                if let Some(gone) = message.params.get("sessionId").and_then(Value::as_str) {
                    self.detached(gone);
                    self.gone(gone);
                }
            }
            "Inspector.targetCrashed" => {
                if let Some(crashed) = &message.session {
                    tracing::debug!("a tab crashed");
                    self.gone(crashed);
                }
            }
            "Fetch.requestPaused" => {
                self.answer_paused(message.session.as_deref(), &message.params)
            }
            _ => {}
        }
        self.offer(message.session, method, message.params);
    }

    fn answered(&self, id: u64, result: Option<Value>, error: Option<Value>) {
        // Taken out under the lock and settled after it: settling can mark a
        // session gone, which takes the lock again.
        let Some(pending) = self.lock().pending.remove(&id) else {
            // A call nobody waits for any more.
            return;
        };
        let outcome = match error {
            Some(error) if error.get("code").and_then(Value::as_i64) == Some(SESSION_NOT_FOUND) => {
                if let Some(session) = &pending.session {
                    self.gone(session);
                }
                Err(CallError::TargetGone {
                    method: pending.method.clone(),
                })
            }
            Some(error) => Err(CallError::Refused {
                method: pending.method.clone(),
                error,
            }),
            None => Ok(result.unwrap_or(Value::Null)),
        };
        match pending.reply {
            Some(reply) => {
                let _ = reply.send(outcome);
            }
            None => {
                if let Err(e) = outcome {
                    tracing::debug!(error = %e, "a command nobody waits for failed");
                }
            }
        }
    }

    /// Fails every call on a session the browser has let go of, and every
    /// call on it from now on.
    fn gone(&self, session: &str) {
        let failed: Vec<Pending> = {
            let mut state = self.lock();
            state.gone.insert(session.to_string());
            state
                .subscribers
                .retain(|s| s.session.as_deref() != Some(session));
            let ids: Vec<u64> = state
                .pending
                .iter()
                .filter(|(_, p)| p.session.as_deref() == Some(session))
                .map(|(id, _)| *id)
                .collect();
            ids.iter()
                .filter_map(|id| state.pending.remove(id))
                .collect()
        };
        for pending in failed {
            if let Some(reply) = pending.reply {
                let _ = reply.send(Err(CallError::TargetGone {
                    method: pending.method,
                }));
            }
        }
    }

    /// A message past the ceiling: the call it answered fails, and nothing
    /// else does.
    fn oversized(&self, head: &[u8]) {
        let Some(id) = id_in_head(head) else {
            tracing::debug!("an event past the ceiling was dropped");
            return;
        };
        let Some(pending) = self.lock().pending.remove(&id) else {
            return;
        };
        let failure = CallError::TooLarge {
            method: pending.method,
        };
        match pending.reply {
            Some(reply) => {
                let _ = reply.send(Err(failure));
            }
            None => tracing::debug!(error = %failure, "a command nobody waits for failed"),
        }
    }

    /// Gives a target the browser has just paused its commands, and lets it go.
    ///
    /// Nothing here waits for a reply: the commands are written in order on
    /// the target's own session, and the last of them lets it run. A target
    /// attached before any plan is set is only registered.
    ///
    /// **A target is dressed once, and released on every session.** The
    /// service worker a site registers is attached twice, once by each
    /// auto-attach, and two things were measured on Chromium 141:
    ///
    /// - Released on only one of its sessions, the worker stayed paused; so
    ///   every attach that is waiting is released.
    /// - The identity lives in the session it was given on: that session
    ///   detached, the worker's requests named `HeadlessChrome` again. So the
    ///   first session carries it, a second is only released, and the identity
    ///   moves when the carrier goes and another stays ([`Shared::detached`]).
    fn attached(&self, params: &Value) {
        let Some(session) = params.get("sessionId").and_then(Value::as_str) else {
            return;
        };
        let kind = params
            .pointer("/targetInfo/type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let target = params
            .pointer("/targetInfo/targetId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let waiting = params
            .get("waitingForDebugger")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let plan = {
            let mut state = self.lock();
            let plan = state.on_attach.clone();
            state
                .target_of
                .insert(session.to_string(), target.to_string());
            let entry = state
                .targets
                .entry(target.to_string())
                .or_insert_with(|| Target {
                    kind: kind.to_string(),
                    sessions: Vec::new(),
                    carrier: None,
                });
            entry.sessions.push(session.to_string());
            match plan {
                Some(plan) if entry.carrier.is_none() => {
                    entry.carrier = Some(session.to_string());
                    Some(plan)
                }
                _ => None,
            }
        };
        tracing::debug!(%kind, waiting, dressed = plan.is_some(), "a target attached");
        if let Some(plan) = plan {
            self.dress(session, kind, plan);
        }
        if waiting {
            self.fire(Some(session), "Runtime.runIfWaitingForDebugger", json!({}));
        }
    }

    /// Gives one session of a target the commands its kind gets.
    fn dress(&self, session: &str, kind: &str, plan: OnAttach) {
        let commands = match kind {
            "page" | "iframe" => plan.page,
            kind if kind.ends_with("worker") => plan.worker,
            _ => Vec::new(),
        };
        for (method, params) in commands {
            self.fire(Some(session), method, params);
        }
        // A frame holds frames and a worker can start workers; each is paused
        // and handed over the same way.
        self.fire(
            Some(session),
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        );
    }

    /// Forgets a session the browser let go of, and moves the identity to
    /// another session of the same target when this one carried it.
    fn detached(&self, session: &str) {
        let (plan, survivor) = {
            let mut state = self.lock();
            let plan = state.on_attach.clone();
            let Some(target) = state.target_of.remove(session) else {
                return;
            };
            let Some(entry) = state.targets.get_mut(&target) else {
                return;
            };
            entry.sessions.retain(|s| s != session);
            if entry.sessions.is_empty() {
                state.targets.remove(&target);
                return;
            }
            if entry.carrier.as_deref() != Some(session) {
                return;
            }
            entry.carrier = entry.sessions.first().cloned();
            (plan, entry.carrier.clone().map(|s| (s, entry.kind.clone())))
        };
        if let (Some(plan), Some((survivor, kind))) = (plan, survivor) {
            tracing::debug!(%kind, "the identity moves to the session that stayed");
            self.dress(&survivor, &kind, plan);
        }
    }

    /// Answers a request the browser paused for this connection, on the
    /// session that paused it.
    ///
    /// Nothing is paused but what a `Fetch.enable` asked for, and the only
    /// one sent is `headless/`'s, which also sets the rule that judges each
    /// (`headless::guard`). Without a rule a paused request is failed: a
    /// connection that asked for pauses and said nothing about them lets
    /// nothing through. A request is failed the way a content blocker fails
    /// one — `BlockedByClient`, which is what a page sees from the
    /// extensions a great many people run.
    ///
    /// Here and not in a subscriber, whose events can be dropped: a pause
    /// nobody answers holds the request, and the page with it, for good.
    fn answer_paused(&self, session: Option<&str>, params: &Value) {
        let Some(request) = params.get("requestId").and_then(Value::as_str) else {
            return;
        };
        let rule = self.lock().pause_rule.clone();
        match rule.map(|rule| rule(params)) {
            Some(Paused::Continue) => {
                self.fire(
                    session,
                    "Fetch.continueRequest",
                    json!({ "requestId": request }),
                );
                return;
            }
            Some(Paused::Fail(why)) => {
                let said = self.lock().refusals.note(&why, std::time::Instant::now());
                if let Some(said) = said {
                    let times = said.refused;
                    if why == Paused::VIDEO {
                        tracing::debug!(request = %why, times, "refused a call the page's app would have sent");
                    } else {
                        tracing::warn!(request = %why, times, "refused a call the page's app would have sent");
                    }
                }
            }
            None => {}
        }
        self.fire(
            session,
            "Fetch.failRequest",
            json!({ "requestId": request, "errorReason": "BlockedByClient" }),
        );
    }

    /// Hands an event to whoever subscribed to its session, without waiting.
    fn offer(&self, session: Option<String>, method: String, params: Value) {
        let mut state = self.lock();
        if !state.subscribers.iter().any(|s| s.session == session) {
            return;
        }
        let event = Event {
            session,
            method,
            params,
        };
        state.subscribers.retain(|s| {
            if s.session != event.session {
                return true;
            }
            match s.sender.try_send(event.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::trace!(method = %event.method, "a subscriber fell behind; dropped");
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// The connection is over: every call waiting fails with `reason`, and so
    /// does every call after.
    fn close(&self, reason: &str) {
        let failed: Vec<Pending> = {
            let mut state = self.lock();
            state.closed.get_or_insert_with(|| reason.to_string());
            state.subscribers.clear();
            state.pending.drain().map(|(_, pending)| pending).collect()
        };
        for pending in failed {
            if let Some(reply) = pending.reply {
                let _ = reply.send(Err(CallError::Closed {
                    method: pending.method,
                    reason: reason.to_string(),
                }));
            }
        }
    }
}

/// The dispatcher: reads every frame, routes it, and closes the connection
/// when the browser's end goes.
async fn dispatch(shared: Weak<Shared>, mut frames: mpsc::Receiver<Frame>) {
    while let Some(frame) = frames.recv().await {
        let Some(shared) = shared.upgrade() else {
            // Nobody holds the connection any more.
            return;
        };
        match frame {
            Frame::Message(bytes) => shared.handle(&bytes),
            Frame::Oversized { head } => shared.oversized(&head),
        }
    }
    if let Some(shared) = shared.upgrade() {
        shared.close("the browser closed its end of the pipe");
    }
}

/// The `id` at the start of a message, read off its first bytes.
///
/// For a message too large to keep, of which only the head survives. Chromium
/// writes a reply's `id` first, so `{"id":` and the digits after it are all
/// there is to find; anything else is an event, which answers no call.
fn id_in_head(head: &[u8]) -> Option<u64> {
    let rest = head.strip_prefix(br#"{"id":"#)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_refusal_is_logged_once_then_counted_every_interval() {
        use std::time::Instant;
        let mut log = RefusalLog::default();
        let start = Instant::now();
        assert_eq!(log.note("video", start).map(|s| s.refused), Some(1));
        for n in 1..=5000u64 {
            let at = start + Duration::from_millis(n);
            assert!(
                log.note("video", at).is_none(),
                "silent inside the interval"
            );
        }
        let later = start + REFUSAL_LOG_EVERY;
        assert_eq!(log.note("video", later).map(|s| s.refused), Some(5001));
        assert!(log.note("video", later).is_none());
        // Another reason has its own first line.
        assert_eq!(log.note("POST /x", later).map(|s| s.refused), Some(1));
    }

    #[test]
    fn reasons_made_of_addresses_cannot_grow_the_table() {
        let mut log = RefusalLog::default();
        let now = std::time::Instant::now();
        for n in 0..10_000 {
            log.note(&format!("POST /{n}"), now);
        }
        assert!(log.told.len() <= REFUSAL_REASONS + 1);
    }

    use super::*;

    /// A browser that says what the test tells it to, and shows what it was
    /// sent.
    struct Fake {
        commands: mpsc::UnboundedReceiver<Message>,
        frames: mpsc::Sender<Frame>,
    }

    fn connected() -> (Connection, Fake) {
        let (to_browser, commands) = mpsc::unbounded_channel();
        let (frames, from_browser) = mpsc::channel(64);
        (
            Connection::start(PipeTransport {
                to_browser,
                from_browser,
            }),
            Fake { commands, frames },
        )
    }

    impl Fake {
        /// The next command written to the browser.
        async fn sent(&mut self) -> Value {
            let message = tokio::time::timeout(Duration::from_secs(5), self.commands.recv())
                .await
                .expect("a command within five seconds")
                .expect("the connection is open");
            serde_json::from_slice(&message).expect("a command is JSON")
        }

        /// Whether anything more was written, without waiting long.
        async fn quiet(&mut self) -> bool {
            tokio::time::timeout(Duration::from_millis(100), self.commands.recv())
                .await
                .is_err()
        }

        async fn say(&self, message: Value) {
            self.frames
                .send(Frame::Message(message.to_string().into_bytes()))
                .await
                .expect("the dispatcher is reading");
        }
    }

    const LONG: Duration = Duration::from_secs(5);

    fn spawn_call(
        connection: &Connection,
        session: Option<&str>,
        method: &str,
    ) -> tokio::task::JoinHandle<Result<Value, CallError>> {
        let connection = connection.clone();
        let session = session.map(str::to_string);
        let method = method.to_string();
        tokio::spawn(async move {
            connection
                .call(session.as_deref(), &method, json!({}), LONG)
                .await
        })
    }

    /// Two calls in flight, answered in the other order: each caller gets its
    /// own answer.
    #[tokio::test]
    async fn answers_find_their_callers_in_any_order() {
        let (connection, mut browser) = connected();
        let first = spawn_call(&connection, None, "First.one");
        let first_id = browser.sent().await["id"].clone();
        let second = spawn_call(&connection, None, "Second.one");
        let second_id = browser.sent().await["id"].clone();

        browser
            .say(json!({ "id": second_id, "result": { "which": 2 } }))
            .await;
        browser
            .say(json!({ "id": first_id, "result": { "which": 1 } }))
            .await;

        assert_eq!(first.await.unwrap().unwrap()["which"], 1);
        assert_eq!(second.await.unwrap().unwrap()["which"], 2);
        assert_eq!(connection.waiting(), 0);
    }

    /// A call given up on — a timeout here, a Ctrl+C in the program — leaves
    /// nothing waiting, its late answer is dropped, and the next call works.
    #[tokio::test]
    async fn a_call_given_up_on_leaves_nothing_behind() {
        let (connection, mut browser) = connected();
        let given_up = connection
            .call(None, "Slow.one", json!({}), Duration::from_millis(20))
            .await;
        assert!(
            matches!(given_up, Err(CallError::TimedOut { .. })),
            "{given_up:?}"
        );
        assert_eq!(connection.waiting(), 0, "the entry went with the future");

        let late = browser.sent().await["id"].clone();
        browser.say(json!({ "id": late, "result": {} })).await;

        let next = spawn_call(&connection, None, "Next.one");
        let id = browser.sent().await["id"].clone();
        browser
            .say(json!({ "id": id, "result": { "ok": true } }))
            .await;
        assert_eq!(next.await.unwrap().unwrap()["ok"], true);
    }

    /// The browser leaving fails every call waiting on it, and every call
    /// after, with the reason — not a timeout twenty seconds later.
    #[tokio::test]
    async fn when_the_pipe_closes_every_call_fails_at_once() {
        let (connection, mut browser) = connected();
        let waiting = spawn_call(&connection, None, "Never.answered");
        let _ = browser.sent().await;
        drop(browser.frames);

        let failed = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("at once, not at the timeout")
            .unwrap();
        assert!(
            matches!(failed, Err(CallError::Closed { .. })),
            "{failed:?}"
        );
        let later = connection.call(None, "After.it", json!({}), LONG).await;
        assert!(matches!(later, Err(CallError::Closed { .. })), "{later:?}");
        assert!(connection.closed().is_some());
    }

    /// Dropping the last clone closes the browser's command pipe: the
    /// dispatcher does not keep a connection nobody holds alive.
    #[tokio::test]
    async fn dropping_the_connection_closes_the_pipe() {
        let (connection, mut browser) = connected();
        drop(connection);
        let ended = tokio::time::timeout(Duration::from_secs(1), browser.commands.recv())
            .await
            .expect("at once");
        assert!(ended.is_none());
    }

    /// A crashed tab fails the calls on its own session and no other, and a
    /// call on it afterwards fails without being sent.
    #[tokio::test]
    async fn a_crashed_tab_fails_only_its_own_calls() {
        let (connection, mut browser) = connected();
        let on_crashed = spawn_call(&connection, Some("A"), "Runtime.evaluate");
        let _ = browser.sent().await;
        let on_other = spawn_call(&connection, Some("B"), "Runtime.evaluate");
        let other_id = browser.sent().await["id"].clone();

        browser
            .say(json!({ "method": "Inspector.targetCrashed", "sessionId": "A", "params": {} }))
            .await;
        let crashed = on_crashed.await.unwrap();
        assert!(
            matches!(crashed, Err(CallError::TargetGone { .. })),
            "{crashed:?}"
        );

        browser
            .say(json!({ "id": other_id, "result": { "fine": true } }))
            .await;
        assert_eq!(on_other.await.unwrap().unwrap()["fine"], true);

        let again = connection
            .call(Some("A"), "Runtime.evaluate", json!({}), LONG)
            .await;
        assert!(
            matches!(again, Err(CallError::TargetGone { .. })),
            "{again:?}"
        );
        assert!(
            browser.quiet().await,
            "nothing was written for a session that is gone"
        );
    }

    /// Chromium's `-32001` is a session it no longer knows: that call and the
    /// others on the session fail, which is what a detach says as well.
    #[tokio::test]
    async fn a_session_the_browser_forgot_fails_every_call_on_it() {
        let (connection, mut browser) = connected();
        let first = spawn_call(&connection, Some("C"), "One.call");
        let first_id = browser.sent().await["id"].clone();
        let second = spawn_call(&connection, Some("C"), "Two.call");
        let _ = browser.sent().await;

        browser
            .say(json!({
                "id": first_id,
                "error": { "code": -32001, "message": "Session with given id not found." },
            }))
            .await;
        assert!(matches!(
            first.await.unwrap(),
            Err(CallError::TargetGone { .. })
        ));
        assert!(matches!(
            second.await.unwrap(),
            Err(CallError::TargetGone { .. })
        ));

        let on_detached = spawn_call(&connection, Some("D"), "Three.call");
        let _ = browser.sent().await;
        browser
            .say(json!({
                "method": "Target.detachedFromTarget",
                "params": { "sessionId": "D", "targetId": "t-d" },
            }))
            .await;
        assert!(matches!(
            on_detached.await.unwrap(),
            Err(CallError::TargetGone { .. })
        ));
    }

    /// An ordinary refusal is carried back as one, and is not a lost target.
    #[tokio::test]
    async fn a_refusal_is_the_answer_it_is() {
        let (connection, mut browser) = connected();
        let call = spawn_call(&connection, Some("E"), "Not.aMethod");
        let id = browser.sent().await["id"].clone();
        browser
            .say(json!({ "id": id, "error": { "code": -32601, "message": "not found" } }))
            .await;
        let refused = call.await.unwrap();
        assert!(
            matches!(refused, Err(CallError::Refused { .. })),
            "{refused:?}"
        );
        let fine = spawn_call(&connection, Some("E"), "Still.there");
        let id = browser.sent().await["id"].clone();
        browser.say(json!({ "id": id, "result": {} })).await;
        assert!(fine.await.unwrap().is_ok());
    }

    /// An answer past the ceiling fails its own call; the call beside it is
    /// answered, and the connection carries on.
    #[tokio::test]
    async fn an_answer_past_the_ceiling_fails_only_its_call() {
        let (connection, mut browser) = connected();
        let huge = spawn_call(&connection, None, "Storage.getCookies");
        let huge_id = browser.sent().await["id"].as_u64().unwrap();
        let small = spawn_call(&connection, None, "Browser.getVersion");
        let small_id = browser.sent().await["id"].clone();

        browser
            .frames
            .send(Frame::Oversized {
                head: format!(r#"{{"id":{huge_id},"result":{{"cookies":[{{"#).into_bytes(),
            })
            .await
            .unwrap();
        browser
            .say(json!({ "id": small_id, "result": { "ok": 1 } }))
            .await;

        assert!(matches!(
            huge.await.unwrap(),
            Err(CallError::TooLarge { .. })
        ));
        assert_eq!(small.await.unwrap().unwrap()["ok"], 1);
        assert!(connection.closed().is_none());
    }

    fn attach(session: &str, target: &str, kind: &str) -> Value {
        json!({
            "method": "Target.attachedToTarget",
            "params": {
                "sessionId": session,
                "targetInfo": { "targetId": target, "type": kind },
                "waitingForDebugger": true,
            },
        })
    }

    /// A target attached while nobody is waiting on any call is given its
    /// commands and let go at once, in that order, on its own session.
    #[tokio::test]
    async fn a_target_is_released_while_nobody_is_waiting() {
        let (connection, mut browser) = connected();
        connection.set_on_attach(OnAttach {
            page: vec![(
                "Emulation.setUserAgentOverride",
                json!({ "userAgent": "UA" }),
            )],
            worker: vec![("Network.setUserAgentOverride", json!({ "userAgent": "UA" }))],
        });

        browser.say(attach("W", "worker-1", "worker")).await;
        let mut said = Vec::new();
        for _ in 0..3 {
            let command = browser.sent().await;
            assert_eq!(command["sessionId"], "W", "{command}");
            said.push(command["method"].as_str().unwrap().to_string());
        }
        assert_eq!(
            said,
            [
                "Network.setUserAgentOverride",
                "Target.setAutoAttach",
                "Runtime.runIfWaitingForDebugger",
            ]
        );
    }

    /// The service worker a site registers is attached twice. The first
    /// session gets the identity; the second is only let go.
    #[tokio::test]
    async fn a_target_attached_twice_is_dressed_once() {
        let (connection, mut browser) = connected();
        connection.set_on_attach(OnAttach {
            page: Vec::new(),
            worker: vec![("Network.setUserAgentOverride", json!({ "userAgent": "UA" }))],
        });
        browser.say(attach("S1", "sw", "service_worker")).await;
        for _ in 0..3 {
            let _ = browser.sent().await;
        }
        browser.say(attach("S2", "sw", "service_worker")).await;
        let only = browser.sent().await;
        assert_eq!(only["method"], "Runtime.runIfWaitingForDebugger");
        assert_eq!(only["sessionId"], "S2");
        assert!(browser.quiet().await);

        // The tab that attached it closes; the worker lives on, and the
        // session that stays is given the identity.
        browser
            .say(json!({
                "method": "Target.detachedFromTarget",
                "params": { "sessionId": "S1", "targetId": "sw" },
            }))
            .await;
        let moved = browser.sent().await;
        assert_eq!(moved["method"], "Network.setUserAgentOverride");
        assert_eq!(moved["sessionId"], "S2");
        assert_eq!(browser.sent().await["method"], "Target.setAutoAttach");
        assert!(browser.quiet().await);

        // A session that did not carry it leaves quietly.
        browser.say(attach("S3", "sw", "service_worker")).await;
        let _ = browser.sent().await;
        browser
            .say(json!({
                "method": "Target.detachedFromTarget",
                "params": { "sessionId": "S3", "targetId": "sw" },
            }))
            .await;
        assert!(browser.quiet().await);
    }

    /// A target attached before the plan is set — the tab snob attaches to
    /// itself, and dresses with calls of its own — is registered and given
    /// nothing.
    #[tokio::test]
    async fn a_target_attached_before_the_plan_is_only_registered() {
        let (_connection, mut browser) = connected();
        browser
            .say(json!({
                "method": "Target.attachedToTarget",
                "params": {
                    "sessionId": "P",
                    "targetInfo": { "targetId": "tab", "type": "page" },
                    "waitingForDebugger": false,
                },
            }))
            .await;
        assert!(browser.quiet().await);
    }

    /// With no rule to judge it, a paused request is refused as a content
    /// blocker refuses one, on the session that paused it.
    #[tokio::test]
    async fn a_paused_request_is_refused() {
        let (_connection, mut browser) = connected();
        browser
            .say(json!({
                "method": "Fetch.requestPaused",
                "sessionId": "T",
                "params": { "requestId": "interception-7" },
            }))
            .await;
        let refused = browser.sent().await;
        assert_eq!(refused["method"], "Fetch.failRequest");
        assert_eq!(refused["sessionId"], "T");
        assert_eq!(refused["params"]["requestId"], "interception-7");
        assert_eq!(refused["params"]["errorReason"], "BlockedByClient");
    }

    /// A paused request the rule lets through goes on, on the session that
    /// paused it, and the rule is handed the event as it came.
    #[tokio::test]
    async fn a_paused_request_the_rule_lets_through_goes_on() {
        let (connection, mut browser) = connected();
        connection.set_pause_rule(Arc::new(|params: &Value| {
            if params["request"]["method"] == "GET" {
                Paused::Continue
            } else {
                Paused::Fail("not a GET".to_string())
            }
        }));
        browser
            .say(json!({
                "method": "Fetch.requestPaused",
                "sessionId": "T",
                "params": { "requestId": "interception-8", "request": { "method": "GET" } },
            }))
            .await;
        let continued = browser.sent().await;
        assert_eq!(continued["method"], "Fetch.continueRequest");
        assert_eq!(continued["sessionId"], "T");
        assert_eq!(continued["params"]["requestId"], "interception-8");
        assert!(browser.quiet().await, "one answer per pause");
    }

    /// A paused request the rule fails is failed as a content blocker fails
    /// one, like every pause with no rule at all.
    #[tokio::test]
    async fn a_paused_request_the_rule_fails_is_refused() {
        let (connection, mut browser) = connected();
        connection.set_pause_rule(Arc::new(|_: &Value| Paused::Fail("anything".to_string())));
        browser
            .say(json!({
                "method": "Fetch.requestPaused",
                "sessionId": "T",
                "params": { "requestId": "interception-9", "request": { "method": "POST" } },
            }))
            .await;
        let refused = browser.sent().await;
        assert_eq!(refused["method"], "Fetch.failRequest");
        assert_eq!(refused["sessionId"], "T");
        assert_eq!(refused["params"]["requestId"], "interception-9");
        assert_eq!(refused["params"]["errorReason"], "BlockedByClient");
        assert!(browser.quiet().await, "one answer per pause");
    }

    /// Events reach the subscribers of their own session, and a subscriber
    /// that does not keep up loses events rather than holding up answers.
    #[tokio::test]
    async fn events_reach_their_session_and_never_hold_up_answers() {
        let (connection, mut browser) = connected();
        let mut on_a = connection.subscribe(Some("A"), 1);
        let mut on_b = connection.subscribe(Some("B"), 8);

        for n in 0..5 {
            browser
                .say(json!({ "method": "Network.dataReceived", "sessionId": "A", "params": { "n": n } }))
                .await;
        }
        browser
            .say(json!({ "method": "Network.dataReceived", "sessionId": "B", "params": { "n": 99 } }))
            .await;
        let call = spawn_call(&connection, None, "Answered.anyway");
        let id = browser.sent().await["id"].clone();
        browser.say(json!({ "id": id, "result": {} })).await;
        assert!(
            call.await.unwrap().is_ok(),
            "a full subscriber held nothing up"
        );

        assert_eq!(on_a.recv().await.unwrap().params["n"], 0);
        assert!(
            on_a.try_recv().is_err(),
            "the rest did not fit and were dropped"
        );
        assert_eq!(on_b.recv().await.unwrap().params["n"], 99);
    }

    /// Whatever the browser writes, the reader goes on reading: it runs in a
    /// process that aborts on a panic, and all of this came from outside.
    #[tokio::test]
    async fn nothing_the_browser_says_stops_the_reader() {
        let (connection, mut browser) = connected();
        connection.set_on_attach(OnAttach::default());
        let call = spawn_call(&connection, None, "Browser.getVersion");
        let id = browser.sent().await["id"].clone();

        for junk in [
            &b"not json"[..],
            b"",
            b"[]",
            b"{\"id\":\"7\"}",
            b"{\"id\":-1}",
            b"{\"method\":42}",
            b"{\"method\":\"Target.attachedToTarget\"}",
            b"{\"method\":\"Target.attachedToTarget\",\"params\":{\"sessionId\":7}}",
            b"{\"method\":\"Target.detachedFromTarget\",\"params\":null}",
            b"{\"method\":\"Fetch.requestPaused\",\"params\":{}}",
            b"{\"method\":\"Inspector.targetCrashed\"}",
            b"{\"id\":99999,\"error\":{\"code\":-32001}}",
        ] {
            browser
                .frames
                .send(Frame::Message(junk.to_vec()))
                .await
                .unwrap();
        }
        browser
            .frames
            .send(Frame::Oversized {
                head: b"{\"method\":".to_vec(),
            })
            .await
            .unwrap();
        browser
            .say(json!({ "id": id, "result": { "still": "here" } }))
            .await;
        assert_eq!(call.await.unwrap().unwrap()["still"], "here");
        assert!(connection.closed().is_none());
    }

    #[test]
    fn the_id_is_read_off_the_head_of_a_message() {
        assert_eq!(id_in_head(br#"{"id":42,"result":{"#), Some(42));
        assert_eq!(id_in_head(br#"{"method":"Network.x","params":{"#), None);
        assert_eq!(id_in_head(br#"{"id":"#), None);
        assert_eq!(id_in_head(b""), None);
    }
}
