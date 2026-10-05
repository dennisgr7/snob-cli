import type { APIRoute } from 'astro';
import { homeToMarkdown } from '@/lib/markdown';

/** The home page as Markdown: the same words as the HTML, from the same data. */
export const GET: APIRoute = ({ site }) =>
  new Response(homeToMarkdown(site ?? '/'), {
    headers: { 'Content-Type': 'text/markdown; charset=utf-8' },
  });
