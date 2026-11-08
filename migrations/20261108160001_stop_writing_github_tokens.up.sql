-- DEV-2185 (expand half): the GitHub OAuth token columns are written on
-- every login and never read (the credentials that leaked in DEV-2165).
-- Corey authorized the purge. The code stops writing them in this same
-- deploy; this migration (a) gives the still-NOT-NULL
-- github_access_token a default so the new INSERT that omits it works,
-- and (b) blanks existing values so the plaintext bytes are overwritten
-- in the live heap pages. The columns are NOT dropped here: the previous
-- revision keeps serving during rollout and still writes them — that is
-- harmless. The drop is DEV-2206, started after this PR serves 100% of
-- traffic (it re-blanks first to cover this window).
ALTER TABLE users ALTER COLUMN github_access_token SET DEFAULT '';

UPDATE users
SET github_access_token = '',
    github_refresh_token = NULL,
    github_token_expires_at = NULL;
