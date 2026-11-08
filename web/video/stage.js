/**
 * The hero video's timeline, and how the stage looks at any moment of it.
 *
 * `STAGE` is set by video.mjs before this runs: `frames`, the terminal frames
 * record.sh captured, already read into runs of styled text, and `chat`, what
 * `snob mcp` answered on the same world. `renderAt(seconds)` draws that moment
 * and returns a hash of what it drew, so the renderer can skip a screenshot
 * when nothing changed.
 *
 * Three scenes in one loop: `snob scan` and its unfollowers with one profile
 * opened, `snob profile` with a story kept, then the same world through an
 * agent that lists the unfollowers and unfollows one, after a yes given in
 * the form the server asks for. The last moment fades into the first.
 */
/* global STAGE */
const { frames, chat } = STAGE;

// --- The timeline -----------------------------------------------------------

/** Keystrokes are human: a fixed seed, so every render types the same. */
function seeded(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let x = Math.imul(state ^ (state >>> 15), 1 | state);
    x = (x + Math.imul(x ^ (x >>> 7), 61 | x)) ^ x;
    return ((x ^ (x >>> 14)) >>> 0) / 4294967296;
  };
}
const jitter = seeded(7);

let now = 0;

/** When each of `count` keys lands, starting now, about `mean` seconds apart. */
function keystrokes(count, mean) {
  const times = [];
  for (let i = 0; i < count; i += 1) {
    now += mean * (0.55 + jitter() * 0.9);
    times.push(now);
  }
  return times;
}

/** The terminal: which frame shows from when, and how its command is typed. */
const steps = [];

function show(frame, hold) {
  if (!frames[frame]) throw new Error(`no frame ${frame}`);
  steps.push({ at: now, frame });
  now += hold;
}

/**
 * The command on the cursor's row of `frame`, typed out after the prompt,
 * then Enter: the cursor drops to the next line while the command starts.
 */
function type(frame) {
  const { cursor } = frames[frame];
  const start = 2; // after "$ "
  const at = now;
  const keys = keystrokes(cursor.x - start, 0.075);
  steps.push({ at, frame, typing: { row: cursor.y, start, keys } });
  now += 0.4;
  steps.push({ at: now, frame, enter: true });
  now += 0.3;
}

show('scan-prompt', 0.7);
type('scan-typed');
show('scan-tray', 1.7);
show('scan-unfollowers', 1.5);
show('scan-down-1', 0.38);
show('scan-down-2', 0.38);
show('scan-down-3', 0.6);
show('scan-peek', 2.1);
show('scan-back', 0.55);
show('scan-receipts', 1.0);
show('profile-prompt', 0.6);
type('profile-typed');
show('profile-card', 2.0);
show('profile-stories-row', 0.7);
show('profile-stories', 1.4);
show('profile-saved', 1.6);
show('profile-receipts', 1.3);

const FADE = 0.45;
const TO_CHAT = now;
now += FADE;

/** The chat: when each thing in it happens. */
const first = chat.unfollowers.items.slice(0, 4).map((item) => item.username);
const said = {
  ask: 'Who doesn’t follow me back?',
  answer: [
    `${chat.unfollowers.total} of the accounts you follow don’t follow you back, among them `,
    ...first.flatMap((name, i) => [
      { code: `@${name}` },
      i < first.length - 2 ? ', ' : i === first.length - 2 ? ' and ' : '.',
    ]),
  ],
  order: 'Unfollow dex.drums',
  done: ['Done: you no longer follow ', { code: `@${chat.unfollow.target.username}` }, '.'],
};

now += 0.5;
const ask = { keys: keystrokes(said.ask.length, 0.05) };
now += 0.35;
ask.sent = now;
now += 0.45;
const list = { at: now };
now += 1.1;
list.done = now;
now += 0.25;
const answer = { at: now, words: wordTimes(said.answer, 0.055) };
now += 1.8;
const order = { keys: keystrokes(said.order.length, 0.055) };
now += 0.35;
order.sent = now;
now += 0.45;
const unfollow = { at: now };
now += 0.6;
const form = { at: now };
now += 1.25;
form.tick = now;
now += 0.65;
form.press = now;
now += 0.3;
unfollow.done = now;
now += 0.3;
const done = { at: now, words: wordTimes(said.done, 0.07) };
now += 2.3;

const TO_START = now;
const TOTAL = now + 0.5;

/** When each word of a message appears, streamed from now on. */
function wordTimes(parts, each) {
  const count = parts.reduce((n, part) => n + (typeof part === 'string' ? part.split(/(?<= )/).length : 1), 0);
  const times = [];
  for (let i = 0; i < count; i += 1) times.push(now + i * each);
  now += count * each;
  return times;
}

// --- Drawing ----------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const screen = $('screen');
const thread = $('thread');
const typed = $('typed');
const send = $('send');
const pointer = $('pointer');

const TICK =
  '<svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5" fill="none" stroke="#12151a" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';

const BOXES = { '─': 'h', '│': 'v', '╭': 'tl', '╮': 'tr', '╰': 'bl', '╯': 'br' };

const escapeHtml = (text) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/** One run of styled text; characters outside ASCII each held to a cell. */
function runHtml(run) {
  let body = '';
  let plain = '';
  for (const ch of run.t) {
    if (ch.charCodeAt(0) < 0x80) {
      plain += ch;
      continue;
    }
    body += escapeHtml(plain);
    plain = '';
    body += BOXES[ch] ? `<span class="c ${BOXES[ch]}"></span>` : `<span class="c">${escapeHtml(ch)}</span>`;
  }
  body += escapeHtml(plain);
  const classes = ['run', run.b && 'b', run.u && 'u'].filter(Boolean).join(' ');
  const style = `color:${run.fg};${run.bg ? `background:${run.bg};` : ''}`;
  return `<span class="${classes}" style="${style}">${body}</span>`;
}

/** The runs of `row` cut to its first `columns` cells. */
function cut(row, columns) {
  const out = [];
  let left = columns;
  for (const run of row) {
    if (left <= 0) break;
    const chars = [...run.t];
    out.push({ ...run, t: chars.slice(0, left).join('') });
    left -= chars.length;
  }
  return out;
}

/** The terminal at time `t`: the frame, and where the cursor is if it shows. */
function terminalAt(t) {
  let index = 0;
  while (index + 1 < steps.length && steps[index + 1].at <= t) index += 1;
  const step = steps[index];
  const frame = frames[step.frame];
  let rows = frame.rows;
  let cursor = frame.cursor.on ? { x: frame.cursor.x, y: frame.cursor.y } : null;
  let blinks = true;
  if (step.typing) {
    const { row, start, keys } = step.typing;
    const count = keys.filter((at) => at <= t).length;
    rows = rows.map((r, y) => (y === row ? cut(r, start + count) : r));
    cursor = { x: start + count, y: row };
    // The cursor holds still while keys arrive, as a terminal's does.
    blinks = count === keys.length && t - keys[keys.length - 1] > 0.5;
  } else if (step.enter) {
    cursor = { x: 0, y: frame.cursor.y + 1 };
    blinks = false;
  }
  const lit = cursor && (!blinks || (t - step.at) % 1.1 < 0.55);
  return { rows, cursor: lit ? cursor : null };
}

function drawTerminal(state) {
  screen.innerHTML =
    state.rows
      .map((row, y) => `<div class="line" style="top:${y * 38}px">${row.map(runHtml).join('')}</div>`)
      .join('') +
    (state.cursor
      ? `<div id="cursor" style="left:calc(${state.cursor.x} * var(--cell));top:${state.cursor.y * 38}px"></div>`
      : '');
}

const clamp = (x) => Math.min(1, Math.max(0, x));
const ease = (x) => 1 - (1 - clamp(x)) ** 3;
const easeInOut = (x) => {
  const c = clamp(x);
  return c < 0.5 ? 4 * c * c * c : 1 - (-2 * c + 2) ** 3 / 2;
};

/** Something that has just appeared: faded and lifted in over a quarter second. */
const entering = (t, at) => {
  const k = ease((t - at) / 0.25);
  return `opacity:${k.toFixed(3)};transform:translateY(${((1 - k) * 14).toFixed(2)}px)`;
};

/** The words of `parts` shown by time `t`, inline code kept whole. */
function streamed(parts, times, t) {
  let shown = times.filter((at) => at <= t).length;
  let html = '';
  for (const part of parts) {
    if (shown <= 0) break;
    if (typeof part === 'string') {
      const words = part.split(/(?<= )/);
      html += escapeHtml(words.slice(0, shown).join(''));
      shown -= words.length;
    } else {
      html += `<code>${escapeHtml(part.code)}</code>`;
      shown -= 1;
    }
  }
  return html;
}

function spinner(t) {
  return `<div class="spin" style="transform:rotate(${Math.round((t * 400) % 360)}deg)"></div>`;
}

function tool(t, name, args, step, out) {
  const finished = t >= step.done;
  return `<div class="tool" style="${entering(t, step.at)}">
    <div class="head"><span>${name}</span><span class="state">${finished ? '<span class="done">✓</span>' : spinner(t)}</span></div>
    <div class="args">${args}</div>
    ${finished ? `<div class="out">→ ${out}</div>` : ''}
  </div>`;
}

function drawChat(t) {
  // The input: the question being typed, empty once sent.
  const typing = [
    [ask, said.ask],
    [order, said.order],
  ].find(([message]) => t >= message.keys[0] - 0.05 && t < message.sent);
  if (typing) {
    const [message, text] = typing;
    const count = message.keys.filter((at) => at <= t).length;
    typed.className = '';
    typed.innerHTML = `${escapeHtml(text.slice(0, count))}<span id="caret"></span>`;
    send.className = count > 0 ? 'ready' : '';
  } else {
    typed.className = 'empty';
    const caret = (t - TO_CHAT) % 1.1 < 0.55 && t < ask.keys[0] ? '<span id="caret"></span>' : '';
    typed.innerHTML = `${caret}Ask anything`;
    send.className = '';
  }

  const items = [];
  if (t >= ask.sent) items.push(`<div class="you" style="${entering(t, ask.sent)}">${escapeHtml(said.ask)}</div>`);
  if (t >= list.at) {
    items.push(`<div class="agent">
      ${tool(t, 'snob_list', 'list: "unfollowers"', list, `total: <b>${chat.unfollowers.total}</b>`)}
      ${t >= answer.at ? `<p>${streamed(said.answer, answer.words, t)}</p>` : ''}
    </div>`);
  }
  if (t >= order.sent) items.push(`<div class="you" style="${entering(t, order.sent)}">${escapeHtml(said.order)}</div>`);
  if (t >= unfollow.at) {
    const field = chat.question.requestedSchema.properties.confirm;
    const ticked = t >= form.tick;
    const pressed = t >= form.press && t < form.press + 0.25;
    const answered = t >= unfollow.done;
    items.push(`<div class="agent">
      ${tool(t, 'snob_unfollow', `account: "${escapeHtml(chat.unfollow.target.username)}"`, unfollow, `outcome: <b>"${escapeHtml(chat.unfollow.outcome)}"</b>`)}
      ${
        t >= form.at
          ? `<div class="form" style="${entering(t, form.at)}">
              <div class="who">snob asks</div>
              <div class="what">${escapeHtml(chat.question.message)}</div>
              <label><span class="box${ticked ? ' ticked' : ''}">${ticked ? TICK : ''}</span>
                <span>${escapeHtml(field.title)}<small>${escapeHtml(field.description)}</small></span></label>
              <div class="buttons">${
                answered
                  ? '<span>Sent</span>'
                  : `<span>Decline</span><span class="go${pressed ? ' pressed' : ''}">Submit</span>`
              }</div>
            </div>`
          : ''
      }
      ${t >= done.at ? `<p>${streamed(said.done, done.words, t)}</p>` : ''}
    </div>`);
  }
  thread.innerHTML = items.join('');

  // The pointer: in from the corner, onto the box, onto Submit, and away.
  const box = thread.querySelector('.box');
  const go = thread.querySelector('.go');
  const shown = box && t >= form.at + 0.25 && t < unfollow.done + 0.45;
  if (shown) {
    const center = (el) => {
      const r = el.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    };
    const from = { x: 1080, y: 860 };
    const onBox = center(box);
    const onGo = go ? center(go) : onBox;
    let at;
    if (t < form.tick) at = mix(from, onBox, easeInOut((t - form.at - 0.25) / (form.tick - form.at - 0.35)));
    else if (t < form.press) at = mix(onBox, onGo, easeInOut((t - form.tick - 0.1) / (form.press - form.tick - 0.2)));
    else at = onGo;
    const fade = clamp((unfollow.done + 0.45 - t) / 0.25);
    pointer.style.cssText = `left:${(at.x - 6).toFixed(1)}px;top:${(at.y - 4).toFixed(1)}px;opacity:${fade.toFixed(3)}`;
  } else {
    pointer.style.cssText = 'opacity:0';
  }
}

const mix = (a, b, k) => ({ x: a.x + (b.x - a.x) * k, y: a.y + (b.y - a.y) * k });

/** FNV-1a over what the stage shows now. */
function hash(text) {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i += 1) {
    h ^= text.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return (h >>> 0).toString(16);
}

function renderAt(seconds) {
  const t = ((seconds % TOTAL) + TOTAL) % TOTAL;
  // Into the chat, and at the end back to the very first frame.
  const chatShows = t < TO_START ? clamp((t - TO_CHAT) / FADE) : 1 - clamp((t - TO_START) / (TOTAL - TO_START));
  const agentTab = chatShows >= 0.5;
  drawTerminal(terminalAt(t >= TO_START ? 0 : Math.min(t, TO_CHAT)));
  drawChat(t >= TO_START ? TO_START : t);
  $('term').style.opacity = (1 - chatShows).toFixed(3);
  $('chat').style.opacity = chatShows.toFixed(3);
  $('tab-terminal').className = agentTab ? '' : 'on';
  $('tab-agent').className = agentTab ? 'on' : '';
  return hash(document.querySelector('header').outerHTML + $('term').outerHTML + $('chat').outerHTML + pointer.style.cssText);
}

window.renderAt = renderAt;
window.TOTAL = TOTAL;
/** When the poster is taken: the unfollowers, the answer to the headline. */
window.POSTER = steps.find((step) => step.frame === 'scan-unfollowers').at + 0.5;
