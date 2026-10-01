import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';
import { mkdirSync } from 'node:fs';

async function capture(page: import('@playwright/test').Page, name: string) {
  const directory = process.env.ARENA_CAPTURE_RANKING_SCREENSHOTS;
  if (!directory) return;
  mkdirSync(directory, { recursive: true });
  await page.screenshot({ path: `${directory}/${name}.png`, fullPage: true });
}

async function findRankingRow(page: import('@playwright/test').Page, profile: string) {
  await page.goto('/rankings');
  for (;;) {
    const row = page.locator('table.global-rankings tr').filter({ has: page.locator(`a[href="${profile}"]`) });
    if (await row.count()) return row;
    const next = page.getByRole('link', { name: 'Next ›', exact: true });
    if (!(await next.count())) throw new Error(`Player missing from rankings: ${profile}`);
    await next.click();
  }
}

test('rankings, profile and committed score changes are public', async ({ authenticatedPage, mockUser, page }) => {
  const [owner] = await query<{ user_id: string }>(
    'SELECT user_id FROM users WHERE github_login = $1', [mockUser.login],
  );
  const [board] = await query<{ leaderboard_id: string }>(
    'SELECT leaderboard_id FROM leaderboards WHERE disabled_at IS NULL ORDER BY name LIMIT 1',
  );
  const [snake] = await query<{ battlesnake_id: string }>(
    `INSERT INTO battlesnakes (user_id, name, url)
     VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id`,
    [owner.user_id, `rating-${mockUser.id}`],
  );
  try {
    const profile = `/users/${mockUser.login}/${owner.user_id}`;
    await page.goto(profile);
    await expect(page.locator('.profile-rating')).toContainText('Unranked');
    await capture(page, 'unranked-desktop');
    await page.setViewportSize({ width: 375, height: 812 });
    await capture(page, 'unranked-mobile');
    await page.setViewportSize({ width: 1280, height: 900 });

    await query(
      `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played, display_score)
       VALUES ($1, $2, 10, 15)`, [board.leaderboard_id, snake.battlesnake_id],
    );
    await page.goto(profile);
    const oldRating = await page.locator('.profile-rating strong').innerText();
    await expect(page.locator('.profile-rating')).toContainText('1 leaderboard');
    await capture(page, 'ranked-desktop');
    await page.setViewportSize({ width: 375, height: 812 });
    await capture(page, 'ranked-mobile');
    await page.setViewportSize({ width: 1280, height: 900 });

    let row = await findRankingRow(page, profile);
    await expect(page.getByRole('heading', { name: 'Global rankings' })).toBeVisible();
    const nav = page.locator('nav.site-nav a[href="/rankings"]');
    await expect(nav.first()).toBeVisible();
    await expect(row).toHaveCount(1);
    await expect(row.locator('td')).toHaveCount(4);
    await expect(row.locator('td.num')).toHaveText('1 board');
    await expect(row.locator('td.rating')).toHaveText(oldRating);
    await capture(page, 'rankings-desktop');

    await query(
      `UPDATE leaderboard_entries SET display_score = 20
       WHERE battlesnake_id = $1 AND leaderboard_id = $2`,
      [snake.battlesnake_id, board.leaderboard_id],
    );
    row = await findRankingRow(page, profile);
    const newRating = await row.locator('td.rating').innerText();
    expect(newRating).not.toBe(oldRating);
    await page.goto(profile);
    await expect(page.locator('.profile-rating strong')).toHaveText(newRating);

    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto('/rankings');
    await page.evaluate(() => document.fonts.ready);
    const widths = await page.evaluate(() => {
      const table = document.querySelector('table.global-rankings');
      return {
        document: document.documentElement.scrollWidth,
        table: table?.scrollWidth ?? 0,
        client: table?.clientWidth ?? 0,
        cells: Array.from(table?.querySelectorAll('tr:first-child th, tbody tr:first-child td') ?? [])
          .map((cell) => ({ text: cell.textContent?.trim(), width: cell.clientWidth, scroll: cell.scrollWidth })),
      };
    });
    await capture(page, 'rankings-mobile');
    expect(widths.document).toBeLessThanOrEqual(375);
    expect(widths.table, JSON.stringify(widths)).toBeLessThanOrEqual(widths.client);
    await expect(row.locator('td')).toHaveCount(4);
  } finally {
    await query('DELETE FROM battlesnakes WHERE battlesnake_id = $1', [snake.battlesnake_id]);
  }
});

test('rankings pages past fifty rows', async ({ page }) => {
  const prefix = `global_rank_${Date.now()}_${Math.floor(Math.random() * 10000)}`;
  const [board] = await query<{ leaderboard_id: string }>(
    'SELECT leaderboard_id FROM leaderboards WHERE disabled_at IS NULL ORDER BY name LIMIT 1',
  );
  try {
    await query(
      `WITH inserted_users AS (
         INSERT INTO users (external_github_id, github_login, github_access_token)
         SELECT 800000000000000 + $1::bigint * 100 + n, $2 || '_' || n, 'test'
         FROM generate_series(1, 55) n RETURNING user_id, github_login
       ), inserted_snakes AS (
         INSERT INTO battlesnakes (user_id, name, url)
         SELECT user_id, github_login, 'https://example.com' FROM inserted_users
         RETURNING battlesnake_id
       )
       INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played, display_score)
       SELECT $3, battlesnake_id, 10, 15 FROM inserted_snakes`,
      [Date.now(), prefix, board.leaderboard_id],
    );
    await page.goto('/rankings');
    let found = 0;
    for (;;) {
      found += await page.locator(`table.global-rankings a.name[href*="${prefix}"]`).count();
      const next = page.getByRole('link', { name: 'Next ›', exact: true });
      if (await next.count() === 0) break;
      await next.click();
    }
    expect(found).toBe(55);
  } finally {
    await query('DELETE FROM users WHERE github_login LIKE $1', [`${prefix}%`]);
  }
});
