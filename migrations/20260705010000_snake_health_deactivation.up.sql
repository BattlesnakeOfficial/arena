-- Snake auto-deactivation from leaderboard matchmaking (BS-3534).
--
-- The health sweeper periodically probes every snake with an active
-- leaderboard entry. After enough consecutive failed probes it disables the
-- snake's entries so a dead server stops tanking its rating, and emails the
-- owner once per deactivation.

-- Distinguishes sweeper-disabled entries from owner-paused ones so
-- reactivation can re-enable exactly what the sweeper disabled and never
-- resurrect a manual pause. NULL for manual pauses; 'health' when the
-- sweeper pulled the snake.
ALTER TABLE leaderboard_entries
    ADD COLUMN disabled_reason TEXT
        CHECK (disabled_reason IS NULL OR disabled_at IS NOT NULL);

-- One row per snake the sweeper has ever probed. `deactivated_at` doubles as
-- the notification gate: the sweeper only sends the owner email on the
-- NULL -> NOW() transition, so job retries can't spam.
CREATE TABLE snake_health_status (
    battlesnake_id UUID PRIMARY KEY REFERENCES battlesnakes (battlesnake_id) ON DELETE CASCADE,
    consecutive_failures INT NOT NULL DEFAULT 0,
    last_checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Human-readable summary of the most recent failed probe, shown to the
    -- owner on the snake profile and in the notification email.
    last_failure TEXT,
    deactivated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
