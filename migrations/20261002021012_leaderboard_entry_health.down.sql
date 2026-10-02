COMMENT ON TABLE snake_health_status IS NULL;

ALTER TABLE leaderboard_entries
    DROP COLUMN health_last_failure,
    DROP COLUMN health_last_checked_at,
    DROP COLUMN health_consecutive_successes,
    DROP COLUMN health_consecutive_failures;
