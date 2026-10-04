//! `snob pfp`: saves an account's profile picture at the best size Instagram
//! serves over the web.
//!
//! From the browser, one request and the picture: the profile (two requests
//! the first time a name is seen), whose query carries the full-size
//! picture's address with no size beside it, and the picture itself from the
//! CDN. An account that keeps the default avatar has no picture, and nothing
//! is downloaded; a profile that names no full-size picture gets the smaller
//! one, with no lookup by id, which the page does not make. Without a browser, three: the profile, the by-id lookup
//! that carries the full-size picture, and the picture. It walks no lists and
//! stores no snapshot, so it never touches the cache the list commands share
//! — but it does answer to the same budget and the same cooldown, because
//! those count requests, not lists.

use anyhow::{Result, anyhow};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::model::{Via, WebProfileInfo};
use snob_store::paths::AccountPaths;
use snob_store::secrets::SecretStore;

use crate::app::{App, Viewer};
use crate::cli::PfpArgs;
use crate::commands::common::{self, Switched};
use crate::engine::target;
use crate::exit::ExitCode;
use crate::output::{self, Rendered};
use crate::report;
use crate::ui;

pub async fn run(args: PfpArgs, store: SecretStore, paths: &AccountPaths) -> Result<ExitCode> {
    let browses = args.browses(crate::ui::a_human_would_watch_the_listing_scroll_by());

    // No bar: three requests do not need one, and the picture goes to standard
    // output when there is no `-o`. A picture is cheap, but the rules do not
    // change with the price: the cooldown refusal is the same as the others'.
    let app = common::reader(&store, paths, args.interactive, false)?;

    // Nothing here records a cooldown. `IgClient::classify_and_record` does,
    // on the request that earned it, which is the one place that sees every
    // request. A second row within milliseconds would be read by
    // `start_cooldown` as a repeat offense inside the last day: one 429 would
    // become two strikes and four hours instead of one strike and two.
    let before = app.client().pacer().spent();
    let known = target::known_pk(&app, &args.target)?;
    let picture = fetch(app.client(), &args.target, known).await?;

    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }

    // The viewer instead of the download — the default at a terminal, the
    // decision made above.
    if browses {
        return browse(app, picture, before, &store, paths).await;
    }

    // Said before the file is written, so a smaller picture is not a caveat
    // tacked onto "Written to ...".
    picture.source.announce();

    // Worked out before the bytes are moved into the payload: the name comes
    // from what actually arrived, not from the URL.
    let extension = picture.extension();
    let username = picture.username;
    let rendered = Rendered::Bytes(picture.bytes);

    match args.output {
        // `-o -`: standard output, said, and refused on a terminal.
        Some(path) if output::is_stdout(&path) => {
            output::one_file_to_stdout(1)?;
            output::write_rendered(&rendered, None)?;
        }
        // The user named it, so replacing what is there is their call.
        Some(path) => output::write_rendered(&rendered, Some(&path))?,
        // A JPEG dumped into a terminal is unreadable noise, so on a terminal
        // it gets a name of its own — created exclusively, because that name
        // came off a server rather than out of anybody's keyboard. Down a pipe
        // it goes to standard output, which is what
        // `snob pfp someone > face.jpg` is asking for.
        None if output::Presentation::detect(None).interactive => {
            let path = output::default_path(std::path::Path::new("."), &username, extension)?;
            output::write_new(&rendered, &path)?;
        }
        None => output::write_rendered(&rendered, None)?,
    }
    Ok(ExitCode::Ok)
}

/// The viewer, as the account the command runs as and then as each account
/// it switches to, fetched again as that one. The row says what the picture
/// is; the closing line says what each account spent on it, once the
/// terminal is back.
async fn browse(
    app: Box<App>,
    picture: Picture,
    before: u32,
    store: &SecretStore,
    paths: &AccountPaths,
) -> Result<ExitCode> {
    struct Session {
        app: Box<App>,
        picture: Picture,
        before: u32,
    }
    common::switching(
        Session {
            app,
            picture,
            before,
        },
        async |s: &mut Session, note: String| {
            crate::ui::pfp::browse(&s.picture, s.app.viewer(), paths, store, note).await
        },
        async |s: &Session, to: Viewer| {
            let name = s.picture.username.clone();
            let from = s.app.viewer();
            let switched = common::switch(store, paths, from, &to, false, async |app: &mut App| {
                let known = target::known_pk(app, &name)?;
                fetch(app.client(), &name, known).await
            })
            .await?;
            Ok(match switched {
                Switched::To((app, picture)) => Switched::To(Session {
                    app,
                    picture,
                    before: 0,
                }),
                Switched::Refused(note) => Switched::Refused(note),
            })
        },
        |s: &Session, several: bool| {
            let spent = s.app.client().pacer().spent().saturating_sub(s.before);
            let whom = if several {
                format!(" as {}", s.app.viewer().label())
            } else {
                String::new()
            };
            ui::info(&format!(
                "profile picture of @{}{whom} - {}",
                printable(&s.picture.username),
                report::requests(spent)
            ));
        },
    )
    .await
}

pub struct Picture {
    /// As Instagram spells it, not as it was typed.
    pub(crate) username: String,
    url: String,
    pub(crate) bytes: Vec<u8>,
    /// Which of the two pictures arrived. The command exists for one of them.
    pub(crate) source: Source,
}

#[cfg(test)]
impl Picture {
    /// A picture the viewer's tests can hold without a fetch. Here rather
    /// than in the test, because `url` is private on purpose.
    pub(crate) fn for_tests(username: &str, bytes: Vec<u8>) -> Self {
        Self {
            username: username.into(),
            url: String::new(),
            bytes,
            source: Source::FullSize { size: None },
        }
    }
}

impl Picture {
    /// Names the file after what actually arrived rather than after the URL.
    ///
    /// `media::extension_of` is the one sniffer. Its `ftyp` arm costs nothing
    /// here: a profile picture that somehow arrives as MP4 is more honestly
    /// named `.mp4` than mislabeled `.jpg`.
    pub(crate) fn extension(&self) -> &'static str {
        crate::media::extension_of(&self.bytes)
    }
}

/// Reports the size rather than the contents: a derived one would spell out a
/// megabyte of pixels into whatever printed it.
impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picture")
            .field("username", &self.username)
            .field("url", &self.url)
            .field("bytes", &self.bytes.len())
            .field("source", &self.source)
            .finish()
    }
}

/// The network half, kept apart from the session and the filesystem so a test
/// can drive it against a mock server.
///
/// Each request pays for itself inside the client, so the API calls below
/// take a slot each. Spending several against one reservation is how a burst
/// allowance quietly stops meaning what it says.
///
/// `known` is the account's pk when this machine has seen it
/// ([`target::known_pk`]).
pub async fn fetch(
    client: &IgClient,
    typed: &str,
    known: Option<snob_core::Pk>,
) -> Result<Picture> {
    let profile = client.profile_named(target::clean(typed), known).await?;
    if profile.anonymous_picture == Some(true) {
        return Err(no_picture(&profile.username));
    }
    let full_size = full_size_of(&profile);
    picture(
        client,
        profile.id,
        &profile.username,
        profile.profile_pic_url_hd.or(profile.profile_pic_url),
        full_size,
    )
    .await
}

/// The full-size picture's address, when the profile read carries it: the
/// web client's profile query answers it (`hd_profile_pic_url_info`), with
/// no size. `web_profile_info`'s field of the same name is downscaled, so
/// on that route there is none.
pub(crate) fn full_size_of(profile: &WebProfileInfo) -> Option<String> {
    if profile.via == Via::Graph {
        profile.profile_pic_url_hd.clone()
    } else {
        None
    }
}

/// What is said for an account with no picture of its own: the default
/// avatar is not worth downloading.
pub(crate) fn no_picture(username: &str) -> anyhow::Error {
    anyhow!("@{} has no profile picture", printable(username))
}

/// The half after the name is resolved: the choice of URL and the picture
/// itself. `full_size` is the full-size address when the profile carried
/// one, and the picture is then all there is to fetch. Without it, from the
/// browser the smaller picture is all there is: the by-id lookup is not a
/// call the page makes. Without a browser, the by-id lookup is one paced
/// request more.
///
/// Split from [`fetch`] so the interactive profile view — which already paid
/// to resolve the name and holds the pk and both addresses — can look at the
/// picture without reading the profile again.
pub(crate) async fn picture(
    client: &IgClient,
    pk: snob_core::Pk,
    username: &str,
    fallback: Option<String>,
    full_size: Option<String>,
) -> Result<Picture> {
    if let Some(url) = full_size {
        let bytes = client.download(&url).await?;
        return Ok(Picture {
            username: username.to_string(),
            url,
            bytes,
            source: Source::FullSize { size: None },
        });
    }
    // Otherwise, without a browser, the full-size picture comes from the
    // by-id endpoint, so this spends a request on it. `web_profile_info` has a field named
    // `profile_pic_url_hd`, but what it hands back is a URL carrying an
    // instruction to the CDN to downscale to 320x320 — and that instruction is
    // covered by the URL's signature, so it cannot simply be stripped off.
    //
    // A failure here is not worth losing the picture over: the smaller one
    // below still works. But the reason is kept and shown rather than logged
    // at `debug!`, which nobody passes `--verbose` to see on a command that
    // appears to work -- and it can be the 429 that has just put the account
    // in cooldown, which the next command's refusal would otherwise leave
    // unexplained.
    let (full_size, why_not) = if client.has_page() {
        (
            None,
            Some("the profile named no full-size picture".to_string()),
        )
    } else {
        match client.user_info(pk).await {
            Ok(info) => (info.and_then(|i| i.hd_profile_pic_url_info), None),
            Err(e) => (None, Some(e.to_string())),
        }
    };

    // Accounts that never set a picture have none of these, and the default
    // avatar is not worth downloading.
    let (url, source) = match full_size {
        Some(p) => (
            p.url,
            Source::FullSize {
                size: p.width.zip(p.height),
            },
        ),
        None => match fallback {
            Some(url) => (url, Source::Smaller { why: why_not }),
            None => return Err(no_picture(username)),
        },
    };

    // The CDN is not Instagram's API and does not count against its budget.
    let bytes = client.download(&url).await?;
    Ok(Picture {
        username: username.to_string(),
        url,
        bytes,
        source,
    })
}

/// Which of the two pictures this is.
///
/// Not `Option<(u32, u32)>`: the by-id endpoint can answer with a picture and
/// no dimensions, which would collapse into the same `None` as not having
/// answered at all — and those are the two cases the whole command turns on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// The profile query carried it, with no size, or the by-id endpoint
    /// answered. The size is what it declared, not what was measured:
    /// nothing here decodes the image.
    FullSize { size: Option<(u32, u32)> },
    /// What the profile page offers, which is the one thing this command exists
    /// not to hand back. `why` is why there is no full-size picture: what the
    /// by-id endpoint said, when it said anything, or that the profile named
    /// none.
    Smaller { why: Option<String> },
}

impl Source {
    /// What the viewer's row calls the picture: the size when the by-id
    /// endpoint declared one, and an honest word for the fallback.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::FullSize { size: Some((w, h)) } => format!("{w}x{h}"),
            Self::FullSize { size: None } => "full size".to_string(),
            Self::Smaller { .. } => "the smaller profile-page size".to_string(),
        }
    }

    /// What to tell the user before the file is written, if anything.
    ///
    /// Said **before** the write, so it does not read as a caveat attached to
    /// "Written to picture.jpg" after the fact.
    fn announce(&self) {
        match self {
            Self::FullSize { size: Some((w, h)) } => ui::info(&format!("Full size: {w}x{h}.")),
            Self::FullSize { size: None } => {}
            Self::Smaller { why } => {
                let mut line = "the full-size picture could not be found, so this is the \
                                smaller picture the profile page serves"
                    .to_string();
                if let Some(why) = why {
                    line.push_str(&format!(" ({why})"));
                }
                ui::warn(&line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use snob_core::budget::{RateBudget, RateBudgetError};
    use snob_core::session::{Session, SessionOrigin};
    use snob_ig::pace::Pacer;
    use url::Url;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
    const SID: &str = "42%3AAbCdEfGh%3A20";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];

    /// The budget is exercised for real elsewhere; here it only has to not get
    /// in the way, and to be counted.
    #[derive(Default)]
    struct CountingBudget(std::sync::atomic::AtomicUsize);

    impl RateBudget for CountingBudget {
        fn reserve(&self) -> Result<std::time::Duration, RateBudgetError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(std::time::Duration::ZERO)
        }
        fn reserve_write(&self) -> Result<std::time::Duration, RateBudgetError> {
            self.reserve()
        }
        fn cooldown(&self) -> Result<Option<snob_core::EpochMs>, RateBudgetError> {
            Ok(None)
        }
        fn start_cooldown(
            &self,
            _reason: &str,
            _minimum: std::time::Duration,
        ) -> Result<snob_core::EpochMs, RateBudgetError> {
            Ok(snob_core::EpochMs::new(0))
        }
    }

    async fn fetch_with(server: &MockServer, target: &str) -> (Result<Picture>, usize) {
        let budget = std::sync::Arc::new(CountingBudget::default());
        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let client = IgClient::new(session, Pacer::new(budget.clone()))
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap());

        let result = fetch(&client, target, None).await;
        let reserved = budget.0.load(std::sync::atomic::Ordering::Relaxed);
        (result, reserved)
    }

    /// Mounts the profile lookup plus a picture served from the same mock, so
    /// the absolute CDN URL is reachable from the test.
    async fn profile_serving(server: &MockServer, body: String) {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .and(query_param("username", "someone"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
        for name in ["/full.jpg", "/hd.jpg", "/small.jpg"] {
            Mock::given(method("GET"))
                .and(path(name))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(JPEG.to_vec()))
                .mount(server)
                .await;
        }
    }

    /// The by-id endpoint, which is the only one carrying the full-size
    /// picture.
    async fn by_id_serving(server: &MockServer, body: String) {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/7/info/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    fn web_profile(server: &MockServer) -> String {
        format!(
            r#"{{"data":{{"user":{{"id":"7","username":"someone",
               "profile_pic_url":"{uri}/small.jpg",
               "profile_pic_url_hd":"{uri}/hd.jpg"}}}}}}"#,
            uri = server.uri()
        )
    }

    /// The whole reason for the extra request: `profile_pic_url_hd` is 320x320
    /// despite the name, and only the by-id endpoint offers 1080x1080.
    #[tokio::test]
    async fn it_takes_the_full_size_picture_over_the_one_called_hd() {
        let server = MockServer::start().await;
        profile_serving(&server, web_profile(&server)).await;
        by_id_serving(
            &server,
            format!(
                r#"{{"user":{{"username":"someone","hd_profile_pic_url_info":
                   {{"url":"{uri}/full.jpg","width":1080,"height":1080}}}}}}"#,
                uri = server.uri()
            ),
        )
        .await;

        let (picture, reserved) = fetch_with(&server, "@someone").await;
        let picture = picture.unwrap();

        assert_eq!(picture.bytes, JPEG);
        assert_eq!(picture.username, "someone");
        assert!(picture.url.ends_with("/full.jpg"), "{}", picture.url);
        // The profile, the by-id lookup, and the picture.
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        // Two of those go to Instagram's API, and each takes its own slot. The
        // CDN is somebody else's server and does not count.
        assert_eq!(reserved, 2);
    }

    /// The full-size lookup is an improvement, not a requirement: if it fails
    /// or says nothing, the smaller picture is still worth having.
    #[tokio::test]
    async fn without_the_full_size_it_falls_back_to_the_smaller_one() {
        let server = MockServer::start().await;
        profile_serving(&server, web_profile(&server)).await;
        by_id_serving(&server, r#"{"user":{"username":"someone"}}"#.into()).await;

        let picture = fetch_with(&server, "someone").await.0.unwrap();
        assert!(picture.url.ends_with("/hd.jpg"), "{}", picture.url);
        assert!(
            matches!(picture.source, Source::Smaller { .. }),
            "falling back has to be recorded, or the command says nothing about having served the one size it exists to avoid"
        );
    }

    /// A by-id endpoint that errors outright must not lose the picture either.
    #[tokio::test]
    async fn a_failing_by_id_lookup_does_not_sink_the_download() {
        let server = MockServer::start().await;
        profile_serving(&server, web_profile(&server)).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/7/info/"))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;

        let picture = fetch_with(&server, "someone").await.0.unwrap();
        assert!(picture.url.ends_with("/hd.jpg"), "{}", picture.url);
        assert!(
            matches!(picture.source, Source::Smaller { .. }),
            "falling back has to be recorded, or the command says nothing about having served the one size it exists to avoid"
        );
    }

    #[tokio::test]
    async fn an_account_with_no_picture_says_so() {
        let server = MockServer::start().await;
        profile_serving(
            &server,
            r#"{"data":{"user":{"id":"7","username":"someone"}}}"#.into(),
        )
        .await;
        by_id_serving(&server, r#"{"user":{"username":"someone"}}"#.into()).await;

        let error = fetch_with(&server, "someone").await.0.unwrap_err();
        assert!(error.to_string().contains("no profile picture"), "{error}");
    }

    /// The session must not reach the CDN. The client test covers the header
    /// itself; this one covers the path `pfp` actually takes.
    #[tokio::test]
    async fn the_picture_request_carries_no_session() {
        let server = MockServer::start().await;
        profile_serving(&server, web_profile(&server)).await;
        by_id_serving(
            &server,
            format!(
                r#"{{"user":{{"username":"someone","hd_profile_pic_url_info":
                   {{"url":"{uri}/full.jpg"}}}}}}"#,
                uri = server.uri()
            ),
        )
        .await;

        fetch_with(&server, "someone").await.0.unwrap();

        let requests = server.received_requests().await.unwrap();
        let to_cdn = requests
            .iter()
            .find(|r| r.url.path() == "/full.jpg")
            .expect("the picture was never requested");
        assert!(to_cdn.headers.get("cookie").is_none());
        // The API requests do carry it; only the CDN is kept clear.
        let to_api = requests
            .iter()
            .find(|r| r.url.path().starts_with("/api/"))
            .unwrap();
        assert!(to_api.headers.get("cookie").is_some());
    }

    #[tokio::test]
    async fn a_name_nobody_owns_is_named_in_the_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"user":null}}"#))
            .mount(&server)
            .await;

        let error = fetch_with(&server, "someone").await.0.unwrap_err();
        assert!(error.to_string().contains("someone"), "{error}");
    }

    /// Instagram's signed URLs say `.webp` while instructing the CDN to return
    /// JPEG, so the bytes are the only honest source for the name.
    #[test]
    fn the_file_is_named_after_the_bytes_not_the_url() {
        let picture = Picture {
            // Deliberately disagreeing with the bytes: the URL must not decide.
            url: "https://cdn.example/x/abc.webp?stp=dst-jpg".into(),
            ..Picture::for_tests("someone", JPEG.to_vec())
        };
        assert_eq!(picture.extension(), "jpg");
    }
}
