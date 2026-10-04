//! `--dry-run` on the list commands: what the walks would cost, against what
//! today has left, said and not done.
//!
//! **Nothing is sent and nobody is asked.** The account is not resolved over
//! the network, so the consent a stranger's lists need is not asked either,
//! as `--offline` does not ask it: nothing is enumerated. The estimate is
//! `engine::estimate`'s, out of what is stored here.

use anyhow::Result;
use serde_json::{Value, json};
use snob_core::model::ListKind;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SessionStore;

use crate::cli::{Format, OutputArgs, WalkArgs};
use crate::commands::common;
use crate::engine::estimate::{self, Estimate, Fate};
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
    let app = common::app(secrets, paths, false)?;
    let query = common::query(target, walk);
    let estimate = estimate::compute(&estimate::gather(&app, &query, kinds)?);
    let subject = crate::engine::target::label(&app, target.as_deref());

    let json = matches!(
        output::effective_format(output.format, output.path.as_deref()),
        Format::Json | Format::Ndjson
    );
    let rendered = if json {
        let mut out = as_json(&estimate);
        out["viewer"] = app.viewer().json();
        Rendered::Text(format!("{}\n", serde_json::to_string_pretty(&out)?))
    } else {
        Rendered::Text(format!("{}\n", as_text(&estimate, &subject)))
    };
    output::write_rendered(&rendered, output.path.as_deref())?;
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
                "pages": list.pages,
                "requests": { "least": list.requests.0, "most": list.requests.1 },
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
        "requests_left": estimate.requests_left,
        "accounts_left": estimate.accounts_left,
        "pauses": estimate.pauses,
        "cooldown_until": estimate.held.map(|until| until.to_epoch()),
    })
}

fn as_text(estimate: &Estimate, subject: &str) -> String {
    let mut lines = vec![format!("A dry run for {subject}: nothing was sent.")];
    lines.push(match estimate.to_find {
        0 => "Finding the account: nothing, it is yours.".to_string(),
        n => format!("Finding the account and opening its profile: {n} requests."),
    });
    for list in &estimate.lists {
        let name = match list.kind {
            ListKind::Followers => "Followers",
            ListKind::Following => "Following",
        };
        let walk = format!(
            "about {} accounts to read, {} pages",
            list.to_read, list.pages
        );
        lines.push(match list.fate {
            Fate::ReusedUnlessMoved => format!(
                "{name}: stored {}; {} if its count has not moved, {} if it has ({walk}).",
                list.taken_at.map_or("earlier".into(), report::stored_on),
                requests(list.requests.0),
                requests(list.requests.1)
            ),
            Fate::Walked => format!(
                "{name}: a walk, {walk}; about {}.",
                requests(list.requests.1)
            ),
            Fate::Unknown => format!(
                "{name}: never read here, so its size is not known; a walk of every page it has."
            ),
            Fate::Refused => format!(
                "{name}: another snob is walking it right now, and a run would stop rather \
                 than walk it twice."
            ),
        });
    }
    let (least, most) = estimate.requests;
    let total = if least == most {
        requests(most)
    } else {
        format!("{least} to {}", requests(most))
    };
    let unknown = if estimate.lists.iter().any(|l| l.fate == Fate::Unknown) {
        ", and the pages of the lists not known here"
    } else {
        ""
    };
    lines.push(format!(
        "In all: {total}{unknown}, up to {} accounts read, in {}.",
        estimate.accounts,
        span(estimate.seconds)
    ));
    lines.push(format!(
        "Today has {} requests and {} accounts left.",
        estimate.requests_left, estimate.accounts_left
    ));
    if estimate.pauses {
        lines.push(format!("More than that: {}.", report::PAUSING_BY_DEFAULT));
    } else if estimate.same_day && estimate.accounts > estimate.accounts_left {
        lines.push(
            "More than that, and --same-day reads past it: only the requests budget stops it."
                .to_string(),
        );
    }
    if let Some(until) = estimate.held {
        lines.push(format!(
            "The account is in cooldown until {}: a run now would answer from storage only.",
            report::stored_on(until.to_epoch())
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
}
