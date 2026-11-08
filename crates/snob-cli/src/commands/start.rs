//! `snob` with no command, on a terminal: where to start.
//!
//! Clap's answer to a missing command is an error that lists every command,
//! the hidden plumbing (`__browser-owner`, `__job`) included, and says nothing
//! about which one to type first. A person who types the name of the tool
//! is asking what it does, so on a terminal the answer is a short screen
//! built from what is stored here: who is signed in, which lists are kept and
//! from when, and the handful of commands worth knowing first.
//!
//! **Sends nothing and writes nothing**, for the same reasons as `snob
//! status`, whose rules it keeps: no request, no browser, no keyring read, no
//! database brought into being (`Store::read_existing`). Off a terminal it is
//! not shown at all: a script that forgot its command gets the error and
//! exit 2 that every other mistake on the command line gets (`main`).
//!
//! One of the listed exceptions to "commands take an App", for the reason
//! `status` gives: an `App` would write.

use anyhow::Result;
use snob_core::model::ListKind;
use snob_store::paths::AppPaths;
use snob_store::registry::Registry;
use snob_store::store::{Store, snapshots};

use crate::exit::ExitCode;
use crate::report;

/// What is said when the command is missing on a terminal. `flag` and `env`
/// are `--account` and `SNOB_ACCOUNT`, so the screen speaks of the account
/// the next command would act as.
pub fn run(paths: &AppPaths, flag: Option<&str>, env: Option<&str>) -> Result<ExitCode> {
    crate::ui::say!("{}", screen(paths, flag, env));
    Ok(ExitCode::Ok)
}

/// The screen, as text. Whatever cannot be read is left out rather than
/// failing: this is a greeting, and every command it suggests says for
/// itself what is wrong.
fn screen(paths: &AppPaths, flag: Option<&str>, env: Option<&str>) -> String {
    let mut out = String::from("snob: Instagram from the terminal.\n\n");
    let registry = Registry::load(paths).unwrap_or_default();
    let account = crate::account::resolve(flag, env, &registry).ok();
    match &account {
        Some(viewer) => {
            out.push_str(&format!("Signed in here as {}.\n", viewer.label()));
            out.push_str(&stored_lists(paths, viewer.pk));
        }
        None if registry.accounts.is_empty() => {
            out.push_str("Nobody is signed in on this computer yet.\n");
        }
        // Several signed in and none chosen, or a name that is none of
        // them: `snob account list` says which there are.
        None => {
            out.push_str("Several accounts are signed in here; \"snob account list\" names them.\n")
        }
    }

    out.push_str("\nStart here:\n");
    let mut lines: Vec<(&str, &str)> = Vec::new();
    if account.is_none() {
        lines.push(("snob login", "sign in, in a browser window snob opens"));
    }
    lines.extend([
        (
            "snob scan",
            "read your followers and following once, and cross them",
        ),
        (
            "snob unfollowers",
            "the accounts you follow that do not follow you back",
        ),
        (
            "snob fans",
            "the accounts that follow you that you do not follow back",
        ),
        (
            "snob profile NAME",
            "an account's card: stories, highlights, posts",
        ),
        ("snob status", "where your account stands; sends nothing"),
        ("snob --help", "every command"),
    ]);
    let width = lines
        .iter()
        .map(|(command, _)| command.len())
        .max()
        .unwrap_or(0);
    for (command, what) in lines {
        out.push_str(&format!("  {command:width$}  {what}\n"));
    }
    out.pop();
    out
}

/// "Stored: followers, 1,204 accounts, read Aug 3 at 14:12." and the same
/// for following, or a line saying none is, for the account `pk`.
fn stored_lists(paths: &AppPaths, pk: snob_core::Pk) -> String {
    let account = paths.account(pk);
    let Ok(Some(store)) = Store::read_existing(&account) else {
        return "No list is stored yet.\n".to_string();
    };
    let mut lines = String::new();
    for kind in [ListKind::Followers, ListKind::Following] {
        if let Ok(Some(snapshot)) = snapshots::latest_complete(store.conn(), pk, kind)
            && let Some(taken_at) = snapshot.taken_at
        {
            lines.push_str(&format!(
                "Stored: your {}, {} accounts, read {}.\n",
                kind.as_str(),
                snapshot.member_count,
                report::stored_on(taken_at),
            ));
        }
    }
    if lines.is_empty() {
        "No list is stored yet.\n".to_string()
    } else {
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_nobody_signed_in_the_screen_starts_with_login_and_lists_no_hidden_command() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(dir.path());
        let text = screen(&paths, None, None);
        assert!(text.contains("Nobody is signed in"), "{text}");
        assert!(text.contains("snob login"), "{text}");
        assert!(text.contains("snob scan"), "{text}");
        assert!(!text.contains("__"), "{text}");
    }

    #[test]
    fn the_screen_writes_nothing_where_nothing_was() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let paths = AppPaths::rooted_at(&root);
        let _ = screen(&paths, None, None);
        assert!(!root.exists());
    }
}
