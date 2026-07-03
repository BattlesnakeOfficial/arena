CREATE TABLE tournaments (
    tournament_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
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
    registration_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    tournament_id UUID NOT NULL REFERENCES tournaments(tournament_id) ON DELETE CASCADE,
    battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
    seed INTEGER NOT NULL CHECK (seed >= 1),
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (tournament_id, battlesnake_id),
    -- Deferrable (but immediate by default) so seed swaps/renumbering can opt
    -- in with `SET CONSTRAINTS tournament_registrations_tournament_id_seed_key
    -- DEFERRED` inside a transaction.
    CONSTRAINT tournament_registrations_tournament_id_seed_key
        UNIQUE (tournament_id, seed) DEFERRABLE INITIALLY IMMEDIATE
);

-- tournament_id lookups are covered by the leading column of
-- UNIQUE (tournament_id, battlesnake_id).
CREATE INDEX tournament_registrations_battlesnake_id_idx ON tournament_registrations (battlesnake_id);
CREATE INDEX tournament_registrations_user_id_idx ON tournament_registrations (user_id);

CREATE TABLE tournament_matches (
    match_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    tournament_id UUID NOT NULL REFERENCES tournaments(tournament_id) ON DELETE CASCADE,
    round INTEGER NOT NULL CHECK (round >= 1),
    position INTEGER NOT NULL CHECK (position >= 0),
    status TEXT NOT NULL DEFAULT 'scheduled',
    next_match_id UUID REFERENCES tournament_matches(match_id) ON DELETE SET NULL,
    winner_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE SET NULL,
    visual_column INTEGER NOT NULL CHECK (visual_column >= 0),
    visual_row INTEGER NOT NULL CHECK (visual_row >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (tournament_id, round, position)
);

-- tournament_id lookups are covered by the leading column of
-- UNIQUE (tournament_id, round, position).
CREATE INDEX tournament_matches_next_match_id_idx ON tournament_matches (next_match_id);
CREATE INDEX tournament_matches_winner_id_idx ON tournament_matches (winner_id);

CREATE TRIGGER update_tournament_matches_updated_at BEFORE
UPDATE ON tournament_matches FOR EACH ROW EXECUTE FUNCTION update_updated_at_column ();

CREATE TABLE match_participants (
    match_participant_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    match_id UUID NOT NULL REFERENCES tournament_matches(match_id) ON DELETE CASCADE,
    -- Which side of the match this participant occupies (1 or 2).
    slot SMALLINT NOT NULL CHECK (slot IN (1, 2)),
    -- NULL until the participant is known (waiting on a feeder match)
    battlesnake_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
    source_match_id UUID REFERENCES tournament_matches(match_id) ON DELETE SET NULL,
    participant_type TEXT NOT NULL DEFAULT 'seed',
    seed_position INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (match_id, slot)
);

-- match_id lookups are covered by the leading column of UNIQUE (match_id, slot).
CREATE UNIQUE INDEX match_participants_match_id_source_match_id_key
    ON match_participants (match_id, source_match_id)
    WHERE source_match_id IS NOT NULL;
CREATE UNIQUE INDEX match_participants_match_id_battlesnake_id_key
    ON match_participants (match_id, battlesnake_id)
    WHERE battlesnake_id IS NOT NULL;
CREATE INDEX match_participants_battlesnake_id_idx ON match_participants (battlesnake_id);
CREATE INDEX match_participants_source_match_id_idx ON match_participants (source_match_id);

CREATE TABLE match_games (
    match_game_id UUID PRIMARY KEY DEFAULT uuid_generate_v4 (),
    match_id UUID NOT NULL REFERENCES tournament_matches(match_id) ON DELETE CASCADE,
    game_id UUID NOT NULL REFERENCES games(game_id) ON DELETE CASCADE,
    game_number INTEGER NOT NULL CHECK (game_number >= 1),
    winner_id UUID REFERENCES battlesnakes(battlesnake_id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (match_id, game_number),
    UNIQUE (game_id)
);

-- match_id lookups are covered by the leading column of UNIQUE (match_id, game_number).
CREATE INDEX match_games_winner_id_idx ON match_games (winner_id);
