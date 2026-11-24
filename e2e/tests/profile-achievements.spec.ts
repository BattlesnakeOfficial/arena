import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

// Specs share one database: seeded rows are removed per test in afterEach.
// Every user here is ours (raw INSERT), so deleting the user row is safe —
// achievement earns cascade from it.
const seededUsers: string[] = [];

test.afterEach(async () => {
  for (const id of seededUsers.splice(0)) {
    await query('DELETE FROM achievement_earns WHERE user_id = $1', [id]);
    await query('DELETE FROM users WHERE user_id = $1', [id]);
  }
});

async function seedUser(login: string): Promise<string> {
  const [user] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login)
     VALUES ($1, $2) RETURNING user_id`,
    [Date.now() * 1000 + Math.floor(Math.random() * 1000), login],
  );
  seededUsers.push(user.user_id);
  return user.user_id;
}

async function seedEarn(userId: string, slug: string, isoDate: string): Promise<void> {
  await query(
    `INSERT INTO achievement_earns (user_id, achievement, live, earned_at)
     VALUES ($1, $2, false, $3::timestamptz)`,
    [userId, slug, isoDate],
  );
}

test('five achievements list newest-first and fit a phone', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `ach_five_${stamp}`;
  const userId = await seedUser(login);
  // Seeded oldest-first so the rendered order has to come from the query,
  // not from insertion order.
  await seedEarn(userId, 'crowd_pleaser', '2026-09-05 12:00:00+00');
  await seedEarn(userId, 'slow_and_steady', '2026-09-12 12:00:00+00');
  await seedEarn(userId, 'constrictor_champ', '2026-09-19 12:00:00+00');
  await seedEarn(userId, 'ladder_regular', '2026-09-26 12:00:00+00');
  await seedEarn(userId, 'first_win', '2026-10-03 12:00:00+00');

  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto(`/users/${login}`);
  await page.evaluate(() => document.fonts.ready);

  await expect(page.getByRole('heading', { name: 'Achievements' })).toBeVisible();
  const items = page.locator('.profile-achievement');
  await expect(items).toHaveCount(5);
  // Newest first.
  await expect(items.first()).toContainText('First Win');
  await expect(items.first()).toContainText('Unlocked Oct 3, 2026');
  await expect(items.nth(3)).toContainText('Slow and Steady');

  // Every item is a tappable target in both dimensions.
  for (const item of await page.locator('.profile-achievements a').all()) {
    const box = await item.boundingBox();
    expect(box?.width, item.toString()).toBeGreaterThanOrEqual(44);
    expect(box?.height, item.toString()).toBeGreaterThanOrEqual(44);
  }

  // Every visible text element renders at 16px or larger.
  for (const el of await page.locator('.profile-achievements *').all()) {
    const info = await el.evaluate((node) => ({
      own: Array.from(node.childNodes).some(
        (n) => n.nodeType === 3 && (n.textContent ?? '').trim().length > 0,
      ),
      size: parseFloat(getComputedStyle(node).fontSize),
      visible: !!(node.offsetWidth || node.offsetHeight),
    }));
    if (info.own && info.visible) {
      expect(info.size, `16px text: ${JSON.stringify(info)}`).toBeGreaterThanOrEqual(16);
    }
  }

  // No horizontal overflow.
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    ),
  ).toBeLessThan(1);

  // The section never links to a game (scoped: Saved Games may).
  await expect(page.locator('.profile-achievements a[href*="/games/"]')).toHaveCount(0);

  // The stable-ID route shows the same section.
  await page.goto(`/users/${login}/${userId}`);
  await expect(page.getByRole('heading', { name: 'Achievements' })).toBeVisible();
  await expect(page.locator('.profile-achievement')).toHaveCount(5);
});

test('art stays legible in dark mode', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `ach_dark_${stamp}`;
  const userId = await seedUser(login);
  await seedEarn(userId, 'first_win', '2026-10-03 12:00:00+00');

  await page.emulateMedia({ colorScheme: 'dark' }); // before goto: bootstrap resolves it pre-paint
  await page.goto(`/users/${login}`);
  await expect(page.getByRole('heading', { name: 'Achievements' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.dataset.appTheme)).toBe('dark');

  const art = page.locator('.profile-achievement-art img');
  const count = await art.count();
  expect(count).toBeGreaterThan(0);
  for (const el of await art.all()) {
    const background = await el.evaluate((node) => getComputedStyle(node).backgroundColor);
    expect(background, 'fixed light tile behind transparent art').toBe('rgb(244, 241, 235)');
  }

  await page.emulateMedia({ colorScheme: 'light' }); // don't leak emulation into later tests
});

test('one achievement and none at all', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const one = `ach_one_${stamp}`;
  const none = `ach_none_${stamp}`;
  const oneId = await seedUser(one);
  await seedUser(none);
  await seedEarn(oneId, 'first_win', '2026-10-03 12:00:00+00');

  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto(`/users/${one}`);
  await expect(page.locator('.profile-achievement')).toHaveCount(1);
  await expect(page.locator('.profile-achievement')).toContainText('First Win');

  await page.goto(`/users/${none}`);
  await expect(page.getByRole('heading', { name: 'Achievements' })).toHaveCount(0);
  await expect(page.locator('.profile-achievements')).toHaveCount(0);
});

test('cards stay one-line and equal-height at desktop width', async ({ page }) => {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const login = `ach_desktop_${stamp}`;
  const userId = await seedUser(login);
  // Same five earns as the phone test, seeded oldest-first.
  await seedEarn(userId, 'crowd_pleaser', '2026-09-05 12:00:00+00');
  await seedEarn(userId, 'slow_and_steady', '2026-09-12 12:00:00+00');
  await seedEarn(userId, 'constrictor_champ', '2026-09-19 12:00:00+00');
  await seedEarn(userId, 'ladder_regular', '2026-09-26 12:00:00+00');
  await seedEarn(userId, 'first_win', '2026-10-03 12:00:00+00');

  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`/users/${login}`);
  await page.evaluate(() => document.fonts.ready);

  await expect(page.getByRole('heading', { name: 'Achievements' })).toBeVisible();
  await expect(page.locator('.profile-achievement')).toHaveCount(5);

  // Every unlock date fits on a single line at this width.
  for (const el of await page.locator('.profile-achievement-date').all()) {
    const box = await el.boundingBox();
    const lineHeight = await el.evaluate((node) =>
      parseFloat(getComputedStyle(node).lineHeight),
    );
    expect(box?.height, 'unlock date wraps onto a second line').toBeLessThan(lineHeight * 1.5);
  }

  // A card whose name wraps must not outgrow its row-mates.
  await page.locator('.profile-achievement-name').first().evaluate((node) => {
    node.textContent = 'An Achievement Name Long Enough To Wrap Onto A Second Line At This Width';
  });
  const boxes = await page.locator('.profile-achievement').evaluateAll((nodes) =>
    nodes.map((node) => {
      const rect = node.getBoundingClientRect();
      return { top: rect.top, height: rect.height };
    }),
  );
  const rows = new Map<number, number[]>();
  for (const box of boxes) {
    const key = Math.round(box.top);
    rows.set(key, [...(rows.get(key) ?? []), box.height]);
  }
  for (const [top, heights] of rows) {
    for (const height of heights) {
      expect(height, `cards in the row at y=${top} must share one height`).toBe(
        Math.max(...heights),
      );
    }
  }

  // The first card links to a working share page.
  const [response] = await Promise.all([
    page.waitForResponse(
      (r) => new URL(r.url()).pathname === `/achievements/first_win/${userId}`,
    ),
    page.locator('.profile-achievement').first().click(),
  ]);
  expect(response.status()).toBe(200);
  expect(page.url().endsWith(`/achievements/first_win/${userId}`)).toBe(true);
  await expect(page.getByRole('heading', { name: 'First Win', level: 1 })).toBeVisible();
});
