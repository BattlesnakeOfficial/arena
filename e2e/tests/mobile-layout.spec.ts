import { test, expect } from '../fixtures/test';

const PHONE = { width: 375, height: 812 };
const PAGES = ['/', '/me', '/games/new', '/leaderboards', '/battlesnakes'];

test.describe('Mobile layout (375px)', () => {
  for (const path of PAGES) {
    test(`no horizontal overflow on ${path} when logged in`, async ({ authenticatedPage }) => {
      await authenticatedPage.setViewportSize(PHONE);
      await authenticatedPage.goto(path);
      const scrollWidth = await authenticatedPage.evaluate(() => document.documentElement.scrollWidth);
      expect(scrollWidth).toBeLessThanOrEqual(PHONE.width);
    });
  }

  test('no horizontal overflow on / when logged out', async ({ page }) => {
    await page.setViewportSize(PHONE);
    await page.goto('/');
    const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
    expect(scrollWidth).toBeLessThanOrEqual(PHONE.width);
  });
});
