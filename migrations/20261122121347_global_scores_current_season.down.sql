SET LOCAL lock_timeout = '5s';

-- Restore the Phase 2 body (registrations-joined, all seasons).
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
        JOIN leaderboard_registrations r ON r.leaderboard_id = le.leaderboard_id
                                       AND r.battlesnake_id = le.battlesnake_id
        WHERE r.disabled_at IS NULL
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
