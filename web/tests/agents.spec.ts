import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { FEATURES, INSTALL } from '@/lib/content';
import { SITE } from '@/lib/site';
import { plainLine } from '@/lib/terminal';

/**
 * What an agent reads: the Markdown twin, llms.txt, the JSON-LD, and the
 * tags a link preview uses. A person and an agent must read the same thing.
 */

const cargoVersion = (() => {
  const manifest = readFileSync(join(process.cwd(), '..', 'Cargo.toml'), 'utf8');
  return /\[workspace\.package\][^[]*?^version\s*=\s*"([^"]+)"/ms.exec(manifest)?.[1];
})();

test('the Markdown twin has every command and every install the page shows', async ({ request, page }) => {
  const response = await request.get('/index.md');
  expect(response.status()).toBe(200);
  expect(response.headers()['content-type']).toContain('text/markdown');
  const markdown = await response.text();

  await page.goto('/');
  const html = (await page.locator('main').textContent()) ?? '';

  for (const feature of FEATURES) {
    expect(markdown).toContain(feature.description);
    expect(html).toContain(feature.description);
    for (const line of feature.sample) {
      expect(markdown).toContain(plainLine(line));
    }
  }
  for (const method of INSTALL) {
    for (const block of method.blocks) {
      for (const command of block.commands) {
        expect(markdown).toContain(`$ ${command}`);
        expect(html).toContain(command);
      }
    }
  }
  expect(markdown).toContain(`HTML version: ${SITE.url}/`);
});

test('llms.txt points at the twin', async ({ request }) => {
  const response = await request.get('/llms.txt');
  expect(response.status()).toBe(200);
  const index = await response.text();
  expect(index).toMatch(new RegExp(`^# ${SITE.name}`));
  expect(index).toContain(`${SITE.url}/index.md`);
});

test('the head tells search, previews and agents what the page is', async ({ page }) => {
  await page.goto('/');
  // The words people search with before they know the name.
  await expect(page).toHaveTitle(new RegExp(SITE.category));
  await expect(page.locator('meta[name="description"]')).toHaveAttribute('content', new RegExp(SITE.category));
  await expect(page.locator('link[rel="canonical"]')).toHaveAttribute('href', `${SITE.url}/`);
  await expect(page.locator('link[rel="alternate"][type="text/markdown"]')).toHaveAttribute('href', `${SITE.url}/index.md`);
  await expect(page.locator('meta[property="og:image"]')).toHaveAttribute('content', `${SITE.url}/og.png`);
  await expect(page.locator('meta[name="twitter:card"]')).toHaveAttribute('content', 'summary_large_image');

  for (const icon of ['/og.png', '/favicon-32.png', '/apple-touch-icon.png']) {
    const response = await page.request.get(icon);
    expect(response.status(), icon).toBe(200);
    expect(response.headers()['content-type'], icon).toContain('image/png');
  }
});

test('the JSON-LD describes the tool at the released version', async ({ page }) => {
  await page.goto('/');
  const text = await page.locator('script[type="application/ld+json"]').textContent();
  expect(text).not.toContain('<');
  const graph = JSON.parse(String(text))['@graph'] as Record<string, unknown>[];
  const software = graph.find((node) => node['@type'] === 'SoftwareApplication');
  expect(software?.['softwareVersion']).toBe(cargoVersion);
  expect(software?.['downloadUrl']).toBe(`${SITE.repository}/releases/latest`);
  expect(graph.some((node) => node['@type'] === 'WebSite')).toBe(true);
});

test('robots.txt and the sitemap list the home page and nothing else', async ({ request }) => {
  const robots = await (await request.get('/robots.txt')).text();
  expect(robots).toContain(`Sitemap: ${SITE.url}/sitemap-index.xml`);
  const sitemap = await (await request.get('/sitemap-0.xml')).text();
  expect(sitemap.match(/<loc>/g)).toHaveLength(1);
  expect(sitemap).toContain(`<loc>${SITE.url}/</loc>`);
});
