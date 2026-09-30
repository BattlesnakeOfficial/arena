-- Leaderboard games store their participants as leaderboard_entry_id with a NULL
-- battlesnake_id. Deleting a battlesnake cascades to its leaderboard_entries, and
-- the old ON DELETE SET NULL then left those game_battlesnakes rows with neither
-- column set, violating game_battlesnakes_snake_or_entry_required. Cascade instead,
-- matching the battlesnake_id FK.
--
-- Added NOT VALID so the swap is catalog-only; the next migration validates it
-- without blocking writes.
SET LOCAL lock_timeout = '5s';

ALTER TABLE game_battlesnakes
    DROP CONSTRAINT game_battlesnakes_leaderboard_entry_id_fkey;

ALTER TABLE game_battlesnakes
    ADD CONSTRAINT game_battlesnakes_leaderboard_entry_id_fkey
        FOREIGN KEY (leaderboard_entry_id)
        REFERENCES leaderboard_entries(leaderboard_entry_id)
        ON DELETE CASCADE
        NOT VALID;
