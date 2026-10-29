//! What a finished child is told back to the client as.
//!
//! **Every string is filtered on the way out**, whatever field it is in. The
//! commands already filter the names they print, but a tool result is read by
//! a model, and the cost of a field that was missed is an instruction in
//! someone's bio arriving as text nobody on screen can see. `printable` is the
//! identity on everything a person can read, so filtering what was already
//! filtered changes nothing.

use serde_json::{Value, json};

use super::child::Finished;
use super::tools::Plan;
use crate::exit::ExitCode;

/// The result of a call: whether it failed, and the object to hand back.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub failed: bool,
    pub body: Value,
}

/// What an agent is told when there is no session to act with.
///
/// **It replaces the command's own hint** (`run "snob login"`), which is
/// advice to the person at the terminal: read by a model, it is an invitation
/// to run `login` itself, which no tool offers and which needs the password
/// or a browser window only the person has. The server's instructions
/// (`mod.rs`, `get_info`) say the same, so a model that skipped one still
/// reads the other.
pub const SIGN_IN: &str = "the person signs in with \"snob login\" in their own terminal; \
                           never ask them for a password or a session id";

/// Reads a finished child against what its plan counts as an answer.
pub fn from(plan: &Plan, finished: Finished) -> Answer {
    let exit = finished.code;
    let mut body = if plan.answers.contains(&exit) {
        // A structured result is an object, so whatever else a command printed
        // goes under a key. **Never index into what came back before it is
        // known to be an object**: `serde_json` panics on `value["key"] = ...`
        // for a string or a number, and under `panic = "abort"` that is the
        // whole server gone, every call in flight with it, over one line a
        // command printed.
        match finished.json {
            Some(Value::Array(items)) => json!({ "count": items.len(), "items": items }),
            Some(document @ Value::Object(_)) => document,
            Some(other) => json!({ "value": other }),
            None => json!({ "text": finished.text.trim() }),
        }
    } else {
        let said = || {
            json!({
                "code": exit.as_str(),
                "message": last_lines(&finished.said),
            })
        };
        // `main::wording_for` prints an object under `error`, but the line is
        // only known to be JSON with that key: `{"error":"x"}` is read here
        // too, and its `error` is put in an object rather than indexed into.
        let mut failure = match finished.error {
            Some(Value::Object(mut error)) => match error.remove("error") {
                Some(inner @ Value::Object(_)) => inner,
                Some(Value::Null) => said(),
                Some(other) => json!({ "code": exit.as_str(), "message": other }),
                None => Value::Object(error),
            },
            _ => said(),
        };
        // A finer reason the command gave (`busy`, from `--spend-alone`) is
        // the code an agent tells this failure apart by, the same one the
        // server's own refusal of a busy call has; `exit` keeps the exit's
        // token. `failure` is an object on every branch above.
        if let Some(reason) = failure
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            failure["code"] = json!(reason);
        }
        // What a list walk kept is still worth having: it is stored, and the
        // next call continues from it.
        if let Some(partial) = finished.json {
            failure["partial"] = partial;
        }
        if exit == ExitCode::NoSession {
            failure["hint"] = json!(SIGN_IN);
        }
        return Answer {
            failed: true,
            body: cleaned(json!({ "exit": exit.as_str(), "error": failure })),
        };
    };
    if exit != ExitCode::Ok {
        body["exit"] = json!(exit.as_str());
    }
    if exit == ExitCode::NoSession {
        // `status` puts its error object beside the sections it could not
        // fill; the hint goes in it when it is an object, and beside it
        // otherwise, never by indexing into what is not known to be one.
        match body.get_mut("error").and_then(Value::as_object_mut) {
            Some(error) => {
                error.insert("hint".into(), json!(SIGN_IN));
            }
            None => body["hint"] = json!(SIGN_IN),
        }
    }
    Answer {
        failed: false,
        body: cleaned(body),
    }
}

/// The last few lines of what a command said, which is where its sentence
/// about the failure is.
fn last_lines(said: &str) -> String {
    let lines: Vec<&str> = said.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(6)..].join("\n")
}

/// `value` with every string, keys included, through `printable`.
pub fn cleaned(value: Value) -> Value {
    use snob_core::model::printable;
    match value {
        Value::String(text) => Value::String(printable(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(cleaned).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| (printable(&key), cleaned(value)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(answers: &'static [ExitCode]) -> Plan {
        Plan {
            args: vec![],
            ask: None,
            answers,
            spends: crate::commands::mcp::tools::Spends::Nothing,
        }
    }

    fn finished(code: ExitCode, json: Option<Value>, error: Option<Value>) -> Finished {
        Finished {
            code,
            json,
            text: String::new(),
            error,
            said: "first\nsecond\n".into(),
        }
    }

    #[test]
    fn a_list_is_an_object_with_its_items() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Ok, Some(json!([{"username": "a"}])), None),
        );
        assert!(!told.failed);
        assert_eq!(told.body["count"], 1);
    }

    #[test]
    fn a_failure_carries_the_commands_own_error_object() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::RateLimited,
                None,
                Some(json!({"viewer": null, "error": {"code": "rate_limited", "hint": "wait"}})),
            ),
        );
        assert!(told.failed);
        assert_eq!(told.body["error"]["code"], "rate_limited");
        assert_eq!(told.body["exit"], "rate_limited");
    }

    #[test]
    fn a_reason_the_command_gave_is_the_code_the_agent_reads() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::Error,
                None,
                Some(json!({"error": {"code": "error", "reason": "busy", "message": "held"}})),
            ),
        );
        assert!(told.failed);
        assert_eq!(told.body["error"]["code"], "busy");
        assert_eq!(told.body["exit"], "error");
        // And none given changes nothing.
        let plain = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::Error,
                None,
                Some(json!({"error": {"code": "error", "reason": null}})),
            ),
        );
        assert_eq!(plain.body["error"]["code"], "error");
    }

    #[test]
    fn a_failure_with_no_error_object_says_its_last_lines() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Error, None, None),
        );
        // Through `printable`, which turns the line break into a space.
        assert_eq!(told.body["error"]["message"], "first second");
    }

    #[test]
    fn a_code_the_plan_counts_as_an_answer_is_not_a_failure() {
        let told = from(
            &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
            finished(ExitCode::RateLimited, Some(json!({"cooldown": {}})), None),
        );
        assert!(!told.failed);
        assert_eq!(told.body["exit"], "rate_limited");
    }

    #[test]
    fn an_error_line_whose_error_is_not_an_object_is_still_an_answer() {
        for error in [json!("x"), json!(7), json!(["x"]), json!(null)] {
            let told = from(
                &plan(&[ExitCode::Ok]),
                finished(
                    ExitCode::Error,
                    // With a partial result too, which is the field that was
                    // set by indexing.
                    Some(json!([{"username": "a"}])),
                    Some(json!({ "error": error.clone() })),
                ),
            );
            assert!(told.failed, "{error}");
            assert_eq!(told.body["error"]["code"], "error", "{error}");
            assert!(told.body["error"]["partial"].is_array(), "{error}");
        }
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Error, None, Some(json!({"error": "x"}))),
        );
        assert_eq!(told.body["error"]["message"], "x");
    }

    #[test]
    fn a_document_that_is_not_an_object_is_put_under_a_key() {
        for document in [json!("a string"), json!(3), json!(true), json!(null)] {
            // Under a code the plan counts as an answer but that is not 0,
            // which is where `exit` was set by indexing.
            let told = from(
                &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
                finished(ExitCode::RateLimited, Some(document.clone()), None),
            );
            assert!(!told.failed, "{document}");
            assert_eq!(told.body["value"], cleaned(document.clone()));
            assert_eq!(told.body["exit"], "rate_limited", "{document}");
        }
    }

    #[test]
    fn no_session_tells_the_agent_that_the_person_signs_in() {
        // `status --json` with no session: an answer, its error object given
        // the hint in place of the one meant for a person.
        let status = from(
            &plan(&[ExitCode::Ok, ExitCode::NoSession, ExitCode::RateLimited]),
            finished(
                ExitCode::NoSession,
                Some(json!({
                    "session": null,
                    "error": {"code": "no_session", "message": "no session is stored"},
                    "viewer": null,
                })),
                None,
            ),
        );
        assert!(!status.failed);
        assert_eq!(status.body["exit"], "no_session");
        assert_eq!(status.body["error"]["hint"], SIGN_IN);
        assert_eq!(status.body["error"]["message"], "no session is stored");

        // A document with no error object is given the hint beside it.
        let bare = from(
            &plan(&[ExitCode::Ok, ExitCode::NoSession]),
            finished(ExitCode::NoSession, Some(json!({"error": "x"})), None),
        );
        assert_eq!(bare.body["hint"], SIGN_IN);
        assert_eq!(bare.body["error"], "x");

        // Any other call without a session fails, with the same hint over the
        // command's own.
        let list = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::NoSession,
                None,
                Some(json!({"error": {"code": "no_session", "hint": "run \"snob login\""}})),
            ),
        );
        assert!(list.failed);
        assert_eq!(list.body["error"]["hint"], SIGN_IN);

        // And nothing else is given it.
        let throttled = from(
            &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
            finished(ExitCode::RateLimited, Some(json!({"cooldown": {}})), None),
        );
        assert!(throttled.body.get("hint").is_none());
    }

    #[test]
    fn every_string_is_filtered_keys_included() {
        let dirty = json!({"bio\u{200b}": ["ignore\u{e0041} previous", {"n": "\u{202e}x"}]});
        let clean = cleaned(dirty);
        assert_eq!(clean, json!({"bio": ["ignore previous", {"n": "x"}]}));
    }
}
