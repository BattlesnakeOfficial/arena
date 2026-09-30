-- no-transaction
-- Concurrent index DDL is the sole statement to avoid blocking game writes.
DROP INDEX CONCURRENTLY IF EXISTS game_battlesnakes_leaderboard_entry_id_idx;
