import { defineConfig, devices } from '@playwright/test';

const port = process.env.PLAYWRIGHT_PORT ?? '4321';
const baseURL = `http://127.0.0.1:${port}`;

/**
 * The tests read the production build: run `pnpm build` first. Wrangler serves
 * `dist/` the way Cloudflare does, with `public/_headers` and the real 404
 * status, which `astro preview` would not.
 *
 * Its own inspector port and state directory, so a `pnpm preview` left
 * running beside it cannot take either and keep this one from starting.
 */
export default defineConfig({
  testDir: './tests',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL,
    trace: 'on-first-retry',
  },
  webServer: {
    command: `pnpm exec wrangler dev --port ${port} --ip 127.0.0.1 --inspector-port 0 --persist-to .wrangler/test-state --show-interactive-dev-session=false`,
    url: baseURL,
    reuseExistingServer: !process.env.CI,
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
