import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Leaderboard Pages', () => {
  test('leaderboard list page renders with seeded leaderboard', async ({ authenticatedPage }) => {
    await authenticatedPage.goto('/leaderboards');

    await expect(authenticatedPage.getByRole('heading', { name: 'Leaderboards' })).toBeVisible();
    // The seed migration creates "Standard 11x11"
    await expect(authenticatedPage.getByText('Standard 11x11')).toBeVisible();
    await expect(authenticatedPage.getByText('Active')).toBeVisible();
  });

  test('leaderboard detail page shows rankings and placement sections', async ({ authenticatedPage }) => {
    // Get the seeded leaderboard ID
    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    expect(leaderboards.length).toBe(1);
    const leaderboardId = leaderboards[0].leaderboard_id;

    await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);

    await expect(authenticatedPage.getByRole('heading', { name: /Leaderboard: Standard 11x11/ })).toBeVisible();
    await expect(authenticatedPage.getByRole('heading', { name: 'Rankings' })).toBeVisible();
    // With no participants, should show the minimum games message
    await expect(authenticatedPage.getByText(/Minimum: 10 games/)).toBeVisible();
  });

  test('can join a leaderboard with a public snake', async ({ authenticatedPage }) => {
    const snakeName = `LB Join Snake ${Date.now()}`;

    // Create a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/lb-join');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Get the seeded leaderboard
    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Visit leaderboard detail page
    await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);

    // Should see the "Your Snakes" section with the join form
    await expect(authenticatedPage.getByRole('heading', { name: 'Your Snakes' })).toBeVisible();

    // Select the snake and join
    await authenticatedPage.getByRole('button', { name: 'Join' }).click();

    // After joining, should see the snake listed as Active
    await expect(authenticatedPage.getByRole('cell', { name: snakeName })).toBeVisible();
    await expect(authenticatedPage.getByText('Active')).toBeVisible();
  });

  test('can pause and resume a snake in a leaderboard', async ({ authenticatedPage }) => {
    const snakeName = `LB Pause Snake ${Date.now()}`;

    // Create a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/lb-pause');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Get the seeded leaderboard
    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Join the leaderboard
    await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);
    await authenticatedPage.getByRole('button', { name: 'Join' }).click();
    await expect(authenticatedPage.getByRole('cell', { name: snakeName })).toBeVisible();

    // Pause the snake
    await authenticatedPage.getByRole('button', { name: 'Pause' }).click();
    await expect(authenticatedPage.getByText('Paused')).toBeVisible();

    // Resume the snake
    await authenticatedPage.getByRole('button', { name: 'Resume' }).click();
    await expect(authenticatedPage.getByText('Active')).toBeVisible();
  });

  test('private snakes cannot join leaderboard', async ({ authenticatedPage }) => {
    const snakeName = `LB Private Snake ${Date.now()}`;

    // Create a private battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/lb-private');
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Get the seeded leaderboard
    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Visit leaderboard detail page - private snake should not appear in the join dropdown
    await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);

    // The join form only shows public snakes, so the private snake name should NOT
    // appear as an option. The join button might not be visible at all if no public snakes exist.
    // We verify by checking the snake name is not in a select option.
    const selectOptions = authenticatedPage.locator('select[name="battlesnake_id"] option');
    const count = await selectOptions.count();
    for (let i = 0; i < count; i++) {
      const text = await selectOptions.nth(i).textContent();
      expect(text).not.toBe(snakeName);
    }
  });

  test('placement entries show games remaining', async ({ authenticatedPage }) => {
    const snakeName = `LB Placement Snake ${Date.now()}`;

    // Create a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/lb-placement');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Get leaderboard and snake IDs
    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Join via UI
    await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);
    await authenticatedPage.getByRole('button', { name: 'Join' }).click();

    // The snake should appear in the "In Placement" section (0 games played)
    await expect(authenticatedPage.getByRole('heading', { name: 'In Placement' })).toBeVisible();
    await expect(authenticatedPage.getByRole('cell', { name: snakeName })).toBeVisible();
    // Games remaining should be 10 (MIN_GAMES_FOR_RANKING - 0 games played)
    await expect(authenticatedPage.getByText('10')).toBeVisible();
  });
});

test.describe('Leaderboard API', () => {
  test('GET /api/leaderboards returns leaderboard list', async ({ authenticatedPage }) => {
    const response = await authenticatedPage.request.get('/api/leaderboards');
    expect(response.status()).toBe(200);

    const leaderboards = await response.json();
    expect(Array.isArray(leaderboards)).toBe(true);
    expect(leaderboards.length).toBeGreaterThanOrEqual(1);

    // The seeded leaderboard should be present
    const standard = leaderboards.find((lb: { name: string }) => lb.name === 'Standard 11x11');
    expect(standard).toBeDefined();
    expect(standard.active).toBe(true);
  });

  test('GET /api/leaderboards/:id/rankings returns rankings', async ({ authenticatedPage }) => {
    // Get the seeded leaderboard
    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    const response = await authenticatedPage.request.get(`/api/leaderboards/${leaderboardId}/rankings`);
    expect(response.status()).toBe(200);

    const data = await response.json();
    expect(data.leaderboard_id).toBe(leaderboardId);
    expect(data.leaderboard_name).toBe('Standard 11x11');
    expect(data.min_games).toBe(10);
    expect(Array.isArray(data.ranked)).toBe(true);
    expect(Array.isArray(data.placement)).toBe(true);
  });

  test('POST /api/leaderboards/:id/entries opts in a snake', async ({ authenticatedPage }) => {
    const snakeName = `API LB Snake ${Date.now()}`;

    // Create a public battlesnake via UI
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/api-lb');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    // Get snake and leaderboard IDs
    const snakes = await query<{ battlesnake_id: string }>(
      "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
      [snakeName]
    );
    const snakeId = snakes[0].battlesnake_id;

    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Opt-in via API
    const response = await authenticatedPage.request.post(`/api/leaderboards/${leaderboardId}/entries`, {
      data: { battlesnake_id: snakeId }
    });

    expect(response.status()).toBe(201);
    const entry = await response.json();
    expect(entry.battlesnake_id).toBe(snakeId);
    expect(entry.display_score).toBe(0.0);
    expect(entry.games_played).toBe(0);
    expect(entry.active).toBe(true);
  });

  test('DELETE /api/leaderboards/:id/entries/:battlesnake_id pauses a snake', async ({ authenticatedPage }) => {
    const snakeName = `API LB Delete Snake ${Date.now()}`;

    // Create a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/api-lb-delete');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const snakes = await query<{ battlesnake_id: string }>(
      "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
      [snakeName]
    );
    const snakeId = snakes[0].battlesnake_id;

    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Opt-in first
    const optInResponse = await authenticatedPage.request.post(`/api/leaderboards/${leaderboardId}/entries`, {
      data: { battlesnake_id: snakeId }
    });
    expect(optInResponse.status()).toBe(201);

    // Opt-out (pause) via DELETE
    const deleteResponse = await authenticatedPage.request.delete(
      `/api/leaderboards/${leaderboardId}/entries/${snakeId}`
    );
    expect(deleteResponse.status()).toBe(204);

    // Verify the entry is now disabled in the database
    const entries = await query<{ disabled_at: string | null }>(
      "SELECT disabled_at FROM leaderboard_entries WHERE leaderboard_id = $1 AND battlesnake_id = $2",
      [leaderboardId, snakeId]
    );
    expect(entries.length).toBe(1);
    expect(entries[0].disabled_at).not.toBeNull();
  });

  test('cannot opt-in a private snake via API', async ({ authenticatedPage }) => {
    const snakeName = `API LB Private Snake ${Date.now()}`;

    // Create a private battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/api-lb-private');
    await authenticatedPage.getByLabel('Visibility').selectOption('private');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const snakes = await query<{ battlesnake_id: string }>(
      "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
      [snakeName]
    );
    const snakeId = snakes[0].battlesnake_id;

    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    const response = await authenticatedPage.request.post(`/api/leaderboards/${leaderboardId}/entries`, {
      data: { battlesnake_id: snakeId }
    });

    expect(response.status()).toBe(400);
    const body = await response.text();
    expect(body).toContain('public');
  });

  test('cannot opt-in another user\'s snake via API', async ({ authenticatedPage, loginAsUser }) => {
    const snakeName = `API LB Other Snake ${Date.now()}`;

    // First user creates a public battlesnake
    await authenticatedPage.goto('/battlesnakes/new');
    await authenticatedPage.getByLabel('Name').fill(snakeName);
    await authenticatedPage.getByLabel('URL').fill('https://example.com/api-lb-other');
    await authenticatedPage.getByLabel('Visibility').selectOption('public');
    await authenticatedPage.getByRole('button', { name: 'Create Battlesnake' }).click();

    const snakes = await query<{ battlesnake_id: string }>(
      "SELECT battlesnake_id FROM battlesnakes WHERE name = $1",
      [snakeName]
    );
    const snakeId = snakes[0].battlesnake_id;

    const leaderboards = await query<{ leaderboard_id: string }>(
      "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
    );
    const leaderboardId = leaderboards[0].leaderboard_id;

    // Logout and login as second user
    await authenticatedPage.goto('/auth/logout');
    const secondUser = createMockUser('lb_other_user');
    await loginAsUser(authenticatedPage, secondUser);

    // Try to opt-in first user's snake
    const response = await authenticatedPage.request.post(`/api/leaderboards/${leaderboardId}/entries`, {
      data: { battlesnake_id: snakeId }
    });

    expect(response.status()).toBe(403);
  });

  test('GET /api/leaderboards/:id/rankings returns 404 for non-existent leaderboard', async ({ authenticatedPage }) => {
    const fakeId = '00000000-0000-0000-0000-000000000000';
    const response = await authenticatedPage.request.get(`/api/leaderboards/${fakeId}/rankings`);
    expect(response.status()).toBe(404);
  });
});
