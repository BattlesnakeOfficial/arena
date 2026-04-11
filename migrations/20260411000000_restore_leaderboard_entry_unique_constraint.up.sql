ALTER TABLE leaderboard_entries
    ADD CONSTRAINT leaderboard_entries_leaderboard_id_battlesnake_id_key
    UNIQUE (leaderboard_id, battlesnake_id);
