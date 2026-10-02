//! The browser snob starts, and the DevTools protocol it is driven over.
//!
//! **Nothing here touches the user's own browser, or the cookie store that
//! belongs to it.** What this drives is a browser *this program started*,
//! pointed at a profile directory under snob's own data directory — empty until
//! the user logs into Instagram themselves, in the window that opens in front
//! of them, or until `headless/` writes a pasted session into it. The cookie
//! then comes back from that browser, through the browser's own debugging
//! protocol, and describes a session the user created a moment earlier. The
//! same profile, without a window, is what every request is sent from.
//!
//! That boundary is deliberate, and it is where the project stops. Reading the
//! real browser's store instead would mean going through the encryption the
//! operating system put around it — on Windows, App-Bound Encryption since
//! Chrome 127 — and that protection is there on purpose. There is no need to go
//! near it: a profile of our own answers the same question, with the user
//! signing in themselves and watching it happen, and that is the route taken.
//!
//! `Storage.getCookies` is the method that matters, because it returns
//! `HttpOnly` cookies too. `sessionid` is `HttpOnly`, which is also why no
//! console snippet can ever read it.
//!
//! **The protocol travels on a pipe, not on a port**: a port hands the session
//! to every account on the machine, as `crate::pipe` tells.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use snob_ig::login::BrowserCookies;
use snob_ig::pace::CancelToken;

use crate::browser::Browser;
use crate::pipe::{BrowserProcess, PipeTransport};

mod connection;

pub use connection::{CallError, Connection, Event, OnAttach, PauseRule, Paused};

/// How long to wait for the browser to answer its first command. Opening the
/// pipe is the first thing it does, so this only ever runs out when it did not
/// start.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for someone to finish logging in. Generous on purpose:
/// two-factor codes arrive by SMS and people go looking for their phone.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Gap between cookie checks, while the login is young.
///
/// Each check asks the browser for its whole cookie jar and reads it back
/// over the pipe, so the interval is how much of that a login costs. Two
/// seconds for the first half minute, so a person who was already signed in
/// is not kept waiting; after that [`POLL_INTERVAL_SETTLED`], because nobody
/// completes a two-factor prompt in under five seconds, and three hundred
/// polls over a ten-minute wait would be work nobody reads.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Gap between cookie checks once the login has been going a while.
const POLL_INTERVAL_SETTLED: Duration = Duration::from_secs(5);

/// How long the quick interval lasts before the settled one takes over.
const POLL_QUICKLY_FOR: Duration = Duration::from_secs(30);

/// Gap between looks at the pipe while waiting for the browser to come up.
///
/// Short, because the other thing this wait watches is the process: Chrome's
/// profile singleton exits within a second, and the whole point of noticing
/// that is not to sit out the startup timeout first.
const POLL_FOR_READY: Duration = Duration::from_millis(100);

/// How long an ordinary browser command may take before the call gives up.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(20);

const LOGIN_URL: &str = "https://www.instagram.com/accounts/login/";

/// A browser we started, and the pipe the protocol travels on.
///
/// Killed when dropped, so an early exit — a failed handshake, an error —
/// takes it down without having to remember to. The ways out that skip
/// destructors end it too, as `crate::pipe` tells.
pub struct Launched {
    process: BrowserProcess,
    /// Handed to the dispatcher by [`Cdp::connect`].
    transport: PipeTransport,
    /// Only to name it in a message when the browser leaves early.
    profile: PathBuf,
    /// Started without a window, to send requests from rather than to log in
    /// with. Only to word that same message: there is no window to close.
    windowless: bool,
}

/// Starts the browser against one of our own profiles with debugging
/// enabled, in a window, on the login page.
pub fn launch(browser: &Browser, profile: &Path) -> Result<Launched> {
    launch_in(browser, profile, &[], LOGIN_URL, false)
}

/// Starts the same browser on a profile with no window, for snob to send its
/// requests from, or for a question asked of the browser itself. See
/// `headless/mod.rs` for why each flag is there.
pub fn launch_headless(browser: &Browser, profile: &Path, flags: &[String]) -> Result<Launched> {
    launch_in(browser, profile, flags, "about:blank", true)
}

/// Removes a profile lock another machine left behind.
///
/// Chromium locks a profile with a link named after the host and process that
/// hold it, clears one whose process on *this* host is gone, and refuses one
/// from any other host, since it cannot see whether that process lives. A
/// container recreated over a kept volume is a new host every time: measured,
/// every launch then exits 21 for good, until the file is deleted by hand.
/// Nothing but this program uses this profile, and nothing on another host is
/// using it now, so a lock naming another host is stale.
#[cfg(unix)]
fn clear_a_lock_left_by_another_host(profile: &Path) {
    let Ok(target) = std::fs::read_link(profile.join("SingletonLock")) else {
        return;
    };
    let target = target.to_string_lossy();
    let Some((host, _pid)) = target.rsplit_once('-') else {
        return;
    };
    let mut name = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length, and the call writes at
    // most that many bytes into it.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return;
    }
    let ours = std::ffi::CStr::from_bytes_until_nul(&name)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    if ours.is_empty() || host == ours {
        return;
    }
    tracing::debug!(%host, %ours, "clearing a profile lock left by another host");
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
}

fn launch_in(
    browser: &Browser,
    profile: &Path,
    flags: &[String],
    start_at: &str,
    windowless: bool,
) -> Result<Launched> {
    // The profile ends up holding a live Instagram session, so the directory
    // has to be the owner's alone. The data directory above it is made private
    // by whoever chose the profile; this is the leaf.
    snob_store::paths::create_private_dir(profile)
        .with_context(|| format!("could not create {}", profile.display()))?;
    #[cfg(unix)]
    clear_a_lock_left_by_another_host(profile);

    // A profile from an older build may hold `DevToolsActivePort`. It is
    // removed rather than ignored, so nothing reading the profile concludes
    // that a port is open.
    let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));

    let mut arguments = vec![
        format!("--user-data-dir={}", profile.display()),
        // The protocol on two inherited pipes rather than on a loopback
        // socket. Nothing else on this machine can reach it, because there is
        // no address for it to reach.
        "--remote-debugging-pipe".to_string(),
        // **The pipe alone makes the page report `navigator.webdriver ===
        // true`.** Measured in September 2026 against Chromium 141, headful
        // and headless: `--remote-debugging-pipe` sets the same
        // automation-controlled bit `--enable-automation` does, and the first
        // thing a login page's bot check reads is that property. The person
        // logging in is typing into the window themselves; the pipe is only
        // how the cookies come back afterwards.
        "--disable-blink-features=AutomationControlled".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-features=Translate".to_string(),
        // Every fresh profile makes the component updater install into a
        // `scoped_dir*` under the system temp directory, which Chrome only
        // removes on a clean exit. That traffic goes to Google, never to the
        // site, so turning it off changes nothing Instagram can see.
        "--disable-component-update".to_string(),
        // The same goes for extensions registered on the machine and Chrome's
        // default apps: each fresh profile installs them, leaving CRX files
        // and `scoped_dir*` behind in the temp directory. Measured in
        // September 2026 on Chrome 154: the page sees the same plugins, MIME
        // types, PDF viewer and `window.chrome` with or without these two.
        "--disable-extensions".to_string(),
        "--disable-default-apps".to_string(),
    ];
    arguments.extend(flags.iter().cloned());
    arguments.push(start_at.to_string());

    // Chrome narrates to standard error: GCM registration failures, a
    // TensorFlow notice. None of it is ours and all of it lands in the middle
    // of our own instructions, so `pipe::spawn` gives it nowhere to go.
    let (process, transport) = crate::pipe::spawn(&browser.path, &arguments)
        .with_context(|| format!("could not start {}", browser.name))?;

    Ok(Launched {
        process,
        transport,
        profile: profile.to_path_buf(),
        windowless,
    })
}

/// Chromium's exit code for "another instance holds this profile and would not
/// take the command line". The code a second headless launch on a busy
/// profile exits with, where a windowed one hands over and exits 0.
const PROFILE_IN_USE: i32 = 21;

/// The browser started and left at once because another holds its profile:
/// another snob's, which a caller may wait out. Says what [`died_early`] or
/// [`windowless_died_early`] says.
#[derive(Debug)]
pub struct ProfileInUse(String);

impl std::fmt::Display for ProfileInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProfileInUse {}

/// What to say when the browser started and stopped again.
///
/// Split out so the wording can be read and changed without a browser: nothing
/// about [`Cdp::connect`]'s wait is testable without launching one, and
/// `ExitStatus` cannot be constructed portably in a test anyway, so this takes
/// the code.
fn died_early(code: Option<i32>, profile: &Path) -> String {
    match code {
        // Exiting cleanly and immediately is the singleton: the browser handed
        // its command line to the instance that already has this profile open.
        // 21 is the same answer where there was nobody to hand it to —
        // Chromium's `PROFILE_IN_USE`, measured on Linux against a profile a
        // headless snob held.
        Some(0 | PROFILE_IN_USE) => format!(
            "the browser closed straight away, which means one is already open on snob's \
             profile at {}.\n\
             Close that window and try again, or use \"snob login --paste\".",
            profile.display()
        ),
        // No code at all means something killed it — a signal on Unix, an
        // external terminate on Windows. Not the singleton: told as that, with
        // the profile path to make it convincing, it sends people hunting for
        // a window that was never open.
        None => "the browser was killed before it answered.\n\
                 Try again, or use \"snob login --paste\"."
            .to_string(),
        Some(code) => format!(
            "the browser exited with code {code} instead of starting.\n\
             Try \"snob login --paste\" instead."
        ),
    }
}

/// The same, for the browser snob sends its requests from.
///
/// Its own wording because the login's is wrong here twice over: there is no
/// window to close, and `snob login --paste` is no way round it, since a
/// pasted session is sent from this same browser. Exiting cleanly at once is
/// still the singleton — and with no window, what holds the profile is
/// another snob in the middle of a run, the monitor's included.
fn windowless_died_early(code: Option<i32>, profile: &Path) -> String {
    match code {
        Some(0 | PROFILE_IN_USE) => format!(
            "another snob is using the browser snob sends its requests from (its profile \
             is at {}).\n\
             Wait for that run to finish and try again.",
            profile.display()
        ),
        None => "the browser snob sends its requests from was killed before it started.\n\
                 Try again."
            .to_string(),
        Some(code) => format!(
            "the browser snob sends its requests from exited with code {code} instead of \
             starting."
        ),
    }
}

/// An open DevTools connection, and the browser on the other end of it.
///
/// The two travel together because neither is useful alone: the connection is
/// what asks the browser to leave, and the browser is what has to be killed if
/// it will not — and what says why, when the connection ends first.
///
/// Every method takes `&self`: the connection has a reader of its own
/// ([`connection`]), so any number of calls may be in flight at once, and
/// dropping one — a timeout, a Ctrl+C — leaves the others and the connection
/// as they were.
pub struct Cdp {
    /// First, so it is dropped before the process: a `Cdp` dropped without
    /// [`Cdp::close`] — a failed [`Cdp::connect`] among them — kills its
    /// browser, and that one only: another account's may be running beside it.
    connection: Connection,
    /// Behind a lock only for `try_wait`, which wants `&mut`; never held
    /// across an `.await`.
    process: std::sync::Mutex<BrowserProcess>,
    /// Only to name it in a message when the browser leaves early.
    profile: PathBuf,
    /// Only to word that same message: there is no window to close.
    windowless: bool,
}

impl Cdp {
    /// Takes a started browser and waits until it answers.
    ///
    /// The pipe was opened before the browser existed, so what this waits for
    /// is the browser reading it. **The wait watches the process as well as
    /// the pipe**: Chrome's profile singleton makes a second `snob login
    /// --browser` hand its command line to the instance already holding the
    /// profile and exit within a second, and that is told at once, not after
    /// thirty seconds as a silent pipe.
    pub async fn connect(launched: Launched, cancel: &CancelToken) -> Result<Self> {
        let Launched {
            process,
            transport,
            profile,
            windowless,
        } = launched;
        let cdp = Self {
            connection: Connection::start(transport),
            process: std::sync::Mutex::new(process),
            profile,
            windowless,
        };
        cdp.wait_until_ready(cancel).await?;
        Ok(cdp)
    }

    async fn wait_until_ready(&self, cancel: &CancelToken) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        let asked = self
            .connection
            .call(None, "Browser.getVersion", json!({}), STARTUP_TIMEOUT);
        tokio::pin!(asked);
        // Set once the pipe has closed without an answer. The future is done
        // then, and must not be polled again; the process check at the top of
        // each turn is what says why.
        let mut closed = false;

        loop {
            if cancel.is_canceled() {
                bail!("canceled");
            }
            // The browser leaving is an answer too, and a faster one than the
            // deadline.
            if let Some(left) = self.left_early() {
                return Err(left);
            }
            if closed {
                tokio::time::sleep(POLL_FOR_READY).await;
            } else {
                match tokio::time::timeout(POLL_FOR_READY, &mut asked).await {
                    // Any answer at all means it is reading the pipe.
                    Ok(Ok(_) | Err(CallError::Refused { .. })) => return Ok(()),
                    Ok(Err(_)) => closed = true,
                    Err(_) => {}
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "the browser did not answer its debugging pipe within {} seconds",
                    STARTUP_TIMEOUT.as_secs()
                );
            }
        }
    }

    /// What to say about a browser that has already exited, if it has:
    /// [`ProfileInUse`] when another holds its profile.
    fn left_early(&self) -> Option<anyhow::Error> {
        let ended = self
            .process
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_wait()
            .ok()
            .flatten()?;
        let said = if self.windowless {
            windowless_died_early(ended.code, &self.profile)
        } else {
            died_early(ended.code, &self.profile)
        };
        Some(match ended.code {
            Some(0 | PROFILE_IN_USE) => ProfileInUse(said).into(),
            _ => anyhow!(said),
        })
    }

    /// Closes the browser politely, so the profile is not left looking like it
    /// crashed and offering to restore tabs on the next login.
    pub async fn close(self) {
        // `call` carries its own timeout, so a browser that has stopped
        // answering delays the exit rather than preventing it.
        let _ = self
            .connection
            .call(None, "Browser.close", json!({}), CALL_TIMEOUT)
            .await;
        // It was asked to leave; this makes sure it did, and that its files
        // in the profile are let go of by the time this returns.
        let mut process = self.process.into_inner().unwrap_or_else(|e| e.into_inner());
        if process.wait_up_to(Duration::from_secs(5)).await.is_none() {
            process.kill();
            process.wait_up_to(Duration::from_secs(5)).await;
        }
    }

    /// The connection itself, for what the façade does not cover: events,
    /// and commands nobody waits for.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// One command to the browser itself, with the ordinary timeout.
    pub async fn browser_call(&self, method: &str, params: Value) -> Result<Value> {
        self.connection
            .call(None, method, params, CALL_TIMEOUT)
            .await
            .map_err(|e| self.explained(e))
    }

    /// One command to a tab this connection is attached to, with a timeout of
    /// the caller's choosing: a page fetch can legitimately take longer than
    /// the twenty seconds a browser command gets.
    pub async fn page_call(
        &self,
        session: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.connection
            .call(Some(session), method, params, timeout)
            .await
            .map_err(|e| self.explained(e))
    }

    /// A failed call, with the browser's exit added when that is why.
    fn explained(&self, error: CallError) -> anyhow::Error {
        if let CallError::Closed { method, .. } = &error
            && let Some(left) = self.left_early()
        {
            return anyhow!("{left}\n(while waiting for {method})");
        }
        anyhow::Error::new(error)
    }

    /// The browser's process id.
    ///
    /// Only so the tests can ask the operating system about that process.
    /// Nothing in the program needs it.
    pub fn browser_pid(&self) -> u32 {
        self.process.lock().unwrap_or_else(|e| e.into_inner()).id()
    }

    /// The exact User-Agent this browser sends.
    ///
    /// Worth asking for rather than reconstructing: the session is tied to it,
    /// and a User-Agent that does not match the browser that created the cookie
    /// is what makes Instagram answer `useragent mismatch`.
    pub async fn user_agent(&self) -> Result<String> {
        let result = self.browser_call("Browser.getVersion", json!({})).await?;
        result
            .get("userAgent")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the browser did not report its User-Agent"))
    }

    /// Every cookie the browser holds, `HttpOnly` ones included; none when
    /// the answer carries no list.
    pub async fn cookies(&self) -> Result<Vec<Value>> {
        let mut result = self.browser_call("Storage.getCookies", json!({})).await?;
        Ok(match result.get_mut("cookies").map(Value::take) {
            Some(Value::Array(cookies)) => cookies,
            _ => Vec::new(),
        })
    }

    /// The Instagram session currently in this browser, if there is one.
    pub async fn instagram_cookies(&self) -> Result<Option<BrowserCookies>> {
        Ok(collect(&self.cookies().await?))
    }
}

/// Whether a cookie's domain, or a host, is Instagram's own: `instagram.com`
/// or a name under it, and nothing that only looks like it.
pub(crate) fn is_instagram(domain: &str) -> bool {
    domain == "instagram.com" || domain.ends_with(".instagram.com")
}

/// Picks the Instagram cookies out of everything the browser holds.
///
/// Returns `None` until `sessionid` is there, which is what "logged in" means:
/// the other cookies show up as soon as the login page loads.
fn collect(cookies: &[Value]) -> Option<BrowserCookies> {
    collect_where(cookies, is_instagram)
}

/// The same, for the cookies a page on `host` is sent: those whose domain is
/// the host or one it sits under, as a browser matches them. Instagram's
/// `www.instagram.com` takes `.instagram.com`'s; a local fake takes its own.
pub fn collect_for(cookies: &[Value], host: &str) -> Option<BrowserCookies> {
    collect_where(cookies, |domain| {
        let domain = domain.trim_start_matches('.');
        !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
    })
}

fn collect_where(cookies: &[Value], ours: impl Fn(&str) -> bool) -> Option<BrowserCookies> {
    let mut found = BrowserCookies::default();

    for cookie in cookies {
        let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
        if !ours(domain) {
            continue;
        }
        let (Some(name), Some(value)) = (
            cookie.get("name").and_then(Value::as_str),
            cookie.get("value").and_then(Value::as_str),
        ) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }

        match name {
            "sessionid" => found.sessionid = value.into(),
            "ds_user_id" => found.ds_user_id = Some(value.to_string()),
            "csrftoken" => found.csrftoken = Some(value.into()),
            "mid" => found.mid = Some(value.to_string()),
            "ig_did" => found.ig_did = Some(value.to_string()),
            "datr" => found.datr = Some(value.to_string()),
            _ => {}
        }
    }

    (!found.sessionid.is_empty()).then_some(found)
}

/// Waits for the login to happen, checking every couple of seconds.
pub async fn wait_for_login(cdp: &Cdp, cancel: &CancelToken) -> Result<BrowserCookies> {
    let started = tokio::time::Instant::now();
    let deadline = started + LOGIN_TIMEOUT;

    loop {
        if cancel.is_canceled() {
            bail!("canceled");
        }
        if let Some(cookies) = cdp.instagram_cookies().await? {
            return Ok(cookies);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no login happened within {} minutes",
                LOGIN_TIMEOUT.as_secs() / 60
            );
        }
        if cancel
            .sleep_or_cancel(poll_interval(started.elapsed()))
            .await
        {
            bail!("canceled");
        }
    }
}

/// Which of the two intervals applies, given how long the login has run.
fn poll_interval(elapsed: Duration) -> Duration {
    if elapsed < POLL_QUICKLY_FOR {
        POLL_INTERVAL
    } else {
        POLL_INTERVAL_SETTLED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first half minute polls quickly; after that, at a pace a person
    /// finding their phone will not notice.
    #[test]
    fn the_poll_slows_down_once_the_login_has_been_going_a_while() {
        assert_eq!(poll_interval(Duration::ZERO), POLL_INTERVAL);
        assert_eq!(poll_interval(Duration::from_secs(29)), POLL_INTERVAL);
        assert_eq!(
            poll_interval(Duration::from_secs(30)),
            POLL_INTERVAL_SETTLED
        );
        assert_eq!(
            poll_interval(Duration::from_secs(500)),
            POLL_INTERVAL_SETTLED
        );
        assert!(POLL_INTERVAL < POLL_INTERVAL_SETTLED);
    }

    /// A second `snob login --browser` hands its command line to the instance
    /// already holding the profile and exits within a second. What is said is
    /// that, not a startup timeout.
    #[test]
    fn a_browser_that_exited_cleanly_names_the_profile() {
        let profile = Path::new("C:/somewhere/browser-profile");
        let said = died_early(Some(0), profile);
        assert!(said.contains("already open"), "{said}");
        assert!(said.contains("browser-profile"), "{said}");
        assert!(said.contains("--paste"), "{said}");
        assert!(
            !said.contains("did not open"),
            "that is the startup timeout, a different failure: {said}"
        );
    }

    /// Without a window, the profile is held by another snob mid-run, and the
    /// login's advice — close the window, paste instead — would send somebody
    /// looking for a window that does not exist.
    #[test]
    fn a_windowless_browser_names_the_other_snob() {
        let profile = Path::new("/home/someone/.local/share/snob/browser-profile");
        let said = windowless_died_early(Some(0), profile);
        assert!(said.contains("another snob"), "{said}");
        assert!(said.contains("browser-profile"), "{said}");
        for wrong in ["window", "--paste"] {
            assert!(!said.contains(wrong), "{wrong}: {said}");
        }
        assert_eq!(windowless_died_early(Some(PROFILE_IN_USE), profile), said);
        let said = windowless_died_early(Some(3), profile);
        assert!(
            said.contains('3') && !said.contains("another snob"),
            "{said}"
        );
    }

    fn cookie(name: &str, value: &str, domain: &str) -> Value {
        json!({ "name": name, "value": value, "domain": domain })
    }

    #[test]
    fn it_collects_the_session_once_it_appears() {
        let cookies = vec![
            cookie("sessionid", "42%3AAbCd%3A20", ".instagram.com"),
            cookie("ds_user_id", "42", ".instagram.com"),
            cookie("csrftoken", "tok", ".instagram.com"),
            cookie("mid", "m", "instagram.com"),
            cookie("ig_did", "d", ".instagram.com"),
            cookie("datr", "dt", ".instagram.com"),
        ];

        let found = collect(&cookies).unwrap();
        assert_eq!(found.sessionid.expose(), "42%3AAbCd%3A20");
        assert_eq!(found.ds_user_id.as_deref(), Some("42"));
        assert_eq!(
            found
                .csrftoken
                .as_ref()
                .map(snob_core::secret::Secret::expose),
            Some("tok")
        );
        assert_eq!(found.ig_did.as_deref(), Some("d"));
        // The device cookie the login was made on. Left behind, every request
        // after the login comes from a browser Instagram has never seen, on a
        // session it has just handed to one it has.
        assert_eq!(found.datr.as_deref(), Some("dt"));
    }

    /// Everything but `sessionid` is there from the moment the login page
    /// loads, so only `sessionid` means the login actually happened.
    #[test]
    fn before_the_login_there_is_no_session_yet() {
        let cookies = vec![
            cookie("csrftoken", "tok", ".instagram.com"),
            cookie("mid", "m", ".instagram.com"),
        ];
        assert!(collect(&cookies).is_none());
    }

    #[test]
    fn an_empty_session_cookie_does_not_count_as_a_login() {
        assert!(collect(&[cookie("sessionid", "", ".instagram.com")]).is_none());
    }

    /// Only Instagram's own cookies. A look-alike domain must not be read, and
    /// no other site's cookies are of any interest.
    #[test]
    fn other_sites_are_left_alone() {
        let cookies = vec![
            cookie("sessionid", "someone-elses", "notinstagram.com"),
            cookie("sessionid", "also-not", "instagram.com.example.net"),
            cookie("sessionid", "nope", "example.com"),
        ];
        assert!(collect(&cookies).is_none());

        let real = vec![cookie("sessionid", "mine", "www.instagram.com")];
        assert_eq!(collect(&real).unwrap().sessionid.expose(), "mine");
    }

    /// A page is sent the cookies of its host and of the domains above it,
    /// and nothing of a look-alike: the rule a browser matches by.
    #[test]
    fn the_cookies_of_a_host_are_the_ones_it_is_sent() {
        let jar = vec![
            cookie("sessionid", "mine", ".instagram.com"),
            cookie("csrftoken", "tok", "www.instagram.com"),
            cookie("datr", "elsewhere", "i.instagram.com"),
            cookie("mid", "no", "notinstagram.com"),
        ];
        let found = collect_for(&jar, "www.instagram.com").unwrap();
        assert_eq!(found.sessionid.expose(), "mine");
        assert_eq!(
            found
                .csrftoken
                .as_ref()
                .map(snob_core::secret::Secret::expose),
            Some("tok")
        );
        assert_eq!(
            found.datr, None,
            "a sibling host's cookie is not this one's"
        );
        assert_eq!(found.mid, None);

        let local = vec![cookie("sessionid", "fake", "127.0.0.1")];
        assert_eq!(
            collect_for(&local, "127.0.0.1").unwrap().sessionid.expose(),
            "fake"
        );
        assert!(collect_for(&local, "127.0.0.2").is_none());
    }

    /// Three different things happen when a launched browser has gone, and
    /// each is told as itself. **No code at all is not the singleton**:
    /// something killed the process, and the message names no profile and no
    /// window to close. A failure code is neither.
    #[test]
    fn what_killed_the_browser_decides_what_is_said() {
        let profile = std::path::Path::new("/tmp/snob-profile");

        let killed = died_early(None, profile);
        assert!(killed.contains("killed"), "{killed}");
        assert!(
            !killed.contains("already open"),
            "there is no window to close: {killed}"
        );

        let failed = died_early(Some(3), profile);
        assert!(failed.contains("code 3"), "{failed}");
        let failed = died_early(Some(127), profile);
        assert!(failed.contains("127"), "{failed}");
        assert!(!failed.contains("already open"), "{failed}");
    }

    /// Every line of these has to start where the terminal puts it. A literal
    /// continued without its trailing backslash carries the source's newline
    /// and indentation into the string: a gap in the middle of a sentence and
    /// a hanging indent, which `report::indented` widens further.
    #[test]
    fn the_browser_messages_have_no_source_indentation_in_them() {
        let profile = std::path::Path::new("/tmp/snob-profile");
        for message in [
            died_early(Some(0), profile),
            died_early(None, profile),
            died_early(Some(3), profile),
        ] {
            assert!(
                !message.contains("  "),
                "a run of spaces survived: {message:?}"
            );
            for line in message.lines() {
                assert!(!line.starts_with(' '), "a line is indented: {line:?}");
            }
        }
    }
}
