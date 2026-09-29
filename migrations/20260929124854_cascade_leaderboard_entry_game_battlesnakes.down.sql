SET LOCAL lock_timeout = '5s';

ALTER TABLE game_battlesnakes
    DROP CONSTRAINT game_battlesnakes_leaderboard_entry_id_fkey;

ALTER TABLE game_battlesnakes
    ADD CONSTRAINT game_battlesnakes_leaderboard_entry_id_fkey
        FOREIGN KEY (leaderboard_entry_id)
        REFERENCES leaderboard_entries(leaderboard_entry_id)
        ON DELETE SET NULL;
