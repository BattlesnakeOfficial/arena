ALTER TABLE games
    ADD COLUMN created_by_user_id UUID REFERENCES users(user_id) ON DELETE SET NULL,
    ADD COLUMN rematch_battlesnake_ids UUID[],
    ADD CONSTRAINT games_rematch_battlesnake_ids_count
        CHECK (rematch_battlesnake_ids IS NULL OR cardinality(rematch_battlesnake_ids) BETWEEN 1 AND 4);

CREATE INDEX games_created_by_user_id_idx
    ON games(created_by_user_id)
    WHERE created_by_user_id IS NOT NULL;
