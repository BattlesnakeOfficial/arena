ALTER TABLE games DROP COLUMN finished_at;
ALTER TABLE customization_grants DROP COLUMN source;
DROP TABLE customization_active_week_backfill_cursor;
DROP TABLE customization_active_weeks;
DROP INDEX game_battlesnakes_created_at_idx;
