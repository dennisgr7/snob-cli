// @ts-check
/**
 * The site's own Plex faces, for the pages the scripts here draw in Chromium
 * (brand.mjs, video.mjs): the @font-face rules and font variables Astro wrote
 * into the built page, each file inlined as a data URL, which loads where a
 * file:// font would not. Read after `pnpm build`.
 */
import { readFileSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';

const DIST = path.join(process.cwd(), 'dist');

/** The @font-face rules and font variables Astro wrote into the built page. */
export function fontCss() {
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
