ALTER TABLE leaderboards
  ADD COLUMN creator_user_id UUID REFERENCES users(user_id) ON DELETE SET NULL,
  ADD COLUMN description TEXT NOT NULL DEFAULT '',
  ADD COLUMN visibility TEXT NOT NULL DEFAULT 'public',
  ADD COLUMN board_height INTEGER NOT NULL DEFAULT 11,
  ADD COLUMN board_width INTEGER NOT NULL DEFAULT 11,
  ADD COLUMN game_type TEXT NOT NULL DEFAULT 'Standard',
  ADD COLUMN matchmaking_enabled BOOLEAN NOT NULL DEFAULT false,
  ADD COLUMN games_per_day INTEGER NOT NULL DEFAULT 100;

UPDATE leaderboards SET matchmaking_enabled = true WHERE creator_user_id IS NULL;

CREATE INDEX idx_leaderboards_public_active
  ON leaderboards (created_at DESC)
  WHERE disabled_at IS NULL AND visibility = 'public';

CREATE INDEX idx_leaderboards_by_creator
  ON leaderboards (creator_user_id)
  WHERE creator_user_id IS NOT NULL;

CREATE TABLE leaderboard_enrollment_requests (
  enrollment_request_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  leaderboard_id UUID NOT NULL REFERENCES leaderboards(leaderboard_id) ON DELETE CASCADE,
  battlesnake_id UUID NOT NULL REFERENCES battlesnakes(battlesnake_id) ON DELETE CASCADE,
  initiated_by_user_id UUID NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
  status TEXT NOT NULL DEFAULT 'pending',
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (leaderboard_id, battlesnake_id)
);

CREATE TRIGGER update_enrollment_requests_updated_at
  BEFORE UPDATE ON leaderboard_enrollment_requests
  FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();
