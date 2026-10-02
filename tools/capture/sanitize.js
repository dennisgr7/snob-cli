// Removes authentication secrets from capture events by structure, not only by
// pattern: post data is parsed as a form, bodies as JSON (a `for (;;);` guard
// allowed), and HTML is scrubbed by key with escaped quotes tolerated. On the
// login and two-factor operations every value goes and only the keys stay.
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

// A JSON document that may carry a for(;;); guard.
function scrubJsonText(text, all) {
  if (typeof text !== 'string') return text;
  const guard = text.startsWith('for (;;);') ? 'for (;;);' : '';
  try {
    return guard + JSON.stringify(walk(JSON.parse(text.slice(guard.length)), all));
  } catch {
    return scrubText(text);
  }
}

// Free text (HTML, JS, anything): key-based redaction that tolerates escaped quotes.
function scrubText(s) {
  if (typeof s !== 'string') return s;
  const keys = 'claim|[a-z_]*nonce[a-z_]*|enc_password|password|verification_code|verificationCode|two_factor_identifier|trusted_device_nonce|access_token|fr';
  const re = new RegExp(`((?:\\\\*")(?:${keys})(?:\\\\*")\\s*:\\s*(?:\\\\*"))((?!<redacted)[^"\\\\]{6,})((?:\\\\*"))`, 'g');
  let out = s.replace(re, (_, a, v, b) => { redactions++; return `${a}<redacted:${v.length}>${b}`; });
  out = out.replace(/#PWD_BROWSER:\d+:\d+:[A-Za-z0-9+/=]+/g, () => { redactions++; return '#PWD_BROWSER:<redacted>'; });
  return out;
}

function scrubPost(post, all) {
  if (typeof post !== 'string' || !post) return post;
  if (post.trim().startsWith('{')) return scrubJsonText(post, all);
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

function scrubEvent(e) {
  const friendly = Object.entries(e.requestHeaders || {}).find(([k]) => k.toLowerCase() === 'x-fb-friendly-name');
  const all = AUTH.test((friendly && friendly[1]) || '') || AUTH.test(e.url || '');
  if ('postData' in e) e.postData = scrubPost(e.postData, all);
  if ('body' in e) e.body = e.kind === 'doc' ? scrubText(e.body) : scrubJsonText(e.body, all);
  if (e.url) e.url = scrubText(e.url);
  return e;
}

module.exports = { scrubEvent };

if (require.main === module) {
  const [input, output] = process.argv.slice(2);
  if (!input || !output) {
    console.error('usage: sanitize.js <in.jsonl> <out.jsonl>');
    process.exit(2);
  }
  const lines = fs.readFileSync(input, 'utf8').split('\n').filter(Boolean);
  const out = lines.map(line => JSON.stringify(scrubEvent(JSON.parse(line))));
  fs.writeFileSync(output, out.join('\n') + '\n');
  console.error(`events ${out.length}, redactions ${redactions}`);
}
