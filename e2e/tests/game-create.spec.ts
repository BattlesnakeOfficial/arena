import { test, expect, createMockUser } from '../fixtures/test';

test.describe('Create Game', () => {
  test('can create a game with one battlesnake', async ({ authenticatedPage }) => {
    const snakeName = `Single Snake ${Date.now()}`;

    // Create a battlesnake first
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/single');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Start game creation
    await authenticatedPage.goto('/games/new');
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);
    await expect(authenticatedPage.getByRole('heading', { name: 'Create New Game' })).toBeVisible();

    // Add the battlesnake
    const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();

    // Create the game
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Should redirect to game details with success message
    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);
    await expect(authenticatedPage.getByRole('heading', { name: 'Game Details' })).toBeVisible();

    // Should see the snake in the results
    await expect(authenticatedPage.getByText(snakeName)).toBeVisible();
  });

  test('can create a game with multiple battlesnakes', async ({ authenticatedPage }) => {
    const snake1 = `Multi Snake 1 ${Date.now()}`;
    const snake2 = `Multi Snake 2 ${Date.now()}`;

    // Create first battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snake1);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/multi1');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Create second battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snake2);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/multi2');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Start game creation
    await authenticatedPage.goto('/games/new');

    // Add both battlesnakes
    const snake1Card = authenticatedPage.locator('.card', { hasText: snake1 });
    await snake1Card.getByRole('button', { name: 'Add to Game' }).click();

    const snake2Card = authenticatedPage.locator('.card', { hasText: snake2 });
    await snake2Card.getByRole('button', { name: 'Add to Game' }).click();

    // Verify both are selected
    await expect(authenticatedPage.getByText('You have selected 2 of 4 possible battlesnakes.')).toBeVisible();

    // Create the game
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Should redirect to game details
    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);

    // Both snakes should be in the results
    await expect(authenticatedPage.getByText(snake1)).toBeVisible();
    await expect(authenticatedPage.getByText(snake2)).toBeVisible();
  });

  test('can create game with maximum 4 battlesnakes', async ({ authenticatedPage }) => {
    const timestamp = Date.now();
    const snakeNames = [
      `Max Snake 1 ${timestamp}`,
      `Max Snake 2 ${timestamp}`,
      `Max Snake 3 ${timestamp}`,
      `Max Snake 4 ${timestamp}`,
    ];

    // Create 4 battlesnakes
    for (const name of snakeNames) {
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(name);
      await authenticatedPage.getByLabel('URL').fill(`https://example.com/${name.replace(/\s+/g, '-')}`);
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    }

    // Start game creation
    await authenticatedPage.goto('/games/new');

    // Add all 4 battlesnakes
    for (const name of snakeNames) {
      const snakeCard = authenticatedPage.locator('.card', { hasText: name });
      await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    }

    // Verify all 4 are selected
    await expect(authenticatedPage.getByText('You have selected 4 of 4 possible battlesnakes.')).toBeVisible();

    // Create the game
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Should redirect to game details
    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);

    // All snakes should be in the results
    for (const name of snakeNames) {
      await expect(authenticatedPage.getByText(name)).toBeVisible();
    }
  });

  test('can select different board sizes', async ({ authenticatedPage }) => {
    const snakeName = `Board Size Snake ${Date.now()}`;

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/board-size');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Test Small board
    await authenticatedPage.goto('/games/new');
    let snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    await authenticatedPage.getByLabel('Board Size').selectOption('7x7');
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();
    await expect(authenticatedPage.locator('.gmeta').getByText('7x7', { exact: true })).toBeVisible();

    // Test Large board
    await authenticatedPage.goto('/games/new');
    snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    await authenticatedPage.getByLabel('Board Size').selectOption('19x19');
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();
    await expect(authenticatedPage.locator('.gmeta').getByText('19x19', { exact: true })).toBeVisible();
  });

  test('can select different game types', async ({ authenticatedPage }) => {
    const snakeName = `Game Type Snake ${Date.now()}`;

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/game-type');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Test each game type
    const gameTypes = ['Standard', 'Royale', 'Constrictor', 'Snail Mode'];

    for (const gameType of gameTypes) {
      await authenticatedPage.goto('/games/new');
      const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
      await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
      await authenticatedPage.getByLabel('Game Type').selectOption(gameType);
      await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();
      await expect(authenticatedPage.locator('.gmeta').getByText(gameType, { exact: true })).toBeVisible();
    }
  });

  test('shows warning when user has no battlesnakes', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/games/new');
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);

    // Should show warning about no battlesnakes
    await expect(authenticatedPage.getByText("You don't have any battlesnakes yet.")).toBeVisible();
    await expect(authenticatedPage.getByRole('link', { name: 'Create a Battlesnake' })).toBeVisible();
  });

  test('shows message to select at least one battlesnake', async ({ authenticatedPage }) => {
    // Create a battlesnake so the empty state doesn't show
    const snakeName = `Select Warning Snake ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/select-warning');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Go to game flow without selecting any snakes
    await authenticatedPage.goto('/games/new');

    // Should show message to select at least one
    await expect(authenticatedPage.getByText('Please select at least one battlesnake to create a game.')).toBeVisible();

    // Create Game button should not be visible when no snakes selected
    await expect(authenticatedPage.getByRole('button', { name: 'Create Game' })).not.toBeVisible();
  });

  test('new_game redirects to flow page', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/games/new');

    // Should redirect to a flow page with UUID
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\/[0-9a-f-]+$/);
  });

  test('can use public battlesnakes from other users', async ({ authenticatedPage, loginAsUser }) => {
    const publicSnakeName = `Public Snake ${Date.now()}`;

    // First user creates a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(publicSnakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/public-snake');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Logout first user
    await authenticatedPage.goto('/auth/logout');

    // Login as second user
    const secondUser = createMockUser('user2');
    await loginAsUser(authenticatedPage, secondUser);

    // Create own snake so we can create a game
    const ownSnakeName = `Own Snake ${Date.now()}`;
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(ownSnakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/own-snake');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Start game creation
    await authenticatedPage.goto('/games/new');

    // Search for the public snake from first user
    await authenticatedPage.getByPlaceholder('Search by name...').fill(publicSnakeName);
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // Should see search results
    await expect(authenticatedPage.getByRole('heading', { name: 'Search Results' })).toBeVisible();
    await expect(authenticatedPage.getByText(publicSnakeName)).toBeVisible();

    // Add the public snake
    const searchResultCard = authenticatedPage.locator('.card', { hasText: publicSnakeName });
    await searchResultCard.getByRole('button', { name: 'Add to Game' }).click();

    // Add own snake
    const ownSnakeCard = authenticatedPage.locator('.card', { hasText: ownSnakeName }).first();
    await ownSnakeCard.getByRole('button', { name: 'Add to Game' }).click();

    // Verify both are selected
    await expect(authenticatedPage.getByText('You have selected 2 of 4 possible battlesnakes.')).toBeVisible();

    // Create the game
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Should see both snakes in results
    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);
    await expect(authenticatedPage.getByText(publicSnakeName)).toBeVisible();
    await expect(authenticatedPage.getByText(ownSnakeName)).toBeVisible();
  });

  test('can remove a battlesnake from selection using card button', async ({ authenticatedPage }) => {
    const snakeName = `Remove Test Snake ${Date.now()}`;

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/remove-test');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Start game creation
    await authenticatedPage.goto('/games/new');
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);

    // Add the battlesnake
    const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName }).first();
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();

    // Verify snake is selected
    await expect(authenticatedPage.getByText('You have selected 1 of 4 possible battlesnakes.')).toBeVisible();

    // The card should now show "Remove" button instead of "Add to Game"
    await expect(snakeCard.getByRole('button', { name: 'Remove' })).toBeVisible();

    // Click Remove on the card
    await snakeCard.getByRole('button', { name: 'Remove' }).click();

    // The card should now show "Add to Game" again
    await expect(snakeCard.getByRole('button', { name: 'Add to Game' })).toBeVisible();

    // Create Game button should not be visible since no snakes are selected
    await expect(authenticatedPage.getByRole('button', { name: 'Create Game' })).not.toBeVisible();
  });

  test('can reset all battlesnake selections', async ({ authenticatedPage }) => {
    const timestamp = Date.now();
    const snake1 = `Reset Test 1 ${timestamp}`;
    const snake2 = `Reset Test 2 ${timestamp}`;

    // Create two battlesnakes
    for (const name of [snake1, snake2]) {
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(name);
      await authenticatedPage.getByLabel('URL').fill(`https://example.com/${name.replace(/\s+/g, '-')}`);
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    }

    // Start game creation
    await authenticatedPage.goto('/games/new');

    // Add both battlesnakes
    const snake1Card = authenticatedPage.locator('.card', { hasText: snake1 });
    await snake1Card.getByRole('button', { name: 'Add to Game' }).click();
    const snake2Card = authenticatedPage.locator('.card', { hasText: snake2 });
    await snake2Card.getByRole('button', { name: 'Add to Game' }).click();

    // Verify both are selected
    await expect(authenticatedPage.getByText('You have selected 2 of 4 possible battlesnakes.')).toBeVisible();

    // Click Reset Selection button
    await authenticatedPage.getByRole('button', { name: 'Reset Selection' }).click();

    // Verify selections are reset
    await expect(authenticatedPage.getByText('Please select at least one battlesnake to create a game.')).toBeVisible();
  });

  test('cannot add more than 4 battlesnakes', async ({ authenticatedPage }) => {
    const timestamp = Date.now();
    const snakeNames = [
      `Max Test 1 ${timestamp}`,
      `Max Test 2 ${timestamp}`,
      `Max Test 3 ${timestamp}`,
      `Max Test 4 ${timestamp}`,
      `Max Test 5 ${timestamp}`,
    ];

    // Create 5 battlesnakes
    for (const name of snakeNames) {
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(name);
      await authenticatedPage.getByLabel('URL').fill(`https://example.com/${name.replace(/\s+/g, '-')}`);
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    }

    // Start game creation
    await authenticatedPage.goto('/games/new');
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);

    // Add first 4 battlesnakes
    for (let i = 0; i < 4; i++) {
      const snakeCard = authenticatedPage.locator('.card', { hasText: snakeNames[i] }).first();
      await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
      // Wait for the selection to update
      await expect(authenticatedPage.getByText(`You have selected ${i + 1} of 4 possible battlesnakes.`)).toBeVisible();
    }

    // Verify all 4 are selected
    await expect(authenticatedPage.getByText('You have selected 4 of 4 possible battlesnakes.')).toBeVisible();

    // The 5th snake card should show "Max reached" (disabled) since we can't add more
    const fifthSnakeCard = authenticatedPage.locator('.card', { hasText: snakeNames[4] }).first();
    await expect(fifthSnakeCard.getByRole('button', { name: 'Max reached' })).toBeVisible();
    await expect(fifthSnakeCard.getByRole('button', { name: 'Max reached' })).toBeDisabled();

    // Should still have only 4 selected
    await expect(authenticatedPage.getByText('You have selected 4 of 4 possible battlesnakes.')).toBeVisible();
  });

  test('private battlesnakes from other users are not visible in search', async ({ authenticatedPage, loginAsUser }) => {
    const privateSnakeName = `Private Snake ${Date.now()}`;
    const publicSnakeName = `Public Snake ${Date.now()}`;

    // First user creates a private and public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(privateSnakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/private-snake');
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(publicSnakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/public-snake');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Logout first user
    await authenticatedPage.goto('/auth/logout');

    // Login as second user
    const secondUser = createMockUser('user2');
    await loginAsUser(authenticatedPage, secondUser);

    // Start game creation
    await authenticatedPage.goto('/games/new');

    // Search for the private snake
    await authenticatedPage.getByPlaceholder('Search by name...').fill(privateSnakeName);
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // Should NOT find the private snake
    await expect(authenticatedPage.getByText('No public battlesnakes found matching your search.')).toBeVisible();

    // Search for the public snake
    await authenticatedPage.getByPlaceholder('Search by name...').fill(publicSnakeName);
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // Should find the public snake
    await expect(authenticatedPage.getByRole('heading', { name: 'Search Results' })).toBeVisible();
    await expect(authenticatedPage.getByText(publicSnakeName)).toBeVisible();
  });

  test('navigates to game details after successful creation', async ({ authenticatedPage }) => {
    const snakeName = `Details Nav Snake ${Date.now()}`;

    // Create a battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/details-nav');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Start game creation
    await authenticatedPage.goto('/games/new');
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);

    // Add the battlesnake
    const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName }).first();
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();

    // Verify snake is selected
    await expect(authenticatedPage.getByText('You have selected 1 of 4 possible battlesnakes.')).toBeVisible();

    // Create the game
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Should redirect to game details page
    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);

    // Should see game details page content
    await expect(authenticatedPage.getByRole('heading', { name: 'Game Details' })).toBeVisible();
    await expect(authenticatedPage.getByRole('heading', { name: 'Game Results' })).toBeVisible();

    // Should see the snake in the results
    await expect(authenticatedPage.getByText(snakeName)).toBeVisible();
  });
});
