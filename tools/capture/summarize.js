// Reads a recording back.
//
//     node tools/capture/summarize.js                    what happened, in order
//     node tools/capture/summarize.js marks              the notes typed while recording
//     node tools/capture/summarize.js ops [needle]       GraphQL operations, with doc_id and what
//                                                        their answers are rooted at
//     node tools/capture/summarize.js shape <needle> [n] the shape of the n-th reply whose
//                                                        friendly name or URL contains <needle>
//     node tools/capture/summarize.js profiles           the profile header of every account
//                                                        visited, one line each, for comparing
//     node tools/capture/summarize.js timeline           every call in order, with the seconds
//                                                        since the one before, between the marks
//     node tools/capture/summarize.js vars <needle>      the variables of every matching call:
//                                                        how the app pages through a list
//     node tools/capture/summarize.js signals            every call that tells rather than asks:
//                                                        seen, logging, mutations, other POSTs
//     node tools/capture/summarize.js embedded [needle] [n]
//                                                        what each HTML document carries inside
//                                                        it, or the shape of the n-th such
//                                                        payload whose name contains <needle>
//     node tools/capture/summarize.js posts [code]       every post object in any answer or
//                                                        document, one line each
//     node tools/capture/summarize.js post <code>        the shape of the fullest object seen
//                                                        for that post, and where it came from
//     node tools/capture/summarize.js media              what the CDN was asked for, a file a line
//
// Any of them reads the newest recording under out/; `--from <directory or
// file>` names another.
//
// The default listing is the marks, the navigations and a count per
// endpoint, which is enough to see which calls a page makes. `ops` names the
// Relay operations on /api/graphql and /graphql/query by their
// X-FB-Friendly-Name, which is how a query is told apart when every one is a
// POST to the same address. `shape` prints the key tree of one body with
// long strings cut, which is what a model in snob-ig is written against.
// `profiles` pulls the same fields out of every PolarisProfilePageContentQuery
// so accounts can be compared side by side -- private, followed, counts,
// mutuals, whether a story is up.
//
// For posts: `embedded` is where a post opened from a link comes from, since
// a cold load carries it in the page and asks for nothing; `posts` lines up
// every copy of a post the app was given (the grid's, the one a click
// fetched, the one a document embedded) with its numbers, so the fields that
// differ between them show; `media` is how one rendition among many was
// picked, and how a video was fetched in pieces. **`signals` is the list of
// what snob must never send** (signals.js says how a call gets on it, and
// that a call left off it is not thereby safe).
'use strict';

const fs = require('fs');
const path = require('path');
const { lines } = require('./sanitize');
const { signalOf, friendlyName } = require('./signals');

const OUT = path.join(__dirname, 'out');

// The recording asked for, else the newest: each run of record.js writes
// into a directory named for when it began, so the last name is the latest.
// A file straight under out/ is where recordings were written before that.
function recording(from) {
  if (from) return fs.statSync(from).isDirectory() ? path.join(from, 'events.jsonl') : from;
  const runs = fs.existsSync(OUT)
    ? fs.readdirSync(OUT).filter(name => fs.existsSync(path.join(OUT, name, 'events.jsonl'))).sort()
    : [];
  if (runs.length) return path.join(OUT, runs[runs.length - 1], 'events.jsonl');
  return path.join(OUT, 'events.jsonl');
}

let LOG = '';

function* events() {
  for (const line of lines(LOG)) yield JSON.parse(line);
}

function variables(event) {
  const params = new URLSearchParams(event.postData || '');
  const raw = params.get('variables');
  if (!raw) return null;
  try { return JSON.parse(raw); } catch { return raw; }
}

function docId(event) {
  return new URLSearchParams(event.postData || '').get('doc_id') || '';
}

// The JSON documents of a body: one, or one a line when the answer came in
// parts (a query with deferred fields does), a `for (;;);` guard allowed.
function documents(body) {
  if (typeof body !== 'string') return [];
  const text = body.startsWith('for (;;);') ? body.slice(9) : body;
  try { return [JSON.parse(text)]; } catch { /* not one document */ }
  const out = [];
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    try { out.push(JSON.parse(line)); } catch { return []; }
  }
  return out;
}

// What an HTML document carries for the app to start from: the JSON of its
// `<script type="application/json">` blocks.
function embedded(html) {
  const out = [];
  if (typeof html !== 'string') return out;
  for (const [, text] of html.matchAll(/<script type="application\/json"[^>]*>([\s\S]*?)<\/script>/g)) {
    try { out.push(JSON.parse(text)); } catch { /* cut short, or not JSON */ }
  }
  return out;
}

// The answers preloaded into a document: every `{__bbox: {result: ...}}`,
// under the name it is filed by (the string before it in its list, which
// names the query's preloader).
function* preloaded(value, name = '') {
  if (Array.isArray(value)) {
    let last = name;
    for (const inner of value) {
      if (typeof inner === 'string') last = inner;
      else yield* preloaded(inner, last);
    }
  } else if (value && typeof value === 'object') {
    const box = value.__bbox;
    if (box && typeof box === 'object' && box.result && typeof box.result === 'object') {
      yield { name, result: box.result };
    }
    for (const inner of Object.values(value)) yield* preloaded(inner, name);
  }
}

function shape(value, depth = 0, lines = []) {
  const pad = '  '.repeat(depth);
  if (Array.isArray(value)) {
    lines.push(`${pad}[list of ${value.length}]`);
    if (value.length) shape(value[0], depth + 1, lines);
  } else if (value && typeof value === 'object') {
    for (const [key, inner] of Object.entries(value)) {
      if (inner && typeof inner === 'object') {
        lines.push(`${pad}${key}:`);
        shape(inner, depth + 1, lines);
      } else {
        lines.push(`${pad}${key}: ${JSON.stringify(inner).slice(0, 90)}`);
      }
    }
  } else {
    lines.push(`${pad}${JSON.stringify(value).slice(0, 90)}`);
  }
  return lines;
}

function label(event) {
  const { signal, carries } = signalOf(event);
  return [signal, carries && `carries:${carries.join(',')}`].filter(Boolean).join(' ');
}

function overview() {
  const endpoints = new Map();
  const kinds = new Map();
  const navs = [];
  const marks = [];
  let told = 0;
  for (const e of events()) {
    kinds.set(e.kind, (kinds.get(e.kind) || 0) + 1);
    if (e.kind === 'nav') navs.push(`${e.seq}\t${e.url}`);
    else if (e.kind === 'mark') marks.push(`${e.seq}\t#${e.n} ${e.note}`);
    else if (e.kind === 'api') {
      const key = `${e.method} ${new URL(e.url).pathname}`;
      endpoints.set(key, (endpoints.get(key) || 0) + 1);
      if (label(e)) told++;
    }
  }
  console.log([...kinds].map(([kind, count]) => `${kind} ${count}`).join('  '));
  console.log(`calls that tell rather than ask (see \`signals\`): ${told}`);
  console.log('\n--- marks');
  console.log(marks.join('\n'));
  console.log('\n--- navigations');
  console.log(navs.join('\n'));
  console.log('\n--- api endpoints');
  for (const [key, count] of [...endpoints].sort((a, b) => b[1] - a[1])) {
    console.log(`${String(count).padStart(4)} ${key}`);
  }
}

function marks() {
  for (const e of events()) {
    if (e.kind !== 'mark') continue;
    console.log(`${String(e.seq).padStart(6)} ${e.t}  #${e.n} ${e.note}\n${' '.repeat(33)}on ${(e.pages || []).join(' , ')}`);
  }
}

function ops(needle) {
  const seen = new Map();
  for (const e of events()) {
    if (e.kind !== 'api') continue;
    const p = new URL(e.url).pathname;
    if (p !== '/api/graphql' && p !== '/graphql/query') continue;
    const name = friendlyName(e) || '<no friendly name>';
    if (needle && !name.includes(needle)) continue;
    const key = `${p} ${name}`;
    const entry = seen.get(key) || { count: 0, ids: new Set(), roots: new Set(), label: label(e) };
    entry.count++;
    entry.ids.add(docId(e));
    // What the answer is rooted at is how snob-ig finds its data, and what
    // tells two operations with look-alike names apart.
    for (const doc of documents(e.body)) {
      for (const root of Object.keys((doc && doc.data) || {})) entry.roots.add(root);
    }
    seen.set(key, entry);
  }
  for (const [key, entry] of [...seen].sort((a, b) => b[1].count - a[1].count)) {
    console.log(`${String(entry.count).padStart(4)} ${key.padEnd(80)} doc_id=${[...entry.ids].join(',')}` +
      `${entry.label ? `  [${entry.label}]` : ''}${entry.roots.size ? `\n       -> ${[...entry.roots].join(', ')}` : ''}`);
  }
}

function show(needle, which) {
  let n = 0;
  for (const e of events()) {
    if (e.kind !== 'api') continue;
    if (!friendlyName(e).includes(needle) && !e.url.includes(needle)) continue;
    if (++n !== which) continue;
    console.log(`=== seq ${e.seq} ${e.method} ${e.url.slice(0, 120)}\npage ${e.page}\nstatus ${e.status}`);
    const vars = variables(e);
    if (vars) console.log(`variables ${JSON.stringify(vars).slice(0, 300)}`);
    const docs = documents(e.body);
    if (!docs.length) console.log(`body is not JSON: ${String(e.body).slice(0, 300)}`);
    docs.forEach((doc, part) => {
      if (docs.length > 1) console.log(`--- part ${part + 1} of ${docs.length}`);
      console.log(shape(doc).join('\n'));
    });
    return;
  }
  console.error(`no reply #${which} matching "${needle}"`);
  process.exit(1);
}

// Every API call in order with the seconds since the one before, which is
// the cadence the app itself keeps, and the marks between them, which say
// what the person was doing when the calls were made.
function timeline() {
  let last = null;
  for (const e of events()) {
    if (e.kind === 'mark') {
      console.log(`\n${String(e.seq).padStart(6)} ======== #${e.n} ${e.note}`);
      continue;
    }
    if (e.kind !== 'api' && e.kind !== 'failed' && e.kind !== 'nav' && e.kind !== 'doc') continue;
    const at = Date.parse(e.t);
    const gap = last === null ? '' : ((at - last) / 1000).toFixed(1);
    last = at;
    if (e.kind === 'nav' || e.kind === 'doc') {
      console.log(`${String(e.seq).padStart(6)} ${gap.padStart(7)}  ${e.kind.padEnd(6)} ${e.url}`);
      continue;
    }
    const what = friendlyName(e) || `${e.method} ${new URL(e.url).pathname}`;
    const status = e.kind === 'failed' ? `failed:${e.reason}` : e.status;
    const told = label(e);
    console.log(`${String(e.seq).padStart(6)} ${gap.padStart(7)}  ${String(status).padEnd(6)} ${what}${told ? `  [${told}]` : ''}`);
  }
}

// The variables of every call matching <needle>, one line each: how the app
// pages through a list (cursor, page size) and what it asks for.
function vars(needle) {
  for (const e of events()) {
    if (e.kind !== 'api') continue;
    if (!friendlyName(e).includes(needle) && !e.url.includes(needle)) continue;
    const v = variables(e);
    const query = new URL(e.url).search;
    console.log(`${String(e.seq).padStart(6)} ${e.status} ${friendlyName(e) || new URL(e.url).pathname}${query.slice(0, 120)} ${v ? JSON.stringify(v).slice(0, 200) : ''}`);
  }
}

// Every call that tells Instagram something, answered or not. A beacon has
// a `req` line and nothing after it, so the requests are read too, and one
// that was answered is listed once, by its answer.
function signals() {
  const answered = new Set();
  const ended = new Map();
  for (const e of events()) {
    if (!e.rid) continue;
    if (e.kind === 'api') answered.add(e.rid);
    else if (e.kind === 'other' || e.kind === 'media') ended.set(e.rid, e.status);
    else if (e.kind === 'failed') ended.set(e.rid, 'failed');
  }
  for (const e of events()) {
    // A recording from before requests were numbered cannot pair the two,
    // and its `req` lines hold nothing an `api` line does not.
    if (e.kind !== 'api' && !(e.kind === 'req' && e.rid && !answered.has(e.rid))) continue;
    const told = label(e);
    if (!told) continue;
    const what = friendlyName(e) || `${e.method} ${new URL(e.url).host}${new URL(e.url).pathname}`;
    const status = e.kind === 'api' ? e.status : ended.get(e.rid) || 'no answer';
    const v = variables(e);
    const sent = v ? JSON.stringify(v) : String(e.postData || '');
    console.log(`${String(e.seq).padStart(6)} ${e.t.slice(11, 19)} ${`[${told}]`.padEnd(12)} ${String(status).padEnd(9)} ${what}\n` +
      `${' '.repeat(16)}from ${e.page}\n${' '.repeat(16)}${sent.slice(0, 240)}`);
  }
}

function embeddedIn(needle, which) {
  let n = 0;
  for (const e of events()) {
    if (e.kind !== 'doc') continue;
    const found = embedded(e.body).flatMap(block => [...preloaded(block)]);
    if (needle === undefined) {
      console.log(`=== seq ${e.seq} ${e.status} ${e.url}  (${String(e.body).length} characters)`);
      for (const { name, result } of found) {
        console.log(`   ${name || '<unnamed>'}\n       -> ${Object.keys(result.data || result).join(', ')}`);
      }
      continue;
    }
    for (const { name, result } of found) {
      if (!name.includes(needle) || ++n !== which) continue;
      console.log(`=== seq ${e.seq} ${e.url}\n${name}`);
      console.log(shape(result).join('\n'));
      return;
    }
  }
  if (needle !== undefined) {
    console.error(`no embedded payload #${which} matching "${needle}"`);
    process.exit(1);
  }
}

// A post, whichever of the two families of names it is written in: the
// REST API's and the newer queries' (`code`, `pk`, `media_type`), or the
// older GraphQL's (`shortcode`, `id`, `display_url`).
function isPost(v) {
  return typeof (v.code || v.shortcode) === 'string' && (v.pk !== undefined || v.id !== undefined)
    && ['media_type', 'image_versions2', 'video_versions', 'carousel_media', 'display_url', 'edge_sidecar_to_children']
      .some(key => key in v);
}

function* postsIn(value, trail = '') {
  if (Array.isArray(value)) {
    for (const [i, inner] of value.entries()) yield* postsIn(inner, `${trail}[${i}]`);
  } else if (value && typeof value === 'object') {
    if (isPost(value)) yield { post: value, trail };
    for (const [key, inner] of Object.entries(value)) {
      if (inner && typeof inner === 'object') yield* postsIn(inner, trail ? `${trail}.${key}` : key);
    }
  }
}

// Every post object the app was given, with the event it came in and where
// in it. Parsing is skipped for a body that cannot hold one.
function* everyPost() {
  for (const e of events()) {
    if (e.kind !== 'api' && e.kind !== 'doc') continue;
    const body = String(e.body || '');
    if (!body.includes('"code":"') && !body.includes('"shortcode":"')) continue;
    const from = e.kind === 'doc' ? `document ${new URL(e.url).pathname}` : friendlyName(e) || new URL(e.url).pathname;
    const sources = e.kind === 'doc'
      ? embedded(body).flatMap(block => [...preloaded(block)]).map(({ name, result }) => ({ name, value: result }))
      : documents(body).map(value => ({ name: '', value }));
    for (const { name, value } of sources) {
      for (const { post, trail } of postsIn(value)) yield { e, from: name ? `${from} ${name}` : from, post, trail };
    }
  }
}

// The fields a post's caption and statistics are made of, when they are
// there and not empty; `post <code>` shows everything else.
const NOTABLE = [
  'caption', 'location', 'usertags', 'coauthor_producers', 'invited_coauthor_producers', 'clips_metadata',
  'music_metadata', 'accessibility_caption', 'carousel_media', 'video_dash_manifest', 'video_versions',
  'image_versions2', 'like_and_view_counts_disabled', 'has_liked', 'has_viewer_saved', 'top_likers',
  'facepile_top_likers', 'social_context', 'sponsor_tags', 'title', 'product_type', 'original_width',
  'original_height', 'video_duration', 'has_audio', 'comments_disabled', 'pinned_for_users',
];

function posts(code) {
  for (const { e, from, post, trail } of everyPost()) {
    const own = post.code || post.shortcode;
    if (code && own !== code) continue;
    const numbers = {};
    for (const [key, value] of Object.entries(post)) {
      if (typeof value === 'number' && /(_count|_at)$/.test(key)) numbers[key] = value;
    }
    const filled = NOTABLE.filter(key => {
      const value = post[key];
      return value !== undefined && value !== null && value !== '' && !(Array.isArray(value) && !value.length);
    });
    console.log(JSON.stringify({
      seq: e.seq, from, at: trail, code: own, pk: post.pk || post.id,
      media_type: post.media_type, typename: post.__typename, product_type: post.product_type,
      children: (post.carousel_media || []).length || undefined,
      images: ((post.image_versions2 || {}).candidates || []).length || undefined,
      videos: (post.video_versions || []).length || undefined,
      ...numbers, keys: Object.keys(post).length, filled,
    }));
  }
}

function post(code) {
  let best = null;
  const copies = [];
  for (const found of everyPost()) {
    if ((found.post.code || found.post.shortcode) !== code) continue;
    const size = JSON.stringify(found.post).length;
    copies.push(`${String(found.e.seq).padStart(6)} ${String(size).padStart(8)} characters  ${found.from}  at ${found.trail}`);
    if (!best || size > best.size) best = { ...found, size };
  }
  if (!best) {
    console.error(`no post "${code}" in any answer or document`);
    process.exit(1);
  }
  console.log(`every copy of ${code}:\n${copies.join('\n')}\n\n=== the fullest, seq ${best.e.seq}, ${best.from}, at ${best.trail}`);
  console.log(shape(best.post).join('\n'));
}

// What the `efg` parameter of a CDN address says about the rendition, when
// it can be read: base64 of a small JSON document.
function rendition(efg) {
  try {
    const said = JSON.parse(Buffer.from(efg, 'base64').toString('utf8'));
    return said.vencode_tag || JSON.stringify(said).slice(0, 80);
  } catch {
    return '';
  }
}

function header(headers, wanted) {
  for (const [name, value] of Object.entries(headers || {})) {
    if (name.toLowerCase() === wanted) return value;
  }
  return '';
}

// One line per file the CDN was asked for, however many requests fetched
// it: the pieces of a video are ranges of one file, asked for with a Range
// header or with `bytestart` and `byteend` in the address.
function media() {
  const files = new Map();
  for (const e of events()) {
    if (e.kind !== 'media') continue;
    const u = new URL(e.url);
    const name = u.pathname.split('/').pop();
    const file = files.get(name) || { seq: e.seq, requests: 0, got: 0, ranges: [], statuses: new Set(), page: e.page };
    file.requests++;
    file.statuses.add(e.status);
    file.type = header(e.responseHeaders, 'content-type') || file.type;
    file.got += Number(header(e.responseHeaders, 'content-length')) || 0;
    const whole = /\/(\d+)$/.exec(header(e.responseHeaders, 'content-range'));
    if (whole) file.whole = Number(whole[1]);
    const range = header(e.requestHeaders, 'range')
      || (u.searchParams.has('bytestart') ? `${u.searchParams.get('bytestart')}-${u.searchParams.get('byteend')}` : '');
    if (range) file.ranges.push(range.replace('bytes=', ''));
    file.stp = u.searchParams.get('stp') || file.stp;
    file.tag = rendition(u.searchParams.get('efg') || '') || file.tag;
    const expires = parseInt(u.searchParams.get('oe') || '', 16);
    if (expires) file.expires = new Date(expires * 1000).toISOString();
    files.set(name, file);
  }
  for (const [name, f] of files) {
    console.log(`${String(f.seq).padStart(6)} x${f.requests} ${[...f.statuses].join('/')} ${f.type || '?'} ` +
      `${f.got}${f.whole ? ` of ${f.whole}` : ''} bytes  ${name.slice(0, 60)}\n` +
      `${' '.repeat(9)}stp=${f.stp || ''}  rendition=${f.tag || ''}  expires=${f.expires || ''}\n` +
      (f.ranges.length ? `${' '.repeat(9)}ranges ${f.ranges.slice(0, 12).join(' ')}${f.ranges.length > 12 ? ` ... ${f.ranges.length} in all` : ''}\n` : '') +
      `${' '.repeat(9)}on ${f.page}`);
  }
}

function profiles() {
  for (const e of events()) {
    if (e.kind !== 'api' || friendlyName(e) !== 'PolarisProfilePageContentQuery') continue;
    let user;
    try { user = JSON.parse(e.body).data.user; } catch { continue; }
    if (!user) continue;
    const f = user.friendship_status || {};
    console.log(JSON.stringify({
      username: user.username,
      pk: user.pk,
      private: user.is_private,
      verified: user.is_verified,
      business: user.is_business,
      followers: user.follower_count,
      following: user.following_count,
      posts: user.media_count,
      mutual: user.mutual_followers_count,
      you_follow: f.following,
      follows_you: f.followed_by,
      story_up: user.latest_reel_media,
      named: (user.profile_context_links_with_user_ids || []).map(x => x.username),
    }));
  }
}

const args = process.argv.slice(2);
const fromAt = args.indexOf('--from');
const from = fromAt === -1 ? undefined : args.splice(fromAt, 2)[1];
const [mode, needle, which] = args;
try {
  LOG = recording(from);
} catch {
  LOG = String(from);
}
if (!fs.existsSync(LOG)) {
  console.error(`nothing recorded at ${LOG}; run record.js first`);
  process.exit(1);
}
console.error(`reading ${LOG}`);
switch (mode) {
  case undefined: overview(); break;
  case 'marks': marks(); break;
  case 'ops': ops(needle); break;
  case 'shape': show(needle || '', Number(which || 1)); break;
  case 'profiles': profiles(); break;
  case 'timeline': timeline(); break;
  case 'vars': vars(needle || ''); break;
  case 'signals': signals(); break;
  case 'embedded': embeddedIn(needle, Number(which || 1)); break;
  case 'posts': posts(needle); break;
  case 'post':
    if (!needle) { console.error('usage: summarize.js post <code>'); process.exit(2); }
    post(needle);
    break;
  case 'media': media(); break;
  default:
    console.error('usage: summarize.js [marks | ops [needle] | shape <needle> [n] | profiles | timeline | vars <needle> |\n' +
      '                     signals | embedded [needle] [n] | posts [code] | post <code> | media] [--from <directory or file>]');
    process.exit(2);
}
