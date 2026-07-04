-- Game-creation attempts, for per-account rate limiting across both entry
-- points (web flow and API). Every attempt is recorded before the limit is
-- checked, so the count is race-safe: concurrent requests each insert
-- first and therefore see each other instead of all reading a stale count
-- and sailing past the gate. Failed/rejected attempts still count.
--
-- Rows are only ever read through the trailing rate-limit window and never
-- deleted; growth is bounded by the limit itself. A periodic prune of rows
-- older than the window would keep it tidy — not wired up here.
CREATE TABLE game_creation_attempts (
    game_creation_attempt_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    -- Which entry point recorded the attempt: 'web' or 'api'. Informational
    -- only — the limit is shared per account across both sources.
    source TEXT NOT NULL,
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX game_creation_attempts_user_idx
    ON game_creation_attempts (user_id, attempted_at);
