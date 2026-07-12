import { test, expect } from '../fixtures/test';

test.describe('Logout Flow', () => {
  test('clicking logout redirects to homepage', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/');

    // Click the logout link
    await authenticatedPage.getByRole('link', { name: 'Logout' }).click();

    // Should redirect to homepage
    await expect(authenticatedPage).toHaveURL('/');
  });

  test('after logout, homepage shows login link', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/');

    // Verify we're logged in first
    await expect(authenticatedPage.getByRole('link', { name: 'Logout' })).toBeVisible();

    // Click logout
    await authenticatedPage.getByRole('link', { name: 'Logout' }).click();
    await authenticatedPage.waitForURL('/');

    // Should now see the logged-out hero with its sign-in CTA instead
    await expect(authenticatedPage.locator('main').getByRole('link', { name: 'Sign in with GitHub' })).toBeVisible();
    await expect(authenticatedPage.getByRole('heading', { name: 'Your code is the controller.' })).toBeVisible();
  });

  test('protected routes return 401 after logout', async ({ authenticatedPage }) => {
    // First verify we can access /me while logged in
    await authenticatedPage.goto('/me');
    await expect(authenticatedPage.getByRole('heading', { name: 'My Profile' })).toBeVisible();

    // Now logout
    await authenticatedPage.goto('/');
    await authenticatedPage.getByRole('link', { name: 'Logout' }).click();
    await authenticatedPage.waitForURL('/');

    // Try to access /me again - should get 401
    const response = await authenticatedPage.goto('/me');
    expect(response?.status()).toBe(401);
  });
});
