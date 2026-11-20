UPDATE leaderboard_entries
SET disabled_at = NULL, disabled_reason = NULL, updated_at = NOW()
WHERE disabled_reason = 'retired';

ALTER TABLE battlesnakes DROP COLUMN retired_at;
