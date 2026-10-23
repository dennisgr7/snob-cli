//! The login shown in a browser on another machine: `snob login --remote`.
//!
//! **What it is for.** Over SSH there is no window to log in through: a Linux
//! machine with no display cannot open one, and a Mac or a Windows PC opens it
//! on a screen the person is not looking at (`crate::seat`). So the browser
//! that would have opened the window runs here without one, on the same
//! profile a windowed login uses, and what it shows is relayed to a page in
//! the browser of the machine the person is at, through a tunnel they open
//! with `ssh -L`; their clicks and keys come back the same way. They log into
//! Instagram themselves, in snob's browser, as they would in the window, and
//! the cookies come back from that browser over its pipe, as in every login
//! (`cdp::wait_for_login`). Nothing is read out of the browser they are
//! looking at: it only shows pictures.
//!
//! **What crosses the tunnel is pictures and input, never the protocol.** The
//! debugging protocol stays on the pipe (`crate::pipe` tells why a port is
//! not acceptable for it). The page is sent JPEG frames of the tab
//! (`Page.startScreencast`) on an event stream and posts back mouse, wheel,
//! key and text events, which `input` turns into the protocol's own
//! `Input.*` commands, the only ones the page can cause, besides moving
//! through the tab's history and reloading it. No address, no script, no
//! cookie and no protocol message is taken from the page.
//!
//! **Who can reach it.**
//!
//! - On Unix, a socket in the data directory, which is private, made `0600`
//!   and checked at every connection for this user at the other end, as the
//!   owner's is (`owner::socket`): `ssh -L port:/path/to/socket` reaches it as
//!   the user who logged in over SSH, and nobody else on the machine can.
//! - Where that cannot be had — Windows, or a data directory too deep for a
//!   socket's path — a TCP port on `127.0.0.1`, never another address; any
//!   account on the machine can connect to that, so the next point is what
//!   keeps them out.
//! - Either way the printed link carries a token of 256 random bits, good for
//!   one visit: the first request that brings it is answered with a cookie
//!   of another 256 bits, `HttpOnly` and `SameSite=Strict`, and the token is
//!   spent. Every other route asks for the cookie; the input route also asks
//!   for a header of its own, which a page on another site cannot send
//!   without a preflight this server never answers. A `Host` that is not a
//!   loopback name is refused, which is what a site the person has open would
//!   send after making its own name resolve to the tunnel (DNS rebinding).
//!
//! **What it leaves out.** The tab is a desktop's, 1920 by 1032 as every
//! request's browser is (`headless`), whatever the screen it is watched on: the
//! profile is the device, and the device does not change shape. Native
//! pop-ups a headless browser does not paint — a `<select>`'s list, a
//! `window.open` — are not shown; Instagram's login uses neither. A
//! JavaScript dialog is accepted at once, since there is nobody to show it to.

#[cfg(unix)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use snob_ig::login::BrowserCookies;
use snob_ig::pace::CancelToken;
use snob_store::paths::AppPaths;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::cdp::{self, CALL_TIMEOUT, Cdp, Connection, Event};
use crate::progress::Progress;
use crate::seat::Ssh;

mod http;
mod input;

const PAGE: &str = include_str!("page.html");
const SCRIPT: &str = include_str!("page.js");
const STYLE: &str = include_str!("page.css");

/// The cookie the spent token is traded for.
const COOKIE: &str = "snob_relay";

/// The header the input route asks for besides the cookie. A page on another
/// site can only send it after a preflight, which nothing here answers.
const INPUT_HEADER: &str = "x-snob-input";

/// How long a frame waits for a page to have sent it before the browser is
/// told it was taken anyway.
///
/// The acknowledgment is the screencast's flow control: the browser sends
/// the next frame only once the last is acknowledged, so a frame is
/// acknowledged once a page has written it out, and a tunnel slower than the
/// browser paints gets fewer frames rather than a queue. With no page
/// connected the browser still paints once a second, so whoever connects next
/// starts from a picture of now.
const FRAME_PATIENCE: Duration = Duration::from_secs(1);

/// How often an idle event stream says something, so that nothing along the
/// way closes it for silence.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How many events the screencast's subscription may fall behind. The
/// browser sends one frame at a time and waits for it to be acknowledged, so
/// a few are plenty; the rest are navigation and dialogs.
const EVENT_ROOM: usize = 64;

/// The JPEG quality of a frame: legible text at a fraction of the size.
const QUALITY: u8 = 70;

/// Where the relay listens, and the tunnel's near end to suggest for it.
pub struct Relay {
    listener: Listener,
    token: String,
    cookie: String,
    /// The port on the person's machine the instructions name.
    local_port: u16,
}

impl Relay {
    /// Starts listening. Done before the browser starts, so that a relay that
    /// cannot listen costs nothing.
    pub fn bind(paths: &AppPaths) -> Result<Self> {
        let listener = Listener::bind(paths)?;
        let local_port = match &listener {
            Listener::Tcp(tcp) => tcp.local_addr()?.port(),
            // Anything in the dynamic range: the person's machine is unlikely
            // to be using it, and they can change it in the command.
            #[cfg(unix)]
            Listener::Socket { .. } => fastrand::u16(49_152..=65_535),
        };
        Ok(Self {
            listener,
            token: secret()?,
            cookie: secret()?,
            local_port,
        })
    }

    /// The command to open the tunnel with, on the person's machine.
    pub fn tunnel(&self, ssh: Option<&Ssh>) -> String {
        let far_end = match &self.listener {
            Listener::Tcp(tcp) => format!(
                "127.0.0.1:{}",
                tcp.local_addr().map(|a| a.port()).unwrap_or_default()
            ),
            #[cfg(unix)]
            Listener::Socket { path, .. } => path.display().to_string(),
        };
        let server = ssh.and_then(|s| s.server.as_ref());
        let host = server.map_or("this-machine", |(address, _)| address.as_str());
        let user = ssh
            .and_then(|s| s.user.as_deref())
            .map(|user| format!("{user}@"))
            .unwrap_or_default();
        let port = match server {
            Some((_, port)) if *port != 22 => format!(" -p {port}"),
            _ => String::new(),
        };
        format!("ssh -N -L {}:{far_end} {user}{host}{port}", self.local_port)
    }

    /// The link to open once the tunnel is up, with its token.
    pub fn link(&self) -> String {
        format!("http://127.0.0.1:{}/{}", self.local_port, self.token)
    }

    /// Relays the tab `tab` of `cdp` to whoever opens the link, until the
    /// browser holds an Instagram session, and returns it.
    ///
    /// Two waits, each as long as a login gets: for the link to be opened,
    /// since opening a tunnel from another machine is a step of its own, and
    /// then for the login. Ctrl+C ends either.
    pub async fn serve(
        self,
        cdp: &Cdp,
        tab: &str,
        cancel: &CancelToken,
        progress: &Progress,
    ) -> Result<BrowserCookies> {
        let Self {
            listener,
            token,
            cookie,
            ..
        } = self;
        let shared = Arc::new(Shared {
            token: Mutex::new(Some(token)),
            cookie,
            opened: watch::Sender::new(false),
            frame: watch::Sender::new(None),
            sent: watch::Sender::new(0),
            address: watch::Sender::new(String::new()),
            done: watch::Sender::new(false),
            connection: cdp.connection().clone(),
            tab: tab.to_string(),
        });

        // Everything relayed runs in these, and ends when they are dropped:
        // on the way out of this function, however it leaves.
        let mut tasks = JoinSet::new();
        // Subscribed before the screencast starts, so its first frame is not
        // missed.
        let events = cdp.connection().subscribe(Some(tab), EVENT_ROOM);
        cdp.page_call(tab, "Page.enable", json!({}), CALL_TIMEOUT)
            .await?;
        tasks.spawn(screencast(Arc::clone(&shared), events));
        // The address the tab is at already: it may have navigated before the
        // events above were asked for, and no `frameNavigated` would then
        // say where it is until the next page.
        if let Some(url) = current_address(cdp, tab).await {
            shared.address.send_if_modified(|address| {
                let unset = address.is_empty();
                if unset {
                    *address = url;
                }
                unset
            });
        }
        cdp.page_call(
            tab,
            "Page.startScreencast",
            json!({
                "format": "jpeg",
                "quality": QUALITY,
                "maxWidth": crate::headless::SCREEN.0,
                "maxHeight": crate::headless::SCREEN.1,
            }),
            CALL_TIMEOUT,
        )
        .await?;
        tasks.spawn(accept(listener, Arc::clone(&shared)));

        progress.waiting("waiting for the link to be opened", cdp::LOGIN_TIMEOUT);
        let mut opened = shared.opened.subscribe();
        tokio::select! {
            _ = async { opened.wait_for(|opened| *opened).await.is_ok() } => {}
            () = tokio::time::sleep(cdp::LOGIN_TIMEOUT) => bail!(
                "nobody opened the link within {} minutes",
                cdp::LOGIN_TIMEOUT.as_secs() / 60
            ),
            () = cancel.canceled() => bail!("canceled"),
        }
        progress.waiting("waiting for you to log in", cdp::LOGIN_TIMEOUT);
        let cookies = cdp::wait_for_login(cdp, cancel).await?;

        // Told to the page, which says it is done, and given a moment to get
        // there before the tunnel's far end goes.
        shared.done.send_replace(true);
        cdp.connection()
            .fire(Some(tab), "Page.stopScreencast", json!({}));
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(cookies)
    }
}

/// The address of the entry the tab's history is at.
async fn current_address(cdp: &Cdp, tab: &str) -> Option<String> {
    let history = cdp
        .page_call(tab, "Page.getNavigationHistory", json!({}), CALL_TIMEOUT)
        .await
        .ok()?;
    let current = usize::try_from(history.get("currentIndex")?.as_i64()?).ok()?;
    let entry = history.get("entries")?.as_array()?.get(current)?;
    Some(entry.get("url")?.as_str()?.to_string())
}

/// What the relay's tasks share.
struct Shared {
    /// The link's token, until it is spent.
    token: Mutex<Option<String>>,
    /// What the spent token was traded for.
    cookie: String,
    /// The link was opened.
    opened: watch::Sender<bool>,
    /// The latest frame, for every page connected and the next to connect.
    frame: watch::Sender<Option<Arc<Frame>>>,
    /// The last frame a page wrote out.
    sent: watch::Sender<u64>,
    /// The tab's address, which the page shows so the person can see it is
    /// Instagram they are typing into.
    address: watch::Sender<String>,
    /// The browser holds a session.
    done: watch::Sender<bool>,
    connection: Connection,
    tab: String,
}

/// One frame of the screencast, as the page is sent it.
struct Frame {
    number: u64,
    /// `{"data": base64 JPEG, "w": width, "h": height}`, the size in the
    /// tab's CSS pixels, which is what the page's coordinates are given in.
    json: String,
}

/// Reads the tab's events: frames to relay and acknowledge, the address as it
/// changes, and dialogs to dismiss.
async fn screencast(shared: Arc<Shared>, mut events: tokio::sync::mpsc::Receiver<Event>) {
    let mut number = 0;
    while let Some(Event { method, params, .. }) = events.recv().await {
        match method.as_str() {
            "Page.screencastFrame" => {
                let Some(session) = params.get("sessionId").and_then(Value::as_i64) else {
                    continue;
                };
                let data = params.get("data").and_then(Value::as_str).unwrap_or("");
                let metadata = params.get("metadata");
                let size = |name| metadata.and_then(|m| m.get(name)).and_then(Value::as_f64);
                number += 1;
                let json = json!({
                    "data": data,
                    "w": size("deviceWidth"),
                    "h": size("deviceHeight"),
                })
                .to_string();
                shared
                    .frame
                    .send_replace(Some(Arc::new(Frame { number, json })));
                let mut sent = shared.sent.subscribe();
                let _ = tokio::time::timeout(FRAME_PATIENCE, sent.wait_for(|s| *s >= number)).await;
                shared.connection.fire(
                    Some(&shared.tab),
                    "Page.screencastFrameAck",
                    json!({ "sessionId": session }),
                );
            }
            "Page.frameNavigated" => {
                let frame = params.get("frame");
                let main = frame.is_some_and(|f| f.get("parentId").is_none());
                if let Some(url) = frame
                    .filter(|_| main)
                    .and_then(|f| f.get("url"))
                    .and_then(Value::as_str)
                {
                    shared.address.send_replace(url.to_string());
                }
            }
            "Page.javascriptDialogOpening" => shared.connection.fire(
                Some(&shared.tab),
                "Page.handleJavaScriptDialog",
                json!({ "accept": true }),
            ),
            _ => {}
        }
    }
}

/// Takes connections until dropped, each in a task of its own that ends with
/// this one.
async fn accept(listener: Listener, shared: Arc<Shared>) {
    let mut connections = JoinSet::new();
    loop {
        match listener.accept().await {
            Ok(stream) => {
                connections.spawn(serve_connection(stream, Arc::clone(&shared)));
            }
            Err(e) => {
                tracing::debug!(error = %e, "refused a connection to the relay");
                // Not a busy loop on a listener that keeps failing.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // Finished connections, so the set does not grow for the length of
        // a login.
        while connections.try_join_next().is_some() {}
    }
}

/// What a route answered.
enum Answered {
    /// Ready for the next request on the connection.
    Again,
    /// The connection is done.
    Over,
}

async fn serve_connection(mut stream: Box<dyn Stream>, shared: Arc<Shared>) {
    let mut buffered = Vec::new();
    loop {
        let request = match http::read(&mut stream, &mut buffered).await {
            Ok(request) => request,
            Err(http::Unread::Closed) => return,
            Err(http::Unread::Bad) => {
                let _ = text(
                    &mut stream,
                    "400 Bad Request",
                    "Not a request snob takes.",
                    false,
                )
                .await;
                return;
            }
        };
        match route(&mut stream, &request, &shared).await {
            Ok(Answered::Again) if request.keeps_alive() => {}
            _ => return,
        }
    }
}

async fn route<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: &http::Request,
    shared: &Shared,
) -> std::io::Result<Answered> {
    let keep = request.keeps_alive();
    if !request.header("host").is_some_and(http::is_loopback_host) {
        text(
            stream,
            "403 Forbidden",
            "Open the link snob printed, as it is.",
            false,
        )
        .await?;
        return Ok(Answered::Over);
    }
    let get = request.method == "GET";
    let path = request.path.as_str();
    // The link's own path: anything that is not one of the routes below.
    if get && !matches!(path, "/" | "/page.js" | "/page.css" | "/events" | "/input") {
        return redeem(stream, path.trim_start_matches('/'), shared, keep).await;
    }
    if !request
        .cookie(COOKIE)
        .is_some_and(|c| same(c.as_bytes(), shared.cookie.as_bytes()))
    {
        text(
            stream,
            "403 Forbidden",
            "This browser has not opened the link snob printed. Each link opens once; \
             run \"snob login --remote\" again for a new one.",
            keep,
        )
        .await?;
        return Ok(Answered::Again);
    }
    match (request.method.as_str(), path) {
        ("GET", "/") => {
            http::respond(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                &[],
                PAGE.as_bytes(),
                keep,
            )
            .await?;
        }
        ("GET", "/page.js") => {
            http::respond(
                stream,
                "200 OK",
                "text/javascript; charset=utf-8",
                &[],
                SCRIPT.as_bytes(),
                keep,
            )
            .await?;
        }
        ("GET", "/page.css") => {
            http::respond(
                stream,
                "200 OK",
                "text/css; charset=utf-8",
                &[],
                STYLE.as_bytes(),
                keep,
            )
            .await?;
        }
        ("GET", "/events") => {
            events(stream, shared).await?;
            return Ok(Answered::Over);
        }
        ("POST", "/input") => {
            let origin_is_ours = request.header("origin").is_none_or(|origin| {
                origin
                    .strip_prefix("http://")
                    .is_some_and(|host| Some(host) == request.header("host"))
            });
            if request.header(INPUT_HEADER) != Some("1") || !origin_is_ours {
                text(stream, "403 Forbidden", "Not from snob's page.", false).await?;
                return Ok(Answered::Over);
            }
            let Some(inputs) = input::parse(&request.body) else {
                text(
                    stream,
                    "400 Bad Request",
                    "Not input snob's page sends.",
                    keep,
                )
                .await?;
                return Ok(Answered::Again);
            };
            for one in inputs {
                if let Some(command) = input::command(one) {
                    perform(shared, command).await;
                }
            }
            http::respond(stream, "204 No Content", "text/plain", &[], b"", keep).await?;
        }
        _ => {
            text(stream, "404 Not Found", "Nothing here.", keep).await?;
        }
    }
    Ok(Answered::Again)
}

/// The link's token traded for the cookie, once.
async fn redeem<S: AsyncWrite + Unpin>(
    stream: &mut S,
    offered: &str,
    shared: &Shared,
    keep: bool,
) -> std::io::Result<Answered> {
    let taken = {
        let mut token = shared.token.lock().unwrap_or_else(|e| e.into_inner());
        let matches = token
            .as_deref()
            .is_some_and(|t| same(t.as_bytes(), offered.as_bytes()));
        if matches {
            *token = None;
        }
        matches
    };
    if !taken {
        text(
            stream,
            "404 Not Found",
            "This link is not valid, or has been opened already. Each link opens once; \
             run \"snob login --remote\" again for a new one.",
            keep,
        )
        .await?;
        return Ok(Answered::Again);
    }
    shared.opened.send_replace(true);
    let cookie = format!(
        "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict",
        shared.cookie
    );
    http::respond(
        stream,
        "303 See Other",
        "text/plain",
        &[("Location", "/"), ("Set-Cookie", &cookie)],
        b"",
        keep,
    )
    .await?;
    Ok(Answered::Again)
}

/// The event stream a page reads frames from, until the login is done or the
/// page goes.
async fn events<S: AsyncWrite + Unpin>(stream: &mut S, shared: &Shared) -> std::io::Result<()> {
    http::start_events(stream).await?;
    let mut frames = shared.frame.subscribe();
    frames.mark_changed();
    let mut address = shared.address.subscribe();
    address.mark_changed();
    let mut done = shared.done.subscribe();
    loop {
        tokio::select! {
            changed = frames.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let frame = frames.borrow_and_update().clone();
                if let Some(frame) = frame {
                    stream.write_all(http::event("frame", &frame.json).as_bytes()).await?;
                    stream.flush().await?;
                    shared.sent.send_modify(|sent| *sent = (*sent).max(frame.number));
                }
            }
            changed = address.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let now = address.borrow_and_update().clone();
                if !now.is_empty() {
                    let said = Value::String(now).to_string();
                    stream.write_all(http::event("address", &said).as_bytes()).await?;
                    stream.flush().await?;
                }
            }
            _ = async { done.wait_for(|done| *done).await.is_ok() } => {
                stream.write_all(http::event("done", "true").as_bytes()).await?;
                stream.flush().await?;
                return Ok(());
            }
            () = tokio::time::sleep(KEEP_ALIVE) => {
                stream.write_all(b": still here\n\n").await?;
                stream.flush().await?;
            }
        }
    }
}

/// Sends what one input became to the tab. A failure is logged and the next
/// input tried: a click the tab missed is clicked again.
async fn perform(shared: &Shared, command: input::Command) {
    let tab = Some(shared.tab.as_str());
    let call = |method: &'static str, params: Value| {
        shared.connection.call(tab, method, params, CALL_TIMEOUT)
    };
    let outcome = match command {
        input::Command::Send(method, params) => call(method, params).await.map(drop),
        input::Command::Reload => call("Page.reload", json!({})).await.map(drop),
        input::Command::History(by) => match call("Page.getNavigationHistory", json!({})).await {
            Ok(history) => {
                let current = history.get("currentIndex").and_then(Value::as_i64);
                let entry = current
                    .map(|current| current + by)
                    .and_then(|wanted| usize::try_from(wanted).ok())
                    .and_then(|wanted| history.get("entries")?.as_array()?.get(wanted).cloned())
                    .and_then(|entry| entry.get("id").cloned());
                match entry {
                    Some(id) => call("Page.navigateToHistoryEntry", json!({ "entryId": id }))
                        .await
                        .map(drop),
                    None => Ok(()),
                }
            }
            Err(e) => Err(e),
        },
    };
    if let Err(e) = outcome {
        tracing::debug!(error = %e, "the tab did not take an input");
    }
}

async fn text<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: &str,
    said: &str,
    keep: bool,
) -> std::io::Result<()> {
    http::respond(
        stream,
        status,
        "text/plain; charset=utf-8",
        &[],
        said.as_bytes(),
        keep,
    )
    .await
}

/// Whether two secrets are equal, in a time that does not depend on where
/// they first differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// 256 random bits from the operating system, as text that fits in a path
/// and a cookie.
fn secret() -> Result<String> {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes).context("the system would not hand out random bytes")?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(unix)]
fn fill_random(bytes: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read as _;
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}

#[cfg(windows)]
fn fill_random(bytes: &mut [u8]) -> std::io::Result<()> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let length = u32::try_from(bytes.len()).map_err(std::io::Error::other)?;
    // SAFETY: the buffer is valid for writes of `length` bytes, and a null
    // algorithm handle is what the system-preferred generator flag asks for.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "BCryptGenRandom failed with {status:#x}"
        )))
    }
}

/// A connection, whichever listener it came from.
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Where the relay listens.
enum Listener {
    /// A socket file only this user can reach.
    #[cfg(unix)]
    Socket {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    },
    /// A port on `127.0.0.1`.
    Tcp(tokio::net::TcpListener),
}

/// The longest socket path taken, short of the 104 bytes macOS allows and
/// the 108 Linux does.
#[cfg(unix)]
const MOST_SOCKET_PATH: usize = 100;

impl Listener {
    fn bind(paths: &AppPaths) -> Result<Self> {
        #[cfg(unix)]
        {
            let dir = std::path::absolute(paths.data_dir())?;
            let name = format!("relay-{:08x}.sock", fastrand::u32(..));
            let path = dir.join(name);
            if path.as_os_str().len() <= MOST_SOCKET_PATH {
                match bind_socket(&path) {
                    Ok(listener) => return Ok(Self::Socket { listener, path }),
                    Err(e) => {
                        tracing::debug!(error = %e, "no socket for the relay; a port instead");
                    }
                }
            }
        }
        #[cfg(not(unix))]
        let _ = paths;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("could not listen on 127.0.0.1 for the relay")?;
        listener.set_nonblocking(true)?;
        Ok(Self::Tcp(tokio::net::TcpListener::from_std(listener)?))
    }

    async fn accept(&self) -> std::io::Result<Box<dyn Stream>> {
        match self {
            #[cfg(unix)]
            Self::Socket { listener, .. } => {
                let (stream, _) = listener.accept().await?;
                same_user(&stream)?;
                Ok(Box::new(stream))
            }
            Self::Tcp(listener) => {
                let (stream, _) = listener.accept().await?;
                stream.set_nodelay(true)?;
                Ok(Box::new(stream))
            }
        }
    }
}

/// Removes the socket file, which is this relay's alone.
impl Drop for Listener {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Self::Socket { path, .. } = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn bind_socket(path: &std::path::Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;
    let listener = tokio::net::UnixListener::bind(path)?;
    // The data directory is private already; this is the leaf.
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(listener)
}

#[cfg(unix)]
fn same_user(stream: &tokio::net::UnixStream) -> std::io::Result<()> {
    let theirs = stream.peer_cred()?.uid();
    // SAFETY: takes nothing and cannot fail.
    let ours = unsafe { libc::geteuid() };
    if theirs == ours {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the other end runs as user {theirs}, not {ours}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(connection: Connection) -> Shared {
        Shared {
            token: Mutex::new(Some("the-token".to_string())),
            cookie: "the-cookie".to_string(),
            opened: watch::Sender::new(false),
            frame: watch::Sender::new(None),
            sent: watch::Sender::new(0),
            address: watch::Sender::new(String::new()),
            done: watch::Sender::new(false),
            connection,
            tab: "tab".to_string(),
        }
    }

    /// What `route` writes for `raw`, against `shared`.
    async fn answer(raw: &str, shared: &Shared) -> String {
        let mut bytes = raw.as_bytes();
        let request = http::read(&mut bytes, &mut Vec::new()).await.unwrap();
        let mut written = Vec::new();
        route(&mut written, &request, shared).await.unwrap();
        String::from_utf8(written).unwrap()
    }

    #[tokio::test]
    async fn the_token_opens_the_page_once() {
        let shared = shared(Connection::detached());
        let first = answer(
            "GET /the-token HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n",
            &shared,
        )
        .await;
        assert!(first.starts_with("HTTP/1.1 303"), "{first}");
        assert!(
            first.contains("Set-Cookie: snob_relay=the-cookie; Path=/; HttpOnly; SameSite=Strict"),
            "{first}"
        );
        assert!(*shared.opened.borrow());

        let again = answer(
            "GET /the-token HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n\r\n",
            &shared,
        )
        .await;
        assert!(again.starts_with("HTTP/1.1 404"), "{again}");
        assert!(again.contains("opened already"), "{again}");
    }

    #[tokio::test]
    async fn a_wrong_token_is_spent_on_nothing() {
        let shared = shared(Connection::detached());
        let wrong = answer("GET /guess HTTP/1.1\r\nHost: localhost\r\n\r\n", &shared).await;
        assert!(wrong.starts_with("HTTP/1.1 404"), "{wrong}");
        assert!(shared.token.lock().unwrap().is_some());
        assert!(!*shared.opened.borrow());
    }

    /// The page and everything after it take the cookie, and nothing else.
    #[tokio::test]
    async fn the_page_asks_for_the_cookie() {
        let shared = shared(Connection::detached());
        let without = answer("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n", &shared).await;
        assert!(without.starts_with("HTTP/1.1 403"), "{without}");
        assert!(!without.contains("<html"), "{without}");
        let wrong = answer(
            "GET / HTTP/1.1\r\nHost: localhost\r\nCookie: snob_relay=the-cooki\r\n\r\n",
            &shared,
        )
        .await;
        assert!(wrong.starts_with("HTTP/1.1 403"), "{wrong}");
        let with = answer(
            "GET / HTTP/1.1\r\nHost: localhost\r\nCookie: snob_relay=the-cookie\r\n\r\n",
            &shared,
        )
        .await;
        assert!(with.starts_with("HTTP/1.1 200"), "{with}");
        assert!(with.contains("<html"), "{with}");
        assert!(
            with.contains("Content-Security-Policy: default-src 'none'"),
            "{with}"
        );
        assert!(with.contains("Cache-Control: no-store"), "{with}");
    }

    /// A name that only resolves here is another site's page, whatever it
    /// carries.
    #[tokio::test]
    async fn another_host_is_refused_even_with_the_token() {
        let shared = shared(Connection::detached());
        let refused = answer(
            "GET /the-token HTTP/1.1\r\nHost: rebound.example:8765\r\n\r\n",
            &shared,
        )
        .await;
        assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
        assert!(
            shared.token.lock().unwrap().is_some(),
            "the token was not spent"
        );
    }

    /// Input takes the cookie, the page's own header, and an origin that is
    /// the page's.
    #[tokio::test]
    async fn input_from_another_page_is_refused() {
        let shared = shared(Connection::detached());
        let cookie = "Cookie: snob_relay=the-cookie\r\n";
        let body = r#"[{"t":"reload"}]"#;
        let post = |extra: &str| {
            format!(
                "POST /input HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n{extra}Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let no_header = answer(&post(cookie), &shared).await;
        assert!(no_header.starts_with("HTTP/1.1 403"), "{no_header}");
        let elsewhere = answer(
            &post(&format!(
                "{cookie}X-Snob-Input: 1\r\nOrigin: https://site.example\r\n"
            )),
            &shared,
        )
        .await;
        assert!(elsewhere.starts_with("HTTP/1.1 403"), "{elsewhere}");
        let ours = answer(
            &post(&format!(
                "{cookie}X-Snob-Input: 1\r\nOrigin: http://127.0.0.1:8765\r\n"
            )),
            &shared,
        )
        .await;
        assert!(ours.starts_with("HTTP/1.1 204"), "{ours}");
        let garbage = format!(
            "POST /input HTTP/1.1\r\nHost: 127.0.0.1:8765\r\n{cookie}X-Snob-Input: 1\r\n\
             Content-Length: 3\r\n\r\nnot"
        );
        let bad = answer(&garbage, &shared).await;
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
    }

    #[test]
    fn secrets_are_compared_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
        assert!(!same(b"", b"a"));
    }

    #[test]
    fn secrets_are_long_and_different() {
        let a = secret().unwrap();
        let b = secret().unwrap();
        assert_eq!(a.len(), 43, "{a}");
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    /// The tunnel names this machine as the SSH client reached it, with the
    /// port only when it is not the usual one, and the link the port the
    /// tunnel opens.
    #[tokio::test]
    async fn the_tunnel_and_the_link_name_the_same_port() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(temporary.path());
        paths.ensure_dirs().unwrap();
        let relay = Relay::bind(&paths).unwrap();
        let ssh = Ssh {
            server: Some(("192.168.1.50".to_string(), 2222)),
            user: Some("someone".to_string()),
        };
        let tunnel = relay.tunnel(Some(&ssh));
        let port = relay.local_port.to_string();
        assert!(
            tunnel.starts_with(&format!("ssh -N -L {port}:")),
            "{tunnel}"
        );
        assert!(
            tunnel.ends_with(" someone@192.168.1.50 -p 2222"),
            "{tunnel}"
        );
        assert!(
            relay
                .link()
                .starts_with(&format!("http://127.0.0.1:{port}/"))
        );
        assert!(relay.link().ends_with(&relay.token));

        let usual = Ssh {
            server: Some(("10.0.0.1".to_string(), 22)),
            user: None,
        };
        assert!(relay.tunnel(Some(&usual)).ends_with(" 10.0.0.1"));
        assert!(relay.tunnel(None).ends_with(" this-machine"));
    }

    /// On Unix the relay is a socket only this user can open, and it goes
    /// with the relay.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_socket_is_private_and_removed_after() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(temporary.path());
        paths.ensure_dirs().unwrap();
        let relay = Relay::bind(&paths).unwrap();
        let Listener::Socket { path, .. } = &relay.listener else {
            panic!("a temporary directory is short enough for a socket");
        };
        let path = path.clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(relay.tunnel(None).contains(&path.display().to_string()));
        drop(relay);
        assert!(!path.exists());
    }
}
