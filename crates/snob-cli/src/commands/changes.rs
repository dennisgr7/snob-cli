//! `snob changes`: who joined, left or was renamed between two stored
//! captures of a list.
//!
//! **Sends nothing and writes nothing**, as `snob snapshots`, whose
//! `stored_account` it reads through: no request, so it answers in a
//! cooldown too, and no mark moved, so it is not the monitor and asking
//! twice gets one answer. The comparison is the monitor's own
//! (`snob_core::watch::ListDiff`, by pk, so a rename is no departure and
//! arrival), and the renames come from `username_history` for the window
//! between the two captures (`store::watch::renames_between`).
//!
//! **Only whole captures are compared** (`usable_snapshots`): the accounts a
//! walk stopped short of would read as departures. An id given that is not
//! one is refused, not stepped over.

use anyhow::{Result, bail};
use serde_json::{Value as Json, json};
use snob_core::model::{ListKind, User};
use snob_core::watch::{ListDiff, Rename};
use snob_core::{Epoch, Pk};
use snob_store::paths::AccountPaths;
use snob_store::store::{Store, snapshots, watch as watch_store};

use crate::cli::{ChangesArgs, Format};
use crate::exit::{ExitCode, ExitError};
use crate::output::grid::{Grid, Value};
use crate::output::{self, Presentation, Rendered};
use crate::report;

/// One list's comparison, or why there is none.
struct Compared {
    kind: ListKind,
    /// The two captures, earlier first; `None` with fewer than two whole
    /// ones stored.
    between: Option<(snapshots::Snapshot, snapshots::Snapshot)>,
    diff: ListDiff,
    renamed: Vec<Rename>,
    /// Said beside the result when the earlier capture is not the one
    /// `--since` asked for: none is that old.
    note: Option<String>,
    /// With no comparison, the whole capture there was nothing to compare
    /// with, and on which side the other one is missing; `None` when fewer
    /// than two whole captures are stored at all.
    alone: Option<(snapshots::Snapshot, Missing)>,
}

/// Which capture of a pair there was none of.
#[derive(Clone, Copy)]
enum Missing {
    /// None was taken after it: the newest, picked by `--from` or by a
    /// `--since` no capture is newer than.
    Later,
    /// None was taken before it: the oldest, picked by `--to`.
    Earlier,
}

pub fn run(args: ChangesArgs, account: &AccountPaths) -> Result<ExitCode> {
    let destination = args.output.path.as_deref();
    let format = output::effective_format(args.output.format, destination);
    output::check_destination(format, destination)?;

    let (store, pk, whose) =
        crate::commands::snapshots::stored_account(account, args.target.as_deref())?;
    let now = snob_core::clock::now();

    let kinds = kinds(&store, pk, &args)?;
    let mut compared = Vec::new();
    for kind in kinds {
        compared.push(compare(&store, pk, kind, &args, now)?);
    }
    // A friend is in both lists and is one person: renamed once.
    let mut seen = std::collections::HashSet::new();
    for one in &mut compared {
        one.renamed.retain(|rename| seen.insert(rename.pk));
    }

    let rendered = match format {
        Format::Json => {
            let mut text = serde_json::to_string_pretty(&document(&whose, &compared))?;
            text.push('\n');
            Rendered::Text(text)
        }
        other => grid(&compared).render(other, Presentation::detect(destination))?,
    };
    output::write_rendered(&rendered, destination)?;
    for one in &compared {
        crate::ui::info(&said(&whose, one));
        if let Some(note) = &one.note {
            crate::ui::warn(note);
        }
    }
    Ok(ExitCode::Ok)
}

/// The lists to compare: the one `--list` names, the one `--from` or `--to`
/// is a capture of, or both.
fn kinds(store: &Store, pk: Pk, args: &ChangesArgs) -> Result<Vec<ListKind>> {
    let named = args.list.map(ListKind::from);
    let by_id = match args.from.or(args.to) {
        Some(id) => Some(usable_of(store, pk, id)?.kind),
        None => None,
    };
    Ok(match (named, by_id) {
        (Some(named), Some(by_id)) if named != by_id => {
            bail!(
                "capture {} is of the {by_id} list, not {named}",
                args.from.or(args.to).unwrap_or_default()
            )
        }
        (_, Some(kind)) | (Some(kind), None) => vec![kind],
        (None, None) => vec![ListKind::Followers, ListKind::Following],
    })
}

/// The whole capture `id` of `pk`'s lists, or the refusal saying why it
/// cannot be compared.
fn usable_of(store: &Store, pk: Pk, id: i64) -> Result<snapshots::Snapshot> {
    match snapshots::find_usable(store.conn(), id)? {
        Some(snapshot) if snapshot.account_pk == pk => Ok(snapshot),
        _ => Err(ExitError::new(
            ExitCode::Error,
            format!(
                "capture {id} is not a whole capture of this account's lists, so it cannot be \
                 compared"
            ),
        )
        .with_reason("no_such_snapshot")
        .with_hint("\"snob snapshots\" lists the captures, and which are whole")
        .into()),
    }
}

/// Picks the two captures of `kind` and compares them.
fn compare(
    store: &Store,
    pk: Pk,
    kind: ListKind,
    args: &ChangesArgs,
    now: Epoch,
) -> Result<Compared> {
    // Newest first.
    let whole = snapshots::usable(store.conn(), pk, kind)?;
    let mut note = None;
    let mut alone = None;
    let between = if let Some(from) = args.from {
        let from = usable_of(store, pk, from)?;
        let to = match args.to {
            Some(to) => usable_of(store, pk, to)?,
            None => whole.first().cloned().unwrap_or_else(|| from.clone()),
        };
        if to.kind != from.kind {
            bail!("captures {} and {} are of different lists", from.id, to.id);
        }
        // A capture against itself is no comparison, as below.
        if from.id == to.id {
            alone = Some((from, Missing::Later));
            None
        } else {
            Some(earlier_first(from, to))
        }
    } else if let Some(to) = args.to {
        let to = usable_of(store, pk, to)?;
        let from = whole
            .iter()
            .find(|older| older.taken_at < to.taken_at)
            .cloned();
        if from.is_none() {
            alone = Some((to.clone(), Missing::Earlier));
        }
        from.map(|from| (from, to))
    } else {
        let after = whole.first().cloned();
        let before = match args.since {
            Some(since) => {
                let at = since.at(now);
                match whole
                    .iter()
                    .find(|capture| capture.taken_at.is_some_and(|t| t <= at))
                {
                    Some(capture) => Some(capture.clone()),
                    // None that old: the oldest there is, said so.
                    None => {
                        let oldest = whole.last().cloned();
                        // Said only where there is a comparison to qualify.
                        if let Some(oldest) = oldest.as_ref().filter(|_| whole.len() > 1) {
                            note = Some(format!(
                                "no {kind} capture is that old; compared from the oldest one \
                                 stored, of {}",
                                report::stored_on(oldest.taken_at.unwrap_or_default())
                            ));
                        }
                        oldest
                    }
                }
            }
            None => whole.get(1).cloned(),
        };
        match (before, after) {
            (Some(before), Some(after)) if before.id != after.id => Some((before, after)),
            // The newest is as old as `--since` reaches: nothing was taken
            // after it, which is not the same as two never being stored.
            (Some(newest), Some(_)) if whole.len() > 1 => {
                alone = Some((newest, Missing::Later));
                None
            }
            _ => None,
        }
    };

    let Some((before, after)) = between else {
        return Ok(Compared {
            kind,
            between: None,
            diff: ListDiff::default(),
            renamed: Vec::new(),
            note,
            alone,
        });
    };
    let diff = ListDiff::between(
        &snapshots::members(store.conn(), before.id)?,
        &snapshots::members(store.conn(), after.id)?,
    );
    let renamed = watch_store::renames_between(
        store.conn(),
        &[before.id, after.id],
        before.taken_at.unwrap_or_default(),
        after.taken_at.unwrap_or_default(),
    )?;
    Ok(Compared {
        kind,
        between: Some((before, after)),
        diff,
        renamed,
        note,
        alone: None,
    })
}

fn earlier_first(
    a: snapshots::Snapshot,
    b: snapshots::Snapshot,
) -> (snapshots::Snapshot, snapshots::Snapshot) {
    if (b.taken_at, b.id) < (a.taken_at, a.id) {
        (b, a)
    } else {
        (a, b)
    }
}

/// The sentence on standard error for one list.
fn said(whose: &str, one: &Compared) -> String {
    let Some((before, after)) = &one.between else {
        return match &one.alone {
            Some((capture, Missing::Later)) => format!(
                "{whose}'s {} list has no whole capture newer than the one of {} ({}), so \
                 there is nothing to compare it with yet; a later walk gives one.",
                one.kind,
                report::stored_on(capture.taken_at.unwrap_or_default()),
                capture.id,
            ),
            Some((capture, Missing::Earlier)) => format!(
                "{whose}'s {} list has no whole capture older than the one of {} ({}), so \
                 there is nothing to compare it with.",
                one.kind,
                report::stored_on(capture.taken_at.unwrap_or_default()),
                capture.id,
            ),
            None => format!(
                "{whose}'s {} list has fewer than two whole captures stored, so there is \
                 nothing to compare yet; a later walk gives one.",
                one.kind
            ),
        };
    };
    format!(
        "{whose}'s {}: {} gained, {} lost, {} renamed, between the capture of {} ({}) and the \
         one of {} ({}).",
        one.kind,
        one.diff.gained.len(),
        one.diff.lost.len(),
        one.renamed.len(),
        report::stored_on(before.taken_at.unwrap_or_default()),
        before.id,
        report::stored_on(after.taken_at.unwrap_or_default()),
        after.id,
    )
}

/// `--format json`: one entry per list, with the two captures it compares.
fn document(whose: &str, compared: &[Compared]) -> Json {
    let capture = |snapshot: &snapshots::Snapshot| json!({ "id": snapshot.id, "taken_at": snapshot.taken_at });
    json!({
        "account": snob_core::model::printable(whose),
        "lists": compared.iter().map(|one| json!({
            "list": one.kind.as_str(),
            "from": one.between.as_ref().map(|(before, _)| capture(before)),
            "to": one.between.as_ref().map(|(_, after)| capture(after)),
            "gained": output::readable(&one.diff.gained),
            "lost": output::readable(&one.diff.lost),
            "renamed": one.renamed.iter().map(|rename| json!({
                "pk": rename.pk,
                "from": snob_core::model::printable(&rename.from),
                "to": snob_core::model::printable(&rename.to),
                "at": rename.at,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// Every other format: one row per change.
fn grid(compared: &[Compared]) -> Grid {
    let mut rows = Vec::new();
    for one in compared {
        let Some((before, after)) = &one.between else {
            continue;
        };
        let ids = [
            Value::Number(before.id.max(0) as u64),
            Value::Number(after.id.max(0) as u64),
        ];
        let account = |change: &str, user: &User| {
            let mut row = vec![
                Value::Text(one.kind.as_str().into()),
                Value::Text(change.into()),
                Value::Number(user.pk.get()),
                Value::text(Some(&user.username)),
                Value::text(user.full_name.as_deref()),
                Value::Empty,
            ];
            row.extend(ids.clone());
            row
        };
        rows.extend(one.diff.gained.iter().map(|user| account("gained", user)));
        rows.extend(one.diff.lost.iter().map(|user| account("lost", user)));
        for rename in &one.renamed {
            let mut row = vec![
                Value::Text(one.kind.as_str().into()),
                Value::Text("renamed".into()),
                Value::Number(rename.pk.get()),
                Value::text(Some(&rename.to)),
                Value::Empty,
                Value::text(Some(&rename.from)),
            ];
            row.extend(ids.clone());
            rows.push(row);
        }
    }
    Grid {
        columns: vec![
            ("list", "List"),
            ("change", "Change"),
            ("pk", "Id"),
            ("username", "Username"),
            ("full_name", "Full name"),
            ("previous_username", "Was"),
            ("from_snapshot", "From"),
            ("to_snapshot", "To"),
        ],
        rows,
    }
}
