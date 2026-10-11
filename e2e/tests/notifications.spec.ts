import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test('drawer, focus, badge polling, and responsive themes', async ({ authenticatedPage, mockUser }) => {
  const page = authenticatedPage;
  const [user] = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [mockUser.login]);
  await query(`
    INSERT INTO notifications (user_id, kind, title, body, link, in_app_visible, created_at, read_at)
    SELECT $1, 'snake_matchmaking_paused', 'Sample alert ' || n,
           'A sample notification for checking the drawer.', '/notifications', true,
           now() - (n || ' minutes')::interval,
           CASE WHEN n = 12 THEN now() ELSE NULL END
    FROM generate_series(1, 12) AS n
  `, [user.user_id]);
  await page.clock.install();
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto('/notifications');
  const bell = page.locator('#notification-bell');
  await expect(bell).toHaveAttribute('aria-label', '9 or more unread notifications');
  await expect(bell.locator('.notification-badge')).toHaveText('9+');
  await page.screenshot({ path: 'test-results/notifications-mobile-nav-light.png' });
  await bell.focus();
  await page.keyboard.press('Shift+Tab');
  await page.keyboard.press('Tab');
  await expect(bell).toBeFocused();
  await page.keyboard.press('Enter');
  const dialog = page.locator('#notification-drawer');
  await expect(dialog).toBeVisible();
  await expect(dialog.locator('.notification-item')).toHaveCount(12);
  await expect(page.locator('#notification-drawer-close')).toBeFocused();
  for (const theme of ['light', 'dark']) {
    await query('UPDATE users SET site_theme = $1 WHERE user_id = $2', [theme, user.user_id]);
    await page.reload();
    await bell.click();
    await expect(dialog.locator('.notification-item')).toHaveCount(12);
    for (const [label, width, height] of [['mobile', 375, 812], ['desktop', 1440, 900]] as const) {
      await page.setViewportSize({ width, height });
      await expect.poll(async () => Math.round((await dialog.boundingBox())?.x ?? -1))
        .toBe(label === 'mobile' ? 0 : 1000);
      await page.screenshot({ path: `test-results/notifications-${label}-${theme}.png` });
    }
  }
  await page.keyboard.press('Escape');
  await expect(dialog).not.toBeVisible();
  await expect(bell).toBeFocused();
  await page.setViewportSize({ width: 375, height: 812 });
  for (const selector of ['#notification-bell', '#notification-drawer-close', '.notification-item button', '.notification-mark-all button']) {
    if (selector !== '#notification-bell') await bell.click();
    const box = await page.locator(selector).first().boundingBox();
    expect(box).not.toBeNull();
    expect(box!.height).toBeGreaterThanOrEqual(44);
    if (selector !== '#notification-bell') await page.keyboard.press('Escape');
  }
  await bell.click();
  await dialog.locator('.notification-mark-all button').click();
  await expect(bell).toHaveAttribute('aria-label', '0 unread notifications');
  await page.keyboard.press('Escape');
  await query(`INSERT INTO notifications (user_id, kind, title, body, link, in_app_visible)
               VALUES ($1, 'snake_matchmaking_paused', 'Fresh alert', 'Now', '/notifications', true)`, [user.user_id]);
  await page.clock.fastForward(60_000);
  await expect(bell).toHaveAttribute('aria-label', '1 unread notifications');
  let countRequests = 0;
  page.on('request', request => { if (request.url().includes('/notifications/count')) countRequests++; });
  await page.evaluate(() => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'hidden' });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  const before = countRequests;
  await page.clock.fastForward(120_000);
  expect(countRequests).toBe(before);
  await page.evaluate(() => {
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => 'visible' });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect.poll(() => countRequests).toBeGreaterThan(before);
});
