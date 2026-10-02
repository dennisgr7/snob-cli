//! `snob account`: the accounts signed in here, and the one a command acts as
//! when none is named.
//!
//! `list` is one of the few places that looks at every account. It asks each
//! whether a session is stored and when its database last changed, and opens
//! none of them.

use anyhow::Result;
use snob_core::{Epoch, Pk};
use snob_store::paths::AppPaths;
use snob_store::registry::Registry;
use snob_store::secrets::SecretStore;

use crate::account::Unresolved;
use crate::app::Viewer;
use crate::cli::{AccountCommand, AccountListArgs, AccountUseArgs};
use crate::exit::ExitCode;
use crate::ui;

/// Runs `account list` or `account use`. `flag` and `env` are what `list`
/// resolves its `viewer` from; `use` names its account itself.
pub fn run(
    command: AccountCommand,
    secrets: &SecretStore,
    paths: &AppPaths,
    flag: Option<&str>,
    env: Option<&str>,
) -> Result<ExitCode> {
    match command {
        AccountCommand::List(args) => list(&args, secrets, paths, flag, env),
        AccountCommand::Use(args) => make_active(&args, secrets, paths),
    }
}

/// One account, as `list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    pk: Pk,
    username: String,
    active: bool,
    /// Where its session is stored: `keyring`, `file`, `unreadable` when
    /// something is there that does not read as one, or nothing.
    session: Option<&'static str>,
    /// When its database last changed.
    last_used: Option<Epoch>,
}

fn list(
    args: &AccountListArgs,
    secrets: &SecretStore,
    paths: &AppPaths,
    flag: Option<&str>,
    env: Option<&str>,
) -> Result<ExitCode> {
    let registry = Registry::load(paths)?;
    // Several accounts and none chosen is what this command helps with, so
    // only a name that fits no account, or more than one, is refused.
    let viewer = match crate::account::resolve(flag, env, &registry) {
        Ok(viewer) => Some(viewer),
        Err(Unresolved::NoAccount | Unresolved::NoneChosen) => None,
        Err(refused) => return Err(refused.into_error()),
    };
    if let Some(viewer) = &viewer {
        crate::report::act_as(viewer.clone());
    }

    let active = registry.active_account().map(|account| account.pk);
    let rows: Vec<Row> = registry
        .accounts
        .iter()
        .map(|account| {
            let paths = paths.account(account.pk);
            let session = match secrets.session_of(&paths).load_located() {
                Ok(Some((_, backend))) => Some(backend.as_str()),
                Ok(None) => None,
                Err(_) => Some("unreadable"),
            };
            Row {
                pk: account.pk,
                username: account.username.clone(),
                active: active == Some(account.pk),
                session,
                last_used: modified(&paths.db_file()),
            }
        })
        .collect();

    if args.output.json {
        ui::say!(
            "{}",
            serde_json::to_string_pretty(&json(active, &rows, viewer.as_ref()))?
        );
    } else if rows.is_empty() {
        ui::say!("No account is signed in here. Run \"snob login\".");
    } else {
        ui::say!("{}", table(&rows));
    }
    Ok(ExitCode::Ok)
}

fn make_active(args: &AccountUseArgs, secrets: &SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    // Looked up under the registry's lock, so the account made active is one
    // the file still lists when it is written.
    let mut outcome = Err(Unresolved::NoAccount);
    Registry::update(paths, |registry| {
        outcome = crate::account::named(&args.name, registry).map(|chosen| {
            let previous = registry.active_account().map(Viewer::from);
            registry.active = Some(chosen.pk);
            (chosen, previous)
        });
    })?;
    let (chosen, previous) = outcome.map_err(Unresolved::into_error)?;

    let now = chosen.label();
    match previous.filter(|previous| previous.pk != chosen.pk) {
        Some(previous) => ui::say!("Active account: {now} (was {})", previous.label()),
        None => ui::say!("Active account: {now}"),
    }
    if !secrets
        .session_of(&paths.account(chosen.pk))
        .something_is_stored()
    {
        ui::warn(&format!(
            "{now} has no session stored here; sign in as it with \"snob login --account {}\"",
            chosen.pk
        ));
    }
    Ok(ExitCode::Ok)
}

/// When a file last changed, if it exists.
fn modified(file: &std::path::Path) -> Option<Epoch> {
    let at = std::fs::metadata(file)
        .and_then(|meta| meta.modified())
        .ok()?;
    let seconds = at.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(seconds).ok().map(Epoch::new)
}

/// The status object. Every value is a stable token or a number; the
/// username is the true value, as `whoami` gives it.
fn json(active: Option<Pk>, rows: &[Row], viewer: Option<&Viewer>) -> serde_json::Value {
    serde_json::json!({
        "active": active,
        "accounts": rows
            .iter()
            .map(|row| serde_json::json!({
                "pk": row.pk,
                "username": row.username,
                "active": row.active,
                "session": row.session,
                "last_used": row.last_used,
            }))
            .collect::<Vec<_>>(),
        "viewer": viewer.map(Viewer::json),
    })
}

/// The accounts, one a line under a header, the active one marked `*`.
fn table(rows: &[Row]) -> String {
    let lines: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            [
                crate::app::label(
                    row.pk,
                    Some(row.username.as_str()).filter(|name| !name.is_empty()),
                ),
                row.pk.to_string(),
                row.session.unwrap_or("no session").to_string(),
                row.last_used
                    .map_or_else(|| "never".to_string(), crate::report::dated),
            ]
        })
        .collect();
    let header = ["ACCOUNT", "ID", "SESSION", "LAST USED"].map(String::from);
    let widths: Vec<usize> = (0..3)
        .map(|column| {
            std::iter::once(&header)
                .chain(&lines)
                .map(|line| line[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let marks =
        std::iter::once(" ").chain(rows.iter().map(|row| if row.active { "*" } else { " " }));
    std::iter::once(&header)
        .chain(&lines)
        .zip(marks)
        .map(|(line, mark)| {
            format!(
                "{mark} {:<w0$}  {:<w1$}  {:<w2$}  {}",
                line[0],
                line[1],
                line[2],
                line[3],
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        vec![
            Row {
                pk: Pk::new(42),
                username: "me".into(),
                active: true,
                session: Some("file"),
                last_used: Some(Epoch::new(1_790_000_000)),
            },
            Row {
                pk: Pk::new(7),
                username: String::new(),
                active: false,
                session: None,
                last_used: None,
            },
        ]
    }

    #[test]
    fn the_table_marks_the_active_account_and_says_what_is_missing() {
        let table = table(&rows());
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        assert!(lines[0].starts_with("  ACCOUNT"), "{table}");
        assert!(lines[1].starts_with("* @me "), "{table}");
        assert!(lines[2].starts_with("  account 7 "), "{table}");
        assert!(lines[2].contains("no session") && lines[2].ends_with("never"));
        // The columns line up.
        let at = |line: &str| line.find("SESSION").or_else(|| line.find("file"));
        assert_eq!(at(lines[0]), at(lines[1]));
    }

    #[test]
    fn the_object_names_the_active_account_and_the_viewer() {
        let viewer = Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        };
        let said = json(Some(Pk::new(42)), &rows(), Some(&viewer));
        assert_eq!(said["active"], 42);
        assert_eq!(said["viewer"]["username"], "me");
        assert_eq!(said["accounts"][0]["session"], "file");
        assert_eq!(said["accounts"][0]["last_used"], 1_790_000_000);
        assert!(said["accounts"][1]["session"].is_null());
        assert!(said["accounts"][1]["last_used"].is_null());

        let nobody = json(None, &[], None);
        assert!(nobody["active"].is_null() && nobody["viewer"].is_null());
        assert_eq!(nobody["accounts"], serde_json::json!([]));
    }
}
