/**
 * The one source of the site's identity. The canonical URL, the sitemap, the
 * Open Graph tags, the JSON-LD, robots.txt and llms.txt all read it from here.
 */
export const SITE = {
  // The root of a host, never a path, or crawlers never find robots.txt and
  // llms.txt. `wrangler.jsonc` routes the same host.
  url: 'https://snob.dennisgr7.dev',
  name: 'Snob CLI',
  description:
    'An Instagram client for the terminal, with extra utilities, media downloads and output ready for AI agents.',
  repository: 'https://github.com/dennisgr7/snob-cli',
  license: 'https://github.com/dennisgr7/snob-cli/blob/main/LICENSE',
  locale: 'en',
  ogLocale: 'en_US',
  /** The footer signature, the one mention of who made it. */
  author: { name: 'Dennis Industries', url: 'https://dennisindustries.net' },
  /** The browser bar, per theme: the page background of each. */
  themeColor: { light: '#fafafa', dark: '#0b0d10' },
} as const;
