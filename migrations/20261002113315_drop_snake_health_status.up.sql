-- DEV-1516: health-sweeper state moved to leaderboard_entries.health_* in
-- 20261002021012_leaderboard_entry_health (DEV-1515), which copied this
-- table's data forward. It was kept only while the previous Cloud Run
-- revision drained; nothing reads or writes it any more.
DROP TABLE IF EXISTS snake_health_status;
