-- Arena as an OAuth 2 provider: clients, grants, single-use authorization
-- codes (stored hashed), and the api_tokens/battlesnakes columns the flow
-- needs. Nothing here is read until the OAuth routes ship in this same
-- deploy; the previous revision ignores every new table and column.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

CREATE TABLE oauth_clients (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    -- Wire-format "client_id" (32 hex chars). Every UUID FK to this table is
    -- named oauth_client_id so "client_id" never means two things.
    public_client_id TEXT NOT NULL UNIQUE,
    -- SHA-256 hex of the 32-byte random secret, like api_tokens.token_hash.
    secret_hash TEXT NOT NULL,
    name TEXT NOT NULL,
    logo_url TEXT,
    homepage_url TEXT,
    kind TEXT NOT NULL DEFAULT 'standard' CHECK (kind IN ('standard', 'hosting_provider')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    disabled_at TIMESTAMPTZ
);

-- Registered redirect targets: exact string match only.
CREATE TABLE oauth_client_redirect_uris (
    oauth_client_id UUID NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
    uri TEXT NOT NULL,
    PRIMARY KEY (oauth_client_id, uri)
);

-- A player's standing consent to one client. One active row per
-- (user, client); the grant's live scope is the scope of its current
-- api_tokens row, never copied here.
CREATE TABLE oauth_grants (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    oauth_client_id UUID NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ
);
CREATE UNIQUE INDEX oauth_grants_one_active_per_user_client
    ON oauth_grants (user_id, oauth_client_id) WHERE revoked_at IS NULL;
CREATE TRIGGER update_oauth_grants_updated_at BEFORE UPDATE ON oauth_grants
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

-- Single-use, 10-minute authorization codes. Stored hashed (SHA-256 hex of
-- the raw code); approved scope rides in the two join tables below.
CREATE TABLE oauth_authorization_codes (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    oauth_client_id UUID NOT NULL REFERENCES oauth_clients(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL,
    code_hash TEXT NOT NULL UNIQUE,
    code_challenge TEXT NOT NULL,
    snake_access TEXT NOT NULL DEFAULT 'all' CHECK (snake_access IN ('all', 'selected')),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    -- What token this code minted, so a replay revokes it (RFC 6749 4.1.2).
    issued_token_id UUID REFERENCES api_tokens(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The CHECK list is the 11 OAuth-requestable permission names (every
-- permission except tokens.manage and admin) — keep it in sync with
-- Permission::OAUTH_REQUESTABLE when permissions are added.
CREATE TABLE oauth_authorization_code_permissions (
    oauth_authorization_code_id UUID NOT NULL REFERENCES oauth_authorization_codes(id) ON DELETE CASCADE,
    permission TEXT NOT NULL CHECK (permission IN (
        'profile.read', 'email.read', 'snakes.read', 'snakes.create',
        'games.read', 'games.create',
        'snake.url.read', 'snake.url.write', 'snake.edit',
        'snake.delete', 'snake.compete'
    )),
    PRIMARY KEY (oauth_authorization_code_id, permission)
);

-- The battlesnakes FK cascade (and reverse lookups) needs this: the composite
-- PK leads with oauth_authorization_code_id.
CREATE TABLE oauth_authorization_code_snakes (
    oauth_authorization_code_id UUID NOT NULL REFERENCES oauth_authorization_codes(id) ON DELETE CASCADE,
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    PRIMARY KEY (oauth_authorization_code_id, battlesnake_id)
);
CREATE INDEX oauth_authorization_code_snakes_battlesnake_id_idx
    ON oauth_authorization_code_snakes (battlesnake_id);

-- How a token says "I am an OAuth token for this grant/client". Enforces the
-- "one active grant ⇔ exactly one live token" invariant.
ALTER TABLE api_tokens
    ADD COLUMN oauth_grant_id UUID REFERENCES oauth_grants(id);
CREATE INDEX api_tokens_oauth_grant_id_idx ON api_tokens (oauth_grant_id);
CREATE UNIQUE INDEX api_tokens_one_live_per_grant
    ON api_tokens (oauth_grant_id)
    WHERE revoked_at IS NULL AND oauth_grant_id IS NOT NULL;

-- Set only when a snake is created through an OAuth token: snake.delete for
-- OAuth tokens applies just to snakes their own client created.
ALTER TABLE battlesnakes
    ADD COLUMN created_by_oauth_client_id UUID REFERENCES oauth_clients(id);

-- The saved /oauth/authorize?... URL for login-and-return. Written/read only
-- through dedicated model functions; never a Session struct field.
ALTER TABLE sessions ADD COLUMN post_login_redirect TEXT;
