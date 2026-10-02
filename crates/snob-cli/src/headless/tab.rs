//! The tab requests are sent from: getting it somewhere, and sending from it.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use snob_ig::client::page::{PageError, PageRequest, PageResponse};
use snob_ig::page_values::{self, PageValues};

use super::{COMMAND_TIMEOUT, LOAD_TIMEOUT, Live, PAGE_WIRE_CAP, SETTLE, broken, origin_of};
use crate::cdp::Cdp;

/// Sends the tab somewhere, waits for it to finish loading, and gives the
/// app the moment it takes to start.
pub(super) async fn navigate(live: &mut Live, url: &str) -> Result<(), PageError> {
    navigate_tab(&live.cdp, &live.tab, url).await?;
    tokio::time::sleep(SETTLE).await;
    Ok(())
}

/// The navigation itself. A page that could not be reached, or never finished
/// loading, is the network's failure and says so; a protocol command that
/// failed is the browser's.
pub(super) async fn navigate_tab(cdp: &Cdp, tab: &str, url: &str) -> Result<(), PageError> {
    let went = cdp
        .page_call(tab, "Page.navigate", json!({ "url": url }), COMMAND_TIMEOUT)
        .await
        .map_err(broken)?;
    if let Some(error) = went.get("errorText").and_then(Value::as_str)
        && !error.is_empty()
    {
        return Err(PageError::Unreachable(format!(
            "the browser could not open {url}: {error}"
        )));
    }
    let deadline = tokio::time::Instant::now() + LOAD_TIMEOUT;
    loop {
        let ready = evaluate(cdp, tab, "document.readyState", COMMAND_TIMEOUT)
            .await
            .map_err(broken)?;
        if ready.as_str() == Some("complete") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(PageError::Unreachable(format!(
                "{url} did not finish loading"
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A navigation whose answer is the document it lands on.
///
/// **The status and the address are the ones the browser saw**, not assumed:
/// a 429 on the page a write reads its tokens from, or a redirect to the login
/// or challenge page, has to reach the client as what it is, not as a page
/// with no tokens in it. The navigation's own timing entry carries the status
/// (Chromium 109 and later) and how many redirects led to it; a browser too
/// old to say is taken at its word that the page loaded. Read from the
/// isolated world, like every request, where the page's scripts cannot see it.
pub(super) async fn navigate_and_read(
    live: &mut Live,
    url: &str,
) -> Result<PageResponse, PageError> {
    navigate(live, url).await?;
    let world = isolated_world(live).await?;
    let page = evaluate_in(
        &live.cdp,
        &live.tab,
        Some(world),
        &format!(
            "(() => {{ const n = performance.getEntriesByType('navigation')[0] || {{}}; \
             const body = document.documentElement.outerHTML; {WIRE_LENGTH} \
             const tooLarge = wire(body) > {PAGE_WIRE_CAP}; \
             return {{ url: location.href, status: n.responseStatus || 0, \
             hops: n.redirectCount || 0, body: tooLarge ? '' : body, tooLarge }}; }})()"
        ),
        COMMAND_TIMEOUT,
    )
    .await
    .map_err(broken)?;
    let landed = page
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or(url)
        .to_string();
    live.origin = origin_of(&landed).map_err(broken)?;
    let status = page
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| *s != 0)
        .unwrap_or(200);
    let hops = page
        .get("hops")
        .and_then(Value::as_u64)
        .and_then(|h| u32::try_from(h).ok())
        .unwrap_or(0);
    Ok(PageResponse {
        status,
        headers: Vec::new(),
        body: page
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        redirected: hops > 0 || !same_document(url, &landed),
        hops,
        url: landed,
        too_large: page
            .get("tooLarge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// How many bytes a string takes as a protocol message: six for every
/// character outside printable ASCII, which the browser writes as `\uXXXX`,
/// two for a quote or a backslash, one for the rest. Declared into the
/// scripts that hand a body back, so the cap is measured in what it protects.
const WIRE_LENGTH: &str = "const wire = (t) => { let n = t.length; \
    for (let i = 0; i < t.length; i++) { const c = t.charCodeAt(i); \
    if (c < 0x20 || c > 0x7e) n += 5; else if (c === 34 || c === 92) n += 1; } \
    return n; };";

/// Whether two addresses name the same document: the fragment aside, which a
/// page is free to change without anything having been requested.
fn same_document(asked: &str, landed: &str) -> bool {
    let strip = |u: &str| {
        url::Url::parse(u).ok().map(|mut u| {
            u.set_fragment(None);
            u
        })
    };
    strip(asked) == strip(landed)
}

/// The script that sends one request from the page.
///
/// **It fills in headers, and never adds one.** The app's families differ in
/// which of the two they carry (Relay never the claim, a route call neither),
/// so only a request that already names one gets it. `X-CSRFToken` is read
/// from the cookie at the moment of sending, because the browser rotates it
/// and this process never sees the rotation; with no cookie it is left off a
/// GET and a POST is refused. The claim is the page's when it keeps one —
/// the app on the tab and this script both keep it current from every
/// answer — and the one this process passed otherwise. It hands back which
/// of the two, or `0`, went out, and whether the answer handed out a new
/// one, never the claim itself ([`note_the_claim`]).
///
/// **No redirect is followed, GET or POST.** A hop the browser follows by
/// itself is a request sent before this process could pay for it, and to
/// wherever the answer pointed — so it could only be charged afterwards and
/// held to the origin rule after it had already gone. Instagram's API does
/// not redirect a working call; the one it is known to redirect is a dead
/// session's, to the login page. With `manual` that arrives as an opaque
/// redirect, status 0, and is refused without a second request having been
/// made. For a POST a followed redirect would be a write sent twice.
const FETCH: &str = r#"(async (q) => {
  const headers = new Headers(q.headers);
  if (headers.has('X-CSRFToken')) {
    const csrf = document.cookie.match(/(?:^|;\s*)csrftoken=([^;]*)/);
    if (csrf) headers.set('X-CSRFToken', decodeURIComponent(csrf[1]));
    else if (q.method !== 'GET') return { error: 'no CSRF token', kind: 'csrf' };
    else headers.delete('X-CSRFToken');
  }
  let claim = null;
  if (headers.has('X-IG-WWW-Claim')) {
    claim = headers.get('X-IG-WWW-Claim') === '0' ? '0' : 'snob';
    try {
      const kept = sessionStorage.getItem('www-claim-v2');
      if (kept) { headers.set('X-IG-WWW-Claim', kept); claim = 'page'; }
    } catch (e) {}
  }
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), q.timeout_ms);
  try {
    const r = await fetch(q.url, {
      method: q.method,
      headers,
      body: q.body === null ? undefined : q.body,
      referrer: q.referrer,
      redirect: 'manual',
      signal: abort.signal,
    });
    const fresh = r.headers.get('x-ig-set-www-claim');
    if (fresh) { try { sessionStorage.setItem('www-claim-v2', fresh); } catch (e) {} }
    const list = [];
    r.headers.forEach((value, name) => list.push([name, value]));
    const text = r.type === 'opaqueredirect' ? '' : await r.text();
    const tooLarge = wire(text) > q.cap;
    return {
      status: r.status, headers: list, body: tooLarge ? '' : text,
      url: r.url, redirected: r.redirected, tooLarge, claim, claimSet: !!fresh,
    };
  } catch (e) {
    return { error: String(e), kind: 'network' };
  } finally {
    clearTimeout(timer);
  }
})"#;

/// Writes down, for a request that carried a claim, which claim went out:
/// `page` (the one the page keeps under `www-claim-v2`), `snob` (one this
/// process kept from its own answers, the page keeping none) or `0`; and
/// whether the answer handed out a new one. Never the claim: the live check
/// reads from it whether the app's claim reaches snob's reads.
fn note_the_claim(answer: &Value) {
    let Some(sent) = answer.get("claim").and_then(Value::as_str) else {
        return;
    };
    let set = answer
        .get("claimSet")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    tracing::debug!(
        sent = %sent,
        set,
        "the claim a read from the tab carried, and whether its answer handed out one"
    );
}

/// Sends `request` from `world`, which [`isolated_world`] made.
pub(super) async fn fetch(
    live: &mut Live,
    world: i64,
    request: &PageRequest,
) -> Result<PageResponse, PageError> {
    let argument = json!({
        "method": request.method,
        "url": request.url,
        "headers": request.headers,
        "referrer": request.referrer,
        "body": request.body,
        "cap": request.cap.min(PAGE_WIRE_CAP),
        "timeout_ms": request.timeout_ms,
    });
    // Named, so the browser names it as the initiator of the request it
    // sends, and the listener does not read its answer a second time. The
    // name is seen by the protocol and nothing else: the script runs in a
    // world the page cannot reach.
    let expression = format!(
        "(() => {{ {WIRE_LENGTH} return {FETCH}({argument}); }})()\n//# sourceURL={}",
        super::listen::OWN_SCRIPT
    );
    let timeout = Duration::from_millis(request.timeout_ms) + Duration::from_secs(10);
    let answer = evaluate_in(&live.cdp, &live.tab, Some(world), &expression, timeout)
        .await
        .map_err(broken)?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        return Err(match answer.get("kind").and_then(Value::as_str) {
            Some("csrf") => PageError::NoCsrfToken,
            Some("network") => PageError::Unreachable(error.to_string()),
            _ => PageError::Browser(format!("the page could not send the request: {error}")),
        });
    }
    note_the_claim(&answer);
    Ok(PageResponse {
        status: answer
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(0),
        headers: answer
            .get("headers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_ascii_lowercase(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect(),
        body: answer
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: answer
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        redirected: answer
            .get("redirected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // `manual` follows none.
        hops: 0,
        too_large: answer
            .get("tooLarge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// The script that fetches one file, as the app's own page would, and hands it
/// back as base64.
///
/// **No cookie and no header of its own**: a cross-site `fetch` carries none of
/// the site's cookies, and the browser adds the rest. **No redirect is
/// followed**, for the reason [`FETCH`] gives: it arrives as an opaque redirect,
/// status 0, and the client asks the other way, which follows the CDN's own,
/// hop by hop. A file longer than `cap` comes back as `tooLarge` without being
/// read when its length says so.
const FETCH_ASSET: &str = r#"(async (q) => {
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), q.timeout_ms);
  try {
    const r = await fetch(q.url, { redirect: 'manual', signal: abort.signal });
    const list = [];
    r.headers.forEach((value, name) => list.push([name, value]));
    const answer = (extra) => Object.assign({
      status: r.status, headers: list, body: '', url: r.url,
      redirected: r.redirected, tooLarge: false,
    }, extra);
    if (r.type === 'opaqueredirect') return answer({});
    const length = Number(r.headers.get('content-length') || 0);
    if (length > q.cap) return answer({ tooLarge: true });
    const bytes = new Uint8Array(await r.arrayBuffer());
    if (bytes.length > q.cap) return answer({ tooLarge: true });
    let binary = '';
    for (let at = 0; at < bytes.length; at += 0x8000) {
      binary += String.fromCharCode.apply(null, bytes.subarray(at, at + 0x8000));
    }
    return answer({ body: btoa(binary) });
  } catch (e) {
    return { error: String(e), kind: 'network' };
  } finally {
    clearTimeout(timer);
  }
})"#;

/// Fetches `url` from `world`, which [`isolated_world`] made, and hands it back
/// as [`FETCH_ASSET`] does: the body base64. `cap` is in bytes of the file,
/// held to what a protocol message carries once it is base64.
pub(super) async fn fetch_asset(
    live: &mut Live,
    world: i64,
    url: &str,
    cap: u64,
    timeout_ms: u64,
) -> Result<PageResponse, PageError> {
    let argument = json!({
        "url": url,
        "cap": cap.min(PAGE_WIRE_CAP / 4 * 3),
        "timeout_ms": timeout_ms,
    });
    // Named as the request script is, so the listener does not take it for
    // the app's: the browser names this as the initiator of the fetch.
    let expression = format!(
        "({FETCH_ASSET})({argument})
//# sourceURL={}",
        super::listen::OWN_SCRIPT
    );
    let timeout = Duration::from_millis(timeout_ms) + Duration::from_secs(10);
    let answer = evaluate_in(&live.cdp, &live.tab, Some(world), &expression, timeout)
        .await
        .map_err(broken)?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        return Err(match answer.get("kind").and_then(Value::as_str) {
            Some("network") => PageError::Unreachable(error.to_string()),
            _ => PageError::Browser(format!("the page could not fetch the file: {error}")),
        });
    }
    Ok(PageResponse {
        status: answer
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .unwrap_or(0),
        headers: Vec::new(),
        body: answer
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: answer
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        redirected: answer
            .get("redirected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        hops: 0,
        too_large: answer
            .get("tooLarge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Runs an expression in the tab and hands back its value.
///
/// `Runtime.evaluate` without `Runtime.enable`: enabling the domain is what
/// makes the console report objects back over the pipe, and that reporting is
/// the side effect pages use to notice a debugger is attached.
pub(super) async fn evaluate(
    cdp: &Cdp,
    tab: &str,
    expression: &str,
    timeout: Duration,
) -> Result<Value> {
    evaluate_in(cdp, tab, None, expression, timeout).await
}

/// [`evaluate`], in a given world of the tab's document.
pub(super) async fn evaluate_in(
    cdp: &Cdp,
    tab: &str,
    world: Option<i64>,
    expression: &str,
    timeout: Duration,
) -> Result<Value> {
    let mut params = json!({
        "expression": expression,
        "awaitPromise": true,
        "returnByValue": true,
    });
    if let Some(world) = world {
        params["contextId"] = json!(world);
    }
    let result = cdp
        .page_call(tab, "Runtime.evaluate", params, timeout)
        .await?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .or_else(|| details.get("text").and_then(Value::as_str))
            .unwrap_or("an exception");
        bail!("the page threw: {text}");
    }
    Ok(result
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

#[cfg(test)]
thread_local! {
    /// Set by a test: the next `Page.getFrameTree` on this thread fails as
    /// one the browser never answered.
    pub(super) static STALL_FRAME_TREE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set by a test: claimed on the latch as that stall is met, as a
    /// push-back the listener heard on the page's own traffic just before.
    pub(super) static HEARD_BEFORE_STALL: std::cell::Cell<Option<snob_ig::client::page::PushedBack>> =
        const { std::cell::Cell::new(None) };
}

/// The world the requests are sent from, made once per document.
///
/// **Not the page's own.** `Runtime.evaluate` runs in the page's main world by
/// default, where the site's scripts run too: a `fetch` they have wrapped sees
/// every call snob makes, and the page's resource timing lists each one —
/// both measured. An isolated world shares the document, its cookies and its
/// storage, sends as the page's origin, and is out of the page's reach.
///
/// It dies with its document, and the document can change without snob
/// asking — the app reloads, or sends a dead session to the login page — so
/// the one kept is used only while the tab's document is still the one it was
/// made in, by the loader that document came from. The document's values
/// ([`PageValues`]) are read as each world is made, so they too are always
/// the current document's.
pub(super) async fn isolated_world(live: &mut Live) -> Result<i64, PageError> {
    #[cfg(test)]
    if STALL_FRAME_TREE.replace(false) {
        if let Some(cause) = HEARD_BEFORE_STALL.take() {
            live.listened.latch.claim(cause);
        }
        return Err(PageError::Browser(
            "the browser stopped answering (Page.getFrameTree)".to_string(),
        ));
    }
    let tree = live
        .cdp
        .page_call(&live.tab, "Page.getFrameTree", json!({}), COMMAND_TIMEOUT)
        .await
        .map_err(broken)?;
    let frame = tree
        .pointer("/frameTree/frame/id")
        .and_then(Value::as_str)
        .ok_or_else(|| PageError::Browser("the tab has no document".to_string()))?
        .to_string();
    let loader = tree
        .pointer("/frameTree/frame/loaderId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some((made_in, world)) = &live.world
        && *made_in == loader
    {
        return Ok(*world);
    }
    let made = live
        .cdp
        .page_call(
            &live.tab,
            "Page.createIsolatedWorld",
            json!({ "frameId": frame, "worldName": "" }),
            COMMAND_TIMEOUT,
        )
        .await
        .map_err(broken)?;
    let world = made
        .get("executionContextId")
        .and_then(Value::as_i64)
        .ok_or_else(|| PageError::Browser("the browser made no world".to_string()))?;
    live.world = Some((loader, world));
    live.page = page_values(&live.cdp, &live.tab, world).await;
    Ok(world)
}

/// How much of a module's definition is handed back: far more than the
/// largest the capture showed, and a bound on what one read carries.
const DEFINITION_REACH: usize = 32 * 1024;

/// The script that hands back, for each marker, the text of the first of
/// the document's inline scripts to contain it, from the marker on; `null`
/// where none does.
const DEFINITIONS: &str = r#"((markers, reach) => markers.map((marker) => {
  for (const script of document.scripts) {
    if (script.src) continue;
    const text = script.textContent;
    const at = text.indexOf(marker);
    if (at >= 0) return text.slice(at, at + reach);
  }
  return null;
}))"#;

/// The values the document in `world` carries, read from the scripts the
/// server wrote into it. A read that fails leaves them all absent: nothing
/// is built from a value the page was not seen to carry.
async fn page_values(cdp: &Cdp, tab: &str, world: i64) -> PageValues {
    let markers: Vec<String> = page_values::DEFINED
        .iter()
        .map(|name| page_values::marker(name))
        .collect();
    let expression = format!("{DEFINITIONS}({}, {DEFINITION_REACH})", json!(markers));
    let values = match evaluate_in(cdp, tab, Some(world), &expression, COMMAND_TIMEOUT).await {
        Ok(found) => PageValues::from_definitions(|name| {
            let at = page_values::DEFINED.iter().position(|n| *n == name)?;
            found.get(at)?.as_str()
        }),
        Err(e) => {
            tracing::debug!(error = %e, "could not read the page's values");
            PageValues::default()
        }
    };
    tracing::debug!(
        viewer = values.viewer.is_some(),
        site = values.site.is_some(),
        lsd = values.lsd.is_some(),
        relay_dtsg = values.relay_dtsg.is_some(),
        session_dtsg = values.session_dtsg.is_some(),
        bloks_version = values.bloks_version.is_some(),
        "read the values of a new document"
    );
    values
}

/// How much of the stories tray's preload is handed back, from its marker:
/// a tray of a few dozen reels, each with its owner and its latest item,
/// with room to spare.
const TRAY_REACH: usize = 512 * 1024;

/// Where the home document's preload of the stories tray starts.
const TRAY_MARKER: &str = r#""xdt_api__v1__feed__reels_tray""#;

/// The script that hands back the first inline script holding `marker`,
/// from the marker on, `reach` characters at most; `null` without one.
const TRAY: &str = r#"((marker, reach) => {
  for (const script of document.scripts) {
    if (script.src) continue;
    const text = script.textContent;
    const at = text.indexOf(marker);
    if (at >= 0) return text.slice(at, at + reach);
  }
  return null;
})"#;

/// The path of the tab's document, read from the isolated world: where the
/// tab really is. `None` when it cannot be read.
pub(super) async fn path_of(live: &mut Live) -> Option<String> {
    let world = isolated_world(live).await.ok()?;
    let path = evaluate_in(
        &live.cdp,
        &live.tab,
        Some(world),
        "location.pathname",
        COMMAND_TIMEOUT,
    )
    .await
    .ok()?;
    path.as_str().map(str::to_string)
}

/// The reels of the stories tray the tab's document was served with, in its
/// order: `None` on a document that preloads none, which is every one but
/// the home page. Read from the isolated world; nothing is sent.
pub(super) async fn tray(live: &mut Live) -> Result<Option<Vec<String>>, PageError> {
    let world = isolated_world(live).await?;
    let expression = format!("{TRAY}({}, {TRAY_REACH})", json!(TRAY_MARKER));
    let found = evaluate_in(
        &live.cdp,
        &live.tab,
        Some(world),
        &expression,
        COMMAND_TIMEOUT,
    )
    .await
    .map_err(broken)?;
    Ok(found.as_str().and_then(snob_ig::model::web::tray_ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fragment is not a redirect; a different path or query is.
    #[test]
    fn only_a_different_document_counts_as_a_redirect() {
        let asked = "https://www.instagram.com/someone/";
        assert!(same_document(
            asked,
            "https://www.instagram.com/someone/#top"
        ));
        assert!(!same_document(
            asked,
            "https://www.instagram.com/accounts/login/?next=%2Fsomeone%2F"
        ));
        assert!(!same_document(
            asked,
            "https://www.instagram.com/challenge/"
        ));
    }
}
