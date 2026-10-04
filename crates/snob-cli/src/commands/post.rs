//! `snob post LINK` (and `snob reel LINK`): one post or reel by its address,
//! and how to keep a copy.
//!
//! The link is read for its code alone ([`snob_ig::shortcode`]): the code is
//! the post's pk, so nothing is looked up, and nothing else the address
//! carries -- where it was shared from, and the token that ties the visit
//! to whoever shared it -- is read or sent. The post is asked for the way
//! the app asks for one it opens, its info from the page it opens on, which
//! is never loaded, and its first page of comments with it, as the app
//! always asks the two together when a post opens: two reads, printed,
//! saved or browsed alike.
//!
//! What it prints and saves is `snob posts someone 3`'s, for the same
//! post: the same lines, the same numbers, the same file names.

use std::path::Path;

use anyhow::Result;
use snob_ig::shortcode::Shortcode;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{Format, PostArgs, StoryFormat};
use crate::commands::common;
use crate::commands::posts::{download_items, post_text};
use crate::exit::{ExitCode, ExitError};
use crate::media;
use crate::output::Presentation;
use crate::posts::{self, CommentLine, Comments, Post, comment_json, post_json};
use crate::report;
use crate::ui;

pub async fn run(args: PostArgs, store: SecretStore, paths: &AccountPaths) -> Result<ExitCode> {
    // Before anything is opened or asked: a link that names no post costs
    // nothing. The link is not repeated in the sentence, as it can carry the
    // share token of whoever sent it.
    let link = Shortcode::parse(&args.link).map_err(|e| {
        ExitError::new(
            ExitCode::Error,
            format!("{e}: give a post's or a reel's link"),
        )
    })?;
    args.action.refuse_stdout_download_early()?;
    let app = common::reader(&store, paths, args.action.interactive, false)?;
    let browses = args.action.browses(
        args.list.format.is_some(),
        ui::a_human_would_watch_the_listing_scroll_by(),
    );

    let post = posts::fetch_link(app.client(), &link).await?;
    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }

    if browses {
        return crate::ui::posts::browse_post(app.client(), post, app.viewer(), paths).await;
    }
    // The app never opens a post without its comments, so neither does the
    // printed form nor a download.
    let comments = posts::first_comments(app.client(), &post).await?;
    let destination = args.action.output.as_deref();
    if let Some(selection) = args.action.selection() {
        return download_items(&app, &post, selection, destination).await;
    }
    list_post(&post, Some(&comments), args.list.format, destination)
}

/// Prints the post, its items numbered, and its comments when they were
/// read. The numbers are what `-d` takes.
fn list_post(
    post: &Post,
    comments: Option<&Comments>,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = common::checked_format(format, destination, "a post")?;
    let text = match format {
        Format::Json | Format::Ndjson => {
            let rows = post_json(post, None)["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            media::rows_or_envelope(rows, format, |_| {
                let mut document = serde_json::json!({ "post": post_json(post, None) });
                if let Some(comments) = comments {
                    document["comments"] = serde_json::json!(
                        comments.lines.iter().map(comment_json).collect::<Vec<_>>()
                    );
                    document["more_comments"] = serde_json::json!(comments.next.is_some());
                }
                document
            })?
        }
        _ => {
            let mut text = post_text(post, Presentation::detect(destination));
            if let Some(comments) = comments {
                text.push('\n');
                text.push_str(&comments_text(comments));
            }
            text
        }
    };
    media::print_listing(
        text,
        destination,
        "--download <number> to save one, --download all for every one, or --interactive to see it with its comments",
    )
}

/// The comments read, one to a line, each shown reply under its comment.
fn comments_text(comments: &Comments) -> String {
    let mut text = match comments.total {
        Some(total) => format!("Comments ({}):\n", posts::grouped(total)),
        None => "Comments:\n".to_string(),
    };
    if comments.lines.is_empty() {
        text.push_str("  none\n");
    }
    for comment in &comments.lines {
        text.push_str(&format!("  {}\n", comment_line(comment)));
        for reply in &comment.replies {
            text.push_str(&format!("      ↳ {}\n", comment_line(reply)));
        }
    }
    if comments.next.is_some() {
        text.push_str("  … more in the view (-i)\n");
    }
    text.pop();
    text
}

/// `@author: text · Aug 3, 2026 · 3 likes · 2 replies`.
pub fn comment_line(comment: &CommentLine) -> String {
    let mut line = format!(
        "@{}: {} · {}",
        comment.author,
        if comment.text.is_empty() {
            "(a sticker or a GIF)"
        } else {
            &comment.text
        },
        report::dated(comment.at)
    );
    if comment.likes > 0 {
        line.push_str(&match comment.likes {
            1 => " · 1 like".to_string(),
            n => format!(" · {} likes", posts::grouped(n)),
        });
    }
    if comment.replies_count > 0 {
        line.push_str(&match comment.replies_count {
            1 => " · 1 reply".to_string(),
            n => format!(" · {} replies", posts::grouped(n)),
        });
    }
    line
}

#[cfg(test)]
mod tests {
    use snob_core::Epoch;

    use super::*;

    fn comment(author: &str, text: &str, likes: u64, replies: u64) -> CommentLine {
        CommentLine {
            author: author.into(),
            text: text.into(),
            at: Epoch::new(1_790_510_400),
            likes,
            replies_count: replies,
            replies: Vec::new(),
        }
    }

    #[test]
    fn a_comment_says_its_likes_and_replies() {
        let line = comment_line(&comment("a", "nice", 3, 1));
        assert!(line.starts_with("@a: nice · "), "{line}");
        assert!(line.ends_with(" · 3 likes · 1 reply"), "{line}");
        let quiet = comment_line(&comment("b", "", 0, 0));
        assert!(quiet.starts_with("@b: (a sticker or a GIF) · "), "{quiet}");
        assert!(!quiet.contains("like"), "{quiet}");
    }

    #[test]
    fn the_comments_show_their_replies_and_whether_more_are_left() {
        let mut first = comment("a", "nice", 0, 2);
        first.replies = vec![comment("b", "yes", 0, 0)];
        let text = comments_text(&Comments {
            lines: vec![first],
            next: Some("x".into()),
            total: Some(1_200),
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Comments (1,200):");
        assert!(lines[1].starts_with("  @a: nice"), "{text}");
        assert!(lines[2].starts_with("      ↳ @b: yes"), "{text}");
        assert_eq!(lines[3], "  … more in the view (-i)");
    }
}
