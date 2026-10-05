import { test, expect } from '@playwright/test';
import { MISSING } from './routes';

test('the home page answers with its basic structure', async ({ page }) => {
  const response = await page.goto('/');
  expect(response?.status()).toBe(200);
  await expect(page.locator('html')).toHaveAttribute('lang', 'en');
  await expect(page.locator('h1')).toHaveCount(1);
  await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', /^index, follow/);
});

test('a missing page is the 404 page, with a 404 status', async ({ page }) => {
  const response = await page.goto(MISSING);
  expect(response?.status()).toBe(404);
  await expect(page.locator('h1')).toHaveText('Page not found');
  await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', 'noindex, follow');
});

test('the design system is not for search', async ({ page, request }) => {
  const response = await page.goto('/design-system/');
  expect(response?.status()).toBe(200);
  await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', 'noindex, follow');

  const sitemap = await (await request.get('/sitemap-0.xml')).text();
  expect(sitemap).not.toContain('/design-system');
  expect(sitemap).not.toContain('/404');
});
