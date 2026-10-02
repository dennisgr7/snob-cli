//! `snob login`.
//!
//! Two ways in, and the order of what each one asks is deliberate. Everything
//! that can fail on its own — the store being writable, a browser being
//! installed, which browser — is settled **before** the user is asked for
//! anything, so nobody completes a two-factor login only to be told afterwards
//! that there was nowhere to put the result.

use anyhow::{Context, Result, bail};
use snob_core::session::Session;
use snob_ig::login::{self, ValidationOutcome};
use snob_ig::pace::CancelToken;
use snob_store::paths::{self, AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::{Backend, SecretStore};

use crate::app::Viewer;
use crate::cli::LoginArgs;
use crate::exit::{ExitCode, ExitError};
use crate::progress::Progress;
use crate::report;
use crate::ui::{self, LoginMethod};
use crate::{browser, cdp, interrupt};

/// Logs in again as `renewing`, the account resolved for this run, or as
/// another account. `named` says the account was named rather than taken
/// from the registry, and is then not asked about. `renewing` picks the
/// browser profile a browser login opens; a paste is whichever account its
/// sessionid is.
pub async fn run(
    args: LoginArgs,
    store: SecretStore,
    paths: &AppPaths,
    renewing: Option<Viewer>,
    named: bool,
) -> Result<ExitCode> {
    // Before anything is asked for: if the session cannot be stored anywhere,
    // better to find out now than after two-factor authentication.
    let wanted = store.backend();
    let usable = match store.probe_writable() {
        Ok(backend) => backend,
        Err(e) => bail!(
            "{e}\n\
             There is nowhere to put the session: neither the system keyring nor \
             a file in the data directory could be written."
        ),
    };

    // Falling back is fine. Falling back quietly is not: a session stored
    // somewhere less protected than the user expected should say so.
    if wanted == Backend::Keyring && usable == Backend::File {
        ui::warn(
            "no system keyring is available here, so the session goes to a \
             protected file instead.\n\
             On a headless machine that is normal — Secret Service needs a \
             desktop session — and the file is readable only by you.",
        );
    }
    let store = store.using(usable);

    // Before anything is asked for, and this is the half the probe above does
    // not cover: `probe_writable` returns as soon as the keyring answers, so it
    // never touches the data directory at all on a machine that has one. The
    // account's database is opened for the first time inside `finish`, *after*
    // the browser login or the paste, once the login says whose it is — so a
    // full or read-only data directory would throw away a login the user had
    // already completed. With `--paste` that means fetching and pasting the
    // sessionid again; with `--browser`, relaunching.
    writable(paths)?;

    let Some(method) = choose_method(&args)? else {
        ui::info("Login canceled.");
        return Ok(ExitCode::Interrupted);
    };

    match method {
        // A paste is whichever account the sessionid is: there is nothing to
        // renew or add, and `finish` says what it replaces once it knows.
        LoginMethod::Paste => by_paste(args, store, paths).await,
        LoginMethod::Browser => {
            // A question not answered is 130, as it is everywhere else —
            // `watch setup`'s menu, a confirmation — and as the exit codes say.
            let renewing = match renewing {
                Some(account) if !named && ui::can_show_a_menu() => match renew_or_add(&account)? {
                    Some(renew) => renew.then_some(account),
                    None => {
                        ui::info("Login canceled.");
                        return Ok(ExitCode::Interrupted);
                    }
                },
                other => other,
            };
            // Before the browser opens, which is when there is still time to
            // stop: the profile is this account's.
            if let Some(account) = &renewing
                && let Ok(Some(stored)) = store.session_of(&paths.account(account.pk)).load()
            {
                announce_replacement(&stored);
            }
            by_browser(args, store, paths, renewing).await
        }
    }
}

/// Adds an account through a browser login, as `snob login --add --browser`
/// does, for the interactive views' account picker, which has no command
/// line to take the flags from.
pub async fn add_by_browser(store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let args = LoginArgs {
        paste: false,
        browser: true,
        user_agent: None,
        csrftoken: None,
        add: true,
        keep_profile: false,
    };
    // Renewing no account is what makes this an add; `add` mirrors the flag.
    run(args, store, paths, None, false).await
}

/// Asks whether this login is the account in use again, or another one:
/// `Some(true)` to log in again, `None` when the question was declined.
fn renew_or_add(account: &Viewer) -> Result<Option<bool>> {
    let again = format!("Log in again as {}", account.label());
    let chosen = ui::menu::choose(
        "Which account is this login for?",
        &[again.as_str(), "Add another account"],
    )?;
    Ok(chosen.map(|i| i == 0))
}

/// Says whose session is about to be replaced.
///
/// Logging in over an existing session is usually deliberate, so this does not
/// ask — but running it by accident and silently losing the account you were
/// on is a surprise worth one line. Only a login as that same account
/// replaces it; one as another is added beside it.
fn announce_replacement(stored: &Session) {
    let who = who(stored);
    ui::info(&format!(
        "There is already a session for {who}. Logging in as {who} again replaces it."
    ));
}

/// How to name the account a session belongs to.
fn who(session: &Session) -> String {
    Viewer {
        pk: session.ds_user_id,
        username: session.username.clone(),
    }
    .label()
}

/// Whether the data directory takes a file: the account's database goes
/// under it once the login says whose it is.
fn writable(paths: &AppPaths) -> Result<()> {
    paths.ensure_dirs()?;
    let probe = paths.data_dir().join(".write-probe");
    std::fs::write(&probe, b"probe")
        .with_context(|| format!("{} cannot be written to", paths.data_dir().display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Works out the method: whatever the flags say, or whatever the user picks
/// from the menu. With no interactive terminal there is no menu, so the method
/// has to be given explicitly.
fn choose_method(args: &LoginArgs) -> Result<Option<LoginMethod>> {
    if args.paste {
        return Ok(Some(LoginMethod::Paste));
    }
    if args.browser {
        return Ok(Some(LoginMethod::Browser));
    }
    if !ui::can_show_a_menu() {
        bail!(
            "there is no interactive terminal to show the menu in.\n\
             Give the method explicitly, for example \"snob login --paste\"."
        );
    }
    ui::choose_login_method()
}

/// Which browser this login is about.
///
/// With one installed there is nothing to ask. With several there is, and it
/// matters both ways: for `--browser` it decides which one opens, and for
/// `--paste` it decides the User-Agent the session will be tied to. Guessing
/// gets it wrong about half the time on a machine with Chrome and Edge.
///
/// The list is passed in rather than detected here. Off Windows, detecting
/// means launching every installed browser to ask its version, so detecting
/// again after `resolve_user_agent`'s own `detect_all` would spawn six
/// processes to answer a question worth three. It would also take the count
/// that decides whether to ask "is that the right browser?" from a different
/// snapshot than the browser actually chosen.
///
/// Esc and Ctrl+C end the command, with 130: a question the person declined is
/// not one to answer for them. Only a menu that could not be drawn falls back
/// on the preferred browser.
fn choose_browser(
    purpose: &str,
    installed: &[browser::Browser],
) -> Result<Option<browser::Browser>> {
    // Nothing to ask about: `first` is already `None` on an empty list and
    // already the only entry on a list of one, so the two cases that have no
    // question in them and the case where there is nobody to ask are one line.
    if installed.len() < 2 || !ui::can_show_a_menu() {
        return Ok(installed.first().cloned());
    }

    let labels: Vec<String> = installed
        .iter()
        .map(|b| format!("{} {}", b.name, b.major_version))
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    match ui::menu::choose(purpose, &refs) {
        // `get`, which is total, rather than trusting the index to be in range.
        Ok(Some(i)) => Ok(installed.get(i).cloned()),
        Ok(None) => {
            Err(ExitError::new(ExitCode::Interrupted, "no browser was chosen".to_string()).into())
        }
        Err(e) if crate::exit::from_chain(&e) == Some(ExitCode::Interrupted) => Err(e),
        // The menu failing to draw: the preferred one still beats refusing to
        // continue.
        Err(_) => Ok(installed.first().cloned()),
    }
}

/// Opens a browser, waits for the login, and takes the session from it.
///
/// Nothing is read out of the user's own browser profile. This one is ours,
/// under our data directory, and the browser hands the cookies over itself
/// through its debugging protocol. `snob logout` deletes it.
async fn by_browser(
    args: LoginArgs,
    store: SecretStore,
    paths: &AppPaths,
    renewing: Option<Viewer>,
) -> Result<ExitCode> {
    let installed = browser::detect_all();
    let Some(found) = choose_browser("Which browser should snob open?", &installed)? else {
        bail!(
            "no Chromium-based browser was found installed, and this needs one to \
             open.\n\
             Use \"snob login --paste\" instead."
        );
    };

    // The profile the login happens in. Logging an account in again, it is
    // that account's own: a profile still signed in is taken as it is, and a
    // new login there comes from the device Instagram already knows. Adding
    // one, it is a fresh one. Either becomes the profile of whoever signs in
    // once the login says who that is — somebody signing in as another account
    // on this one's profile takes that profile with them, and is told.
    //
    // Whether it was already there is asked before anything creates it. A
    // profile that was already here is the device every request is sent from,
    // and it may be in use right now by another snob — a failed or abandoned
    // login below is no reason to take it away. Only one this login created
    // is.
    //
    // The account's browser is closed first: a window on a profile one has
    // open would hand the login over to it and close, and the profile the
    // login ends up in replaces the account's. Closed, not held closed: a
    // command still connected to the owner — a monitor, an open view —
    // starts the account's browser again at its next request, and a window
    // opened then is handed over to it. A fresh profile has no browser on it,
    // and every other account's is left to the commands using it.
    let (profile, existed) = match &renewing {
        Some(previous) => {
            crate::owner::release(paths, Some(previous.pk)).await;
            crate::headless::profile::for_account(paths, previous.pk)?
        }
        None => (crate::headless::profile::for_a_login(paths)?, false),
    };
    let cancel = interrupt::install();

    // Where the profile is kept, rather than the name a login's own profile
    // has until its account is known.
    let kept_at = match &renewing {
        Some(_) => profile.clone(),
        None => paths.browser_profile(),
    };
    ui::info(&format!(
        "Opening {} on Instagram's login page.\n\
         It uses a profile of its own, under {}, so it is not your everyday browser \
         and logging in there changes nothing about it.\n\
         Log in as usual; snob will notice when you are done, and gives up after \
         {} minutes. Ctrl+C to cancel.",
        found.name,
        kept_at.display(),
        cdp::LOGIN_TIMEOUT.as_secs() / 60
    ));

    // Anything from here on can be interrupted, and a Ctrl+C has to read as
    // one rather than as a failure, so the result is held rather than unwrapped
    // until the browser has been shut down.
    let captured = capture(&found, args.user_agent.clone(), &profile, &cancel).await;
    // Whatever `capture` came back with, and before either exit below: the
    // profile holds a live session from the moment the form was submitted.
    //
    // **Kept when the login worked.** The profile is the device every request
    // is sent from (`headless/`): removing it would make each login a new,
    // never-seen browser. It goes when the login did not happen and this
    // login is what created it. **Never when it was already here**: the
    // commonest way for this launch to fail is another snob holding that very
    // profile, and it is not deleted under that snob's browser.
    if !existed && (captured.is_err() || cancel.is_canceled()) {
        // Off the worker: the removal retries with sleeps adding up to five
        // seconds, and this runtime has two workers, one of which has to stay
        // free for the Ctrl+C task to run at all.
        let profile = profile.clone();
        tokio::task::spawn_blocking(move || discard_profile(&profile))
            .await
            .context("the profile could not be removed")?;
    }
    if cancel.is_canceled() {
        ui::info("Login canceled.");
        return Ok(ExitCode::Interrupted);
    }
    let (cookies, user_agent) = captured?;

    let mut session = login::session_from_cookies(&cookies, &user_agent)?;
    session.user_agent_pinned = args.user_agent.is_some();
    session.browser = Some(found.name.to_string());

    if let Some(previous) = &renewing
        && existed
        && previous.pk != session.ds_user_id
    {
        let previous = previous.label();
        ui::warn(&format!(
            "account {} signed in on the browser profile of {previous}, which is now \
             {}'s: Instagram has seen both sign in from it. {previous} starts from a \
             fresh profile when it logs in again.",
            session.ds_user_id, session.ds_user_id
        ));
    }

    // A browser the owner started on the account's profile since this command
    // began, for another command, is closed before the profile is replaced.
    // So is the renewed account's when someone else signed in on its profile:
    // the swap moves that profile away.
    crate::owner::release(paths, Some(session.ds_user_id)).await;
    if let Some(previous) = &renewing
        && previous.pk != session.ds_user_id
    {
        crate::owner::release(paths, Some(previous.pk)).await;
    }

    // The login happened in this profile, so it is the device Instagram just
    // saw sign in, and it becomes the account's before anything is sent: the
    // check below goes out from the account's profile. A profile the account
    // had before is set aside until the session is stored. A move that will
    // not happen costs a warning, not the login.
    let swap =
        match crate::headless::profile::ProfileSwap::replace(paths, &profile, session.ds_user_id) {
            Ok(swap) => Some(swap),
            Err(e) => {
                ui::warn(&format!(
                    "{e:#}\n\
                     The profile the login happened in stays at {}, holding the session, \
                     and the account's requests go out from another; \"snob logout\" \
                     removes it.",
                    profile.display()
                ));
                None
            }
        };
    let made_it = swap
        .as_ref()
        .map_or(profile.as_path(), |s| s.profile.as_path());
    // The profile made this session, and is the browser's to keep current from
    // now on: the next run must not write the stored copy back over it.
    crate::headless::ProfileMark::after_login(made_it, &found, &session);
    let finished = finish(
        session,
        store,
        LoginMethod::Browser,
        paths,
        renewing.as_ref(),
    )
    .await;
    if let Some(swap) = swap {
        match &finished {
            Ok(_) => swap.keep(),
            Err(_) => swap.undo(),
        }
    }
    finished
}

/// Removes a profile a login created and did not finish.
///
/// **About 90 MB, and possibly a half-finished session.** A login that worked
/// keeps its profile, because every request is sent from it (`headless/`);
/// one that failed or was abandoned has nothing to keep it for, and was the
/// only thing that made it. `by_browser` says which.
///
/// Guarded by `is_safe_to_remove` like every other recursive delete here, and
/// retried briefly: the browser has just been told to close and Windows holds
/// a directory until the last handle in it goes. A failure is a warning rather
/// than an error, because the session has already been captured and losing the
/// login over housekeeping would be the worse outcome — and the sentence names
/// the path, so it can be removed by hand.
fn discard_profile(profile: &std::path::Path) {
    if !profile.exists() {
        return;
    }
    if !paths::is_safe_to_remove(profile) {
        ui::warn(&format!(
            "{} is too close to the root to remove; delete it by hand",
            profile.display()
        ));
        return;
    }

    if let Err(e) = paths::remove_tree_patiently(profile) {
        ui::warn(&format!(
            "the browser profile at {} could not be removed ({e}). It holds a \
             logged-in session; delete it by hand, or run \
             \"snob logout\" later.",
            profile.display()
        ));
    }
}

/// Drives the browser from launch to captured session, and always closes it.
///
/// A browser that is never connected to is killed by its own `Drop`, so only
/// the connected case needs closing by hand — which is why the result is held
/// rather than propagated until after `close`.
async fn capture(
    found: &browser::Browser,
    requested_user_agent: Option<String>,
    profile: &std::path::Path,
    cancel: &CancelToken,
) -> Result<(login::BrowserCookies, String)> {
    let cdp = cdp::Cdp::connect(cdp::launch(found, profile)?, cancel).await?;
    let outcome = collect(&cdp, found, requested_user_agent, cancel).await;
    cdp.close().await;
    outcome
}

async fn collect(
    cdp: &cdp::Cdp,
    found: &browser::Browser,
    requested_user_agent: Option<String>,
    cancel: &CancelToken,
) -> Result<(login::BrowserCookies, String)> {
    // Asked for rather than reconstructed: this is the browser the cookie will
    // belong to, and Instagram checks that the two agree. An explicit
    // --user-agent still wins, since it is there to override exactly this.
    let user_agent = match requested_user_agent {
        Some(ua) => ua,
        None => cdp.user_agent().await.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "falling back to the rebuilt User-Agent");
            found.user_agent()
        }),
    };

    // The profile keeps whatever was logged in last time, and Instagram sends a
    // browser that still has a session straight past the login page. Saying so
    // beats announcing an account nobody chose here.
    let cookies = match cdp.instagram_cookies().await? {
        Some(existing) => {
            ui::info(
                "This browser profile was still logged in, so that session is the one \
                 being stored.\n\
                 To sign in as somebody else, run \"snob login --add\".",
            );
            existing
        }
        None => {
            // Ten minutes with nothing on screen reads as a hang, and this is
            // the one stretch where the user is in another window typing a
            // password and a code and comes back to check.
            //
            // Started inside this branch rather than before `capture()`: the
            // sibling branch above prints with a plain `eprintln!`, which would
            // be overdrawn by a bar already running. The countdown is
            // deadline-driven, so one call covers the whole wait.
            //
            // `true` means "a bar was wanted", which is all this flag says.
            // Whether one can be drawn is `Progress`'s own question, and it
            // already asks it: the bar hides itself when standard error is not a
            // terminal, and `quiet` is read back off the bar rather than off the
            // flag. Probing stderr here as well would be a second answer to a
            // question that has one.
            let progress = Progress::new(true);
            progress.waiting("waiting for you to log in", cdp::LOGIN_TIMEOUT);
            let captured = cdp::wait_for_login(cdp, cancel).await;
            // Before the `?`, so both the Ctrl+C bail and a broken socket leave
            // a clean terminal behind.
            progress.finish();
            captured?
        }
    };

    Ok((cookies, user_agent))
}

async fn by_paste(args: LoginArgs, store: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // The User-Agent is settled first because it is the half that can still ask
    // questions. Sorting it out after a seventy-character paste means answering
    // a menu with the credential already sitting on screen.
    let chosen = match args.user_agent {
        // The flag is the user saying it outright.
        Some(ua) => ChosenAgent::pinned(ua),
        None => resolve_user_agent()?,
    };

    ui::paste_instructions();

    let sessionid = ui::prompt_secret("sessionid: ")?;
    if sessionid.trim().is_empty() {
        bail!("no sessionid was entered");
    }

    let mut session = login::session_from_paste(&sessionid, &chosen.user_agent)?;
    session.user_agent_pinned = chosen.pinned;
    session.browser = chosen.browser;
    // The requests go out from the browser that made the account's profile,
    // while it is installed (`ProfileMark`), whichever one was named here.
    if let Some(named) = &session.browser
        && !crate::headless::env_flag("SNOB_NO_BROWSER")
        && let Some(maker) =
            crate::headless::ProfileMark::read(&paths.browser_profile_for(session.ds_user_id))
                .browser
        && maker.is_file()
        && browser::detect_named(named).is_some_and(|b| b.path != maker)
    {
        ui::info(&format!(
            "Requests for this account go out from {}, the browser that made its profile, \
             rather than from {named}: a profile opens only in the browser that made it.",
            maker.display()
        ));
    }
    // Taken as given rather than prompted for, because it is not a credential
    // in the sense the sessionid is — it is the CSRF double-submit token, which
    // is worth nothing without the cookie — and because prompting for a second
    // secret nobody needs would put a question in front of every paste login to
    // serve the two commands that write. It still goes into a `Secret`: it is
    // part of the session record, and everything in that record is handled the
    // same way regardless of what it is worth on its own.
    //
    // Trimmed, and an empty value is left unset rather than stored blank.
    // `cookie_header` skips an empty optional cookie, so a blank one would be
    // silently absent while `IgClient::post`'s guard saw `Some` and let the
    // request go — a refusal moved from before the request to after it, for
    // no reason.
    session.csrftoken = args
        .csrftoken
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(Into::into);
    // The check goes out from a browser that takes this session, whatever the
    // clock said when the one it held was made (`headless::newer_login`).
    crate::owner::release(paths, Some(session.ds_user_id)).await;
    crate::headless::ProfileMark::forget_session(&paths.browser_profile_for(session.ds_user_id));
    finish(session, store, LoginMethod::Paste, paths, None).await
}

/// Validates, stores and reports. Shared by both methods so a session obtained
/// either way is checked and announced identically. `renewing` is the account
/// a browser login was for, whose replacement was said before it began.
async fn finish(
    mut session: Session,
    secrets: SecretStore,
    method: LoginMethod,
    paths: &AppPaths,
    renewing: Option<&Viewer>,
) -> Result<ExitCode> {
    let pk = session.ds_user_id;
    let account = paths.account(pk);
    let existed = account.dir().exists();
    // Before anything is written to the account's own database, which would
    // then be the one it keeps. Housekeeping: a failure costs a warning, not
    // the login.
    let adopted = match snob_store::layout::adopt(paths, pk) {
        Ok(adopted) => adopted,
        Err(e) => {
            ui::warn(&format!(
                "{e}\nThe data kept from before several accounts could be signed in stays at {}.",
                paths.unclaimed_dir().display()
            ));
            false
        }
    };
    let store = secrets.session_of(&account);
    let previous = store
        .load()
        .ok()
        .flatten()
        .filter(|previous| previous.ds_user_id == pk);
    if let Some(previous) = &previous
        && renewing.is_none_or(|renewing| renewing.pk != pk)
    {
        announce_replacement(previous);
    }
    // Neither method hands over a username. From the browser `validate`
    // reads it off the tab's document for nothing; without one it asks
    // `/api/v1/users/{pk}/info/` — a second request right after the check,
    // which is where a 429 and a two-hour cooldown land. Logging the stored
    // account in again already knows its name: the commonest login, after a
    // session expired, then costs the one request the check needs. So a
    // renamed account keeps its old name through a login, until
    // `snob whoami` reads the new one off the browser's document.
    if session.username.is_none() {
        session.username = previous.and_then(|previous| previous.username);
    }

    // Checking a session is a request like any other, charged to the budget
    // of the account that signed in: repeated logins spend.
    let pacer = crate::app::pacer_saying_a_line(&account)?;
    ui::info("Checking the session against Instagram...");
    let validated = login::validate(&mut session, pacer).await;
    let rejected = matches!(
        &validated,
        Err(login::LoginError::Instagram(ig)) if ig.invalidates_session()
    );
    let stored = match validated {
        Ok(outcome) => store.save(&session).map(|()| outcome).map_err(Into::into),
        // A rejection during login means different things depending on where
        // the session came from. Telling someone to check what they pasted is
        // useless advice when the browser handed it over itself.
        Err(e) => Err(match &e {
            login::LoginError::Instagram(ig) if ig.invalidates_session() => anyhow::anyhow!(e)
                .context(match method {
                    LoginMethod::Paste => {
                        "Instagram rejected the session. Check that the sessionid was \
                             copied whole and that the User-Agent belongs to the same browser"
                    }
                    LoginMethod::Browser => {
                        "Instagram rejected the session the browser handed over. It may \
                             have been signed out in the meantime; run \"snob logout\" and \
                             log in again"
                    }
                }),
            _ => anyhow::anyhow!(e),
        }),
    };
    let outcome = match stored {
        Ok(outcome) => outcome,
        Err(e) => {
            // The browser the check went out from holds this session now, as
            // the account's newest, and would go on sending it in place of the
            // one still stored; and a browser login's profile is about to be
            // put back under it. It is closed, and its mark forgets the
            // session, so the next command starts one that takes the stored
            // session whenever that was made.
            crate::owner::release(paths, Some(pk)).await;
            crate::headless::ProfileMark::forget_session(&paths.browser_profile_for(pk));
            forget_the_attempt(&account, adopted, !existed && rejected);
            return Err(e);
        }
    };
    if adopted {
        ui::info(&format!(
            "The data kept from before several accounts could be signed in is {}'s now.",
            who(&session)
        ));
    }

    // The account that signed in is the one commands act as from now on. A
    // login that did not learn the name keeps the one it had.
    let username = session.username.clone();
    let mut previous = None;
    let registry = Registry::update(paths, |registry| {
        previous = registry.active_account().map(crate::app::Viewer::from);
        let name = username
            .or_else(|| registry.get(pk).map(|a| a.username.clone()))
            .unwrap_or_default();
        registry.upsert(pk, &name, snob_core::clock::now());
        registry.active = Some(pk);
    })
    .context("the session was stored, but the list of accounts could not be updated")?;

    match outcome {
        ValidationOutcome::Confirmed => {
            crate::ui::say!(
                "Session stored for {} in the {}.",
                who(&session),
                store.describe()
            );
        }
        ValidationOutcome::Unconfirmed => {
            crate::ui::say!("Session stored in the {}.", store.describe());
            ui::warn(
                "it could not be confirmed with Instagram because it is throttling \
                 requests. The session is probably valid; check in a few minutes \
                 with \"snob whoami\".",
            );
        }
        ValidationOutcome::Skipped { until_ms } => {
            crate::ui::say!("Session stored in the {}.", store.describe());
            // Asked again, as the check's pacer is spent: what held it may be
            // the brake on every account rather than a cooldown of its own.
            let held = crate::app::pacer_saying_a_line(&account)
                .and_then(|pacer| crate::app::held(&pacer, Some(paths)))
                .ok()
                .flatten()
                .unwrap_or(crate::app::Held {
                    until_ms,
                    braked: Vec::new(),
                });
            ui::warn(&format!(
                "it was not checked: {}. Nothing is spent until then — not even the \
                 single request this would cost. Check it with \"snob whoami\" once it \
                 lifts.",
                report::held_until(&held)
            ));
        }
    }
    // Said when it moved, and only where there is another to have moved from.
    if registry.accounts.len() > 1 && previous.as_ref().is_none_or(|was| was.pk != pk) {
        let now = who(&session);
        match previous {
            Some(was) => crate::ui::say!("Active account: {now} (was {})", was.label()),
            None => crate::ui::say!("Active account: {now}"),
        }
    }

    // A browser login meant for another account that signed in as this one
    // leaves that account's session where it was.
    if let Some(renewing) = renewing.filter(|renewing| renewing.pk != pk)
        && secrets
            .session_of(&paths.account(renewing.pk))
            .something_is_stored()
    {
        ui::info(&format!(
            "{} stays signed in; \"snob logout --account {}\" signs it out.",
            renewing.label(),
            renewing.pk
        ));
    }

    // Somebody who has just logged in has no idea what to type next, and the
    // whole-account summary is the answer to the question they came with.
    ui::info("Try \"snob scan\" for the whole picture, or \"snob unfollowers\".");

    Ok(ExitCode::Ok)
}

/// Undoes what a login that did not happen left for an account nothing
/// signs in as, so the id stays nobody's: the data it adopted goes back to
/// waiting, and a directory it made for a session Instagram rejected goes.
/// Only while the registry does not list the account, and a failure costs a
/// warning: the login's own error is the one to report.
fn forget_the_attempt(account: &AccountPaths, adopted: bool, made_for_nothing: bool) {
    let listed =
        Registry::load(account).map_or(true, |registry| registry.get(account.pk()).is_some());
    if listed {
        return;
    }
    if adopted {
        if let Err(e) = snob_store::layout::unadopt(account, account.pk()) {
            ui::warn(&format!(
                "{e}\nThe data kept from before several accounts could be signed in stays at {}.",
                account.dir().display()
            ));
        }
    } else if made_for_nothing
        && paths::is_safe_to_remove(&account.dir())
        && let Err(e) = paths::remove_tree_patiently(&account.dir())
    {
        ui::warn(&format!(
            "{} could not be removed: {e}",
            account.dir().display()
        ));
    }
}

/// A User-Agent and what is known about where it came from.
struct ChosenAgent {
    user_agent: String,
    /// The user gave it outright, so nothing may rewrite it later.
    pinned: bool,
    /// Which browser it describes, when one was picked. This is what lets the
    /// daily refresh follow the right browser on a machine with several.
    browser: Option<String>,
}

impl ChosenAgent {
    fn pinned(user_agent: String) -> Self {
        Self {
            user_agent,
            pinned: true,
            browser: None,
        }
    }
}

/// Gets the User-Agent without sending the user to the browser console.
///
/// Instagram shows a warning in that console saying that anyone asking you to
/// paste something there is scamming you, and copying from it easily drags in
/// dozens of log lines that end up running as commands in the terminal. It is
/// rebuilt from the installed browser's version, which is the only part of
/// Chrome's User-Agent that varies since Google reduced it.
fn resolve_user_agent() -> Result<ChosenAgent> {
    let installed = browser::detect_all();

    if let Some(b) = choose_browser("Which browser is your Instagram session in?", &installed)? {
        let user_agent = b.user_agent();
        ui::info(&format!(
            "Using the User-Agent of {} {}.",
            b.name, b.major_version
        ));

        // With one browser installed nothing was asked, so the confirmation is
        // the only chance to say it guessed wrong. No second gate on there
        // being somebody to ask: `confirm` decides that itself, and a second
        // predicate for the same question can disagree with it, and a stricter
        // one would accept the guess in silence on a run with its output
        // redirected. That guess becomes the stored User-Agent, and a wrong one
        // is an `IgError::UserAgentMismatch` several commands later.
        if installed.len() == 1 && !ui::confirm("Is that the browser your session is in?", true)? {
            return prompt_user_agent();
        }
        return Ok(ChosenAgent {
            user_agent,
            pinned: false,
            browser: Some(b.name.to_string()),
        });
    }

    ui::warn("no Chromium-based browser was found installed");
    prompt_user_agent()
}

/// Asks for it outright, which is the last resort and also the most explicit
/// thing the user can do — so what comes back is pinned. Following a browser's
/// updates makes no sense for a string we could not have produced ourselves.
fn prompt_user_agent() -> Result<ChosenAgent> {
    // Guarded, or `echo "$SESSIONID" | snob login --paste` on a server with no
    // browser installed would feed the piped sessionid in as the User-Agent,
    // and then fail with a message about the User-Agent — never mentioning
    // that the thing it had eaten was the credential.
    if !ui::can_be_asked() {
        bail!(
            "there is no browser installed to take a User-Agent from, and no terminal \
             to ask for one at.\n\
             Give it explicitly, for example:\n\
            \x20   snob login --paste --user-agent \"Mozilla/5.0 (X11; Linux x86_64) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36\"\n\
             Copy the whole \"User Agent\" line from about:version in the browser your \
             Instagram session is in. The sessionid can still be piped in on standard \
             input."
        );
    }

    eprintln!(
        "\n\
         In the address bar of the browser your session is in, go to:\n\
        \n\
             about:version\n\
        \n\
         and copy the whole \"User Agent\" line. Your browser may show that\n\
         label translated.\n"
    );

    let text = ui::prompt_line("User-Agent: ")?;

    if ui::looks_like_console_dump(&text) {
        bail!(
            "that looks like a dump of the browser console, not a User-Agent.\n\
             Copying from the \"Console\" tab easily picks up dozens of lines, and \
             pasting them makes the terminal try to run each one.\n\
             Use \"about:version\", which shows the value on its own."
        );
    }

    Ok(ChosenAgent::pinned(text))
}
