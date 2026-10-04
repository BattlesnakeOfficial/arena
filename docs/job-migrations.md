# Background-job schema

Arena runs cja's job workers but applies only the SQL files in this repository's
`migrations/` directory. Updating the cja dependency does not apply its migrations.
The worker uses runtime SQL, so a missing framework table can compile successfully
and fail only when the relevant job path executes.

Keep cja's migrations byte-identical to the pinned dependency. Arena supports
renaming an upstream `.sql` file to `.up.sql` and adding a corresponding
`.down.sql`. The `cja_migrations_are_present_and_unchanged` test locates the pinned
dependency through `cargo metadata` and checks the copies. The two upstream
`AddSessions` migrations are intentionally excluded: Arena has its own session
schema and does not use cja's AppSession.

The job migrations include `dead_letter_jobs`, which retains the payload and
failure details after a job exhausts its retries, and `idx_jobs_fetch_next`, which
supports polling by priority, scheduled time, and creation time.

## Job leases

Arena heartbeats each running job every 30 seconds (`ARENA_JOB_HEARTBEAT_INTERVAL_SECS`).
`ARENA_JOB_RECLAIM_WINDOW_SECS` is the reclaim window since the last successful
heartbeat, default 240 seconds. This allows for roughly 30-second Neon stalls
while keeping dead-worker recovery under five minutes. A roughly 42-minute game
remains exclusively leased while heartbeats succeed. A hard-killed worker's lease
can be reclaimed after four minutes plus the next poll when the database is
healthy. Polling is separate: `ARENA_JOB_POLL_INTERVAL_MS` is 200 ms in production.
`ARENA_JOB_SHUTDOWN_DRAIN_SECS` controls graceful shutdown drain (default 5 seconds).

If heartbeat writes keep failing, cja's watchdog stops the job body before the
lease expires. A later reclaim charges one failed attempt. A panic also charges
an attempt and releases the lock while the worker keeps running. With N retries,
the terminal dead-letter row records N+1 failed attempts. Rows locked by the
pre-heartbeat revision (cja `a6406d7`) have no heartbeat. On the first deploy,
new workers may reclaim one 240 seconds after its `locked_at` if the old
revision has not drained or released it. That charges one attempt and replays
the game from turn zero. This cja bump needs no new migration file.

Watch Cloud Logging for `Job lease watchdog expired`, `Job lease ownership lost`,
`Job failure finalization lost ownership`, `Job completion lost ownership`, and
`Job lease lost; body stopped`. Spikes warrant checking database access, pool
pressure, and the configured reclaim window. A prolonged database outage can
delay reclaim and cause a game to replay from turn zero.

## Deployment and rollback

Normal startup applies pending migrations before starting workers. The original
upstream versions are preserved, so previously omitted migrations can apply even
when newer Arena migrations are already installed.

Once these migrations have been recorded in `_sqlx_migrations`, an older image
that omits them will fail SQLx's missing-migration validation. Roll application
code forward or build a rollback image that retains the migration files.
The dead-letter down migration deletes retained failure history; export any
needed records before deliberately rolling that schema back. Do not use a broad
`sqlx migrate revert --target` to remove an older framework migration: that also
reverts newer Arena migrations.

After deployment, verify `/health` identifies the deployed commit and use a
read-only database transaction to check both objects and their migration records:

```sql
BEGIN READ ONLY;
SELECT to_regclass('public.dead_letter_jobs'),
       to_regclass('public.idx_jobs_fetch_next');
SELECT version, success FROM _sqlx_migrations
WHERE version IN (20260206200552, 20260703120000);
ROLLBACK;
```

The isolated `exhausted_job_is_archived_and_worker_continues` test runs the real
cja worker against Arena's migrations. It verifies that an exhausted failing job
leaves the active queue, retains its diagnostic fields in the dead-letter table,
and does not prevent the following healthy job from completing.
