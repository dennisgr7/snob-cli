//! The tab's guard: what the browser pauses on its way out, and the verdict
//! on each.
//!
//! **Why.** `snob_ig::allowlist` refuses what snob would send that it does
//! not name, before it leaves. Instagram's app runs on every document the tab
//! loads and sends calls of its own from it — promotions, message badges, and
//! the signals that tell somebody what was seen among them — which snob's own
//! path never sees. So the browser pauses every call to the API, the app's
//! and snob's alike, and each is judged here before it leaves: a GET goes on,
//! and a POST goes on only when [`refused`] lets it, the same check snob's own
//! requests pass. A call refused is failed as a content blocker fails one
//! (`cdp::connection`), and said in the log.
//!
//! **The app's own reads go on, by name and number.** Its boot calls ask for
//! the promotions, the message badge's counts and what a profile says of its
//! account, its posts and the accounts it suggests among them;
//! `snob_ig::allowlist::refused_app` names the few of them that read and write
//! nothing, the posts under any number their call vouches for since it
//! rotates, and everything else the app POSTs is refused as before.
//! Why each is in or out, and why the beacons (`/ajax/bz`, `/ajax/qm/`) stay
//! out, is written at `APP_READS`. The numbers of the registry's reads follow
//! the app when it moves them ([`Rediscovered`]).
//!
//! **One exception, named.** The app's empty POST to [`CLAIM_HANDED_OUT`] is
//! let through though the allowlist does not name it (snob never sends it):
//! it is neither a write nor a seen signal, and its answer is the only one
//! that hands out the `X-IG-WWW-Claim` the app, and snob after it, echo on
//! every REST read. Without it every read would carry the claim `0`. With a
//! body, it is judged like any other POST.
//!
//! **Beyond the API**, three paths are paused though no call snob makes is
//! under them: the view-count path (`/video/`, never spelled out here), the
//! app's event log, and the sync it runs with Facebook, whose hosts are the
//! only ones the patterns name. Everything else has no host, so a sandbox's
//! fake is judged the same way.
//!
//! **What it cannot see**: the app's realtime socket, and a route the app
//! takes inside the page, which is not a request. snob never clicks, and the
//! tab goes only where the allowlist lets it.
//!
//! **And no video reaches the page.** The feed plays a video that scrolls
//! into view, muted, on its own, and Instagram counts a play of a reel from
//! its start. Measured on Chromium 153, a muted video plays under every
//! `--autoplay-policy`; what holds is the video never arriving. By address as
//! well as by kind, since the site's player fetches its video in pieces with
//! `fetch()`: a file named `.mp4`, `.webm` or `.m3u8` followed by a query, as
//! every piece on the CDN is. The query is what keeps a name out of it —
//! `clips.mp4` is a username snob may ask about — and `\?` is a literal `?`
//! in the protocol's patterns, where a bare one is any character.

use base64::Engine as _;
use serde_json::{Value, json};
use snob_ig::allowlist::{Operation, Rediscovered, refused_app};
use snob_ig::client::page::{Method, PageRequest};
use url::Url;

use crate::cdp::Paused;

/// The one call of the app's let through that the allowlist does not name,
/// when its body is empty. The capture notes give only its last two
/// segments, so the prefix is among the checks nobody has run yet
/// (AGENTS.md): matched exactly, a wrong one leaves every read on claim `0`.
const CLAIM_HANDED_OUT: &str = "/api/v1/web/fxcal/ig_sso_users/";

/// Why a call to Facebook is refused.
const FACEBOOK: &str = "the app's sync with Facebook";

/// Every piece of video the site would load, feed and reels and stories
/// alike.
pub(super) fn video_patterns() -> Vec<Value> {
    vec![
        json!({ "urlPattern": "*.mp4\\?*", "requestStage": "Request" }),
        json!({ "urlPattern": "*.webm\\?*", "requestStage": "Request" }),
        json!({ "urlPattern": "*.m3u8\\?*", "requestStage": "Request" }),
        json!({ "resourceType": "Media", "requestStage": "Request" }),
    ]
}

/// The one `Fetch.enable` a target gets: a second would replace it.
pub(super) fn patterns() -> Value {
    let judged = [
        // The API, the app's and snob's.
        "*/api/graphql*",
        "*/graphql/query*",
        "*/ajax/*",
        "*/api/v1/*",
        // What the app sends outside the API and snob never does.
        "*/video/*",
        "*/logging_client_events*",
        "*/sync/instagram*",
        "*://facebook.com/*",
        "*://*.facebook.com/*",
    ];
    let mut patterns = video_patterns();
    patterns.extend(
        judged
            .into_iter()
            .map(|pattern| json!({ "urlPattern": pattern, "requestStage": "Request" })),
    );
    json!({ "patterns": patterns })
}

/// What becomes of one paused request, from the `Fetch.requestPaused`
/// event's `params`, with the numbers the app was seen to send the
/// registry's reads under (`found`).
pub(super) fn judge(params: &Value, found: &Rediscovered) -> Paused {
    let request = &params["request"];
    let url = request["url"].as_str().unwrap_or_default();
    if params["resourceType"] == "Media" || is_video(url) {
        return Paused::Fail(Paused::VIDEO.to_string());
    }
    let parsed = Url::parse(url).ok();
    if parsed
        .as_ref()
        .and_then(Url::host_str)
        .is_some_and(is_facebook)
    {
        return Paused::Fail(FACEBOOK.to_string());
    }
    let path = parsed
        .as_ref()
        .map_or("an address that does not parse", Url::path);
    match request["method"].as_str() {
        Some("GET") => Paused::Continue,
        Some("POST") => judge_post(request, url, path, found),
        Some(other) => Paused::Fail(format!("{other} {path}")),
        None => Paused::Fail(format!("a request with no method to {path}")),
    }
}

/// A POST, judged on the form it carries.
fn judge_post(request: &Value, url: &str, path: &str, found: &Rediscovered) -> Paused {
    let Ok(form) = form_of(request) else {
        return Paused::Fail(format!(
            "POST {path} with a form the browser did not hand over"
        ));
    };
    if path == CLAIM_HANDED_OUT && form.as_deref().is_none_or(str::is_empty) {
        return Paused::Continue;
    }
    let headers: Vec<(String, String)> = request["headers"]
        .as_object()
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let sent = PageRequest {
        method: Method::Post,
        url: url.to_string(),
        headers,
        referrer: String::new(),
        body: form,
        navigate: false,
        cap: 0,
        timeout_ms: 0,
    };
    match refused_app(&sent, found) {
        None => Paused::Continue,
        Some(what) => {
            warn_of_a_rotation(sent.body.as_deref());
            match operation_named(sent.body.as_deref()) {
                Some(name) => Paused::Fail(format!("{what} ({name})")),
                None => Paused::Fail(what),
            }
        }
    }
}

/// The operation a refused Relay form says it is, for the log: a refusal
/// read back as `POST /graphql/query` alone says nothing of whether the
/// app was kept from its feed, its tray or a beacon. The name is the page's
/// own word, so only a plain identifier of a sane length is repeated, and it
/// makes each operation a reason of its own, said once.
fn operation_named(form: Option<&str>) -> Option<String> {
    let name = url::form_urlencoded::parse(form?.as_bytes())
        .find(|(name, _)| name == "fb_api_req_friendly_name")?
        .1;
    let plain = !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    plain.then(|| name.into_owned())
}

/// The form a paused POST carries: `None` for none at all, and an error when
/// the browser says there is one and does not hand it over, which is judged
/// as the worst it could be.
pub(super) fn form_of(request: &Value) -> Result<Option<String>, ()> {
    if let Some(form) = request["postData"].as_str() {
        return Ok(Some(form.to_string()));
    }
    if let Some(entries) = request["postDataEntries"].as_array() {
        let mut bytes = Vec::new();
        for entry in entries {
            let piece = entry["bytes"].as_str().unwrap_or_default();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(piece)
                .map_err(|_| ())?;
            bytes.extend(decoded);
        }
        return String::from_utf8(bytes).map(Some).map_err(|_| ());
    }
    if request["hasPostData"].as_bool().unwrap_or(false) {
        return Err(());
    }
    Ok(None)
}

/// Says so when a refused form names one of the registry's operations under
/// another `doc_id`: the app has most likely moved to a new one, and the
/// registry's reads with it would be refused next.
fn warn_of_a_rotation(form: Option<&str>) {
    let Some(form) = form else {
        return;
    };
    let fields: Vec<(String, String)> = url::form_urlencoded::parse(form.as_bytes())
        .into_owned()
        .collect();
    let field = |name: &str| {
        fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let (Some(name), Some(doc_id)) = (field("fb_api_req_friendly_name"), field("doc_id")) else {
        return;
    };
    if let Some(operation) = Operation::named(name)
        && operation.doc_id() != doc_id
    {
        tracing::warn!(
            operation = name,
            doc_id,
            registry = operation.doc_id(),
            "the app sent one of the registry's operations under another doc_id"
        );
    }
}

/// A piece of video, by its address.
fn is_video(url: &str) -> bool {
    [".mp4?", ".webm?", ".m3u8?"]
        .iter()
        .any(|video| url.contains(video))
}

fn is_facebook(host: &str) -> bool {
    host == "facebook.com" || host.ends_with(".facebook.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "https://www.instagram.com";

    /// A call judged with no number rediscovered.
    fn judge(params: &Value) -> Paused {
        super::judge(params, &Rediscovered::default())
    }

    fn paused(method: &str, url: &str, request: Value) -> Value {
        let mut request = request;
        request["method"] = json!(method);
        request["url"] = json!(url);
        json!({ "requestId": "interception-1", "resourceType": "Fetch", "request": request })
    }

    fn relay_form(name: &str, doc_id: &str) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("fb_dtsg", "relay:token"),
                ("fb_api_req_friendly_name", name),
                ("variables", r#"{"id":"2345678901"}"#),
                ("doc_id", doc_id),
            ])
            .finish()
    }

    fn relay(name: &str, doc_id: &str) -> Value {
        paused(
            "POST",
            &format!("{SITE}/api/graphql"),
            json!({
                "headers": { "X-FB-Friendly-Name": name, "Content-Type": "application/x-www-form-urlencoded" },
                "hasPostData": true,
                "postData": relay_form(name, doc_id),
            }),
        )
    }

    fn fails(verdict: &Paused) -> bool {
        matches!(verdict, Paused::Fail(_))
    }

    #[test]
    fn a_get_goes_on() {
        for url in [
            format!("{SITE}/api/v1/friendships/2345678901/followers/?count=12"),
            format!("{SITE}/api/graphql"),
            format!("{SITE}/ajax/bz"),
        ] {
            assert_eq!(judge(&paused("GET", &url, json!({}))), Paused::Continue);
        }
    }

    #[test]
    fn a_query_of_the_registrys_goes_on() {
        let hover = Operation::HoverCard;
        assert_eq!(
            judge(&relay(hover.friendly_name(), hover.doc_id())),
            Paused::Continue
        );
    }

    #[test]
    fn the_route_definitions_go_on() {
        let form = "route_urls[0]=%2Fsomeone%2F&routing_namespace=igx_www&__a=1";
        let routes = paused(
            "POST",
            &format!("{SITE}/ajax/bulk-route-definitions/"),
            json!({ "hasPostData": true, "postData": form }),
        );
        assert_eq!(judge(&routes), Paused::Continue);
    }

    /// The app's boot reads that write nothing go on, by name and number;
    /// the beacons, the inbox, the posts and everything else it POSTs do
    /// not.
    #[test]
    fn the_apps_boot_reads_go_on_and_its_beacons_do_not() {
        for (name, doc_id) in [
            (
                "QuickPromotionSupportIGSchemaBatchFetchQuery",
                "26673487622279953",
            ),
            ("IGDBadgeCountOffMsysQuery", "27393860900250970"),
            ("PolarisProfileNoteBubbleQuery", "38260824646898178"),
        ] {
            assert_eq!(judge(&relay(name, doc_id)), Paused::Continue, "{name}");
            // Under another number it is anybody's guess.
            assert!(fails(&judge(&relay(name, "1000000000000001"))), "{name}");
        }
        for name in [
            "IGDThreadDetailQuery",
            "PolarisProfilePostsQuery",
            "PolarisProfileSuggestedUsersWithPreloadableQuery",
            "PolarisAPIGetFrCookieQuery",
        ] {
            assert!(fails(&judge(&relay(name, "28570182382647478"))), "{name}");
        }
        for path in ["/ajax/bz", "/ajax/qm/", "/ajax/navigation/"] {
            let post = paused(
                "POST",
                &format!("{SITE}{path}"),
                json!({ "hasPostData": true, "postData": "a=b" }),
            );
            assert!(fails(&judge(&post)), "{path}");
        }
    }

    /// A read the app was seen to send under a number the registry does not
    /// hold goes on under that number, the app's own call included. (What
    /// refuses it without the number is `allowlist`'s test: a refusal of a
    /// registry name under another number warns, and only the test of that
    /// warning may do it here, since a callsite first reached without its
    /// subscriber is not logged for the one that follows.)
    #[test]
    fn a_read_goes_on_under_the_number_the_app_was_seen_to_send() {
        let hover = Operation::HoverCard;
        let moved = "1000000000000777";
        let sent = relay(hover.friendly_name(), moved);
        let found = Rediscovered::of(hover, moved);
        assert_eq!(super::judge(&sent, &found), Paused::Continue);
    }

    /// A Relay call the registry does not name is refused, whatever it
    /// calls itself.
    #[test]
    fn a_relay_call_not_in_the_registry_fails() {
        let verdict = judge(&relay("SomeBadgeQuery", "1000000000000001"));
        assert_eq!(
            verdict,
            Paused::Fail("POST /api/graphql (SomeBadgeQuery)".to_string())
        );
    }

    /// A name that is not a plain identifier is not repeated into the log.
    #[test]
    fn a_refused_operation_is_named_only_when_its_name_is_plain() {
        assert_eq!(
            super::operation_named(Some("a=1&fb_api_req_friendly_name=FeedQuery_x1")),
            Some("FeedQuery_x1".to_string())
        );
        for form in [
            "fb_api_req_friendly_name=",
            "fb_api_req_friendly_name=a%0Ab",
            "fb_api_req_friendly_name=%3Cscript%3E",
            "doc_id=1",
        ] {
            assert_eq!(super::operation_named(Some(form)), None, "{form}");
        }
        assert_eq!(super::operation_named(None), None);
    }

    #[test]
    fn a_post_to_the_rest_api_fails() {
        let post = paused(
            "POST",
            &format!("{SITE}/api/v1/web/some/thing/"),
            json!({ "hasPostData": true, "postData": "a=b" }),
        );
        assert_eq!(
            judge(&post),
            Paused::Fail("POST /api/v1/web/some/thing/".to_string())
        );
    }

    /// The app's empty POST that hands out the claim goes on; with a body it
    /// is any other POST, and so is another path under the same API.
    #[test]
    fn only_the_empty_post_that_hands_out_the_claim_goes_on() {
        let url = format!("{SITE}{CLAIM_HANDED_OUT}");
        for empty in [
            json!({}),
            json!({ "hasPostData": false }),
            json!({ "hasPostData": true, "postData": "" }),
            json!({ "hasPostData": true, "postDataEntries": [] }),
        ] {
            assert_eq!(judge(&paused("POST", &url, empty)), Paused::Continue);
        }
        let with_a_body = json!({ "hasPostData": true, "postData": "a=b" });
        assert!(fails(&judge(&paused("POST", &url, with_a_body))));
        let beside = format!("{SITE}/api/v1/web/fxcal/other/");
        assert!(fails(&judge(&paused("POST", &beside, json!({})))));
    }

    #[test]
    fn the_paths_outside_the_api_fail() {
        for path in ["/video/x/", "/sync/instagram/", "/logging_client_events"] {
            let post = paused(
                "POST",
                &format!("{SITE}{path}"),
                json!({ "hasPostData": true, "postData": "a=b" }),
            );
            assert!(fails(&judge(&post)), "{path}");
        }
    }

    #[test]
    fn facebook_fails_whatever_the_method() {
        for url in [
            "https://facebook.com/instagram/login_sync/",
            "https://www.facebook.com/instagram/login_sync/",
        ] {
            for method in ["GET", "POST"] {
                assert_eq!(
                    judge(&paused(method, url, json!({}))),
                    Paused::Fail(FACEBOOK.to_string()),
                    "{method} {url}"
                );
            }
        }
        // A host that only ends in the name is somebody else's.
        let lookalike = paused("GET", "https://notfacebook.com/x", json!({}));
        assert_eq!(judge(&lookalike), Paused::Continue);
    }

    /// A form handed over in pieces is judged as the whole it makes.
    #[test]
    fn a_form_in_pieces_is_judged_whole() {
        let hover = Operation::HoverCard;
        let form = relay_form(hover.friendly_name(), hover.doc_id());
        let (first, second) = form.split_at(form.len() / 2);
        let encode = |piece: &str| base64::engine::general_purpose::STANDARD.encode(piece);
        let registry = paused(
            "POST",
            &format!("{SITE}/api/graphql"),
            json!({
                "headers": { "X-FB-Friendly-Name": hover.friendly_name() },
                "hasPostData": true,
                "postDataEntries": [ { "bytes": encode(first) }, { "bytes": encode(second) } ],
            }),
        );
        assert_eq!(judge(&registry), Paused::Continue);

        let other = relay_form("SomeBadgeQuery", "1000000000000001");
        let invented = paused(
            "POST",
            &format!("{SITE}/api/graphql"),
            json!({
                "headers": { "X-FB-Friendly-Name": "SomeBadgeQuery" },
                "hasPostData": true,
                "postDataEntries": [ { "bytes": encode(&other) } ],
            }),
        );
        assert!(fails(&judge(&invented)));
    }

    /// A form the browser says is there and does not hand over is refused,
    /// even to an address a registry call is sent to.
    #[test]
    fn a_form_not_handed_over_fails() {
        let hidden = paused(
            "POST",
            &format!("{SITE}/api/graphql"),
            json!({ "hasPostData": true }),
        );
        assert_eq!(
            judge(&hidden),
            Paused::Fail("POST /api/graphql with a form the browser did not hand over".to_string())
        );
        let unreadable = paused(
            "POST",
            &format!("{SITE}/api/graphql"),
            json!({ "hasPostData": true, "postDataEntries": [ { "bytes": "not base64!" } ] }),
        );
        assert!(fails(&judge(&unreadable)));
    }

    #[test]
    fn video_fails_by_address_and_by_kind() {
        let piece = paused(
            "GET",
            "https://scontent.cdninstagram.com/v/t50/x.m3u8?oh=1",
            json!({}),
        );
        assert_eq!(judge(&piece), Paused::Fail(Paused::VIDEO.to_string()));
        let mut media = paused("GET", "http://127.0.0.1:8080/a", json!({}));
        media["resourceType"] = json!("Media");
        assert_eq!(judge(&media), Paused::Fail(Paused::VIDEO.to_string()));
    }

    #[test]
    fn a_method_other_than_get_or_post_fails() {
        let put = paused("PUT", &format!("{SITE}/api/v1/x/"), json!({}));
        assert_eq!(judge(&put), Paused::Fail("PUT /api/v1/x/".to_string()));
    }

    /// A registry query under a `doc_id` not the registry's is refused, and
    /// said: the app has most likely moved to another.
    #[test]
    fn a_registry_name_under_another_doc_id_fails_and_warns() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let hover = Operation::HoverCard;
        let verdict = tracing::subscriber::with_default(subscriber, || {
            judge(&relay(hover.friendly_name(), "1000000000000002"))
        });
        assert!(fails(&verdict));
        let said = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        assert!(said.contains("under another doc_id"), "{said}");
        assert!(said.contains("1000000000000002"), "{said}");
    }

    /// Every pattern is at the request stage, and only the Facebook ones
    /// name a host.
    #[test]
    fn only_facebook_is_named_by_host() {
        let all = patterns();
        let patterns = all["patterns"].as_array().unwrap();
        assert!(patterns.iter().all(|p| p["requestStage"] == "Request"));
        let hosts: Vec<&str> = patterns
            .iter()
            .filter_map(|p| p["urlPattern"].as_str())
            .filter(|p| p.contains("://"))
            .collect();
        assert_eq!(hosts, ["*://facebook.com/*", "*://*.facebook.com/*"]);
    }
}
