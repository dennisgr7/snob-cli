//! Models of a profile's posts, one post, and its comments, as the web app
//! reads them: the grid's two queries, `media/<pk>/info/` and
//! `media/<pk>/comments/`.
//!
//! The grid and the info read describe a post with the same fields, so one
//! type reads both. Every field but the pk is optional, as everywhere in
//! these models: the answers name dozens more, change them without notice,
//! and answer `null` where a post has nothing.

use serde::Deserialize;
use snob_core::Epoch;

use super::web::Connection;
use super::{Candidates, PictureVersion, flexible_id, nullable};
use crate::shortcode::MediaPk;

/// A post, a reel, or one item of a carousel.
#[derive(Debug, Clone, Deserialize)]
pub struct Media {
    pub pk: MediaPk,
    /// The code its address carries: eleven characters, or 39 for a private
    /// account's post.
    #[serde(default)]
    pub code: Option<String>,
    /// 1 a photo, 2 a video, 8 a carousel: Instagram's integer, turned into
    /// something meaningful where it is shown.
    #[serde(default)]
    pub media_type: u8,
    /// `feed`, `carousel_container`, `clips` (a reel), `carousel_item`.
    #[serde(default)]
    pub product_type: Option<String>,
    #[serde(default)]
    pub taken_at: Epoch,
    #[serde(default)]
    pub caption: Option<Caption>,
    #[serde(default)]
    pub like_count: Option<u64>,
    #[serde(default)]
    pub comment_count: Option<u64>,
    /// How many times a reel was played. The grid's answer leaves it out;
    /// the info read and the profile's reels carry it.
    #[serde(default)]
    pub play_count: Option<u64>,
    #[serde(default)]
    pub ig_play_count: Option<u64>,
    /// The owner hid the counts: the likes are then not said as a number.
    #[serde(default)]
    pub like_and_view_counts_disabled: Option<bool>,
    /// The accounts the app names under the post, "Liked by <name> and
    /// others": the viewer's own contacts who liked it, as plain names.
    #[serde(default, deserialize_with = "nullable")]
    pub top_likers: Vec<String>,
    #[serde(default)]
    pub usertags: Option<Usertags>,
    /// The accounts it is posted with, a collaboration's other authors.
    #[serde(default, deserialize_with = "nullable")]
    pub coauthor_producers: Vec<Account>,
    /// The owner.
    #[serde(default)]
    pub user: Option<Account>,
    #[serde(default)]
    pub location: Option<Location>,
    #[serde(default)]
    pub image_versions2: Option<Candidates>,
    /// A video's progressive files: one MP4 with its sound, 720 wide at most
    /// in the captures, offered three times under three `type`s. The higher
    /// resolutions are only in `video_dash_manifest`, as separate video and
    /// sound tracks, which snob does not join.
    #[serde(default, deserialize_with = "nullable")]
    pub video_versions: Vec<PictureVersion>,
    /// A carousel's items, each a [`Media`] of its own.
    #[serde(default, deserialize_with = "nullable")]
    pub carousel_media: Vec<Media>,
    #[serde(default)]
    pub carousel_media_count: Option<u32>,
    #[serde(default)]
    pub original_width: Option<u32>,
    #[serde(default)]
    pub original_height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Caption {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usertags {
    #[serde(default, rename = "in", deserialize_with = "nullable")]
    pub tagged: Vec<Usertag>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usertag {
    #[serde(default)]
    pub user: Option<Account>,
}

/// An account named on a post: its owner, a tagged account, a co-author or
/// a commenter. Only the name is read.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub full_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub name: Option<String>,
}

impl Media {
    /// What a person sees as the post's items: a carousel's, or the post
    /// itself when it is one photo or one video.
    pub fn items(&self) -> Vec<&Media> {
        if self.carousel_media.is_empty() {
            vec![self]
        } else {
            self.carousel_media.iter().collect()
        }
    }

    /// The best copy of this one item: a video's progressive file, the
    /// widest of them, and otherwise the largest picture. `None` when the
    /// answer offered none, which happens and is not a crash.
    pub fn best_url(&self) -> Option<&str> {
        widest(&self.video_versions).or_else(|| {
            self.image_versions2
                .as_ref()
                .and_then(|c| widest(&c.candidates))
        })
    }

    /// Whether this one item is a video.
    pub fn is_video(&self) -> bool {
        self.media_type == 2 || !self.video_versions.is_empty()
    }

    /// The caption's text, or nothing.
    pub fn caption_text(&self) -> &str {
        self.caption
            .as_ref()
            .and_then(|c| c.text.as_deref())
            .unwrap_or_default()
    }

    /// The owner's name, when the answer gave it.
    pub fn owner(&self) -> Option<&str> {
        self.user.as_ref().and_then(|u| u.username.as_deref())
    }

    /// The accounts tagged on it or on any of its items, each once, in the
    /// order they were first tagged.
    pub fn tagged(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let all = std::iter::once(self).chain(self.carousel_media.iter());
        for media in all {
            let tags = media.usertags.iter().flat_map(|t| t.tagged.iter());
            for name in tags.filter_map(|t| t.user.as_ref()?.username.as_deref()) {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    /// The names the caption mentions with an at sign, each once.
    pub fn mentioned(&self) -> Vec<String> {
        mentions_in(self.caption_text())
    }

    /// The accounts it is posted with.
    pub fn with(&self) -> Vec<String> {
        self.coauthor_producers
            .iter()
            .filter_map(|a| a.username.clone())
            .collect()
    }

    /// How many times it was played: a reel's count, from whichever field
    /// carried it.
    pub fn plays(&self) -> Option<u64> {
        self.play_count.or(self.ig_play_count)
    }

    /// The line the app shows under a post: "Liked by <name> and N others",
    /// from the first of [`Self::top_likers`] and the like count. `None` when
    /// nobody the viewer knows liked it, where the app shows the count alone.
    pub fn liked_by(&self) -> Option<LikedBy> {
        let name = self.top_likers.first()?.clone();
        let others = if self.like_and_view_counts_disabled == Some(true) {
            None
        } else {
            self.like_count.map(|n| n.saturating_sub(1))
        };
        Some(LikedBy { name, others })
    }
}

/// The widest of `versions`, the tallest among equals, the first among
/// those: the largest copy offered.
fn widest(versions: &[PictureVersion]) -> Option<&str> {
    versions
        .iter()
        .rev()
        .max_by_key(|v| (v.width.unwrap_or(0), v.height.unwrap_or(0)))
        .map(|v| v.url.as_str())
}

/// "Liked by <name> and N others", or "and others" when the owner hid the
/// count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LikedBy {
    pub name: String,
    pub others: Option<u64>,
}

/// The names `text` mentions: an at sign at the start or after a character
/// that is not part of a name, then one to thirty letters, digits, dots and
/// underscores, without the dots a sentence may end it with. Each once, in
/// order.
pub fn mentions_in(text: &str) -> Vec<String> {
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '_';
    let mut names: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    for (at, c) in text.char_indices() {
        let starts = c == '@' && !previous.is_some_and(name_char);
        previous = Some(c);
        if !starts {
            continue;
        }
        let rest = &text[at + 1..];
        let len = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
        let name = rest[..len].trim_end_matches('.');
        if crate::allowlist::profile_name(name) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// The answer of the grid's two queries: `data.<root>`, a connection of
/// posts (`allowlist::Operation::ProfilePosts`).
#[derive(Debug, Clone, Deserialize)]
pub struct TimelinePage {
    #[serde(default)]
    pub data: Option<TimelineData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimelineData {
    #[serde(
        default,
        rename = "xdt_api__v1__feed__user_timeline_graphql_connection"
    )]
    pub timeline: Option<Connection<Media>>,
}

impl TimelinePage {
    /// The posts, in the grid's order, and where the next page starts.
    ///
    /// **A short page is not the last one**: in the capture of 2026-10-01 one
    /// page of a profile's grid held two posts and said there were more. Only
    /// the cursor says where the grid ends.
    pub fn posts(self) -> (Vec<Media>, Option<String>) {
        let Some(timeline) = self.data.and_then(|d| d.timeline) else {
            return (Vec::new(), None);
        };
        let next = timeline.next_cursor().map(str::to_string);
        (timeline.nodes(), next)
    }
}

/// `media/<pk>/info/`: the post, as the only item of `items`.
#[derive(Debug, Clone, Deserialize)]
pub struct MediaInfo {
    #[serde(default, deserialize_with = "nullable")]
    pub items: Vec<Media>,
}

/// One page of `media/<pk>/comments/`.
#[derive(Debug, Clone, Deserialize)]
pub struct CommentsPage {
    #[serde(default, deserialize_with = "nullable")]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub comment_count: Option<u64>,
    #[serde(default)]
    pub has_more_comments: Option<bool>,
    #[serde(default)]
    pub has_more_headload_comments: Option<bool>,
    /// Where the next page starts: a JSON object as text, sent back as it
    /// came, as `min_id`.
    #[serde(default)]
    pub next_min_id: Option<String>,
}

impl CommentsPage {
    /// Where the next page starts, when there is one.
    ///
    /// **`has_more_comments` is no guide**: it was `false` on all 25 pages of
    /// the capture of 2026-10-01, a post with 2,797 comments among them. What
    /// the app pages by is `next_min_id`, which every page carried beside
    /// `has_more_headload_comments: true`. A page that says neither has more
    /// is the last.
    pub fn next(&self) -> Option<&str> {
        let more =
            self.has_more_headload_comments != Some(false) || self.has_more_comments == Some(true);
        self.next_min_id
            .as_deref()
            .filter(|next| more && !next.is_empty())
    }

    /// How many accounts the page names: each comment's author and the
    /// author of each reply it shows. What the page is charged to the day's
    /// accounts with.
    pub fn accounts(&self) -> usize {
        self.comments
            .iter()
            .map(|c| 1 + c.preview_child_comments.len())
            .sum()
    }
}

/// A comment, or a reply shown under one.
#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    #[serde(default, deserialize_with = "flexible_id")]
    pub pk: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub created_at: Epoch,
    #[serde(default)]
    pub user: Option<Account>,
    #[serde(default)]
    pub comment_like_count: Option<u64>,
    /// How many replies it has. Only the ones in
    /// [`Self::preview_child_comments`] are read: the replies call was not in
    /// any capture.
    #[serde(default)]
    pub child_comment_count: Option<u64>,
    #[serde(default, deserialize_with = "nullable")]
    pub preview_child_comments: Vec<Comment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid page as the capture shapes it: a carousel of two photos, a
    /// reel with its three progressive files, and the cursor.
    fn grid() -> &'static str {
        r#"{"data":{"xdt_api__v1__feed__user_timeline_graphql_connection":{
            "edges":[
              {"node":{"pk":"3995556159532654737","code":"DdzEShhEWCR","media_type":8,
                "product_type":"carousel_container","taken_at":1790527432,
                "caption":{"text":"sun with @friend.one and @friend.one. hi@nope"},
                "like_count":80,"comment_count":8,"top_likers":["close"],
                "like_and_view_counts_disabled":false,
                "usertags":null,"coauthor_producers":[{"username":"partner"}],
                "user":{"username":"someone"},"location":{"name":"A beach"},
                "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/big.jpg","width":1440,"height":1920},{"url":"https://x.cdninstagram.com/small.jpg","width":150,"height":150}]},
                "video_versions":null,
                "carousel_media":[
                  {"pk":"3995554512760065776","media_type":1,"usertags":{"in":[{"user":{"username":"tagged"}}]},
                   "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/a.jpg","width":1080,"height":1440}]},"video_versions":null},
                  {"pk":"3995554515981225378","media_type":2,
                   "image_versions2":{"candidates":[{"url":"https://x.cdninstagram.com/b.jpg","width":720,"height":1280}]},
                   "video_versions":[{"type":101,"width":720,"height":1280,"url":"https://x.cdninstagram.com/b.mp4"},{"type":102,"width":480,"height":854,"url":"https://x.cdninstagram.com/b-small.mp4"}]}
                ],"carousel_media_count":2}},
              {"node":{"pk":"3947056156557494178","code":"DbGwql4IMei","media_type":2,"product_type":"clips",
                "taken_at":1790000000,"caption":null,"like_count":206082,"comment_count":552,
                "like_and_view_counts_disabled":true,"top_likers":["close"],
                "video_versions":[{"type":101,"url":"https://x.cdninstagram.com/r.mp4"}],
                "carousel_media":null}}
            ],
            "page_info":{"end_cursor":"AQH-next","has_next_page":true}},
          "xdt_viewer":{"user":{"id":"42"}}},"extensions":{"is_final":true}}"#
    }

    #[test]
    fn a_grid_page_reads_its_posts_and_its_cursor() {
        let page: TimelinePage = serde_json::from_str(grid()).unwrap();
        let (posts, next) = page.posts();
        assert_eq!(next.as_deref(), Some("AQH-next"));
        assert_eq!(posts.len(), 2);

        let carousel = &posts[0];
        assert_eq!(carousel.pk, MediaPk::new(3_995_556_159_532_654_737));
        assert_eq!(carousel.items().len(), 2);
        assert_eq!(
            carousel.items()[0].best_url(),
            Some("https://x.cdninstagram.com/a.jpg")
        );
        assert!(carousel.items()[1].is_video());
        assert_eq!(
            carousel.items()[1].best_url(),
            Some("https://x.cdninstagram.com/b.mp4"),
            "the widest progressive file, not its cover"
        );
        assert_eq!(carousel.tagged(), ["tagged"]);
        assert_eq!(carousel.mentioned(), ["friend.one"]);
        assert_eq!(carousel.with(), ["partner"]);
        assert_eq!(carousel.owner(), Some("someone"));
        assert_eq!(
            carousel.liked_by(),
            Some(LikedBy {
                name: "close".into(),
                others: Some(79)
            })
        );

        let reel = &posts[1];
        assert_eq!(reel.items().len(), 1);
        assert_eq!(reel.caption_text(), "");
        assert_eq!(
            reel.liked_by(),
            Some(LikedBy {
                name: "close".into(),
                others: None
            }),
            "a hidden count is not said"
        );
    }

    #[test]
    fn the_last_grid_page_has_no_cursor() {
        let page: TimelinePage = serde_json::from_str(
            r#"{"data":{"xdt_api__v1__feed__user_timeline_graphql_connection":{"edges":[],"page_info":{"end_cursor":null,"has_next_page":false}}}}"#,
        )
        .unwrap();
        let (posts, next) = page.posts();
        assert!(posts.is_empty());
        assert_eq!(next, None);
    }

    /// The comments' next page is `next_min_id`, whatever
    /// `has_more_comments` says, as long as the page says more are loaded
    /// at its head.
    #[test]
    fn comments_page_by_their_next_min_id() {
        let page: CommentsPage = serde_json::from_str(
            r#"{"comments":[{"pk":"18177246133428661","text":"nice","created_at":1790527508,
                "user":{"username":"friend"},"comment_like_count":3,"child_comment_count":2,
                "preview_child_comments":[{"pk":1,"text":"yes","user":{"username":"owner"}}]}],
               "comment_count":836,"has_more_comments":false,"has_more_headload_comments":true,
               "next_min_id":"{\"cached_comments_cursor\":\"1\",\"bifilter_token\":\"x\"}"}"#,
        )
        .unwrap();
        assert_eq!(
            page.next(),
            Some(r#"{"cached_comments_cursor":"1","bifilter_token":"x"}"#)
        );
        assert_eq!(page.accounts(), 2);
        assert_eq!(page.comments[0].preview_child_comments.len(), 1);

        let last: CommentsPage = serde_json::from_str(
            r#"{"comments":[],"has_more_comments":false,"has_more_headload_comments":false,"next_min_id":"{}"}"#,
        )
        .unwrap();
        assert_eq!(last.next(), None);
    }

    #[test]
    fn a_mention_is_an_at_sign_and_a_name() {
        assert_eq!(
            mentions_in("@a.b, @c_d! mail@example.com (@e.) @@f @"),
            ["a.b", "c_d", "e", "f"]
        );
        assert!(mentions_in("no names here").is_empty());
    }
}
