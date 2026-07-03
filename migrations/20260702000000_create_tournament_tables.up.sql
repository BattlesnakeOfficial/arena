CREATE TABLE tournaments (
    tournament_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    description TEXT,
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    game_type TEXT NOT NULL DEFAULT 'Standard',
    board_size TEXT NOT NULL DEFAULT '11x11',
    registration_status TEXT NOT NULL DEFAULT 'open',
    visibility TEXT NOT NULL DEFAULT 'public',
    status TEXT NOT NULL DEFAULT 'created',
    match_style TEXT NOT NULL DEFAULT 'single_game',
    max_snakes_per_user INTEGER NOT NULL DEFAULT 1,
    required_participants INTEGER NOT NULL DEFAULT 2,
    current_round INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX tournaments_user_id_idx ON tournaments (user_id);
CREATE INDEX tournaments_status_idx ON tournaments (status);

CREATE TRIGGER update_tournaments_updated_at BEFORE
UPDATE ON tournaments FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE tournament_registrations (
    registration_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tournament_id UUID NOT NULL REFERENCES tournaments(tournament_id) ON DELETE CASCADE,
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    seed INTEGER NOT NULL,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (tournament_id, battlesnake_id),
    -- Deferrable so seed swaps/renumbering can happen within a transaction
    UNIQUE (tournament_id, seed) DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX tournament_registrations_tournament_id_idx ON tournament_registrations (tournament_id);
CREATE INDEX tournament_registrations_user_id_idx ON tournament_registrations (user_id);

CREATE TABLE tournament_matches (
    match_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tournament_id UUID NOT NULL REFERENCES tournaments(tournament_id) ON DELETE CASCADE,
    round INTEGER NOT NULL,
    position INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'scheduled',
    next_match_id UUID REFERENCES tournament_matches(match_id) ON DELETE SET NULL,
    winner_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE SET NULL,
    visual_column INTEGER NOT NULL,
    visual_row INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (tournament_id, round, position)
);

CREATE INDEX tournament_matches_tournament_id_idx ON tournament_matches (tournament_id);
CREATE INDEX tournament_matches_next_match_id_idx ON tournament_matches (next_match_id);

CREATE TRIGGER update_tournament_matches_updated_at BEFORE
UPDATE ON tournament_matches FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE match_participants (
    match_participant_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    match_id UUID NOT NULL REFERENCES tournament_matches(match_id) ON DELETE CASCADE,
    -- NULL until the participant is known (waiting on a feeder match)
    battlesnake_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    source_match_id UUID REFERENCES tournament_matches(match_id) ON DELETE SET NULL,
    participant_type TEXT NOT NULL DEFAULT 'seed',
    seed_position INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX match_participants_match_id_idx ON match_participants (match_id);
CREATE INDEX match_participants_source_match_id_idx ON match_participants (source_match_id);

CREATE TABLE match_games (
    match_game_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    match_id UUID NOT NULL REFERENCES tournament_matches(match_id) ON DELETE CASCADE,
    game_id UUID NOT NULL REFERENCES games(game_id) ON DELETE CASCADE,
    game_number INTEGER NOT NULL,
    winner_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (match_id, game_number),
    UNIQUE (game_id)
);

CREATE INDEX match_games_match_id_idx ON match_games (match_id);
