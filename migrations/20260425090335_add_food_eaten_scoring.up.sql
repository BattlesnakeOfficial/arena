CREATE TABLE food_eaten_stats (
    food_eaten_stat_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_entry_id UUID NOT NULL REFERENCES leaderboard_entries(leaderboard_entry_id) ON DELETE CASCADE,
    food_score BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (leaderboard_entry_id)
);

INSERT INTO food_eaten_stats (leaderboard_entry_id)
SELECT leaderboard_entry_id FROM leaderboard_entries
ON CONFLICT (leaderboard_entry_id) DO NOTHING;

ALTER TABLE leaderboard_game_results
    ADD COLUMN food_eaten INT NOT NULL DEFAULT 0;
