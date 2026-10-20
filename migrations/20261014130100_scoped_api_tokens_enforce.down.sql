SET LOCAL lock_timeout = '5s';

-- Make the revoke a clean cut: INSERT/UPDATE on api_tokens take ROW
-- EXCLUSIVE, so no token is created while the UPDATE below runs. The lock
-- ends at COMMIT and does NOT stop a revision that is still serving from
-- minting afterwards: an insert queued behind it commits right after, and the
-- tables stay, so it survives. So the rollback runbook runs this only once
-- the enforce code is the only code serving (the contract code mints narrow
-- tokens directly). The enforce code's copy-mint re-checks that its caller is
-- still live after the INSERT and revokes a copy that escaped this revoke
-- before returning its secret. The runbook then re-runs this UPDATE by hand
-- as a check before deploying the expand code.
LOCK TABLE api_tokens IN SHARE ROW EXCLUSIVE MODE;

-- Reverting enforcement fails open unless narrow tokens go first: the expand
-- revision treats every live token as full account access, and its admin
-- routes check only users.is_admin. So revoke every live token it would grant
-- more than the token holds. The survivors have `all` access plus the 11
-- permissions other than `admin` and `email.read` (the expand revision never
-- exposes email, so holding it changes nothing there), plus `admin` iff the
-- owner is an admin. An admin-owned token without `admin` would gain the
-- admin API.
UPDATE api_tokens t
SET revoked_at = NOW()
FROM users u
WHERE u.user_id = t.user_id
  AND t.revoked_at IS NULL
  AND (
    t.snake_access <> 'all'
    OR (
      SELECT count(*) FROM api_token_permissions p
      WHERE p.api_token_id = t.id AND p.permission NOT IN ('admin', 'email.read')
    ) < 11
    OR (
      u.is_admin
      AND NOT EXISTS (
        SELECT 1 FROM api_token_permissions p
        WHERE p.api_token_id = t.id AND p.permission = 'admin'
      )
    )
  );
