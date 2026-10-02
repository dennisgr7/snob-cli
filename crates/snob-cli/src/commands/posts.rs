//! `snob posts`: the posts on an account's grid, and how to keep a copy.
//!
//! `highlights` twice over, in the same shape: `snob posts someone` numbers
//! the grid, and `snob posts someone 3` opens the third post and numbers what
//! it holds, a carousel's photos and videos, or the one file a photo or a
//! reel is. `-d`, `-o`, `--format` and `-i` mean on each listing what they
//! mean on `highlights`'; without a post's number `-d` takes whole posts.
//!
//! **What it costs** is in [`crate::posts`]: the profile, then one read per
//! page of twelve posts. The listing reads the first page, or `--pages`; a
//! post's number past them reads on until the grid holds it, up to one
//! sitting. Listing what a post holds costs nothing more, as the grid's
//! answer carries every item of every post; the view, which shows a post's
//! comments, opens one with the two reads the app opens it with.
//!
//! **A private account the viewer does not follow is told apart from an
//! account with no posts**, as `highlights` tells it.

use std::path::Path;

use anyhow::{Result, anyhow};
use comfy_table::Cell;
use snob_core::model::printable;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{DownloadSelection, Format, PostsArgs, StoryFormat};
use crate::commands::common;
use crate::exit::ExitCode;
use crate::media::{self, empty_document};
use crate::output::Presentation;
use crate::posts::{self, Fetched, Grid, Post, fetch_grid, files_of, post_json};
use crate::report;
use crate::ui;

pub async fn run(args: PostsArgs, store: SecretStore, paths: &AccountPaths) -> Result<ExitCode> {
    let app = common::reader(&store, paths, args.action.interactive, false)?;
    let typed = common::target_or_own(&app, args.target.as_deref()).await?;
    let known = crate::engine::target::known_pk_of(&app, args.target.as_deref(), &typed)?;

    // Decided before anything is read, because it decides how much is: the
    // view reads the first page and the next as it is scrolled to, and the
    // printed form what `--pages` says. `--pages` asks for the printed form
    // as a format does.
    let browses = args.action.browses(
        args.list.format.is_some() || args.pages.is_some(),
        ui::a_human_would_watch_the_listing_scroll_by(),
    );
    let pages = if browses { 1 } else { args.pages.unwrap_or(1) };
    let at_least = args
        .post
        .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));

    let grid = match fetch_grid(
        app.client(),
        &typed,
        app.viewer().pk,
        known,
        pages,
        at_least,
    )
    .await?
    {
        Fetched::Grid(grid) => grid,
        Fetched::Hidden { username } => {
            ui::info(&format!(
                "the posts of @{} are not visible: the account is private and you do not follow it",
                printable(&username)
            ));
            if args.action.selection().is_none() && !args.action.interactive {
                let destination = args.action.output.as_deref();
                let format = common::checked_format(args.list.format, destination, "a grid")?;
                empty_document(format, destination, || {
                    Ok(serde_json::to_string_pretty(&serde_json::json!({
                        "username": username,
                        "posts": null,
                    }))?)
                })?;
            }
            return Ok(ExitCode::Ok);
        }
    };

    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }

    if grid.posts.is_empty() {
        ui::info(&format!("@{} has no posts.", printable(&grid.username)));
        if args.action.selection().is_none() && !args.action.interactive {
            let destination = args.action.output.as_deref();
            let format = common::checked_format(args.list.format, destination, "a grid")?;
            empty_document(format, destination, || grid_json(&grid, Format::Json))?;
        }
        return Ok(ExitCode::Ok);
    }

    if let Some(number) = args.post
        && number as usize > grid.posts.len()
    {
        return Err(no_such_post(&grid, number as usize));
    }

    if browses {
        let start = args.post.map(|number| number as usize - 1);
        return crate::ui::posts::browse_grid(app.client(), grid, start, app.viewer(), paths).await;
    }

    let destination = args.action.output.as_deref();
    match (args.post, args.action.selection()) {
        (None, Some(selection)) => {
            let numbers =
                media::numbers_of(selection, grid.posts.len(), |n| no_such_post(&grid, n))?;
            let chosen: Vec<&Post> = numbers.iter().map(|n| &grid.posts[n - 1]).collect();
            posts::download(app.client_shared(), files_of(&chosen), destination).await
        }
        (None, None) => list_grid(&grid, args.list.format, destination),
        (Some(number), selection) => {
            let post = &grid.posts[number as usize - 1];
            match selection {
                Some(selection) => download_items(&app, post, selection, destination).await,
                None => list_items(
                    &grid.username,
                    number as usize,
                    post,
                    args.list.format,
                    destination,
                ),
            }
        }
    }
}

/// `-d` inside one post: the items by the numbers its listing printed.
pub(crate) async fn download_items(
    app: &crate::app::App,
    post: &Post,
    selection: DownloadSelection,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let numbers = media::numbers_of(selection, post.items.len(), |n| no_such_item(post, n))?;
    let files = numbers.iter().map(|n| post.file(n - 1)).collect();
    posts::download(app.client_shared(), files, destination).await
}

/// The sentence for a post number the grid does not hold.
fn no_such_post(grid: &Grid, number: usize) -> anyhow::Error {
    let read = grid.posts.len();
    let more = if grid.next.is_some() {
        " in the pages read; --pages reads further"
    } else {
        ""
    };
    anyhow!(
        "there is no post {number}: the grid of @{} holds {}{more}",
        printable(&grid.username),
        match read {
            1 => "one".to_string(),
            n => format!("{n} posts"),
        }
    )
}

/// The sentence for an item number a post does not hold.
pub(crate) fn no_such_item(post: &Post, asked: usize) -> anyhow::Error {
    anyhow!(
        "there is no item {asked}: the post {} holds {}",
        post.code,
        match post.items.len() {
            1 => "one".to_string(),
            n => format!("{n} items"),
        }
    )
}

/// Prints the grid. The numbers here are what the second positional and a
/// grid-level `-d` take.
fn list_grid(
    grid: &Grid,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = common::checked_format(format, destination, "a grid")?;
    let text = match format {
        Format::Json | Format::Ndjson => grid_json(grid, format)?,
        _ => grid_table(grid, Presentation::detect(destination)),
    };
    let hint = format!(
        "snob posts {} <number> to look inside one, -d <number> to save one whole, or -i to browse",
        printable(&grid.username)
    );
    media::print_listing(text, destination, &hint)
}

/// Prints what one post holds, after what it says. The numbers here are
/// what `-d` takes.
pub(crate) fn list_items(
    username: &str,
    number: usize,
    post: &Post,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = common::checked_format(format, destination, "a post's listing")?;
    let text = match format {
        Format::Json | Format::Ndjson => {
            let rows = post_json(post, None)["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            media::rows_or_envelope(rows, format, |_| {
                serde_json::json!({
                    "username": username,
                    "post": post_json(post, Some(number)),
                })
            })?
        }
        _ => post_text(post, Presentation::detect(destination)),
    };
    media::print_listing(
        text,
        destination,
        "--download <number> to save one, or --interactive to see it with its comments",
    )
}

/// A post as a person reads it in a terminal: what it says, then its items
/// numbered.
pub(crate) fn post_text(post: &Post, presentation: Presentation) -> String {
    let mut text = String::new();
    for line in about(post) {
        text.push_str(&line);
        text.push('\n');
    }
    let mut table = media::listing_table(&["#", "Kind", "Size"], presentation);
    for (index, item) in post.items.iter().enumerate() {
        table.add_row([
            Cell::new(index + 1),
            Cell::new(item.kind.label()),
            Cell::new(size_of(item.width, item.height)),
        ]);
    }
    text.push_str(&table.to_string());
    text
}

/// The lines that say what a post is, in the order the app shows them
/// under it: whose and with whom, when and where, the caption, who is
/// tagged and mentioned, the likes, the comments and the plays.
pub fn about(post: &Post) -> Vec<String> {
    let mut lines = Vec::new();
    let mut whose = format!("@{}", post.owner);
    if !post.with.is_empty() {
        whose.push_str(&format!(" with {}", names(&post.with)));
    }
    lines.push(format!("{whose} · {}", post.kind.label()));
    let mut when = report::dated(post.taken_at);
    if let Some(place) = &post.location {
        when.push_str(&format!(" · {place}"));
    }
    lines.push(when);
    lines.push(post.link());
    if !post.caption.is_empty() {
        lines.push(String::new());
        lines.extend(post.caption.iter().cloned());
        lines.push(String::new());
    }
    if !post.tagged.is_empty() {
        lines.push(format!("Tagged: {}", names(&post.tagged)));
    }
    if !post.mentions.is_empty() {
        lines.push(format!("Mentions: {}", names(&post.mentions)));
    }
    let mut counts: Vec<String> = Vec::new();
    if let Some(liked) = post.liked_line() {
        counts.push(liked);
    }
    if let Some(n) = post.comments {
        counts.push(match n {
            1 => "1 comment".into(),
            n => format!("{} comments", posts::grouped(n)),
        });
    }
    if let Some(n) = post.plays {
        counts.push(match n {
            1 => "1 play".into(),
            n => format!("{} plays", posts::grouped(n)),
        });
    }
    if !counts.is_empty() {
        lines.push(counts.join(" · "));
    }
    lines
}

/// `@a, @b`.
fn names(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("@{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `1080×1350`, or a dash when the answer did not say.
pub(crate) fn size_of(width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(w), Some(h)) => format!("{w}×{h}"),
        _ => "-".into(),
    }
}

fn grid_table(grid: &Grid, presentation: Presentation) -> String {
    let mut table = media::listing_table(
        &["#", "Kind", "Posted", "Likes", "Comments", "Caption"],
        presentation,
    );
    for (index, post) in grid.posts.iter().enumerate() {
        let likes = if post.likes_hidden {
            "-".to_string()
        } else {
            post.likes.map_or("-".into(), posts::grouped)
        };
        table.add_row([
            Cell::new(index + 1),
            Cell::new(post.kind.label()),
            Cell::new(report::dated(post.taken_at)),
            Cell::new(likes),
            Cell::new(post.comments.map_or("-".into(), posts::grouped)),
            Cell::new(clipped(post.headline(), 60)),
        ]);
    }
    table.to_string()
}

/// The first `width` characters of `text`, and an ellipsis when it went on.
pub(crate) fn clipped(text: &str, width: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(width).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn grid_json(grid: &Grid, format: Format) -> Result<String> {
    let rows: Vec<serde_json::Value> = grid
        .posts
        .iter()
        .enumerate()
        .map(|(index, post)| post_json(post, Some(index + 1)))
        .collect();
    media::rows_or_envelope(rows, format, |rows| {
        serde_json::json!({
            "username": grid.username,
            "declared_posts": grid.declared,
            "more": grid.next.is_some(),
            "posts": rows,
        })
    })
}

#[cfg(test)]
mod tests {
    use snob_ig::model::post::Media;

    use super::*;

    fn post(json: &str) -> Post {
        Post::from_media(&serde_json::from_str::<Media>(json).unwrap(), "someone")
    }

    #[test]
    fn a_post_is_said_in_the_order_the_app_shows_it() {
        let post = post(
            r#"{"pk":"3995556159532654737","code":"DdzEShhEWCR","media_type":1,"taken_at":1790510400,
            "caption":{"text":"sun\nwith @friend"},"like_count":80,"comment_count":8,
            "top_likers":["close"],"usertags":{"in":[{"user":{"username":"tagged"}}]},
            "coauthor_producers":[{"username":"partner"}],"location":{"name":"A beach"}}"#,
        );
        let lines = about(&post);
        assert_eq!(lines[0], "@someone with @partner · photo");
        assert!(lines[1].ends_with(" · A beach"), "{lines:?}");
        assert_eq!(lines[2], "https://www.instagram.com/p/DdzEShhEWCR/");
        assert_eq!(lines[4..6], ["sun", "with @friend"]);
        assert!(lines.contains(&"Tagged: @tagged".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"Mentions: @friend".to_string()),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "Liked by close and 79 others · 8 comments"
        );
    }

    #[test]
    fn a_long_caption_is_clipped_in_a_row() {
        assert_eq!(clipped("abcdef", 3), "abc…");
        assert_eq!(clipped("abc", 3), "abc");
    }
}
