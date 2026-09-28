import { test, expect } from '@playwright/test';

test('anonymous visitor can read public stats and JSON without horizontal scroll', async ({ page }) => {
  await page.goto('/');
  await page.locator('.site-nav .links').getByRole('link', { name: 'Stats' }).click();
  await expect(page).toHaveURL(/\/stats$/);
  await expect(page.getByRole('heading', { name: 'Arena stats' })).toBeVisible();
  await expect(page.locator('.public-stats-tiles .stat')).toHaveCount(8);
  await expect(page.locator('svg.public-stats-chart')).toHaveCount(6);
  await expect(page.locator('table.public-stats-table')).toHaveCount(6);
  await expect(page.getByText(/UTC; through \d{4}-\d{2}-\d{2}/)).toBeVisible();
  await expect(page.getByText(/Live activity tracking starts on/)).toBeVisible();
  await expect(page.getByText(/An active user is an account/)).toBeVisible();
  await expect(page.getByText(/A played game finished/)).toBeVisible();

  const response = await page.request.get('/api/stats');
  expect(response.ok()).toBeTruthy();
  const data = await response.json();
  expect(Object.keys(data.headlines).sort()).toEqual([
    'active_snakes_7d', 'dau', 'dau_mau_percent', 'games_7d',
    'mau', 'registered_users', 'total_snakes', 'wau',
  ]);
  for (const key of ['daily_active_users', 'weekly_active_users', 'daily_games',
    'weekly_games', 'weekly_growth', 'weekly_active_snakes']) {
    expect(Array.isArray(data[key])).toBeTruthy();
  }
  expect(data.daily_active_users).toHaveLength(90);
  expect(data.weekly_active_users).toHaveLength(52);

  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto('/');
  await page.locator('.mobile-menu summary').click();
  await page.locator('.mobile-menu').getByRole('link', { name: 'Stats' }).click();
  await page.evaluate(() => document.fonts.ready);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBeTruthy();
  expect(await page.locator('body').evaluate((el) => parseFloat(getComputedStyle(el).fontSize))).toBeGreaterThanOrEqual(16);
  await expect(page.locator('svg.public-stats-chart')).toHaveCount(6);
});

test('stats charts and tables render without JavaScript', async ({ browser }) => {
  const context = await browser.newContext({ javaScriptEnabled: false });
  try {
    const page = await context.newPage();
    await page.goto('/stats');
    await expect(page.getByRole('heading', { name: 'Arena stats' })).toBeVisible();
    await expect(page.locator('svg.public-stats-chart')).toHaveCount(6);
    await expect(page.locator('table.public-stats-table')).toHaveCount(6);
  } finally {
    await context.close();
  }
});
