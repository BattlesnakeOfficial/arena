DROP TABLE IF EXISTS leaderboard_enrollment_requests;
DROP INDEX IF EXISTS idx_leaderboards_by_creator;
DROP INDEX IF EXISTS idx_leaderboards_public_active;
ALTER TABLE leaderboards
  DROP COLUMN IF EXISTS games_per_day,
  DROP COLUMN IF EXISTS matchmaking_enabled,
  DROP COLUMN IF EXISTS game_type,
  DROP COLUMN IF EXISTS board_height,
  DROP COLUMN IF EXISTS board_width,
  DROP COLUMN IF EXISTS visibility,
  DROP COLUMN IF EXISTS description,
  DROP COLUMN IF EXISTS creator_user_id;
