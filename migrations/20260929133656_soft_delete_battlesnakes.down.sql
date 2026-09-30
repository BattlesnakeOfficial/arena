-- Fails if a live snake reuses a deleted snake's name; rename or purge those
-- rows first.
SET LOCAL lock_timeout = '5s';

DROP INDEX unique_battlesnake_name_per_user;

CREATE UNIQUE INDEX unique_battlesnake_name_per_user
    ON battlesnakes (user_id, name);

ALTER TABLE battlesnakes
    DROP COLUMN deleted_at;
