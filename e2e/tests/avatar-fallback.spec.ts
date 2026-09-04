import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

async function expectFallback(page: import('@playwright/test').Page, selector: string, initial: string) {
  const avatar = page.locator(selector);
  await expect(avatar).toBeVisible();
  const fallback = avatar.locator('.avatar-fallback');
  await expect(fallback).toBeVisible();
  await expect(fallback).toHaveText(initial);
  await expect(avatar.locator('img')).toHaveCount(0);
}

test('failed avatars fall back on every user-avatar surface', async ({ authenticatedPage, mockUser }) => {
  const testId = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  const snakeName = `Avatar Fallback Snake ${testId}`;
  const leaderboardName = `Avatar Fallback Leaderboard ${testId}`;
  let leaderboardId: string | undefined;
  let entryId: string | undefined;

  await authenticatedPage.route('https://example.com/avatar.png', route => route.abort());

  try {
    await authenticatedPage.goto('/');
    await expectFallback(authenticatedPage, '.nav-avatar', 'T');
    await expectFallback(authenticatedPage, '.welcome-avatar', 'T');

    await authenticatedPage.goto('/me');
    await expectFallback(authenticatedPage, '.nav-avatar', 'T');
    await expectFallback(authenticatedPage, '.profile-head .avatar', 'T');

    await authenticatedPage.goto(`/users/${mockUser.login}`);
    await expectFallback(authenticatedPage, '.profile-head .avatar', 'T');

    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/avatar-fallback-snake');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const [snake] = await query<{ battlesnake_id: string }>(
      'SELECT battlesnake_id FROM battlesnakes WHERE name = $1',
      [snakeName],
    );
    await authenticatedPage.goto(`/battlesnakes/${snake.battlesnake_id}/profile`);
    await expectFallback(authenticatedPage, '.owner-avatar', 'T');
    await expect(authenticatedPage.locator('img[alt="Owner avatar"]')).toHaveCount(0);
    await expect(authenticatedPage.locator('.owner-avatar img:not([alt=""])')).toHaveCount(0);
    await expect(authenticatedPage.getByText('Owner avatar', { exact: true })).toHaveCount(0);

    const [leaderboard] = await query<{ leaderboard_id: string }>(
      'INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id',
      [leaderboardName],
    );
    leaderboardId = leaderboard.leaderboard_id;
    const [entry] = await query<{ leaderboard_entry_id: string }>(
      `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
       VALUES ($1, $2) RETURNING leaderboard_entry_id`,
      [leaderboard.leaderboard_id, snake.battlesnake_id],
    );
    entryId = entry.leaderboard_entry_id;

    await authenticatedPage.goto(
      `/leaderboards/${leaderboard.leaderboard_id}/entries/${entry.leaderboard_entry_id}`,
    );
    await expectFallback(authenticatedPage, '.entry-avatar', 'T');
  } finally {
    if (entryId) {
      await query('DELETE FROM leaderboard_entries WHERE leaderboard_entry_id = $1', [entryId]);
    }
    if (leaderboardId) {
      await query('DELETE FROM leaderboards WHERE leaderboard_id = $1', [leaderboardId]);
    }
  }
});

test('missing avatar URLs render fallbacks without image requests', async ({ authenticatedPage, mockUser }) => {
  try {
    await query('UPDATE users SET github_avatar_url = NULL WHERE github_login = $1', [mockUser.login]);

    await authenticatedPage.goto('/');
    await expectFallback(authenticatedPage, '.nav-avatar', 'T');
    await expectFallback(authenticatedPage, '.welcome-avatar', 'T');

    await authenticatedPage.goto('/me');
    await expectFallback(authenticatedPage, '.nav-avatar', 'T');
    await expectFallback(authenticatedPage, '.profile-head .avatar', 'T');
  } finally {
    await query(
      'UPDATE users SET github_avatar_url = $1 WHERE github_login = $2',
      ['https://example.com/avatar.png', mockUser.login],
    );
  }
});

test('mobile nav keeps the personalized accessible name', async ({ authenticatedPage, mockUser }) => {
  await authenticatedPage.setViewportSize({ width: 375, height: 812 });
  await authenticatedPage.goto('/');

  await expect(authenticatedPage.locator('.nav-avatar')).toBeVisible();
  const name = authenticatedPage.locator('.nav-user-name');
  await expect(name).toHaveCSS('position', 'absolute');
  await expect(name).toHaveCSS('width', '1px');
  await expect(name).toHaveCSS('height', '1px');
  await expect(name).not.toHaveCSS('display', 'none');
  expect(await name.evaluate(element => element.getBoundingClientRect().width)).toBeLessThanOrEqual(1);
  await expect(
    authenticatedPage.getByRole('link', { name: mockUser.login, exact: true }),
  ).toHaveAttribute('href', '/me');
});
