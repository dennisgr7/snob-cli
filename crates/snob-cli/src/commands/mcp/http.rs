//! `snob mcp --listen`: the same server over Streamable HTTP, on a loopback
//! address, behind the key `snob mcp key` keeps, for a tunnel of the
//! person's own (`ssh -L`, cloudflared, a VPN) to reach from elsewhere.
//!
//! **Loopback only** ([`loopback`]): nothing here is meant to be reached
//! from a network directly, and TLS is the tunnel's job. **The key is
//! mandatory**: a TCP port on loopback is open to every process of every
//! user on the machine, unlike the owner's pipe or the relay's socket, so
//! the address is no secret and the key is what tells the person's client
//! from anything else that connects. The server will not start without one.
//!
//! What a request goes through, in this order, each step before the next is
//! paid for ([`Front::answer`]):
//!
//! 1. **The path**: `/mcp`, and nothing else, `/.well-known/` included, so a
//!    client looking for OAuth is told there is none.
//! 2. **The key**, in `Authorization: Bearer`, **before any of the body is
//!    read** and before a session exists ([`Gate`]). Both sides are signed
//!    under a key drawn at start and compared whole, so the time a refusal
//!    takes says nothing about how much of the key was right. A refusal
//!    closes the connection, so failed attempts cannot hold connections
//!    open.
//! 3. **The body**, read here, at most [`MOST_BODY`] bytes within
//!    [`BODY_WITHIN`]: hyper's header timeout does not cover the body, and
//!    a body that never arrives would hold its connection for ever.
//! 4. **Sessions**: a new one only while fewer than [`MOST_SESSIONS`] are
//!    open.
//! 5. The SDK's own checks (`Host` and `Origin`, the protocol's headers),
//!    and then the same handler `snob mcp` serves on standard input and
//!    output, with the same tickets, limits and children.
//!
//! **A connection that has not shown the key holds no place.** Anything on
//! the machine can open connections and send nothing, so the cap on open
//! connections ([`MOST_CONNECTIONS`]) is filled first by the key's holders:
//! a connection whose first request has not arrived within
//! [`FIRST_REQUEST_WITHIN`] is closed, and at the cap the oldest connection
//! that has not shown the key is closed to make room ([`Open`]). Only when
//! every connection open has shown it is a new one refused. A client's new
//! connection shows the key in its first request, within milliseconds, so
//! pushing it out would take more than the cap of new connections opened in
//! that time.
//!
//! **Against a caller that is not a browser, the key is the only control.**
//! The `Host` and `Origin` checks keep a web page in the person's own
//! browser from reaching the port through DNS rebinding; anything else sets
//! those headers as it likes.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use snob_core::secret::Secret;
use snob_core::watch::sign;
use snob_ig::pace::CancelToken;
use snob_store::secrets::SecretStore;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinSet};
use url::Url;

use super::{InFlight, Running, Server, Settings, child, key};
use crate::cli::ConfirmInChat;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// The one path served.
pub const PATH: &str = super::HTTP_PATH;

/// Connections open at once. A client keeps one or two (a stream it
/// listens on, and a request), a tunnel a few more. **Counted against the
/// connections that have shown the key**: a new one past this closes the
/// oldest that has not ([`Open::admit`]), so a process without the key that
/// holds idle connections only pushes out its own. Past this many that have
/// shown it, whoever holds them is not one person's clients, and a new one is
/// closed at once.
const MOST_CONNECTIONS: usize = 32;

/// How long a new connection has to send the head of its first request. A
/// client connects to send one, and sends it at once; a connection that sends
/// nothing is closed this soon, rather than after [`HEADERS_WITHIN`], so the
/// places it holds turn over fast.
const FIRST_REQUEST_WITHIN: Duration = Duration::from_secs(2);

/// Sessions open at once (the revisions before 2026-07-28 open one per
/// conversation). A session is a little memory and a task, and each client
/// a person uses holds one; eight is a few clients and their reconnections.
const MOST_SESSIONS: usize = 8;

/// The largest request body taken. The largest call a client makes is a
/// tool call with a few arguments and a ticket, well under a kilobyte; a
/// quarter of a megabyte leaves room for a client that sends its whole
/// `_meta` on every request, and none for one that means harm.
const MOST_BODY: usize = 256 * 1024;

/// How long a body may take to arrive, from when it is first read.
const BODY_WITHIN: Duration = Duration::from_secs(30);

/// How long a request's headers may take to arrive, and so how long a
/// connection may sit idle between two requests: hyper counts both from when
/// it starts waiting for a request. A client opens another connection when
/// it needs one.
const HEADERS_WITHIN: Duration = Duration::from_secs(10);

/// hyper's read buffer, which bounds the headers of a request. 16 KiB is
/// twice hyper's floor (8 KiB, below which it panics) and far past what a
/// client sends.
const BUFFER: usize = 16 * 1024;

/// How long a session no request reaches is kept: a conversation left open
/// over a coffee keeps its session, one abandoned does not hold a place
/// among [`MOST_SESSIONS`] for the life of the server.
const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);

/// Once the calls have settled on the way out: how long their answers get to
/// be written, and then how long the connections still open get to end.
const CLOSING_WITHIN: Duration = Duration::from_secs(2);

/// Where to listen, and the address a tunnel reaches it at, if one does.
#[derive(Debug, Clone)]
pub struct Listen {
    pub address: SocketAddr,
    pub public: Option<Url>,
}

/// `--listen`'s value: an address on this machine's loopback, `localhost`
/// standing for `127.0.0.1`. **Anything else is refused**, whatever it
/// would have reached: a wildcard, a LAN address, a public one.
pub fn loopback(text: &str) -> Result<SocketAddr, String> {
    let refused = || {
        format!(
            "\"{text}\" is not a loopback address with a port, such as 127.0.0.1:8750 or \
             [::1]:8750; snob mcp listens on this machine only, and is reached from elsewhere \
             through a tunnel of your own (ssh -L, cloudflared, a VPN)"
        )
    };
    let address = match text.strip_prefix("localhost:") {
        Some(port) => SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port.parse().map_err(|_| refused())?,
        ),
        None => text.parse::<SocketAddr>().map_err(|_| refused())?,
    };
    if address.ip().is_loopback() {
        Ok(address)
    } else {
        Err(refused())
    }
}

/// `--public-url`'s value: the `https` address a tunnel serves this at.
/// Only its host and origin are used, to accept the requests addressed to
/// it ([`allowed`]); `http` is refused, since a tunnel that does not end TLS
/// would carry the key in plain text.
pub fn public_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|e| format!("\"{text}\" is not an address: {e}"))?;
    if url.scheme() != "https" {
        return Err(format!(
            "\"{text}\" is not https: the key travels in every request, and only a tunnel that \
             ends TLS keeps it from everything on the way"
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("\"{text}\" names no host"));
    }
    Ok(url)
}

/// The `Host` values and browser origins taken: loopback's names, the
/// address the server is bound to (any of 127/8 may be, and a client told
/// that address names it), and the public URL's host and origin. **Never an
/// empty list of hosts**, which the SDK reads as any host at all.
fn allowed(bound: IpAddr, public: Option<&Url>) -> (Vec<String>, Vec<String>) {
    let mut hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    // Without brackets, as the SDK compares an IPv6 host.
    let bound = bound.to_string();
    if !hosts.contains(&bound) {
        hosts.push(bound);
    }
    let mut origins = Vec::new();
    if let Some(url) = public
        && let Some(host) = url.host_str()
    {
        hosts.push(host.trim_matches(['[', ']']).to_string());
        // With the port written out: an origin entry without one is read
        // by the SDK as any port, a reading it says it will drop.
        let port = url.port_or_known_default().unwrap_or(443);
        origins.push(format!("{}://{host}:{port}", url.scheme()));
    }
    (hosts, origins)
}

/// Serves `snob mcp` over HTTP on `listen` until asked to stop (Ctrl+C, the
/// system, or standard input closing under `--stop-when-stdin-closes`), then
/// gives the calls still running their grace and exits 130.
pub async fn serve(
    launch: child::Launch,
    settings: Settings,
    listen: Listen,
    store: &SecretStore,
) -> Result<ExitCode> {
    let Some((key, source)) = key::load(store)? else {
        return Err(ExitError::new(
            ExitCode::Error,
            "snob mcp --listen needs a key for its clients to present, and there is none",
        )
        .with_hint("make one with \"snob mcp key new\"")
        .into());
    };
    // Narrower than on standard input and output: whoever holds the key
    // can call, and a yes in the chat may be theirs.
    let in_chat = settings.in_chat.unwrap_or(ConfirmInChat::Lists);
    let closing = crate::interrupt::install();
    let server = Server::new(launch, &settings, in_chat, closing.clone())?;
    let in_flight = Arc::clone(&server.in_flight);

    let listener = TcpListener::bind(listen.address)
        .await
        .with_context(|| format!("could not listen on {}", listen.address))?;
    let bound = listener.local_addr()?;
    let gate = Gate::new(&key)?;
    drop(key);

    let (hosts, origins) = allowed(bound.ip(), listen.public.as_ref());
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        .with_allowed_origins(origins)
        // With no public URL there is no origin to allow, and an empty
        // list would otherwise mean no check at all: a page in a browser
        // on this machine has no business here.
        .enforce_origin_validation()
        .with_max_request_body_bytes(MOST_BODY)
        // Sessions for the revisions before 2026-07-28, which is where a
        // question asked with `elicitation/create` travels, on the stream of
        // the call that asks it.
        .with_legacy_session_mode(true);
    let stopping = config.cancellation_token.clone();
    // Both `#[non_exhaustive]`: built from their defaults, then changed.
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(SESSION_IDLE);
    let manager = Arc::new(manager);
    let service =
        StreamableHttpService::new(move || Ok(server.clone()), Arc::clone(&manager), config);
    let front = Arc::new(Front {
        gate,
        service,
        manager,
        answering: Arc::new(InFlight::default()),
    });

    started(
        bound,
        source,
        &front.gate.fingerprint,
        &listen,
        &settings,
        in_chat,
    );

    let mut connections = JoinSet::new();
    let mut open = Open::default();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = closing.canceled() => break,
        };
        // Finished connections, before they are counted.
        while connections.try_join_next().is_some() {}
        match accepted {
            Ok(_) if !open.admit() => {
                tracing::debug!(
                    "refused a connection to snob mcp: every one open has shown the key"
                );
            }
            Ok((stream, _)) => {
                let seen = Arc::new(Seen::default());
                let task = connections.spawn(connection(
                    stream,
                    Arc::clone(&front),
                    closing.clone(),
                    Arc::clone(&seen),
                ));
                open.push(seen, task);
            }
            Err(e) => {
                tracing::debug!(error = %e, "refused a connection to snob mcp");
                // Not a busy loop on a listener that keeps failing.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    // Stopping: nothing new is taken, and each call's child has been asked to
    // stop (the handlers race their child against the same token) and gets
    // its grace. **Then the answers are written, and only then are the
    // streams ended**: the SDK's own token, canceled, drops an answer it has
    // not yet handed to its response (a 500, or a stream with no result), so
    // the server first waits for every response to a request to have been
    // sent ([`Front::answering`]), which a connection told to close finishes
    // writing. What is left is a stream that never ends by itself (a
    // session's own), which the token ends.
    drop(listener);
    in_flight
        .settled(child::GRACE + Duration::from_secs(5))
        .await;
    front.answering.settled(CLOSING_WITHIN).await;
    stopping.cancel();
    let ended = async { while connections.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(CLOSING_WITHIN, ended).await;
    connections.abort_all();
    Ok(ExitCode::Interrupted)
}

/// What a connection has done so far, which the accept loop counts it by.
#[derive(Default)]
struct Seen {
    /// The head of a request has arrived on it.
    asked: AtomicBool,
    /// A request on it has shown the key ([`Gate::lets_in`]).
    keyed: AtomicBool,
}

/// The connections open, oldest first.
#[derive(Default)]
struct Open(VecDeque<(Arc<Seen>, AbortHandle)>);

impl Open {
    /// Whether a new connection may be taken: always while fewer than
    /// [`MOST_CONNECTIONS`] are open, and otherwise once **the oldest that
    /// has not shown the key** has been closed to make room. Refused only
    /// when every connection open has shown it.
    fn admit(&mut self) -> bool {
        self.0.retain(|(_, task)| !task.is_finished());
        if self.0.len() < MOST_CONNECTIONS {
            return true;
        }
        let Some(oldest) = self
            .0
            .iter()
            .position(|(seen, _)| !seen.keyed.load(Ordering::SeqCst))
        else {
            return false;
        };
        if let Some((_, task)) = self.0.remove(oldest) {
            // Its task dropped, its connection is closed.
            task.abort();
        }
        true
    }

    fn push(&mut self, seen: Arc<Seen>, task: AbortHandle) {
        self.0.push_back((seen, task));
    }
}

/// What the person running the server is told when it starts, on standard
/// error: where it listens, which key it takes (by fingerprint), and what
/// whoever holds that key may do.
fn started(
    bound: SocketAddr,
    source: key::Source,
    fingerprint: &str,
    listen: &Listen,
    settings: &Settings,
    in_chat: ConfirmInChat,
) {
    ui::info(&format!(
        "snob mcp is serving on http://{bound}{PATH}, for clients that present the MCP key \
         from {} (fingerprint {fingerprint}).",
        source.describe()
    ));
    if let Some(public) = &listen.public {
        ui::info(&format!(
            "It takes requests addressed to {}; TLS ends at the tunnel, which sees the key and \
             every answer.",
            public.origin().ascii_serialization()
        ));
    }
    if settings.writes {
        let confirm = if in_chat == ConfirmInChat::All {
            ", and can confirm them in the chat itself"
        } else {
            ""
        };
        ui::warn(&format!(
            "with --allow-writes, whoever holds the key can call follow and unfollow{confirm}; \
             keep the key to clients you trust."
        ));
    }
    ui::info("Ctrl+C stops it, giving the calls still running their grace.");
}

/// One connection, served until it ends or the server stops, when it is
/// told to finish the response it is writing and take no other. One whose
/// first request has not arrived within [`FIRST_REQUEST_WITHIN`] is closed.
async fn connection(stream: TcpStream, front: Arc<Front>, closing: CancelToken, seen: Arc<Seen>) {
    // Small writes are the stream of a call: a heartbeat, an answer.
    let _ = stream.set_nodelay(true);
    let service = {
        let seen = Arc::clone(&seen);
        hyper::service::service_fn(move |request| {
            let front = Arc::clone(&front);
            let seen = Arc::clone(&seen);
            async move { Ok::<_, Infallible>(front.answer(request, &seen).await) }
        })
    };
    let mut builder = http1::Builder::new();
    // **Without a timer, the header timeout panics** rather than timing out
    // (hyper's `common/time.rs`), and a buffer under hyper's 8 KiB floor
    // panics too (`max_buf_size`).
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADERS_WITHIN)
        .max_buf_size(BUFFER);
    let serving = builder.serve_connection(TokioIo::new(stream), service);
    let mut serving = std::pin::pin!(serving);
    let unasked = async {
        tokio::time::sleep(FIRST_REQUEST_WITHIN).await;
        if seen.asked.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        _ = serving.as_mut() => return,
        // Dropped, which closes it.
        () = unasked => return,
        () = closing.canceled() => {}
    }
    serving.as_mut().graceful_shutdown();
    let _ = serving.await;
}

/// A response, as the SDK writes them.
type Response = http::Response<BoxBody<Bytes, Infallible>>;

/// What stands in front of the SDK's service.
struct Front {
    gate: Gate,
    service: StreamableHttpService<Server, LocalSessionManager>,
    manager: Arc<LocalSessionManager>,
    /// The responses to a `POST` not yet sent whole, from when the request
    /// was let in until its body has been written or dropped: what the server
    /// waits for on its way out before it ends the streams.
    answering: Arc<InFlight>,
}

impl Front {
    /// Answers one request, in the order the header gives, noting on `seen`
    /// that it came, and whether it showed the key.
    async fn answer(&self, request: Request<Incoming>, seen: &Seen) -> Response {
        seen.asked.store(true, Ordering::SeqCst);
        if request.uri().path() != PATH {
            return plain(StatusCode::NOT_FOUND, "Not Found", false);
        }
        if !self.gate.lets_in(request.headers()) {
            tracing::debug!("refused a request to snob mcp: no key, or not the key");
            return unauthorized();
        }
        seen.keyed.store(true, Ordering::SeqCst);
        // A `GET` is a stream only the server's stopping ends, and is not
        // waited for.
        let answering = (request.method() == Method::POST).then(|| self.answering.enter());
        let (parts, body) = request.into_parts();
        let body = match read(&parts.headers, body).await {
            Ok(body) => body,
            Err(Unread::TooLarge) => {
                return plain(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large", true);
            }
            Err(Unread::TooSlow) => {
                return plain(StatusCode::REQUEST_TIMEOUT, "Request Timeout", true);
            }
            Err(Unread::Broken) => return plain(StatusCode::BAD_REQUEST, "Bad Request", true),
        };
        if opens_a_session(&parts.method, &parts.headers, &body)
            && self.manager.sessions.read().await.len() >= MOST_SESSIONS
        {
            return plain(
                StatusCode::SERVICE_UNAVAILABLE,
                "Service Unavailable: too many sessions are open; close one, or try again later",
                false,
            );
        }
        let response = self
            .service
            .handle(Request::from_parts(parts, Full::new(body)))
            .await;
        match answering {
            Some(running) => response.map(|body| {
                Answering {
                    body,
                    _running: running,
                }
                .boxed()
            }),
            None => response,
        }
    }
}

/// A response's body that counts as being answered ([`Front::answering`])
/// until it has been sent whole, or dropped with its connection.
struct Answering {
    body: BoxBody<Bytes, Infallible>,
    /// Held for its drop, which ends the count.
    _running: Running,
}

impl hyper::body::Body for Answering {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

/// Whether a request would open a session: an `initialize` sent with no
/// session, which is what opens one on the revisions before 2026-07-28. A
/// stateless call opens none, and is not refused when the sessions are
/// full.
fn opens_a_session(method: &Method, headers: &HeaderMap, body: &[u8]) -> bool {
    method == Method::POST
        && !headers.contains_key("mcp-session-id")
        && serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|message| {
            message.get("method").and_then(|m| m.as_str()) == Some("initialize")
        })
}

/// Why a body was not read.
#[derive(Debug, PartialEq, Eq)]
enum Unread {
    TooLarge,
    TooSlow,
    Broken,
}

/// The body, whole, if it is at most [`MOST_BODY`] bytes and arrives within
/// [`BODY_WITHIN`]. A body that announces more is refused before any of it
/// is read.
async fn read(headers: &HeaderMap, mut body: Incoming) -> Result<Bytes, Unread> {
    let announced = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok())
        .and_then(|length| length.parse::<u64>().ok());
    if announced.is_some_and(|length| length > MOST_BODY as u64) {
        return Err(Unread::TooLarge);
    }
    let whole = async {
        let mut collected = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| Unread::Broken)?;
            if let Ok(data) = frame.into_data() {
                if data.len() > MOST_BODY - collected.len() {
                    return Err(Unread::TooLarge);
                }
                collected.extend_from_slice(&data);
            }
        }
        Ok(Bytes::from(collected))
    };
    tokio::time::timeout(BODY_WITHIN, whole)
        .await
        .unwrap_or(Err(Unread::TooSlow))
}

/// A short answer of ours. `close`: the connection is closed after it, for
/// a request whose body was left unread, which would otherwise be read as
/// the next request.
fn plain(status: StatusCode, said: &'static str, close: bool) -> Response {
    let mut response = http::Response::new(Full::new(Bytes::from_static(said.as_bytes())).boxed());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if close {
        headers.insert(
            header::CONNECTION,
            header::HeaderValue::from_static("close"),
        );
    }
    response
}

/// No key, or not the key: 401, with the scheme a client is to use, and the
/// connection closed, so that guessing costs a connection a guess.
fn unauthorized() -> Response {
    let mut response = plain(StatusCode::UNAUTHORIZED, "Unauthorized", true);
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Bearer realm=\"snob\""),
    );
    response
}

/// The key, as the server holds it to check a request: not the key itself,
/// but its signature under a key drawn at start, against which what a
/// client presents is signed and compared. Two signatures of one length,
/// compared whole, so neither the length of what was sent nor where it went
/// wrong shows in the time a refusal takes.
struct Gate {
    under: Secret,
    expected: String,
    fingerprint: String,
}

impl Gate {
    fn new(key: &key::Key) -> Result<Self> {
        use base64::Engine as _;
        let mut bytes = [0u8; 32];
        crate::relay::fill_random(&mut bytes)
            .context("the system would not hand out random bytes")?;
        let under = Secret::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes));
        Ok(Self {
            expected: sign::sign(key.expose(), &under),
            under,
            fingerprint: key.fingerprint(),
        })
    }

    /// Whether the request carries the key: `Authorization: Bearer <key>`,
    /// the scheme in any case, as RFC 9110 has it.
    fn lets_in(&self, headers: &HeaderMap) -> bool {
        let Some(presented) = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim())
        else {
            return false;
        };
        sign::same(
            sign::sign(presented, &self.under).as_bytes(),
            self.expected.as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_loopback_address_with_a_port_is_listened_on() {
        assert_eq!(
            loopback("127.0.0.1:8750").unwrap(),
            "127.0.0.1:8750".parse().unwrap()
        );
        assert_eq!(loopback("[::1]:0").unwrap(), "[::1]:0".parse().unwrap());
        assert_eq!(
            loopback("localhost:8750").unwrap(),
            "127.0.0.1:8750".parse().unwrap()
        );
        assert!(
            loopback("127.0.0.2:8750").is_ok(),
            "all of 127/8 is loopback"
        );
        for refused in [
            "0.0.0.0:8750",
            "[::]:8750",
            "192.168.1.10:8750",
            "8.8.8.8:443",
            "example.com:8750",
            "127.0.0.1",
            "localhost",
            "localhost:x",
        ] {
            let why = loopback(refused).unwrap_err();
            assert!(why.contains("tunnel"), "{refused}: {why}");
        }
    }

    #[test]
    fn a_public_url_is_https_with_a_host() {
        assert!(public_url("https://snob.example.com").is_ok());
        assert!(public_url("https://snob.example.com:8443/mcp").is_ok());
        assert!(public_url("http://snob.example.com").is_err());
        assert!(public_url("snob.example.com").is_err());
        assert!(public_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn loopback_and_the_public_host_are_the_hosts_taken() {
        let home = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let (hosts, origins) = allowed(home, None);
        assert_eq!(hosts, ["localhost", "127.0.0.1", "::1"]);
        assert!(origins.is_empty(), "no browser origin at all");
        let (hosts, _) = allowed("::1".parse().unwrap(), None);
        assert_eq!(hosts, ["localhost", "127.0.0.1", "::1"]);

        // Bound elsewhere in 127/8, the address a client is told is taken.
        let (hosts, _) = allowed("127.0.0.2".parse().unwrap(), None);
        assert_eq!(hosts, ["localhost", "127.0.0.1", "::1", "127.0.0.2"]);

        let public = public_url("https://snob.example.com/mcp").unwrap();
        let (hosts, origins) = allowed(home, Some(&public));
        assert_eq!(hosts.last().unwrap(), "snob.example.com");
        assert_eq!(origins, ["https://snob.example.com:443"]);

        let public = public_url("https://snob.example.com:8443").unwrap();
        let (_, origins) = allowed(home, Some(&public));
        assert_eq!(origins, ["https://snob.example.com:8443"]);
    }

    /// At the cap, the oldest connection that has not shown the key makes
    /// room; when every one has shown it, a new one is refused.
    #[tokio::test]
    async fn a_connection_without_the_key_makes_room_for_one_with_it() {
        let mut tasks = JoinSet::new();
        let mut open = Open::default();
        let mut seen = Vec::new();
        for _ in 0..MOST_CONNECTIONS {
            assert!(open.admit());
            let one = Arc::new(Seen::default());
            let task = tasks.spawn(std::future::pending::<()>());
            open.push(Arc::clone(&one), task);
            seen.push(one);
        }
        // Every one but the third has shown the key.
        for (n, one) in seen.iter().enumerate() {
            one.keyed.store(n != 2, Ordering::SeqCst);
        }
        assert!(open.admit(), "the one without the key makes room");
        assert_eq!(open.0.len(), MOST_CONNECTIONS - 1);
        assert!(
            open.0
                .iter()
                .all(|(one, _)| one.keyed.load(Ordering::SeqCst)),
            "the one without the key is the one that went"
        );
        let keyed = Arc::new(Seen::default());
        keyed.keyed.store(true, Ordering::SeqCst);
        open.push(keyed, tasks.spawn(std::future::pending::<()>()));
        assert!(!open.admit(), "every one open has shown the key");
        tasks.abort_all();
    }

    #[test]
    fn only_the_key_as_a_bearer_lets_a_request_in() {
        let key = key::Key::generate().unwrap();
        let gate = Gate::new(&key).unwrap();
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            gate.lets_in(&headers)
        };
        assert!(with(&format!("Bearer {}", key.expose())));
        assert!(with(&format!("bearer {}", key.expose())), "any case");
        assert!(with(&format!("BEARER {} ", key.expose())));
        assert!(!with(&format!("Basic {}", key.expose())));
        assert!(!with(key.expose()), "no scheme");
        assert!(!with(&format!("Bearer {}x", key.expose())));
        assert!(!with("Bearer "));
        let other = key::Key::generate().unwrap();
        assert!(!with(&format!("Bearer {}", other.expose())));
        assert!(!gate.lets_in(&HeaderMap::new()));
        // What the server keeps is not the key.
        assert!(!gate.expected.contains(key.expose()));
    }

    #[test]
    fn only_an_initialize_with_no_session_opens_one() {
        let mut none = HeaderMap::new();
        let hello = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let call = br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}"#;
        assert!(opens_a_session(&Method::POST, &none, hello));
        assert!(!opens_a_session(&Method::POST, &none, call));
        assert!(!opens_a_session(&Method::GET, &none, hello));
        assert!(!opens_a_session(&Method::POST, &none, b"not json"));
        none.insert("mcp-session-id", "abc".parse().unwrap());
        assert!(!opens_a_session(&Method::POST, &none, hello));
    }

    #[test]
    fn a_refusal_closes_its_connection() {
        let refused = unauthorized();
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(refused.headers()[header::CONNECTION], "close");
        assert_eq!(
            refused.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"snob\""
        );
        assert!(
            plain(StatusCode::PAYLOAD_TOO_LARGE, "x", true)
                .headers()
                .contains_key(header::CONNECTION)
        );
        assert!(
            !plain(StatusCode::NOT_FOUND, "x", false)
                .headers()
                .contains_key(header::CONNECTION)
        );
    }
}
