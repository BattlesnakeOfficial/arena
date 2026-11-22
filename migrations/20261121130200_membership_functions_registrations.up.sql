SET LOCAL lock_timeout = '5s';

-- Membership is league-level: a user with a non-deleted snake registered on
-- the league, active or paused (deliberately no disabled_at filter).
CREATE OR REPLACE FUNCTION leaderboard_member(p_leaderboard_id uuid, p_user_id uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (
        SELECT 1
        FROM leaderboard_registrations r
        JOIN battlesnakes b ON b.battlesnake_id = r.battlesnake_id
        WHERE r.leaderboard_id = p_leaderboard_id
          AND b.user_id = p_user_id
          AND b.deleted_at IS NULL
    )
$$;

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
