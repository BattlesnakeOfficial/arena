-- DEV-2344: durable work list for re-archiving the 1,024 v1 Play objects
-- that dropped snake Team / header Mode / MaxTurnsToNextFoodSpawn.
-- Filled ONCE by the sweep from the checked-in census list (see
-- dropped_fields_repair.rs), filtered to rows still on the v1 1970 path;
-- rows are never deleted, so "empty" means "never filled". No pending
-- rows = the repair is done (the sweep's stop condition). old_gcs_path /
-- old_archive_version are captured at fill time and make rollback a
-- single UPDATE (see docs/game-archive.md).
CREATE TABLE dropped_fields_repair_queue (
    engine_game_id TEXT PRIMARY KEY,
    -- pending = needs a new object; done = repointed; failed = terminal
    -- (any failed row blocks engine-DB teardown sign-off; see the module
    -- docs for re-arming)
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'done', 'failed')),
    -- set when status = 'failed'
    reason TEXT,
    -- the arena row's values BEFORE repair, captured at fill time
    old_gcs_path TEXT,
    old_archive_version INTEGER,
    enqueued_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed the sweep cron so the first tick runs one interval after deploy,
-- not at boot (same rationale as 20261105000000 for PlayHistoryWalkJob:
-- cja's IntervalSchedule::should_run treats a cron with no row as
-- never-run and fires it on the first poll, and prod polls every ~2s).
INSERT INTO crons (cron_id, name, last_run_at, created_at, updated_at)
VALUES (gen_random_uuid(), 'DroppedFieldsRepairSweepJob', NOW(), NOW(), NOW())
ON CONFLICT (name) DO NOTHING;
