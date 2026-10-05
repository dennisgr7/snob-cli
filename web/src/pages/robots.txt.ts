import type { APIRoute } from 'astro';

/**
 * Generated rather than kept in `public/` so the sitemap URL comes from
 * `site` and cannot drift from the domain.
 *
 * Search engines and the AI answer engines are allowed: being cited is the
 * point of a page about a tool. Training crawlers are allowed too; to opt out
 * of training without losing search or citations, turn their `Allow: /` into
 * `Disallow: /`.
 */
export const GET: APIRoute = ({ site }) => {
  const body = `# Classic search engines
User-agent: Googlebot
Allow: /

User-agent: bingbot
Allow: /

# AI search and answer engines (the citation channel)
User-agent: OAI-SearchBot
Allow: /

User-agent: ChatGPT-User
Allow: /

User-agent: Claude-SearchBot
Allow: /

User-agent: Claude-User
Allow: /

User-agent: PerplexityBot
Allow: /

User-agent: Perplexity-User
Allow: /

# Model training crawlers
User-agent: GPTBot
Allow: /

User-agent: ClaudeBot
Allow: /

User-agent: Google-Extended
Allow: /

# Default rule and content-use signal
User-agent: *
Content-Signal: search=yes, ai-input=yes, ai-train=yes
Allow: /

# Known to ignore the rules and crawl aggressively
User-agent: Bytespider
Disallow: /

Sitemap: ${new URL('/sitemap-index.xml', site)}
`;
  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
