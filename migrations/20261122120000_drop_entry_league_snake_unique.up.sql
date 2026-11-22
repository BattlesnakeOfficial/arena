SET LOCAL lock_timeout = '5s';
-- One (league, snake) may now hold one entry per season; uniqueness lives on
-- leaderboard_entries_season_snake_idx (season_id, battlesnake_id).
ALTER TABLE leaderboard_entries DROP CONSTRAINT leaderboard_entries_leaderboard_id_battlesnake_id_key;
