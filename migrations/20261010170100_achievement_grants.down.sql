SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '15s';

UPDATE customization_grants SET source = 'pre_token' WHERE source = 'achievement';
ALTER TABLE customization_grants DROP CONSTRAINT customization_grants_source_check;
ALTER TABLE customization_grants ADD CONSTRAINT customization_grants_source_check
    CHECK (source IN ('pre_token', 'play_import', 'admin', 'token'));
DROP TABLE achievement_backfill_cursor;
