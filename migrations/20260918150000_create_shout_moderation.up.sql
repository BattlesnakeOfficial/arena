-- Post-game shout screening (DEV-1297). shout_screenings is the one-row-per-game
-- idempotency marker + metrics record; suppressed_shouts is the serve-time
-- suppression set. snake_id is the frame's snake ID verbatim (a
-- game_battlesnake_id string in games this runner produced) stored as TEXT
-- because archived/imported frames can carry other id shapes — deliberately no FK.
CREATE TABLE shout_screenings (
    game_id UUID PRIMARY KEY REFERENCES games (game_id) ON DELETE CASCADE,
    outcome TEXT NOT NULL,              -- screened | no_shouts | jev_error
    distinct_shout_count INT NOT NULL,
    judged_count INT NOT NULL,
    suppressed_count INT NOT NULL,
    over_cap_count INT NOT NULL,
    model TEXT,
    latency_ms DOUBLE PRECISION,
    input_tokens BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW ()
);

CREATE TABLE suppressed_shouts (
    suppressed_shout_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    game_id UUID NOT NULL REFERENCES games (game_id) ON DELETE CASCADE,
    snake_id TEXT NOT NULL,
    text_hash TEXT NOT NULL,            -- sha256 hex of the shout text
    text TEXT NOT NULL,
    probability DOUBLE PRECISION,       -- NULL for over-cap (unjudged) suppressions
    model TEXT,                         -- reported model; 'over-cap' for unjudged
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW (),
    UNIQUE (game_id, snake_id, text_hash)
);

CREATE INDEX idx_suppressed_shouts_game ON suppressed_shouts (game_id);
