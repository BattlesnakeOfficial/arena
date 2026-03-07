import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Public Game Viewing', () => {
  // Helper: create a game via the API using an authenticated page, return game_id
  async function createGameViaDb(): Promise<string> {
    // Insert a minimal game directly in the DB for testing view access
    const result = await query<{ game_id: string }>(
      `INSERT INTO games (board_size, game_type, status, created_at, updated_at)
       VALUES ('small', 'standard', 'finished', NOW(), NOW())
       RETURNING game_id::text AS game_id`
    );
    return result[0].game_id;
  }

  test('unauthenticated user can view a game page directly', async ({ page }) => {
    const gameId = await createGameViaDb();

    // Visit the game page without logging in
    const response = await page.goto(`/games/${gameId}`);

    // Should NOT get a 401 — the page should load successfully
    expect(response?.status()).toBe(200);

    // Should see the game details heading
    await expect(page.getByRole('heading', { name: 'Game Details' })).toBeVisible();

    // Should see the game ID on the page
    await expect(page.getByText(`Game ${gameId}`)).toBeVisible();
  });

  test('unauthenticated user sees public navigation on game page', async ({ page }) => {
    const gameId = await createGameViaDb();

    await page.goto(`/games/${gameId}`);

    // Should see "View Leaderboards" link (not "Create Another Game" which is for auth users)
    await expect(page.getByRole('link', { name: 'View Leaderboards' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to Home' })).toBeVisible();

    // Should NOT see authenticated-only navigation
    await expect(page.getByRole('link', { name: 'Create Another Game' })).not.toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to Profile' })).not.toBeVisible();
  });

  test('authenticated user sees authenticated navigation on game page', async ({ authenticatedPage }) => {
    const gameId = await createGameViaDb();

    await authenticatedPage.goto(`/games/${gameId}`);

    // Authenticated users should see "Create Another Game" and "Back to Profile"
    await expect(authenticatedPage.getByRole('link', { name: 'Create Another Game' })).toBeVisible();
    await expect(authenticatedPage.getByRole('link', { name: 'Back to Profile' })).toBeVisible();

    // Should NOT see public-only navigation
    await expect(authenticatedPage.getByRole('link', { name: 'View Leaderboards' })).not.toBeVisible();
    await expect(authenticatedPage.getByRole('link', { name: 'Back to Home' })).not.toBeVisible();
  });

  test('game page shows board viewer iframe for unauthenticated user', async ({ page }) => {
    const gameId = await createGameViaDb();

    await page.goto(`/games/${gameId}`);

    // The board viewer iframe should be present
    const iframe = page.locator('#board-viewer');
    await expect(iframe).toBeVisible();

    // Iframe src should contain the game ID
    const src = await iframe.getAttribute('src');
    expect(src).toContain(gameId);
  });
});

test.describe('Homepage Leaderboard Link for Unauthenticated Users', () => {
  test('homepage shows Leaderboards link when not logged in', async ({ page }) => {
    await page.goto('/');

    // Unauthenticated users should see a Leaderboards link/button
    await expect(page.getByRole('link', { name: 'Leaderboards' })).toBeVisible();
  });

  test('Leaderboards link on homepage navigates to leaderboards page', async ({ page }) => {
    await page.goto('/');

    // Click the leaderboards link
    await page.getByRole('link', { name: 'Leaderboards' }).click();

    // Should navigate to the leaderboards page
    await expect(page).toHaveURL('/leaderboards');
    await expect(page.getByRole('heading', { name: 'Leaderboards' })).toBeVisible();
  });
});
