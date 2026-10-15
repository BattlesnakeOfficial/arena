-- Nothing reads scopes at this stage (enforcement ships in the next deploy),
-- so dropping them changes no token's power.
SET LOCAL lock_timeout = '5s';

DROP TABLE IF EXISTS api_token_snakes;
DROP TABLE IF EXISTS api_token_permissions;
ALTER TABLE api_tokens DROP COLUMN IF EXISTS snake_access;
