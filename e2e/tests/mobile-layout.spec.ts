import type { Page } from '@playwright/test';
import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

const PHONE = { width: 375, height: 812 };
const PAGES = ['/', '/me', '/games/new', '/leaderboards', '/battlesnakes', '/players?q=snake', '/snakes?q=snake'];

async function assertNoHorizontalOverflow(page: Page, path: string) {
  await page.setViewportSize(PHONE);
  await page.goto(path);
  // Measure in the real web font, not the fallback: the display face is wider.
  await page.evaluate(() => document.fonts.ready);
  const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
  expect(scrollWidth, `${path} overflows at ${PHONE.width}px`).toBeLessThanOrEqual(PHONE.width);
}

test.describe('Mobile layout (375px)', () => {
  test('snake editing and game building have readable inputs and tappable controls', async ({ authenticatedPage, mockUser }) => {
    const [snake] = await query<{ battlesnake_id: string }>(
      `INSERT INTO battlesnakes (user_id, name, url)
       SELECT user_id, 'Mobile target test', 'https://example.com/mobile'
       FROM users WHERE github_login = $1 RETURNING battlesnake_id`,
      [mockUser.login],
    );
    await authenticatedPage.setViewportSize(PHONE);
    for (const path of [`/battlesnakes/${snake.battlesnake_id}/edit`, '/games/new', '/players?q=snake', '/snakes?q=snake']) {
      await assertNoHorizontalOverflow(authenticatedPage, path);
      const inputs = authenticatedPage.locator('main input:not([type=checkbox]):not([type=hidden]), main select, main textarea');
      for (const input of await inputs.all()) {
        const size = await input.evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
        expect(size, `${path} input text`).toBeGreaterThanOrEqual(16);
      }
    }
    await authenticatedPage.goto('/games/new');
    await authenticatedPage.getByRole('button', { name: 'Add to Game' }).click();
    // Includes both row actions, the small lineup X, Search, Reset, and Create Game.
    for (const button of await authenticatedPage.locator('main button').all()) {
      const box = await button.boundingBox();
      expect(box?.height, await button.innerText()).toBeGreaterThanOrEqual(44);
      expect(box?.width, await button.innerText()).toBeGreaterThanOrEqual(44);
    }
    await authenticatedPage.locator('.gc-x').click();
    await expect(authenticatedPage.locator('.gc-x')).toHaveCount(0);
    expect(await authenticatedPage.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(PHONE.width);
  });

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
