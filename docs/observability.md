# Eyes observability

Arena declares its operational contract in `server/src/observability.rs`. One
boot-stable `ProcessIdentity` is shared by the tracing subscriber, boot manifest,
and heartbeat in both GCP and local tracing paths. Registration is bounded and
best effort after workers start; a registration failure is logged without
stopping Arena. The heartbeat starts only after manifest acceptance. Controlled
startup/worker failures now log their cause and flush Eyes before returning.
Abrupt process death can still lose buffered events; Cloud Logging remains the
source for those final crash messages.

The `arena` process role represents the combined Cloud Run service, with at
least one instance expected. Only enabled features declare HTTP checks, cron
schedules, and critical jobs. This assumes the current combined deployment;
split web/worker deployments need separate role and authority design.

The `arena-operations` dashboard covers HTTP volume/latency, errors, committed
game completions, queue wait, turn database writes, processing overhead, and job
attempts/failures. It uses existing tracing fields. Request counts include
`/health` checks; failure-event counts include retry attempts, not unique jobs.
HTTP duration metrics are microseconds; game timings are milliseconds.

Processing overhead measures elapsed turn-loop time minus elapsed time awaiting
parallel move requests, including persistence and scheduling. The old event
subtracted summed snake latencies, double-counting overlapping requests and
producing negative overhead. The metric now requires
`timing_basis = "elapsed_move_wait"` to exclude those historical measurements;
it has no data until a game completes on the corrected build.

Critical jobs cover game execution, tournament advancement, ratings, and
individual game backups. Every enabled cron participates in Eyes run-health
monitoring through the process role. Named metric thresholds use five-minute
windows, a 30-second ingestion delay, one-minute evaluations, three consecutive
breaches, and two healthy evaluations for recovery:

| Metric | Warning | Critical |
| --- | --- | --- |
| Game queue wait p95 | > 10 seconds | > 30 seconds |
| Turn persistence p95 | > 1 second | > 3 seconds |

These are initial conservative boundaries. A read-only production check on
2026-09-13 observed roughly 297ms queue-wait p95 and 317ms turn-write p95 over an
hour. No-data periods are not outage alerts. Metric evaluations can be incomplete
under query budgets; Eyes exposes that state instead of claiming an exact result.

## Release sequence

1. Publish `eyes-query` 0.1.0, then `eyes-subscriber` 0.8.0 from the Eyes repo
   (its server already accepts these contracts).
2. Merge cja's subscriber bump / `TracingConfig` process-identity support.
3. Update Arena's lockfile to that cja commit and published subscriber. Confirm
   only one subscriber version with `cargo tree -i eyes-subscriber`.
4. Deploy Arena, then verify its process role, heartbeat, run-health declarations,
   all 14 named metrics, both thresholds, and dashboard in production Eyes.

Local validation before publication uses an uncommitted Cargo patch file for
cja, eyes-query, and eyes-subscriber. Those path overrides must not ship. Live
notification delivery additionally depends on Eyes' Discord webhook configuration.

## Game and job investigation evidence

Each `run_game` invocation has an `arena.game` span carrying `game_id` at
creation, nested under its cja job attempt. Its phases are `load_game`,
`reset_game` (retries), `prepare_snakes`, `start_snakes`, `request_moves`,
`persist_turn`, `end_snakes`, `finish_game`, and `post_completion`.

An `arena.game.phase` span carries game ID, phase, and the applicable turn.
Events with `event_type=game_phase` record `state=started`, followed by exactly
one of `completed`, `failed`, or `cancelled` if execution unwinds normally.
Terminal events include elapsed milliseconds; failures preserve the error cause
chain. `finish_game` completes only after the database commit, and follow-up
jobs are a separate phase. An interrupted future never emits `completed`.

Turn persistence has nested phases with the same lifecycle contract:

| Phase | Measured operation |
| --- | --- |
| `persist_turn.acquire_frame_connection` | SQLx connection acquisition before the frame write |
| `persist_turn.insert_frame` | Frame INSERT on the acquired connection |
| `persist_turn.notify` | Channel-map read lock and local broadcast send |
| `persist_turn.acquire_snake_connection` | SQLx connection acquisition before each snake move write |
| `persist_turn.insert_snake` | One snake move INSERT on the acquired connection |

Every stage carries game ID and turn at span creation and inherits the job/game
trace. `eyes investigate --game <UUID> --since 1h` includes these invocations.
Acquisition can include pool checkout checks or opening a connection; insert
timing includes the database round trip and row decoding, not just server CPU.
Notification does not await WebSocket delivery. The frame connection is returned
before notification; writes retain their existing order and autocommit behavior.
The enclosing `persist_turn` duration includes these stages, so summing parent
and child durations double-counts time. Routine stage fields contain no frame
payloads, SQL parameters, or credentials.

Abrupt process death can prevent both the terminal event and buffered startup
information from arriving. An unmatched start means the phase has no observed
terminal event, not proof that its worker crashed. Correlate the containing job
attempt, process instance heartbeat, deployment, and Cloud Run logs.

Cja's enqueue receipt (`event_type=job_enqueued`) identifies the persisted job
UUID, which matches `job.id` on the later worker attempt. Enqueue spans alone
include failed attempts and are not proof that work entered the queue.

## Telemetry delivery

Production uses `EYES_TRANSPORT=batching` with eyes-subscriber 0.8.1 or later.
The serial HTTP default sends one event per request and fell roughly 35 minutes
behind during live games even while process heartbeats remained fresh. Batches
flush up to 500 events at a time or after one second, preserving buffered events
through failed requests. A fresh heartbeat proves process liveness, not event
delivery freshness. Verify a completed game's final phase and completion receipt
in Eyes after rollout, alongside the source Cloud Logging records.
