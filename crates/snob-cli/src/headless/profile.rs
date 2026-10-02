//! The profile a browser runs on, and the session it is handed: what snob
//! writes beside it, and how the cookies it carries are kept the right ones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use snob_core::Pk;
use snob_core::session::Session;
use snob_store::paths::AppPaths;

use super::{COMMAND_TIMEOUT, Live};

/// Makes sure the browser carries this session, and only this account, and
/// says whether it does: `false` when it keeps a later login of the account
/// instead.
///
/// **A login is authoritative; after it, the browser is.** The browser's cookie
/// jar is fresher than anything stored — it is where `csrftoken` and `rur`
/// rotate — so a session it already carries is left alone. What tells "the
/// session it already carries" apart from "a new login's" is the profile mark
/// ([`ProfileMark`]): the fingerprint of the session last handed to this
/// profile, and when that session was made. A session whose fingerprint is
/// not the mark's came from a login since, and its cookies are written in over
/// whatever the browser had. Holding *a* session of the account is not
/// enough: a fresh session pasted over a dead one has to reach the browser, or
/// every retry checks the dead one again.
///
/// **Unless it was made before the mark's.** A command started before a login
/// in another terminal still holds the older session, and a browser started
/// for it — after the owner closed the one before, idle or for the login —
/// keeps the login's instead. Which is later is the wall clock's answer when
/// each was made, as in [`super::newer_login`]. A login's own check never
/// depends on that answer: a pasted login forgets the mark's session first
/// ([`ProfileMark::forget_session`]), and a browser login's mark names its
/// own.
///
/// **A browser already holding exactly the session is left alone** too,
/// whatever its mark says, and the mark is brought up to date. The mark is
/// written after the session is stored when the browser's own copy is written
/// back at the end of a run, so a run that stopped between the two leaves a
/// mark one session behind; the cookie itself is what says they agree.
///
/// **Another account's cookies are emptied out first.** Cookies and site data
/// both: `mid`, `ig_did` and `datr` name the device, and carrying one
/// account's into another's session is how two accounts come to look like one
/// person's. With a profile per account this should not happen at all, so it
/// is said when it does.
pub(super) async fn sync_cookies(
    live: &mut Live,
    session: &Session,
    origin: &str,
    mark: &mut ProfileMark,
) -> Result<bool> {
    let host = url::Url::parse(origin)?
        .host_str()
        .unwrap_or_default()
        .to_string();
    let on_instagram = crate::cdp::is_instagram(&host);
    let named = |c: &Value, name: &str| c.get("name").and_then(Value::as_str) == Some(name);

    let jar = live.cdp.cookies().await?;
    let site: Vec<&Value> = jar
        .iter()
        .filter(|c| of_site(&host, c.get("domain").and_then(Value::as_str).unwrap_or("")))
        .collect();
    let held = site
        .iter()
        .filter(|c| named(c, "sessionid"))
        .filter_map(|c| c.get("value").and_then(Value::as_str))
        .collect::<Vec<_>>();

    let emptied = match needs(&held, mark, session) {
        Needs::Nothing => {
            mark.handed(session);
            return Ok(true);
        }
        Needs::KeepsALaterLogin => return Ok(false),
        Needs::Writing { emptied } => emptied,
    };

    let mut device_kept = std::collections::HashSet::new();
    if emptied {
        tracing::warn!(
            account = %session.ds_user_id,
            "the account's browser profile held another account's cookies; emptying it"
        );
        live.cdp
            .browser_call("Storage.clearCookies", json!({}))
            .await?;
        // Sent to the tab, not the browser: on the browser's own session this
        // answers "Internal error" whatever it is asked, measured on Chromium
        // 153, and on a tab's it clears.
        live.cdp
            .page_call(
                &live.tab,
                "Storage.clearDataForOrigin",
                json!({ "origin": origin, "storageTypes": "all" }),
                COMMAND_TIMEOUT,
            )
            .await?;
    } else {
        // The device cookies the browser already has are its own, and stay.
        for name in ["mid", "ig_did", "datr"] {
            if site.iter().any(|c| named(c, name)) {
                device_kept.insert(name);
            }
        }
    }

    let a_year = snob_core::clock::now().get() + 365 * 24 * 3600;
    let secure = origin.starts_with("https://");
    let cookie = |name: &str, value: &str, http_only: bool| {
        let mut c = json!({
            "name": name,
            "value": value,
            "path": "/",
            "secure": secure,
            "httpOnly": http_only,
            "expires": a_year,
        });
        if on_instagram {
            c["domain"] = json!(".instagram.com");
        } else {
            c["url"] = json!(format!("{origin}/"));
        }
        c
    };
    let mut set = vec![
        cookie("sessionid", session.sessionid.expose(), true),
        cookie("ds_user_id", &session.ds_user_id.to_string(), false),
    ];
    if let Some(token) = &session.csrftoken {
        set.push(cookie("csrftoken", token.expose(), false));
    }
    for (name, value) in [
        ("mid", session.mid.as_deref()),
        ("ig_did", session.ig_did.as_deref()),
        ("datr", session.datr.as_deref()),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty())
            && !device_kept.contains(name)
        {
            set.push(cookie(name, value, name != "mid"));
        }
    }
    live.cdp
        .browser_call("Storage.setCookies", json!({ "cookies": set }))
        .await?;

    mark.handed(session);
    Ok(true)
}

/// The cookies that name the device and what it has done on the site: the
/// three the login hands over, `wd` (the window's size, which the page's own
/// script sets), and `rur`, the region the server pins the session to.
const DEVICE_COOKIES: [&str; 5] = ["datr", "ig_did", "mid", "wd", "rur"];

/// Which of [`DEVICE_COOKIES`] the site's jar holds at `host`, and which it
/// does not: names only.
///
/// **A pasted session brings `sessionid` and not always the rest**, and a
/// profile that was handed none starts without them. They are not made up:
/// the first document the tab loads sets `datr`, `ig_did` and `mid`, its own
/// script sets `wd`, and the first logged-in answer sets `rur`, all in the
/// profile, which keeps them for the runs after; `write_back` stores the
/// three the session has a place for. What is left to do is to say whether
/// they came, which this does.
pub(super) fn device_cookies(jar: &[Value], host: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let held = |name: &str| {
        jar.iter().any(|c| {
            c.get("name").and_then(Value::as_str) == Some(name)
                && c.get("value")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && of_site(host, c.get("domain").and_then(Value::as_str).unwrap_or(""))
        })
    };
    DEVICE_COOKIES.into_iter().partition(|name| held(name))
}

/// What a browser holding the `sessionid` cookies `held`, beside `mark`, needs
/// before it sends as `session`. See [`sync_cookies`].
#[derive(Debug, PartialEq, Eq)]
enum Needs {
    /// It carries the session already.
    Nothing,
    /// It carries a login of the account made after the session.
    KeepsALaterLogin,
    /// The session is written in; over another account's cookies, which are
    /// emptied out first.
    Writing { emptied: bool },
}

/// Whether a cookie on `domain` is the site's at `host`: any of Instagram's
/// on Instagram, and elsewhere, such as a local fake, the host's own.
fn of_site(host: &str, domain: &str) -> bool {
    if crate::cdp::is_instagram(host) {
        crate::cdp::is_instagram(domain)
    } else {
        domain.trim_start_matches('.') == host
    }
}

fn needs(held: &[&str], mark: &ProfileMark, session: &Session) -> Needs {
    let this = |v: &&str| snob_core::session::account_in(v) == Some(session.ds_user_id);
    let holds_another =
        held.iter().any(|v| !this(v)) || mark.pk.is_some_and(|pk| pk != session.ds_user_id.get());
    if holds_another {
        return Needs::Writing { emptied: true };
    }
    if held.iter().any(this) {
        let given = session.fingerprint();
        if mark.session.as_deref() == Some(given.as_str())
            || held.contains(&session.sessionid.expose())
        {
            return Needs::Nothing;
        }
        if mark.session.is_some() && mark.made.is_some_and(|made| session.created_at < made) {
            return Needs::KeepsALaterLogin;
        }
    }
    Needs::Writing { emptied: false }
}

/// What snob writes down beside the browser's profile, in the profile's own
/// directory so that it goes wherever the profile goes.
///
/// Two facts the profile cannot tell about itself. **Which browser made it**:
/// Chrome, Chromium and Edge each seal their cookies with a key of their own,
/// so a profile opened by the wrong one looks logged out at best, and a newer
/// profile opened by an older browser can be damaged — and "the first browser
/// found" changes the day somebody installs another. **Which session it was
/// given**: see [`sync_cookies`]. Neither is a secret; the session is named by
/// [`Session::fingerprint`], which cannot be turned back into the cookie.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileMark {
    /// The executable that created the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<PathBuf>,
    /// The account the profile holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pk: Option<u64>,
    /// The fingerprint of the session last handed to the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// When that session was made ([`Session::created_at`]): one made earlier
    /// is an older login's, and does not replace it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made: Option<snob_core::Epoch>,
}

impl ProfileMark {
    const FILE: &'static str = "snob-profile.json";

    /// The mark beside the profile at `profile`, or an empty one: a profile
    /// without a mark is one nothing is known about, which is what empty says.
    pub fn read(profile: &Path) -> Self {
        std::fs::read(profile.join(Self::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Written whole and moved into place, so that a reader never finds half
    /// of it. A mark lost all the same reads as empty: the next browser on the
    /// profile writes the session it is handed over the one it holds, and the
    /// browser that made the profile is found by detection again.
    pub fn write(&self, profile: &Path) {
        let path = profile.join(Self::FILE);
        let temporary = profile.join(format!(".{}.{}.tmp", Self::FILE, std::process::id()));
        let written = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&temporary, bytes))
            .and_then(|()| std::fs::rename(&temporary, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temporary);
            tracing::debug!(error = %e, path = %path.display(), "could not write the profile mark");
        }
    }

    /// The profile carries `session` now.
    pub(super) fn handed(&mut self, session: &Session) {
        self.pk = Some(session.ds_user_id.get());
        self.session = Some(session.fingerprint());
        self.made = Some(session.created_at);
    }

    /// The mark a browser login leaves: the profile was made by `browser`, and
    /// the session it produced is the one being stored.
    pub fn after_login(profile: &Path, browser: &crate::browser::Browser, session: &Session) {
        let mut mark = Self {
            browser: Some(browser.path.clone()),
            ..Self::default()
        };
        mark.handed(session);
        mark.write(profile);
    }

    /// Forgets which session the profile at `profile` was handed, and keeps
    /// the rest: the next browser on it takes the session it is handed. For a
    /// pasted login about to be checked, which has to reach the browser
    /// whatever the clock said when the session before it was made, and for a
    /// login that was not stored, whose session must not keep the stored one
    /// out.
    pub fn forget_session(profile: &Path) {
        let mut mark = Self::read(profile);
        if mark.session.is_none() && mark.made.is_none() {
            return;
        }
        mark.session = None;
        mark.made = None;
        mark.write(profile);
    }
}

/// The profile an account's requests are sent from, created if it is not
/// there yet, and whether it was already there.
///
/// **One per account** ([`AppPaths::browser_profile_for`]): one account is
/// only ever sent from one browser, and two accounts never share a profile.
/// A profile from before that rule is moved under the account it holds first
/// ([`settle_the_layout`]).
pub fn for_account(paths: &AppPaths, pk: Pk) -> Result<(PathBuf, bool)> {
    settle_the_layout(paths, Some(pk))?;
    let profile = paths.browser_profile_for(pk);
    let existed = profile.is_dir();
    create_private(&paths.browser_profile())?;
    create_private(&profile)?;
    Ok((profile, existed))
}

/// A fresh profile for a login whose account is not known yet.
///
/// Named after this process, under the directory only this user can enter, and
/// made anew whatever was left at the name: a login that did not finish is the
/// only thing that leaves one. [`ProfileSwap`] puts it under its account once
/// the login says whose it is.
pub fn for_a_login(paths: &AppPaths) -> Result<PathBuf> {
    settle_the_layout(paths, None)?;
    create_private(&paths.browser_profile())?;
    let profile = paths
        .browser_profile()
        .join(format!("{LOGIN_PREFIX}{}", std::process::id()));
    snob_store::paths::create_fresh_private_dir(&profile)
        .with_context(|| format!("could not create {}", profile.display()))?;
    Ok(profile)
}

/// What a login's own profile is named after, before its account is known.
const LOGIN_PREFIX: &str = "login-";

/// Between the account's id and this process's in the name of an account's
/// older profile, set aside while a login's takes its place.
const REPLACED: &str = ".replaced-";

/// Where a profile from before per-account profiles goes when nobody can say
/// whose it is.
const UNCLAIMED: &str = "unclaimed";

/// Where a profile from before per-account profiles waits while it is moved.
/// Beside the directory, since it cannot be moved into itself in one step.
fn moving(paths: &AppPaths) -> PathBuf {
    paths.data_dir().join("browser-profile.moving")
}

/// Every place a profile holding a live session can be, for `snob logout`:
/// the directory all of them live under, and a move that did not finish.
pub fn every_profile(paths: &AppPaths) -> Vec<PathBuf> {
    vec![paths.browser_profile(), moving(paths)]
}

/// Moves a profile from before profiles were kept per account under the
/// account it holds.
///
/// That profile is the directory itself: Chromium's `Local State` and
/// `Default` at the top, and the mark beside them. It is renamed aside, the
/// directory is made again, and it goes back in under the account its mark
/// names — or, with no mark, the account asking for it, which is who it has
/// been sending as — and `unclaimed` when neither is known, where `snob
/// logout` still finds it. A move that stopped halfway is finished by the next
/// run, which finds it aside. What its mark said about the machine goes to
/// [`super::identity::KnownHints`].
///
/// **Never while a browser has it open.** Renamed under a running browser, the
/// profile would be written to where it no longer is: an older snob mid-run is
/// told to finish first.
///
/// **Never over profiles kept per account.** A directory that is one profile
/// and holds accounts' profiles too is an older snob still running after the
/// move, which made the one profile again; moving the whole directory would
/// bury every account's profile inside one of them. That is refused, and the
/// person told what to do.
///
/// **Never over one already there.** Two runs making this move at once meet
/// here: the one that finds the account's profile in place and nothing aside
/// finds the other's move finished.
fn settle_the_layout(paths: &AppPaths, asking: Option<Pk>) -> Result<()> {
    paths.ensure_dirs()?;
    let root = paths.browser_profile();
    let aside = moving(paths);
    if !aside.exists() {
        if !is_one_profile(&root) {
            return Ok(());
        }
        if holds_account_profiles(&root) {
            bail!(
                "an older version of snob is using {} as a single browser profile, beside \
                 the profiles this version keeps there for each account.\n\
                 Stop or upgrade that version — a scheduled \"snob watch\" is the usual one — \
                 then remove everything at the top of that directory except the account \
                 directories, whose names are numbers.",
                root.display()
            );
        }
        if in_use(&root) {
            bail!(
                "another snob is using the browser profile at {}, which this version keeps \
                 per account.\n\
                 Wait for that run to finish and try again.",
                root.display()
            );
        }
        rename_patiently(&root, &aside)?;
    }

    let mark = std::fs::read(aside.join(ProfileMark::FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    if let Some(hints) = mark
        .get("hints")
        .cloned()
        .and_then(|h| serde_json::from_value::<super::identity::MachineHints>(h).ok())
    {
        let mut known = super::identity::KnownHints::read(paths);
        known.remember(hints);
        known.write(paths);
    }
    let owner = mark
        .get("pk")
        .and_then(Value::as_u64)
        .map(Pk::new)
        .or(asking);
    create_private(&root)?;
    let target = match owner {
        Some(pk) => paths.browser_profile_for(pk),
        None => root.join(UNCLAIMED),
    };
    if target.exists() {
        if !aside.exists() {
            return Ok(());
        }
        bail!(
            "a browser profile from an earlier version, at {}, was to be moved to {}, \
             where there is one already.\n\
             Move one of the two away by hand.",
            aside.display(),
            target.display()
        );
    }
    rename_patiently(&aside, &target)?;
    // Written back without what it said about the machine, which has a file
    // of its own.
    let mut settled = ProfileMark::read(&target);
    if settled.pk.is_none() {
        settled.pk = owner.map(Pk::get);
    }
    settled.write(&target);
    tracing::debug!(to = %target.display(), "moved the browser profile under its account");
    Ok(())
}

/// Whether `dir` is itself a browser profile, as the one profile from before
/// they were kept per account was.
fn is_one_profile(dir: &Path) -> bool {
    dir.join(ProfileMark::FILE).is_file()
        || dir.join("Local State").is_file()
        || dir.join("Default").is_dir()
}

/// Whether `dir` holds anything this version keeps under it: an account's
/// profile, a login's, one set aside, or the one nobody claimed.
fn holds_account_profiles(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        entry.path().is_dir()
            && (name.parse::<u64>().is_ok()
                || name.starts_with(LOGIN_PREFIX)
                || name.contains(REPLACED)
                || name == UNCLAIMED)
    })
}

/// Whether a browser on this machine has the profile at `dir` open.
///
/// Chromium locks a profile with a link naming the host and the process that
/// holds it. A lock left by a process that is gone, or by another host, is no
/// browser of this machine's. On Windows there is no such link; a rename under
/// an open browser fails there instead, which [`rename_patiently`] reports.
fn in_use(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(target) = std::fs::read_link(dir.join("SingletonLock")) else {
            return false;
        };
        let target = target.to_string_lossy();
        let Some((host, pid)) = target.rsplit_once('-') else {
            return false;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            return false;
        };
        if host != this_host() || pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 sends nothing; it asks whether the process exists.
        let asked = unsafe { libc::kill(pid, 0) };
        asked == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        false
    }
}

#[cfg(unix)]
fn this_host() -> String {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is valid for its whole length, and the call writes at
    // most that many bytes into it.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return String::new();
    }
    std::ffi::CStr::from_bytes_until_nul(&name)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Renames a profile, a few times over a few seconds before giving up
/// ([`snob_store::paths::rename_patiently`]).
fn rename_patiently(from: &Path, to: &Path) -> Result<()> {
    snob_store::paths::rename_patiently(from, to).with_context(|| {
        format!(
            "could not move the browser profile at {} to {}; another snob may be using it",
            from.display(),
            to.display()
        )
    })
}

fn create_private(dir: &Path) -> Result<()> {
    snob_store::paths::create_private_dir(dir)
        .with_context(|| format!("could not create {}", dir.display()))
}

/// A login's profile, put under the account the login turned out to be.
///
/// The login happened in that profile, so it is the device Instagram just saw
/// sign in, and it takes the account's place: the account's older profile is
/// set aside rather than deleted until the session is known to be good, and
/// comes back if it is not. Nothing is moved when the login was made in the
/// account's own profile already.
pub struct ProfileSwap {
    /// Where the login's profile now is: the account's.
    pub profile: PathBuf,
    /// The account's older profile, set aside.
    aside: Option<PathBuf>,
    /// Whether anything was moved at all.
    moved: bool,
}

impl ProfileSwap {
    pub fn replace(paths: &AppPaths, used: &Path, pk: Pk) -> Result<Self> {
        let target = paths.browser_profile_for(pk);
        if used == target {
            return Ok(Self {
                profile: target,
                aside: None,
                moved: false,
            });
        }
        let aside = if target.exists() {
            let aside = paths
                .browser_profile()
                .join(format!("{pk}{REPLACED}{}", std::process::id()));
            snob_store::paths::remove_tree_patiently(&aside)
                .with_context(|| format!("could not remove {}", aside.display()))?;
            rename_patiently(&target, &aside)?;
            Some(aside)
        } else {
            None
        };
        if let Err(e) = rename_patiently(used, &target) {
            if let Some(aside) = &aside {
                let _ = snob_store::paths::rename_patiently(aside, &target);
            }
            return Err(e);
        }
        Ok(Self {
            profile: target,
            aside,
            moved: true,
        })
    }

    /// The session is stored: the older profile goes.
    pub fn keep(self) {
        if let Some(aside) = &self.aside
            && let Err(e) = snob_store::paths::remove_tree_patiently(aside)
        {
            tracing::warn!(error = %e, path = %aside.display(), "could not remove the replaced profile");
        }
    }

    /// The session was not stored — refused, or the check or the store
    /// failed: the login's profile goes, holding a session nothing stored
    /// names, and the older one comes back. Only once no browser has the
    /// account's profile open (`owner::release`).
    pub fn undo(self) {
        if !self.moved {
            return;
        }
        if let Err(e) = snob_store::paths::remove_tree_patiently(&self.profile) {
            tracing::warn!(
                error = %e,
                path = %self.profile.display(),
                older = ?self.aside,
                "could not remove the login's profile, so the older one stays where it was set aside"
            );
            return;
        }
        if let Some(aside) = &self.aside
            && let Err(e) = snob_store::paths::rename_patiently(aside, &self.profile)
        {
            tracing::warn!(error = %e, path = %aside.display(), "could not put the older profile back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(root: &Path) -> AppPaths {
        AppPaths::rooted_at(root)
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// On Instagram every one of its cookies is the site's; on a local fake,
    /// only the fake's own host, with or without the leading dot.
    /// The device cookies a profile holds are told by name, and only the
    /// site's own count; an empty value is no cookie.
    #[test]
    fn the_device_cookies_a_profile_holds_are_named() {
        let jar = vec![
            json!({ "name": "datr", "value": "d", "domain": ".instagram.com" }),
            json!({ "name": "mid", "value": "", "domain": ".instagram.com" }),
            json!({ "name": "wd", "value": "1920x1032", "domain": "www.instagram.com" }),
            json!({ "name": "rur", "value": "r", "domain": ".facebook.com" }),
            json!({ "name": "ig_did", "value": "i", "domain": ".instagram.com" }),
        ];
        let (held, missing) = device_cookies(&jar, "www.instagram.com");
        assert_eq!(held, ["datr", "ig_did", "wd"]);
        assert_eq!(missing, ["mid", "rur"]);
        let (held, missing) = device_cookies(&[], "www.instagram.com");
        assert!(held.is_empty() && missing.len() == 5);
    }

    #[test]
    fn the_site_cookies_are_the_ones_on_its_host() {
        for domain in [".127.0.0.1", "127.0.0.1"] {
            assert!(of_site("127.0.0.1", domain), "{domain}");
        }
        for domain in ["x.127.0.0.1", "127.0.0.2", ".instagram.com"] {
            assert!(!of_site("127.0.0.1", domain), "{domain}");
        }
        for domain in [".instagram.com", "instagram.com", "i.instagram.com"] {
            assert!(of_site("www.instagram.com", domain), "{domain}");
        }
        for domain in ["notinstagram.com", "instagram.com.example.net"] {
            assert!(!of_site("www.instagram.com", domain), "{domain}");
        }
    }

    /// The one profile from before goes under the account its mark names —
    /// not the one asking — and what it said about the machine leaves it for
    /// the file every profile shares.
    #[test]
    fn a_single_profile_moves_under_the_account_it_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let root = paths.browser_profile();
        touch(&root.join("Local State"));
        std::fs::write(
            root.join(ProfileMark::FILE),
            serde_json::to_vec(&json!({
                "pk": 42,
                "session": "0123456789abcdef",
                "hints": { "browser": "/usr/bin/chromium", "version": "141.0.1.2", "values": {} },
            }))
            .unwrap(),
        )
        .unwrap();

        let (asking, existed) = for_account(&paths, Pk::new(99)).unwrap();
        assert_eq!(asking, paths.browser_profile_for(Pk::new(99)));
        assert!(!existed, "the account asking had no profile of its own");

        let moved = paths.browser_profile_for(Pk::new(42));
        assert!(moved.join("Local State").is_file());
        assert!(!root.join("Local State").exists());
        let mark = ProfileMark::read(&moved);
        assert_eq!(mark.pk, Some(42));
        assert_eq!(mark.session.as_deref(), Some("0123456789abcdef"));
        let written = std::fs::read_to_string(moved.join(ProfileMark::FILE)).unwrap();
        assert!(!written.contains("hints"), "{written}");
        let known = super::super::identity::KnownHints::read(&paths);
        assert_eq!(known.hints.len(), 1);
        assert!(!moving(&paths).exists());
    }

    /// With no mark to say whose it is, it goes to the account asking for
    /// it, which is who it has been sending as.
    #[test]
    fn with_no_mark_the_profile_goes_to_the_account_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        touch(&paths.browser_profile().join("Default").join("Cookies"));

        let (profile, existed) = for_account(&paths, Pk::new(7)).unwrap();
        assert!(existed);
        assert!(profile.join("Default").join("Cookies").is_file());
        assert_eq!(ProfileMark::read(&profile).pk, Some(7));
    }

    /// A login whose account is not known yet leaves it unclaimed, where
    /// `snob logout` still finds it.
    #[test]
    fn with_nobody_to_claim_it_the_profile_is_kept_unclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        touch(&paths.browser_profile().join("Local State"));

        let fresh = for_a_login(&paths).unwrap();
        assert!(fresh.is_dir());
        assert!(
            paths
                .browser_profile()
                .join("unclaimed")
                .join("Local State")
                .is_file()
        );
        assert!(
            fresh.starts_with(paths.browser_profile()),
            "a login's profile is where `snob logout` looks"
        );
    }

    /// A move stopped between its two renames is finished by the next run.
    #[test]
    fn a_move_that_stopped_halfway_is_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        paths.ensure_dirs().unwrap();
        let aside = moving(&paths);
        touch(&aside.join("Local State"));
        ProfileMark {
            pk: Some(5),
            ..ProfileMark::default()
        }
        .write(&aside);

        let (profile, existed) = for_account(&paths, Pk::new(5)).unwrap();
        assert!(existed);
        assert!(profile.join("Local State").is_file());
        assert!(!aside.exists());
    }

    /// Profiles already kept per account are left exactly as they are.
    #[test]
    fn a_profile_per_account_is_not_moved_again() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let mine = paths.browser_profile_for(Pk::new(42));
        touch(&mine.join("Local State"));
        let other = paths.browser_profile_for(Pk::new(43));
        touch(&other.join("Local State"));

        let (profile, existed) = for_account(&paths, Pk::new(42)).unwrap();
        assert_eq!(profile, mine);
        assert!(existed);
        assert!(other.join("Local State").is_file());
    }

    /// Renamed under a browser that has it open, the profile would be written
    /// to where it no longer is: a live lock on this machine is left alone.
    #[cfg(unix)]
    #[test]
    fn a_profile_a_browser_has_open_is_left_where_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let root = paths.browser_profile();
        touch(&root.join("Local State"));
        std::os::unix::fs::symlink(
            format!("{}-{}", this_host(), std::process::id()),
            root.join("SingletonLock"),
        )
        .unwrap();

        let refused = for_account(&paths, Pk::new(42)).unwrap_err();
        assert!(
            format!("{refused:#}").contains("another snob"),
            "{refused:#}"
        );
        assert!(root.join("Local State").is_file(), "nothing was moved");
    }

    /// The login's profile takes the account's place, and the older one
    /// comes back if the session turns out not to be good.
    #[test]
    fn a_login_takes_the_accounts_place_and_gives_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let account = paths.browser_profile_for(Pk::new(42));
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("which"), b"older").unwrap();
        let login = for_a_login(&paths).unwrap();
        std::fs::write(login.join("which"), b"login").unwrap();

        let swap = ProfileSwap::replace(&paths, &login, Pk::new(42)).unwrap();
        assert_eq!(swap.profile, account);
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"login");
        assert!(!login.exists());
        swap.undo();
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"older");

        let login = for_a_login(&paths).unwrap();
        std::fs::write(login.join("which"), b"login").unwrap();
        ProfileSwap::replace(&paths, &login, Pk::new(42))
            .unwrap()
            .keep();
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"login");
        let left: Vec<_> = std::fs::read_dir(paths.browser_profile())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            [std::ffi::OsString::from("42")],
            "nothing set aside is left"
        );
    }

    /// A login made in the account's own profile moves nothing, and undoing
    /// it removes nothing.
    #[test]
    fn a_login_in_the_accounts_own_profile_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let (account, _) = for_account(&paths, Pk::new(42)).unwrap();
        std::fs::write(account.join("which"), b"own").unwrap();

        let swap = ProfileSwap::replace(&paths, &account, Pk::new(42)).unwrap();
        swap.undo();
        assert_eq!(std::fs::read(account.join("which")).unwrap(), b"own");
    }

    /// Somebody signing in as another account on the stored one's profile
    /// takes that profile with them, and the stored account is left none.
    #[test]
    fn a_login_as_another_account_takes_the_profile_it_happened_in() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let (stored, _) = for_account(&paths, Pk::new(42)).unwrap();
        std::fs::write(stored.join("which"), b"42's device").unwrap();

        ProfileSwap::replace(&paths, &stored, Pk::new(43))
            .unwrap()
            .keep();
        let taken = paths.browser_profile_for(Pk::new(43));
        assert_eq!(std::fs::read(taken.join("which")).unwrap(), b"42's device");
        assert!(!stored.exists());
    }

    /// A directory that is one profile and holds accounts' profiles too is an
    /// older snob at work after the move: nothing is moved into anything.
    #[test]
    fn a_single_profile_beside_account_profiles_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let mine = paths.browser_profile_for(Pk::new(42));
        touch(&mine.join("Local State"));
        touch(&paths.browser_profile().join("Local State"));

        let refused = for_account(&paths, Pk::new(42)).unwrap_err();
        assert!(
            format!("{refused:#}").contains("older version of snob"),
            "{refused:#}"
        );
        assert!(mine.join("Local State").is_file(), "the account's stays");
        assert!(!moving(&paths).exists(), "nothing was set aside");
    }

    /// A profile already where the move would put it is never deleted: with
    /// the copy aside gone, another run finished the move; with it still
    /// there, the two are left for the person.
    #[test]
    fn a_move_never_deletes_the_profile_where_it_would_go() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = at(tmp.path());
        let mine = paths.browser_profile_for(Pk::new(5));
        touch(&mine.join("which"));
        let aside = moving(&paths);
        touch(&aside.join("Local State"));
        ProfileMark {
            pk: Some(5),
            ..ProfileMark::default()
        }
        .write(&aside);

        let refused = for_account(&paths, Pk::new(5)).unwrap_err();
        assert!(format!("{refused:#}").contains("by hand"), "{refused:#}");
        assert!(mine.join("which").is_file());
        assert!(aside.join("Local State").is_file());

        std::fs::remove_dir_all(&aside).unwrap();
        assert!(settle_the_layout(&paths, Some(Pk::new(5))).is_ok());
        assert!(mine.join("which").is_file());
    }

    fn handed(sessionid: &str, made: i64) -> Session {
        let mut session = Session::from_sessionid(
            sessionid,
            "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        session.created_at = snob_core::Epoch::new(made);
        session
    }

    /// A fresh browser is handed a newer session and writes it; handed an
    /// older one after that, it keeps the newer login.
    #[test]
    fn an_older_session_does_not_replace_a_later_login() {
        let older = handed("42%3Aold%3A1", 1_000);
        let newer = handed("42%3Anew%3A1", 2_000);
        let mut mark = ProfileMark {
            pk: Some(42),
            ..ProfileMark::default()
        };
        mark.handed(&older);

        assert_eq!(
            needs(&["42%3Aold%3A1"], &mark, &newer),
            Needs::Writing { emptied: false }
        );
        mark.handed(&newer);
        assert_eq!(
            needs(&["42%3Anew%3A1"], &mark, &older),
            Needs::KeepsALaterLogin
        );
        assert_eq!(needs(&["42%3Anew%3A1"], &mark, &newer), Needs::Nothing);

        // A login forgets the mark's session, and whatever it was made, it
        // goes in.
        mark.session = None;
        mark.made = None;
        assert_eq!(
            needs(&["42%3Anew%3A1"], &mark, &older),
            Needs::Writing { emptied: false }
        );
    }

    /// A session pasted with its colon written out, or in lower case, is the
    /// account's own: nothing is emptied for it, and a browser holding it is
    /// left alone.
    #[test]
    fn a_session_is_the_accounts_however_its_colon_is_written() {
        for pasted in ["42:abc:1", "42%3aabc%3a1"] {
            let session = handed(pasted, 1_000);
            let empty = ProfileMark::default();
            assert_eq!(needs(&[pasted], &empty, &session), Needs::Nothing);
            assert_eq!(
                needs(&["42%3Arotated%3A2"], &empty, &session),
                Needs::Writing { emptied: false },
                "{pasted}"
            );
            assert_eq!(
                needs(&["43:abc:1"], &empty, &session),
                Needs::Writing { emptied: true },
                "{pasted}"
            );
        }
    }

    /// Forgetting keeps which browser made the profile and whose it is.
    #[test]
    fn forgetting_the_session_keeps_the_rest_of_the_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mark = ProfileMark {
            browser: Some(PathBuf::from("/usr/bin/chromium")),
            ..ProfileMark::default()
        };
        mark.handed(&handed("42%3Aabc%3A1", 1_000));
        mark.write(tmp.path());
        assert_eq!(ProfileMark::read(tmp.path()), mark);

        ProfileMark::forget_session(tmp.path());
        let forgotten = ProfileMark::read(tmp.path());
        assert_eq!(forgotten.browser, mark.browser);
        assert_eq!(forgotten.pk, Some(42));
        assert_eq!((forgotten.session, forgotten.made), (None, None));
        let left: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary is left beside the mark");
    }
}
