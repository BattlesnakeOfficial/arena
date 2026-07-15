-- Recovery streak for health-deactivated snakes: the sweeper keeps probing
-- them and auto-reactivates after enough consecutive healthy probes.
ALTER TABLE snake_health_status
    ADD COLUMN consecutive_successes INTEGER NOT NULL DEFAULT 0;
