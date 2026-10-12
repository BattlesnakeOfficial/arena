import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test('admin browses email catalog and previews at mobile width', async ({ authenticatedPage, mockUser }, testInfo) => {
  const capture = async (name: string) => {
    const path = testInfo.outputPath(`${name}.png`);
    await authenticatedPage.screenshot({ path, fullPage: true });
    await testInfo.attach(name, { path, contentType: 'image/png' });
  };
  await query('UPDATE users SET is_admin = true WHERE github_login = $1', [mockUser.login]);
  await authenticatedPage.setViewportSize({ width: 375, height: 812 });
  await authenticatedPage.goto('/admin');
  await authenticatedPage.getByRole('link', { name: 'Emails' }).click();
  await expect(authenticatedPage).toHaveURL(/\/admin\/emails$/);
  await expect(authenticatedPage.getByRole('heading', { name: 'Emails' })).toBeVisible();
  await capture('email-catalog-mobile-light');

  for (const name of ['Snake matchmaking paused', 'Claim verification']) {
    await authenticatedPage.getByRole('link', { name }).click();
    await expect(authenticatedPage.getByRole('heading', { name })).toBeVisible();
    await expect(authenticatedPage.locator('iframe[title="Email HTML preview"]')).toHaveAttribute('sandbox', '');
    await expect(authenticatedPage.getByRole('heading', { name: 'Plain text' })).toBeVisible();
    for (const selector of ['iframe[title="Email HTML preview"]', 'pre', 'h2:has-text("Extra headers") + div']) {
      const width = await authenticatedPage.locator(selector).evaluate((node) => node.getBoundingClientRect().width);
      expect(width).toBeLessThanOrEqual(375);
    }
    const button = authenticatedPage.getByRole('button', { name: 'Send test to me' });
    expect(await button.evaluate((node) => node.getBoundingClientRect().width)).toBeLessThanOrEqual(375);
    expect(await button.evaluate((node) => node.getBoundingClientRect().height)).toBeGreaterThanOrEqual(44);
    expect(await authenticatedPage.locator('body').evaluate((node) => parseFloat(getComputedStyle(node).fontSize))).toBeGreaterThanOrEqual(16);
    expect(await authenticatedPage.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await authenticatedPage.getByRole('link', { name: 'All emails' }).click();
  }

  await authenticatedPage.getByRole('link', { name: 'Snake matchmaking paused' }).click();
  await capture('email-preview-mobile-light');
  await authenticatedPage.setViewportSize({ width: 1280, height: 900 });
  await authenticatedPage.emulateMedia({ colorScheme: 'dark' });
  await capture('email-preview-desktop-dark');
});

test('admin catalog and previews navigate without JavaScript', async ({ browser, authenticatedPage, mockUser }) => {
  await query('UPDATE users SET is_admin = true WHERE github_login = $1', [mockUser.login]);
  const context = await browser.newContext({ javaScriptEnabled: false, viewport: { width: 375, height: 812 } });
  try {
    await context.addCookies(await authenticatedPage.context().cookies());
    const page = await context.newPage();
    await page.goto('/admin');
    await page.getByRole('link', { name: 'Emails' }).click();
    await page.getByRole('link', { name: 'Claim verification' }).click();
    await expect(page.getByRole('heading', { name: 'Claim verification' })).toBeVisible();
    await expect(page.locator('iframe[title="Email HTML preview"]')).toHaveAttribute('sandbox', '');
    await expect(page.getByRole('heading', { name: 'Plain text' })).toBeVisible();
  } finally {
    await context.close();
  }
});
