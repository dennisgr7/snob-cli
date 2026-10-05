//! `snob followers` and `snob following`.
//!
//! Both are the same command with the list named differently. What they do is
//! ask [`crate::engine`] for a list and print it; the deciding — cache, walk,
//! cooldown — happens there.

use anyhow::Result;
use snob_core::filters::Filter;
use snob_core::model::{ListKind, User};
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::app::{App, Viewer};
use crate::cli::ListArgs;
use crate::commands::{common, dry_run};
use crate::engine::{self, ListOutcome};
use crate::exit::ExitCode;
use crate::report;
use crate::ui;
use crate::ui::accounts::Browsed;

pub async fn run(
    args: ListArgs,
    store: SecretStore,
    paths: &AccountPaths,
    kind: ListKind,
) -> Result<ExitCode> {
    if args.walk.dry_run {
        let secrets = store.session_of(paths);
        return dry_run::run(
            &args.walk,
            &args.target,
            &args.output,
            &secrets,
            paths,
            &[kind],
        );
    }
    let (filter, destination, browses) = common::prepare(&args.filter, &args.output, &args.browse)?;

    let mut app = common::open(&args.walk, &store.session_of(paths), paths)?;
    let walked = walk(
        &mut app,
        engine::ListQuery::from(&args),
        kind,
        &filter,
        args.limit,
    )
    .await?;

    // The browser instead of the listing — the default at a terminal, the
    // decision made above. An empty result is not browsed: there is nothing
    // to move over, and the summary line already says the count.
    if browses && !walked.shown.is_empty() {
        return browse(app, walked, &args, kind, &filter, &store, paths).await;
    }
    destination.write(&walked.shown)?;
    print_summary(&walked, kind, None);
    // A plain list is the one place a partial answer is still worth having:
    // every account in it really is in the list, only some are missing. So it
    // prints, says so, and exits with what stopped it.
    Ok(walked.outcome.exit_code_for_a_printed_result())
}

/// One list as one account: what the view shows and the summary says.
struct Walked {
    shown: Vec<User>,
    /// After the filter, before the cap.
    kept: usize,
    /// Before the filter.
    total: usize,
    outcome: ListOutcome,
    /// How the list's account is named on screen.
    subject: String,
    /// Its name, for another account to walk the same list with.
    name: Option<String>,
}

/// Walks the list `query` asks for as `app`, and narrows it.
async fn walk(
    app: &mut App,
    query: engine::ListQuery,
    kind: ListKind,
    filter: &Filter,
    limit: Option<usize>,
) -> Result<Walked> {
    // Named before the engine runs, so the bar says what it is about during
    // consent, resolution and the counter poll rather than only once pages
    // start arriving.
    let subject = engine::target::label(app, query.target.as_deref());
    let typed = query.target.clone();
    let result = common::walk_named(app, &query, kind, &subject, |_| Ok(())).await;
    app.progress().finish();
    let (found, outcome) = result?;
    let name = common::walked_name(app, typed.as_deref(), outcome.account_pk)?;
    let common::Narrowed { shown, kept, total } = common::narrow(found, filter, limit);
    Ok(Walked {
        shown,
        kept,
        total,
        outcome,
        subject,
        name,
    })
}

/// The same list walked again as `to`, from what `from` walked.
async fn walk_as(
    app: &mut App,
    args: &ListArgs,
    name: String,
    kind: ListKind,
    filter: &Filter,
) -> Result<Walked> {
    let query = engine::ListQuery {
        target: Some(name),
        ..engine::ListQuery::from(args)
    };
    walk(app, query, kind, filter, args.limit).await
}

/// The list in the browser, as the account the command runs as and then as
/// each account it is walked again as. Each account's summary is said as its
/// view is left.
async fn browse(
    app: Box<App>,
    walked: Walked,
    args: &ListArgs,
    kind: ListKind,
    filter: &Filter,
    store: &SecretStore,
    paths: &AccountPaths,
) -> Result<ExitCode> {
    common::switching(
        (app, walked),
        async |(app, walked): &mut (Box<App>, Walked), note: String| {
            let shelf = ui::people::Shelf::flat(
                format!("{kind} of {}", walked.subject),
                &walked.shown,
                app.viewer().clone(),
            );
            let rewalk = ui::people::Rewalk {
                secrets: store,
                paths,
                what: format!("{kind} of {}", walked.subject),
                requests: common::rewalk_cost(
                    app,
                    walked.outcome.account_pk,
                    &[(kind, walked.total)],
                )?,
            };
            // Leaving with Ctrl+C outranks what stopped the walk: it is the
            // freshest thing the user said.
            Ok(
                match ui::people::browse(app, &shelf, &rewalk, note).await? {
                    Browsed::Done(ExitCode::Interrupted) => Browsed::Done(ExitCode::Interrupted),
                    Browsed::Done(_) => {
                        Browsed::Done(walked.outcome.exit_code_for_a_printed_result())
                    }
                    switch => switch,
                },
            )
        },
        async |(app, walked): &(Box<App>, Walked), to: Viewer| {
            common::rewalk(
                store,
                paths,
                app.viewer(),
                walked.name.as_deref(),
                &to,
                common::shows_progress(&args.walk),
                async |app: &mut App, name: String| walk_as(app, args, name, kind, filter).await,
            )
            .await
        },
        |(app, walked): &(Box<App>, Walked), several: bool| {
            print_summary(walked, kind, several.then(|| app.viewer()));
        },
    )
    .await
}

/// The singular of a list's name. `Display` gives the plural, and for
/// `Following` the two differ by more than a letter. It lives here rather than
/// on `ListKind` because wording belongs in the commands, not in the domain.
fn one_of(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Followers => "follower",
        ListKind::Following => "account you follow",
    }
}

/// The closing line, naming the account it was walked as when the view
/// showed it as more than one.
fn print_summary(walked: &Walked, kind: ListKind, whom: Option<&Viewer>) {
    let outcome = &walked.outcome;
    let mut line = report::counted(
        walked.shown.len(),
        walked.kept,
        walked.total,
        one_of(kind),
        &kind.to_string(),
    );
    if let Some(whom) = whom {
        line.push_str(&format!(" as {}", whom.label()));
    }

    if outcome.is_stored() {
        line.push_str(&format!(
            " - list stored on {}",
            report::stored_on(outcome.taken_at)
        ));
        line.push_str(&report::spent(outcome.requests));
    } else {
        line.push_str(&format!(" - {}", report::requests(outcome.requests)));
        if let Some(why) = report::why_incomplete(outcome.reason) {
            line.push_str(&format!(" - INCOMPLETE: {why}"));
        }
    }

    ui::info(&line);

    if !outcome.is_complete() && !outcome.is_stored() {
        ui::warn(&format!(
            "the list is incomplete, so it cannot be compared against another one. {}",
            report::try_again_advice(outcome.reason, outcome.resumable)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ConsentArgs, ProgressArgs, WalkArgs};
    use crate::commands::common::Switched;
    use snob_core::Pk;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// An app acting as `pk`, `name`, with a database of its own, asking the
    /// fake Instagram at `server`.
    fn app_as(server: &MockServer, pk: u64, name: &str) -> Box<App> {
        let session = snob_core::session::Session::from_sessionid(
            &format!("{pk}%3AAbCdEfGh%3A20"),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/138.0.0.0 Safari/537.36",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        let client = snob_ig::client::IgClient::new(session, snob_ig::pace::Pacer::unlimited())
            .unwrap()
            .with_base_url(url::Url::parse(&server.uri()).unwrap());
        Box::new(App::for_test(
            client,
            snob_store::store::Store::in_memory().unwrap(),
            Viewer {
                pk: Pk::new(pk),
                username: Some(name.to_string()),
            },
        ))
    }

    /// Picking another account in the view and answering yes walks the same
    /// list, of the same account, as the account picked, and draws the view
    /// again as it.
    #[tokio::test]
    async fn yes_walks_the_same_list_as_the_account_picked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .and(query_param("username", "me"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":{"user":{"id":"42","username":"me",
                    "edge_followed_by":{"count":2},"edge_follow":{"count":0}}}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/followers/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"users":[{"pk":1,"username":"anna"},{"pk":2,"username":"bob"}]}"#,
            ))
            .mount(&server)
            .await;

        let args = ListArgs {
            target: None,
            filter: Default::default(),
            output: Default::default(),
            limit: None,
            browse: Default::default(),
            walk: WalkArgs {
                progress: ProgressArgs { no_progress: true },
                consent: ConsentArgs { yes: true },
                ..WalkArgs::default()
            },
        };
        let filter = Filter::default();
        let mut me = app_as(&server, 42, "me");
        let walked = walk(
            &mut me,
            engine::ListQuery::from(&args),
            ListKind::Followers,
            &filter,
            None,
        )
        .await
        .unwrap();
        assert_eq!(walked.name.as_deref(), Some("me"));
        let profile_asks = async || {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == "/api/v1/users/web_profile_info/")
                .count()
        };
        let asked_as_me = profile_asks().await;

        let work = Viewer {
            pk: Pk::new(43),
            username: Some("work".to_string()),
        };
        let mut script =
            vec![Browsed::SwitchTo(work.clone()), Browsed::Done(ExitCode::Ok)].into_iter();
        let mut drawn = Vec::new();
        let mut as_work = Some(app_as(&server, 43, "work"));
        let code = common::switching(
            (me, walked),
            async |(app, walked): &mut (Box<App>, Walked), _note: String| {
                drawn.push((
                    app.viewer().label(),
                    walked.subject.clone(),
                    walked.shown.len(),
                    walked.outcome.account_pk,
                ));
                Ok(script.next().expect("drawn once too often"))
            },
            async |(_, walked): &(Box<App>, Walked), to: Viewer| {
                assert_eq!(to, work);
                let mut app = as_work.take().expect("switched once too often");
                let name = walked.name.clone().unwrap();
                let walked = walk_as(&mut app, &args, name, ListKind::Followers, &filter).await?;
                Ok(Switched::To((app, walked)))
            },
            |_: &(Box<App>, Walked), _| {},
        )
        .await
        .unwrap();

        assert_eq!(code, ExitCode::Ok);
        assert_eq!(
            drawn,
            [
                ("@me".to_string(), "@me".to_string(), 2, Pk::new(42)),
                ("@work".to_string(), "@me".to_string(), 2, Pk::new(42)),
            ]
        );
        // As @work the account is somebody else's, found by its name first.
        assert!(profile_asks().await > asked_as_me);
    }
}
