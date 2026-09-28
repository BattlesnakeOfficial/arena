CREATE TABLE user_activity_days (
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    day DATE NOT NULL,
    PRIMARY KEY (user_id, day)
);

CREATE INDEX user_activity_days_day_user_idx ON user_activity_days(day, user_id);

CREATE TABLE stats_tracking_start (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    tracking_started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    backfill_completed_at TIMESTAMPTZ NULL
);

INSERT INTO stats_tracking_start(singleton) VALUES (TRUE);

CREATE TABLE stats_activity_backfill_progress (
    source TEXT PRIMARY KEY,
    cursor_id UUID NULL,
    done BOOLEAN NOT NULL DEFAULT FALSE
);

INSERT INTO stats_activity_backfill_progress(source) VALUES
    ('sessions'), ('games'), ('battlesnakes'), ('saved_games'),
    ('tournament_registrations'), ('leaderboard_entries');
