-- Add UNIQUE constraint on leaderboard_game_results to prevent duplicate rating applications
-- This guards against idempotency bugs from job retries
ALTER TABLE leaderboard_game_results
    ADD CONSTRAINT uq_leaderboard_game_results_game_entry
    UNIQUE (leaderboard_game_id, leaderboard_entry_id);

-- Add index on leaderboard_game_results.leaderboard_game_id for lookups
CREATE INDEX idx_leaderboard_game_results_game_id
    ON leaderboard_game_results (leaderboard_game_id);

-- Rename wins/losses to first_place_finishes/non_first_finishes
-- In a 4-player game, only 1st place counts as a "win" — calling 2nd place a "loss" is misleading
ALTER TABLE leaderboard_entries RENAME COLUMN wins TO first_place_finishes;
ALTER TABLE leaderboard_entries RENAME COLUMN losses TO non_first_finishes;
