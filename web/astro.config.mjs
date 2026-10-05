// @ts-check
import { defineConfig } from 'astro/config';

import tailwindcss from '@tailwindcss/vite';
import sitemap from '@astrojs/sitemap';

import { SITE } from './src/lib/site.ts';

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
      // The 404 page is `noindex`, so it stays out.
      filter: (page) => !new URL(page).pathname.startsWith('/404'),
    }),
  ],

  // GitHub Pages cannot set response headers, so the policy travels as a
  // `<meta http-equiv>` that Astro writes on every page, with the hashes of the
  // scripts and styles it bundles. A meta policy cannot carry `frame-ancestors`,
  // `report-uri` or `sandbox`; browsers ignore them there.
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
