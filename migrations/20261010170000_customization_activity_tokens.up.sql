SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

CREATE TABLE customization_active_weeks (
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    week_start DATE NOT NULL CHECK (EXTRACT(ISODOW FROM week_start) = 1),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, week_start)
);

ALTER TABLE customization_grants
    ADD COLUMN source TEXT NOT NULL DEFAULT 'pre_token'
    CHECK (source IN ('pre_token', 'play_import', 'admin', 'token'));
ALTER TABLE customization_grants ALTER COLUMN source DROP DEFAULT;

CREATE INDEX game_battlesnakes_created_at_idx ON game_battlesnakes (created_at);

CREATE TABLE customization_active_week_backfill_cursor (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    scanned_through TIMESTAMPTZ NOT NULL
);
INSERT INTO customization_active_week_backfill_cursor (singleton, scanned_through)
VALUES (TRUE, (SELECT COALESCE(MIN(created_at), NOW()) FROM game_battlesnakes));

ALTER TABLE games ADD COLUMN finished_at TIMESTAMPTZ;
