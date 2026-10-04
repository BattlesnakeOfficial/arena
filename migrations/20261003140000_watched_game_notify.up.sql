SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION notify_watched_game_turn() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('arena_watched_games', 'turn:' || NEW.game_id || ':' || NEW.turn_number);
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION notify_watched_game_status() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('arena_watched_games', 'status:' || NEW.game_id);
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION notify_watched_game_reset() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    watched_id uuid;
BEGIN
    FOR watched_id IN SELECT DISTINCT game_id FROM old_turns LOOP
        PERFORM pg_notify('arena_watched_games', 'reset:' || watched_id);
    END LOOP;
    RETURN NULL;
END;
$$;

CREATE TRIGGER watched_game_turn_insert AFTER INSERT ON turns
    FOR EACH ROW EXECUTE FUNCTION notify_watched_game_turn();
CREATE TRIGGER watched_game_status_update AFTER UPDATE OF status ON games
    FOR EACH ROW WHEN (OLD.status IS DISTINCT FROM NEW.status)
    EXECUTE FUNCTION notify_watched_game_status();
CREATE TRIGGER watched_game_turn_delete AFTER DELETE ON turns
    REFERENCING OLD TABLE AS old_turns FOR EACH STATEMENT
    EXECUTE FUNCTION notify_watched_game_reset();
