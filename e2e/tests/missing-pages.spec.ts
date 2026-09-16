import { randomUUID } from 'node:crypto';
import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Missing pages', () => {
  test('public resource links return a navigable 404', async ({ page }) => {
    const id = randomUUID();
    for (const path of [
      `/users/missing-${id}`,
      `/users/missing/${id}`,
      `/games/${id}`,
      `/battlesnakes/${id}/profile`,
      '/battlesnakes/not-a-uuid/profile',
      `/leaderboards/${id}`,
      `/leaderboards/${id}/entries/${id}`,
      `/tournaments/${id}`,
    ]) {
      const response = await page.goto(path);
      expect(response?.status(), path).toBe(404);
      await expect(page.getByRole('heading', { name: '404 — Page not found' })).toBeVisible();
      await expect(page.getByRole('link', { name: 'Back to Home', exact: true })).toHaveAttribute('href', '/');
    }
    await page.getByRole('link', { name: 'Back to Home', exact: true }).click();
    await expect(page).toHaveURL('/');

    // The HTML recovery must not change the board viewer API's response type.
    const api = await page.request.get(`/api/games/${id}`);
    expect(api.status()).toBe(404);
    expect(api.headers()['content-type'] ?? '').not.toContain('text/html');
  });

  test('another player cannot distinguish a private builder from a missing one', async ({ authenticatedPage, loginAsUser }) => {
    await authenticatedPage.goto('/games/new');
    const otherPlayersFlow = new URL(authenticatedPage.url()).pathname;
    await loginAsUser(authenticatedPage, createMockUser('outsider'));

    const existing = await authenticatedPage.goto(otherPlayersFlow);
    expect(existing?.status()).toBe(404);
    const existingBody = await authenticatedPage.locator('main').innerText();
    const missing = await authenticatedPage.goto(`/games/flow/${randomUUID()}`);
    expect(missing?.status()).toBe(404);
    expect(await authenticatedPage.locator('main').innerText()).toBe(existingBody);
    await authenticatedPage.getByRole('link', { name: 'Start a new game', exact: true }).click();
    await expect(authenticatedPage.getByRole('heading', { name: 'Create New Game' })).toBeVisible();
    expect(new URL(authenticatedPage.url()).pathname).not.toBe(otherPlayersFlow);
  });

  test('hidden tournaments use the same page as missing tournaments', async ({ authenticatedPage, mockUser, page }) => {
    const [tournament] = await query<{ tournament_id: string }>(
      `INSERT INTO tournaments (user_id, name, visibility)
       SELECT user_id, 'Hidden tournament test', 'participants_only'
       FROM users WHERE github_login = $1 RETURNING tournament_id`,
      [mockUser.login],
    );
    // Anonymous page has no session belonging to the owner.
    const hidden = await page.goto(`/tournaments/${tournament.tournament_id}`);
    expect(hidden?.status()).toBe(404);
    const hiddenBody = await page.locator('main').innerText();
    const missing = await page.goto(`/tournaments/${randomUUID()}`);
    expect(missing?.status()).toBe(404);
    expect(await page.locator('main').innerText()).toBe(hiddenBody);
  });
});
