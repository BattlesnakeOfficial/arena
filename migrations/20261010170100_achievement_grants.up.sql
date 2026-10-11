SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

ALTER TABLE customization_grants DROP CONSTRAINT customization_grants_source_check;
ALTER TABLE customization_grants ADD CONSTRAINT customization_grants_source_check
    CHECK (source IN ('pre_token', 'play_import', 'admin', 'token', 'achievement'));

CREATE TABLE achievement_backfill_cursor (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    after_user_id UUID NULL,
    completed_at TIMESTAMPTZ NULL
);
INSERT INTO achievement_backfill_cursor (singleton, after_user_id, completed_at)
VALUES (TRUE, NULL, NULL);
