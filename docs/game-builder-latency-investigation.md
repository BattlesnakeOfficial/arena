# Game builder latency investigation

## Status and scope

This is an intermediate diagnostic change for DEV-1283, not a completed timeout fix.
The reported timeout has not been reproduced and its root cause is not established.
No production configuration was changed, no production process was restarted, and no
synthetic production game or flow was created.

The historical request and current local measurements must not be conflated:

- The known slow production trace ran Arena `8839c4f`, before PRs #178 and #179.
- Local measurements below ran the exact `f36de20d0194757a3ac036df6177a86609e279e7`
  source tree. The prior DEV-1282 worktree commit had the identical Git tree hash and
  its dependency artifacts were reused; Cargo rebuilt and linked the Arena binary.

## Historical production evidence

Trace `387ac546ae984d788d39285c4cbb9ab3` started at
`2026-09-18T00:51:13.968467Z`. It is a successful HTTP 303 response from
`POST /games/flow/{id}/create` and took 3,357.54 ms. The trace contains only five
records: the root server span, a `jobs.enqueue` child, the enqueue event, the
game-created event, and the request-finished event. There are no hidden auth or SQL
children.

`jobs.enqueue` started 2,492 ms into the request and lasted 201.173 ms. Approximately
664 ms remained after enqueue. A complete narrow five-second Cloud Logging window
contained 68 rows, below its 500-row cap, and did not contain a slow-SQL warning that
explained this request. Therefore the pre-enqueue interval cannot be assigned to SQL,
pool checkout, runtime scheduling, locks, or auth from the retained evidence.

Source reconstruction at `8839c4f` shows one `CurrentUserWithSession` extraction on
this POST, followed by rate-limit recording/counting, flow load, whole-flow update,
game/participant persistence, `enqueued_at` update, enqueue, flow deletion, and flash
persistence. Repeated session extraction on the builder GET is a separate hypothesis
and cannot explain this POST's 2,492 ms pre-enqueue interval.

## Bounded local baseline

The local deployment used PostgreSQL on the same VM, four job workers, a 200 ms poll
interval, ten pool connections, one authenticated Chromium browser at 1280x720, 1,000
public snakes across 100 owners, one owned snake, and 101 completed games. The test ran
ten iterations each of:

1. finished viewer -> Create Another Game -> `GET /games/new` -> builder GET;
2. finished viewer -> rematch POST -> builder GET; and
3. final create POST -> viewer GET.

All 30 idle and all 30 background-profile navigations reached the expected destination
without a manual refresh, 5xx response, or timeout. Request timings are Playwright
request timings; complete timings include browser event/redirect/render overhead.

| Profile | Sequence | Complete samples (ms) | Complete p95 / max | Server-leg p95 / max |
| --- | --- | --- | --- | --- |
| Idle | Create Another -> builder | 113, 73, 87, 85, 77, 78, 74, 109, 77, 83 | 113 / 113 | 8 / 29 |
| Idle | Rematch -> builder | 91, 89, 82, 88, 91, 85, 95, 83, 85, 92 | 95 / 95 | 7 / 8 |
| Idle | Create -> viewer | 176, 180, 175, 174, 176, 181, 175, 170, 165, 178 | 181 / 181 | 11 / 12 |
| Background | Create Another -> builder | 87, 75, 86, 98, 83, 82, 84, 87, 79, 79 | 99 / 99 | 7 / 10 |
| Background | Rematch -> builder | 96, 82, 103, 93, 88, 92, 81, 86, 92, 89 | 104 / 104 | 7 / 7 |
| Background | Create -> viewer | 175, 181, 171, 171, 175, 171, 173, 194, 178, 171 | 195 / 195 | 10 / 32 |

The background profile queued five ordinary two-snake loopback games immediately
before sampling. The harness recorded game status only at the end, when four background
games and ten measured games were finished and one background game was still waiting.
It did not record active-game count during each sample, so these results must not be
described as measurements under five concurrently active games. At the final snapshot
PostgreSQL showed one active and eight idle connections. `/proc/meminfo` reported
7.47 GiB available memory and 2.38 GiB free swap. These observations demonstrate a
bounded clean baseline, not reproduction of production latency.

## Missing attribution and diagnostic spans

The following static, field-free child spans partition every awaited database boundary
around successful final creation:

- `game_builder.create.rate_limit`
- `game_builder.create.load_flow`
- `game_builder.create.update_settings`
- `game_builder.create.persist_game`
- `game_builder.create.mark_enqueued`
- existing `jobs.enqueue`
- `game_builder.create.delete_flow`
- `game_builder.create.set_flash`

The root-to-first-child interval retains auth/session/form extraction. Gaps between
children retain handler/runtime scheduling. The new names carry no flow, game, user,
session, URL, form, lineup, or other high-cardinality/private fields.

The next narrow experiment is to merge and deploy this diagnostic commit normally,
then inspect the next naturally occurring slow create trace. The child durations and
gaps will identify the dominant boundary for a focused regression and fix. Until such
evidence exists, session caching, worker/pool tuning, timeout changes, idempotency, and
recovery redesign remain unsupported and are intentionally absent.
