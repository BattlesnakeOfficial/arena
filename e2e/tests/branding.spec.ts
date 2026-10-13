import { test, expect } from '../fixtures/test';

const PUBLIC_PAGES = [
  '/', '/leaderboards', '/tournaments', '/snakes', '/players', '/rankings',
  '/customizations', '/customizations/studio/guide', '/conduct', '/privacy', '/terms',
];

test('public pages render no user-visible Arena and brand as Battlesnake', async ({ page }) => {
  for (const path of PUBLIC_PAGES) {
    await page.goto(path);
    const visible = await page.locator('body').innerText();
    expect(visible.match(/\bArena\b/), `${path} visible text`).toBeNull();
    expect(await page.content(), `${path} html`).not.toMatch(/\bArena\b/);
    await expect(page.locator('meta[property="og:site_name"]')).toHaveAttribute('content', 'Battlesnake');
    await expect(page.locator('.wordmark-text')).toHaveText('Battlesnake');
  }
  await page.goto('/');
  await expect(page.locator('.hero h1')).toContainText('Battlesnake');
  await expect(page).toHaveTitle('Home — Battlesnake');
});

test('wordmark text stays visible on phones when logged out', async ({ page }) => {
  // DEV-1751: the <=420px hide is scoped to [data-authed] pages, so the
  // brand must be visible on public pages at phone width, without
  // introducing horizontal scroll.
  for (const width of [360, 375]) {
    await page.setViewportSize({ width, height: 812 });
    await page.goto('/');
    await expect(page.locator('.wordmark-text'), `${width}px`).toBeVisible();
    const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
    expect(scrollWidth, `${width}px horizontal overflow`).toBeLessThanOrEqual(width);
  }
});
