-- Recreates the empty table (schema of 20260705010000 + 20260715140000).
-- The data lives on in leaderboard_entries.health_*.
CREATE TABLE snake_health_status (
    battlesnake_id UUID PRIMARY KEY REFERENCES battlesnakes (battlesnake_id) ON DELETE CASCADE,
    consecutive_failures INT NOT NULL DEFAULT 0,
    last_checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_failure TEXT,
    deactivated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    consecutive_successes INTEGER NOT NULL DEFAULT 0
);
