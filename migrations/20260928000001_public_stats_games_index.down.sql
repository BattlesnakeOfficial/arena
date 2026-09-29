-- no-transaction
-- Concurrent index DDL is the sole statement to avoid blocking game writes.
DROP INDEX CONCURRENTLY IF EXISTS games_public_stats_created_at_idx;
