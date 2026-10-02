//! Removes everything the tool has stored on this computer.
//!
//! This exists because no package manager can do it. `winget uninstall`,
//! `brew uninstall` and `apt remove` take the binary away and stop there: the
//! session, the database and the browser profiles live in the user's own
//! directories, which the package never owned and therefore never deletes. The
//! first of those is a working Instagram credential, and leaving one behind on
//! a machine whose owner has just decided to get rid of the tool is the failure
//! this command exists to prevent.
//!
//! **The binary itself is not removed.** A running process cannot reliably
//! delete its own executable on Windows, and a binary that deleted itself out
//! from under `winget` or `apt` would leave that manager reporting a version
//! that is no longer there. The command says where the file is and lets
//! whatever installed it take it away.

use std::path::{Path, PathBuf};

use anyhow::Result;
use snob_core::Pk;
use snob_store::paths::{self, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::{Kind, SecretStore};
use snob_store::store::shared::Shared;

use crate::app::Viewer;
use crate::cli::PurgeArgs;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// What a purge would remove, worked out before anything is deleted.
///
/// Built whole and shown before the question is asked. This is the only command
/// in the tool that destroys data the user cannot get back, and "yes" to a list
/// of paths is a different answer from "yes" to a warning.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Whether there is a stored session to delete.
    pub session: bool,
    /// The accounts whose sessions go when everything does: every one the
    /// registry names and every one with a directory, in case the registry is
    /// gone or damaged. `only` names the one when one goes.
    pub accounts: Vec<Pk>,
    /// The monitor's stored secrets, when there are any.
    ///
    /// **For the listing only.** What `execute` removes is `Kind::ALL`, always,
    /// and it does not consult this: the deletion must not be gated on a survey
    /// the keyring could answer wrongly.
    pub secrets: Vec<Kind>,
    /// Directories to remove, whole.
    pub directories: Vec<PathBuf>,
    /// The one account `purge --account` removes, its place in the registry
    /// with it; `None` is everything.
    pub only: Option<Viewer>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        !self.session
            && self.secrets.is_empty()
            && self.directories.is_empty()
            && self.only.is_none()
    }

    /// One line per thing that will go, for the user to read before answering.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.session {
            // Deliberately not named after one backend. The session is deleted
            // from the keyring *and* from the fallback file whichever of them
            // this run would have written to, so naming either would be a
            // half-truth in the one message that has to be exact.
            lines.push("the stored session, from the system keyring and from disk".to_string());
        }
        // Named separately from the session, because they are a different
        // thing to lose: a webhook token and a signing key belong to the
        // user's own server, not to Instagram, and somebody re-running `setup`
        // afterwards has to know they will have to enter them again.
        if !self.secrets.is_empty() {
            let named: Vec<&str> = self
                .secrets
                .iter()
                .map(|kind| match kind {
                    Kind::WatchToken => "webhook token",
                    Kind::WatchSigningKey => "signing key",
                })
                .collect();
            lines.push(format!(
                "the monitor's stored {}, from the system keyring",
                named.join(" and ")
            ));
        }
        lines.extend(self.directories.iter().map(|d| d.display().to_string()));
        if let Some(account) = &self.only {
            lines.push(format!(
                "{} from the accounts signed in here",
                account.label()
            ));
        }
        lines
    }
}

/// Looks at the machine and reports what there is to remove.
pub fn survey(store: &SecretStore, app_paths: &AppPaths) -> Plan {
    let accounts = Registry::known_accounts(app_paths);
    Plan {
        // This command's promise is that afterwards there is no session on the
        // machine, so it asks the store the question that covers a credential too
        // corrupt to parse as well as a readable one.
        session: store.sessions_stored(&accounts),
        accounts,
        secrets: store.monitor_secrets_stored(),
        directories: app_paths
            .owned_dirs()
            .into_iter()
            .filter(|dir| dir.exists())
            .filter(|dir| {
                let safe = paths::is_safe_to_remove(dir);
                if !safe {
                    ui::warn(&format!(
                        "not removing {}: it is not where this tool's data belongs",
                        dir.display()
                    ));
                }
                safe
            })
            .collect(),
        only: None,
    }
}

/// What `purge --account` removes: one account's session, its directory and
/// its browser profile, and its place in the registry. Nothing of any other
/// account's, and none of the monitor's secrets.
pub fn survey_account(store: &SecretStore, app_paths: &AppPaths, account: Viewer) -> Plan {
    let paths = app_paths.account(account.pk);
    Plan {
        session: store.session_of(&paths).something_is_stored(),
        accounts: Vec::new(),
        secrets: Vec::new(),
        directories: [paths.dir(), paths.browser_profile()]
            .into_iter()
            .filter(|dir| dir.exists() && paths::is_safe_to_remove(dir))
            .collect(),
        only: Some(account),
    }
}

/// Something that would not go, and why.
///
/// Collected rather than returned as an error at the first failure. The likely
/// one is a browser profile still locked by a browser that has not quit, and
/// letting that end the run would leave the credential — the item that actually
/// matters — sitting in the keyring.
#[derive(Debug)]
pub struct Failure {
    pub what: String,
    pub why: String,
    /// Whether this is the credential rather than a directory of stored data.
    ///
    /// A purge that leaves a browser profile behind has failed at housekeeping;
    /// one that leaves the session behind has failed at the only thing it
    /// exists for. The two do not deserve the same closing sentence, and a
    /// field says which is which without anyone matching on [`Failure::what`].
    pub credential: bool,
}

/// Deletes what the plan describes and returns whatever refused to go.
///
/// The session goes first, on purpose: it is the only item here that is a live
/// credential, and it must not be held hostage by a directory that turns out to
/// be locked.
pub fn execute(plan: &Plan, store: &SecretStore, app_paths: &AppPaths) -> Vec<Failure> {
    let mut failures = Vec::new();

    // `delete_all`, not `delete`: this is the command whose whole promise is
    // that nothing of snob's is left, so it takes the monitor's webhook token
    // and signing key too. `logout` is the caller that must not.
    //
    // **Unconditional**, not gated on `plan.session`, which reads the session
    // entry and nothing else. `snob watch setup` needs no session, and `snob
    // logout` removes the one there is by design, so after the ordinary
    // sequence setup → logout → purge a gated call would report snob's files
    // gone, exit 0, and leave the webhook token and the signing key in the
    // keyring for good.
    //
    // Nothing is lost by always asking: `delete_all` walks `Kind::ALL` and a
    // `NoEntry` for every one of them is already `Ok(())`.
    let credentials = match &plan.only {
        Some(account) => store.session_of(&app_paths.account(account.pk)).delete(),
        None => store.delete_all(&plan.accounts),
    };
    if let Err(e) = credentials {
        failures.push(Failure {
            what: "the stored credentials".to_string(),
            why: e.to_string(),
            credential: true,
        });
    }

    for dir in &plan.directories {
        // `remove_tree`, not `remove_dir_all`: the scratch root sits at a
        // predictable name in the shared temporary directory, and a link
        // planted there is removed as a link instead of being reported as our
        // failure to clean up. Patiently: the owner has just closed the
        // browsers on the profiles in there, and a browser takes a moment to
        // let go of them.
        if let Err(e) = paths::remove_tree_patiently(dir) {
            failures.push(Failure {
                what: dir.display().to_string(),
                why: e.to_string(),
                credential: false,
            });
        }
    }

    for dir in &plan.directories {
        remove_if_empty(dir.parent());
    }

    // Its push-backs, which every other account reads from `shared.db`. A
    // purge of everything takes the file whole with the data directory.
    if let Some(account) = &plan.only
        && app_paths.shared_db_file().exists()
        && let Err(e) = Shared::open(app_paths).and_then(|shared| shared.forget(account.pk))
    {
        failures.push(Failure {
            what: format!(
                "the push-backs on {} that the other accounts read",
                account.label()
            ),
            why: e.to_string(),
            credential: false,
        });
    }

    if let Some(account) = &plan.only
        && let Err(e) = Registry::update(app_paths, |registry| registry.remove(account.pk))
    {
        failures.push(Failure {
            what: format!("{} from the accounts signed in here", account.label()),
            why: e.to_string(),
            credential: false,
        });
    }

    failures
}

/// Removes a parent that its contents leaving has left empty.
///
/// On Windows the configuration directory and the one earlier versions used are
/// siblings inside a single `snob-ig` folder, so removing both leaves that
/// folder behind: empty, and named after a tool the user has just finished
/// removing. `remove_dir` succeeds only on an empty directory, which is what
/// makes this safe to point at a parent at all.
///
/// It has to be **our** folder, though. Elsewhere the layout is flat: on Linux the parents are
/// `~/.local/share` and `~/.config`, on macOS `~/Library/Application Support`.
/// Those belong to the platform rather than to snob, they are routinely empty
/// on a minimal container or server image, and removing one is outside what
/// this command was asked to do.
fn remove_if_empty(dir: Option<&Path>) {
    if let Some(dir) = dir
        && dir
            .file_name()
            .is_some_and(|name| name == paths::app_dir_name())
        && paths::is_safe_to_remove(dir)
    {
        let _ = std::fs::remove_dir(dir);
    }
}

pub fn run(args: PurgeArgs, store: SecretStore, app_paths: &AppPaths) -> Result<ExitCode> {
    run_with(args, store, app_paths, ui::can_be_asked())
}

/// Split from [`run`] so a test can say whether anybody is there.
///
/// **The fourth argument is for tests only.** Being a terminal is a property of
/// the process's streams, and a test binary inherits whatever the suite was
/// started from — so the one refusal here that depends on it would otherwise be
/// pinned by a test that passes or fails according to who ran it.
#[doc(hidden)]
pub fn run_with(
    args: PurgeArgs,
    store: SecretStore,
    app_paths: &AppPaths,
    someone_is_there: bool,
) -> Result<ExitCode> {
    carry_out(
        args,
        &survey(&store, app_paths),
        &store,
        app_paths,
        someone_is_there,
    )
}

/// `purge --account`: one account's session, data and browser profile.
///
/// Named only by what was typed: an account taken from `SNOB_ACCOUNT` or the
/// registry is not one somebody chose to destroy.
pub async fn run_for_account(
    args: PurgeArgs,
    store: SecretStore,
    app_paths: &AppPaths,
    name: &str,
) -> Result<ExitCode> {
    let account = crate::account::named(name, &Registry::load(app_paths)?)
        .map_err(crate::account::Unresolved::into_error)?;
    // Its browser would write into the profile as it went, or keep Windows
    // from removing it. The owner and the other accounts' browsers stay.
    crate::owner::release(app_paths, Some(account.pk)).await;
    warn_about_watched(app_paths, &account);
    let plan = survey_account(&store, app_paths, account);
    carry_out(args, &plan, &store, app_paths, ui::can_be_asked())
}

/// Says which of the monitor's entries name `account` as their viewer.
fn warn_about_watched(app_paths: &AppPaths, account: &Viewer) {
    let Ok(Some(watch)) = snob_store::config::load(app_paths) else {
        return;
    };
    let naming = watch
        .accounts
        .iter()
        .filter(|entry| entry.viewer == Some(account.pk))
        .count();
    if naming > 0 {
        ui::warn(&format!(
            "{naming} of the accounts watch.toml watches name {} as their viewer; give them \
             another, or remove them, in {}",
            account.label(),
            snob_store::config::path(app_paths).display()
        ));
    }
}

/// Shows the plan, asks, and removes it.
fn carry_out(
    args: PurgeArgs,
    plan: &Plan,
    store: &SecretStore,
    app_paths: &AppPaths,
    someone_is_there: bool,
) -> Result<ExitCode> {
    if plan.is_empty() {
        crate::ui::say!("There is nothing of snob's stored on this computer.");
        report_the_binary();
        return Ok(ExitCode::Ok);
    }

    crate::ui::say!("This will delete, from this computer:");
    for line in plan.lines() {
        crate::ui::say!("  {line}");
    }

    if args.dry_run {
        crate::ui::say!("\nNothing was deleted (--dry-run).");
        return Ok(ExitCode::Ok);
    }

    // With no terminal, `confirm` keeps its default without asking anything —
    // and the default here is no. An uninstall script that got exit 0 for that
    // would carry on to `apt remove` with the session cookie still in the
    // keyring. Success is the one thing that must not be reported there: this is the
    // command whose entire purpose is that a live credential does not outlive
    // the tool.
    //
    // `--yes` remains the way to do it unattended, and it has to be typed on
    // purpose, which is the right way round for the only command here that
    // destroys anything.
    // The same predicate `confirm` gates on, deliberately: two questions about
    // whether anybody is there, asked differently, is how one of them starts
    // answering for a person who is sitting right in front of it.
    if !args.consent.yes && !someone_is_there {
        // The shared refusal, which is where the exit code and the "advice in
        // the message, never on a hint" rule live.
        return Err(crate::report::refuse_unattended(
            "nothing was deleted: it needs confirmation".to_string(),
            match &plan.only {
                Some(account) => format!(
                    "To delete it unattended, run \"snob purge --account {} --yes\".",
                    account.pk
                ),
                None => "To delete it unattended, run \"snob purge --yes\".".to_string(),
            },
        ));
    }

    // 130, as the exit codes promise for a confirmation not given and as
    // `follow` answers it: a script that asked for a purge and got a "no" has
    // not had its purge, and 0 said it had.
    if !args.consent.yes && !ui::confirm("\nDelete all of it?", false)? {
        crate::ui::say!("Nothing was deleted.");
        return Ok(ExitCode::Interrupted);
    }

    let failures = execute(plan, store, app_paths);

    if failures.is_empty() {
        crate::ui::say!("Done.");
        // Said rather than implied. Removing a file unlinks it; on any modern
        // filesystem the bytes may survive in a journal, a shadow copy, a
        // snapshot, or — on flash — in a block the drive has not yet erased.
        // No program running as an ordinary user can promise otherwise, and a
        // command called `purge` is exactly where someone would assume it had.
        let whose = plan
            .only
            .as_ref()
            .map_or_else(|| "snob".to_string(), |account| account.label());
        ui::info(&format!(
            "{whose}'s files are gone from this computer. Whether the underlying bytes can \
             still\nbe recovered from the disk is not something any program can decide; \
             full-disk\nencryption is what makes a deletion final."
        ));
    }
    for failure in &failures {
        ui::warn(&format!(
            "could not remove {}: {}",
            failure.what, failure.why
        ));
    }

    // The credential is the point of this command, so a refusal there is not
    // one more line in the warning list. It also decides which closing sentence
    // is true: "the session is still active on Instagram" says the only thing
    // left is Instagram's side, which is the false half of the message when the
    // copy on this computer is what would not go.
    let session_survived = failures.iter().any(|f| f.credential);
    if session_survived {
        ui::warn(
            "the stored session is still on this computer. Removing it is what this command \
             exists for, so treat the rest of this run as not done.",
        );
    } else if plan.session {
        // What logout says, for the same reason: nothing was closed on
        // Instagram's side, because closing it would be a write.
        ui::info(
            "\nThe session is still active on Instagram. To really close it, use\n\
             \"Active sessions\" in the app's settings.",
        );
    }

    // Uninstalling is what a whole purge is for; one account's is not.
    if plan.only.is_none() {
        report_the_binary();
    }

    if failures.is_empty() {
        Ok(ExitCode::Ok)
    } else {
        // A non-zero exit matters more than the wording here: whoever scripted
        // an uninstall needs to find out that part of it did not happen.
        Err(ExitError::new(ExitCode::Error, "some of it could not be removed").into())
    }
}

/// Says where the executable is, since this command deliberately leaves it.
///
/// The path rather than a command to run: which one is right depends on how the
/// binary was installed, which this cannot know, and naming the wrong package
/// manager is worse than naming none.
fn report_the_binary() {
    let location = match std::env::current_exe() {
        Ok(path) => format!(
            "\nThe binary itself is still installed, at {}.",
            path.display()
        ),
        Err(_) => "\nThe binary itself is still installed.".to_string(),
    };
    ui::info(&format!(
        "{location}\nRemove it with whatever installed it, or delete that file."
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_plan_lists_nothing() {
        assert!(Plan::default().is_empty());
        assert!(Plan::default().lines().is_empty());
    }

    /// The session is one line whatever backend holds it, and the directories
    /// follow it verbatim.
    #[test]
    fn the_listing_covers_the_session_and_every_directory() {
        let plan = Plan {
            session: true,
            accounts: vec![],
            secrets: vec![],
            directories: vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")],
            only: None,
        };
        let lines = plan.lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("session"));
        assert!(lines[1].contains("one"));
        assert!(lines[2].contains("two"));
    }

    /// The monitor's secrets are named separately from the session, because
    /// they are a different thing to lose: they belong to the user's own
    /// server, and somebody re-running `setup` afterwards has to know they will
    /// have to enter them again.
    #[test]
    fn the_listing_names_the_monitors_secrets_too() {
        let plan = Plan {
            session: false,
            accounts: vec![],
            secrets: vec![Kind::WatchToken, Kind::WatchSigningKey],
            directories: vec![],
            only: None,
        };
        assert!(!plan.is_empty(), "two live credentials are not nothing");

        let lines = plan.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("webhook token"), "{lines:?}");
        assert!(lines[0].contains("signing key"), "{lines:?}");
        assert!(
            !lines[0].contains("session"),
            "there is no session here to claim: {lines:?}"
        );
    }

    /// Nothing to remove is not a failure: someone uninstalling a tool that
    /// never got as far as storing anything should be told so, not errored at.
    #[test]
    fn a_plan_with_no_session_and_no_directories_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        // A name of its own per run, like every other test service name in the
        // tree. A fixed one is shared with whatever else happens to be running
        // -- and `cargo test --workspace` runs this binary alongside
        // `snob-core`'s, in a different process, against the one credential
        // store the operating system has.
        let store = SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-purge-empty-{}", std::process::id()));

        let failures = execute(&Plan::default(), &store, &paths);
        assert!(failures.is_empty());
    }
}
