import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { ALL_ROUTES } from './routes';

/**
 * The page's policy is the `<meta>` Astro writes with a hash for everything the
 * page runs; anything it misses is blocked in silence. These load every route in
 * both themes and fail on any violation or console error.
 */

declare global {
  interface Window {
    __cspViolations?: string[];
  }
}

const HEADERS = readFileSync(join(process.cwd(), 'public', '_headers'), 'utf8');

for (const colorScheme of ['light', 'dark'] as const) {
  for (const route of ALL_ROUTES) {
    test(`${route} in ${colorScheme} breaks no policy`, async ({ page, baseURL }) => {
      const origin = new URL(String(baseURL)).origin;
      const consoleErrors: string[] = [];
      page.on('console', (message) => {
        if (message.type() !== 'error' || !message.location().url.startsWith(origin)) return;
        // The 404 document logs its own status as a failed load.
        if (message.text().startsWith('Failed to load resource') && message.location().url === page.url()) {
          return;
        }
        consoleErrors.push(message.text());
      });
      await page.addInitScript(() => {
        window.__cspViolations = [];
        document.addEventListener('securitypolicyviolation', (event) => {
          window.__cspViolations?.push(
            `${event.violatedDirective} blocks ${event.blockedURI || 'inline'} at ${event.sourceFile}:${event.lineNumber}`,
          );
        });
      });
      await page.emulateMedia({ colorScheme });

      await page.goto(route);
      await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
      await page.waitForLoadState('networkidle');

      expect(await page.evaluate(() => window.__cspViolations ?? [])).toEqual([]);
      expect(consoleErrors).toEqual([]);
    });
  }
}

test('the page carries a policy and the headers from _headers', async ({ page }) => {
  const response = await page.goto('/');
  await expect(page.locator('meta[http-equiv="content-security-policy"]')).toHaveCount(1);
  const headers = response?.headers() ?? {};
  for (const name of [
    'strict-transport-security',
    'x-content-type-options',
    'x-frame-options',
    'referrer-policy',
    'permissions-policy',
    'content-security-policy',
  ]) {
    expect(HEADERS.toLowerCase()).toContain(`${name}:`);
    expect(headers[name], name).toBeTruthy();
  }
  expect(headers['content-security-policy']).toContain("frame-ancestors 'none'");
});
