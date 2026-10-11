import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

const ACHIEVEMENTS = '/customizations/achievements';
const NAMES = ['First Win', 'Crowd Pleaser', 'Constrictor Champ', 'Slow and Steady', 'Ladder Regular'];

test('guest ladder fits a phone and tabs navigate', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await page.goto(ACHIEVEMENTS);
  await page.evaluate(() => document.fonts.ready);
  await expect(page.getByRole('heading', { name: 'Achievements', level: 1 })).toBeVisible();
  const ladder = page.locator('ol.cz-ladder');
  const steps = ladder.locator('li.cz-step');
  await expect(steps).toHaveCount(5);
  for (let index = 0; index < NAMES.length; index++) {
    await expect(steps.nth(index).getByRole('heading', { name: NAMES[index] })).toBeVisible();
    await expect(steps.nth(index).locator('.cz-step-state')).toHaveText('Locked');
  }
  const tabs = page.getByRole('navigation', { name: 'Customizations' });
  await expect(tabs.getByRole('link', { name: 'Achievements' })).toHaveAttribute('aria-current', 'page');
  await expect(tabs.getByRole('link', { name: 'Catalog' })).not.toHaveAttribute('aria-current');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);
  for (const link of await tabs.getByRole('link').all()) {
    expect((await link.boundingBox())?.height).toBeGreaterThanOrEqual(44);
  }
  for (const selector of ['body', '.cz-step-description', '.cz-step-reward']) {
    expect(await page.locator(selector).first().evaluate(el => parseFloat(getComputedStyle(el).fontSize))).toBeGreaterThanOrEqual(16);
  }
  for (const selector of ['.cz-ladder', '.cz-step-card', '.cz-step-reward']) {
    for (const element of await page.locator(selector).all()) {
      const dimensions = await element.evaluate(el => ({
        right: el.getBoundingClientRect().right,
        scrollWidth: el.scrollWidth,
        clientWidth: el.clientWidth,
      }));
      expect(dimensions.right).toBeLessThanOrEqual(375);
      expect(dimensions.scrollWidth).toBeLessThanOrEqual(dimensions.clientWidth);
    }
  }
  const footer = page.locator('.cz-ladder-foot');
  await expect(footer.getByRole('link', { name: 'Browse the full catalog' })).toBeVisible();
  const footerText = await footer.locator('p').boundingBox();
  const catalogLink = await footer.getByRole('link').boundingBox();
  expect(catalogLink!.y).toBeGreaterThanOrEqual(footerText!.y + footerText!.height);
  await tabs.getByRole('link', { name: 'Catalog' }).click();
  await expect(page).toHaveURL(/\/customizations$/);
  await expect(page.getByRole('navigation', { name: 'Customizations' }).getByRole('link', { name: 'Catalog' })).toHaveAttribute('aria-current', 'page');
  await page.getByRole('link', { name: 'See the achievement ladder' }).click();
  await expect(page).toHaveURL(new RegExp(`${ACHIEVEMENTS}$`));
  await expect(page.locator('.links a[href="/customizations"]')).toHaveClass(/active/);
});

test('desktop ladder has a connector rail', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(ACHIEVEMENTS);
  await expect(page.getByRole('heading', { name: 'Achievements', level: 1 })).toBeVisible();
  const rail = await page.locator('.cz-step').first().evaluate(el => {
    const style = getComputedStyle(el, '::before');
    return { border: parseFloat(style.borderLeftWidth), height: parseFloat(style.height) };
  });
  expect(rail.border).toBeGreaterThan(0);
  expect(rail.height).toBeGreaterThan(0);
});

test('signed-in Frog reward unlocks only the first step', async ({ authenticatedPage, mockUser }) => {
  const [user] = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [mockUser.login]);
  await query("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'frog', 'achievement')", [user.user_id]);
  await authenticatedPage.goto(ACHIEVEMENTS);
  const steps = authenticatedPage.locator('ol.cz-ladder li.cz-step');
  await expect(steps.first().locator('.cz-step-state')).toHaveText('Unlocked');
  await expect(steps.nth(1).locator('.cz-step-state')).toHaveText('Locked');
  await expect(steps.nth(1).locator('.cz-next-up')).toHaveText('Next up');
  await expect(steps.locator('.cz-next-up')).toHaveCount(1);
});
