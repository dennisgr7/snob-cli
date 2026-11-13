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
//! **A reader that comes back** names the newest posts it already has with
//! `--until`, and the read stops at the first page that holds one
//! ([`posts::reached`]): the listing then says how many pages it asked for
//! (`pages_read`) and whether it got there (`until`), so what is new is told
//! from what was only not read.
//!
//! **A private account the viewer does not follow is told apart from an
//! account with no posts**, as `highlights` tells it.

use std::path::Path;

use anyhow::{Result, anyhow};
use comfy_table::Cell;
use snob_core::model::printable;
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{
    DownloadSelection, Format, MediaActionArgs, PostsArgs, Quality, StoryFormat, VideoArgs,
};
use crate::commands::common;
use crate::exit::ExitCode;
use crate::media::{self, empty_document};
use crate::output;
use crate::output::Presentation;
use crate::posts::{self, Fetched, Grid, GridRead, Post, Videos, files_of, post_json, read_grid};
use crate::report;
use crate::ui;

pub async fn run(args: PostsArgs, store: SecretStore, paths: &AccountPaths) -> Result<ExitCode> {
    args.action.refuse_stdout_download_early()?;
    let videos = videos_of(&args.video, &args.action)?;
    let app = common::reader(&store, paths, args.action.interactive, false)?;
    let typed = common::target_or_own(&app, args.target.as_deref()).await?;
    let known = crate::engine::target::known_pk_of(&app, args.target.as_deref(), &typed)?;

    // Decided before anything is read, because it decides how much is: the
    // view reads the first page and the next as it is scrolled to, and the
    // printed form what `--pages` says. `--pages` asks for the printed form
    // as a format does.
    // `--until` asks for the printed form too: it is about how far a
    // listing reads.
    let browses = args.action.browses(
        args.list.format.is_some() || args.pages.is_some() || !args.until.is_empty(),
        ui::a_human_would_watch_the_listing_scroll_by(),
    );
    let until: Vec<_> = args.until.iter().map(|link| link.pk()).collect();
    let pages = match args.pages {
        _ if browses => 1,
        Some(pages) => pages,
        None if until.is_empty() => 1,
        None => posts::UNTIL_PAGES,
    };
    let at_least = args
        .post
        .map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));

    let (fetched, read) = read_grid(
        app.client(),
        &typed,
        app.viewer().pk,
        known,
        pages,
        at_least,
        &until,
    )
    .await?;
    let asked = Asked {
        until: !until.is_empty(),
        read,
    };
    let grid = match fetched {
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
            empty_document(format, destination, || {
                grid_json(&grid, &asked, Format::Json)
            })?;
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
            let files = videos.prepare(files_of(&chosen));
            posts::download(app.client_shared(), files, destination).await
        }
        (None, None) => {
            // A post's picture is its first item's: what its square on the
            // grid shows.
            let pictures = match args.list.small_pictures {
                true => {
                    let firsts = grid
                        .posts
                        .iter()
                        .enumerate()
                        .map(|(index, post)| {
                            (index + 1, post.items.first().and_then(|i| i.thumb.clone()))
                        })
                        .collect();
                    Some(media::small_pictures(app.client(), firsts).await?)
                }
                false => None,
            };
            list_grid(&grid, &asked, pictures, args.list.format, destination)
        }
        (Some(number), selection) => {
            let post = &grid.posts[number as usize - 1];
            match selection {
                Some(selection) => {
                    download_items(&app, post, selection, destination, &videos).await
                }
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
    videos: &Videos,
) -> Result<ExitCode> {
    let numbers = media::numbers_of(selection, post.items.len(), |n| no_such_item(post, n))?;
    let files = videos.prepare(numbers.iter().map(|n| post.file(n - 1)).collect());
    posts::download(app.client_shared(), files, destination).await
}

/// What `-d` saves a video as, decided before anything is asked of
/// Instagram, so a `--quality best` with no ffmpeg costs nothing. One file
/// to standard output is the progressive one: a join cannot be streamed.
pub(crate) fn videos_of(video: &VideoArgs, action: &MediaActionArgs) -> Result<Videos> {
    if action.selection().is_none() {
        return Ok(Videos::Compat { say_how: false });
    }
    if action.output.as_deref().is_some_and(output::is_stdout) {
        if video.quality == Some(Quality::Best) {
            anyhow::bail!(
                "-o - streams one file as it arrives, and a video at its best is two \
                 joined once both are whole; save it to a file, or leave out --quality best"
            );
        }
        return Ok(Videos::Compat { say_how: false });
    }
    Videos::resolve(video.quality, video.ffmpeg.as_deref())
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

/// How a grid's listing was asked for and read: whether `--until` named a
/// post to stop at, and how the read went.
#[derive(Debug, Clone, Copy, Default)]
struct Asked {
    until: bool,
    read: GridRead,
}

impl Asked {
    /// What a person is told of a read `--until` shaped, on standard error:
    /// a listing that stopped where it was asked to, or one that never got
    /// there, looks the same in a table.
    fn say(&self, grid: &Grid) {
        if !self.until {
            return;
        }
        let pages = match self.read.pages {
            1 => "one page".to_string(),
            n => format!("{n} pages"),
        };
        match self.read.reached {
            Some(pk) => ui::info(&format!(
                "Read {pages}, as far as the post {}.",
                snob_ig::shortcode::code_of(pk)
            )),
            None if grid.next.is_some() => ui::info(&format!(
                "Read {pages} and none holds a post named with --until: there are more \
                 new posts than these, or that post is gone or pinned. --pages reads further."
            )),
            None => ui::info(&format!(
                "Read {pages}, the whole grid, and none holds a post named with --until."
            )),
        }
    }
}

/// Prints the grid. The numbers here are what the second positional and a
/// grid-level `-d` take.
fn list_grid(
    grid: &Grid,
    asked: &Asked,
    pictures: Option<Vec<serde_json::Value>>,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = common::checked_format(format, destination, "a grid")?;
    let text = match format {
        Format::Json | Format::Ndjson => {
            media::with_pictures(grid_json(grid, asked, format)?, pictures)?
        }
        _ => grid_table(grid, Presentation::detect(destination)),
    };
    asked.say(grid);
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
    let mut what = format!("{whose} · {}", post.kind.label());
    if let Some(length) = post.duration.map(posts::length) {
        what.push_str(&format!(" · {length}"));
    }
    lines.push(what);
    let mut when = report::dated(post.taken_at);
    if let Some(place) = &post.location {
        when.push_str(&format!(" · {place}"));
    }
    lines.push(when);
    lines.push(post.link());
    let marks: Vec<&str> = [
        (post.pinned, "Pinned"),
        (post.paid_partnership, "Paid partnership"),
        (post.caption_edited, "Caption edited"),
    ]
    .into_iter()
    .filter_map(|(on, mark)| on.then_some(mark))
    .collect();
    if !marks.is_empty() {
        lines.push(marks.join(" · "));
    }
    if let Some(music) = post.music.as_ref().and_then(posts::music_line) {
        lines.push(music);
    }
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
        let mut plays = match n {
            1 => "1 play".to_string(),
            n => format!("{} plays", posts::grouped(n)),
        };
        if let (Some(instagram), Some(facebook)) = (post.plays_instagram, post.plays_facebook)
            && facebook > 0
        {
            plays.push_str(&format!(
                " ({} on Instagram, {} on Facebook)",
                posts::grouped(instagram),
                posts::grouped(facebook)
            ));
        }
        counts.push(plays);
    }
    if let Some(n) = post.reposts.filter(|n| *n > 0) {
        counts.push(match n {
            1 => "1 repost".into(),
            n => format!("{} reposts", posts::grouped(n)),
        });
    }
    if let Some(n) = post.shares {
        counts.push(match n {
            1 => "1 share".into(),
            n => format!("{} shares", posts::grouped(n)),
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

/// The grid's JSON document: the posts under `posts`, and beside them how
/// many the profile says it has, whether the grid goes on past what was
/// read, how many pages were asked for (`pages_read`, one request each, the
/// profile's own reads apart), and for a read `--until` shaped, whether it
/// reached one of the posts named and which (`until`, `null` when none was
/// named).
fn grid_json(grid: &Grid, asked: &Asked, format: Format) -> Result<String> {
    let rows: Vec<serde_json::Value> = grid
        .posts
        .iter()
        .enumerate()
        .map(|(index, post)| post_json(post, Some(index + 1)))
        .collect();
    let until = asked.until.then(|| {
        serde_json::json!({
            "reached": asked.read.reached.is_some(),
            "code": asked.read.reached.map(snob_ig::shortcode::code_of),
        })
    });
    media::rows_or_envelope(rows, format, |rows| {
        serde_json::json!({
            "username": grid.username,
            "declared_posts": grid.declared,
            "more": grid.next.is_some(),
            "pages_read": asked.read.pages,
            "until": until,
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

    /// A reel says its length, its marks, its sound, and its plays by
    /// network beside its reposts and shares.
    #[test]
    fn a_reel_says_its_length_marks_sound_and_counts() {
        let reel = post(
            r#"{"pk":"5","product_type":"clips","media_type":2,"taken_at":1790510400,
            "like_count":10,"comment_count":2,"user":{"pk":"42","username":"someone"},
            "media_repost_count":4,"reshare_count":6,
            "play_count":100,"ig_play_count":90,"fb_play_count":10,"video_duration":42.7,
            "clips_metadata":{"original_sound_info":{"original_audio_title":"Original audio",
              "ig_artist":{"username":"someone"}}},
            "timeline_pinned_user_ids":["42"],"is_paid_partnership":true}"#,
        );
        let lines = about(&reel);
        assert_eq!(lines[0], "@someone · reel · 0:42");
        assert!(
            lines.contains(&"Pinned · Paid partnership".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"Sound: Original audio · someone".to_string()),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some(
                "10 likes · 2 comments · 100 plays (90 on Instagram, 10 on Facebook) · 4 reposts · 6 shares"
            )
        );
    }

    #[test]
    fn a_long_caption_is_clipped_in_a_row() {
        assert_eq!(clipped("abcdef", 3), "abc…");
        assert_eq!(clipped("abc", 3), "abc");
    }
}
