import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Homepage - Authenticated User', () => {
  test('displays user info when logged in', async ({ authenticatedPage, mockUser }) => {
    await authenticatedPage.route('https://example.com/avatar.png', route => route.fulfill({
      contentType: 'image/png',
      body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M/wHwAF/gL+XbK3AAAAAElFTkSuQmCC', 'base64'),
    }));
    await authenticatedPage.goto('/');

    // User's GitHub login name is displayed
    await expect(authenticatedPage.getByText(`Welcome, ${mockUser.login}!`)).toBeVisible();

    // User's avatar is displayed (decorative img in the welcome band)
    const avatar = authenticatedPage.locator('.welcome-avatar');
    await expect(avatar).toBeVisible();
    await expect.poll(() => avatar.locator('img').evaluate((image: HTMLImageElement) => image.naturalWidth)).toBeGreaterThan(0);
  });

  test('shows navigation links for authenticated users', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/');

    // Profile link is visible
    await expect(authenticatedPage.getByRole('link', { name: 'Profile' })).toBeVisible();

    // Both snake directories are in the nav. Scoped to the nav (and exact) so
    // they don't match the homepage's own "My snakes" CTA.
    const nav = authenticatedPage.locator('nav.site-nav');
    await expect(nav.getByRole('link', { name: 'My Snakes', exact: true })).toBeVisible();
    await expect(nav.getByRole('link', { name: 'Snakes', exact: true })).toBeVisible();

    // Logout link is visible
    await expect(authenticatedPage.getByRole('link', { name: 'Logout' })).toBeVisible();
  });

  test('shows the public snakes link but not My Snakes when anonymous', async ({ page }) => {
    await page.goto('/');

    const nav = page.locator('nav.site-nav');
    await expect(nav.getByRole('link', { name: 'Snakes', exact: true })).toBeVisible();
    await expect(nav.getByRole('link', { name: 'My Snakes', exact: true })).toHaveCount(0);
  });

  test('does not show login link when authenticated', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/');

    // Sign-in link should NOT be visible anywhere for authenticated users
    await expect(authenticatedPage.getByRole('link', { name: 'Sign in with GitHub' })).not.toBeVisible();
  });
});

test.describe('Homepage - calm surfaces', () => {
  test('ticker strip is static and capped, live dot is solid', async ({ page, authenticatedPage }) => {
    const snakeName = `Home Ticker Snake ${Date.now()}`;

    // Seed enough activity (12 games) to overflow the 8-item feed cap on the
    // featured home leaderboard ("Standard 11x11" is the first active board).
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/home-ticker');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const [snake] = await query<{ battlesnake_id: string }>(
      'SELECT battlesnake_id FROM battlesnakes WHERE name = $1',
      [snakeName]
    );
    const [leaderboard] = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const [entry] = await query<{ leaderboard_entry_id: string }>(
      `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
       VALUES ($1, $2) RETURNING leaderboard_entry_id`,
      [leaderboard.leaderboard_id, snake.battlesnake_id]
    );
    for (let i = 0; i < 12; i++) {
      const [game] = await query<{ game_id: string }>(
        `INSERT INTO games (board_size, game_type, status)
         VALUES ('11x11', 'Standard', 'finished') RETURNING game_id`
      );
      const [lg] = await query<{ leaderboard_game_id: string }>(
        `INSERT INTO leaderboard_games (leaderboard_id, game_id)
         VALUES ($1, $2) RETURNING leaderboard_game_id`,
        [leaderboard.leaderboard_id, game.game_id]
      );
      await query(
        `INSERT INTO leaderboard_game_results
           (leaderboard_game_id, leaderboard_entry_id, placement,
            mu_before, mu_after, sigma_before, sigma_after, display_score_change)
         VALUES ($1, $2, 1, 25, 25.5, 8.333, 8.333, 1.5)`,
        [lg.leaderboard_game_id, entry.leaderboard_entry_id]
      );
    }

    // Anonymous visit: strip and rail render logged-out
    await page.goto('/');

    // Exactly one copy of the ticker content inside the strip
    const strip = page.locator('.strip .inner');
    await expect(strip).toHaveCount(1);

    // Feed is capped at 8: each event renders one .sep, so 12 seeded games
    // produce exactly 8. If the duplicated marquee is ever re-added, the
    // count doubles and this fails.
    expect(await strip.locator('.sep').count()).toBe(8);

    // No marquee animation on the strip
    const anim = await page.$eval('.strip .inner', (el) => getComputedStyle(el).animationName);
    expect(anim).toBe('none');

    // The live dot is still present but no longer pulses
    await expect(page.locator('.live-dot').first()).toBeVisible();
    const dotAnim = await page.$eval('.live-dot', (el) => getComputedStyle(el).animationName);
    expect(dotAnim).toBe('none');

    // Served stylesheet no longer carries the removed animations
    const css = await (await page.request.get('/static/arena.css')).text();
    expect(css).not.toContain('home-ticker');
    expect(css).not.toContain('@keyframes pulse');
  });
});
