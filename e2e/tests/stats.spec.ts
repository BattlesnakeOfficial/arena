import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test('logged-out visitors cannot reach stats or find it in navigation', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('.site-nav .links').getByRole('link', { name: 'Stats' })).toHaveCount(0);
  await page.setViewportSize({ width: 375, height: 812 });
  await page.locator('.mobile-menu summary').click();
  await expect(page.locator('.mobile-menu').getByRole('link', { name: 'Stats' })).toHaveCount(0);
  const response = await page.goto('/stats');
  expect(response?.status()).toBe(401);
  await expect(page.getByRole('heading', { name: 'Arena stats' })).toHaveCount(0);
  expect((await page.request.get('/api/stats')).ok()).toBeFalsy();
});

test('admin reaches stats from dashboard and sees aggregates at mobile width', async ({ authenticatedPage, mockUser }) => {
  await query('UPDATE users SET is_admin = true WHERE github_login = $1', [mockUser.login]);
  await authenticatedPage.goto('/admin');
  await authenticatedPage.getByRole('link', { name: 'Stats' }).click();
  await expect(authenticatedPage).toHaveURL(/\/stats$/);
  await expect(authenticatedPage.getByRole('heading', { name: 'Arena stats' })).toBeVisible();
  await expect(authenticatedPage.locator('.public-stats-tiles .stat')).toHaveCount(8);
  await expect(authenticatedPage.locator('svg.public-stats-chart')).toHaveCount(4);
  await expect(authenticatedPage.locator('table.public-stats-table')).toHaveCount(4);
  await expect(authenticatedPage.locator('.public-stats-collecting')).toHaveCount(2);

  const response = await authenticatedPage.request.get('/api/stats');
  expect(response.ok()).toBeTruthy();
  const data = await response.json();
  expect(Object.keys(data.headlines).sort()).toEqual([
    'active_snakes_7d', 'dau', 'dau_available_on', 'dau_mau_percent',
    'dau_mau_percent_available_on', 'games_7d', 'mau', 'mau_available_on',
    'registered_users', 'total_snakes', 'wau', 'wau_available_on',
  ]);
  for (const key of ['daily_active_users', 'weekly_active_users', 'daily_games',
    'weekly_games', 'weekly_growth', 'weekly_active_snakes']) {
    expect(Array.isArray(data[key])).toBeTruthy();
  }
  expect(data.daily_active_users.length).toBeLessThanOrEqual(90);
  expect(data.weekly_active_users.length).toBeLessThanOrEqual(52);
  expect(data.daily_active_users.every((row: { date: string }) => row.date >= data.live_tracking_started_on)).toBeTruthy();
  expect(data.weekly_active_users.every((row: { week_start: string }) => row.week_start >= data.live_tracking_started_on)).toBeTruthy();
  for (const [metric, available] of [
    ['dau', 'dau_available_on'], ['wau', 'wau_available_on'],
    ['mau', 'mau_available_on'], ['dau_mau_percent', 'dau_mau_percent_available_on'],
  ]) {
    expect(typeof data.headlines[available]).toBe('string');
    expect(data.headlines[metric] === null || typeof data.headlines[metric] === 'number').toBeTruthy();
  }

  await authenticatedPage.setViewportSize({ width: 375, height: 812 });
  await authenticatedPage.reload();
  await authenticatedPage.evaluate(() => document.fonts.ready);
  expect(await authenticatedPage.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBeTruthy();
  expect(await authenticatedPage.locator('body').evaluate((el) => parseFloat(getComputedStyle(el).fontSize))).toBeGreaterThanOrEqual(16);
  await expect(authenticatedPage.locator('svg.public-stats-chart')).toHaveCount(4);
  await expect(authenticatedPage.locator('table.public-stats-table')).toHaveCount(4);
  await expect(authenticatedPage.locator('.public-stats-collecting')).toHaveCount(2);
  const narrowTablesFit = await authenticatedPage.locator('.public-stats-table-wrap').evaluateAll((wrappers) =>
    wrappers.filter((wrapper) => wrapper.querySelectorAll('thead th').length <= 3)
      .every((wrapper) => wrapper.scrollWidth <= wrapper.clientWidth));
  expect(narrowTablesFit).toBeTruthy();
});

test('admin stats charts and tables render without JavaScript', async ({ browser, authenticatedPage, mockUser }) => {
  await query('UPDATE users SET is_admin = true WHERE github_login = $1', [mockUser.login]);
  const context = await browser.newContext({ javaScriptEnabled: false });
  try {
    await context.addCookies(await authenticatedPage.context().cookies());
    const page = await context.newPage();
    await page.goto('/stats');
    await expect(page.getByRole('heading', { name: 'Arena stats' })).toBeVisible();
    await expect(page.locator('svg.public-stats-chart')).toHaveCount(4);
    await expect(page.locator('table.public-stats-table')).toHaveCount(4);
    await expect(page.locator('.public-stats-collecting')).toHaveCount(2);
  } finally {
    await context.close();
  }
});
