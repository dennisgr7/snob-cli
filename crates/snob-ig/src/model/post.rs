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
use super::{Candidates, PictureVersion, flexible_id, flexible_ids, nullable};
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
    /// others": the viewer's own contacts who liked it, as plain names. At
    /// most one in the capture of 2026-11-13.
    #[serde(default, deserialize_with = "nullable")]
    pub top_likers: Vec<String>,
    /// The same contacts as accounts, the faces the app draws beside that
    /// line: none to three in the capture of 2026-11-13, `null` on some
    /// posts, and a few entries with no name at all.
    #[serde(default, deserialize_with = "nullable")]
    pub facepile_top_likers: Vec<Account>,
    /// How many accounts reposted it.
    #[serde(default)]
    pub media_repost_count: Option<u64>,
    /// How many times it was shared: a reel's count, absent when the owner
    /// turned it off ([`Self::share_count_disabled`]).
    #[serde(default)]
    pub reshare_count: Option<u64>,
    #[serde(default)]
    pub share_count_disabled: Option<bool>,
    /// A reel's plays on Facebook, beside [`Self::ig_play_count`]; the two
    /// make [`Self::play_count`] where it is on both.
    #[serde(default)]
    pub fb_play_count: Option<u64>,
    /// A video's length in seconds.
    #[serde(default)]
    pub video_duration: Option<f64>,
    #[serde(default)]
    pub has_audio: Option<bool>,
    /// The DASH manifest of a video, an MPD as text: its renditions, the
    /// widest of them wider than [`Self::video_versions`] on most videos.
    #[serde(default)]
    pub video_dash_manifest: Option<String>,
    /// The picture's description, written by its owner or made by
    /// Instagram: per item on a carousel.
    #[serde(default)]
    pub accessibility_caption: Option<String>,
    /// The accounts whose grid it is pinned to the top of, by pk: its
    /// owner's or a co-author's when it is pinned (20 lists of 20 in the
    /// capture of 2026-11-13).
    #[serde(default, deserialize_with = "flexible_ids")]
    pub timeline_pinned_user_ids: Vec<String>,
    #[serde(default)]
    pub is_paid_partnership: Option<bool>,
    #[serde(default)]
    pub caption_is_edited: Option<bool>,
    /// A reel's sound: licensed music, or the original sound of a video.
    #[serde(default)]
    pub clips_metadata: Option<ClipsMetadata>,
    /// The music of a photo or a carousel; `null` on a reel.
    #[serde(default)]
    pub music_metadata: Option<MusicMetadata>,
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

/// An account named on a post: its owner, a tagged account, a co-author, a
/// liker the viewer knows, or a commenter.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Account {
    /// Text in most answers, a number in some nested ones (a reply's
    /// parent's author in the capture of 2026-11-13): read as either.
    #[serde(default, deserialize_with = "flexible_id")]
    pub pk: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_verified: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Location {
    #[serde(default, deserialize_with = "flexible_id")]
    pub pk: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lng: Option<f64>,
}

/// What a reel says of its sound.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClipsMetadata {
    /// `licensed_music` or `original_sounds`, and absent on some reels,
    /// which are then told apart by which of the two below is filled.
    #[serde(default)]
    pub audio_type: Option<String>,
    #[serde(default)]
    pub music_info: Option<MusicInfo>,
    #[serde(default)]
    pub original_sound_info: Option<OriginalSound>,
}

/// What a photo or a carousel says of its music.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MusicMetadata {
    #[serde(default)]
    pub audio_type: Option<String>,
    #[serde(default)]
    pub music_info: Option<MusicInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MusicInfo {
    #[serde(default)]
    pub music_asset_info: Option<MusicAsset>,
}

/// A licensed song.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MusicAsset {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub display_artist: Option<String>,
    #[serde(default)]
    pub ig_username: Option<String>,
}

/// A video's own sound, and the account it came from.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OriginalSound {
    #[serde(default)]
    pub original_audio_title: Option<String>,
    #[serde(default)]
    pub ig_artist: Option<Account>,
}

/// What a post plays, whichever of the two places said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Music {
    /// `true` for a licensed song, `false` for a video's own sound.
    pub licensed: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// The artist's account on Instagram, when it has one.
    pub artist_username: Option<String>,
}

/// An account a post names, by pk and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub pk: Option<String>,
    pub username: String,
}

/// The most likers the app names under a post, and so the most snob does:
/// three faces in the capture of 2026-11-13, never more.
pub const FACEPILE_SHOWN: usize = 3;

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

    /// The accounts tagged on it or on any of its items, with their pks,
    /// each once, in the order [`Self::tagged`] gives their names.
    pub fn tagged_accounts(&self) -> Vec<Named> {
        let mut found: Vec<Named> = Vec::new();
        let all = std::iter::once(self).chain(self.carousel_media.iter());
        for media in all {
            let tags = media.usertags.iter().flat_map(|t| t.tagged.iter());
            for account in tags.filter_map(|t| named(t.user.as_ref()?)) {
                if !found
                    .iter()
                    .any(|n| n.username.eq_ignore_ascii_case(&account.username))
                {
                    found.push(account);
                }
            }
        }
        found
    }

    /// The accounts it is posted with, with their pks.
    pub fn with_accounts(&self) -> Vec<Named> {
        self.coauthor_producers.iter().filter_map(named).collect()
    }

    /// The likers the viewer knows that the app names under the post, at
    /// most [`FACEPILE_SHOWN`]: the faces it draws, by pk and name, or the
    /// one name of [`Self::top_likers`] when it drew none.
    pub fn known_likers(&self) -> Vec<Named> {
        let faces: Vec<Named> = self
            .facepile_top_likers
            .iter()
            .filter_map(named)
            .take(FACEPILE_SHOWN)
            .collect();
        if !faces.is_empty() {
            return faces;
        }
        self.top_likers
            .iter()
            .take(FACEPILE_SHOWN)
            .map(|name| Named {
                pk: None,
                username: name.clone(),
            })
            .collect()
    }

    /// What it plays: a reel's song or its own sound, or a photo's song.
    pub fn music(&self) -> Option<Music> {
        let clips = self.clips_metadata.as_ref();
        let song = clips
            .and_then(|c| c.music_info.as_ref())
            .or_else(|| {
                self.music_metadata
                    .as_ref()
                    .and_then(|m| m.music_info.as_ref())
            })
            .and_then(|m| m.music_asset_info.as_ref())
            .filter(|song| song.title.is_some() || song.display_artist.is_some());
        if let Some(song) = song {
            return Some(Music {
                licensed: true,
                title: song.title.clone(),
                artist: song.display_artist.clone(),
                artist_username: song.ig_username.clone(),
            });
        }
        let sound = clips.and_then(|c| c.original_sound_info.as_ref())?;
        let artist = sound.ig_artist.as_ref().and_then(|a| a.username.clone());
        (sound.original_audio_title.is_some() || artist.is_some()).then(|| Music {
            licensed: false,
            title: sound.original_audio_title.clone(),
            artist: artist.clone(),
            artist_username: artist,
        })
    }

    /// Whether it is pinned to the top of its owner's grid, or of a
    /// co-author's: the list of accounts it is pinned for names one of them.
    pub fn pinned(&self) -> bool {
        let owner = self.user.as_ref().and_then(|u| u.pk.as_deref());
        let coauthors = self
            .coauthor_producers
            .iter()
            .filter_map(|a| a.pk.as_deref());
        owner
            .into_iter()
            .chain(coauthors)
            .any(|pk| self.timeline_pinned_user_ids.iter().any(|id| id == pk))
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

/// `account` by pk and name, when it has a name.
fn named(account: &Account) -> Option<Named> {
    Some(Named {
        pk: account.pk.clone(),
        username: account.username.clone()?,
    })
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
        let next = self
            .next_min_id
            .as_deref()
            .filter(|next| more && !next.is_empty())?;
        if !comments_cursor_has_its_shape(next) {
            // Ended rather than sent: a cursor is handed back as `min_id`
            // whole, so one of another shape would be text nobody vouched
            // for in a request. Said, since it ends the comments early.
            tracing::warn!(
                bytes = next.len(),
                "a page of comments handed out a cursor of a shape never seen; reading no further"
            );
            return None;
        }
        Some(next)
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

/// The longest comments cursor taken: the longest of the capture of
/// 2026-11-13 was 346 bytes (a 280-character token), and four times that
/// leaves room for a longer token without taking text of any length.
const MOST_CURSOR_BYTES: usize = 1_400;

/// Whether `cursor` has the shape of every `next_min_id` Instagram handed
/// out in the capture of 2026-11-13 (9 of 9 pages): a JSON object holding
/// `cached_comments_cursor`, a comment's pk in digits, and maybe
/// `bifilter_token`, base64 of either alphabet, and nothing else. What
/// [`CommentsPage::next`] hands on, and what
/// `IgClient::media_comments` sends.
pub fn comments_cursor_has_its_shape(cursor: &str) -> bool {
    if cursor.len() > MOST_CURSOR_BYTES {
        return false;
    }
    let Ok(serde_json::Value::Object(keys)) = serde_json::from_str(cursor) else {
        return false;
    };
    let digits = |v: &serde_json::Value| {
        v.as_str()
            .is_some_and(|s| (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()))
    };
    let base64 = |v: &serde_json::Value| {
        v.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_' | b'=')
                })
        })
    };
    keys.get("cached_comments_cursor").is_some_and(digits)
        && keys.iter().all(|(key, value)| match key.as_str() {
            "cached_comments_cursor" => true,
            "bifilter_token" => base64(value),
            _ => false,
        })
}

/// `media/<pk>/likers/`: the accounts that liked a post, as many as the app
/// is shown (105 of 603,328 in the capture of 2026-11-13, the same 105 both
/// times), and how many there are. **No cursor**: the app asked for no
/// second page, so there is none to read.
#[derive(Debug, Clone, Deserialize)]
pub struct LikersPage {
    #[serde(default, deserialize_with = "nullable")]
    pub users: Vec<Liker>,
    #[serde(default)]
    pub user_count: Option<u64>,
}

impl LikersPage {
    /// The likers that name an account, each with how the viewer stands
    /// with it when the answer said. One with no pk or no name is skipped
    /// rather than failing the rest.
    pub fn accounts(&self) -> Vec<(snob_core::model::User, Option<super::FriendshipStatus>)> {
        self.users
            .iter()
            .filter_map(|liker| {
                let pk = liker.pk.as_deref()?.parse().ok()?;
                let username = liker.username.clone().filter(|name| !name.is_empty())?;
                Some((
                    snob_core::model::User {
                        pk,
                        username,
                        full_name: liker.full_name.clone(),
                        is_private: liker.is_private,
                        is_verified: liker.is_verified,
                        pfp_url: liker.profile_pic_url.clone(),
                    },
                    liker.friendship_status.clone(),
                ))
            })
            .collect()
    }
}

/// One account in [`LikersPage`]: what a list's account carries, and how
/// the viewer stands with it, which is why the app's `show_many` after the
/// list is not needed.
#[derive(Debug, Clone, Deserialize)]
pub struct Liker {
    #[serde(default, deserialize_with = "flexible_id")]
    pub pk: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_verified: Option<bool>,
    #[serde(default)]
    pub profile_pic_url: Option<String>,
    #[serde(default)]
    pub friendship_status: Option<super::FriendshipStatus>,
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

    #[test]
    fn a_comments_cursor_is_taken_only_in_the_shape_instagram_hands_out() {
        let token = "QVFBd0RfZm9v-_=";
        for good in [
            format!(
                r#"{{"cached_comments_cursor":"18012345678901234","bifilter_token":"{token}"}}"#
            ),
            r#"{"cached_comments_cursor":"18012345678901234"}"#.to_string(),
        ] {
            assert!(comments_cursor_has_its_shape(&good), "{good}");
        }
        for bad in [
            String::new(),
            "18012345678901234".to_string(),
            r#"{"c":"1"}"#.to_string(),
            r#"{"bifilter_token":"QVFB"}"#.to_string(),
            r#"{"cached_comments_cursor":18012345678901234}"#.to_string(),
            r#"{"cached_comments_cursor":"12a"}"#.to_string(),
            r#"{"cached_comments_cursor":"1","bifilter_token":"a&b=c"}"#.to_string(),
            r#"{"cached_comments_cursor":"1","bifilter_token":""}"#.to_string(),
            r#"{"cached_comments_cursor":"1","sort_order":"newest"}"#.to_string(),
            format!(
                r#"{{"cached_comments_cursor":"1","bifilter_token":"{}"}}"#,
                "A".repeat(2_000)
            ),
        ] {
            assert!(!comments_cursor_has_its_shape(&bad), "{bad}");
        }
    }

    #[test]
    fn a_page_whose_cursor_has_another_shape_is_the_last() {
        let page = |next: &str| -> CommentsPage {
            serde_json::from_value(serde_json::json!({
                "comments": [], "has_more_headload_comments": true, "next_min_id": next,
            }))
            .unwrap()
        };
        let good = r#"{"cached_comments_cursor":"1","bifilter_token":"QVFB"}"#;
        assert_eq!(page(good).next(), Some(good));
        assert_eq!(page(r#"{"c":"1"}"#).next(), None);
    }

    /// The likers are read with their standing, a pk as text or as a
    /// number; one with no name or no pk is skipped, not the whole answer.
    #[test]
    fn the_likers_are_read_with_how_the_viewer_stands_with_each() {
        let page: LikersPage = serde_json::from_str(
            r#"{"users":[
              {"pk":"11","username":"first","is_private":true,
               "friendship_status":{"following":true,"followed_by":false}},
              {"pk":12,"username":"second"},
              {"pk":"13"},
              {"username":"nopk"}
            ],"user_count":603328,"status":"ok"}"#,
        )
        .unwrap();
        assert_eq!(page.user_count, Some(603_328));
        let accounts = page.accounts();
        let names: Vec<&str> = accounts.iter().map(|(u, _)| u.username.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
        assert_eq!(accounts[0].0.pk, snob_core::Pk::new(11));
        assert!(
            accounts[0]
                .1
                .as_ref()
                .is_some_and(|f| f.following && !f.followed_by)
        );
        assert!(accounts[1].1.is_none());
        let empty: LikersPage = serde_json::from_str(r#"{"users":null}"#).unwrap();
        assert!(empty.accounts().is_empty());
    }

    fn media(json: &str) -> Media {
        serde_json::from_str(json).unwrap()
    }

    /// The faces the app draws are the known likers, at most three, by pk
    /// and name; one with no name is skipped, a `null` list is an empty one,
    /// and with no faces the one name of `top_likers` stands in.
    #[test]
    fn a_facepile_entry_with_no_name_is_skipped() {
        let faces = media(
            r#"{"pk":"1","top_likers":["first"],"facepile_top_likers":[
              {"pk":"11","username":"first"},
              {"id":"12","profile_pic_url":"https://x.cdninstagram.com/p.jpg"},
              {"pk":13,"username":"second"},
              {"pk":"14","username":"third"},
              {"pk":"15","username":"fourth"}]}"#,
        );
        let names: Vec<_> = faces
            .known_likers()
            .into_iter()
            .map(|n| (n.pk, n.username))
            .collect();
        assert_eq!(
            names,
            [
                (Some("11".into()), "first".into()),
                (Some("13".into()), "second".into()),
                (Some("14".into()), "third".into()),
            ]
        );
        let none = media(r#"{"pk":"1","top_likers":["first"],"facepile_top_likers":null}"#);
        assert!(none.facepile_top_likers.is_empty());
        assert_eq!(
            none.known_likers(),
            [Named {
                pk: None,
                username: "first".into()
            }]
        );
        assert!(media(r#"{"pk":"1"}"#).known_likers().is_empty());
    }

    /// An account's pk reads from text and from a number alike, as both
    /// occur; so do the ids a post is pinned for.
    #[test]
    fn an_account_pk_is_read_as_text_or_as_a_number() {
        let text: Account = serde_json::from_str(r#"{"pk":"42","username":"a"}"#).unwrap();
        let number: Account = serde_json::from_str(r#"{"pk":42,"username":"a"}"#).unwrap();
        assert_eq!(text.pk.as_deref(), Some("42"));
        assert_eq!(number.pk.as_deref(), Some("42"));
        let pinned = media(r#"{"pk":"1","timeline_pinned_user_ids":[42,"43"]}"#);
        assert_eq!(pinned.timeline_pinned_user_ids, ["42", "43"]);
        let none = media(r#"{"pk":"1","timeline_pinned_user_ids":null}"#);
        assert!(none.timeline_pinned_user_ids.is_empty());
    }

    /// Pinned is the owner's or a co-author's pk among the accounts it is
    /// pinned for, never another's.
    #[test]
    fn a_post_is_pinned_for_its_owner_or_a_coauthor() {
        let owners = media(r#"{"pk":"1","user":{"pk":"42"},"timeline_pinned_user_ids":[42]}"#);
        assert!(owners.pinned());
        let coauthors = media(
            r#"{"pk":"1","user":{"pk":"42"},"coauthor_producers":[{"pk":"7","username":"b"}],
            "timeline_pinned_user_ids":["7"]}"#,
        );
        assert!(coauthors.pinned());
        let anothers = media(r#"{"pk":"1","user":{"pk":"42"},"timeline_pinned_user_ids":["9"]}"#);
        assert!(!anothers.pinned());
        assert!(!media(r#"{"pk":"1"}"#).pinned());
    }

    /// A reel's song, a reel's own sound, and a photo's song, each read from
    /// where its kind of post says it; a sound that names nothing is none.
    #[test]
    fn music_is_read_from_a_reel_or_a_photo() {
        let song = media(
            r#"{"pk":"1","clips_metadata":{"audio_type":"licensed_music","music_info":
              {"music_asset_info":{"title":"A Song","display_artist":"Singer","ig_username":"singer"}}}}"#,
        );
        assert_eq!(
            song.music(),
            Some(Music {
                licensed: true,
                title: Some("A Song".into()),
                artist: Some("Singer".into()),
                artist_username: Some("singer".into()),
            })
        );
        let sound = media(
            r#"{"pk":"1","clips_metadata":{"audio_type":"original_sounds","music_info":null,
              "original_sound_info":{"original_audio_title":"Original audio","ig_artist":{"pk":"2","username":"maker"}}}}"#,
        );
        assert_eq!(
            sound.music(),
            Some(Music {
                licensed: false,
                title: Some("Original audio".into()),
                artist: Some("maker".into()),
                artist_username: Some("maker".into()),
            })
        );
        let photo = media(
            r#"{"pk":"1","clips_metadata":null,"music_metadata":{"audio_type":"licensed_music",
              "music_info":{"music_asset_info":{"title":"Tune","display_artist":"Band","ig_username":null}}}}"#,
        );
        assert_eq!(photo.music().and_then(|m| m.title), Some("Tune".into()));
        let silent = media(r#"{"pk":"1","clips_metadata":{"original_sound_info":{}}}"#);
        assert_eq!(silent.music(), None);
    }

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
