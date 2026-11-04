-- no-transaction
-- Discovery working set for turn pruning (DEV-1981): archived v2 native
-- games with an object path whose turns are not yet pruned. Rows leave
-- the index when turns_pruned_at is set.
CREATE INDEX CONCURRENTLY IF NOT EXISTS games_turns_prunable_idx
  ON games (created_at)
  WHERE engine_game_id IS NULL
    AND status IN ('finished', 'failed')
    AND archived_at IS NOT NULL
    AND archive_version = 2
    AND gcs_path IS NOT NULL
    AND turns_pruned_at IS NULL;
