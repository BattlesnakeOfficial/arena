SET LOCAL lock_timeout = '5s';

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
CREATE TRIGGER fill_leaderboard_game_season_before_insert
    BEFORE INSERT ON leaderboard_games FOR EACH ROW EXECUTE FUNCTION fill_leaderboard_season_id();
