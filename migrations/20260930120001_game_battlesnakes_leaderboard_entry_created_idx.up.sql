-- no-transaction
-- Concurrent index DDL is the sole statement to avoid blocking game writes.
-- Serves "a snake's newest games" without walking every game (snake profile).
CREATE INDEX CONCURRENTLY IF NOT EXISTS game_battlesnakes_leaderboard_entry_created_idx ON game_battlesnakes (leaderboard_entry_id, created_at DESC) WHERE leaderboard_entry_id IS NOT NULL;
