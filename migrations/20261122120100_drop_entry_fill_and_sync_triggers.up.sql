SET LOCAL lock_timeout = '5s';
-- Touches leaderboard_entries only. fill_leaderboard_season_id() is still used
-- by the games trigger and is dropped in the next migration.
DROP TRIGGER fill_leaderboard_entry_season_before_insert ON leaderboard_entries;
DROP TRIGGER sync_entry_state_to_registration ON leaderboard_entries;
DROP FUNCTION sync_entry_state_to_registration();
