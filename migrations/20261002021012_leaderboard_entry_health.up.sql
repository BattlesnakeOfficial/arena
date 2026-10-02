-- Health-sweeper state moves from per snake (snake_health_status) to per
-- leaderboard entry (DEV-1515). The sweeper now probes an entry with a test
-- game shaped like its leaderboard (mode, board size, match size), so a
-- snake that only breaks on Royale is pulled from Royale alone.
--
-- The entry itself is the deactivation marker (disabled_reason = 'health');
-- these columns are just the streaks that decide when to flip it:
--   health_consecutive_failures   failed probes in a row while in matchmaking
--   health_consecutive_successes  passing probes in a row while health-paused
--   health_last_checked_at        last probe; also the cursor for "new bad
--                                 turns since we last looked" (NULL = never)
--   health_last_failure           what the most recent failed probe saw
--
-- Constant defaults make the column adds metadata-only; the lock timeout
-- keeps the brief ACCESS EXCLUSIVE lock from queueing rating updates
-- behind it indefinitely.
SET LOCAL lock_timeout = '5s';

ALTER TABLE leaderboard_entries
    ADD COLUMN health_consecutive_failures INT NOT NULL DEFAULT 0,
    ADD COLUMN health_consecutive_successes INT NOT NULL DEFAULT 0,
    ADD COLUMN health_last_checked_at TIMESTAMPTZ,
    ADD COLUMN health_last_failure TEXT;

-- Carry the per-snake streaks over to the entries they describe:
--   * health-paused entries keep both streaks (recovery continues);
--   * enabled entries keep a partial failure streak, unless the snake was
--     deactivated (then the owner resumed this entry by hand: start fresh);
--   * manual pauses and deleted-snake entries are never probed: skip.
-- No entry's disabled_* changes, so no notification fires.
UPDATE leaderboard_entries le
SET health_consecutive_failures = s.consecutive_failures,
    health_consecutive_successes = CASE
        WHEN le.disabled_reason = 'health' THEN s.consecutive_successes
        ELSE 0
    END,
    health_last_checked_at = s.last_checked_at,
    health_last_failure = s.last_failure
FROM snake_health_status s
WHERE s.battlesnake_id = le.battlesnake_id
  AND (
    (le.disabled_at IS NOT NULL AND le.disabled_reason = 'health')
    OR (le.disabled_at IS NULL AND s.deactivated_at IS NULL)
  );

-- snake_health_status stays until the old revision is gone (it reads the
-- table during the rollout); a follow-up migration drops it.
COMMENT ON TABLE snake_health_status IS
    'Deprecated (DEV-1515): superseded by leaderboard_entries.health_*; drop after rollout.';
