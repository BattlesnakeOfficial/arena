-- Contract: every running revision (enforce and later) writes snake_access
-- explicitly, so drop the expand-era DEFAULT. A missed insert site now fails
-- loudly instead of silently minting an `all`-access token.
SET LOCAL lock_timeout = '5s';

ALTER TABLE api_tokens ALTER COLUMN snake_access DROP DEFAULT;
