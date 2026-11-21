-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS leaderboard_games_season_created_idx ON leaderboard_games(season_id, created_at DESC);
