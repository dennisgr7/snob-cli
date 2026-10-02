//! The session the browser kept current, written back where it is stored.
//!
//! **Why.** Once the browser has the session it is where the session lives:
//! Instagram rotates `csrftoken` — and now and then `sessionid` itself —
//! through the answers it sends, and the browser keeps whatever it is sent.
//! Without this the stored copy learns none of it, and a profile that is lost
//! — deleted, damaged, or a machine restored from an older backup — comes back
//! as the session it was a login ago, which Instagram may no longer take, with
//! every rotated token forgotten the moment the profile went.
//!
//! **When.** When a command leaves the owner's browsers (`owner`), and when a
//! process closes browsers it ran itself — at the end of a run and between
//! the monitor's runs: the cookies are read once, and written only when they
//! differ from what is stored. A browser the owner closes later, idle, writes
//! nothing back; what it rotated since stays in its profile, where the next
//! command's browser reads it. So does one killed rather than closed — a
//! second Ctrl+C, a panic.
//!
//! **What wins.** A login is authoritative: the stored session is written over
//! only while it is still the one the browser holds, as it was handed to it
//! ([`Rotated`] carries which). A `snob login` in another terminal meanwhile
//! stored a different one, and that one stays. The account has to match too;
//! a browser never carries another account's session to the store. What is
//! not guarded is a login stored between this reading the store and writing
//! it, a moment with no lock around it.

use snob_core::Pk;
use snob_core::session::Session;
use snob_ig::login::BrowserCookies;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use super::profile::ProfileMark;

/// What one account's browser held when it was left or closed.
#[derive(Debug)]
pub struct Rotated {
    pk: Pk,
    /// The fingerprint of the session the browser holds, as it was handed to
    /// it: what it was last written, not what a command asked it to send.
    handed: String,
    jar: BrowserCookies,
}

impl Rotated {
    pub(crate) fn new(pk: Pk, handed: String, jar: BrowserCookies) -> Self {
        Self { pk, handed, jar }
    }
}

/// Writes back whatever the browsers rotated, each to its own account's
/// session, where that session was found. A failure is said and nothing else:
/// the run has done its work, and the browser still holds the cookies for the
/// next one.
pub fn keep(secrets: &SecretStore, paths: &AppPaths, rotated: Vec<Rotated>) {
    for rotated in rotated {
        let store = secrets.session_of(&paths.account(rotated.pk));
        let (stored, backend) = match store.load_located() {
            Ok(Some(found)) => found,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(error = %e, "the stored session could not be read to update it");
                continue;
            }
        };
        if stored.ds_user_id != rotated.pk {
            continue;
        }
        let Some(fresh) = merged(&stored, &rotated.handed, &rotated.jar) else {
            continue;
        };
        if let Err(e) = store.save_in(backend, &fresh) {
            tracing::warn!(error = %e, "the cookies the browser rotated could not be stored");
            continue;
        }
        tracing::debug!("stored the cookies the browser rotated");
        // A new `sessionid` is a new fingerprint: the mark follows, so the next
        // run does not take the stored copy for a login since and write it
        // over the browser's. Were this lost, the browser holding exactly the
        // stored session would still say so (`sync_cookies`).
        if fresh.sessionid.expose() != stored.sessionid.expose() {
            let profile = paths.browser_profile_for(rotated.pk);
            let mut mark = ProfileMark::read(&profile);
            mark.session = Some(fresh.fingerprint());
            mark.made = Some(fresh.created_at);
            mark.write(&profile);
        }
    }
}

/// The stored session with what the browser holds in place of what it had,
/// or `None` when there is nothing to write — or nothing that may be.
pub fn merged(stored: &Session, handed: &str, jar: &BrowserCookies) -> Option<Session> {
    let held = jar.sessionid.expose();
    // A jar with no session erases nothing: a browser that was signed out
    // says so at the next request, and that is for the login to answer.
    if held.is_empty() {
        return None;
    }
    // Another account's session is never this store's, whatever the browser
    // was doing with it.
    if snob_core::session::account_in(held) != Some(stored.ds_user_id) {
        return None;
    }
    // A login since this run began stored a session of its own, and a login
    // is authoritative. What the browser holds is either that login's session
    // or an older one; either way there is nothing to write.
    if stored.fingerprint() != handed && stored.sessionid.expose() != held {
        return None;
    }

    let mut fresh = stored.clone();
    fresh.sessionid = jar.sessionid.clone();
    if let Some(token) = jar.csrftoken.as_ref().filter(|t| !t.is_empty()) {
        fresh.csrftoken = Some(token.clone());
    }
    for (kept, found) in [
        (&mut fresh.mid, &jar.mid),
        (&mut fresh.ig_did, &jar.ig_did),
        (&mut fresh.datr, &jar.datr),
    ] {
        if let Some(value) = found.as_ref().filter(|v| !v.is_empty()) {
            *kept = Some(value.clone());
        }
    }
    let secret = |s: &Option<snob_core::secret::Secret>| s.as_ref().map(|t| t.expose().to_string());
    let same = fresh.sessionid.expose() == stored.sessionid.expose()
        && secret(&fresh.csrftoken) == secret(&stored.csrftoken)
        && fresh.mid == stored.mid
        && fresh.ig_did == stored.ig_did
        && fresh.datr == stored.datr;
    (!same).then_some(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::session::SessionOrigin;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

    fn stored(sessionid: &str) -> Session {
        let mut s = Session::from_sessionid(sessionid, UA, SessionOrigin::Paste).unwrap();
        s.csrftoken = Some("old-token".into());
        s.mid = Some("device-mid".to_string());
        s
    }

    fn jar(sessionid: &str, token: &str) -> BrowserCookies {
        BrowserCookies {
            sessionid: sessionid.into(),
            csrftoken: Some(token.into()),
            mid: Some("device-mid".to_string()),
            ..BrowserCookies::default()
        }
    }

    #[test]
    fn a_rotated_token_is_taken() {
        let s = stored("42%3Aabc%3A1");
        let fresh = merged(&s, &s.fingerprint(), &jar("42%3Aabc%3A1", "new-token")).unwrap();
        assert_eq!(fresh.csrftoken.unwrap().expose(), "new-token");
        assert_eq!(fresh.sessionid.expose(), "42%3Aabc%3A1");
    }

    #[test]
    fn a_rotated_session_is_taken_with_its_account() {
        let s = stored("42%3Aabc%3A1");
        let fresh = merged(&s, &s.fingerprint(), &jar("42%3Axyz%3A2", "old-token")).unwrap();
        assert_eq!(fresh.sessionid.expose(), "42%3Axyz%3A2");
        assert_eq!(fresh.ds_user_id, s.ds_user_id);
        assert_ne!(fresh.fingerprint(), s.fingerprint());
    }

    #[test]
    fn an_unchanged_jar_writes_nothing() {
        let s = stored("42%3Aabc%3A1");
        assert!(merged(&s, &s.fingerprint(), &jar("42%3Aabc%3A1", "old-token")).is_none());
    }

    /// A `snob login` stored another session while this run was going: the
    /// browser's copy of the older one is not written over it.
    #[test]
    fn a_login_since_is_not_written_over() {
        let before = stored("42%3Aabc%3A1");
        let since = stored("42%3Anew%3A9");
        assert!(merged(&since, &before.fingerprint(), &jar("42%3Aabc%3A1", "t")).is_none());
        assert!(merged(&since, &before.fingerprint(), &jar("42%3Aabc%3A7", "t")).is_none());
        // Unless the browser already holds exactly that login's session.
        let fresh = merged(&since, &before.fingerprint(), &jar("42%3Anew%3A9", "t")).unwrap();
        assert_eq!(fresh.csrftoken.unwrap().expose(), "t");
    }

    /// A session pasted with its colon written out, or in lower case, is the
    /// same account's when the browser rotates it into Instagram's spelling.
    #[test]
    fn a_session_is_the_accounts_however_its_colon_is_written() {
        for pasted in ["42:abc:1", "42%3aabc%3a1"] {
            let s = stored(pasted);
            let fresh = merged(&s, &s.fingerprint(), &jar("42%3Axyz%3A2", "t")).unwrap();
            assert_eq!(fresh.sessionid.expose(), "42%3Axyz%3A2", "{pasted}");
            let fresh = merged(&s, &s.fingerprint(), &jar(pasted, "t")).unwrap();
            assert_eq!(fresh.csrftoken.unwrap().expose(), "t", "{pasted}");
        }
    }

    #[test]
    fn another_accounts_jar_is_never_stored() {
        let s = stored("42%3Aabc%3A1");
        assert!(merged(&s, &s.fingerprint(), &jar("43%3Aabc%3A1", "t")).is_none());
        assert!(merged(&s, &s.fingerprint(), &jar("421%3Aabc%3A1", "t")).is_none());
    }

    /// Two accounts' browsers rotated in one run: each account's session
    /// learns what its own browser holds, and nothing of the other's.
    #[test]
    fn each_account_keeps_what_its_own_browser_rotated() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let secrets = SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-write-back-{}", std::process::id()));
        let mut rotated = Vec::new();
        for (sessionid, token) in [("42%3Aabc%3A1", "t42"), ("43%3Adef%3A1", "t43")] {
            let session = stored(sessionid);
            let store = secrets.session_of(&paths.account(session.ds_user_id));
            store.save(&session).unwrap();
            rotated.push(Rotated::new(
                session.ds_user_id,
                session.fingerprint(),
                jar(sessionid, token),
            ));
        }

        keep(&secrets, &paths, rotated);

        for (pk, token) in [(42, "t42"), (43, "t43")] {
            let store = secrets.session_of(&paths.account(Pk::new(pk)));
            let kept = store.load().unwrap().unwrap();
            assert_eq!(kept.ds_user_id, Pk::new(pk));
            assert_eq!(kept.csrftoken.unwrap().expose(), token);
            store.delete().unwrap();
        }
    }

    /// Only what the browser holds replaces what is stored; nothing it lacks
    /// is erased.
    #[test]
    fn an_empty_jar_erases_nothing() {
        let s = stored("42%3Aabc%3A1");
        assert!(merged(&s, &s.fingerprint(), &BrowserCookies::default()).is_none());
        let without_token = BrowserCookies {
            sessionid: "42%3Aabc%3A1".into(),
            datr: Some("fresh-datr".to_string()),
            ..BrowserCookies::default()
        };
        let fresh = merged(&s, &s.fingerprint(), &without_token).unwrap();
        assert_eq!(fresh.csrftoken.unwrap().expose(), "old-token");
        assert_eq!(fresh.mid.as_deref(), Some("device-mid"));
        assert_eq!(fresh.datr.as_deref(), Some("fresh-datr"));
    }
}
