DROP INDEX games_created_by_user_id_idx;
ALTER TABLE games
    DROP CONSTRAINT games_rematch_battlesnake_ids_count,
    DROP COLUMN rematch_battlesnake_ids,
    DROP COLUMN created_by_user_id;
