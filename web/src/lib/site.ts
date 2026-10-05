/**
 * The one source of the site's identity. The canonical URL, the sitemap, the
 * Open Graph tags, the JSON-LD, robots.txt and llms.txt all read it from here.
 */
export const SITE = {
  // TODO: the custom domain GitHub Pages will serve. It must be the root of a
  // host (no path), or robots.txt and llms.txt are never found by crawlers.
  url: 'https://snob.example.com',
  name: 'Snob CLI',
  description:
    'An Instagram client for the terminal, with extra utilities, media downloads and output ready for AI agents.',
  repository: 'https://github.com/dennisgr7/snob-cli',
  license: 'https://github.com/dennisgr7/snob-cli/blob/main/LICENSE',
  locale: 'en',
  ogLocale: 'en_US',
} as const;
