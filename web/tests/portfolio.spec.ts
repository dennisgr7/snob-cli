import { test, expect, type Page } from '@playwright/test';

const PORTFOLIO = 'https://dennisgr7.dev';
const SITE = 'https://snob.dennisgr7.dev';
const LIGHT_BG = 'rgb(250, 250, 250)';
const DARK_BG = 'rgb(11, 13, 16)';

/**
 * The portfolio's live preview, played out on the build: the portfolio is a
 * stub page that frames the site, and the site's own origin is answered by the
 * local server, headers included, so `frame-ancestors` and the referrer check
 * see what they see in production.
 */
async function frameFromThePortfolio(page: Page, baseURL: string, theme: string) {
  await page.route(`${PORTFOLIO}/`, (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: `<iframe src="${SITE}/?preview-theme=${theme}" width="1600" height="1000"></iframe>`,
    }),
  );
  await page.route(`${SITE}/**`, async (route) => {
    const url = new URL(route.request().url());
    const response = await route.fetch({ url: `${baseURL}${url.pathname}${url.search}` });
    await route.fulfill({ response });
  });
  await page.goto(`${PORTFOLIO}/`);
  return page.frameLocator('iframe').locator('html');
}

const background = (html: ReturnType<Page['locator']>) =>
  html.evaluate((node) => getComputedStyle(node).backgroundColor);

test('the portfolio preview paints in the theme the portfolio shows', async ({ page, baseURL }) => {
  await page.emulateMedia({ colorScheme: 'light' });
  const html = await frameFromThePortfolio(page, baseURL!, 'dark');
  await expect(html).toHaveAttribute('data-theme', 'dark');
  await expect.poll(() => background(html)).toBe(DARK_BG);
});

test('the portfolio preview follows the portfolio when its theme changes', async ({ page, baseURL }) => {
  await page.emulateMedia({ colorScheme: 'dark' });
  const html = await frameFromThePortfolio(page, baseURL!, 'dark');
  await expect.poll(() => background(html)).toBe(DARK_BG);

  const send = (theme: string) =>
    page.evaluate(
      ([theme, site]) =>
        document
          .querySelector('iframe')!
          .contentWindow!.postMessage({ type: 'chesey:preview-theme', theme }, site!),
      [theme, SITE],
    );
  await send('light');
  await expect.poll(() => background(html)).toBe(LIGHT_BG);
  await send('purple');
  await expect(html).toHaveAttribute('data-theme', 'light');
});

test('on its own tab the page ignores the theme in the URL', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'light' });
  await page.goto('/?preview-theme=dark');
  await expect(page.locator('html')).not.toHaveAttribute('data-theme');
  expect(await background(page.locator('html'))).toBe(LIGHT_BG);
});
