-- Leaderboards table
CREATE TABLE leaderboards (
    leaderboard_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    disabled_at TIMESTAMPTZ NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TRIGGER update_leaderboards_updated_at
    BEFORE UPDATE ON leaderboards
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Leaderboard entries: one row per snake per leaderboard
CREATE TABLE leaderboard_entries (
    leaderboard_entry_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_id UUID NOT NULL REFERENCES leaderboards(leaderboard_id) ON DELETE CASCADE,
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    mu DOUBLE PRECISION NOT NULL DEFAULT 25.0,
    sigma DOUBLE PRECISION NOT NULL DEFAULT 8.333,
    display_score DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    games_played INT NOT NULL DEFAULT 0,
    wins INT NOT NULL DEFAULT 0,
    losses INT NOT NULL DEFAULT 0,
    disabled_at TIMESTAMPTZ NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (leaderboard_id, battlesnake_id)
);

CREATE TRIGGER update_leaderboard_entries_updated_at
    BEFORE UPDATE ON leaderboard_entries
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Partial index for ranking queries on active entries
CREATE INDEX idx_leaderboard_entries_ranking
    ON leaderboard_entries (leaderboard_id, display_score DESC)
    WHERE disabled_at IS NULL;

-- Leaderboard games: links a game to a leaderboard
CREATE TABLE leaderboard_games (
    leaderboard_game_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_id UUID NOT NULL REFERENCES leaderboards(leaderboard_id) ON DELETE CASCADE,
    game_id UUID NOT NULL UNIQUE REFERENCES games(game_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Leaderboard game results: per-snake rating changes (audit trail)
CREATE TABLE leaderboard_game_results (
    leaderboard_game_result_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_game_id UUID NOT NULL REFERENCES leaderboard_games(leaderboard_game_id) ON DELETE CASCADE,
    leaderboard_entry_id UUID NOT NULL REFERENCES leaderboard_entries(leaderboard_entry_id) ON DELETE CASCADE,
    placement INT NOT NULL,
    mu_before DOUBLE PRECISION NOT NULL,
    mu_after DOUBLE PRECISION NOT NULL,
    sigma_before DOUBLE PRECISION NOT NULL,
    sigma_after DOUBLE PRECISION NOT NULL,
    display_score_change DOUBLE PRECISION NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the initial Standard 11x11 leaderboard
INSERT INTO leaderboards (name) VALUES ('Standard 11x11');
