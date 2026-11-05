DELETE FROM crons WHERE name = 'PlayHistoryWalkJob';
DROP TABLE IF EXISTS play_archive_exceptions;
DROP TABLE IF EXISTS play_history_walk_cursor;
