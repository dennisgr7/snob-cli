//! What a report looks like on the wire.
//!
//! The JSON `--json` prints and the body a webhook receives. Its own module
//! because it is **protocol, not presentation**: `payload` builds the exact
//! string that gets signed and stored, so an edit here is an edit to what a
//! receiver deduplicates on and to what a signature covers, which is not the
//! same kind of change as rewording a sentence. `super::say` is where the
//! sentences live.
//!
//! [`SENT_SCHEMA`] and [`PRINTED_SCHEMA`] are the numbers that say so out
//! loud. Everything a receiver may rely on is in here, in one place, so that
//! "did this change break somebody's integration" is a question about one file.

use snob_core::Epoch;
use snob_core::model::User;
use snob_core::watch::{Basis, Rename};

use crate::app::Viewer;
use crate::engine::watch::{ListReport, Skipped, TickReport, WatchReport, Watched};
use crate::exit::ExitCode;

use super::status::{MarkOf, RunOf};

/// `viewer` is the account the check was run as, when one was chosen.
pub(super) fn check_json(
    report: &crate::engine::check::CheckReport,
    viewer: Option<&Viewer>,
) -> serde_json::Value {
    use crate::engine::check::What;

    serde_json::json!({
        "verdict": report.verdict().as_str(),
        "viewer": viewer.map(Viewer::json),
        "checks": report.checked.iter().map(|checked| {
            let (what, detail) = match &checked.what {
                What::NotConfigured => ("config", serde_json::json!(null)),
                What::Schedule { next } => ("schedule", serde_json::json!({ "next": next })),
                What::Session { viewer, backend } => (
                    "session",
                    serde_json::json!({ "viewer": viewer, "storage": backend }),
                ),
                What::Account { target, pk, followers, following, may_run_unattended } => (
                    "account",
                    serde_json::json!({
                        "target": target,
                        "pk": pk,
                        "followers": followers,
                        "following": following,
                        "may_run_unattended": may_run_unattended,
                    }),
                ),
                What::Webhook { destination, status, signed } => (
                    "webhook",
                    serde_json::json!({
                        "destination": destination,
                        "status": status,
                        "signed": signed,
                    }),
                ),
                What::Baseline { taken_at } => (
                    "baseline",
                    serde_json::json!({
                        "lists": taken_at.iter()
                            .map(|(kind, at)| serde_json::json!({ "kind": kind.as_str(), "taken_at": at }))
                            .collect::<Vec<_>>(),
                    }),
                ),
            };
            serde_json::json!({
                "what": what,
                "verdict": checked.verdict.as_str(),
                "detail": detail,
                // The sentence a person reads, so it comes from where the
                // sentences are, and this and the terminal report agree by
                // construction.
                "problem": checked.problem.as_ref().map(super::say::problem_line),
            })
        }).collect::<Vec<_>>(),
    })
}

/// A report, in the one shape every emitter of one uses: the body a webhook
/// receives, each line of the `--json` stream and `diff --json`.
///
/// Hand-built rather than derived from the report, because this is a contract
/// with whatever is reading it and the struct behind it is not: renaming a
/// field in `ListReport` must not silently rename a key here.
///
/// `run` is what the run says about itself ([`run_json`]); a delivery adds the
/// id a receiver deduplicates on, and nothing else differs between them.
///
/// Four decisions worth knowing about, all of them about what an n8n node
/// actually needs:
///
/// - **`counts` is separate from `events`**, and redundant with the array
///   lengths on purpose. `{{ $json.counts.followers_lost > 0 }}` is the
///   condition people write, and it is far less fragile than an expression over
///   `.length` on a field that may be absent.
/// - **`schema` and `event` are at the top**, so fields can be added later
///   without breaking anybody and a Switch node can tell a heartbeat from a
///   report without looking inside.
/// - **`looked` is not derivable from the arrays.** Empty changes mean "nothing
///   happened" when the run could see and "I could not look" when it could not,
///   and something watching for silence reads those as the same thing.
/// - **`run.lists` is `looked` at the granularity a receiver needs.** `looked`
///   is true when *either* list was read, so a run that walked followers and
///   was refused following says `true` while `lists.following` is `null` and
///   `counts.following_lost` is `0` — indistinguishable from a quiet run, and
///   from an account with no following capture at all. The token is per list
///   and additive, so it moves no `schema`. `lists.<kind>` deliberately stays
///   `null` rather than becoming an object: a receiver testing it against
///   `null` is the shape this shipped with, and there is no version to warn
///   them by.
fn report_json(
    schema: u32,
    event: &str,
    run: serde_json::Value,
    viewer: &Viewer,
    report: &WatchReport,
) -> serde_json::Value {
    let changes = report.changes();

    serde_json::json!({
        "schema": schema,
        "event": event,
        "run": run,
        // Who read it, which on somebody else's account is not `account`.
        "viewer": viewer.json(),
        // The true value, unfiltered: `printable` is for terminals; a machine
        // format has to carry the name that identifies the account, and
        // `serde_json` escapes what it emits.
        "account": {
            "pk": report.account_pk,
            "username": report.username,
            "is_self": report.is_self,
        },
        "lists": {
            "followers": list_json(report.followers.as_ref()),
            "following": list_json(report.following.as_ref()),
        },
        "counts": {
            "followers_gained": changes.followers.gained.len(),
            "followers_lost":   changes.followers.lost.len(),
            "following_gained": changes.following.gained.len(),
            "following_lost":   changes.following.lost.len(),
            "renamed": changes.renamed.len(),
            "total": changes.len(),
        },
        "events": {
            // The accounts are the same `User` that `snob followers --format
            // json` already emits, plus the address: whoever receives this is
            // usually about to put it in a message, and rebuilding the URL at
            // the other end is exactly where somebody pastes a name without
            // encoding it.
            "followers_gained": changes.followers.gained.iter().map(account_json).collect::<Vec<_>>(),
            "followers_lost":   changes.followers.lost.iter().map(account_json).collect::<Vec<_>>(),
            "following_gained": changes.following.gained.iter().map(account_json).collect::<Vec<_>>(),
            "following_lost":   changes.following.lost.iter().map(account_json).collect::<Vec<_>>(),
            "renamed": changes.renamed.iter().map(rename_json).collect::<Vec<_>>(),
        },
    })
}

/// The `run` object, the same in every report and every line of the stream,
/// a failed tick's included, so one reader takes `run.at` off any of them.
fn run_json(at: Epoch, looked: bool, requests: u32, lists: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "at": at,
        "looked": looked,
        "requests": requests,
        "lists": lists,
        "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// A tick's `run`.
///
/// The tick's own moment, not a third reading of the clock. It is what
/// `commit_report` files the mark at and what the `--json` line carries, so
/// one event has one time in all three places.
fn tick_run_json(tick: &TickReport) -> serde_json::Value {
    run_json(
        tick.at(),
        tick.looked(),
        tick.requests,
        run_lists_json(tick),
    )
}

/// The event a report is: news when it holds a change, and otherwise the
/// heartbeat a quiet run is sent as when one is asked for.
///
/// A printed report carries one whether or not anything changed, since it is
/// printed either way; whether a quiet run is sent at all is
/// `delivery::event_for`'s question, and it names the event through here.
pub(super) fn event(has_changes: bool) -> &'static str {
    if has_changes {
        "watch.changes"
    } else {
        "watch.heartbeat"
    }
}

/// What `snob watch status --json` prints.
///
/// Here and not in `status`, for the reason the header of this file gives: it
/// is the output a monitoring system is most likely to parse.
pub(super) fn status_json(
    configured: bool,
    config_path: &std::path::Path,
    owed: &snob_store::store::deliveries::Owed,
    health: &super::status::Health,
    last_runs: &[RunOf],
    marks: &[MarkOf],
) -> serde_json::Value {
    serde_json::json!({
        "configured": configured,
        "config_path": config_path.display().to_string(),
        "deliveries": {
            // Split, because "the next run tries these" and "nothing here can
            // send these" are two facts.
            "waiting": owed.waiting,
            "elsewhere": owed.elsewhere,
            // Not owed -- owing has ended. Deliberately apart from the two
            // above, which count work still to do: this is work that will
            // never be done, and a probe that added it to the queue length
            // would report a backlog that no run can shorten.
            "given_up": owed.given_up,
        },
        // The verdict, so a caller reading this does not have to reimplement
        // which combinations of the fields below mean the monitor has stopped
        // doing its job.
        "health": {
            "verdict": health.verdict.as_str(),
            "notes": health.notes,
        },
        // Told apart from the marks below on purpose. A run that could not
        // look moves no mark, so without this a monitor sitting in a cooldown
        // is indistinguishable from one that was killed.
        // Each row names the account whose database recorded it, which is the
        // account the run read as.
        "last_runs": last_runs.iter().map(|RunOf { run, viewer, .. }| serde_json::json!({
            "viewer": viewer.json(),
            "pk": run.account_pk,
            "at": run.started_at,
            // The token, which is the string this field has always carried —
            // including for a row spelled by a build that is not this one,
            // which is printed back as it was rather than as "unknown".
            "outcome": run.outcome.as_ref().map(|outcome| outcome.as_str()),
            "requests": run.requests,
            "changes": run.changes,
        })).collect::<Vec<_>>(),
        "accounts": marks.iter().map(|MarkOf { mark: m, viewer, .. }| serde_json::json!({
            "viewer": viewer.json(),
            "pk": m.account_pk,
            "kind": m.kind.as_str(),
            "last_reported_at": m.compared_at,
            "has_baseline": m.snapshot_id.is_some(),
        })).collect::<Vec<_>>(),
    })
}

/// A tick's line in the `--json` stream: the body a webhook would be sent for
/// it, less `run.id`.
///
/// No `run.id`, and that is right rather than an omission: an id exists to
/// deduplicate an at-least-once delivery, and a line written once to a local
/// file is not one. The event is the one the body `--heartbeat` asks for would
/// carry, since a line is printed whether or not anything changed.
///
/// **`schema` is here for the same reason it is on the wire.** The README's own
/// recipe is `snob watch --json >> events.ndjson`, which makes this file a data
/// feed with readers of its own; [`PRINTED_SCHEMA`] says why its number is not
/// the body's.
pub(super) fn tick_json(tick: &TickReport) -> serde_json::Value {
    report_json(
        PRINTED_SCHEMA,
        event(tick.report.has_changes()),
        tick_run_json(tick),
        &tick.viewer,
        &tick.report,
    )
}

/// What `snob watch diff --json` prints: the stored report, as a line of the
/// stream would carry it, so one reader takes both.
///
/// Nothing ran, and its `run` says so rather than being left out: `looked` is
/// false and `lists` empty, because no list was read from Instagram,
/// `requests` is 0, and `at` is `now`, the moment it was asked. `viewer` is the
/// account whose database it was read from.
pub(super) fn stored_json(viewer: &Viewer, report: &WatchReport, now: Epoch) -> serde_json::Value {
    report_json(
        PRINTED_SCHEMA,
        event(report.has_changes()),
        run_json(now, false, 0, serde_json::json!([])),
        viewer,
        report,
    )
}

/// Which lists this run read, and which it refused.
///
/// One builder for both streams. A list served during a cooldown, after a
/// failed poll or cut short is dropped before the comparison, so it reaches
/// [`payload`] as `None` and serializes to `null`, the same `null` an account
/// with no capture of that list produces, with `counts.following_lost` at `0`
/// either way. This is what tells a run where the following walk met the
/// truncation wall from a run where nothing happened.
fn run_lists_json(tick: &TickReport) -> serde_json::Value {
    tick.lists
        .iter()
        .map(|l| {
            serde_json::json!({
                "kind": l.kind.as_str(),
                "skipped": l.skipped.map(skipped_token),
            })
        })
        .collect::<Vec<_>>()
        .into()
}

/// The line a tick that failed leaves in the stream.
///
/// So a tick that could not resolve an account, met a mid-walk cooldown, or
/// could not read `history_head` still leaves a line for that interval in a
/// file the README offers as a complete way to use the tool.
///
/// It carries the same `run` object a successful line carries, so one reader
/// can take `run.at` off every line without asking which kind it is, and
/// `error` is what tells the two apart. `error.code` is the vocabulary of the
/// README's exit table and of `watch_runs.outcome`, which is the field worth
/// branching on; `error.message` is the chain, for a person reading the file.
///
/// The name and the message are the true values, unfiltered, for the reason
/// [`report_json`] gives about a username: `printable` is for terminals, a
/// machine format has to carry what identifies the account, and `serde_json`
/// escapes what it emits. Everything drawn at a person still goes through
/// `report::print_error`.
///
/// `viewer` is the account the entry was to be read as; `None` when nobody is
/// signed in to read it as.
pub(super) fn failed_tick_json(
    viewer: Option<&Viewer>,
    watched: &Watched,
    error: &anyhow::Error,
    at: Epoch,
    requests: u32,
) -> serde_json::Value {
    serde_json::json!({
        // The failure line carries it too: the README says every line of the
        // `--json` stream does, and a reader checks it before anything else.
        "schema": PRINTED_SCHEMA,
        "viewer": viewer.map(Viewer::json),
        "account": {
            "username": watched.name(),
            "is_self": watched.name().is_none(),
        },
        "run": run_json(at, false, requests, serde_json::json!([])),
        "error": {
            "code": crate::exit::from_chain(error).unwrap_or(ExitCode::Error).as_str(),
            "message": format!("{error:#}"),
        },
    })
}

/// One JSON line. One object, one line, in both modes.
///
/// Not a mode's decision: a run prints one object per account with nothing
/// wrapping them, so two pretty-printed objects back to back would be in no
/// documented format at all, neither one document nor NDJSON. `--json` is a
/// stream of lines, which is what `snob watch once --json >> events.ndjson` has
/// to mean. What the two modes differ on is whether a tick with no news is
/// printed at all, and that is [`Printing::watching`], where it belongs.
///
/// The fallback is `Value::to_string` rather than a `?` because one call site
/// is reached from the arm already handling a failure: a serializer error on a
/// `Value` built here is not a second failure worth returning instead of the
/// first.
pub(super) fn json_line(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// The version of the shape a receiver is sent: the webhook body and the
/// preflight `snob watch check` posts.
///
/// One number for both, in one place: separate literals could tell a receiver
/// a report is schema 1 and a preflight schema 2 for the same release.
///
/// It moves when a field is removed or its meaning changes, and not when one is
/// added: additive is what lets a receiver keep working, and `run.lists`
/// arriving beside `counts` did not move it.
pub(super) const SENT_SCHEMA: u32 = 1;

/// The version of the shape this tool prints: every line of the `--json`
/// stream, a failed tick's included, and `diff --json`.
///
/// Its own number, because what is printed and what is sent are separate
/// contracts with readers of their own, even though a line is the body less
/// `run.id`: each number moves, by the rule [`SENT_SCHEMA`] gives, only when
/// its own output loses a field or changes what one means.
pub(super) const PRINTED_SCHEMA: u32 = 2;

/// What goes on the wire: the report, and the id a receiver deduplicates on.
///
/// A contract with whatever is on the other end, so it is built here by hand
/// and asserted in a test: this is the one output of the tool that a stranger's
/// automation branches on, and a field renamed by accident breaks a workflow
/// somebody built months ago. [`report_json`] is the shape and the decisions
/// behind it.
pub(super) fn payload(tick: &TickReport, run_id: &str, event: &str) -> serde_json::Value {
    let mut body = report_json(
        SENT_SCHEMA,
        event,
        tick_run_json(tick),
        &tick.viewer,
        &tick.report,
    );
    body["run"]["id"] = serde_json::json!(run_id);
    body
}

/// What `snob watch check` posts.
///
/// The same skeleton [`payload`] uses — `schema`, `event`, and the id and the
/// moment under `run` — so a receiver can branch on it exactly as it branches
/// on the rest.
///
/// `looked`, `requests` and `lists` are deliberately absent: this run looked at
/// nothing and spent nothing on the account, and a `false` there would read as
/// a report that could not see rather than as a message that is not a report.
/// The `note` says so in words, for whoever opens one by hand.
pub(super) fn preflight_body(run_id: &str, at: Epoch) -> serde_json::Value {
    serde_json::json!({
        "schema": SENT_SCHEMA,
        "event": crate::engine::check::PREFLIGHT_EVENT,
        "run": {
            "id": run_id,
            "at": at,
            "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
        },
        "note": "snob watch check: this is not a report, and nothing is queued",
    })
}

/// One account, as an automation wants it.
fn account_json(user: &User) -> serde_json::Value {
    let mut value = serde_json::to_value(user).unwrap_or(serde_json::Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "profile_url".to_string(),
            serde_json::Value::String(user.profile_url()),
        );
    }
    value
}

/// The stable name of why a list was left out.
fn skipped_token(skipped: Skipped) -> &'static str {
    match skipped {
        Skipped::NobodyLooked(_) => "not_verified",
        Skipped::Incomplete(..) => "incomplete",
    }
}

fn list_json(report: Option<&ListReport>) -> serde_json::Value {
    let Some(report) = report else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        // A stable token, so a caller can tell "nothing changed" from "this is
        // the first look" without reading a sentence.
        "basis": basis_token(report.basis),
        "since": report.since,
        "until": report.until,
        "count": report.total,
    })
}

fn rename_json(rename: &Rename) -> serde_json::Value {
    serde_json::json!({
        "pk": rename.pk,
        "from": rename.from,
        "to": rename.to,
        "changed_at": rename.at,
    })
}

/// The stable name of what a list's report is. Written out here rather than on
/// [`Basis`] because it is vocabulary aimed at a caller, and the domain does
/// not decide how it is spelled.
fn basis_token(basis: Basis) -> &'static str {
    match basis {
        Basis::Baseline { .. } => "baseline",
        Basis::Unchanged { .. } => "unchanged",
        Basis::Compare { .. } => "compared",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::{list, report_with, user};
    use crate::engine::Provenance;
    use crate::engine::watch::TickList;
    use snob_core::Pk;
    use snob_core::model::ListKind;
    use snob_core::watch::{Basis, ListDiff, Rename};

    /// The tokens are what a caller branches on, so they are asserted rather
    /// than left to whatever the enum happens to be called.
    #[test]
    fn the_json_carries_stable_tokens_for_each_basis() {
        for (basis, token) in [
            (Basis::Baseline { snapshot_id: 1 }, "baseline"),
            (Basis::Unchanged { snapshot_id: 1 }, "unchanged"),
            (
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                "compared",
            ),
        ] {
            assert_eq!(basis_token(basis), token);
        }
    }

    /// Where the README publishes the body, it has to be the body: a receiver is
    /// written from the document, and `run.id` and `run.at` are what the
    /// surrounding prose depends on.
    ///
    /// It compares keys and not values, because the block is an example and
    /// abbreviates the arrays. What it may not do is name a key the payload does
    /// not emit, or leave one out of `run`.
    ///
    /// The version inside `tool` is deliberately not asserted: doing that makes
    /// every release a README edit.
    #[test]
    fn the_readme_publishes_the_body_that_goes_out() {
        // Walks up from this crate until a `Cargo.lock` shows up, the way the
        // source-reading guards in `snob-core` do, and answers nothing from a
        // packaged build where there is no repository to read.
        let Some(root) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|d| d.join("Cargo.lock").is_file())
        else {
            return;
        };

        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        // A README that shows no payload publishes nothing to keep in step.
        if !readme.contains("```json") {
            return;
        }
        assert_eq!(
            readme.matches("```json").count(),
            1,
            "the payload is the only JSON block, and this test takes the first"
        );
        let block = readme
            .split("```json")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the README shows the payload");
        let published: serde_json::Value =
            serde_json::from_str(block).expect("the README's example has to be JSON");

        let real = payload(
            &TickReport::for_test(report_with(None, vec![]), 14, Epoch::new(1_700_000_000)),
            "run-1",
            "watch.changes",
        );

        let keys = |value: &serde_json::Value| {
            let mut names: Vec<String> = value
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            names
        };
        assert_eq!(
            keys(&published["run"]),
            keys(&real["run"]),
            "the `run` object is published in full or not at all"
        );

        // One direction only: the example abbreviates, and it is allowed to.
        // What it may not do is publish a field nothing sends.
        fn only_real_keys(published: &serde_json::Value, real: &serde_json::Value, path: &str) {
            let (Some(published), Some(real)) = (published.as_object(), real.as_object()) else {
                return;
            };
            for (key, value) in published {
                let here = format!("{path}.{key}");
                let counterpart = real.get(key).unwrap_or_else(|| {
                    panic!("the README publishes \"{here}\", which is not sent")
                });
                only_real_keys(value, counterpart, &here);
            }
        }
        only_real_keys(&published, &real, "");

        // Left out, a receiver serving several accounts would be written
        // without the one field that says whose run it was.
        assert!(
            published.get("viewer").is_some(),
            "the README shows who read the account"
        );
    }

    /// Every message this tool emits can be version-checked, and the two that
    /// go to a receiver put the id and the moment in the same place.
    ///
    /// The `--json` stream is the third emitter, under a number of its own. It
    /// carries no `run.id`, and that is right rather than an omission: an id
    /// exists to deduplicate an at-least-once delivery, and a line written once
    /// to a local file is not one.
    #[test]
    fn every_message_carries_the_schema() {
        let report = payload(
            &TickReport::for_test(report_with(None, vec![]), 0, Epoch::new(1_700_000_000)),
            "run-1",
            "watch.changes",
        );
        let preflight = preflight_body("run-2", Epoch::new(1_700_000_000));
        let streamed = tick_json(&TickReport::for_test(
            report_with(None, vec![]),
            0,
            Epoch::new(1_700_000_000),
        ));

        for (which, message, schema) in [
            ("report", &report, 1),
            ("preflight", &preflight, 1),
            ("stream line", &streamed, 2),
        ] {
            assert_eq!(
                message["schema"], schema,
                "{which} cannot be version-checked"
            );
            assert_eq!(
                message["run"]["at"], 1_700_000_000,
                "{which} does not say when"
            );
            assert!(
                message.get("at").is_none(),
                "{which} still has the moment at the top level"
            );
        }

        for (which, message) in [("report", &report), ("preflight", &preflight)] {
            assert!(
                message["event"].as_str().is_some(),
                "{which} has nothing for a Switch node to read"
            );
            assert!(
                message["run"]["id"].as_str().is_some(),
                "{which} does not say which run it is"
            );
            assert!(
                message.get("run_id").is_none(),
                "{which} still has the id at the top level"
            );
        }

        // And the name is one constant, so the header and the body cannot come
        // to disagree.
        assert_eq!(preflight["event"], crate::engine::check::PREFLIGHT_EVENT);
        assert_eq!(
            preflight["event"], "watch.preflight",
            "the name on the wire"
        );
    }

    /// A refused list is not a quiet one, and the body has to say which it was.
    ///
    /// `tick` drops a list it could not verify before the comparison, so it
    /// reaches `payload` as `None` and `list_json` turns it into `null` — the
    /// same `null` an account with no capture of that list produces, with
    /// `counts.following_lost` at `0` in both.
    ///
    /// `run.looked` cannot resolve it, and it is asserted equal here to say so:
    /// it is `any`, not `all`, so a run that read followers and was refused
    /// following reports `true`. Which list is the question.
    #[test]
    fn a_refused_list_is_not_reported_as_a_quiet_one() {
        let body = |skipped| {
            let mut tick = TickReport::for_test(
                report_with(
                    Some(list(
                        Basis::Compare {
                            before: 1,
                            after: 2,
                        },
                        ListDiff {
                            gained: vec![user(Pk::new(1), "arrived")],
                            lost: vec![],
                        },
                        Some(Epoch::new(1_000)),
                    )),
                    vec![],
                ),
                7,
                Epoch::new(1_700_000_000),
            );
            tick.lists = vec![
                TickList {
                    kind: ListKind::Followers,
                    skipped: None,
                },
                TickList {
                    kind: ListKind::Following,
                    skipped,
                },
            ];
            payload(&tick, "run-1", "watch.changes")
        };

        let refused = body(Some(Skipped::NobodyLooked(Provenance::PollFailed)));
        let read = body(None);

        // The arrays cannot tell them apart and neither can the counts.
        assert_eq!(refused["lists"], read["lists"]);
        assert_eq!(refused["counts"], read["counts"]);
        assert_eq!(
            refused["run"]["looked"], read["run"]["looked"],
            "`looked` is `any`, so it says `true` for both"
        );

        assert_ne!(
            refused["run"]["lists"], read["run"]["lists"],
            "a receiver has no field to read the refusal from"
        );
        assert_eq!(refused["run"]["lists"][1]["kind"], "following");
        assert_eq!(refused["run"]["lists"][1]["skipped"], "not_verified");
        assert_eq!(
            read["run"]["lists"][1]["skipped"],
            serde_json::Value::Null,
            "a list that was read carries no refusal"
        );
    }

    /// An event line says when it happened. The lists' own moments go away
    /// with a list that is refused, so without `run.at` every line of a
    /// cooldown would be byte-identical, and the file could not be queried by
    /// time, windowed or deduplicated.
    #[test]
    fn an_event_line_says_when_it_happened() {
        let quiet_run_at = |at| {
            let mut tick = TickReport::for_test(report_with(None, vec![]), 0, at);
            tick.lists = vec![TickList {
                kind: ListKind::Followers,
                skipped: Some(Skipped::NobodyLooked(Provenance::Cooldown)),
            }];
            tick_json(&tick)
        };

        let first = quiet_run_at(Epoch::new(1_700_000_000));
        let second = quiet_run_at(Epoch::new(1_700_021_600));

        assert_eq!(first["run"]["at"], 1_700_000_000);
        assert_ne!(
            first, second,
            "six hours apart and the same bytes: nothing in the file can date a run"
        );

        // The moment is the tick's own, so the file and whatever the webhook
        // delivered can be joined on it.
        let tick = TickReport::for_test(report_with(None, vec![]), 0, Epoch::new(1_700_000_000));
        assert_eq!(
            tick_json(&tick)["run"]["at"],
            payload(&tick, "run-1", "watch.changes")["run"]["at"],
            "one event, one moment"
        );
    }

    /// A failed tick leaves a line in the stream, not only an English
    /// paragraph on standard error that a consumer of the file is not reading.
    ///
    /// The line has to be readable by the same reader the successful lines
    /// have, which is why `run` is the same object. `error` is what tells them
    /// apart, and a successful line must not have one.
    #[test]
    fn a_failed_tick_leaves_a_line_in_the_stream() {
        let error = anyhow::anyhow!("@friend's account is private");
        let line = failed_tick_json(
            None,
            &Watched::consented("friend".into(), crate::engine::watch::Consent),
            &error,
            Epoch::new(1_700_000_000),
            1,
        );

        assert_eq!(
            line["run"]["at"], 1_700_000_000,
            "the gap in the file has to be datable, which is the whole of it"
        );
        assert_eq!(line["run"]["looked"], false);
        assert_eq!(line["run"]["requests"], 1, "the poll was charged");
        assert_eq!(line["account"]["username"], "friend");
        assert_eq!(
            line["error"]["code"], "error",
            "the vocabulary of the exit table, not free text"
        );

        // And the two kinds of line are told apart by the field itself, not by
        // what is missing from the rest of the object.
        let ok = tick_json(&TickReport::for_test(
            report_with(None, vec![]),
            0,
            Epoch::new(1_700_000_000),
        ));
        assert!(
            ok.get("error").is_none(),
            "a run that worked must not look like one that failed"
        );
        assert_eq!(line["schema"], ok["schema"], "one stream, one number");
        let keys = |run: &serde_json::Value| {
            run.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(
            keys(&line["run"]),
            keys(&ok["run"]),
            "the same `run`, so one reader takes it off every line"
        );

        // One object, one line, in both modes. `run_accounts` prints one per
        // account with nothing wrapping them, so a line laid out over several
        // of them stops parsing the moment a second account is watched.
        assert!(!json_line(&line).contains('\n'));
        assert!(
            !json_line(&ok).contains('\n'),
            "the successful line is a line too"
        );
    }

    /// A line of the stream is the body a webhook is sent, less the id: one
    /// shape, so one reader takes both, and the event is the one `--heartbeat`
    /// would send when nothing changed.
    #[test]
    fn a_line_is_the_body_without_its_id() {
        let changed = TickReport::for_test(
            report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff {
                        gained: vec![user(Pk::new(1), "arrived")],
                        lost: vec![],
                    },
                    Some(Epoch::new(1_000)),
                )),
                vec![],
            ),
            3,
            Epoch::new(1_700_000_000),
        );
        let quiet = TickReport::for_test(report_with(None, vec![]), 1, Epoch::new(1_700_000_000));

        for (tick, event) in [(&changed, "watch.changes"), (&quiet, "watch.heartbeat")] {
            let mut body = payload(tick, "run-1", event);
            let id = body["run"].as_object_mut().and_then(|run| run.remove("id"));
            assert_eq!(id, Some(serde_json::json!("run-1")));
            body["schema"] = serde_json::json!(PRINTED_SCHEMA);

            assert_eq!(tick_json(tick), body);
        }
    }

    /// `diff --json` prints what is stored in the stream's shape, and its `run`
    /// says that nothing was looked at or spent rather than being left out.
    #[test]
    fn a_stored_report_is_printed_in_the_streams_shape() {
        let report = report_with(
            Some(list(
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                ListDiff {
                    gained: vec![],
                    lost: vec![user(Pk::new(2), "left")],
                },
                Some(Epoch::new(1_000)),
            )),
            vec![],
        );
        let viewer = Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        };
        let stored = stored_json(&viewer, &report, Epoch::new(1_700_000_000));

        let mut tick = TickReport::for_test(report, 0, Epoch::new(1_700_000_000));
        tick.viewer = viewer;
        let line = tick_json(&tick);
        let keys = |value: &serde_json::Value| {
            value
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(keys(&stored), keys(&line), "one shape");
        assert_eq!(keys(&stored["run"]), keys(&line["run"]));

        assert_eq!(stored["schema"], 2);
        assert_eq!(stored["event"], "watch.changes");
        assert_eq!(stored["counts"]["followers_lost"], 1);
        assert_eq!(stored["events"]["followers_lost"][0]["username"], "left");
        assert_eq!(stored["run"]["at"], 1_700_000_000);
        assert_eq!(stored["run"]["looked"], false, "nothing was read");
        assert_eq!(stored["run"]["requests"], 0, "nothing was spent");
        assert_eq!(stored["run"]["lists"], serde_json::json!([]));
    }

    /// The shape a stranger's automation branches on, pinned to a literal.
    ///
    /// This is the one output of the tool that somebody else's workflow reads,
    /// and a key renamed by accident breaks something built months ago with no
    /// error anywhere. Comparing against a literal means a change to the
    /// contract has to be a change somebody made on purpose.
    ///
    /// Both of the fields that move in production are arguments here: `run.id`
    /// is random and `run.at` is the tick's own moment, so the literal can
    /// carry them rather than the comparison having to skip them.
    #[test]
    fn the_payload_has_the_shape_a_receiver_was_promised() {
        let tick = TickReport::for_test(
            report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff {
                        gained: vec![user(Pk::new(1), "arrived")],
                        lost: vec![user(Pk::new(2), "left")],
                    },
                    Some(Epoch::new(1_000)),
                )),
                vec![Rename {
                    pk: Pk::new(7),
                    history_id: 7,
                    from: "before".into(),
                    to: "after".into(),
                    at: Epoch::new(1_500),
                }],
            ),
            14,
            Epoch::new(1_700_000_000),
        );

        let payload = payload(&tick, "run-1", "watch.changes");

        assert_eq!(
            payload,
            serde_json::json!({
                "schema": 1,
                "event": "watch.changes",
                "run": {
                    "id": "run-1",
                    "at": 1_700_000_000,
                    "looked": false,
                    "requests": 14,
                    "lists": [],
                    "tool": { "name": "snob", "version": env!("CARGO_PKG_VERSION") },
                },
                "viewer": { "pk": 42, "username": "me" },
                "account": { "pk": 42, "username": "me", "is_self": true },
                "lists": {
                    "followers": {
                        "basis": "compared",
                        "since": 1_000,
                        "until": 2_000,
                        "count": 10,
                    },
                    "following": null,
                },
                "counts": {
                    "followers_gained": 1,
                    "followers_lost": 1,
                    "following_gained": 0,
                    "following_lost": 0,
                    "renamed": 1,
                    "total": 3,
                },
                "events": {
                    "followers_gained": [{
                        "pk": 1,
                        "username": "arrived",
                        "profile_url": "https://www.instagram.com/arrived/",
                    }],
                    "followers_lost": [{
                        "pk": 2,
                        "username": "left",
                        "profile_url": "https://www.instagram.com/left/",
                    }],
                    "following_gained": [],
                    "following_lost": [],
                    "renamed": [{
                        "pk": 7,
                        "from": "before",
                        "to": "after",
                        "changed_at": 1_500,
                    }],
                },
            })
        );
    }

    /// Every line and body names the account it was read as, which on
    /// somebody else's account is not `account`: one webhook carries the
    /// reports of every viewer.
    #[test]
    fn every_report_names_the_account_it_was_read_as() {
        let mut report = report_with(None, vec![]);
        report.account_pk = Pk::new(7);
        report.username = Some("friend".into());
        report.is_self = false;
        let mut tick = TickReport::for_test(report, 0, Epoch::new(1_700_000_000));
        tick.viewer = Viewer {
            pk: Pk::new(42),
            username: Some("me".into()),
        };

        let reader = serde_json::json!({ "pk": 42, "username": "me" });
        assert_eq!(payload(&tick, "run-1", "watch.changes")["viewer"], reader);
        assert_eq!(tick_json(&tick)["viewer"], reader);
        assert_eq!(tick_json(&tick)["account"]["pk"], 7);

        let friend = Watched::consented("friend".into(), crate::engine::watch::Consent);
        let error = anyhow::anyhow!("skipped this run");
        let at = Epoch::new(1_700_000_000);
        assert_eq!(
            failed_tick_json(Some(&tick.viewer), &friend, &error, at, 0)["viewer"],
            reader
        );
        assert!(
            failed_tick_json(None, &friend, &error, at, 0)["viewer"].is_null(),
            "nobody was signed in to read it as"
        );

        let checked = crate::engine::check::CheckReport::default();
        assert_eq!(check_json(&checked, Some(&tick.viewer))["viewer"], reader);
        assert!(check_json(&checked, None)["viewer"].is_null());
    }
}
