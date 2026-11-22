SET LOCAL lock_timeout = '5s';
DROP TRIGGER sync_entry_state_to_registration ON leaderboard_entries;
DROP FUNCTION sync_entry_state_to_registration();
