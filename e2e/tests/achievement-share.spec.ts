import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

const TAP_HEIGHT = 44;

async function seedEarn(userId: string, slug: string): Promise<void> {
  await query(
    `INSERT INTO achievement_earns (user_id, achievement, live, earned_at)
     VALUES ($1, $2, false, TIMESTAMPTZ '2026-10-31 12:00:00+00')`,
    [userId, slug],
  );
}

// Specs share one database: seeded rows are removed per test (none of
// these tests seed games or snakes, so deleting the user cascades the
// earn; authenticatedPage's own mock users are cleaned by its fixture).
const seededUsers: string[] = [];

test.afterEach(async () => {
  for (const userId of seededUsers.splice(0)) {
    await query('DELETE FROM users WHERE user_id = $1', [userId]);
  }
});

test('guest share page fits a phone and the CTA posts', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `share_guest_${stamp}`;
  const [user] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, '') RETURNING user_id`,
    [Date.now() * 1000 + Math.floor(Math.random() * 1000), login],
  );
  seededUsers.push(user.user_id);
  await seedEarn(user.user_id, 'first_win');
  const path = `/achievements/first_win/${user.user_id}`;

  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto(path);
  await page.evaluate(() => document.fonts.ready);
  await expect(page.getByRole('heading', { name: 'First Win', level: 1 })).toBeVisible();
  await expect(page.locator('.ach-share .badge')).toHaveText('Unlocked');
  await expect(page.locator('.ach-share-owner .name')).toHaveText(login);
  await expect(page.getByText('Unlocked Oct 31, 2026')).toBeVisible();

  // The owner avatar is sized by the share page itself: .user-avatar has no
  // default size, so nothing here may let it collapse to its fallback glyph.
  const avatar = await page.locator('.ach-share-owner .user-avatar').boundingBox();
  expect(avatar?.width).toBe(56);
  expect(avatar?.height).toBe(56);

  // Both visitor CTAs are visible and comfortably tappable.
  const cta = page.locator('.share-cta .btn');
  await expect(cta).toHaveCount(2);
  for (const link of await cta.all()) {
    expect((await link.boundingBox())?.height).toBeGreaterThanOrEqual(TAP_HEIGHT);
  }

  // Every visible control inside the share page is tappable and every text
  // element renders at 16px or larger.
  for (const el of await page.locator('.ach-share a, .ach-share button, .ach-share input').all()) {
    const box = await el.boundingBox();
    expect(box?.height, el.toString()).toBeGreaterThanOrEqual(TAP_HEIGHT);
  }
  for (const el of await page.locator('.ach-share *').all()) {
    const info = await el.evaluate((node) => ({
      own: Array.from(node.childNodes).some(
        (n) => n.nodeType === 3 && (n.textContent ?? '').trim().length > 0,
      ),
      size: parseFloat(getComputedStyle(node).fontSize),
      visible: !!(node.offsetWidth || node.offsetHeight),
    }));
    if (info.own && info.visible) {
      expect(info.size, `16px text: ${info}`).toBeGreaterThanOrEqual(16);
    }
  }
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    ),
  ).toBeLessThan(1);

  // Clicking the achievements CTA records the click before navigating.
  await Promise.all([
    page.waitForRequest(
      (request) => request.method() === 'POST' && request.url().includes('/cta-click'),
    ),
    cta.first().click(),
  ]);
  await expect(page).toHaveURL(/\/customizations\/achievements$/);
});

test('long unbroken names still fit a phone', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  // GitHub logins run to 39 characters and carry no break opportunities.
  const login = `longlogin${stamp}`.padEnd(39, 'x');
  const snakeName = `longsnake${stamp}`.padEnd(64, 'y');
  const [user] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, '') RETURNING user_id`,
    [Date.now() * 1000 + Math.floor(Math.random() * 1000), login],
  );
  const [snake] = await query<{ battlesnake_id: string }>(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     VALUES ($1, $2, 'https://example.com', 'private') RETURNING battlesnake_id`,
    [user.user_id, snakeName],
  );
  try {
    await query(
      `INSERT INTO achievement_earns (user_id, achievement, battlesnake_id, live, earned_at)
       VALUES ($1, 'first_win', $2, true, TIMESTAMPTZ '2026-10-31 12:00:00+00')`,
      [user.user_id, snake.battlesnake_id],
    );
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto(`/achievements/first_win/${user.user_id}`);
    await page.evaluate(() => document.fonts.ready);
    await expect(page.locator('.ach-share-owner .name')).toHaveText(login);
    await expect(page.locator('.ach-share-snake')).toContainText(snakeName);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
      ),
    ).toBeLessThan(1);
  } finally {
    await query('DELETE FROM achievement_earns WHERE user_id = $1', [user.user_id]);
    await query('DELETE FROM battlesnakes WHERE battlesnake_id = $1', [snake.battlesnake_id]);
    await query('DELETE FROM users WHERE user_id = $1', [user.user_id]);
  }
});

test('unknown, unearned, and malformed share URLs are branded 404s', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `share_missing_${stamp}`;
  const [noEarn] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, '') RETURNING user_id`,
    [Date.now() * 1000 + Math.floor(Math.random() * 1000), login],
  );
  seededUsers.push(noEarn.user_id);
  for (const path of [
    `/achievements/nope/${noEarn.user_id}`,
    `/achievements/first_win/${noEarn.user_id}`,
    '/achievements/first_win/not-a-uuid',
  ]) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(404);
    await expect(page.getByRole('heading', { name: '404 — Page not found' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to Home', exact: true })).toHaveAttribute('href', '/');
  }
});

test('owner sees share actions and copy posts', async ({ authenticatedPage, mockUser }) => {
  const [user] = await query<{ user_id: string }>(
    'SELECT user_id FROM users WHERE github_login = $1',
    [mockUser.login],
  );
  await seedEarn(user.user_id, 'first_win');
  const path = `/achievements/first_win/${user.user_id}`;

  // No native share sheet in this browser: the button stays hidden.
  await authenticatedPage.addInitScript(
    'Object.defineProperty(navigator, "share", { value: undefined })',
  );
  await authenticatedPage.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  await authenticatedPage.goto(path);
  const copy = authenticatedPage.locator('#share-copy');
  await expect(copy).toBeVisible();
  await expect(authenticatedPage.locator('#share-download')).toBeVisible();
  await expect(authenticatedPage.locator('#share-native')).not.toBeVisible();
  await expect(authenticatedPage.locator('#share-url')).toHaveValue(new RegExp(`${path}$`));

  await Promise.all([
    authenticatedPage.waitForRequest(
      (request) => request.method() === 'POST' && request.url().includes('/share-action'),
    ),
    copy.click(),
  ]);
  await expect(copy).toHaveText('Copied!');
});

test('the ladder links Share only for earned steps', async ({ authenticatedPage, mockUser }) => {
  const [user] = await query<{ user_id: string }>(
    'SELECT user_id FROM users WHERE github_login = $1',
    [mockUser.login],
  );
  await seedEarn(user.user_id, 'first_win');
  await authenticatedPage.goto('/customizations/achievements');
  const steps = authenticatedPage.locator('ol.cz-ladder li.cz-step');
  await expect(steps.first().locator('a.cz-share-link')).toHaveAttribute(
    'href',
    `/achievements/first_win/${user.user_id}`,
  );
  await expect(steps.locator('a.cz-share-link')).toHaveCount(1);
});
