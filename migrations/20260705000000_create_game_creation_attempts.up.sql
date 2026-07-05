-- Game-creation attempts, for per-account rate limiting across every
-- user-triggerable entry point (web flow, API, tournament rounds). Every
-- attempt is recorded before the limit is checked, so the count is
-- race-safe: concurrent requests each insert first and therefore see each
-- other instead of all reading a stale count and sailing past the gate.
-- Failed/rejected attempts still count.
--
-- Every request inserts a row (including rejected ones — that's the
-- race-safety), so the table grows with traffic; RateLimitPruneJob deletes
-- rows past retention on a cron.
CREATE TABLE game_creation_attempts (
    game_creation_attempt_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    user_id UUID NOT NULL REFERENCES users (user_id) ON DELETE CASCADE,
    -- Which entry point recorded the attempt: 'web', 'api', or
    -- 'tournament'. Informational only — the limit is shared per account
    -- across all sources.
    source TEXT NOT NULL,
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX game_creation_attempts_user_idx
    ON game_creation_attempts (user_id, attempted_at);
