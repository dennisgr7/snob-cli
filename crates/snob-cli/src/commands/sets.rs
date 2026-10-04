//! `snob unfollowers`, `snob fans` and `snob friends`.
//!
//! All three cross an account's two lists, so they cost two walks instead of
//! one. The cache policy makes them just as cheap as `followers` and
//! `following`: when both lists are stored and neither counter has moved, the
//! cross comes out of storage and costs one request, the one that reads both
//! counters.

use anyhow::Result;
use snob_core::filters::Filter;
use snob_core::model::{ListKind, User};
use snob_core::sets;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::app::{App, Viewer};
use crate::cli::ListArgs;
use crate::commands::common;
use crate::engine::{self, ListOutcome};
use crate::exit::ExitCode;
use crate::report;
use crate::ui;
use crate::ui::accounts::Browsed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// Those you follow who do not follow you back.
    Unfollowers,
    /// Those who follow you and you do not follow.
    Fans,
    /// Those you follow each other.
    Friends,
}

impl SetOp {
    /// What the result is, singular first. Both are written out because the
    /// singular of these is not the plural with its `s` taken off.
    fn description(self) -> (&'static str, &'static str) {
        match self {
            Self::Unfollowers => (
                "account you follow that does not follow you back",
                "accounts you follow that do not follow you back",
            ),
            Self::Fans => (
                "account that follows you and you do not follow",
                "accounts that follow you and you do not follow",
            ),
            Self::Friends => (
                "friend, you follow each other",
                "friends, following each other",
            ),
        }
    }

    /// The command's own name: what `-i`'s heading calls the result.
    fn name(self) -> &'static str {
        match self {
            Self::Unfollowers => "unfollowers",
            Self::Fans => "fans",
            Self::Friends => "friends",
        }
    }

    /// The list the results come out of.
    fn base(self) -> ListKind {
        match self {
            Self::Unfollowers => ListKind::Following,
            Self::Fans | Self::Friends => ListKind::Followers,
        }
    }

    /// The list it is crossed against.
    fn against(self) -> ListKind {
        match self {
            Self::Unfollowers => ListKind::Followers,
            Self::Fans | Self::Friends => ListKind::Following,
        }
    }

    /// What an account missing from the crossed-against list would be made to
    /// look like. This is the sentence that explains why a partial one is
    /// refused rather than reported.
    fn misreads_as(self) -> &'static str {
        match self {
            Self::Unfollowers => "they did not follow you",
            Self::Fans => "you did not follow them",
            Self::Friends => "you were not friends",
        }
    }

    /// The refusal of an incomplete list to cross against, naming that list
    /// and this crossing's misreading.
    fn require_against_complete(self, outcome: &ListOutcome) -> Result<()> {
        common::require_complete(self.against(), outcome, self.misreads_as())
    }
}

pub async fn run(
    args: ListArgs,
    store: SecretStore,
    paths: &AccountPaths,
    op: SetOp,
) -> Result<ExitCode> {
    if args.walk.dry_run {
        let secrets = store.session_of(paths);
        // In the order `cross` walks them: the list crossed against first.
        let kinds = [op.against(), op.base()];
        return crate::commands::dry_run::run(
            &args.walk,
            &args.target,
            &args.output,
            &secrets,
            paths,
            &kinds,
        );
    }
    let (filter, destination, browses) = common::prepare(&args.filter, &args.output, &args.browse)?;

    let mut app = common::open(&args.walk, &store.session_of(paths), paths)?;
    let crossed = cross(
        &mut app,
        engine::ListQuery::from(&args),
        op,
        &filter,
        args.limit,
    )
    .await?;

    // The browser instead of the listing — the default at a terminal, the
    // decision made above. An empty result is not browsed: there is nothing
    // to move over, and the summary line already says the count.
    if browses && !crossed.result.is_empty() {
        return browse(app, crossed, &args, op, &filter, &store, paths).await;
    }
    destination.write(&crossed.result)?;
    print_summary(op, &crossed, None);

    // Decided by what stopped the **base** list alone. The list crossed
    // against is not consulted: it was refused outright in `cross`, well
    // before there was a result to code.
    Ok(crossed.base_outcome.exit_code_for_a_printed_result())
}

/// One crossing as one account: what the view shows and the summary says.
struct Crossed {
    result: Vec<User>,
    /// After the filter, before the cap.
    kept: usize,
    /// Before the filter.
    total: usize,
    base_len: usize,
    against_len: usize,
    base_outcome: ListOutcome,
    against_outcome: ListOutcome,
    /// How the crossed account is named on screen.
    subject: String,
    /// Its name, for another account to cross the same lists with.
    name: Option<String>,
}

/// Walks both lists `query` asks for as `app`, crosses and narrows them.
async fn cross(
    app: &mut App,
    query: engine::ListQuery,
    op: SetOp,
    filter: &Filter,
    limit: Option<usize>,
) -> Result<Crossed> {
    // The list being crossed against comes first. If it turns out incomplete
    // there is no result to give, so it is worth finding out before spending
    // the second walk.
    let subject = engine::target::label(app, query.target.as_deref());
    let typed = query.target.clone();
    // This is where the two lists differ in a way that matters. If the list
    // being crossed against is incomplete, every account missing from it
    // **shows up in the result without deserving to**: in `unfollowers`,
    // someone who does follow you but was never read out of your followers
    // would be presented as not following you. That is not a partial result,
    // it is a wrong one, so it stops. The base list being incomplete only
    // leaves results out, and the ones shown are true: that gets a warning.
    let (against, against_outcome) =
        common::walk_named(app, &query, op.against(), &subject, |outcome| {
            op.require_against_complete(outcome)
        })
        .await?;

    // The second walk renames the bar: a crossing is two lists, and without
    // this the slower half looked exactly like the first.
    let second = common::walk_named(app, &query, op.base(), &subject, |_| Ok(())).await;
    app.progress().finish();
    let (base, base_outcome) = second?;

    engine::cooldown::check_same_moment(&against_outcome, &base_outcome)?;

    let result = match op {
        SetOp::Unfollowers | SetOp::Fans => sets::difference(&base, &against),
        SetOp::Friends => sets::intersection(&base, &against),
    };
    let common::Narrowed {
        shown: result,
        kept,
        total,
    } = common::narrow(result, filter, limit);
    let name = common::walked_name(app, typed.as_deref(), base_outcome.account_pk)?;
    Ok(Crossed {
        result,
        kept,
        total,
        base_len: base.len(),
        against_len: against.len(),
        base_outcome,
        against_outcome,
        subject,
        name,
    })
}

/// The crossing in the browser, as the account the command runs as and then
/// as each account it is crossed again as. Each account's summary is said as
/// its view is left.
async fn browse(
    app: Box<App>,
    crossed: Crossed,
    args: &ListArgs,
    op: SetOp,
    filter: &Filter,
    store: &SecretStore,
    paths: &AccountPaths,
) -> Result<ExitCode> {
    common::switching(
        (app, crossed),
        async |(app, crossed): &mut (Box<App>, Crossed), note: String| {
            let what = format!("{} of {}", op.name(), crossed.subject);
            let shelf =
                ui::people::Shelf::flat(what.clone(), &crossed.result, app.viewer().clone());
            let rewalk = ui::people::Rewalk {
                secrets: store,
                paths,
                what,
                requests: common::rewalk_cost(
                    app,
                    crossed.base_outcome.account_pk,
                    &[
                        (op.against(), crossed.against_len),
                        (op.base(), crossed.base_len),
                    ],
                )?,
            };
            // Leaving with Ctrl+C outranks what stopped the walk: it is the
            // freshest thing the user said.
            Ok(
                match ui::people::browse(app, &shelf, &rewalk, note).await? {
                    Browsed::Done(ExitCode::Interrupted) => Browsed::Done(ExitCode::Interrupted),
                    Browsed::Done(_) => {
                        Browsed::Done(crossed.base_outcome.exit_code_for_a_printed_result())
                    }
                    switch => switch,
                },
            )
        },
        async |(app, crossed): &(Box<App>, Crossed), to: Viewer| {
            common::rewalk(
                store,
                paths,
                app.viewer(),
                crossed.name.as_deref(),
                &to,
                common::shows_progress(&args.walk),
                async |app: &mut App, name: String| {
                    let query = engine::ListQuery {
                        target: Some(name),
                        ..engine::ListQuery::from(args)
                    };
                    cross(app, query, op, filter, args.limit).await
                },
            )
            .await
        },
        |(app, crossed): &(Box<App>, Crossed), several: bool| {
            print_summary(op, crossed, several.then(|| app.viewer()));
        },
    )
    .await
}

/// The closing lines, naming the account the lists were walked as when the
/// view showed them as more than one.
fn print_summary(op: SetOp, crossed: &Crossed, whom: Option<&Viewer>) {
    let line = summary_line(
        op,
        crossed.result.len(),
        crossed.kept,
        crossed.total,
        crossed.base_len,
        &crossed.base_outcome,
        &crossed.against_outcome,
        whom,
    );
    ui::info(&line);

    if !crossed.base_outcome.is_complete() {
        ui::warn(
            "the starting list is incomplete, so results are missing. \
             The ones shown are correct.",
        );
    }
}

/// The one line a crossing prints about itself.
///
/// Built rather than printed, so a test can read it.
#[expect(
    clippy::too_many_arguments,
    reason = "the parts of one line, apart so a test can give them"
)]
fn summary_line(
    op: SetOp,
    found: usize,
    kept: usize,
    total: usize,
    base_len: usize,
    base_outcome: &ListOutcome,
    against_outcome: &ListOutcome,
    whom: Option<&Viewer>,
) -> String {
    let total_requests = base_outcome.requests + against_outcome.requests;

    let (one, many) = op.description();
    let mut line = report::counted(found, kept, total, one, many);
    if let Some(whom) = whom {
        line.push_str(&format!(" as {}", whom.label()));
    }
    line.push_str(&format!(
        " - {}",
        // The proportion describes the crossing, so it is the count before any
        // filter or cap: "3 of 412 accounts you follow".
        proportion(total, base_len)
    ));

    // When it is stored, say so and say from when, as `snob followers
    // --offline` does with "list stored on Aug 3 at 14:12": a month-old
    // crossing must not read like one walked five minutes ago.
    if base_outcome.is_stored() || against_outcome.is_stored() {
        line.push_str(&format!(
            " - lists stored on {}",
            report::stored_on_the_older_of(base_outcome.taken_at, against_outcome.taken_at)
        ));
    }

    line.push_str(&report::spent(total_requests));
    line
}

fn proportion(part: usize, total: usize) -> String {
    if total == 0 {
        return "no data".into();
    }
    format!("{part} of {total}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Epoch;
    use snob_core::model::StopReason;

    fn outcome(reason: StopReason) -> ListOutcome {
        ListOutcome::for_test(engine::Provenance::Walked, reason)
    }

    fn stored(taken_at: Epoch) -> ListOutcome {
        ListOutcome {
            provenance: engine::Provenance::CacheFlag,
            requests: 0,
            taken_at,
            ..outcome(StopReason::Completed)
        }
    }

    /// A crossing served from storage says so, and says from when.
    ///
    /// `snob unfollowers --offline` a month later must not read like a
    /// crossing walked five minutes ago, while `snob followers --offline` says
    /// "list stored on Aug 3 at 14:12" for the very same capture.
    #[test]
    fn a_crossing_served_from_storage_says_when_it_is_from() {
        // Two captures a day apart. The line has to name the older.
        let older = Epoch::new(1_700_000_000);
        let line = summary_line(
            SetOp::Unfollowers,
            3,
            3,
            3,
            412,
            &stored(older),
            &stored(older + std::time::Duration::from_secs(24 * 3_600)),
            None,
        );

        assert!(
            line.contains(&report::stored_on(older)),
            "a crossing is only as recent as its staler half: {line}"
        );
        assert!(
            line.contains("without touching the network"),
            "nothing was spent, and that is worth saying: {line}"
        );
    }

    /// And a freshly walked one does not claim to be stored.
    #[test]
    fn a_crossing_that_was_walked_says_what_it_spent() {
        let line = summary_line(
            SetOp::Unfollowers,
            3,
            3,
            3,
            412,
            &outcome(StopReason::Completed),
            &outcome(StopReason::Completed),
            None,
        );

        assert!(!line.contains("stored on"), "{line}");
        assert!(!line.contains("without touching the network"), "{line}");
        assert!(line.contains(&report::requests(2)), "{line}");
    }

    #[test]
    fn each_operation_crosses_the_lists_the_right_way_round() {
        assert_eq!(SetOp::Unfollowers.base(), ListKind::Following);
        assert_eq!(SetOp::Unfollowers.against(), ListKind::Followers);
        assert_eq!(SetOp::Fans.base(), ListKind::Followers);
        assert_eq!(SetOp::Fans.against(), ListKind::Following);
        assert_eq!(SetOp::Friends.base(), ListKind::Followers);
        assert_eq!(SetOp::Friends.against(), ListKind::Following);
    }

    /// The refusal has to name the mistake the user would otherwise have made,
    /// and each crossing produces a different one.
    #[test]
    fn each_operation_explains_its_own_misreading() {
        let text = |op: SetOp| {
            op.require_against_complete(&outcome(StopReason::Truncated))
                .unwrap_err()
                .to_string()
        };
        assert!(text(SetOp::Unfollowers).contains("the followers list"));
        assert!(text(SetOp::Unfollowers).contains("they did not follow you"));
        assert!(text(SetOp::Fans).contains("you did not follow them"));
        assert!(text(SetOp::Friends).contains("you were not friends"));
    }

    #[test]
    fn the_proportion_does_not_divide_by_zero() {
        assert_eq!(proportion(0, 0), "no data");
        assert_eq!(proportion(3, 10), "3 of 10");
    }
}
