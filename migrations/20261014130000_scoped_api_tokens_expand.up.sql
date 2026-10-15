-- Expand: add token scope storage alongside today's behavior. Nothing reads
-- these tables until the next deploy, so the previous revision keeps working
-- unchanged while this one rolls out.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

-- The DEFAULT stays until the contract deploy: the previous revision keeps
-- inserting tokens without this column while this one rolls out.
ALTER TABLE api_tokens
    ADD COLUMN snake_access TEXT NOT NULL DEFAULT 'all'
        CHECK (snake_access IN ('all', 'selected'));

CREATE TABLE api_token_permissions (
    api_token_id UUID NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    permission TEXT NOT NULL CHECK (permission IN (
        'profile.read', 'email.read', 'snakes.read', 'snakes.create',
        'games.read', 'games.create', 'tokens.manage', 'admin',
        'snake.url.read', 'snake.url.write', 'snake.edit',
        'snake.delete', 'snake.compete'
    )),
    PRIMARY KEY (api_token_id, permission)
);

CREATE TABLE api_token_snakes (
    api_token_id UUID NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    PRIMARY KEY (api_token_id, battlesnake_id)
);

-- The battlesnakes FK cascade (and reverse lookups) needs this: the composite
-- PK leads with api_token_id.
CREATE INDEX api_token_snakes_battlesnake_id_idx ON api_token_snakes (battlesnake_id);

-- Backfill: existing tokens keep exactly the power they have. No token can
-- read the user's email today, so `email.read` is not granted by default.
-- Tokens the previous revision mints during this rollout get no rows here;
-- the enforce deploy's migration backfills them before anything reads scopes.
INSERT INTO api_token_permissions (api_token_id, permission)
SELECT t.id, p.permission
FROM api_tokens t
CROSS JOIN (VALUES
    ('profile.read'), ('snakes.read'), ('snakes.create'),
    ('games.read'), ('games.create'), ('tokens.manage'),
    ('snake.url.read'), ('snake.url.write'), ('snake.edit'),
    ('snake.delete'), ('snake.compete')
) AS p(permission);

INSERT INTO api_token_permissions (api_token_id, permission)
SELECT t.id, 'admin'
FROM api_tokens t
JOIN users u ON u.user_id = t.user_id
WHERE u.is_admin;
