-- First-class per-cron pause (DEV-2227). A row means "this cron is paused
-- until paused_until"; no row means "not paused". Not FK'd to crons: a
-- cron's first crons row is created lazily by its first claim, and a pause
-- must be able to precede that first fire. paused_until is compared to
-- clock_timestamp() inside the cron claim (cja::cron::registry), so only
-- database time can put a pause in force or expire it; 'infinity' pauses
-- indefinitely.
CREATE TABLE cron_pauses (
    name TEXT PRIMARY KEY,
    paused_until TIMESTAMPTZ NOT NULL,
    paused_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
