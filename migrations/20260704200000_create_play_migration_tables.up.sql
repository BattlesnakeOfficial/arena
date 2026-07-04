-- Staging tables for the play -> arena account migration. The importer
-- copies play's Postgres into these; nothing here is live until a user
-- claims the account (auto-link by GitHub ID, or email+password claim),
-- at which point snakes/grants/profile are materialized into the real
-- arena tables.

-- Arena-side display identity: play usernames migrate here. NULL falls
-- back to github_login in the UI.
ALTER TABLE users ADD COLUMN display_name TEXT;

CREATE TABLE imported_accounts (
    imported_account_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    play_user_id TEXT NOT NULL UNIQUE,
    play_account_id TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL,
    -- Django password hash string (pbkdf2_sha256$iter$salt$b64). Empty =
    -- no usable password (OAuth-only play account); such accounts can only
    -- auto-link by GitHub ID or go through admin support.
    password_hash TEXT NOT NULL DEFAULT '',
    is_email_verified BOOLEAN NOT NULL DEFAULT false,
    username TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    pronouns TEXT NOT NULL DEFAULT '',
    country TEXT NOT NULL DEFAULT '',
    backstory TEXT NOT NULL DEFAULT '',
    -- GitHub numeric user ID from play's social auth link; matches arena's
    -- users.external_github_id for zero-friction auto-claim.
    github_uid BIGINT,
    github_login TEXT,
    -- Points balances imported for the future economy rework; NOT
    -- materialized anywhere on claim yet.
    points INTEGER NOT NULL DEFAULT 0,
    points_high_score INTEGER NOT NULL DEFAULT 0,
    is_staff BOOLEAN NOT NULL DEFAULT false,
    play_created_at TIMESTAMPTZ,
    claimed_by_user_id UUID REFERENCES users (user_id) ON DELETE SET NULL,
    claimed_at TIMESTAMPTZ,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- social_django guarantees (provider, uid) uniqueness, so at most one play
-- account per GitHub identity.
CREATE UNIQUE INDEX imported_accounts_github_uid_key
    ON imported_accounts (github_uid) WHERE github_uid IS NOT NULL;
CREATE INDEX imported_accounts_email_idx ON imported_accounts (lower(email));
CREATE INDEX imported_accounts_claimed_by_idx ON imported_accounts (claimed_by_user_id);

CREATE TRIGGER update_imported_accounts_updated_at BEFORE
UPDATE ON imported_accounts FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE imported_snakes (
    imported_snake_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    imported_account_id UUID NOT NULL
        REFERENCES imported_accounts (imported_account_id) ON DELETE CASCADE,
    play_snake_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    head TEXT NOT NULL DEFAULT '',
    tail TEXT NOT NULL DEFAULT '',
    color TEXT NOT NULL DEFAULT '',
    is_public BOOLEAN NOT NULL DEFAULT false,
    -- Set when the claim flow creates the real battlesnake row.
    materialized_battlesnake_id UUID
        REFERENCES battlesnakes (battlesnake_id) ON DELETE SET NULL,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX imported_snakes_account_idx ON imported_snakes (imported_account_id);

CREATE TRIGGER update_imported_snakes_updated_at BEFORE
UPDATE ON imported_snakes FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

-- Grants staged as (type, slug) rather than FKs so the import doesn't
-- depend on catalog IDs; resolved against customizations at claim time.
CREATE TABLE imported_grants (
    imported_grant_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    imported_account_id UUID NOT NULL
        REFERENCES imported_accounts (imported_account_id) ON DELETE CASCADE,
    customization_type TEXT NOT NULL CHECK (customization_type IN ('head', 'tail')),
    slug TEXT NOT NULL,
    UNIQUE (imported_account_id, customization_type, slug)
);

-- Password-claim attempts, for rate limiting. Rate limited on BOTH
-- dimensions: per arena user (one account can't enumerate many emails)
-- and per target email (many arena accounts can't brute-force one play
-- email — arena login is GitHub-only, so per-user alone would let an
-- attacker mint fresh budgets against a single victim). Every attempt is
-- recorded before verification, so the count is race-safe.
CREATE TABLE claim_attempts (
    claim_attempt_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX claim_attempts_user_idx ON claim_attempts (user_id, attempted_at);
CREATE INDEX claim_attempts_email_idx ON claim_attempts (lower(email), attempted_at);
