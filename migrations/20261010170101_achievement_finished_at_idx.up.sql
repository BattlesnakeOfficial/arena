-- no-transaction
-- Recent achievement repair scans only recently finished games.
CREATE INDEX CONCURRENTLY IF NOT EXISTS games_achievement_finished_at_idx ON games (finished_at, game_id) WHERE status = 'finished' AND finished_at IS NOT NULL;
