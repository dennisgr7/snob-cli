//! What a finished child is told back to the client as.
//!
//! **Every string is filtered on the way out**, whatever field it is in. The
//! commands already filter the names they print, but a tool result is read by
//! a model, and the cost of a field that was missed is an instruction in
//! someone's bio arriving as text nobody on screen can see. `printable` is the
//! identity on everything a person can read, so filtering what was already
//! filtered changes nothing.
//!
//! **A list is handed back a page at a time** ([`page`]): a client reads a
//! tool's whole result into the model's context, and cuts what is too long
//! (Claude Code at 25,000 tokens, `MAX_MCP_OUTPUT_TOKENS`, as its documents
//! were quoted to us; not checked here). The page says the whole list's
//! `count` and the `next_offset` to call again with, and the later pages are
//! handed back from what was already read (`pages.rs`, `tools::Page`).

use serde_json::{Value, json};

use super::child::Finished;
use super::tools::Plan;
use crate::exit::ExitCode;

/// The result of a call: whether it failed, and the object to hand back.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub failed: bool,
    pub body: Value,
    /// The whole list, each item as its compact JSON, when `body` is one page
    /// of a longer one: for the server to hold for the pages after it.
    pub whole: Option<Vec<String>>,
}

/// The most accounts on a page.
pub const PAGE: usize = 200;

/// The most bytes of items on a page, as compact JSON, before the result
/// is filtered. An account is 100 to 150 bytes without a picture's address
/// and some 450 with one, whose signed query string tokenizes at about 3
/// bytes a token; the result is sent twice, as the structured content and as
/// its text (the specification asks for both), so 30,000 bytes keeps a page
/// near 20,000 tokens in all, under the cut quoted above. Without addresses
/// a page is [`PAGE`] accounts; with them, about 65.
pub const PAGE_BYTES: usize = 30_000;

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

/// What an agent is told to do about a refusal, by the reason the command
/// gave it, **in place of the command's own hint**, which is advice to the
/// person at a terminal: a command to type (`snob jobs cancel`), or a flag
/// the agent never passed and cannot (`--offline`, which its server adds for
/// a stored list, `--max-wait`, which its server sets). As [`SIGN_IN`] is
/// for no session; a reason not named here keeps its hint, which is already
/// one an agent can act on.
fn agent_hint(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "too_many_jobs" => {
            "one job runs at a time for the account: ask snob_job how that one is going (its \
             id is in the message), or stop it with snob_job_cancel, and call this again once \
             it has ended"
        }
        "no_such_job" => "snob_jobs lists this account's jobs and their ids",
        "nothing_stored" => {
            "nothing of this list is stored yet: call snob_list again without offset and with \
             mode \"auto\" to read it"
        }
        "budget_wait" => {
            "the request budget has no room within the wait this server allows: call it again \
             later; snob_status says when the budget has room"
        }
        _ => return None,
    })
}

/// Reads a finished child against what its plan counts as an answer.
pub fn from(plan: &Plan, finished: Finished) -> Answer {
    let exit = finished.code;
    let offset = plan.page.as_ref().map_or(0, |page| page.offset);
    let mut whole = None;
    let mut body = if plan.answers.contains(&exit) {
        // A structured result is an object, so whatever else a command printed
        // goes under a key. **Never index into what came back before it is
        // known to be an object**: `serde_json` panics on `value["key"] = ...`
        // for a string or a number, and under `panic = "abort"` that is the
        // whole server gone, every call in flight with it, over one line a
        // command printed.
        match finished.json {
            Some(Value::Array(items)) => {
                let items = compact(&items);
                // A call that takes no offset has no next page to offer.
                let body = page(&items, offset, plan.page.is_some());
                if !body["next_offset"].is_null() {
                    whole = Some(items);
                }
                body
            }
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
            if let Some(hint) = agent_hint(&reason) {
                failure["hint"] = json!(hint);
            }
            failure["code"] = json!(reason);
        }
        // What a list walk kept is still worth having: it is stored, and the
        // next call continues from it. One page of it, with no next one:
        // what a walk stopped short read is not stored as a whole list, so a
        // later page could not be read back, and the next call for the list
        // continues the walk instead.
        if let Some(partial) = finished.json {
            failure["partial"] = match partial {
                Value::Array(items) => page(&compact(&items), offset, false),
                other => other,
            };
        }
        if exit == ExitCode::NoSession {
            failure["hint"] = json!(SIGN_IN);
        }
        return Answer {
            failed: true,
            body: cleaned(json!({ "exit": exit.as_str(), "error": failure })),
            whole: None,
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
        whole,
    }
}

/// The page at `offset` of a list the server holds (`pages.rs`), as the
/// answer of a call that ran nothing.
pub fn held(items: &[String], offset: usize) -> Answer {
    Answer {
        failed: false,
        body: cleaned(page(items, offset, true)),
        whole: None,
    }
}

/// Each item as its compact JSON, which is what a page is measured in and
/// what the server holds.
fn compact(items: &[Value]) -> Vec<String> {
    items.iter().map(Value::to_string).collect()
}

/// The page of `items` that starts at `offset`: as many as fit in
/// [`PAGE_BYTES`], at most [`PAGE`] and at least one, so a page always moves
/// on. `{count, offset, items, next_offset}`, where `count` is the whole
/// list's and `next_offset` is null on the last page, or always when the
/// call cannot be made again for another (`pages` false). An offset past the
/// end is an empty page. **Sliced with `get`, never by indexing**: the offset
/// is the client's number, and a panic here is the whole server gone.
pub fn page(items: &[String], offset: usize, pages: bool) -> Value {
    let count = items.len();
    let rest = items.get(offset.min(count)..).unwrap_or_default();
    let (mut taken, mut bytes) = (0, 0);
    for item in rest.iter().take(PAGE) {
        bytes += item.len();
        if taken > 0 && bytes > PAGE_BYTES {
            break;
        }
        taken += 1;
    }
    let shown: Vec<Value> = rest
        .get(..taken)
        .unwrap_or_default()
        .iter()
        // Read back from what `compact` wrote, so it does not fail; a null
        // would still be better than a panic.
        .map(|item| serde_json::from_str(item).unwrap_or(Value::Null))
        .collect();
    let end = offset.min(count) + taken;
    let next_offset = (pages && end < count).then_some(end);
    json!({
        "count": count,
        "offset": offset,
        "items": shown,
        "next_offset": next_offset,
    })
}

/// What an agent is told of a page with another after it, beside the JSON:
/// which accounts these are, and how the next ones are had, by calling
/// `tool` again (`snob_list`, or `snob_job` for a job's result). `None` for
/// any other answer.
pub fn next_page(body: &Value, tool: &str) -> Option<String> {
    let next = body.get("next_offset")?.as_u64()?;
    let count = body.get("count")?.as_u64()?;
    let offset = body.get("offset")?.as_u64()?;
    // A confirmation on a call that asks nothing is refused, and only a
    // list's call may carry one.
    let and = if tool == "snob_list" {
        ", no confirmation,"
    } else {
        ""
    };
    Some(format!(
        "Accounts {first} to {next} of {count}. For the next ones, call {tool} again with \
         the same arguments{and} and \"offset\": {next}; a later page is handed back from \
         what was already read, and sends nothing and asks nothing.",
        first = offset + 1,
    ))
}

/// The last few lines of what a command said, which is where its sentence
/// about the failure is: the records of the log left out first
/// (`jobs::show::sentences`), which carry what Instagram answered and would
/// push the sentence out.
fn last_lines(said: &str) -> String {
    let lines =
        crate::jobs::show::sentences(said.lines().filter(|l| !l.trim().is_empty()).collect());
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
            page: None,
            run: crate::commands::mcp::tools::Run::Inline,
        }
    }

    /// A list's plan, for the page at `offset`.
    fn list_at(offset: usize) -> Plan {
        Plan {
            page: Some(crate::commands::mcp::tools::Page {
                offset,
                of: "the call".into(),
                held_only: false,
            }),
            ..plan(&[ExitCode::Ok])
        }
    }

    /// `n` accounts, as a list command prints them.
    fn accounts(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|n| json!({"pk": n, "username": format!("user{n}")}))
                .collect(),
        )
    }

    fn usernames(body: &Value) -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["username"].as_str().unwrap().to_string())
            .collect()
    }

    /// A refusal whose hint is advice to the person at a terminal is told
    /// to the agent in its own tools' words.
    #[test]
    fn a_hint_for_the_terminal_is_told_to_the_agent_in_its_tools() {
        for (reason, exit, tool) in [
            ("too_many_jobs", ExitCode::Error, "snob_job_cancel"),
            ("no_such_job", ExitCode::Error, "snob_jobs"),
            ("nothing_stored", ExitCode::Error, "mode \"auto\""),
            ("budget_wait", ExitCode::Interrupted, "snob_status"),
        ] {
            let told = from(
                &plan(&[ExitCode::Ok]),
                finished(
                    exit,
                    None,
                    Some(json!({"error": {
                        "code": exit.as_str(),
                        "reason": reason,
                        "message": "it was refused",
                        "hint": "run \"snob jobs cancel\", or drop --offline, or --max-wait",
                    }})),
                ),
            );
            assert!(told.failed);
            let error = &told.body["error"];
            assert_eq!(error["code"], reason);
            let hint = error["hint"].as_str().unwrap();
            assert!(hint.contains(tool), "{reason}: {hint}");
            assert!(!hint.contains("snob jobs cancel"), "{reason}: {hint}");
            assert!(!hint.contains("--"), "{reason}: {hint}");
        }
        // A reason with a hint an agent can already act on keeps it.
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(
                ExitCode::Error,
                None,
                Some(json!({"error": {"code": "error", "reason": "busy", "hint": "wait"}})),
            ),
        );
        assert_eq!(told.body["error"]["hint"], "wait");
    }

    /// The records of the log a failed walk wrote are no part of what the
    /// agent is told: the sentences are.
    #[test]
    fn a_failure_says_its_sentences_not_its_log() {
        let mut walked = finished(ExitCode::RateLimited, Some(accounts(2)), None);
        walked.said = " WARN snob_store::store::rate_budget: account in cooldown \
                       reason=\"rate_limit\" minutes=120\n \
                       WARN snob_ig::client: Instagram pushed back endpoint=\"/api/v1/x/\" \
                       status=429 body={\"message\":\"Please wait a few minutes\"}\n\
                       warning: Instagram asked to slow down\n\
                       warning: the walk stopped after 2 accounts; they are stored\n"
            .into();
        let told = from(&list_at(0), walked);
        assert!(told.failed);
        assert_eq!(
            told.body["error"]["message"],
            "warning: Instagram asked to slow down \
             warning: the walk stopped after 2 accounts; they are stored"
        );
    }

    #[test]
    fn a_long_list_is_handed_back_a_page_at_a_time() {
        let mut read = Vec::new();
        let mut offset = 0;
        let mut kept = None;
        loop {
            let told = from(
                &list_at(offset),
                finished(ExitCode::Ok, Some(accounts(450)), None),
            );
            assert!(!told.failed);
            assert_eq!(told.body["count"], 450, "the whole list's");
            assert_eq!(told.body["offset"], offset);
            let page = usernames(&told.body);
            assert!(page.len() <= PAGE);
            read.extend(page);
            if let Some(whole) = told.whole {
                // What the server holds pages the same as the command did.
                assert_eq!(held(&whole, offset).body, told.body);
                kept = Some(whole);
            }
            match told.body["next_offset"].as_u64() {
                Some(next) => {
                    assert!(
                        next_page(&told.body, "snob_list")
                            .unwrap()
                            .contains(&format!("\"offset\": {next}"))
                    );
                    offset = usize::try_from(next).unwrap();
                }
                None => break,
            }
        }
        let every: Vec<String> = (0..450).map(|n| format!("user{n}")).collect();
        assert_eq!(read, every, "each account once, in the order served");
        assert_eq!(kept.unwrap().len(), 450);
    }

    #[test]
    fn the_last_page_has_no_next_offset() {
        let told = from(
            &list_at(400),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(usernames(&told.body).len(), 50);
        assert!(told.body["next_offset"].is_null());
        assert!(next_page(&told.body, "snob_list").is_none());
        assert!(told.whole.is_none(), "nothing after it to hold for");

        // A list that fits on one page is the same shape, and held for nothing.
        let short = from(&list_at(0), finished(ExitCode::Ok, Some(accounts(3)), None));
        assert_eq!(short.body["count"], 3);
        assert!(short.body["next_offset"].is_null());
        assert!(short.whole.is_none());
    }

    #[test]
    fn an_offset_past_the_end_is_an_empty_page() {
        for offset in [450, 451, usize::MAX] {
            let told = from(
                &list_at(offset),
                finished(ExitCode::Ok, Some(accounts(450)), None),
            );
            assert!(!told.failed, "{offset}");
            assert_eq!(told.body["items"], json!([]), "{offset}");
            assert_eq!(told.body["count"], 450, "{offset}");
            assert!(told.body["next_offset"].is_null(), "{offset}");
            assert_eq!(held(&[], offset).body["items"], json!([]), "{offset}");
        }
    }

    #[test]
    fn a_page_of_long_accounts_is_cut_by_its_size_and_still_moves_on() {
        let long = |n: usize, length: usize| {
            Value::Array(
                (0..n)
                    .map(|n| json!({"pk": n, "pfp_url": "x".repeat(length)}))
                    .collect(),
            )
        };
        let told = from(
            &list_at(0),
            finished(ExitCode::Ok, Some(long(300, 400)), None),
        );
        let shown = told.body["items"].as_array().unwrap().len();
        assert!(shown < PAGE, "{shown}");
        let bytes: usize = told.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.to_string().len())
            .sum();
        assert!(bytes <= PAGE_BYTES, "{bytes}");
        assert_eq!(told.body["next_offset"], shown);

        // One account past the size on its own is a page of one, not none.
        let huge = from(
            &list_at(0),
            finished(ExitCode::Ok, Some(long(2, PAGE_BYTES)), None),
        );
        assert_eq!(huge.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(huge.body["next_offset"], 1);
    }

    #[test]
    fn what_a_walk_stopped_short_read_is_one_page_with_none_after_it() {
        let told = from(
            &list_at(0),
            finished(
                ExitCode::Interrupted,
                Some(accounts(450)),
                Some(json!({"error": {"code": "interrupted"}})),
            ),
        );
        assert!(told.failed);
        let partial = &told.body["error"]["partial"];
        assert_eq!(partial["count"], 450);
        assert_eq!(usernames(partial).len(), PAGE);
        assert!(partial["next_offset"].is_null());
        assert!(told.whole.is_none());
        assert!(next_page(&told.body, "snob_list").is_none());
    }

    #[test]
    fn a_list_from_a_call_that_takes_no_offset_offers_no_next_page() {
        let told = from(
            &plan(&[ExitCode::Ok]),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(told.body["count"], 450);
        assert_eq!(usernames(&told.body).len(), PAGE);
        assert!(told.body["next_offset"].is_null());
        assert!(told.whole.is_none());
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
            assert!(told.body["error"]["partial"]["items"].is_array(), "{error}");
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
