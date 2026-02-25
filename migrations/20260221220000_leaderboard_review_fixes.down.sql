ALTER TABLE leaderboard_entries RENAME COLUMN first_place_finishes TO wins;
ALTER TABLE leaderboard_entries RENAME COLUMN non_first_finishes TO losses;

DROP INDEX IF EXISTS idx_leaderboard_game_results_game_id;

ALTER TABLE leaderboard_game_results
    DROP CONSTRAINT IF EXISTS uq_leaderboard_game_results_game_entry;
