import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

async function seedOpponentCatalog(prefix: string, count: number) {
  const login = `${prefix}_${Date.now()}_${Math.floor(Math.random() * 10000)}`;
  const externalId = Date.now() * 1000 + Math.floor(Math.random() * 1000);
  const [owner] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, 'test-token') RETURNING user_id`,
    [externalId, login],
  );
  const snakes: { battlesnake_id: string; name: string }[] = [];
  for (let index = 0; index < count; index += 1) {
    const name = `${prefix} Snake ${index.toString().padStart(2, '0')}`;
    const [snake] = await query<{ battlesnake_id: string }>(
      `INSERT INTO battlesnakes (user_id, name, url, visibility)
       VALUES ($1, $2, $3, 'public') RETURNING battlesnake_id`,
      [owner.user_id, name, `https://example.com/${index}`],
    );
    snakes.push({ battlesnake_id: snake.battlesnake_id, name });
  }
  await query(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     VALUES ($1, $2, 'https://example.com/private', 'private')`,
    [owner.user_id, `${prefix} Hidden`],
  );
  return { ownerId: owner.user_id, login, snakes };
}

async function removeOpponentCatalog(ownerId: string) {
  await query('DELETE FROM battlesnakes WHERE user_id = $1', [ownerId]);
  await query('DELETE FROM users WHERE user_id = $1', [ownerId]);
}

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
    const builderUrl = authenticatedPage.url();

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

    // Completed builders can remain in browser history or another tab.
    const expired = await authenticatedPage.goto(builderUrl);
    expect(expired?.status()).toBe(404);
    await expect(authenticatedPage.getByRole('heading', { name: 'Game setup unavailable' })).toBeVisible();
    await authenticatedPage.getByRole('link', { name: 'Start a new game', exact: true }).click();
    await expect(authenticatedPage.getByRole('heading', { name: 'Create New Game' })).toBeVisible();
    expect(authenticatedPage.url()).not.toBe(builderUrl);

    // Submitting the old form also reaches recovery without creating a game.
    const stalePost = await authenticatedPage.request.post(`${builderUrl}/create`, {
      form: { board_size: '11x11', game_type: 'Standard' },
    });
    expect(stalePost.status()).toBe(404);
    expect(await stalePost.text()).toContain('Game setup unavailable');
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
    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('7x7');
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();
    await expect(authenticatedPage.locator('.gmeta').getByText('7x7', { exact: true })).toBeVisible();

    // Test Large board
    await authenticatedPage.goto('/games/new');
    snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
    await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('19x19');
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

    // Test each game type (Solo passes exactly-one validation since the
    // loop adds one snake per iteration)
    const gameTypes = ['Standard', 'Royale', 'Constrictor', 'Snail Mode', 'Solo'];

    for (const gameType of gameTypes) {
      await authenticatedPage.goto('/games/new');
      const snakeCard = authenticatedPage.locator('.card', { hasText: snakeName });
      await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
      await authenticatedPage.getByLabel('Game Type', { exact: true }).selectOption(gameType);
      await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();
      await expect(authenticatedPage.locator('.gmeta').getByText(gameType, { exact: true })).toBeVisible();
    }
  });

  test('solo game with two snakes is rejected, one succeeds', async ({ authenticatedPage }) => {
    const snakeNames = [`Solo Flow 1 ${Date.now()}`, `Solo Flow 2 ${Date.now()}`];

    // Create two battlesnakes
    for (const name of snakeNames) {
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(name);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/solo-flow');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();
    }

    // Select both, choose Solo, submit
    await authenticatedPage.goto('/games/new');
    for (const name of snakeNames) {
      const snakeCard = authenticatedPage.locator('.card', { hasText: name });
      await snakeCard.getByRole('button', { name: 'Add to Game' }).click();
    }
    await authenticatedPage.getByLabel('Game Type', { exact: true }).selectOption('Solo');
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    // Remains on the flow page with the validation flash (rendered once,
    // by the page shell above <main>)
    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);
    await expect(
      authenticatedPage.getByText('Solo games require exactly one battlesnake')
    ).toBeVisible();

    // Reduce selection to one snake and create successfully
    const removeCard = authenticatedPage.locator('.card', { hasText: snakeNames[1] });
    await removeCard.getByRole('button', { name: 'Remove' }).first().click();
    await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

    await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);
    await expect(authenticatedPage.getByRole('heading', { name: 'Game Details' })).toBeVisible();
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
    await authenticatedPage.getByLabel('Search public opponents').fill(publicSnakeName);
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // Should see search results
    await expect(authenticatedPage.getByText(/Showing 1–1 of 1 public opponents/)).toBeVisible();
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

  test('browses, searches, pages, clears, and adds public opponents in one flow', async ({ authenticatedPage }) => {
    const catalog = await seedOpponentCatalog('BuilderCatalog', 12);
    try {
      await authenticatedPage.goto('/games/new');
      const flowPath = new URL(authenticatedPage.url()).pathname;
      await expect(authenticatedPage.getByText(catalog.snakes[0].name, { exact: true })).toBeVisible();
      await expect(authenticatedPage.getByText(`${catalog.snakes[0].name.split(' Snake')[0]} Hidden`)).not.toBeVisible();
      await expect(authenticatedPage.getByRole('link', { name: catalog.login, exact: true }).first()).toBeVisible();
      await expect(authenticatedPage.getByText(/Showing 1–10 of 12 public opponents/)).toBeVisible();

      await authenticatedPage.getByRole('link', { name: 'Next ›' }).click();
      await expect(authenticatedPage).toHaveURL(/page=1/);
      await expect(authenticatedPage.getByText(catalog.snakes[10].name, { exact: true })).toBeVisible();
      expect(new URL(authenticatedPage.url()).pathname).toBe(flowPath);

      const pageTwoRow = authenticatedPage.locator('.gc-public-row', { hasText: catalog.snakes[10].name });
      await pageTwoRow.getByRole('button', { name: 'Add to Game' }).click();
      await expect(authenticatedPage).toHaveURL(/page=1/);
      await expect(pageTwoRow.getByText('In lineup')).toBeVisible();
      await pageTwoRow.getByRole('button', { name: 'Add to Game' }).click();
      await expect(pageTwoRow.getByText('In lineup ×2')).toBeVisible();

      await authenticatedPage.getByLabel('Search public opponents').fill(catalog.snakes[3].name.toLowerCase());
      await authenticatedPage.getByRole('button', { name: 'Search' }).click();
      await expect(authenticatedPage).toHaveURL(/q=/);
      await expect(authenticatedPage.getByText(catalog.snakes[3].name, { exact: true })).toBeVisible();
      expect(new URL(authenticatedPage.url()).pathname).toBe(flowPath);

      await authenticatedPage.getByLabel('Search public opponents').fill(catalog.login.toUpperCase());
      await authenticatedPage.getByRole('button', { name: 'Search' }).click();
      await expect(authenticatedPage.getByText(/Showing 1–10 of 12 public opponents/)).toBeVisible();
      await authenticatedPage.getByRole('link', { name: 'Clear' }).click();
      await expect.poll(() => {
        const url = new URL(authenticatedPage.url());
        return `${url.pathname}${url.search}`;
      }).toBe(flowPath);
      await expect(authenticatedPage.getByText('You have selected 2 of 4 possible battlesnakes.')).toBeVisible();

      await authenticatedPage.setViewportSize({ width: 375, height: 812 });
      const dimensions = await authenticatedPage.evaluate(() => ({
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
        inputSize: getComputedStyle(document.querySelector('#opponent-search')!).fontSize,
        searchHeight: (document.querySelector('.gc-search button') as HTMLElement).getBoundingClientRect().height,
      }));
      expect(dimensions.scrollWidth).toBeLessThanOrEqual(dimensions.clientWidth);
      expect(dimensions.inputSize).toBe('16px');
      expect(dimensions.searchHeight).toBeGreaterThanOrEqual(44);
    } finally {
      await removeOpponentCatalog(catalog.ownerId);
    }
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
    await authenticatedPage.getByLabel('Search public opponents').fill(privateSnakeName);
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // Should NOT find the private snake
    await expect(authenticatedPage.getByText('No public opponents match your search.')).toBeVisible();

    // Search for the public snake
    await authenticatedPage.getByLabel('Search public opponents').fill(publicSnakeName);
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // Should find the public snake
    await expect(authenticatedPage.getByText(/Showing 1–1 of 1 public opponents/)).toBeVisible();
    await expect(authenticatedPage.getByText(publicSnakeName)).toBeVisible();
  });

  test('keeps every selection when a public opponent becomes unavailable before create', async ({ authenticatedPage, mockUser }) => {
    const catalog = await seedOpponentCatalog('BecomesPrivate', 1);
    try {
      const ownName = `Owned private ${Date.now()}`;
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(ownName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/owned-private');
      await authenticatedPage.getByLabel('Visibility').selectOption('private');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      await authenticatedPage.goto('/games/new');
      const flowUrl = authenticatedPage.url();
      await authenticatedPage.locator('.card', { hasText: ownName }).getByRole('button', { name: 'Add to Game' }).click();
      let opponent = authenticatedPage.locator('.gc-public-row', { hasText: catalog.snakes[0].name });
      await opponent.getByRole('button', { name: 'Add to Game' }).click();
      opponent = authenticatedPage.locator('.gc-public-row', { hasText: catalog.snakes[0].name });
      await opponent.getByRole('button', { name: 'Add to Game' }).click();
      await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('19x19');
      await authenticatedPage.getByLabel('Game Type', { exact: true }).selectOption('Royale');
      await expect(authenticatedPage.getByText('You have selected 3 of 4 possible battlesnakes.')).toBeVisible();

      const [creator] = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [mockUser.login]);
      const [before] = await query<{ count: string }>('SELECT COUNT(*)::text AS count FROM games WHERE created_by_user_id = $1', [creator.user_id]);
      await query("UPDATE battlesnakes SET visibility = 'private' WHERE battlesnake_id = $1", [catalog.snakes[0].battlesnake_id]);
      await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();

      await expect(authenticatedPage).toHaveURL(flowUrl);
      await expect(authenticatedPage.getByText('One or more snakes became unavailable. Correct the lineup and try again.')).toBeVisible();
      await expect(authenticatedPage.locator('.gc-unavailable .badge')).toHaveText('×2');
      await expect(authenticatedPage.getByLabel('Board Size', { exact: true })).toHaveValue('19x19');
      await expect(authenticatedPage.getByLabel('Game Type', { exact: true })).toHaveValue('Royale');
      const [after] = await query<{ count: string }>('SELECT COUNT(*)::text AS count FROM games WHERE created_by_user_id = $1', [creator.user_id]);
      expect(after.count).toBe(before.count);

      await authenticatedPage.getByLabel('Remove one unavailable snake from lineup').click();
      await expect(authenticatedPage.locator('.gc-unavailable .badge')).toHaveText('×1');
      await authenticatedPage.getByLabel('Remove one unavailable snake from lineup').click();
      await expect(authenticatedPage.locator('.gc-unavailable')).toHaveCount(0);
      await authenticatedPage.getByRole('button', { name: 'Create Game' }).click();
      await expect(authenticatedPage).toHaveURL(/\/games\/[0-9a-f-]+$/);
    } finally {
      await removeOpponentCatalog(catalog.ownerId);
    }
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

  test('retains the first discovery action while configure is outstanding', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/games/new');
    const flowUrl = authenticatedPage.url();
    await authenticatedPage.goto(`${flowUrl}?q=old`);

    let releaseConfigure!: () => void;
    const configureReleased = new Promise<void>((resolve) => { releaseConfigure = resolve; });
    let configureStarted!: () => void;
    const configureRequest = new Promise<void>((resolve) => { configureStarted = resolve; });
    const payloads: URLSearchParams[] = [];
    let outstanding = 0;
    let maxOutstanding = 0;
    let released = false;
    await authenticatedPage.route('**/configure', async (route) => {
      outstanding += 1;
      maxOutstanding = Math.max(maxOutstanding, outstanding);
      payloads.push(new URLSearchParams(route.request().postData() || ''));
      configureStarted();
      if (!released) await configureReleased;
      await route.continue();
      outstanding -= 1;
    });

    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('19x19');
    await configureRequest;
    await authenticatedPage.getByLabel('Game Type', { exact: true }).selectOption('Royale');
    await authenticatedPage.getByLabel('Search public opponents').fill('new search');
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();

    // A second keyboard-activated action cannot bypass the retained search.
    await authenticatedPage.getByRole('link', { name: 'Clear' }).press('Enter');
    await expect(authenticatedPage).toHaveURL(/\?q=old$/);

    released = true;
    releaseConfigure();
    await expect(authenticatedPage).toHaveURL(/\?q=new(?:\+|%20)search$/);
    await expect(authenticatedPage.getByLabel('Board Size', { exact: true })).toHaveValue('19x19');
    await expect(authenticatedPage.getByLabel('Game Type', { exact: true })).toHaveValue('Royale');
    expect(maxOutstanding).toBe(1);
    expect(payloads.map((body) => `${body.get('board_size')}/${body.get('game_type')}`)).toEqual([
      '19x19/Standard',
      '19x19/Royale',
    ]);
  });

  test('retry saves the newest snapshot and cancel drops only the retained action', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/games/new');
    const flowUrl = authenticatedPage.url();
    const flowId = new URL(flowUrl).pathname.split('/').pop()!;
    await authenticatedPage.goto(`${flowUrl}?q=old`);

    let releaseFailure!: () => void;
    let failing = true;
    const failureGate = new Promise<void>((resolve) => { releaseFailure = resolve; });
    const payloads: string[] = [];
    await authenticatedPage.route('**/configure', async (route) => {
      const body = new URLSearchParams(route.request().postData() || '');
      payloads.push(`${body.get('board_size')}/${body.get('game_type')}`);
      if (failing) {
        await failureGate;
        failing = false;
        await route.fulfill({ status: 500, body: 'nope' });
      } else {
        await route.continue();
      }
    });

    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('7x7');
    await authenticatedPage.getByLabel('Search public opponents').fill('retry target');
    await authenticatedPage.getByRole('button', { name: 'Search' }).click();
    releaseFailure();
    const alert = authenticatedPage.getByRole('alert');
    await expect(alert).toBeVisible();
    await expect(authenticatedPage).toHaveURL(/\?q=old$/);

    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('19x19');
    await authenticatedPage.getByLabel('Game Type', { exact: true }).selectOption('Royale');
    await alert.getByRole('button', { name: 'Retry' }).click();
    await expect(authenticatedPage).toHaveURL(/q=retry(?:\+|%20)target/);
    expect(payloads).toEqual(['7x7/Standard', '19x19/Royale']);
    let [persisted] = await query<{ board_size: string; game_type: string }>(
      'SELECT board_size, game_type FROM game_flows WHERE flow_id = $1', [flowId],
    );
    expect(persisted).toEqual({ board_size: '19x19', game_type: 'Royale' });

    let releaseCancelFailure!: () => void;
    const cancelGate = new Promise<void>((resolve) => { releaseCancelFailure = resolve; });
    let cancelFailure = true;
    await authenticatedPage.unroute('**/configure');
    await authenticatedPage.route('**/configure', async (route) => {
      if (cancelFailure) {
        await cancelGate;
        cancelFailure = false;
        await route.fulfill({ status: 500, body: 'nope' });
      } else {
        await route.continue();
      }
    });
    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('7x7');
    await authenticatedPage.getByRole('link', { name: 'Clear' }).click();
    releaseCancelFailure();
    await expect(alert).toBeVisible();
    await authenticatedPage.getByLabel('Game Type', { exact: true }).selectOption('Constrictor');
    await alert.getByRole('button', { name: 'Cancel' }).click();
    await expect(authenticatedPage).toHaveURL(/q=retry(?:\+|%20)target/);
    await expect.poll(async () => {
      const [row] = await query<{ board_size: string; game_type: string }>(
        'SELECT board_size, game_type FROM game_flows WHERE flow_id = $1', [flowId],
      );
      return `${row.board_size}/${row.game_type}`;
    }).toBe('7x7/Constrictor');
    persisted = (await query<{ board_size: string; game_type: string }>(
      'SELECT board_size, game_type FROM game_flows WHERE flow_id = $1', [flowId],
    ))[0];
    expect(persisted).toEqual({ board_size: '7x7', game_type: 'Constrictor' });
  });

  test('a stalled configure times out without running the retained action', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/games/new');
    const flowUrl = authenticatedPage.url();
    await authenticatedPage.goto(`${flowUrl}?q=keep-me`);
    await authenticatedPage.clock.install();

    let releaseStall!: () => void;
    const stall = new Promise<void>((resolve) => { releaseStall = resolve; });
    await authenticatedPage.route('**/configure', async (route) => {
      await stall;
      await route.abort().catch(() => {});
    });
    await authenticatedPage.getByLabel('Board Size', { exact: true }).selectOption('19x19');
    await authenticatedPage.getByRole('link', { name: 'Clear' }).click();
    await authenticatedPage.clock.fastForward(15_001);
    const alert = authenticatedPage.getByRole('alert');
    await expect(alert).toBeVisible();
    await expect(authenticatedPage).toHaveURL(/\?q=keep-me$/);

    await authenticatedPage.unroute('**/configure');
    await alert.getByRole('button', { name: 'Cancel' }).click();
    releaseStall();
    await expect(authenticatedPage).toHaveURL(/\?q=keep-me$/);
    await expect(alert).toBeHidden();
  });
});
