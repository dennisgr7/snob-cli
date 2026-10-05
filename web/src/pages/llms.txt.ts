import type { APIRoute } from 'astro';
import { SITE } from '@/lib/site';

/**
 * The llmstxt.org index of the site. It is not a ranking lever; it is here so
 * an agent reads the same thing a person does. Every page added to the site
 * gets a line here.
 */
export const GET: APIRoute = ({ site }) => {
  const body = `# ${SITE.name}

> ${SITE.description}

## Pages

- [Home](${new URL('/', site)}): what Snob CLI is and how to install it.

## Source

- [Repository](${SITE.repository}): source code, releases and the README.
`;
  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
