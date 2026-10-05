// @ts-check
import { defineConfig, fontProviders } from 'astro/config';

import tailwindcss from '@tailwindcss/vite';
import sitemap from '@astrojs/sitemap';

import { SITE } from './src/lib/site.ts';

/** Pages that exist but are not for search: the 404 and the design system. */
const OUT_OF_SITEMAP = ['/404', '/design-system'];

// https://astro.build/config
export default defineConfig({
  // Feeds the canonical URL, the sitemap, Open Graph, JSON-LD and the absolute
  // URLs of robots.txt and llms.txt. A custom domain, so no `base`.
  site: SITE.url,

  // Pure SSG: all the content is in the first HTML (best for SEO, GEO and speed).
  output: 'static',

  // Opt-in prefetch: only the links marked for it, on hover.
  prefetch: {
    prefetchAll: false,
    defaultStrategy: 'hover',
  },

  integrations: [
    sitemap({
      // No `lastmod`: the build date on every URL says nothing about real
      // changes, and Google ignores dates that do not reflect one.
      // Both are `noindex`, so neither belongs here.
      filter: (page) => !OUT_OF_SITEMAP.some((path) => new URL(page).pathname.startsWith(path)),
    }),
  ],

  // Self-hosted: Astro downloads the files at build time and serves them from
  // the site, so no request ever goes to Google and the policy stays
  // `font-src 'self'`. DESIGN.md says why Plex.
  fonts: [
    {
      provider: fontProviders.google(),
      name: 'IBM Plex Mono',
      cssVariable: '--font-plex-mono',
      weights: [400, 700],
      // The italic is the mark's (Wordmark.astro); a face is only fetched
      // when the page uses it.
      styles: ['normal', 'italic'],
      subsets: ['latin'],
      display: 'swap',
      fallbacks: ['ui-monospace', 'monospace'],
    },
    {
      provider: fontProviders.google(),
      name: 'IBM Plex Sans',
      cssVariable: '--font-plex-sans',
      weights: [400, 600],
      styles: ['normal'],
      subsets: ['latin'],
      display: 'swap',
      fallbacks: ['ui-sans-serif', 'system-ui', 'sans-serif'],
    },
  ],

  // The policy travels as a `<meta http-equiv>` Astro writes on every page, with
  // the hashes of the scripts and styles that page runs, so nothing inline is
  // trusted wholesale. What a meta policy cannot carry (`frame-ancestors`) and
  // the other headers are in `public/_headers`.
  security: {
    csp: {
      algorithm: 'SHA-256',
      directives: [
        "default-src 'self'",
        "base-uri 'self'",
        "form-action 'self'",
        "object-src 'none'",
        "img-src 'self' data:",
        "font-src 'self'",
        "connect-src 'self'",
      ],
    },
  },

  // Shiki highlights with inline styles, which the policy above refuses. When the
  // site shows code, switch this to 'prism' (classes, not inline styles).
  markdown: {
    syntaxHighlight: false,
  },

  build: {
    // CSS always inlined: no render-blocking requests (better LCP).
    inlineStylesheets: 'always',
  },

  vite: {
    // Tailwind v4 runs as a Vite plugin (do not reintroduce @astrojs/tailwind).
    plugins: [tailwindcss()],
  },
});
