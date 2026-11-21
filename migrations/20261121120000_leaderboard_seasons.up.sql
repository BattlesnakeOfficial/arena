SET LOCAL lock_timeout = '5s';

CREATE EXTENSION IF NOT EXISTS btree_gist;

CREATE TABLE leaderboard_seasons (
    season_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    leaderboard_id UUID NOT NULL REFERENCES leaderboards(leaderboard_id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(btrim(name)) > 0),
    starts_at TIMESTAMPTZ NOT NULL,
    ends_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT leaderboard_seasons_end_after_start CHECK (ends_at IS NULL OR ends_at > starts_at),
    CONSTRAINT leaderboard_seasons_id_leaderboard_unique UNIQUE (season_id, leaderboard_id),
    CONSTRAINT leaderboard_seasons_no_overlap EXCLUDE USING gist (
        leaderboard_id WITH =,
        tstzrange(starts_at, ends_at, '[)') WITH &&
    )
);

CREATE TRIGGER update_leaderboard_seasons_updated_at
    BEFORE UPDATE ON leaderboard_seasons
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

WITH entry_times AS (
    SELECT leaderboard_id, min(created_at) AS earliest FROM leaderboard_entries GROUP BY leaderboard_id
), game_times AS (
    SELECT lg.leaderboard_id, min(lg.created_at) AS earliest_link, min(g.created_at) AS earliest_game
    FROM leaderboard_games lg JOIN games g ON g.game_id = lg.game_id GROUP BY lg.leaderboard_id
)
INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
SELECT l.leaderboard_id, 'Preseason',
       least(l.created_at, coalesce(e.earliest, l.created_at),
             coalesce(gt.earliest_link, l.created_at), coalesce(gt.earliest_game, l.created_at))
FROM leaderboards l
LEFT JOIN entry_times e ON e.leaderboard_id = l.leaderboard_id
LEFT JOIN game_times gt ON gt.leaderboard_id = l.leaderboard_id
WHERE NOT EXISTS (SELECT 1 FROM leaderboard_seasons s WHERE s.leaderboard_id = l.leaderboard_id);
