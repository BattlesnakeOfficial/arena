import { test, expect } from '../fixtures/test';

test.describe('Profile Page', () => {
  test('displays user information', async ({ authenticatedPage, mockUser }) => {
    await authenticatedPage.goto('/me');

    // Should show profile heading
    await expect(authenticatedPage.getByRole('heading', { name: 'My Profile' })).toBeVisible();

    // Should show user's GitHub login
    await expect(authenticatedPage.getByRole('heading', { name: mockUser.login })).toBeVisible();

    // Should show avatar image
    const avatar = authenticatedPage.locator('img[alt="Avatar"]');
    await expect(avatar).toBeVisible();

    // Should show Account Details section
    await expect(authenticatedPage.getByRole('heading', { name: 'Account Details' })).toBeVisible();
    await expect(authenticatedPage.getByText(/GitHub ID:/)).toBeVisible();
    await expect(authenticatedPage.getByText(/Account created:/)).toBeVisible();
    await expect(authenticatedPage.getByText(/Last updated:/)).toBeVisible();
  });

  test('shows user name and email when available', async ({ authenticatedPage, mockUser }) => {
    await authenticatedPage.goto('/me');

    // Mock user has name and email set
    await expect(authenticatedPage.getByText(mockUser.name)).toBeVisible();
    await expect(authenticatedPage.getByText(mockUser.email)).toBeVisible();
  });

  test('Manage Battlesnakes link navigates to battlesnakes list', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/me');

    await authenticatedPage.getByRole('link', { name: 'Manage Battlesnakes' }).click();

    await expect(authenticatedPage).toHaveURL('/battlesnakes');
    await expect(authenticatedPage.getByRole('heading', { name: 'Your Battlesnakes' })).toBeVisible();
  });

  test('Create New Game link navigates to game flow', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/me');

    await authenticatedPage.getByRole('link', { name: 'Create New Game' }).click();

    // Should redirect to a game flow page with UUID
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\/[0-9a-f-]+$/);
    await expect(authenticatedPage.getByRole('heading', { name: 'Create New Game' })).toBeVisible();
  });

  test('Back to Home link navigates to homepage', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/me');

    await authenticatedPage.getByRole('link', { name: 'Back to Home' }).click();

    await expect(authenticatedPage).toHaveURL('/');
    await expect(authenticatedPage.getByRole('heading', { name: 'Hello, world!' })).toBeVisible();
  });

  test('Logout link logs out the user', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/me');

    await authenticatedPage.getByRole('link', { name: 'Logout' }).click();

    // Should redirect to homepage
    await expect(authenticatedPage).toHaveURL('/');

    // Should show login link (logged out state)
    await expect(authenticatedPage.getByRole('link', { name: 'Login with GitHub' })).toBeVisible();
  });

  test('profile page requires authentication', async ({ page }) => {
    const response = await page.goto('/me');
    expect(response?.status()).toBe(401);
  });
});
