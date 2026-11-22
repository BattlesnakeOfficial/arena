SET LOCAL lock_timeout = '5s';
-- Scratch DBs only: fails once any (league, snake) has entries in two seasons.
ALTER TABLE leaderboard_entries ADD CONSTRAINT leaderboard_entries_leaderboard_id_battlesnake_id_key
    UNIQUE (leaderboard_id, battlesnake_id);
