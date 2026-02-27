ALTER TABLE game_battlesnakes
    DROP CONSTRAINT game_battlesnakes_snake_or_entry_required;

ALTER TABLE game_battlesnakes
    DROP COLUMN leaderboard_entry_id;

-- Will fail if any rows have NULL battlesnake_id
ALTER TABLE game_battlesnakes
    ALTER COLUMN battlesnake_id SET NOT NULL;
