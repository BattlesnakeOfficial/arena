import type { Page } from '@playwright/test';
import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

const PHONE = { width: 375, height: 812 };
const PAGES = ['/', '/me', '/games/new', '/leaderboards', '/battlesnakes', '/players?q=snake', '/rankings', '/snakes?q=snake'];

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

  test('owner tables fit a phone with a 39-character login', async ({ page, loginAsUser }, testInfo) => {
    testInfo.setTimeout(60_000);
    const user = createMockUser('owner');
    user.login = `o${Date.now().toString(36)}`.padEnd(39, 'x');
    await loginAsUser(page, user);
    const [owner] = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [user.login]);
    const snakeIds: string[] = [];
    for (const [name, url] of [
      ['Phone table snake', 'http://127.0.0.1:9/unreachable'],
      ['Phone table snake two', `https://${'very-long-hostname-'.repeat(4)}example.com/battlesnake`],
    ]) {
      const [snake] = await query<{ battlesnake_id: string }>(
        `INSERT INTO battlesnakes (user_id, name, url, visibility) VALUES ($1, $2, $3, 'public') RETURNING battlesnake_id`,
        [owner.user_id, name, url],
      );
      snakeIds.push(snake.battlesnake_id);
    }
    const [leaderboard] = await query<{ leaderboard_id: string }>(
      'INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id',
      [`Phone Tables LB ${testInfo.workerIndex} ${Date.now()}`],
    );
    try {
      await query(
        `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played)
         VALUES ($1, $2, 3), ($1, $3, 1)`,
        [leaderboard.leaderboard_id, ...snakeIds],
      );
      const [tournament] = await query<{ tournament_id: string }>(
        `INSERT INTO tournaments (name, user_id, status) VALUES ($1, $2, 'registration') RETURNING tournament_id`,
        [`Phone Tables Cup ${Date.now()}`, owner.user_id],
      );
      await query(
        `INSERT INTO tournament_registrations (tournament_id, battlesnake_id, user_id, seed)
         VALUES ($1, $2, $4, 1), ($1, $3, $4, 2)`,
        [tournament.tournament_id, ...snakeIds, owner.user_id],
      );

      // In Placement lists the long login under each snake.
      await assertNoHorizontalOverflow(page, `/leaderboards/${leaderboard.leaderboard_id}`);
      await expect(page.getByRole('heading', { name: 'In Placement' })).toBeVisible();

      // Seed moves scroll with the table, and the seed box is big enough not to trigger iOS zoom.
      await assertNoHorizontalOverflow(page, `/tournaments/${tournament.tournament_id}`);
      const seed = page.getByLabel('New seed').first();
      expect(await seed.evaluate((el) => parseFloat(getComputedStyle(el).fontSize))).toBeGreaterThanOrEqual(16);
      expect((await seed.boundingBox())?.height).toBeGreaterThanOrEqual(44);

      // The URL rides under the name, so the row actions stay on screen.
      await assertNoHorizontalOverflow(page, '/battlesnakes');
      const actions = await page.locator('table.my-snakes .row-actions .btn').all();
      expect(actions.length).toBeGreaterThan(0);
      for (const action of actions) {
        const box = await action.boundingBox();
        expect((box?.x ?? Infinity) + (box?.width ?? 0), await action.innerText()).toBeLessThanOrEqual(PHONE.width);
      }

      // Test results give each call's details a full-width row instead of a squeezed column.
      await page.locator(`form[action="/battlesnakes/${snakeIds[0]}/test"] button`).click();
      await expect(page.getByRole('button', { name: 'Run Test Again' })).toBeVisible({ timeout: 30_000 });
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(PHONE.width);
      const details = await page.locator('table.test-results tr.call-details td').first().boundingBox();
      expect((details?.x ?? Infinity) + (details?.width ?? 0)).toBeLessThanOrEqual(PHONE.width);
    } finally {
      await query('DELETE FROM leaderboards WHERE leaderboard_id = $1', [leaderboard.leaderboard_id]);
      await query('DELETE FROM tournaments WHERE user_id = $1', [owner.user_id]);
      await query('DELETE FROM battlesnakes WHERE user_id = $1', [owner.user_id]);
    }
  });

  test('leaderboard table keeps every column and scrolls sideways', async ({ authenticatedPage, mockUser }, testInfo) => {
    // A dedicated board: other specs expect the seeded boards to have no ranked snakes.
    const [leaderboard] = await query<{ leaderboard_id: string }>(
      'INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id',
      [`Mobile Table LB ${testInfo.workerIndex} ${Date.now()}`],
    );
    try {
      const [snake] = await query<{ battlesnake_id: string }>(
        `INSERT INTO battlesnakes (user_id, name, url)
         SELECT user_id, 'Mobile table snake', 'https://example.com/mobile-table'
         FROM users WHERE github_login = $1 RETURNING battlesnake_id`,
        [mockUser.login],
      );
      await query(
        `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played, first_place_finishes, display_score)
         VALUES ($1, $2, 12, 3, 30)`,
        [leaderboard.leaderboard_id, snake.battlesnake_id],
      );

      await assertNoHorizontalOverflow(authenticatedPage, `/leaderboards/${leaderboard.leaderboard_id}`);
      const scroller = authenticatedPage.locator('.table-scroll').first();
      const headers = scroller.locator('thead th');
      await expect(headers.last()).toHaveText('Games');
      for (const th of await headers.all()) {
        await expect(th, 'no column is dropped on a phone').toBeVisible();
      }

      const lastHeaderOverhang = () => scroller.evaluate((el) => {
        const th = el.querySelector('thead th:last-child');
        return (th?.getBoundingClientRect().right ?? 0) - el.getBoundingClientRect().right;
      });
      expect(await scroller.evaluate((el) => el.scrollWidth > el.clientWidth), 'table overflows its scroller').toBe(true);
      expect(await lastHeaderOverhang()).toBeGreaterThan(0);
      await scroller.evaluate((el) => { el.scrollLeft = el.scrollWidth; });
      // Scroll offsets are whole pixels but layout isn't, so a fully scrolled
      // table can still overhang by a fraction of a pixel.
      expect(await lastHeaderOverhang()).toBeLessThan(1);
    } finally {
      await query('DELETE FROM leaderboards WHERE leaderboard_id = $1', [leaderboard.leaderboard_id]);
      await query("DELETE FROM battlesnakes WHERE name = 'Mobile table snake' AND url = 'https://example.com/mobile-table'");
    }
  });
});
