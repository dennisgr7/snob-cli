//! `snob likers LINK`: the accounts that liked a post, as many as Instagram
//! shows the web app.
//!
//! **A sample, never the whole list.** The app is shown about a hundred
//! (105 of 603,328 twice in the capture of 2026-11-13), the viewer's
//! contacts first, with no cursor to a second page; snob reads what the app
//! is shown and says so, beside the count when the owner did not hide it.
//!
//! Two reads, the app's when a post opens and its "others" is clicked: the
//! post (`posts::fetch_link`), then its likers from the post's page
//! (`IgClient::media_likers`). The post first because that is the order the
//! app sends them in, and because it says whether the owner hid the counts,
//! which the likers' own count is not taken to override. The app's
//! `show_many` after the likers is not sent: their answer already says how
//! the viewer stands with each.
//!
//! **They are other people's accounts**, so this keeps the rule a list walk
//! keeps (AGENTS.md, "Consent"): asked first, before any request, since the
//! question cannot name the owner before the post is read; `-y` answers in
//! advance, and no terminal without `-y` is exit 130 with nothing sent. They
//! are charged to the day's accounts as a list page's are, and a day with
//! fewer than [`LIKERS_AT_MOST`] left is refused before anything is asked.

use anyhow::Result;
use snob_core::model::post::{Liker, Likers};
use snob_ig::shortcode::Shortcode;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::app::App;
use crate::cli::{Format, LikersArgs};
use crate::commands::common;
use crate::exit::{ExitCode, ExitError};
use crate::output::Rendered;
use crate::posts::{self, Post};
use crate::ui;

/// The most accounts one answer was seen to hold (105, twice), with a
/// margin: what the day's accounts must still allow before the request is
/// sent, so a read the day cannot pay for is refused rather than made.
pub const LIKERS_AT_MOST: u32 = 120;

pub async fn run(args: LikersArgs, store: SecretStore, paths: &AccountPaths) -> Result<ExitCode> {
    // Before anything is opened: a link that names no post costs nothing,
    // and a destination that cannot hold the format must not cost a request.
    let link = Shortcode::parse(&args.link).map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("{e}: give a post's or a reel's link"),
        )
    })?;
    let destination = common::destination(&args.output)?;
    // Refuses in a cooldown, before anything is asked.
    let mut app = common::reader(&store, paths, false, false)?;

    // Costs nothing to know, so it is known before the question.
    let left = app.client().pacer().accounts_left()?;
    if left < LIKERS_AT_MOST {
        return Err(ExitError::new(
            ExitCode::Error,
            format!(
                "the accounts this account may read today are nearly spent ({left} left), \
                 and a post's likers can be {LIKERS_AT_MOST}; nothing was sent"
            ),
        )
        .with_hint("they free up over 24 hours; \"snob status\" says how many are left")
        .into());
    }

    ask(&app, args.consent.yes, ui::can_be_asked(), link.code()).await?;
    common::record_agent_act(&mut app, &args.agent, "likers", Some(link.code()), None)?;

    let post = posts::fetch_link(app.client(), &link).await?;
    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }
    let page = app.client().media_likers(post.pk, &post.page()).await?;
    let likers = likers_of(&post, &page);
    ui::info(&shown_line(&likers));

    let accounts: Vec<_> = likers
        .accounts
        .iter()
        .map(|liker| liker.account.clone())
        .collect();
    match destination.format() {
        Format::Json | Format::Ndjson => {
            let rows = likers
                .accounts
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?;
            let mut text = crate::media::rows_or_envelope(rows, destination.format(), |_| {
                serde_json::to_value(&likers).unwrap_or_default()
            })?;
            if !text.is_empty() {
                text.push('\n');
            }
            destination.write_rendered(&Rendered::Text(text))?;
        }
        _ => destination.write(&accounts)?,
    }
    Ok(ExitCode::Ok)
}

/// Asks before the likers of `code` are read, as a walk of somebody else's
/// list asks. `someone_is_there` is `ui::can_be_asked()`, an argument so a
/// test can say it.
async fn ask(app: &App, yes: bool, someone_is_there: bool, code: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !someone_is_there {
        return Err(crate::report::refuse_unattended(
            format!("reading the likers of {code} needs confirmation"),
            "Pass -y to confirm in advance.".to_string(),
        ));
    }
    app.warn(crate::report::READING_A_POSTS_LIKERS);
    if !ui::confirm_off_thread(
        app.progress(),
        format!("Read the accounts that liked {code}?"),
        false,
    )
    .await?
    {
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!("nothing was read: the likers of {code} were not confirmed"),
        )
        .into());
    }
    Ok(())
}

/// The document `snob likers` prints: every account the answer named, its
/// names made safe to print, and the count unless the owner hid it.
fn likers_of(post: &Post, page: &snob_ig::model::post::LikersPage) -> Likers {
    let accounts: Vec<Liker> = page
        .accounts()
        .into_iter()
        .map(|(account, standing)| Liker {
            account: snob_core::model::User {
                username: account.safe_username(),
                full_name: account.safe_full_name(),
                ..account
            },
            you_follow: standing.as_ref().map(|s| s.following),
            follows_you: standing.as_ref().map(|s| s.followed_by),
        })
        .collect();
    // The post's count first, the one the app prints; the likers' own when
    // the post said none. Never either when the owner hid them: what
    // Instagram answers then is not what the owner chose to show.
    let likes = if post.likes_hidden {
        None
    } else {
        post.likes.or(page.user_count)
    };
    let shown = accounts.len();
    Likers {
        code: post.code.clone(),
        url: post.link(),
        likes,
        shown,
        complete: likes.is_some_and(|likes| likes == shown as u64),
        accounts,
    }
}

/// What standard error says about how much of the list this is.
fn shown_line(likers: &Likers) -> String {
    let shown = posts::grouped(likers.shown as u64);
    let accounts = if likers.shown == 1 {
        "account"
    } else {
        "accounts"
    };
    match likers.likes {
        _ if likers.complete => format!("all {shown} {accounts} that liked it"),
        Some(likes) => format!(
            "{shown} of the {} accounts that liked it: Instagram shows a sample, the accounts \
             you know first",
            posts::grouped(likes)
        ),
        None => format!(
            "{shown} {accounts} that liked it; the owner hid how many there are, and Instagram \
             shows a sample"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn likers(likes: Option<u64>, shown: usize) -> Likers {
        Likers {
            code: "DbGwql4IMei".into(),
            url: "https://www.instagram.com/p/DbGwql4IMei/".into(),
            likes,
            shown,
            complete: likes == Some(shown as u64),
            accounts: Vec::new(),
        }
    }

    #[test]
    fn the_likers_say_they_are_a_sample() {
        assert_eq!(
            shown_line(&likers(Some(603_328), 105)),
            "105 of the 603,328 accounts that liked it: Instagram shows a sample, the \
             accounts you know first"
        );
        assert_eq!(
            shown_line(&likers(Some(2), 2)),
            "all 2 accounts that liked it"
        );
    }

    #[test]
    fn hidden_counts_withhold_the_total() {
        let line = shown_line(&likers(None, 40));
        assert!(
            line.starts_with("40 accounts that liked it; the owner hid"),
            "{line}"
        );
        assert!(!line.contains(" of "), "{line}");
    }
}
