import { test, expect } from '../fixtures/test';
import { query } from '../fixtures/db';

// Pinned PR review finding: the live-refresh script resolves its manual-refresh
// fallback with `document.currentScript.nextElementSibling`. An inline classic
// script executes while the parser is still at its own `</script>` tag, so the
// following element does not exist yet and the lookup yields null. Every path
// that is supposed to surface the fallback (exhausted tick budget, unusable
// sessionStorage) therefore throws instead of revealing it, and the page stops
// refreshing with no visible indication and no Refresh link.
test.describe('Live refresh fallback', () => {
  test('reveals the manual refresh fallback when sessionStorage is unusable', async ({
    authenticatedPage,
  }, testInfo) => {
    const name = `Refresh Fallback LB ${testInfo.workerIndex} ${Date.now()}`;
    let leaderboardId: string | undefined;
    let gameId: string | undefined;

    try {
      const [leaderboard] = await query<{ leaderboard_id: string }>(
        'INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id',
        [name]
      );
      leaderboardId = leaderboard.leaderboard_id;
      const [game] = await query<{ game_id: string }>(
        `INSERT INTO games (board_size, game_type, status)
         VALUES ('11x11', 'Standard', 'waiting') RETURNING game_id`
      );
      gameId = game.game_id;
      await query('INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)', [
        leaderboardId,
        gameId,
      ]);

      // Simulate a browser that refuses live-refresh session storage writes
      // (private browsing / storage partitioning). The component must fail
      // closed *visibly*: no automatic reload, but a usable manual refresh.
      await authenticatedPage.addInitScript(() => {
        const originalSetItem = Storage.prototype.setItem;
        Storage.prototype.setItem = function patchedSetItem(key: string, value: string) {
          if (String(key).startsWith('arena:live-refresh:')) {
            throw new DOMException('blocked', 'SecurityError');
          }
          return originalSetItem.call(this, key, value);
        };
      });

      await authenticatedPage.goto(`/leaderboards/${leaderboardId}`);
      await expect(authenticatedPage.locator('[data-live-page-refresh]')).toHaveCount(1);
      await expect(authenticatedPage.locator('[data-live-page-refresh-expired]')).toBeVisible();
      await expect(
        authenticatedPage.locator('[data-live-page-refresh-expired]').getByRole('link', {
          name: 'Refresh',
        })
      ).toBeVisible();
    } finally {
      if (leaderboardId)
        await query('DELETE FROM leaderboards WHERE leaderboard_id = $1', [leaderboardId]);
      if (gameId) await query('DELETE FROM games WHERE game_id = $1', [gameId]);
    }
  });
});
