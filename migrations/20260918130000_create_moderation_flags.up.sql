-- DOUBLE PRECISION (FLOAT8) not REAL (FLOAT4): sqlx maps FLOAT4 <-> f32 and
-- FLOAT8 <-> f64; the Rust types are Option<f64>, and query! type-checking
-- rejects an f64 bind/decode against a REAL column at compile time.
CREATE TABLE moderation_flags (
    moderation_flag_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    field_kind TEXT NOT NULL,              -- snake_name | tournament_name | tournament_description | saved_game_title
    text TEXT NOT NULL,
    subject_id UUID,                       -- battlesnake/tournament/saved_game id when known; NULL on creates (moderated pre-insert). No FK: polymorphic.
    user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    decision TEXT NOT NULL,                -- blocked | flagged | unchecked
    hate_or_slur DOUBLE PRECISION,
    sexual_or_graphic DOUBLE PRECISION,
    harassment_or_threat DOUBLE PRECISION,
    impersonates_staff_or_platform DOUBLE PRECISION,
    disguised_evasion DOUBLE PRECISION,
    action_choice TEXT,                    -- allow | flag_for_review | block (Jev's chosen action)
    action_confidence DOUBLE PRECISION,    -- distribution concentration (recorded, not load-bearing)
    action_block_mass DOUBLE PRECISION,    -- probabilities["block"] — the decision signal
    model TEXT,                            -- reported model, requested model on failure, 'hardblock' for the local list
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW (),
    reviewed_at TIMESTAMPTZ,
    review_outcome TEXT
);

CREATE INDEX idx_moderation_flags_created_at ON moderation_flags (created_at DESC);
CREATE INDEX idx_moderation_flags_unreviewed ON moderation_flags (created_at DESC) WHERE reviewed_at IS NULL;
