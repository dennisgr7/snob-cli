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
//!   (`CdnClient::download_capped` says why), photos fetched by the tab and
//!   videos directly, as a play would count.
//!
//! **Nothing here tells anybody what was looked at**: the app reports what it
//! shows through calls of its own, which snob has no code to send, and the
//! allowlist and `crates/snob-core/tests/no_seen.rs` keep it that way.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::Value;
use snob_core::Epoch;
use snob_core::model::post::{self as document, AccountRef, Place};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::model::dash;
use snob_ig::model::post::{Comment, CommentsPage, LikedBy, Media, Named as NamedAccount};
use snob_ig::pace::{PAGES_PER_SITTING, Pace};
use snob_ig::shortcode::{self, MediaPk, Opens, Shortcode};

use crate::cli::Quality;
use crate::exit::{ExitCode, ExitError};
use crate::ffmpeg::{self, Ffmpeg};
use crate::media::{
    Best, Kind, Named, Saved, download_named, save_file, save_file_saying, save_file_to, save_named,
};
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
    /// The item's own media id.
    pub pk: Option<String>,
    /// The picture's description.
    pub alt: Option<String>,
    /// A video's length in seconds.
    pub duration: Option<f64>,
    /// A video's widest track and its sound, when they are wider than
    /// [`Self::url`]'s file.
    pub best: Option<Best>,
    /// A small picture of it for a view to draw: a photo's own, a video's
    /// cover ([`thumb_of`]). Not in the JSON contract.
    pub thumb: Option<String>,
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
    /// What it is, for its address: a reel's is `/reel/<code>/`.
    pub opens: Opens,
    /// The page it is asked about from, which is not always its address: the
    /// app opens a post or a reel from a grid over `/p/<code>/` alike (4 reels
    /// of 4 in the capture of 2026-11-13, their `Referer` `/p/<code>/`), and a
    /// link from the page the link names.
    pub asked_from: Opens,
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
    pub owner_pk: Option<String>,
    pub tagged_accounts: Vec<AccountRef>,
    pub with_accounts: Vec<AccountRef>,
    /// The likers the viewer knows that the app names under it, at most
    /// three ([`snob_ig::model::post::FACEPILE_SHOWN`]).
    pub likers_known: Vec<AccountRef>,
    pub reposts: Option<u64>,
    /// `None` for a photo, and when the owner turned the count off.
    pub shares: Option<u64>,
    pub plays_instagram: Option<u64>,
    pub plays_facebook: Option<u64>,
    /// A video's length in seconds.
    pub duration: Option<f64>,
    pub music: Option<document::Music>,
    pub place: Option<Place>,
    pub pinned: bool,
    pub paid_partnership: bool,
    pub caption_edited: bool,
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
                    pk: digits(&item.pk.to_string()),
                    alt: item
                        .accessibility_caption
                        .as_deref()
                        .map(printable)
                        .filter(|alt| !alt.trim().is_empty()),
                    duration: seconds(item.video_duration),
                    best: best_of(item, width),
                    // A carousel the grid handed over without its items is
                    // one item, the post itself, and so its own pictures.
                    thumb: thumb_of(item),
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
            asked_from: Opens::Post,
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
            owner_pk: media
                .user
                .as_ref()
                .and_then(|u| u.pk.as_deref())
                .and_then(digits),
            tagged_accounts: media.tagged_accounts().iter().map(account).collect(),
            with_accounts: media.with_accounts().iter().map(account).collect(),
            likers_known: media.known_likers().iter().map(account).collect(),
            reposts: media.media_repost_count,
            shares: media
                .reshare_count
                .filter(|_| media.share_count_disabled != Some(true)),
            plays_instagram: media.ig_play_count,
            plays_facebook: media.fb_play_count,
            duration: seconds(media.video_duration),
            music: media.music().map(|music| document::Music {
                kind: if music.licensed { "song" } else { "original" }.to_string(),
                title: music.title.as_deref().map(printable),
                artist: music.artist.as_deref().map(printable),
                artist_username: music.artist_username.as_deref().map(printable),
            }),
            place: media.location.as_ref().map(|l| Place {
                pk: l.pk.as_deref().and_then(digits),
                name: l.name.as_deref().map(printable),
                lat: l.lat.filter(|lat| lat.is_finite()),
                lng: l.lng.filter(|lng| lng.is_finite()),
            }),
            pinned: media.pinned(),
            paid_partnership: media.is_paid_partnership == Some(true),
            caption_edited: media.caption_is_edited == Some(true),
        }
    }

    /// The page the app opens it on: what it is asked about from.
    pub fn page(&self) -> String {
        shortcode::page_of(&self.code, self.asked_from)
    }

    /// Its address, for a listing to print.
    pub fn link(&self) -> String {
        format!("{SITE}{}", shortcode::page_of(&self.code, self.opens))
    }

    /// The first line of its caption, for a row.
    pub fn headline(&self) -> &str {
        self.caption.first().map_or("", String::as_str)
    }

    /// "Liked by <name> and N others", as the app writes it under a post, or
    /// "N likes" when nobody the viewer knows liked it. Nothing when the
    /// owner hid the count and nobody is named.
    pub fn liked_line(&self) -> Option<String> {
        let names: Vec<&str> = self
            .likers_known
            .iter()
            .map(|a| a.username.as_str())
            .collect();
        let Some((last, first)) = names.split_last() else {
            if self.likes_hidden {
                return None;
            }
            return self.likes.map(|n| match n {
                1 => "1 like".to_string(),
                n => format!("{} likes", grouped(n)),
            });
        };
        let listed = names.join(", ");
        // Never read under hidden counts: the number is then a placeholder.
        let others = if self.likes_hidden {
            None
        } else {
            self.likes.map(|n| n.saturating_sub(names.len() as u64))
        };
        Some(match others {
            Some(0) if first.is_empty() => format!("Liked by {last}"),
            Some(0) => format!("Liked by {} and {last}", first.join(", ")),
            Some(1) => format!("Liked by {listed} and 1 other"),
            Some(n) => format!("Liked by {listed} and {} others", grouped(n)),
            None => format!("Liked by {listed} and others"),
        })
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
            best: self.items[index].best.clone(),
            ffmpeg: None,
        }
    }
}

/// The picture of `item` a view draws small: of its candidates (a video's
/// are its cover), the one [`media::thumb_among`] picks.
fn thumb_of(item: &Media) -> Option<String> {
    crate::media::thumb_among(&item.image_versions2.as_ref()?.candidates)
}

/// `item`'s best tracks, read from its DASH manifest, when its widest video
/// is wider than its progressive file (`width`), as it is on 165 of 180
/// post videos of the capture of 2026-11-13.
fn best_of(item: &Media, width: Option<u32>) -> Option<Best> {
    let tracks = dash::tracks(item.video_dash_manifest.as_deref()?)?;
    let wider = tracks.video.width.unwrap_or(0) > width.unwrap_or(0);
    wider.then_some(Best {
        video: tracks.video,
        audio: tracks.audio,
    })
}

/// What a run saves a post's videos as ([`crate::cli::Quality`]).
#[derive(Debug, Clone)]
pub enum Videos {
    /// The progressive file; `say_how` when it is so because no ffmpeg was
    /// found, which a run that would have saved better says once.
    Compat { say_how: bool },
    /// The best tracks, joined by this ffmpeg.
    Best(std::sync::Arc<Ffmpeg>),
}

impl Videos {
    /// What `asked` and `named` (`--ffmpeg`) make of this run: best when
    /// ffmpeg is found and nothing else was asked, and `--quality best`
    /// with no ffmpeg refused before anything is asked of Instagram.
    pub fn resolve(asked: Option<Quality>, named: Option<&Path>) -> Result<Self> {
        let found = Ffmpeg::find(named).map(std::sync::Arc::new);
        match (asked, found) {
            (Some(Quality::Compat), _) => Ok(Self::Compat { say_how: false }),
            (_, Some(ffmpeg)) => Ok(Self::Best(ffmpeg)),
            (None, None) => Ok(Self::Compat { say_how: true }),
            (Some(Quality::Best), None) => Err(ExitError::new(
                ExitCode::Error,
                "--quality best joins a video's tracks with ffmpeg, and none was found",
            )
            .with_hint(format!(
                "install it ({}), or name it with --ffmpeg or {}",
                ffmpeg::how_to_install(),
                ffmpeg::ENV
            ))
            .into()),
        }
    }

    /// What the full-screen view keeps a video as: the run's default, which
    /// never fails.
    pub fn for_the_view() -> Self {
        Self::resolve(None, None).unwrap_or(Self::Compat { say_how: false })
    }

    /// `files` made ready to save as this says: each video's best tracks
    /// kept, with the ffmpeg that joins them, or dropped. Without ffmpeg, a
    /// run that would have saved a better video says once how to get it.
    pub(crate) fn prepare(&self, files: Vec<Named>) -> Vec<Named> {
        if matches!(self, Self::Compat { say_how: true }) && files.iter().any(|f| f.best.is_some())
        {
            ui::info(&format!(
                "Videos are saved as the app's 720-wide file: with ffmpeg installed ({}), \
                 snob saves them at their best, joined into one MP4",
                ffmpeg::how_to_install()
            ));
        }
        files
            .into_iter()
            .map(|file| match self {
                Self::Best(ffmpeg) => Named {
                    ffmpeg: Some(std::sync::Arc::clone(ffmpeg)),
                    ..file
                },
                Self::Compat { .. } => Named { best: None, ..file },
            })
            .collect()
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

/// A video's length, `0:42` or `1:02:03`, from seconds; a part of a second
/// is dropped, as the app's player shows it.
pub fn length(seconds: f64) -> String {
    // Saturates on anything past u64, which `seconds` from a post is not.
    let whole = seconds.max(0.0) as u64;
    let (hours, minutes, seconds) = (whole / 3600, whole / 60 % 60, whole % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The line that says what a post plays: "Music: <title> · <artist>" for a
/// song, "Sound: <title> · <artist>" for a video's own; `None` when it says
/// neither.
pub fn music_line(music: &document::Music) -> Option<String> {
    let what = if music.kind == "song" {
        "Music"
    } else {
        "Sound"
    };
    let parts: Vec<&str> = [music.title.as_deref(), music.artist.as_deref()]
        .into_iter()
        .flatten()
        .filter(|part| !part.trim().is_empty())
        .collect();
    (!parts.is_empty()).then(|| format!("{what}: {}", parts.join(" · ")))
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
    pub pk: Option<String>,
    pub author_pk: Option<String>,
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
            pk: comment.pk.as_deref().and_then(digits),
            author_pk: comment
                .user
                .as_ref()
                .and_then(|u| u.pk.as_deref())
                .and_then(digits),
        }
    }
}

/// `pk` when it is one: digits, and nothing else Instagram might put there.
fn digits(pk: &str) -> Option<String> {
    (!pk.is_empty() && pk.bytes().all(|b| b.is_ascii_digit())).then(|| pk.to_string())
}

/// A length in seconds, when it is one.
fn seconds(duration: Option<f64>) -> Option<f64> {
    duration.filter(|d| d.is_finite() && *d >= 0.0)
}

/// An account a post names, ready for a terminal: its name filtered, its pk
/// held to digits.
fn account(named: &NamedAccount) -> AccountRef {
    AccountRef {
        pk: named.pk.as_deref().and_then(digits),
        username: printable(&named.username),
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
        if rest_before_a_page(client, "the grid").await? || !more(client, &mut grid).await? {
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
            fresh.asked_from = post.asked_from;
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
    post.asked_from = link.opens();
    post.code = link.code().to_string();
    Ok(post)
}

/// The first page of `post`'s comments, one read.
pub async fn first_comments(client: &IgClient, post: &Post) -> Result<Comments> {
    let mut comments = Comments::default();
    comments.take(&client.media_comments(post.pk, None, &post.page()).await?);
    Ok(comments)
}

/// The most pages of a post's comments one run reads: twenty, some 300
/// comments at the fifteen a page the app reads (capture of 2026-11-13),
/// and half a sitting ([`PAGES_PER_SITTING`]), so that one post cannot be a
/// list's worth of reading.
pub const MOST_COMMENT_PAGES: u32 = 20;

/// `post`'s comments, `pages` pages of them, or fewer when they end: the
/// first, then each next one a comment step after the page before
/// ([`Pace::for_comments`]), as "load more" is clicked, past any rest the
/// account's sitting owes. At most [`MOST_COMMENT_PAGES`]. `None` when
/// Ctrl+C stopped a wait.
pub async fn comment_pages(client: &IgClient, post: &Post, pages: u32) -> Result<Option<Comments>> {
    let mut comments = first_comments(client, post).await?;
    let pace = Pace::for_comments();
    for _ in 1..pages.min(MOST_COMMENT_PAGES) {
        if comments.next.is_none() {
            break;
        }
        if rest_before_a_page(client, "the comments").await? || client.step_between(&pace).await {
            return Ok(None);
        }
        more_comments(client, post, &mut comments).await?;
    }
    Ok(Some(comments))
}

/// Pays a rest the account's sitting owes before a page of `of` that a
/// command reads on its own, saying so on standard error, since nothing
/// else would say why it went quiet for minutes. `true` when it was
/// canceled.
async fn rest_before_a_page(client: &IgClient, of: &str) -> Result<bool> {
    Ok(client
        .rest_before_a_page(&Pace::default(), |left| {
            ui::info(&format!(
                "Resting {} between sittings before the next page of {of}.",
                crate::report::rest_left(left)
            ));
        })
        .await?)
}

/// Saves item `index` (zero-based) of `post` into `dir` under its own name,
/// as its progressive file: what the view hands the system viewer, which
/// starts at once and plays anywhere (QuickTime plays no VP9).
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

/// Item `index` of `post` as the view keeps one: at its best when ffmpeg
/// is found ([`Videos::for_the_view`]).
fn kept_file(post: &Post, index: usize) -> Result<Named> {
    Videos::for_the_view()
        .prepare(vec![post.file(index)])
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("item {} is not in the post", index + 1))
}

/// Saves item `index` of `post` into `dir` as the view keeps one, with the
/// sentence to show when it was saved as the app's smaller file after all.
async fn keep_one(
    client: &IgClient,
    post: &Post,
    index: usize,
    dir: &Path,
) -> Result<(Saved, Option<String>)> {
    save_file_saying(client, &kept_file(post, index)?, dir).await
}

/// What [`keep_item`] saved: where, and the sentence to show beside it
/// when the video was saved as the app's smaller file after all.
#[derive(Debug)]
pub struct Kept {
    pub path: PathBuf,
    pub said: Option<String>,
}

/// Downloads `files` the way `-d` does: one file and `-o` names it; several,
/// or none named, and they go into a directory (`-o`, or the working one)
/// under their own names, three at a time, past the ones that fail.
pub(crate) async fn download(
    client: std::sync::Arc<IgClient>,
    files: Vec<Named>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    if destination.is_some_and(output::is_stdout) {
        output::one_file_to_stdout(files.len())?;
        let file = &files[0];
        let url = file
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("{} has no downloadable version", file.what))?;
        let mut sink = output::StdoutSink::default();
        let streamed = client.download_to(url, file.cap, &mut sink).await;
        sink.finish(streamed)?;
        return Ok(ExitCode::Ok);
    }
    if let ([file], Some(path)) = (files.as_slice(), destination)
        && !path.is_dir()
    {
        save_file_to(&client, file, path).await?;
        ui::info(&format!("Saved {}", path.display()));
        return Ok(ExitCode::Ok);
    }
    if files.len() == 1 && destination.is_none() {
        let file = &files[0];
        return match save_file(&client, file, Path::new(".")).await? {
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

/// `post` as the JSON contract has it ([`document::Post`]): everything a
/// listing shows, its items with their addresses, and its number in the
/// listing when it has one.
pub fn post_document(post: &Post, number: Option<usize>) -> document::Post {
    let items = post
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| document::PostItem {
            number: i + 1,
            kind: item.kind.label().to_string(),
            width: item.width,
            height: item.height,
            url: item.url.clone(),
            pk: item.pk.clone(),
            alt: item.alt.clone(),
            duration: item.duration,
            best: item.best.as_ref().map(|best| document::BestTracks {
                width: best.video.width,
                height: best.video.height,
                codec: best.video.codecs.as_deref().map(printable),
                bytes: best.video.bytes.map(|video| {
                    let audio = best.audio.as_ref().and_then(|a| a.bytes).unwrap_or(0);
                    video.saturating_add(audio)
                }),
                video_url: best.video.url.clone(),
                audio_url: best.audio.as_ref().map(|a| a.url.clone()),
            }),
        })
        .collect();
    document::Post {
        pk: post.pk.to_string(),
        code: post.code.clone(),
        url: post.link(),
        owner: post.owner.clone(),
        kind: post.kind.word().to_string(),
        taken_at: post.taken_at.get(),
        caption: post.caption.join("\n"),
        location: post.location.clone(),
        likes: (!post.likes_hidden).then_some(post.likes).flatten(),
        liked_by: post.liked_by.as_ref().map(|l| document::LikedBy {
            name: l.name.clone(),
            others: l.others,
        }),
        comments: post.comments,
        plays: post.plays,
        tagged: post.tagged.clone(),
        mentions: post.mentions.clone(),
        with: post.with.clone(),
        items,
        number,
        owner_pk: post.owner_pk.clone(),
        tagged_accounts: post.tagged_accounts.clone(),
        with_accounts: post.with_accounts.clone(),
        likers_known: post.likers_known.clone(),
        likes_hidden: post.likes_hidden,
        reposts: post.reposts,
        shares: post.shares,
        plays_instagram: post.plays_instagram,
        plays_facebook: post.plays_facebook,
        duration: post.duration,
        music: post.music.clone(),
        place: post.place.clone(),
        pinned: post.pinned,
        paid_partnership: post.paid_partnership,
        caption_edited: post.caption_edited,
    }
}

/// `comment` as the JSON contract has it, its shown replies under it.
pub fn comment_document(comment: &CommentLine) -> document::PostComment {
    document::PostComment {
        author: comment.author.clone(),
        text: comment.text.clone(),
        created_at: comment.at.get(),
        likes: comment.likes,
        replies: comment.replies_count,
        shown_replies: comment.replies.iter().map(comment_document).collect(),
        pk: comment.pk.clone(),
        author_pk: comment.author_pk.clone(),
    }
}

/// A post as JSON ([`post_document`]), for the listings that put rows
/// together as values. A document of these types always serializes.
pub fn post_json(post: &Post, number: Option<usize>) -> Value {
    serde_json::to_value(post_document(post, number)).unwrap_or_default()
}

/// A comment as JSON ([`comment_document`]).
pub fn comment_json(comment: &CommentLine) -> Value {
    serde_json::to_value(comment_document(comment)).unwrap_or_default()
}

/// Where a post shown by a view was saved into the scratch directory to be
/// handed to the system viewer, per item, so opening one twice is one
/// download.
pub type Opened = Vec<Option<PathBuf>>;

/// Item `index` of `post`, fetched into `scratch` as the view opens one,
/// the app's own file, and not opened: once per item, so asking twice is
/// one download. What [`open_item`] hands the system viewer, and what a
/// preview drawn in the view reads.
pub async fn fetched_item(
    client: &IgClient,
    post: &Post,
    index: usize,
    scratch: &Path,
    opened: &mut Opened,
) -> Result<PathBuf> {
    if let Some(existing) = opened.get(index).cloned().flatten()
        && existing.is_file()
    {
        return Ok(existing);
    }
    let path = match save_item(client, post, index, scratch).await? {
        Saved::Now(path) | Saved::Already(path) => path,
    };
    if opened.len() < post.items.len() {
        opened.resize(post.items.len(), None);
    }
    if let Some(slot) = opened.get_mut(index) {
        *slot = Some(path.clone());
    }
    Ok(path)
}

/// Hands item `index` of `post` to the system viewer, from `scratch`.
pub async fn open_item(
    client: &IgClient,
    post: &Post,
    index: usize,
    scratch: &Path,
    opened: &mut Opened,
) -> Result<PathBuf> {
    let path = fetched_item(client, post, index, scratch, opened).await?;
    opener::open(&path).map_err(|e| anyhow!("the system viewer would not start: {e}"))?;
    Ok(path)
}

/// Saves item `index` of `post` where the user works: the copy already
/// fetched to look at, when there is one and it is what would be saved, and
/// otherwise the CDN's. A video to be saved at its best is not the copy the
/// view opened, which is the app's file.
pub async fn keep_item(
    client: &IgClient,
    post: &Post,
    index: usize,
    opened: &Opened,
) -> Result<Kept> {
    let at_its_best = kept_file(post, index)?.best.is_some();
    if !at_its_best
        && let Some(seen) = opened.get(index).cloned().flatten()
        && seen.is_file()
        && let Some(name) = seen.file_name()
    {
        let target = Path::new(".").join(name);
        let mut out = output::create_new(&target)?;
        let mut from = std::fs::File::open(&seen)?;
        std::io::copy(&mut from, &mut out)?;
        return Ok(Kept {
            path: target,
            said: None,
        });
    }
    match keep_one(client, post, index, Path::new(".")).await? {
        (Saved::Now(path), said) => Ok(Kept { path, said }),
        (Saved::Already(path), _) => Err(anyhow!("{} is already here", path.display())),
    }
}

/// Saves every item of `post` where the user works, one at a time. The
/// sentence for the note line, and whether anything was saved.
pub async fn keep_post(client: &IgClient, post: &Post) -> (String, bool) {
    let total = post.items.len();
    let (mut kept, mut failed, mut smaller) = (0usize, 0usize, 0usize);
    for index in 0..total {
        if client.pacer().cancel_token().is_canceled() {
            return (format!("Stopped after {kept} of {total}"), kept > 0);
        }
        match keep_one(client, post, index, Path::new(".")).await {
            Ok((_, said)) => {
                kept += 1;
                smaller += usize::from(said.is_some());
            }
            Err(_) => failed += 1,
        }
    }
    let mut note = if failed == 0 {
        format!("Saved {kept} of {total} from {} here", post.code)
    } else {
        format!(
            "Saved {kept} of {total} from {}; {failed} failed",
            post.code
        )
    };
    if smaller > 0 {
        note.push_str(&format!("; {smaller} as the app's smaller file"));
    }
    (note, kept > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(json: &str) -> Media {
        serde_json::from_str(json).unwrap()
    }

    /// A one-item post whose pictures are `candidates`, a JSON array.
    fn thumb_among(candidates: &str) -> Option<String> {
        let json = format!(
            r#"{{"pk":"9","media_type":1,"image_versions2":{{"candidates":{candidates}}}}}"#
        );
        Post::from_media(&media(&json), "x").items[0].thumb.clone()
    }

    #[test]
    fn the_thumbnail_is_the_narrowest_picture_wide_enough() {
        let picked = thumb_among(
            r#"[{"url":"https://x.cdninstagram.com/1440.jpg","width":1440},
                {"url":"https://x.cdninstagram.com/640.jpg","width":640},
                {"url":"https://x.cdninstagram.com/320.jpg","width":320},
                {"url":"https://x.cdninstagram.com/150.jpg","width":150}]"#,
        );
        assert_eq!(
            picked.as_deref(),
            Some("https://x.cdninstagram.com/320.jpg")
        );
    }

    #[test]
    fn with_none_wide_enough_the_thumbnail_is_the_widest() {
        let picked = thumb_among(
            r#"[{"url":"https://x.cdninstagram.com/150.jpg","width":150},
                {"url":"https://x.cdninstagram.com/240.jpg","width":240}]"#,
        );
        assert_eq!(
            picked.as_deref(),
            Some("https://x.cdninstagram.com/240.jpg")
        );
    }

    #[test]
    fn of_two_thumbnails_alike_the_first_listed_is_taken() {
        let wide = thumb_among(
            r#"[{"url":"https://x.cdninstagram.com/a.jpg","width":480},
                {"url":"https://x.cdninstagram.com/b.jpg","width":480}]"#,
        );
        assert_eq!(wide.as_deref(), Some("https://x.cdninstagram.com/a.jpg"));
        let narrow = thumb_among(
            r#"[{"url":"https://x.cdninstagram.com/c.jpg","width":200},
                {"url":"https://x.cdninstagram.com/d.jpg","width":200}]"#,
        );
        assert_eq!(narrow.as_deref(), Some("https://x.cdninstagram.com/c.jpg"));
    }

    #[test]
    fn a_carousel_handed_over_without_its_items_is_drawn_from_its_own_pictures() {
        let post = Post::from_media(
            &media(
                r#"{"pk":"6","media_type":8,"carousel_media_count":3,
                "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/post.jpg","width":640}]}}"#,
            ),
            "x",
        );
        assert_eq!(post.items.len(), 1);
        assert_eq!(
            post.items[0].thumb.as_deref(),
            Some("https://x.cdninstagram.com/post.jpg")
        );
    }

    #[test]
    fn a_videos_thumbnail_is_its_cover_and_each_item_has_its_own() {
        let post = Post::from_media(
            &media(
                r#"{"pk":"5","media_type":8,
                "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/post.jpg","width":640}]},
                "carousel_media":[
                  {"pk":"51","media_type":1,
                   "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/one.jpg","width":640}]}},
                  {"pk":"52","media_type":2,
                   "video_versions":[{"url":"https://x.cdninstagram.com/two.mp4","width":720}],
                   "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/cover.jpg","width":640}]}},
                  {"pk":"53","media_type":1}
                ]}"#,
            ),
            "x",
        );
        let thumbs: Vec<Option<&str>> = post.items.iter().map(|i| i.thumb.as_deref()).collect();
        assert_eq!(
            thumbs,
            [
                Some("https://x.cdninstagram.com/one.jpg"),
                Some("https://x.cdninstagram.com/cover.jpg"),
                // Never another item's picture, the post's cover included.
                None,
            ]
        );
    }

    #[test]
    fn the_thumbnail_stays_out_of_the_json() {
        for (post, number) in golden_posts() {
            let json = serde_json::to_string(&post_json(&post, number)).unwrap();
            assert!(!json.contains("thumb"));
        }
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
        // Asked about from `/p/`, as the app opens a reel from a grid, and
        // still a reel's address when it is printed.
        assert_eq!(post.page(), "/p/DbGwql4IMei/");
        assert_eq!(post.link(), "https://www.instagram.com/reel/DbGwql4IMei/");
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

    /// The four shapes a post's JSON takes, made of invented accounts: a
    /// photo with a place and a liker, a carousel with a video, a reel with
    /// plays and hidden counts, and a post with no caption and no counts.
    fn golden_posts() -> [(Post, Option<usize>); 4] {
        let photo = Post::from_media(
            &media(
                r#"{"pk":"3995556159532654737","code":"DdzEShhEWCR","media_type":1,"taken_at":1790000000,
                "caption":{"text":"a line\nwith @friend"},"like_count":80,"comment_count":4,
                "top_likers":["close"],"location":{"name":"Somewhere"},
                "user":{"username":"owner.one"},
                "usertags":{"in":[{"user":{"username":"tagged.one"}}]},
                "coauthor_producers":[{"username":"with.one"}],
                "original_width":1080,"original_height":1350,
                "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/a.jpg","width":1080,"height":1350}]}}"#,
            ),
            "",
        );
        let carousel = Post::from_media(
            &media(
                r#"{"pk":"2","media_type":8,"taken_at":1790000001,"like_count":5,
                "carousel_media":[
                  {"pk":"21","media_type":1,"original_width":1080,"original_height":1080,
                   "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/c.jpg","width":1080,"height":1080}]}},
                  {"pk":"22","media_type":2,"video_versions":[{"url":"https://x.cdninstagram.com/d.mp4","width":720,"height":1280}]}
                ]}"#,
            ),
            "owner.two",
        );
        let reel = Post::from_media(
            &media(
                r#"{"pk":"3","product_type":"clips","media_type":2,"taken_at":1790000002,"like_count":3,
                "like_and_view_counts_disabled":true,"play_count":1234,"comment_count":0,
                "top_likers":["close"],
                "video_versions":[{"url":"https://x.cdninstagram.com/r.mp4","width":720,"height":1280}]}"#,
            ),
            "owner.three",
        );
        let bare = Post::from_media(&media(r#"{"pk":"4"}"#), "owner.four");
        [
            (photo, Some(1)),
            (carousel, Some(2)),
            (reel, None),
            (bare, None),
        ]
    }

    /// `printed` with only the keys `golden` has, at every depth: what the
    /// keys a document always had say, whatever keys were added beside them.
    fn only_the_keys_of(printed: &Value, golden: &Value) -> Value {
        match (printed, golden) {
            (Value::Object(printed), Value::Object(golden)) => Value::Object(
                golden
                    .iter()
                    .map(|(key, old)| {
                        let new = printed.get(key).cloned().unwrap_or(Value::Null);
                        (key.clone(), only_the_keys_of(&new, old))
                    })
                    .collect(),
            ),
            (Value::Array(printed), Value::Array(golden)) => Value::Array(
                printed
                    .iter()
                    .zip(golden.iter().chain(std::iter::repeat(&Value::Null)))
                    .map(|(new, old)| only_the_keys_of(new, old))
                    .collect(),
            ),
            (printed, _) => printed.clone(),
        }
    }

    /// The keys a post's JSON has always had, byte for byte: the contract
    /// of `snob posts` and `snob post`, which typed documents and the keys
    /// added since must not change. Taken from the output of 2026-11-13,
    /// before either.
    #[test]
    fn a_post_serializes_to_the_keys_it_always_had() {
        let golden = [
            r#"{"caption":"a line\nwith @friend","code":"DdzEShhEWCR","comments":4,"items":[{"height":1350,"kind":"photo","number":1,"url":"https://x.cdninstagram.com/a.jpg","width":1080}],"kind":"photo","liked_by":{"name":"close","others":79},"likes":80,"location":"Somewhere","mentions":["friend"],"number":1,"owner":"owner.one","pk":"3995556159532654737","plays":null,"tagged":["tagged.one"],"taken_at":1790000000,"url":"https://www.instagram.com/p/DdzEShhEWCR/","with":["with.one"]}"#,
            r#"{"caption":"","code":"AAAAAAAAAAC","comments":null,"items":[{"height":1080,"kind":"photo","number":1,"url":"https://x.cdninstagram.com/c.jpg","width":1080},{"height":1280,"kind":"video","number":2,"url":"https://x.cdninstagram.com/d.mp4","width":720}],"kind":"carousel","liked_by":null,"likes":5,"location":null,"mentions":[],"number":2,"owner":"owner.two","pk":"2","plays":null,"tagged":[],"taken_at":1790000001,"url":"https://www.instagram.com/p/AAAAAAAAAAC/","with":[]}"#,
            r#"{"caption":"","code":"AAAAAAAAAAD","comments":0,"items":[{"height":1280,"kind":"video","number":1,"url":"https://x.cdninstagram.com/r.mp4","width":720}],"kind":"reel","liked_by":{"name":"close","others":null},"likes":null,"location":null,"mentions":[],"owner":"owner.three","pk":"3","plays":1234,"tagged":[],"taken_at":1790000002,"url":"https://www.instagram.com/reel/AAAAAAAAAAD/","with":[]}"#,
            r#"{"caption":"","code":"AAAAAAAAAAE","comments":null,"items":[{"height":null,"kind":"photo","number":1,"url":null,"width":null}],"kind":"photo","liked_by":null,"likes":null,"location":null,"mentions":[],"owner":"owner.four","pk":"4","plays":null,"tagged":[],"taken_at":0,"url":"https://www.instagram.com/p/AAAAAAAAAAE/","with":[]}"#,
        ];
        for ((post, number), golden) in golden_posts().into_iter().zip(golden) {
            let old: Value = serde_json::from_str(golden).unwrap();
            let kept = only_the_keys_of(&post_json(&post, number), &old);
            assert_eq!(serde_json::to_string(&kept).unwrap(), golden);
        }
    }

    /// A comment's keys, byte for byte, its shown replies under it.
    #[test]
    fn a_comment_serializes_to_the_keys_it_always_had() {
        let reply = CommentLine {
            author: "replier".into(),
            text: "a reply".into(),
            at: Epoch::new(1_790_000_010),
            likes: 0,
            replies_count: 0,
            replies: Vec::new(),
            pk: Some("18000000000000002".into()),
            author_pk: Some("2".into()),
        };
        let comment = CommentLine {
            author: "commenter".into(),
            text: "a comment".into(),
            at: Epoch::new(1_790_000_005),
            likes: 7,
            replies_count: 3,
            replies: vec![reply],
            pk: Some("18000000000000001".into()),
            author_pk: None,
        };
        let golden = concat!(
            r#"{"author":"commenter","created_at":1790000005,"likes":7,"replies":3,"#,
            r#""shown_replies":[{"author":"replier","created_at":1790000010,"likes":0,"#,
            r#""replies":0,"shown_replies":[],"text":"a reply"}],"text":"a comment"}"#
        );
        let printed = comment_json(&comment);
        let old: Value = serde_json::from_str(golden).unwrap();
        assert_eq!(
            serde_json::to_string(&only_the_keys_of(&printed, &old)).unwrap(),
            golden
        );
        // And the keys added beside them.
        assert_eq!(printed["pk"], "18000000000000001");
        assert_eq!(printed["author_pk"], Value::Null);
        assert_eq!(printed["shown_replies"][0]["author_pk"], "2");
    }

    /// The line under a post names the likers the viewer knows, up to three,
    /// and counts the rest from the likes.
    #[test]
    fn the_liked_line_names_up_to_three_and_counts_the_rest() {
        let liked = |likes: u64, faces: &str| {
            Post::from_media(
                &media(&format!(
                    r#"{{"pk":"1","like_count":{likes},"facepile_top_likers":[{faces}]}}"#
                )),
                "x",
            )
            .liked_line()
        };
        let three =
            r#"{"pk":"1","username":"a"},{"pk":"2","username":"b"},{"pk":"3","username":"c"}"#;
        assert_eq!(
            liked(1_003, three).as_deref(),
            Some("Liked by a, b, c and 1,000 others")
        );
        assert_eq!(
            liked(4, three).as_deref(),
            Some("Liked by a, b, c and 1 other")
        );
        assert_eq!(liked(3, three).as_deref(), Some("Liked by a, b and c"));
        assert_eq!(
            liked(1, r#"{"pk":"1","username":"a"}"#).as_deref(),
            Some("Liked by a")
        );
        assert_eq!(liked(5, "").as_deref(), Some("5 likes"));
    }

    /// Under hidden counts the like count is a placeholder, 3 on every one
    /// of 21 such posts in the capture of 2026-11-13: never said, in a line
    /// or in the JSON.
    #[test]
    fn a_hidden_like_count_is_never_read_as_three() {
        let post = Post::from_media(
            &media(
                r#"{"pk":"1","like_count":3,"like_and_view_counts_disabled":true,
                "facepile_top_likers":[{"pk":"2","username":"a"}]}"#,
            ),
            "x",
        );
        assert_eq!(post.liked_line().as_deref(), Some("Liked by a and others"));
        let json = post_json(&post, None);
        assert_eq!(json["likes"], Value::Null);
        assert_eq!(json["likes_hidden"], true);
        assert!(!json.to_string().contains(":3"), "{json}");
    }

    /// What a post says beyond its counts goes into its JSON beside the keys
    /// it always had: the accounts by pk, the reposts and shares, the plays
    /// by network, the length, the music, the place with where it is, and
    /// its marks; and per item its own id and description.
    #[test]
    fn a_post_carries_what_its_answer_says_beside_the_old_keys() {
        let post = Post::from_media(
            &media(
                r#"{"pk":"5","product_type":"clips","media_type":2,"like_count":10,
                "user":{"pk":"42","username":"owner"},
                "coauthor_producers":[{"pk":"7","username":"with.one"}],
                "usertags":{"in":[{"user":{"pk":8,"username":"tagged.one"}}]},
                "facepile_top_likers":[{"pk":"9","username":"liker"}],
                "media_repost_count":4,"reshare_count":6,"share_count_disabled":false,
                "play_count":100,"ig_play_count":90,"fb_play_count":10,"video_duration":42.7,
                "clips_metadata":{"original_sound_info":{"original_audio_title":"Original audio",
                  "ig_artist":{"username":"owner"}}},
                "location":{"pk":"77","name":"Somewhere","lat":28.1,"lng":-15.4},
                "timeline_pinned_user_ids":[42],"is_paid_partnership":true,"caption_is_edited":true,
                "accessibility_caption":"A view",
                "video_versions":[{"url":"https://x.cdninstagram.com/r.mp4","width":720,"height":1280}]}"#,
            ),
            "",
        );
        let json = post_json(&post, None);
        assert_eq!(json["owner_pk"], "42");
        assert_eq!(
            json["with_accounts"],
            serde_json::json!([{"pk": "7", "username": "with.one"}])
        );
        assert_eq!(
            json["tagged_accounts"],
            serde_json::json!([{"pk": "8", "username": "tagged.one"}])
        );
        assert_eq!(
            json["likers_known"],
            serde_json::json!([{"pk": "9", "username": "liker"}])
        );
        assert_eq!(json["reposts"], 4);
        assert_eq!(json["shares"], 6);
        assert_eq!(json["plays"], 100);
        assert_eq!(json["plays_instagram"], 90);
        assert_eq!(json["plays_facebook"], 10);
        assert_eq!(json["duration"], 42.7);
        assert_eq!(json["music"]["kind"], "original");
        assert_eq!(json["music"]["title"], "Original audio");
        assert_eq!(json["place"]["pk"], "77");
        assert_eq!(json["place"]["lat"], 28.1);
        assert_eq!(json["location"], "Somewhere");
        assert_eq!(json["pinned"], true);
        assert_eq!(json["paid_partnership"], true);
        assert_eq!(json["caption_edited"], true);
        assert_eq!(json["items"][0]["pk"], "5");
        assert_eq!(json["items"][0]["alt"], "A view");
        assert_eq!(json["items"][0]["duration"], 42.7);
        assert_eq!(json["likes_hidden"], false);

        // Shares turned off are no count, and a photo says none.
        let off = Post::from_media(
            &media(r#"{"pk":"6","reshare_count":6,"share_count_disabled":true}"#),
            "x",
        );
        assert_eq!(post_json(&off, None)["shares"], Value::Null);
        // A pk that is not digits is not passed on.
        let odd = Post::from_media(&media(r#"{"pk":"6","user":{"pk":"4a2"}}"#), "x");
        assert_eq!(post_json(&odd, None)["owner_pk"], Value::Null);
    }

    #[test]
    fn a_length_reads_as_the_player_shows_it() {
        assert_eq!(length(0.0), "0:00");
        assert_eq!(length(42.7), "0:42");
        assert_eq!(length(62.0), "1:02");
        assert_eq!(length(3_723.0), "1:02:03");
        assert_eq!(length(-5.0), "0:00");
    }

    #[test]
    fn a_music_line_says_what_plays() {
        let music = |kind: &str, title: Option<&str>, artist: Option<&str>| document::Music {
            kind: kind.into(),
            title: title.map(str::to_string),
            artist: artist.map(str::to_string),
            artist_username: None,
        };
        assert_eq!(
            music_line(&music("song", Some("A Song"), Some("Singer"))).as_deref(),
            Some("Music: A Song · Singer")
        );
        assert_eq!(
            music_line(&music("original", Some("Original audio"), None)).as_deref(),
            Some("Sound: Original audio")
        );
        assert_eq!(music_line(&music("song", None, Some(" "))), None);
    }

    /// A video's best tracks are read from its manifest only when wider than
    /// its progressive file; compat drops them, best keeps them with the
    /// ffmpeg that joins them.
    #[test]
    fn a_video_is_saved_at_its_best_only_when_asked_and_wider() {
        let manifest = |width: u32| {
            format!(
                concat!(
                    r#"<MPD><Representation bandwidth="1" mimeType="video/mp4" width="{w}" height="1920">"#,
                    r#"<BaseURL>https://x.fbcdn.net/v</BaseURL></Representation>"#,
                    r#"<Representation bandwidth="1" mimeType="audio/mp4">"#,
                    r#"<BaseURL>https://x.fbcdn.net/a</BaseURL></Representation></MPD>"#
                ),
                w = width
            )
        };
        let reel = |width: u32| {
            Post::from_media(
                &media(
                    &serde_json::json!({
                        "pk": "5", "media_type": 2,
                        "video_versions": [{"url": "https://x.cdninstagram.com/r.mp4", "width": 720, "height": 1280}],
                        "video_dash_manifest": manifest(width),
                    })
                    .to_string(),
                ),
                "x",
            )
        };
        let wider = reel(1080);
        let best = wider.items[0].best.as_ref().expect("wider than 720");
        assert_eq!(best.video.url, "https://x.fbcdn.net/v");
        assert_eq!(
            best.audio.as_ref().map(|a| a.url.as_str()),
            Some("https://x.fbcdn.net/a")
        );
        assert!(reel(720).items[0].best.is_none(), "not wider");

        let compat = Videos::Compat { say_how: false }.prepare(vec![wider.file(0)]);
        assert!(compat[0].best.is_none() && compat[0].ffmpeg.is_none());
        let ffmpeg =
            std::sync::Arc::new(Ffmpeg::find(Some(&std::env::current_exe().unwrap())).unwrap());
        let at_best = Videos::Best(ffmpeg).prepare(vec![wider.file(0)]);
        assert!(at_best[0].best.is_some() && at_best[0].ffmpeg.is_some());
    }

    #[test]
    fn numbers_are_grouped_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(4_017_638), "4,017,638");
    }
}
