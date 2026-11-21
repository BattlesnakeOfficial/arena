SET LOCAL lock_timeout = '5s';
ALTER TABLE leaderboard_entries ADD CONSTRAINT leaderboard_entries_season_required CHECK (season_id IS NOT NULL) NOT VALID;
ALTER TABLE leaderboard_games ADD CONSTRAINT leaderboard_games_season_required CHECK (season_id IS NOT NULL) NOT VALID;
