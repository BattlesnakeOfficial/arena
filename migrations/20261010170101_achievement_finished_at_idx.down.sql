-- no-transaction
-- Drop the recent achievement repair index.
DROP INDEX CONCURRENTLY IF EXISTS games_achievement_finished_at_idx;
