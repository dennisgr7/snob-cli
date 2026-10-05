// @ts-check
/**
 * Draws the brand images into public/: og.png (1200×630, the preview a link
 * shows), favicon-32.png and apple-touch-icon.png (180×180). An SVG favicon
 * cannot use the web font, so the mark is rendered here instead. The output is
 * committed; run this again only when the mark, the headline or the domain
 * changes.
 *
 * Usage, after `pnpm build` (it reads the Plex files Astro downloaded):
 *   pnpm brand
 *
 * No new dependency: Playwright's Chromium does the drawing, and the fonts go
 * in as data URLs, which load where a file:// font would not.
 */
import { readFileSync } from 'node:fs';
import { rename, rm } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { chromium } from '@playwright/test';
import { HERO, HERO_INSTALL } from '../src/lib/content.ts';
import { SITE } from '../src/lib/site.ts';

const DIST = path.join(process.cwd(), 'dist');
const PUBLIC = path.join(process.cwd(), 'public');

/** The @font-face rules and font variables Astro wrote into the built page. */
function fontCss() {
  const html = readFileSync(path.join(DIST, 'index.html'), 'utf8');
  const faces = html.match(/@font-face\{[^}]*\}/g) ?? [];
  const variables = html.match(/:root\{--font-plex-[^}]*\}/g) ?? [];
  if (faces.length === 0) throw new Error('No @font-face in dist/index.html: run pnpm build first');
  const inlined = faces.map((face) =>
    face.replace(/url\("?(\/_astro\/fonts\/[^")]+)"?\)/g, (_, url) => {
      const bytes = readFileSync(path.join(DIST, url));
      return `url(data:font/woff2;base64,${bytes.toString('base64')})`;
    }),
  );
  return [...inlined, ...variables].join('\n');
}

const BASE = `
  * { margin: 0; padding: 0; box-sizing: border-box; }
  :root { --bg: #0b0d10; --text: #e6e8ec; --dim: #8b929c; --accent: #f472b6; }
  body { background: var(--bg); color: var(--text); font-family: var(--font-plex-sans); }
  .mono { font-family: var(--font-plex-mono); }
  .accent { color: var(--accent); }
`;

const domain = new URL(SITE.url).host;

const og = `
  <style>
    body { width: 1200px; height: 630px; padding: 72px 80px; display: flex; flex-direction: column; justify-content: space-between; }
    .mark { font-weight: 700; font-style: italic; font-size: 44px; letter-spacing: -0.04em; }
    h1 { font-weight: 700; font-size: 84px; line-height: 1.04; letter-spacing: -0.03em; max-width: 15ch; }
    .cursor { display: inline-block; width: 0.55em; height: 0.85em; background: var(--accent); vertical-align: -0.08em; margin-left: 0.08em; }
    .foot { display: flex; justify-content: space-between; align-items: baseline; font-size: 28px; color: var(--dim); }
    .foot .mono { color: var(--text); }
  </style>
  <div class="mono mark">snob<span class="accent">.</span></div>
  <h1 class="mono">${HERO.title} <span class="accent">${HERO.accent}</span><span class="cursor"></span></h1>
  <div class="foot"><span class="mono"><span class="accent">$</span> ${HERO_INSTALL.macos.commands.at(-1)}</span><span>${domain}</span></div>
`;

/**
 * The "s." on a dark tile: legible on light and dark tabs alike.
 * @param {number} size
 */
const icon = (size) => `
  <style>
    body { width: ${size}px; height: ${size}px; display: grid; place-items: center; background: transparent; }
    .tile { width: ${size}px; height: ${size}px; border-radius: ${Math.round(size * 0.22)}px; background: var(--bg);
            display: grid; place-items: center; font-weight: 700; font-style: italic;
            font-size: ${Math.round(size * 0.6)}px; letter-spacing: -0.08em; line-height: 1; padding-bottom: ${Math.round(size * 0.08)}px; }
  </style>
  <div class="tile mono"><span>s<span class="accent">.</span></span></div>
`;

const targets = [
  { file: 'og.png', width: 1200, height: 630, body: og, transparent: false },
  { file: 'favicon-32.png', width: 32, height: 32, body: icon(32), transparent: true },
  { file: 'apple-touch-icon.png', width: 180, height: 180, body: icon(180), transparent: false },
];

const fonts = fontCss();
const browser = await chromium.launch({ headless: true });
try {
  for (const target of targets) {
    const page = await browser.newPage({ viewport: { width: target.width, height: target.height }, deviceScaleFactor: 1 });
    await page.setContent(`<!doctype html><html><head><meta charset="utf-8"><style>${fonts}${BASE}</style></head><body>${target.body}</body></html>`);
    await page.evaluate(() => document.fonts.ready);
    const destination = path.join(PUBLIC, target.file);
    // Written beside the target and renamed, so a failure never leaves half a file.
    const temporary = `${destination}.tmp-${process.pid}.png`;
    try {
      await page.screenshot({ path: temporary, omitBackground: target.transparent });
      await rename(temporary, destination);
    } catch (error) {
      await rm(temporary, { force: true });
      throw error;
    }
    console.log(`[brand] public/${target.file} (${target.width}×${target.height})`);
    await page.close();
  }
} finally {
  await browser.close();
}
