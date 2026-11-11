-- Rollback only: re-add the dropped OAuth token columns with the shape
-- they had after DEV-2185 (github_access_token NOT NULL DEFAULT '').
-- Values are NOT restored — DEV-2185's blanking was irreversible.
-- Re-added columns land at the end of the table rather than their
-- original 2025 position; all INSERTs/SELECTs here are name-based, so
-- order does not matter.
ALTER TABLE users
  ADD COLUMN github_access_token TEXT NOT NULL DEFAULT '',
  ADD COLUMN github_refresh_token TEXT,
  ADD COLUMN github_token_expires_at TIMESTAMPTZ;
