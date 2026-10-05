//! The CDN client: where a picture or a video is fetched from, with no account
//! behind it.
//!
//! The CDN is a different host from the API, under the opposite redirect rule:
//! an asset request has to be allowed to move between CDN hosts, and an API
//! call must not leave instagram.com. That is why there are two policies and
//! two clients.
//!
//! **A [`CdnClient`] holds no session and no budget.** It is built from a
//! User-Agent, the address of the site it is judged against and a cancel token,
//! so nothing identifying can travel from it (no cookie, no app id) and nothing
//! is charged against Instagram's budget. The address of a file still comes out
//! of an authenticated answer (the profile query, a post's info), which is what
//! this does not replace; fetching the bytes of an address already known needs
//! no account.
//!
//! [`super::IgClient`] holds one and sends a download through it when the tab
//! does not fetch the file (see [`super::media`]).

use url::Url;

use crate::client_hints::ClientHints;
use crate::error::IgError;
use crate::pace::CancelToken;

use super::transport::{MAX_HOPS, build_cdn_client, read_capped_bytes, stream_capped};

/// Ceiling on a downloaded asset. A profile picture tops out at 1080x1080 and
/// lands far below this; the cap exists so that a redirect to something else
/// cannot make us read until memory runs out.
pub(super) const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;

/// Whether a URL is somewhere a profile picture actually comes from: the
/// rule is [`crate::allowlist::refused_asset`]'s, which the tab holds a fetch
/// to as well.
pub(super) fn serves_pictures(base: &Url, url: &Url) -> bool {
    crate::allowlist::refused_asset(base.as_str(), url.as_str()).is_none()
}

/// [`CdnClient::check_downloadable`], for a caller that has the site's address
/// and no reason to build a client: the tab's path judges the address before
/// it asks the tab, and only needs the `CdnClient` when the tab cannot fetch.
pub(super) fn check_downloadable(base: &Url, url: &Url) -> Result<(), IgError> {
    if serves_pictures(base, url) {
        return Ok(());
    }
    Err(IgError::Unexpected {
        status: 0,
        body: format!(
            "the picture URL points somewhere pictures do not come from: {}",
            url.host_str().unwrap_or("nowhere")
        ),
    })
}

/// Redirects for an asset: every hop held to the same rule as the first.
///
/// The picture URL comes out of Instagram's own answer, so a redirect chain is
/// the one place where a response gets to choose where the next request goes;
/// checking only the address as written would leave the later hops unjudged.
pub(super) fn cdn_policy(base: Url) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_HOPS {
            attempt.error("too many redirects")
        } else if serves_pictures(&base, attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("a redirect tried to leave the CDN")
        }
    })
}

/// Downloads from Instagram's CDN, carrying nothing that identifies an
/// account.
pub struct CdnClient {
    http: reqwest::Client,
    /// The site the address is judged against: its own address is the one
    /// exception to "a CDN host", matched whole.
    base: Url,
    /// The browser's, for the reason [`CdnClient::fetch`] gives.
    accept_encoding: &'static str,
    cancel: CancelToken,
}

impl CdnClient {
    /// A client that sends `user_agent` and nothing else of its own.
    ///
    /// The User-Agent is the one the account's browser has: it is what makes a
    /// CDN answer as it would to that browser, and it names nobody.
    pub fn new(user_agent: &str, base: Url, cancel: CancelToken) -> Result<Self, IgError> {
        Ok(Self {
            http: build_cdn_client(user_agent, cdn_policy(base.clone()))?,
            accept_encoding: ClientHints::from_user_agent(user_agent).accept_encoding,
            base,
            cancel,
        })
    }

    /// Downloads a public asset, such as a profile picture.
    ///
    /// These live on the CDN, a different host from the API, so the URL arrives
    /// absolute. Nothing identifying travels with it: no session cookie, no app
    /// id. The CDN is a third party and has no business seeing either, and the
    /// asset is public anyway.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, IgError> {
        self.download_capped(url, MAX_ASSET_BYTES).await
    }

    /// The same, with the caller naming the ceiling.
    ///
    /// Public because a story video does not fit under [`MAX_ASSET_BYTES`],
    /// which was sized for a 1080x1080 picture: rather than raising that
    /// constant -- and with it the ceiling on every profile picture, for a
    /// reason that has nothing to do with profile pictures -- the caller that
    /// needs a different cap says so, and says why where it says it.
    /// Everything else is the same for every caller,
    /// [`CdnClient::check_downloadable`] included: the URL still has to point
    /// at the CDN, and every redirect hop after it is held to the same rule.
    pub async fn download_capped(&self, url: &str, cap: usize) -> Result<Vec<u8>, IgError> {
        let response = self.fetch(url).await?;
        // Raced against the token like the API read, for the same reason: a
        // CDN that answers with headers and then stalls holds this process for
        // as long as it likes.
        tokio::select! {
            biased;
            () = self.cancel.canceled() => Err(IgError::Canceled),
            bytes = read_capped_bytes(response, cap as u64) => bytes,
        }
    }

    /// [`CdnClient::download_capped`], written to `sink` as it arrives rather
    /// than held in memory first.
    ///
    /// For a story or a reel video. `download_capped` returns the whole body as
    /// a `Vec`, which holds a forty-megabyte clip in memory whose only
    /// destination is a file. This hands each chunk to the sink and keeps
    /// nothing, so the peak is one chunk whatever the size.
    ///
    /// What comes back is [`Downloaded`]: the byte count and the first few
    /// bytes, because the file's extension is decided from its magic number
    /// and a caller that streamed everything to disk no longer has them.
    ///
    /// On any error the sink holds a prefix of the file; whoever owns the file
    /// removes it.
    pub async fn download_to(
        &self,
        url: &str,
        cap: usize,
        sink: &mut (impl std::io::Write + Send),
    ) -> Result<Downloaded, IgError> {
        let mut head = Vec::new();
        let mut tee = Tee {
            head: &mut head,
            sink,
        };
        let response = self.fetch(url).await?;
        let len = tokio::select! {
            biased;
            () = self.cancel.canceled() => Err(IgError::Canceled),
            n = stream_capped(response, cap as u64, &mut tee) => n,
        }?;
        Ok(Downloaded { len, head })
    }

    /// Refuses a picture URL that does not go where a picture goes.
    ///
    /// The address of the first hop comes straight out of Instagram's answer,
    /// so it is the one an attacker gets to choose: a `profile_pic_url` of
    /// `http://127.0.0.1:9222/json` or of a cloud metadata address would be
    /// fetched as written. It is held to the same rule [`cdn_policy`] holds
    /// every hop after it to, which is the point of the rule being one
    /// function.
    pub fn check_downloadable(&self, url: &Url) -> Result<(), IgError> {
        check_downloadable(&self.base, url)
    }

    /// The GET behind every download: checked, unpaced, and refused on a
    /// status the CDN's own terms explain.
    async fn fetch(&self, url: &str) -> Result<reqwest::Response, IgError> {
        let url = Url::parse(url)?;
        self.check_downloadable(&url)?;
        tracing::debug!(%url, "GET asset");

        // Deliberately not paced: the CDN is a different host with its own
        // limits, and charging a picture against Instagram's budget would make
        // the number mean two things at once.
        //
        // `Accept-Encoding` is the browser's, for the same reason it is on the
        // API request and not for a different one. This request sends no header
        // of its own, so reqwest inserted the string it assembles from whichever
        // decoders were compiled in — `zstd,gzip,deflate,br`, which no browser
        // has ever sent — under a User-Agent that says Chrome. Everything else
        // about this client is deliberately unlike the API one; this is not one
        // of those things.
        let request = self
            .http
            .get(url)
            .header("Accept-Encoding", self.accept_encoding);
        // `biased`, and the cancel branch answers `Canceled`: see
        // `IgClient::send_or_cancel`, which this is the CDN's copy of.
        let response = tokio::select! {
            biased;
            () = self.cancel.canceled() => return Err(IgError::Canceled),
            response = request.send() => response?,
        };
        let status = response.status();

        // Deliberately not `classify`: that reads Instagram's API vocabulary,
        // and the CDN does not speak it. Its 403 means the signed link has
        // expired, not that the session died, and saying otherwise would send
        // someone to log in again over a stale URL.
        if !status.is_success() {
            return Err(IgError::Unexpected {
                status: status.as_u16(),
                body: "the picture could not be downloaded".into(),
            });
        }
        Ok(response)
    }
}

/// What a streamed download leaves the caller with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    /// Bytes written to the sink.
    pub len: u64,
    /// The first bytes of the file, up to [`HEAD_BYTES`]: enough for the magic
    /// number that decides the extension, retained because the rest went
    /// straight to disk.
    pub head: Vec<u8>,
}

/// How many leading bytes [`Downloaded::head`] keeps. Twelve is what the
/// WebP check needs (`RIFF....WEBP`); sixteen leaves room.
pub const HEAD_BYTES: usize = 16;

/// Copies the first [`HEAD_BYTES`] aside and forwards everything to the sink.
struct Tee<'a, W: std::io::Write> {
    head: &'a mut Vec<u8>,
    sink: &'a mut W,
}

impl<W: std::io::Write> std::io::Write for Tee<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // The sink writes first, and the head copies only what it accepted:
        // after a short write `write_all` retries with `&buf[n..]`, and
        // copying first would put the same bytes into the head twice.
        let written = self.sink.write(buf)?;
        let room = HEAD_BYTES.saturating_sub(self.head.len());
        if room > 0 {
            self.head.extend_from_slice(&buf[..written.min(room)]);
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::UA;

    fn standalone(server: &MockServer) -> CdnClient {
        CdnClient::new(
            UA,
            Url::parse(&server.uri()).unwrap(),
            CancelToken::default(),
        )
        .unwrap()
    }

    /// The point of the type: it is built from a User-Agent and nothing else,
    /// and what it sends names no account.
    #[tokio::test]
    async fn a_cdn_client_downloads_with_no_account_at_all() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF]))
            .mount(&server)
            .await;

        let bytes = standalone(&server)
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap();
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF]);

        let requests = server.received_requests().await.unwrap();
        let headers = &requests[0].headers;
        assert!(headers.get("cookie").is_none());
        assert!(headers.get("x-ig-app-id").is_none());
        assert_eq!(headers.get("user-agent").unwrap().to_str().unwrap(), UA);
    }

    /// A Ctrl+C already given sends nothing, and says so as a cancel rather
    /// than as a network error, which would be retried.
    #[tokio::test]
    async fn a_download_after_ctrl_c_sends_nothing() {
        let server = MockServer::start().await;
        let cancel = CancelToken::default();
        cancel.cancel();
        let cdn = CdnClient::new(UA, Url::parse(&server.uri()).unwrap(), cancel).unwrap();

        let error = cdn
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// The rule holds with no client of Instagram's behind it.
    #[tokio::test]
    async fn a_cdn_client_refuses_an_address_off_the_cdn() {
        let server = MockServer::start().await;
        let cdn = standalone(&server);
        for refused in [
            "https://evil.test/p.jpg",
            "http://scontent.cdninstagram.com/p.jpg",
            "https://evilcdninstagram.com/p.jpg",
        ] {
            let error = cdn.download(refused).await.unwrap_err();
            assert!(matches!(error, IgError::Unexpected { .. }), "{refused}");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
