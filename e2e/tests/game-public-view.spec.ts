import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Public Game Viewing', () => {
  // Helper: create a game via the API using an authenticated page, return game_id
  async function createGameViaDb(
    status: 'waiting' | 'running' | 'finished' | 'failed' = 'finished'
  ): Promise<string> {
    // Insert a minimal game directly in the DB for testing view access
    const result = await query<{ game_id: string }>(
      `INSERT INTO games (board_size, game_type, status, created_at, updated_at)
       VALUES ('small', 'standard', $1, NOW(), NOW())
       RETURNING game_id::text AS game_id`,
      [status]
    );
    return result[0].game_id;
  }

  // Helper: a finished Solo game with final frame stats.
  // game_type mirrors what GameType::as_str() writes for real Solo games.
  async function createSoloGameViaDb(finalTurn: number, deathCause: string | null): Promise<string> {
    const game = await query<{ game_id: string }>(
      `INSERT INTO games (board_size, game_type, status, created_at, updated_at)
       VALUES ('11x11', 'Solo', 'finished', NOW(), NOW())
       RETURNING game_id::text AS game_id`
    );
    const gameId = game[0].game_id;

    const death = deathCause
      ? JSON.stringify({ Cause: deathCause, Turn: finalTurn, EliminatedBy: '' })
      : 'null';
    const frame = JSON.stringify({
      Turn: finalTurn,
      Snakes: [
        {
          ID: 'solo-snake',
          Name: 'Solo Snake',
          Body: [{ X: 5, Y: 5 }],
          Health: 0,
          Death: JSON.parse(death),
          EliminatedCause: deathCause ?? '',
          EliminatedBy: ''
        }
      ],
      Food: [],
      Hazards: []
    });

    await query(
      `INSERT INTO turns (game_id, turn_number, frame_data)
       VALUES ($1::uuid, $2, $3::jsonb)`,
      [gameId, String(finalTurn), frame]
    );
    return gameId;
  }

  test('unauthenticated user can view a game page directly', async ({ page }) => {
    const gameId = await createGameViaDb();

    // Visit the game page without logging in
    const response = await page.goto(`/games/${gameId}`);

    // Should NOT get a 401 — the page should load successfully
    expect(response?.status()).toBe(200);

    // Should see the game details heading
    await expect(page.getByRole('heading', { name: 'Game Details' })).toBeVisible();

    // Should see the game ID on the page
    await expect(page.getByText(`Game ${gameId}`)).toBeVisible();
  });

  test('unauthenticated user sees public navigation on game page', async ({ page }) => {
    const gameId = await createGameViaDb();

    await page.goto(`/games/${gameId}`);

    // Should see "View Leaderboards" link (not "Create Another Game" which is for auth users)
    await expect(page.getByRole('link', { name: 'View Leaderboards' })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to Home' })).toBeVisible();

    // Should NOT see authenticated-only navigation
    await expect(page.getByRole('link', { name: 'Create Another Game' })).not.toBeVisible();
    await expect(page.getByRole('link', { name: 'Back to Profile' })).not.toBeVisible();
  });

  test('authenticated user sees authenticated navigation on game page', async ({ authenticatedPage }) => {
    const gameId = await createGameViaDb();

    await authenticatedPage.goto(`/games/${gameId}`);

    // Authenticated users should see "Create Another Game" and "Back to Profile"
    await expect(authenticatedPage.getByRole('link', { name: 'Create Another Game' })).toBeVisible();
    await expect(authenticatedPage.getByRole('link', { name: 'Back to Profile' })).toBeVisible();

    // Should NOT see public-only navigation
    await expect(authenticatedPage.getByRole('link', { name: 'View Leaderboards' })).not.toBeVisible();
    await expect(authenticatedPage.getByRole('link', { name: 'Back to Home' })).not.toBeVisible();
  });

  test('game page shows board viewer iframe for unauthenticated user', async ({ page }) => {
    const gameId = await createGameViaDb();

    await page.goto(`/games/${gameId}`);

    // The board viewer iframe should be present
    const iframe = page.locator('#board-viewer');
    await expect(iframe).toBeVisible();

    // Iframe src should contain the game ID
    const src = await iframe.getAttribute('src');
    expect(src).toContain(gameId);
  });

  test('finished solo game shows survival stats and cause of death', async ({ page }) => {
    const gameId = await createSoloGameViaDb(42, 'out-of-health');

    await page.goto(`/games/${gameId}`);

    const meta = page.locator('.gmeta .meta-list');
    await expect(meta.getByText('Turns Survived', { exact: true })).toBeVisible();
    await expect(meta.getByText('42', { exact: true })).toBeVisible();
    await expect(meta.getByText('Cause of Death', { exact: true })).toBeVisible();
    await expect(meta.getByText('Starved', { exact: true })).toBeVisible();

    // Progression viewer regression: the board iframe still renders for solo games
    const iframe = page.locator('#board-viewer');
    await expect(iframe).toBeVisible();
    const src = await iframe.getAttribute('src');
    expect(src).toContain(gameId);
  });

  test('finished solo game at the turn cap shows the survival outcome', async ({ page }) => {
    const gameId = await createSoloGameViaDb(5000, null);

    await page.goto(`/games/${gameId}`);

    const meta = page.locator('.gmeta .meta-list');
    await expect(meta.getByText('Turns Survived', { exact: true })).toBeVisible();
    await expect(meta.getByText('5000', { exact: true })).toBeVisible();
    // No death cause: the cap outcome row appears instead
    await expect(meta.getByText('Outcome', { exact: true })).toBeVisible();
    await expect(meta.getByText('Survived to the 5,000-turn limit')).toBeVisible();
    await expect(meta.getByText('Cause of Death', { exact: true })).not.toBeVisible();
  });

  test('finished game page shows an Export GIF link pointing at the exporter', async ({ page }) => {
    const gameId = await createGameViaDb();

    await page.goto(`/games/${gameId}`);

    // The e2e server env does not set BASE_URL, so the app falls back to its
    // default origin — the exporter href embeds that default.
    const link = page.getByRole('link', { name: 'Export GIF' });
    await expect(link).toHaveCount(1);
    await expect(link).toHaveAttribute(
      'href',
      `https://exporter.battlesnake.com/games/${gameId}/gif?engine_url=http://localhost:3000/api`
    );
    await expect(link).toHaveAttribute('target', '_blank');
    await expect(link).toHaveAttribute('rel', 'noopener');
  });

  test('unfinished game pages show no Export GIF link', async ({ page }) => {
    for (const status of ['waiting', 'running', 'failed'] as const) {
      const gameId = await createGameViaDb(status);

      await page.goto(`/games/${gameId}`);

      await expect(page.getByRole('link', { name: 'Export GIF' })).toHaveCount(0);
    }
  });

  test('Copy Link button copies the share URL (share script is not HTML-escaped)', async ({
    page,
    context,
    baseURL,
  }) => {
    // Regression: maud HTML-escapes `&&` inside script{} blocks, so the share
    // script used to serialize as `navigator.clipboard &amp;&amp; ...` — a JS
    // SyntaxError that silently killed the click handler, so Copy Link did
    // nothing on every game page. If the handler is attached, clicking flips
    // the label to "Copied!".
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], {
      origin: baseURL,
    });

    const gameId = await createGameViaDb();
    await page.goto(`/games/${gameId}`);

    const copyBtn = page.locator('#share-copy');
    await expect(copyBtn).toHaveText('Copy Link');
    await copyBtn.click();
    await expect(copyBtn).toHaveText('Copied!');

    // And the clipboard actually holds the share URL.
    const clipboard = await page.evaluate(() => navigator.clipboard.readText());
    expect(clipboard).toContain(`/games/${gameId}`);
  });
});

test.describe('Homepage Leaderboard Link for Unauthenticated Users', () => {
  test('homepage shows Leaderboards link when not logged in', async ({ page }) => {
    await page.goto('/');

    // Unauthenticated users should see a leaderboards link/button (the hero
    // CTA; "Leaderboards" alone would also match the global nav link)
    await expect(page.getByRole('link', { name: 'Browse leaderboards' })).toBeVisible();
  });

  test('Leaderboards link on homepage navigates to leaderboards page', async ({ page }) => {
    await page.goto('/');

    // Click the leaderboards hero CTA
    await page.getByRole('link', { name: 'Browse leaderboards' }).click();

    // Should navigate to the leaderboards page
    await expect(page).toHaveURL('/leaderboards');
    await expect(page.getByRole('heading', { name: 'Leaderboards' })).toBeVisible();
  });
});
