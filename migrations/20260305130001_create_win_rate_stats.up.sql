CREATE TABLE win_rate_stats (
    win_rate_stat_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_entry_id UUID NOT NULL REFERENCES leaderboard_entries(leaderboard_entry_id) ON DELETE CASCADE,
    wins INT NOT NULL DEFAULT 0,
    losses INT NOT NULL DEFAULT 0,
    games_played INT NOT NULL DEFAULT 0,
    score DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (leaderboard_entry_id)
);

INSERT INTO win_rate_stats (leaderboard_entry_id, wins, losses, games_played, score)
SELECT
    leaderboard_entry_id,
    first_place_finishes,
    non_first_finishes,
    games_played,
    CASE WHEN games_played > 0
         THEN first_place_finishes::double precision / games_played::double precision * 100.0
         ELSE 0.0
    END
FROM leaderboard_entries;
