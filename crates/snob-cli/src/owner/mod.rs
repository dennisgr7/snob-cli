//! One owner of the browsers for every command of this user's.
//!
//! **Why.** A browser profile is Chromium's, and a second browser on it hands
//! over to the first and exits: two commands on one account, each with a
//! browser of its own, could not both send, the second finding the browser
//! held by another snob. Each would also pay a browser's cold start — seconds,
//! and hundreds of megabytes — however little it asked. So the browsers belong
//! to one process, the owner, started by the first command that needs one and
//! gone once it has nothing to do ([`server`]); the commands are its clients.
//! Two views of one account share its browser, their requests taking turns on
//! its tab and paced by the budget they already share through the database; a
//! terminal on another account gets that account's browser; and a command that
//! follows another finds the browser warm.
//!
//! **How.** A command sends snob's own requests over a socket only this user
//! can reach ([`socket`]) and never the browser's protocol ([`wire`]). The
//! pacing, the budget and everything else about a request stay in the
//! command; the owner only sends. A push-back one browser hears is told to
//! every command the moment it is heard. When a command ends it reads the
//! cookies of each browser it used and writes back what they rotated
//! (`headless::write_back`), then leaves them to the owner.
//!
//! **When it cannot be reached** — it would not start, a socket path too long
//! for the platform, a pipe another user took first, an owner of a newer
//! build, or of an older one still serving other commands — the command runs
//! its browser itself until it leaves; a monitor tries the owner again at its
//! next run. `SNOB_NO_OWNER=1` asks for that outright, for good.
//!
//! **What the browsers log goes to the owner's log**, `browser-owner.log` in
//! the data directory, at the level of the command that started the owner:
//! the app's calls the listener heard refused, among them.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::client::page::{
    AskFuture, Page, PageError, PageFactory, PageFuture, PageRequest, PageResponse, PushedBack,
};
use snob_ig::web::{Call, Told};
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;
use tokio::sync::{mpsc, oneshot};

use crate::headless::write_back::Rotated;

mod server;
mod socket;
mod spawn;
mod wire;

pub use server::{Linger, serve};

/// The hidden command the owner runs as.
pub const COMMAND: &str = "__browser-owner";

/// How long a command waits for an owner to answer, one it started included,
/// before it runs its browser itself.
const REACH: Duration = Duration::from_secs(10);

/// How long a request may take beyond its own timeout: the owner may have to
/// start the browser and load the site first.
const ALLOWANCE: Duration = Duration::from_secs(120);

/// How long leaving may take: with nothing to linger for, the owner closes
/// the browsers first.
const LEAVING: Duration = Duration::from_secs(30);

/// This process's way to the owner, once something has asked for a page.
static REMOTE: OnceLock<Arc<Remote>> = OnceLock::new();

/// Sends every request of every client built from now on through the owner
/// of the browsers. `start` is what the owner is started with: the global
/// flags that decide where it keeps its files, then [`COMMAND`]; `None` has
/// this process run its browsers itself, which is what `SNOB_NO_OWNER` asks.
///
/// Called once from `main`, before any client exists.
pub fn install(paths: &AppPaths, start: Option<Vec<OsString>>) {
    // Which binary this is, fixed now rather than at the first request.
    wire::build();
    let remote = REMOTE
        .get_or_init(|| Arc::new(Remote::new(paths.clone(), start)))
        .clone();
    let factory: PageFactory = Arc::new(move |session: &Session| {
        Arc::new(AsSession {
            remote: Arc::clone(&remote),
            session: session.clone(),
        }) as Arc<dyn Page>
    });
    let _ = snob_ig::client::page::send_every_request_from(factory);
}

/// Ends a run: leaves the owner's browsers to it and closes this process's
/// own, then writes back what each held for the accounts this command sent
/// as (`headless::write_back`).
pub async fn finish(secrets: &SecretStore, paths: &AppPaths) {
    let mut held = match REMOTE.get() {
        Some(remote) => remote.leave().await,
        None => Vec::new(),
    };
    held.extend(crate::headless::close_own().await);
    crate::headless::write_back::keep(secrets, paths, held);
}

/// Closes the account's browser, or every one, and waits for it to close:
/// before something removes or replaces the profiles they are on, or when a
/// browser holds a session that is not to be sent again. The owner's, of
/// whichever build, when one is running, and this process's own, when it
/// runs any (`SNOB_NO_OWNER`, or an owner it could not reach). Starts no
/// owner.
///
/// Closed, not held closed: a command still connected to the owner starts
/// the browser again at its next request.
pub async fn release(paths: &AppPaths, pk: Option<Pk>) {
    if let Ok(link) = Link::open(paths, None, Arc::default()).await
        && let Err(e) = link
            .call(|id| wire::ToOwner::Release { id, pk }, LEAVING)
            .await
    {
        tracing::debug!(error = %e, "the owner of the browsers did not close them");
    }
    crate::headless::release_own(pk).await;
}

/// Closes every browser the owner runs, of whichever build, and has it leave,
/// and waits for it to be gone: before the data directory it keeps its log
/// in is removed, which Windows refuses while the file is open. Starts no
/// owner.
pub async fn quit(paths: &AppPaths) {
    let Ok(mut link) = Link::open(paths, None, Arc::default()).await else {
        return;
    };
    // Frames go out in order, so the answer to the release says the owner
    // has read the retire before it.
    if let Ok(frame) = wire::frame(&wire::ToOwner::Retire) {
        let _ = link.outgoing.send(frame);
    }
    let _ = link
        .call(|id| wire::ToOwner::Release { id, pk: None }, LEAVING)
        .await;
    let owner = link.owner.take();
    // Its last command is this one, so it leaves once this link is gone.
    drop(link);
    if let Some(owner) = owner {
        let _ = tokio::task::spawn_blocking(move || owner.wait_up_to(LEAVING)).await;
    }
}

/// Whether an owner is running for these paths. For the tests.
pub async fn is_running(paths: &AppPaths) -> bool {
    Link::open(paths, None, Arc::default()).await.is_ok()
}

/// The page every client of this process sends from.
struct Remote {
    paths: AppPaths,
    /// What the owner is started with; `None` when this process runs its
    /// browsers itself for good.
    start: Option<Vec<OsString>>,
    link: tokio::sync::Mutex<Option<Arc<Link>>>,
    /// What the owner said it heard, per account, until this command leaves.
    heard: Arc<Mutex<HashMap<Pk, PushedBack>>>,
    /// The accounts whose push-back the owner heard this command was pointed
    /// to its log for, once each.
    told: Mutex<HashSet<Pk>>,
    /// The accounts this command sent as, with the fingerprint of the session
    /// it handed: what the owner hands back cookies for only while the
    /// browser holds that very session.
    used: Mutex<HashMap<Pk, String>>,
    /// Set once the owner could not be reached: the browsers are this
    /// process's own until this command leaves. A monitor's next run tries
    /// the owner again, since what kept this one from it may have passed.
    alone: AtomicBool,
}

impl Remote {
    fn new(paths: AppPaths, start: Option<Vec<OsString>>) -> Self {
        Self {
            paths,
            start,
            link: tokio::sync::Mutex::new(None),
            heard: Arc::default(),
            told: Mutex::new(HashSet::new()),
            used: Mutex::new(HashMap::new()),
            alone: AtomicBool::new(false),
        }
    }

    /// What the owner is started with, while this process sends through one.
    fn through_the_owner(&self) -> Option<&[OsString]> {
        self.start
            .as_deref()
            .filter(|_| !self.alone.load(Ordering::SeqCst))
    }

    /// The connection to the owner, made again after one that broke.
    async fn link(&self, start: &[OsString]) -> Result<Arc<Link>> {
        let mut slot = self.link.lock().await;
        if let Some(link) = slot.as_ref().filter(|link| link.alive()) {
            return Ok(Arc::clone(link));
        }
        let link = Arc::new(Link::open(&self.paths, Some(start), Arc::clone(&self.heard)).await?);
        *slot = Some(Arc::clone(&link));
        Ok(link)
    }

    /// The link to the owner a request as `session` goes through, the
    /// session noted as used; `None` when this process runs the browsers
    /// itself, which it does from the first time the owner could not be
    /// reached.
    async fn through(&self, session: &Session) -> Option<Arc<Link>> {
        let start = self.through_the_owner()?;
        match self.link(start).await {
            Ok(link) => {
                self.used
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(session.ds_user_id, session.fingerprint());
                Some(link)
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "the owner of the browsers could not be reached; running one here");
                self.alone.store(true, Ordering::SeqCst);
                None
            }
        }
    }

    /// This process's own browsers.
    fn here(&self) -> Arc<crate::headless::Headless> {
        crate::headless::alone(&self.paths)
    }

    /// Sends `request` as `session`: through the owner, or from this
    /// process's own browser when the owner could not be reached.
    async fn send_as(
        &self,
        session: &Session,
        request: PageRequest,
    ) -> Result<PageResponse, PageError> {
        let Some(link) = self.through(session).await else {
            return self.here().send_as(session, request).await;
        };
        let patience = Duration::from_millis(request.timeout_ms) + ALLOWANCE;
        let session = Box::new(session.clone());
        let said = link
            .call(
                move |id| wire::ToOwner::Send {
                    id,
                    session,
                    request,
                },
                patience,
            )
            .await;
        match owner_said(said)? {
            wire::FromOwner::Answer { answer, .. } => answer,
            other => Err(unexpected(&other)),
        }
    }

    /// Answers `call` as `session`: through the owner, or from this
    /// process's own browser when the owner could not be reached, as
    /// [`Self::send_as`] sends.
    async fn ask_as(&self, session: &Session, call: Call) -> Result<Told, PageError> {
        let Some(link) = self.through(session).await else {
            return self.here().ask_as(session, call).await;
        };
        let patience = Duration::from_millis(call.timeout_ms) + ALLOWANCE;
        let session = Box::new(session.clone());
        let call = Box::new(call);
        let said = link
            .call(move |id| wire::ToOwner::Ask { id, session, call }, patience)
            .await;
        match owner_said(said)? {
            wire::FromOwner::Told { told, .. } => told.map(|told| *told),
            other => Err(unexpected(&other)),
        }
    }

    /// Reads what each browser this command used holds, then leaves them.
    async fn leave(&self) -> Vec<Rotated> {
        let used: Vec<(Pk, String)> = self
            .used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        self.heard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.told.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.alone.store(false, Ordering::SeqCst);
        let Some(link) = self.link.lock().await.take().filter(|link| link.alive()) else {
            return Vec::new();
        };
        // A command stopped by Ctrl+C, or a stop key in the interactive
        // browser, does not wait for the cookies: they wait for the request
        // it gave up, which the owner may still be sending. What the browser
        // rotated stays in its profile.
        let used = if crate::interrupt::interrupted() {
            Vec::new()
        } else {
            used
        };
        let mut held = Vec::new();
        for (pk, handed) in used {
            let asked = handed.clone();
            match link
                .call(
                    |id| wire::ToOwner::Cookies {
                        id,
                        pk,
                        handed: asked,
                    },
                    LEAVING,
                )
                .await
            {
                Ok(wire::FromOwner::Cookies {
                    cookies: Some(cookies),
                    ..
                }) => held.push(Rotated::new(pk, handed, cookies)),
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "could not read the browser's cookies"),
            }
        }
        if let Err(e) = link.call(|id| wire::ToOwner::Leave { id }, LEAVING).await {
            tracing::debug!(error = %e, "the owner of the browsers did not see this command leave");
        }
        held
    }

    /// What was heard for the account, by the owner or by this process's own
    /// browser.
    fn heard_for(&self, pk: Pk) -> Option<PushedBack> {
        if self.through_the_owner().is_none() {
            return crate::headless::local(&self.paths).heard_for(pk);
        }
        let heard = self
            .heard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pk)
            .cloned()?;
        if self
            .told
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pk)
        {
            tracing::warn!(
                "Instagram pushed back on the page's own calls; which call, and what it answered, is in {}",
                log_of(&self.paths).display()
            );
        }
        Some(heard)
    }
}

/// The page one client sends from: the owner, as the session the client was
/// built for.
struct AsSession {
    remote: Arc<Remote>,
    session: Session,
}

impl Page for AsSession {
    fn send(&self, request: PageRequest) -> PageFuture<'_> {
        Box::pin(self.remote.send_as(&self.session, request))
    }

    fn ask(&self, call: Call) -> AskFuture<'_> {
        Box::pin(self.remote.ask_as(&self.session, call))
    }

    fn heard(&self) -> Option<PushedBack> {
        self.remote.heard_for(self.session.ds_user_id)
    }
}

/// Requests waiting for their answers, by id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<wire::FromOwner>>>>;

/// One connection to the owner. To an owner of another build, only for what
/// every build reads: `Release` and `Retire`.
struct Link {
    /// Whole frames, for the one task that writes them: a request given up
    /// half-way through being written would leave the rest of the stream
    /// unreadable.
    outgoing: mpsc::UnboundedSender<zeroize::Zeroizing<Vec<u8>>>,
    pending: Pending,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
    tasks: [tokio::task::JoinHandle<()>; 2],
    /// The owner's process, where it can be waited for.
    owner: Option<socket::Owner>,
}

impl Drop for Link {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Forgets a request whose caller stopped waiting, so its late answer is
/// dropped rather than kept.
struct Forget<'a>(&'a Pending, u64);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.1);
    }
}

impl Link {
    /// Connects, starting the owner when nobody is listening and `start`
    /// says how, and checks it is an owner of this build. An owner of an
    /// older build serving nobody else is asked to leave and one of this
    /// build started in its place; one still serving other commands is left
    /// to them, as is one of a newer build, and this command runs its browser
    /// itself. With no `start`, an owner of any build will do, for
    /// [`release`] and [`quit`].
    async fn open(
        paths: &AppPaths,
        start: Option<&[OsString]>,
        heard: Arc<Mutex<HashMap<Pk, PushedBack>>>,
    ) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + REACH;
        let mut started: Option<spawn::Started> = None;
        let mut retired = false;
        loop {
            match socket::connect(paths).await {
                Ok(stream) => match handshake(stream, &wire::build()).await {
                    Ok(Handshake::Ours(stream)) => {
                        if start.is_some() && started.is_none() {
                            tracing::debug!(
                                log = %log_of(paths).display(),
                                "the owner of the browsers was running already; the browsers log there, at the level of the command that started it"
                            );
                        }
                        return Ok(Self::over(stream, heard));
                    }
                    // Asked already, and still taking commands for a moment.
                    Ok(Handshake::Theirs { .. }) if retired => {}
                    Ok(Handshake::Theirs { stream, .. }) if start.is_none() => {
                        return Ok(Self::over(stream, heard));
                    }
                    Ok(Handshake::Theirs {
                        mut stream,
                        build,
                        others,
                    }) => {
                        if others > 0 || !wire::older(&build, &wire::build()) {
                            let why = if others > 0 {
                                "still serving other commands"
                            } else {
                                "a newer one"
                            };
                            tracing::warn!(
                                other = %build,
                                "the owner of the browsers is another build of snob, {why}; this command runs its own browser"
                            );
                            bail!("the owner of the browsers running is another build of snob");
                        }
                        wire::write(&mut stream, &wire::frame(&wire::ToOwner::Retire)?).await?;
                        retired = true;
                        started = None;
                    }
                    // An owner on its way out, most likely: it stopped
                    // listening as this connected.
                    Err(e) => {
                        tracing::debug!(error = %format!("{e:#}"), "no answer from the owner")
                    }
                },
                Err(e) if socket::nobody_there(&e) => {
                    let Some(args) = start else {
                        bail!("no owner of the browsers is running");
                    };
                    match started.as_ref().map(spawn::Started::gone) {
                        None => started = Some(start_one(paths, args)?),
                        Some(false) => {}
                        // It found the name still held by the owner it
                        // replaces, which lets go of it in a moment.
                        Some(true) if retired => started = Some(start_one(paths, args)?),
                        Some(true) => bail!(
                            "the owner of the browsers left as soon as it started; {} says why",
                            log_of(paths).display()
                        ),
                    }
                }
                // On Windows the pipe of an owner just asked to leave lasts
                // until its last connection has closed.
                Err(e) if retired => {
                    tracing::debug!(error = %e, "the retiring owner still holds its name")
                }
                Err(e) => return Err(e).context("could not reach the owner of the browsers"),
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "the owner of the browsers did not answer within {} seconds",
                    REACH.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn over(stream: socket::Stream, heard: Arc<Mutex<HashMap<Pk, PushedBack>>>) -> Self {
        let owner = socket::owner_of(&stream);
        let (mut from, mut to) = tokio::io::split(stream);
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (outgoing, mut queue) = mpsc::unbounded_channel::<zeroize::Zeroizing<Vec<u8>>>();
        let writer = tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                if wire::write(&mut to, &frame).await.is_err() {
                    return;
                }
            }
        });
        let reader = {
            let (pending, alive) = (Arc::clone(&pending), Arc::clone(&alive));
            tokio::spawn(async move {
                loop {
                    let body = match wire::read_body(&mut from).await {
                        Ok(Some(body)) => body,
                        Ok(None) | Err(_) => break,
                    };
                    // An owner of another build may say what this one cannot
                    // read; the link is not lost over it.
                    match serde_json::from_slice::<wire::FromOwner>(&body) {
                        Ok(wire::FromOwner::Heard { pk, cause }) => {
                            heard
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(pk, cause);
                        }
                        Ok(message) => {
                            let waiting = message.id().and_then(|id| {
                                pending
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&id)
                            });
                            if let Some(waiting) = waiting {
                                let _ = waiting.send(message);
                            }
                        }
                        // Not said with the parser's words, which can quote
                        // the message, and a message can carry a session.
                        Err(_) => tracing::debug!("skipped a message from the owner"),
                    }
                }
                // Every request still waiting fails now, and every later one
                // at once.
                alive.store(false, Ordering::SeqCst);
                pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
            })
        };
        Self {
            outgoing,
            pending,
            next: AtomicU64::new(1),
            alive,
            tasks: [writer, reader],
            owner,
        }
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Sends what `make` builds with a fresh id and waits for its answer.
    async fn call(
        &self,
        make: impl FnOnce(u64) -> wire::ToOwner,
        patience: Duration,
    ) -> Result<wire::FromOwner> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (answered, answer) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, answered);
        let _forget = Forget(&self.pending, id);
        if !self.alive() {
            bail!("the connection to it is closed");
        }
        let frame = wire::frame(&make(id))?;
        self.outgoing
            .send(frame)
            .map_err(|_| anyhow!("the connection to it is closed"))?;
        match tokio::time::timeout(patience, answer).await {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(_)) => bail!("it went away before answering"),
            Err(_) => bail!("no answer within {} seconds", patience.as_secs()),
        }
    }
}

enum Handshake {
    Ours(socket::Stream),
    /// An owner of another build, serving `others` commands besides this one.
    Theirs {
        stream: socket::Stream,
        build: String,
        others: usize,
    },
}

/// Says this is the binary `build`, and hears who the owner is.
async fn handshake(mut stream: socket::Stream, build: &str) -> Result<Handshake> {
    let hello = wire::frame(&wire::ToOwner::Hello {
        build: build.to_string(),
    })?;
    wire::write(&mut stream, &hello).await?;
    let welcome = tokio::time::timeout(
        Duration::from_secs(5),
        wire::read::<wire::FromOwner>(&mut stream),
    )
    .await
    .context("it did not answer")??;
    match welcome {
        Some(wire::FromOwner::Welcome { build: theirs, .. }) if theirs == build => {
            Ok(Handshake::Ours(stream))
        }
        Some(wire::FromOwner::Welcome {
            build: theirs,
            others,
        }) => Ok(Handshake::Theirs {
            stream,
            build: theirs,
            others,
        }),
        Some(other) => bail!("it answered {other:?}"),
        None => bail!("it closed the connection"),
    }
}

/// Where the owner writes what it logs.
fn log_of(paths: &AppPaths) -> PathBuf {
    std::path::absolute(paths.data_dir())
        .unwrap_or_else(|_| paths.data_dir().to_path_buf())
        .join("browser-owner.log")
}

/// Starts an owner, from this very binary, in the background.
fn start_one(paths: &AppPaths, args: &[OsString]) -> Result<spawn::Started> {
    // An owner started as root could only fail, the browser refusing root,
    // and would leave root's files in a data directory the user's own owners
    // then cannot open. The browser this process starts instead says why.
    crate::headless::refuse_root()?;
    snob_store::paths::create_private_dir(paths.data_dir())?;
    let program = std::env::current_exe().context("could not find this program to start it")?;
    let started = spawn::spawn(&program, args, &log_of(paths))
        .context("could not start the owner of the browsers")?;
    tracing::debug!("started the owner of the browsers");
    Ok(started)
}

/// What the owner said, or the link's failure as the browser's.
fn owner_said(said: Result<wire::FromOwner>) -> Result<wire::FromOwner, PageError> {
    said.map_err(|e| PageError::Browser(format!("the owner of the browsers: {e}")))
}

/// An answer of the owner's that is not the one asked for.
fn unexpected(other: &wire::FromOwner) -> PageError {
    PageError::Browser(format!("the owner of the browsers answered {other:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connects as the binary `build`, once the owner listens.
    async fn reach(paths: &AppPaths, build: &str) -> Handshake {
        let deadline = tokio::time::Instant::now() + REACH;
        while tokio::time::Instant::now() < deadline {
            if let Ok(stream) = socket::connect(paths).await {
                return handshake(stream, build).await.expect("the owner answers");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no owner listened");
    }

    /// A command that ran its browser itself, the owner out of reach, tries
    /// the owner again once it has left: a monitor's next run is not held to
    /// what kept its last one from it.
    #[tokio::test]
    async fn a_command_that_fell_back_tries_the_owner_again_after_leaving() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(root.path());
        let remote = Remote::new(paths.clone(), Some(Vec::new()));
        assert!(remote.through_the_owner().is_some());
        remote.alone.store(true, Ordering::SeqCst);
        assert!(
            remote.through_the_owner().is_none(),
            "not for the rest of this run"
        );
        assert!(remote.leave().await.is_empty());
        assert!(
            remote.through_the_owner().is_some(),
            "and the next run tries it first"
        );

        let own = Remote::new(paths, None);
        own.leave().await;
        assert!(
            own.through_the_owner().is_none(),
            "SNOB_NO_OWNER is for good"
        );
    }

    /// What the owner said it heard is this command's until it leaves: a
    /// monitor's next run, once the cooldown is over, is not stopped by the
    /// last one's push-back.
    #[tokio::test]
    async fn a_push_back_the_owner_told_of_ends_with_the_command() {
        let root = tempfile::tempdir().unwrap();
        let remote = Remote::new(AppPaths::rooted_at(root.path()), Some(Vec::new()));
        let pk = Pk::new(42);
        remote
            .heard
            .lock()
            .unwrap()
            .insert(pk, PushedBack::RateLimited);
        assert_eq!(remote.heard_for(pk), Some(PushedBack::RateLimited));
        assert!(remote.leave().await.is_empty());
        assert_eq!(remote.heard_for(pk), None);
    }

    /// Says why a test that needs a browser does not run, or fails it where
    /// one is required.
    fn no_browser_here() -> bool {
        let why = if crate::headless::refuse_root().is_err() {
            "running as root, where the browser will not start"
        } else if crate::browser::detect().is_none() {
            "no browser installed"
        } else {
            return false;
        };
        assert!(
            !crate::headless::env_flag("SNOB_TEST_REQUIRE_BROWSER"),
            "{why}, and SNOB_TEST_REQUIRE_BROWSER needs one"
        );
        eprintln!("{why}; skipping");
        true
    }

    /// An intent crosses to the owner and is answered from its browser, and
    /// is answered by this process's own browser with no owner to send
    /// through; one the allowlist refuses, handed to the owner straight
    /// over its socket, is refused there without a browser.
    #[tokio::test]
    async fn an_intent_is_answered_through_the_owner_and_without_one() {
        use snob_ig::allowlist::Operation;
        use snob_ig::web::Ask;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        if no_browser_here() {
            return;
        }
        let site = MockServer::start().await;
        let document = r#"<!doctype html><title>site</title><script type="application/json">{"define":[["PolarisViewer",[],{"data":{"id":"42","username":"me","fbid":"17841400000000042"},"id":"42"},1508]]}</script>"#;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(document, "text/html"))
            .mount(&site)
            .await;
        let call = |ask: Ask| Call {
            ask,
            origin: site.uri(),
            referrer: "/".to_string(),
            claim: "0".to_string(),
            cap: 1024 * 1024,
            timeout_ms: 20_000,
        };
        let session = Session::from_sessionid(
            "42%3Aowner%3A1",
            "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(root.path());
        let owner = tokio::spawn({
            let paths = paths.clone();
            let engine = crate::headless::Headless::apart(&paths);
            async move { server::run_on(&paths, Linger::for_a_sandbox(), wire::build(), engine).await }
        });

        // Handed over by the client, as every intent is.
        let remote = Remote::new(paths.clone(), Some(vec![OsString::from("--list")]));
        let told = remote.ask_as(&session, call(Ask::Viewer)).await;
        let told = match told {
            Err(PageError::Browser(e)) => {
                assert!(
                    !crate::headless::env_flag("SNOB_TEST_REQUIRE_BROWSER"),
                    "the browser would not start: {e}"
                );
                eprintln!("the browser found here will not start ({e}); skipping");
                remote.leave().await;
                return;
            }
            told => told.unwrap(),
        };
        assert!(
            matches!(&told, Told::Viewer(v) if v.pk == Pk::new(42) && v.username == "me"),
            "{told:?}"
        );
        assert!(
            !remote.alone.load(Ordering::SeqCst),
            "through the owner, not a browser of this process's"
        );

        // A write named as a read, handed to the owner by something that is
        // not the client: refused where it would be built.
        let Handshake::Ours(mut forged) = reach(&paths, &wire::build()).await else {
            panic!("an owner of this build is ours");
        };
        let follow = Ask::Query {
            operation: Operation::Follow,
            variables: r#"{"target_user_id":"9001"}"#.to_string(),
        };
        let asked = wire::frame(&wire::ToOwner::Ask {
            id: 5,
            session: Box::new(session.clone()),
            call: Box::new(call(follow)),
        })
        .unwrap();
        wire::write(&mut forged, &asked).await.unwrap();
        let answer = loop {
            match wire::read::<wire::FromOwner>(&mut forged).await.unwrap() {
                Some(wire::FromOwner::Heard { .. }) => {}
                other => break other,
            }
        };
        assert!(
            matches!(
                &answer,
                Some(wire::FromOwner::Told { id: 5, told: Err(PageError::NotAllowed(what)) })
                    if what == "POST /api/graphql"
            ),
            "{answer:?}"
        );
        drop(forged);
        remote.leave().await;
        tokio::time::timeout(Duration::from_secs(30), owner)
            .await
            .expect("the owner leaves with its last command")
            .unwrap()
            .unwrap();
        assert!(
            site.received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .all(|r| r.method.as_str() == "GET"),
            "the refused write never left"
        );

        // With no owner to send through, this process's own browser answers.
        let alone = Remote::new(paths.clone(), None);
        let told = alone.ask_as(&session, call(Ask::Viewer)).await.unwrap();
        assert!(
            matches!(&told, Told::Viewer(v) if v.pk == Pk::new(42)),
            "{told:?}"
        );
        crate::headless::release_own(Some(Pk::new(42))).await;
    }

    /// A command of another build leaves alone an owner still serving a
    /// command of its own, and an owner asked to leave all the same finishes
    /// with the commands it has before it goes.
    #[tokio::test]
    async fn an_owner_other_commands_use_is_not_retired_from_under_them() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(root.path());
        let owner = tokio::spawn({
            let paths = paths.clone();
            async move { server::run(&paths, Linger::for_a_sandbox(), "A".to_string()).await }
        });

        let Handshake::Ours(mut ours) = reach(&paths, "A").await else {
            panic!("an owner of the same build is ours");
        };
        let Handshake::Theirs { build, others, .. } = reach(&paths, "B").await else {
            panic!("an owner of another build is not");
        };
        assert_eq!(build, "A");
        assert_eq!(others, 1, "it says it serves another command");

        // This binary is not `A` either. `--list` is what a regression that
        // retired the owner would start in its place: this test binary,
        // listing its tests and gone.
        let start = [OsString::from("--list")];
        assert!(
            Link::open(&paths, Some(&start), Arc::default())
                .await
                .is_err(),
            "a command of another build runs its own browser"
        );
        assert!(
            matches!(reach(&paths, "A").await, Handshake::Ours(_)),
            "and the owner still takes commands"
        );

        // A first message that is not `Hello` is not answered.
        let mut stranger = socket::connect(&paths).await.unwrap();
        let leave = wire::frame(&wire::ToOwner::Leave { id: 1 }).unwrap();
        wire::write(&mut stranger, &leave).await.unwrap();
        assert!(
            wire::read::<wire::FromOwner>(&mut stranger)
                .await
                .unwrap_or(None)
                .is_none()
        );

        // Asked to leave, as a purge asks, it still answers the command it has.
        let Handshake::Theirs { mut stream, .. } = reach(&paths, "B").await else {
            panic!("an owner of another build is not");
        };
        let retire = wire::frame(&wire::ToOwner::Retire).unwrap();
        wire::write(&mut stream, &retire).await.unwrap();
        drop(stream);
        let release = wire::frame(&wire::ToOwner::Release { id: 7, pk: None }).unwrap();
        wire::write(&mut ours, &release).await.unwrap();
        assert!(matches!(
            wire::read::<wire::FromOwner>(&mut ours).await.unwrap(),
            Some(wire::FromOwner::Done { id: 7 })
        ));
        assert!(!owner.is_finished(), "not while a command is connected");

        // And leaves once that command has.
        drop(ours);
        tokio::time::timeout(Duration::from_secs(10), owner)
            .await
            .expect("the owner leaves with its last command")
            .unwrap()
            .unwrap();
    }
}
