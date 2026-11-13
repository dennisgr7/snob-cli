// Records what a browser does while somebody walks Instagram by hand.
//
//     node tools/capture/record.js [address to open first]
//
// Launches the installed Chrome with a throwaway profile, opens instagram.com,
// and writes one JSON line per event to out/<timestamp>/events.jsonl until
// the last tab is closed. Log in inside that window: it is not your everyday
// profile, it lives in a temporary directory Playwright owns, and it is
// deleted with the browser when the recording ends. Nothing of the session
// touches out/.
//
// **Say what you are doing as you do it**: type a line in this terminal and
// press Enter, and it is written into the recording as a `mark`, between the
// calls that came before it and the ones that come after ("opening a
// carousel from the grid"). Enter alone is a mark with no note. That is how
// whoever reads the recording tells one thing you did from the next. `/quit`
// stops the recording, as closing the last tab does, and as Ctrl+C does;
// all three end it in order.
//
//   mark   a line typed in the terminal, numbered, with the tabs' addresses
//   tab    a tab appeared: the first one, or one a link was opened in
//   nav    the main frame moved: full navigations and SPA pushState alike
//   load   a page finished loading, with its title
//   req    a request leaving, any host, with the time it left (pacing), the
//          operation's name when it has one, and for anything but a GET to
//          instagram.com its post data: a beacon gets no answer, so this is
//          the only place its payload is ever seen (to any other host, the
//          size of what was sent and nothing of it)
//   api    a call to instagram.com (any subdomain) and its answer: every
//          XHR, fetch and beacon, every request that is not a GET, and
//          anything under /api/, /graphql/, /ajax/, /web/ or /logging,
//          with request headers, post data, response headers and the body
//   doc    an HTML document, with its body: on a cold load the profile header
//          or the post is embedded in the page rather than fetched
//   media  a picture or a piece of video from the CDN: both sets of headers
//          (range, content-type, content-length, content-range) and the
//          address whole, whose query says which rendition it is and when
//          it expires; no body
//   other  everything else, static assets mostly: method, url, status, type,
//          size on the wire, and no body
//   failed a request that never got an answer, with the reason
//   user   what the person did: click (the element, never typed text), key
//          (named keys only; characters are counted, not kept), scroll,
//          wheel, focus and visibility changes, history moves, and a video
//          starting, pausing or changing size
//   console, pageerror, log
//          the page's console, uncaught errors, and the browser's own log
//          (interventions, violations, network notices)
//   ws     a websocket opened or closed, and each frame's direction and size
//
// `req`, `api`, `media`, `other` and `failed` share a `rid`, so the time a
// request left can be put beside its answer, and a request with no answer
// found. A `req` or `api` that tells Instagram something rather than asking
// carries a `signal` (`seen`, `log`, `mutation`, `rest-post`) and the
// seen-fields it `carries`: see signals.js. **Those are the calls snob never
// sends**; they are recorded so that they are recognized, not copied.
//
// Each run writes into its own out/<timestamp>/, so an earlier recording is
// never overwritten.
//
// Secrets are redacted before anything is written: the cookie header keeps
// its cookie *names* and loses the values, and the csrf token, the LSD and
// DTSG tokens and the session id are replaced wherever they appear, in
// headers, post data and bodies, by pattern and by structure (sanitize.js).
// On top of that, every secret met where it is secret by position is
// remembered for the length of the run and taken out by value wherever else
// it appears, and the finished file is swept once more for the values
// learned after a line was written. Presence and order are findings; values
// are not. What is left still holds real usernames, so out/ is gitignored;
// delete it once the findings are written down.
//
// **What is put into the page cannot be seen by it.** The listeners that
// report clicks and scrolls run in an isolated world (as an extension's
// content script does): they share the document and its events with the
// page's scripts and nothing else, so no global, no patched function and no
// binding is there for the app to find, and none of the listeners changes
// what an event does. Marks are typed in the terminal for the same reason:
// a shortcut would have to be a key the page hears. What the page can
// notice is what it notices of anybody: its window losing focus while a
// mark is typed, which is recorded too (`user`, `window-blur`). That the
// browser is driven over the DevTools protocol at all is as it was before:
// nothing here adds to what a page could tell of that.
//
// Needs Node and the `playwright` package. It is resolved from the global
// install when it is not local, so `npm i -g playwright` is enough; the
// browser is the Chrome already on the machine, not Playwright's Chromium,
// because Instagram treats Chrome for Testing differently from Chrome.
//
// Read it back with summarize.js.
'use strict';

const path = require('path');
const fs = require('fs');
const readline = require('readline');
const { execSync } = require('child_process');
const { scrubEvent, learn, scrubKnown, sweep, late } = require('./sanitize');
const { signalOf } = require('./signals');

function playwright() {
  try {
    return require('playwright');
  } catch {
    const globalRoot = execSync('npm root -g', { encoding: 'utf8' }).trim();
    return require(path.join(globalRoot, 'playwright'));
  }
}

const MAX_BODY = 8 * 1024 * 1024;

// `x-mid` and `x-web-device-id` repeat the `mid` and `ig_did` cookies, whose
// values the cookie header loses.
const SECRET_HEADERS = new Set([
  'cookie', 'set-cookie', 'x-csrftoken', 'x-fb-lsd', 'x-ig-www-claim',
  'authorization', 'x-fb-dtsg', 'x-mid', 'x-web-device-id',
]);

// The server hands state back in `ig-set-*` and `x-ig-set-*` headers (the
// www-claim among them); their names are findings, their values are not.
function isSecretHeader(key) {
  return SECRET_HEADERS.has(key) || key.startsWith('ig-set-') || key.startsWith('x-ig-set-');
}

function redactText(s) {
  if (!s) return s;
  return s
    .replace(/"csrf_token":"[^"]*"/g, '"csrf_token":"<redacted>"')
    .replace(/"token":"[^"]*"/g, '"token":"<redacted>"')
    .replace(/"async_get_token":"[^"]*"/g, '"async_get_token":"<redacted>"')
    .replace(/"fb_dtsg":"[^"]*"/g, '"fb_dtsg":"<redacted>"')
    .replace(/"machine_id":"[^"]*"/g, '"machine_id":"<redacted>"')
    .replace(/sessionid=[^;&"\s]*/g, 'sessionid=<redacted>')
    .replace(/fb_dtsg(_ag)?=[^&"\s]*/g, 'fb_dtsg$1=<redacted>')
    .replace(/(^|[?&])lsd=[^&"\s]*/g, '$1lsd=<redacted>');
}

function redactHeaders(headers) {
  const out = {};
  for (const [name, value] of Object.entries(headers || {})) {
    const key = name.toLowerCase();
    if (!isSecretHeader(key)) {
      out[name] = value;
    } else if (key === 'cookie') {
      // Which cookies travel is a finding; their values are not.
      out[name] = value.split(';').map(c => c.trim().split('=')[0]).join('; ') + ' <values redacted>';
    } else {
      out[name] = `<redacted:${value.length}>`;
    }
  }
  return out;
}

// Cookies that are not secrets and whose text is all over the data: the
// account's own id and the window's size.
const PLAIN_COOKIES = new Set(['ds_user_id', 'wd']);

function learnCookie(pair) {
  const at = pair.indexOf('=');
  if (at < 1) return;
  const name = pair.slice(0, at).trim();
  if (!PLAIN_COOKIES.has(name)) learn(name, pair.slice(at + 1));
}

// Hands sanitize.js every secret these headers hold, before they are
// redacted, so that the same text is removed wherever else it appears.
function learnHeaders(headers) {
  for (const [name, value] of Object.entries(headers || {})) {
    const key = name.toLowerCase();
    if (key === 'cookie') value.split(';').forEach(learnCookie);
    // Playwright joins several Set-Cookie headers with a newline.
    else if (key === 'set-cookie') value.split('\n').forEach(line => learnCookie(line.split(';')[0]));
    else if (isSecretHeader(key)) value.split(/\s+/).forEach(part => learn(key, part));
  }
}

// The tokens a form or a query carries by name.
function learnFields(text) {
  if (typeof text !== 'string') return;
  for (const [, name, value] of text.matchAll(/(?:^|[?&])(fb_dtsg(?:_ag)?|lsd)=([^&"\s]+)/g)) {
    learn(name, value);
  }
}

function hostOf(url) {
  try { return new URL(url).hostname; } catch { return ''; }
}

const INSTAGRAM = /(^|\.)instagram\.com$/;
const API_PATH = /^\/(api\/|graphql\/|ajax\/|web\/|logging)/;
const CALLS = new Set(['xhr', 'fetch', 'eventsource', 'ping']);

// A call to Instagram, on whatever subdomain (`graph.` takes the logging,
// `i.` some of the REST API): anything a script sent, anything that is not
// a GET, and the API's paths however they were asked. Facebook's hosts are
// left to `other` on purpose: what the app syncs with them is credentials,
// and nothing a post needs.
function isApi(url, method, type) {
  if (!INSTAGRAM.test(hostOf(url))) return false;
  let pathname = '';
  try { pathname = new URL(url).pathname; } catch { /* no path */ }
  return method !== 'GET' || CALLS.has(type) || API_PATH.test(pathname);
}

const CDN = /(^|\.)(cdninstagram\.com|fbcdn\.net)$/;
const STATIC = new Set(['script', 'stylesheet', 'font']);

// A picture or a piece of video: the CDN's hosts but for the one that
// serves the app's own code, and anything the browser loaded as media.
function isMedia(url, type) {
  const host = hostOf(url);
  if (type === 'media') return true;
  return CDN.test(host) && !host.startsWith('static.') && !STATIC.has(type);
}

// The body a request sends, when it is text. A compressed or binary body
// cannot be read for secrets, so only its size is kept.
function postOf(request) {
  const buffer = request.postDataBuffer();
  if (!buffer) return null;
  if (buffer.length > MAX_BODY) return `<truncated:${buffer.length}>`;
  const text = buffer.toString('utf8');
  return Buffer.from(text, 'utf8').equals(buffer) ? text : `<binary:${buffer.length}>`;
}

// The world the listeners below live in, and the one function it is given.
const WORLD = 'snob-capture';
const BINDING = '__snobRecord';

// Runs in an isolated world of every document, before the page's own
// scripts. Reports what the person does, never what they type: a key event
// names Enter, Tab, the arrows and the like, and printable characters are
// only counted. It only listens: every listener is passive or reads and
// returns, and nothing here is visible from the page's world (see the top
// of this file), which is why `history` is not wrapped to hear pushState;
// `nav` already reports those.
const USER_SCRIPT = `(() => {
  if (globalThis.__snobRec) return; globalThis.__snobRec = true;
  const send = e => { try { ${BINDING}(JSON.stringify(e)); } catch {} };
  const describe = el => {
    if (!el || !el.closest) return null;
    const link = el.closest('a');
    const t = el.closest('button,[role=button],[role=link],[role=tab],[role=menuitem],[role=dialog]') || link || el;
    const text = (t.getAttribute('aria-label') || t.innerText || t.getAttribute('alt') || '').trim().slice(0, 80);
    return { tag: t.tagName.toLowerCase(), role: t.getAttribute('role'), text,
             href: link ? link.getAttribute('href') : null, type: t.getAttribute('type') };
  };
  addEventListener('click', ev => send({ ev: 'click', target: describe(ev.target), x: ev.clientX, y: ev.clientY }), true);
  addEventListener('auxclick', ev => send({ ev: 'auxclick', button: ev.button, target: describe(ev.target) }), true);
  let typed = 0, typedTimer = null;
  addEventListener('keydown', ev => {
    if (ev.key && ev.key.length === 1 && !ev.ctrlKey && !ev.metaKey) {
      typed++; clearTimeout(typedTimer);
      typedTimer = setTimeout(() => { send({ ev: 'typed', chars: typed }); typed = 0; }, 800);
      return;
    }
    send({ ev: 'key', key: ev.key, ctrl: ev.ctrlKey, meta: ev.metaKey, shift: ev.shiftKey });
  }, true);
  const lastY = new WeakMap();
  let timer = null;
  addEventListener('scroll', ev => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      const el = ev.target === document ? document.scrollingElement : ev.target;
      if (!el || !el.tagName) return;
      const y = Math.round(el.scrollTop || 0);
      send({ ev: 'scroll', y, dy: y - (lastY.get(el) || 0), height: el.scrollHeight, view: el.clientHeight,
             inner: el !== document.scrollingElement });
      lastY.set(el, y);
    }, 250);
  }, true);
  let ticks = 0, wheelTimer = null;
  addEventListener('wheel', () => {
    ticks++; clearTimeout(wheelTimer);
    wheelTimer = setTimeout(() => { send({ ev: 'wheel', ticks }); ticks = 0; }, 400);
  }, { capture: true, passive: true });
  // When a video really plays, from where (a blob: source is the app's own
  // player feeding it pieces) and at what size, to set beside the pieces
  // the CDN was asked for.
  for (const what of ['play', 'pause', 'ended', 'seeked', 'loadedmetadata', 'resize']) {
    addEventListener(what, ev => {
      const v = ev.target;
      if (!v || v.tagName !== 'VIDEO') return;
      send({ ev: 'video', what, src: String(v.currentSrc || '').slice(0, 300), at: Math.round(v.currentTime * 10) / 10,
             duration: Number.isFinite(v.duration) ? Math.round(v.duration * 10) / 10 : null,
             width: v.videoWidth, height: v.videoHeight, muted: v.muted });
    }, true);
  }
  document.addEventListener('visibilitychange', () => send({ ev: 'visibility', state: document.visibilityState }));
  addEventListener('focus', () => send({ ev: 'window-focus' }));
  addEventListener('blur', () => send({ ev: 'window-blur' }));
  addEventListener('popstate', () => send({ ev: 'popstate', url: location.href }));
})();`;

// Puts USER_SCRIPT into an isolated world of every document this tab
// loads, and of the one it already shows (a tab the page opened is wired
// after its first document began).
async function watchPerson(cdp, heard) {
  cdp.on('Runtime.bindingCalled', ({ name, payload }) => {
    if (name !== BINDING) return;
    try { heard(JSON.parse(payload)); } catch { /* not ours to read */ }
  });
  await cdp.send('Page.enable');
  // The binding is only given to worlds created while Runtime is enabled.
  // Playwright's own session has it enabled already, so this shows the
  // page nothing new.
  await cdp.send('Runtime.enable');
  await cdp.send('Runtime.addBinding', { name: BINDING, executionContextName: WORLD });
  await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: USER_SCRIPT, worldName: WORLD });
  const { frameTree } = await cdp.send('Page.getFrameTree');
  const { executionContextId } = await cdp.send('Page.createIsolatedWorld', { frameId: frameTree.frame.id, worldName: WORLD });
  await cdp.send('Runtime.evaluate', { expression: USER_SCRIPT, contextId: executionContextId });
}

async function bodyOf(response) {
  try {
    const buffer = await response.body();
    return buffer.length > MAX_BODY ? `<truncated:${buffer.length}>` : buffer.toString('utf8');
  } catch (e) {
    return `<unavailable:${e.message}>`;
  }
}

// `start` is the address opened first. `out`, `input` and `before` are for a
// harness: where to write, where marks are read from, and a hook given the
// browser context before any page exists (to answer instagram.com from
// fixtures, so that trying the recorder reaches nobody).
async function record({ start = 'https://www.instagram.com/', out, input = process.stdin, before } = {}) {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const dir = out || path.join(__dirname, 'out', stamp);
  const log = path.join(dir, 'events.jsonl');
  fs.mkdirSync(dir, { recursive: true });
  const stream = fs.createWriteStream(log, { flags: 'a' });
  let seq = 0;
  // Every string goes through the redaction, not only bodies and post data:
  // a GET carries fb_dtsg in its query, and the referer header repeats it.
  const redactAll = v =>
    typeof v === 'string' ? redactText(v)
      : Array.isArray(v) ? v.map(redactAll)
        : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, redactAll(x)]))
          : v;
  let closed = false;
  const emit = event => {
    // A tab still answering while the browser closes has nowhere to write.
    if (closed) return;
    event.seq = ++seq;
    event.t = new Date().toISOString();
    if (event.kind === 'req' || event.kind === 'api') Object.assign(event, signalOf(event));
    // By pattern, then by structure, then by value: the last pass sees what
    // the first two left, in whatever place they did not look.
    stream.write(JSON.stringify(scrubKnown(scrubEvent(redactAll(event)))) + '\n');
  };

  // `launch` rather than a persistent context on purpose: the profile is a
  // temporary directory Playwright creates and removes in `browser.close()`,
  // so there is no profile of ours to delete afterwards -- and a persistent
  // one could not be, because Chrome kept running in the background after
  // its window closed and held the directory open.
  //
  // The signals are ours to handle: Playwright's own handlers kill the
  // browser and exit at once, which loses the lines not yet written and
  // skips the sweep.
  const { chromium } = playwright();
  const browser = await chromium.launch({
    channel: 'chrome',
    headless: false,
    args: ['--start-maximized', '--disable-blink-features=AutomationControlled'],
    ignoreDefaultArgs: ['--enable-automation'],
    handleSIGINT: false, handleSIGTERM: false, handleSIGHUP: false,
  });
  const context = await browser.newContext({ viewport: null });
  if (before) await before(context);

  const counts = { mark: 0, nav: 0, api: 0, doc: 0, media: 0, other: 0, user: 0, console: 0, ws: 0 };
  const status = () => Object.entries(counts).map(([k, n]) => `${k} ${n}`).join('  ');
  // The counts live in the terminal's title, not on its last line, which is
  // where marks are typed. At most four times a second: a page load is
  // hundreds of events.
  let titled = false;
  const tick = () => {
    if (!process.stderr.isTTY || titled) return;
    titled = true;
    setTimeout(() => {
      titled = false;
      process.stderr.write(`\x1b]0;recording  ${status()}\x07`);
    }, 250);
  };

  let stop;
  const stopped = new Promise(resolve => { stop = resolve; });

  // The same number on a request and on everything said about it later.
  const rids = new WeakMap();
  let lastRid = 0;
  const ridOf = request => {
    if (!rids.has(request)) rids.set(request, ++lastRid);
    return rids.get(request);
  };

  // The tab a request belongs to. A tab's first request leaves before the
  // tab is announced, and a navigation's before its frame exists, which is
  // why the network is listened to on the context and not tab by tab: the
  // document a link opens in a new tab would otherwise never be recorded.
  const pageOf = request => {
    try { return request.frame().page().url(); } catch { return null; }
  };

  // When each request left, any host, so the real client's pacing can be
  // read back next to the answers. The post data is kept here as well as
  // on `api`: a beacon is never answered, so no `api` line follows it.
  context.on('request', request => {
    const method = request.method();
    const url = request.url();
    const event = { kind: 'req', rid: ridOf(request), method, url, type: request.resourceType(), page: pageOf(request) };
    // The headers the page set, which is all there is before the request
    // has left; the name is among them.
    const friendly = request.headers()['x-fb-friendly-name'];
    if (friendly) event.friendly = friendly;
    learnFields(url);
    if (method !== 'GET') {
      const post = postOf(request);
      learnFields(post);
      if (INSTAGRAM.test(hostOf(url))) event.postData = post;
      else event.postBytes = (request.postDataBuffer() || []).length;
    }
    emit(event);
  });
  context.on('response', async response => {
    const request = response.request();
    const url = request.url();
    const type = request.resourceType();
    const method = request.method();
    const base = { rid: ridOf(request), method, url, status: response.status(), type, page: pageOf(request) };
    // Known only once the answer has ended, so asked for after the body.
    const ms = () => {
      const timing = request.timing();
      return timing && timing.responseEnd > 0 ? Math.round(timing.responseEnd) : null;
    };
    try {
      const requestHeaders = await request.allHeaders();
      const responseHeaders = await response.allHeaders();
      learnHeaders(requestHeaders);
      learnHeaders(responseHeaders);
      const headers = { requestHeaders: redactHeaders(requestHeaders), responseHeaders: redactHeaders(responseHeaders) };
      if (type === 'document' && INSTAGRAM.test(hostOf(url))) {
        counts.doc++; tick();
        const body = await bodyOf(response);
        emit({ kind: 'doc', ...base, ms: ms(), ...headers, postData: postOf(request), body });
      } else if (isApi(url, method, type)) {
        counts.api++; tick();
        const body = await bodyOf(response);
        emit({ kind: 'api', ...base, ms: ms(), ...headers, postData: postOf(request), body });
      } else if (isMedia(url, type)) {
        // Written as the answer begins, not when it ends: a video is
        // abandoned halfway more often than not, and its headers already
        // say what was asked for and how much of it there is.
        counts.media++; tick();
        emit({ kind: 'media', ...base, ...headers });
      } else {
        counts.other++; tick();
        let size = null;
        try { size = (await request.sizes()).responseBodySize; } catch { /* it did not finish */ }
        emit({ kind: 'other', ...base, ms: ms(), size });
      }
    } catch (e) {
      emit({ kind: 'error', rid: base.rid, url, message: String(e) });
    }
  });
  // A call that never got an answer -- blocked, aborted by the app, cut by
  // a navigation -- is as much a finding as one that did.
  context.on('requestfailed', request => {
    emit({
      kind: 'failed', rid: ridOf(request), method: request.method(), url: request.url(),
      type: request.resourceType(), page: pageOf(request),
      reason: (request.failure() || {}).errorText || '',
    });
  });

  async function wire(page) {
    page.on('console', msg => {
      counts.console++; tick();
      const loc = msg.location() || {};
      emit({ kind: 'console', level: msg.type(), text: msg.text().slice(0, 4000), source: loc.url || null, page: page.url() });
    });
    page.on('pageerror', err => {
      counts.console++; tick();
      emit({ kind: 'pageerror', message: String((err && err.stack) || err).slice(0, 4000), page: page.url() });
    });
    page.on('dialog', d => emit({ kind: 'dialog', type: d.type(), message: d.message() }));
    page.on('websocket', ws => {
      counts.ws++; tick();
      emit({ kind: 'ws', ev: 'open', url: ws.url() });
      let sent = 0, recv = 0;
      ws.on('framesent', f => { sent++; emit({ kind: 'ws', ev: 'sent', url: ws.url(), bytes: f.payload.length }); });
      ws.on('framereceived', f => { recv++; emit({ kind: 'ws', ev: 'recv', url: ws.url(), bytes: f.payload.length }); });
      ws.on('close', () => emit({ kind: 'ws', ev: 'close', url: ws.url(), sent, recv }));
      ws.on('socketerror', e => emit({ kind: 'ws', ev: 'error', url: ws.url(), message: String(e) }));
    });
    page.on('framenavigated', frame => {
      if (frame !== page.mainFrame()) return;
      counts.nav++; tick();
      emit({ kind: 'nav', url: frame.url() });
    });
    page.on('load', async () => {
      let title = '';
      try { title = await page.title(); } catch { /* the page went away */ }
      emit({ kind: 'load', url: page.url(), title });
    });
    try {
      const cdp = await context.newCDPSession(page);
      // The browser's own log: interventions, deprecations, blocked requests.
      cdp.on('Log.entryAdded', ({ entry }) => {
        counts.console++; tick();
        emit({ kind: 'log', source: entry.source, level: entry.level, text: (entry.text || '').slice(0, 4000), url: entry.url || null, page: page.url() });
      });
      await cdp.send('Log.enable');
      await watchPerson(cdp, e => {
        counts.user++; tick();
        emit({ kind: 'user', page: page.url(), ...e });
      });
    } catch (e) {
      emit({ kind: 'error', message: `the browser log and the person's actions are not recorded on this tab: ${e}` });
    }
  }

  // The recording ends when the last tab is gone, whichever tab that is.
  const wired = new WeakMap();
  context.on('page', page => {
    // A tab the page opened has begun its first document by now, so its
    // first `nav` was never heard: this says a tab appeared, and where.
    emit({ kind: 'tab', url: page.url() });
    wired.set(page, wire(page));
    page.on('close', () => {
      if (context.pages().length === 0) stop('the last tab was closed');
    });
  });
  browser.on('disconnected', () => stop('the browser went away'));
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(signal, () => stop(`stopped by ${signal}`));

  // Marks. Line by line and with no prompt of ours: the terminal's own line
  // editing is enough, and nothing is drawn over what is being typed.
  const typed = readline.createInterface({ input, terminal: false });
  typed.on('line', line => {
    const note = line.trim();
    if (note === '/quit') { stop('stopped from the terminal'); return; }
    counts.mark++; tick();
    emit({ kind: 'mark', n: counts.mark, note, pages: context.pages().map(p => p.url()) });
    process.stderr.write(`  mark ${counts.mark} written at event ${seq}  (${status()})\n`);
  });

  const page = await context.newPage();
  // The listeners first: a request that leaves before them is not recorded.
  await wired.get(page);
  emit({ kind: 'start', note: 'browser open; close its last tab, or type /quit, to stop recording' });
  process.stderr.write(
    `Recording to ${log}\n` +
    'Log in and browse in the window that opened. Here, type what you are about to do and press Enter\n' +
    'to mark it in the recording. Close the window, or type /quit, when done.\n');
  try {
    await page.goto(start);
  } catch (e) {
    emit({ kind: 'error', message: `could not open ${start}: ${e}` });
    process.stderr.write(`Could not open ${start}; type the address in the window.\n`);
  }

  const why = await stopped;
  typed.close();
  emit({ kind: 'end', why, counts });
  // Closing the browser is what removes its temporary profile, session and
  // all. Playwright kills the process tree if it does not go on its own.
  await browser.close().catch(() => {});
  closed = true;
  await new Promise(resolve => stream.end(resolve));
  const asWritten = late();
  const swept = sweep(log);
  process.stderr.write(
    `\nDone (${why}): ${status()} -> ${log}\n` +
    `Secrets taken out by value: ${asWritten} strings as they were written, ${swept} lines in the sweep afterwards.\n` +
    'The browser and its profile are gone. Delete out/ once the findings are written down.\n');
  return { log, counts };
}

module.exports = { record, isApi, isMedia };

if (require.main === module) {
  record({ start: process.argv[2] }).then(() => process.exit(0), e => {
    console.error(e && e.stack || e);
    process.exit(1);
  });
}
