import { test, expect } from '../fixtures/test';

test.describe('Authentication via Mock OAuth', () => {
  test('authenticatedPage shows logged in user on homepage', async ({ authenticatedPage, mockUser }) => {
    // authenticatedPage starts at home page after OAuth redirect
    await authenticatedPage.goto('/');

    // Should show the welcome message; new users have no display name, so it
    // falls back to their GitHub login
    await expect(authenticatedPage.getByText(`Welcome, ${mockUser.login}!`)).toBeVisible();
  });

  test('authenticatedPage can access protected routes', async ({ authenticatedPage }) => {
    // Profile page requires authentication (at /me)
    await authenticatedPage.goto('/me');

    // Should load profile page instead of redirecting
    await expect(authenticatedPage).toHaveURL('/me');
    await expect(authenticatedPage.getByRole('heading', { name: 'My Profile' })).toBeVisible();
  });

  test('mockUser has expected properties', async ({ mockUser }) => {
    // MockUser should have the expected shape
    expect(mockUser.id).toBeGreaterThan(0);
    expect(mockUser.login).toMatch(/^testuser_\d+$/);
    expect(mockUser.name).toMatch(/^Test User \d+$/);
    expect(mockUser.email).toMatch(/^test\d+@example\.com$/);
  });
});
