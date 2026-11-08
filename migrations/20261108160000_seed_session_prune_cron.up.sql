-- DEV-2185: seed the SessionPruneJob cron row (same rationale as
-- 20261103210000 for TurnRetentionJob): cja's IntervalSchedule treats a
-- cron with no row as never-run and fires it on the first 2s poll tick,
-- which would start the 1M-row backlog drain at boot. Seeding makes the
-- first prune run one interval after the migration applies (migrations
-- run at boot BEFORE any worker starts, so this row always exists before
-- the first cron poll) and makes the pause lever work from deploy:
--   UPDATE crons SET last_run_at = NOW() + INTERVAL '4 hours'
--   WHERE name = 'SessionPruneJob';  -- future last_run_at = pause
INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
VALUES (gen_random_uuid(), 'SessionPruneJob', NOW(), NOW(), NOW())
ON CONFLICT (name) DO NOTHING;
