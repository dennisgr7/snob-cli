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

use serde::Serialize;

/// One post or reel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
}

/// "Liked by `name` and `others` others".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LikedBy {
    pub name: String,
    /// `None` when the owner hid the counts.
    pub others: Option<u64>,
}

/// One file of a post: a photo, or a video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PostItem {
    /// From one, in the post's order.
    pub number: usize,
    /// `photo` or `video`.
    pub kind: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Where the file is; `None` when Instagram offered no copy of it.
    pub url: Option<String>,
}

/// One comment, with the replies shown under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PostComment {
    pub author: String,
    pub text: String,
    /// Seconds since the epoch.
    pub created_at: i64,
    pub likes: u64,
    /// How many replies it has; only `shown_replies` are in the answer.
    pub replies: u64,
    pub shown_replies: Vec<PostComment>,
}
