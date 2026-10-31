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
//! were quoted to us; not checked here). The page says the `count` of the
//! list the command printed and the `next_offset` to call again with, and
//! the later pages are handed back from what was already read (`pages.rs`,
//! `tools::Page`), with what the first page said of where the list came from.
//!
//! **A list is never handed back as more than it is.** One served from
//! storage says so and how old it is (`served`, [`not_read_now`]), and one
//! a walk cut short by `max_pages` says it is not the whole list
//! (`complete: false`, [`incomplete`]), since both are a success to the
//! command, whose warnings a person reads on standard error and an agent
//! never sees.

use serde_json::{Value, json};

use super::child::Finished;
use super::pages::Whole;
use super::tools::Plan;
use crate::exit::ExitCode;

/// The result of a call: whether it failed, and the object to hand back.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub failed: bool,
    pub body: Value,
    /// The whole list, each item as its compact JSON, when `body` is one page
    /// of a longer one: for the server to hold for the pages after it, with
    /// what the page said about the list ([`ABOUT`]).
    pub whole: Option<Whole>,
}

/// The fields of a list's answer that are about the whole list rather than
/// the page, which every later page handed back from it repeats: where it
/// came from and when, when a cooldown lifts, and whether it is all of it.
const ABOUT: [&str; 3] = ["served", "cooldown_until", "complete"];

/// The most accounts on a page.
pub const PAGE: usize = 200;

/// The most bytes of items on a page, as compact JSON, before the result
/// is filtered. An account is 100 to 150 bytes once its picture's address
/// is left out ([`LEFT_OUT`]), so a page is [`PAGE`] accounts; the bound is
/// for longer ones, names in scripts that take several bytes a character.
/// The result is sent twice, as the structured content and as its text (the
/// specification asks for both), so 30,000 bytes keeps a page near 20,000
/// tokens in all, under the cut quoted above.
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
        "job_stopping" => {
            "the account's job was asked to stop and is stopping: snob_job tells when it has \
             stopped (its id is in the message); call this again then"
        }
        "nothing_stored" => {
            "nothing of this list is stored yet: call snob_list again without offset and with \
             mode \"auto\" to read it"
        }
        "stored_moments" => {
            "the two stored lists were read too far apart to be crossed: call snob_list again \
             without offset and with mode \"auto\" (not \"stored\"), so both lists are checked \
             against the account"
        }
        "busy" => {
            "another call that reaches Instagram is running for this account, and one runs \
             at a time; nothing was sent: call this again once it has ended, not beside it \
             (snob_jobs names a job that is walking, and snob_job_cancel stops it)"
        }
        "mcp_write_cap" => {
            "agents have made as many follows and unfollows on this account as it allows \
             them in a day: do not call this again before retry_at (seconds since the \
             epoch); tell the person, who can make it in their own terminal"
        }
        "walked_elsewhere" => {
            "another walk holds this list and nothing was started: when retry_at is set \
             (seconds since the epoch), a job that stopped without letting go holds it until \
             then, so call this again only after it; otherwise call it again once that walk \
             has finished, and it uses what the walk stored"
        }
        "budget_wait" => {
            "the request budget has no room within the wait this server allows: call it again \
             later; snob_status says when the budget has room"
        }
        _ => return None,
    })
}

/// What an agent is told of a push-back met by this very call, which
/// started a cooldown the failure does not date (`report::error_json_for`
/// dates only one that refused the run): the command's own sentence says to
/// run it again once the cooldown lifts, which an agent cannot see.
pub const PUSHED_BACK: &str = "Instagram pushed back and a cooldown has started for this \
                               account: snob_status, which sends nothing, says when it lifts; \
                               call nothing that reaches Instagram before then, and tell the \
                               person when the wait ends";

/// Reads a finished child against what its plan counts as an answer.
pub fn from(plan: &Plan, finished: Finished) -> Answer {
    let exit = finished.code;
    let offset = plan.page.as_ref().map_or(0, |page| page.offset);
    let limit = plan.page.as_ref().and_then(|page| page.limit);
    let mut whole_items = None;
    // Past 0, only with the document it is an answer for: `status` and a dry
    // run print one whatever their exit, and the same exit with nothing on
    // standard output is a refusal, which an empty answer would hide.
    let answered = plan.answers.contains(&exit)
        && (exit == ExitCode::Ok || matches!(finished.json, Some(Value::Object(_))));
    let mut body = if answered {
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
                let body = page(&items, offset, limit, plan.page.is_some());
                if !body["next_offset"].is_null() {
                    whole_items = Some(items);
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
                Value::Array(items) => page(&compact(&items), offset, limit, false),
                other => other,
            };
        }
        if exit == ExitCode::NoSession {
            failure["hint"] = json!(SIGN_IN);
        }
        // A push-back this call met: the cooldown it started is not in the
        // failure, and the command's advice is to type it again later.
        if exit == ExitCode::RateLimited
            && !failure.get("cooldown_until").is_some_and(Value::is_i64)
        {
            failure["hint"] = json!(PUSHED_BACK);
        }
        return Answer {
            failed: true,
            body: cleaned(json!({ "exit": exit.as_str(), "error": failure })),
            whole: None,
        };
    };
    // Where the list came from, as its walk said it (`Progress::served`), and
    // when a cooldown that had it served from storage lifts: a stored list
    // is handed to the agent as one, with its date, as a person is told it
    // in the warning, never as if it had just been read. `body` is an object
    // for every list (a page), and nothing is indexed into one that is not.
    //
    // **And whether it is all of the list**: a walk cut short by `max_pages`
    // is a success, exit 0, and its `reason` (`page_limit`), with how many
    // accounts it read of how many the counter declared, is the only place
    // that says the list is short. An agent told nothing would give `count`
    // as how many followers the account has.
    if !finished.served.is_empty() && body.is_object() {
        let served: Vec<Value> = finished
            .served
            .iter()
            .map(|said| {
                let mut told = json!({
                    "list": said.get("list"),
                    "from": said.get("from"),
                    "taken_at": said.get("taken_at"),
                });
                if let Some(reason) = said
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|reason| *reason != "completed")
                {
                    told["stopped"] = json!(reason);
                    told["read"] = said.get("accounts").cloned().unwrap_or(Value::Null);
                    told["of"] = said.get("of").cloned().unwrap_or(Value::Null);
                }
                told
            })
            .collect();
        if served.iter().any(|told| told.get("stopped").is_some()) {
            body["complete"] = json!(false);
        }
        let lifts = finished
            .served
            .iter()
            .filter_map(|said| said.get("cooldown_until"))
            .find(|until| until.is_i64());
        if let Some(lifts) = lifts {
            body["cooldown_until"] = lifts.clone();
        }
        body["served"] = Value::Array(served);
    }
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
    let whole = whole_items.map(|items| Whole {
        items,
        about: ABOUT
            .iter()
            .filter_map(|key| Some((key.to_string(), body.get(*key)?.clone())))
            .collect(),
    });
    Answer {
        failed: false,
        body: cleaned(body),
        whole,
    }
}

/// The page at `offset` of a list the server holds (`pages.rs`), as the
/// answer of a call that ran nothing, saying what the first page said of
/// the whole list.
pub fn held(whole: &Whole, offset: usize, limit: Option<usize>) -> Answer {
    let mut body = page(&whole.items, offset, limit, true);
    if let Some(fields) = body.as_object_mut() {
        fields.extend(whole.about.clone());
    }
    Answer {
        failed: false,
        body: cleaned(body),
        whole: None,
    }
}

/// What an agent is not handed of an account: its picture's address, a
/// signed URL of 300 to 500 bytes that answers nothing an agent is asked
/// and would make most of a page.
const LEFT_OUT: [&str; 1] = ["pfp_url"];

/// Each item as its compact JSON, which is what a page is measured in and
/// what the server holds, without the fields [`LEFT_OUT`].
fn compact(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            Value::Object(fields) => {
                let mut fields = fields.clone();
                for key in LEFT_OUT {
                    fields.remove(key);
                }
                Value::Object(fields).to_string()
            }
            other => other.to_string(),
        })
        .collect()
}

/// The page of `items` that starts at `offset`: as many as fit in
/// [`PAGE_BYTES`], at most [`PAGE`] and at least one, so a page always moves
/// on. `{total, count, offset, items, next_offset}`, where `total` is how
/// many the command printed (the whole list, or what a walk cut short read,
/// which `complete: false` says), `count` how many of them the call hands
/// back across its pages (`total`, or `limit` when that is fewer), and
/// `next_offset` is null on the last page, or always when the call cannot
/// be made again for another (`pages` false). An offset past the
/// end is an empty page. **Sliced with `get`, never by indexing**: the offset
/// is the client's number, and a panic here is the whole server gone.
pub fn page(items: &[String], offset: usize, limit: Option<usize>, pages: bool) -> Value {
    let total = items.len();
    let count = limit.map_or(total, |limit| limit.min(total));
    let items = items.get(..count).unwrap_or_default();
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
        "total": total,
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
    let of = match body.get("total").and_then(Value::as_u64) {
        Some(total) if total > count => format!("{count} (the limit; the list has {total})"),
        _ => count.to_string(),
    };
    // A confirmation on a call that asks nothing is refused, and only a
    // list's call may carry one.
    let and = if tool == "snob_list" {
        ", no confirmation,"
    } else {
        ""
    };
    Some(format!(
        "Accounts {first} to {next} of {of}. For the next ones, call {tool} again with \
         the same arguments{and} and \"offset\": {next}; a later page is handed back from \
         what was already read, and sends nothing and asks nothing.",
        first = offset + 1,
    ))
}

/// What an agent is told beside a list that was not read now, **however
/// old it is**: served from storage in a cooldown, or because the check of
/// its counter failed (`engine::cooldown::serve`, `engine::freshness`), or
/// because the call asked for what is stored (`mode: "stored"`, and a later
/// page read from the store), or because its counter moved but it was
/// walked too recently to walk again. A list whose counter was checked now
/// and had not moved was as good as read now, and has no sentence. The
/// moments in UTC: the agent and the person may be in another zone than
/// this machine. `None` for any other answer.
pub fn not_read_now(body: &Value) -> Option<String> {
    let served = body.get("served")?.as_array()?;
    let dated = |wanted: &[&str]| -> Vec<String> {
        served
            .iter()
            .filter(|said| {
                said.get("from")
                    .and_then(Value::as_str)
                    .is_some_and(|from| wanted.contains(&from))
            })
            .filter_map(|said| {
                let list = said.get("list")?.as_str()?;
                let at = said.get("taken_at")?.as_i64()?;
                Some(format!("the {list} list as it was stored at {}", utc(at)))
            })
            .collect()
    };
    let mut said = Vec::new();
    let stale = dated(&["stored_in_cooldown", "stored_after_failed_poll"]);
    if !stale.is_empty() {
        let why = match body.get("cooldown_until").and_then(Value::as_i64) {
            Some(until) => format!(
                "the account is in cooldown until {}, when nothing may be sent",
                utc(until)
            ),
            None => "checking whether it had changed failed".to_string(),
        };
        said.push(format!(
            "Not read now: {why}, so this is {}",
            stale.join(" and ")
        ));
    }
    let asked = dated(&["stored"]);
    if !asked.is_empty() {
        said.push(format!(
            "Read from what is stored, not from Instagram: this is {}",
            asked.join(" and ")
        ));
    }
    let recent = dated(&["stored_walked_recently"]);
    if !recent.is_empty() {
        said.push(format!(
            "Not read now: its counter has moved, but it was walked too recently to walk \
             again, so this is {}",
            recent.join(" and ")
        ));
    }
    if said.is_empty() {
        return None;
    }
    Some(format!(
        "{}, however old that is. Say so, with that date, when you present it.",
        said.join("; ")
    ))
}

/// What an agent is told beside a list its `limit` cut, which then has no
/// next page: that the list has more, so `count` is not read as its size.
/// A page with another after it says so in [`next_page`] instead.
pub fn limited(body: &Value) -> Option<String> {
    if !body.get("next_offset")?.is_null() {
        return None;
    }
    let count = body.get("count")?.as_u64()?;
    let total = body.get("total")?.as_u64()?;
    (total > count).then(|| {
        format!(
            "These are the first {count} accounts, as limit asked: the list has {total}. \
             Call again without limit, or with a higher one, for the rest."
        )
    })
}

/// What an agent is told beside a list that is not all of it: a walk cut
/// short by `max_pages`, or stopped for another reason that still printed
/// what it read. `None` for a whole list.
///
/// **A crossing is told otherwise**: its `count` is what it found among the
/// accounts read of a list cut short, against the whole of the other (a cut
/// list to cross against is refused, `SetOp::require_against_complete`), so
/// some results are missing and the ones given are right, as a person is
/// told it (`commands::sets`). Its body is the one that says where two
/// lists came from.
pub fn incomplete(body: &Value) -> Option<String> {
    if body.get("complete").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let served = body.get("served")?.as_array()?;
    let crossing = served.len() > 1;
    let short: Vec<String> = served
        .iter()
        .filter_map(|said| {
            let list = said.get("list")?.as_str()?;
            let why = match said.get("stopped")?.as_str()? {
                "page_limit" => "at max_pages".to_string(),
                "truncated" => "where Instagram stopped serving pages".to_string(),
                other => format!("early ({other})"),
            };
            let read = said.get("read").and_then(Value::as_u64).unwrap_or(0);
            let of = match said.get("of").and_then(Value::as_u64) {
                Some(of) => format!(" of about {of}"),
                None => String::new(),
            };
            Some(format!(
                "the {list} list stopped {why}, with {read}{of} accounts read"
            ))
        })
        .collect();
    if short.is_empty() {
        return None;
    }
    let count = if crossing {
        "some results are missing and the ones given are right: count is how many were \
         found among the accounts read, not how many there are"
    } else {
        "count is what was read, not how many the account has"
    };
    Some(format!(
        "Not the whole list: {}, so {count}; say so when you present it. Calling snob_list \
         again without max_pages reads the rest.",
        short.join(" and ")
    ))
}

/// A moment in seconds since the epoch, in UTC, as an agent is told it.
fn utc(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || seconds.to_string(),
        |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// The last few lines of what a command said, which is where its sentence
/// about the failure is: the records of the log left out first
/// (`jobs::show::sentences`), which carry what Instagram answered and would
/// push the sentence out, and the lines of `--progress-json`, which a stored
/// read is given for its `served` line.
fn last_lines(said: &str) -> String {
    let lines = crate::jobs::show::sentences(
        said.lines()
            .filter(|line| !line.trim().is_empty())
            .filter(|line| {
                serde_json::from_str::<Value>(line.trim())
                    .ok()
                    .is_none_or(|value| value.get("progress").is_none())
            })
            .collect(),
    );
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
        limited_at(offset, None)
    }

    /// A list's plan given `limit`, for the page at `offset`.
    fn limited_at(offset: usize, limit: Option<usize>) -> Plan {
        Plan {
            page: Some(crate::commands::mcp::tools::Page {
                offset,
                of: "the call".into(),
                held_only: false,
                limit,
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
            ("job_stopping", ExitCode::Error, "snob_job "),
            ("nothing_stored", ExitCode::Error, "mode \"auto\""),
            ("stored_moments", ExitCode::Error, "mode \"auto\""),
            ("budget_wait", ExitCode::Interrupted, "snob_status"),
            ("busy", ExitCode::Error, "snob_job_cancel"),
            ("mcp_write_cap", ExitCode::Error, "retry_at"),
            ("walked_elsewhere", ExitCode::Error, "retry_at"),
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
                Some(json!({"error": {"code": "error", "reason": "refused_argv", "hint": "wait"}})),
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
                assert_eq!(held(&whole, offset, None).body, told.body);
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
        assert_eq!(kept.unwrap().items.len(), 450);
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
    fn a_limit_keeps_the_first_accounts_and_still_says_the_whole_list() {
        // Within one page: no next one, and a sentence saying there are more,
        // so the limit is never read as the list's size.
        let five = from(
            &limited_at(0, Some(5)),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(five.body["total"], 450);
        assert_eq!(five.body["count"], 5);
        assert_eq!(
            usernames(&five.body),
            ["user0", "user1", "user2", "user3", "user4"]
        );
        assert!(five.body["next_offset"].is_null());
        assert!(next_page(&five.body, "snob_list").is_none());
        assert!(limited(&five.body).unwrap().contains("the list has 450"));

        // Across pages: they stop at the limit, and each says both numbers.
        let first = from(
            &limited_at(0, Some(300)),
            finished(ExitCode::Ok, Some(accounts(450)), None),
        );
        assert_eq!(first.body["total"], 450);
        assert_eq!(first.body["count"], 300);
        assert_eq!(first.body["next_offset"], PAGE);
        assert!(limited(&first.body).is_none(), "next_page says it instead");
        assert!(
            next_page(&first.body, "snob_list")
                .unwrap()
                .contains("of 300 (the limit; the list has 450)")
        );
        let whole = first.whole.unwrap();
        assert_eq!(whole.items.len(), 450, "the whole list is held");
        let last = held(&whole, PAGE, Some(300));
        assert_eq!(usernames(&last.body).len(), 300 - PAGE);
        assert!(last.body["next_offset"].is_null());
        assert_eq!(last.body["total"], 450);

        // A limit past the list is no limit, and nothing more to say.
        let all = from(
            &limited_at(0, Some(10)),
            finished(ExitCode::Ok, Some(accounts(3)), None),
        );
        assert_eq!(
            (all.body["total"].clone(), all.body["count"].clone()),
            (json!(3), json!(3))
        );
        assert!(limited(&all.body).is_none());
    }

    #[test]
    fn an_account_is_handed_back_without_its_picture() {
        let printed = json!([{
            "pk": 1,
            "username": "anna",
            "full_name": "Anna",
            "is_private": false,
            "is_verified": false,
            "pfp_url": "https://cdn.example/a.jpg?signed=0123456789",
        }]);
        let told = from(
            &list_at(0),
            finished(ExitCode::Ok, Some(printed.clone()), None),
        );
        let item = &told.body["items"][0];
        assert!(item.get("pfp_url").is_none(), "{item}");
        assert_eq!(item["username"], "anna");
        assert_eq!(item["full_name"], "Anna");
        // What a walk kept before it failed is handed back the same way.
        let failed = from(&list_at(0), finished(ExitCode::Error, Some(printed), None));
        assert!(
            failed.body["error"]["partial"]["items"][0]
                .get("pfp_url")
                .is_none()
        );
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
            let none = Whole {
                items: vec![],
                about: serde_json::Map::new(),
            };
            assert_eq!(
                held(&none, offset, None).body["items"],
                json!([]),
                "{offset}"
            );
        }
    }

    #[test]
    fn a_page_of_long_accounts_is_cut_by_its_size_and_still_moves_on() {
        let long = |n: usize, length: usize| {
            Value::Array(
                (0..n)
                    .map(|n| json!({"pk": n, "full_name": "x".repeat(length)}))
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
            served: Vec::new(),
        }
    }

    /// A dry run in a cooldown exits 5 with its estimate printed, which is the
    /// answer asked for; exit 5 with nothing printed is still a refusal.
    #[test]
    fn an_estimate_in_a_cooldown_is_an_answer_and_a_bare_exit_five_is_not() {
        let dry = plan(&[ExitCode::Ok, ExitCode::RateLimited]);
        let estimate =
            json!({"dry_run": true, "sends_now": false, "cooldown_until": 1_800_000_000});
        let told = from(&dry, finished(ExitCode::RateLimited, Some(estimate), None));
        assert!(!told.failed, "{}", told.body);
        assert_eq!(told.body["sends_now"], false);
        assert_eq!(told.body["exit"], "rate_limited");
        assert_eq!(told.body["cooldown_until"], 1_800_000_000);

        let bare = from(&dry, finished(ExitCode::RateLimited, None, None));
        assert!(bare.failed, "{}", bare.body);
        assert_eq!(bare.body["error"]["code"], "rate_limited");
    }

    /// A list served from storage in a cooldown says so, when it was read and
    /// when the cooldown lifts, in the JSON and in a sentence; a list walked
    /// now says it was, and has no sentence.
    #[test]
    fn a_list_served_in_a_cooldown_says_it_is_stored_and_how_old() {
        let mut stored = finished(ExitCode::Ok, Some(accounts(2)), None);
        stored.served = vec![json!({
            "event": "served",
            "list": "followers",
            "from": "stored_in_cooldown",
            "taken_at": 1_793_340_000,
            "cooldown_until": 1_793_347_200,
            "page": 0,
        })];
        let told = from(&list_at(0), stored);
        assert!(!told.failed);
        assert_eq!(told.body["count"], 2);
        assert_eq!(
            told.body["served"],
            json!([{"list": "followers", "from": "stored_in_cooldown", "taken_at": 1_793_340_000}])
        );
        assert_eq!(told.body["cooldown_until"], 1_793_347_200);
        let said = not_read_now(&told.body).expect("a sentence");
        assert!(said.contains("stored at 2026-10-30 06:00 UTC"), "{said}");
        assert!(
            said.contains("cooldown until 2026-10-30 08:00 UTC"),
            "{said}"
        );

        let mut walked = finished(ExitCode::Ok, Some(accounts(2)), None);
        walked.served = vec![json!({
            "list": "followers",
            "from": "walked",
            "taken_at": 1_793_340_000,
            "cooldown_until": null,
        })];
        let told = from(&list_at(0), walked);
        assert_eq!(told.body["served"][0]["from"], "walked");
        assert!(told.body.get("cooldown_until").is_none());
        assert!(not_read_now(&told.body).is_none());
    }

    /// A walk cut short by `max_pages` exits 0 with what it read, and only
    /// its `served` line says it stopped: the answer says it is not the whole
    /// list, how much of it was read, and so does every later page.
    #[test]
    fn a_walk_cut_short_says_it_is_not_the_whole_list() {
        let mut short = finished(ExitCode::Ok, Some(accounts(250)), None);
        short.served = vec![json!({
            "event": "served",
            "list": "followers",
            "from": "walked",
            "taken_at": 1_793_340_000,
            "cooldown_until": null,
            "reason": "page_limit",
            "page": 21,
            "accounts": 250,
            "of": 2500,
        })];
        let told = from(&list_at(0), short);
        assert!(!told.failed);
        assert_eq!(told.body["complete"], false);
        assert_eq!(told.body["count"], 250);
        assert_eq!(
            told.body["served"],
            json!([{
                "list": "followers", "from": "walked", "taken_at": 1_793_340_000,
                "stopped": "page_limit", "read": 250, "of": 2500,
            }])
        );
        let said = incomplete(&told.body).expect("a sentence");
        assert!(said.contains("stopped at max_pages"), "{said}");
        assert!(said.contains("250 of about 2500 accounts read"), "{said}");
        assert!(not_read_now(&told.body).is_none(), "it was read now");

        // The next page, handed back from what the server holds, says it too.
        let whole = told.whole.expect("a page after this one");
        let later = held(&whole, 200, None);
        assert_eq!(later.body["complete"], false);
        assert_eq!(later.body["served"], told.body["served"]);
        assert!(incomplete(&later.body).is_some());

        // A whole walk says nothing of the kind.
        let mut whole_walk = finished(ExitCode::Ok, Some(accounts(2)), None);
        whole_walk.served = vec![json!({
            "list": "followers", "from": "walked", "taken_at": 1, "reason": "completed",
            "accounts": 2, "of": 2,
        })];
        let told = from(&list_at(0), whole_walk);
        assert!(told.body.get("complete").is_none());
        assert!(told.body["served"][0].get("stopped").is_none());
        assert!(incomplete(&told.body).is_none());
    }

    /// A crossing whose base list was cut short found its count among what
    /// was read, against the whole other list: some results are missing and
    /// the ones given are right, and its count is not what was read.
    #[test]
    fn a_crossing_cut_short_says_results_are_missing() {
        let mut short = finished(ExitCode::Ok, Some(accounts(2)), None);
        short.served = vec![
            json!({
                "list": "following", "from": "walked", "taken_at": 1_793_340_000,
                "reason": "completed", "accounts": 30, "of": 30,
            }),
            json!({
                "list": "followers", "from": "walked", "taken_at": 1_793_340_000,
                "reason": "page_limit", "accounts": 12, "of": 25,
            }),
        ];
        let told = from(&list_at(0), short);
        assert_eq!(told.body["complete"], false);
        assert_eq!(told.body["count"], 2);
        let said = incomplete(&told.body).expect("a sentence");
        assert!(said.contains("12 of about 25 accounts read"), "{said}");
        assert!(said.contains("some results are missing"), "{said}");
        assert!(!said.contains("count is what was read"), "{said}");
    }

    /// What is read from storage because the call asked for it is dated as
    /// any other stored list is.
    #[test]
    fn a_list_read_from_storage_as_asked_is_dated() {
        let mut stored = finished(ExitCode::Ok, Some(accounts(2)), None);
        stored.served = vec![json!({
            "list": "fans", "from": "stored", "taken_at": 1_793_340_000,
            "cooldown_until": null, "reason": "completed",
        })];
        let told = from(&list_at(0), stored);
        assert_eq!(told.body["served"][0]["taken_at"], 1_793_340_000);
        let said = not_read_now(&told.body).expect("a sentence");
        assert!(said.contains("Read from what is stored"), "{said}");
        assert!(said.contains("stored at 2026-10-30 06:00 UTC"), "{said}");
    }

    /// A push-back met by the call itself has started a cooldown that the
    /// failure does not date: the agent is pointed at `snob_status`, in
    /// place of the command's own advice to type it again later. One that
    /// an existing cooldown refused is dated, and keeps its own hint.
    #[test]
    fn a_push_back_met_now_points_at_snob_status() {
        let met = finished(
            ExitCode::RateLimited,
            None,
            Some(json!({"error": {
                "code": "rate_limited",
                "message": "Instagram is throttling requests",
                "cooldown_until": null,
                "hint": null,
            }})),
        );
        let told = from(&list_at(0), met);
        assert!(told.failed);
        assert_eq!(told.body["error"]["hint"], PUSHED_BACK);

        let refused = finished(
            ExitCode::RateLimited,
            None,
            Some(json!({"error": {
                "code": "rate_limited",
                "message": "in cooldown",
                "cooldown_until": 1_793_347_200,
                "hint": "wait",
            }})),
        );
        let told = from(&list_at(0), refused);
        assert_eq!(told.body["error"]["hint"], "wait");
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
            let told = from(
                &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
                finished(ExitCode::Ok, Some(document.clone()), None),
            );
            assert!(!told.failed, "{document}");
            assert_eq!(told.body["value"], cleaned(document.clone()));
            // Under a code the plan counts as an answer but that is not 0,
            // which is where `exit` was set by indexing: no answer without an
            // object, and nothing indexed into the document either way.
            let told = from(
                &plan(&[ExitCode::Ok, ExitCode::RateLimited]),
                finished(ExitCode::RateLimited, Some(document.clone()), None),
            );
            assert!(told.failed, "{document}");
            assert_eq!(told.body["error"]["partial"], cleaned(document.clone()));
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
