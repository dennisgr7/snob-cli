//! A post, its items and its comments as snob hands them out: the JSON of
//! `snob posts` and `snob post`.
//!
//! Separate from `snob_ig::model::post`, which is what Instagram sends and
//! changes when Instagram does, for the reason the module above gives. Every
//! string in these came off somebody else's account and is already filtered
//! for a terminal ([`super::printable`]) by whoever fills them in.
//!
//! **The order of the keys a reader sees is not the order of the fields.**
//! Each document goes out through a `serde_json::Value`, whose map keeps its
//! keys sorted (`serde_json` is built without `preserve_order`), so the keys
//! come out in alphabetical order, as they did when the JSON was written by
//! hand. A new field changes nothing of the old keys; it only adds its own.
//!
//! **No key for saves**: no answer of the web app carries a count of them
//! (none of 529 posts in the capture of 2026-11-13), and an empty one would
//! read as zero.

use serde::Serialize;

/// One post or reel.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Post {
    /// The media's id, as text: it does not fit every JSON reader's numbers.
    pub pk: String,
    /// As its address carries it.
    pub code: String,
    /// Its address.
    pub url: String,
    pub owner: String,
    /// `photo`, `video`, `reel` or `carousel`.
    pub kind: String,
    /// Seconds since the epoch.
    pub taken_at: i64,
    /// Its lines, joined by a newline; empty when there is none.
    pub caption: String,
    pub location: Option<String>,
    /// `None` when the owner hid the counts, or the answer said none.
    pub likes: Option<u64>,
    pub liked_by: Option<LikedBy>,
    pub comments: Option<u64>,
    pub plays: Option<u64>,
    pub tagged: Vec<String>,
    pub mentions: Vec<String>,
    /// The accounts it was posted with.
    pub with: Vec<String>,
    pub items: Vec<PostItem>,
    /// Its number in the listing it is in, when it is in one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<usize>,
    /// The owner's pk; the name is `owner`.
    pub owner_pk: Option<String>,
    /// The tagged accounts by pk and name; the names alone are `tagged`.
    pub tagged_accounts: Vec<AccountRef>,
    /// The accounts it is posted with by pk and name; the names alone are
    /// `with`.
    pub with_accounts: Vec<AccountRef>,
    /// The likers the viewer knows that the app names under it, at most
    /// three; `liked_by` is the first of the app's older field.
    pub likers_known: Vec<AccountRef>,
    /// The owner hid the counts: `likes` is then `null`, whatever the
    /// answer said, since it says a placeholder.
    pub likes_hidden: bool,
    /// How many accounts reposted it.
    pub reposts: Option<u64>,
    /// How many times it was shared: `null` for a photo, and when the owner
    /// turned the count off.
    pub shares: Option<u64>,
    /// A reel's plays on Instagram and on Facebook; `plays` is the sum.
    pub plays_instagram: Option<u64>,
    pub plays_facebook: Option<u64>,
    /// A video's length in seconds.
    pub duration: Option<f64>,
    pub music: Option<Music>,
    /// The place it was posted from, with where it is; `location` is its
    /// name alone.
    pub place: Option<Place>,
    /// Pinned to the top of its owner's grid, or of a co-author's.
    pub pinned: bool,
    /// Labeled as a paid partnership.
    pub paid_partnership: bool,
    /// Its caption was edited after it was posted.
    pub caption_edited: bool,
}

/// An account a post names, by pk and name. The pk is `null` where the
/// answer gave a name alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountRef {
    pub pk: Option<String>,
    pub username: String,
}

/// What a post plays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Music {
    /// `song` for licensed music, `original` for a video's own sound.
    pub kind: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The artist's account on Instagram, when it has one.
    pub artist_username: Option<String>,
}

/// A place a post names.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Place {
    pub pk: Option<String>,
    pub name: Option<String>,
    pub lat: Option<f64>,
    pub lng: Option<f64>,
}

/// "Liked by `name` and `others` others".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LikedBy {
    pub name: String,
    /// `None` when the owner hid the counts.
    pub others: Option<u64>,
}

/// One file of a post: a photo, or a video.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PostItem {
    /// From one, in the post's order.
    pub number: usize,
    /// `photo` or `video`.
    pub kind: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Where the file is; `None` when Instagram offered no copy of it.
    pub url: Option<String>,
    /// The item's own media id, a carousel's child's included.
    pub pk: Option<String>,
    /// The picture's description.
    pub alt: Option<String>,
    /// A video's length in seconds.
    pub duration: Option<f64>,
    /// A video's widest rendition and its sound, when wider than `url`'s
    /// file: what a download saves when ffmpeg is installed.
    pub best: Option<BestTracks>,
}

/// A video's widest rendition, a video track and a sound track apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BestTracks {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The video track's codec, `vp09...` as a rule.
    pub codec: Option<String>,
    /// Both tracks' size, as stated before any byte is fetched.
    pub bytes: Option<u64>,
    pub video_url: String,
    /// `null` for a video with no sound.
    pub audio_url: Option<String>,
}

/// One comment, with the replies shown under it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PostComment {
    pub author: String,
    pub text: String,
    /// Seconds since the epoch.
    pub created_at: i64,
    pub likes: u64,
    /// How many replies it has; only `shown_replies` are in the answer.
    pub replies: u64,
    pub shown_replies: Vec<PostComment>,
    /// The comment's id.
    pub pk: Option<String>,
    /// The author's pk; the name is `author`.
    pub author_pk: Option<String>,
}

/// One account that liked a post, as `snob likers` hands it out: a list's
/// account, and how the viewer stands with it when the answer said.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Liker {
    #[serde(flatten)]
    pub account: super::User,
    /// Whether the viewer follows it.
    pub you_follow: Option<bool>,
    /// Whether it follows the viewer.
    pub follows_you: Option<bool>,
}

/// The JSON document of `snob likers`: the post, how many liked it, and the
/// accounts Instagram showed of them, **a sample**: the web app is shown
/// about a hundred, the viewer's contacts first, and is never handed more.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Likers {
    /// As its address carries it.
    pub code: String,
    /// The post's address.
    pub url: String,
    /// How many accounts liked it. `None` when the owner hid the counts,
    /// which the answer is not taken to override.
    pub likes: Option<u64>,
    /// How many of them `accounts` holds.
    pub shown: usize,
    /// Whether `accounts` is every account that liked it: only when the
    /// count is known and they all came.
    pub complete: bool,
    pub accounts: Vec<Liker>,
}
