import { test, expect } from '../fixtures/test';

test.describe('Smoke Tests', () => {
  test('homepage loads successfully', async ({ page }) => {
    await page.goto('/');

    // Verify the page loads with the hero content
    await expect(page.getByRole('heading', { name: 'Your code is the controller.' })).toBeVisible();
    await expect(page.getByText('Write a web server that plays snake.')).toBeVisible();
  });

  test('shows login link for unauthenticated users', async ({ page }) => {
    await page.goto('/');

    // Hero sign-in CTA (scoped to main; the global nav has its own sign-in button)
    await expect(page.locator('main').getByRole('link', { name: 'Sign in with GitHub' })).toBeVisible();
  });
});
