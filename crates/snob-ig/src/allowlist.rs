//! The calls snob may send from the page, and the check that refuses the
//! rest before they leave.
//!
//! **An allowlist, because the app's own writes cannot be listed.** The web
//! app sends seen signals, view counts, time spent and search history on its
//! own, under names and numbers that change. Naming what may go out holds
//! whatever they are called; `crates/snob-core/tests/no_seen.rs` names the
//! known ones as a backstop, and is the only file that may.
//!
//! The registry is [`Operation`], [`Rest`], the route definitions and the
//! two calls a list is opened and read with: the profile, hover card,
//! highlight and story queries the app reads with, the two writes, the REST
//! friendship lists, [`crate::web::BULK_ROUTE_DEFINITIONS`],
//! [`crate::web::NAVIGATION`] and [`crate::web::SHOW_MANY`]. Call sites pass these values,
//! never a path or a name, and every client's page goes through [`refused`],
//! which reads the request itself. What a command asks the tab for
//! ([`crate::web::Ask`]) is held to the same registry by [`refused_ask`],
//! before it is handed over and again where the request is built.
//!
//! **It is written for the calls snob sends, and the tab holds the app's
//! to it too.** The app keeps running on each document the tab loads (the
//! home page, a profile) and sends its own calls from it, seen signals among
//! them. The browser pauses each of its API calls and judges a POST with
//! [`refused`] before it leaves (`snob-cli`'s `headless::guard`), with one
//! named exception there; its GETs go on.
//!
//! What [`refused`] lets out:
//!
//! - a POST that is one of the registry's, on its own endpoint, carrying its
//!   own `doc_id`. A write's `doc_id` rotates and is found beside the write's
//!   name in the app's code, so for the two writes only the name and the
//!   endpoint are held;
//! - the route definitions; the app's navigation to the home page, a
//!   profile or a profile's mutual followers ([`list_route`]), and no other
//!   route; and `friendships/show_many` with nothing in its form but the pks
//!   it asks about and the token, which is a read sent as a POST (see
//!   [`show_many`]);
//! - a GET fetch of one of the registry's REST reads: the followers, the
//!   following or the mutual followers of an account, by its pk. Every
//!   other read goes out as a query of the registry's, so a GET a command
//!   was not moved off fails here instead of reaching Instagram;
//! - a navigation to the home page or to a profile, never to one of the
//!   app's own screens, some of which mark what they show as seen.
//!
//! **A read's `doc_id` is pinned, and follows the app when it moves.** The
//! number is what picks the query Instagram runs, so a name alone never lets
//! one out. When the app is seen to send one of the registry's reads under
//! another number, [`vouched`] checks the call (the name, the endpoint, the
//! shape of its variables, the root field it names) and [`Rediscovered`]
//! holds the number as the one to send; [`refused_with`] lets it out and
//! nothing else. A number never vouched for is refused as it always was.
//!
//! **What the app's own traffic may do beyond that** is [`refused_app`]'s:
//! the reads it sends on every document it loads (`APP_READS`), by name and
//! number, or by a number the call vouches for where the app moves it, and
//! nothing it writes.

use std::collections::HashMap;

use snob_core::Pk;
use url::Url;

use crate::client::page::{Method, PageRequest};
use crate::graphql::Mutation;
use crate::shortcode::MediaPk;
use crate::web::{Ask, BULK_ROUTE_DEFINITIONS, NAVIGATION, SHOW_MANY};

/// Which Relay endpoint an operation is sent to, which decides two of its
/// headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `POST /api/graphql`.
    Relay,
    /// `POST /graphql/query`, which names the answer's root field in a header.
    RelayQuery,
}

/// A Relay operation snob may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Operation {
    /// A profile's header, by pk.
    ProfilePage,
    /// A profile's counters, by pk: the cheapest header read.
    HoverCard,
    /// A profile's highlights, without their items.
    HighlightsTray,
    /// The items of highlights.
    HighlightsPage,
    /// The stories of the reels in a tray.
    ReelGallery,
    /// More reels of the same tray.
    ReelGalleryPage,
    /// The stories tray: which reels there are, in the order the gallery
    /// takes them.
    StoriesTray,
    /// The note over a profile's picture, by pk: one of the reads the app
    /// sends with every profile it opens, and snob with every one it opens
    /// (`IgClient::profile_by_pk`). Its answer is not read.
    NoteBubble,
    /// Whether a profile carries a school's badge, by pk: the other read of
    /// that burst whose answer is not read.
    SchoolPartnerBadge,
    /// The first page of a profile's posts, by name: what the app sends with
    /// every profile it opens, and the grid `snob posts` lists.
    ProfilePosts,
    /// The grid's next page, by name and the cursor the page before ended
    /// at: what the app sends as the grid is scrolled.
    ProfilePostsPage,
    Follow,
    Unfollow,
}

impl Operation {
    pub const ALL: [Self; 13] = [
        Self::ProfilePage,
        Self::HoverCard,
        Self::HighlightsTray,
        Self::HighlightsPage,
        Self::ReelGallery,
        Self::ReelGalleryPage,
        Self::StoriesTray,
        Self::NoteBubble,
        Self::SchoolPartnerBadge,
        Self::ProfilePosts,
        Self::ProfilePostsPage,
        Self::Follow,
        Self::Unfollow,
    ];

    /// The operation the app calls `name`, if the registry holds it.
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.friendly_name() == name)
    }

    pub fn friendly_name(self) -> &'static str {
        match self {
            Self::ProfilePage => "PolarisProfilePageContentQuery",
            Self::HoverCard => "PolarisUserHoverCardContentV2Query",
            Self::HighlightsTray => "PolarisProfileStoryHighlightsTrayContentQuery",
            Self::HighlightsPage => "PolarisStoriesV3HighlightsPageQuery",
            Self::ReelGallery => "PolarisStoriesV3ReelPageGalleryQuery",
            Self::ReelGalleryPage => "PolarisStoriesV3ReelPageGalleryPaginationQuery",
            Self::StoriesTray => "PolarisStoriesV3TrayContainerQuery",
            Self::NoteBubble => "PolarisProfileNoteBubbleQuery",
            Self::SchoolPartnerBadge => "PolarisSchoolPartnerProfileBadgeQuery",
            Self::ProfilePosts => "PolarisProfilePostsQuery",
            Self::ProfilePostsPage => "PolarisProfilePostsTabContentQuery_connection",
            Self::Follow => Mutation::Follow.friendly_name(),
            Self::Unfollow => Mutation::Unfollow.friendly_name(),
        }
    }

    /// The operation's `doc_id`, as captured on 2026-09-29, and unchanged in
    /// the capture of 2026-10-01; the note and the badge are from both. A
    /// write's is the seed its discovery starts from.
    ///
    /// The grid's two are from the captures of 2026-10-01, and the first page's
    /// is known to move: it was 28570182382647478 two days before. A moved one
    /// is sent under the number the app's own call vouches for ([`vouched`]),
    /// which every profile document the tab loads sends.
    pub fn doc_id(self) -> &'static str {
        match self {
            Self::ProfilePage => "28036671149327607",
            Self::HoverCard => "28949219061332276",
            Self::HighlightsTray => "26970053832668570",
            Self::HighlightsPage => "28325328583775973",
            Self::ReelGallery => "28262315486766731",
            Self::ReelGalleryPage => "28606543452290985",
            Self::StoriesTray => "27703822975903310",
            Self::NoteBubble => "38260824646898178",
            Self::SchoolPartnerBadge => "28647748031575520",
            Self::ProfilePosts => "28991540097136703",
            Self::ProfilePostsPage => "29240983615539641",
            Self::Follow => Mutation::Follow.seed_doc_id(),
            Self::Unfollow => Mutation::Unfollow.seed_doc_id(),
        }
    }

    /// The endpoint the app sends it to. It never sends one operation to
    /// both.
    pub fn family(self) -> Family {
        match self {
            Self::HighlightsPage
            | Self::ReelGallery
            | Self::ReelGalleryPage
            | Self::ProfilePosts
            | Self::ProfilePostsPage => Family::RelayQuery,
            _ => Family::Relay,
        }
    }

    pub fn path(self) -> &'static str {
        match self.family() {
            Family::RelayQuery => "/graphql/query",
            _ => "/api/graphql",
        }
    }

    /// The field of `data` the answer is under. On `/graphql/query` it is
    /// also sent, as `X-Root-Field-Name`. Every one is the field the recorded
    /// app's answers came under and its header named; an answer under another
    /// field does not decode, and its debug line names the fields it was
    /// under. The gallery's next page is never sent: one account's reel comes
    /// whole in the gallery's first answer.
    pub fn root_field(self) -> &'static str {
        match self {
            Self::ProfilePage => "user",
            Self::HoverCard => "xig_user_by_igid_v2",
            Self::HighlightsTray => "highlights",
            Self::HighlightsPage | Self::ReelGallery | Self::ReelGalleryPage => {
                "xdt_api__v1__feed__reels_media__connection"
            }
            Self::StoriesTray => "xdt_api__v1__feed__reels_tray",
            Self::NoteBubble => "xdt_get_inbox_tray_items",
            Self::SchoolPartnerBadge => "xig_user_by_igid_v2",
            Self::ProfilePosts | Self::ProfilePostsPage => {
                "xdt_api__v1__feed__user_timeline_graphql_connection"
            }
            Self::Follow => "xdt_create_friendship",
            Self::Unfollow => "xdt_destroy_friendship",
        }
    }

    /// The write this is, if it is one.
    pub fn write(self) -> Option<Mutation> {
        match self {
            Self::Follow => Some(Mutation::Follow),
            Self::Unfollow => Some(Mutation::Unfollow),
            _ => None,
        }
    }

    /// The operation `mutation` is sent as.
    pub fn of_write(mutation: Mutation) -> Self {
        match mutation {
            Mutation::Follow => Self::Follow,
            Mutation::Unfollow => Self::Unfollow,
        }
    }

    /// The keys of the operation's variables that name what is asked, in the
    /// order the app sends them, the first of them the one that tells the
    /// operation's call from another's. A call copies the app's other keys,
    /// its flags, rather than invent them.
    pub fn asked_keys(self) -> &'static [&'static str] {
        match self {
            Self::ProfilePage => &["id"],
            Self::HoverCard => &["userID"],
            Self::HighlightsTray | Self::NoteBubble => &["user_id"],
            Self::SchoolPartnerBadge => &["igid"],
            Self::HighlightsPage | Self::ReelGallery => {
                &["initial_reel_id", "reel_ids", "first", "last"]
            }
            Self::ReelGalleryPage => &[
                "initial_reel_id",
                "after",
                "before",
                "first",
                "is_highlight",
                "last",
                "reel_ids",
            ],
            Self::StoriesTray => &["data", "suggestedUsersData"],
            Self::ProfilePosts => &["username"],
            // `after` first: it is what tells the next page from the first.
            Self::ProfilePostsPage => &["after", "before", "first", "last", "username"],
            Self::Follow | Self::Unfollow => &["target_user_id"],
        }
    }
}

/// Whether the app's own call, which sent the registry's read `operation`
/// under `doc_id`, is one whose number may stand in for the registry's.
///
/// What is checked is what a number moved to another query could not
/// keep: the call is a read of the registry's, on its own endpoint, whose
/// variables carry the key that names what it asks ([`Operation::asked_keys`])
/// and whose root field is the operation's where the operation is sent on
/// `/graphql/query`, in `X-Root-Field-Name`, and absent on `/api/graphql`,
/// as the app sends it. An answer that is not under the operation's root
/// field does not decode afterwards, whatever number it was asked under.
pub fn vouched(
    operation: Operation,
    path: &str,
    doc_id: &str,
    variables: &str,
    root_field: Option<&str>,
) -> bool {
    let asked = operation.asked_keys().first().copied();
    let named = serde_json::from_str::<serde_json::Value>(variables)
        .ok()
        .is_some_and(|variables| asked.is_some_and(|key| variables.get(key).is_some()));
    let root_ok = match operation.family() {
        Family::RelayQuery => root_field == Some(operation.root_field()),
        Family::Relay => root_field.is_none(),
    };
    operation.write().is_none() && path == operation.path() && is_doc_id(doc_id) && named && root_ok
}

/// A `doc_id`: digits, and as many as Instagram's have ever been.
fn is_doc_id(doc_id: &str) -> bool {
    (10..=20).contains(&doc_id.len()) && doc_id.bytes().all(|b| b.is_ascii_digit())
}

/// The numbers the app was seen to send the registry's reads under, when
/// they are not the registry's: what [`refused_with`] lets a read go out
/// under besides it.
///
/// An entry is made only from a call [`vouched`] held for, by whoever sees
/// the app's calls; this module holds none of its own, so a request from
/// nowhere is refused as ever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rediscovered(HashMap<Operation, String>);

impl Rediscovered {
    /// Holds `doc_id` for the read `operation`, if that is a number a read
    /// can have. A write's is found in the app's code instead.
    pub fn of(operation: Operation, doc_id: &str) -> Self {
        let mut found = Self::default();
        found.insert(operation, doc_id);
        found
    }

    /// Holds `doc_id` for `operation` in place of any it held, for a number
    /// [`vouched`] held for.
    pub fn insert(&mut self, operation: Operation, doc_id: &str) {
        if operation.write().is_none() && is_doc_id(doc_id) {
            self.0.insert(operation, doc_id.to_string());
        }
    }

    /// The number held for `operation`.
    pub fn get(&self, operation: Operation) -> Option<&str> {
        self.0.get(&operation).map(String::as_str)
    }
}

/// A REST read snob may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Rest {
    Followers(Pk),
    Following(Pk),
    MutualFollowers(Pk),
    /// A post, by its pk: what the app asks the moment a post or a reel
    /// opens, from a profile's grid, its reels or a message (about twenty
    /// times in the captures of 2026-10-01, always beside [`Rest::Comments`]).
    MediaInfo(MediaPk),
    /// A page of a post's comments, by its pk. The first page is asked with
    /// the post; the next ones as the comments are scrolled, by the
    /// `min_id` the page before handed out.
    Comments(MediaPk),
}

impl Rest {
    pub fn path(self) -> String {
        match self {
            Self::Followers(pk) => format!("/api/v1/friendships/{pk}/followers/"),
            Self::Following(pk) => format!("/api/v1/friendships/{pk}/following/"),
            Self::MutualFollowers(pk) => format!("/api/v1/friendships/{pk}/mutual_followers/"),
            Self::MediaInfo(pk) => format!("/api/v1/media/{pk}/info/"),
            Self::Comments(pk) => format!("/api/v1/media/{pk}/comments/"),
        }
    }

    /// The query keys the app sends with the read, and the one value a key
    /// is held to, where it is.
    fn keys(self) -> &'static [(&'static str, Option<&'static str>)] {
        match self {
            Self::Followers(_) => &[
                ("count", None),
                ("max_id", None),
                ("search_surface", Some("follow_list_page")),
            ],
            Self::Following(_) => &[("count", None), ("max_id", None)],
            Self::MutualFollowers(_) => &[("page_size", None), ("max_id", None)],
            Self::MediaInfo(_) => &[],
            // The first page carries the first two, the next ones the first,
            // the cursor and the order, as the app sends them.
            Self::Comments(_) => &[
                ("can_support_threading", Some("true")),
                ("permalink_enabled", Some("false")),
                ("min_id", None),
                ("sort_order", Some("popular")),
            ],
        }
    }
}

/// Whether `request`, a POST to `path`, is one of the registry's.
fn posted(request: &PageRequest, path: &str, found: &Rediscovered) -> bool {
    match path {
        BULK_ROUTE_DEFINITIONS => true,
        NAVIGATION => navigation(request),
        SHOW_MANY => show_many(request),
        _ => operation_posted(request, path, found).is_some(),
    }
}

/// The fields of a form, in its order.
fn form_of(request: &PageRequest) -> Vec<(String, String)> {
    url::form_urlencoded::parse(request.body.as_deref().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// Whether `request` is the app's navigation to a route snob may be on: one
/// `route_url`, and that one a [`list_route`]. The rest of the form is the
/// app's own envelope, which a navigation shares with the route
/// definitions.
///
/// The app sends it whenever its router moves (the capture of 2026-10-01
/// holds one for every profile opened and every mutual-followers tab), and
/// its answer is the route's definition: what the page will show, and
/// nothing about what was seen. Held to the routes snob reads from, so that
/// the app's own navigations to its screens, the story viewer among them,
/// stay refused.
fn navigation(request: &PageRequest) -> bool {
    let form = form_of(request);
    let mut routes = form.iter().filter(|(name, _)| name == "route_url");
    let route = routes.next();
    routes.next().is_none() && route.is_some_and(|(_, route)| list_route(route))
}

/// The most accounts a `show_many` may ask about: more than the twelve a
/// list page is asked for (`pace::ACCOUNTS_PER_PAGE`) and the fifty a list
/// endpoint serves at most, so a page of either fits.
pub const MOST_STATUSES: usize = 50;

/// Whether `request` is `friendships/show_many` as the app sends it after a
/// list page: `user_ids`, a comma-separated list of between one and
/// [`MOST_STATUSES`] pks, then `jazoest` and `fb_dtsg`, and nothing else.
///
/// **A read, though it is a POST.** It asks how the viewer stands with the
/// accounts a page just listed (`friendship_statuses`: following, outgoing
/// request, private, restricted) and changes nothing; the app sends it after
/// every list page it loads, searches and mutual followers included (16 of
/// 16 in the capture of 2026-10-01). Being a POST is why it was once kept
/// out, when POST meant write here; the rule now is that a write is a
/// [`crate::graphql::Mutation`] and nothing else, which this is not. It is
/// paid for as a read, out of the read budgets, and can never reach the
/// write bucket: it is asked as `web::Ask::Statuses`, and only
/// `web::Ask::Write` pays that one.
fn show_many(request: &PageRequest) -> bool {
    let form = form_of(request);
    let names: Vec<&str> = form.iter().map(|(name, _)| name.as_str()).collect();
    let field = |name: &str| {
        form.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    names == ["user_ids", "jazoest", "fb_dtsg"]
        && field("user_ids").is_some_and(|ids| statuses_of(ids).is_some())
        && field("jazoest").is_some_and(|j| !j.is_empty() && j.bytes().all(|b| b.is_ascii_digit()))
}

/// The pks of a `show_many`'s `user_ids`, when it lists between one and
/// [`MOST_STATUSES`] of them and nothing else.
fn statuses_of(ids: &str) -> Option<usize> {
    let count = ids.split(',').count();
    let pks = ids
        .split(',')
        .all(|pk| !pk.is_empty() && pk.len() <= 20 && pk.bytes().all(|b| b.is_ascii_digit()));
    (pks && (1..=MOST_STATUSES).contains(&count)).then_some(count)
}

/// Whether `route` is one snob may navigate the app to: the home page, a
/// profile ([`document`]), or a profile's mutual followers,
/// `/<name>/followers/mutualOnly`, the one tab of a profile with an address
/// of its own that snob reads from. Never one of the app's screens.
pub fn list_route(route: &str) -> bool {
    if document(route) {
        return true;
    }
    route
        .strip_suffix("followers/mutualOnly")
        .is_some_and(|profile| profile != "/" && document(profile))
}

/// The registry's operation `request` is, if it is one.
fn operation_posted(request: &PageRequest, path: &str, found: &Rediscovered) -> Option<Operation> {
    let form: Vec<(String, String)> =
        url::form_urlencoded::parse(request.body.as_deref()?.as_bytes())
            .into_owned()
            .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let name = field("fb_api_req_friendly_name")?;
    let operation = Operation::named(name)?;
    let announced = request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("x-fb-friendly-name"))
        .map(|(_, v)| v.as_str());
    let doc_id = field("doc_id")?;
    let holds = operation.path() == path
        && announced == Some(name)
        && (operation.write().is_some()
            || doc_id == operation.doc_id()
            || found.get(operation) == Some(doc_id));
    holds.then_some(operation)
}

/// The app's own screens, which a profile's name can never be, in any case.
/// Some mark what they show as seen the moment it is shown.
const SCREENS: [&str; 5] = ["stories", "explore", "direct", "reels", "accounts"];

/// Whether `name` can be an account's name: one to thirty ASCII letters,
/// digits, dots and underscores, which is every name Instagram hands out.
/// Anything else, a percent-encoded name among it, is no profile's.
pub fn profile_name(name: &str) -> bool {
    (1..=30).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
}

/// What `ask` is, when the page must not be asked it. The same question
/// [`refused`] asks of a request, asked of the intent before anything is
/// built from it, and again where it is built, whoever sent it.
///
/// Only the method and the path; a refused `Pk` does not print its name.
pub fn refused_ask(ask: &Ask) -> Option<String> {
    match ask {
        Ask::Viewer | Ask::Tray => None,
        Ask::Query { operation, .. } => operation
            .write()
            .is_some()
            .then(|| format!("POST {}", operation.path())),
        Ask::Write { .. } => None,
        // Where it may come from needs the site's own address, which the
        // intent does not carry: [`refused_asset`] asks it where the call
        // is answered.
        Ask::Asset { url } => Url::parse(url)
            .map_or(true, |url| !matches!(url.scheme(), "http" | "https"))
            .then(|| "a download from an address that is not a web one".to_string()),
        Ask::Pk { name } => (!profile_name(name)).then(|| format!("POST {BULK_ROUTE_DEFINITIONS}")),
        Ask::Navigation { route } => (!list_route(route)).then(|| format!("POST {NAVIGATION}")),
        Ask::Statuses { pks } => {
            (!(1..=MOST_STATUSES).contains(&pks.len())).then(|| format!("POST {SHOW_MANY}"))
        }
        Ask::Document { path } => (path.contains('?') || !document(path))
            .then(|| format!("a navigation to {}", path.escape_debug())),
        Ask::Rest { read, query } => {
            let keys = read.keys();
            let held = query.iter().all(|(name, value)| {
                keys.iter()
                    .any(|(key, only)| key == name && only.is_none_or(|only| only == value))
            });
            (!held).then(|| format!("GET {}", read.path()))
        }
    }
}

/// Why the tab must not fetch `url` as an asset of the site at `origin`: it
/// is neither on Instagram's CDN nor a test's own server, nor served over
/// HTTPS. The one copy of where a file may come from: the client asks it
/// before it asks the tab, and the tab asks it again before it fetches,
/// whoever sent the intent.
///
/// **HTTPS, and one of the two hosts Instagram serves media from** (the
/// leading dot is what stops `evilcdninstagram.com` matching), or exactly the
/// site's own scheme, host and port when the site is not Instagram, which is
/// how a test serves a file from its own server. Matched whole, since on host
/// alone it would let `http://127.0.0.1:9/x` through beside a test on another
/// port; and never for Instagram itself, where it would make every address on
/// instagram.com a "download": `snob fetch` takes its address from whoever
/// typed it, and the site's API is not a file to be read unpaced and outside
/// the allowlist.
pub fn refused_asset(origin: &str, url: &str) -> Option<String> {
    const CDN_HOSTS: [&str; 2] = ["cdninstagram.com", "fbcdn.net"];

    let refusal = |url: &Url| {
        Some(format!(
            "a download from {}",
            url.host_str().unwrap_or("nowhere")
        ))
    };
    let Ok(url) = Url::parse(url) else {
        return Some("a download from an address that does not parse".into());
    };
    let instagram = |host: &str| host == "instagram.com" || host.ends_with(".instagram.com");
    if Url::parse(origin).is_ok_and(|origin| {
        !origin.host_str().is_some_and(instagram)
            && origin.scheme() == url.scheme()
            && origin.host_str() == url.host_str()
            && origin.port_or_known_default() == url.port_or_known_default()
    }) {
        return None;
    }
    let cdn = url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            CDN_HOSTS
                .iter()
                .any(|cdn| host == *cdn || host.ends_with(&format!(".{cdn}")))
        });
    if cdn { None } else { refusal(&url) }
}

/// What `request` is, when the page must not send it.
///
/// Only the method and the path: the rest can carry tokens.
pub fn refused(request: &PageRequest) -> Option<String> {
    refused_with(request, &Rediscovered::default())
}

/// [`refused`], with the numbers the app was seen to send the registry's
/// reads under, which a read may go out under as well as the registry's.
pub fn refused_with(request: &PageRequest, found: &Rediscovered) -> Option<String> {
    let method = match request.method {
        Method::Get if request.navigate => "a navigation to",
        Method::Get => "GET",
        Method::Post => "POST",
    };
    let Ok(url) = Url::parse(&request.url) else {
        return Some(format!("{method} an address that does not parse"));
    };
    let path = url.path();
    let allowed = match (request.method, request.navigate) {
        (Method::Post, false) => posted(request, path, found),
        (Method::Get, false) => rest_read(path),
        (Method::Get, true) => url.query().is_none() && document(path),
        (Method::Post, true) => false,
    };
    (!allowed).then(|| format!("{method} {path}"))
}

/// A read the app sends on its own as a document loads, which snob never
/// sends and the tab lets the app send: by name, on its own endpoint, and
/// under a number that is either pinned or vouched for by the call itself.
struct AppRead {
    name: &'static str,
    /// `/api/graphql` or `/graphql/query`: the app never sends one read to
    /// both.
    path: &'static str,
    number: Number,
}

/// How an app read's `doc_id` is held.
enum Number {
    /// The one number it was captured under.
    Pinned(&'static str),
    /// Any number, when the call vouches for it the way [`vouched`] holds a
    /// registry read's moved number: the answer's root field named in
    /// `X-Root-Field-Name`, as the app names it on `/graphql/query`, and the
    /// key that names what is asked among its variables.
    Vouched {
        root_field: &'static str,
        asked_key: &'static str,
    },
}

/// What the tab lets the app's own boot calls do beyond the registry's, as the
/// recorded app sends them: the promotions it asks for on every document, the
/// unread counts of the message badge and jewel, and what a profile asks about
/// the account it shows, its posts and the accounts it suggests beside it among
/// them. Every one of them reads and writes nothing, and none carries a field
/// about what was looked at.
///
/// **A profile's posts and suggestions go out as the app sends them.** They
/// were left out once, as a weight of images nobody asked for; refused, the
/// profile snob loads was a profile whose app never asked for its grid or its
/// suggestions, which no person's browser is, and the owner decided that the
/// requests the app makes for a profile go out (2026-10-01). The suggestions
/// are a read; the write the app sends about them, a REST POST under
/// `/api/v1/web/discover/`, stays refused, as every write does.
///
/// **The posts' number is not pinned**: it moved from 28570182382647478 to
/// 28991540097136703 between the captures of 2026-09-29 and 2026-10-01, and a
/// pinned one would refuse the app's grid again at the next move. It is held
/// as the registry's reads are when the app moves them ([`vouched`]): by its
/// name, on `/graphql/query`, answering under its own root field and asking
/// by `username`. Not through [`Rediscovered`], which holds the numbers snob
/// sends the registry's reads under and learns them from the app's calls; for
/// a read snob never sends, the call being judged is the only one there is to
/// learn from, so the call vouches for itself here, with the same checks.
///
/// **Left out on purpose**: the message inbox and thread reads, the recipients
/// of the share sheet, the search boxes, and the sync with Facebook. The
/// beacons (`/ajax/bz`, `/ajax/qm/`) stay out too: their payloads were not
/// recorded, and the first batches the app's own events, impressions among
/// them.
const APP_READS: [AppRead; 7] = [
    AppRead {
        name: "QuickPromotionSupportIGSchemaBatchFetchQuery",
        path: "/api/graphql",
        number: Number::Pinned("26673487622279953"),
    },
    AppRead {
        name: "IGDBadgeCountOffMsysQuery",
        path: "/api/graphql",
        number: Number::Pinned("27393860900250970"),
    },
    AppRead {
        name: "IGDChatTabsJewelOffMsysQuery",
        path: "/api/graphql",
        number: Number::Pinned("27647971824866335"),
    },
    AppRead {
        name: "PolarisProfileDirectOrPartnershipInboxMessageEligibilityQuery",
        path: "/api/graphql",
        number: Number::Pinned("26821612144133136"),
    },
    AppRead {
        name: "PolarisProfileSuggestedUsersWithPreloadableQuery",
        path: "/api/graphql",
        number: Number::Pinned("27929823133325729"),
    },
    // The app's own spelling, with the word twice.
    AppRead {
        name: "PolarisProfileSuggestedUsersWithLazyQueryQuery",
        path: "/api/graphql",
        number: Number::Pinned("28011006998510477"),
    },
    AppRead {
        name: "PolarisProfilePostsQuery",
        path: "/graphql/query",
        number: Number::Vouched {
            root_field: "xdt_api__v1__feed__user_timeline_graphql_connection",
            asked_key: "username",
        },
    },
];

/// What `request`, one the app sent on its own, is when the tab must not let
/// it out: [`refused_with`]'s, unless it is one of [`APP_READS`]. Nothing
/// snob builds goes through here; its own calls are held to the registry.
pub fn refused_app(request: &PageRequest, found: &Rediscovered) -> Option<String> {
    let what = refused_with(request, found)?;
    (!app_read(request)).then_some(what)
}

/// Whether `request` is one of [`APP_READS`], announced in
/// `X-FB-Friendly-Name` as the app does, on its own endpoint, under its
/// number.
fn app_read(request: &PageRequest) -> bool {
    if request.method != Method::Post || request.navigate {
        return false;
    }
    let Ok(url) = Url::parse(&request.url) else {
        return false;
    };
    let Some(body) = request.body.as_deref() else {
        return false;
    };
    let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let (Some(name), Some(doc_id)) = (field("fb_api_req_friendly_name"), field("doc_id")) else {
        return false;
    };
    let Some(read) = APP_READS.iter().find(|read| read.name == name) else {
        return false;
    };
    let number = match read.number {
        Number::Pinned(pinned) => doc_id == pinned,
        Number::Vouched {
            root_field,
            asked_key,
        } => {
            let asks = field("variables")
                .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                .is_some_and(|v| v.get(asked_key).is_some());
            is_doc_id(doc_id) && header("x-root-field-name") == Some(root_field) && asks
        }
    };
    url.path() == read.path && header("x-fb-friendly-name") == Some(name) && number
}

/// Whether `path` is one of [`Rest`]'s: a friendship list of an account,
/// or a post or its comments, by a pk of digits. Nothing else under a post:
/// not its likers, and not the link the app makes for sharing it.
fn rest_read(path: &str) -> bool {
    let digits = |pk: &str| !pk.is_empty() && pk.bytes().all(|b| b.is_ascii_digit());
    let under = |prefix: &str, ends: &[&str]| {
        path.strip_prefix(prefix)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(pk, rest)| digits(pk) && ends.contains(&rest))
    };
    under(
        "/api/v1/friendships/",
        &["followers/", "following/", "mutual_followers/"],
    ) || under("/api/v1/media/", &["info/", "comments/"])
}

/// Whether `path` is the home page or a profile's: where the tab may go.
pub fn document(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    path.strip_prefix('/')
        .and_then(|rest| rest.strip_suffix('/'))
        .is_some_and(|name| {
            profile_name(name) && !SCREENS.iter().any(|s| s.eq_ignore_ascii_case(name))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://www.instagram.com";

    fn request(
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> PageRequest {
        PageRequest {
            method,
            url: format!("{BASE}{path}"),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            referrer: format!("{BASE}/someone/"),
            body: body.map(str::to_string),
            navigate: false,
            cap: 1 << 20,
            timeout_ms: 30_000,
        }
    }

    fn relay(path: &str, name: &str, doc_id: &str) -> PageRequest {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("fb_dtsg", "relay:token"),
                ("fb_api_req_friendly_name", name),
                ("variables", r#"{"id":"2345678901"}"#),
                ("doc_id", doc_id),
            ])
            .finish();
        request(
            Method::Post,
            path,
            &[("X-FB-Friendly-Name", name)],
            Some(&body),
        )
    }

    /// Written out rather than derived, so changing one is a decision made
    /// here.
    #[test]
    fn the_registry_is_the_calls_the_web_client_reads_with_and_the_two_writes() {
        let listed: Vec<_> = Operation::ALL
            .map(|op| (op.friendly_name(), op.doc_id(), op.path(), op.root_field()))
            .into();
        assert_eq!(
            listed,
            [
                (
                    "PolarisProfilePageContentQuery",
                    "28036671149327607",
                    "/api/graphql",
                    "user"
                ),
                (
                    "PolarisUserHoverCardContentV2Query",
                    "28949219061332276",
                    "/api/graphql",
                    "xig_user_by_igid_v2"
                ),
                (
                    "PolarisProfileStoryHighlightsTrayContentQuery",
                    "26970053832668570",
                    "/api/graphql",
                    "highlights"
                ),
                (
                    "PolarisStoriesV3HighlightsPageQuery",
                    "28325328583775973",
                    "/graphql/query",
                    "xdt_api__v1__feed__reels_media__connection"
                ),
                (
                    "PolarisStoriesV3ReelPageGalleryQuery",
                    "28262315486766731",
                    "/graphql/query",
                    "xdt_api__v1__feed__reels_media__connection"
                ),
                (
                    "PolarisStoriesV3ReelPageGalleryPaginationQuery",
                    "28606543452290985",
                    "/graphql/query",
                    "xdt_api__v1__feed__reels_media__connection"
                ),
                (
                    "PolarisStoriesV3TrayContainerQuery",
                    "27703822975903310",
                    "/api/graphql",
                    "xdt_api__v1__feed__reels_tray"
                ),
                (
                    "PolarisProfileNoteBubbleQuery",
                    "38260824646898178",
                    "/api/graphql",
                    "xdt_get_inbox_tray_items"
                ),
                (
                    "PolarisSchoolPartnerProfileBadgeQuery",
                    "28647748031575520",
                    "/api/graphql",
                    "xig_user_by_igid_v2"
                ),
                (
                    "PolarisProfilePostsQuery",
                    "28991540097136703",
                    "/graphql/query",
                    "xdt_api__v1__feed__user_timeline_graphql_connection"
                ),
                (
                    "PolarisProfilePostsTabContentQuery_connection",
                    "29240983615539641",
                    "/graphql/query",
                    "xdt_api__v1__feed__user_timeline_graphql_connection"
                ),
                (
                    "usePolarisFollowMutation",
                    "26508036048874888",
                    "/api/graphql",
                    "xdt_create_friendship"
                ),
                (
                    "usePolarisUnfollowMutation",
                    "27789106940691111",
                    "/api/graphql",
                    "xdt_destroy_friendship"
                ),
            ]
        );
        // Every read is a query, the grid's next page under the app's name
        // for a query's connection, and the only writes are the two there are.
        for operation in Operation::ALL {
            let name = operation.friendly_name();
            assert_eq!(
                operation.write().is_some(),
                !(name.ends_with("Query") || name.ends_with("Query_connection")),
                "{name}"
            );
        }
        assert_eq!(
            Operation::ALL
                .iter()
                .filter_map(|op| op.write())
                .collect::<Vec<_>>(),
            Mutation::ALL
        );

        let pk = Pk::new(2_345_678_901);
        let rest = [
            (
                Rest::Followers(pk),
                "/api/v1/friendships/2345678901/followers/",
            ),
            (
                Rest::Following(pk),
                "/api/v1/friendships/2345678901/following/",
            ),
            (
                Rest::MutualFollowers(pk),
                "/api/v1/friendships/2345678901/mutual_followers/",
            ),
            (
                Rest::MediaInfo(MediaPk::new(3_807_824_420_233_075_826)),
                "/api/v1/media/3807824420233075826/info/",
            ),
            (
                Rest::Comments(MediaPk::new(3_807_824_420_233_075_826)),
                "/api/v1/media/3807824420233075826/comments/",
            ),
        ];
        for (read, path) in rest {
            assert_eq!(read.path(), path);
        }
    }

    /// Each operation is found by the name the app gives it, and a name the
    /// registry does not hold finds nothing.
    #[test]
    fn an_operation_is_found_by_its_friendly_name() {
        for operation in Operation::ALL {
            assert_eq!(Operation::named(operation.friendly_name()), Some(operation));
        }
        assert_eq!(Operation::named("SomeBadgeQuery"), None);
        assert_eq!(Operation::named(""), None);
    }

    #[test]
    fn every_operation_of_the_registry_goes_out_on_its_own_endpoint() {
        for operation in Operation::ALL {
            let sent = relay(
                operation.path(),
                operation.friendly_name(),
                operation.doc_id(),
            );
            assert_eq!(refused(&sent), None, "{operation:?}");
            let other = match operation.family() {
                Family::RelayQuery => "/api/graphql",
                _ => "/graphql/query",
            };
            let elsewhere = relay(other, operation.friendly_name(), operation.doc_id());
            assert!(refused(&elsewhere).is_some(), "{operation:?} on {other}");
        }
    }

    /// Anything the app sends that is not in the registry, whatever it is
    /// called: made-up names, as the real ones are not written here.
    #[test]
    fn a_relay_call_not_in_the_registry_is_refused() {
        for (name, doc_id) in [
            ("PolarisSomethingElseQuery", "1000000000000001"),
            ("usePolarisSomethingMutation", "1000000000000002"),
        ] {
            let sent = relay("/api/graphql", name, doc_id);
            assert_eq!(refused(&sent).as_deref(), Some("POST /api/graphql"));
        }
    }

    /// A known name with another operation's number is another operation.
    #[test]
    fn a_query_goes_out_only_with_its_own_doc_id() {
        let profile = Operation::ProfilePage;
        let sent = relay("/api/graphql", profile.friendly_name(), "1000000000000001");
        assert!(refused(&sent).is_some());
        let sent = relay(
            "/api/graphql",
            profile.friendly_name(),
            Operation::HoverCard.doc_id(),
        );
        assert!(refused(&sent).is_some());
    }

    /// A write's number rotates and is rediscovered beside its name, so the
    /// name and the endpoint are what hold it.
    #[test]
    fn a_write_goes_out_with_a_rediscovered_doc_id() {
        let follow = Operation::Follow;
        let sent = relay("/api/graphql", follow.friendly_name(), "1000000000000003");
        assert_eq!(refused(&sent), None);
    }

    /// The header and the form name the same operation, or nothing goes.
    #[test]
    fn a_relay_call_whose_header_names_another_operation_is_refused() {
        let hover = Operation::HoverCard;
        let mut sent = relay("/api/graphql", hover.friendly_name(), hover.doc_id());
        sent.headers = vec![(
            "X-FB-Friendly-Name".into(),
            "PolarisSomethingElseQuery".into(),
        )];
        assert!(refused(&sent).is_some());
        sent.headers.clear();
        assert!(refused(&sent).is_some());
        sent.body = None;
        assert!(refused(&sent).is_some());
    }

    #[test]
    fn no_other_post_goes_out() {
        let routes = request(
            Method::Post,
            BULK_ROUTE_DEFINITIONS,
            &[],
            Some("route_urls[0]=%2Fa%2F"),
        );
        assert_eq!(refused(&routes), None);
        for path in [
            "/api/v1/friendships/show_many/",
            "/api/v1/web/something/",
            "/ajax/navigation/",
            "/ajax/bz",
        ] {
            let sent = request(Method::Post, path, &[], Some("a=b"));
            assert_eq!(refused(&sent), Some(format!("POST {path}")));
        }
    }

    /// The page fetches the friendship lists and nothing else by GET: every
    /// other read is a query, so the REST reads the client sends without a
    /// browser are refused from it, a highlight's items among them.
    #[test]
    fn a_get_goes_out_only_for_a_friendship_list() {
        for path in [
            "/api/v1/friendships/2345678901/followers/?count=12&search_surface=follow_list_page",
            "/api/v1/friendships/2345678901/following/?count=12&max_id=12",
            "/api/v1/friendships/2345678901/mutual_followers/?page_size=12",
            "/api/v1/media/3807824420233075826/info/",
            "/api/v1/media/3807824420233075826/comments/?can_support_threading=true&permalink_enabled=false",
        ] {
            assert_eq!(
                refused(&request(Method::Get, path, &[], None)),
                None,
                "{path}"
            );
        }
        for path in [
            "/api/v1/users/web_profile_info/?username=someone",
            "/api/v1/users/2345678901/info/",
            "/api/v1/feed/reels_media/?reel_ids=2345678901",
            "/api/v1/feed/reels_media/?reel_ids=highlight%3A17900000000000001",
            "/api/v1/highlights/2345678901/highlights_tray/",
            "/web/search/topsearch/?query=someone",
            "/api/v1/friendships/pending/",
            "/api/v1/friendships/show_many/",
            "/api/v1/friendships/someone/following/",
            "/api/v1/friendships/2345678901/following/extra/",
            "/api/v1/friendships//followers/",
            "/api/v1/media/3807824420233075826/permalink/",
            "/api/v1/media/3807824420233075826/likers/",
            "/api/v1/media/3807824420233075826/comments/18000000000000000/child_comments/",
            "/api/v1/media/DTYHCKvDNxy/info/",
            "/api/v1/media//info/",
            "/api/graphql",
            "/graphql/query",
            "/ajax/bulk-route-definitions/",
        ] {
            let path_alone = path.split('?').next().unwrap();
            assert_eq!(
                refused(&request(Method::Get, path, &[], None)),
                Some(format!("GET {path_alone}")),
                "{path}"
            );
        }
    }

    #[test]
    fn the_page_goes_only_to_the_home_page_or_a_profile() {
        let navigation = |path: &str| {
            let mut sent = request(Method::Get, path, &[], None);
            sent.navigate = true;
            sent
        };
        for path in ["/", "/someone/", "/some.one_else/"] {
            assert_eq!(refused(&navigation(path)), None, "{path}");
        }
        for path in ["/Stories/", "/REELS/", "/Explore/", "/some%2Eone/"] {
            assert!(refused(&navigation(path)).is_some(), "{path}");
        }
        for path in [
            "/stories/someone/",
            "/stories/",
            "/explore/",
            "/direct/inbox/",
            "/reels/",
            "/someone/followers/",
            "/someone",
            "/someone/?next=x",
        ] {
            assert!(refused(&navigation(path)).is_some(), "{path}");
        }
        assert_eq!(
            refused(&navigation("/stories/")).as_deref(),
            Some("a navigation to /stories/")
        );
        let mut posted = navigation("/someone/");
        posted.method = Method::Post;
        assert!(refused(&posted).is_some());
        let mut nowhere = request(Method::Get, "/", &[], None);
        nowhere.url = "not an address".into();
        assert!(refused(&nowhere).is_some());
    }

    /// A screen is a screen whatever its case, and a name is only ever one
    /// Instagram could have handed out.
    #[test]
    fn a_profile_is_only_a_name_instagram_could_hand_out() {
        for name in ["someone", "a.b_c", "A1", &"a".repeat(30)] {
            assert!(profile_name(name), "{name}");
            assert!(document(&format!("/{name}/")), "{name}");
        }
        for name in [
            "",
            &"a".repeat(31),
            "some-one",
            "s\u{f6}meone",
            "some%2Fone",
            "some one",
            "..%2F",
        ] {
            assert!(!profile_name(name), "{name}");
        }
        for path in [
            "/Stories/",
            "/REELS/",
            "/Explore/",
            "/Direct/",
            "/aCCounts/",
            "/some%2Eone/",
            "/%73tories/",
            "/some-one/",
        ] {
            assert!(!document(path), "{path}");
        }
    }

    fn rest(read: Rest, query: &[(&str, &str)]) -> Ask {
        Ask::Rest {
            read,
            query: query
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// The intent is held to the registry before anything is built from
    /// it.
    #[test]
    fn an_ask_is_held_to_the_registry() {
        let pk = Pk::new(2_345_678_901);
        let allowed = [
            Ask::Viewer,
            Ask::Tray,
            Ask::Pk {
                name: "someone".into(),
            },
            Ask::Document { path: "/".into() },
            Ask::Document {
                path: "/some.one_else/".into(),
            },
            Ask::Query {
                operation: Operation::ProfilePage,
                variables: "{}".into(),
            },
            Ask::Write {
                mutation: Mutation::Follow,
                variables: "{}".into(),
                doc_id: "1000000000000003".into(),
                route: "comet.igweb.PolarisProfilePostsTabRoute".into(),
            },
            rest(
                Rest::Followers(pk),
                &[
                    ("count", "12"),
                    ("max_id", "24"),
                    ("search_surface", "follow_list_page"),
                ],
            ),
            rest(Rest::Following(pk), &[("count", "12"), ("max_id", "24")]),
            rest(
                Rest::MutualFollowers(pk),
                &[("page_size", "12"), ("max_id", "x")],
            ),
            rest(Rest::Following(pk), &[]),
            rest(
                Rest::MediaInfo(MediaPk::new(3_807_824_420_233_075_826)),
                &[],
            ),
            rest(
                Rest::Comments(MediaPk::new(3_807_824_420_233_075_826)),
                &[
                    ("can_support_threading", "true"),
                    ("permalink_enabled", "false"),
                ],
            ),
            rest(
                Rest::Comments(MediaPk::new(3_807_824_420_233_075_826)),
                &[
                    ("can_support_threading", "true"),
                    ("min_id", r#"{"cached_comments_cursor":"1"}"#),
                    ("sort_order", "popular"),
                ],
            ),
            Ask::Navigation { route: "/".into() },
            Ask::Navigation {
                route: "/someone/followers/mutualOnly".into(),
            },
            Ask::Statuses { pks: vec![pk] },
            Ask::Statuses {
                pks: vec![pk; MOST_STATUSES],
            },
        ];
        for ask in allowed {
            assert_eq!(refused_ask(&ask), None, "{ask:?}");
        }

        let refusals = [
            (
                Ask::Query {
                    operation: Operation::Follow,
                    variables: "{}".into(),
                },
                "POST /api/graphql",
            ),
            (
                Ask::Query {
                    operation: Operation::Unfollow,
                    variables: "{}".into(),
                },
                "POST /api/graphql",
            ),
            (
                Ask::Pk {
                    name: "stories/someone".into(),
                },
                "POST /ajax/bulk-route-definitions/",
            ),
            (
                Ask::Pk { name: "".into() },
                "POST /ajax/bulk-route-definitions/",
            ),
            (
                Ask::Document {
                    path: "/stories/".into(),
                },
                "a navigation to /stories/",
            ),
            (
                Ask::Document {
                    path: "/Stories/".into(),
                },
                "a navigation to /Stories/",
            ),
            (
                Ask::Document {
                    path: "/someone/?x=1".into(),
                },
                "a navigation to /someone/?x=1",
            ),
            (
                Ask::Document {
                    path: "/someone/followers/".into(),
                },
                "a navigation to /someone/followers/",
            ),
            (
                rest(
                    Rest::Following(pk),
                    &[("search_surface", "follow_list_page")],
                ),
                "GET /api/v1/friendships/2345678901/following/",
            ),
            (
                rest(Rest::Followers(pk), &[("search_surface", "elsewhere")]),
                "GET /api/v1/friendships/2345678901/followers/",
            ),
            (
                rest(Rest::Followers(pk), &[("query", "a")]),
                "GET /api/v1/friendships/2345678901/followers/",
            ),
            (
                rest(Rest::MutualFollowers(pk), &[("count", "12")]),
                "GET /api/v1/friendships/2345678901/mutual_followers/",
            ),
            (
                rest(
                    Rest::MediaInfo(MediaPk::new(3_807_824_420_233_075_826)),
                    &[("count", "12")],
                ),
                "GET /api/v1/media/3807824420233075826/info/",
            ),
            (
                rest(
                    Rest::Comments(MediaPk::new(3_807_824_420_233_075_826)),
                    &[("permalink_enabled", "true")],
                ),
                "GET /api/v1/media/3807824420233075826/comments/",
            ),
            (
                rest(
                    Rest::Comments(MediaPk::new(3_807_824_420_233_075_826)),
                    &[("sort_order", "newest")],
                ),
                "GET /api/v1/media/3807824420233075826/comments/",
            ),
            (
                Ask::Navigation {
                    route: "/stories/someone/".into(),
                },
                "POST /ajax/navigation/",
            ),
            (
                Ask::Statuses { pks: Vec::new() },
                "POST /api/v1/friendships/show_many/",
            ),
            (
                Ask::Statuses {
                    pks: vec![pk; MOST_STATUSES + 1],
                },
                "POST /api/v1/friendships/show_many/",
            ),
        ];
        for (ask, said) in refusals {
            assert_eq!(refused_ask(&ask).as_deref(), Some(said), "{ask:?}");
        }
    }

    /// What makes the app's call vouch for the number it sent a read under:
    /// the operation's own endpoint and variables, and its root field where
    /// the app names one. A write never does.
    #[test]
    fn a_call_of_the_app_vouches_for_a_read_only_by_what_it_asks() {
        let profile = Operation::ProfilePage;
        let moved = "1000000000000777";
        let vouch = |operation: Operation, path, doc_id, variables, root| {
            vouched(operation, path, doc_id, variables, root)
        };
        assert!(vouch(profile, "/api/graphql", moved, r#"{"id":"1"}"#, None));
        // Wrong endpoint, wrong variables, a number that is none, a root
        // field sent where the app sends none.
        assert!(!vouch(
            profile,
            "/graphql/query",
            moved,
            r#"{"id":"1"}"#,
            None
        ));
        assert!(!vouch(
            profile,
            "/api/graphql",
            moved,
            r#"{"userID":"1"}"#,
            None
        ));
        assert!(!vouch(profile, "/api/graphql", moved, "[1]", None));
        assert!(!vouch(profile, "/api/graphql", moved, "not json", None));
        for doc_id in ["", "123", "10000000000007x7", "10000000000007770000000"] {
            assert!(
                !vouch(profile, "/api/graphql", doc_id, r#"{"id":"1"}"#, None),
                "{doc_id}"
            );
        }
        assert!(!vouch(
            profile,
            "/api/graphql",
            moved,
            r#"{"id":"1"}"#,
            Some("user")
        ));
        // On /graphql/query the root field is the operation's.
        let gallery = Operation::ReelGallery;
        let variables = r#"{"initial_reel_id":"1"}"#;
        let root = Some(gallery.root_field());
        assert!(vouch(gallery, "/graphql/query", moved, variables, root));
        assert!(!vouch(gallery, "/graphql/query", moved, variables, None));
        assert!(!vouch(
            gallery,
            "/graphql/query",
            moved,
            variables,
            Some("user")
        ));
        // A write's number is found in the app's code, not here.
        for write in [Operation::Follow, Operation::Unfollow] {
            assert!(!vouch(
                write,
                "/api/graphql",
                moved,
                r#"{"target_user_id":"1"}"#,
                None
            ));
        }
    }

    /// A number held for a read lets that read out under it, and only that
    /// read, only on its own endpoint and under its own name; nothing else
    /// about the check moves.
    #[test]
    fn a_number_held_for_a_read_lets_that_read_out_and_nothing_else() {
        let moved = "1000000000000777";
        let profile = Operation::ProfilePage;
        let sent = relay("/api/graphql", profile.friendly_name(), moved);
        assert!(refused(&sent).is_some());
        let found = Rediscovered::of(profile, moved);
        assert_eq!(refused_with(&sent, &found), None);
        // Held for another operation, or under another number: refused.
        let hover = Operation::HoverCard;
        assert!(refused_with(&sent, &Rediscovered::of(hover, moved)).is_some());
        assert!(refused_with(&sent, &Rediscovered::of(profile, "1000000000000778")).is_some());
        // The header still has to name the same operation, and the endpoint
        // is still the operation's.
        let mut other = relay("/api/graphql", profile.friendly_name(), moved);
        other.headers.clear();
        assert!(refused_with(&other, &found).is_some());
        let elsewhere = relay("/graphql/query", profile.friendly_name(), moved);
        assert!(refused_with(&elsewhere, &found).is_some());
        // A write takes none: the number is not a read's to hold.
        let follow = Operation::Follow;
        assert_eq!(Rediscovered::of(follow, moved), Rediscovered::default());
        // Nor does a number that is not one.
        assert_eq!(Rediscovered::of(profile, "abc"), Rediscovered::default());
        assert_eq!(found.get(profile), Some(moved));
        assert_eq!(found.get(hover), None);
    }

    /// An app read as the app sends it: its name, on its own endpoint, under
    /// `doc_id`, with `variables`, naming `root` where the endpoint takes one.
    fn app_sent(read: &AppRead, doc_id: &str, variables: &str, root: Option<&str>) -> PageRequest {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("fb_api_req_friendly_name", read.name),
                ("variables", variables),
                ("doc_id", doc_id),
            ])
            .finish();
        let mut headers = vec![("X-FB-Friendly-Name", read.name)];
        if let Some(root) = root {
            headers.push(("X-Root-Field-Name", root));
        }
        request(Method::Post, read.path, &headers, Some(&body))
    }

    /// The app's own reads that write nothing go out by name and number, on
    /// their own endpoint; the rest of what the app sends does not.
    #[test]
    fn the_apps_boot_reads_go_out_by_name_and_number_and_nothing_else_does() {
        let nobody = Rediscovered::default();
        for read in &APP_READS {
            let Number::Pinned(doc_id) = read.number else {
                continue;
            };
            let sent = app_sent(read, doc_id, r#"{"id":"1"}"#, None);
            assert!(refused(&sent).is_some(), "{} is not snob's", read.name);
            assert_eq!(refused_app(&sent, &nobody), None, "{}", read.name);
            // Under another number, on another endpoint, or announced as
            // another name, it is anybody's guess.
            let moved = app_sent(read, "1000000000000999", "{}", None);
            assert!(refused_app(&moved, &nobody).is_some(), "{}", read.name);
            let mut elsewhere = sent.clone();
            elsewhere.url = format!("{BASE}/graphql/query");
            assert!(refused_app(&elsewhere, &nobody).is_some(), "{}", read.name);
            let mut renamed = sent.clone();
            renamed.headers = vec![("X-FB-Friendly-Name".into(), "SomethingElse".into())];
            assert!(refused_app(&renamed, &nobody).is_some(), "{}", read.name);
        }
        // Reads of the app's that are left out, a write, and the beacons.
        for name in [
            "IGDThreadDetailQuery",
            "IGDInboxTrayQuery",
            "PolarisSearchBoxRefetchableQuery",
            "PolarisAPIGetFrCookieQuery",
            "usePolarisSomethingMutation",
        ] {
            let sent = relay("/api/graphql", name, "1000000000000001");
            assert!(refused_app(&sent, &nobody).is_some(), "{name}");
        }
        for path in ["/ajax/bz", "/ajax/qm/", "/ajax/navigation/"] {
            let sent = request(Method::Post, path, &[], Some("a=b"));
            assert_eq!(
                refused_app(&sent, &nobody),
                Some(format!("POST {path}")),
                "{path}"
            );
        }
        // A GET and a navigation are the page's, as before.
        let get = request(Method::Get, "/api/v1/web/something/", &[], None);
        assert!(refused_app(&get, &nobody).is_some());
        // The registry's own go on as ever, and a rediscovered number with
        // them.
        let profile = Operation::ProfilePage;
        let sent = relay("/api/graphql", profile.friendly_name(), profile.doc_id());
        assert_eq!(refused_app(&sent, &nobody), None);
        let moved = relay("/api/graphql", profile.friendly_name(), "1000000000000777");
        assert!(refused_app(&moved, &nobody).is_some());
        assert_eq!(
            refused_app(&moved, &Rediscovered::of(profile, "1000000000000777")),
            None
        );
    }

    /// The profile's posts go out under whatever number the app sends them,
    /// both of the two captured among them, when the call vouches for it: on
    /// `/graphql/query`, under its root field, asking by `username`. Not on
    /// the other endpoint, not under another root field, not without the
    /// name it asks by, and not under something that is not a number.
    #[test]
    fn the_profile_posts_go_out_under_any_number_the_call_vouches_for() {
        let nobody = Rediscovered::default();
        let posts = APP_READS
            .iter()
            .find(|read| read.name == "PolarisProfilePostsQuery")
            .unwrap();
        let root = "xdt_api__v1__feed__user_timeline_graphql_connection";
        let asks = r#"{"data":{"count":12},"username":"someone"}"#;
        for doc_id in ["28570182382647478", "28991540097136703", "1000000000000777"] {
            let sent = app_sent(posts, doc_id, asks, Some(root));
            // Snob's own goes out under the registry's number alone, or one
            // the app was seen to send; the app's under any it vouches for.
            assert_eq!(
                refused(&sent).is_none(),
                doc_id == Operation::ProfilePosts.doc_id(),
                "{doc_id}"
            );
            assert_eq!(refused_app(&sent, &nobody), None, "{doc_id}");
        }
        // A number not the registry's: the app's call stands on its own.
        let doc_id = "28570182382647478";
        let mut elsewhere = app_sent(posts, doc_id, asks, Some(root));
        elsewhere.url = format!("{BASE}/api/graphql");
        let refusals = [
            elsewhere,
            app_sent(posts, doc_id, asks, None),
            app_sent(posts, doc_id, asks, Some("user")),
            app_sent(posts, doc_id, r#"{"data":{"count":12}}"#, Some(root)),
            app_sent(posts, doc_id, "not json", Some(root)),
            app_sent(posts, "12ab", asks, Some(root)),
        ];
        for (i, sent) in refusals.iter().enumerate() {
            assert!(refused_app(sent, &nobody).is_some(), "case {i}");
        }
    }

    /// The reads a profile's own document sends that were refused until
    /// 2026-10-01 go out now: its posts, and the accounts it suggests.
    #[test]
    fn a_profiles_posts_and_suggestions_are_the_apps_reads() {
        let names: Vec<&str> = APP_READS.iter().map(|read| read.name).collect();
        for name in [
            "PolarisProfilePostsQuery",
            "PolarisProfileSuggestedUsersWithPreloadableQuery",
            "PolarisProfileSuggestedUsersWithLazyQueryQuery",
        ] {
            assert!(names.contains(&name), "{name}");
        }
    }

    /// Every one of the app's boot reads is a query by its name, and none is
    /// the registry's, so what snob sends is never widened by it, but one:
    /// the posts' first page, which the app sends on every profile document
    /// under any number the call vouches for, and which snob sends itself
    /// under the registry's number or one the app was seen to send.
    #[test]
    fn the_apps_boot_reads_are_queries_that_are_not_the_registrys() {
        for read in &APP_READS {
            assert!(read.name.ends_with("Query"), "{}", read.name);
            let snobs = Operation::named(read.name);
            assert!(
                snobs.is_none() || snobs == Some(Operation::ProfilePosts),
                "{}",
                read.name
            );
            assert!(
                matches!(read.path, "/api/graphql" | "/graphql/query"),
                "{}",
                read.name
            );
            if let Number::Pinned(doc_id) = read.number {
                assert!(is_doc_id(doc_id), "{}", read.name);
            }
        }
    }

    /// `show_many` goes out as the app sends it after a list page and in no
    /// other shape: the pks, then `jazoest` and `fb_dtsg`, nothing more.
    #[test]
    fn show_many_goes_out_only_as_the_app_sends_it() {
        let posted = |body: &str| request(Method::Post, SHOW_MANY, &[], Some(body));
        for body in [
            "user_ids=1&jazoest=22858&fb_dtsg=t",
            "user_ids=2345678901%2C3456789012%2C7&jazoest=22858&fb_dtsg=t",
        ] {
            assert_eq!(refused(&posted(body)), None, "{body}");
        }
        let fifty_one = (0..=MOST_STATUSES)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("%2C");
        let too_many = format!("user_ids={fifty_one}&jazoest=22858&fb_dtsg=t");
        for body in [
            "",
            "user_ids=&jazoest=22858&fb_dtsg=t",
            "user_ids=1%2C&jazoest=22858&fb_dtsg=t",
            "user_ids=someone&jazoest=22858&fb_dtsg=t",
            "jazoest=22858&fb_dtsg=t&user_ids=1",
            "user_ids=1&jazoest=x&fb_dtsg=t",
            "user_ids=1&jazoest=22858&fb_dtsg=t&target_user_id=1",
            "user_ids=1&jazoest=22858",
            too_many.as_str(),
        ] {
            assert_eq!(
                refused(&posted(body)).as_deref(),
                Some("POST /api/v1/friendships/show_many/"),
                "{body}"
            );
        }
        // A GET of it is not one.
        let get = request(Method::Get, SHOW_MANY, &[], None);
        assert!(refused(&get).is_some());
    }

    /// The app's navigation goes out to the home page, a profile, and a
    /// profile's mutual followers, and to none of the app's screens.
    #[test]
    fn a_navigation_goes_out_only_to_a_route_snob_reads_from() {
        let posted = |route: &str| {
            let body = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs([
                    ("client_previous_actor_id", "17841400000000001"),
                    ("route_url", route),
                    ("routing_namespace", "igx_www"),
                ])
                .finish();
            request(Method::Post, NAVIGATION, &[], Some(&body))
        };
        for route in ["/", "/someone/", "/some.one_else/followers/mutualOnly"] {
            assert_eq!(refused(&posted(route)), None, "{route}");
            assert!(list_route(route), "{route}");
        }
        for route in [
            "/stories/someone/",
            "/stories/highlights/17938499744579885/?r=1",
            "/explore/",
            "/direct/inbox/",
            "/reels/",
            "/someone/reposts/",
            "/someone/followers/",
            "/someone/followers/mutualFirst",
            "/followers/mutualOnly",
            "/stories/followers/mutualOnly",
            "/someone/?x=1",
        ] {
            assert_eq!(
                refused(&posted(route)).as_deref(),
                Some("POST /ajax/navigation/"),
                "{route}"
            );
            assert!(!list_route(route), "{route}");
        }
        // Two routes in one form, or none, are not one navigation.
        let twice = request(
            Method::Post,
            NAVIGATION,
            &[],
            Some("route_url=%2Fa%2F&route_url=%2Fstories%2F"),
        );
        assert!(refused(&twice).is_some());
        let none = request(Method::Post, NAVIGATION, &[], Some("routing_namespace=x"));
        assert!(refused(&none).is_some());
    }

    /// Only the method and the path: the form carries tokens.
    #[test]
    fn a_refusal_names_no_token() {
        let sent = relay("/api/graphql", "PolarisSomethingElseQuery", "1");
        let said = refused(&sent).unwrap();
        assert!(!said.contains("relay:token"), "{said}");
    }
}
