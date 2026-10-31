import { test, expect } from '../fixtures/test';

test.describe('Favicon', () => {
  test('/favicon.ico is the real ICO (no console 404 on every page)', async ({ page }) => {
    const response = await page.request.get('/favicon.ico');
    expect(response.status()).toBe(200);
    expect(response.headers()['content-type']).toBe('image/x-icon');
  });

  test('pages declare the official icon set in the head', async ({ page }) => {
    await page.goto('/');
    // The svg favicon is the official circle snake-head artwork.
    const svgLink = page.locator('link[rel="icon"][type="image/svg+xml"]');
    const svgHref = await svgLink.getAttribute('href');
    expect(svgHref).toMatch(/^\/assets\/favicon\.[0-9a-f]{8}\.svg$/);
    const svgResponse = await page.request.get(svgHref!);
    expect(svgResponse.status()).toBe(200);
    expect(await svgResponse.text()).toContain('M8,0A8,8');

    // ICO fallback for engines that don't pick the SVG.
    const icoHref = await page
      .locator('link[rel="icon"][sizes="32x32"]')
      .getAttribute('href');
    expect(icoHref).toMatch(/^\/assets\/favicon\.[0-9a-f]{8}\.ico$/);

    // Home-screen bookmark icon.
    const appleHref = await page
      .locator('link[rel="apple-touch-icon"]')
      .getAttribute('href');
    expect(appleHref).toMatch(/^\/assets\/apple-touch-icon\.[0-9a-f]{8}\.png$/);
    const appleResponse = await page.request.get(appleHref!);
    expect(appleResponse.status()).toBe(200);
    expect(appleResponse.headers()['content-type']).toBe('image/png');
  });
});
