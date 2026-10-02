//! `snob watch once`: one run of the monitor, by hand or on a timer.

use anyhow::Result;
use snob_core::Pk;
use snob_store::config;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::WatchOnceArgs;
use crate::exit::ExitCode;
use crate::report;
use crate::ui;

use super::delivery::delivery_from;
use super::run::{Printing, run_viewers, settle_all};
use super::watched::watched_from;

/// One run of the monitor: look, report, and remember having reported.
///
/// **The scheduled run without the loop**, which is what the README's "one run,
/// for cron or a systemd timer" describes: the webhook and the accounts both
/// come from `watch.toml`, through the same `watched_from` the scheduled mode
/// asks. Building the watched set from the command line alone would never walk
/// an account `watch setup` added, and typing the name instead would fail every
/// unattended run, because `Watched::asking` discards the recorded consent that
/// is the only thing such a run accepts.
///
/// An entry that names no viewer is read as `in_use`, the account this
/// command resolved, if any.
pub(super) async fn once(
    args: WatchOnceArgs,
    secrets: SecretStore,
    paths: &AppPaths,
    in_use: Option<Pk>,
) -> Result<ExitCode> {
    // Settled before anything can refuse: a session that has gone, or a webhook
    // address `webhook::check` refuses, would otherwise leave owed reports aging
    // past `MAX_AGE_SECS`, where `due` no longer returns them and `failed` (the
    // only other thing that expires one) is never reached, while `status` goes
    // on promising the next run will try them.
    settle_all(paths);

    // Before the session is opened and long before a request is spent, so a
    // webhook address that could never work costs nothing to find out about.
    // The file is read here too: `once` on a timer should need no more
    // arguments than the scheduled mode does.
    let configured = config::load(paths)?;
    let delivery = delivery_from(&args.delivery, configured.as_ref(), &secrets)?;

    let watched = watched_from(args.target.clone(), configured.as_ref(), in_use);

    let outcome = run_viewers(
        paths,
        &secrets,
        &watched,
        delivery.as_ref(),
        Printing::watched(args.output.json),
        !args.progress.no_progress,
    )
    .await;

    // Said whether or not an account failed: a run that stopped halfway still
    // spent requests, and this is the mode somebody is watching.
    ui::info(&format!(
        "{} - {}",
        report::stored_on(snob_core::clock::now()),
        report::requests(outcome.spent)
    ));

    if let Some(e) = outcome.failed {
        return Err(e);
    }

    // The run's own verdict, as written into `watch_runs`. The exit codes exist
    // so a caller on a timer can tell "wait" from "log in again" without
    // parsing text, and `once` is the mode that is put on a timer.
    Ok(outcome.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::owed_long_ago;
    use snob_store::config::WatchConfig;
    use snob_store::store::deliveries;

    /// And neither does a webhook the run refuses.
    ///
    /// `delivery_from` ends in `webhook::check`, and both modes call it before
    /// anything is opened, so an address that could never work costs nothing to
    /// find out about; retention must still happen. A header this tool already
    /// sends is a natural thing to write into `[webhook.headers]`, and it is
    /// refused.
    #[tokio::test]
    async fn a_webhook_the_run_refuses_still_lets_the_queue_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snob_store::paths::AppPaths::rooted_at(tmp.path());
        let account = paths.account(snob_core::Pk::new(42));
        let now = snob_core::clock::now();
        let id = owed_long_ago(&account, now);

        let secrets = snob_store::secrets::SecretStore::new(paths.clone(), true)
            .with_service(&format!("snob-ig-test-refused-{}", std::process::id()));
        let args = WatchOnceArgs {
            delivery: crate::cli::WebhookArgs {
                webhook: Some("https://receiver.example/hook".to_string()),
                header: vec!["X-Snob-Source: homelab".to_string()],
                sign_with: None,
                heartbeat: false,
            },
            target: None,
            output: crate::cli::StatusOutputArgs { json: false },
            progress: crate::cli::ProgressArgs { no_progress: true },
        };

        let refused = once(args, secrets, &paths, Some(account.pk())).await;
        assert!(
            refused.is_err(),
            "that header is part of what snob sends, so the address is refused"
        );

        let db = snob_store::store::Store::open(&account).unwrap();
        assert_eq!(
            deliveries::state(db.conn(), id).unwrap().as_deref(),
            Some("expired"),
            "the address was refused, and the database still has to be tidied"
        );
    }

    /// `snob watch once` watches what the file says to watch.
    ///
    /// Both modes go through `watched_from`, so there is one answer to "which
    /// accounts" rather than two that disagree. Typing the name instead is no
    /// answer: `Watched::asking` carries no consent, and an unattended run
    /// accepts only a recorded one.
    ///
    /// What this pins is that answer. That `once` asks for it rather than
    /// building its own is not reachable from here — it would take an
    /// integration test that opens a session and a keyring to drive the command
    /// — so it is said in the doc-comment on `once` instead, where somebody
    /// changing it will read it.
    #[test]
    fn once_watches_the_accounts_the_file_lists() {
        let file = WatchConfig {
            schema: 1,
            every: Some(std::time::Duration::from_secs(6 * 3600)),
            accounts: vec![
                snob_store::config::AccountConfig {
                    target: "self".to_string(),
                    viewer: None,
                    consent: None,
                },
                snob_store::config::AccountConfig {
                    target: "friend".to_string(),
                    viewer: None,
                    consent: Some(snob_store::config::ConsentConfig {
                        agreed_at: snob_core::Epoch::new(1_700_000_000),
                    }),
                },
            ],
            ..Default::default()
        };

        let watched = watched_from(None, Some(&file), None);
        let names: Vec<Option<&str>> = watched.iter().map(|w| w.name()).collect();
        assert_eq!(
            names,
            vec![None, Some("friend")],
            "the file's own account and the one it lists"
        );
        assert!(
            watched.iter().all(|w| w.may_run_unattended()),
            "the recorded consent is what makes the second one legal on a timer"
        );

        // And a name typed on the command line still picks up the answer on
        // record, rather than discarding it and failing every unattended run.
        let typed = watched_from(Some("friend".to_string()), Some(&file), None);
        assert_eq!(typed.len(), 1);
        assert!(typed[0].may_run_unattended());
    }
}
