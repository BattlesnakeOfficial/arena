import { test, expect } from '../fixtures/test';

test.describe('Favicon', () => {
  test('/favicon.ico is served (no console 404 on every page)', async ({ page }) => {
    const response = await page.request.get('/favicon.ico');
    expect(response.status()).toBe(200);
    expect(response.headers()['content-type']).toContain('image/svg+xml');
  });

  test('pages declare an icon link in the head', async ({ page }) => {
    await page.goto('/');
    const iconHref = await page.locator('link[rel="icon"]').getAttribute('href');
    expect(iconHref).toContain('favicon.svg');
  });
});
