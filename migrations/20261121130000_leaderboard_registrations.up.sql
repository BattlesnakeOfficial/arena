SET LOCAL lock_timeout = '5s';

-- League membership: one row per (league, snake), independent of seasons.
-- Per-season ratings stay on leaderboard_entries.
CREATE TABLE leaderboard_registrations (
    leaderboard_registration_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_id UUID NOT NULL REFERENCES leaderboards(leaderboard_id) ON DELETE CASCADE,
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    disabled_at TIMESTAMPTZ,
    disabled_reason TEXT,
    health_consecutive_failures INT NOT NULL DEFAULT 0,
    health_consecutive_successes INT NOT NULL DEFAULT 0,
    health_last_checked_at TIMESTAMPTZ,
    health_last_failure TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT leaderboard_registrations_league_snake_key UNIQUE (leaderboard_id, battlesnake_id)
);
CREATE TRIGGER update_leaderboard_registrations_updated_at
    BEFORE UPDATE ON leaderboard_registrations
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
CREATE INDEX leaderboard_registrations_battlesnake_idx ON leaderboard_registrations (battlesnake_id);
