# Game archive

Arena archives every completed game to object storage and serves old replays
from it. This page covers the object layout, who writes and reads, and the
serving and failure rules.

## Bucket and object layout

- Bucket: `battlesnake-game-backups` (env `GCS_BUCKET`; unset in dev/e2e).
- Play v1/v2 objects: `games/YYYY/MM/DD/<engine-game-id>.json.zst` — a
  `GameExport` (`engine_models.rs`): `{game, frames, exported_at}` for v1
  (zstd 3, snake URLs inside), `{archive_version, game_id, game, frames,
  exported_at}` for v2 (zstd 9, snake `URL`/`ProxyURL`/`APIVersion` omitted).
  `game` is `EngineGame`-shaped.
- Native objects (DEV-1980): `native/YYYY/MM/DD/<arena-game-id>.json.zst` —
  `{archive_version, game_id, game, frames, exported_at}` where `game` is a
  `NativeGameHeader` (NOT `EngineGame`-shaped) and `frames` are raw
  `turns.frame_data` values.

## Who writes

- `backup.rs` (Play/engine-DB games) writes through
  `AppState.object_store` (`put`).
- DEV-1980's `native_backup.rs` writes through the same seam.

## Who reads

- `archive.rs` reads through the same `AppState.object_store` Arc (`get`)
  and serves the public endpoints: `GET /api/games/{id}`,
  `GET /api/games/{id}/frames`, `GET /api/games/{id}/events` (websocket),
  and the `/games/{id}` page. All four accept either an arena `game_id` or
  a Play game id (`games.engine_game_id`).
- Decoded archives are cached in-process as raw JSON text, bounded by
  `ARCHIVE_CACHE_MAX_BYTES` (default 64 MiB; 0 disables caching). Same-game
  loads singleflight; decompression and page parsing run on the blocking
  pool under a 2-permit semaphore.

## Serving rule

A row serves from the archive iff it has `gcs_path` set, is terminal
(`finished`/`failed`), and has no renderable frame in `turns`
(`models::turn::has_frames`). Live games always take the DB/live path; the
`turns` probe only runs for archived-looking terminal rows. All 5.6M prod
stub rows were verified `finished` on 2026-10-31 (the only writer of
`engine_game_id` hardcodes that status). A hypothetical non-terminal stub
takes the DB path and serves `{Game, LastFrame: null}` — never a silent 404.

## Row-decides rule

The `games` row — never the object — decides how `Game` is served:

- `engine_game_id IS NOT NULL` ⇒ Play: the archived `EngineGame` header is
  validated at decode time and served (its real `Width`, `Height`,
  `Ruleset`, `RulesetName`, `Map`, `Source`, `Created`, …; status still
  comes from the row).
- `engine_game_id IS NULL` ⇒ native: the header is ignored and `Game` is
  built from the arena row via the same constructor the DB path uses, so a
  native game's `Game` is byte-identical before and after `turns` pruning
  (DEV-1981).

## Failure semantics

- Unknown id (no row under either id): 404.
- Row has `gcs_path` but the object is missing: 404 to the client plus an
  ERROR log with the game id and path — that is archive data loss.
- Storage error, timeout (10 s fetch cap), or corrupt/oversized object
  (64 MiB caps before and after decompression): 500; nothing partial is
  cached. The websocket uses its usual error-event-then-close behavior.

## Turn retention (pruning, DEV-1981)

`turns`/`snake_turns` rows for archived native games are deleted after a
retention window, once the archive object's CONTENT is confirmed. Env
`ARENA_TURN_RETENTION_DAYS` (default 30, clamped ≥ 1; large values
effectively disable pruning).

**Prune predicate** (hourly discovery, oldest-first, `LIMIT 2000`/run):
native (`engine_game_id IS NULL`), terminal (`finished`/`failed`),
`archived_at` set with `archive_version = 2` and a `gcs_path`, older than
retention, not yet pruned (`turns_pruned_at IS NULL`), and no
`game_battlesnakes` link inside the active-week backfill's rescan window
(the cursor minus 1 day — the backfill rescans from
`scanned_through - 1 day` and its `legacy_finishes` reads turns, so a
prune inside that window could re-credit a WRONG week). The predicate's
partial index is `games_turns_prunable_idx`; rows leave it when
`turns_pruned_at` is set, which is also the idempotency marker.

**Content confirmation before delete**: GET the object, decode it, and
require `archive_version = 2`, `game_id` matching the row, and frames
equal one-for-one (as `serde_json::Value`) to what the archiver would
write now (`native_backup::fetch_native_frame_texts` — the same selection
on both sides). Rows are NEVER deleted for a game whose object is missing,
undecodable, wrong-version, wrong-id, or content/frame-count mismatched:
those are retained (fail-closed — the turns are the only complete copy),
logged at ERROR, re-enqueued hourly. Remedy for permanently stuck games:
clear `archived_at`/`gcs_path`/`archive_version` so DEV-1980 discovery
re-archives (the path is deterministic, so the re-archive overwrites the
bad object). Enough stuck games (≥ the 2000/run cap) stall pruning
entirely — visible as discovery candidates that never shrink.

**Concurrency**: per-game prune jobs (`PruneGameTurnsJob`, priority −20,
below backup jobs' −10) serialize on the shared 1-permit `BACKUP_SLOT`
(one heavy archive-family op per instance; never concurrent with a
backup). The slot is acquired BEFORE the eligibility re-check, so
duplicate jobs on one instance are silent no-ops; the slot is
per-process, so a duplicate racing another INSTANCE's prune is caught
after the content confirm instead (a 0-frame count mismatch re-reads
`turns_pruned_at` and skips silently when the other instance already
committed). The DELETE + `turns_pruned_at` mark
commit in one transaction with `lock_timeout` (5s) and
`statement_timeout` (60s, per statement); `snake_turns` cascade via the
FK. Each DELETE fires `watched_game_turn_delete` once: one
`reset:<game_id>` NOTIFY at commit (watched-game viewers get a clean
reload signal; unwatched games are a no-op) plus a transient
transition-table materialization of the deleted rows.

**Known side effects**:

- The `turns_pruned_at` UPDATE bumps `games.updated_at` (trigger), so a
  pruned month-old game reports `updated_at` ≈ prune time in public
  list/detail APIs (same as `archived_at` marks; accepted).
- NULL-`frame_data` turns (an incomplete last turn) and their
  `snake_turns` are deleted unarchived — the writer never archived them
  and no reader renders them.
- Archive fallbacks keep `/api/games/{id}/move-request`, shout
  moderation's distinct-shout extraction, and the `/og/games/{id}.png`
  social card (final/opening board, turn count, worn cosmetics —
  `routes/og.rs` reads `get_latest_frame`/`get_turn_frames_page`, both
  now archived for pruned native games) correct for pruned games
  (native rows only; imported Play rows keep today's behavior — empty
  board).
- Accepted degradations: the snake latency chart only covers games inside
  retention (empty state otherwise); Solo stats rows and the token-authed
  `/api/games/{id}/details` frames are empty for pruned games (same
  shape DEV-1979 accepted for the public frames endpoint); highlights
  admin previews of windows older than retention lose frame sections and
  faults.
