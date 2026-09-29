import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

const PROMPT = { name: 'Played on play.battlesnake.com?' };

test.describe('Play account claim prompt on /me', () => {
  test('explains every claim path and stays dismissed', async ({ authenticatedPage: page }) => {
    await page.goto('/me');

    const prompt = page.getByRole('region', PROMPT);
    await expect(prompt).toBeVisible();
    await expect(prompt.getByRole('link', { name: 'Sign in with GitHub again' })).toHaveAttribute('href', '/auth/github');
    await expect(prompt.getByRole('link', { name: 'enter your old play login' })).toHaveAttribute('href', '/claim');
    await expect(prompt.getByRole('link', { name: 'Get a one-time claim link' })).toHaveAttribute('href', '/claim/email');

    await prompt.getByRole('button', { name: 'Dismiss' }).click();
    await expect(page).toHaveURL('/me');
    await expect(page.getByRole('heading', { name: 'My Profile' })).toBeVisible();
    await expect(page.getByRole('region', PROMPT)).toHaveCount(0);

    // Persisted server-side, not just hidden in this page view.
    await page.reload();
    await expect(page.getByRole('heading', { name: 'My Profile' })).toBeVisible();
    await expect(page.getByRole('region', PROMPT)).toHaveCount(0);
  });

  test('is hidden once a play account is claimed', async ({ authenticatedPage: page, mockUser }) => {
    const playId = `e2e_claimed_${mockUser.id}`;
    await query(
      `INSERT INTO imported_accounts
         (play_user_id, play_account_id, email, username, claimed_by_user_id, claimed_at)
       SELECT $1, $1, $2, $1, user_id, NOW() FROM users WHERE github_login = $3`,
      [playId, `${playId}@example.com`, mockUser.login],
    );
    try {
      await page.goto('/me');
      await expect(page.getByRole('heading', { name: 'My Profile' })).toBeVisible();
      await expect(page.getByRole('region', PROMPT)).toHaveCount(0);
    } finally {
      await query('DELETE FROM imported_accounts WHERE play_user_id = $1', [playId]);
    }
  });
});
