-- Re-add the unique constraint on (leaderboard_id, battlesnake_id).
-- Ensure production has no duplicates before running this migration.
ALTER TABLE leaderboard_entries
    ADD CONSTRAINT leaderboard_entries_leaderboard_id_battlesnake_id_key
    UNIQUE (leaderboard_id, battlesnake_id);
