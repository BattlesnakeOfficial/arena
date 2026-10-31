//! Archive arena-native games (engine_game_id IS NULL) to GCS.
//!
//! Write side of the native archive; `backup.rs` is the Play/engine-DB write
//! side, DEV-1979 is the read side. Object layout (format v2 family):
//! `native/YYYY/MM/DD/<game_id>.json.zst`, dated by the game's created_at.
//! Body: {"archive_version":2,"game_id":<uuid>,"game":{game_type,board_size,
//! status,created_at,finished_at},"frames":[…raw turns.frame_data values in
//! turn_number order…],"exported_at":…}. Frames are stored EXACTLY as
//! persisted — fetched as frame_data::text (a memcpy; Postgres renders the
//! JSONB server-side), parsed to Value on the BLOCKING pool, with no
//! moderation, author fill, or legacy-frame normalization (those are
//! serve-time concerns). No JSON is parsed on the async workers anywhere in
//! this module. A reader distinguishes native objects from Play objects by
//! the games row (engine_game_id IS NULL), not by the object. Relational
//! rows (games, game_battlesnakes, snake_turns) are never archived — only
//! frames plus the header.
//!
//! "Archived" means "as of the current terminal state": a game the sweeper
//! failed and later finished gets its archive fields cleared by
//! models::game::mark_game_finished_tx and is re-archived (same key,
//! complete frames) by the next discovery run.

use std::sync::Arc;

use chrono::Utc;
use color_eyre::eyre::{Context as _, eyre};
use sqlx::PgPool;
use uuid::Uuid;

use crate::backup::{BACKUP_JOB_PRIORITY, BACKUP_SLOT, BackupError, ZSTD_COMPRESSION_LEVEL};
use crate::engine_models::ARCHIVE_VERSION;
use crate::jobs::NativeBackupSingleGameJob;
use crate::state::AppState;
use cja::jobs::Job as _;

/// A native game is discoverable for archival for this long after its last
/// row update. Hourly discovery gives an unarchived game this many re-enqueue
/// passes — the safety net for a failed first attempt — while the 2 h SLA
/// needs only one. 6 h ≈ 17x the steady-state hourly game rate.
const NATIVE_RECENT_WINDOW_HOURS: i32 = 6;
/// Recent-tier cap: games/enqueue/hour whose terminal flip is fresh.
const NATIVE_RECENT_CAP: i64 = 2_500;
/// Backlog-tier cap: oldest-first games/enqueue/hour. 129k / 2000/h ≈ 2.7
/// days — the 7-day backfill bound with 2.6x margin.
const NATIVE_BACKLOG_CAP: i64 = 2_000;

/// Game header for a native archive object: verbatim `games` column values.
#[derive(Debug, Clone, serde::Serialize)]
struct NativeGameHeader {
    game_type: String,
    board_size: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// v2-family export object for one native game. `archive_version` and
/// `game_id` are declared first so they serialize first (self-describing,
/// same contract as engine_models::GameExport). `frames` are raw
/// `turns.frame_data` values, in turn_number order, unmodified.
#[derive(Debug, Clone, serde::Serialize)]
struct NativeGameExport {
    archive_version: i32,
    game_id: String,
    game: NativeGameHeader,
    frames: Vec<serde_json::Value>,
    exported_at: chrono::DateTime<chrono::Utc>,
}

/// GCS key: own top-level namespace (`native/`), distinct from Play's
/// `games/YYYY/MM/DD/<play_id>.json.zst` even though both ids are UUIDs.
fn native_gcs_path(game_id: Uuid, created_at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "native/{}/{:02}/{:02}/{}.json.zst",
        created_at.format("%Y"),
        created_at.format("%m"),
        created_at.format("%d"),
        game_id
    )
}

/// Fetch one native game's persisted frames as raw JSON text — the ONLY
/// `turns` read in the feature, one game at a time through the game_id
/// index, never a turns scan. `frame_data::text` makes Postgres render the
/// JSONB server-side; sqlx's TEXT -> String decode is a memcpy. NO
/// serde_json work happens here or anywhere else on the async side.
async fn fetch_native_frame_texts(db: &PgPool, game_id: Uuid) -> cja::Result<Vec<String>> {
    let texts: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT frame_data::text AS "frame_data!"
        FROM turns
        WHERE game_id = $1 AND frame_data IS NOT NULL
        ORDER BY turn_number ASC
        "#,
        game_id
    )
    .fetch_all(db)
    .await
    .wrap_err("Failed to fetch native game turns")?;
    Ok(texts)
}

/// Parse the raw frame texts, build the export, and zstd-compress its JSON.
/// ALL JSON-shaped work in the feature happens here — parse, serialize,
/// compress — so this runs on the blocking pool, never on an async worker.
/// Callers: the `spawn_blocking` closure in `archive_native_game`, and the
/// round-trip test (with texts from `fetch_native_frame_texts`).
fn build_native_compressed_export(
    game_id: Uuid,
    header: NativeGameHeader,
    frame_texts: Vec<String>,
) -> cja::Result<Vec<u8>> {
    let mut frames = Vec::with_capacity(frame_texts.len());
    for text in &frame_texts {
        let frame: serde_json::Value = serde_json::from_str(text)
            .wrap_err_with(|| format!("Failed to parse frame text for game {game_id}"))?;
        frames.push(frame);
    }
    let export = NativeGameExport {
        archive_version: ARCHIVE_VERSION,
        game_id: game_id.to_string(),
        game: header,
        frames,
        exported_at: Utc::now(),
    };
    let json = serde_json::to_vec(&export).wrap_err("Failed to serialize native game export")?;
    let compressed = zstd::stream::encode_all(json.as_slice(), ZSTD_COMPRESSION_LEVEL)
        .wrap_err("Failed to compress native game export")?;
    tracing::debug!(
        game_id = %game_id,
        json_size = json.len(),
        compressed_size = compressed.len(),
        "Compressed native game for upload"
    );
    Ok(compressed)
}

/// Record the completed archive on the game row. Row leaves the
/// games_native_unarchived_idx partial set (archived_at becomes non-NULL).
///
/// Status-guarded (optimistic concurrency): the UPDATE only lands when the
/// row's status still equals what this job's header fetch read. The race
/// this closes: the sweeper failed a slow-but-alive game, this job archived
/// it, and the live runner's finish transaction (mark_game_finished_tx)
/// flipped the row to finished and cleared the archive fields BETWEEN this
/// job's header fetch and this mark — without the guard, the stale
/// partial-frames archive would be re-recorded on the finished row. A
/// zero-row result is Ok: the game stays unarchived and the next discovery
/// run re-archives it, overwriting the stale object at the same key. (The
/// slot serializes all native marks against each other, so status — the
/// dimension the finish changes — is the only guard needed.)
async fn mark_native_game_archived(
    db: &PgPool,
    game_id: Uuid,
    gcs_path: &str,
    status_at_read: &str,
) -> cja::Result<()> {
    let result = sqlx::query!(
        r#"
        UPDATE games
        SET archived_at = NOW(), gcs_path = $2, archive_version = $3
        WHERE game_id = $1 AND status = $4
        "#,
        game_id,
        gcs_path,
        ARCHIVE_VERSION,
        status_at_read
    )
    .execute(db)
    .await
    .wrap_err("Failed to mark native game archived")?;

    if result.rows_affected() == 0 {
        tracing::info!(
            game_id = %game_id,
            expected_status = status_at_read,
            "Game status changed during archival; leaving unarchived for rediscovery"
        );
    }
    Ok(())
}

/// Header row as fetched for archival. `archived_at` is read fresh here —
/// under the slot — so the job's idempotency skip uses current state and a
/// duplicate queued behind this job sees the completed mark. TEXT and
/// timestamp columns only: nothing JSON-shaped is decoded on the async side.
#[derive(sqlx::FromRow)]
struct NativeGameHeaderRow {
    game_type: String,
    board_size: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Archive one native game to GCS: acquire the backup slot, fetch its header
/// and frame TEXTS, then parse/compress/upload/record. Idempotent: `archived_at` is
/// re-checked from the DB WHILE HOLDING the slot, so a duplicate enqueued
/// behind this job no-ops after this job's mark instead of uploading again.
/// Games that are missing or no longer terminal-native (e.g. reset for
/// retry) also no-op. No serde_json work runs on the async worker: the
/// frame texts are fetched as `::text` (a memcpy) and parsed inside the
/// spawn_blocking closure below.
pub(crate) async fn archive_native_game(
    app_state: &AppState,
    game_id: Uuid,
) -> Result<(), BackupError> {
    // Acquire FIRST, before any per-game data is fetched:
    // - memory: up to 100 workers can hold queued native jobs; fetching
    //   before acquiring would leave every blocked worker holding a full
    //   game's frame data — only the slot holder ever holds per-game data.
    // - dedup: the archived_at check below runs under the slot, so a
    //   duplicate sees this job's completed mark.
    // - drain safety: the owned permit moves into spawn_blocking below, so
    //   a deploy dropping this future mid-parse/compress cannot free the
    //   slot while the CPU work still runs.
    let permit = Arc::clone(&BACKUP_SLOT)
        .acquire_owned()
        .await
        .wrap_err("Backup slot semaphore closed")?;

    let row: Option<NativeGameHeaderRow> = sqlx::query_as!(
        NativeGameHeaderRow,
        r#"
        SELECT game_type, board_size, status, created_at, finished_at, archived_at
        FROM games
        WHERE game_id = $1
          AND engine_game_id IS NULL
          AND status IN ('finished', 'failed')
        "#,
        game_id
    )
    .fetch_optional(&app_state.db)
    .await
    .wrap_err("Failed to fetch native game for archival")?;

    let Some(row) = row else {
        tracing::debug!(%game_id, "Game is not a terminal native game, skipping archive");
        return Ok(());
    };
    if row.archived_at.is_some() {
        tracing::debug!(%game_id, "Native game already archived, skipping");
        return Ok(());
    }

    // After the skips (idempotent no-ops stay no-ops even in a
    // misconfigured env) and before the turns fetch. The store owns the
    // bucket; `is_configured()` is false only for DisabledObjectStore.
    if !app_state.object_store.is_configured() {
        return Err(eyre!("GCS bucket not configured").into());
    };

    // One game at a time through the game_id index — never a turns scan.
    // ::text => Vec<String>: a memcpy, NO parse on the async worker.
    // Only the slot holder reaches this line.
    let frame_texts = fetch_native_frame_texts(&app_state.db, game_id).await?;

    let status_at_read = row.status.clone();
    let created_at = row.created_at;
    let header = NativeGameHeader {
        game_type: row.game_type,
        board_size: row.board_size,
        status: row.status,
        created_at: row.created_at,
        finished_at: row.finished_at,
    };

    // The frame texts AND the permit ride INTO the closure (slot held for
    // the CPU work even if this future is dropped mid-drain) and the permit
    // comes back OUT (slot held through upload + mark). ALL JSON work —
    // parse, serialize, zstd-9 — happens inside, on the blocking pool.
    // Panics in the closure surface as the JoinError that `?` converts to Err.
    let (_permit, build_result) = tokio::task::spawn_blocking(move || {
        let result = build_native_compressed_export(game_id, header, frame_texts);
        (permit, result)
    })
    .await
    .wrap_err("Native backup compression task panicked")?;
    let compressed = build_result?;

    let path = native_gcs_path(game_id, created_at);
    app_state.object_store.put(&path, compressed).await?;

    mark_native_game_archived(&app_state.db, game_id, &path, &status_at_read).await?;

    tracing::info!(game_id = %game_id, path = %path, "Archived native game");
    Ok(())
}

/// Hourly discovery of native games needing archive: a recent tier (fresh
/// terminal flips — the 2 h SLA path, never starved by the backlog) and a
/// backlog tier (oldest unarchived first — the backfill). Both filter on
/// archived_at IS NULL, so re-running never re-enqueues archived games.
pub(crate) async fn run_native_backup_discovery(app_state: &AppState) -> Result<(), BackupError> {
    run_discovery_with_caps(
        app_state,
        NATIVE_RECENT_WINDOW_HOURS,
        NATIVE_RECENT_CAP,
        NATIVE_BACKLOG_CAP,
    )
    .await
}

async fn run_discovery_with_caps(
    app_state: &AppState,
    recent_window_hours: i32,
    recent_cap: i64,
    backlog_cap: i64,
) -> Result<(), BackupError> {
    if !app_state.object_store.is_configured() {
        tracing::warn!("GCS bucket not configured, skipping native backup discovery");
        return Ok(());
    }

    let recent: Vec<Uuid> = sqlx::query_scalar!(
        r#"
        SELECT game_id FROM games
        WHERE engine_game_id IS NULL
          AND status IN ('finished', 'failed')
          AND archived_at IS NULL
          AND updated_at >= NOW() - make_interval(hours => $1)
        LIMIT $2
        "#,
        recent_window_hours,
        recent_cap
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to fetch recent unarchived native games")?;

    let backlog: Vec<Uuid> = sqlx::query_scalar!(
        r#"
        SELECT game_id FROM games
        WHERE engine_game_id IS NULL
          AND status IN ('finished', 'failed')
          AND archived_at IS NULL
          AND updated_at < NOW() - make_interval(hours => $1)
        ORDER BY created_at ASC, game_id ASC
        LIMIT $2
        "#,
        recent_window_hours,
        backlog_cap
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to fetch backlog unarchived native games")?;

    for game_id in recent.iter().chain(backlog.iter()) {
        NativeBackupSingleGameJob { game_id: *game_id }
            .enqueue(
                app_state.clone(),
                format!("archive native game {game_id}"),
                Some(BACKUP_JOB_PRIORITY),
            )
            .await
            .wrap_err_with(|| format!("Failed to enqueue native backup job for {game_id}"))?;
    }

    tracing::info!(
        recent = recent.len(),
        backlog = backlog.len(),
        "Native backup discovery complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Insert a games row with explicit timestamps (the updated_at trigger only
    /// fires on UPDATE, so old rows must set both timestamps in the INSERT).
    async fn fixture_game(
        pool: &sqlx::PgPool,
        status: &str,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
        archived: bool,
        engine_game_id: Option<&str>,
    ) -> cja::Result<Uuid> {
        let id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO games (board_size, game_type, status, created_at, updated_at, engine_game_id, archived_at)
               VALUES ('11x11', 'Standard', $1, $2, $3, $4, $5) RETURNING game_id"#)
            .bind(status).bind(created_at).bind(updated_at)
            .bind(engine_game_id)
            .bind(archived.then(chrono::Utc::now))
            .fetch_one(pool).await?;
        Ok(id)
    }

    /// Insert one turns row with the given frame JSON (binds serde_json::Value
    /// straight into the JSONB column).
    async fn fixture_turn(
        pool: &sqlx::PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame: serde_json::Value,
    ) -> cja::Result<()> {
        sqlx::query(r#"INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)"#)
            .bind(game_id)
            .bind(turn_number)
            .bind(frame)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// The production comparison query, test-side: frame_data decoded as Value,
    /// turn order. (Value decode in TEST code is fine — the charter bar governs
    /// the job path, not assertions.)
    async fn db_frames_as_values(
        pool: &sqlx::PgPool,
        game_id: Uuid,
    ) -> cja::Result<Vec<serde_json::Value>> {
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            r#"SELECT frame_data FROM turns
               WHERE game_id = $1 AND frame_data IS NOT NULL
               ORDER BY turn_number ASC"#,
        )
        .bind(game_id)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(|(v,)| v).collect())
    }

    /// Simulate a fully-completed archive mark (pure-SQL fixtures).
    async fn mark_archived_via_sql(pool: &sqlx::PgPool, game_id: Uuid) -> cja::Result<()> {
        sqlx::query("UPDATE games SET archived_at = NOW(), gcs_path = $2, archive_version = 2 WHERE game_id = $1")
            .bind(game_id).bind("native/2026/10/31/test.json.zst")
            .execute(pool).await?;
        Ok(())
    }

    /// game_ids of every queued NativeBackupSingleGameJob row.
    async fn enqueued_native_job_ids(pool: &sqlx::PgPool) -> cja::Result<Vec<serde_json::Value>> {
        let ids = sqlx::query_scalar(
            r#"SELECT payload->'game_id' FROM jobs WHERE name = 'NativeBackupSingleGameJob'"#,
        )
        .fetch_all(pool)
        .await?;
        Ok(ids)
    }

    /// Configured env: a fresh InMemoryObjectStore on AppState.object_store
    /// (is_configured() == true; discovery and enqueueing never contact GCS).
    fn configured_state(state: &mut AppState) {
        state.object_store = std::sync::Arc::new(crate::object_store::InMemoryObjectStore::default());
    }

    /// A modern full frame: every serialized EngineGameFrame/FrameSnake key
    /// with real PascalCase keys.
    fn modern_frame(turn: i32) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": "snake-1",
                "Name": "Test Snake",
                "Body": [{"X": 5, "Y": 5}],
                "Health": 100,
                "Color": "#ff0000",
                "HeadType": "sugar-skull",
                "TailType": "fat-rattle",
                "Latency": "42",
                "Shout": "hello",
                "Squad": "",
                "APIVersion": "1",
                "Author": "corey",
                "Death": null,
                "EliminatedCause": "",
                "EliminatedBy": "",
                "StatusCode": 200,
                "Error": "",
                "TimingMicros": {"total": 42},
                "IsBot": false,
                "IsEnvironment": false,
                "URL": "",
                "ProxyURL": ""
            }],
            "Food": [{"X": 1, "Y": 2}],
            "Hazards": [{"X": 3, "Y": 4}],
            "GameState": {},
            "PointState": []
        })
    }

    /// A legacy pre-DEV-1502 sparse frame: no StatusCode, no Hazards, no
    /// URL keys, timeout recorded as `Latency: "timeout"`.
    fn legacy_frame(turn: i32) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": "snake-1",
                "Name": "Test Snake",
                "Body": [{"X": 5, "Y": 6}],
                "Health": 99,
                "Latency": "timeout",
                "Death": null
            }],
            "Food": [{"X": 1, "Y": 2}]
        })
    }

    /// SC: the export holds the persisted frames exactly as stored. DB-backed
    /// end to end through the production fetch/build fns: a modern frame and
    /// a legacy sparse frame must both come through byte-for-value identical,
    /// with no normalization applied.
    #[sqlx::test(migrations = "../migrations")]
    async fn native_export_round_trips_raw_frames_exactly(pool: sqlx::PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let created_at = Utc::now() - chrono::Duration::days(5);
        let game_id = fixture_game(&pool, "failed", created_at, Utc::now(), false, None).await?;
        fixture_turn(&pool, game_id, 0, modern_frame(0)).await?;
        fixture_turn(&pool, game_id, 1, legacy_frame(1)).await?;

        // The production path, minus the job wrapper.
        let texts = fetch_native_frame_texts(&state.db, game_id).await?;
        let header = NativeGameHeader {
            game_type: "Standard".to_string(),
            board_size: "11x11".to_string(),
            status: "failed".to_string(),
            created_at,
            finished_at: None,
        };
        let compressed = build_native_compressed_export(game_id, header, texts)?;

        let json = zstd::decode_all(compressed.as_slice())?;
        // Field order is load-bearing: a reader identifies the format and
        // game before anything else.
        assert!(json.starts_with(br#"{"archive_version":2,"game_id":""#.as_slice()));
        let uuid_prefix = format!(r#"{{"archive_version":2,"game_id":"{game_id}","#);
        assert!(
            json.starts_with(uuid_prefix.as_bytes()),
            "export must start with archive_version and game_id, got: {}",
            String::from_utf8_lossy(&json[..uuid_prefix.len().min(json.len())])
        );

        let export: serde_json::Value = serde_json::from_slice(&json)?;

        // Raw passthrough: exactly the DB frame values, in turn order — a
        // legacy `Latency: "timeout"` that came back `"500"` or a
        // materialized StatusCode fails this.
        let db_frames = db_frames_as_values(&pool, game_id).await?;
        let export_frames = export["frames"].as_array().expect("frames array");
        assert_eq!(export_frames.len(), db_frames.len());
        for (exported, db) in export_frames.iter().zip(db_frames.iter()) {
            assert_eq!(exported, db);
        }
        assert_eq!(export_frames[0], modern_frame(0));
        assert_eq!(export_frames[1], legacy_frame(1));

        // Header: verbatim games column values.
        let game = &export["game"];
        assert_eq!(game["game_type"], "Standard");
        assert_eq!(game["board_size"], "11x11");
        assert_eq!(game["status"], "failed");
        // RFC3339 forms of the inputs, exactly as serde renders them.
        assert_eq!(game["created_at"], serde_json::to_value(created_at)?);
        assert!(game["finished_at"].is_null());

        // Same function + level + input => same bytes.
        let reencoded = zstd::stream::encode_all(json.as_slice(), 9)?;
        assert_eq!(compressed, reencoded);

        Ok(())
    }

    /// SC: native keys can't collide with Play's namespace.
    #[test]
    fn native_gcs_path_uses_its_own_namespace() {
        let uuid = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let created = chrono::DateTime::parse_from_rfc3339("2026-01-29T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let path = native_gcs_path(uuid, created);
        assert_eq!(
            path,
            "native/2026/01/29/00000000-0000-0000-0000-000000000001.json.zst"
        );
        assert!(!path.starts_with("games/"));
        assert_ne!(path, format!("games/2026/01/29/{uuid}.json.zst"));
    }

    /// SC: discovery enqueues exactly the unarchived terminal native games —
    /// not running ones, not archived ones, not imported (engine) games.
    #[sqlx::test(migrations = "../migrations")]
    async fn native_discovery_enqueues_unarchived_terminal_native_games(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        configured_state(&mut state);
        let now = Utc::now();
        let finished = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        let failed = fixture_game(
            &pool,
            "failed",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        let running = fixture_game(
            &pool,
            "running",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        let archived = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(5),
            now,
            true,
            None,
        )
        .await?;
        let imported = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(5),
            now,
            false,
            Some("11111111-1111-1111-1111-111111111111"),
        )
        .await?;

        run_discovery_with_caps(&state, 6, 10, 10).await?;

        let ids = enqueued_native_job_ids(&pool).await?;
        let mut got: Vec<String> = ids
            .into_iter()
            .map(|v| v.as_str().expect("game_id string").to_string())
            .collect();
        got.sort_unstable();
        let mut want = vec![finished.to_string(), failed.to_string()];
        want.sort_unstable();
        assert_eq!(got, want, "only unarchived terminal native games enqueued");
        assert!(!got.contains(&running.to_string()));
        assert!(!got.contains(&archived.to_string()));
        assert!(!got.contains(&imported.to_string()));
        Ok(())
    }

    /// SC: the backlog tier drains oldest-first, and the recent tier is never
    /// crowded out by the backlog cap.
    #[sqlx::test(migrations = "../migrations")]
    async fn native_discovery_backlog_tier_enqueues_oldest_unarchived_games(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        configured_state(&mut state);
        let now = Utc::now();
        let a = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(30),
            now - chrono::Duration::days(2),
            false,
            None,
        )
        .await?;
        let b = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(20),
            now - chrono::Duration::days(2),
            false,
            None,
        )
        .await?;
        let c = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(10),
            now - chrono::Duration::days(2),
            false,
            None,
        )
        .await?;
        let r = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(1),
            now,
            false,
            None,
        )
        .await?;

        run_discovery_with_caps(&state, 6, 10, 2).await?;

        let ids = enqueued_native_job_ids(&pool).await?;
        let contains = |id: Uuid| {
            ids.iter()
                .any(|v| v.as_str() == Some(id.to_string().as_str()))
        };
        assert!(contains(a), "oldest backlog game A enqueued");
        assert!(contains(b), "second-oldest backlog game B enqueued");
        assert!(contains(r), "recent game R not crowded out");
        assert!(!contains(c), "game C beyond backlog cap not enqueued");
        Ok(())
    }

    /// SC: re-running discovery doesn't re-enqueue archived games.
    #[sqlx::test(migrations = "../migrations")]
    async fn native_discovery_is_idempotent_for_archived_games(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        configured_state(&mut state);
        let now = Utc::now();
        let game_id = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;

        run_discovery_with_caps(&state, 6, 10, 10).await?;
        let after_first = enqueued_native_job_ids(&pool).await?;
        assert_eq!(
            after_first
                .iter()
                .filter(|v| v.as_str() == Some(game_id.to_string().as_str()))
                .count(),
            1
        );

        mark_archived_via_sql(&pool, game_id).await?;
        run_discovery_with_caps(&state, 6, 10, 10).await?;
        let after_second = enqueued_native_job_ids(&pool).await?;
        assert_eq!(
            after_second
                .iter()
                .filter(|v| v.as_str() == Some(game_id.to_string().as_str()))
                .count(),
            1,
            "no new job row for the archived game"
        );
        Ok(())
    }

    /// SC: the per-game job's skip checks precede any store/GCS touch — all
    /// in the misconfigured env (no bucket), where reaching the upload would
    /// error.
    #[sqlx::test(migrations = "../migrations")]
    async fn archive_native_game_skips_already_archived_and_missing_games(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        // Misconfigured env: DisabledObjectStore (is_configured() == false;
        // reaching the upload would error).
        let mut state = AppState::test_from_pool(pool.clone());
        state.object_store = std::sync::Arc::new(crate::object_store::DisabledObjectStore);
        let now = Utc::now();

        // (a) already archived (gcs_path still NULL) -> no-op
        let archived = fixture_game(
            &pool,
            "failed",
            now - chrono::Duration::days(5),
            now,
            true,
            None,
        )
        .await?;
        archive_native_game(&state, archived).await?;
        let path: Option<String> =
            sqlx::query_scalar("SELECT gcs_path FROM games WHERE game_id = $1")
                .bind(archived)
                .fetch_one(&pool)
                .await?;
        assert!(path.is_none(), "gcs_path stays NULL for the skipped game");

        // (b) missing game -> no-op
        archive_native_game(&state, Uuid::new_v4()).await?;

        // (c) native running game -> no-op, no rows touched
        let running = fixture_game(
            &pool,
            "running",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        archive_native_game(&state, running).await?;
        let archived_at: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT archived_at FROM games WHERE game_id = $1")
                .bind(running)
                .fetch_one(&pool)
                .await?;
        assert!(archived_at.is_none());

        Ok(())
    }

    /// SC: a duplicate job queued behind a completed archive no-ops at the
    /// under-slot archived_at re-check (in the misconfigured env, reaching
    /// the upload would have errored — Ok(()) proves the skip).
    #[sqlx::test(migrations = "../migrations")]
    async fn archive_native_game_is_idempotent_after_a_completed_archive(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        // Misconfigured env: DisabledObjectStore (reaching the upload would
        // have errored — Ok(()) proves the skip).
        let mut state = AppState::test_from_pool(pool.clone());
        state.object_store = std::sync::Arc::new(crate::object_store::DisabledObjectStore);
        let now = Utc::now();
        let game_id = fixture_game(
            &pool,
            "failed",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;

        // Job A completed and marked.
        mark_archived_via_sql(&pool, game_id).await?;

        // Job B (the duplicate) runs.
        archive_native_game(&state, game_id).await?;

        let (gcs_path, archive_version): (String, i32) =
            sqlx::query_as("SELECT gcs_path, archive_version FROM games WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(gcs_path, "native/2026/10/31/test.json.zst");
        assert_eq!(archive_version, 2);
        Ok(())
    }

    /// SC: the archive mark records path + version, and its status guard
    /// refuses to record a stale archive on a row whose status changed.
    #[sqlx::test(migrations = "../migrations")]
    async fn mark_native_game_archived_sets_row_fields(pool: sqlx::PgPool) -> cja::Result<()> {
        let now = Utc::now();
        // (a) positive
        let guarded = fixture_game(
            &pool,
            "failed",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        mark_native_game_archived(&pool, guarded, "native/2026/10/31/x.json.zst", "failed").await?;
        let (archived_at, gcs_path, archive_version): (
            Option<chrono::DateTime<Utc>>,
            Option<String>,
            Option<i32>,
        ) = sqlx::query_as(
            "SELECT archived_at, gcs_path, archive_version FROM games WHERE game_id = $1",
        )
        .bind(guarded)
        .fetch_one(&pool)
        .await?;
        assert!(archived_at.is_some());
        assert_eq!(gcs_path.as_deref(), Some("native/2026/10/31/x.json.zst"));
        assert_eq!(archive_version, Some(2));

        // (b) guard: the finish raced past this job's header read.
        let raced = fixture_game(
            &pool,
            "failed",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        sqlx::query("UPDATE games SET status = 'finished' WHERE game_id = $1")
            .bind(raced)
            .execute(&pool)
            .await?;
        mark_native_game_archived(&pool, raced, "native/2026/10/31/y.json.zst", "failed").await?;
        let archived_at: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT archived_at FROM games WHERE game_id = $1")
                .bind(raced)
                .fetch_one(&pool)
                .await?;
        assert!(archived_at.is_none(), "stale archive must not be recorded");
        Ok(())
    }

    /// SC: finishing a sweeper-failed game invalidates its failed-era archive
    /// and makes it discoverable again.
    #[sqlx::test(migrations = "../migrations")]
    async fn finishing_a_failed_game_clears_stale_archive_and_rediscovers(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        let now = Utc::now();
        let game_id = fixture_game(
            &pool,
            "failed",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;
        mark_archived_via_sql(&pool, game_id).await?;

        // The finish path's actual write.
        let mut tx = pool.begin().await?;
        crate::models::game::mark_game_finished_tx(&mut tx, game_id, Utc::now()).await?;
        tx.commit().await?;

        #[derive(sqlx::FromRow)]
        struct GameRow {
            status: String,
            finished_at: Option<chrono::DateTime<Utc>>,
            archived_at: Option<chrono::DateTime<Utc>>,
            gcs_path: Option<String>,
            archive_version: Option<i32>,
        }
        let row: GameRow = sqlx::query_as(
            "SELECT status, finished_at, archived_at, gcs_path, archive_version FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.status, "finished");
        assert!(row.finished_at.is_some());
        assert!(row.archived_at.is_none());
        assert!(row.gcs_path.is_none());
        assert!(row.archive_version.is_none());

        configured_state(&mut state);
        run_discovery_with_caps(&state, 6, 10, 10).await?;
        let ids = enqueued_native_job_ids(&pool).await?;
        assert!(
            ids.iter()
                .any(|v| v.as_str() == Some(game_id.to_string().as_str()))
        );
        Ok(())
    }

    /// SC: native archive jobs enqueue at the backup priority, below game
    /// runners.
    #[sqlx::test(migrations = "../migrations")]
    async fn native_backup_jobs_enqueue_at_backup_priority(pool: sqlx::PgPool) -> cja::Result<()> {
        use cja::jobs::Job as _;

        let mut state = AppState::test_from_pool(pool.clone());
        configured_state(&mut state);
        let now = Utc::now();
        fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(5),
            now,
            false,
            None,
        )
        .await?;

        run_discovery_with_caps(&state, 6, 10, 10).await?;
        crate::jobs::GameRunnerJob {
            game_id: Uuid::new_v4(),
        }
        .enqueue(state.clone(), "game".to_string(), None)
        .await?;

        let rows: Vec<(String, i32)> = sqlx::query_as("SELECT name, priority FROM jobs")
            .fetch_all(&pool)
            .await?;
        let native: Vec<i32> = rows
            .iter()
            .filter(|(name, _)| name == "NativeBackupSingleGameJob")
            .map(|(_, p)| *p)
            .collect();
        assert!(!native.is_empty());
        assert!(native.iter().all(|&p| p == BACKUP_JOB_PRIORITY));
        let runner: Vec<i32> = rows
            .iter()
            .filter(|(name, _)| name == "GameRunnerJob")
            .map(|(_, p)| *p)
            .collect();
        assert_eq!(runner, vec![0], "game runners keep the default priority");
        assert!(runner[0] > BACKUP_JOB_PRIORITY);
        Ok(())
    }
}
