import { test, expect } from '../fixtures/test';

test.describe('View transition scroll restoration', () => {
  test('cross-page navigation throws no uncaught JS errors', async ({ page }) => {
    // Regression: viewTransition.js read `navigation.activation.from.url`
    // without guarding for `from === null` (its value on a fresh load), which
    // threw "Cannot read properties of null (reading 'url')" into the console
    // on essentially every page. Fail the test on any uncaught page error.
    const pageErrors: string[] = [];
    page.on('pageerror', (err) => pageErrors.push(err.message));

    await page.goto('/');
    await page.goto('/tournaments');
    await page.goto('/leaderboards');

    expect(pageErrors).toEqual([]);
  });
});
