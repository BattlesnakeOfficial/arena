import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test('admin creates a private leaderboard and a player joins it with the invite code', async ({ authenticatedPage: page, mockUser }, testInfo) => {
  const capture = async (name: string) => {
    const path = testInfo.outputPath(`${name}.png`);
    await page.screenshot({ path, fullPage: true });
    await testInfo.attach(name, { path, contentType: 'image/png' });
  };
  const name = `Private League ${testInfo.workerIndex} ${Date.now()}`;
  const snakeName = `Private LB Snake ${Date.now()}`;
  await query('UPDATE users SET is_admin = true WHERE github_login = $1', [mockUser.login]);
  await page.setViewportSize({ width: 375, height: 812 });

  try {
    // Admin creates the ladder from the dashboard.
    await page.goto('/admin');
    await page.getByRole('main').getByRole('link', { name: 'Leaderboards' }).click();
    await expect(page).toHaveURL(/\/admin\/leaderboards$/);
    await page.getByLabel('Name').fill(name);
    await page.getByLabel('Game type').selectOption('Royale');
    await page.getByLabel('Snakes per game').selectOption('2');
    const create = page.getByRole('button', { name: 'Create private leaderboard' });
    expect(await create.evaluate((node) => node.getBoundingClientRect().height)).toBeGreaterThanOrEqual(44);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await create.click();
    await expect(page.locator('.flash-message[data-flash-type="success"]')).toContainText(`Created ${name}`);

    const card = page.getByRole('article', { name });
    await expect(card).toContainText('Royale · 11x11 · 2 snakes');
    const code = (await card.locator('code').textContent())?.trim() ?? '';
    expect(code).toMatch(/^[A-Z2-9]{12}$/);
    // The link and code are what gets copied: both fit a phone unscrolled.
    for (const target of [card.locator('code'), card.getByText('/leaderboards/')]) {
      const box = await target.boundingBox();
      expect(box && box.x + box.width).toBeLessThanOrEqual(375);
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await capture('admin-leaderboards-mobile');

    // Unlisted: not on the public directory.
    await page.goto('/leaderboards');
    await expect(page.getByRole('link', { name })).toHaveCount(0);

    // The player needs a snake to join with.
    await page.goto('/battlesnakes/new');
    await page.getByLabel('Name').fill(snakeName);
    await page.getByLabel('URL').fill('https://example.com/private-lb');
    await page.getByLabel('Visibility').selectOption('public');
    await page.getByRole('button', { name: 'Create Battlesnake' }).click();

    // The direct link works, flagged private and kept out of search.
    await page.goto('/admin/leaderboards');
    await page.getByRole('link', { name }).click();
    await expect(page.getByRole('heading', { name })).toContainText('Private');
    await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', 'noindex');

    const invite = page.getByLabel('Invite code');
    expect(await invite.evaluate((node) => parseFloat(getComputedStyle(node).fontSize))).toBeGreaterThanOrEqual(16);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await capture('private-leaderboard-invite-mobile');
    await invite.fill('WRONGWRONG22');
    await page.getByRole('button', { name: 'Join' }).click();
    await expect(page.locator('.flash-message[data-flash-type="error"]')).toContainText('invite code');
    await expect(page.getByRole('cell', { name: snakeName })).toHaveCount(0);

    await page.getByLabel('Invite code').fill(code.toLowerCase());
    await page.getByRole('button', { name: 'Join' }).click();
    await expect(page.locator('.flash-message[data-flash-type="success"]')).toContainText('matchmaking rotation');
    // Members aren't asked for the code again.
    await expect(page.getByLabel('Invite code')).toHaveCount(0);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
    await capture('private-leaderboard-joined-mobile');
  } finally {
    await query('DELETE FROM leaderboards WHERE name = $1', [name]);
  }
});
