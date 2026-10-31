//! `snob follow` and `snob unfollow`: the only two commands that change
//! anything on Instagram.
//!
//! Two requests: the profile, to turn a name into an id and to find out what
//! the relationship is already, and the write itself. Both are paid for, the
//! second out of the write budget, and neither happens during a cooldown.
//!
//! **One account per run, and there is no flag that changes it.** The reasoning
//! is in `AGENTS.md` with the rest of the write regime, and the short version is
//! that a burst is what strains a service, not a daily total — so the shape of
//! this command is the safeguard, not a limit inside it. Somebody determined to
//! run it in a loop can write the loop; what this refuses to do is ship one.
//!
//! The other half of the design is that it asks first, because a follow reaches
//! another person in a way a read never does. Unfollowing afterwards does not
//! take back the notification they already got, and follow-then-unfollow churn
//! is a growth-hacking trick rather than the housekeeping this tool is for. So
//! the question is asked before the request, and `-y` is how a script answers it
//! in advance.
//!
//! A run an agent was told yes to through `snob mcp` (`--confirmed-via`) is
//! recorded before its first request, and refused with nothing sent once the
//! day's writes made that way reach the cap (`common::record_agent_act`).
//!
//! `--json` prints one object on standard output for every way the run ends
//! (done, already so, declined), so a program never has to read the sentence;
//! the failures are the JSON error `main` prints when `--json` was asked for.

use anyhow::{Result, anyhow};
use snob_core::model::printable;
use snob_ig::graphql::DocIds;
use snob_ig::model::FriendshipStatus;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SessionStore;

use crate::cli::FollowArgs;
use crate::commands::common;
use crate::engine::target;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// Which of the two verbs is being run.
///
/// Not a boolean. Every sentence this command prints differs between the two,
/// and a boolean named `unfollow` reads backwards at exactly the call sites
/// where getting it backwards is worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Follow,
    Unfollow,
}

impl Verb {
    fn present(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Unfollow => "unfollow",
        }
    }
}

pub async fn run(
    args: FollowArgs,
    verb: Verb,
    secrets: SessionStore,
    paths: &AccountPaths,
) -> Result<ExitCode> {
    // No progress bar: two requests, and the write may sit on the write
    // budget for a quarter of an hour (from the browser, before the profile
    // is loaded), which is a wait to be told about in a sentence rather than
    // animated.
    if args.check_ahead {
        return check_ahead(&args, verb, &secrets, paths);
    }
    let mut app = common::app(&secrets, paths, false)?;

    // **Before anything is spent.** A session cannot write only when requests
    // are sent directly (SNOB_NO_BROWSER) and it was pasted without
    // `--csrftoken`, and finding out from Instagram's 403 would cost a
    // request, a slot of write budget, and a message telling the user their
    // session had expired when it had not.
    if !app.client().can_write() {
        return Err(cannot_write(verb));
    }

    common::refuse_during_cooldown(&app, NOTHING_SENT)?;

    // **Also before anything is spent.** Whether anybody can answer is a
    // property of this process's streams, not of the account, so it is known
    // now — and a run that is going to refuse for want of an answer should not
    // pay for a lookup first.
    //
    // The question itself still waits until after the lookup, deliberately: it
    // names the account the way Instagram spells it, it says "send a follow
    // request" rather than "follow" for a private account, and it is not asked
    // at all when the relationship already holds. All three need the profile.
    if !args.consent.yes && !ui::can_be_asked() {
        // The advice goes in the message, where it arrives: a hint on exit 130
        // is never shown, because `report::rendered` returns early for that
        // code.
        return Err(crate::report::refuse_unattended(
            format!("nothing was {}ed: it needs confirmation", verb.present()),
            "Pass -y to confirm in advance.".to_string(),
        ));
    }

    // **Before the first request**, for a run an agent was told yes to
    // through `snob mcp`: recorded where the person sees it, and refused with
    // nothing sent past the day's cap on an agent's writes. An attempt that
    // then finds nothing to do, or is refused by Instagram, still counts.
    let act = common::record_agent_act(
        &mut app,
        &args.agent,
        verb.present(),
        Some(&args.target),
        args.mcp_write_cap,
    )?;

    let known = target::known_pk(&app, &args.target)?;
    let profile = app
        .client()
        .profile_named(target::clean(&args.target), known)
        .await?;
    if let Some(id) = act
        && let Err(e) = snob_store::store::mcp_acts::resolved(app.db().conn(), id, profile.id)
    {
        // The act stays recorded by the name it was asked about.
        tracing::debug!(error = %e, "the account an act was about could not be stored");
    }

    // Instagram spells the name; the user typed a spelling of it. Everything
    // from here on uses Instagram's, filtered, because it is going to a
    // terminal and it came off a server.
    let name = printable(&profile.username);

    // Refusing to follow yourself here rather than letting Instagram do it:
    // its answer is a generic 400 that `classify` cannot tell apart from a
    // real failure, and the user would be told the account is throttled.
    if Some(profile.id) == Some(app.client().session().ds_user_id) {
        return Err(anyhow!("you cannot {} your own account", verb.present()));
    }

    // Nothing to do is worth saying rather than doing. It also saves the write
    // budget for a write that changes something, which matters when a slot is
    // fifteen minutes.
    if let Some(settled) = already_done(verb, &profile) {
        return tell(&args, verb, &profile, &name, settled, None);
    }

    let question = match verb {
        Verb::Follow if profile.is_private == Some(true) => {
            format!("Send @{name} a follow request?")
        }
        Verb::Follow => format!("Follow @{name}?"),
        Verb::Unfollow => format!("Unfollow @{name}?"),
    };

    // `confirm` answers with its default the moment nobody can answer, which
    // would turn "nobody was there" into "they said no" — two different
    // events. The guard above is what keeps that unreachable here, so this
    // branch only ever runs with somebody at the keyboard.
    if !args.consent.yes && !ui::confirm(&question, false)? {
        tell(&args, verb, &profile, &name, Outcome::Declined, None)?;
        return Ok(ExitCode::Interrupted);
    }

    let ids = RememberedIds { db: app.db() };
    let (client, pk, username) = (app.client(), profile.id, &profile.username);
    let status = match verb {
        Verb::Follow => client.follow(pk, username, &ids).await?,
        Verb::Unfollow => client.unfollow(pk, username, &ids).await?,
    };
    tell(
        &args,
        verb,
        &profile,
        &name,
        outcome(verb, &status),
        Some(&status),
    )
}

/// What a write refused in a cooldown says it did not do.
const NOTHING_SENT: &str = "nothing can be sent";

/// The refusal of a session that has no CSRF token to write with.
fn cannot_write(verb: Verb) -> anyhow::Error {
    ExitError::new(
        ExitCode::NoSession,
        format!(
            "this session can read but not {}: it has no CSRF token",
            verb.present()
        ),
    )
    .with_hint(
        "run \"snob login --browser\", which captures it, or \
         \"snob login --paste --csrftoken <token>\"",
    )
    .into()
}

/// `--check-ahead`: what would refuse this write now if an agent made it,
/// **sending nothing, asking nothing and writing nothing**, or `{"would":
/// "send"}` on standard output when nothing would. `snob mcp` runs it before
/// it asks the person for a yes to a follow or an unfollow, so nobody says
/// yes to one that is then refused, and no ticket is spent on one
/// (AGENTS.md, "The MCP server").
///
/// The refusals of [`run`] that cost nothing to know, in its order, each the
/// same error a run would give: another run started by an agent holding the
/// account (a job's walk, or anything under `--spend-alone`), no CSRF token,
/// the cooldown, the day's cap on an agent's writes (`--mcp-write-cap`, or
/// its default), and a wait for the budget past `--max-wait`. Read from the
/// budget's state and the account's database as `--dry-run` reads them
/// (`common::standing`). **Not known ahead**: what the profile says (already
/// so, the viewer's own account, a name that resolves to nothing), and
/// whatever changes between this and the yes.
fn check_ahead(
    args: &FollowArgs,
    verb: Verb,
    secrets: &SessionStore,
    paths: &AccountPaths,
) -> Result<ExitCode> {
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
    let now = snob_core::clock::now_ms();
    // A job's walk holds the account for as long as it walks, from before
    // its walk takes `spend.lock` (queued) to its end.
    if let Some(going) = crate::jobs::all(paths, now.get())?
        .into_iter()
        .find(|seen| !seen.status.is_over())
    {
        return Err(ExitError::new(
            ExitCode::Error,
            format!(
                "{} of this account is walking a list, and only one run started by an agent \
                 may use the account on Instagram at a time; nothing was sent",
                going.id
            ),
        )
        .with_reason("busy")
        .with_hint(format!(
            "ask again once it has ended, which \"snob jobs show {id}\" tells, or stop it \
             with \"snob jobs cancel {id}\"",
            id = going.id
        ))
        .into());
    }
    if snob_store::paths::lock_is_held(&paths.spend_lock())?.unwrap_or(false) {
        return Err(crate::app::held_elsewhere());
    }
    if session.csrftoken.is_none() && !snob_ig::client::page::a_client_would_use_a_page() {
        return Err(cannot_write(verb));
    }
    let (store, budget) = common::standing(paths, now)?;
    if let Some(held) = common::held_in(&budget, paths, now) {
        return Err(common::in_cooldown(&held, NOTHING_SENT));
    }
    let cap = args
        .mcp_write_cap
        .unwrap_or(snob_store::store::mcp_acts::WRITES_A_DAY);
    if let Some(snob_store::store::mcp_acts::Recorded::Capped { writes, free_at }) =
        snob_store::store::mcp_acts::over_the_cap(store.conn(), cap, now)?
    {
        return Err(common::write_cap_reached(writes, free_at));
    }
    // The write pays the pace and the day as the profile before it does, and
    // the write bucket on top, so its wait is the longest of the run's.
    let owed = budget.next_write_at.get().saturating_sub(now.get());
    let wait = std::time::Duration::from_millis(u64::try_from(owed).unwrap_or(0));
    if let Some(limit) = crate::app::max_wait()
        && wait > limit
    {
        return Err(snob_ig::error::IgError::WaitTooLong { wait, limit }.into());
    }
    crate::ui::say!("{}", serde_json::json!({ "would": "send" }));
    Ok(ExitCode::Ok)
}

/// How a run ended, as a word a program can match on and a sentence a person
/// can read. One list, so `--json` and the sentence cannot describe two
/// different runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    AlreadyFollowing,
    AlreadyRequested,
    NotFollowing,
    /// The question was asked and answered no; nothing was sent.
    Declined,
    Followed,
    /// A private account: following it asks, and they have to accept.
    Requested,
    Unfollowed,
    /// Instagram took the write and reported the relationship as it was.
    Unchanged,
}

impl Outcome {
    /// The `outcome` field of `--json`. Part of the JSON contract.
    fn token(self) -> &'static str {
        match self {
            Self::AlreadyFollowing => "already_following",
            Self::AlreadyRequested => "already_requested",
            Self::NotFollowing => "not_following",
            Self::Declined => "declined",
            Self::Followed => "followed",
            Self::Requested => "requested",
            Self::Unfollowed => "unfollowed",
            Self::Unchanged => "unchanged",
        }
    }

    /// Whether this run changed anything on Instagram.
    fn changed(self) -> bool {
        matches!(self, Self::Followed | Self::Requested | Self::Unfollowed)
    }

    /// The sentence a person is told. `None` for a refusal they gave
    /// themselves, which needs no telling back.
    fn sentence(self, verb: Verb, name: &str) -> Option<String> {
        Some(match self {
            Self::AlreadyFollowing => format!("You already follow @{name}."),
            Self::AlreadyRequested => {
                format!("You have already asked to follow @{name}, and they have not answered yet.")
            }
            Self::NotFollowing => format!("You do not follow @{name}."),
            Self::Declined => return None,
            Self::Followed => format!("Now following @{name}."),
            Self::Requested => format!("Asked to follow @{name}. They have to accept it."),
            Self::Unfollowed => format!("No longer following @{name}."),
            // Instagram answered, and answered that nothing is different.
            // Better said plainly than dressed up as success.
            Self::Unchanged => match verb {
                Verb::Follow => {
                    format!("Instagram accepted the request, but @{name} is still not followed.")
                }
                Verb::Unfollow => {
                    format!("Instagram accepted the request, but @{name} is still followed.")
                }
            },
        })
    }
}

/// Says how the run ended, as a sentence on standard error or, with `--json`,
/// as one object on standard output. Exit 0 for everything but `Declined`,
/// whose 130 the caller returns.
fn tell(
    args: &FollowArgs,
    verb: Verb,
    profile: &snob_ig::model::WebProfileInfo,
    name: &str,
    outcome: Outcome,
    status: Option<&FriendshipStatus>,
) -> Result<ExitCode> {
    if args.output.json {
        // What the relationship is now: Instagram's answer to the write when
        // there was one, and the profile's own fields when there was not
        // (`null` where the profile left them out, as `already_done` reads
        // them).
        let (following, requested) = match status {
            Some(status) => (Some(status.following), Some(status.outgoing_request)),
            None => (profile.followed_by_viewer, profile.requested_by_viewer),
        };
        ui::say!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "action": verb.present(),
                "target": {
                    "pk": profile.id,
                    "username": name,
                    "is_private": profile.is_private,
                },
                "outcome": outcome.token(),
                "changed": outcome.changed(),
                "following": following,
                "requested": requested,
                "viewer": crate::report::acting_as().map(|viewer| viewer.json()),
            }))?
        );
    } else if let Some(sentence) = outcome.sentence(verb, name) {
        ui::info(&sentence);
    }
    Ok(ExitCode::Ok)
}

/// What the profile already says, when it says the request would change
/// nothing.
///
/// `followed_by_viewer` and `requested_by_viewer` are `Option` because the
/// endpoint omits them on some answers, and `None` means unknown. Unknown must
/// send the request rather than assume: refusing on a missing field would make
/// the command stop working the day Instagram drops it from the response.
fn already_done(verb: Verb, profile: &snob_ig::model::WebProfileInfo) -> Option<Outcome> {
    match verb {
        Verb::Follow if profile.followed_by_viewer == Some(true) => Some(Outcome::AlreadyFollowing),
        Verb::Follow if profile.requested_by_viewer == Some(true) => {
            Some(Outcome::AlreadyRequested)
        }
        // Both facts have to be *known*: an absent `requested_by_viewer` is not
        // "no pending request", which is the assumption the doc-comment above
        // forbids -- an unfollow that would withdraw a pending request must
        // not be refused on a field Instagram merely left out.
        Verb::Unfollow
            if profile.followed_by_viewer == Some(false)
                && profile.requested_by_viewer == Some(false) =>
        {
            Some(Outcome::NotFollowing)
        }
        _ => None,
    }
}

/// The mutation ids, remembered in the `meta` table between runs.
///
/// **Discovery is what makes a rotated id fix itself, and caching is what makes
/// discovery affordable.** Finding one means fetching JavaScript bundles until
/// a mutation turns up in one, and those are megabytes; once per rotation is
/// nothing, once per follow would be absurd.
///
/// Failures are swallowed on both sides, and the same reasoning applies to
/// each: a read that fails means discovery runs, which is slow and correct, and
/// a write that fails means the next run discovers again. Neither is worth
/// failing a follow over, and neither is worth a message the user cannot act
/// on.
struct RememberedIds<'a> {
    db: &'a snob_store::store::Store,
}

impl DocIds for RememberedIds<'_> {
    fn get(&self, friendly_name: &str) -> Option<String> {
        self.db.remembered(&key(friendly_name)).ok().flatten()
    }

    fn put(&self, friendly_name: &str, doc_id: &str) {
        if let Err(e) = self.db.remember(&key(friendly_name), doc_id) {
            tracing::debug!(error = %e, "the mutation id could not be remembered");
        }
    }
}

/// Namespaced, because `meta` holds facts about the database and these are
/// facts about Instagram. Keyed by the mutation's own name so that a renamed
/// operation gets a new key rather than a stale value under an old one.
fn key(friendly_name: &str) -> String {
    format!("graphql.doc_id.{friendly_name}")
}

/// What actually happened, as Instagram describes it afterwards.
///
/// **Following a private account does not follow it — it asks.** Saying
/// "Followed" there would be the command lying about the one thing it was run
/// to do, and the user would go looking for stories that will not be there.
fn outcome(verb: Verb, status: &FriendshipStatus) -> Outcome {
    match verb {
        Verb::Follow if status.following => Outcome::Followed,
        Verb::Follow if status.outgoing_request => Outcome::Requested,
        Verb::Unfollow if !status.following && !status.outgoing_request => Outcome::Unfollowed,
        _ => Outcome::Unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Pk;
    use snob_ig::model::WebProfileInfo;

    fn profile(followed: Option<bool>, requested: Option<bool>) -> WebProfileInfo {
        WebProfileInfo {
            id: Pk::new(7),
            username: "someone".into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            followed_by_viewer: followed,
            requested_by_viewer: requested,
            profile_pic_url: None,
            profile_pic_url_hd: None,
            followers: None,
            following: None,
            follows_viewer: None,
            has_requested_viewer: None,
            biography: None,
            external_url: None,
            posts: None,
            mutual: None,
            highlight_reel_count: None,
            is_business_account: None,
            category_name: None,
            anonymous_picture: None,
            latest_reel_media: None,
            // The route that answered. Search is the fallback for the accounts
            // `web_profile_info` cannot serialize; these fixtures are the
            // ordinary case.
            via: snob_ig::model::Via::Profile,
        }
    }

    #[test]
    fn a_relationship_that_already_holds_costs_no_request() {
        assert!(already_done(Verb::Follow, &profile(Some(true), None)).is_some());
        assert!(already_done(Verb::Unfollow, &profile(Some(false), Some(false))).is_some());
        assert!(already_done(Verb::Follow, &profile(Some(false), Some(true))).is_some());
    }

    /// The endpoint omits these fields on some answers. Unknown has to mean
    /// "send it and find out", or the command stops working the day Instagram
    /// drops the field.
    #[test]
    fn an_unknown_relationship_is_not_treated_as_settled() {
        assert!(already_done(Verb::Follow, &profile(None, None)).is_none());
        assert!(already_done(Verb::Unfollow, &profile(None, None)).is_none());
        // Half-known is still unknown: not following, but with the pending
        // request unreported, unfollow may still have a request to withdraw.
        assert!(already_done(Verb::Unfollow, &profile(Some(false), None)).is_none());
    }

    /// A pending request is not a follow, and unfollow has something to do
    /// about it: withdrawing the request.
    #[test]
    fn a_pending_request_is_still_something_to_withdraw() {
        assert!(already_done(Verb::Unfollow, &profile(Some(false), Some(true))).is_none());
    }

    #[test]
    fn following_a_private_account_is_reported_as_the_request_it_is() {
        let asked = FriendshipStatus {
            following: false,
            outgoing_request: true,
            ..Default::default()
        };
        let told = outcome(Verb::Follow, &asked);
        assert_eq!(told, Outcome::Requested);
        let said = told.sentence(Verb::Follow, "someone").unwrap();
        assert!(said.contains("Asked to follow"), "{said}");
        assert!(!said.contains("Now following"), "{said}");
    }

    #[test]
    fn a_write_instagram_accepted_but_did_not_apply_is_not_called_a_success() {
        let unchanged = FriendshipStatus::default();
        let told = outcome(Verb::Follow, &unchanged);
        assert!(!told.changed());
        let said = told.sentence(Verb::Follow, "someone").unwrap();
        assert!(said.contains("still not followed"), "{said}");
    }
}
