-- no-transaction
-- Discovery working set for native-game archival (DEV-1980): unarchived
-- terminal native games. Rows leave the index when archived_at is set.
CREATE INDEX CONCURRENTLY IF NOT EXISTS games_native_unarchived_idx
  ON games (updated_at)
  WHERE engine_game_id IS NULL
    AND status IN ('finished', 'failed')
    AND archived_at IS NULL;
