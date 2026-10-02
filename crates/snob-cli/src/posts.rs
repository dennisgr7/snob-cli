//! Posts and reels once they are read: what a listing and a view show of
//! them, how they are fetched, and how their files are kept. `posts`,
//! `post` and their views share it, as `stories` and `highlights` share
//! [`crate::media`].
//!
//! **What is read, and what it costs** (`snob_ig::client::IgClient`'s
//! `profile_posts`, `media_info` and `media_comments` say why each is the
//! app's call):
//!
//! - a profile's grid is its profile (the three or four reads `profile`
//!   spends) and then one read per page of twelve posts, a step apart as the
//!   pages of any action are ([`snob_ig::pace::STEP_MS`]). A page of posts is
//!   not a list of accounts and is not charged to the day's accounts;
//! - opening a post in a view is the pair the app sends as one opens, its
//!   info and its first page of comments: two reads, the comments' authors
//!   charged to the day's accounts as a list page's are. More comments are
//!   one read a page, asked for one at a time;
//! - a link is the post's info alone, and its comments only when they are
//!   shown;
//! - the files come from the CDN, which is not paced
//!   (`IgClient::download_capped` says why), photos fetched by the tab and
//!   videos directly, as a play would count.
//!
//! **Nothing here tells anybody what was looked at**: the app reports what it
//! shows through calls of its own, which snob has no code to send, and the
//! allowlist and `crates/snob-core/tests/no_seen.rs` keep it that way.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use snob_core::Epoch;
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::model::post::{Comment, CommentsPage, LikedBy, Media};
use snob_ig::pace::{PAGES_PER_SITTING, Pace};
use snob_ig::shortcode::{self, MediaPk, Opens, Shortcode};

use crate::exit::{ExitCode, ExitError};
use crate::media::{Kind, Named, Saved, download_named, save_named};
use crate::output;
use crate::ui;

/// Ceiling on one downloaded file of a post: 128 MiB.
///
/// Far above [`crate::media::MAX_STORY_BYTES`], and it has to be: a reel runs
/// for minutes where a story runs for seconds, and a video post for longer
/// still, though a 720-pixel reel rarely passes 100 MiB. Nothing of it is held in memory -- every file streams to disk
/// (`IgClient::download_to`) -- so the ceiling is only there so that a
/// redirect to something endless cannot fill the disk.
pub const MAX_POST_BYTES: usize = 128 * 1024 * 1024;

/// The most pages of a grid one run reads: one sitting
/// ([`PAGES_PER_SITTING`]), about 480 posts. A grid past that is what a
/// person scrolling stops somewhere in.
pub const MOST_PAGES: u32 = PAGES_PER_SITTING;

/// The site, for the address a listing prints for each post.
const SITE: &str = "https://www.instagram.com";

/// What a post is, as a listing says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostKind {
    Photo,
    Video,
    /// A video posted as a reel.
    Reel,
    /// A carousel, and how many items it holds.
    Carousel(usize),
}

impl PostKind {
    /// "photo", "video", "reel", "3 items".
    pub fn label(self) -> String {
        match self {
            Self::Photo => "photo".into(),
            Self::Video => "video".into(),
            Self::Reel => "reel".into(),
            Self::Carousel(n) => format!("{n} items"),
        }
    }

    /// The word a JSON document uses: a carousel's count is beside it.
    fn word(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Reel => "reel",
            Self::Carousel(_) => "carousel",
        }
    }
}

/// One file of a post: a photo, or a video's progressive file.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    /// Where the best copy is. Absent when Instagram described the item and
    /// offered no version of it.
    pub url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// One post or reel, reduced to what a listing, a view and a download need.
/// Every text in it came off somebody else's account and is filtered for a
/// terminal ([`printable`]).
#[derive(Debug, Clone)]
pub struct Post {
    pub pk: MediaPk,
    /// As its address carries it.
    pub code: String,
    /// As Instagram spells it.
    pub owner: String,
    pub opens: Opens,
    pub kind: PostKind,
    pub taken_at: Epoch,
    /// One entry per line of the caption.
    pub caption: Vec<String>,
    pub location: Option<String>,
    pub likes: Option<u64>,
    /// The owner hid the counts.
    pub likes_hidden: bool,
    pub comments: Option<u64>,
    pub plays: Option<u64>,
    pub tagged: Vec<String>,
    pub mentions: Vec<String>,
    pub with: Vec<String>,
    pub liked_by: Option<LikedBy>,
    pub items: Vec<Item>,
}

impl Post {
    /// The post `media` describes, owned by `owner` when the answer does not
    /// say whose it is.
    pub fn from_media(media: &Media, owner: &str) -> Self {
        let code = media
            .code
            .clone()
            .filter(|code| shortcode::pk_of(code).is_some())
            .unwrap_or_else(|| shortcode::code_of(media.pk));
        let reel = media.product_type.as_deref() == Some("clips");
        let items: Vec<Item> = media
            .items()
            .into_iter()
            .map(|item| {
                let video = item.is_video();
                let (width, height) = if video {
                    let best = item
                        .video_versions
                        .iter()
                        .max_by_key(|v| (v.width.unwrap_or(0), v.height.unwrap_or(0)));
                    (best.and_then(|v| v.width), best.and_then(|v| v.height))
                } else {
                    let largest = item.image_versions2.as_ref().and_then(|c| {
                        c.candidates
                            .iter()
                            .max_by_key(|v| (v.width.unwrap_or(0), v.height.unwrap_or(0)))
                    });
                    (
                        item.original_width.or(largest.and_then(|v| v.width)),
                        item.original_height.or(largest.and_then(|v| v.height)),
                    )
                };
                Item {
                    kind: if video { Kind::Video } else { Kind::Photo },
                    url: item.best_url().map(str::to_string),
                    width,
                    height,
                }
            })
            .collect();
        let kind = if !media.carousel_media.is_empty() {
            PostKind::Carousel(items.len())
        } else if reel {
            PostKind::Reel
        } else if media.is_video() {
            PostKind::Video
        } else {
            PostKind::Photo
        };
        Self {
            pk: media.pk,
            opens: if reel { Opens::Reel } else { Opens::Post },
            code,
            owner: printable(media.owner().unwrap_or(owner)),
            kind,
            taken_at: media.taken_at,
            caption: media
                .caption_text()
                .lines()
                .map(printable)
                .filter(|line| !line.trim().is_empty())
                .collect(),
            location: media
                .location
                .as_ref()
                .and_then(|l| l.name.as_deref())
                .map(printable)
                .filter(|name| !name.is_empty()),
            likes: media.like_count,
            likes_hidden: media.like_and_view_counts_disabled == Some(true),
            comments: media.comment_count,
            plays: media.plays(),
            tagged: media.tagged().iter().map(|n| printable(n)).collect(),
            mentions: media.mentioned(),
            with: media.with().iter().map(|n| printable(n)).collect(),
            liked_by: media.liked_by().map(|l| LikedBy {
                name: printable(&l.name),
                others: l.others,
            }),
            items,
        }
    }

    /// The page the app opens it on: what it is asked about from.
    pub fn page(&self) -> String {
        shortcode::page_of(&self.code, self.opens)
    }

    /// Its address, for a listing to print.
    pub fn link(&self) -> String {
        format!("{SITE}{}", self.page())
    }

    /// The first line of its caption, for a row.
    pub fn headline(&self) -> &str {
        self.caption.first().map_or("", String::as_str)
    }

    /// "Liked by <name> and N others", as the app writes it under a post, or
    /// "N likes" when nobody the viewer knows liked it. Nothing when the
    /// owner hid the count and nobody is named.
    pub fn liked_line(&self) -> Option<String> {
        match &self.liked_by {
            Some(LikedBy { name, others }) => Some(match others {
                Some(0) => format!("Liked by {name}"),
                Some(1) => format!("Liked by {name} and 1 other"),
                Some(n) => format!("Liked by {name} and {} others", grouped(*n)),
                None => format!("Liked by {name} and others"),
            }),
            None if self.likes_hidden => None,
            None => self.likes.map(|n| match n {
                1 => "1 like".to_string(),
                n => format!("{} likes", grouped(n)),
            }),
        }
    }

    /// The name every file of item `index` (zero-based) is saved under:
    /// `<owner>-<code>`, and `-<n>` after it when the post holds more than
    /// one, so that `-d` and the views write the very same names.
    ///
    /// **Eleven characters of the code**, which are the post's pk
    /// ([`shortcode`]): a private account's post has a code of 39, and with a
    /// thirty-character name and the item's number the name would pass what
    /// `output::default_path` accepts.
    pub fn stem(&self, index: usize) -> String {
        let code = self.code.get(..11).unwrap_or(&self.code);
        let base = format!("{}-{code}", printable(&self.owner));
        if self.items.len() > 1 {
            format!("{base}-{}", index + 1)
        } else {
            base
        }
    }

    /// Item `index` (zero-based) as a file to download.
    pub(crate) fn file(&self, index: usize) -> Named {
        Named {
            number: index + 1,
            stem: self.stem(index),
            url: self.items[index].url.clone(),
            what: format!("item {}", index + 1),
            cap: MAX_POST_BYTES,
        }
    }
}

/// `1234567` as `1,234,567`.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One comment, or a reply shown under one, filtered for a terminal.
#[derive(Debug, Clone)]
pub struct CommentLine {
    pub author: String,
    pub text: String,
    pub at: Epoch,
    pub likes: u64,
    /// How many replies it has; only the ones in `replies` are shown.
    pub replies_count: u64,
    pub replies: Vec<CommentLine>,
}

impl CommentLine {
    fn from(comment: &Comment) -> Self {
        Self {
            author: printable(
                comment
                    .user
                    .as_ref()
                    .and_then(|u| u.username.as_deref())
                    .unwrap_or("someone"),
            ),
            text: printable(comment.text.as_deref().unwrap_or_default()),
            at: comment.created_at,
            likes: comment.comment_like_count.unwrap_or(0),
            replies_count: comment.child_comment_count.unwrap_or(0),
            replies: comment
                .preview_child_comments
                .iter()
                .map(Self::from)
                .collect(),
        }
    }
}

/// The comments of a post read so far, and where the next page starts.
#[derive(Debug, Clone, Default)]
pub struct Comments {
    pub lines: Vec<CommentLine>,
    pub next: Option<String>,
    pub total: Option<u64>,
}

impl Comments {
    fn take(&mut self, page: &CommentsPage) {
        self.lines
            .extend(page.comments.iter().map(CommentLine::from));
        self.next = page.next().map(str::to_string);
        self.total = page.comment_count.or(self.total);
    }
}

/// A profile's grid as far as it was read.
#[derive(Debug, Clone)]
pub struct Grid {
    /// As Instagram spells it.
    pub username: String,
    pub posts: Vec<Post>,
    /// Where the next page starts; `None` once the grid has ended.
    pub next: Option<String>,
    /// How many posts the profile says it has.
    pub declared: Option<u64>,
}

/// What a profile's grid could show.
pub enum Fetched {
    Grid(Grid),
    /// Private, and the viewer does not follow it: "no posts" and "posts you
    /// may not see" are different sentences, as `profile` keeps them.
    Hidden {
        username: String,
    },
}

/// The profile `typed` names and the first `pages` pages of its grid, more
/// if `at_least` posts take more, up to [`MOST_PAGES`]. `known` is the
/// account's pk when this machine has seen it.
pub async fn fetch_grid(
    client: &IgClient,
    typed: &str,
    viewer: snob_core::Pk,
    known: Option<snob_core::Pk>,
    pages: u32,
    at_least: usize,
) -> Result<Fetched> {
    let info = client
        .profile_named(crate::engine::target::clean(typed), known)
        .await?;
    let own = info.id == viewer;
    let is_private = info.is_private.unwrap_or(false);
    let you_follow = info.followed_by_viewer.unwrap_or(false);
    if !(own || !is_private || you_follow) {
        return Ok(Fetched::Hidden {
            username: info.username,
        });
    }
    let declared = info.posts.map(|p| p.count);
    let mut grid = Grid {
        username: info.username,
        posts: Vec::new(),
        next: None,
        declared,
    };
    // Zero is an answer the profile already gave, as in `highlights`.
    if declared == Some(0) {
        return Ok(Fetched::Grid(grid));
    }
    let first = client.profile_posts(&grid.username, None).await?;
    grid.take(first);
    let mut read = 1;
    while (read < pages || grid.posts.len() < at_least) && read < MOST_PAGES {
        if !more(client, &mut grid).await? {
            break;
        }
        read += 1;
    }
    Ok(Fetched::Grid(grid))
}

impl Grid {
    fn take(&mut self, page: snob_ig::client::PostsPage) {
        let owner = self.username.clone();
        self.posts
            .extend(page.posts.iter().map(|m| Post::from_media(m, &owner)));
        self.next = page.next;
    }
}

/// The grid's next page, a step after the page before as the pages of an
/// action are. `false` when the grid had ended, or Ctrl+C stopped the wait.
pub async fn more(client: &IgClient, grid: &mut Grid) -> Result<bool> {
    let Some(after) = grid.next.clone() else {
        return Ok(false);
    };
    if client.step_between(&Pace::default()).await {
        return Ok(false);
    }
    let page = client.profile_posts(&grid.username, Some(&after)).await?;
    grid.take(page);
    Ok(true)
}

/// A post opened the way the app opens one: its info read again and its
/// first page of comments, the pair the app sends. Two reads. The post comes
/// back as the info describes it, which is the fresher; the grid's stands
/// when the info names nothing.
pub async fn open(client: &IgClient, post: &Post) -> Result<(Post, Comments)> {
    let page = post.page();
    let fresh = client.media_info(post.pk, &page).await?;
    let post = fresh
        .as_ref()
        .map(|media| {
            let mut fresh = Post::from_media(media, &post.owner);
            fresh.opens = post.opens;
            fresh
        })
        .unwrap_or_else(|| post.clone());
    let mut comments = Comments::default();
    comments.take(&client.media_comments(post.pk, None, &page).await?);
    Ok((post, comments))
}

/// The next page of `post`'s comments, one read. `false` when there was
/// none to read.
pub async fn more_comments(
    client: &IgClient,
    post: &Post,
    comments: &mut Comments,
) -> Result<bool> {
    let Some(after) = comments.next.clone() else {
        return Ok(false);
    };
    let page = client
        .media_comments(post.pk, Some(&after), &post.page())
        .await?;
    comments.take(&page);
    Ok(true)
}

/// The post `link` names, read the way the app reads one it opens: its info,
/// from the page it opens on. One read. A post the answer does not hold is
/// not found.
pub async fn fetch_link(client: &IgClient, link: &Shortcode) -> Result<Post> {
    let media = client.media_info(link.pk(), &link.page()).await?;
    let media = media.ok_or_else(|| {
        ExitError::new(
            ExitCode::Error,
            format!(
                "no post answers to {}: it was deleted, or it is not shown to you",
                link.code()
            ),
        )
    })?;
    let mut post = Post::from_media(&media, "");
    // Where it was asked from is where it opens, whatever kind it is.
    post.opens = link.opens();
    post.code = link.code().to_string();
    Ok(post)
}

/// The first page of `post`'s comments, one read.
pub async fn first_comments(client: &IgClient, post: &Post) -> Result<Comments> {
    let mut comments = Comments::default();
    comments.take(&client.media_comments(post.pk, None, &post.page()).await?);
    Ok(comments)
}

/// Saves item `index` (zero-based) of `post` into `dir` under its own name.
pub(crate) async fn save_item(
    client: &IgClient,
    post: &Post,
    index: usize,
    dir: &Path,
) -> Result<Saved> {
    let file = post.file(index);
    save_named(
        client,
        &file.stem,
        file.url.as_deref(),
        &file.what,
        file.cap,
        dir,
    )
    .await
}

/// Downloads `files` the way `-d` does: one file and `-o` names it; several,
/// or none named, and they go into a directory (`-o`, or the working one)
/// under their own names, three at a time, past the ones that fail.
pub(crate) async fn download(
    client: std::sync::Arc<IgClient>,
    files: Vec<Named>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    if let ([file], Some(path)) = (files.as_slice(), destination)
        && !path.is_dir()
    {
        save_to(&client, file, path).await?;
        ui::info(&format!("Saved {}", path.display()));
        return Ok(ExitCode::Ok);
    }
    if files.len() == 1 && destination.is_none() {
        let file = &files[0];
        return match save_named(
            &client,
            &file.stem,
            file.url.as_deref(),
            &file.what,
            file.cap,
            Path::new("."),
        )
        .await?
        {
            Saved::Now(path) => {
                ui::info(&format!("Saved {}", path.display()));
                Ok(ExitCode::Ok)
            }
            Saved::Already(path) => {
                ui::info(&format!("Already saved {}", path.display()));
                Ok(ExitCode::Ok)
            }
        };
    }
    download_named(client, files, destination, "files").await
}

/// One file into the path the user named, streamed: replaced if it is
/// there, as `-o` always replaces, and removed again if the download fails
/// halfway.
async fn save_to(client: &IgClient, file: &Named, path: &Path) -> Result<()> {
    let url = file
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("{} has no downloadable version", file.what))?;
    let mut out = std::fs::File::create(path)
        .map_err(|e| anyhow!("could not write {}: {e}", path.display()))?;
    if let Err(e) = client.download_to(url, file.cap, &mut out).await {
        drop(out);
        let _ = std::fs::remove_file(path);
        return Err(e.into());
    }
    Ok(())
}

/// The files of every item of `posts`, in order.
pub(crate) fn files_of(posts: &[&Post]) -> Vec<Named> {
    let mut number = 0;
    let mut files = Vec::new();
    for post in posts {
        for index in 0..post.items.len() {
            number += 1;
            files.push(Named {
                number,
                ..post.file(index)
            });
        }
    }
    files
}

/// A post as JSON: everything a listing shows, its items with their
/// addresses, and its number in the listing when it has one.
pub fn post_json(post: &Post, number: Option<usize>) -> Value {
    let items: Vec<Value> = post
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            json!({
                "number": i + 1,
                "kind": item.kind.label(),
                "width": item.width,
                "height": item.height,
                "url": item.url,
            })
        })
        .collect();
    let mut value = json!({
        "pk": post.pk.to_string(),
        "code": post.code,
        "url": post.link(),
        "owner": post.owner,
        "kind": post.kind.word(),
        "taken_at": post.taken_at.get(),
        "caption": post.caption.join("\n"),
        "location": post.location,
        "likes": (!post.likes_hidden).then_some(post.likes).flatten(),
        "liked_by": post.liked_by.as_ref().map(|l| json!({ "name": l.name, "others": l.others })),
        "comments": post.comments,
        "plays": post.plays,
        "tagged": post.tagged,
        "mentions": post.mentions,
        "with": post.with,
        "items": items,
    });
    if let Some(number) = number {
        value["number"] = json!(number);
    }
    value
}

/// A comment as JSON, its shown replies under it.
pub fn comment_json(comment: &CommentLine) -> Value {
    json!({
        "author": comment.author,
        "text": comment.text,
        "created_at": comment.at.get(),
        "likes": comment.likes,
        "replies": comment.replies_count,
        "shown_replies": comment.replies.iter().map(comment_json).collect::<Vec<_>>(),
    })
}

/// Where a post shown by a view was saved into the scratch directory to be
/// handed to the system viewer, per item, so opening one twice is one
/// download.
pub type Opened = Vec<Option<PathBuf>>;

/// Hands item `index` of `post` to the system viewer, from `scratch`.
pub async fn open_item(
    client: &IgClient,
    post: &Post,
    index: usize,
    scratch: &Path,
    opened: &mut Opened,
) -> Result<PathBuf> {
    if let Some(existing) = opened.get(index).cloned().flatten()
        && existing.is_file()
    {
        opener::open(&existing).map_err(|e| anyhow!("the system viewer would not start: {e}"))?;
        return Ok(existing);
    }
    let path = match save_item(client, post, index, scratch).await? {
        Saved::Now(path) | Saved::Already(path) => path,
    };
    opener::open(&path).map_err(|e| anyhow!("the system viewer would not start: {e}"))?;
    if opened.len() < post.items.len() {
        opened.resize(post.items.len(), None);
    }
    opened[index] = Some(path.clone());
    Ok(path)
}

/// Saves item `index` of `post` where the user works: the copy already
/// fetched to look at, when there is one, and otherwise the CDN's.
pub async fn keep_item(
    client: &IgClient,
    post: &Post,
    index: usize,
    opened: &Opened,
) -> Result<PathBuf> {
    if let Some(seen) = opened.get(index).cloned().flatten()
        && seen.is_file()
        && let Some(name) = seen.file_name()
    {
        let target = Path::new(".").join(name);
        let mut out = output::create_new(&target)?;
        let mut from = std::fs::File::open(&seen)?;
        std::io::copy(&mut from, &mut out)?;
        return Ok(target);
    }
    match save_item(client, post, index, Path::new(".")).await? {
        Saved::Now(path) => Ok(path),
        Saved::Already(path) => Err(anyhow!("{} is already here", path.display())),
    }
}

/// Saves every item of `post` where the user works, one at a time. The
/// sentence for the note line, and whether anything was saved.
pub async fn keep_post(client: &IgClient, post: &Post) -> (String, bool) {
    let total = post.items.len();
    let (mut kept, mut failed) = (0usize, 0usize);
    for index in 0..total {
        if client.pacer().cancel_token().is_canceled() {
            return (format!("Stopped after {kept} of {total}"), kept > 0);
        }
        match save_item(client, post, index, Path::new(".")).await {
            Ok(_) => kept += 1,
            Err(_) => failed += 1,
        }
    }
    let note = if failed == 0 {
        format!("Saved {kept} of {total} from {} here", post.code)
    } else {
        format!(
            "Saved {kept} of {total} from {}; {failed} failed",
            post.code
        )
    };
    (note, kept > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(json: &str) -> Media {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_carousel_is_its_items_and_names_each_file() {
        let post = Post::from_media(
            &media(
                r#"{"pk":"3995556159532654737","code":"DdzEShhEWCRnxelCuqsxG5vhMRx3FuF81XXoEM0","media_type":8,
                "caption":{"text":"first line\n\nsecond with @friend"},"like_count":80,"top_likers":["close"],
                "carousel_media":[
                  {"pk":"1","media_type":1,"original_width":1080,"original_height":1350,
                   "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/a.jpg","width":1080,"height":1350}]}},
                  {"pk":"2","media_type":2,"video_versions":[{"url":"https://x.cdninstagram.com/b.mp4","width":720,"height":1280}]}
                ]}"#,
            ),
            "someone",
        );
        assert_eq!(post.kind, PostKind::Carousel(2));
        assert_eq!(post.kind.label(), "2 items");
        assert_eq!(post.owner, "someone");
        assert_eq!(post.caption, ["first line", "second with @friend"]);
        assert_eq!(post.mentions, ["friend"]);
        assert_eq!(post.stem(0), "someone-DdzEShhEWCR-1");
        assert_eq!(post.stem(1), "someone-DdzEShhEWCR-2");
        assert_eq!(post.items[1].kind, Kind::Video);
        assert_eq!(post.items[1].width, Some(720));
        assert_eq!(
            post.liked_line().as_deref(),
            Some("Liked by close and 79 others")
        );
        assert_eq!(
            post.link(),
            "https://www.instagram.com/p/DdzEShhEWCRnxelCuqsxG5vhMRx3FuF81XXoEM0/"
        );
    }

    #[test]
    fn a_reel_is_one_file_named_without_a_number() {
        let post = Post::from_media(
            &media(
                r#"{"pk":"3947056156557494178","product_type":"clips","media_type":2,"like_count":2000,
                "like_and_view_counts_disabled":false,"play_count":4017638,
                "user":{"username":"thegrefg"},
                "video_versions":[{"url":"https://x.cdninstagram.com/r.mp4","width":720,"height":1280}]}"#,
            ),
            "",
        );
        assert_eq!(post.kind, PostKind::Reel);
        assert_eq!(post.code, "DbGwql4IMei", "made from the pk when absent");
        assert_eq!(post.page(), "/reel/DbGwql4IMei/");
        assert_eq!(post.stem(0), "thegrefg-DbGwql4IMei");
        assert_eq!(post.liked_line().as_deref(), Some("2,000 likes"));
        assert_eq!(post.plays, Some(4_017_638));
        let json = post_json(&post, Some(3));
        assert_eq!(json["number"], 3);
        assert_eq!(json["kind"], "reel");
        assert_eq!(json["items"][0]["kind"], "video");
    }

    #[test]
    fn hidden_likes_are_not_counted() {
        let post = Post::from_media(
            &media(r#"{"pk":"1","like_count":5,"like_and_view_counts_disabled":true}"#),
            "x",
        );
        assert_eq!(post.liked_line(), None);
        let named = Post::from_media(
            &media(
                r#"{"pk":"1","like_count":5,"like_and_view_counts_disabled":true,"top_likers":["a"]}"#,
            ),
            "x",
        );
        assert_eq!(named.liked_line().as_deref(), Some("Liked by a and others"));
        assert_eq!(post_json(&post, None)["likes"], Value::Null);
    }

    #[test]
    fn numbers_are_grouped_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(4_017_638), "4,017,638");
    }
}
