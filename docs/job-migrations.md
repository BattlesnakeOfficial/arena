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
schema and does not use cja's AppSession. Current cja-owned copies include
`cron_pauses` (with its down migration) and the `jobs_lock_pair_check` pair
(`AddJobsLockPairCheck` NOT VALID + `ValidateJobsLockPairCheck`), all kept
byte-identical to the upstream versions.

## cja pin

Arena holds cja at `4409b108f75ee56d36065fdcb92ce6118b9392e3` (the PostHog
telemetry, product events, and declaration sync adopted by DEV-2485). Bumping cja must always be a
precise pin, never a bare `cargo update` or `cargo update -p cja`:

```
cargo update -p cja --precise <full-sha>
```

After any bump, check whether cja main gained new migrations, run
`cja sync-migrations` (or copy by hand, following the byte-identical
`.up.sql`/`.down.sql` convention above), and commit any new migration
files in the same PR — the `cja_migrations_are_present_and_unchanged`
test validates the copies against the new rev automatically. Arena has
no dependabot/renovate, so this note is the only guard. Standing rule:
reverting code past a cja bump that added migrations fails boot with
`VersionMissing` — to roll back, revert the code and keep the migration
files.

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
`ARENA_JOB_SHUTDOWN_DRAIN_SECS` controls graceful shutdown drain (default 4 seconds).
Arena then allows 2 seconds of exit grace, up to 0.5 seconds for the Eyes
process-shutdown signal, and up to 3 seconds for telemetry flush. These
bounded steps total 4 + 2 + 0.5 + 3 = 9.5 seconds at the defaults; this is
not a bound on process exit. After the supervisor deadline, cja
`Supervisor::run_until` (pinned revision `4409b108`) calls the unbounded
`self.tasks.shutdown().await` join **before** Arena signals Eyes shutdown and
flushes telemetry. `JoinSet::shutdown` cancels async tasks at their next yield
but can wait on synchronous worker code. **After** the telemetry flush, Arena
drops its multi-thread Tokio runtime, which waits for in-flight
`spawn_blocking` work. A GIF render, backup, archive, or turn-retention
operation can therefore take process exit past Cloud Run's fixed 10-second
SIGTERM-to-SIGKILL interval; increasing the drain consumes more of that window.

Arena shares one cja `IdlePollGate` across its workers. With the production
`ARENA_JOB_WORKERS=100`, `ARENA_JOB_IDLE_POLLERS=2`, and 200 ms interval, at
most two workers make periodic idle claims (about 10 claims/second on an empty
queue). A successful autocommit `Job::enqueue` wakes one parked local worker;
direct SQL inserts, due retries, and expired leases are found by the periodic
claims. Notifications are local and may coalesce. The gate does not limit jobs
already running, and priority still orders the next unclaimed row. No database
session features are used.

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

## Cron and dispatch behavior

Since cja #84, a cron tick error is logged and survived: the cron worker keeps
running and the process is not restarted. Since cja #87, a job whose `name` is
not in this binary's registry is released UNCHARGED on an age-scaled delay
(about 30 seconds while the row is young, hourly after an hour, dead-letter
past 14 days) instead of climbing `error_count` — queries that drift on
`error_count` no longer match unknown names during rolling deploys. Since
cja #89, cron due decisions use the database clock, and a future-stamped
`crons.last_run_at` now FIRES the cron immediately instead of pausing it.
Future-stamping was the documented emergency pause lever before #89 — for
`TurnRetentionJob` (a destructive prune) a future stamp would immediately
enqueue a prune batch. Never future-stamp; pause with `cron_pauses` instead:

```sql
INSERT INTO cron_pauses (name, paused_until) SELECT name, now() + interval '4 hours' FROM crons WHERE name = '<JobName>' ON CONFLICT (name) DO UPDATE SET paused_until = EXCLUDED.paused_until, paused_at = clock_timestamp()
```

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
       to_regclass('public.idx_jobs_fetch_next'),
       to_regclass('public.cron_pauses');
SELECT version, success FROM _sqlx_migrations
WHERE version IN (20260206200552, 20260703120000,
                  20261107120000, 20261107120001, 20261110120000);
ROLLBACK;
```

The isolated `exhausted_job_is_archived_and_worker_continues` test runs the real
cja worker against Arena's migrations. It verifies that an exhausted failing job
leaves the active queue, retains its diagnostic fields in the dead-letter table,
and does not prevent the following healthy job from completing. It now uses a
registered failing job (`LeaderboardRatingUpdateJob` against a nonexistent
game), and `unknown_job_name_is_released_uncharged_and_worker_continues` covers
cja #87's uncharged release of unknown job names.
