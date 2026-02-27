ALTER TABLE game_battlesnakes
    ALTER COLUMN battlesnake_id DROP NOT NULL;

ALTER TABLE game_battlesnakes
    ADD COLUMN leaderboard_entry_id UUID
        REFERENCES leaderboard_entries(leaderboard_entry_id)
        ON DELETE SET NULL;

ALTER TABLE game_battlesnakes
    ADD CONSTRAINT game_battlesnakes_snake_or_entry_required
    CHECK (battlesnake_id IS NOT NULL OR leaderboard_entry_id IS NOT NULL);
