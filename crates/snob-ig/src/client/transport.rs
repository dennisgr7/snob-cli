//! How a request goes out to Instagram's API, and how the answer comes back.
//!
//! The origin rule, the redirect loop that pays for every hop it follows, the
//! ceilings on what will be read, and the two races against the cancel token.
//! [`IgClient::get`] is what the endpoints in [`super::read`] go through, and
//! [`Answer`] is what it hands back for [`IgClient::decode`] to read.
//!
//! What a request *says* about itself is in [`super::headers`]. The CDN's own
//! rules are in [`super::media`] rather than here, because they are the
//! opposite rules: an asset request has to be allowed to move between hosts
//! and an API call must not. Keeping the two apart is what stops the looser of
//! them governing the requests that carry the credentials.

use serde::de::DeserializeOwned;
use url::Url;

use crate::error::IgError;

use super::IgClient;
use super::headers::Surface;

/// Ceiling on an API response.
///
/// A page of twelve accounts is a few kilobytes; this is orders of magnitude
/// above anything real. It exists because the body is read into memory whole,
/// so without a limit a hostile or broken answer decides how much memory this
/// process uses.
pub(super) const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// How long a single request may take end to end, and how long the connection
/// itself may take to come up. Generous: the walk's own pacing is what keeps
/// requests apart, and these are only here so that nothing waits forever.
pub(super) const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How far a redirect chain may go before it is a loop by another name.
pub(super) const MAX_HOPS: usize = 3;

/// Same scheme, same host, same port. Not "the same host": a different port
/// or scheme on the same name is a different server.
pub(super) fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Redirects for the API: none, because following one is a request and a
/// request has to be paid for.
///
/// `IgClient::get_body` follows them itself, charging `Pacer::clear_to_send`
/// per hop and holding every hop to the same origin; the reasoning is on
/// `get_body`, next to the loop that does it.
///
/// **The write goes out on this client too, and depends on this staying
/// `none()`.** Following a redirect on a POST means asking Instagram to do the
/// thing again, and "again" is a follow or an unfollow that nobody confirmed:
/// reqwest turns a 307 or 308 into a repeat of the same method and body.
pub(super) fn api_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::none()
}

/// One HTTP client under a given redirect policy.
///
/// The timeouts are not optional. Without them, a server that accepts the
/// connection and then says nothing hangs the process for good: neither the
/// cancel token nor any deadline above reaches a socket that is simply waiting.
pub(super) fn build_client(
    user_agent: &str,
    redirect: reqwest::redirect::Policy,
) -> Result<reqwest::Client, IgError> {
    // The trust store is read here rather than passed in, for the reason
    // `http::TRUST` gives: `login::validate` builds a client inside this crate
    // and never sees the binary's arguments.
    Ok(crate::http::builder(
        user_agent,
        redirect,
        CONNECT_TIMEOUT,
        REQUEST_TIMEOUT,
        &crate::http::chosen_trust(),
    )?
    .build()?)
}

/// Reads a response body, refusing one that will not fit.
///
/// `text()` would buffer whatever arrives, which lets the far end decide how
/// much memory this process uses. Read in chunks so that a response with no
/// `Content-Length` — which is most of them — is bounded too.
pub(super) async fn read_capped(response: reqwest::Response, cap: u64) -> Result<String, IgError> {
    // Lossy rather than strict: a body that is not valid UTF-8 is not JSON
    // either, and saying "could not parse" is more use than "invalid encoding".
    Ok(utf8_or_lossy(read_capped_bytes(response, cap).await?))
}

/// The ceiling itself: refuse a declared length over it, and read in chunks so
/// that a response without a `Content-Length` — which is most of them — is
/// bounded too.
///
/// One copy for the API body and the CDN download, and the CDN is the one
/// that must not be left behind at the next tightening: its URL comes out of
/// Instagram's own answer, so it is the one place a response chooses where the
/// next request goes.
///
/// `http::read_capped` is a different rule for a different reader: its doc
/// names why it truncates rather than refusing.
pub(super) async fn read_capped_bytes(
    response: reqwest::Response,
    cap: u64,
) -> Result<Vec<u8>, IgError> {
    // Sized from the declaration when there is one, which in practice means
    // the CDN: tower-http drops `Content-Length` when it decompresses, and
    // every API answer is compressed, so an API body grows by doubling and a
    // picture arrives into a buffer of the right size. Capped by the ceiling
    // so a lying header cannot reserve more than this would ever accept.
    let mut bytes: Vec<u8> = Vec::with_capacity(
        response
            .content_length()
            .map_or(0, |declared| declared.min(cap) as usize),
    );
    stream_capped(response, cap, &mut bytes).await?;
    Ok(bytes)
}

/// The body, chunk by chunk, into whatever the caller hands over -- a `Vec`
/// for the API and a file for a story -- refusing past `cap`.
///
/// **The one copy of the chunked read.** The in-memory reader above is this
/// over a `Vec`, and the story download is this over a file on disk, so the
/// ceiling is counted in one loop whichever way the bytes are going. The
/// declared-length check in front of it only ever fires on an uncompressed
/// answer (see `read_capped_bytes` for why that means the CDN); the running
/// count is the barrier every answer meets.
///
/// Returns how many bytes were written. On `TooLarge` the sink holds what
/// arrived before the ceiling; a caller writing to disk removes the file.
pub(super) async fn stream_capped(
    mut response: reqwest::Response,
    cap: u64,
    sink: &mut (impl std::io::Write + Send),
) -> Result<u64, IgError> {
    let too_large = || IgError::TooLarge {
        limit: cap as usize,
    };

    if let Some(declared) = response.content_length()
        && declared > cap
    {
        return Err(too_large());
    }

    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await? {
        if written + chunk.len() as u64 > cap {
            return Err(too_large());
        }
        sink.write_all(&chunk)
            .map_err(|e| IgError::Decode(format!("could not write the download: {e}")))?;
        written += chunk.len() as u64;
    }
    Ok(written)
}

/// A body as text, copied only when it has to be.
///
/// `String::from_utf8_lossy(..).into_owned()` copies the whole body even when
/// it is valid UTF-8 -- which every answer here is -- because the borrowed
/// `Cow` has to be owned. `from_utf8` moves the buffer instead, and the lossy
/// path is kept for the byte sequence that is not text, with the same
/// replacement characters it always produced.
fn utf8_or_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The load headers, if Instagram volunteered any, as one string to log.
///
/// Absent from a CDN answer and from anything that is not the API, so `None` is
/// ordinary rather than notable.
pub(super) fn load_of(response: &reqwest::Response) -> Option<String> {
    // Read on every answer whether or not anything will log it, on purpose:
    // the value travels on `Answer` and the test that holds the recording
    // promise reads it from there, with no subscriber installed. Gating it on
    // the log level would make the field lie exactly where it is checked.
    let headers = response.headers();
    load_from(|name| headers.get(name).and_then(|value| value.to_str().ok()))
}

/// The headers in which Instagram describes its own load.
const LOAD_HEADERS: [&str; 2] = ["x-ig-capacity-level", "x-ig-peak-time"];

/// Which backend answered, and in what form. Only one of the two backends
/// sends the load headers, so these say whether an absent load is that
/// backend's ordinary answer or something else.
const SERVED_HEADERS: [&str; 2] = ["x-stack", "content-type"];

/// [`LOAD_HEADERS`] joined into one string, from whichever answer `lookup`
/// reads: the one copy for the wire and for the page.
pub(super) fn load_from<'a>(lookup: impl Fn(&str) -> Option<&'a str>) -> Option<String> {
    joined(&LOAD_HEADERS, lookup)
}

/// [`SERVED_HEADERS`], joined as [`load_from`] joins the load.
pub(super) fn served_from<'a>(lookup: impl Fn(&str) -> Option<&'a str>) -> Option<String> {
    joined(&SERVED_HEADERS, lookup)
}

/// The served headers of a `reqwest` answer, for the log.
pub(super) fn served_of(response: &reqwest::Response) -> Option<String> {
    let headers = response.headers();
    served_from(|name| headers.get(name).and_then(|value| value.to_str().ok()))
}

/// The headers in `names` that were sent, as `name=value` pairs.
fn joined<'a>(names: &[&str], lookup: impl Fn(&str) -> Option<&'a str>) -> Option<String> {
    let found: Vec<String> = names
        .iter()
        .filter_map(|name| lookup(name).map(|value| format!("{name}={value}")))
        .collect();
    (!found.is_empty()).then(|| found.join(" "))
}

/// `Retry-After`, if the answer carried one.
///
/// A string rather than a parsed duration on purpose: the header has two legal
/// forms, seconds and an HTTP date, and until it is known which of them these
/// endpoints send — if either — turning it into a number would be deciding the
/// answer to the question the logging exists to ask.
pub(super) fn retry_after(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// One answer from Instagram: what it said, and the one header worth keeping
/// hold of.
///
/// `classify` is a pure function of a status and a body and does not receive
/// headers, which is why `Retry-After` and the load below travel separately
/// rather than being read where the decision is made. **Nothing decides
/// anything from either of them**, deliberately — see
/// [`IgClient::note_push_back`].
pub(super) struct Answer {
    /// The path the request went to, and only the path: the query can carry
    /// a name. Said on the line a push-back leaves, since which endpoint
    /// Instagram refused is the first thing anybody asks.
    pub(super) endpoint: String,
    pub(super) status: u16,
    pub(super) body: String,
    pub(super) retry_after: Option<String>,
    /// What Instagram said about its own load while answering.
    ///
    /// `x-ig-capacity-level` and `x-ig-peak-time`, joined. Not decided on, and
    /// the reason is with the rest of the pacing reasoning in [`crate::pace`]:
    /// they describe a datacenter's headroom, which is the same for everyone in
    /// that region, and what the pace is managing is a checkpoint on one
    /// account. Carried so that the run which is finally refused can say what
    /// the load was at that moment, which is the observation nobody has.
    pub(super) load: Option<String>,
    /// `x-stack` and `content-type`, joined: which backend answered, logged
    /// beside the load and decided on no more than it.
    pub(super) served: Option<String>,
}

impl Answer {
    /// Whether the status is a 2xx. Half the question: a 200 can still
    /// declare failure in the body, and [`crate::error::refused`] asks both
    /// halves.
    pub(super) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl IgClient {
    /// One request to Instagram's API, with the headers a browser would send.
    ///
    /// `referer` is the path of the page the call would have come from, without
    /// the leading slash. It is not decoration: the tool asks for a followers
    /// list from a URL that, in a browser, only that account's followers page
    /// ever calls, and a generic referer next to a specific endpoint is an
    /// incoherence that costs nothing to avoid.
    pub(super) async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        referer: &str,
    ) -> Result<T, IgError> {
        let answer = self.get_body(path, query, referer, Surface::App).await?;
        self.decode(&answer)
    }

    /// Sends the request and follows any redirect itself, **paying for every
    /// hop**.
    ///
    /// The loop is this function's rather than reqwest's because
    /// `Pacer::clear_to_send` sits inside it: a hop the HTTP client followed on
    /// its own would go out without being charged and without waiting, and
    /// invisibly, because the budget's own count is what the user is shown. A
    /// chain of three would report one request and send four.
    ///
    /// The origin rule is applied to every hop rather than to the first:
    /// Instagram's JSON endpoints do not redirect off their own host, so
    /// refusing costs nothing — and two of the headers on these requests are
    /// credentials. reqwest drops `Cookie` when a redirect crosses hosts, but
    /// `X-CSRFToken` is not on the list it knows about and would travel to
    /// wherever the response pointed. A boundary that depends on somebody
    /// else's list of header names is not one.
    ///
    /// **A refusal here has to reach [`Reaction::Abort`]**, which is why the
    /// two ways out are variants of their own rather than an `Unexpected` or a
    /// `Network`: `Network`'s reaction is `Retry`, and the pager would send the
    /// same impossible request three more times with the session on it, each
    /// one on the wire and not all of them charged.
    ///
    /// The query is attached to the first request only. A `Location` carries
    /// whatever query it means to carry, and appending ours to it would send a
    /// parameter the server did not ask to see twice.
    pub(super) async fn get_body(
        &self,
        path: &str,
        query: &[(&str, &str)],
        referer: &str,
        style: Surface<'_>,
    ) -> Result<Answer, IgError> {
        if let Some(page) = &self.page {
            return self
                .get_body_in_page(page.as_ref(), path, query, referer, style)
                .await;
        }

        let mut url = self.base.join(path)?;
        let mut query: Option<&[(&str, &str)]> = Some(query);
        let mut hops: usize = 0;

        loop {
            tracing::debug!(%url, "GET");

            // Paid for before it is sent, and there is no way in that skips
            // this — the hops included, which is the whole point of the loop.
            self.pacer.clear_to_send().await?;

            let response = self
                .send_or_cancel(self.api_request(&url, query, referer, style))
                .await?;
            self.remember_claim(&response);
            let status = response.status();

            if status.is_redirection() {
                let next = self.next_hop(&url, &response)?;
                // A hop to the challenge or the login form is already the
                // answer, the same one the page path reads off a navigation;
                // it is not sent, so there is nothing more to pay.
                if let Some(refused) = crate::error::landed_on(
                    &next[url::Position::BeforePath..url::Position::AfterQuery],
                ) {
                    return Err(self.record(refused));
                }
                if hops >= MAX_HOPS {
                    return Err(IgError::TooManyRedirects);
                }
                hops += 1;
                url = next;
                query = None;
                continue;
            }

            // The status is already in hand, and a body that will not read must
            // not take it away. With `?` here, a 429 whose body dies mid-stream
            // would become `IgError::Network` — whose reaction is `Retry` — so
            // the walker would fire three more requests into an endpoint that
            // has just said no, and `classify_and_record` would never run:
            // no cooldown written, and the next run knocks again. What
            // Instagram said is the status; the body only refines it.
            let retry_after = retry_after(&response);
            let load = load_of(&response);
            let served = served_of(&response);

            let body = match self.read_or_cancel(response, MAX_BODY_BYTES).await {
                Ok(body) => body,
                // A canceled read is the user, not the server, and must not be
                // turned into a push-back that gets written down as one.
                Err(IgError::Canceled) => return Err(IgError::Canceled),
                Err(_) if !status.is_success() => {
                    let answer = Answer {
                        endpoint: url.path().to_string(),
                        status: status.as_u16(),
                        body: String::new(),
                        retry_after,
                        load,
                        served,
                    };
                    return Err(self.refuse(&answer));
                }
                Err(e) => return Err(e),
            };
            return Ok(Answer {
                endpoint: url.path().to_string(),
                status: status.as_u16(),
                body,
                retry_after,
                load,
                served,
            });
        }
    }

    /// [`Self::get_body`], sent from the browser tab.
    ///
    /// The same three promises as the `reqwest` loop above, kept differently.
    /// **Paid for first**, through the same `clear_to_send`. **No hop goes out
    /// unpaid**: the page follows no redirect on an API call (see `FETCH` in
    /// `headless/tab.rs`), so a redirect comes back as status 0 and is refused
    /// here, with nothing sent after it. **The origin rule holds** for the one
    /// request a browser does follow redirects on, a document navigation:
    /// [`Self::answer_from_page`] refuses an answer that ended up anywhere but
    /// where it started, and charges the hop. From the page that navigation
    /// is an `Ask::Document`, read through the same function
    /// (`IgClient::ask_page`); one built here is refused by the process
    /// holding the browser. A navigation that lands on a push-back is refused
    /// by the page once the owner has recorded it, so its hops go uncounted,
    /// as when the listener hears it first.
    async fn get_body_in_page(
        &self,
        page: &dyn super::page::Page,
        path: &str,
        query: &[(&str, &str)],
        referer: &str,
        style: Surface<'_>,
    ) -> Result<Answer, IgError> {
        let mut url = self.base.join(path)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        tracing::debug!(%url, "GET from the page");

        // Before paying: a push-back the browser heard on the app's own calls
        // stops this with its cause, and is not written down again.
        if let Some(cause) = page.heard() {
            return Err(cause.into());
        }
        self.pacer.clear_to_send().await?;
        let request = self.page_request(&url, referer, style);
        let response = tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => return Err(IgError::Canceled),
            sent = page.send(request) => sent.map_err(IgError::from)?,
        };
        // An opaque redirect: Instagram pointed somewhere and the page did
        // not go. `Unexpected` with no status is an `Abort` everywhere it is
        // read — no retry, no second route, no rediscovery — which is the
        // reqwest path's answer to a hop it will not follow as well.
        if response.status == 0 && !response.redirected {
            return Err(IgError::Unexpected {
                status: 0,
                body: "Instagram answered with a redirect, which is not followed".into(),
            });
        }
        self.answer_from_page(&url, response).await
    }

    /// What the page said, read the way an answer off the wire is read.
    ///
    /// A redirect here is a document navigation's, the only request the
    /// browser follows one on. Its hops went out before this process could
    /// pay for them, so they are paid for now, each of them — and **what
    /// Instagram answered is kept even when paying is refused**: a cooldown
    /// already running, or a Ctrl+C, must not leave a 429 in hand
    /// unclassified. A chain longer than [`MAX_HOPS`] is refused after it is
    /// paid for, as the `reqwest` loop refuses it. A landing on a push-back
    /// never reaches here: the page refuses it after the owner records it, and
    /// its hops go uncounted.
    ///
    /// The same holds for a body over the ceiling. The status is already in
    /// hand, so a refusal is classified from it, as `get_body` does, and only
    /// a success too large to read is [`IgError::TooLarge`].
    pub(super) async fn answer_from_page(
        &self,
        asked: &Url,
        response: super::page::PageResponse,
    ) -> Result<Answer, IgError> {
        let mut landing = None;
        if response.redirected {
            let landed = Url::parse(&response.url).map_err(|_| IgError::OffOrigin {
                to: crate::error::body_excerpt(&response.url),
            })?;
            if !same_origin(asked, &landed) {
                return Err(IgError::OffOrigin {
                    to: crate::error::body_excerpt(landed.as_str()),
                });
            }
            landing = crate::error::landed_on(
                &landed[url::Position::BeforePath..url::Position::AfterQuery],
            );
        }
        if let Some(fresh) = response.header("x-ig-set-www-claim") {
            *self.claim.lock().unwrap_or_else(|e| e.into_inner()) = fresh.to_string();
        }
        // A change of address with no redirect counted still took a request.
        let hops = if response.redirected {
            response.hops.max(1)
        } else {
            0
        };
        if let Some(refused) = landing {
            // The hops went out whatever is decided now; they are paid for,
            // and then the landing is the answer.
            let _ = self.pay_for_hops(hops).await;
            return Err(self.record(refused));
        }
        let too_large = response.too_large;
        let retry_after = response.header("retry-after").map(str::to_string);
        let load = response.load();
        let served = response.served();
        let answer = Answer {
            endpoint: asked.path().to_string(),
            status: response.status,
            retry_after,
            load,
            served,
            body: if too_large {
                String::new()
            } else {
                response.body
            },
        };
        let paid = self.pay_for_hops(hops).await.and_then(|()| {
            if hops as usize > MAX_HOPS {
                Err(IgError::TooManyRedirects)
            } else {
                Ok(())
            }
        });
        if let Err(refused) = paid {
            if !answer.is_success() {
                // Recorded for what it says; the refusal is still what the
                // caller hears, because it is why nothing else is sent.
                let _ = self.refuse(&answer);
            }
            return Err(refused);
        }
        if too_large {
            if !answer.is_success() {
                return Err(self.refuse(&answer));
            }
            return Err(IgError::TooLarge {
                limit: MAX_BODY_BYTES as usize,
            });
        }
        Ok(answer)
    }

    /// Pays for hops a navigation already followed, stopping at the first
    /// charge refused.
    async fn pay_for_hops(&self, hops: u32) -> Result<(), IgError> {
        for _ in 0..hops {
            self.pacer.clear_to_send().await?;
        }
        Ok(())
    }

    /// Sends the request, or gives up the moment the user asks it to.
    ///
    /// **Ctrl+C does not wait for the server.** Cancellation is read in
    /// `Pacer::clear_to_send` and in every deliberate wait, which is where about
    /// nine interrupts in ten land — a stop during a budget wait takes about a
    /// second. The tenth lands here, and without this the exit would track
    /// however long the far end chose to hold the connection, up to
    /// `REQUEST_TIMEOUT`, or 254 seconds on a black-holed connection because
    /// `Network` is retried. That matters most under a service manager, where a
    /// stalled request outlasts the stop grace period and the process is killed
    /// before it can close its snapshot.
    ///
    /// **The cancel branch must answer [`IgError::Canceled`]**, and that is the
    /// part worth guarding. Dropping the future and letting the resulting
    /// `reqwest::Error` fall through would classify as `Network`, whose reaction
    /// is `Retry`, so the pager would answer a Ctrl+C by sending the request
    /// three more times.
    ///
    /// `biased`, so a token that is already set wins against a response that
    /// happens to be ready in the same poll.
    pub(super) async fn send_or_cancel(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, IgError> {
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            response = request.send() => Ok(response?),
        }
    }

    /// Reads the body, or gives up the moment the user asks it to.
    ///
    /// The other half of [`IgClient::send_or_cancel`], and not an afterthought:
    /// a server that answers with headers and then stalls mid-body holds the
    /// connection exactly as long, and the read is where those seconds are
    /// spent.
    async fn read_or_cancel(
        &self,
        response: reqwest::Response,
        cap: u64,
    ) -> Result<String, IgError> {
        tokio::select! {
            biased;
            () = self.pacer.cancel_token().canceled() => Err(IgError::Canceled),
            body = read_capped(response, cap) => body,
        }
    }

    /// Where a redirect points, if it points somewhere this client may go.
    ///
    /// A `Location` is allowed to be relative, so it is resolved against the
    /// URL that produced it rather than parsed on its own — and the result is
    /// held to the same origin rule as the first request. Both refusals are
    /// excerpted like every other error that prints something a server chose:
    /// this string is printed to a terminal and Instagram wrote it.
    fn next_hop(&self, from: &Url, response: &reqwest::Response) -> Result<Url, IgError> {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| IgError::Unexpected {
                status: response.status().as_u16(),
                body: "a redirect arrived with nowhere to go".into(),
            })?;

        let next = from.join(location).map_err(|_| IgError::OffOrigin {
            to: crate::error::body_excerpt(location),
        })?;

        if !same_origin(&self.base, &next) {
            return Err(IgError::OffOrigin {
                to: crate::error::body_excerpt(next.as_str()),
            });
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snob_core::Pk;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::Direction;
    use crate::client::harness::{Scripted, client, page_said, watching};

    /// A 429 whose body dies mid-stream is still a 429.
    ///
    /// Read as `Network`, it would be retried three more times and no cooldown
    /// would be written: both halves of the rule at once.
    ///
    /// Served from a raw socket rather than from `wiremock`, because what has to
    /// happen is a body that stops arriving: the headers announce a length and
    /// the connection closes before it is sent.
    #[tokio::test]
    async fn a_throttled_answer_with_a_body_that_dies_is_still_throttled() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());

        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();

            // Drained first, or the close below races the request still being
            // written and the failure lands on the send rather than on the read.
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while socket.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }

            // Announced as 4096 bytes, and then the connection goes away with
            // none of them sent. The pause is what lets the head be delivered
            // and the body read begin before that happens.
            let _ =
                socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 4096\r\n\r\n");
            let _ = socket.flush();
            std::thread::sleep(std::time::Duration::from_millis(150));
        });

        let (client, budget) = watching(&base);
        let error = client.validate().await.unwrap_err();
        server.join().unwrap();

        assert!(
            matches!(error, IgError::RateLimited),
            "a body that would not read must not turn a 429 into a network error: {error:?}"
        );
        assert_eq!(error.reaction(), crate::error::Reaction::Cooldown);
        assert_eq!(
            budget.calls(),
            vec![(
                "rate_limit".to_string(),
                snob_core::budget::RATE_LIMIT_COOLDOWN
            )]
        );
    }

    /// `Cookie` is dropped by the HTTP client on a cross-host redirect, but
    /// `X-CSRFToken` is not on its list and would have traveled. Instagram's
    /// API does not redirect off its own host, so refusing costs nothing.
    #[tokio::test]
    async fn an_api_redirect_off_the_origin_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://evil.test/collected"),
            )
            .mount(&server)
            .await;

        let error = client(&server).await.validate().await.unwrap_err();
        // `OffOrigin` rather than `Network`: the refusal is this client's now
        // that `get_body` follows the chain itself, and it has to keep landing
        // on `Abort` — see `a_hop_off_the_origin_is_refused_and_not_retried`.
        assert!(matches!(error, IgError::OffOrigin { .. }), "{error:?}");

        // The one request that was made is the one we made on purpose.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
    }

    /// A followed hop is a request, and every request is paid for.
    ///
    /// The budget's count is what the user is shown and what rate control is
    /// built on, so a chain of two has to count three requests, not one.
    #[tokio::test]
    async fn every_redirect_hop_is_charged() {
        let server = MockServer::start().await;
        let body = r#"{"users":[],"next_max_id":null}"#;

        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/api/v1/hop-one/"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/hop-one/"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/api/v1/hop-two/"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/hop-two/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let page = client
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
            .await
            .expect("the chain ends in an answer");
        assert!(page.users.is_empty());

        assert_eq!(
            client.pacer().spent(),
            3,
            "one request and two hops is three requests, and the budget has to know"
        );
    }

    /// A hop off instagram.com is refused, and refused in a way that stops the
    /// walk rather than making it try again.
    ///
    /// Two of the headers on these requests are credentials. reqwest drops
    /// `Cookie` across hosts but has never heard of `X-CSRFToken`, so a
    /// followed hop would carry it wherever the response pointed.
    ///
    /// The reaction matters as much as the refusal: read as `Network`, whose
    /// reaction is `Retry`, the pager would send the same impossible request
    /// three more times.
    #[tokio::test]
    async fn a_hop_off_the_origin_is_refused_and_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "https://example.invalid/collect"),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
            .await
            .expect_err("it must not follow that");

        assert!(
            matches!(&error, IgError::OffOrigin { to } if to.contains("example.invalid")),
            "{error:?}"
        );
        assert_eq!(
            error.reaction(),
            crate::error::Reaction::Abort,
            "retrying a redirect that will be refused again is what cost 13 requests"
        );
        assert_eq!(
            client.pacer().spent(),
            1,
            "the hop was never sent, so it is never charged"
        );
    }

    /// A chain that never ends stops at `MAX_HOPS`, having paid for exactly the
    /// requests it made.
    #[tokio::test]
    async fn a_redirect_loop_stops_and_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/api/v1/friendships/1/followers/"),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
            .await
            .expect_err("a loop is not an answer");

        assert!(matches!(error, IgError::TooManyRedirects), "{error:?}");
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert_eq!(
            client.pacer().spent(),
            (MAX_HOPS + 1) as u32,
            "the first request and every hop it was allowed"
        );
    }

    /// The query goes on the first request and not on the hops.
    ///
    /// A `Location` carries whatever query it means to carry. Appending ours to
    /// it would send a parameter the server did not ask to see twice, and on an
    /// endpoint that takes a cursor that is a different request from the one
    /// the redirect described.
    #[tokio::test]
    async fn the_query_is_not_reattached_to_a_hop() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .and(query_param("count", "50"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "/api/v1/landed/?count=7"),
            )
            .mount(&server)
            .await;
        // Mounted with the hop's own count, so it only matches if ours was not
        // added alongside it.
        Mock::given(method("GET"))
            .and(path("/api/v1/landed/"))
            .and(query_param("count", "7"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"users":[],"next_max_id":null}"#),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        client
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
            .await
            .expect("the hop's own query is the one that travels");
    }

    /// A redirect with no `Location` is an answer nobody can act on, and it must
    /// not become a silent success or a retry.
    #[tokio::test]
    async fn a_redirect_with_nowhere_to_go_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/1/followers/"))
            .respond_with(ResponseTemplate::new(302))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
            .await
            .expect_err("there is nowhere to go");
        assert!(
            matches!(error, IgError::Unexpected { status: 302, .. }),
            "{error:?}"
        );
    }

    /// A stop during a request in flight does not wait for the server.
    ///
    /// The interrupt that does not land in a budget wait lands here, and
    /// without the race it would track however long the far end holds the
    /// connection (see `send_or_cancel`).
    ///
    /// The delay here is thirty seconds so that a passing run cannot be one
    /// that simply waited it out: the assertion is that the call came back in a
    /// fraction of it.
    #[tokio::test]
    async fn canceling_does_not_wait_for_the_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_string(r#"{"users":[],"next_max_id":null}"#),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let token = client.pacer().cancel_token().clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token.cancel();
        });

        let started = std::time::Instant::now();
        let error = client
            .friendships_page(Pk::new(1), "someone", Direction::Followers, 50, None)
            .await
            .expect_err("the run was canceled");

        // `Canceled`, not `Network`. Letting the dropped request become a
        // network error would give it `Reaction::Retry`, so the pager would
        // answer a Ctrl+C by sending the request three more times.
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "it waited {:?}, which is the server's patience rather than the user's",
            started.elapsed()
        );
    }

    /// A stop is the user's answer, not Instagram's, and nothing is written
    /// down as though it were.
    ///
    /// The endpoint here would classify as a push-back and earn a cooldown if
    /// its answer were ever read. Cancellation has to win first, and win
    /// without the interrupted request leaving a mark: a cooldown recorded
    /// because somebody pressed Ctrl+C would refuse the next run for half an
    /// hour over something Instagram never said.
    #[tokio::test]
    async fn canceling_records_no_cooldown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_string(r#"{"message":"feedback_required"}"#),
            )
            .mount(&server)
            .await;

        let (client, budget) = watching(&server.uri());

        let token = client.pacer().cancel_token().clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token.cancel();
        });

        let error = client.validate().await.expect_err("the run was canceled");
        assert!(matches!(error, IgError::Canceled), "{error:?}");
        assert!(
            budget.calls().is_empty(),
            "a canceled request was recorded as a push-back: {:?}",
            budget.calls()
        );
    }

    /// A page that has heard a push-back, or that answers with one it heard,
    /// and counts what it is asked to send.
    struct Pushed {
        heard: Option<crate::client::page::PushedBack>,
        sent: std::sync::atomic::AtomicUsize,
    }

    impl crate::client::page::Page for Pushed {
        fn send(&self, _: crate::client::page::PageRequest) -> crate::client::page::PageFuture<'_> {
            self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Err(crate::client::page::PageError::PushedBack(
                    crate::client::page::PushedBack::RateLimited,
                ))
            })
        }
        fn heard(&self) -> Option<crate::client::page::PushedBack> {
            self.heard.clone()
        }
    }

    /// A push-back the browser heard on the app's own calls stops the next
    /// request before it is paid for, with its own cause — a challenge keeps
    /// its link — and is not written down a second time.
    #[tokio::test]
    async fn a_push_back_the_page_heard_stops_before_anything_is_paid() {
        let (client, budget) = crate::client::harness::watching("http://127.0.0.1:9/");
        let page = Arc::new(Pushed {
            heard: Some(crate::client::page::PushedBack::Challenge {
                url: Some("https://www.instagram.com/challenge/x/".into()),
            }),
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let client = client.through(page.clone());

        let error = client
            .get_body("/api/v1/friendships/42/following/", &[], "", Surface::App)
            .await
            .err()
            .expect("it was refused");
        assert!(
            matches!(error, IgError::Challenge { url: Some(_) }),
            "{error:?}"
        );
        assert_eq!(page.sent.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(client.pacer().spent(), 0, "nothing was paid for");
        assert!(
            budget.calls().is_empty(),
            "recorded again: {:?}",
            budget.calls()
        );
    }

    /// One the page reports as the answer to a request is its cause too, and
    /// it was recorded where it was heard: never here as well, which would
    /// double the cooldown.
    #[tokio::test]
    async fn a_push_back_the_page_reports_is_never_recorded_again() {
        let (client, budget) = crate::client::harness::watching("http://127.0.0.1:9/");
        let page = Arc::new(Pushed {
            heard: None,
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let client = client.through(page.clone());

        let error = client
            .get_body("/api/v1/friendships/42/following/", &[], "", Surface::App)
            .await
            .err()
            .expect("it was refused");
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
        assert_eq!(page.sent.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            budget.calls().is_empty(),
            "recorded again: {:?}",
            budget.calls()
        );
    }

    /// Every push-back says what `Retry-After` it carried, and nothing acts on
    /// it.
    ///
    /// `classify` takes a status and a body and never sees a header, so the
    /// question of whether these endpoints send this at all has never been
    /// answerable from a real run. It is now. What must **not** happen is the
    /// header changing anything before somebody has seen one: the cooldown
    /// recorded here is the same one that was recorded before, and a
    /// server-named thirty seconds must never shorten it.
    #[tokio::test]
    async fn a_push_back_says_what_retry_after_it_carried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "30")
                    // What Instagram volunteers about its own load on every
                    // API answer. Carried for the same reason and decided on
                    // for neither: see `note_push_back`.
                    .insert_header("x-ig-capacity-level", "2")
                    .insert_header("x-ig-peak-time", "1")
                    .insert_header("x-stack", "distillery")
                    .set_body_raw(
                        r#"{"message":"Please wait a few minutes"}"#,
                        "application/json",
                    ),
            )
            .mount(&server)
            .await;

        let (client, budget) = watching(&server.uri());

        // **The header on the answer, not the line in the log.** A capturing
        // subscriber is thread-local and an async path is polled wherever the
        // runtime likes, so a log assertion is flaky. What matters is that the
        // header is read off the response and carried, which `Answer` holds;
        // the logging is one line over this value.
        let answer = client
            .get_body(
                &format!(
                    "/api/v1/friendships/{}/following/",
                    client.session.ds_user_id
                ),
                &[("count", "1")],
                "",
                Surface::App,
            )
            .await
            .expect("a 429 is an answer, not a transport failure");
        assert_eq!(answer.status, 429);
        assert_eq!(answer.retry_after.as_deref(), Some("30"));
        assert_eq!(
            answer.load.as_deref(),
            Some("x-ig-capacity-level=2 x-ig-peak-time=1"),
            "nobody has ever recorded the load Instagram announced while refusing"
        );
        assert_eq!(
            answer.served.as_deref(),
            Some("x-stack=distillery content-type=application/json")
        );

        // And the cooldown is untouched by either of them. Thirty seconds is
        // far shorter than the rate-limit cooldown, so a header allowed to
        // shorten anything would show up right here.
        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
        let recorded = budget.calls();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert_eq!(recorded[0].0, "rate_limit");
        assert_eq!(
            recorded[0].1,
            snob_core::budget::RATE_LIMIT_COOLDOWN,
            "the server's number reached the cooldown, and it must not"
        );
    }

    /// A push-back with no such header still says so, which is the answer the
    /// logging is really after: these endpoints may simply never send one.
    ///
    /// A 200 carrying `spam: true` is a push-back, and one that a check on the
    /// status alone would have walked straight past.
    #[tokio::test]
    async fn a_push_back_without_the_header_is_recorded_as_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"status":"fail","spam":true}"#),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let answer = client
            .get_body("/api/v1/friendships/42/following/", &[], "", Surface::App)
            .await
            .expect("a 200 is an answer");
        assert_eq!(answer.status, 200);
        assert!(
            answer.retry_after.is_none(),
            "nothing sent one, so nothing may be invented"
        );

        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }

    /// A hop to the challenge is the challenge, from `reqwest` as from the
    /// page: classified, written down, and not followed, so the one request
    /// charged is the one that was sent.
    #[tokio::test]
    async fn a_redirect_to_the_challenge_is_a_challenge_and_is_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "/challenge/x/?next=%2F"),
            )
            .mount(&server)
            .await;

        let (client, budget) = watching(&server.uri());
        let error = client.validate().await.unwrap_err();
        assert!(matches!(error, IgError::Challenge { .. }), "{error:?}");
        assert_eq!(
            budget.calls(),
            vec![(
                "challenge".to_string(),
                snob_core::budget::CHALLENGE_COOLDOWN
            )]
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(client.pacer().spent(), 1);
    }

    fn from_the_page(status: u16, too_large: bool) -> super::super::page::PageResponse {
        super::super::page::PageResponse {
            status,
            url: "http://127.0.0.1:9/api/v1/x/".into(),
            too_large,
            ..Default::default()
        }
    }

    /// A body over the ceiling does not take the status away from the page
    /// path either: a 429 too large to read is still a 429, with its cooldown,
    /// and only a success too large to read is `TooLarge`.
    #[tokio::test]
    async fn a_refusal_too_large_to_read_from_the_page_is_still_a_refusal() {
        let (client, budget) = watching("http://127.0.0.1:9");
        let asked = Url::parse("http://127.0.0.1:9/api/v1/x/").unwrap();

        let refused = client
            .answer_from_page(&asked, from_the_page(429, true))
            .await;
        assert!(matches!(refused, Err(IgError::RateLimited)));
        assert_eq!(budget.calls().len(), 1);

        let too_large = client
            .answer_from_page(&asked, from_the_page(200, true))
            .await;
        assert!(matches!(too_large, Err(IgError::TooLarge { .. })));
        assert_eq!(budget.calls().len(), 1, "a success records nothing");
    }

    /// A navigation that lands on a profile whose name starts with
    /// "challenge" landed on a profile.
    #[tokio::test]
    async fn a_profile_named_like_the_challenge_is_a_profile() {
        let (client, budget) = watching("http://127.0.0.1:9");
        let asked = Url::parse("http://127.0.0.1:9/challenger/").unwrap();
        let mut response = from_the_page(200, false);
        response.url = "http://127.0.0.1:9/challenger/?hl=en".into();
        response.redirected = true;

        let answer = client.answer_from_page(&asked, response).await;
        assert!(matches!(answer, Ok(Answer { status: 200, .. })));
        assert!(budget.calls().is_empty());
    }

    const PAGE_BASE: &str = "http://127.0.0.1:9";

    /// A redirect the page followed to `url`, `hops` of them, answering
    /// `status`.
    fn landed(url: &str, hops: u32, status: u16) -> super::super::page::PageResponse {
        super::super::page::PageResponse {
            status,
            body: "{}".into(),
            url: url.into(),
            redirected: true,
            hops,
            ..Default::default()
        }
    }

    /// One GET from `page`, the way every endpoint sends one: a friendship
    /// list, the one kind of GET the page sends.
    async fn get_from(client: &IgClient) -> Result<Answer, IgError> {
        client
            .get_body(
                "/api/v1/friendships/2345678901/following/",
                &[],
                "",
                Surface::App,
            )
            .await
    }

    /// An API call the page would not follow a redirect on comes back as an
    /// opaque redirect, and is refused: one request sent, one paid for,
    /// nothing recorded, nothing retried.
    #[tokio::test]
    async fn an_opaque_redirect_from_the_page_is_refused_with_nothing_more_sent() {
        let (client, budget) = watching(PAGE_BASE);
        let page = Scripted::new(|_| Ok(page_said(0, "")));
        let client = client.through(page.clone());

        let Err(error) = get_from(&client).await else {
            panic!("an opaque redirect is not an answer");
        };
        assert!(
            matches!(error, IgError::Unexpected { status: 0, .. }),
            "{error:?}"
        );
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert_eq!(page.asked().len(), 1);
        assert_eq!(client.pacer().spent(), 1);
        assert!(budget.calls().is_empty(), "{:?}", budget.calls());
    }

    /// A navigation that ended on another origin is refused before its hop is
    /// paid for, since nothing is read from it.
    #[tokio::test]
    async fn a_page_answer_off_the_origin_is_refused() {
        let (client, _) = watching(PAGE_BASE);
        let page = Scripted::new(|_| Ok(landed("https://example.invalid/collect", 1, 200)));
        let client = client.through(page);

        let Err(error) = get_from(&client).await else {
            panic!("another origin is not an answer");
        };
        assert!(
            matches!(&error, IgError::OffOrigin { to } if to.contains("example.invalid")),
            "{error:?}"
        );
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
    }

    /// A navigation that landed on the challenge is the challenge: written
    /// down, and its hop paid for beside the request.
    #[tokio::test]
    async fn a_page_that_landed_on_the_challenge_is_a_challenge() {
        let (client, budget) = watching(PAGE_BASE);
        let page = Scripted::new(|_| Ok(landed("http://127.0.0.1:9/challenge/abc/", 1, 200)));
        let client = client.through(page);

        let Err(error) = get_from(&client).await else {
            panic!("the challenge is not an answer");
        };
        assert!(matches!(error, IgError::Challenge { .. }), "{error:?}");
        assert_eq!(
            budget.calls(),
            vec![(
                "challenge".to_string(),
                snob_core::budget::CHALLENGE_COOLDOWN
            )]
        );
        assert_eq!(client.pacer().spent(), 2, "the request and its hop");
    }

    /// Every hop the browser followed is paid for, and a chain longer than
    /// the `reqwest` loop would follow is refused the same way, once paid.
    #[tokio::test]
    async fn every_hop_the_page_followed_is_paid_for() {
        let (client, _) = watching(PAGE_BASE);
        let page = Scripted::new(|_| Ok(landed("http://127.0.0.1:9/api/v1/y/", 2, 200)));
        let client = client.through(page);
        assert!(get_from(&client).await.is_ok());
        assert_eq!(client.pacer().spent(), 3, "the request and its two hops");

        let (client, _) = watching(PAGE_BASE);
        let too_many = MAX_HOPS as u32 + 1;
        let page =
            Scripted::new(move |_| Ok(landed("http://127.0.0.1:9/api/v1/y/", too_many, 200)));
        let client = client.through(page);
        let Err(error) = get_from(&client).await else {
            panic!("a chain that long is a loop");
        };
        assert!(matches!(error, IgError::TooManyRedirects), "{error:?}");
        assert_eq!(error.reaction(), crate::error::Reaction::Abort);
        assert_eq!(client.pacer().spent(), 1 + too_many);
    }

    /// Paying for a hop can be refused — here by a Ctrl+C — and a 429 the
    /// page already holds is recorded all the same.
    #[tokio::test]
    async fn a_refusal_the_page_holds_is_recorded_when_its_hop_cannot_be_paid() {
        let (client, budget) = watching(PAGE_BASE);
        client.pacer().cancel_token().cancel();
        let asked = Url::parse("http://127.0.0.1:9/api/v1/x/").unwrap();

        let answer = client
            .answer_from_page(&asked, landed("http://127.0.0.1:9/api/v1/y/", 1, 429))
            .await;
        assert!(matches!(answer, Err(IgError::Canceled)));
        assert_eq!(
            budget.calls(),
            vec![(
                "rate_limit".to_string(),
                snob_core::budget::RATE_LIMIT_COOLDOWN
            )]
        );
    }

    /// The claim a page's answer hands out goes back on the next request.
    #[tokio::test]
    async fn the_claim_a_page_answer_sets_is_sent_next() {
        let (client, _) = watching(PAGE_BASE);
        let page = Scripted::new(|_| {
            let mut answer = page_said(200, "{}");
            answer.headers = vec![("x-ig-set-www-claim".into(), "hmac.fresh".into())];
            Ok(answer)
        });
        let client = client.through(page.clone());
        assert!(get_from(&client).await.is_ok());
        assert!(get_from(&client).await.is_ok());

        let claim = |request: &super::super::page::PageRequest| {
            request
                .headers
                .iter()
                .find(|(name, _)| name == "X-IG-WWW-Claim")
                .map(|(_, value)| value.clone())
        };
        let asked = page.asked();
        assert_eq!(claim(&asked[0]).as_deref(), Some("0"));
        assert_eq!(claim(&asked[1]).as_deref(), Some("hmac.fresh"));
    }
}
