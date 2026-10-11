import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Profile Page', () => {
  test('displays user information', async ({ authenticatedPage, mockUser }) => {
    await authenticatedPage.route('https://example.com/avatar.png', route => route.fulfill({
      contentType: 'image/png',
      body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M/wHwAF/gL+XbK3AAAAAElFTkSuQmCC', 'base64'),
    }));
    await authenticatedPage.goto('/me');

    // Should show profile heading
    await expect(authenticatedPage.getByRole('heading', { name: 'My Profile' })).toBeVisible();

    // Should show user's GitHub login
    await expect(authenticatedPage.getByRole('heading', { name: mockUser.login })).toBeVisible();

    // Should show avatar image
    const avatar = authenticatedPage.locator('.profile-head .user-avatar.avatar');
    await expect(avatar).toBeVisible();
    await expect.poll(() => avatar.locator('img').evaluate((image: HTMLImageElement) => image.naturalWidth)).toBeGreaterThan(0);

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
    // Authenticated home leads with a personalized welcome (redesign slice 4)
    await expect(authenticatedPage.getByRole('heading', { name: /Welcome,/ })).toBeVisible();
  });

  test('Logout link logs out the user', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/me');

    await authenticatedPage.getByRole('link', { name: 'Logout' }).click();

    // Should redirect to homepage
    await expect(authenticatedPage).toHaveURL('/');

    // Should show the logged-out hero sign-in CTA (scoped to main; nav has its own)
    await expect(authenticatedPage.locator('main').getByRole('link', { name: 'Sign in with GitHub' })).toBeVisible();
  });

  test('profile page requires authentication', async ({ page }) => {
    const response = await page.goto('/me');
    expect(response?.status()).toBe(401);
  });

  test('notification email controls fit a 375px viewport with pending and no-recipient states', async ({ authenticatedPage, mockUser }) => {
    await authenticatedPage.setViewportSize({ width: 375, height: 812 });
    await query('UPDATE users SET github_email = NULL WHERE github_login = $1', [mockUser.login]);
    await authenticatedPage.goto('/me');
    const input = authenticatedPage.getByRole('textbox', { name: 'Notification email' });
    await expect(input).toBeVisible();
    await expect(authenticatedPage.getByText('Add an email to get notifications')).toBeVisible();
    await input.fill('pending@example.com');
    await authenticatedPage.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(authenticatedPage.getByText(/pending confirmation/)).toBeVisible();
    await expect(authenticatedPage.getByText('Add an email to get notifications')).toBeVisible();
    for (const control of [input, authenticatedPage.getByRole('button', { name: 'Save', exact: true }), authenticatedPage.getByRole('button', { name: 'Resend' }), authenticatedPage.getByRole('button', { name: 'Clear' })]) {
      const box = await control.boundingBox();
      expect(box).not.toBeNull();
      expect(box!.height).toBeGreaterThanOrEqual(44);
      expect(box!.x).toBeGreaterThanOrEqual(0);
      expect(box!.x + box!.width).toBeLessThanOrEqual(375);
      const fontSize = await control.evaluate(element => parseFloat(getComputedStyle(element).fontSize));
      expect(fontSize).toBeGreaterThanOrEqual(16);
    }
    expect(await authenticatedPage.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(375);
    const notifications = authenticatedPage.locator('section.block').filter({ has: authenticatedPage.getByRole('heading', { name: 'Notifications' }) });
    expect(await notifications.evaluate(element => element.scrollWidth)).toBeLessThanOrEqual(await notifications.evaluate(element => element.clientWidth));
    await authenticatedPage.screenshot({ path: 'test-results/notification-email-mobile.png', fullPage: true });
  });
});
