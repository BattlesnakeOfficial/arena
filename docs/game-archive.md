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
