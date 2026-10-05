import type { APIRoute } from 'astro';
import { FEATURES, HERO } from '@/lib/content';
import { SITE } from '@/lib/site';

/**
 * The llmstxt.org index of the site. It is not a ranking lever; it is here so
 * an agent reads the same thing a person does. Every page added to the site
 * gets a line here, pointing at its Markdown twin.
 */
export const GET: APIRoute = ({ site }) => {
  const at = (path: string) => new URL(path, site).href;
  const commands = FEATURES.flatMap((feature) => feature.name.split(' · ')).map((name) => `snob ${name}`).join(', ');

  const body = `# ${SITE.name}

> ${SITE.description}

${HERO.lead} Commands: ${commands}.

## Pages

- [Home](${at('/index.md')}): what Snob CLI does, how it behaves, its output for scripts and agents, and how to install it.

## Source

- [Repository](${SITE.repository}): source code, releases and the README.
`;
  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
