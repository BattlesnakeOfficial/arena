-- Live resume state for in-flight (Running) games: the engine-authoritative
-- board at the last committed turn, plus per-snake runner bookkeeping.
-- Written inside persist_turn's transaction; deleted at finish, reset, and
-- the stuck-game sweeper's Failed transition. A Finished/Failed game never
-- keeps rows here. JSONB holds only point lists SQL never queries into.
CREATE TABLE game_resume_states (
    game_id UUID PRIMARY KEY REFERENCES games (game_id) ON DELETE CASCADE,
    turn_number INTEGER NOT NULL,
    food JSONB NOT NULL,    -- array of {x,y} rules::Point
    hazards JSONB NOT NULL, -- raw board.hazards: Snail Mode off-board
                            -- bookkeeping points included, order and
                            -- duplicates preserved
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW ()
);

CREATE TABLE game_resume_snakes (
    game_id UUID NOT NULL REFERENCES game_resume_states (game_id) ON DELETE CASCADE,
    game_battlesnake_id UUID NOT NULL REFERENCES game_battlesnakes (game_battlesnake_id) ON DELETE CASCADE, -- dropping a roster row drops its resume bookkeeping
    health INTEGER NOT NULL,
    eliminated_cause TEXT NOT NULL,      -- serde snake_case of rules::EliminationCause
    eliminated_by TEXT NOT NULL,         -- rules::Snake.eliminated_by is a String
    eliminated_on_turn INTEGER NOT NULL, -- rules field is i32, 0 when not eliminated
    last_move TEXT,                      -- Direction wire text; NULL = no last_moves entry
    context_present BOOLEAN NOT NULL,    -- snake_contexts holds an entry for this snake
    latency_ms INTEGER,                  -- SnakeContext.latency_ms (i64) narrowed, as snake_turns does
    shout TEXT,
    food_eaten INTEGER NOT NULL DEFAULT 0,
    death_turn INTEGER,                  -- death_* are NULL as a group while the snake
    death_cause TEXT,                    -- has no death_info entry (death_order = its
    death_eliminated_by TEXT,            -- position in the runner's death_info vector)
    death_order INTEGER,
    body JSONB NOT NULL,                 -- array of {x,y} rules::Point
    PRIMARY KEY (game_id, game_battlesnake_id)
);
