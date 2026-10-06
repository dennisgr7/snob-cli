//! The owner of the browsers: the process that runs them, for every command
//! of this user's.
//!
//! It holds one browser per account in use (`headless`), each on the
//! account's profile, and sends what each command asks as the session the
//! command hands it. A browser closes [`IDLE`] after its last
//! request, or — once Instagram has pushed back on it, or in a sandbox —
//! as soon as none of the commands that used it is still connected. The
//! owner leaves when it holds no browser and no command is connected.
//!
//! **The site's app keeps running in a browser while it lingers**, making its
//! own calls, and the listener keeps hearing them: a push-back heard then is
//! recorded, and the next command meets the cooldown. That is kept: a
//! person's tab does not go quiet the moment they stop clicking, and the next
//! command finds the browser warm. Nobody waits on it meanwhile, so it runs
//! efficiently (`power::qos`): at the normal pace only while a command
//! somebody waits on is using it ([`ToOwner::Attention`]).
//!
//! The one thing stored from here is the cooldown such a push-back earns,
//! written by the listener (`headless::listen`) to the account's database,
//! and from it to `shared.db` when that opens. What a browser rotated is
//! written back by the command that leaves it, which asks for the cookies
//! first ([`ToOwner::Cookies`]); a browser that closes later, idle, keeps
//! anything rotated since in its profile, where the next command reads it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use snob_core::Pk;
use snob_store::paths::AppPaths;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc};

use super::socket::{self, Listener};
use super::wire::{self, FromOwner, ToOwner};
use crate::headless::{Headless, IDLE, Open};

/// How long browsers are kept.
#[derive(Debug, Clone, Copy)]
pub struct Linger {
    /// Close a browser as soon as no connected command has used it, rather
    /// than keep it for the next.
    pub close_when_unused: bool,
    /// The longest the owner sleeps without looking.
    ///
    /// **Not how often it looks.** It wakes when something happens — a command
    /// connecting or leaving, a push-back heard, a request to leave — and
    /// otherwise when the first open browser would idle out
    /// ([`crate::headless::Reaped::next`]); this only bounds the sleep, in case
    /// something changes that tells nobody. A browser crashing is such a
    /// thing, and was never noticed by looking either.
    pub tick: Duration,
}

impl Linger {
    /// A person's: a browser is kept for the next command until it idles.
    pub fn for_people() -> Self {
        Self {
            close_when_unused: false,
            // Once a second, it woke three hundred times over a browser's
            // five idle minutes to find nothing to do on all but the last.
            tick: Duration::from_secs(60),
        }
    }

    /// A sandbox's: a browser closes as soon as the commands using it have
    /// left, before the last one is told it may go, so a test finds the
    /// profile at rest when the command returns and nothing outlives it.
    pub fn for_a_sandbox() -> Self {
        Self {
            close_when_unused: true,
            tick: Duration::from_millis(100),
        }
    }
}

/// How long an owner nobody has reached yet waits for its first command: the
/// one that started it may have died on the way.
const FIRST_COMMAND: Duration = Duration::from_secs(30);

/// Whether `open` is to be closed now, `users` commands being connected that
/// used it. An owner that was asked to leave closes at once every browser no
/// connected command uses, and keeps the others for the commands still on
/// them.
fn due(open: &Open, users: usize, linger: &Linger, retiring: bool) -> bool {
    open.idle >= IDLE || (users == 0 && (retiring || open.latched || linger.close_when_unused))
}

/// How long the loop sleeps before it looks again: until the first open
/// browser would idle out, or the first command was due to have arrived,
/// whichever comes first, and never longer than `cap`.
fn nap(next: Option<Duration>, first_command_left: Option<Duration>, cap: Duration) -> Duration {
    [next, first_command_left]
        .into_iter()
        .flatten()
        .fold(cap, Duration::min)
}

/// What every connection shares with the loop that decides when to close.
struct State {
    /// Which binary this owner was started from ([`wire::build`]).
    build: String,
    /// Per account, the commands connected that have sent as it.
    users: std::sync::Mutex<HashMap<Pk, usize>>,
    /// Per account, how many of those somebody is waiting on
    /// ([`ToOwner::Attention`]): the browser runs at the normal pace while
    /// any is, and efficiently otherwise (`power::qos`).
    watched: std::sync::Mutex<HashMap<Pk, usize>>,
    connections: AtomicUsize,
    reached: AtomicBool,
    retiring: AtomicBool,
    /// Poked when any of the above changes.
    changed: Notify,
}

impl State {
    fn users(&self, pk: Pk) -> usize {
        self.users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pk)
            .copied()
            .unwrap_or(0)
    }
}

/// Runs the owner until it has nothing left to do.
pub async fn serve(paths: &AppPaths, linger: Linger) -> anyhow::Result<()> {
    // One owner after another appends to the same log, so each says which it
    // is whatever the level (`main::log_filter`).
    tracing::info!(
        pid = std::process::id(),
        build = %wire::build(),
        "the owner of the browsers is starting"
    );
    run(paths, linger, wire::build()).await
}

/// [`serve`], as the binary `build`.
pub(super) async fn run(paths: &AppPaths, linger: Linger, build: String) -> anyhow::Result<()> {
    run_on(paths, linger, build, crate::headless::local(paths)).await
}

/// [`run`], with the browsers of `engine`: this process's own, or a test's.
pub(super) async fn run_on(
    paths: &AppPaths,
    linger: Linger,
    build: String,
    engine: Arc<Headless>,
) -> anyhow::Result<()> {
    snob_store::paths::create_private_dir(paths.data_dir())?;
    let Some(listener) = socket::bind(paths)? else {
        tracing::debug!("another owner of the browsers is running");
        return Ok(());
    };
    tracing::debug!(
        pid = std::process::id(),
        "the owner of the browsers is listening"
    );
    let state = Arc::new(State {
        build,
        users: std::sync::Mutex::new(HashMap::new()),
        watched: std::sync::Mutex::new(HashMap::new()),
        connections: AtomicUsize::new(0),
        reached: AtomicBool::new(false),
        retiring: AtomicBool::new(false),
        changed: Notify::new(),
    });

    let (accepted, mut arrivals) = mpsc::channel(16);
    let mut accepting = Some(tokio::spawn(accept(listener, accepted)));
    let started = Instant::now();
    let mut quit = std::pin::pin!(asked_to_quit());
    // A push-back latches a browser, which then closes once no connected
    // command uses it; hearing it is what wakes the loop to look. The sender
    // lives in `engine`, which this holds, so the channel never closes under
    // it and a closed one cannot spin the loop.
    let mut hear = engine.hear();
    let mut sleep = linger.tick;
    loop {
        tokio::select! {
            Some(stream) = arrivals.recv() => {
                state.connections.fetch_add(1, Ordering::SeqCst);
                state.reached.store(true, Ordering::SeqCst);
                tokio::spawn(connection(stream, Arc::clone(&engine), Arc::clone(&state), linger));
            }
            () = tokio::time::sleep(sleep) => {}
            () = state.changed.notified() => {}
            _ = hear.recv() => {}
            () = &mut quit => break,
        }
        let retiring = state.retiring.load(Ordering::SeqCst);
        if retiring && let Some(accepting) = accepting.take() {
            // Stops listening, which lets an owner of the new build take the
            // name while this one finishes with the commands it has: on Unix
            // at once, on Windows once the last of them has left, since a
            // pipe's name lasts while any instance of it is open.
            accepting.abort();
        }
        let reaped = engine
            .reap(|open| due(open, state.users(open.pk), &linger, retiring))
            .await;
        let connected = state.connections.load(Ordering::SeqCst);
        let reached = state.reached.load(Ordering::SeqCst);
        let settled = reached || started.elapsed() >= FIRST_COMMAND;
        if connected == 0 && reaped.open == 0 && settled {
            break;
        }
        let first_command_left =
            (!reached).then(|| FIRST_COMMAND.saturating_sub(started.elapsed()));
        sleep = nap(reaped.next, first_command_left, linger.tick);
    }
    if let Some(accepting) = accepting.take() {
        accepting.abort();
    }
    engine.release(None).await;
    // The window holding the end of the session may let it go now.
    #[cfg(windows)]
    super::end_of_session::closed();
    tracing::debug!("the owner of the browsers is leaving");
    Ok(())
}

/// Hands every command that connects to the loop, until it is stopped.
async fn accept(mut listener: Listener, accepted: mpsc::Sender<socket::ServerStream>) {
    loop {
        match listener.accept().await {
            Ok(stream) => {
                if accepted.send(stream).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "could not accept a command");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// A signal to leave: the browsers are closed politely on the way out.
async fn asked_to_quit() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hangup), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
            signal(SignalKind::interrupt()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hangup.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
    // On Windows the owner has no console to be sent an event on: the end of
    // the session reaches it as a window message (`end_of_session`).
    #[cfg(windows)]
    match super::end_of_session::listen() {
        Some(ending) => ending.heard().await,
        None => std::future::pending::<()>().await,
    }
    #[cfg(not(any(unix, windows)))]
    std::future::pending::<()>().await
}

/// One command, for as long as it stays connected.
async fn connection<S>(stream: S, engine: Arc<Headless>, state: Arc<State>, linger: Linger)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut from, mut to) = tokio::io::split(stream);
    // Every message out goes through one writer, whole: answers finish in any
    // order, and a push-back can be told in the middle of them.
    let (outgoing, mut queue) = mpsc::unbounded_channel::<FromOwner>();
    let writer = tokio::spawn(async move {
        while let Some(message) = queue.recv().await {
            let Ok(frame) = wire::frame(&message) else {
                continue;
            };
            if wire::write(&mut to, &frame).await.is_err() {
                return;
            }
        }
    });
    let mut hear = engine.hear();
    let tell = outgoing.clone();
    let telling = tokio::spawn(async move {
        loop {
            match hear.recv().await {
                Ok((pk, cause)) => {
                    if tell.send(FromOwner::Heard { pk, cause }).is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    let mut used = HashSet::new();
    // Whether somebody waits on this command. Taken as yes until it says
    // otherwise, which a command of this build does at once: a browser runs
    // efficiently only when nobody is said to be waiting.
    let mut attended = true;
    // Whatever the command is, it is told who this is and whom else this
    // serves; one of another build then asks this owner to leave, or goes.
    if let Ok(Some(ToOwner::Hello { .. })) = wire::read::<ToOwner>(&mut from).await {
        let _ = outgoing.send(FromOwner::Welcome {
            build: state.build.clone(),
            others: state.connections.load(Ordering::SeqCst).saturating_sub(1),
        });
        serve_commands(
            &mut from,
            &outgoing,
            &engine,
            &state,
            &linger,
            &mut used,
            &mut attended,
        )
        .await;
    }

    leave(&engine, &state, &linger, &mut used, attended).await;
    telling.abort();
    drop(outgoing);
    let _ = writer.await;
    state.connections.fetch_sub(1, Ordering::SeqCst);
    state.changed.notify_one();
}

async fn serve_commands<R: AsyncRead + Unpin>(
    from: &mut R,
    outgoing: &mpsc::UnboundedSender<FromOwner>,
    engine: &Arc<Headless>,
    state: &Arc<State>,
    linger: &Linger,
    used: &mut HashSet<Pk>,
    attended: &mut bool,
) {
    loop {
        let message = match wire::read::<ToOwner>(from).await {
            Ok(Some(message)) => message,
            Ok(None) => return,
            Err(e) => {
                tracing::debug!(error = %e, "a command sent something unreadable");
                return;
            }
        };
        match message {
            ToOwner::Hello { .. } => {}
            ToOwner::Send {
                id,
                session,
                request,
            } => {
                uses(engine, state, used, session.ds_user_id, *attended);
                let (engine, outgoing) = (Arc::clone(engine), outgoing.clone());
                tokio::spawn(async move {
                    let answer = engine.send_as(&session, request).await;
                    let _ = outgoing.send(FromOwner::Answer { id, answer });
                });
            }
            ToOwner::Ask { id, session, call } => {
                uses(engine, state, used, session.ds_user_id, *attended);
                let (engine, outgoing) = (Arc::clone(engine), outgoing.clone());
                tokio::spawn(async move {
                    let told = engine.ask_as(&session, *call).await.map(Box::new);
                    let _ = outgoing.send(FromOwner::Told { id, told });
                });
            }
            ToOwner::Cookies { id, pk, handed } => {
                let (engine, outgoing) = (Arc::clone(engine), outgoing.clone());
                tokio::spawn(async move {
                    let cookies = engine.cookies_of(pk, &handed).await;
                    let _ = outgoing.send(FromOwner::Cookies { id, cookies });
                });
            }
            ToOwner::Leave { id } => {
                leave(engine, state, linger, used, *attended).await;
                let _ = outgoing.send(FromOwner::Done { id });
            }
            ToOwner::Release { id, pk } => {
                let (engine, outgoing, state) =
                    (Arc::clone(engine), outgoing.clone(), Arc::clone(state));
                tokio::spawn(async move {
                    engine.release(pk).await;
                    // The loop sleeps until something changes, and a browser
                    // fewer may be the last thing keeping the owner up.
                    state.changed.notify_one();
                    let _ = outgoing.send(FromOwner::Done { id });
                });
            }
            ToOwner::Retire => {
                tracing::debug!("asked to leave once the commands connected are done");
                state.retiring.store(true, Ordering::SeqCst);
                state.changed.notify_one();
            }
            ToOwner::Attention { attended: now } => {
                if now != *attended {
                    *attended = now;
                    for pk in used.iter() {
                        watch(state, *pk, now);
                    }
                    for pk in used.iter() {
                        reconsider(engine, state, *pk);
                    }
                }
            }
        }
    }
}

/// The command uses the account `pk`'s browser: counted once per command,
/// so the browser is left open while any command still uses it, and counted
/// as watched when somebody waits on the command.
fn uses(engine: &Headless, state: &State, used: &mut HashSet<Pk>, pk: Pk, attended: bool) {
    if used.insert(pk) {
        *state
            .users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(pk)
            .or_default() += 1;
        if attended {
            watch(state, pk, true);
        }
        reconsider(engine, state, pk);
    }
}

/// One command on `pk`'s browser more (`true`) or fewer that somebody waits
/// on.
fn watch(state: &State, pk: Pk, more: bool) {
    let mut watched = state.watched.lock().unwrap_or_else(|e| e.into_inner());
    let count = watched.entry(pk).or_default();
    *count = if more {
        *count + 1
    } else {
        count.saturating_sub(1)
    };
}

/// Tells `pk`'s browser whether anybody waits on it now, and this process
/// whether anybody waits on any: the owner runs efficiently when nobody does.
fn reconsider(engine: &Headless, state: &State, pk: Pk) {
    let (this, any) = {
        let watched = state.watched.lock().unwrap_or_else(|e| e.into_inner());
        (
            watched.get(&pk).copied().unwrap_or(0) > 0,
            watched.values().any(|n| *n > 0),
        )
    };
    engine.attend(pk, this);
    crate::power::qos::own(crate::power::qos::Mode::for_attention(any));
}

/// A command is done with the browsers it used. With nothing to linger for,
/// each one no other command is using is closed before it is told. Each one
/// that stays is efficient from now on unless another command somebody waits
/// on is using it: a browser lingering for the next command waits on nobody.
async fn leave(
    engine: &Headless,
    state: &State,
    linger: &Linger,
    used: &mut HashSet<Pk>,
    attended: bool,
) {
    for pk in used.drain() {
        if attended {
            watch(state, pk, false);
        }
        reconsider(engine, state, pk);
        let remaining = {
            let mut users = state.users.lock().unwrap_or_else(|e| e.into_inner());
            let count = users.entry(pk).or_default();
            *count = count.saturating_sub(1);
            *count
        };
        if remaining == 0 && linger.close_when_unused {
            engine.release(Some(pk)).await;
        }
    }
    state.changed.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(idle: u64, latched: bool) -> Open {
        Open {
            pk: Pk::new(42),
            idle: Duration::from_secs(idle),
            latched,
        }
    }

    #[test]
    fn a_browser_in_use_is_kept_until_it_idles() {
        let people = Linger::for_people();
        assert!(!due(&open(10, false), 1, &people, false));
        assert!(
            !due(&open(10, false), 0, &people, false),
            "kept for the next command"
        );
        assert!(due(&open(5 * 60, false), 0, &people, false));
        assert!(
            due(&open(5 * 60, false), 2, &people, false),
            "an open view does not hold a browser for ever"
        );
    }

    #[test]
    fn a_browser_pushed_back_on_goes_when_its_commands_have() {
        let people = Linger::for_people();
        assert!(
            !due(&open(0, true), 1, &people, false),
            "not under a command that has yet to hear why"
        );
        assert!(due(&open(0, true), 0, &people, false));
    }

    #[test]
    fn a_sandbox_closes_what_nobody_uses() {
        let sandbox = Linger::for_a_sandbox();
        assert!(due(&open(0, false), 0, &sandbox, false));
        assert!(!due(&open(0, false), 1, &sandbox, false));
    }

    fn state() -> State {
        State {
            build: "test".to_string(),
            users: std::sync::Mutex::new(HashMap::new()),
            watched: std::sync::Mutex::new(HashMap::new()),
            connections: AtomicUsize::new(0),
            reached: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }

    /// A browser runs at the normal pace while any command on it is waited
    /// on, efficiently once none is, and efficiently while it lingers with no
    /// command at all.
    #[tokio::test]
    async fn a_browser_is_efficient_while_nobody_waits_on_it() {
        use crate::power::qos::Mode;

        let tmp = tempfile::tempdir().unwrap();
        let engine = Headless::apart(&AppPaths::rooted_at(tmp.path()));
        let state = state();
        let people = Linger::for_people();
        let pk = Pk::new(42);

        let (mut script, mut person) = (HashSet::new(), HashSet::new());
        uses(&engine, &state, &mut script, pk, false);
        assert_eq!(engine.mode_of(pk), Mode::Eco, "a script waits on nobody");

        uses(&engine, &state, &mut person, pk, true);
        assert_eq!(
            engine.mode_of(pk),
            Mode::Normal,
            "a person at a terminal does"
        );

        leave(&engine, &state, &people, &mut person, true).await;
        assert_eq!(engine.mode_of(pk), Mode::Eco, "the person is gone");

        leave(&engine, &state, &people, &mut script, false).await;
        assert_eq!(engine.mode_of(pk), Mode::Eco, "lingering for nobody");
        assert_eq!(state.watched.lock().unwrap().get(&pk).copied(), Some(0));
    }

    /// The loop sleeps until the first thing that could need it, and never
    /// past its cap.
    #[test]
    fn the_owner_sleeps_until_the_first_thing_due() {
        let cap = Duration::from_secs(60);
        let secs = Duration::from_secs;
        assert_eq!(nap(None, None, cap), cap, "nothing open, already reached");
        assert_eq!(nap(Some(secs(240)), None, cap), cap);
        assert_eq!(nap(Some(secs(7)), None, cap), secs(7));
        assert_eq!(nap(None, Some(secs(12)), cap), secs(12));
        assert_eq!(nap(Some(secs(30)), Some(secs(12)), cap), secs(12));
        assert_eq!(nap(Some(Duration::ZERO), None, cap), Duration::ZERO);
    }

    #[test]
    fn a_retiring_owner_keeps_only_what_a_connected_command_uses() {
        let people = Linger::for_people();
        assert!(due(&open(0, false), 0, &people, true));
        assert!(
            !due(&open(0, false), 1, &people, true),
            "not under a command of the old build still using it"
        );
    }
}
