CREATE TABLE weng_lin_ratings (
    weng_lin_rating_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_entry_id UUID NOT NULL REFERENCES leaderboard_entries(leaderboard_entry_id) ON DELETE CASCADE,
    mu DOUBLE PRECISION NOT NULL DEFAULT 25.0,
    sigma DOUBLE PRECISION NOT NULL DEFAULT 8.333,
    display_score DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (leaderboard_entry_id)
);

INSERT INTO weng_lin_ratings (leaderboard_entry_id, mu, sigma, display_score)
SELECT leaderboard_entry_id, mu, sigma, display_score
FROM leaderboard_entries;
