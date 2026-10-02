use anyhow::Result;
use snob_core::Epoch;
use snob_ig::client::IgClient;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WhoamiArgs;
use crate::exit::ExitCode;
use crate::report;
use crate::ui;

/// Reports on the session of `account`, the account resolved for this run,
/// if any account is signed in at all.
pub async fn run(
    args: WhoamiArgs,
    secrets: SecretStore,
    account: Option<AccountPaths>,
) -> Result<ExitCode> {
    let store = account.as_ref().map(|paths| secrets.session_of(paths));
    // The account asked about, whether or not it has a session to report on.
    let viewer = crate::report::acting_as().map(|viewer| viewer.json());
    let found = match &store {
        Some(store) => store.load()?,
        None => None,
    };
    let (Some(mut session), Some(store), Some(paths)) = (found, &store, &account) else {
        eprintln!("No session stored. Run \"snob login\".");
        // `--json` still gets an object, as a session that has *died* does:
        // the two states an automation most wants to tell apart share an exit
        // code, and neither may hand the parser nothing to read. Every field
        // the object always carries is here; everything that describes a
        // session that does not exist is null.
        if args.output.json {
            crate::ui::say!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "pk": serde_json::Value::Null,
                    "username": serde_json::Value::Null,
                    "origin": serde_json::Value::Null,
                    "storage": secrets.backend().as_str(),
                    "storage_path": store.as_ref().and_then(|s| s.storage_path()),
                    "created_at": serde_json::Value::Null,
                    "validated_at": serde_json::Value::Null,
                    "alive": false,
                    "not_checked": "no_session",
                    "cooldown_until": serde_json::Value::Null,
                    "error": {
                        "code": ExitCode::NoSession.as_str(),
                        "message": "no session is stored on this computer",
                    },
                    "viewer": viewer,
                }))?
            );
        }
        return Ok(ExitCode::NoSession);
    };

    let mut alive = None;
    // The two facts the command holds on to: when a cooldown ends, and what
    // Instagram actually said. The address in a challenge is held
    // exactly once ever — nothing stores it — so dropping it means the user has
    // to provoke the error again to see it.
    //
    // Why the session was not checked is not a third variable. It is these two
    // read back at the point it is reported, so a return added inside the block
    // below cannot leave it claiming a check that never happened.
    let mut cooldown_until: Option<Epoch> = None;
    let mut failure: Option<serde_json::Value> = None;
    // The code the command exits with, held rather than returned so the JSON
    // still gets printed on the way out. It matches `alive`: `alive: false`
    // under exit 0 would tell a script the opposite of what the body says,
    // and the machine format is precisely the one that cannot read the
    // sentence.
    let mut code = ExitCode::Ok;

    if !args.offline {
        // Through `app::pacer_saying_a_line`, so Ctrl+C reaches the request
        // and a wait the budget imposes is announced.
        let pacer = crate::app::pacer_saying_a_line(paths)?;

        // A cooldown means nothing is spent, and checking a session is a
        // request like any other. `--offline` is the way to ask anyway, and it
        // is what this falls back to.
        if let Some(held) = crate::app::held(&pacer, Some(paths))? {
            // Seconds, like `created_at` and `validated_at` in the same object.
            // The conversion is the moment type's own, so the field here and the
            // date on the next line cannot disagree about which second it is.
            cooldown_until = Some(held.until_ms.to_epoch());
            let mut said = report::held_until(&held);
            said[..1].make_ascii_uppercase();
            eprintln!("{said}, so the session was not checked.");
        } else {
            let client = IgClient::new(session.clone(), pacer)?;
            match client.whoami().await {
                Ok(identity) => {
                    alive = Some(true);
                    // A name the session did not have, or a new one: the
                    // browser reads the account's name off every document
                    // it loads, so a rename is picked up here.
                    if identity.username.is_some() && identity.username != session.username {
                        session.username = identity.username;
                    }
                    session.mark_validated();
                    // Worth persisting so the name is not looked up again —
                    // and this is the only command that writes it back, so a
                    // silent failure here means every later run pays for the
                    // lookup again and nothing ever says why.
                    if let Err(e) = store.save(&session) {
                        ui::warn(&format!(
                            "the session could not be updated, so the account name will be \
                             looked up again next time: {e}"
                        ));
                    }
                }
                Err(e) => {
                    alive = Some(false);
                    code = crate::exit::from_ig_error(&e);
                    // Through `report`, in both shapes: the advice naming a
                    // `snob` subcommand lives there, and a dead session, which
                    // is the whole reason somebody runs this command, is one
                    // of the failures that carries it.
                    let said = report::what_instagram_said(&e);
                    failure = Some(serde_json::json!({
                        "code": code.as_str(),
                        // Validated against instagram.com before it ever reached
                        // an `IgError`, so handing it to a caller adds no trust.
                        "url": e.challenge_url(),
                        "message": said,
                    }));
                    // The detail goes to standard error either way, so the JSON
                    // on standard output stays parseable.
                    eprintln!("The session is not responding: {said}");
                }
            }
        }
    }

    if args.output.json {
        // Every value here is a stable token, never a human-facing string —
        // with exactly one exception, `error.message`, which is documentation
        // for a person and which nothing may branch on. Rewording anything else
        // would break this contract.
        //
        // `cooldown_until` says **when**, never **why**: the reason is written
        // to the cooldowns table but the read side does not hand it back, and
        // widening a snob-core trait for it is not worth doing here.
        let out = serde_json::json!({
            "pk": session.ds_user_id,
            "username": session.username,
            "origin": session.origin.as_str(),
            "storage": store.backend().as_str(),
            "storage_path": store.storage_path(),
            "created_at": session.created_at,
            "validated_at": session.validated_at,
            "alive": alive,
            "not_checked": not_checked(args.offline, cooldown_until),
            "cooldown_until": cooldown_until,
            "error": failure,
            "viewer": viewer,
        });
        crate::ui::say!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        crate::ui::say!("{}", describe(&session, alive));
    }

    Ok(code)
}

/// Why the session was not checked, when it was not.
///
/// Derived rather than tracked. Both inputs are already held for their own sake
/// — `--offline` is the request not to look, and a cooldown end only exists when
/// one was found — so there is nothing here that can disagree with them.
fn not_checked(offline: bool, cooldown_until: Option<Epoch>) -> Option<&'static str> {
    if offline {
        Some("offline")
    } else if cooldown_until.is_some() {
        Some("cooldown")
    } else {
        None
    }
}

fn describe(session: &snob_core::session::Session, alive: Option<bool>) -> String {
    let mut lines = Vec::new();

    match &session.username {
        // Filtered here and not in the JSON above: this line is drawn on a
        // terminal, and the name was written by `whoami` out of Instagram's
        // answer. `serde_json` escapes what it emits, and a machine format has
        // to carry the true value.
        Some(u) => lines.push(format!(
            "Account:  @{} ({})",
            snob_core::model::printable(u),
            session.ds_user_id
        )),
        None => lines.push(format!("Account:  {}", session.ds_user_id)),
    }
    lines.push(format!("Origin:   {}", session.origin));
    lines.push(match alive {
        Some(true) => "Status:   the session is responding".to_string(),
        Some(false) => "Status:   the session is NOT responding".to_string(),
        None => "Status:   not checked".to_string(),
    });

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three states, and the one that matters: `--offline` is why nothing
    /// was checked even when a cooldown is also in force, because the user asked
    /// not to look before anything went to find out.
    #[test]
    fn why_nothing_was_checked_is_read_back_off_the_two_facts() {
        assert_eq!(not_checked(true, None), Some("offline"));
        assert_eq!(not_checked(true, Some(Epoch::new(1))), Some("offline"));
        assert_eq!(not_checked(false, Some(Epoch::new(1))), Some("cooldown"));
        assert_eq!(not_checked(false, None), None);
    }
}
