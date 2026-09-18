import { test, expect, createMockUser } from '../fixtures/test';
import { query } from '../fixtures/db';

async function userId(login: string): Promise<string> {
  const rows = await query<{ user_id: string }>('SELECT user_id FROM users WHERE github_login = $1', [login]);
  return rows[0].user_id;
}

async function snake(owner: string, name: string, visibility = 'public'): Promise<string> {
  const rows = await query<{ battlesnake_id: string }>(
    `INSERT INTO battlesnakes (user_id, name, url, visibility)
     VALUES ($1, $2, 'https://example.com/snake', $3) RETURNING battlesnake_id`,
    [owner, name, visibility],
  );
  return rows[0].battlesnake_id;
}

async function finishedGame(creator: string | null, lineup: string[], status = 'finished'): Promise<string> {
  const rows = await query<{ game_id: string }>(
    `INSERT INTO games (board_size, game_type, status, created_by_user_id, rematch_battlesnake_ids)
     VALUES ('19x19', 'Royale', $1, $2, $3) RETURNING game_id`,
    [status, creator, lineup],
  );
  for (const id of lineup) {
    await query('INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)', [rows[0].game_id, id]);
  }
  return rows[0].game_id;
}

test.describe('game rematch', () => {
  test('creator gets an exact duplicate-preserving prefilled correction flow', async ({ authenticatedPage, mockUser }) => {
    const creator = await userId(mockUser.login);
    const a = await snake(creator, `Rematch A ${Date.now()}`, 'private');
    const b = await snake(creator, `Rematch B ${Date.now()}`);
    const game = await finishedGame(creator, [a, a, b]);

    await authenticatedPage.goto(`/games/${game}`);
    const rematch = authenticatedPage.getByRole('button', { name: 'Rematch' });
    await expect(rematch).toBeVisible();
    await expect(rematch).toHaveCSS('min-height', '44px');
    await rematch.click();

    await expect(authenticatedPage).toHaveURL(/\/games\/flow\//);
    await expect(authenticatedPage.getByLabel('Board Size', { exact: true })).toHaveValue('19x19');
    await expect(authenticatedPage.getByLabel('Game Type', { exact: true })).toHaveValue('Royale');
    const flow = authenticatedPage.url().split('/').pop()!;
    const rows = await query<{ selected_battlesnakes: string[] }>(
      'SELECT selected_battlesnakes FROM game_flows WHERE flow_id = $1', [flow],
    );
    expect(rows[0].selected_battlesnakes).toEqual([a, a, b]);
  });

  test('failed creator can rematch, but another user and historical games cannot', async ({
    authenticatedPage, mockUser, browser, loginAsUser,
  }) => {
    const creator = await userId(mockUser.login);
    const a = await snake(creator, `Failed A ${Date.now()}`);
    const failed = await finishedGame(creator, [a], 'failed');
    await query('UPDATE game_battlesnakes SET placement = 1 WHERE game_id = $1', [failed]);
    await authenticatedPage.goto(`/games/${failed}`);
    await expect(authenticatedPage.getByText('Incomplete')).toBeVisible();
    await expect(authenticatedPage.getByText('No result', { exact: true })).toBeVisible();
    await expect(authenticatedPage.getByText('1st', { exact: true })).toHaveCount(0);
    await expect(authenticatedPage.getByRole('button', { name: 'Rematch' })).toBeVisible();

    const otherPage = await browser.newPage();
    await loginAsUser(otherPage, createMockUser('noncreator'));
    await otherPage.goto(`/games/${failed}`);
    await expect(otherPage.getByRole('button', { name: 'Rematch' })).toHaveCount(0);
    expect((await otherPage.request.post(`/games/${failed}/rematch`)).status()).toBe(403);

    const historical = await finishedGame(null, [a]);
    expect((await authenticatedPage.request.post(`/games/${historical}/rematch`)).status()).toBe(403);
    await otherPage.close();
  });

  test('unavailable duplicate selections remain explicit until removed one at a time', async ({
    authenticatedPage, mockUser, browser, loginAsUser,
  }) => {
    const creator = await userId(mockUser.login);
    const otherPage = await browser.newPage();
    const other = createMockUser('private-owner');
    await loginAsUser(otherPage, other);
    const otherId = await userId(other.login);
    const unavailable = await snake(otherId, `Private ${Date.now()}`, 'private');
    const own = await snake(creator, `Own ${Date.now()}`, 'private');
    const game = await finishedGame(creator, [unavailable, own, unavailable]);

    await authenticatedPage.goto(`/games/${game}`);
    await authenticatedPage.getByRole('button', { name: 'Rematch' }).click();
    await expect(authenticatedPage.locator('.gc-unavailable')).toContainText('Unavailable snake');
    await expect(authenticatedPage.locator('.gc-unavailable .badge')).toHaveText('×2');
    await expect(authenticatedPage.getByRole('button', { name: 'Create Game' })).toHaveCount(0);
    await authenticatedPage.getByLabel('Remove one unavailable snake from lineup').click();
    await expect(authenticatedPage.locator('.gc-unavailable .badge')).toHaveText('×1');
    await otherPage.close();
  });
});
