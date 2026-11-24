-- DEV-2174: leaderboard_game 5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175
-- (engine game 3c7eb192-5e4d-4a97-a62b-4ca69594ce81, Standard 11x11) was
-- scored twice on 2026-10-26 when two update_ratings runs raced: both
-- idempotency COUNTs read 0 before either transaction committed, the
-- second run re-incremented the counters, and its result-row inserts were
-- silently dropped by ON CONFLICT DO NOTHING. Four entries
-- (a2646f79-…, b599c892-…, 4245fc28-…, 120c431a-…) ended one application
-- ahead on every counter that game bumped.
--
-- This backs out exactly that game's second application. Every statement
-- is keyed to the evidenced game and guarded on its own table's drift
-- signature, so each is independently idempotent: the guards stop matching
-- once a row is repaired and never match a healthy row. mu/sigma on
-- leaderboard_entries and the weng_lin_ratings table are NOT repaired:
-- the second rating application can't be cleanly undone and is negligible
-- after 2000+ games.
--
-- Expected on prod (2026-11-07): 4 leaderboard_entries rows, 4
-- win_rate_stats rows, 3 food_eaten_stats rows (the fourth snake ate
-- nothing in this game and never drifted).

-- 1) leaderboard_entries: drop the extra game from games_played and from
--    the first/non-first split the writer bumped (update_rating increments
--    first_place_finishes for the game's single outright winner,
--    non_first_finishes for everyone else).
WITH game_entries AS (
    SELECT
        r.leaderboard_entry_id,
        (r.placement = 1 AND (
            SELECT COUNT(*) = 1
            FROM leaderboard_game_results w
            WHERE w.leaderboard_game_id = '5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175'
              AND w.placement = 1
        )) AS is_outright_winner
    FROM leaderboard_game_results r
    WHERE r.leaderboard_game_id = '5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175'
)
UPDATE leaderboard_entries le
SET games_played = le.games_played - 1,
    first_place_finishes = le.first_place_finishes
        - (CASE WHEN ge.is_outright_winner THEN 1 ELSE 0 END),
    non_first_finishes = le.non_first_finishes
        - (CASE WHEN ge.is_outright_winner THEN 0 ELSE 1 END)
FROM game_entries ge
WHERE ge.leaderboard_entry_id = le.leaderboard_entry_id
  AND le.games_played = (
        SELECT COUNT(*) + 1
        FROM leaderboard_game_results r2
        WHERE r2.leaderboard_entry_id = le.leaderboard_entry_id
      );

-- 2) win_rate_stats: same drift guard on its own games_played; back out the
--    win/loss the second application recorded and recompute score exactly
--    like the writer (wins / games_played * 100, using post-repair values —
--    Postgres evaluates every SET expression against the pre-UPDATE row).
WITH game_entries AS (
    SELECT
        r.leaderboard_entry_id,
        (r.placement = 1 AND (
            SELECT COUNT(*) = 1
            FROM leaderboard_game_results w
            WHERE w.leaderboard_game_id = '5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175'
              AND w.placement = 1
        )) AS is_outright_winner
    FROM leaderboard_game_results r
    WHERE r.leaderboard_game_id = '5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175'
)
UPDATE win_rate_stats wr
SET games_played = wr.games_played - 1,
    wins = wr.wins - (CASE WHEN ge.is_outright_winner THEN 1 ELSE 0 END),
    losses = wr.losses - (CASE WHEN ge.is_outright_winner THEN 0 ELSE 1 END),
    score = CASE WHEN wr.games_played - 1 > 0
             THEN (wr.wins - (CASE WHEN ge.is_outright_winner THEN 1 ELSE 0 END))::double precision
                  / (wr.games_played - 1)::double precision * 100.0
             ELSE 0.0 END,
    updated_at = NOW()
FROM game_entries ge
WHERE ge.leaderboard_entry_id = wr.leaderboard_entry_id
  AND wr.games_played = (
        SELECT COUNT(*) + 1
        FROM leaderboard_game_results r2
        WHERE r2.leaderboard_entry_id = wr.leaderboard_entry_id
      );

-- 3) food_eaten_stats: the second application added this game's food a
--    second time. Guard: score exceeds the sum of recorded per-game food
--    by exactly this game's food for that entry. Entries that ate nothing
--    in this game never drifted (the writer skips zero-food snakes).
UPDATE food_eaten_stats fe
SET food_score = fe.food_score - r.food_eaten,
    updated_at = NOW()
FROM leaderboard_game_results r
WHERE r.leaderboard_game_id = '5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175'
  AND r.leaderboard_entry_id = fe.leaderboard_entry_id
  AND r.food_eaten > 0
  AND fe.food_score = (
        SELECT COALESCE(SUM(r2.food_eaten), 0) + r.food_eaten
        FROM leaderboard_game_results r2
        WHERE r2.leaderboard_entry_id = fe.leaderboard_entry_id
      );
