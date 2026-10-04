//! `--dry-run` on the list commands: what the walks would cost, against what
//! today has left, said and not done.
//!
//! **Nothing is sent, nobody is asked, and nothing stored is changed.** The
//! account is not resolved over the network, so the consent a stranger's
//! lists need is not asked either, as `--offline` does not ask it: nothing is
//! enumerated. The estimate is `engine::estimate`'s, out of what is stored
//! here.
//!
//! One of the listed exceptions to "commands take an App", for the reason
//! `snob status` is: an `App` creates the database, refreshes the User-Agent
//! and settles the day's retention, each a write, and builds a client. This
//! reads the session, the account's database and `shared.db` the way `status`
//! does, and asks whether a client would send from the browser without
//! building one.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use snob_core::model::ListKind;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SessionStore;
use snob_store::store::Store;
use snob_store::store::rate_budget;
use snob_store::store::shared::Shared;

use crate::app::Viewer;
use crate::cli::{Format, OutputArgs, WalkArgs};
use crate::commands::common;
use crate::engine::estimate::{self, Estimate, Fate, Standing};
use crate::exit::ExitCode;
use crate::output::{self, Rendered};
use crate::report;

/// Estimates the walks of `kinds`, in the order the command makes them, of
/// `target` or of the viewer, and prints the estimate where the result would
/// have gone. Exits 5 in a cooldown, as `snob status` does.
pub fn run(
    walk: &WalkArgs,
    target: &Option<String>,
    output: &OutputArgs,
    secrets: &SessionStore,
    paths: &AccountPaths,
    kinds: &[ListKind],
) -> Result<ExitCode> {
    // Before anything is read: an estimate is said, or it is an object.
    let destination = output.path.as_deref();
    let format = output::effective_format(output.format, destination);
    if matches!(format, Format::Csv | Format::Xlsx | Format::Md) {
        return Err(anyhow!(
            "an estimate has no {} form; it can be a table (the sentences), json or ndjson",
            format!("{format:?}").to_ascii_lowercase()
        ));
    }

    let Some(session) = secrets.load()? else {
        return Err(common::no_session());
    };
    if session.ds_user_id != paths.pk() {
        anyhow::bail!(
            "the session stored for account {} is account {}'s; log in again with \
             \"snob login\"",
            paths.pk(),
            session.ds_user_id
        );
    }
    let viewer = Viewer {
        pk: session.ds_user_id,
        username: session.username.clone(),
    };
    report::act_as(viewer.clone());

    // An account that has never sent anything has no database, and its
    // budget is at rest: read over an empty one rather than making its file.
    let store = match Store::read_existing(paths)? {
        Some(store) => store,
        None => Store::in_memory()?,
    };
    let shared = Shared::read_existing(paths).unwrap_or_else(|e| {
        tracing::debug!(error = %e, "shared.db could not be read; no brake is counted");
        None
    });
    let now = snob_core::clock::now_ms();
    let budget = rate_budget::state(store.conn(), shared.as_ref(), now)?;
    let standing = Standing {
        viewer: &viewer,
        store: &store,
        browser: snob_ig::client::page::a_client_would_use_a_page(),
        budget: &budget,
        held: budget.held_until(now),
        now,
    };
    let query = common::query(target, walk);
    let estimate = estimate::compute(&estimate::gather(&standing, &query, kinds)?);
    let subject = crate::engine::target::label_for(&viewer, target.as_deref());

    let rendered = match format {
        Format::Json | Format::Ndjson => {
            let mut out = as_json(&estimate);
            out["viewer"] = viewer.json();
            let text = if format == Format::Json {
                serde_json::to_string_pretty(&out)?
            } else {
                serde_json::to_string(&out)?
            };
            Rendered::Text(format!("{text}\n"))
        }
        _ => Rendered::Text(format!("{}\n", as_text(&estimate, &subject))),
    };
    output::write_rendered(&rendered, destination)?;
    Ok(if estimate.held.is_some() {
        ExitCode::RateLimited
    } else {
        ExitCode::Ok
    })
}

fn fate_token(fate: Fate) -> &'static str {
    match fate {
        Fate::ReusedUnlessMoved => "reused_unless_moved",
        Fate::Walked => "walked",
        Fate::Unknown => "unknown",
        Fate::Refused => "refused",
        Fate::NotReached => "not_reached",
    }
}

fn as_json(estimate: &Estimate) -> Value {
    let lists: Vec<Value> = estimate
        .lists
        .iter()
        .map(|list| {
            json!({
                "list": list.kind.as_str(),
                "fate": fate_token(list.fate),
                "size": list.size,
                "taken_at": list.taken_at,
                "to_read": list.to_read,
                "pages": { "least": list.pages.0, "most": list.pages.1 },
                "requests": { "least": list.requests.0, "most": list.requests.1 },
                "truncated": list.truncated,
            })
        })
        .collect();
    json!({
        "dry_run": true,
        "to_find": estimate.to_find,
        "lists": lists,
        "requests": { "least": estimate.requests.0, "most": estimate.requests.1 },
        "accounts": estimate.accounts,
        "seconds": { "least": estimate.seconds.0, "most": estimate.seconds.1 },
        "unknown_pages": estimate.unknown(),
        "requests_left": estimate.requests_left,
        "accounts_left": estimate.accounts_left,
        "pauses": estimate.pauses,
        // A run now sends nothing while this stands; the cost above is what
        // the walks take once it has ended.
        "sends_now": estimate.held.is_none(),
        "cooldown_until": estimate.held.map(|until| until.to_epoch_not_before()),
    })
}

fn as_text(estimate: &Estimate, subject: &str) -> String {
    let mut lines = vec![format!("A dry run for {subject}: nothing was sent.")];
    lines.push(match estimate.to_find {
        0 => "Finding the account: nothing, it is yours.".to_string(),
        n => format!(
            "Finding the account and opening its profile: {}.",
            requests(n)
        ),
    });
    for list in &estimate.lists {
        let name = match list.kind {
            ListKind::Followers => "Followers",
            ListKind::Following => "Following",
        };
        let walk = format!(
            "about {} accounts to read, {}",
            list.to_read,
            pages(list.pages)
        );
        lines.push(match list.fate {
            Fate::ReusedUnlessMoved => format!(
                "{name}: stored {}; {} if its count has not moved, {} if it has ({walk}).",
                list.taken_at.map_or("earlier".into(), report::stored_on),
                requests(list.requests.0),
                requests(list.requests.1)
            ),
            Fate::Walked => format!("{name}: a walk, {walk}; {}.", range(list.requests)),
            Fate::Unknown => format!(
                "{name}: never read here, so its size is not known; a walk of every page it \
                 has, {} for its first.",
                requests(list.requests.0)
            ),
            Fate::Refused => format!(
                "{name}: another snob is walking it right now; a run would spend {} and stop \
                 rather than walk it twice.",
                requests(list.requests.0)
            ),
            Fate::NotReached => {
                format!("{name}: not reached, since the run stops at the list before it.")
            }
        });
        if list.truncated {
            lines.push(format!(
                "  Past {} pages a walk stops there as truncated, and a list cut short is \
                 not crossed against.",
                snob_ig::pager::HARD_PAGE_CAP
            ));
        }
    }
    let unknown = if estimate.unknown() {
        ", and the pages of the lists not known here"
    } else {
        ""
    };
    lines.push(format!(
        "In all: {}{unknown}, up to {} accounts read, in {}.",
        range(estimate.requests),
        estimate.accounts,
        span(estimate.seconds)
    ));
    lines.push(format!(
        "Today has {} requests and {} accounts left.",
        estimate.requests_left, estimate.accounts_left
    ));
    if estimate.requests.1 > estimate.requests_left {
        lines.push(format!(
            "More requests than that: past it the day's budget lets one through every {} \
             seconds, which the time above counts.",
            rate_budget::DAILY_EMISSION_MS / 1000
        ));
    }
    if estimate.pauses {
        lines.push(format!(
            "More accounts than that: {}; the time above does not count the pause.",
            report::PAUSING_BY_DEFAULT
        ));
    } else if estimate.same_day && estimate.accounts > estimate.accounts_left {
        lines.push(
            "More accounts than that, and --same-day reads past them: only the request \
             budget stops it."
                .to_string(),
        );
    }
    if let Some(until) = estimate.held {
        lines.push(format!(
            "The account is in cooldown until {}: a run now sends nothing and exits 5; the \
             cost above is once it ends.",
            report::stored_on(until.to_epoch_not_before())
        ));
    }
    lines.join("\n")
}

fn requests(n: u64) -> String {
    match n {
        1 => "1 request".to_string(),
        n => format!("{n} requests"),
    }
}

/// "12 requests", or "12 to 18 requests".
fn range((least, most): (u64, u64)) -> String {
    if least == most {
        requests(most)
    } else {
        format!("{least} to {}", requests(most))
    }
}

/// "10 pages", or "10 to 15 pages".
fn pages((least, most): (u64, u64)) -> String {
    match (least, most) {
        (1, 1) => "1 page".to_string(),
        (least, most) if least == most => format!("{most} pages"),
        (least, most) => format!("{least} to {most} pages"),
    }
}

/// "under a minute", "about 12 minutes", "12 to 40 minutes".
fn span((least, most): (u64, u64)) -> String {
    let minutes = |seconds: u64| seconds.div_ceil(60);
    if most < 60 {
        "under a minute".to_string()
    } else if minutes(least) == minutes(most) {
        format!("about {} minutes", minutes(most))
    } else {
        format!("{} to {} minutes", minutes(least), minutes(most))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_is_said_in_minutes() {
        assert_eq!(span((0, 30)), "under a minute");
        assert_eq!(span((100, 100)), "about 2 minutes");
        assert_eq!(span((0, 1_200)), "0 to 20 minutes");
    }

    #[test]
    fn a_range_is_one_number_when_it_is_one() {
        assert_eq!(range((3, 3)), "3 requests");
        assert_eq!(range((1, 1)), "1 request");
        assert_eq!(range((2, 9)), "2 to 9 requests");
        assert_eq!(pages((10, 15)), "10 to 15 pages");
        assert_eq!(pages((1, 1)), "1 page");
    }
}
