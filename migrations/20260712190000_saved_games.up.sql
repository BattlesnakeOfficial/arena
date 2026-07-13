-- Saved games on user profiles (BS-219e7007).
--
-- Users can save any game with an optional title; saved games are listed on
-- their public profile. One row per (user, game): re-saving the same game
-- updates the title instead of creating a duplicate.
CREATE TABLE saved_games (
    saved_game_id UUID PRIMARY KEY DEFAULT gen_random_uuid (),
    user_id UUID NOT NULL REFERENCES users (user_id),
    game_id UUID NOT NULL REFERENCES games (game_id),
    title TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, game_id)
);
