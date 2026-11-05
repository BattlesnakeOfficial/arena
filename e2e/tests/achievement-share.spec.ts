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

// Specs share one database: seeded rows are removed per test in afterEach.
// The early tests seed only own users; the clip tests also seed snakes and
// games. A fixture's mock user is NOT ours to delete (its session blocks
// the delete; the authenticatedPage teardown owns it) — only its earns.
const seededUsers: { id: string; own: boolean }[] = [];
const seededGames: string[] = [];
const seededSnakes: string[] = [];

test.afterEach(async () => {
  for (const { id, own } of seededUsers.splice(0)) {
    await query(
      'DELETE FROM achievement_clips WHERE achievement_earn_id IN (SELECT achievement_earn_id FROM achievement_earns WHERE user_id = $1)',
      [id],
    );
    await query('DELETE FROM achievement_earns WHERE user_id = $1', [id]);
    if (own) {
      await query('DELETE FROM users WHERE user_id = $1', [id]);
    }
  }
  for (const gameId of seededGames.splice(0)) {
    await query('DELETE FROM turns WHERE game_id = $1', [gameId]);
    await query('DELETE FROM game_battlesnakes WHERE game_id = $1', [gameId]);
    await query('DELETE FROM games WHERE game_id = $1', [gameId]);
  }
  for (const snakeId of seededSnakes.splice(0)) {
    await query('DELETE FROM battlesnakes WHERE battlesnake_id = $1', [snakeId]);
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
  seededUsers.push({ id: user.user_id, own: true });
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
  seededUsers.push({ id: noEarn.user_id, own: true });
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

// A 1x1 transparent GIF: the e2e stack has no object store, so the clip
// URL is fulfilled from the wire (the route must never hit the server).
const CLIP_GIF_BASE64 = 'R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7';

// Seeds a live first_win earn with a finished game and a clip row in the
// given status. Returns the share path.
async function seedEarnWithClip(
  userId: string,
  status: 'rendered' | 'pending',
): Promise<string> {
  const [game] = await query<{ game_id: string }>(
    `INSERT INTO games (board_size, game_type, status, finished_at)
     VALUES ('11x11', 'Standard', 'finished', NOW()) RETURNING game_id`,
  );
  seededGames.push(game.game_id);
  const [snake] = await query<{ battlesnake_id: string }>(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     VALUES ($1, 'clip e2e snake', 'https://example.com', 'private') RETURNING battlesnake_id`,
    [userId],
  );
  seededSnakes.push(snake.battlesnake_id);
  await query(
    `INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement) VALUES ($1, $2, 1)`,
    [game.game_id, snake.battlesnake_id],
  );
  await query(
    `INSERT INTO achievement_earns (user_id, achievement, battlesnake_id, game_id, turn_number, live, earned_at)
     VALUES ($1, 'first_win', $2, $3, 5, TRUE, NOW())`,
    [userId, snake.battlesnake_id, game.game_id],
  );
  await query(
    `INSERT INTO achievement_clips (achievement_earn_id, status, byte_size, render_ms, rendered_at)
     SELECT achievement_earn_id, $2, 1000, 10, CASE WHEN $2 = 'rendered' THEN NOW() ELSE NULL END
     FROM achievement_earns WHERE user_id = $1 AND achievement = 'first_win'`,
    [userId, status],
  );
  return `/achievements/first_win/${userId}`;
}

test('a rendered clip shows on the share page and the owner can download it', async ({
  authenticatedPage,
  mockUser,
}) => {
  const [user] = await query<{ user_id: string }>(
    'SELECT user_id FROM users WHERE github_login = $1',
    [mockUser.login],
  );
  seededUsers.push({ id: user.user_id, own: false }); // the mock user: only its earns are ours
  const path = await seedEarnWithClip(user.user_id, 'rendered');

  await authenticatedPage.route('**/clip.gif', (route) =>
    route.fulfill({ contentType: 'image/gif', body: Buffer.from(CLIP_GIF_BASE64, 'base64') }),
  );
  await authenticatedPage.setViewportSize({ width: 375, height: 812 });
  await authenticatedPage.goto(path);
  await authenticatedPage.evaluate(() => document.fonts.ready);

  const img = authenticatedPage.locator('figure.ach-share-clip img');
  await expect(img).toBeVisible();
  await expect(img).toHaveAttribute('src', /\/clip\.gif$/);
  // The image really decoded before measuring layout against it.
  expect(
    await img.evaluate((node) => (node as HTMLImageElement).naturalWidth),
  ).toBeGreaterThan(0);
  expect(
    await authenticatedPage.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    ),
  ).toBeLessThan(1);

  // The owner's clip download is a real tap target and posts the share
  // action like the card download.
  const download = authenticatedPage.locator('figure.ach-share-clip [data-share-download]');
  await expect(download).toBeVisible();
  expect((await download.boundingBox())?.height).toBeGreaterThanOrEqual(TAP_HEIGHT);
  await Promise.all([
    authenticatedPage.waitForRequest(
      (request) => request.method() === 'POST' && request.url().includes('/share-action'),
    ),
    download.click(),
  ]);
});

test('a pending clip leaves the page finished with the card', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `clip_pending_${stamp}`;
  const [user] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, '') RETURNING user_id`,
    [Date.now() * 1000 + Math.floor(Math.random() * 1000), login],
  );
  seededUsers.push({ id: user.user_id, own: true });
  const path = await seedEarnWithClip(user.user_id, 'pending');

  await page.route('**/clip.gif', (route) =>
    route.fulfill({ contentType: 'image/gif', body: Buffer.from(CLIP_GIF_BASE64, 'base64') }),
  );
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto(path);
  await expect(page.getByRole('heading', { name: 'First Win', level: 1 })).toBeVisible();
  await expect(page.locator('.ach-share .badge')).toHaveText('Unlocked');
  // No clip figure, no clip URL anywhere: pending looks finished.
  await expect(page.locator('.ach-share-clip')).toHaveCount(0);
  expect(await page.content()).not.toContain('clip.gif');
});
test('the share art keeps its light tile in dark mode', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `share_dark_${stamp}`;
  const [user] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, '') RETURNING user_id`,
    [Date.now() * 1000 + Math.floor(Math.random() * 1000), login],
  );
  seededUsers.push({ id: user.user_id, own: true });
  await seedEarn(user.user_id, 'first_win');

  // The head/tail art has no fill of its own: without a fixed light tile
  // (like .cz-swatch) it renders black on transparent in the dark theme.
  await page.emulateMedia({ colorScheme: 'dark' }); // before goto: bootstrap resolves it pre-paint
  await page.goto(`/achievements/first_win/${user.user_id}`);
  expect(await page.evaluate(() => document.documentElement.dataset.appTheme)).toBe('dark');
  expect(
    await page.evaluate(
      () => getComputedStyle(document.querySelector('.ach-share-art img')!).backgroundColor,
    ),
  ).toBe('rgb(244, 241, 235)');
  await page.emulateMedia({ colorScheme: 'light' }); // don't leak emulation into later tests
});
