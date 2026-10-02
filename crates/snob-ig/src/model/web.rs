//! Models of the answers to the web client's own calls: its Relay queries
//! and its route definitions.
//!
//! Each converts into the type the REST route already fills, so a command
//! reads the same value whichever route answered. GraphQL answers `null`
//! where REST leaves a field out, so every field here is optional, lists
//! included.

use serde::Deserialize;
use serde_json::Value;
use snob_core::Pk;

use super::{
    CountEdge, Counters, Highlight, HighlightsTray, MutualEdge, PictureVersion, Reel, UsernameEdge,
    UsernameNode, Via, WebProfileInfo, flexible_pk,
};

/// Answer of `PolarisProfilePageContentQuery`: `data.user`, and
/// `data.viewer.user` for whom it was asked by.
#[derive(Debug, Clone, Deserialize)]
pub struct ProfilePage {
    #[serde(default)]
    pub data: Option<ProfilePageData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfilePageData {
    #[serde(default)]
    pub user: Option<GraphUser>,
    #[serde(default)]
    pub viewer: Option<PageViewer>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageViewer {
    #[serde(default)]
    pub user: Option<ViewerUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ViewerUser {
    #[serde(deserialize_with = "flexible_pk")]
    pub pk: Pk,
}

impl ProfilePage {
    pub fn user(&self) -> Option<&GraphUser> {
        self.data.as_ref()?.user.as_ref()
    }

    /// Whether the profile is the viewer's own. Decided by pk, because the
    /// relationship is `null` there rather than a relationship with oneself.
    pub fn is_self(&self) -> bool {
        let viewer = self
            .data
            .as_ref()
            .and_then(|d| d.viewer.as_ref())
            .and_then(|v| v.user.as_ref());
        matches!((viewer, self.user()), (Some(v), Some(u)) if v.pk == u.pk)
    }

    /// The profile, in the shape `web_profile_info` fills. `None` when the
    /// answer carries no user, or one without a username.
    pub fn profile(self) -> Option<WebProfileInfo> {
        let is_self = self.is_self();
        self.data?.user?.into_profile(is_self)
    }
}

/// An account as the web client's GraphQL queries describe it: the profile
/// query's `data.user` and the hover card's `user_dict`.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphUser {
    #[serde(deserialize_with = "flexible_pk")]
    pub pk: Pk,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub biography: Option<String>,
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default)]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_verified: Option<bool>,
    #[serde(default)]
    pub is_business: Option<bool>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub follower_count: Option<u64>,
    #[serde(default)]
    pub following_count: Option<u64>,
    #[serde(default)]
    pub media_count: Option<u64>,
    #[serde(default)]
    pub profile_pic_url: Option<String>,
    /// The full-size picture, with no downscale in the URL and no size
    /// beside it.
    #[serde(default)]
    pub hd_profile_pic_url_info: Option<PictureVersion>,
    /// The default avatar, which is not worth downloading.
    #[serde(default)]
    pub has_anonymous_profile_picture: Option<bool>,
    /// When the newest story was posted, `0` with none up.
    #[serde(default)]
    pub latest_reel_media: Option<i64>,
    /// `null` on the viewer's own profile.
    #[serde(default)]
    pub friendship_status: Option<Relationship>,
    /// `null` on the viewer's own profile.
    #[serde(default)]
    pub mutual_followers_count: Option<u64>,
    /// The names in the "Followed by" line, first to last.
    #[serde(default)]
    pub profile_context_links_with_user_ids: Option<Vec<ContextLink>>,
}

/// The viewer's relationship to an account. Every flag is optional, as on
/// [`WebProfileInfo`]: unknown must never block anything.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Relationship {
    #[serde(default)]
    pub following: Option<bool>,
    #[serde(default)]
    pub followed_by: Option<bool>,
    #[serde(default)]
    pub outgoing_request: Option<bool>,
    #[serde(default)]
    pub incoming_request: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextLink {
    #[serde(default)]
    pub username: Option<String>,
}

impl GraphUser {
    /// The two counters, which the hover card carries with no username
    /// beside them for [`Self::into_profile`] to need.
    pub fn counters(&self) -> Counters {
        Counters {
            followers: self.follower_count,
            following: self.following_count,
        }
    }

    /// The profile, in the shape `web_profile_info` fills, each field from
    /// its counterpart in the profile query (`follower_count` for
    /// `edge_followed_by.count`, `friendship_status` for the relationship,
    /// `hd_profile_pic_url_info` for the full-size picture). There is no
    /// highlight count on this route, so that stays unknown. `is_self` drops
    /// the relationship, which the viewer's own profile has none of.
    pub fn into_profile(self, is_self: bool) -> Option<WebProfileInfo> {
        let username = self.username?;
        let relationship = if is_self {
            None
        } else {
            self.friendship_status
        };
        let flag = |pick: fn(&Relationship) -> Option<bool>| relationship.as_ref().and_then(pick);
        let mutual = self.mutual_followers_count.map(|count| MutualEdge {
            count,
            edges: self
                .profile_context_links_with_user_ids
                .unwrap_or_default()
                .into_iter()
                .filter_map(|link| link.username)
                .map(|username| UsernameEdge {
                    node: UsernameNode { username },
                })
                .collect(),
        });
        Some(WebProfileInfo {
            via: Via::Graph,
            id: self.pk,
            username,
            full_name: self.full_name,
            is_private: self.is_private,
            is_verified: self.is_verified,
            followed_by_viewer: flag(|r| r.following),
            requested_by_viewer: flag(|r| r.outgoing_request),
            profile_pic_url: self.profile_pic_url,
            profile_pic_url_hd: self.hd_profile_pic_url_info.map(|p| p.url),
            followers: self.follower_count.map(|count| CountEdge { count }),
            following: self.following_count.map(|count| CountEdge { count }),
            follows_viewer: flag(|r| r.followed_by),
            has_requested_viewer: flag(|r| r.incoming_request),
            biography: self.biography,
            external_url: self.external_url,
            posts: self.media_count.map(|count| CountEdge { count }),
            mutual,
            highlight_reel_count: None,
            is_business_account: self.is_business,
            category_name: self.category,
            anonymous_picture: self.has_anonymous_profile_picture,
            latest_reel_media: self.latest_reel_media,
        })
    }
}

/// Answer of `PolarisUserHoverCardContentV2Query`:
/// `data.xig_user_by_igid_v2.user_dict`, the counters by pk.
#[derive(Debug, Clone, Deserialize)]
pub struct HoverCard {
    #[serde(default)]
    pub data: Option<HoverCardData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HoverCardData {
    #[serde(default)]
    pub xig_user_by_igid_v2: Option<HoverCardUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HoverCardUser {
    #[serde(default)]
    pub user_dict: Option<GraphUser>,
}

impl HoverCard {
    pub fn user(&self) -> Option<&GraphUser> {
        self.data
            .as_ref()?
            .xig_user_by_igid_v2
            .as_ref()?
            .user_dict
            .as_ref()
    }
}

/// A Relay connection: the nodes and where the next page starts.
#[derive(Debug, Clone, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Connection<T> {
    #[serde(default)]
    pub edges: Option<Vec<Edge<T>>>,
    #[serde(default)]
    pub page_info: Option<PageInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Edge<T> {
    pub node: T,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageInfo {
    #[serde(default)]
    pub end_cursor: Option<String>,
    #[serde(default)]
    pub has_next_page: Option<bool>,
}

impl<T> Connection<T> {
    pub fn nodes(self) -> Vec<T> {
        self.edges
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.node)
            .collect()
    }

    /// The cursor of the next page, when the answer says there is one.
    pub fn next_cursor(&self) -> Option<&str> {
        let info = self.page_info.as_ref()?;
        if info.has_next_page != Some(true) {
            return None;
        }
        info.end_cursor.as_deref().filter(|c| !c.is_empty())
    }
}

/// Answer of `PolarisProfileStoryHighlightsTrayContentQuery`:
/// `data.highlights`. Its nodes carry an id, a title and a cover, and no item
/// count or dates.
#[derive(Debug, Clone, Deserialize)]
pub struct HighlightsTrayPage {
    #[serde(default)]
    pub data: Option<HighlightsTrayData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HighlightsTrayData {
    #[serde(default)]
    pub highlights: Option<Connection<Highlight>>,
}

impl HighlightsTrayPage {
    /// The tray, in the shape the REST route fills, with its counts and
    /// dates unknown.
    pub fn tray(self) -> HighlightsTray {
        HighlightsTray {
            tray: self
                .data
                .and_then(|d| d.highlights)
                .map(Connection::nodes)
                .unwrap_or_default(),
        }
    }
}

/// Answer of the reel queries on `/graphql/query` (a gallery, its next
/// page, and the highlights page): one connection of reels under
/// `xdt_api__v1__feed__reels_media__connection`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReelsPage {
    #[serde(default)]
    pub data: Option<ReelsData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReelsData {
    #[serde(default, rename = "xdt_api__v1__feed__reels_media__connection")]
    pub reels: Option<Connection<Reel>>,
}

impl ReelsPage {
    fn connection(&self) -> Option<&Connection<Reel>> {
        self.data.as_ref()?.reels.as_ref()
    }

    /// The cursor of the next page of reels.
    pub fn next_cursor(&self) -> Option<&str> {
        self.connection()?.next_cursor()
    }

    /// The reels, in the order of the answer.
    pub fn reels(self) -> Vec<Reel> {
        self.data
            .and_then(|d| d.reels)
            .map(Connection::nodes)
            .unwrap_or_default()
    }

    /// The reels as highlights. A highlight's items never expire, but this
    /// answer gives each one a day after it was taken, long past; that is
    /// dropped, as the REST answer leaves it out.
    pub fn highlights(self) -> Vec<Reel> {
        let mut reels = self.reels();
        for item in reels.iter_mut().flat_map(|reel| reel.items.iter_mut()) {
            item.expiring_at = None;
        }
        reels
    }
}

/// What the route definitions say about one route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RouteAnswer {
    /// A profile's route, and the user's pk.
    Pk(Pk),
    /// The route is not a profile's: a redirect, or an entry whose props
    /// carry no numeric `id`. No account goes by the name.
    NoProfile,
    /// The answer says nothing about the route: an error, a body that is not
    /// the envelope, or no entry for it. Not a missing account.
    Error,
}

/// What the answer of `/ajax/bulk-route-definitions/` says about
/// `route_url`: the `id` of its route props, which is how the app knows a
/// profile's pk without asking.
///
/// The answer is a Comet one, JSON behind `for (;;);`, with each asked
/// route under `payload.payloads`. Only that route's entry is read, and a
/// `props` with a numeric `id` is taken wherever the entry nests it,
/// searched key by key in the order the keys sort, not the order they were
/// written. A profile's route carries one, the user's pk. An envelope or an
/// entry carrying an `error` is an [`RouteAnswer::Error`], never a missing
/// account: what the error means is not known here.
pub fn route_answer(body: &str, route_url: &str) -> RouteAnswer {
    let json = body.trim_start().strip_prefix("for (;;);").unwrap_or(body);
    let Ok(answer) = serde_json::from_str::<Value>(json) else {
        return RouteAnswer::Error;
    };
    let Some(route) = answer
        .get("payload")
        .and_then(|p| p.get("payloads"))
        .and_then(|p| p.get(route_url))
    else {
        return RouteAnswer::Error;
    };
    if erred(&answer) || erred(route) {
        return RouteAnswer::Error;
    }
    let redirect = route
        .get("result")
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        == Some("route_redirect");
    match props_id(route) {
        Some(pk) if !redirect => RouteAnswer::Pk(pk),
        _ => RouteAnswer::NoProfile,
    }
}

/// Whether a Comet envelope, or one entry of it, carries an error: an
/// `error` that is neither `false` nor `null`.
fn erred(value: &Value) -> bool {
    value
        .get("error")
        .is_some_and(|e| !matches!(e, Value::Null | Value::Bool(false)))
}

/// The pk a route definition carries for `route_url`, when it names a
/// profile.
pub fn route_pk(body: &str, route_url: &str) -> Option<Pk> {
    match route_answer(body, route_url) {
        RouteAnswer::Pk(pk) => Some(pk),
        RouteAnswer::NoProfile | RouteAnswer::Error => None,
    }
}

/// The pk of the profile a document was served for: the route props the
/// profile document embeds, `props.id`, read from its inline
/// `<script type="application/json">` blocks in order, the first numeric
/// one each holds, by the same walk as [`route_answer`].
pub fn document_pk(html: &str) -> Option<Pk> {
    const OPEN: &str = "<script";
    const CLOSE: &str = "</script>";
    let mut rest = html;
    while let Some(at) = rest.find(OPEN) {
        let tag = &rest[at..];
        let end = tag.find('>')?;
        let attributes = &tag[OPEN.len()..end];
        let body = &tag[end + 1..];
        let close = body.find(CLOSE)?;
        if attributes.contains(r#"type="application/json""#)
            && let Ok(json) = serde_json::from_str::<Value>(&body[..close])
            && let Some(pk) = props_id(&json)
        {
            return Some(pk);
        }
        rest = &body[close + CLOSE.len()..];
    }
    None
}

/// The ids of the reels in a stories tray, in the order it lists them:
/// `tray[].id` of the object after `text`'s first
/// `"xdt_api__v1__feed__reels_tray"`, as the home document preloads it; a
/// reel with no id is left out. `None` when there is no tray, or it does
/// not read whole.
pub fn tray_ids(text: &str) -> Option<Vec<String>> {
    const MARKER: &str = r#""xdt_api__v1__feed__reels_tray""#;
    let after = &text[text.find(MARKER)? + MARKER.len()..];
    let after = after.trim_start().strip_prefix(':')?;
    let tray: Value = serde_json::Deserializer::from_str(after)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let ids = tray
        .get("tray")?
        .as_array()?
        .iter()
        .filter_map(|reel| match reel.get("id")? {
            Value::String(id) if !id.is_empty() => Some(id.clone()),
            Value::Number(id) => Some(id.to_string()),
            _ => None,
        })
        .collect();
    Some(ids)
}

fn props_id(value: &Value) -> Option<Pk> {
    match value {
        Value::Object(map) => {
            if let Some(pk) = map
                .get("props")
                .and_then(|props| props.get("id"))
                .and_then(pk_of)
            {
                return Some(pk);
            }
            map.values().find_map(props_id)
        }
        Value::Array(items) => items.iter().find_map(props_id),
        _ => None,
    }
}

fn pk_of(value: &Value) -> Option<Pk> {
    match value {
        Value::String(text) => text.parse().ok(),
        Value::Number(n) => n.as_u64().map(Pk::new),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::largest;

    /// Another account's profile, shaped like the capture's, with made-up
    /// values.
    const PROFILE: &str = r#"{"data":{"user":{"pk":"1000000001","id":"1000000001",
        "fbid_v2":"17800000000000001","username":"someone","full_name":"Some One",
        "biography":"hello","bio_links":[],"external_url":null,"is_private":true,
        "is_verified":true,"is_business":false,"category":"Artist","account_type":1,
        "follower_count":1200,"following_count":340,"media_count":0,
        "total_clips_count":null,"profile_pic_url":"https://cdn.test/s.jpg",
        "hd_profile_pic_url_info":{"url":"https://cdn.test/full.jpg"},
        "has_anonymous_profile_picture":false,"latest_reel_media":0,
        "has_story_archive":null,
        "friendship_status":{"following":false,"followed_by":true,
            "outgoing_request":true,"incoming_request":false,"blocking":false,
            "muting":false,"is_restricted":false,"is_bestie":false,
            "is_feed_favorite":false},
        "mutual_followers_count":31,
        "profile_context_links_with_user_ids":[{"start":0,"end":3,"username":"a"},
            {"start":5,"end":6,"username":"b"}]},
        "viewer":{"user":{"pk":"2000000002","id":"2000000002"}}},
        "extensions":{"is_final":true}}"#;

    /// Every row of the mapping table, in one answer.
    #[test]
    fn the_profile_query_fills_what_web_profile_info_did() {
        let page: ProfilePage = serde_json::from_str(PROFILE).unwrap();
        assert!(!page.is_self());
        let u = page.profile().unwrap();
        assert_eq!(u.via, Via::Graph);
        assert!(u.counters_are_knowable());
        assert_eq!(u.anonymous_picture, Some(false));
        assert_eq!(u.latest_reel_media, Some(0));
        assert!(u.counters_are_knowable());
        assert_eq!(u.id, Pk::new(1_000_000_001));
        assert_eq!(u.username, "someone");
        assert_eq!(u.full_name.as_deref(), Some("Some One"));
        assert_eq!(u.biography.as_deref(), Some("hello"));
        assert_eq!(u.is_verified, Some(true));
        assert_eq!(u.follower_count(), Some(1200));
        assert_eq!(u.following_count(), Some(340));
        assert_eq!(u.posts.map(|p| p.count), Some(0));
        assert_eq!(u.is_private, Some(true));
        assert_eq!(u.followed_by_viewer, Some(false));
        assert_eq!(u.requested_by_viewer, Some(true));
        assert_eq!(u.follows_viewer, Some(true));
        assert_eq!(u.has_requested_viewer, Some(false));
        let mutual = u.mutual.as_ref().unwrap();
        assert_eq!(mutual.count, 31);
        assert_eq!(mutual.names().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(u.is_business_account, Some(false));
        assert_eq!(u.category_name.as_deref(), Some("Artist"));
        assert_eq!(
            u.profile_pic_url_hd.as_deref(),
            Some("https://cdn.test/full.jpg")
        );
        assert_eq!(u.highlight_reel_count, None, "this route has no count");
    }

    /// On the viewer's own profile the relationship is `null`, and that is
    /// not "not following": it is nobody's relationship.
    #[test]
    fn the_own_profile_is_known_by_pk_and_has_no_relationship() {
        let own = r#"{"data":{"user":{"pk":"2000000002","username":"me",
            "follower_count":10,"following_count":20,"friendship_status":null,
            "mutual_followers_count":null,"profile_context_links_with_user_ids":null},
            "viewer":{"user":{"pk":2000000002}}}}"#;
        let page: ProfilePage = serde_json::from_str(own).unwrap();
        assert!(page.is_self());
        let u = page.profile().unwrap();
        assert_eq!(u.followed_by_viewer, None);
        assert_eq!(u.requested_by_viewer, None);
        assert!(u.mutual.is_none());
        assert_eq!(u.follower_count(), Some(10));
    }

    #[test]
    fn a_missing_user_is_no_profile() {
        let page: ProfilePage =
            serde_json::from_str(r#"{"data":{"user":null,"viewer":null}}"#).unwrap();
        assert!(!page.is_self());
        assert!(page.profile().is_none());
    }

    #[test]
    fn the_hover_card_carries_the_counters_by_pk() {
        let card = r#"{"data":{"xig_user_by_igid_v2":{"user_dict":{"pk":1000000001,
            "follower_count":1200,"following_count":340,"media_count":5,
            "is_private":false,"friendship_status":{"following":true}}}}}"#;
        let card: HoverCard = serde_json::from_str(card).unwrap();
        let u = card.user().unwrap();
        assert_eq!(u.pk, Pk::new(1_000_000_001));
        assert_eq!(
            u.counters(),
            Counters {
                followers: Some(1200),
                following: Some(340),
            }
        );
        assert_eq!(u.username, None);
        assert_eq!(
            u.friendship_status.as_ref().and_then(|r| r.following),
            Some(true)
        );
    }

    #[test]
    fn a_highlights_tray_has_no_counts_or_dates() {
        let tray = r#"{"data":{"highlights":{"edges":[{"node":{"id":"highlight:17900000000000001",
            "title":"trip","cover_media":{"cropped_image_version":{"url":"https://cdn.test/c.jpg"}},
            "user":{"id":"1000000001"}}},{"node":{"id":"highlight:17900000000000002",
            "title":null,"cover_media":null}}],
            "page_info":{"end_cursor":null,"has_next_page":false}}}}"#;
        let tray = serde_json::from_str::<HighlightsTrayPage>(tray)
            .unwrap()
            .tray()
            .tray;
        assert_eq!(tray.len(), 2);
        assert_eq!(tray[0].id, "highlight:17900000000000001");
        assert_eq!(tray[0].title.as_deref(), Some("trip"));
        assert_eq!(tray[0].media_count, None);
        assert_eq!(tray[0].created_at, None);
        assert_eq!(tray[0].updated_timestamp, None);
        assert!(tray[1].cover_media.is_none());

        let hidden: HighlightsTrayPage =
            serde_json::from_str(r#"{"data":{"highlights":{"edges":[]}}}"#).unwrap();
        assert!(hidden.tray().tray.is_empty());
    }

    /// A gallery reel, shaped like the capture's: mentions in a sticker,
    /// videos without sizes.
    const GALLERY: &str = r#"{"data":{"xdt_api__v1__feed__reels_media__connection":{
        "edges":[{"node":{"id":"1000000001","reel_type":"user_reel",
            "user":{"pk":"1000000001","username":"someone"},
            "items":[{"pk":"3600000000000000001","id":"3600000000000000001_1000000001",
                "code":"AbCd","media_type":2,"taken_at":1700000000,"expiring_at":1700086400,
                "image_versions2":{"candidates":[{"url":"https://cdn.test/s.jpg","width":320,"height":568},
                    {"url":"https://cdn.test/b.jpg","width":1080,"height":1920}]},
                "video_versions":[{"type":101,"url":"https://cdn.test/first.mp4"},
                    {"type":102,"url":"https://cdn.test/second.mp4"},
                    {"type":103,"url":"https://cdn.test/third.mp4"}],
                "video_dash_manifest":"<MPD/>",
                "story_bloks_stickers":[{"bloks_sticker":{"sticker_data":{
                    "ig_mention":{"username":"friend","full_name":"A Friend"}}}},
                    {"bloks_sticker":{"sticker_data":{"other":{}}}}]}]},
            "cursor":"1000000001"},
            {"node":{"id":"1000000003","user":{"pk":"1000000003","username":"other"},"items":null}}],
        "page_info":{"end_cursor":"1000000003","has_next_page":true}}}}"#;

    #[test]
    fn a_gallery_reads_as_reels() {
        let page: ReelsPage = serde_json::from_str(GALLERY).unwrap();
        assert_eq!(page.next_cursor(), Some("1000000003"));
        let reels = page.reels();
        assert_eq!(reels.len(), 2);
        assert_eq!(reels[0].id.as_deref(), Some("1000000001"));
        assert!(reels[1].items.is_empty(), "a reel not loaded yet");

        let item = &reels[0].items[0];
        assert_eq!(item.mentioned().collect::<Vec<_>>(), ["friend"]);
        assert_eq!(
            largest(&item.video_versions).unwrap().url,
            "https://cdn.test/first.mp4",
            "with no sizes, the first listed wins, not the last"
        );
        assert_eq!(
            largest(&item.image_versions2.as_ref().unwrap().candidates)
                .unwrap()
                .url,
            "https://cdn.test/b.jpg"
        );
        assert!(item.expiring_at.is_some(), "a story does expire");
    }

    /// A highlight's items carry an expiry a day after they were taken, which
    /// would list them all as expired.
    #[test]
    fn highlight_items_do_not_expire() {
        let page = r#"{"data":{"xdt_api__v1__feed__reels_media__connection":{
            "edges":[{"node":{"id":"highlight:17900000000000001","reel_type":"highlight_reel",
            "items":[{"pk":"3600000000000000009","media_type":1,"taken_at":1600000000,
            "expiring_at":1600086400}]}}],
            "page_info":{"has_next_page":false,"end_cursor":"x"}}}}"#;
        let page: ReelsPage = serde_json::from_str(page).unwrap();
        assert_eq!(
            page.next_cursor(),
            None,
            "no next page, whatever the cursor"
        );
        let reels = page.highlights();
        assert_eq!(reels[0].id.as_deref(), Some("highlight:17900000000000001"));
        assert_eq!(reels[0].items[0].expiring_at, None);
    }

    /// A made-up route definition, nested the way a Comet answer nests one.
    const ROUTES: &str = r#"for (;;);{"__ar":1,"payload":{"payloads":{
        "/someone/":{"error":false,"result":{"type":"route_definition","exports":{
            "rootView":{"props":{"page_logging":"profile"}},
            "hostableView":{"props":{"id":"1000000001","polaris_route":"profile"}}}}},
        "/other/":{"error":false,"result":{"type":"route_definition","exports":{
            "hostableView":{"props":{"id":1000000003}}}}},
        "/explore/tags/x/":{"error":false,"result":{"type":"route_redirect",
            "redirect_result":{"url":"/explore/search/keyword/?q=x"}}}}}}"#;

    #[test]
    fn a_route_definition_names_the_profile_pk() {
        assert_eq!(route_pk(ROUTES, "/someone/"), Some(Pk::new(1_000_000_001)));
        assert_eq!(route_pk(ROUTES, "/other/"), Some(Pk::new(1_000_000_003)));
        assert_eq!(route_pk(ROUTES, "/explore/tags/x/"), None);
        assert_eq!(route_pk(ROUTES, "/nobody/"), None, "only the asked route");
        assert_eq!(route_pk("<html>", "/someone/"), None);
    }

    /// A profile's route, one that is none, and an answer that says nothing
    /// about the route, told apart.
    #[test]
    fn a_route_answer_tells_a_missing_account_from_an_error() {
        assert_eq!(
            route_answer(ROUTES, "/someone/"),
            RouteAnswer::Pk(Pk::new(1_000_000_001))
        );
        assert_eq!(
            route_answer(ROUTES, "/explore/tags/x/"),
            RouteAnswer::NoProfile
        );
        let propless = r#"for (;;);{"payload":{"payloads":{"/nobody/":{"error":false,
            "result":{"type":"route_definition","exports":{"rootView":{"props":{"id":"x"}}}}}}}}"#;
        assert_eq!(route_answer(propless, "/nobody/"), RouteAnswer::NoProfile);
        assert_eq!(route_answer(ROUTES, "/nobody/"), RouteAnswer::Error);
        assert_eq!(route_answer("<html>", "/someone/"), RouteAnswer::Error);
        // An invented error envelope, in the Comet shape.
        let envelope = r#"for (;;);{"__ar":1,"error":1000001,"errorSummary":"Something",
            "errorDescription":"Something went wrong","payload":null}"#;
        assert_eq!(route_answer(envelope, "/someone/"), RouteAnswer::Error);
        let entry = r#"for (;;);{"payload":{"payloads":{"/someone/":{"error":true,
            "result":{"exports":{"hostableView":{"props":{"id":"1000000001"}}}}}}}}"#;
        assert_eq!(route_answer(entry, "/someone/"), RouteAnswer::Error);
    }

    /// The route props a profile document embeds, in an invented document.
    #[test]
    fn a_profile_document_names_its_pk() {
        let document = r#"<!DOCTYPE html><html><head>
            <script type="application/json" data-sjs>{"require":[["Badge",[],{"count":3}]]}</script>
            <script>var props = {"id": 7};</script>
            </head><body>
            <script type="application/json" data-content-len="80" data-sjs>{"require":[["RouteProps",null,null,[{"rootView":{"props":{"id":"1000000001","page_logging":"profile"}}}]]]}</script>
            <script type="application/json">{"props":{"id":"1000000009"}}</script>
            </body></html>"#;
        assert_eq!(document_pk(document), Some(Pk::new(1_000_000_001)));
        let home = r#"<html><script type="application/json">{"require":[["Badge",[],{"count":3}]]}</script>
            <script>{"props":{"id":"1000000001"}}</script></html>"#;
        assert_eq!(document_pk(home), None);
        assert_eq!(document_pk(""), None);
        assert_eq!(
            document_pk(r#"<script type="application/json">{"props":{"id":"1"#),
            None
        );
    }

    /// The home document's preload, cut from its marker the way the tab
    /// hands it over, with invented reels.
    #[test]
    fn a_tray_lists_its_reels_in_order() {
        let preload = r#""xdt_api__v1__feed__reels_tray":{"tray":[{"id":"1000000003",
            "user":{"username":"other"},"seen":0},{"id":1000000001,"latest_reel_media":1700000000},
            {"user":{}},{"id":"highlight:17900000000000001"}],"broadcasts":[]}},"extensions":{}}]]]}"#;
        assert_eq!(
            tray_ids(preload),
            Some(vec![
                "1000000003".to_string(),
                "1000000001".to_string(),
                "highlight:17900000000000001".to_string(),
            ])
        );
        assert_eq!(
            tray_ids(r#"x "xdt_api__v1__feed__reels_tray" : {"tray":[]}"#),
            Some(Vec::new())
        );
        assert_eq!(tray_ids("no tray here"), None);
        assert_eq!(
            tray_ids(r#""xdt_api__v1__feed__reels_tray":{"tray":[{"id":"1"#),
            None,
            "cut short"
        );
        assert_eq!(tray_ids(r#""xdt_api__v1__feed__reels_tray":null"#), None);
    }

    #[test]
    fn the_full_size_picture_has_no_size() {
        let page: ProfilePage = serde_json::from_str(PROFILE).unwrap();
        let picture = page
            .user()
            .unwrap()
            .hd_profile_pic_url_info
            .as_ref()
            .unwrap();
        assert_eq!(picture.url, "https://cdn.test/full.jpg");
        assert_eq!((picture.width, picture.height), (None, None));
    }
}
