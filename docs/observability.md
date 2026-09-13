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
