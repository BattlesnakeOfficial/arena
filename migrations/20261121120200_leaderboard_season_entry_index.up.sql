-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS leaderboard_entries_season_snake_idx ON leaderboard_entries(season_id, battlesnake_id);
