SET LOCAL lock_timeout = '5s';

CREATE FUNCTION create_leaderboard_preseason() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
    VALUES (NEW.leaderboard_id, 'Preseason', NEW.created_at);
    RETURN NEW;
END;
$$;
CREATE TRIGGER create_leaderboard_preseason_after_insert
    AFTER INSERT ON leaderboards FOR EACH ROW EXECUTE FUNCTION create_leaderboard_preseason();

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

ALTER TABLE leaderboard_entries ADD COLUMN season_id UUID;
ALTER TABLE leaderboard_games ADD COLUMN season_id UUID;

ALTER TABLE leaderboard_entries ADD CONSTRAINT leaderboard_entries_season_league_fk
    FOREIGN KEY (season_id, leaderboard_id)
    REFERENCES leaderboard_seasons(season_id, leaderboard_id) ON DELETE NO ACTION NOT VALID;
ALTER TABLE leaderboard_games ADD CONSTRAINT leaderboard_games_season_league_fk
    FOREIGN KEY (season_id, leaderboard_id)
    REFERENCES leaderboard_seasons(season_id, leaderboard_id) ON DELETE NO ACTION NOT VALID;

CREATE FUNCTION fill_leaderboard_season_id() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.season_id IS NULL THEN
        SELECT s.season_id INTO NEW.season_id
        FROM leaderboard_seasons s
        WHERE s.leaderboard_id = NEW.leaderboard_id
          AND s.starts_at <= clock_timestamp()
          AND (s.ends_at IS NULL OR clock_timestamp() < s.ends_at);
        IF NEW.season_id IS NULL THEN
            RAISE EXCEPTION 'no active season for leaderboard %', NEW.leaderboard_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER fill_leaderboard_entry_season_before_insert
    BEFORE INSERT ON leaderboard_entries FOR EACH ROW EXECUTE FUNCTION fill_leaderboard_season_id();
CREATE TRIGGER fill_leaderboard_game_season_before_insert
    BEFORE INSERT ON leaderboard_games FOR EACH ROW EXECUTE FUNCTION fill_leaderboard_season_id();
