-- DEV-1981: seed the TurnRetentionJob cron row. cja's
-- IntervalSchedule::should_run treats a cron with no row as "run now",
-- so without this seed the deploy's first tick (2s poll) would enqueue
-- the first prune batch within seconds of boot. Seeding last_run_at
-- makes the first tick fire one interval after this migration applies,
-- and makes the pause lever (UPDATE crons ... WHERE name =
-- 'TurnRetentionJob') functional from deploy.
INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
VALUES (gen_random_uuid(), 'TurnRetentionJob', NOW(), NOW(), NOW())
ON CONFLICT (name) DO NOTHING;
