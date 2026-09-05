import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

test.describe('Tournament Pages', () => {
  test('a live tournament refreshes through scheduled, live, and terminal states', async ({
    authenticatedPage,
    mockUser,
  }, testInfo) => {
    const name = `Live Refresh Tournament ${testInfo.workerIndex} ${Date.now()}`;
    let tournamentId: string | undefined;

    try {
      const [user] = await query<{ user_id: string }>(
        'SELECT user_id FROM users WHERE github_login = $1',
        [mockUser.login]
      );
      const snakes = await query<{ battlesnake_id: string }>(
        `INSERT INTO battlesnakes (user_id, name, url, visibility)
         VALUES ($1, $2, 'https://example.com/tournament-one', 'public'),
                ($1, $3, 'https://example.com/tournament-two', 'public')
         RETURNING battlesnake_id`,
        [user.user_id, `${name} Snake 1`, `${name} Snake 2`]
      );
      const [tournament] = await query<{ tournament_id: string }>(
        `INSERT INTO tournaments (name, user_id, status, current_round, visibility)
         VALUES ($1, $2, 'in_progress', 1, 'public') RETURNING tournament_id`,
        [name, user.user_id]
      );
      tournamentId = tournament.tournament_id;
      for (let index = 0; index < snakes.length; index++) {
        await query(
          `INSERT INTO tournament_registrations
             (tournament_id, battlesnake_id, user_id, seed)
           VALUES ($1, $2, $3, $4)`,
          [tournamentId, snakes[index].battlesnake_id, user.user_id, index + 1]
        );
      }
      const [match] = await query<{ match_id: string }>(
        `INSERT INTO tournament_matches
           (tournament_id, round, position, visual_column, visual_row)
         VALUES ($1, 1, 0, 0, 0) RETURNING match_id`,
        [tournamentId]
      );
      for (let index = 0; index < snakes.length; index++) {
        await query(
          `INSERT INTO match_participants
             (match_id, slot, battlesnake_id, participant_type, seed_position)
           VALUES ($1, $2::smallint, $3, 'seed', $2::integer)`,
          [match.match_id, index + 1, snakes[index].battlesnake_id]
        );
      }

      await authenticatedPage.goto(`/tournaments/${tournamentId}?watch_round=1`);
      await expect(authenticatedPage.getByText('Final · scheduled')).toBeVisible();
      await expect(authenticatedPage.locator('[data-live-page-refresh]')).toHaveCount(1);

      await query("UPDATE tournament_matches SET status = 'in_progress' WHERE match_id = $1", [match.match_id]);
      await expect(authenticatedPage.getByText('Final · live')).toBeVisible({ timeout: 10_000 });

      await query("UPDATE tournament_matches SET status = 'completed' WHERE match_id = $1", [match.match_id]);
      await query("UPDATE tournaments SET status = 'completed' WHERE tournament_id = $1", [tournamentId]);
      await expect(authenticatedPage.getByText('Completed', { exact: true })).toBeVisible({ timeout: 10_000 });
      await expect(authenticatedPage.locator('[data-live-page-refresh]')).toHaveCount(0);
    } finally {
      if (tournamentId) await query('DELETE FROM tournaments WHERE tournament_id = $1', [tournamentId]);
    }
  });
});
