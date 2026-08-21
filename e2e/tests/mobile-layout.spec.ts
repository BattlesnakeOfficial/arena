import type { Page } from '@playwright/test';
import { test, expect, createMockUser } from '../fixtures/test';

const PHONE = { width: 375, height: 812 };
const PAGES = ['/', '/me', '/games/new', '/leaderboards', '/battlesnakes'];

async function assertNoHorizontalOverflow(page: Page, path: string) {
  await page.setViewportSize(PHONE);
  await page.goto(path);
  // Measure in the real web font, not the fallback: the display face is wider.
  await page.evaluate(() => document.fonts.ready);
  const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
  expect(scrollWidth, `${path} overflows at ${PHONE.width}px`).toBeLessThanOrEqual(PHONE.width);
}

test.describe('Mobile layout (375px)', () => {
  for (const path of PAGES) {
    test(`no horizontal overflow on ${path} when logged in`, async ({ authenticatedPage }) => {
      await assertNoHorizontalOverflow(authenticatedPage, path);
    });
  }

  test('no horizontal overflow on / when logged out', async ({ page }) => {
    await assertNoHorizontalOverflow(page, '/');
  });

  test('home welcome heading wraps a 39-character GitHub login', async ({ page, loginAsUser }) => {
    // GitHub's maximum login length, with no break opportunities.
    const user = createMockUser('long');
    user.login = `l${Date.now().toString(36)}`.padEnd(39, 'x');
    await loginAsUser(page, user);
    await assertNoHorizontalOverflow(page, '/');
  });
});
