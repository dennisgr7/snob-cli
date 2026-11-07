// @ts-check
/**
 * Renders the hero's video into public/video/: hero.webm (VP9) and hero.mp4
 * (H.264, for the browsers without VP9), and hero.webp, the poster shown
 * before it plays and instead of it under reduced motion. The output is
 * committed; run this again when the captures or the stage change.
 *
 * What it shows was recorded by video/record.sh, never drawn by hand: the
 * terminal frames are the real views of snob on an invented Instagram, and
 * the chat's answers are what `snob mcp` said on the same world. This script
 * only lays them out (video/stage.html, video/stage.js) and films them, one
 * frame at a time: the stage draws any moment as a pure function of the time,
 * so a frame is never a screenshot of an animation caught halfway.
 *
 * Usage, after `pnpm build` (it reads the Plex files Astro downloaded), with
 * ffmpeg on the PATH:
 *   pnpm video
 * or, to look at some moments before filming all of them, which writes a PNG
 * of each into a temporary directory and encodes nothing:
 *   pnpm video --at 4.5 --at 21
 *
 * No new dependency: Playwright's Chromium draws and ffmpeg encodes.
 */
import { spawn } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { mkdir, mkdtemp, rename, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { chromium } from '@playwright/test';
import { fontCss } from './fonts.mjs';

const ROOT = process.cwd();
const VIDEO = path.join(ROOT, 'video');
const CAPTURES = path.join(VIDEO, 'captures');
const OUT = path.join(ROOT, 'public', 'video');

const WIDTH = 1200;
const HEIGHT = 900;
const FPS = 30;
/** The terminal's height in rows, as record.sh draws it. */
const ROWS = 19;

/** The stage's colors: DESIGN.md's terminal palette, for the 16 the views use. */
const BG = '#12151a';
const FG = '#e6e8ec';
const DIM = '#8b929c';
const PALETTE = [
  '#1b1f26', '#e06c75', '#7ad46b', '#e5c07b', '#6aa8f0', '#f472b6', '#3ec7d6', '#c8ccd2',
  '#6b7280', '#ef8d95', '#9be38f', '#f0d49b', '#93c1f7', '#f9a8d4', '#7ddfe9', '#ffffff',
];

/** @param {string} a @param {string} b @param {number} k */
function mix(a, b, k) {
  const channel = (/** @type {string} */ hex, /** @type {number} */ i) => parseInt(hex.slice(1 + i * 2, 3 + i * 2), 16);
  const out = [0, 1, 2].map((i) => Math.round(channel(a, i) * (1 - k) + channel(b, i) * k));
  return `#${out.map((c) => c.toString(16).padStart(2, '0')).join('')}`;
}

/** @param {number[]} params @param {number} i @returns {[string | null, number]} */
function extended(params, i) {
  if (params[i + 1] === 5) {
    const n = params[i + 2] ?? 0;
    return [n < 16 ? (PALETTE[n] ?? null) : null, i + 2];
  }
  if (params[i + 1] === 2) {
    const [r, g, b] = params.slice(i + 2, i + 5).map((c) => (c ?? 0).toString(16).padStart(2, '0'));
    return [`#${r}${g}${b}`, i + 4];
  }
  return [null, i];
}

/**
 * @typedef {{ fg: string | null, bg: string | null, bold: boolean, dim: boolean, underline: boolean, reverse: boolean }} Pen
 * @typedef {{ t: string, fg: string, bg: string | null, b: boolean, u: boolean }} Run
 */

/** @returns {Pen} */
const freshPen = () => ({ fg: null, bg: null, bold: false, dim: false, underline: false, reverse: false });

/** @param {Pen} pen @param {number[]} params */
function apply(pen, params) {
  if (params.length === 0) params = [0];
  for (let i = 0; i < params.length; i += 1) {
    const p = params[i] ?? 0;
    if (p === 0) Object.assign(pen, freshPen());
    else if (p === 1) pen.bold = true;
    else if (p === 2) pen.dim = true;
    else if (p === 4) pen.underline = true;
    else if (p === 7) pen.reverse = true;
    else if (p === 22) pen.bold = pen.dim = false;
    else if (p === 24) pen.underline = false;
    else if (p === 27) pen.reverse = false;
    else if (p >= 30 && p <= 37) pen.fg = PALETTE[p - 30] ?? null;
    else if (p >= 90 && p <= 97) pen.fg = PALETTE[p - 82] ?? null;
    else if (p >= 40 && p <= 47) pen.bg = PALETTE[p - 40] ?? null;
    else if (p >= 100 && p <= 107) pen.bg = PALETTE[p - 92] ?? null;
    else if (p === 39) pen.fg = null;
    else if (p === 49) pen.bg = null;
    else if (p === 38) [pen.fg, i] = extended(params, i);
    else if (p === 48) [pen.bg, i] = extended(params, i);
  }
}

/** What a pen draws with: dim as the terminal palette dims, reverse as a terminal swaps. */
function colors(/** @type {Pen} */ pen) {
  const base = pen.fg ?? FG;
  if (pen.reverse) return { fg: pen.dim ? '#5d636d' : (pen.bg ?? BG), bg: base };
  const fg = pen.dim ? (pen.fg ? mix(pen.fg, BG, 0.4) : DIM) : base;
  return { fg, bg: pen.bg };
}

/** One line of `tmux capture-pane -e`, as runs of one style each. */
function parseLine(/** @type {string} */ line) {
  /** @type {Run[]} */
  const runs = [];
  const pen = freshPen();
  // SGR is all capture-pane writes, but anything else is skipped, not drawn.
  const tokens = line.split(/(\x1b\[[0-9;:?]*[A-Za-z]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\))/);
  for (const token of tokens) {
    if (!token) continue;
    if (token.startsWith('\x1b')) {
      if (token.startsWith('\x1b[') && token.endsWith('m')) {
        apply(pen, token.slice(2, -1).split(/[;:]/).filter((p) => p !== '').map(Number));
      }
      continue;
    }
    const { fg, bg } = colors(pen);
    const last = runs.at(-1);
    if (last && last.fg === fg && last.bg === bg && last.b === pen.bold && last.u === pen.underline) last.t += token;
    else runs.push({ t: token, fg, bg, b: pen.bold, u: pen.underline });
  }
  return runs;
}

/** A frame record.sh wrote: where the cursor was, then the pane. */
function parseFrame(/** @type {string} */ name) {
  const [head = '', ...lines] = readFileSync(path.join(CAPTURES, `${name}.ans`), 'utf8').split('\n');
  const cursor = /^cursor (\d+) (\d+) (\d)$/.exec(head);
  if (!cursor) throw new Error(`${name}.ans does not start with the cursor`);
  const rows = lines.slice(0, ROWS).map(parseLine);
  while (rows.length < ROWS) rows.push([]);
  return { cursor: { x: Number(cursor[1]), y: Number(cursor[2]), on: cursor[3] === '1' }, rows };
}

const json = (/** @type {string} */ name) => JSON.parse(readFileSync(path.join(CAPTURES, name), 'utf8'));

const frames = Object.fromEntries(
  readdirSync(CAPTURES)
    .filter((file) => file.endsWith('.ans'))
    .map((file) => file.slice(0, -'.ans'.length))
    .map((name) => [name, parseFrame(name)]),
);
const chat = {
  unfollowers: json('mcp-unfollowers.json'),
  question: json('mcp-question.json'),
  unfollow: json('mcp-unfollow.json'),
};

// `<` escaped, so nothing in the data can close the script it travels in.
const data = JSON.stringify({ frames, chat }).replace(/</g, '\\u003c');
const html = readFileSync(path.join(VIDEO, 'stage.html'), 'utf8')
  .replace('/*FONTS*/', () => fontCss())
  .replace('/*SCRIPT*/', () => `const STAGE = ${data};\n${readFileSync(path.join(VIDEO, 'stage.js'), 'utf8')}`);

/** Runs ffmpeg, failing with what it said. @param {string[]} args */
function ffmpeg(args) {
  return new Promise((resolve, reject) => {
    const child = spawn('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-y', ...args], { stdio: ['ignore', 'inherit', 'inherit'] });
    child.on('error', reject);
    child.on('exit', (code) => (code === 0 ? resolve(undefined) : reject(new Error(`ffmpeg exited ${code}`))));
  });
}

/** Writes `file` in public/video/ through a temporary beside it, so a failure never leaves half a file. */
async function publish(/** @type {string} */ file, /** @type {(to: string) => Promise<unknown>} */ make) {
  const destination = path.join(OUT, file);
  const temporary = path.join(OUT, `.${process.pid}-${file}`);
  try {
    await make(temporary);
    await rename(temporary, destination);
  } catch (error) {
    await rm(temporary, { force: true });
    throw error;
  }
  console.log(`[video] public/video/${file}`);
}

// Colors as the site's: sRGB into BT.709, tagged so players read them back the same.
const COLOR = [
  '-vf', `fps=${FPS},scale=out_color_matrix=bt709:out_range=tv,format=yuv420p`,
  '-colorspace', 'bt709', '-color_primaries', 'bt709', '-color_trc', 'bt709', '-color_range', 'tv',
];

/** The moments asked for with `--at`, in seconds. */
const stills = process.argv.flatMap((arg, i, all) => (arg === '--at' ? [Number(all[i + 1])] : []));
if (stills.some((t) => !Number.isFinite(t))) throw new Error('--at takes a number of seconds');

const work = await mkdtemp(path.join(tmpdir(), 'snob-video-'));
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({ viewport: { width: WIDTH, height: HEIGHT }, deviceScaleFactor: 1 });
  await page.setContent(html);
  await page.evaluate(async () => {
    // Every face now, not when first used; the fallbacks Astro adds name local
    // fonts this machine may not have, and those fail harmlessly.
    await Promise.allSettled([...document.fonts].map((face) => face.load()));
    await document.fonts.ready;
  });
  const total = await page.evaluate(() => /** @type {any} */ (window).TOTAL);
  const poster = await page.evaluate(() => /** @type {any} */ (window).POSTER);
  const count = Math.round(total * FPS);

  if (stills.length > 0) {
    for (const t of stills) {
      await page.evaluate((at) => /** @type {any} */ (window).renderAt(at), t);
      const file = path.join(tmpdir(), `snob-video-${t}s.png`);
      await page.screenshot({ path: file });
      console.log(`[video] ${file}`);
    }
    console.log(`[video] the loop lasts ${total.toFixed(2)} s; the poster is at ${poster.toFixed(2)} s`);
    process.exitCode = 0;
  } else {

  // Every distinct frame once, with how long it lasts.
  /** @type {{ file: string, frames: number }[]} */
  const shots = [];
  let last = '';
  for (let i = 0; i < count; i += 1) {
    const drawn = await page.evaluate((t) => /** @type {any} */ (window).renderAt(t), i / FPS);
    const previous = shots.at(-1);
    if (drawn === last && previous) {
      previous.frames += 1;
      continue;
    }
    last = drawn;
    const file = path.join(work, `${String(shots.length).padStart(5, '0')}.png`);
    await page.screenshot({ path: file });
    shots.push({ file, frames: 1 });
    if (shots.length % 50 === 0) console.log(`[video] ${i + 1} of ${count} frames, ${shots.length} distinct`);
  }
  console.log(`[video] ${count} frames (${total.toFixed(1)} s), ${shots.length} distinct`);

  await page.evaluate((t) => /** @type {any} */ (window).renderAt(t), poster);
  const still = path.join(work, 'poster.png');
  await page.screenshot({ path: still });

  // The concat demuxer takes each distinct frame for its duration; the last
  // is named twice, as it asks, or its duration is dropped.
  const list = path.join(work, 'frames.txt');
  await writeFile(
    list,
    `${shots.map((shot) => `file '${shot.file}'\nduration ${(shot.frames / FPS).toFixed(6)}`).join('\n')}\nfile '${shots.at(-1)?.file}'\n`,
  );
  const input = ['-f', 'concat', '-safe', '0', '-i', list, '-t', (count / FPS).toFixed(6)];

  await mkdir(OUT, { recursive: true });
  await publish('hero.webm', (to) =>
    ffmpeg([...input, ...COLOR, '-c:v', 'libvpx-vp9', '-crf', '34', '-b:v', '0', '-deadline', 'good', '-cpu-used', '2',
      '-row-mt', '1', '-g', '240', '-an', '-f', 'webm', to]),
  );
  await publish('hero.mp4', (to) =>
    ffmpeg([...input, ...COLOR, '-c:v', 'libx264', '-preset', 'slow', '-tune', 'animation', '-crf', '24',
      '-profile:v', 'high', '-g', '240', '-movflags', '+faststart', '-an', '-f', 'mp4', to]),
  );
  await publish('hero.webp', (to) => ffmpeg(['-i', still, '-c:v', 'libwebp', '-quality', '82', '-compression_level', '6', '-f', 'webp', to]));
  }
} finally {
  await browser.close();
  await rm(work, { recursive: true, force: true });
}
