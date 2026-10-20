-- Enforce: from this deploy on, token scopes are read. Tokens that the
-- pre-expand revision minted while the expand deploy rolled out have no
-- permission rows. Give them the default set before anything reads scopes.
-- Every token minted since the expand deploy already has its rows (and
-- nothing can mint a zero-permission token yet), so "no rows" means exactly
-- those tokens.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

WITH bare AS (
    SELECT t.id, u.is_admin
    FROM api_tokens t
    JOIN users u ON u.user_id = t.user_id
    WHERE NOT EXISTS (
        SELECT 1 FROM api_token_permissions p WHERE p.api_token_id = t.id
    )
)
INSERT INTO api_token_permissions (api_token_id, permission)
SELECT bare.id, p.permission
FROM bare
CROSS JOIN (VALUES
    ('profile.read'), ('snakes.read'), ('snakes.create'),
    ('games.read'), ('games.create'), ('tokens.manage'),
    ('snake.url.read'), ('snake.url.write'), ('snake.edit'),
    ('snake.delete'), ('snake.compete')
) AS p(permission)
UNION ALL
SELECT bare.id, 'admin' FROM bare WHERE bare.is_admin;
