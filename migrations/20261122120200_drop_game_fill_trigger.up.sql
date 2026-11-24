SET LOCAL lock_timeout = '5s';
-- Touches leaderboard_games only. Both fill triggers are gone after this, so
-- the shared function can be dropped.
DROP TRIGGER fill_leaderboard_game_season_before_insert ON leaderboard_games;
DROP FUNCTION fill_leaderboard_season_id();
