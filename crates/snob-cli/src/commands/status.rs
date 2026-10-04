//! `snob status`: where the account stands, read from what is stored here.
//!
//! **Sends nothing and writes nothing.** It is the question somebody asks
//! before a heavy walk, and it must not cost what it reports on: no request,
//! no budget, no browser, and no database brought into being for an account
//! that has none yet.
//!
//! One of the listed exceptions to "commands take an App": an `App` would
//! create the database, refresh the User-Agent and settle the day's retention,
//! each a write. It opens the account's database with
//! [`Store::open_existing`], as `watch status` does, and `shared.db` the same
//! way.

use anyhow::Result;
use serde_json::{Value, json};
use snob_core::model::{ListKind, printable};
use snob_core::{Epoch, EpochMs};
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::SecretStore;
use snob_store::store::rate_budget::{self, BucketState, BudgetState};
use snob_store::store::shared::Shared;
use snob_store::store::{Store, accounts, snapshots, users, watch as watch_store};

use crate::cli::{StatusArgs, StatusSections};
use crate::exit::ExitCode;
use crate::report;

/// Reports on `account`, the account resolved for this run.
pub fn run(
    args: StatusArgs,
    secrets: &SecretStore,
    paths: &AppPaths,
    account: Option<AccountPaths>,
) -> Result<ExitCode> {
    let viewer = report::acting_as().map(|viewer| viewer.json());
    let session_store = account.as_ref().map(|account| secrets.session_of(account));
    let session = match &session_store {
        Some(store) => store.load()?,
        None => None,
    };
    let (Some(session), Some(session_store), Some(account)) = (session, session_store, account)
    else {
        eprintln!("No session stored. Run \"snob login\".");
        if args.output.json {
            crate::ui::say!(
                "{}",
                serde_json::to_string_pretty(&json!({
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

    let now = snob_core::clock::now_ms();
    // An account that has never sent anything has no database, and its
    // budget is at rest: read over an empty one rather than making its file.
    let store = match Store::open_existing(&account)? {
        Some(store) => store,
        None => Store::in_memory()?,
    };
    let shared = Shared::open_existing(paths).unwrap_or_else(|e| {
        tracing::debug!(error = %e, "shared.db could not be read; no brake is reported");
        None
    });
    let budget = rate_budget::state(store.conn(), shared.as_ref(), now)?;
    let held = budget.held_until(now);
    let braked = braked_names(&budget, held, paths);

    let pk = account.pk();
    let sections = args.sections();
    let mut lists = Vec::new();
    if sections.lists {
        let counters = accounts::find(store.conn(), pk)?;
        for kind in [ListKind::Followers, ListKind::Following] {
            lists.push(Stored {
                kind,
                latest: snapshots::latest_complete(store.conn(), pk, kind)?,
                resumable: snapshots::is_resumable(store.conn(), pk, kind)?,
                counter: counters.as_ref().and_then(|c| c.counter(kind)),
                polled_at: counters.as_ref().and_then(|c| c.polled_at),
            });
        }
    }
    let mut runs = Vec::new();
    if sections.watch {
        for run in watch_store::last_runs(store.conn())? {
            let name = users::name(store.conn(), run.account_pk)?;
            runs.push((run, name));
        }
    }

    let found = Found {
        sections,
        session: &session,
        storage: session_store.backend().as_str(),
        budget: &budget,
        held,
        braked: &braked,
        lists: &lists,
        runs: &runs,
        now,
    };
    if args.output.json {
        let mut out = found.json();
        out["viewer"] = viewer.unwrap_or(Value::Null);
        crate::ui::say!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        crate::ui::say!("{}", found.text());
    }

    // The account's state, whatever sections were printed: a script asks
    // "may I send" with any of them.
    Ok(if held.is_some() {
        ExitCode::RateLimited
    } else {
        ExitCode::Ok
    })
}

/// The accounts whose push-backs braked every account, named from the
/// registry, when the brake is what holds this one.
fn braked_names(budget: &BudgetState, held: Option<EpochMs>, paths: &AppPaths) -> Vec<String> {
    let Some(brake) = &budget.brake else {
        return Vec::new();
    };
    if held != Some(brake.until) {
        return Vec::new();
    }
    let registry = Registry::load(paths).unwrap_or_default();
    brake
        .accounts
        .iter()
        .map(|&pk| crate::app::label(pk, registry.get(pk).map(|r| r.username.as_str())))
        .collect()
}

/// A line of the text form: the label padded to a column, then the value.
fn field(label: &str, value: &str) -> String {
    format!("{label:<10} {value}")
}

/// A line inside a section, indented under its heading.
fn row(label: &str, value: &str) -> String {
    format!("  {label:<14} {value}")
}

/// One of the account's own lists, as stored.
struct Stored {
    kind: ListKind,
    latest: Option<snapshots::Snapshot>,
    resumable: bool,
    counter: Option<u64>,
    polled_at: Option<Epoch>,
}

/// Everything read, for the two ways of saying it.
struct Found<'a> {
    sections: StatusSections,
    session: &'a snob_core::session::Session,
    storage: &'a str,
    budget: &'a BudgetState,
    held: Option<EpochMs>,
    braked: &'a [String],
    lists: &'a [Stored],
    runs: &'a [(watch_store::Run, Option<String>)],
    now: EpochMs,
}

impl Found<'_> {
    /// Every value a stable token or a number; moments in seconds, as
    /// `whoami` gives them.
    fn json(&self) -> Value {
        let mut out = serde_json::Map::new();
        if self.sections.session {
            out.insert(
                "session".into(),
                json!({
                    "pk": self.session.ds_user_id,
                    "username": self.session.username,
                    "origin": self.session.origin.as_str(),
                    "storage": self.storage,
                    "created_at": self.session.created_at,
                    "validated_at": self.session.validated_at,
                }),
            );
        }
        if self.sections.budget {
            let b = self.budget;
            let bucket = |s: &BucketState| json!({ "left": s.left, "most": s.most, "free_at": s.free_at.to_epoch() });
            out.insert(
                "budget".into(),
                json!({
                    "requests": bucket(&b.daily),
                    "pace": bucket(&b.pace),
                    "writes": bucket(&b.writes),
                    "next_write_at": b.next_write_at.to_epoch(),
                    "accounts_read": b.accounts_read,
                    "accounts_ceiling": b.accounts_ceiling,
                    "accounts_left": b.accounts_left(),
                }),
            );
        }
        if self.sections.cooldown {
            let last = self.budget.last_cooldown.as_ref().map(|c| {
                json!({
                    "until": c.until.to_epoch(),
                    "set_at": c.set_at.to_epoch(),
                    "reason": c.reason,
                    "strikes": c.strikes,
                    "escalates_until": c.escalates_until().to_epoch(),
                })
            });
            let brake = self
                .budget
                .brake
                .as_ref()
                .map(|b| json!({ "until": b.until.to_epoch(), "accounts": b.accounts }));
            out.insert(
                "cooldown".into(),
                json!({
                    "active": self.held.is_some(),
                    "until": self.held.map(EpochMs::to_epoch),
                    "last": last,
                    "brake": brake,
                }),
            );
        }
        if self.sections.lists {
            let mut lists = serde_json::Map::new();
            for list in self.lists {
                lists.insert(
                    list.kind.as_str().into(),
                    json!({
                        "taken_at": list.latest.as_ref().and_then(|s| s.taken_at),
                        "members": list.latest.as_ref().map(|s| s.member_count),
                        "declared": list.latest.as_ref().and_then(|s| s.declared_count),
                        "resumable": list.resumable,
                        "counter": list.counter,
                        "polled_at": list.polled_at,
                    }),
                );
            }
            out.insert("lists".into(), Value::Object(lists));
        }
        if self.sections.watch {
            let runs: Vec<Value> = self
                .runs
                .iter()
                .map(|(run, name)| {
                    json!({
                        "pk": run.account_pk,
                        "username": name,
                        "at": run.started_at,
                        "outcome": run.outcome.as_ref().map(|o| o.as_str()),
                        "requests": run.requests,
                        "changes": run.changes,
                    })
                })
                .collect();
            out.insert("watch".into(), json!({ "last_runs": runs }));
        }
        Value::Object(out)
    }

    fn text(&self) -> String {
        let mut blocks: Vec<Vec<String>> = Vec::new();
        if self.sections.session {
            let s = self.session;
            let who = match &s.username {
                Some(name) => format!("@{} ({})", printable(name), s.ds_user_id),
                None => s.ds_user_id.to_string(),
            };
            let checked = match s.validated_at {
                Some(at) => format!("last checked {}", report::stored_on(at)),
                None => "never checked".to_string(),
            };
            blocks.push(vec![
                field("Account", &who),
                field(
                    "Session",
                    &format!(
                        "{}, kept in {}; {checked}",
                        match s.origin {
                            snob_core::session::SessionOrigin::Browser => {
                                "signed in through the browser"
                            }
                            snob_core::session::SessionOrigin::Paste => "pasted by hand",
                        },
                        match self.storage {
                            "keyring" => "the system keyring",
                            _ => "a file",
                        }
                    ),
                ),
            ]);
        }
        if self.sections.budget {
            let b = self.budget;
            blocks.push(vec![
                "Budget".to_string(),
                row(
                    "Requests",
                    &format!(
                        "{} of {} without a wait{}",
                        b.daily.left,
                        b.daily.most,
                        self.free_again(&b.daily)
                    ),
                ),
                row(
                    "In a row",
                    &format!(
                        "{} of {}{}",
                        b.pace.left,
                        b.pace.most,
                        self.free_again(&b.pace)
                    ),
                ),
                row(
                    "Writes",
                    &format!(
                        "{} of {} in a row{}",
                        b.writes.left,
                        b.writes.most,
                        // Said only when the next one waits: on the write
                        // bucket, or on the reads a write pays as well.
                        if b.next_write_at > self.now {
                            format!("; the next {}", self.when(b.next_write_at))
                        } else {
                            String::new()
                        }
                    ),
                ),
                row(
                    "Accounts read",
                    &format!(
                        "{} of {} in the last 24 hours",
                        b.accounts_read, b.accounts_ceiling
                    ),
                ),
            ]);
        }
        if self.sections.cooldown {
            let mut lines = vec!["Cooldown".to_string()];
            match self.held {
                Some(until) => {
                    let mut said = report::held_until(&crate::app::Held {
                        until_ms: until,
                        braked: self.braked.to_vec(),
                    });
                    said[..1].make_ascii_uppercase();
                    lines.push(format!("  {said}"));
                }
                None => lines.push("  None; requests may go out".to_string()),
            }
            if let Some(last) = &self.budget.last_cooldown {
                lines.push(row(
                    "The last one",
                    &format!(
                        "{} on {}, strike {}",
                        printable(&last.reason),
                        report::stored_on(last.set_at.to_epoch()),
                        last.strikes
                    ),
                ));
                if self.now < last.escalates_until() {
                    lines.push(format!(
                        "  Another push-back before {} would be longer",
                        report::stored_on(last.escalates_until().to_epoch())
                    ));
                }
            }
            blocks.push(lines);
        }
        if self.sections.lists {
            let mut lines = vec!["Lists".to_string()];
            for list in self.lists {
                let name = match list.kind {
                    ListKind::Followers => "Followers",
                    ListKind::Following => "Following",
                };
                let stored = match &list.latest {
                    Some(s) => format!(
                        "{} stored {}",
                        s.member_count,
                        s.taken_at.map_or("earlier".to_string(), report::stored_on)
                    ),
                    None => "none stored".to_string(),
                };
                let counter = match (list.counter, list.polled_at) {
                    (Some(n), Some(at)) => {
                        format!("; the counter said {n} {}", report::stored_on(at))
                    }
                    _ => String::new(),
                };
                let resume = if list.resumable {
                    "; a walk is left to resume"
                } else {
                    ""
                };
                lines.push(row(name, &format!("{stored}{counter}{resume}")));
            }
            blocks.push(lines);
        }
        if self.sections.watch {
            let mut lines = vec!["Monitor".to_string()];
            if self.runs.is_empty() {
                lines.push("  It has not run from this account".to_string());
            }
            for (run, name) in self.runs {
                let outcome = run.outcome.as_ref().map_or("unfinished", |o| o.as_str());
                lines.push(format!(
                    "  {}  {outcome} {}, {} requests, {} changes",
                    crate::app::label(run.account_pk, name.as_deref()),
                    report::stored_on(run.started_at),
                    run.requests,
                    run.changes
                ));
            }
            blocks.push(lines);
        }
        blocks
            .iter()
            .map(|block| block.join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// When a spent bucket lets the next one through, said after its count.
    fn free_again(&self, bucket: &BucketState) -> String {
        if bucket.left > 0 {
            String::new()
        } else {
            format!("; the next {}", self.when(bucket.free_at))
        }
    }

    /// "now", or the moment.
    fn when(&self, at: EpochMs) -> String {
        if at <= self.now {
            "now".to_string()
        } else {
            report::stored_on(at.to_epoch())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> StatusArgs {
        use clap::Parser;
        let mut argv = vec!["snob", "status"];
        argv.extend_from_slice(flags);
        match crate::cli::Cli::parse_from(argv).command {
            crate::cli::Command::Status(args) => args,
            other => panic!("{other:?}"),
        }
    }

    /// No section named is every section; one named is that one alone.
    #[test]
    fn the_sections_are_all_of_them_or_the_ones_named() {
        let all = args(&[]).sections();
        assert!(all.session && all.budget && all.cooldown && all.lists && all.watch);
        let some = args(&["--budget", "--cooldown"]).sections();
        assert!(some.budget && some.cooldown);
        assert!(!some.session && !some.lists && !some.watch);
    }

    /// The JSON carries only what was asked for.
    #[test]
    fn the_json_carries_the_sections_asked_for() {
        let session = snob_core::session::Session::from_sessionid(
            "42%3AAbCdEfGh%3A20",
            "Mozilla/5.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        let store = Store::in_memory().unwrap();
        let now = snob_core::clock::now_ms();
        let budget = rate_budget::state(store.conn(), None, now).unwrap();
        let found = Found {
            sections: args(&["--budget"]).sections(),
            session: &session,
            storage: "file",
            budget: &budget,
            held: None,
            braked: &[],
            lists: &[],
            runs: &[],
            now,
        };
        let out = found.json();
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["budget"]);
        assert_eq!(out["budget"]["requests"]["left"], 2_001);
        assert_eq!(out["budget"]["accounts_left"], 2_000);
        assert!(found.text().starts_with("Budget"));
    }
}
