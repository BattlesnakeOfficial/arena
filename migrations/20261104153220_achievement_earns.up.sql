SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

-- Durable record of an achievement rule fire: the game, snake, and turn
-- that earned it live, or a grant-dated row for pre-feature holders.
-- customization_grants remains the source of truth for what a snake may
-- wear (the ladder page still derives Unlocked from ownership); nothing
-- gates on this table yet.
CREATE TABLE achievement_earns (
    achievement_earn_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    achievement TEXT NOT NULL CHECK (achievement <> ''),
    battlesnake_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE SET NULL,
    game_id UUID REFERENCES games(game_id) ON DELETE SET NULL,
    turn_number INTEGER,
    earned_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    live BOOLEAN NOT NULL,
    -- Non-live rows are grant-dated history: no game, snake, or turn.
    CHECK (live OR (game_id IS NULL AND battlesnake_id IS NULL AND turn_number IS NULL)),
    UNIQUE (user_id, achievement)
);

-- Earliest earner per achievement.
CREATE INDEX achievement_earns_achievement_earned_at_idx
    ON achievement_earns (achievement, earned_at);
