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
           CASE WHEN n = 2 THEN now() ELSE NULL END
    FROM generate_series(1, 12) AS n
  `, [user.user_id]);
  await page.clock.install();
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto('/notifications');
  const bell = page.locator('#notification-bell');
  await expect(bell).toHaveAttribute('aria-label', '9 or more unread notifications');
  await expect(bell.locator('.notification-badge')).toHaveText('9+');
  await page.screenshot({ path: 'test-results/notifications-mobile-nav-light.png' });
  const navWidth = 1221;
  await page.setViewportSize({ width: navWidth, height: 900 });
  const nav = await page.evaluate(() => ({
    linkTops: [...document.querySelectorAll('.site-nav .links a')]
      .filter(link => (link as HTMLElement).getBoundingClientRect().width > 0)
      .map(link => (link as HTMLElement).getBoundingClientRect().top),
    userRight: document.querySelector('.nav-user')!.getBoundingClientRect().right,
  }));
  console.log(`notification nav at ${navWidth}px: link tops ${JSON.stringify(nav.linkTops)}, user right ${nav.userRight}px`);
  await page.screenshot({ path: 'test-results/notifications-nav-breakpoint.png' });
  expect(new Set(nav.linkTops).size).toBe(1);
  expect(nav.userRight).toBeLessThanOrEqual(navWidth);
  await page.setViewportSize({ width: 375, height: 812 });
  const modifiedClicks: boolean[] = [];
  await bell.evaluate((node) => {
    (window as typeof window & { notificationClicks: boolean[] }).notificationClicks = [];
    node.addEventListener('click', event => {
      (window as typeof window & { notificationClicks: boolean[] }).notificationClicks.push(event.defaultPrevented);
      event.preventDefault();
    });
  });
  await bell.dispatchEvent('click', { button: 0, ctrlKey: true });
  modifiedClicks.push(...await page.evaluate(() => (window as typeof window & { notificationClicks: boolean[] }).notificationClicks));
  expect(modifiedClicks).toEqual([false]);
  await expect(page.locator('#notification-drawer')).not.toBeVisible();
  await bell.dispatchEvent('click', { button: 0 });
  expect(await page.evaluate(() => (window as typeof window & { notificationClicks: boolean[] }).notificationClicks)).toEqual([false, true]);
  await expect(page.locator('#notification-drawer')).toBeVisible();
  await page.keyboard.press('Escape');
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
    await expect.poll(async () => (await page.locator(selector).first().boundingBox())?.height ?? 0)
      .toBeGreaterThanOrEqual(44);
    if (selector !== '#notification-bell') await page.keyboard.press('Escape');
  }
  await bell.click();
  await dialog.locator('.notification-mark-all button').click();
  await expect(bell).toHaveAttribute('aria-label', '0 unread notifications');
  await page.keyboard.press('Escape');
  await query(`INSERT INTO notifications (user_id, kind, title, body, link, in_app_visible)
               VALUES ($1, 'snake_matchmaking_paused', 'Fresh alert', 'Now', '/notifications', true)`, [user.user_id]);
  await page.clock.fastForward(60_000);
  await expect(bell).toHaveAttribute('aria-label', '1 unread notification');
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

test('empty drawer at mobile width', async ({ authenticatedPage, mockUser }) => {
  const page = authenticatedPage;
  await query('UPDATE users SET site_theme = $1 WHERE github_login = $2', ['light', mockUser.login]);
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto('/notifications');
  await expect(page.locator('html')).toHaveAttribute('data-app-theme', 'light');
  await page.locator('#notification-bell').click();
  const drawer = page.locator('#notification-drawer');
  await expect(drawer.locator('.notification-empty')).toHaveText('No notifications yet.');
  await expect(drawer.locator('.notification-mark-all')).toHaveCount(0);
  await expect.poll(async () => Math.round((await drawer.boundingBox())?.x ?? -1)).toBe(0);
  await page.screenshot({ path: 'test-results/notifications-mobile-empty-light.png' });
});
