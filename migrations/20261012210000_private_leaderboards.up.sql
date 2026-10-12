SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

-- Private leaderboards are unlisted: reachable only by direct link, and
-- joining needs the invite code. The code is stored in plaintext so admins
-- can re-share it; it only gates joining a ladder.
ALTER TABLE leaderboards
    ADD COLUMN visibility TEXT NOT NULL DEFAULT 'public'
        CHECK (visibility IN ('public', 'private')),
    ADD COLUMN invite_code TEXT,
    ADD CONSTRAINT leaderboards_private_invite_code
        CHECK ((visibility = 'private') = (invite_code IS NOT NULL));

-- The global rating is a public rollup, so private ladders stay out of it
-- (both the per-board bests and the enabled-board count).
CREATE OR REPLACE FUNCTION global_player_scores(min_games integer)
RETURNS TABLE (
    user_id uuid,
    best_scores double precision[],
    total_score double precision,
    enabled_leaderboards bigint
) LANGUAGE sql STABLE AS $$
    WITH best_per_board AS (
        SELECT b.user_id, le.leaderboard_id,
               MAX(le.display_score) AS best_score
        FROM leaderboard_entries le
        JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
        JOIN battlesnakes b ON b.battlesnake_id = le.battlesnake_id
        WHERE le.disabled_at IS NULL
          AND l.disabled_at IS NULL
          AND l.visibility = 'public'
          AND b.deleted_at IS NULL
          AND le.games_played >= min_games
        GROUP BY b.user_id, le.leaderboard_id
    )
    SELECT bb.user_id,
           ARRAY_AGG(bb.best_score ORDER BY bb.leaderboard_id),
           SUM(bb.best_score)::double precision,
           (SELECT COUNT(*) FROM leaderboards
             WHERE disabled_at IS NULL AND visibility = 'public')::bigint
    FROM best_per_board bb
    GROUP BY bb.user_id
$$;
