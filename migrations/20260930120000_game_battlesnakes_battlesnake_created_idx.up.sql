-- no-transaction
-- Concurrent index DDL is the sole statement to avoid blocking game writes.
-- Serves "a snake's newest games" without walking every game (snake profile).
CREATE INDEX CONCURRENTLY IF NOT EXISTS game_battlesnakes_battlesnake_created_idx ON game_battlesnakes (battlesnake_id, created_at DESC) WHERE battlesnake_id IS NOT NULL;
