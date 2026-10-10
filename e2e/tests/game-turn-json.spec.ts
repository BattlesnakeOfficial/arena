import { readFile } from 'node:fs/promises';
import type { Page } from '@playwright/test';
import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

interface Duel {
  gameId: string;
  /** Alpha plays every turn. */
  alpha: string;
  /** Bravo hits the wall on turn 2. */
  bravo: string;
}

/** Users seeded by this file, removed (with their snakes and games) after each test. */
const seededUsers: string[] = [];

test.afterEach(async () => {
  const users = seededUsers.splice(0);
  if (users.length === 0) return;
  const games = `SELECT game_id FROM game_battlesnakes WHERE battlesnake_id IN
    (SELECT battlesnake_id FROM battlesnakes WHERE user_id = ANY($1::uuid[]))`;
  await query(`DELETE FROM turns WHERE game_id IN (${games})`, [users]);
  const deleted = await query<{ game_id: string }>(
    `DELETE FROM game_battlesnakes WHERE game_id IN (${games}) RETURNING game_id::text AS game_id`,
    [users],
  );
  await query('DELETE FROM games WHERE game_id = ANY($1::uuid[])', [deleted.map((g) => g.game_id)]);
  await query('DELETE FROM battlesnakes WHERE user_id = ANY($1::uuid[])', [users]);
  await query('DELETE FROM users WHERE user_id = ANY($1::uuid[])', [users]);
});

/** A finished two-snake game with frames for turns 0-2. The snakes are
 * private so they never show up in other specs' public snake lists (the game
 * builder's opponents, the directory). */
async function seedDuel(): Promise<Duel> {
  const unique = Date.now() * 1000 + Math.floor(Math.random() * 1000);
  const [user] = await query<{ user_id: string }>(
    `INSERT INTO users (external_github_id, github_login, github_access_token)
     VALUES ($1, $2, '') RETURNING user_id::text AS user_id`,
    [unique, `turnjson_${unique}`],
  );
  seededUsers.push(user.user_id);
  const [game] = await query<{ game_id: string }>(
    `INSERT INTO games (board_size, game_type, status)
     VALUES ('11x11', 'Standard', 'finished') RETURNING game_id::text AS game_id`,
  );
  const join = async (name: string, placement: number): Promise<string> => {
    const [snake] = await query<{ battlesnake_id: string }>(
      `INSERT INTO battlesnakes (user_id, name, url, visibility)
       VALUES ($1, $2, 'https://example.com', 'private') RETURNING battlesnake_id::text AS battlesnake_id`,
      [user.user_id, name],
    );
    const [entry] = await query<{ id: string }>(
      `INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement)
       VALUES ($1, $2, $3) RETURNING game_battlesnake_id::text AS id`,
      [game.game_id, snake.battlesnake_id, placement],
    );
    return entry.id;
  };
  const alpha = await join('Alpha', 1);
  const bravo = await join('Bravo', 2);

  for (const turn of [0, 1, 2]) {
    const frame = {
      Turn: turn,
      Snakes: [
        {
          ID: alpha,
          Name: 'Alpha',
          Health: 100 - turn,
          Body: [5 + turn, 4 + turn, 3 + turn].map((y) => ({ X: 5, Y: y })),
          Latency: '42',
          Shout: '',
          Death: null,
        },
        {
          ID: bravo,
          Name: 'Bravo',
          Health: 100 - turn,
          Body: [1 - turn, 2 - turn, 3 - turn].map((x) => ({ X: x, Y: 1 })),
          Latency: '42',
          Shout: '',
          Death: turn === 2 ? { Cause: 'wall-collision', Turn: 2, EliminatedBy: '' } : null,
        },
      ],
      Food: [{ X: 8, Y: 8 }],
      Hazards: [],
    };
    await query(
      `INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1::uuid, $2, $3::jsonb)`,
      [game.game_id, String(turn), JSON.stringify(frame)],
    );
  }
  return { gameId: game.game_id, alpha, bravo };
}

/** Replace the hosted board with one that reports showing `turn`, the way
 * board.battlesnake.com posts TURN messages to its parent. */
async function stubBoard(page: Page, turn: number): Promise<void> {
  await page.route('https://board.battlesnake.com/**', (route) =>
    route.fulfill({
      contentType: 'text/html',
      body: `<!doctype html><script>parent.postMessage({event: "TURN", data: {turn: ${turn}}}, "*")</script>`,
    }),
  );
}

const ELIMINATED = 'Bravo was eliminated on turn 2, so it wasn\'t sent a move request on turn 2. '
  + 'Its last move request was on turn 1.';

test.describe('turn JSON on the game page', () => {
  test('Download saves the turn the board is showing without leaving the page', async ({ page }) => {
    const { gameId, alpha } = await seedDuel();
    await stubBoard(page, 1);
    await page.goto(`/games/${gameId}`);

    const form = page.locator('#move-request-form');
    await expect(form.locator('#move-request-turn')).toHaveValue('1');
    await form.locator('select[name=you]').selectOption(alpha);
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      form.getByRole('button', { name: 'Download' }).click(),
    ]);

    expect(download.suggestedFilename()).toBe(`${gameId}-turn-1-alpha.json`);
    const request = JSON.parse(await readFile(await download.path(), 'utf8'));
    expect(request.turn).toBe(1);
    expect(request.you.id).toBe(alpha);
    expect(request.you.head).toEqual({ x: 5, y: 6 });
    expect(page.url()).toMatch(new RegExp(`/games/${gameId}$`));
    await expect(page.locator('#move-request-error')).toBeHidden();
  });

  test('a snake with no request on that turn is explained inline', async ({ page }) => {
    const { gameId, bravo } = await seedDuel();
    await stubBoard(page, 2);
    await page.goto(`/games/${gameId}`);

    const form = page.locator('#move-request-form');
    await expect(form.locator('#move-request-turn')).toHaveValue('2');
    await form.locator('select[name=you]').selectOption(bravo);
    await form.getByRole('button', { name: 'Download' }).click();

    await expect(page.getByRole('alert')).toHaveText(ELIMINATED);
    expect(page.url()).toMatch(new RegExp(`/games/${gameId}$`));
  });

  test('Copy puts the request on the clipboard', async ({ page, context }) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    const { gameId, alpha, bravo } = await seedDuel();
    await stubBoard(page, 1);
    await page.goto(`/games/${gameId}`);

    const form = page.locator('#move-request-form');
    await expect(form.locator('#move-request-turn')).toHaveValue('1');
    await form.locator('select[name=you]').selectOption(alpha);
    await form.getByRole('button', { name: 'Copy' }).click();
    await expect(form.getByRole('button', { name: 'Copied!' })).toBeVisible();

    const copied = JSON.parse(await page.evaluate(() => navigator.clipboard.readText()));
    expect(copied.turn).toBe(1);
    expect(copied.you.id).toBe(alpha);

    // Errors land inline for Copy too.
    await form.locator('select[name=you]').selectOption(bravo);
    await form.locator('#move-request-turn').fill('2');
    await form.getByRole('button', { name: 'Copy' }).click();
    await expect(page.getByRole('alert')).toHaveText(ELIMINATED);
  });

  test('without JavaScript the plain form downloads natively', async ({ browser }) => {
    const { gameId, alpha, bravo } = await seedDuel();
    const context = await browser.newContext({ javaScriptEnabled: false });
    const page = await context.newPage();
    await stubBoard(page, 0);
    await page.goto(`/games/${gameId}?turn=1`);

    const form = page.locator('#move-request-form');
    await expect(form.getByRole('button', { name: 'Copy' })).toBeHidden();
    await expect(form.locator('#move-request-turn')).toHaveValue('1');
    await form.locator('select[name=you]').selectOption(alpha);
    const [download] = await Promise.all([
      page.waitForEvent('download'),
      form.getByRole('button', { name: 'Download' }).click(),
    ]);
    expect(download.suggestedFilename()).toBe(`${gameId}-turn-1-alpha.json`);

    // Without JS an error replaces the page with the plain-text reason.
    await form.locator('select[name=you]').selectOption(bravo);
    await form.locator('#move-request-turn').fill('2');
    await form.getByRole('button', { name: 'Download' }).click();
    await expect(page.locator('body')).toHaveText(ELIMINATED);
    await context.close();
  });
});
