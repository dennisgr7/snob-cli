// Labels the calls in a recording that tell Instagram something, as opposed
// to asking it something.
//
// snob reads and never reports: nothing it sends may mark a post, a reel or
// a comment as seen, count a view, or log time spent (AGENTS.md, "Nothing
// ever marks anything as seen"). The web app does all of that on its own
// while somebody merely looks, and those calls sit in a recording between
// the reads, as POSTs to the same two GraphQL addresses. A reader of the
// recording who takes one of them for part of "how a post is opened" would
// write it into snob. So record.js labels them as it writes, and
// summarize.js labels them again as it reads (a recording made before a
// spelling was added here is still labeled).
//
// **A label is a pointer, not a verdict.** It is given by name and address,
// and Instagram renames things; a call with no label is not thereby safe to
// send. What decides that is `snob_ig::allowlist`, which names every call
// snob may make. The labels:
//
//   seen       says what was looked at: a seen or read receipt, a view or
//              impression count, time spent, a search kept as recent
//   log        the app's own logging and beacons, whose batches carry
//              impressions and video-play events among everything else
//   mutation   a Relay operation named ...Mutation: a write of some kind
//   rest-post  a request that is not a GET outside the GraphQL addresses;
//              usually a write, though a few reads are POSTs
//
// and beside any of them, or alone, `carries`: the names of the fields in a
// call's form or variables that report what was seen from inside an ordinary
// read (the feed and the reels screen send their watched lists that way).
'use strict';

// The spellings `crates/snob-core/tests/no_seen.rs` keeps out of the Rust
// source, which were read off earlier recordings, and the ones a name is
// likely to take next. That guard reads `.rs` files only, so naming them
// here does not trip it.
const SEEN = [
  /media\/seen|reels\/seen|\/seen\/?(\?|$)/i,
  /mark_?(as_?)?(seen|read)|inbox_seen|mark_su_seen/i,
  /Seen(Mutation|State)?\b/,
  /MarkThreadAsRead|ReadReceipt/i,
  /RegisterInRecentSearches|recent_searches\/(register|add)/i,
  /ScreenTime|time_?spent|TimeSpent/i,
  /unified_cvc|video_?view|log_?view|view_?count.*(Mutation|log)|Impression/i,
  /Watch(ed|Time)|PlayCount.*Mutation|playback.*(log|report)/i,
];

// Logging by address or by name. `/ajax/bz` is the beacon that batches the
// app's events and `/ajax/qm/` its companion (`snob_ig::allowlist` leaves
// both out because their payloads had not been recorded); `falco` and
// `logging_client_events` are the same thing under newer names; `/tr` is the
// Facebook pixel.
const LOG = [
  /\/ajax\/(bz|qm|bnzai)\b/i,
  /\/logging(\/|_client_events)|falco|banzai/i,
  /\/(tr|pixel)\/?(\?|$)/i,
  /Logg(er|ing)|Telemetry|QPL|Falco/,
];

// Fields that report what was seen from inside a read. The first three are
// in `no_seen.rs`; the rest are what such a field would be called.
const CARRIED = /\b(feed_view_info|seen_reels|client_view_state_media_list|unified_cvc|seen_state|viewed_media_ids|seen_media_ids|session_impressions|impression_token|time_spent(_ms)?|watch_time(_ms)?|view_info)\b/g;

const GRAPHQL = /^\/(api\/graphql|graphql\/query)\/?$/;

function header(event, wanted) {
  for (const [name, value] of Object.entries(event.requestHeaders || {})) {
    if (name.toLowerCase() === wanted) return value;
  }
  return '';
}

// The operation's name: the header the app sends it in, what record.js kept
// of it on a `req` event, or the form field that repeats it.
function friendlyName(event) {
  const named = header(event, 'x-fb-friendly-name') || event.friendly;
  if (named) return named;
  const post = typeof event.postData === 'string' ? event.postData : '';
  const field = /(?:^|&)fb_api_req_friendly_name=([^&]*)/.exec(post);
  return field ? decodeURIComponent(field[1]) : '';
}

function pathOf(url) {
  try { return new URL(url).pathname; } catch { return ''; }
}

// `{}` for a call that reads, else `{ signal, carries }` with either left out
// when it does not apply.
function signalOf(event) {
  const name = friendlyName(event);
  const path = pathOf(event.url || '');
  const post = typeof event.postData === 'string' ? event.postData : '';
  const out = {};
  // The form as sent and as read: field names survive both, and the
  // variables are JSON inside a percent-encoded field.
  let readable = post;
  try { readable = decodeURIComponent(post.replace(/\+/g, ' ')); } catch { /* not a form */ }
  const carried = [...new Set((readable.match(CARRIED) || []))];
  if (carried.length) out.carries = carried;

  const what = `${name} ${path}`;
  const sends = (event.method || 'GET') !== 'GET';
  // A read named for what it shows ("...SeenStateQuery") asks and does not
  // tell, so a seen label needs a request that can carry a body, or a name
  // that says Mutation. Asked before the logging, which a time-spent logger
  // is by name and is not by what it says.
  if ((sends || /Mutation$/.test(name)) && !/Query$/.test(name) && SEEN.some(re => re.test(what))) out.signal = 'seen';
  else if (LOG.some(re => re.test(what))) out.signal = 'log';
  else if (/Mutation$/.test(name)) out.signal = 'mutation';
  else if (sends && !GRAPHQL.test(path)) out.signal = 'rest-post';
  return out;
}

module.exports = { signalOf, friendlyName };
