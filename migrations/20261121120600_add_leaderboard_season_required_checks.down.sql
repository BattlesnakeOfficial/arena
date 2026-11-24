SET LOCAL lock_timeout = '5s';
ALTER TABLE leaderboard_entries DROP CONSTRAINT leaderboard_entries_season_required;
ALTER TABLE leaderboard_games DROP CONSTRAINT leaderboard_games_season_required;
