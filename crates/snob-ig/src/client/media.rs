//! Profile pictures and story media, and the rules about where one may come
//! from.
//!
//! The CDN is a different host from the API, under the opposite redirect rule:
//! an asset request has to be allowed to move between CDN hosts, and an API
//! call must not leave instagram.com. That is why there are two policies and
//! two clients, and why this half is not in [`super::transport`].
//!
//! Nothing identifying travels from here: no session cookie, no app id, and no
//! charge against Instagram's budget. [`super::write`] reads the mutation
//! bundles through the same path, for both of those reasons.
//!
//! **From a browser, the tab fetches them** ([`crate::web::Ask::Asset`]):
//! the browser's own TLS handshake and HTTP/2, its headers, `Sec-Fetch-*` and
//! cookie rules for the CDN host, and the page's `Referer`, as the app's own
//! images and scripts arrive. Two things stay with `reqwest`, as ever without
//! the session: a video, which the tab lets no further than it lets a play
//! (`headless::guard`), and a file past what the tab hands back over its
//! protocol ([`PAGE_ASSET_BYTES`]); and so does anything the tab could not
//! read, such as an answer the CDN gave no access to the page. Without a
//! browser, all of it does.

use url::Url;

use crate::error::IgError;
use crate::web::{Ask, Told};

use super::IgClient;
use super::transport::{MAX_HOPS, read_capped_bytes, stream_capped};

/// Ceiling on a downloaded asset. A profile picture tops out at 1080x1080 and
/// lands far below this; the cap exists so that a redirect to something else
/// cannot make us read until memory runs out.
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;

/// Whether a URL is somewhere a profile picture actually comes from: the
/// rule is [`crate::allowlist::refused_asset`]'s, which the tab holds a fetch
/// to as well.
fn serves_pictures(base: &Url, url: &Url) -> bool {
    crate::allowlist::refused_asset(base.as_str(), url.as_str()).is_none()
}

/// The most the tab hands back of one file: what travels over its protocol as
/// text, base64 making a third more of it, with room for the envelope. A
/// bigger file is `reqwest`'s.
pub(super) const PAGE_ASSET_BYTES: usize = 4 * 1024 * 1024;

/// Whether `url` is a piece of video, which the tab never lets through: the
/// guard fails one by address, since a play counts.
fn is_video(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    [".mp4", ".webm", ".m3u8"]
        .iter()
        .any(|video| path.ends_with(video))
}

/// The bytes of standard base64, as the tab hands a file back. `None` for
/// anything else.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    }
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    for (at, group) in text.chunks(4).enumerate() {
        let last = at + 1 == text.len() / 4;
        let padding = group.iter().rev().take_while(|b| **b == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut word = 0;
        for byte in &group[..4 - padding] {
            word = word << 6 | sextet(*byte)?;
        }
        word <<= 6 * padding;
        bytes.extend_from_slice(&word.to_be_bytes()[1..4 - padding]);
    }
    Some(bytes)
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

impl IgClient {
    /// Downloads a public asset, such as a profile picture.
    ///
    /// These live on the CDN, a different host from the API, so the URL arrives
    /// absolute and this does not go through [`IgClient::get`]. Nothing
    /// identifying travels with it: no session cookie, no app id. The CDN is a
    /// third party and has no business seeing either, and the asset is public
    /// anyway.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, IgError> {
        self.download_capped(url, MAX_ASSET_BYTES).await
    }

    /// The same, with the caller naming the ceiling.
    ///
    /// Refuses a picture URL that does not go where a picture goes.
    ///
    /// The address of the first hop comes straight out of Instagram's answer,
    /// so it is the one an attacker gets to choose: a `profile_pic_url` of
    /// `http://127.0.0.1:9222/json` or of a cloud metadata address would be
    /// fetched as written. It is held to the same rule [`cdn_policy`] holds
    /// every hop after it to, which is the point of the rule being one
    /// function.
    fn check_downloadable(&self, url: &Url) -> Result<(), IgError> {
        if serves_pictures(&self.base, url) {
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

    /// The body of [`IgClient::download`], with the ceiling as an argument so a
    /// test can reach it without moving eight megabytes around.
    ///
    /// Public because a story video does not fit under [`MAX_ASSET_BYTES`],
    /// which was sized for a 1080x1080 picture: rather than raising that
    /// constant -- and with it the ceiling on every profile picture, for a
    /// reason that has nothing to do with profile pictures -- the caller that
    /// needs a different cap says so, and says why where it says it.
    /// Everything else is the same for every caller,
    /// [`IgClient::check_downloadable`] included: the URL still has to point
    /// at the CDN, and every redirect hop after it is held to the same rule.
    pub async fn download_capped(&self, url: &str, cap: usize) -> Result<Vec<u8>, IgError> {
        if let Some(bytes) = self.asset_from_the_page(url, cap).await? {
            return Ok(bytes);
        }
        let response = self.fetch_asset(url).await?;
        // Raced against the token like the API read, for the same reason: a
        // CDN that answers with headers and then stalls holds this process for
        // as long as it likes.
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            bytes = read_capped_bytes(response, cap as u64) => bytes,
        }
    }

    /// [`IgClient::download_capped`], written to `sink` as it arrives rather
    /// than held in memory first.
    ///
    /// For a story video. `download_capped` returns the whole body as a `Vec`,
    /// which holds a forty-megabyte clip in memory whose only destination is a
    /// file. This hands each chunk to the sink and keeps nothing, so the peak
    /// is one chunk whatever the size.
    ///
    /// What comes back is [`Downloaded`]: the byte count and the first few
    /// bytes, because the file's extension is decided from its magic number
    /// and a caller that streamed everything to disk no longer has them.
    ///
    /// Everything else is the same as `download_capped` -- the CDN-only
    /// redirect rule, the unpaced client, the browser's `Accept-Encoding`,
    /// the ceiling, the race against Ctrl+C -- because it is the same fetch
    /// with a different place to put the bytes. On any error the sink holds a
    /// prefix of the file; whoever owns the file removes it.
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
        if let Some(bytes) = self.asset_from_the_page(url, cap).await? {
            std::io::Write::write_all(&mut tee, &bytes)
                .map_err(|e| IgError::Decode(format!("could not write the download: {e}")))?;
            return Ok(Downloaded {
                len: bytes.len() as u64,
                head,
            });
        }
        let response = self.fetch_asset(url).await?;
        let len = tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            n = stream_capped(response, cap as u64, &mut tee) => n,
        }?;
        Ok(Downloaded { len, head })
    }

    /// The file at `url`, fetched by the tab, when the tab is to fetch it:
    /// `None` leaves it to [`Self::fetch_asset`] (no browser, a video, a
    /// file past [`PAGE_ASSET_BYTES`], or an answer the tab could not read).
    ///
    /// Held to the same rule as the other path before anything is asked
    /// ([`Self::check_downloadable`]). A status that is not a success is the
    /// answer here too, never retried the other way; what is left to the
    /// other path is what the tab could not read. A push-back the browser
    /// heard, and Ctrl+C, stop the download.
    async fn asset_from_the_page(&self, url: &str, cap: usize) -> Result<Option<Vec<u8>>, IgError> {
        let url = Url::parse(url)?;
        self.check_downloadable(&url)?;
        if self.page.is_none() || is_video(&url) {
            return Ok(None);
        }
        let ask = Ask::Asset {
            url: url.to_string(),
        };
        let told = match self.ask_page(ask, "/").await {
            Ok(told) => told,
            Err(
                e @ (IgError::Canceled
                | IgError::InCooldown { .. }
                | IgError::RateLimited
                | IgError::FeedbackRequired
                | IgError::Challenge { .. }
                | IgError::Checkpoint { .. }),
            ) => return Err(e),
            Err(e) => {
                tracing::debug!(error = %e, %url, "the tab could not fetch the asset; asked directly");
                return Ok(None);
            }
        };
        let Told::Asset(answer) = told else {
            return Err(super::ask::told_otherwise("an asset"));
        };
        tracing::debug!(%url, status = answer.status, "an asset fetched by the tab");
        if answer.too_large {
            // Past what the tab hands back: the caller's own ceiling, if that
            // is the lower, and otherwise the other path's to read.
            return if cap <= PAGE_ASSET_BYTES {
                Err(IgError::TooLarge { limit: cap })
            } else {
                Ok(None)
            };
        }
        if answer.status == 0 {
            // A redirect, which the tab does not follow: the other path
            // follows it, hop by hop, over the CDN.
            return Ok(None);
        }
        if !(200..300).contains(&answer.status) {
            return Err(IgError::Unexpected {
                status: answer.status,
                body: "the picture could not be downloaded".into(),
            });
        }
        let Some(bytes) = decode_base64(&answer.body) else {
            tracing::debug!(%url, "the tab's answer was not base64; asked directly");
            return Ok(None);
        };
        if bytes.len() > cap {
            return Err(IgError::TooLarge { limit: cap });
        }
        Ok(Some(bytes))
    }

    /// The GET behind both downloads: checked, unpaced, and refused on a
    /// status the CDN's own terms explain.
    async fn fetch_asset(&self, url: &str) -> Result<reqwest::Response, IgError> {
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
        let response = self
            .send_or_cancel(
                self.cdn()?
                    .get(url)
                    .header("Accept-Encoding", self.hints.accept_encoding),
            )
            .await?;
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
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::BASE_URL;
    use crate::client::harness::{Scripted, UA, client, page_said, spending_client};
    use crate::client::page::{PageError, PageResponse, PushedBack};

    /// The whole point of a separate download path: the CDN is someone else's
    /// server, and the session must not reach it.
    #[tokio::test]
    async fn a_download_carries_nothing_identifying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF, 0xE0]))
            .mount(&server)
            .await;

        let bytes = client(&server)
            .await
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap();
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, 0xE0]);

        let requests = server.received_requests().await.unwrap();
        let headers = &requests[0].headers;
        assert!(
            headers.get("cookie").is_none(),
            "the session reached the CDN"
        );
        assert!(headers.get("x-ig-app-id").is_none());
        // The User-Agent does travel: it is on the client, and a mismatched one
        // is what makes a CDN answer differently than the browser would.
        assert_eq!(headers.get("user-agent").unwrap().to_str().unwrap(), UA);
    }

    /// Standard base64, for the tab's answers in these tests.
    fn encoded(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut text = String::new();
        for group in bytes.chunks(3) {
            let word = group
                .iter()
                .enumerate()
                .fold(0u32, |word, (at, b)| word | u32::from(*b) << (16 - 8 * at));
            for at in 0..=group.len() {
                text.push(char::from(DIGITS[(word >> (18 - 6 * at) & 63) as usize]));
            }
            text.push_str(&"=".repeat(3 - group.len()));
        }
        text
    }

    #[test]
    fn base64_is_read_back_as_written() {
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encoded(bytes), text);
            assert_eq!(decode_base64(text).as_deref(), Some(bytes), "{text}");
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode_base64(&encoded(&all)), Some(all));
        for wrong in [
            "Zg", "Zg=", "Z===", "Zm9v====", "Zg==Zm9v", "Zm 9", "Zm9\n", "Zm9v!",
        ] {
            assert_eq!(decode_base64(wrong), None, "{wrong:?}");
        }
    }

    const PICTURE: &str = "https://scontent.cdninstagram.com/v/p.jpg?oh=1";

    fn page_serving(status: u16, bytes: &[u8]) -> Arc<Scripted> {
        let body = encoded(bytes);
        Scripted::telling(move |call| {
            assert!(matches!(call.ask, Ask::Asset { .. }), "{:?}", call.ask);
            Ok(Told::Asset(page_said(status, &body)))
        })
    }

    /// With a browser, the tab fetches the picture, paid for with nothing,
    /// and what comes back is the file: the bytes, whole, whatever they are.
    #[tokio::test]
    async fn a_picture_is_fetched_by_the_tab_when_there_is_one() {
        let bytes: Vec<u8> = (0..=255).chain(0..100).collect();
        let page = page_serving(200, &bytes);
        let (client, budget) = spending_client(page.clone());

        assert_eq!(client.download(PICTURE).await.unwrap(), bytes);
        let asks = page.asks();
        assert_eq!(asks.len(), 1);
        assert_eq!(
            asks[0].ask,
            Ask::Asset {
                url: PICTURE.into()
            }
        );
        assert_eq!(asks[0].cap, PAGE_ASSET_BYTES as u64);
        assert_eq!(budget.reserved(), (0, 0), "the CDN is not paced");

        // Streamed to a sink, with the head kept for the extension.
        let mut sink = Vec::new();
        let got = client.download_to(PICTURE, 4096, &mut sink).await.unwrap();
        assert_eq!(sink, bytes);
        assert_eq!(got.len, bytes.len() as u64);
        assert_eq!(got.head, bytes[..HEAD_BYTES].to_vec());

        // Held to the same rules as the other path before the tab is asked.
        for refused in [
            "http://scontent.cdninstagram.com/v/p.jpg",
            "https://evil.test/p.jpg",
            "https://evilcdninstagram.com/p.jpg",
        ] {
            let error = client.download(refused).await.unwrap_err();
            assert!(matches!(error, IgError::Unexpected { .. }), "{error:?}");
        }
        assert_eq!(page.asks().len(), 2, "no refused address reached the tab");
    }

    /// The tab's status is the answer: a link that has expired is an expired
    /// link, and is not tried the other way; nor is a ceiling lower than
    /// the file; a push-back the browser heard stops it.
    #[tokio::test]
    async fn the_tabs_answer_to_an_asset_is_final_when_it_is_a_status_a_size_or_a_push_back() {
        let (client, _) = spending_client(page_serving(403, b"expired"));
        let error = client.download(PICTURE).await.unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 403, .. }),
            "{error:?}"
        );

        let (client, _) = spending_client(page_serving(200, &[0; 64]));
        let error = client.download_capped(PICTURE, 8).await.unwrap_err();
        assert!(matches!(error, IgError::TooLarge { limit: 8 }), "{error:?}");

        // Past what the tab hands back, under the caller's own ceiling.
        let big = Scripted::telling(|_| {
            Ok(Told::Asset(PageResponse {
                status: 200,
                too_large: true,
                ..Default::default()
            }))
        });
        let (client, _) = spending_client(big);
        let error = client.download_capped(PICTURE, 1024).await.unwrap_err();
        assert!(
            matches!(error, IgError::TooLarge { limit: 1024 }),
            "{error:?}"
        );

        let heard = Scripted::telling(|_| Err(PageError::PushedBack(PushedBack::RateLimited)));
        let (client, _) = spending_client(heard);
        let error = client.download(PICTURE).await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");

        // And a Ctrl+C before anything is asked.
        let page = page_serving(200, b"x");
        let (client, _) = spending_client(page.clone());
        client.pacer().cancel_token().cancel();
        let error = client.download(PICTURE).await.unwrap_err();
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert!(page.asks().is_empty());
    }

    /// What the tab cannot hand back is fetched the other way, from the
    /// same address and under the same rules: a video, which the tab lets no
    /// further than it lets a play; a file past what its protocol carries,
    /// under a higher ceiling; a redirect it does not follow; an answer that
    /// is not base64; the browser failing.
    #[tokio::test]
    async fn what_the_tab_cannot_hand_back_is_fetched_the_other_way() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"direct".to_vec()))
            .mount(&server)
            .await;
        let received = || async { server.received_requests().await.unwrap().len() };

        // A video never reaches the tab.
        let page = page_serving(200, b"tab");
        let client = client(&server).await.through(page.clone());
        let video = format!("{}/clip.mp4?bytestart=0", server.uri());
        assert_eq!(
            client.download_capped(&video, 1 << 20).await.unwrap(),
            b"direct"
        );
        assert!(page.asks().is_empty());
        assert_eq!(received().await, 1);

        let picture = format!("{}/p.jpg", server.uri());
        let too_large = Told::Asset(PageResponse {
            status: 200,
            too_large: true,
            ..Default::default()
        });
        let cases: [(Result<Told, PageError>, usize, &str); 4] = [
            (
                Err(PageError::Browser("gone".into())),
                1 << 20,
                "the browser failing",
            ),
            (
                Ok(Told::Asset(page_said(0, ""))),
                1 << 20,
                "a redirect the tab does not follow",
            ),
            (
                Ok(Told::Asset(page_said(200, "not base64!"))),
                1 << 20,
                "an answer that is not base64",
            ),
            (
                Ok(too_large),
                PAGE_ASSET_BYTES + 1,
                "a file past the tab's size under a higher ceiling",
            ),
        ];
        for (told, cap, why) in cases {
            let before = received().await;
            let page = Scripted::telling(move |_| told.clone());
            let client = self::client(&server).await.through(page.clone());
            assert_eq!(
                client.download_capped(&picture, cap).await.unwrap(),
                b"direct",
                "{why}"
            );
            assert_eq!(page.asks().len(), 1, "{why}");
            assert_eq!(received().await, before + 1, "{why}");
        }

        // The wrong kind of answer is a defect, not a reason to ask again.
        let before = received().await;
        let page = Scripted::telling(|_| Ok(Told::Tray(None)));
        let client = self::client(&server).await.through(page);
        assert!(client.download(&picture).await.is_err());
        assert_eq!(received().await, before);
    }

    /// Without a browser, a download is the other path's alone.
    #[tokio::test]
    async fn without_a_tab_a_download_is_fetched_directly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"direct".to_vec()))
            .mount(&server)
            .await;
        let client = client(&server).await;
        assert!(!client.has_page());
        let bytes = client
            .download(&format!("{}/p.jpg", server.uri()))
            .await
            .unwrap();
        assert_eq!(bytes, b"direct");
    }

    /// What may be fetched is one rule for both paths, with the site's own
    /// address the one exception, matched whole.
    #[test]
    fn the_tab_and_the_client_hold_a_file_to_one_rule() {
        use crate::allowlist::refused_asset;

        let site = "https://www.instagram.com";
        for allowed in [
            "https://scontent.cdninstagram.com/v/a.jpg",
            "https://instagram.flpa4-1.fna.fbcdn.net/v/a.jpg",
            "https://cdninstagram.com/a.jpg",
            "https://www.instagram.com/a.jpg",
        ] {
            assert_eq!(refused_asset(site, allowed), None, "{allowed}");
        }
        for refused in [
            "http://scontent.cdninstagram.com/a.jpg",
            "https://evilcdninstagram.com/a.jpg",
            "https://fbcdn.net.evil.test/a.jpg",
            "http://www.instagram.com/a.jpg",
            "https://www.instagram.com:8443/a.jpg",
            "file:///etc/passwd",
            "not an address",
        ] {
            assert!(refused_asset(site, refused).is_some(), "{refused}");
        }
        // A test's own server is its own site, whole.
        assert_eq!(
            refused_asset("http://127.0.0.1:8080", "http://127.0.0.1:8080/a.jpg"),
            None
        );
        assert!(refused_asset("http://127.0.0.1:8080", "http://127.0.0.1:9/a.jpg").is_some());
    }

    #[test]
    fn a_video_is_told_by_its_path() {
        for (url, video) in [
            ("https://s.cdninstagram.com/v/a.mp4?x=1", true),
            ("https://s.cdninstagram.com/v/a.MP4", true),
            ("https://s.cdninstagram.com/v/a.webm?x=1", true),
            ("https://s.cdninstagram.com/v/a.m3u8", true),
            ("https://s.cdninstagram.com/v/a.jpg?x=.mp4", false),
            ("https://s.cdninstagram.com/v/a.js", false),
        ] {
            assert_eq!(is_video(&Url::parse(url).unwrap()), video, "{url}");
        }
    }

    /// An expired signed URL is a 403 from the CDN. It must not read as a dead
    /// session, which would send someone to log in again for nothing.
    #[tokio::test]
    async fn a_refused_download_is_not_mistaken_for_a_dead_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("expired"))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap_err();

        assert!(matches!(error, IgError::Unexpected { status: 403, .. }));
    }

    /// The streamed download leaves the caller the two things it still needs
    /// after the bytes have gone to disk: how many there were, and the magic
    /// number -- the whole file, when the file is shorter than the head.
    #[tokio::test]
    async fn a_streamed_download_keeps_its_head_and_its_length() {
        let server = MockServer::start().await;
        let body: Vec<u8> = (0..40u8).collect();
        Mock::given(method("GET"))
            .and(path("/clip.mp4"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;

        let url = format!("{}/clip.mp4", server.uri());
        let client = client(&server).await;
        let mut sink = Vec::new();
        let got = client.download_to(&url, 64, &mut sink).await.unwrap();

        assert_eq!(sink, body, "every byte reached the sink, in order");
        assert_eq!(got.len, 40);
        assert_eq!(got.head, body[..HEAD_BYTES].to_vec());

        // Shorter than the head: the head is the whole thing.
        Mock::given(method("GET"))
            .and(path("/tiny"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;
        let mut sink = Vec::new();
        let got = client
            .download_to(&format!("{}/tiny", server.uri()), 64, &mut sink)
            .await
            .unwrap();
        assert_eq!(got.head, vec![1, 2, 3]);
        assert_eq!(got.len, 3);
    }

    /// Something that is not a picture must not be read until memory runs out.
    #[tokio::test]
    async fn a_download_past_the_ceiling_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 64]))
            .mount(&server)
            .await;

        let url = format!("{}/pic.jpg", server.uri());
        let client = client(&server).await;

        let error = client.download_capped(&url, 8).await.unwrap_err();
        assert!(matches!(error, IgError::TooLarge { limit: 8 }));

        // The streaming twin meets the same ceiling in the same loop, and the
        // sink holds what arrived before it -- a prefix, never the whole
        // body -- which is what lets a caller writing to disk remove the
        // file rather than keep a truncated one.
        let mut sink = Vec::new();
        let error = client.download_to(&url, 8, &mut sink).await.unwrap_err();
        assert!(matches!(error, IgError::TooLarge { limit: 8 }));
        assert!(
            sink.len() < 64,
            "the whole body reached the sink: {}",
            sink.len()
        );

        // The same body under a ceiling that fits arrives whole.
        assert_eq!(client.download_capped(&url, 64).await.unwrap().len(), 64);
    }

    /// The exception that lets a test serve a picture over plain HTTP matches
    /// scheme, host and port: on host alone it would be live against the real
    /// base URL, ahead of the https check.
    #[test]
    fn the_test_server_exception_does_not_open_a_hole_in_production() {
        let production = Url::parse(BASE_URL).unwrap();
        for refused in [
            "http://www.instagram.com/pic.jpg",
            "http://www.instagram.com:8080/pic.jpg",
            "https://www.instagram.com:8443/pic.jpg",
        ] {
            assert!(
                !serves_pictures(&production, &Url::parse(refused).unwrap()),
                "{refused} should not be downloadable"
            );
        }
        assert!(serves_pictures(
            &production,
            &Url::parse("https://scontent-mad1-1.cdninstagram.com/v/pic.jpg").unwrap()
        ));
    }

    /// The leading dot is what makes this a suffix rather than a substring.
    #[test]
    fn a_host_that_merely_ends_in_the_cdns_name_is_refused() {
        let production = Url::parse(BASE_URL).unwrap();
        for impostor in [
            "https://evilcdninstagram.com/pic.jpg",
            "https://fbcdn.net.evil.test/pic.jpg",
            "https://cdninstagram.com.evil.test/pic.jpg",
        ] {
            assert!(
                !serves_pictures(&production, &Url::parse(impostor).unwrap()),
                "{impostor} should not be downloadable"
            );
        }
    }

    /// Checking only the address as written left hops two and three judged by
    /// scheme alone, which is a weaker rule than the first hop gets.
    #[tokio::test]
    async fn a_redirect_off_the_cdn_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pic.jpg"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://evil.test/pic.jpg"),
            )
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .download(&format!("{}/pic.jpg", server.uri()))
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::Network(_)), "{error:?}");
    }
}
