import { test, expect } from '../fixtures/test';

test.describe('Game Details', () => {
  test('game details page shows battlesnakes and placements', async ({ authenticatedPage }) => {
    // Create a battlesnake
    const snakeName = `Details Test Snake ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/details-test');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Create a game
    await authenticatedPage.goto('/games/new');
    const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Should be on game details page
    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);

    // Should show game results table
    await expect(authenticatedPage.getByRole('heading', { name: 'Game Results' })).toBeVisible();

    // Should show the snake name in the table
    await expect(authenticatedPage.getByText(snakeName)).toBeVisible();

    // Games now run asynchronously via job queue. Poll until the game completes
    // by reloading the page periodically until we see the placement badge.
    // The page shows "In Progress" while waiting and "1st Place" when done.
    // Job worker polls every 2 seconds in e2e tests (configured in playwright.config.ts).
    const maxAttempts = 15;
    let placementVisible = false;
    for (let attempt = 0; attempt < maxAttempts && !placementVisible; attempt++) {
      placementVisible = await authenticatedPage.getByText('1st Place').isVisible();
      if (!placementVisible) {
        await authenticatedPage.waitForTimeout(2000); // Wait 2 seconds between reloads
        await authenticatedPage.reload();
      }
    }
    await expect(authenticatedPage.getByText('1st Place')).toBeVisible();
  });

  test('game details shows board size and game type', async ({ authenticatedPage }) => {
    // Create a battlesnake
    const snakeName = `Config Test Snake ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/config-test');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Create a game with specific settings
    await authenticatedPage.goto('/games/new');
    const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    await authenticatedPage.getByLabel('Board Size').selectOption('7x7');
    await authenticatedPage.getByLabel('Game Type').selectOption('Constrictor');
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Verify details page shows correct config
    await expect(authenticatedPage.getByText('Board Size: 7x7')).toBeVisible();
    await expect(authenticatedPage.getByText('Game Type: Constrictor')).toBeVisible();
  });

});
