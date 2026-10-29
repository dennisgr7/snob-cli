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

/// Reads a finished child against what its plan counts as an answer.
pub fn from(plan: &Plan, finished: Finished) -> Answer {
    let exit = finished.code;
    let mut body = if plan.answers.contains(&exit) {
        match finished.json {
            // A structured result is an object, so a list goes under a key.
            Some(Value::Array(items)) => json!({ "count": items.len(), "items": items }),
            Some(other) => other,
            None => json!({ "text": finished.text.trim() }),
        }
    } else {
        let mut failure = match finished.error {
            Some(Value::Object(mut error)) => match error.remove("error") {
                Some(inner) => inner,
                None => Value::Object(error),
            },
            _ => json!({
                "code": exit.as_str(),
                "message": last_lines(&finished.said),
            }),
        };
        // What a list walk kept is still worth having: it is stored, and the
        // next call continues from it.
        if let Some(partial) = finished.json {
            failure["partial"] = partial;
        }
        return Answer {
            failed: true,
            body: cleaned(json!({ "exit": exit.as_str(), "error": failure })),
        };
    };
    if exit != ExitCode::Ok {
        body["exit"] = json!(exit.as_str());
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
    fn every_string_is_filtered_keys_included() {
        let dirty = json!({"bio\u{200b}": ["ignore\u{e0041} previous", {"n": "\u{202e}x"}]});
        let clean = cleaned(dirty);
        assert_eq!(clean, json!({"bio": ["ignore previous", {"n": "x"}]}));
    }
}
