import { test, expect, type Page } from '@playwright/test';
import { HERO_INSTALL } from '@/lib/content';
import { detectPlatform } from '@/lib/platform';

/** User agents as each system's browser sends them, cut to what matters here. */
const AGENTS = {
  windows: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36',
  macos: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/19.0 Safari/605.1.15',
  linux: 'Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0',
  mobile: 'Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148',
} as const;

test.describe('platform detection', () => {
  test('reads the client hints first', () => {
    expect(detectPlatform({ platform: 'Windows', userAgent: AGENTS.macos })).toBe('windows');
    expect(detectPlatform({ platform: 'macOS', userAgent: '' })).toBe('macos');
    expect(detectPlatform({ platform: 'Linux', userAgent: '' })).toBe('linux');
    expect(detectPlatform({ platform: 'Chrome OS', userAgent: '' })).toBe('linux');
    expect(detectPlatform({ platform: 'Android', userAgent: '' })).toBe('mobile');
    expect(detectPlatform({ platform: 'Windows', mobile: true, userAgent: '' })).toBe('mobile');
  });

  test('falls back to the coarse name in the user agent', () => {
    for (const [platform, userAgent] of Object.entries(AGENTS)) {
      expect(detectPlatform({ userAgent }), platform).toBe(platform);
    }
  });

  test('an iPad asking for the desktop site is still a tablet', () => {
    expect(detectPlatform({ userAgent: AGENTS.macos, touchPoints: 5 })).toBe('mobile');
  });

  test('anything unknown gets the macOS command, as without JavaScript', () => {
    expect(detectPlatform({ userAgent: 'curl/8.0' })).toBe('macos');
  });
});

/** Makes the page see one system, with no client hints, as Firefox and Safari do. */
async function pretendToBe(page: Page, userAgent: string) {
  await page.addInitScript((agent) => {
    Object.defineProperty(Navigator.prototype, 'userAgent', { get: () => agent });
    Object.defineProperty(Navigator.prototype, 'userAgentData', { get: () => undefined });
    Object.defineProperty(Navigator.prototype, 'maxTouchPoints', { get: () => 0 });
  }, userAgent);
}

const heroVariant = (page: Page, platform: string) => page.locator(`[data-install-hero] [data-platform="${platform}"]`);

for (const platform of ['windows', 'macos', 'linux'] as const) {
  test(`the hero offers ${platform} its own command`, async ({ page }) => {
    await pretendToBe(page, AGENTS[platform]);
    await page.goto('/');
    const variant = heroVariant(page, platform);
    await expect(variant).toBeVisible();
    await expect(variant.locator('pre')).toContainText(HERO_INSTALL[platform].commands.at(-1)!);
    await expect(page.locator('[data-install-hero] [data-platform]:visible')).toHaveCount(1);
  });
}

test('a phone is told snob runs on a computer, and gets no command', async ({ page }) => {
  await pretendToBe(page, AGENTS.mobile);
  await page.goto('/');
  await expect(heroVariant(page, 'mobile')).toBeVisible();
  await expect(page.locator('[data-install-hero] [data-platform]:visible')).toHaveCount(1);
  await expect(page.getByRole('link', { name: 'other ways ↓' })).toBeVisible();
});

test('the install tabs open on the one for this system', async ({ page }) => {
  await pretendToBe(page, AGENTS.windows);
  await page.goto('/#install');
  await expect(page.getByRole('tab', { name: 'Windows' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('tabpanel')).toHaveCount(1);
  await expect(page.getByRole('tabpanel')).toContainText('scoop install snob');
});

test('the install tabs move with the arrow keys', async ({ page }) => {
  await pretendToBe(page, AGENTS.macos);
  await page.goto('/#install');
  const tabs = page.getByRole('tab');
  await tabs.first().focus();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.nth(1)).toBeFocused();
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('End');
  await expect(tabs.last()).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('ArrowRight');
  await expect(tabs.first()).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('tabpanel')).toHaveCount(1);
});

test('a copy button copies its command and says so', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await pretendToBe(page, AGENTS.macos);
  await page.goto('/');
  const button = heroVariant(page, 'macos').getByRole('button');
  await button.click();
  // Windows hands the clipboard back with CRLF line ends.
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  expect(copied.replaceAll('\r\n', '\n')).toBe(HERO_INSTALL.macos.commands.join('\n'));
  await expect(button.locator('[data-copy-done]')).toBeVisible();
  await expect(button.locator('[data-copy-idle]')).toBeHidden();
  await expect(page.locator('#announcer')).toHaveText('Copied');
});

test.describe('without JavaScript', () => {
  test.use({ javaScriptEnabled: false });

  test('the page still has everything', async ({ page }) => {
    await page.goto('/');
    await expect(heroVariant(page, 'macos')).toBeVisible();
    await expect(page.locator('[data-install-hero] [data-platform]:visible')).toHaveCount(1);
    // No tabs: every method one after another, under its heading.
    await expect(page.getByRole('tablist')).toBeHidden();
    await expect(page.locator('[data-panel]:visible')).toHaveCount(5);
    await expect(page.getByRole('heading', { name: 'From source' })).toBeVisible();
    // A copy button that could not copy is never shown.
    await expect(page.locator('button[data-copy]:visible')).toHaveCount(0);
  });
});

test.describe('motion', () => {
  test('the cursor blinks', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await page.goto('/');
    const name = await page.locator('h1 .cursor').evaluate((el) => getComputedStyle(el).animationName);
    expect(name).toBe('blink');
  });

  test('nothing moves under reduced motion', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.goto('/');
    const name = await page.locator('h1 .cursor').evaluate((el) => getComputedStyle(el).animationName);
    expect(name).toBe('none');
  });
});
