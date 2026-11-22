SET LOCAL lock_timeout = '5s';

-- A player's global score counts each league's currently active season only
-- (half-open [starts_at, ends_at), NULL end = open, resolved at query time).
-- Gap and future-only leagues contribute no score and no denominator slot;
-- past-season entries stop counting the moment their season ends. While every
-- league has a single open-ended Preseason this returns the same rows as the
-- previous body.
CREATE OR REPLACE FUNCTION global_player_scores(min_games integer)
RETURNS TABLE (
    user_id uuid,
    best_scores double precision[],
    total_score double precision,
    enabled_leaderboards bigint
) LANGUAGE sql STABLE AS $$
    WITH active_seasons AS (
        SELECT s.season_id, s.leaderboard_id
        FROM leaderboard_seasons s
        WHERE s.starts_at <= clock_timestamp()
          AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at)
    ),
    best_per_board AS (
        SELECT b.user_id, le.leaderboard_id,
               MAX(le.display_score) AS best_score
        FROM leaderboard_entries le
        JOIN active_seasons a ON a.season_id = le.season_id
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
           (SELECT COUNT(*)
             FROM leaderboards l
             JOIN active_seasons a ON a.leaderboard_id = l.leaderboard_id
             WHERE l.disabled_at IS NULL AND l.visibility = 'public')::bigint
    FROM best_per_board bb
    GROUP BY bb.user_id
$$;
