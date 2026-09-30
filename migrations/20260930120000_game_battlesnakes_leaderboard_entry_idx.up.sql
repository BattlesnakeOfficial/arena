-- no-transaction
-- Concurrent index DDL is the sole statement to avoid blocking game writes.
CREATE INDEX CONCURRENTLY IF NOT EXISTS game_battlesnakes_leaderboard_entry_id_idx ON game_battlesnakes (leaderboard_entry_id) WHERE leaderboard_entry_id IS NOT NULL;
