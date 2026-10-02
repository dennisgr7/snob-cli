//! Downloading a story never marks it as seen.
//!
//! `snob stories` reads a reel and downloads media from the CDN. Registering a
//! view is a **separate** call that Instagram's own clients make explicitly,
//! and this project makes no calls that write. The consequence belongs to the
//! person whose story it is rather than to the person running the tool — their
//! viewer list stays a record of people who opened the story in the app — and
//! it is the one property here that nobody running the tool would ever notice
//! being broken.
//!
//! It is easy to break by accident and impossible to see. Adding `media/seen`
//! to the client is four lines that compile, pass every test, and read as an
//! obvious courtesy — mark what was viewed as viewed — while quietly turning a
//! read into a write. So the check reads the source: if any spelling of the
//! call appears in any `.rs` file of the repository, this fails and says where.
//!
//! # This is a backstop, and it is not the guarantee
//!
//! **A list of spellings cannot hold this line, and it is worth being honest
//! about why.** A browser capture in August 2026 showed what Instagram's own
//! client actually sends when a story is viewed: not `media/seen`, but the
//! GraphQL mutation `PolarisStoriesV3SeenMutation`, whose identifier on the
//! wire is `doc_id=26234228992942885`. A number. No list of English words
//! contains a number, and extending the list every time Meta renames something
//! is a race this file loses.
//!
//! What actually holds the line is in `snob-ig`: `IgClient::post` takes a
//! `graphql::Mutation` rather than a path, and that enum has two variants whose
//! absence of a wildcard arm makes a third a compile error. This file catches
//! the earlier mistake — a helper, a constant, a URL written before the request
//! exists — which is the case its own planted-call test is about. Both are
//! worth having. Only one of them is structural.
//!
//! A source check rather than a runtime one, for the reason `tests/keyring.rs`
//! and `tests/sandbox.rs` both give: an integration test compiles the library
//! without `cfg(test)`, so an assertion inside the process is blind in exactly
//! the files that matter. There is also nothing to assert *about* — the defect
//! being guarded against is the presence of code, not the behavior of code that
//! is there.
//!
//! # Not only stories
//!
//! A capture of the web client in September 2026 showed it telling Instagram
//! what its user looked at in more ways than one: the activity feed and the
//! suggested accounts marked seen, view counts, per-post dwell and reels
//! watched carried inside ordinary reads, recent searches, time spent, and a
//! story like. Each is a write about the person running the tool, or about
//! the person whose content it is, and none is anything snob does. Their
//! spellings are here too. What holds the line for the page is the allowlist
//! in `snob-ig` (`allowlist.rs`), which lets out only the calls it names.
//!
//! **This file is exempt from itself.** It names every spelling in the list
//! below, which is the whole point of it.

mod common;
use common::{relative, repo_root, source_files};

/// Every spelling of "tell them I looked".
///
/// `media/seen` is the endpoint the mobile API uses; `reels/seen` is the same
/// thing on a reel; `mark_seen` and `mark_as_seen` are what somebody would name
/// the function before they wrote the path, and the function usually arrives
/// before the URL does.
///
/// The last two are what the **web** client sends, which is the surface this
/// program is on and was therefore the gap that mattered: `SeenMutation` is the
/// Relay operation, and the seventeen digits are the `doc_id` it travels as.
/// Both were read off a real browser session in August 2026. The number is here
/// with no illusions about what it buys -- it catches a copied line, and it
/// catches nothing else, which is the point made at the top of this file.
///
/// The rest are what the web client sent on its own in September 2026: the
/// activity feed marked seen, the suggested accounts marked seen, the view
/// count of a video, the dwell and watched-reels fields inside feed and reels
/// reads, the ads-pool view state, the recent-search write, the time-spent
/// logger, and the two numbers of the story like and unlike.
///
/// The next two are the messages' read receipts, sent by the web client in a
/// recording of 2026-10-01 when a thread was opened: marking a thread as read
/// tells its other side the message was seen, which is a seen signal like any
/// other.
///
/// The last two are what the web client sends to share a post, from the same
/// recording: the link it mints for copying, which carries a token that ties
/// whoever opens it to the account that shared it, and the share sheet's
/// reads. Reading a post or a reel needs neither, and `snob post` drops that
/// token from a link it is given rather than send it.
const SEEN: [&str; 21] = [
    "media/seen",
    "reels/seen",
    "mark_seen",
    "mark_as_seen",
    "MarkSeen",
    "SeenMutation",
    "26234228992942885",
    "inbox_seen",
    "mark_su_seen",
    "unified_cvc",
    "feed_view_info",
    "seen_reels",
    "client_view_state_media_list",
    "RegisterInRecentSearches",
    "ScreenTimeLogger",
    "26938887309082050",
    "26510485515280697",
    "useIGDMarkThreadAsReadMutation",
    "useIGDMarkThreadAsReadValidationMutation",
    "share_to_app",
    "ShareSheetV3",
];

/// This file, which names all of them.
const EXEMPT: [&str; 1] = ["crates/snob-core/tests/no_seen.rs"];

#[test]
fn nothing_tells_instagram_what_was_seen() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root, &["rs"]) {
        let name = relative(&root, &file);
        if EXEMPT.contains(&name.as_str()) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        violations.extend(mentions_in(&name, &contents));
    }

    assert!(
        violations.is_empty(),
        "{} line(s) would tell somebody their story was viewed:\n{}\n\n\
         Reading a reel does not register a view. Sending one of these does, \
         and AGENTS.md forbids it.",
        violations.len(),
        violations.join("\n")
    );
}

/// The walk over one file's text, so a test can hand it a fixture.
///
/// Split out for the reason the sibling guards give about their own: a guard
/// that walks the tree and finds nothing looks exactly like a guard that walks
/// nothing, and the two have to be told apart by something.
fn mentions_in(name: &str, contents: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        for spelling in SEEN {
            if line.contains(spelling) {
                found.push(format!("{name}:{}: {}", index + 1, line.trim()));
            }
        }
    }
    found
}

/// The guard finds what it is looking for when it is there.
///
/// Without this, a typo in `SEEN` — or a walk that reads no files — is a test
/// that passes for ever while watching nothing, which is the failure mode every
/// source-reading guard in this directory shares.
#[test]
fn the_guard_sees_the_call_it_exists_to_stop() {
    let planted = r#"
        // An innocent-looking courtesy.
        self.post("/api/v1/media/seen/", &fields, referer).await?;
    "#;
    let caught = mentions_in("crates/snob-ig/src/client.rs", planted);
    assert_eq!(
        caught.len(),
        1,
        "the guard missed a planted call: {caught:?}"
    );
    assert!(caught[0].contains("client.rs:3"));

    assert!(
        mentions_in("x.rs", "let seen = already_downloaded.contains(&pk);").is_empty(),
        "the word \"seen\" on its own is not the call, and must not be reported"
    );

    // The spelling the web client actually uses, which is the one the list was
    // missing until a capture showed it. Planted in both of its forms, because
    // whoever adds this will have copied one or the other.
    for planted in [
        r#"        Self::Seen => "PolarisStoriesV3SeenMutation","#,
        r#"        Self::Seen => "26234228992942885","#,
    ] {
        assert_eq!(
            mentions_in("crates/snob-ig/src/graphql.rs", planted).len(),
            1,
            "the guard missed the mutation a browser really sends: {planted}"
        );
    }
}

/// Each spelling the web client was seen sending on its own, planted the way
/// it would arrive: a path, a field copied into a form, an operation's name
/// or number.
#[test]
fn the_guard_sees_what_the_web_client_sends_on_its_own() {
    for planted in [
        r#"    self.post("/api/v1/news/inbox_seen/", &[], "").await?;"#,
        r#"    const SUGGESTED: &str = "/api/v1/web/discover/mark_su_seen/";"#,
        r#"    let url = base.join("/video/unified_cvc/")?;"#,
        r#"    form.push(("feed_view_info", dwell));"#,
        r#"    variables["seen_reels"] = watched;"#,
        r#"    variables["client_view_state_media_list"] = shown;"#,
        r#"    Self::Recent => "usePolarisRegisterInRecentSearchesMutation","#,
        r#"    Self::Time => "PolarisScreenTimeLogger_syncMutation","#,
        r#"    Self::Like => "26938887309082050","#,
        r#"    Self::Unlike => "26510485515280697","#,
        r#"    Self::Read => "useIGDMarkThreadAsReadMutation","#,
        r#"    Self::Checked => "useIGDMarkThreadAsReadValidationMutation","#,
    ] {
        assert_eq!(
            mentions_in("crates/snob-ig/src/client/write.rs", planted).len(),
            1,
            "the guard missed a write the web client sends: {planted}"
        );
    }
}
