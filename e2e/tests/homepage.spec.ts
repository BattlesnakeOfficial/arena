import { test, expect } from '../fixtures/test';

test.describe('Homepage - Authenticated User', () => {
  test('displays user info when logged in', async ({ authenticatedPage, mockUser }) => {
    await authenticatedPage.goto('/');

    // User's GitHub login name is displayed
    await expect(authenticatedPage.getByText(`Welcome, ${mockUser.login}!`)).toBeVisible();

    // User's avatar is displayed (decorative img in the welcome band)
    const avatar = authenticatedPage.locator('.welcome img');
    await expect(avatar).toBeVisible();
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
