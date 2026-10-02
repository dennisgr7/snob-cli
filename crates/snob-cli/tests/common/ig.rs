//! One fake Instagram, of invented people, served in the shapes the web app
//! is served in.
//!
//! **A [`World`] is the data, and everything served is read from it**: the
//! documents the tab loads, the app's own calls on them, the Relay queries,
//! the route definitions, the friendship lists, the REST reads the client
//! sends without a browser, and the pictures. A test changes the world and
//! every face of it agrees: the web app's ([`World::serve_web`]) and the one
//! the REST path reads ([`World::serve_rest`]).
//!
//! **The Relay endpoints are an oracle, written apart from `snob_ig::web`**,
//! so a form built wrong is refused here rather than agreed with: the fields
//! in the app's order, `av` the viewer's fbid, the tokens of the document the
//! form names, the module bitmaps the app sent, a `__req` that rises within
//! each document, the registry's `doc_id`, and the headers of each family.
//! Any violation is a 400 that says which.
//!
//! The documents' boot scripts stand in for the app: their calls carry
//! [`APP_HEADER`], so a test tells snob's requests from the app's by it, and
//! [`World::audit`] holds only snob's to the allowlist.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value, json};
use snob_ig::allowlist::{self, Family, Operation};
use snob_ig::client::page::{Method, PageRequest};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The header the app's calls carry here, and snob's never do.
pub const APP_HEADER: &str = "X-Test-App";

/// The CSRF token every document load sets.
pub const CSRF: &str = "set-by-the-page-load";

/// The claim the first page of a list hands out.
pub const CLAIM: &str = "hmac.from-the-server";

/// The device cookie a document load sets, unless the world is told
/// otherwise.
pub const DEVICE: &str = "datr=device-of-the-first-account";

/// The accounts every world starts with.
pub const ME: u64 = 42;
pub const SOMEONE: u64 = 9001;
pub const GHOST: u64 = 99;
/// `user0`'s pk; `userN` is `FIRST_USER + N`.
pub const FIRST_USER: u64 = 1000;
const USERS: u64 = 30;

/// The Facebook-side id of account `pk`, which Relay sends as `av`.
pub fn fbid(pk: u64) -> String {
    (17_841_400_000_000_000 + pk).to_string()
}

/// The per-load tokens a document served to `pk` carries.
pub fn lsd(pk: u64) -> String {
    format!("lsd{pk}")
}

pub fn relay_token(pk: u64) -> String {
    format!("relay:{pk}")
}

pub fn session_token(pk: u64) -> String {
    format!("session:{pk}")
}

/// The module bitmaps the app's calls carry, which snob copies.
pub const BITMAPS: [(&str, &str); 5] = [
    ("__dyn", "7xeUmwlE"),
    ("__csr", "gP4ll"),
    ("__hsdp", ""),
    ("__hblp", ""),
    ("__sjsp", "g4o"),
];

const REVISION: &str = "1000000001";
const HASTE: &str = "20000.HYP:instagram_web_pkg.2.1...0";
const HSI: &str = "7000000000000000001";
const SPIN_B: &str = "trunk";
const DPR: &str = "1";
const CCG: &str = "EXCELLENT";

/// The Bloks version every document names.
pub fn bloks() -> String {
    "0123456789abcdef".repeat(4)
}

/// An operation the app sends that the registry does not hold.
pub const NOT_IN_THE_REGISTRY: &str = "PolarisInventedBadgeQuery";

/// The variables the app sends with its own profile query: the id, and a
/// flag of its own, invented, that snob can only have copied.
pub fn app_profile_variables(pk: u64) -> String {
    format!(r#"{{"id":"{pk}","a_flag_the_app_sends":true}}"#)
}

/// One story, or one item of a highlight.
#[derive(Debug, Clone)]
pub struct Story {
    pub pk: String,
    pub taken_at: i64,
    pub video: bool,
}

/// One highlight under a bio, with its items.
#[derive(Debug, Clone)]
pub struct Highlight {
    /// The bare id; the tray spells it `highlight:<id>`.
    pub id: String,
    pub title: String,
    pub items: Vec<Story>,
}

/// One account.
#[derive(Debug, Clone)]
pub struct Person {
    pub pk: u64,
    pub username: String,
    pub full_name: String,
    pub private: bool,
    /// Whom it follows, in the order it followed them. Its followers are
    /// the people whose `following` names it.
    pub following: Vec<u64>,
    /// Whom it has asked to follow, and who has not answered.
    pub requested: Vec<u64>,
    pub highlights: Vec<Highlight>,
    pub stories: Vec<Story>,
    /// It keeps the default avatar.
    pub anonymous_picture: bool,
    /// Its profile query names its full-size picture.
    pub full_size_picture: bool,
    pub posts: u64,
}

impl Person {
    fn named(pk: u64, username: &str) -> Self {
        Self {
            pk,
            username: username.into(),
            full_name: format!("Invented {username}"),
            private: false,
            following: Vec::new(),
            requested: Vec::new(),
            highlights: Vec::new(),
            stories: Vec::new(),
            anonymous_picture: false,
            full_size_picture: true,
            posts: 3,
        }
    }
}

/// One document served: whom to, and the per-load values it carried.
#[derive(Debug, Clone)]
struct Load {
    viewer: u64,
    session: String,
    spin_t: u64,
}

/// A document as the World renders it.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub status: u16,
    pub html: String,
    /// The app's calls as the document boots: where each is posted, and its
    /// form. The boot script sends them; a tab with no browser takes them in
    /// as its listener would.
    pub forms: Vec<(String, String)>,
}

#[derive(Debug)]
struct State {
    people: BTreeMap<u64, Person>,
    loads: Vec<Load>,
    /// The highest `__req` seen within each document, by its web session id.
    reqs: HashMap<String, u32>,
    repeat_every: Option<usize>,
    refused: HashSet<String>,
    route_errors: HashSet<String>,
    device_cookie: Option<String>,
    app_calls_missing: bool,
    profile_queries: Vec<String>,
    search_surfaces: Vec<Option<String>>,
    later_claims: Vec<String>,
    /// Whether the app's boot sends a REST POST with its REST token.
    app_rest_posts: bool,
    /// The route of each navigation snob sent, in order.
    navigations: Vec<String>,
    /// The `user_ids` of each `show_many` snob sent, in order.
    statuses: Vec<String>,
    base: String,
}

/// The fake Instagram's data, shared by everything that serves it.
#[derive(Debug, Clone)]
pub struct World(Arc<Mutex<State>>);

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    /// `me` (42), `someone` (9001), the private `ghost` (99) and `user0` to
    /// `user29`:
    ///
    /// - `me` follows `user0`..`user9`, and is followed by `someone`,
    ///   `user0`..`user4` and `user10`..`user14`;
    /// - `someone` follows `me` and `user0`..`user9`, is followed by
    ///   `user0`..`user24`, has two stories up and two highlights;
    /// - `ghost` follows `user0` and is followed by `user0`..`user2`;
    /// - `user0` has a story up, and `user29` keeps the default avatar.
    pub fn new() -> Self {
        let mut people = BTreeMap::new();
        let mut me = Person::named(ME, "me");
        me.following = (0..10).map(|n| FIRST_USER + n).collect();
        let mut someone = Person::named(SOMEONE, "someone");
        someone.following = std::iter::once(ME)
            .chain((0..10).map(|n| FIRST_USER + n))
            .collect();
        someone.stories = vec![
            story("3000000000000000001", 0),
            story("3000000000000000002", 1),
        ];
        someone.highlights = vec![
            Highlight {
                id: "17900000000000001".into(),
                title: "Trips".into(),
                items: vec![
                    story("3100000000000000001", 10),
                    story("3100000000000000002", 11),
                ],
            },
            Highlight {
                id: "17900000000000002".into(),
                title: "Food".into(),
                items: vec![story("3100000000000000003", 12)],
            },
        ];
        let mut ghost = Person::named(GHOST, "ghost");
        ghost.private = true;
        ghost.following = vec![FIRST_USER];
        people.insert(ME, me);
        people.insert(SOMEONE, someone);
        people.insert(GHOST, ghost);
        for n in 0..USERS {
            let pk = FIRST_USER + n;
            let mut user = Person::named(pk, &format!("user{n}"));
            if n < 25 {
                user.following.push(SOMEONE);
            }
            if n < 5 || (10..15).contains(&n) {
                user.following.push(ME);
            }
            if n < 3 {
                user.following.push(GHOST);
            }
            if n == 0 {
                user.stories = vec![story("3000000000000000100", 2)];
            }
            user.anonymous_picture = n == USERS - 1;
            people.insert(pk, user);
        }
        Self(Arc::new(Mutex::new(State {
            people,
            loads: Vec::new(),
            reqs: HashMap::new(),
            repeat_every: None,
            refused: HashSet::new(),
            route_errors: HashSet::new(),
            device_cookie: Some(DEVICE.into()),
            app_calls_missing: false,
            profile_queries: Vec::new(),
            search_surfaces: Vec::new(),
            later_claims: Vec::new(),
            app_rest_posts: false,
            navigations: Vec::new(),
            statuses: Vec::new(),
            base: String::new(),
        })))
    }

    /// The app sends a REST POST with its REST token as each document
    /// boots, as it does where a person opens the activity: the one call a
    /// `show_many` copies its token from. Off by default, as on the
    /// documents snob loads the app sends none.
    pub fn app_rest_posts(&self, sends: bool) {
        self.state().app_rest_posts = sends;
    }

    /// The route of each navigation snob sent, in order.
    pub fn navigations(&self) -> Vec<String> {
        self.state().navigations.clone()
    }

    /// The `user_ids` of each `show_many` snob sent, in order.
    pub fn statuses(&self) -> Vec<String> {
        self.state().statuses.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Changes `pk`'s account, adding it first when there is none.
    pub fn person(&self, pk: u64, change: impl FnOnce(&mut Person)) {
        let mut state = self.state();
        let person = state
            .people
            .entry(pk)
            .or_insert_with(|| Person::named(pk, &format!("user{pk}")));
        change(person);
    }

    /// Every `n`-th row of a list page after the first repeats an account an
    /// earlier page listed, as Instagram's lists do.
    pub fn repeat_every(&self, n: Option<usize>) {
        self.state().repeat_every = n;
    }

    /// A session Instagram no longer takes: its documents are logged out,
    /// and its API calls answer `login_required`.
    pub fn refuse(&self, sessionid: &str) {
        self.state().refused.insert(sessionid.into());
    }

    /// A name whose route definition answers with an error.
    pub fn route_error(&self, name: &str) {
        self.state().route_errors.insert(name.to_ascii_lowercase());
    }

    /// The device cookie a document load sets, or none.
    pub fn device_cookie(&self, cookie: Option<&str>) {
        self.state().device_cookie = cookie.map(str::to_string);
    }

    /// Whether the app on a tab with no browser makes no calls at all.
    pub fn app_calls_missing(&self, missing: bool) {
        self.state().app_calls_missing = missing;
    }

    pub fn is_app_calls_missing(&self) -> bool {
        self.state().app_calls_missing
    }

    /// The variables of each profile query snob sent, in order.
    pub fn profile_queries(&self) -> Vec<String> {
        self.state().profile_queries.clone()
    }

    /// The `search_surface` of each followers page asked, in order.
    pub fn search_surfaces(&self) -> Vec<Option<String>> {
        self.state().search_surfaces.clone()
    }

    /// The claim each list page after a first one carried, in order.
    pub fn later_claims(&self) -> Vec<String> {
        self.state().later_claims.clone()
    }

    /// The account of `pk`: a known one, or any other as `user<pk>`.
    pub fn account(&self, pk: u64) -> Person {
        self.state().account(pk)
    }

    /// The pk of the account called `name`, in any case.
    pub fn pk_named(&self, name: &str) -> Option<u64> {
        self.state().named(name).map(|p| p.pk)
    }

    /// Who the session `sessionid` is, when Instagram takes it.
    pub fn viewer_of(&self, sessionid: &str) -> Option<u64> {
        self.state().viewer_of(sessionid)
    }

    /// The home document served to `viewer`, `None` for a logged-out one,
    /// running `extra_script` after the app's own.
    pub fn document(&self, viewer: Option<u64>, extra_script: &str) -> String {
        self.render(viewer, "/", extra_script).html
    }

    /// The document at `path`, `/` or `/<name>/`, served to `viewer`.
    pub fn render(&self, viewer: Option<u64>, path: &str, extra_script: &str) -> Rendered {
        let mut state = self.state();
        state.render(viewer, path, extra_script)
    }

    /// A document answer, per the `Cookie` it is asked with, running
    /// `extra_script` after the app's own. With the world's device cookie
    /// when `device` says so.
    pub fn page(&self, extra_script: &str) -> WorldPage {
        WorldPage {
            world: self.clone(),
            script: extra_script.into(),
            device: true,
        }
    }

    /// The friendship lists, as served.
    pub fn lists(&self) -> impl Respond + use<> {
        Lists(self.clone())
    }

    /// The document `request` asks for, as served.
    pub fn document_answer(
        &self,
        request: &Request,
        extra_script: &str,
        device: bool,
    ) -> ResponseTemplate {
        let viewer = self.viewer_of_request(request);
        let mut state = self.state();
        let rendered = state.render(viewer, request.url.path(), extra_script);
        let mut answer = ResponseTemplate::new(rendered.status)
            .append_header("Set-Cookie", format!("csrftoken={CSRF}; Path=/"))
            .set_body_raw(rendered.html, "text/html");
        if device && let Some(cookie) = &state.device_cookie {
            answer = answer.append_header("Set-Cookie", format!("{cookie}; Path=/"));
        }
        answer
    }

    fn viewer_of_request(&self, request: &Request) -> Option<u64> {
        let sessionid = cookie(request, "sessionid")?;
        self.viewer_of(&sessionid)
    }

    /// Serves the world: documents, the Relay oracle, route definitions,
    /// lists, the REST reads, and pictures.
    pub async fn serve_web(&self) -> MockServer {
        let server = MockServer::start().await;
        self.state().base = server.uri();
        Mock::given(method("GET"))
            .and(path_regex(r"^/([A-Za-z0-9._]+/)?$"))
            .respond_with(self.page(""))
            .mount(&server)
            .await;
        for posted in ["/api/graphql", "/graphql/query"] {
            Mock::given(method("POST"))
                .and(path(posted))
                .respond_with(Oracle(self.clone()))
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/ajax/bulk-route-definitions/"))
            .respond_with(Routes(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ajax/navigation/"))
            .respond_with(Navigations(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/friendships/show_many/"))
            .respond_with(Statuses(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/friendships/\d+/(followers|following|mutual_followers)/$",
            ))
            .respond_with(Lists(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/(users/\d+/info|users/web_profile_info|highlights/\d+/highlights_tray|feed/reels_media)/$",
            ))
            .respond_with(Legacy(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v1/media/\d+/(info|comments)/$"))
            .respond_with(Posts(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/media/.*\.mp4$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"\0\0\0\x18ftypmp42 invented".to_vec(), "video/mp4"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/media/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"\xff\xd8\xff\xe0 invented".to_vec(), "image/jpeg"),
            )
            .mount(&server)
            .await;
        server
    }

    /// Serves the same world in the REST shapes the client sends without a
    /// browser (`SNOB_NO_BROWSER`, the sandbox): the lists, `count=1` among
    /// them, `users/{pk}/info` and `web_profile_info`, `highlights_tray` and
    /// `reels_media`, the profile document a write reads its tokens from and
    /// the write itself, and the pictures. For the parity test only: what one
    /// command shows through each face is what the two paths are compared
    /// on. A Relay read or a route call here is refused, since the REST path
    /// sends none.
    pub async fn serve_rest(&self) -> MockServer {
        let server = MockServer::start().await;
        self.state().base = server.uri();
        Mock::given(method("GET"))
            .and(path_regex(r"^/([A-Za-z0-9._]+/)?$"))
            .respond_with(self.page(""))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(RestWrites(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/friendships/\d+/(followers|following|mutual_followers)/$",
            ))
            .respond_with(Lists(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/api/v1/(users/\d+/info|users/web_profile_info|highlights/\d+/highlights_tray|feed/reels_media)/$",
            ))
            .respond_with(Legacy(self.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/media/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(b"\xff\xd8\xff\xe0 invented".to_vec(), "image/jpeg"),
            )
            .mount(&server)
            .await;
        server
    }

    /// What was wrong with the requests `server` received from snob: each
    /// one the allowlist refuses (a navigation to one of the app's screens
    /// among them), and each whose path is `retired`. A retired path is
    /// written with `*` for one segment, as `/api/v1/users/*/info/`. The
    /// app's own calls are not snob's and are left out, and so are the
    /// pictures, which the media client downloads with no page, and what a
    /// browser loads by itself for a document, such as its icon (a `no-cors`
    /// GET). Both are GETs: a POST is judged whatever its path and fetch
    /// mode, so a beacon (`no-cors` too) is never left out.
    pub async fn audit(&self, server: &MockServer, retired: &[&str]) -> Vec<String> {
        let mut found = Vec::new();
        for request in server.received_requests().await.unwrap_or_default() {
            let get = request.method.as_str() == "GET";
            if is_the_apps(&request)
                || (get && request.url.path().starts_with("/media/"))
                || (get && header(&request, "sec-fetch-mode") == Some("no-cors"))
            {
                continue;
            }
            let path = request.url.path().to_string();
            let method = match request.method.as_str() {
                "GET" => Method::Get,
                "POST" => Method::Post,
                other => {
                    found.push(format!("{other} {path}"));
                    continue;
                }
            };
            let page = PageRequest {
                method,
                url: request.url.to_string(),
                headers: request
                    .headers
                    .iter()
                    .filter_map(|(n, v)| Some((n.to_string(), v.to_str().ok()?.to_string())))
                    .collect(),
                referrer: header(&request, "referer").unwrap_or_default().into(),
                body: (!request.body.is_empty())
                    .then(|| String::from_utf8_lossy(&request.body).into_owned()),
                navigate: header(&request, "sec-fetch-mode") == Some("navigate"),
                cap: 0,
                timeout_ms: 0,
            };
            if let Some(what) = allowlist::refused(&page) {
                found.push(format!("refused: {what}"));
            }
            if retired.iter().any(|r| matches_path(r, &path)) {
                found.push(format!("retired: {} {path}", request.method));
            }
        }
        found
    }
}

/// A story taken `minutes` after an invented moment.
fn story(pk: &str, minutes: i64) -> Story {
    Story {
        pk: pk.into(),
        taken_at: 1_790_000_000 + minutes * 60,
        video: false,
    }
}

impl State {
    fn account(&self, pk: u64) -> Person {
        self.people
            .get(&pk)
            .cloned()
            .unwrap_or_else(|| Person::named(pk, &format!("user{pk}")))
    }

    fn named(&self, name: &str) -> Option<&Person> {
        self.people
            .values()
            .find(|p| p.username.eq_ignore_ascii_case(name))
    }

    fn follows(&self, who: u64, whom: u64) -> bool {
        self.people
            .get(&who)
            .is_some_and(|p| p.following.contains(&whom))
    }

    fn requested(&self, who: u64, whom: u64) -> bool {
        self.people
            .get(&who)
            .is_some_and(|p| p.requested.contains(&whom))
    }

    fn followers(&self, pk: u64) -> Vec<u64> {
        self.people
            .values()
            .filter(|p| p.following.contains(&pk))
            .map(|p| p.pk)
            .collect()
    }

    fn following(&self, pk: u64) -> Vec<u64> {
        self.people
            .get(&pk)
            .map(|p| p.following.clone())
            .unwrap_or_default()
    }

    fn viewer_of(&self, sessionid: &str) -> Option<u64> {
        let decoded = sessionid.replace("%3A", ":").replace("%3a", ":");
        if self.refused.contains(sessionid) || self.refused.contains(&decoded) {
            return None;
        }
        decoded.split(':').next()?.parse().ok()
    }

    /// Whether `viewer` may see `pk`'s lists and reels.
    fn may_see(&self, viewer: u64, pk: u64) -> bool {
        viewer == pk || !self.account(pk).private || self.follows(viewer, pk)
    }

    fn render(&mut self, viewer: Option<u64>, path: &str, extra_script: &str) -> Rendered {
        let n = self.loads.len() as u64 + 1;
        let spin_t = 1_790_000_000 + n;
        let Some(viewer) = viewer else {
            return Rendered {
                status: 200,
                html: shaped(None, spin_t, "", extra_script),
                forms: Vec::new(),
            };
        };
        let session = format!("web{viewer}:tab{n}:load{n}");
        self.loads.push(Load {
            viewer,
            session: session.clone(),
            spin_t,
        });
        let load = self.loads.last().expect("just pushed").clone();
        let name = path.trim_matches('/');
        let profile = (!name.is_empty()).then(|| self.named(name).map(|p| p.pk));
        let hover = Operation::HoverCard;
        let mut forms = vec![
            (
                hover.path().to_string(),
                relay_form(
                    &load,
                    hover.friendly_name(),
                    hover.doc_id(),
                    &format!(r#"{{"userID":"{viewer}"}}"#),
                    1,
                ),
            ),
            (
                "/ajax/bulk-route-definitions/".to_string(),
                routes_form(&load, 2),
            ),
            (
                "/api/graphql".to_string(),
                relay_form(&load, NOT_IN_THE_REGISTRY, "1000000000000009", "{}", 3),
            ),
        ];
        if self.app_rest_posts {
            forms.push(("/api/v1/news/inbox/".to_string(), rest_post_form(viewer)));
        }
        let mut extra_json = String::new();
        let mut status = 200;
        match profile {
            None => {
                let tray: Vec<Value> = self
                    .following(viewer)
                    .into_iter()
                    .filter(|pk| !self.account(*pk).stories.is_empty())
                    .map(|pk| json!({ "id": pk.to_string(), "user": { "pk": pk.to_string() } }))
                    .collect();
                extra_json = format!(
                    r#"<script type="application/json">{{"require":[["RelayPrefetchedStreamCache","next",[],["adp_PolarisStoriesV3TrayContainerQueryRelayPreloader",{{"__bbox":{{"complete":true,"result":{{"data":{{"xdt_api__v1__feed__reels_tray":{}}}}}}}}}]]]}}</script>"#,
                    json!({ "tray": tray })
                );
            }
            Some(Some(pk)) => {
                let operation = Operation::ProfilePage;
                forms.push((
                    operation.path().to_string(),
                    relay_form(
                        &load,
                        operation.friendly_name(),
                        operation.doc_id(),
                        &app_profile_variables(pk),
                        4,
                    ),
                ));
                extra_json = format!(
                    r#"<script type="application/json">{{"require":[["RouteProps",null,null,[{{"props":{{"id":"{pk}"}}}}]]]}}</script>"#
                );
            }
            Some(None) => status = 404,
        }
        let boot = boot_script(viewer, &forms);
        let html = shaped(
            Some(&self.account(viewer)),
            spin_t,
            &format!("{extra_json}<script>{boot}</script>"),
            extra_script,
        );
        Rendered {
            status,
            html,
            forms,
        }
    }
}

/// A document shaped like the web app's: the modules the page values are
/// read from, `between` (the preloads and the app's boot), then
/// `extra_script`.
fn shaped(viewer: Option<&Person>, spin_t: u64, between: &str, extra_script: &str) -> String {
    let (polaris, lsd, relay, session) = match viewer {
        Some(p) => (
            json!({
                "data": {
                    "id": p.pk.to_string(),
                    "username": p.username,
                    "full_name": p.full_name,
                    "fbid": fbid(p.pk),
                    "is_private": p.private,
                },
                "id": p.pk.to_string(),
            }),
            lsd(p.pk),
            relay_token(p.pk),
            session_token(p.pk),
        ),
        None => (
            json!({ "data": null, "id": null }),
            "lsdLoggedOut".to_string(),
            String::new(),
            String::new(),
        ),
    };
    let site = |spin_t: u64| {
        json!({
            "client_revision": REVISION.parse::<u64>().unwrap(),
            "haste_session": HASTE,
            "hsi": HSI,
            "__spin_r": REVISION.parse::<u64>().unwrap(),
            "__spin_b": SPIN_B,
            "__spin_t": spin_t,
        })
    };
    let define = json!([
        ["SiteData", [], site(spin_t), 317],
        ["SiteData", [], site(1), 317],
        ["LSD", [], { "token": lsd }, 323],
        ["DTSGInitialData", [], { "token": relay }, 258],
        ["DTSGInitData", [], { "token": session, "async_get_token": "async" }, 3515],
        ["WebBloksVersioningID", [], { "versioningID": bloks() }, 6013],
        ["PolarisViewer", [], polaris, 1508],
    ]);
    let modules = json!({
        "require": [["ScheduledServerJS", "handle", null, [{ "__bbox": { "define": define } }]]]
    });
    let extra = if extra_script.is_empty() {
        String::new()
    } else {
        format!("<script>{extra_script}</script>")
    };
    format!(
        r#"<!doctype html><title>Instagram</title>
<script type="application/json" data-sjs>{modules}</script>
{between}{extra}"#
    )
}

/// A Relay form of the app's on `load`, its counter at `req`.
fn relay_form(load: &Load, name: &str, doc_id: &str, variables: &str, req: u32) -> String {
    let token = relay_token(load.viewer);
    let mut fields = vec![
        ("av", fbid(load.viewer)),
        ("__d", "www".into()),
        ("__user", "0".into()),
        ("__a", "1".into()),
        ("__req", base36(req)),
        ("__hs", HASTE.into()),
        ("dpr", DPR.into()),
        ("__ccg", CCG.into()),
        ("__rev", REVISION.into()),
        ("__s", load.session.clone()),
        ("__hsi", HSI.into()),
    ];
    fields.extend(BITMAPS.map(|(n, v)| (n, v.to_string())));
    fields.extend([
        ("__comet_req", "7".to_string()),
        ("jazoest", snob_ig::graphql::jazoest(&token)),
        ("lsd", lsd(load.viewer)),
        ("__spin_r", REVISION.into()),
        ("__spin_b", SPIN_B.into()),
        ("__spin_t", load.spin_t.to_string()),
        ("fb_api_caller_class", "RelayModern".into()),
        ("fb_api_req_friendly_name", name.into()),
        ("server_timestamps", "true".into()),
        ("variables", variables.into()),
        ("doc_id", doc_id.into()),
    ]);
    // `fb_dtsg` goes before `jazoest`, where the app puts it.
    let at = fields.iter().position(|(n, _)| *n == "jazoest").unwrap();
    fields.insert(at, ("fb_dtsg", token));
    encode(&fields)
}

/// A REST POST's form of the app's, carrying the REST token: what the app
/// sends where a person opens the activity, and the one call snob takes the
/// token of a `show_many` from.
fn rest_post_form(viewer: u64) -> String {
    let token = rest_token(viewer);
    encode(&[
        ("jazoest", snob_ig::graphql::jazoest(&token)),
        ("fb_dtsg", token),
    ])
}

/// The REST token of `pk`'s session, which no document shows.
pub fn rest_token(pk: u64) -> String {
    format!("rest:{pk}")
}

/// A route call's form of the app's on `load`, its counter at `req`.
fn routes_form(load: &Load, req: u32) -> String {
    let token = relay_token(load.viewer);
    let fields = vec![
        ("route_urls[0]", "/someone/".to_string()),
        ("routing_namespace", "igx_www".into()),
        ("__d", "www".into()),
        ("__user", "0".into()),
        ("__a", "1".into()),
        ("__req", base36(req)),
        ("__hs", HASTE.into()),
        ("dpr", DPR.into()),
        ("__ccg", CCG.into()),
        ("__rev", REVISION.into()),
        ("__s", load.session.clone()),
        ("__hsi", HSI.into()),
        ("__dyn", BITMAPS[0].1.into()),
        ("__csr", BITMAPS[1].1.into()),
        ("__comet_req", "7".into()),
        ("fb_dtsg", token.clone()),
        ("jazoest", snob_ig::graphql::jazoest(&token)),
        ("lsd", lsd(load.viewer)),
        ("__spin_r", REVISION.into()),
        ("__spin_b", SPIN_B.into()),
        ("__spin_t", load.spin_t.to_string()),
        ("__crn", "comet.igweb.PolarisFeedRoute".into()),
    ];
    encode(&fields)
}

/// The script that sends the app's calls as the document boots, each marked
/// as the app's.
fn boot_script(viewer: u64, forms: &[(String, String)]) -> String {
    let calls: Vec<String> = forms
        .iter()
        .map(|(to, form)| {
            let name = url::form_urlencoded::parse(form.as_bytes())
                .find(|(n, _)| n == "fb_api_req_friendly_name")
                .map(|(_, v)| v.into_owned());
            let mut headers = json!({
                "Content-Type": "application/x-www-form-urlencoded",
                "X-FB-LSD": lsd(viewer),
                APP_HEADER: "1",
            });
            if let Some(name) = name {
                headers["X-FB-Friendly-Name"] = json!(name);
            }
            format!(
                "fetch({}, {{ method: 'POST', headers: {headers}, body: {} }}).catch(() => {{}});",
                json!(to),
                json!(form)
            )
        })
        .collect();
    calls.join("\n")
}

fn encode(fields: &[(&str, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

fn base36(mut n: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).expect("ASCII digits")
}

pub fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

/// Refuses the two headers a Relay call never carries, the claim and
/// `X-Requested-With`: on the web face and the REST one alike.
fn no_app_headers(request: &Request) -> Result<(), String> {
    for never in ["x-ig-www-claim", "x-requested-with"] {
        if header(request, never).is_some() {
            return Err(format!("{never} on a Relay call"));
        }
    }
    Ok(())
}

/// Whether the app sent `request`, rather than snob.
pub fn is_the_apps(request: &Request) -> bool {
    header(request, APP_HEADER).is_some()
}

/// The cookie `name` of `request`.
fn cookie(request: &Request, name: &str) -> Option<String> {
    header(request, "cookie")?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.to_string())
}

/// Whether `path` is `pattern`, where a `*` stands for one segment.
fn matches_path(pattern: &str, path: &str) -> bool {
    let (pattern, path): (Vec<&str>, Vec<&str>) =
        (pattern.split('/').collect(), path.split('/').collect());
    pattern.len() == path.len() && pattern.iter().zip(&path).all(|(p, s)| *p == "*" || p == s)
}

/// A document of the world, per the `Cookie` it is asked with.
pub struct WorldPage {
    world: World,
    script: String,
    device: bool,
}

impl WorldPage {
    /// Without the world's device cookie.
    pub fn without_a_device(mut self) -> Self {
        self.device = false;
        self
    }
}

impl Respond for WorldPage {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.world
            .document_answer(request, &self.script, self.device)
    }
}

/// What a session Instagram does not take is answered on the API.
fn login_required() -> ResponseTemplate {
    ResponseTemplate::new(401).set_body_raw(
        r#"{"message":"login_required","status":"fail"}"#,
        "application/json",
    )
}

/// The oracle refusing a form: a 400 that says why, never with a token.
fn wrong(reason: impl std::fmt::Display) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_string(format!("the fake refused the call: {reason}"))
}

fn json_answer(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
}

/// The route a write from a profile is made on.
const PROFILE_ROUTE: &str = "comet.igweb.PolarisProfilePostsTabRoute";

/// The fields of a Relay form, in the app's order, `__crn` aside.
const RELAY_FIELDS: [&str; 28] = [
    "av",
    "__d",
    "__user",
    "__a",
    "__req",
    "__hs",
    "dpr",
    "__ccg",
    "__rev",
    "__s",
    "__hsi",
    "__dyn",
    "__csr",
    "__hsdp",
    "__hblp",
    "__sjsp",
    "__comet_req",
    "fb_dtsg",
    "jazoest",
    "lsd",
    "__spin_r",
    "__spin_b",
    "__spin_t",
    "fb_api_caller_class",
    "fb_api_req_friendly_name",
    "server_timestamps",
    "variables",
    "doc_id",
];

type Form = Vec<(String, String)>;

fn form_of(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

fn field<'a>(form: &'a Form, name: &str) -> Option<&'a str> {
    form.iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// The Relay endpoints.
struct Oracle(World);

impl Respond for Oracle {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        relay_answered(&self.0, request, State::judge_relay)
    }
}

/// What `world` answers the Relay form of `request`, once `judge` has taken
/// it: the operation's answer, or a 400 that says why not.
fn relay_answered(
    world: &World,
    request: &Request,
    judge: fn(&mut State, &Request, &Form, u64) -> Result<Operation, String>,
) -> ResponseTemplate {
    let Some(viewer) = world.viewer_of_request(request) else {
        return login_required();
    };
    let form = form_of(request);
    let mut state = world.state();
    let operation = match judge(&mut state, request, &form, viewer) {
        Ok(operation) => operation,
        Err(reason) => return wrong(reason),
    };
    let variables: Value = field(&form, "variables")
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(Value::Null);
    if operation == Operation::ProfilePage && !is_the_apps(request) {
        state
            .profile_queries
            .push(field(&form, "variables").unwrap_or_default().into());
    }
    let answer = state.relay_answer(operation, &variables, viewer);
    match operation.family() {
        Family::RelayQuery => {
            ResponseTemplate::new(200).set_body_raw(answer.to_string(), "text/javascript")
        }
        Family::Relay => json_answer(answer),
    }
}

/// The Relay endpoint of the REST face: the two writes, as the client sends
/// them without a browser, and nothing else.
struct RestWrites(World);

impl Respond for RestWrites {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        relay_answered(&self.0, request, State::judge_rest_write)
    }
}

/// A variable that names an account, as a pk.
fn pk_in(variables: &Value, name: &str) -> Option<u64> {
    match variables.get(name)? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

impl State {
    /// What is wrong with a Relay form, or the operation it is.
    fn judge_relay(
        &mut self,
        request: &Request,
        form: &Form,
        viewer: u64,
    ) -> Result<Operation, String> {
        let name = field(form, "fb_api_req_friendly_name").ok_or("no friendly name")?;
        let operation = Operation::named(name).ok_or("an operation the registry does not hold")?;
        if header(request, "x-fb-friendly-name") != Some(name) {
            return Err("X-FB-Friendly-Name is not the form's".into());
        }
        if request.url.path() != operation.path() {
            return Err(format!("{name} on the wrong endpoint"));
        }
        if header(request, "x-fb-lsd") != Some(lsd(viewer).as_str())
            || field(form, "lsd") != Some(lsd(viewer).as_str())
        {
            return Err("not the document's lsd".into());
        }
        if let Some(csrf) = header(request, "x-csrftoken")
            && Some(csrf.to_string()) != cookie(request, "csrftoken")
        {
            return Err("X-CSRFToken is not the cookie's".into());
        }
        no_app_headers(request)?;
        let names: Vec<&str> = form
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| *n != "__crn")
            .collect();
        if names != RELAY_FIELDS {
            return Err(format!(
                "the fields are not the app's, in its order: {names:?}"
            ));
        }
        if let Some(at) = form.iter().position(|(n, _)| n == "__crn")
            && form.get(at - 1).map(|(n, _)| n.as_str()) != Some("__spin_t")
        {
            return Err("__crn out of place".into());
        }
        if field(form, "av") != Some(fbid(viewer).as_str()) {
            return Err("av is not the viewer's fbid".into());
        }
        if operation.write().is_some() && field(form, "__crn") != Some(PROFILE_ROUTE) {
            return Err("a write off the profile's route".into());
        }
        let fixed = [
            ("__d", "www"),
            ("__user", "0"),
            ("__a", "1"),
            ("__hs", HASTE),
            ("dpr", DPR),
            ("__ccg", CCG),
            ("__rev", REVISION),
            ("__hsi", HSI),
            ("__comet_req", "7"),
            ("__spin_r", REVISION),
            ("__spin_b", SPIN_B),
            ("fb_api_caller_class", "RelayModern"),
            ("server_timestamps", "true"),
        ];
        for (name, value) in fixed.into_iter().chain(BITMAPS) {
            if field(form, name) != Some(value) {
                return Err(format!("{name} is not the page's"));
            }
        }
        let token = relay_token(viewer);
        if field(form, "fb_dtsg") != Some(token.as_str())
            || field(form, "jazoest") != Some(snob_ig::graphql::jazoest(&token).as_str())
        {
            return Err("not the document's Relay token".into());
        }
        self.judge_load(request, form, viewer)?;
        // A write's is the seed, the id this fake serves it under.
        if field(form, "doc_id") != Some(operation.doc_id()) {
            return Err(format!("{name} under another doc_id"));
        }
        match operation.family() {
            Family::RelayQuery => {
                if header(request, "x-root-field-name") != Some(operation.root_field()) {
                    return Err("X-Root-Field-Name is not the operation's".into());
                }
                if header(request, "x-bloks-version-id") != Some(bloks().as_str()) {
                    return Err("X-Bloks-Version-Id is not the page's".into());
                }
            }
            Family::Relay => {
                if header(request, "x-root-field-name").is_some() {
                    return Err("X-Root-Field-Name on /api/graphql".into());
                }
            }
        }
        Ok(operation)
    }

    /// What is wrong with a write as the client sends it without a
    /// browser, or the operation it is: the twelve fields, the token the
    /// document hands a write, and a `doc_id`. Nothing but a write.
    fn judge_rest_write(
        &mut self,
        request: &Request,
        form: &Form,
        viewer: u64,
    ) -> Result<Operation, String> {
        let name = field(form, "fb_api_req_friendly_name").ok_or("no friendly name")?;
        let operation = Operation::named(name)
            .filter(|operation| operation.write().is_some())
            .ok_or("a Relay read on the REST face")?;
        if header(request, "x-fb-friendly-name") != Some(name) {
            return Err("X-FB-Friendly-Name is not the form's".into());
        }
        if request.url.path() != operation.path() {
            return Err(format!("{name} on the wrong endpoint"));
        }
        if header(request, "x-fb-lsd") != Some(lsd(viewer).as_str())
            || field(form, "lsd") != Some(lsd(viewer).as_str())
        {
            return Err("not the document's lsd".into());
        }
        if let Some(csrf) = header(request, "x-csrftoken")
            && Some(csrf.to_string()) != cookie(request, "csrftoken")
        {
            return Err("X-CSRFToken is not the cookie's".into());
        }
        no_app_headers(request)?;
        if form.len() != 12 || field(form, "av").is_some() {
            return Err("not the twelve fields of the write without a browser".into());
        }
        if field(form, "fb_dtsg") != Some(session_token(viewer).as_str()) {
            return Err("not the document's write token".into());
        }
        if field(form, "doc_id") != Some(operation.doc_id()) {
            return Err(format!("{name} under another doc_id"));
        }
        Ok(operation)
    }

    /// The per-load values of a form: a web session id handed to this
    /// viewer, its document's `__spin_t`, and a counter past every one that
    /// document has sent. The app's own calls are not held to the counter:
    /// they leave as the document boots, in no fixed order.
    fn judge_load(&mut self, request: &Request, form: &Form, viewer: u64) -> Result<(), String> {
        let session = field(form, "__s").ok_or("no __s")?;
        let load = self
            .loads
            .iter()
            .find(|l| l.session == session)
            .ok_or("a web session id no document handed out")?;
        if load.viewer != viewer {
            return Err("another account's web session id".into());
        }
        if field(form, "__spin_t") != Some(load.spin_t.to_string().as_str()) {
            return Err("not the document's __spin_t".into());
        }
        let req = field(form, "__req")
            .and_then(|r| u32::from_str_radix(r, 36).ok())
            .ok_or("a __req that does not read")?;
        let highest = self.reqs.entry(session.to_string()).or_insert(0);
        if !is_the_apps(request) && req <= *highest {
            return Err(format!("__req {req} after {highest} on the same document"));
        }
        *highest = (*highest).max(req);
        Ok(())
    }

    fn relay_answer(&mut self, operation: Operation, variables: &Value, viewer: u64) -> Value {
        match operation {
            Operation::ProfilePage => {
                let user = pk_in(variables, "id")
                    .filter(|pk| self.people.contains_key(pk) || *pk == viewer)
                    .map(|pk| self.graph_user(pk, viewer));
                json!({
                    "data": {
                        "user": user,
                        "viewer": { "user": { "pk": viewer.to_string(), "id": viewer.to_string() } },
                    },
                    "extensions": { "is_final": true },
                })
            }
            Operation::HoverCard => {
                let user = pk_in(variables, "userID")
                    .filter(|pk| self.people.contains_key(pk) || *pk == viewer)
                    .map(|pk| self.graph_user(pk, viewer));
                json!({ "data": { "xig_user_by_igid_v2": { "user_dict": user } } })
            }
            Operation::HighlightsTray => {
                let pk = pk_in(variables, "user_id").unwrap_or_default();
                let edges: Vec<Value> = if self.may_see(viewer, pk) {
                    self.account(pk)
                        .highlights
                        .iter()
                        .map(|h| {
                            json!({ "node": {
                                "id": format!("highlight:{}", h.id),
                                "title": h.title,
                                "cover_media": { "cropped_image_version": { "url": self.media(&h.id) } },
                                "user": { "pk": pk.to_string() },
                            }})
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                json!({ "data": { "highlights": {
                    "edges": edges,
                    "page_info": { "has_next_page": false, "end_cursor": null },
                }}})
            }
            Operation::HighlightsPage | Operation::ReelGallery | Operation::ReelGalleryPage => {
                let ids: Vec<String> = variables
                    .get("reel_ids")
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| match id {
                                Value::String(s) => Some(s.clone()),
                                Value::Number(n) => Some(n.to_string()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let edges: Vec<Value> = ids
                    .iter()
                    .filter_map(|id| self.reel(id, viewer, true))
                    .map(|node| json!({ "node": node }))
                    .collect();
                json!({ "data": { "xdt_api__v1__feed__reels_media__connection": {
                    "edges": edges,
                    "page_info": { "has_next_page": false, "end_cursor": null },
                }}})
            }
            Operation::StoriesTray => json!({ "data": null }),
            Operation::NoteBubble => {
                json!({ "data": { "xdt_get_inbox_tray_items": { "inbox_tray_items": [] } } })
            }
            Operation::SchoolPartnerBadge => {
                let pk = pk_in(variables, "igid").unwrap_or_default();
                json!({ "data": { "xig_user_by_igid_v2": {
                    "id": pk.to_string(),
                    "school_partner": null,
                }}})
            }
            Operation::ProfilePosts | Operation::ProfilePostsPage => {
                let after = match operation {
                    Operation::ProfilePostsPage => {
                        let Some(after) = variables.get("after").and_then(Value::as_str) else {
                            return json!({ "errors": [{ "message": "a next page with no cursor" }] });
                        };
                        let Some(offset) = after
                            .strip_prefix("posts:")
                            .and_then(|n| n.parse::<usize>().ok())
                        else {
                            return json!({ "errors": [{ "message": "a cursor this grid never handed out" }] });
                        };
                        offset
                    }
                    _ => 0,
                };
                let shown = variables
                    .get("username")
                    .and_then(Value::as_str)
                    .and_then(|name| self.named(name).map(|p| p.pk))
                    .filter(|pk| self.may_see(viewer, *pk));
                let posts: Vec<Value> = shown.map(|pk| self.posts_of(pk)).unwrap_or_default();
                let page: Vec<Value> = posts
                    .iter()
                    .skip(after)
                    .take(POSTS_PER_PAGE)
                    .cloned()
                    .collect();
                let next = after + page.len();
                let more = next < posts.len();
                let root = operation.root_field();
                json!({ "data": {
                    root: {
                        "edges": page.into_iter().map(|node| json!({ "node": node, "cursor": "" })).collect::<Vec<_>>(),
                        "page_info": {
                            "end_cursor": more.then(|| format!("posts:{next}")),
                            "has_next_page": more,
                            "has_previous_page": false,
                            "start_cursor": null,
                        },
                    },
                    "xdt_viewer": { "user": { "id": viewer.to_string() } },
                }, "extensions": { "is_final": true }})
            }
            Operation::Follow | Operation::Unfollow => {
                let target = pk_in(variables, "target_user_id").unwrap_or_default();
                let follow = operation == Operation::Follow;
                let private = self.account(target).private;
                let me = self
                    .people
                    .entry(viewer)
                    .or_insert_with(|| Person::named(viewer, &format!("user{viewer}")));
                me.following.retain(|pk| *pk != target);
                me.requested.retain(|pk| *pk != target);
                if follow && private {
                    me.requested.push(target);
                } else if follow {
                    me.following.push(target);
                }
                let status = json!({
                    "following": self.follows(viewer, target),
                    "outgoing_request": self.requested(viewer, target),
                    "followed_by": self.follows(target, viewer),
                    "is_private": private,
                });
                let root = operation.root_field();
                json!({ "data": { root: { "friendship_status": status } }, "status": "ok" })
            }
        }
    }

    /// Account `pk` as the profile query and the hover card describe it to
    /// `viewer`.
    fn graph_user(&self, pk: u64, viewer: u64) -> Value {
        let person = self.account(pk);
        let own = pk == viewer;
        let relationship = (!own).then(|| {
            json!({
                "following": self.follows(viewer, pk),
                "followed_by": self.follows(pk, viewer),
                "outgoing_request": self.requested(viewer, pk),
                "incoming_request": self.requested(pk, viewer),
                "blocking": false,
                "muting": false,
                "is_restricted": false,
                "is_bestie": false,
                "is_feed_favorite": false,
            })
        });
        let mutual: Vec<u64> = if own {
            Vec::new()
        } else {
            let followers = self.followers(pk);
            self.following(viewer)
                .into_iter()
                .filter(|f| followers.contains(f))
                .collect()
        };
        let links: Vec<Value> = mutual
            .iter()
            .take(3)
            .map(|pk| json!({ "username": self.account(*pk).username }))
            .collect();
        let latest = person.stories.iter().map(|s| s.taken_at).max().unwrap_or(0);
        json!({
            "pk": pk.to_string(),
            "id": pk.to_string(),
            "username": person.username,
            "full_name": person.full_name,
            "biography": format!("the invented bio of {}", person.username),
            "external_url": null,
            "is_private": person.private,
            "is_verified": false,
            "is_business": false,
            "category": null,
            "follower_count": self.followers(pk).len(),
            "following_count": self.following(pk).len(),
            "media_count": person.posts,
            "profile_pic_url": self.media(&format!("{pk}-small")),
            "hd_profile_pic_url_info": person
                .full_size_picture
                .then(|| json!({ "url": self.media(&format!("{pk}-full")) })),
            "has_anonymous_profile_picture": person.anonymous_picture,
            "latest_reel_media": latest,
            "friendship_status": relationship,
            "mutual_followers_count": (!own).then_some(mutual.len()),
            "profile_context_links_with_user_ids": links,
        })
    }

    /// The posts of account `pk`, newest first, as the grid and the info
    /// read describe them: as many as its `posts` says, the first a carousel
    /// of a photo and a video tagging `user1`, the second a reel, the rest
    /// photos. Each is liked by `user2` among others, and mentions `user3`.
    fn posts_of(&self, pk: u64) -> Vec<Value> {
        let person = self.account(pk);
        (0..person.posts)
            .map(|n| {
                let media = post_pk(pk, n);
                let code = code_of(media);
                let picture = |name: &str| {
                    json!({ "candidates": [
                        { "url": self.media(name), "width": 1080, "height": 1350 },
                        { "url": self.media(&format!("{name}-small")), "width": 150, "height": 150 },
                    ]})
                };
                let mut post = json!({
                    "pk": media.to_string(),
                    "id": format!("{media}_{pk}"),
                    "code": code,
                    "taken_at": 1_790_000_000 - (n as i64) * 86_400,
                    "media_type": 1,
                    "product_type": "feed",
                    "caption": { "text": format!("post {n} with @user3"), "pk": "1" },
                    "like_count": 10 + n,
                    "comment_count": COMMENTS,
                    "like_and_view_counts_disabled": false,
                    "top_likers": ["user2"],
                    "usertags": null,
                    "coauthor_producers": [],
                    "user": { "pk": pk.to_string(), "username": person.username },
                    "image_versions2": picture(&media.to_string()),
                    "video_versions": null,
                    "carousel_media": null,
                });
                match n {
                    0 => {
                        post["media_type"] = json!(8);
                        post["product_type"] = json!("carousel_container");
                        post["carousel_media_count"] = json!(2);
                        post["carousel_media"] = json!([
                            {
                                "pk": (media + 1).to_string(),
                                "media_type": 1,
                                "product_type": "carousel_item",
                                "image_versions2": picture(&format!("{media}-1")),
                                "video_versions": null,
                                "usertags": { "in": [{ "user": { "username": "user1" }, "position": [0.5, 0.5] }] },
                            },
                            {
                                "pk": (media + 2).to_string(),
                                "media_type": 2,
                                "product_type": "carousel_item",
                                "image_versions2": picture(&format!("{media}-2")),
                                "video_versions": [
                                    { "type": 101, "width": 720, "height": 1280, "url": self.video(&format!("{media}-2")) },
                                ],
                            },
                        ]);
                    }
                    1 => {
                        post["media_type"] = json!(2);
                        post["product_type"] = json!("clips");
                        post["play_count"] = json!(1234);
                        post["video_versions"] = json!([
                            { "type": 101, "width": 720, "height": 1280, "url": self.video(&media.to_string()) },
                            { "type": 102, "width": 720, "height": 1280, "url": self.video(&media.to_string()) },
                        ]);
                    }
                    _ => {}
                }
                post
            })
            .collect()
    }

    /// The post `media`, with its owner, when `viewer` may see it.
    fn post(&self, media: u64, viewer: u64) -> Option<Value> {
        let offset = media.checked_sub(FIRST_POST)?;
        let (owner, n) = (offset / POST_STRIDE, offset % POST_STRIDE);
        if !self.people.contains_key(&owner) || !self.may_see(viewer, owner) {
            return None;
        }
        self.posts_of(owner).into_iter().nth(n as usize)
    }

    /// A video's address on the fake: an MP4, as the CDN serves one.
    fn video(&self, name: &str) -> String {
        format!("{}/media/{name}.mp4", self.base)
    }

    /// A picture's address on the fake.
    fn media(&self, name: &str) -> String {
        format!("{}/media/{name}.jpg", self.base)
    }

    /// The reel `id` names, a pk or `highlight:<id>`, when `viewer` may see
    /// one. `graph` shapes its items as the Relay answers do.
    fn reel(&self, id: &str, viewer: u64, graph: bool) -> Option<Value> {
        let (owner, items) = match id.strip_prefix("highlight:") {
            Some(bare) => self.people.values().find_map(|p| {
                p.highlights
                    .iter()
                    .find(|h| h.id == bare)
                    .map(|h| (p.pk, h.items.clone()))
            })?,
            None => {
                let pk: u64 = id.parse().ok()?;
                (pk, self.account(pk).stories)
            }
        };
        if items.is_empty() || !self.may_see(viewer, owner) {
            return None;
        }
        let owner = self.account(owner);
        let items: Vec<Value> = items
            .iter()
            .map(|item| {
                let mut shaped = json!({
                    "pk": item.pk,
                    "id": format!("{}_{}", item.pk, owner.pk),
                    "media_type": if item.video { 2 } else { 1 },
                    "taken_at": item.taken_at,
                    "image_versions2": { "candidates": [
                        { "width": 1080, "height": 1920, "url": self.media(&item.pk) },
                    ]},
                });
                // A photo's Relay item says `null`; the REST one leaves the
                // key out, as Instagram's does.
                if item.video {
                    shaped["video_versions"] =
                        json!([{ "type": 101, "url": self.media(&format!("{}-video", item.pk)) }]);
                } else if graph {
                    shaped["video_versions"] = Value::Null;
                }
                if graph || !id.starts_with("highlight:") {
                    shaped["expiring_at"] = json!(item.taken_at + 86_400);
                }
                shaped
            })
            .collect();
        Some(json!({
            "id": id,
            "items": items,
            "user": { "pk": owner.pk, "username": owner.username },
        }))
    }
}

/// The route definitions.
struct Routes(World);

impl Respond for Routes {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Some(viewer) = self.0.viewer_of_request(request) else {
            return login_required();
        };
        let form = form_of(request);
        let mut state = self.0.state();
        if header(request, "x-fb-lsd") != Some(lsd(viewer).as_str())
            || field(&form, "lsd") != Some(lsd(viewer).as_str())
        {
            return wrong("not the document's lsd");
        }
        if field(&form, "fb_dtsg") != Some(relay_token(viewer).as_str()) {
            return wrong("not the document's Relay token");
        }
        if header(request, "x-csrftoken").is_some() || header(request, "x-ig-www-claim").is_some() {
            return wrong("a route call carries neither the CSRF token nor the claim");
        }
        if let Err(reason) = state.judge_load(request, &form, viewer) {
            return wrong(reason);
        }
        let mut payloads = serde_json::Map::new();
        for (_, route) in form.iter().filter(|(n, _)| n.starts_with("route_urls[")) {
            let name = route.trim_matches('/');
            let entry = if state.route_errors.contains(&name.to_ascii_lowercase()) {
                json!({ "error": { "code": 1_357_004, "summary": "an invented error" } })
            } else if let Some(person) = state.named(name) {
                json!({ "error": false, "result": {
                    "type": "route_definition",
                    "exports": { "hostableView": { "props": { "id": person.pk.to_string() } } },
                }})
            } else {
                json!({ "error": false, "result": {
                    "type": "route_redirect",
                    "redirect_result": { "url": "/explore/search/keyword/" },
                }})
            };
            payloads.insert(route.clone(), entry);
        }
        ResponseTemplate::new(200).set_body_raw(
            format!(
                "for (;;);{}",
                json!({ "payload": { "payloads": payloads } })
            ),
            "application/x-javascript",
        )
    }
}

/// The app's router navigating: a route snob may be on, in the route
/// definitions' envelope, after the viewer's `fbid` and the route.
struct Navigations(World);

impl Respond for Navigations {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Some(viewer) = self.0.viewer_of_request(request) else {
            return login_required();
        };
        let form = form_of(request);
        let mut state = self.0.state();
        if header(request, "x-fb-lsd") != Some(lsd(viewer).as_str())
            || field(&form, "lsd") != Some(lsd(viewer).as_str())
        {
            return wrong("not the document's lsd");
        }
        if field(&form, "fb_dtsg") != Some(relay_token(viewer).as_str()) {
            return wrong("not the document's Relay token");
        }
        if header(request, "x-csrftoken").is_some() || header(request, "x-ig-www-claim").is_some() {
            return wrong("a navigation carries neither the CSRF token nor the claim");
        }
        let names: Vec<&str> = form.iter().take(3).map(|(n, _)| n.as_str()).collect();
        if names != ["client_previous_actor_id", "route_url", "routing_namespace"] {
            return wrong(format!("not the app's navigation: {names:?}"));
        }
        if field(&form, "client_previous_actor_id") != Some(fbid(viewer).as_str()) {
            return wrong("client_previous_actor_id is not the viewer's fbid");
        }
        let route = field(&form, "route_url").unwrap_or_default().to_string();
        if !allowlist::list_route(&route) {
            return wrong("a navigation to a route snob does not read from");
        }
        let referrer = header(request, "referer").unwrap_or_default();
        if !referrer.ends_with(&route) {
            return wrong("a navigation not made from its route");
        }
        if let Err(reason) = state.judge_load(request, &form, viewer) {
            return wrong(reason);
        }
        if !is_the_apps(request) {
            state.navigations.push(route);
        }
        ResponseTemplate::new(200).set_body_raw(
            format!(
                "for (;;);{}",
                json!({ "payload": { "payload": { "error": false, "result": {
                    "type": "route_definition",
                }}}})
            ),
            "application/x-javascript",
        )
    }
}

/// `friendships/show_many`, which answers how the viewer stands with each
/// account asked about.
struct Statuses(World);

impl Respond for Statuses {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Some(viewer) = self.0.viewer_of_request(request) else {
            return login_required();
        };
        let form = form_of(request);
        let mut state = self.0.state();
        let token = rest_token(viewer);
        if field(&form, "fb_dtsg") != Some(token.as_str())
            || field(&form, "jazoest") != Some(snob_ig::graphql::jazoest(&token).as_str())
        {
            return wrong("not the session's REST token");
        }
        if header(request, "x-instagram-ajax") != Some(REVISION) {
            return wrong("X-Instagram-AJAX is not the page's revision");
        }
        if let Some(csrf) = header(request, "x-csrftoken")
            && Some(csrf.to_string()) != cookie(request, "csrftoken")
        {
            return wrong("X-CSRFToken is not the cookie's");
        }
        let ids = field(&form, "user_ids").unwrap_or_default().to_string();
        let mut statuses = serde_json::Map::new();
        for pk in ids.split(',').filter_map(|pk| pk.parse::<u64>().ok()) {
            statuses.insert(
                pk.to_string(),
                json!({
                    "following": state.follows(viewer, pk),
                    "outgoing_request": state.requested(viewer, pk),
                    "is_private": state.account(pk).private,
                }),
            );
        }
        if !is_the_apps(request) {
            state.statuses.push(ids);
        }
        json_answer(json!({ "friendship_statuses": statuses, "status": "ok" }))
    }
}

/// The friendship lists.
struct Lists(World);

impl Respond for Lists {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Some(viewer) = self.0.viewer_of_request(request) else {
            return login_required();
        };
        let segments: Vec<&str> = request.url.path().split('/').collect();
        let (Some(pk), Some(kind)) = (
            segments.get(4).and_then(|s| s.parse::<u64>().ok()),
            segments.get(5),
        ) else {
            return wrong("not a list");
        };
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let mut state = self.0.state();
        if !state.may_see(viewer, pk) {
            return ResponseTemplate::new(400).set_body_raw(
                r#"{"message":"Not authorized to view user","status":"fail"}"#,
                "application/json",
            );
        }
        let (rows, size) = match *kind {
            "followers" => {
                state
                    .search_surfaces
                    .push(query.get("search_surface").cloned());
                (state.followers(pk), query.get("count"))
            }
            "following" => (state.following(pk), query.get("count")),
            _ => {
                let followers = state.followers(pk);
                let mutual = state
                    .following(viewer)
                    .into_iter()
                    .filter(|f| followers.contains(f))
                    .collect();
                (mutual, query.get("page_size"))
            }
        };
        let size: usize = size.and_then(|s| s.parse().ok()).unwrap_or(12).max(1);
        let offset: usize = query
            .get("max_id")
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        let first = !query.contains_key("max_id");
        if !first {
            state
                .later_claims
                .push(header(request, "x-ig-www-claim").unwrap_or_default().into());
        }
        let mut page = Vec::new();
        let mut next = offset;
        for slot in 0..size {
            let repeat = state
                .repeat_every
                .is_some_and(|n| offset > 0 && n > 0 && (slot + 1) % n == 0);
            if repeat {
                page.push(rows[0]);
            } else if let Some(pk) = rows.get(next) {
                page.push(*pk);
                next += 1;
            } else {
                break;
            }
        }
        let users: Vec<Value> = page
            .iter()
            .map(|pk| {
                let p = state.account(*pk);
                json!({
                    "pk": p.pk,
                    "pk_id": p.pk.to_string(),
                    "username": p.username,
                    "full_name": p.full_name,
                    "is_private": p.private,
                    "is_verified": false,
                    "profile_pic_url": state.media(&format!("{}-small", p.pk)),
                })
            })
            .collect();
        let mut body = json!({ "users": users, "big_list": next < rows.len(), "status": "ok" });
        if next < rows.len() {
            body["next_max_id"] = json!(next.to_string());
        }
        let mut answer = json_answer(body);
        if first {
            answer = answer.insert_header("x-ig-set-www-claim", CLAIM);
        }
        answer
    }
}

/// How many posts a page of the grid holds: the app's twelve.
const POSTS_PER_PAGE: usize = 12;

/// The pk of every post is `FIRST_POST + owner * POST_STRIDE + n`, so the
/// owner and the post are read back from it.
const FIRST_POST: u64 = 3_800_000_000_000_000_000;
const POST_STRIDE: u64 = 1_000;

/// How many comments every post has: two pages of them.
const COMMENTS: u64 = 20;

/// How many comments a page of them holds.
const COMMENTS_PER_PAGE: u64 = 15;

/// The pk of account `owner`'s post `n`.
pub fn post_pk(owner: u64, n: u64) -> u64 {
    FIRST_POST + owner * POST_STRIDE + n
}

/// The code a post's address carries: its pk in base64url, eleven
/// characters, as Instagram writes it.
pub fn code_of(pk: u64) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    (0..11)
        .rev()
        .map(|i| ALPHABET[((u128::from(pk) >> (6 * i)) & 63) as usize] as char)
        .collect()
}

/// A post and its comments, as the app reads them when a post opens: by
/// pk, with the REST family's headers, the comments a page of fifteen at a
/// time by the `min_id` the page before handed out.
struct Posts(World);

impl Respond for Posts {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Some(viewer) = self.0.viewer_of_request(request) else {
            return login_required();
        };
        if header(request, "x-requested-with") != Some("XMLHttpRequest")
            || header(request, "x-ig-app-id").is_none()
        {
            return wrong("not the app's REST headers");
        }
        let segments: Vec<&str> = request.url.path().split('/').collect();
        let (Some(media), Some(kind)) = (
            segments.get(4).and_then(|s| s.parse::<u64>().ok()),
            segments.get(5),
        ) else {
            return wrong("not a post");
        };
        let query: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
        let state = self.0.state();
        let Some(post) = state.post(media, viewer) else {
            return ResponseTemplate::new(400).set_body_raw(
                r#"{"message":"Media not found or unavailable","status":"fail"}"#,
                "application/json",
            );
        };
        if *kind == "info" {
            if !query.is_empty() {
                return wrong("the info read takes no query");
            }
            return json_answer(json!({ "items": [post], "num_results": 1, "status": "ok" }));
        }
        let keys: Vec<&str> = query.iter().map(|(k, _)| k.as_str()).collect();
        let offset = match keys.as_slice() {
            ["can_support_threading", "permalink_enabled"] => 0,
            ["can_support_threading", "min_id", "sort_order"] => {
                let cursor: Value = serde_json::from_str(&query[1].1).unwrap_or(Value::Null);
                match cursor
                    .get("cached_comments_cursor")
                    .and_then(Value::as_str)
                    .and_then(|n| n.parse::<u64>().ok())
                {
                    Some(offset) => offset,
                    None => return wrong("a cursor this post never handed out"),
                }
            }
            _ => return wrong(format!("not the app's comments query: {keys:?}")),
        };
        let end = (offset + COMMENTS_PER_PAGE).min(COMMENTS);
        let comments: Vec<Value> = (offset..end)
            .map(|n| {
                json!({
                    "pk": format!("1800000000000{n:04}"),
                    "text": format!("comment {n}"),
                    "created_at": 1_790_000_100 + n as i64,
                    "user": { "pk": (FIRST_USER + n % USERS).to_string(), "username": format!("user{}", n % USERS) },
                    "comment_like_count": n,
                    "child_comment_count": if n == 0 { 3 } else { 0 },
                    "preview_child_comments": if n == 0 {
                        json!([{ "pk": "1", "text": "a reply", "user": { "username": "user4" } }])
                    } else {
                        json!([])
                    },
                })
            })
            .collect();
        let mut body = json!({
            "comments": comments,
            "comment_count": COMMENTS,
            "has_more_comments": false,
            "has_more_headload_comments": end < COMMENTS,
            "status": "ok",
        });
        if end < COMMENTS {
            body["next_min_id"] = json!(
                json!({ "cached_comments_cursor": end.to_string(), "bifilter_token": "invented" })
                    .to_string()
            );
        }
        json_answer(body)
    }
}

/// The REST reads the client sends without a browser.
struct Legacy(World);

impl Respond for Legacy {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Some(viewer) = self.0.viewer_of_request(request) else {
            return login_required();
        };
        let path = request.url.path();
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let state = self.0.state();
        let segments: Vec<&str> = path.split('/').collect();
        if path == "/api/v1/users/web_profile_info/" {
            let Some(person) = query.get("username").and_then(|n| state.named(n)) else {
                return ResponseTemplate::new(404)
                    .set_body_raw(r#"{"message":"","status":"fail"}"#, "application/json");
            };
            return json_answer(
                json!({ "data": { "user": state.web_profile_info(person.pk, viewer) }, "status": "ok" }),
            );
        }
        if path.starts_with("/api/v1/users/") {
            let pk: u64 = segments[4].parse().unwrap_or_default();
            let person = state.account(pk);
            return json_answer(json!({ "user": {
                "pk": pk,
                "username": person.username,
                "full_name": person.full_name,
                "hd_profile_pic_url_info": { "url": state.media(&format!("{pk}-full")), "width": 1080, "height": 1080 },
            }, "status": "ok" }));
        }
        if path.starts_with("/api/v1/highlights/") {
            let pk: u64 = segments[4].parse().unwrap_or_default();
            let tray: Vec<Value> = if state.may_see(viewer, pk) {
                state
                    .account(pk)
                    .highlights
                    .iter()
                    .map(|h| {
                        json!({
                            "id": format!("highlight:{}", h.id),
                            "title": h.title,
                            "media_count": h.items.len(),
                            "created_at": h.items.first().map(|i| i.taken_at),
                            "updated_timestamp": h.items.last().map(|i| i.taken_at),
                            "cover_media": { "cropped_image_version": { "url": state.media(&h.id) } },
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            return json_answer(json!({ "tray": tray, "status": "ok" }));
        }
        let reels: Vec<Value> = query
            .get("reel_ids")
            .map(|ids| {
                ids.split(',')
                    .filter_map(|id| state.reel(id, viewer, false))
                    .collect()
            })
            .unwrap_or_default();
        json_answer(json!({ "reels_media": reels, "status": "ok" }))
    }
}

impl State {
    /// Account `pk` as `web_profile_info` describes it to `viewer`.
    fn web_profile_info(&self, pk: u64, viewer: u64) -> Value {
        let person = self.account(pk);
        let own = pk == viewer;
        let followers = self.followers(pk);
        let mutual: Vec<u64> = self
            .following(viewer)
            .into_iter()
            .filter(|f| !own && followers.contains(f))
            .collect();
        json!({
            "id": pk.to_string(),
            "username": person.username,
            "full_name": person.full_name,
            "is_private": person.private,
            "is_verified": false,
            "followed_by_viewer": self.follows(viewer, pk),
            "requested_by_viewer": self.requested(viewer, pk),
            "follows_viewer": self.follows(pk, viewer),
            "has_requested_viewer": self.requested(pk, viewer),
            "profile_pic_url": self.media(&format!("{pk}-small")),
            "profile_pic_url_hd": self.media(&format!("{pk}-hd")),
            "edge_followed_by": { "count": followers.len() },
            "edge_follow": { "count": self.following(pk).len() },
            "edge_owner_to_timeline_media": { "count": person.posts },
            "edge_mutual_followed_by": {
                "count": mutual.len(),
                "edges": mutual.iter().take(3).map(|pk| json!({ "node": { "username": self.account(*pk).username } })).collect::<Vec<_>>(),
            },
            "highlight_reel_count": person.highlights.len(),
            "biography": format!("the invented bio of {}", person.username),
        })
    }
}
