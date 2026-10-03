SET LOCAL lock_timeout = '5s';
DROP TRIGGER IF EXISTS watched_game_turn_delete ON turns;
DROP TRIGGER IF EXISTS watched_game_status_update ON games;
DROP TRIGGER IF EXISTS watched_game_turn_insert ON turns;
DROP FUNCTION IF EXISTS notify_watched_game_reset();
DROP FUNCTION IF EXISTS notify_watched_game_status();
DROP FUNCTION IF EXISTS notify_watched_game_turn();
