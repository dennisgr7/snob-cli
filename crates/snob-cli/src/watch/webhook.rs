//! Sending a report to the address the user chose.
//!
//! Everything here is arranged around one rule: **this client cannot carry the
//! session.** [`WebhookClient::new`] takes no `Session` and no `IgClient`, and
//! the client under it is built by `snob_ig::http::plain`, which has no
//! argument to pass a credential through. The destination is a host somebody
//! typed into a configuration file; sending Instagram's cookie there would be
//! the same failure `CdnClient::check_downloadable` exists to prevent for the
//! CDN, one crate up.
//!
//! Two smaller rules follow from the same place. Redirects are not followed at
//! all — a 3xx is exactly how a body lands at a host nobody named — and the
//! User-Agent says `snob`, because claiming to be a browser to the user's own
//! server would be a lie told for no reason and would leak which browser they
//! have.

use anyhow::{Result, bail};
use snob_core::secret::Secret;
use snob_core::watch::sign;
use snob_ig::http::{self, reqwest};
use url::Url;

/// How much of an error response is read back to show the user.
///
/// Bounded for the reason every read in this project is: without it the far end
/// decides how much memory this process uses.
const MAX_RESPONSE_BYTES: usize = 4 * 1024;

/// What the receiver is told, beyond the body.
pub const EVENT_HEADER: &str = "X-Snob-Event";
pub const DELIVERY_HEADER: &str = "X-Snob-Delivery";
pub const ATTEMPT_HEADER: &str = "X-Snob-Attempt";

/// Where a report goes, and what to put on it.
pub struct Webhook {
    pub url: Url,
    /// Extra headers, as the user gave them.
    pub headers: Vec<(String, String)>,
    /// The shared secret, when there is one.
    pub key: Option<Secret>,
}

/// A client that can reach the user's server and nothing else of ours.
pub struct WebhookClient {
    client: reqwest::Client,
    webhook: Webhook,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Delivered {
        status: u16,
    },
    /// Worth trying again: the far end was busy, restarting, unreachable, or
    /// answered with a status it may not answer with next time.
    ///
    /// **Every HTTP answer lands here, 4xx included.** A refusal is expired by
    /// the outbox with no retries, and the mark has already moved by then, so a
    /// 404 from an n8n workflow that happened not to be registered would throw
    /// away the only copy of a set of arrivals and departures.
    Failed {
        status: Option<u16>,
        error: String,
    },
    /// The request could not be sent at all, so there is nothing to try again.
    ///
    /// Not "the server said no": a configuration that never went through
    /// [`check`] and produces a header this cannot build. Retrying it sends the
    /// identical unbuildable request.
    ///
    /// **No status, rather than a zero.** `watch_deliveries.last_status` is "the
    /// HTTP code, when there was one": NULL already says there was none, and 0
    /// is not a code any server can answer with.
    Refused {
        error: String,
    },
}

impl Attempt {
    /// The HTTP code the far end answered with, when it answered at all.
    ///
    /// `Refused` has none to give: the request was never built, so no server
    /// answered anything.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Delivered { status } => Some(*status),
            Self::Failed { status, .. } => *status,
            Self::Refused { .. } => None,
        }
    }

    /// What went wrong, in whatever words the far end or the client used.
    ///
    /// On the type rather than beside one of the callers, so `engine::check`
    /// and `commands::watch` describe an attempt with the same words and
    /// neither falls back to `Debug`.
    pub fn error(&self) -> &str {
        match self {
            Self::Delivered { .. } => "",
            Self::Failed { error, .. } | Self::Refused { error, .. } => error,
        }
    }
}

impl WebhookClient {
    /// Builds the client.
    ///
    /// **No credential can reach this.** There is no argument for one, which is
    /// the guard rather than a rule somebody has to keep.
    pub fn new(webhook: Webhook) -> Result<Self> {
        // A private destination is one `check` allowed to be unencrypted
        // *because* it is private, so it must not leave the machine through a
        // proxy named by an inherited environment variable -- which is a
        // measured leak of the report, the `Authorization` header and the
        // signature, in the clear, to a third party. A public receiver keeps
        // honoring the environment, because somebody behind a mandatory proxy
        // has no other route out.
        let build = if is_private(&webhook.url) {
            http::plain_direct
        } else {
            http::plain
        };
        Ok(Self {
            client: build(
                &format!("snob/{}", env!("CARGO_PKG_VERSION")),
                // Not `limited(0)`, which follows nothing but reports a 3xx as
                // an error the same way; `none()` hands the response back so
                // the status can be read and reported. Either way the body does
                // not travel to wherever the redirect pointed.
                reqwest::redirect::Policy::none(),
            )?,
            webhook,
        })
    }

    /// Posts one report.
    ///
    /// `body` is sent verbatim and signed verbatim. It is not re-serialized
    /// here, and `.json()` is deliberately not used: that would render the
    /// value again, and the signature covers bytes.
    ///
    /// `event` is what the body says it is, and the header has to agree: the
    /// point of the header is that a receiver can route on it without parsing
    /// the body.
    pub async fn post(&self, body: &str, event: &str, delivery_id: &str, attempt: i64) -> Attempt {
        // One map for the whole request: snob's headers first, then the user's
        // inserted over them, and the map handed over in one go.
        // `RequestBuilder::header` is `HeaderMap::append`, so two entries of one
        // name would both go out: a configured `Authorization` beside the
        // keyring token, or the user's `Content-Type` beside snob's.
        //
        // `HeaderMap` is case-insensitive, so `insert` over the whole map is
        // what "the last one wins" needs to be true. What the user must not be
        // able to replace is refused by `check`, not resolved quietly here.
        let mut headers = reqwest::header::HeaderMap::new();
        let mut own = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (EVENT_HEADER.to_string(), event.to_string()),
            (DELIVERY_HEADER.to_string(), delivery_id.to_string()),
            (ATTEMPT_HEADER.to_string(), attempt.to_string()),
        ];
        if let Some(key) = &self.webhook.key {
            own.push((sign::HEADER.to_string(), sign::sign(body, key)));
        }
        for (name, value) in own.iter().chain(&self.webhook.headers) {
            // Both were validated by `check` before this client was built, so a
            // failure here is a configuration that never went through it.
            match (
                reqwest::header::HeaderName::try_from(name.as_str()),
                reqwest::header::HeaderValue::try_from(value.as_str()),
            ) {
                (Ok(name), Ok(value)) => {
                    headers.insert(name, value);
                }
                _ => {
                    return Attempt::Refused {
                        error: format!(
                            "\"{}\" is not a header this can send",
                            snob_core::model::printable(name)
                        ),
                    };
                }
            }
        }
        let request = self.client.post(self.webhook.url.clone()).headers(headers);

        match request.body(body.to_string()).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Attempt::Delivered {
                        status: status.as_u16(),
                    };
                }

                let detail = http::read_capped(response, MAX_RESPONSE_BYTES)
                    .await
                    .unwrap_or_default();
                // Scrubbed before `describe` cuts it short, so a cut cannot
                // leave half an address that no longer matches.
                let error = describe(status, &without_address(&detail, &self.webhook.url));

                // Every answer is worth another try, including a 4xx, for the
                // reason `Attempt::Failed` gives. The 4xx a webhook actually
                // gives are mostly transient: n8n answers 404 for a workflow
                // that is not currently registered, a reverse proxy answers 404
                // or 403 while it reloads, an expired bearer token answers 401.
                // The far end is the user's own server, so retrying costs
                // nothing that matters, and the attempt and age bounds
                // (`deliveries::MAX_ATTEMPTS`, not written out here so a copy of
                // the number cannot go stale) still stop it going on for ever.
                //
                // `Refused` is left for a request that could not be sent at all,
                // which is the only failure retrying genuinely cannot change.
                Attempt::Failed {
                    status: Some(status.as_u16()),
                    error,
                }
            }
            // No response at all: a refused connection, a name that does not
            // resolve, a timeout. All of those are things that come back.
            Err(e) => Attempt::Failed {
                status: None,
                error: unanswered(e, &self.webhook.url),
            },
        }
    }
}

/// Why a request got no answer, in words that never carry the address.
///
/// **Not `reqwest::Error`'s own `Display`**, which ends in ` for url (<the
/// whole address>)`: that string is written to `watch_deliveries`, warned on
/// every retry, handed to `snob watch check` and its `--json`, and so into
/// the log of an unattended service, and on a Slack or Discord webhook the
/// address is the credential [`shown`] exists to keep out of all four.
/// `without_url` drops it; the causes under it (a refused connection, a name
/// that does not resolve, a certificate) are what a person can act on, so
/// they follow it the way anyhow's `{:#}` would, and the top line alone
/// ("error sending request") says nothing. None of them is known to name the
/// address, but this string is kept and printed, so [`without_address`]
/// checks rather than trusts that, and `printable` takes out what a terminal
/// would obey.
fn unanswered(error: reqwest::Error, url: &Url) -> String {
    let error = error.without_url();
    let mut words = error.to_string();
    let mut cause = std::error::Error::source(&error);
    while let Some(next) = cause {
        let said = next.to_string();
        // Some layers repeat their cause in their own sentence.
        if !said.is_empty() && !words.ends_with(&said) {
            words.push_str(": ");
            words.push_str(&said);
        }
        cause = next.source();
    }
    snob_core::model::printable(&without_address(&words, url))
}

/// `text` with the address replaced by [`shown`]'s, and where the path or
/// the query is the credential, each of those on its own too.
///
/// For text this process did not write (an error's causes, a receiver's
/// answer) that is stored and printed: an address whose shown form is itself
/// hides nothing, and is left alone.
fn without_address(text: &str, url: &Url) -> String {
    let shown = shown(url);
    if shown == url.as_str() {
        return text.to_string();
    }
    let mut text = text.replace(url.as_str(), &shown);
    if let Ok(clean) = Url::parse(&shown)
        && clean.path() != url.path()
    {
        text = text.replace(url.path(), clean.path());
    }
    // A receiver may echo the parameters it was given without the rest.
    if let Some(query) = url.query().filter(|query| !query.is_empty()) {
        text = text.replace(query, HIDDEN);
    }
    text
}

/// What the far end said, in one line.
fn describe(status: reqwest::StatusCode, body: &str) -> String {
    // Two hundred characters are kept, so sixty-four words is more than the
    // excerpt can ever show; the body is the receiver's, up to four kilobytes,
    // and this is the path a receiver that is down sends every retry through.
    let excerpt: String = body
        .split_whitespace()
        .take(64)
        .collect::<Vec<_>>()
        .join(" ");
    if excerpt.is_empty() {
        return status.to_string();
    }
    let mut excerpt: String = excerpt.chars().take(200).collect();
    // The body came off a server this tool does not control, so it is filtered
    // like any other text before it reaches a terminal.
    excerpt = snob_core::model::printable(&excerpt);
    format!("{status}: {excerpt}")
}

/// What [`shown`] puts where a part of the address was.
const HIDDEN: &str = "...";

/// The address as it may be shown to a person or written to a log.
///
/// Userinfo cleared: `check` refuses an address that carries a password, and a
/// refusal, a `status` line or a warning built before `check` is reached must
/// not print the secret it is refusing.
///
/// The scheme, host and path are kept, because which address was meant is the
/// whole content of every one of those messages.
///
/// **The query is not, nor the fragment.** On many receivers the query is the
/// credential: Google Chat's `?key=…&token=…`, the `&sig=…` of an Azure Logic
/// Apps or Power Automate trigger, the `?token=` of a self-hosted endpoint.
/// One that is there is shown as `?...`, which says there was one, and the
/// host and path already say which address was meant.
///
/// **Nor is the path, where the path is the credential.** A Slack incoming
/// webhook is `/services/T…/B…/<secret>`, a Discord one
/// `/api/webhooks/<id>/<token>`, a Zapier one `/hooks/catch/<id>/<key>`, an
/// IFTTT one `/trigger/<event>/with/key/<key>`, a Teams one on
/// `*.webhook.office.com` `/webhookb2/<ids>/IncomingWebhook/<ids>`, and a
/// Make one `hook.<zone>.make.com/<token>`; there is no userinfo to clear
/// because the whole secret is in the path, which would otherwise be printed
/// on every `watch check`, in `--json`, and into the log of an unattended
/// service. For those hosts the path stops at the segment that names the
/// service, which is still enough to recognize, or goes whole where no
/// segment does (Make's, or one of those hosts' paths without the segment).
///
/// **An address with no host is shown as one that does not parse**
/// ([`unparsed`]). A hand-edited one that lost its scheme but has a colon
/// before its first `/` parses all the same, as an opaque URL whose scheme is
/// whatever came before the colon: `alice:hunter2@n8n.local/hook` has the
/// scheme `alice` and the path `hunter2@n8n.local/hook`, and
/// `hooks.slack.com:443/services/...` the scheme `hooks.slack.com`. There is
/// no userinfo to clear and no host to judge the path by, so the rules above
/// would show it as it came, password or secret path included.
pub fn shown(url: &Url) -> String {
    if url.host_str().is_none() {
        return unparsed(url.as_str());
    }
    let mut clean = url.clone();
    // Both calls fail only on a URL that cannot have a host, which is shown
    // above as one that does not parse.
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    if clean.query().is_some_and(|query| !query.is_empty()) {
        clean.set_query(Some(HIDDEN));
    } else {
        clean.set_query(None);
    }
    clean.set_fragment(None);

    if let Some(marker) = clean.host_str().and_then(secret_path) {
        let segments: Vec<&str> = clean.path().split('/').filter(|s| !s.is_empty()).collect();
        let kept = marker
            .and_then(|marker| segments.iter().position(|s| *s == marker))
            .map_or_else(String::new, |at| format!("/{}", segments[..=at].join("/")));
        clean.set_path(&format!("{kept}/{HIDDEN}"));
    }
    clean.to_string()
}

/// Whether `host` is a receiver whose path is its credential, and if so, the
/// segment of the path up to which it may be shown (none: no part of it).
fn secret_path(host: &str) -> Option<Option<&'static str>> {
    match host {
        "hooks.slack.com" => Some(Some("services")),
        "discord.com" | "discordapp.com" | "canary.discord.com" | "ptb.discord.com" => {
            Some(Some("webhooks"))
        }
        "hooks.zapier.com" => Some(Some("catch")),
        "maker.ifttt.com" => Some(Some("trigger")),
        _ if host.ends_with(".webhook.office.com") => Some(Some("webhookb2")),
        _ if host.starts_with("hook.") && host.ends_with(".make.com") => Some(None),
        _ => None,
    }
}

/// [`shown`], asked of an address held as a string (the outbox's, the file's).
///
/// **An address that does not parse is never shown as it came**: `shown`'s
/// rules cannot be applied to what has no parts, and the likeliest such
/// address, one hand-edited without its scheme, is a Slack or Discord one
/// whose path is the secret. Which address was meant is still the content
/// of the line, so its host is kept, as far as the first `/`, `?`, `#`, a
/// backslash or a port's `:`, with what came before an `@` and after the
/// host left out.
///
/// **An `@` past that first `/`, `?`, `#` or backslash hides the host too.**
/// A password holding one of those characters unencoded is what makes such
/// an address fail to parse (`https://alice:hunter/2@n8n.local/hook`), and
/// cut there the "host" would be the user and the start of the password;
/// the `@` might as well be a path's, and what follows it the end of a
/// secret path, so nothing around it is shown.
pub fn shown_str(address: &str) -> String {
    Url::parse(address).map_or_else(|_| unparsed(address), |url| shown(&url))
}

/// The host of an address that does not parse, as [`shown_str`] says.
fn unparsed(address: &str) -> String {
    // A scheme only where what precedes `://` could be one, so that a `://`
    // later in a path does not make the path a scheme.
    let (scheme, rest) = match address.split_once("://") {
        Some((scheme, rest))
            if scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (format!("{scheme}://"), rest)
        }
        _ => (String::new(), address),
    };
    let end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let (authority, after) = rest.split_at(end);
    let host = if after.contains('@') {
        HIDDEN
    } else {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        // A port is no part of which address was meant, and in an address
        // that lost its scheme what follows a colon may be a password.
        match host.strip_prefix('[') {
            Some(inside) => inside
                .split_once(']')
                .map_or(HIDDEN, |(literal, _)| &host[..literal.len() + 2]),
            None => host.split_once(':').map_or(host, |(name, _)| name),
        }
    };
    let tail = if end < rest.len() {
        format!("/{HIDDEN}")
    } else {
        String::new()
    };
    format!("{scheme}{host}{tail}")
}

/// The same refusals, asked of a configuration file rather than of a built
/// client, and without touching the keyring.
///
/// For `snob watch status`, which builds no delivery: a hand-edited
/// `url = "n8n.local/hook"` with no scheme kills every run at the parse,
/// before `record_failed_run` is reached, so without this `status` would say
/// "it has not run yet" with exit 0 while printing the address as though it
/// were fine, and disagree with `snob watch check` about the same file.
///
/// Config-only on purpose. A missing token is a different question and one the
/// keyring answers; what this asks is whether the address could work at all,
/// which is a property of the file and available to a probe that has opened
/// nothing.
pub fn problem_with_config(
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let url = match Url::parse(url) {
        Ok(url) => url,
        Err(e) => return Some(format!("the configured address cannot be used: {e}")),
    };
    let webhook = Webhook {
        url,
        headers: headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        key: None,
    };
    check(&webhook).err().map(|e| e.to_string())
}

/// Refuses a destination that should not be posted to before anything is.
///
/// Checked when the address is given rather than at the first run, so a service
/// that would have been shouting a token into the open fails at the moment
/// somebody can still read the message.
pub fn check(webhook: &Webhook) -> Result<()> {
    // A password in the address is a credential in a plain-text file, which is
    // the one thing `watch.toml` promises not to hold — and `reqwest` turns it
    // into a second `Authorization` header, so it also collides with the one
    // the user configured. It would be echoed by `status` and written into the
    // log of an unattended service by the refusal below.
    if !webhook.url.username().is_empty() || webhook.url.password().is_some() {
        bail!(
            "the address carries a username or password. Put the credential in a header \
             instead -- \"snob watch setup\" stores one in the keyring -- so it is not sitting \
             in a configuration file and in every log line that names the address."
        );
    }

    // Before the scheme, because it is a property of the destination and not of
    // the transport: `https://169.254.169.254/` is refused as surely as the
    // `http://` spelling.
    if is_the_metadata_service(&webhook.url) {
        bail!(
            "{} is the cloud instance metadata service, which answers to anything that can \
             reach it and hands back the machine's own credentials. snob will not post \
             to it over any scheme.",
            shown(&webhook.url)
        );
    }

    match webhook.url.scheme() {
        "https" => {}
        "http" if is_private(&webhook.url) => {}
        "http" => bail!(
            "{} is not encrypted, and it is not on a private network.\n\
             The report carries account names, and any header you configured -- a token, \
             usually -- travels with it in the clear. Use https, or an address on your own \
             network.",
            shown(&webhook.url)
        ),
        other => bail!("\"{other}\" is not an address this can post to; use https"),
    }

    // Headers the user does not get to set.
    //
    // Two kinds, and both are refused here rather than resolved silently at the
    // point of sending. `X-Snob-*` is the protocol: the signature covers bytes
    // this tool computed, `X-Snob-Event` is what a receiver routes on without
    // parsing the body, and `X-Snob-Delivery` is what it deduplicates on — a
    // configured one of those makes the value ambiguous, and most frameworks
    // join duplicates with ", ".
    //
    // The rest frame the message, and `Content-Length` is the one that matters:
    // hyper honors a caller-supplied one in preference to measuring the body, so
    // `Content-Length = "0"` would send the POST with no body at all while
    // `X-Snob-Signature` still claimed the whole document. A receiver that
    // verifies rejects every attempt; one that does not silently ingests an
    // empty report of changes.
    //
    // `Content-Type` is deliberately **not** here. Asking for
    // `application/json; charset=utf-8` is an ordinary thing to want, and `post`
    // builds one map so the user's value replaces snob's rather than traveling
    // beside it.
    const FRAMING: [&str; 5] = [
        "content-length",
        "transfer-encoding",
        "connection",
        "host",
        "expect",
    ];
    for (name, _) in &webhook.headers {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("x-snob-") {
            bail!(
                "{} is part of what snob sends and cannot be configured",
                snob_core::model::printable(name)
            );
        }
        if FRAMING.contains(&lower.as_str()) {
            bail!(
                "{} is set by the transport and cannot be configured: a wrong one truncates or \
                 empties the report while the signature still covers all of it.",
                snob_core::model::printable(name)
            );
        }
    }

    // Every header actually has to be one, built here and not first at the
    // POST: `--header "X Token: abc"`, an empty name, or a value with a newline
    // in it would otherwise pass, and then every POST would die inside
    // reqwest's builder before a socket opened. The report would be queued,
    // retried against an error no waiting can fix, and expire.
    for (name, value) in &webhook.headers {
        if reqwest::header::HeaderName::try_from(name.as_str()).is_err() {
            bail!(
                "\"{}\" is not a header name (they may not contain spaces or punctuation \
                 beyond \"-\")",
                snob_core::model::printable(name)
            );
        }
        if reqwest::header::HeaderValue::try_from(value.as_str()).is_err() {
            bail!(
                "the value of \"{}\" is not one a header can carry (a line break, most likely)",
                snob_core::model::printable(name)
            );
        }
        // An empty value, for the same reason `--sign-with ""` is refused: it is
        // `--header "Authorization: ${SNOB_TOKEN}"` in a unit file whose
        // variable is not set. Taken literally it sends an empty
        // `Authorization`, and because a header given by name is what stops the
        // keyring token being attached, it *replaces* the stored token with
        // nothing. The receiver answers 401 to every retry until the report
        // expires, and the change it carried is gone.
        if value.trim().is_empty() {
            bail!(
                "\"{}\" was given an empty value. If that came from an environment variable that \
                 is not set, leave the header out: a header with nothing in it replaces whatever \
                 \"snob watch setup\" stored under the same name.",
                snob_core::model::printable(name)
            );
        }
    }
    Ok(())
}

/// Whether an address is somewhere plain HTTP is a reasonable thing to speak.
///
/// Loopback, the three private IPv4 ranges, IPv6 unique-local and link-local,
/// and the `.local` and `.internal` suffixes a homelab uses. Anything else is
/// the open internet as far as this can tell, and a token does not go there
/// unencrypted.
///
/// Never the metadata service, though some of its spellings fall inside the
/// ranges above: `check` refuses it before asking this, and the answer also
/// picks the proxy in [`WebhookClient::new`].
fn is_private(url: &Url) -> bool {
    if is_the_metadata_service(url) {
        return false;
    }
    // `url.host()` rather than `host_str()`. The string form of an IPv6 address
    // keeps the brackets the URL syntax requires — `[::1]` — which does not
    // parse as an address. The typed host tells the three cases apart.
    match url.host() {
        Some(url::Host::Ipv4(v4)) => is_private_v4(v4),
        Some(url::Host::Ipv6(v6)) => {
            // An IPv4-mapped address is an IPv4 address written the long way,
            // so `[::ffff:127.0.0.1]` is loopback.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            // `is_unique_local` and `is_unicast_link_local` are still unstable,
            // so the prefixes are matched directly: fc00::/7 and fe80::/10.
            let first = v6.segments()[0];
            // `[::]` is what `0.0.0.0` is, for the reason `is_private_v4`
            // gives.
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
        Some(url::Host::Domain(host)) => {
            // One trailing dot is the fully-qualified form of the same name,
            // and `n8n.local.` resolves exactly where `n8n.local` does.
            let host = host.to_ascii_lowercase();
            let host = host.strip_suffix('.').unwrap_or(&host);
            host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host.ends_with(".internal")
                || host.ends_with(".home.arpa")
        }
        None => false,
    }
}

/// Whether an address is the cloud instance metadata service, however spelled.
///
/// The one destination refused outright rather than by scheme. It answers to
/// anything that can reach it, with no authentication of any kind, and what it
/// answers with is the machine's own role credentials — so a webhook pointed
/// at it is not a delivery, it is a request the user did not mean to make.
///
/// Every spelling is asked here, because several of them look private:
/// 169.254.169.254 is link-local, `fd00:ec2::254` sits inside fc00::/7, and
/// `metadata.google.internal` carries the `.internal` suffix a homelab uses.
fn is_the_metadata_service(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(v4)) => v4 == METADATA_V4,
        Some(url::Host::Ipv6(v6)) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4 == METADATA_V4;
            }
            METADATA_V6.contains(&v6)
        }
        Some(url::Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            let host = host.strip_suffix('.').unwrap_or(&host);
            METADATA_HOSTS.contains(&host)
        }
        None => false,
    }
}

/// The cloud metadata service, by name. `metadata.google.internal` resolves to
/// 169.254.169.254: a denylist of one spelling is not a denylist.
const METADATA_HOSTS: [&str; 3] = ["metadata", "metadata.google.internal", "metadata.goog"];

/// The cloud metadata service, over IPv6.
const METADATA_V6: [std::net::Ipv6Addr; 2] = [
    // AWS's documented IMDS over IPv6.
    std::net::Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254),
    // The link-local form of 169.254.169.254.
    std::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0xa9fe, 0xa9fe),
];

/// Whether an IPv4 address is somewhere plain HTTP is reasonable.
///
/// `0.0.0.0` is treated as loopback: it means "this host" and nothing routes to
/// it, so refusing it only makes the local case harder to write.
fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
}

/// The cloud instance metadata endpoint, on every provider that has one.
///
/// It is link-local, and the rest of that range is allowed, but it answers
/// credentials to anything that asks. A mistyped address is one thing; a
/// mistyped address that POSTs the body and the stored token at the hypervisor
/// is another.
const METADATA_V4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(169, 254, 169, 254);

#[cfg(test)]
mod tests {
    use super::*;

    fn webhook(url: &str) -> Webhook {
        Webhook {
            url: Url::parse(url).unwrap(),
            headers: vec![],
            key: None,
        }
    }

    /// A homelab webhook is `http://` on the local network, and refusing that
    /// would make the feature unusable where it is most wanted.
    #[test]
    fn plain_http_is_allowed_on_a_private_network() {
        for url in [
            "http://localhost:5678/webhook",
            "http://127.0.0.1:5678/hook",
            "http://192.168.1.5/hook",
            "http://10.0.0.9/hook",
            "http://172.16.4.1/hook",
            "http://n8n.local/webhook/snob",
            "http://box.home.arpa/hook",
            "http://[::1]:5678/hook",
            // The same three written the other ways people write them.
            "http://n8n.local./webhook/snob",
            "http://[::ffff:127.0.0.1]:5678/hook",
            "http://0.0.0.0:5678/hook",
        ] {
            assert!(check(&webhook(url)).is_ok(), "{url} should be allowed");
        }
    }

    /// Link-local, and allowed by that rule — but it is the cloud metadata
    /// endpoint, which answers credentials to whatever asks. A typo that sends
    /// the body and the stored token to the hypervisor is not a typo worth
    /// being permissive about.
    #[test]
    fn the_cloud_metadata_endpoint_is_refused_though_it_is_link_local() {
        assert!(check(&webhook("http://169.254.169.254/hook")).is_err());
        // The rest of the range is still fine.
        assert!(check(&webhook("http://169.254.4.4/hook")).is_ok());
    }

    /// And over https as well, because it is the destination that is refused.
    #[test]
    fn the_metadata_endpoint_is_refused_whatever_the_scheme() {
        for url in [
            "https://169.254.169.254/hook",
            "https://metadata.google.internal/hook",
            "https://metadata/hook",
            "https://[fd00:ec2::254]/hook",
            "https://[::ffff:169.254.169.254]/hook",
            "http://169.254.169.254/hook",
        ] {
            assert!(
                check(&webhook(url)).is_err(),
                "{url} answers with the machine's own credentials"
            );
        }
    }

    /// A password in the address is refused, and the refusal does not repeat it:
    /// it would be written into the log of an unattended service.
    #[test]
    fn a_refusal_does_not_print_the_credential_it_is_refusing() {
        let error = check(&webhook("https://alice:hunter2@example.test/hook"))
            .expect_err("userinfo is refused")
            .to_string();
        assert!(!error.contains("hunter2"), "{error}");

        // And the same for the address a scheme refusal names.
        let plain = check(&webhook("http://alice:hunter2@example.test/hook"))
            .expect_err("plain http to a public host is refused")
            .to_string();
        assert!(!plain.contains("hunter2"), "{plain}");
    }

    /// Cleared, not replaced: which address was meant is the content of every
    /// message that names one.
    #[test]
    fn the_shown_address_keeps_everything_but_the_credential() {
        let shown = shown(&Url::parse("https://alice:hunter2@host.test/hook").unwrap());
        assert_eq!(shown, "https://host.test/hook");
    }

    /// A query can be the credential (Google Chat's `key` and `token`, a
    /// Power Automate trigger's `sig`), so it is said to be there and no
    /// more, and a fragment goes.
    #[test]
    fn the_shown_address_says_there_is_a_query_and_not_what_it_is() {
        let chat = Url::parse(
            "https://chat.googleapis.com/v1/spaces/AAAA/messages?key=K-secret&token=T-secret",
        )
        .unwrap();
        assert_eq!(
            shown(&chat),
            "https://chat.googleapis.com/v1/spaces/AAAA/messages?..."
        );
        let flow = Url::parse("https://host.test/hook?sp=x&sig=S-secret#part").unwrap();
        assert_eq!(shown(&flow), "https://host.test/hook?...");
        let empty = Url::parse("https://host.test/hook?#").unwrap();
        assert_eq!(shown(&empty), "https://host.test/hook");
    }

    /// An address held as a string loses the same parts, and one that does not
    /// parse is never shown as it came, only as far as its host.
    #[test]
    fn an_address_held_as_a_string_is_shown_the_same_way() {
        assert_eq!(
            shown_str("https://alice:hunter2@host.test/hook"),
            "https://host.test/hook"
        );
        assert_eq!(
            shown_str("https://hooks.slack.com/services/T0000/B0000/XXXXYYYY"),
            "https://hooks.slack.com/services/..."
        );
        assert_eq!(shown_str("n8n.local/hook"), "n8n.local/...");
        assert_eq!(
            shown_str("hooks.slack.com/services/T0000/B0000/XXXXYYYY"),
            "hooks.slack.com/..."
        );
        assert_eq!(shown_str("n8n.local"), "n8n.local");
        // What came before an `@` is somebody's credential, a `://` in the
        // path is no scheme, and a host that does not parse is still a host.
        assert_eq!(
            shown_str("https://alice:hunter2@exa mple.test/hook?key=K"),
            "https://exa mple.test/..."
        );
        assert_eq!(
            shown_str("hooks.slack.com/services/T0/x://B0/XXXXYYYY"),
            "hooks.slack.com/..."
        );
        assert_eq!(
            shown_str("hooks.slack.com\\services\\XXXXYYYY"),
            "hooks.slack.com/..."
        );
        // A password holding a `/`, `?` or `#` unencoded is why the address
        // does not parse, and none of it is shown, nor the user.
        for address in [
            "https://alice:hunter/2xyz@n8n.local/hook",
            "https://alice:hun?ter2@n8n.local/hook",
            "https://alice:hun#ter2@n8n.local/hook",
        ] {
            let shown = shown_str(address);
            assert_eq!(shown, "https://.../...", "{address}");
        }
        // A port is left out, and so is whatever follows a colon in an
        // address with no scheme; an address literal is kept whole.
        assert_eq!(
            shown_str("https://n8n.local:99999/hook"),
            "https://n8n.local/..."
        );
        assert_eq!(
            shown_str("https://[fd00::1]:99999/hook"),
            "https://[fd00::1]/..."
        );
    }

    /// An address that lost its scheme but has a colon before its first `/`
    /// parses, as an opaque URL with no host, and is shown as one that does
    /// not: its "scheme" is a user or a host, and its "path" a password or a
    /// secret path.
    #[test]
    fn an_address_with_no_host_is_shown_as_one_that_does_not_parse() {
        for (address, expected) in [
            ("alice:hunter2@n8n.local/hook", "n8n.local/..."),
            (
                "hooks.slack.com:443/services/T0/B0/XXXXsecret",
                "hooks.slack.com/...",
            ),
        ] {
            let url = Url::parse(address).expect("an opaque URL parses");
            assert!(url.cannot_be_a_base(), "{address}");
            assert_eq!(shown(&url), expected, "{address}");
            assert_eq!(shown_str(address), expected, "{address}");
            let said = without_address(&format!("no answer from {address}"), &url);
            assert_eq!(said, format!("no answer from {expected}"));
        }
    }

    /// Where the path is the credential, the path is what goes.
    #[test]
    fn a_webhook_whose_path_is_the_secret_is_shown_without_it() {
        let slack = Url::parse("https://hooks.slack.com/services/T0000/B0000/XXXXYYYY").unwrap();
        assert_eq!(shown(&slack), "https://hooks.slack.com/services/...");
        let discord =
            Url::parse("https://discord.com/api/webhooks/123456/abcdef-token?wait=true").unwrap();
        assert_eq!(shown(&discord), "https://discord.com/api/webhooks/...?...");
        let zapier = Url::parse("https://hooks.zapier.com/hooks/catch/123/abc-key/").unwrap();
        assert_eq!(shown(&zapier), "https://hooks.zapier.com/hooks/catch/...");
        let ifttt = Url::parse("https://maker.ifttt.com/trigger/run/with/key/abc-key").unwrap();
        assert_eq!(shown(&ifttt), "https://maker.ifttt.com/trigger/...");
        let teams = Url::parse(
            "https://contoso.webhook.office.com/webhookb2/1@2/IncomingWebhook/abc-key/3",
        )
        .unwrap();
        assert_eq!(
            shown(&teams),
            "https://contoso.webhook.office.com/webhookb2/..."
        );
        // Make's path is the token and nothing else, and a path on one of
        // those hosts without the segment that names the service goes whole.
        let make = Url::parse("https://hook.eu1.make.com/abc-token").unwrap();
        assert_eq!(shown(&make), "https://hook.eu1.make.com/...");
        let odd = Url::parse("https://hooks.slack.com/T0000/B0000/XXXXYYYY").unwrap();
        assert_eq!(shown(&odd), "https://hooks.slack.com/...");
        // Anybody else's path is the address, and stays.
        let own = Url::parse("https://n8n.local/webhook/snob").unwrap();
        assert_eq!(shown(&own), "https://n8n.local/webhook/snob");
    }

    /// A report that found nobody listening says why, and not where: the
    /// failure is stored and printed on every retry, and a webhook's path can
    /// be its credential.
    #[tokio::test]
    async fn a_request_that_got_no_answer_does_not_name_the_address() {
        // Something that takes the connection and closes it unanswered, so
        // the failure is the client's and comes from no server's words.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });

        let client = WebhookClient::new(webhook(&format!(
            "http://127.0.0.1:{port}/api/webhooks/123456/a-secret-token"
        )))
        .unwrap();
        let attempt = client.post("{}", "watch.changes", "run-1", 1).await;

        let Attempt::Failed {
            status: None,
            error,
        } = &attempt
        else {
            panic!("no answer is a failure worth retrying: {attempt:?}");
        };
        assert!(!error.contains("a-secret-token"), "{error}");
        assert!(!error.contains("for url"), "{error}");
        assert!(
            error.starts_with("error sending request: "),
            "the cause is said after the top line: {error}"
        );
    }

    /// Whatever a failure's words carry, a Slack or Discord address comes out
    /// as [`shown`] has it, the path on its own included.
    #[test]
    fn a_failure_never_names_a_secret_address() {
        let discord = Url::parse("https://discord.com/api/webhooks/123456/abcdef-token").unwrap();

        // reqwest's own words name the address it was given.
        let error = reqwest::Client::new()
            .get("not an address")
            .build()
            .expect_err("that is no address")
            .with_url(discord.clone());
        assert!(error.to_string().contains("abcdef-token"), "{error}");
        let said = unanswered(error, &discord);
        assert!(!said.contains("abcdef-token"), "{said}");

        // And text this process did not write, a receiver's answer echoing
        // the address or its path, loses it too.
        let echoed = without_address(
            "no webhook at https://discord.com/api/webhooks/123456/abcdef-token \
             (POST /api/webhooks/123456/abcdef-token)",
            &discord,
        );
        assert_eq!(
            echoed,
            "no webhook at https://discord.com/api/webhooks/... \
             (POST /api/webhooks/...)"
        );

        // A query is the credential on others, and goes the same way, the
        // address whole and the parameters echoed on their own.
        let chat =
            Url::parse("https://chat.googleapis.com/v1/spaces/A/messages?key=K1&token=T1").unwrap();
        assert_eq!(
            without_address(
                "403 for https://chat.googleapis.com/v1/spaces/A/messages?key=K1&token=T1 \
                 (key=K1&token=T1)",
                &chat,
            ),
            "403 for https://chat.googleapis.com/v1/spaces/A/messages?... (...)"
        );

        // An address that hides nothing is left as it was.
        let own = Url::parse("https://n8n.local/webhook/snob").unwrap();
        assert_eq!(
            without_address("https://n8n.local/webhook/snob is down", &own),
            "https://n8n.local/webhook/snob is down"
        );
    }

    /// `[::]` is what `0.0.0.0` is, and both are accepted.
    #[test]
    fn the_unspecified_address_is_this_host_in_both_families() {
        assert!(check(&webhook("http://0.0.0.0:8787/hook")).is_ok());
        assert!(check(&webhook("http://[::]:8787/hook")).is_ok());
    }

    /// `is_private` never answers yes for the metadata service, by any
    /// spelling, while the private neighbors of those spellings stay private.
    /// `check` refuses the service before asking, so only a direct call sees
    /// this answer; the proxy choice in `WebhookClient::new` reads it too.
    #[test]
    fn the_metadata_service_is_never_private() {
        for url in [
            "http://169.254.169.254/",
            "http://[::ffff:169.254.169.254]/",
            "http://[fd00:ec2::254]/",
            "http://[fe80::a9fe:a9fe]/",
            "http://metadata/",
            "http://metadata.google.internal/",
            "http://metadata.google.internal./",
            "http://METADATA.GOOGLE.INTERNAL/",
            "http://metadata.goog/",
        ] {
            assert!(!is_private(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "http://169.254.4.4/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://n8n.internal/",
        ] {
            assert!(is_private(&Url::parse(url).unwrap()), "{url}");
        }
    }

    /// And by every spelling, not only the one nobody types: GCP publishes
    /// `metadata.google.internal`, which carries the `.internal` suffix allowed
    /// for homelabs, and AWS publishes `fd00:ec2::254`, which sits inside the
    /// fc00::/7 range.
    #[test]
    fn the_metadata_endpoint_is_refused_by_name_and_over_ipv6() {
        for url in [
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.google.internal./computeMetadata/v1/",
            "http://METADATA.GOOGLE.INTERNAL/computeMetadata/v1/",
            "http://metadata/computeMetadata/v1/",
            "http://metadata.goog/computeMetadata/v1/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "http://[fe80::a9fe:a9fe]/latest/meta-data/",
        ] {
            assert!(check(&webhook(url)).is_err(), "{url} was allowed");
        }

        // And the homelab addresses those branches exist for still work.
        for url in [
            "http://n8n.internal/hook",
            "http://[fd00::1]/hook",
            "http://[fe80::1]/hook",
        ] {
            assert!(check(&webhook(url)).is_ok(), "{url} was refused");
        }
    }

    /// A password in the address is a credential in a plain-text file, which is
    /// what the configuration promises not to hold — and it would be echoed
    /// into an unattended service's log by the refusal path.
    #[test]
    fn a_credential_in_the_address_is_refused() {
        for url in [
            "https://alice:s3cret@example.com/hook",
            "https://token@example.com/hook",
        ] {
            let error = check(&webhook(url)).unwrap_err();
            assert!(
                error.to_string().contains("username or password"),
                "{error}"
            );
        }
    }

    /// A header that reqwest cannot build is refused where the address is, not
    /// discovered when every POST dies in the builder and the report is retried
    /// against an error no waiting can fix.
    #[test]
    fn a_header_that_could_never_be_sent_is_refused_up_front() {
        for (name, value) in [
            ("X Token", "abc"),
            ("", "abc"),
            ("X-Token", "line\r\nInjected: yes"),
        ] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push((name.into(), value.into()));
            assert!(
                check(&hook).is_err(),
                "\"{name}: {value}\" should be refused"
            );
        }
    }

    /// And refused anywhere else, because the token travels with it.
    #[test]
    fn plain_http_to_the_open_internet_is_refused_with_the_reason() {
        let error = check(&webhook("http://example.com/hook")).unwrap_err();
        assert!(error.to_string().contains("not encrypted"), "{error}");

        for url in ["http://8.8.8.8/hook", "http://n8n.example.com/hook"] {
            assert!(check(&webhook(url)).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn https_is_allowed_anywhere() {
        assert!(check(&webhook("https://n8n.example.com/webhook/snob")).is_ok());
    }

    #[test]
    fn a_scheme_that_is_not_http_is_refused() {
        for url in ["ftp://host/x", "file:///tmp/x"] {
            assert!(check(&webhook(url)).is_err(), "{url} should be refused");
        }
    }

    /// A configured header that replaced the signature would leave the receiver
    /// verifying something this tool did not compute — which is worse than no
    /// signature, because it looks like one.
    #[test]
    fn a_configured_header_cannot_replace_the_signature() {
        let mut hook = webhook("https://example.com/hook");
        hook.headers
            .push(("x-snob-signature".into(), "anything".into()));

        let error = check(&hook).unwrap_err();
        assert!(
            error.to_string().contains("part of what snob sends"),
            "{error}"
        );
    }

    /// Nor any other header the protocol owns.
    ///
    /// `X-Snob-Event` is what a receiver routes on without parsing the body and
    /// `X-Snob-Delivery` is what it deduplicates on; a configured one of either
    /// would go out beside snob's, and most frameworks join duplicates with ", ".
    #[test]
    fn a_configured_header_cannot_replace_any_of_the_protocol_ones() {
        for name in ["X-Snob-Event", "x-snob-delivery", "X-Snob-Attempt"] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push((name.into(), "mine".into()));
            assert!(check(&hook).is_err(), "{name} was accepted");
        }
    }

    /// A header that frames the message is not the user's to set.
    ///
    /// `Content-Length = "0"` would send the POST with no body at all while the
    /// signature still claimed the whole document.
    #[test]
    fn a_header_that_frames_the_message_is_refused() {
        for name in ["Content-Length", "transfer-encoding", "Host", "Connection"] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push((name.into(), "0".into()));
            let error = check(&hook).unwrap_err();
            assert!(
                error.to_string().contains("set by the transport"),
                "{name}: {error}"
            );
        }
    }

    /// An empty value is a variable that was not set, and it would silently
    /// replace the stored token with nothing: a header given by name is what
    /// stops the keyring token being attached. `--sign-with ""` is refused for
    /// the same reason.
    #[test]
    fn a_header_with_an_empty_value_is_refused() {
        for value in ["", "   "] {
            let mut hook = webhook("https://example.com/hook");
            hook.headers.push(("Authorization".into(), value.into()));
            let error = check(&hook).unwrap_err();
            assert!(error.to_string().contains("empty value"), "{error}");
        }
    }

    #[test]
    fn an_ordinary_header_is_fine() {
        let mut hook = webhook("https://example.com/hook");
        hook.headers
            .push(("Authorization".into(), "Bearer x".into()));
        assert!(check(&hook).is_ok());

        // Including one snob also sends: `post` builds a single map, so the
        // user's value replaces snob's instead of traveling beside it.
        let mut hook = webhook("https://example.com/hook");
        hook.headers.push((
            "Content-Type".into(),
            "application/json; charset=utf-8".into(),
        ));
        assert!(check(&hook).is_ok());
    }
}
