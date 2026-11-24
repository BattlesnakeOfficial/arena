-- DEV-2206 (contract half of DEV-2185's GitHub token purge): drop the
-- unused OAuth token columns from users (the credentials that leaked in
-- DEV-2165; Corey authorized the purge). DEV-2185 stopped writing them
-- and its revision now serves 100% of traffic, so nothing INSERTs them
-- any more and the columns can go.
--
-- Re-blank first: the previous revision still wrote these columns
-- during DEV-2185's rollout overlap, so a fresh blanking covers
-- anything it wrote in that window before the DROP makes the bytes
-- unreferenced. Pattern mirrors 20261108160001_stop_writing_github_tokens.
-- Locks (all held until this transaction commits): DISABLE/ENABLE
-- TRIGGER take SHARE ROW EXCLUSIVE, which blocks writers but not
-- readers for the whole transaction; the UPDATE itself runs under ROW
-- EXCLUSIVE and does not block readers; the DROP COLUMN ALTERs are a
-- momentary metadata-only ACCESS EXCLUSIVE at the very end. The
-- updated_at trigger is disabled around the UPDATE so this maintenance
-- write does not bump every user's "last updated" (shown on /me).
ALTER TABLE users DISABLE TRIGGER update_users_updated_at;

UPDATE users
SET github_access_token = '',
    github_refresh_token = NULL,
    github_token_expires_at = NULL;

ALTER TABLE users ENABLE TRIGGER update_users_updated_at;

ALTER TABLE users
  DROP COLUMN github_access_token,
  DROP COLUMN github_refresh_token,
  DROP COLUMN github_token_expires_at;
