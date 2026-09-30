-- Deleting a battlesnake now soft-deletes it, so the games, placements and
-- tournament results it took part in stay intact for everyone else.
-- Deleted snakes are hidden from every active surface (listings, game
-- creation, matchmaking, the API) and their profile renders a small
-- "deleted" page.
SET LOCAL lock_timeout = '5s';

ALTER TABLE battlesnakes
    ADD COLUMN deleted_at TIMESTAMPTZ;

-- Names only need to be unique among live snakes, so a deleted snake's name
-- can be reused. Same index name: the create/update handlers match on it to
-- show a friendly duplicate-name error.
DROP INDEX unique_battlesnake_name_per_user;

CREATE UNIQUE INDEX unique_battlesnake_name_per_user
    ON battlesnakes (user_id, name)
    WHERE deleted_at IS NULL;
