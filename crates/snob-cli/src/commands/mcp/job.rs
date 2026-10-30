//! What a job's document is told back to the client as: the walk's answer
//! once it has one, read as a call's own child is read, and otherwise how the
//! job stands and how to ask again.
//!
//! **The server reads a job only through a `snob __job` child** (`tools.rs`,
//! [`tools::Run`]), which prints one JSON document (`jobs::start`,
//! `jobs::show`): the job's record, and the walk's answer under `finished`
//! once it has ended. So the server still opens no store and no job's file
//! (AGENTS.md, "The MCP server"), and a job of another account's is that
//! child's "no such job", never this server's.
//!
//! **A finished job answers as the call would have** ([`finished_of`] and
//! [`plan_of`]): the walk's exit, output and error line go through
//! `answer::from` with the plan of the call that started it, rebuilt from the
//! tool and the arguments the job recorded, so a walk handed back later reads
//! the same as one answered inline, a page at a time.

use serde_json::{Value, json};

use super::answer::{self, Answer};
use super::child::Finished;
use super::tools::{self, Plan};
use crate::exit::ExitCode;

/// The document a `snob __job` child printed: an object with a job's id,
/// from a child that exited 0. Anything else is the child's own failure (no
/// session, no such job, a refusal), handed back as it is.
pub fn document(finished: Finished) -> Result<Value, Finished> {
    let is_one = finished.code == ExitCode::Ok
        && finished
            .json
            .as_ref()
            .and_then(|doc| doc.get("job_id"))
            .is_some_and(Value::is_string);
    match finished.json {
        Some(doc) if is_one => Ok(doc),
        json => Err(Finished { json, ..finished }),
    }
}

/// The walk's answer a job's document carries under `finished`, as a call's
/// child is read (`child.rs`): `None` while the walk has not ended.
pub fn finished_of(doc: &Value) -> Option<Finished> {
    let finished = doc
        .get("finished")
        .filter(|finished| finished.is_object())?;
    let number = finished
        .get("exit_number")
        .and_then(Value::as_i64)
        .and_then(|number| i32::try_from(number).ok());
    let code = match number {
        Some(number) => crate::exit::outcome_of(number).unwrap_or(ExitCode::Error),
        // Killed past its grace after being asked to stop: stopped, as a
        // call's child killed the same way is read.
        None if matches!(
            doc.get("status").and_then(Value::as_str),
            Some("canceled" | "stopped")
        ) =>
        {
            ExitCode::Interrupted
        }
        None => ExitCode::Error,
    };
    // What the walk printed, as `jobs::show::finished` read it: JSON, or the
    // text it was when it was not.
    let (json, text) = match finished.get("output") {
        None | Some(Value::Null) => (None, String::new()),
        Some(Value::String(text)) => (None, text.clone()),
        Some(output) => (Some(output.clone()), String::new()),
    };
    Some(Finished {
        code,
        json,
        text,
        error: finished
            .get("error")
            .filter(|error| error.is_object())
            .cloned(),
        said: finished
            .get("said")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Whether a finished walk's result was swept already (`jobs::sweep`, a day
/// after it ended): it exited 0 and its output is gone.
pub fn result_gone(walk: &Finished) -> bool {
    walk.code == ExitCode::Ok && walk.json.is_none() && walk.text.trim().is_empty()
}

/// The plan of the call that started the job, rebuilt from the tool and the
/// arguments it recorded (`tools::recorded`), for the page at `offset` of its
/// result. A record this build cannot plan again (a job not started by a
/// tool call) is read as a list that exits 0, which is what a job runs.
pub fn plan_of(doc: &Value, offset: usize) -> Plan {
    let rebuilt = doc
        .get("tool")
        .and_then(Value::as_str)
        .zip(doc.get("arguments").and_then(Value::as_object))
        .and_then(|(tool, arguments)| tools::plan(tool, arguments, false).ok());
    let mut plan = rebuilt.unwrap_or(Plan {
        args: Vec::new(),
        ask: None,
        answers: &[ExitCode::Ok],
        spends: tools::Spends::Nothing,
        page: None,
        run: tools::Run::Inline,
    });
    // Not held under anything: the result is the job's file, read again for
    // every page, so `of` names nothing the server keeps.
    plan.page = Some(tools::Page {
        offset,
        of: String::new(),
        held_only: false,
    });
    plan
}

/// The walk's answer, with the job it ran as beside it.
pub fn with_id(mut told: Answer, doc: &Value) -> Answer {
    if let (Some(body), Some(id)) = (
        told.body.as_object_mut(),
        doc.get("job_id").and_then(Value::as_str),
    ) {
        body.insert("job_id".into(), json!(id));
    }
    told
}

/// Whether the job's walk is still to come or going on: queued or running.
pub fn going(doc: &Value) -> bool {
    matches!(
        doc.get("status").and_then(Value::as_str),
        Some("queued" | "running")
    )
}

/// A job that has not ended, or one whose walk never ran: how it stands,
/// and a sentence on what to do next. `cancel` is for the answer to
/// `snob_job_cancel`.
pub fn standing(doc: &Value, cancel: bool) -> (Answer, String) {
    let status = doc.get("status").and_then(Value::as_str).unwrap_or("lost");
    let going = going(doc);
    let id = doc
        .get("job_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let field = |key: &str| doc.get(key).cloned().unwrap_or(Value::Null);
    let mut body = json!({
        "status": status,
        "done": !going,
        "job_id": id,
        "command": field("command"),
        "account": field("account"),
        "created_at": field("created_at"),
        "stop_asked": field("stop_asked"),
        "progress": field("progress"),
        "eta_seconds": field("eta_seconds"),
        "detached": field("detached"),
        "started_by": field("started_by"),
    });
    if let Some(reused) = doc.get("reused") {
        body["reused"] = reused.clone();
    }
    if going {
        body["poll_with"] = json!("snob_job");
    }

    // Lost: its supervisor went away with the walk unfinished. A failure,
    // with the moment the list it held is free again.
    if status == "lost" {
        let body = json!({
            "exit": "error",
            "job_id": id,
            "error": {
                "code": "job_lost",
                "message": "the job's walk stopped without ending (the machine slept or signed \
                            out, or the client that started it closed and took it along); what \
                            it read is stored, and the next walk of the list continues from it",
                "claim_frees_at": field("claim_frees_at"),
            },
        });
        let said = "The job is lost. The list it walked can be read again once its claim frees \
                    (claim_frees_at); a snob_list call before then is refused as \
                    walked_elsewhere."
            .to_string();
        return (
            Answer {
                failed: true,
                body: answer::cleaned(body),
                whole: None,
            },
            said,
        );
    }

    let said = if cancel {
        if going {
            format!(
                "Job {id} was asked to stop and is stopping; what it read is kept. snob_job \
                 tells when it has stopped."
            )
        } else if doc.get("stop_asked") == Some(&Value::Bool(true)) {
            format!(
                "Job {id} has stopped ({status}); what it read is kept, and the next walk of \
                 the list continues from it. snob_job with its job_id gives what it read."
            )
        } else {
            format!(
                "Job {id} had already ended ({status}); snob_job with its job_id gives its result."
            )
        }
    } else if going {
        let left = doc
            .get("eta_seconds")
            .and_then(|eta| Some((eta.get("least")?.as_u64()?, eta.get("most")?.as_u64()?)))
            .map(|(least, most)| {
                format!(
                    ", with about {} to {} left",
                    crate::report::rest_left(std::time::Duration::from_secs(least)),
                    crate::report::rest_left(std::time::Duration::from_secs(most))
                )
            })
            .unwrap_or_default();
        let mut said = format!(
            "The walk goes on as job {id}{left}. Call snob_job with \"job_id\": \"{id}\" to hear \
             how it is going; it waits up to wait_seconds for the walk to end, and then answers \
             with its result. snob_job_cancel stops it and keeps what it read. The same \
             snob_list call made again is answered with this job; another walk of this account \
             is refused while it runs."
        );
        if doc.get("detached").and_then(Value::as_str)
            == Some(crate::jobs::Detached::InClientJob.as_str())
        {
            said.push_str(
                " Closing this client ends the walk: the client keeps the processes it starts \
                 together, and this one could not leave them.",
            );
        }
        said
    } else {
        format!("Job {id} has ended ({status}).")
    };
    (
        Answer {
            failed: false,
            body: answer::cleaned(body),
            whole: None,
        },
        snob_core::model::printable(&said),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job's document as `__job start` and `__job show` print it.
    fn doc(status: &str, finished: Value) -> Value {
        json!({
            "job_id": "j0000000000000000000000000",
            "status": status,
            "command": "followers",
            "account": "someone",
            "tool": "snob_list",
            "arguments": {"list": "followers", "account": "someone"},
            "stop_asked": false,
            "progress": {"event": "page", "accounts": 24, "of": 300},
            "eta_seconds": {"least": 100, "most": 400},
            "detached": "left_job",
            "finished": finished,
        })
    }

    fn child(code: ExitCode, json: Option<Value>) -> Finished {
        Finished {
            code,
            json,
            text: String::new(),
            error: None,
            said: String::new(),
        }
    }

    #[test]
    fn only_a_document_with_a_job_id_from_a_child_that_worked_is_a_jobs() {
        let one = doc("running", Value::Null);
        assert!(document(child(ExitCode::Ok, Some(one.clone()))).is_ok());
        for refused in [
            child(ExitCode::Error, Some(one)),
            child(ExitCode::Ok, Some(json!({"jobs": []}))),
            child(ExitCode::Ok, Some(json!([1, 2]))),
            child(ExitCode::NoSession, None),
        ] {
            let code = refused.code;
            let back = document(refused).unwrap_err();
            assert_eq!(back.code, code, "handed back as it was");
        }
    }

    #[test]
    fn a_finished_walk_is_read_as_a_calls_child_is() {
        let walked = finished_of(&doc(
            "succeeded",
            json!({"exit": "ok", "exit_number": 0, "output": [{"username": "a"}],
                   "error": null, "said": ""}),
        ))
        .unwrap();
        assert_eq!(walked.code, ExitCode::Ok);
        assert_eq!(walked.json, Some(json!([{"username": "a"}])));
        assert!(!result_gone(&walked));

        let throttled = finished_of(&doc(
            "failed",
            json!({"exit": "rate_limited", "exit_number": 5, "output": null,
                   "error": {"error": {"code": "rate_limited"}}, "said": "slow down"}),
        ))
        .unwrap();
        assert_eq!(throttled.code, ExitCode::RateLimited);
        assert_eq!(throttled.error.unwrap()["error"]["code"], "rate_limited");
        assert_eq!(throttled.said, "slow down");

        // Killed past its grace once asked to stop: stopped, not a crash.
        let killed = finished_of(&doc(
            "canceled",
            json!({"exit": "error", "exit_number": null, "output": null}),
        ))
        .unwrap();
        assert_eq!(killed.code, ExitCode::Interrupted);

        // Swept a day after: nothing to hand back, and said so.
        let swept = finished_of(&doc(
            "succeeded",
            json!({"exit": "ok", "exit_number": 0, "output": null}),
        ))
        .unwrap();
        assert!(result_gone(&swept));

        assert!(finished_of(&doc("running", Value::Null)).is_none());
    }

    #[test]
    fn a_finished_job_answers_as_the_call_that_started_it_would_have() {
        let accounts: Vec<Value> = (0..450)
            .map(|n| json!({"pk": n, "username": format!("user{n}")}))
            .collect();
        let doc = doc(
            "succeeded",
            json!({"exit": "ok", "exit_number": 0, "output": accounts, "said": ""}),
        );
        let plan = plan_of(&doc, 200);
        assert!(
            plan.args.contains(&"someone".to_string()),
            "{:?}",
            plan.args
        );
        let told = with_id(answer::from(&plan, finished_of(&doc).unwrap()), &doc);
        assert!(!told.failed);
        assert_eq!(told.body["count"], 450);
        assert_eq!(told.body["offset"], 200);
        assert_eq!(told.body["items"][0]["username"], "user200");
        assert_eq!(told.body["job_id"], "j0000000000000000000000000");

        // A record no call made is still read as a list.
        let bare = json!({"job_id": "j1", "finished": {"exit_number": 0, "output": [1]}});
        let plan = plan_of(&bare, 0);
        assert_eq!(plan.answers, [ExitCode::Ok]);
        assert_eq!(
            answer::from(&plan, finished_of(&bare).unwrap()).body["count"],
            1
        );
    }

    #[test]
    fn a_running_job_says_how_it_stands_and_how_to_ask_again() {
        let (told, said) = standing(&doc("running", Value::Null), false);
        assert!(!told.failed);
        assert_eq!(told.body["status"], "running");
        assert_eq!(told.body["done"], false);
        assert_eq!(told.body["poll_with"], "snob_job");
        assert_eq!(told.body["eta_seconds"]["most"], 400);
        assert!(said.contains("snob_job with \"job_id\": \"j0000"), "{said}");
        assert!(said.contains("about 2 minutes to 7 minutes left"), "{said}");
        assert!(!said.contains("Closing this client"), "{said}");

        let mut held = doc("running", Value::Null);
        held["detached"] = json!("in_client_job");
        let (_, said) = standing(&held, false);
        assert!(said.contains("Closing this client ends the walk"), "{said}");

        let (lost, said) = standing(&doc("lost", Value::Null), false);
        assert!(lost.failed);
        assert_eq!(lost.body["error"]["code"], "job_lost");
        assert!(said.contains("walked_elsewhere"), "{said}");
    }

    #[test]
    fn a_cancel_says_whether_the_walk_has_stopped() {
        let mut asked = doc("running", Value::Null);
        asked["stop_asked"] = json!(true);
        let (told, said) = standing(&asked, true);
        assert_eq!(told.body["done"], false);
        assert!(said.contains("is stopping"), "{said}");

        asked["status"] = json!("canceled");
        let (told, said) = standing(&asked, true);
        assert_eq!(told.body["done"], true);
        assert!(told.body.get("poll_with").is_none());
        assert!(said.contains("has stopped (canceled)"), "{said}");

        let (_, said) = standing(&doc("succeeded", Value::Null), true);
        assert!(said.contains("had already ended (succeeded)"), "{said}");
    }
}
