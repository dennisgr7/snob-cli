// Removes authentication secrets from capture events by structure, not only by
// pattern: post data is parsed as a form, bodies as JSON (a `for (;;);` guard
// allowed, and a streamed answer of one JSON document per line), and HTML is
// scrubbed by key with escaped quotes tolerated. On the login and two-factor
// operations every value goes and only the keys stay.
//
// And by value: record.js hands `learn` every secret it meets in a place that
// is secret by position (a cookie, a token header, the `fb_dtsg` field), and
// `scrubKnown` then removes that same text wherever else it turns up, under
// whatever key and however deeply quoted, leaving `<redacted:name>` so that
// where a secret travels stays a finding. A pattern can only find a secret
// where somebody thought to look; a logging batch or a page's embedded JSON
// repeats the csrf token under names nobody listed. `sweep` runs the same
// pass over a finished recording, for the lines written before a value was
// learned. **The values are held in memory only and die with the process**,
// so a recording that was cut short cannot be swept afterwards.
//
// record.js runs every event through `scrubEvent` before writing it. To clean a
// recording made before that:
//
//     node tools/capture/sanitize.js <in.jsonl> <out.jsonl>
'use strict';
const fs = require('fs');

// Operations whose every value is authentication material: keep keys, drop values.
const AUTH = /Login|TwoStep|TwoFactor|2FA|one_tap|onetap|Cookie(Mutation|Query)|GetFrCookie|fxcal|ig_sso_users|accounts\/login|two_factor/i;
// Keys whose values are secret wherever they appear.
const SECRET_KEY = /^(claim|nonce|[a-z_]*_nonce|nonce_[a-z_]*|password|enc_password|[a-z_]*password[a-z_]*|verification_?code|verificationCode|code_?input|two_factor_identifier|identifier|login_nonce|trusted_device_nonce|fr|datr|mid|ig_did|csrftoken|sessionid|rur|shbid|shbts|ds_user_id_token|token|access_token|auth_token|fb_dtsg|fb_dtsg_ag|lsd|async_get_token|machine_id|session_key|cookie)$/i;

let redactions = 0;
function red(value) {
  if (typeof value === 'string' && value.startsWith('<redacted')) return value;
  redactions++;
  return typeof value === 'string' ? `<redacted:${value.length}>` : '<redacted>';
}

function walk(v, all) {
  if (Array.isArray(v)) return v.map(x => walk(x, all));
  if (v && typeof v === 'object') {
    const out = {};
    for (const [k, x] of Object.entries(v)) {
      if (SECRET_KEY.test(k) && (typeof x === 'string' || typeof x === 'number')) out[k] = red(String(x));
      else out[k] = walk(x, all);
    }
    return out;
  }
  if (all && typeof v === 'string' && v.length > 0) return red(v);
  if (typeof v === 'string') return scrubText(v);
  return v;
}

// A JSON document that may carry a for(;;); guard, or several documents one
// to a line: a query with deferred parts is answered that way, and read as
// free text it would be scrubbed by the short key list only.
function scrubJsonText(text, all) {
  if (typeof text !== 'string') return text;
  const guard = text.startsWith('for (;;);') ? 'for (;;);' : '';
  const rest = text.slice(guard.length);
  try {
    return guard + JSON.stringify(walk(JSON.parse(rest), all));
  } catch { /* not one document */ }
  try {
    const each = rest.split('\n');
    if (each.length > 1) {
      return guard + each.map(line => (line.trim() ? JSON.stringify(walk(JSON.parse(line), all)) : line)).join('\n');
    }
  } catch { /* nor a document a line */ }
  return scrubText(text);
}

// Free text (HTML, JS, anything): key-based redaction that tolerates escaped quotes.
function scrubText(s) {
  if (typeof s !== 'string') return s;
  const keys = 'claim|[a-z_]*nonce[a-z_]*|enc_password|password|verification_code|verificationCode|two_factor_identifier|trusted_device_nonce|access_token|fr|csrf_token|csrftoken|sessionid|fb_dtsg|async_get_token|machine_id|token';
  const re = new RegExp(`((?:\\\\*")(?:${keys})(?:\\\\*")\\s*:\\s*(?:\\\\*"))((?!<redacted)[^"\\\\]{6,})((?:\\\\*"))`, 'g');
  let out = s.replace(re, (_, a, v, b) => { redactions++; return `${a}<redacted:${v.length}>${b}`; });
  out = out.replace(/#PWD_BROWSER:\d+:\d+:[A-Za-z0-9+/=]+/g, () => { redactions++; return '#PWD_BROWSER:<redacted>'; });
  return out;
}

function scrubPost(post, all) {
  if (typeof post !== 'string' || !post) return post;
  // What record.js writes in place of a body it did not keep.
  if (/^<(binary|truncated|unavailable):/.test(post)) return post;
  const start = post.trim()[0];
  if (start === '{' || start === '[') return scrubJsonText(post, all);
  // Anything that is not a form (a multipart body, plain text) would come
  // out of URLSearchParams as one mangled key.
  if (!/^[^=&\s]+=/.test(post)) return all ? red(post) : scrubText(post);
  const params = new URLSearchParams(post);
  const out = new URLSearchParams();
  for (const [k, v] of params) {
    if (SECRET_KEY.test(k)) out.append(k, red(v));
    else if (k === 'variables' || k === 'params' || k === 'signed_body') out.append(k, scrubJsonText(v, all));
    else if (all && !/^(__[a-z]+|doc_id|fb_api_[a-z_]+|server_timestamps|av|dpr|jazoest|variables)$/.test(k)) out.append(k, red(v));
    else out.append(k, scrubText(v));
  }
  return out.toString();
}

// The operation's name wherever a request states it: the header, the copy
// record.js keeps on a `req` event (which has no headers yet), or the form
// field. A login whose name is only in its form is a login all the same.
function namedIn(e) {
  const header = Object.entries(e.requestHeaders || {}).find(([k]) => k.toLowerCase() === 'x-fb-friendly-name');
  const field = typeof e.postData === 'string' ? /(?:^|&)fb_api_req_friendly_name=([^&]*)/.exec(e.postData) : null;
  return `${(header && header[1]) || ''} ${e.friendly || ''} ${field ? field[1] : ''}`;
}

function scrubEvent(e) {
  const all = AUTH.test(namedIn(e)) || AUTH.test(e.url || '');
  if ('postData' in e) e.postData = scrubPost(e.postData, all);
  if ('body' in e) e.body = e.kind === 'doc' ? scrubText(e.body) : scrubJsonText(e.body, all);
  if (e.url) e.url = scrubText(e.url);
  return e;
}

// Secret text to the name it goes by.
const known = new Map();
let ordered = null;
let found = 0;

// A value short enough to occur by chance, or made of digits alone (an
// account's id, a timestamp), is not learned: replacing it everywhere would
// eat the data, and neither is a credential.
function learn(name, value) {
  if (typeof value !== 'string') return;
  const bare = value.trim().replace(/^"(.*)"$/, '$1');
  const label = String(name).replace(/[^A-Za-z0-9_-]/g, '_');
  const forms = new Set([bare]);
  // A cookie is percent-encoded in its header and not in a body.
  try { forms.add(decodeURIComponent(bare)); } catch { /* not encoded */ }
  forms.add(encodeURIComponent(bare));
  // And inside a JSON body its backslashes and quotes are escaped.
  for (const form of [...forms]) forms.add(JSON.stringify(form).slice(1, -1));
  for (const form of forms) {
    if (form.length < 8 || /^\d+$/.test(form) || form.startsWith('<redacted') || known.has(form)) continue;
    known.set(form, label);
    ordered = null;
  }
}

// Every string of `v`, with what was learned taken out; the longest first,
// so that a secret holding another goes whole.
function scrubKnown(v) {
  if (typeof v === 'string') {
    if (!known.size) return v;
    ordered = ordered || [...known].sort((a, b) => b[0].length - a[0].length);
    let out = v;
    for (const [value, label] of ordered) {
      if (!out.includes(value)) continue;
      found++;
      out = out.split(value).join(`<redacted:${label}>`);
    }
    return out;
  }
  if (Array.isArray(v)) return v.map(scrubKnown);
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, scrubKnown(x)]));
  return v;
}

// One line at a time and never the whole file: a long session is larger
// than the longest string Node can hold.
function* lines(file) {
  const fd = fs.openSync(file, 'r');
  try {
    const chunk = Buffer.alloc(1 << 20);
    let held = [];
    for (;;) {
      const read = fs.readSync(fd, chunk, 0, chunk.length, null);
      if (!read) break;
      const data = chunk.subarray(0, read);
      let from = 0;
      for (let at = data.indexOf(10); at !== -1; at = data.indexOf(10, from)) {
        held.push(Buffer.from(data.subarray(from, at)));
        const line = Buffer.concat(held).toString('utf8');
        held = [];
        from = at + 1;
        if (line) yield line;
      }
      if (from < read) held.push(Buffer.from(data.subarray(from)));
    }
    const last = Buffer.concat(held).toString('utf8');
    if (last) yield last;
  } finally {
    fs.closeSync(fd);
  }
}

// Runs every line of a finished recording through `scrubKnown` again and
// rewrites the file when any line changed. Returns how many did: the places
// a secret was written before it had been learned.
function sweep(file) {
  if (!known.size) return 0;
  const temp = `${file}.sweep`;
  const out = fs.openSync(temp, 'w');
  let changed = 0;
  try {
    for (const line of lines(file)) {
      const clean = JSON.stringify(scrubKnown(JSON.parse(line)));
      if (clean !== line) changed++;
      fs.writeSync(out, clean + '\n');
    }
  } finally {
    fs.closeSync(out);
  }
  if (changed) fs.renameSync(temp, file);
  else fs.unlinkSync(temp);
  return changed;
}

module.exports = { scrubEvent, learn, scrubKnown, sweep, lines, late: () => found };

if (require.main === module) {
  const [input, output] = process.argv.slice(2);
  if (!input || !output) {
    console.error('usage: sanitize.js <in.jsonl> <out.jsonl>');
    process.exit(2);
  }
  const out = fs.openSync(output, 'w');
  let count = 0;
  for (const line of lines(input)) {
    fs.writeSync(out, JSON.stringify(scrubEvent(JSON.parse(line))) + '\n');
    count++;
  }
  fs.closeSync(out);
  console.error(`events ${count}, redactions ${redactions}`);
}
