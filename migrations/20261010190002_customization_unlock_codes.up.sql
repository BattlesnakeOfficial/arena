SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

ALTER TABLE customization_grants DROP CONSTRAINT customization_grants_source_check;
ALTER TABLE customization_grants ADD CONSTRAINT customization_grants_source_check
    CHECK (source IN ('pre_token', 'play_import', 'admin', 'token', 'achievement', 'code'));

CREATE TABLE customization_unlock_codes (
    code_id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    code_hash TEXT NOT NULL UNIQUE,
    customization_type TEXT NOT NULL CHECK (customization_type IN ('head', 'tail')),
    slug TEXT NOT NULL CHECK (slug <> ''),
    max_redemptions INTEGER NOT NULL CHECK (max_redemptions >= 1),
    redemptions_used INTEGER NOT NULL DEFAULT 0 CHECK (redemptions_used >= 0 AND redemptions_used <= max_redemptions),
    expires_at TIMESTAMPTZ,
    note TEXT,
    created_by_user_id UUID REFERENCES users(user_id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    disabled_at TIMESTAMPTZ
);

CREATE TABLE customization_code_redemptions (
    code_id UUID NOT NULL REFERENCES customization_unlock_codes(code_id),
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    redeemed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (code_id, user_id)
);

CREATE TABLE customization_code_failed_attempts (
    attempt_id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX customization_code_failed_attempts_user_time_idx
    ON customization_code_failed_attempts (user_id, attempted_at);
