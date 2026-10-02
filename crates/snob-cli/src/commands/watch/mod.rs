//! `snob watch`: the monitor.
//!
//! Two ways to ask the same question. `diff` answers out of storage and leaves
//! everything as it found it, so it can be run as often as anybody likes and
//! costs nothing. `once` goes and looks, reports, and remembers having
//! reported — which is the difference that matters, because what a run reports
//! it does not report again.
//!
//! The scheduled mode is what remains: it is `once` on a timer, and the report
//! it produces is the same one.
//!
//! Wording, dates and exit codes live here. What actually changed is
//! [`crate::engine::watch`]'s answer, and this never recomputes any of it.
//!
//! Getting a report to a receiver is [`delivery`], and the split is by question
//! rather than by layer: this module decides what a report says, that one
//! decides where it goes and what travels with it.

use anyhow::Result;
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::secrets::SecretStore;

use crate::cli::{WatchArgs, WatchCommand};
use crate::exit::ExitCode;

pub(super) mod delivery;

/// The two commands that are about the configuration rather than about a
/// report.
pub(super) mod setup;
pub(super) mod status;

/// A report said two ways. [`wire`] is what a receiver is sent and what a
/// signature covers; [`say`] is what a person reads. Apart, so "does this change
/// break somebody's integration" and "is this sentence right" are questions
/// about different pages.
pub(super) mod say;
pub(super) mod wire;

/// The commands somebody types, one file each, and the loop that runs the
/// first of them on a timer. What the four have in common is `run`: one run of
/// the monitor over the accounts it watches, which is the whole of `once` and
/// one turn of `scheduled`.
mod check;
mod diff;
mod once;
mod run;
mod scheduled;

/// What a run is told before it starts — which accounts it is for and on what
/// schedule — and, for `check` and `setup`, whether any of it would work at
/// all. Read by the commands above and by `setup` and `status`, which is why
/// they are not part of any one of them.
mod preflight;
mod schedule;
mod watched;

/// Shapes for the tests of every file here.
#[cfg(test)]
mod fixtures;

use check::check;
use diff::diff;
use once::once;
use scheduled::scheduled;

/// Runs the monitor. `account` is the account resolved for this run, when any
/// is signed in; a watched entry that names no viewer is read as it.
pub async fn run(
    args: WatchArgs,
    secrets: SecretStore,
    paths: &AppPaths,
    account: Option<AccountPaths>,
) -> Result<ExitCode> {
    let in_use = account.as_ref().map(|account| account.pk());
    let account = account.as_ref();
    match args.command {
        Some(WatchCommand::Diff(args)) => diff(args, secrets, account),
        Some(WatchCommand::Once(args)) => once(args, secrets, paths, in_use).await,
        Some(WatchCommand::Check(args)) => check(args, secrets, paths, account).await,
        Some(WatchCommand::Setup(args)) => setup::setup(args, secrets, paths, account).await,
        Some(WatchCommand::Status(args)) => status::status(args, paths, account),
        // With nobody signed in there is nobody to read any entry as, and a
        // service started then would read the entries that name no viewer as
        // nobody for as long as it ran.
        None => {
            let in_use = in_use.ok_or_else(crate::commands::common::no_session)?;
            scheduled(args.run, secrets, paths, in_use).await
        }
    }
}
