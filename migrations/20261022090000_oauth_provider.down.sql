-- Fail closed: old (pre-OAuth) code would ignore oauth_grant_id entirely,
-- and the snake.delete "app-created-only" restriction would revert to plain
-- delete (a real privilege expansion). Revoke every OAuth-minted token first.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

LOCK TABLE api_tokens IN SHARE ROW EXCLUSIVE MODE;
UPDATE api_tokens SET revoked_at = NOW()
    WHERE revoked_at IS NULL AND oauth_grant_id IS NOT NULL;

ALTER TABLE sessions DROP COLUMN post_login_redirect;
ALTER TABLE battlesnakes DROP COLUMN created_by_oauth_client_id;
DROP TABLE oauth_authorization_code_snakes;
DROP TABLE oauth_authorization_code_permissions;
DROP TABLE oauth_authorization_codes;
-- Must drop before oauth_grants or the FK blocks it (drops both indexes too).
ALTER TABLE api_tokens DROP COLUMN oauth_grant_id;
DROP TABLE oauth_grants;
DROP TABLE oauth_client_redirect_uris;
DROP TABLE oauth_clients;
