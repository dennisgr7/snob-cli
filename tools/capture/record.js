// Records what a browser does while somebody walks Instagram by hand.
//
//     node tools/capture/record.js
//
// Launches the installed Chrome with a throwaway profile, opens instagram.com,
// and writes one JSON line per event to out/events.jsonl until the last tab
// is closed. Log in inside that window: it is not your everyday profile, it
// lives in a temporary directory Playwright owns, and it is deleted with the
// browser when the recording ends. Nothing of the session touches out/.
//
//   nav    the main frame moved: full navigations and SPA pushState alike
//   load   a page finished loading, with its title
//   api    a request to instagram.com under /api/, /graphql/, /ajax/ or /web/,
//          with request headers, post data, response headers and the body
//   doc    an HTML document, with its body: on a cold load the profile header
//          is embedded in the page rather than fetched
//   other  everything else -- CDN, static assets: method, url, status, type,
//          size, and no body
//   failed a request that never got an answer, with the reason
//   req    a request leaving, any host, with the time it left (pacing)
//   user   what the person did: click (the element, never typed text), key
//          (named keys only; characters are counted, not kept), scroll,
//          wheel, focus and visibility changes, history moves
//   console, pageerror, log
//          the page's console, uncaught errors, and the browser's own log
//          (interventions, violations, network notices)
//   ws     a websocket opened or closed, and each frame's direction and size
//
// Each run writes into its own out/<timestamp>/, so an earlier recording is
// never overwritten.
//
// Secrets are redacted before anything is written: the cookie header keeps
// its cookie *names* and loses the values, and the csrf token, the LSD and
// DTSG tokens and the session id are replaced wherever they appear, in
// headers, post data and bodies. Presence and order are findings; values are
// not. What is left still holds real usernames, so out/ is gitignored;
// delete it once the findings are written down.
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
const { execSync } = require('child_process');
const { scrubEvent } = require('./sanitize');

function playwright() {
  try {
    return require('playwright');
  } catch {
    const globalRoot = execSync('npm root -g', { encoding: 'utf8' }).trim();
    return require(path.join(globalRoot, 'playwright'));
  }
}

const STAMP = new Date().toISOString().replace(/[:.]/g, '-');
const OUT = path.join(__dirname, 'out', STAMP);
const LOG = path.join(OUT, 'events.jsonl');
const MAX_BODY = 8 * 1024 * 1024;

const SECRET_HEADERS = new Set([
  'cookie', 'set-cookie', 'x-csrftoken', 'x-fb-lsd', 'x-ig-www-claim',
  'authorization', 'x-fb-dtsg',
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

function isApi(url) {
  try {
    const u = new URL(url);
    return /(^|\.)instagram\.com$/.test(u.hostname) && /^\/(api\/|graphql\/|ajax\/|web\/)/.test(u.pathname);
  } catch {
    return false;
  }
}

// Runs in every frame before the page's own scripts. Reports what the person
// does, never what they type: a key event names Enter, Tab, the arrows and
// the like, and printable characters are only counted.
const USER_SCRIPT = `(() => {
  if (window.__snobRec) return; window.__snobRec = true;
  const send = e => { try { window.__snobRecord(e); } catch {} };
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
  document.addEventListener('visibilitychange', () => send({ ev: 'visibility', state: document.visibilityState }));
  addEventListener('focus', () => send({ ev: 'window-focus' }));
  addEventListener('blur', () => send({ ev: 'window-blur' }));
  addEventListener('popstate', () => send({ ev: 'popstate', url: location.href }));
  for (const m of ['pushState', 'replaceState']) {
    const orig = history[m];
    history[m] = function (...a) { const r = orig.apply(this, a); send({ ev: m, url: location.href }); return r; };
  }
})();`;

async function bodyOf(response) {
  try {
    const buffer = await response.body();
    return buffer.length > MAX_BODY ? `<truncated:${buffer.length}>` : buffer.toString('utf8');
  } catch (e) {
    return `<unavailable:${e.message}>`;
  }
}

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const stream = fs.createWriteStream(LOG, { flags: 'a' });
  let seq = 0;
  // Every string goes through the redaction, not only bodies and post data:
  // a GET carries fb_dtsg in its query, and the referer header repeats it.
  const redactAll = v =>
    typeof v === 'string' ? redactText(v)
      : Array.isArray(v) ? v.map(redactAll)
        : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, redactAll(x)]))
          : v;
  const emit = event => {
    event.seq = ++seq;
    event.t = new Date().toISOString();
    stream.write(JSON.stringify(scrubEvent(redactAll(event))) + '\n');
  };

  // `launch` rather than a persistent context on purpose: the profile is a
  // temporary directory Playwright creates and removes in `browser.close()`,
  // so there is no profile of ours to delete afterwards -- and a persistent
  // one could not be, because Chrome kept running in the background after
  // its window closed and held the directory open.
  const { chromium } = playwright();
  const browser = await chromium.launch({
    channel: 'chrome',
    headless: false,
    args: ['--start-maximized', '--disable-blink-features=AutomationControlled'],
    ignoreDefaultArgs: ['--enable-automation'],
  });
  const context = await browser.newContext({ viewport: null });

  const counts = { nav: 0, api: 0, doc: 0, other: 0, user: 0, console: 0, ws: 0 };
  const tick = () => process.stderr.write(
    `\r[recording] nav ${counts.nav}  api ${counts.api}  doc ${counts.doc}  other ${counts.other}` +
    `  user ${counts.user}  console ${counts.console}  ws ${counts.ws}   `);

  await context.exposeBinding('__snobRecord', ({ page }, e) => {
    counts.user++; tick();
    emit({ kind: 'user', page: page ? page.url() : null, ...e });
  });
  await context.addInitScript(USER_SCRIPT);

  async function wire(page) {
    // When each request left, any host, so the real client's pacing can be
    // read back next to the answers.
    page.on('request', request => {
      emit({ kind: 'req', method: request.method(), url: request.url(), type: request.resourceType(), page: page.url() });
    });
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
    // The browser's own log: interventions, deprecations, blocked requests.
    try {
      const cdp = await context.newCDPSession(page);
      cdp.on('Log.entryAdded', ({ entry }) => {
        counts.console++; tick();
        emit({ kind: 'log', source: entry.source, level: entry.level, text: (entry.text || '').slice(0, 4000), url: entry.url || null, page: page.url() });
      });
      await cdp.send('Log.enable');
    } catch (e) {
      emit({ kind: 'error', message: `browser log unavailable: ${e}` });
    }

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
    page.on('response', async response => {
      const request = response.request();
      const url = request.url();
      const type = request.resourceType();
      const timing = request.timing();
      const base = {
        method: request.method(), url, status: response.status(), type, page: page.url(),
        ms: timing && timing.responseEnd > 0 ? Math.round(timing.responseEnd) : null,
      };
      try {
        if (isApi(url)) {
          counts.api++; tick();
          emit({
            kind: 'api', ...base,
            requestHeaders: redactHeaders(await request.allHeaders()),
            postData: redactText(request.postData()),
            responseHeaders: redactHeaders(await response.allHeaders()),
            body: redactText(await bodyOf(response)),
          });
        } else if (type === 'document' && /instagram\.com/.test(url)) {
          counts.doc++; tick();
          emit({
            kind: 'doc', ...base,
            requestHeaders: redactHeaders(await request.allHeaders()),
            responseHeaders: redactHeaders(await response.allHeaders()),
            body: redactText(await bodyOf(response)),
          });
        } else {
          counts.other++; tick();
          let size = null;
          try { size = (await response.body()).length; } catch { /* not kept */ }
          emit({ kind: 'other', ...base, size });
        }
      } catch (e) {
        emit({ kind: 'error', url, message: String(e) });
      }
    });
    // A call that never got an answer -- blocked, aborted by the app, cut by
    // a navigation -- is as much a finding as one that did.
    page.on('requestfailed', request => {
      emit({
        kind: 'failed', method: request.method(), url: request.url(),
        type: request.resourceType(), page: page.url(),
        reason: (request.failure() || {}).errorText || '',
      });
    });
  }

  // The recording ends when the last tab is gone, whichever tab that is.
  const lastTabClosed = new Promise(resolve => {
    context.on('page', page => {
      wire(page);
      page.on('close', () => {
        if (context.pages().length === 0) resolve();
      });
    });
  });

  const page = await context.newPage();
  await page.goto('https://www.instagram.com/');
  emit({ kind: 'start', note: 'browser open; close its last tab to stop recording' });
  process.stderr.write(`Browser is open. Log in, browse, and close the window when done.\nRecording to ${LOG}\n`);

  await lastTabClosed;
  emit({ kind: 'end', counts });
  // Closing the browser is what removes its temporary profile, session and
  // all. Playwright kills the process tree if it does not go on its own.
  await browser.close();
  stream.end(() => {
    process.stderr.write(`\nDone: ${JSON.stringify(counts)} -> ${LOG}\n`);
    process.stderr.write('The browser and its profile are gone. Delete out/ once the findings are written down.\n');
    process.exit(0);
  });
})().catch(e => {
  console.error(e && e.stack || e);
  process.exit(1);
});
