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
    await expect(page.getByRole('link', { name: 'Battlesnake — home' })).toBeVisible();
  }
  // Desktop viewport: the full official wordmark is the visible variant,
  // and the four-square glyph appears nowhere.
  await expect(page.locator('a.wordmark .wordmark-word')).toBeVisible();
  await expect(page.locator('a.wordmark svg.glyph')).toHaveCount(0);
  await page.goto('/');
  await expect(page.locator('.hero h1')).toContainText('Battlesnake');
  await expect(page).toHaveTitle('Home — Battlesnake');

  // The wordmark's BATTLE/SNAKE fills must resolve to the page's theme
  // tokens in both themes (anonymous pages resolve prefers-color-scheme
  // pre-paint via theme-bootstrap.js).
  const hexToRgb = (hex: string) => {
    const n = parseInt(hex.slice(1), 16);
    return `rgb(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255})`;
  };
  for (const scheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme: scheme }); // before goto: bootstrap resolves it pre-paint
    await page.goto('/');
    expect(await page.evaluate(() => document.documentElement.dataset.appTheme)).toBe(scheme);
    const fills = await page.evaluate(() => {
      const cs = getComputedStyle(document.documentElement);
      const word = document.querySelector('a.wordmark .wordmark-word')!;
      return {
        ink: cs.getPropertyValue('--ink').trim(),
        pink: cs.getPropertyValue('--pink').trim(),
        battle: getComputedStyle(word.querySelector('path.cls-2')!).fill,
        snake: getComputedStyle(word.querySelector('path.cls-3')!).fill,
      };
    });
    expect(fills.battle, `${scheme} BATTLE fill`).toBe(hexToRgb(fills.ink));
    expect(fills.snake, `${scheme} SNAKE fill`).toBe(hexToRgb(fills.pink));
  }
  await page.emulateMedia({ colorScheme: 'light' }); // don't leak emulation into later tests
});

test('wordmark stays visible on phones when logged out', async ({ page }) => {
  // DEV-1751: the brand must be visible on public pages at phone width,
  // without introducing horizontal scroll. At 360/375px the full wordmark
  // fits the logged-out nav; below 360px every page falls back to the
  // circle badge.
  for (const width of [360, 375]) {
    await page.setViewportSize({ width, height: 812 });
    await page.goto('/');
    await expect(page.locator('a.wordmark .wordmark-word'), `${width}px`).toBeVisible();
    await expect(page.locator('a.wordmark .wordmark-badge'), `${width}px`).toBeHidden();
    await expect(page.locator('a.wordmark svg.glyph'), `${width}px`).toHaveCount(0);
    const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
    expect(scrollWidth, `${width}px horizontal overflow`).toBeLessThanOrEqual(width);
  }
  await page.setViewportSize({ width: 320, height: 812 });
  await page.goto('/');
  await expect(page.locator('a.wordmark .wordmark-badge'), '320px').toBeVisible();
  await expect(page.locator('a.wordmark .wordmark-word'), '320px').toBeHidden();
  await expect(page.locator('a.wordmark svg.glyph'), '320px').toHaveCount(0);
  const scrollWidth = await page.evaluate(() => document.documentElement.scrollWidth);
  expect(scrollWidth, '320px horizontal overflow').toBeLessThanOrEqual(320);
});

test('signed-in header keeps a brand mark and the avatar on phones', async ({ authenticatedPage }) => {
  // The signed-in nav chrome is ~300px fixed, so the wordmark steps down
  // (full -> compact -> circle badge) as the viewport narrows. At every
  // width the header keeps a visible brand mark, the avatar stays fully
  // inside the viewport, and the four-square glyph appears nowhere.
  const noOverflow = async (width: number) => {
    const scrollWidth = await authenticatedPage.evaluate(() => document.documentElement.scrollWidth);
    expect(scrollWidth, `${width}px horizontal overflow`).toBeLessThanOrEqual(width);
  };
  const noGlyph = async (width: number) => {
    await expect(authenticatedPage.locator('a.wordmark svg.glyph'), `${width}px`).toHaveCount(0);
  };

  await authenticatedPage.setViewportSize({ width: 400, height: 812 });
  await authenticatedPage.goto('/');
  await expect(authenticatedPage.locator('a.wordmark .wordmark-word')).toBeVisible();
  await noOverflow(400);
  await noGlyph(400);

  await authenticatedPage.setViewportSize({ width: 390, height: 812 });
  await expect(authenticatedPage.locator('a.wordmark .wordmark-word')).toBeVisible();
  const compactHeight = (await authenticatedPage.locator('a.wordmark .wordmark-word').boundingBox())?.height;
  expect(compactHeight, '390px compact wordmark height').toBeGreaterThanOrEqual(28);
  await noOverflow(390);
  await noGlyph(390);

  await authenticatedPage.setViewportSize({ width: 375, height: 812 });
  await expect(authenticatedPage.locator('a.wordmark .wordmark-badge')).toBeVisible();
  await expect(authenticatedPage.locator('a.wordmark .wordmark-word')).toBeHidden();
  const avatar = await authenticatedPage.locator('.nav-avatar').boundingBox();
  expect(avatar, '375px avatar bounding box').toBeTruthy();
  if (avatar) {
    expect(avatar.x, '375px avatar left edge').toBeGreaterThanOrEqual(0);
    expect(avatar.x + avatar.width, '375px avatar right edge').toBeLessThanOrEqual(375);
  }
  await noOverflow(375);
  await noGlyph(375);

  for (const width of [360, 320]) {
    await authenticatedPage.setViewportSize({ width, height: 812 });
    await expect(authenticatedPage.locator('a.wordmark .wordmark-badge'), `${width}px`).toBeVisible();
    await expect(authenticatedPage.locator('a.wordmark .wordmark-word'), `${width}px`).toBeHidden();
    await noOverflow(width);
    await noGlyph(width);
  }
});
