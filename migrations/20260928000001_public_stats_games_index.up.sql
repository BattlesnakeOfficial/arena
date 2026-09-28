-- no-transaction
-- Concurrent index DDL is the sole statement to avoid blocking game writes.
CREATE INDEX CONCURRENTLY IF NOT EXISTS games_public_stats_created_at_idx ON games(created_at) WHERE status = 'finished' AND engine_game_id IS NULL;
