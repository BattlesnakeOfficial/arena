import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Games API', () => {
  test.describe('POST /api/games - Create Game', () => {
    test('can create a game with valid snakes via API', async ({ authenticatedPage }) => {
      const snakeName = `API Snake ${Date.now()}`;

      // Create a battlesnake first via UI
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/api-snake');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      // Get the snake ID from database
      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      expect(snakes.length).toBe(1);
      const snakeId = snakes[0].battlesnake_id;

      // Create game via API
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [snakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(201);
      const game = await response.json();
      expect(game.id).toBeDefined();
      expect(game.status).toBe('waiting');
    });

    test('can create game with multiple snakes', async ({ authenticatedPage }) => {
      const timestamp = Date.now();
      const snakeNames = [`Multi API 1 ${timestamp}`, `Multi API 2 ${timestamp}`];
      const snakeIds: string[] = [];

      // Create battlesnakes
      for (const name of snakeNames) {
        await authenticatedPage.goto('/battlesnakes/new');
        await authenticatedPage.getByLabel('Name').fill(name);
        await authenticatedPage.getByLabel('URL').fill(`https://example.com/${name.replace(/\s+/g, '-')}`);
        await authenticatedPage.getByLabel('Visibility').selectOption('public');
        await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

        const snakes = await query<{ battlesnake_id: string }>(
          "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
          [name]
        );
        snakeIds.push(snakes[0].battlesnake_id);
      }

      // Create game via API with both snakes
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: snakeIds,
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(201);
      const game = await response.json();
      expect(game.id).toBeDefined();
    });

    test('can create game with different board sizes', async ({ authenticatedPage }) => {
      const snakeName = `Board Size API ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/board-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Test each board size
      for (const boardSize of ['7x7', '11x11', '19x19']) {
        const response = await authenticatedPage.request.post('/api/games', {
          data: {
            snakes: [snakeId],
            board: boardSize,
            game_type: 'standard'
          }
        });

        expect(response.status()).toBe(201);
        const game = await response.json();
        expect(game.id).toBeDefined();
      }
    });

    test('can create game with different game types', async ({ authenticatedPage }) => {
      const snakeName = `Game Type API ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/gametype-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Test each game type (case-insensitive)
      for (const gameType of ['standard', 'ROYALE', 'Constrictor', 'snail', 'SOLO']) {
        const response = await authenticatedPage.request.post('/api/games', {
          data: {
            snakes: [snakeId],
            board: '11x11',
            game_type: gameType
          }
        });

        expect(response.status()).toBe(201);
        const game = await response.json();
        expect(game.id).toBeDefined();
      }
    });

    test('rejects solo game with zero or two snakes', async ({ authenticatedPage }) => {
      const timestamp = Date.now();
      const snakeNames = [`Solo Reject 1 ${timestamp}`, `Solo Reject 2 ${timestamp}`];
      const snakeIds: string[] = [];

      for (const name of snakeNames) {
        await authenticatedPage.goto('/battlesnakes/new');
        await authenticatedPage.getByLabel('Name').fill(name);
        await authenticatedPage.getByLabel('URL').fill('https://example.com/solo-reject');
        await authenticatedPage.getByLabel('Visibility').selectOption('public');
        await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

        const snakes = await query<{ battlesnake_id: string }>(
          "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
          [name]
        );
        snakeIds.push(snakes[0].battlesnake_id);
      }

      // Zero snakes: the shared empty-selection rule applies.
      const zeroResponse = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [],
          board: '11x11',
          game_type: 'solo'
        }
      });
      expect(zeroResponse.status()).toBe(400);
      expect(await zeroResponse.text()).toContain('At least one battlesnake is required for a game');

      // Two snakes: Solo requires exactly one.
      const twoResponse = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: snakeIds,
          board: '11x11',
          game_type: 'solo'
        }
      });
      expect(twoResponse.status()).toBe(400);
      expect(await twoResponse.text()).toContain('Solo games require exactly one battlesnake');
    });

    test('rejects game with no snakes', async ({ authenticatedPage }) => {
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(400);
      const body = await response.text();
      expect(body).toContain('At least one battlesnake is required for a game');
    });

    test('rejects game with more than 4 snakes', async ({ authenticatedPage }) => {
      const timestamp = Date.now();
      const snakeIds: string[] = [];

      // Create 5 battlesnakes
      for (let i = 0; i < 5; i++) {
        const name = `Too Many ${i} ${timestamp}`;
        await authenticatedPage.goto('/battlesnakes/new');
        await authenticatedPage.getByLabel('Name').fill(name);
        await authenticatedPage.getByLabel('URL').fill(`https://example.com/too-many-${i}`);
        await authenticatedPage.getByLabel('Visibility').selectOption('public');
        await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

        const snakes = await query<{ battlesnake_id: string }>(
          "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
          [name]
        );
        snakeIds.push(snakes[0].battlesnake_id);
      }

      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: snakeIds,
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(400);
      const body = await response.text();
      expect(body).toContain('A maximum of 4 battlesnakes are allowed in a game');
    });

    test('allows game with duplicate snake IDs (same snake multiple times)', async ({ authenticatedPage }) => {
      const snakeName = `Duplicate API ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/duplicate-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Create a game with the same snake twice
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [snakeId, snakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(201);
      const body = await response.json();
      expect(body.id).toBeDefined();
      expect(body.status).toBe('waiting');

      // Verify the game has 2 entries for the same snake
      const gameSnakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM game_battlesnakes WHERE game_id = $1",
        [body.id]
      );
      expect(gameSnakes.length).toBe(2);
      expect(gameSnakes[0].battlesnake_id).toBe(snakeId);
      expect(gameSnakes[1].battlesnake_id).toBe(snakeId);
    });

    test('allows game with same snake 4 times', async ({ authenticatedPage }) => {
      const snakeName = `Quad Snake ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/quad-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Create a game with the same snake 4 times
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [snakeId, snakeId, snakeId, snakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(201);
      const body = await response.json();
      expect(body.id).toBeDefined();

      // Verify the game has 4 entries for the same snake
      const gameSnakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM game_battlesnakes WHERE game_id = $1",
        [body.id]
      );
      expect(gameSnakes.length).toBe(4);
    });

    test('rejects game with invalid snake ID', async ({ authenticatedPage }) => {
      const fakeSnakeId = '00000000-0000-0000-0000-000000000000';

      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [fakeSnakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(400);
      const body = await response.text();
      expect(body).toContain('not found or not accessible');
    });

    test('rejects game with invalid board size', async ({ authenticatedPage }) => {
      const snakeName = `Invalid Board ${Date.now()}`;

      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/invalid-board');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [snakeId],
          board: '10x10',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(400);
      const body = await response.text();
      expect(body).toContain('Invalid board size');
    });

    test('rejects game with invalid game type', async ({ authenticatedPage }) => {
      const snakeName = `Invalid Type ${Date.now()}`;

      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/invalid-type');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [snakeId],
          board: '11x11',
          game_type: 'invalid_type'
        }
      });

      expect(response.status()).toBe(400);
      const body = await response.text();
      expect(body).toContain('Invalid game type');
    });

    test('can use public snake from another user', async ({ authenticatedPage, loginAsUser }) => {
      const publicSnakeName = `Public API Snake ${Date.now()}`;

      // First user creates a public snake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(publicSnakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/public-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [publicSnakeName]
      );
      const publicSnakeId = snakes[0].battlesnake_id;

      // Logout and login as second user
      await authenticatedPage.goto('/auth/logout');
      const secondUser = createMockUser('api_user2');
      await loginAsUser(authenticatedPage, secondUser);

      // Second user can use the public snake
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [publicSnakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(201);
    });

    test('cannot use private snake from another user', async ({ authenticatedPage, loginAsUser }) => {
      const privateSnakeName = `Private API Snake ${Date.now()}`;

      // First user creates a private snake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(privateSnakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/private-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('private');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [privateSnakeName]
      );
      const privateSnakeId = snakes[0].battlesnake_id;

      // Logout and login as second user
      await authenticatedPage.goto('/auth/logout');
      const secondUser = createMockUser('api_user3');
      await loginAsUser(authenticatedPage, secondUser);

      // Second user cannot use the private snake
      const response = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [privateSnakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(400);
      const body = await response.text();
      expect(body).toContain('not found or not accessible');
    });
  });

  test.describe('GET /api/games - List Games', () => {
    test('lists games where user has a snake', async ({ authenticatedPage }) => {
      const snakeName = `List API Snake ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/list-api');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Create a game
      const createResponse = await authenticatedPage.request.post('/api/games', {
        data: {
          snakes: [snakeId],
          board: '11x11',
          game_type: 'standard'
        }
      });
      expect(createResponse.status()).toBe(201);
      const createdGame = await createResponse.json();

      // List games
      const listResponse = await authenticatedPage.request.get('/api/games');
      expect(listResponse.status()).toBe(200);
      const games = await listResponse.json();

      expect(Array.isArray(games)).toBe(true);
      const gameIds = games.map((g: { id: string }) => g.id);
      expect(gameIds).toContain(createdGame.id);
    });

    test('can filter games by snake_id', async ({ authenticatedPage }) => {
      const timestamp = Date.now();
      const snake1Name = `Filter Snake 1 ${timestamp}`;
      const snake2Name = `Filter Snake 2 ${timestamp}`;

      // Create two battlesnakes
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snake1Name);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/filter-1');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snake2Name);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/filter-2');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const allSnakes = await query<{ battlesnake_id: string; name: string }>(
        "SELECT battlesnake_id, name FROM battlesnakes WHERE name IN ($1, $2)",
        [snake1Name, snake2Name]
      );
      const snake1 = allSnakes.find(s => s.name === snake1Name)!;
      const snake2 = allSnakes.find(s => s.name === snake2Name)!;

      // Create games for each snake
      await authenticatedPage.request.post('/api/games', {
        data: { snakes: [snake1.battlesnake_id], board: '11x11', game_type: 'standard' }
      });
      await authenticatedPage.request.post('/api/games', {
        data: { snakes: [snake2.battlesnake_id], board: '11x11', game_type: 'standard' }
      });

      // Filter by snake1
      const response = await authenticatedPage.request.get(`/api/games?snake_id=${snake1.battlesnake_id}`);
      expect(response.status()).toBe(200);
      const games = await response.json();

      // All returned games should include snake1
      for (const game of games) {
        const snakeIds = game.snakes.map((s: { id: string }) => s.id);
        expect(snakeIds).toContain(snake1.battlesnake_id);
      }
    });

    test('respects limit parameter', async ({ authenticatedPage }) => {
      const snakeName = `Limit Snake ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/limit');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Create multiple games
      for (let i = 0; i < 5; i++) {
        await authenticatedPage.request.post('/api/games', {
          data: { snakes: [snakeId], board: '11x11', game_type: 'standard' }
        });
      }

      // Request with limit
      const response = await authenticatedPage.request.get('/api/games?limit=2');
      expect(response.status()).toBe(200);
      const games = await response.json();
      expect(games.length).toBeLessThanOrEqual(2);
    });

    test('returns empty array when user has no games', async ({ authenticatedPage }) => {
      // Just list games (new user has no snakes/games)
      const response = await authenticatedPage.request.get('/api/games');
      expect(response.status()).toBe(200);
      const games = await response.json();
      expect(Array.isArray(games)).toBe(true);
    });
  });

  test.describe('GET /api/games/{id}/details - Show Game', () => {
    test('returns game details with frames', async ({ authenticatedPage }) => {
      const snakeName = `Details Snake ${Date.now()}`;

      // Create a battlesnake
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/details');
      await authenticatedPage.getByLabel('Visibility').selectOption('public');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      // Create a game
      const createResponse = await authenticatedPage.request.post('/api/games', {
        data: { snakes: [snakeId], board: '11x11', game_type: 'standard' }
      });
      const createdGame = await createResponse.json();

      // Get game details
      const response = await authenticatedPage.request.get(`/api/games/${createdGame.id}/details`);
      expect(response.status()).toBe(200);
      const game = await response.json();

      // Verify structure
      expect(game.id).toBe(createdGame.id);
      expect(game.status).toBeDefined();
      expect(game.board).toBe('11x11');
      expect(game.game_type).toBe('Standard');
      expect(Array.isArray(game.snakes)).toBe(true);
      expect(Array.isArray(game.frames)).toBe(true);
      expect(game.created_at).toBeDefined();

      // Verify snake info
      expect(game.snakes.length).toBe(1);
      expect(game.snakes[0].id).toBe(snakeId);
      expect(game.snakes[0].name).toBe(snakeName);
    });

    test('returns 404 for non-existent game', async ({ authenticatedPage }) => {
      const fakeGameId = '00000000-0000-0000-0000-000000000000';

      const response = await authenticatedPage.request.get(`/api/games/${fakeGameId}/details`);
      expect(response.status()).toBe(404);
    });

    test('any authenticated user can view any game', async ({ authenticatedPage, loginAsUser }) => {
      const snakeName = `View Any Snake ${Date.now()}`;

      // First user creates a snake and game
      await authenticatedPage.goto('/battlesnakes/new');
      await authenticatedPage.getByLabel('Name').fill(snakeName);
      await authenticatedPage.getByLabel('URL').fill('https://example.com/view-any');
      await authenticatedPage.getByLabel('Visibility').selectOption('private');
      await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

      const snakes = await query<{ battlesnake_id: string }>(
        "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
        [snakeName]
      );
      const snakeId = snakes[0].battlesnake_id;

      const createResponse = await authenticatedPage.request.post('/api/games', {
        data: { snakes: [snakeId], board: '11x11', game_type: 'standard' }
      });
      const createdGame = await createResponse.json();

      // Logout and login as second user
      await authenticatedPage.goto('/auth/logout');
      const secondUser = createMockUser('viewer_user');
      await loginAsUser(authenticatedPage, secondUser);

      // Second user can view the game (games are viewable by anyone authenticated)
      const response = await authenticatedPage.request.get(`/api/games/${createdGame.id}/details`);
      expect(response.status()).toBe(200);
      const game = await response.json();
      expect(game.id).toBe(createdGame.id);
    });
  });

  test.describe('Authentication', () => {
    test('requires authentication for create game', async ({ page }) => {
      // Make request without authentication
      const response = await page.request.post('/api/games', {
        data: {
          snakes: ['00000000-0000-0000-0000-000000000000'],
          board: '11x11',
          game_type: 'standard'
        }
      });

      expect(response.status()).toBe(401);
    });

    test('requires authentication for list games', async ({ page }) => {
      const response = await page.request.get('/api/games');
      expect(response.status()).toBe(401);
    });

    test('requires authentication for show game', async ({ page }) => {
      const response = await page.request.get('/api/games/00000000-0000-0000-0000-000000000000/details');
      expect(response.status()).toBe(401);
    });
  });
});
