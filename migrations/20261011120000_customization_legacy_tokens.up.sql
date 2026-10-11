CREATE TABLE customization_legacy_tokens (
    imported_account_id UUID PRIMARY KEY
        REFERENCES imported_accounts(imported_account_id) ON DELETE RESTRICT,
    play_points INTEGER NOT NULL CHECK (play_points > 0),
    token_count BIGINT NOT NULL
        CHECK (token_count > 0 AND token_count = (play_points::bigint + 199) / 200),
    converted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
