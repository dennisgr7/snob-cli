//! `snob snapshots`: the captures of an account's lists stored here.
//!
//! **Sends nothing and writes nothing**, for the reasons `snob status` gives:
//! it reads the account's database with `Store::read_existing`, which neither
//! creates nor migrates it. So it is one of the listed exceptions to
//! "commands take an App", and so is `snob changes`, which shares
//! [`stored_account`].
//!
//! Every capture is listed, a walk that stopped short included: this is where
//! somebody finds out one did, and why, and which ids `snob changes --from`
//! may take (only whole ones).

use anyhow::Result;
use snob_core::Pk;
use snob_core::model::{ListKind, printable};
use snob_store::paths::AccountPaths;
use snob_store::store::{Store, accounts, snapshots};

use crate::cli::SnapshotsArgs;
use crate::exit::{ExitCode, ExitError};
use crate::output::grid::{Grid, Value};
use crate::output::{self, Presentation};

/// The account's database, read without writing, and the account a command
/// is about in it: the viewer's own, or the one `target` names, found by
/// the name the store has for it. Refused when nothing is stored, since
/// nothing could be answered.
pub(crate) fn stored_account(
    account: &AccountPaths,
    target: Option<&str>,
) -> Result<(Store, Pk, String)> {
    let Some(store) = Store::read_existing(account)? else {
        return Err(nothing_here(target));
    };
    let Some(name) = target else {
        let own = crate::report::acting_as().map_or_else(|| "your account".into(), |v| v.label());
        return Ok((store, account.pk(), own));
    };
    let name = crate::engine::target::clean(name);
    match accounts::find_pk_by_username(store.conn(), name)? {
        Some(pk) => Ok((store, pk, format!("@{}", printable(name)))),
        None => Err(nothing_here(Some(name))),
    }
}

/// Nothing stored to answer from: no list of this account was ever walked
/// here, or of the account named.
fn nothing_here(target: Option<&str>) -> anyhow::Error {
    let (whose, walk) = match target {
        Some(name) => {
            let name = printable(crate::engine::target::clean(name));
            (format!("@{name}"), format!("snob followers {name}"))
        }
        None => ("your account".to_string(), "snob scan".to_string()),
    };
    ExitError::new(
        ExitCode::Error,
        format!("nothing of {whose} is stored on this computer"),
    )
    .with_reason("nothing_stored")
    .with_hint(format!("a walk stores it: \"{walk}\""))
    .into()
}

pub fn run(args: SnapshotsArgs, account: &AccountPaths) -> Result<ExitCode> {
    let destination = args.output.path.as_deref();
    let format = output::effective_format(args.output.format, destination);
    output::check_destination(format, destination)?;

    let (store, pk, whose) = stored_account(account, args.target.as_deref())?;
    let kind = args.list.map(ListKind::from);
    let listed = snapshots::listed(store.conn(), pk, kind)?;

    let grid = grid(&listed);
    let rendered = grid.render(format, Presentation::detect(destination))?;
    output::write_rendered(&rendered, destination)?;
    crate::ui::info(&match listed.len() {
        0 => format!("No capture of {whose}'s lists is stored."),
        1 => format!("1 capture of {whose}'s lists is stored."),
        n => format!("{n} captures of {whose}'s lists are stored."),
    });
    Ok(ExitCode::Ok)
}

/// One row per capture, newest first.
fn grid(listed: &[snapshots::Listed]) -> Grid {
    Grid {
        columns: vec![
            ("id", "Id"),
            ("list", "List"),
            ("taken_at", "Ended"),
            ("started_at", "Began"),
            ("source", "Source"),
            ("accounts", "Accounts"),
            ("declared", "Declared"),
            ("complete", "Whole"),
            ("stopped", "Stopped"),
        ],
        rows: listed
            .iter()
            .map(|listed| {
                let snapshot = &listed.snapshot;
                vec![
                    Value::Number(snapshot.id.max(0) as u64),
                    Value::Text(snapshot.kind.as_str().to_string()),
                    snapshot.taken_at.map_or(Value::Empty, Value::Moment),
                    Value::Moment(snapshot.started_at),
                    Value::Text(listed.source.clone()),
                    Value::Number(snapshot.member_count),
                    snapshot.declared_count.map_or(Value::Empty, Value::Number),
                    Value::Flag(listed.complete),
                    Value::text(listed.stopped_by.as_deref()),
                ]
            })
            .collect(),
    }
}
