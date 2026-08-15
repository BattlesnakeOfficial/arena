import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

// Tag order everywhere is (category ASC, name ASC): 'language' sorts before
// 'platform', so Rust comes before Raspberry Pi.
const EXPECTED_TAGS = ['Rust', 'Raspberry Pi'];

test.describe('Snake Tags', () => {
  test('tags render as chips on user profile, leaderboard, and snake profile', async ({
    authenticatedPage,
    mockUser,
  }) => {
    const taggedName = `Tagged Snake ${Date.now()}`;
    const untaggedName = `Plain Snake ${Date.now()}`;

    // The authenticatedPage fixture logged mockUser in via mock OAuth.
    const users = await query<{ user_id: string }>(
      'SELECT user_id FROM users WHERE github_login = $1',
      [mockUser.login]
    );
    expect(users.length).toBe(1);
    const userId = users[0].user_id;

    try {
      // Seed one tagged snake and one untagged snake via direct inserts.
      const tagged = await query<{ battlesnake_id: string }>(
        `INSERT INTO battlesnakes (user_id, name, url)
         VALUES ($1, $2, 'https://example.com/tagged')
         RETURNING battlesnake_id`,
        [userId, taggedName]
      );
      const taggedId = tagged[0].battlesnake_id;

      const untagged = await query<{ battlesnake_id: string }>(
        `INSERT INTO battlesnakes (user_id, name, url)
         VALUES ($1, $2, 'https://example.com/plain')
         RETURNING battlesnake_id`,
        [userId, untaggedName]
      );
      const untaggedId = untagged[0].battlesnake_id;

      await query(
        `INSERT INTO battlesnake_tags (battlesnake_id, tag_id)
         SELECT $1, tag_id FROM tags WHERE name = ANY($2)`,
        [taggedId, EXPECTED_TAGS]
      );

      // Leaderboard entries: tagged snake is ranked (>= 10 games, high score
      // so it stays inside the LIMIT 100 window), untagged is in placement.
      const leaderboards = await query<{ leaderboard_id: string }>(
        "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
      );
      const leaderboardId = leaderboards[0].leaderboard_id;

      await query(
        `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played, display_score)
         VALUES ($1, $2, 10, 999)`,
        [leaderboardId, taggedId]
      );
      await query(
        `INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played)
         VALUES ($1, $2, 0)`,
        [leaderboardId, untaggedId]
      );

      // --- User profile: chips on the tagged snake's card, nothing on the untagged one ---
      await authenticatedPage.goto(`/users/${mockUser.login}`);

      const taggedCard = authenticatedPage.locator('.scard').filter({ hasText: taggedName });
      const untaggedCard = authenticatedPage.locator('.scard').filter({ hasText: untaggedName });

      await expect(taggedCard.locator('.snake-tag')).toHaveText(EXPECTED_TAGS);
      await expect(untaggedCard.locator('.snake-tags')).toHaveCount(0);
      await expect(untaggedCard.locator('.snake-tag')).toHaveCount(0);

      // Chips wrap rather than overflow their row.
      const wrap = await taggedCard.locator('.snake-tags').evaluate(
        (el) => getComputedStyle(el).flexWrap
      );
      expect(wrap).toBe('wrap');

      // --- Leaderboard: same chips on the ranked row, none on the placement row ---
      await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);

      const taggedRow = authenticatedPage.locator('tr').filter({ hasText: taggedName });
      const untaggedRow = authenticatedPage.locator('tr').filter({ hasText: untaggedName });

      await expect(taggedRow).toHaveCount(1);
      await expect(taggedRow.locator('.snake-tag')).toHaveText(EXPECTED_TAGS);
      await expect(taggedRow.locator('.snake-tag').first()).toHaveClass('snake-tag');

      await expect(untaggedRow).toHaveCount(1);
      await expect(untaggedRow.locator('.snake-tags')).toHaveCount(0);
      await expect(untaggedRow.locator('.snake-tag')).toHaveCount(0);

      // --- Snake profile: the shared component guards the refactor ---
      await authenticatedPage.goto(`/battlesnakes/${taggedId}/profile`);
      await expect(authenticatedPage.locator('.snake-tag')).toHaveText(EXPECTED_TAGS);
    } finally {
      // Sessions reference users without ON DELETE CASCADE, so clear those first.
      await query(
        'DELETE FROM sessions WHERE user_id IN (SELECT user_id FROM users WHERE github_login = $1)',
        [mockUser.login]
      );
      await query('DELETE FROM users WHERE github_login = $1', [mockUser.login]);
    }
  });
});
