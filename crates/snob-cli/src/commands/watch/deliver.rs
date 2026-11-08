//! `snob watch deliver`: send what the queue owes the webhook, now.
//!
//! **No request to Instagram, and no session.** A report is queued with its
//! bytes in the same transaction that moves the monitor on
//! (`store::watch::commit_report`), so delivering it needs the account's
//! database and the webhook and nothing else: no client is built, no
//! session is read, and the budget is not touched. It is the queue drain a
//! run ends with (`run::drain_all`), without the run.
//!
//! The two differences are the person: the reports still waiting out their
//! backoff go too, since somebody asked for them now (`deliveries::owed_now`),
//! and what came of it is the answer, with exit 1 (`not_delivered`) when a
//! report it tried did not arrive, as `watch once` has.

use anyhow::Result;
use snob_store::config;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;
use snob_store::store::{Store, deliveries};

use crate::cli::WatchDeliverArgs;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

use super::delivery::delivery_from;
use super::run::{account_databases, drain_all, settle_all};
use super::wire::deliver_json;

pub(super) async fn deliver(
    args: WatchDeliverArgs,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // What has aged past being news is given up on first, as every run does,
    // so it is neither sent nor counted as owed.
    settle_all(paths);

    let configured = config::load(paths)?;
    let Some(delivery) = delivery_from(&args.delivery, configured.as_ref(), &secrets)? else {
        return Err(ExitError::new(
            ExitCode::Error,
            "there is no webhook, so there is nowhere to deliver to",
        )
        .with_hint("\"snob watch setup\" configures one, or pass --webhook <url>")
        .into());
    };
    let shown = crate::watch::webhook::shown_str(&delivery.destination);

    let cancel = crate::interrupt::install();
    let tried = drain_all(paths, &cancel, &delivery, true).await;

    // What is left for this address, every account's queue together.
    let mut left = deliveries::Owed::default();
    for account in account_databases(paths) {
        if let Ok(Some(db)) = Store::open_existing(&account)
            && let Ok(owed) = deliveries::owed(db.conn(), Some(&delivery.destination))
        {
            left.waiting += owed.waiting;
            left.elsewhere += owed.elsewhere;
        }
    }

    if args.output.json {
        ui::say!(
            "{}",
            super::wire::json_line(&deliver_json(&shown, tried, &left))
        );
    }
    ui::info(&match (tried, left.waiting) {
        (0, 0) => format!("Nothing is owed to {shown}."),
        (tried, 0) => format!("{tried} owed report{} sent to {shown}.", plural(tried)),
        (tried, waiting) => format!(
            "{tried} owed report{} tried at {shown}; {waiting} still waiting{}.",
            plural(tried),
            if tried > 0 && !delivery.left_undelivered() {
                ", run it again for the next ones"
            } else {
                ""
            }
        ),
    });
    if left.elsewhere > 0 {
        ui::info(&format!(
            "{} more {} queued for another address, and only that address is sent {}.",
            left.elsewhere,
            if left.elsewhere == 1 { "is" } else { "are" },
            if left.elsewhere == 1 { "it" } else { "them" },
        ));
    }

    if cancel.is_canceled() {
        return Ok(ExitCode::Interrupted);
    }
    if delivery.left_undelivered() {
        return Err(ExitError::new(
            ExitCode::Error,
            "a report did not reach the webhook; it stays queued, unless it was given up on \
             as said above",
        )
        .with_reason("not_delivered")
        .into());
    }
    Ok(ExitCode::Ok)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
