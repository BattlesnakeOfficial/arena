//! Game backup module for archiving games from the Engine database to GCS.

use std::io::Write;

use chrono::{Duration, Utc};
use color_eyre::eyre::{Context as _, eyre};
use google_cloud_storage::{
    client::{Client as GcsClient, ClientConfig},
    http::objects::upload::{Media, UploadObjectRequest, UploadType},
};
use sqlx::{FromRow, PgPool};

use crate::engine_models::{ARCHIVE_VERSION, EngineGame, EngineGameFrame, GameExport};
use crate::jobs::{BackupSingleGameJob, HistoricalBackupDiscoveryJob};
use crate::state::AppState;
use cja::jobs::Job;

/// Batch size for historical backfill discovery
const HISTORICAL_BATCH_SIZE: i32 = 500;

/// Backups run one at a time per process. Discovery enqueues a burst of
/// jobs every hour, and prod has a single vCPU shared with live games: a
/// burst of concurrent frame parsing + zstd stalled every running game for
/// ~300ms, long enough to time out snakes mid-turn.
static BACKUP_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Job priority for [`BackupSingleGameJob`], below the default 0 that game
/// runners use. cja claims `ORDER BY priority DESC, run_at, created_at`, and a
/// claimed backup holds its worker while it waits for [`BACKUP_SLOT`], so an
/// hourly burst of ~1,100 backups pins most workers and frees them one backup at
/// a time. At equal priority each freed worker took the next backup, and a game
/// queued behind the burst waited 15-111s to start. Lower, the next freed worker
/// takes the game.
pub(crate) const BACKUP_JOB_PRIORITY: i32 = -10;

/// zstd level for archive objects. 9 vs the old 3 is 1.5-1.8x smaller for
/// ~1ms/game and faster to decompress (bench 2026-10-30,
/// learning arena-game-archive-format-bench-2026-10). 19 is rejected: it
/// takes 50-90ms/game and stalled live games on the shared vCPU (DEV-1498).
const ZSTD_COMPRESSION_LEVEL: i32 = 9;

/// Enqueue one game's backup at [`BACKUP_JOB_PRIORITY`].
async fn enqueue_backup_job(
    app_state: &AppState,
    engine_game_id: &str,
    batch_id: Option<i32>,
) -> cja::Result<()> {
    BackupSingleGameJob {
        engine_game_id: engine_game_id.to_owned(),
        batch_id,
    }
    .enqueue(
        app_state.clone(),
        format!("backup game {engine_game_id}"),
        Some(BACKUP_JOB_PRIORITY),
    )
    .await
    .wrap_err_with(|| format!("Failed to enqueue backup job for game {engine_game_id}"))?;
    Ok(())
}

/// Shared GCS client. Building one per job re-ran TLS setup and a metadata
/// token fetch for every archived game; the client refreshes its own token.
static GCS_CLIENT: tokio::sync::OnceCell<GcsClient> = tokio::sync::OnceCell::const_new();

async fn gcs_client() -> cja::Result<&'static GcsClient> {
    GCS_CLIENT
        .get_or_try_init(|| async {
            let config = ClientConfig::default()
                .with_auth()
                .await
                .wrap_err("Failed to configure GCS client")?;
            Ok(GcsClient::new(config))
        })
        .await
}

/// Row from Engine's games table
#[derive(FromRow)]
struct EngineGameRow {
    id: String,
    value: serde_json::Value,
    /// Engine DB uses TIMESTAMP (no timezone), not TIMESTAMPTZ
    created: chrono::NaiveDateTime,
}

/// Fetch completed games from the Engine database within the given time window.
async fn fetch_completed_games(
    engine_db: &PgPool,
    hours_ago: i64,
) -> cja::Result<Vec<EngineGameRow>> {
    // Engine DB uses TIMESTAMP (no timezone), so use NaiveDateTime
    let since = (Utc::now() - Duration::hours(hours_ago)).naive_utc();

    // Note: We use query_as (not the macro) because this is a different database
    // with a different schema that sqlx doesn't know about at compile time.
    // Limit to 5000 as a safety valve - if we hit this, we'll catch the rest next run.
    let rows: Vec<EngineGameRow> = sqlx::query_as(
        r#"
        SELECT id, value, created
        FROM games
        WHERE value->>'Status' IN ('complete', 'error')
          AND created >= $1
        ORDER BY created ASC
        LIMIT 5000
        "#,
    )
    .bind(since)
    .fetch_all(engine_db)
    .await
    .wrap_err("Failed to fetch completed games from Engine")?;

    Ok(rows)
}

/// Fetch a single game from the Engine database by ID.
async fn fetch_game_by_id(engine_db: &PgPool, game_id: &str) -> cja::Result<Option<EngineGameRow>> {
    let row: Option<EngineGameRow> = sqlx::query_as(
        r#"
        SELECT id, value, created
        FROM games
        WHERE id = $1
        "#,
    )
    .bind(game_id)
    .fetch_optional(engine_db)
    .await
    .wrap_err("Failed to fetch game from Engine")?;

    Ok(row)
}

/// Fetch all frames for a game from the Engine database, unparsed.
async fn fetch_game_frames(
    engine_db: &PgPool,
    game_id: &str,
) -> cja::Result<Vec<serde_json::Value>> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        r#"
        SELECT value
        FROM game_frames
        WHERE id = $1
        ORDER BY turn ASC
        "#,
    )
    .bind(game_id)
    .fetch_all(engine_db)
    .await
    .wrap_err("Failed to fetch game frames from Engine")?;

    Ok(rows.into_iter().map(|(value,)| value).collect())
}

/// Check if a game has already been archived (exists in local games table with archived_at set).
async fn is_already_archived(db: &PgPool, engine_game_id: &str) -> cja::Result<bool> {
    let result = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM games
            WHERE engine_game_id = $1 AND archived_at IS NOT NULL
        ) as "exists!"
        "#,
        engine_game_id
    )
    .fetch_one(db)
    .await
    .wrap_err("Failed to check if game is already archived")?;

    Ok(result)
}

/// Generate the GCS path for a game based on its creation date.
fn gcs_path(game: &EngineGame) -> String {
    let created = game.created_at();
    format!(
        "games/{}/{:02}/{:02}/{}.json.zst",
        created.format("%Y"),
        created.format("%m"),
        created.format("%d"),
        game.id
    )
}

/// Blank the engine-only snake fields that must never be archived: real
/// snake URLs can embed secrets and no archive consumer needs them. Mirrors
/// what `engine::frame::normalize_public_frame` blanks at serve time.
fn strip_secret_snake_fields(frames: &mut [EngineGameFrame]) {
    for frame in frames.iter_mut() {
        for snake in frame.snakes.iter_mut() {
            snake.url = None;
            snake.proxy_url = None;
            snake.api_version = None;
        }
    }
}

/// Parse frames, build the export, and zstd-compress its JSON. Pure CPU
/// work: callers run it on the blocking pool, never on an async worker.
fn build_compressed_export(
    game: EngineGame,
    frame_values: Vec<serde_json::Value>,
) -> cja::Result<Vec<u8>> {
    let mut frames: Vec<EngineGameFrame> = frame_values
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .wrap_err("Failed to deserialize game frames")?;

    strip_secret_snake_fields(&mut frames);

    let export = GameExport {
        archive_version: ARCHIVE_VERSION,
        game_id: game.id.clone(),
        game,
        frames,
        exported_at: Utc::now(),
    };

    // Serialize to JSON
    let json = serde_json::to_vec(&export).wrap_err("Failed to serialize game export")?;

    let mut encoder = zstd::Encoder::new(Vec::new(), ZSTD_COMPRESSION_LEVEL)
        .wrap_err("Failed to create zstd encoder")?;
    encoder
        .write_all(&json)
        .wrap_err("Failed to write to zstd encoder")?;
    let compressed = encoder
        .finish()
        .wrap_err("Failed to finish zstd compression")?;

    tracing::debug!(
        game_id = %export.game.id,
        json_size = json.len(),
        compressed_size = compressed.len(),
        ratio = format!("{:.1}%", (compressed.len() as f64 / json.len() as f64) * 100.0),
        "Compressed game for upload"
    );

    Ok(compressed)
}

/// Upload an already-compressed export to GCS.
async fn upload_to_gcs(
    client: &GcsClient,
    bucket: &str,
    path: &str,
    compressed: Vec<u8>,
) -> cja::Result<()> {
    let upload_type = UploadType::Simple(Media::new(path.to_string()));
    client
        .upload_object(
            &UploadObjectRequest {
                bucket: bucket.to_string(),
                ..Default::default()
            },
            compressed,
            &upload_type,
        )
        .await
        .wrap_err("Failed to upload to GCS")?;

    Ok(())
}

/// Insert or update a game record in the local database after archiving.
///
/// Imported games get no `game_battlesnakes` rows. `models::snake_latency`
/// relies on that to skip an `engine_game_id IS NULL` filter that wrecks its
/// query plan; linking snakes to imported games would need that filter back.
async fn upsert_game_record(db: &PgPool, game: &EngineGame, gcs_path: &str) -> cja::Result<()> {
    let now = Utc::now();
    let board_size = game.board_size();
    let game_type = game.game_type();
    let created_at = game.created_at();

    sqlx::query!(
        r#"
        INSERT INTO games (engine_game_id, board_size, game_type, status, created_at, archived_at, gcs_path, archive_version)
        VALUES ($1, $2, $3, 'finished', $4, $5, $6, $7)
        ON CONFLICT (engine_game_id) DO UPDATE SET
            archived_at = $5,
            gcs_path = $6,
            archive_version = $7,
            updated_at = $5
        "#,
        game.id,
        board_size,
        game_type,
        created_at,
        now,
        gcs_path,
        ARCHIVE_VERSION
    )
    .execute(db)
    .await
    .wrap_err("Failed to upsert game record")?;

    Ok(())
}

/// Backup error type that implements std::error::Error for cron compatibility.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BackupError {
    message: String,
}

impl From<color_eyre::Report> for BackupError {
    fn from(err: color_eyre::Report) -> Self {
        Self {
            message: format!("{err:?}"),
        }
    }
}

/// Hours to look back for games to backup.
const BACKUP_WINDOW_HOURS: i64 = 4;

/// Run the game backup discovery process.
///
/// Finds completed games from the Engine database and enqueues individual
/// backup jobs for each game that hasn't been archived yet.
pub async fn run_backup_discovery(app_state: &AppState) -> Result<(), BackupError> {
    tracing::info!(
        window_hours = BACKUP_WINDOW_HOURS,
        "Starting backup discovery"
    );

    let engine_db = match &app_state.engine_db {
        Some(db) => db,
        None => {
            tracing::warn!("Engine database not configured, skipping backup discovery");
            return Ok(());
        }
    };

    // Fetch games from the lookback window
    let games = fetch_completed_games(engine_db, BACKUP_WINDOW_HOURS).await?;
    tracing::info!(
        count = games.len(),
        "Found completed games to check for archival"
    );

    let mut enqueued_count = 0;
    let mut skipped_count = 0;

    for game_row in games {
        // Check if already archived
        if is_already_archived(&app_state.db, &game_row.id).await? {
            skipped_count += 1;
            continue;
        }

        // Enqueue a job to backup this game (no batch_id for regular discovery)
        enqueue_backup_job(app_state, &game_row.id, None).await?;

        enqueued_count += 1;
    }

    tracing::info!(
        enqueued = enqueued_count,
        skipped = skipped_count,
        "Backup discovery complete"
    );

    Ok(())
}

/// Backup a single game from the Engine database to GCS.
///
/// Called by BackupSingleGameJob. Fetches the game and frames from Engine,
/// compresses and uploads to GCS, and records the archival in the local database.
///
/// If `batch_id` is provided, this is part of a historical backfill batch.
/// On completion, the batch's completed count will be incremented, and if this
/// is the last job in the batch, the next discovery job will be enqueued.
pub async fn backup_single_game(
    app_state: &AppState,
    engine_game_id: &str,
    batch_id: Option<i32>,
) -> Result<(), BackupError> {
    // Check if already archived (idempotency)
    if is_already_archived(&app_state.db, engine_game_id).await? {
        tracing::debug!(game_id = %engine_game_id, "Game already archived, skipping");
        return Ok(());
    }

    let engine_db = match &app_state.engine_db {
        Some(db) => db,
        None => {
            return Err(eyre!("Engine database not configured").into());
        }
    };

    let bucket = match &app_state.config.gcs_bucket {
        Some(b) => b.clone(),
        None => {
            return Err(eyre!("GCS bucket not configured").into());
        }
    };

    // One backup at a time: the parse/compress below competes with live
    // games for the CPU (see BACKUP_SLOT).
    let _slot = BACKUP_SLOT
        .acquire()
        .await
        .wrap_err("Backup slot semaphore closed")?;

    // Fetch the game from Engine
    let game_row = fetch_game_by_id(engine_db, engine_game_id)
        .await?
        .ok_or_else(|| eyre!("Game {} not found in Engine database", engine_game_id))?;

    // Parse the game data
    let game: EngineGame = serde_json::from_value(game_row.value)
        .wrap_err_with(|| format!("Failed to parse game data for {}", engine_game_id))?;

    // Fetch frames
    let frame_values = fetch_game_frames(engine_db, &game.id).await?;

    // Parse + serialize + compress off the async workers
    let export_game = game.clone();
    let compressed =
        tokio::task::spawn_blocking(move || build_compressed_export(export_game, frame_values))
            .await
            .wrap_err("Backup compression task panicked")??;

    // Generate path and upload
    let path = gcs_path(&game);
    upload_to_gcs(gcs_client().await?, &bucket, &path, compressed).await?;

    // Record in local database
    upsert_game_record(&app_state.db, &game, &path).await?;

    tracing::info!(game_id = %game.id, path = %path, "Archived game");

    // If this is part of a batch, handle completion tracking
    if let Some(batch_id) = batch_id {
        handle_batch_completion(app_state, batch_id).await?;
    }

    Ok(())
}

// =============================================================================
// Historical Backfill
// =============================================================================

/// Start the historical backfill process.
///
/// This should be called once to kick off the backfill. It will check if there's
/// already an incomplete batch in progress. If so, it does nothing.
/// Otherwise, it enqueues the first HistoricalBackupDiscoveryJob.
pub async fn start_historical_backfill(app_state: &AppState) -> Result<(), BackupError> {
    // Check if there's already an incomplete batch
    let incomplete_batch = sqlx::query_scalar!(
        r#"
        SELECT id FROM backup_batches
        WHERE completed_at IS NULL
        LIMIT 1
        "#
    )
    .fetch_optional(&app_state.db)
    .await
    .wrap_err("Failed to check for incomplete batches")?;

    if let Some(batch_id) = incomplete_batch {
        tracing::info!(
            batch_id = batch_id,
            "Historical backfill already in progress, not starting another"
        );
        return Ok(());
    }

    tracing::info!("Starting historical backfill from the beginning");

    // Enqueue the first discovery job with no cursor (start from oldest)
    HistoricalBackupDiscoveryJob {
        after_created: None,
        after_id: None,
    }
    .enqueue(
        app_state.clone(),
        "historical backup discovery (initial)".to_string(),
        None,
    )
    .await
    .wrap_err("Failed to enqueue initial historical discovery job")?;

    Ok(())
}

/// Result of atomically incrementing batch completion count.
struct BatchCompletionResult {
    jobs_enqueued: i32,
    jobs_completed: i32,
    next_cursor_created: Option<chrono::NaiveDateTime>,
    next_cursor_id: Option<String>,
}

/// Atomically increment a batch's completed count and check if batch is done.
///
/// Uses SELECT FOR UPDATE to lock the row, preventing race conditions.
/// If this was the last job, enqueues the next discovery job.
async fn handle_batch_completion(app_state: &AppState, batch_id: i32) -> cja::Result<()> {
    // Use a transaction with row-level locking
    let mut tx = app_state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin transaction")?;

    // Atomically increment and get the result with row lock
    let result = sqlx::query_as!(
        BatchCompletionResult,
        r#"
        UPDATE backup_batches
        SET jobs_completed = jobs_completed + 1,
            completed_at = CASE
                WHEN jobs_completed + 1 = jobs_enqueued THEN NOW()
                ELSE completed_at
            END
        WHERE id = $1
        RETURNING
            jobs_enqueued,
            jobs_completed,
            next_cursor_created,
            next_cursor_id
        "#,
        batch_id
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err_with(|| format!("Failed to increment batch {} completion", batch_id))?;

    tx.commit()
        .await
        .wrap_err("Failed to commit batch completion")?;

    tracing::debug!(
        batch_id = batch_id,
        completed = result.jobs_completed,
        total = result.jobs_enqueued,
        "Batch job completed"
    );

    // If this was the last job, enqueue the next discovery
    if result.jobs_completed == result.jobs_enqueued {
        tracing::info!(
            batch_id = batch_id,
            "Batch complete, enqueuing next discovery"
        );

        HistoricalBackupDiscoveryJob {
            after_created: result.next_cursor_created,
            after_id: result.next_cursor_id,
        }
        .enqueue(
            app_state.clone(),
            "historical backup discovery".to_string(),
            None,
        )
        .await
        .wrap_err("Failed to enqueue next historical discovery job")?;
    }

    Ok(())
}

/// Fetch oldest completed games from Engine, using cursor for pagination.
async fn fetch_oldest_completed_games(
    engine_db: &PgPool,
    after_created: Option<chrono::NaiveDateTime>,
    after_id: Option<&str>,
    limit: i32,
) -> cja::Result<Vec<EngineGameRow>> {
    let rows: Vec<EngineGameRow> = match (after_created, after_id) {
        (Some(created), Some(id)) => {
            // Cursor-based pagination using row comparison
            sqlx::query_as(
                r#"
                SELECT id, value, created
                FROM games
                WHERE value->>'Status' IN ('complete', 'error')
                  AND (created, id) > ($1, $2)
                ORDER BY created ASC, id ASC
                LIMIT $3
                "#,
            )
            .bind(created)
            .bind(id)
            .bind(limit)
            .fetch_all(engine_db)
            .await
            .wrap_err("Failed to fetch oldest completed games with cursor")?
        }
        _ => {
            // Initial query, no cursor
            sqlx::query_as(
                r#"
                SELECT id, value, created
                FROM games
                WHERE value->>'Status' IN ('complete', 'error')
                ORDER BY created ASC, id ASC
                LIMIT $1
                "#,
            )
            .bind(limit)
            .fetch_all(engine_db)
            .await
            .wrap_err("Failed to fetch oldest completed games")?
        }
    };

    Ok(rows)
}

/// Batch check which game IDs are already archived in our database.
async fn get_archived_game_ids(
    db: &PgPool,
    engine_game_ids: &[String],
) -> cja::Result<Vec<String>> {
    let ids: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT engine_game_id as "engine_game_id!"
        FROM games
        WHERE engine_game_id = ANY($1) AND archived_at IS NOT NULL
        "#,
        engine_game_ids
    )
    .fetch_all(db)
    .await
    .wrap_err("Failed to check archived game IDs")?;

    Ok(ids)
}

/// Run historical backup discovery.
///
/// Fetches a batch of oldest games from Engine, filters out already-archived ones,
/// creates a batch record, and enqueues backup jobs.
pub async fn run_historical_backup_discovery(
    app_state: &AppState,
    after_created: Option<chrono::NaiveDateTime>,
    after_id: Option<&str>,
) -> Result<(), BackupError> {
    tracing::info!(
        after_created = ?after_created,
        after_id = ?after_id,
        "Starting historical backup discovery"
    );

    let engine_db = match &app_state.engine_db {
        Some(db) => db,
        None => {
            tracing::warn!("Engine database not configured, skipping historical discovery");
            return Ok(());
        }
    };

    // Fetch batch of oldest games
    let games =
        fetch_oldest_completed_games(engine_db, after_created, after_id, HISTORICAL_BATCH_SIZE)
            .await?;

    if games.is_empty() {
        tracing::info!("No more games to archive, historical backfill complete!");
        return Ok(());
    }

    tracing::info!(count = games.len(), "Found games for historical backfill");

    // Batch check which are already archived
    let game_ids: Vec<String> = games.iter().map(|g| g.id.clone()).collect();
    let archived_ids = get_archived_game_ids(&app_state.db, &game_ids).await?;
    let archived_set: std::collections::HashSet<&str> =
        archived_ids.iter().map(|s| s.as_str()).collect();

    // Filter to unarchived games
    let unarchived: Vec<&EngineGameRow> = games
        .iter()
        .filter(|g| !archived_set.contains(g.id.as_str()))
        .collect();

    if unarchived.is_empty() {
        tracing::info!(
            "All {} games in batch already archived, continuing to next batch",
            games.len()
        );
        // Enqueue next discovery immediately
        let last = games.last().unwrap();
        HistoricalBackupDiscoveryJob {
            after_created: Some(last.created),
            after_id: Some(last.id.clone()),
        }
        .enqueue(
            app_state.clone(),
            "historical backup discovery".to_string(),
            None,
        )
        .await
        .wrap_err("Failed to enqueue next historical discovery job")?;
        return Ok(());
    }

    // Compute cursor for next batch (from last game in the FULL batch, not just unarchived)
    let last_game = games.last().unwrap();
    let next_cursor_created = Some(last_game.created);
    let next_cursor_id = Some(last_game.id.clone());

    // Create batch record
    let batch_id = sqlx::query_scalar!(
        r#"
        INSERT INTO backup_batches (next_cursor_created, next_cursor_id, jobs_enqueued)
        VALUES ($1, $2, $3)
        RETURNING id
        "#,
        next_cursor_created,
        next_cursor_id,
        unarchived.len() as i32
    )
    .fetch_one(&app_state.db)
    .await
    .wrap_err("Failed to create batch record")?;

    tracing::info!(
        batch_id = batch_id,
        jobs = unarchived.len(),
        skipped = archived_set.len(),
        "Created backup batch"
    );

    // Enqueue backup jobs
    for game in &unarchived {
        enqueue_backup_job(app_state, &game.id, Some(batch_id)).await?;
    }

    tracing::info!(batch_id = batch_id, "Enqueued all backup jobs for batch");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The off-thread export step must produce exactly what the old inline
    /// path uploaded: zstd-compressed JSON of the game plus every frame, in
    /// order.
    #[test]
    fn compressed_export_round_trips_game_and_frames() {
        let game: EngineGame = serde_json::from_value(serde_json::json!({
            "ID": "g1",
            "Status": "complete",
            "Width": 11,
            "Height": 11,
            "Source": null,
            "RulesetName": "standard",
            "RulesStages": null,
            "Map": "standard",
        }))
        .unwrap();
        let frames = (0..3)
            .map(|turn| serde_json::json!({ "Turn": turn, "Snakes": [], "Food": [{"X": 1, "Y": 2}] }))
            .collect();

        let compressed = build_compressed_export(game, frames).unwrap();

        let json = zstd::decode_all(compressed.as_slice()).unwrap();
        let export: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(export["archive_version"], 2);
        assert_eq!(export["game_id"], "g1");
        assert_eq!(export["game"]["ID"], "g1");
        let turns: Vec<i64> = export["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["Turn"].as_i64().unwrap())
            .collect();
        assert_eq!(turns, vec![0, 1, 2]);
        assert_eq!(export["frames"][2]["Food"][0]["Y"], 2);
    }

    #[test]
    fn v2_export_is_self_describing_and_strips_snake_urls() {
        let game: EngineGame = serde_json::from_value(serde_json::json!({
            "ID": "g-v2",
            "Status": "complete",
            "Width": 11,
            "Height": 11,
            "Source": null,
            "RulesetName": "standard",
            "RulesStages": null,
            "Map": "standard",
        }))
        .unwrap();

        // Frame 0: MUST list every one of EngineSnake's 20 fields in canonical
        // serialized form — ID, Name, URL, Body, Health, Death, Color, HeadType,
        // TailType, Latency (string), Shout, Squad, APIVersion (string), Author,
        // StatusCode, Error, TimingMicros, IsBot, IsEnvironment, ProxyURL — plus
        // every EngineGameFrame field (Turn, Snakes, Food, Hazards, GameState,
        // PointState). Only a complete canonical fixture survives the typed
        // round-trip unchanged, so "input minus the three stripped keys == output"
        // is an exact Value equality below. The equality doubles as a canary: if a
        // field is later added to EngineSnake without updating this fixture, the
        // round-trip materializes it (null/default) and the assert fails loudly.
        // Note GameState values MUST be strings — the field is a
        // HashMap<String, String> (engine_models.rs) and non-string values fail
        // deserialization.
        let frame_full = serde_json::json!({
            "Turn": 0,
            "Snakes": [
                {
                    "ID": "s1",
                    "Name": "Snek",
                    "URL": "https://snake.example.com/move?token=sekrit",
                    "Body": [{"X": 5, "Y": 5}, {"X": 5, "Y": 4}],
                    "Health": 100,
                    "Death": {"Cause": "head-to-head", "Turn": 15, "EliminatedBy": "s2"},
                    "Color": "#ff0000",
                    "HeadType": "silly",
                    "TailType": "round",
                    "Latency": "12",
                    "Shout": "hello",
                    "Squad": "alpha",
                    "APIVersion": "1",
                    "Author": "coreyja",
                    "StatusCode": 200,
                    "Error": null,
                    "TimingMicros": {
                        "Connect": 1200, "DNS": 340, "FirstByte": 9800,
                        "Latency": 11340, "TLS": 2000
                    },
                    "IsBot": false,
                    "IsEnvironment": false,
                    "ProxyURL": "https://proxy.example.com/s1"
                }
            ],
            "Food": [{"X": 1, "Y": 2}],
            "Hazards": [{"X": 3, "Y": 4}],
            "GameState": {"phase": "start"},
            "PointState": [{"X": 3, "Y": 4, "Value": 1}]
        });
        // Frame 1: older-engine shape (int Latency/APIVersion). The typed model
        // normalizes ints to strings, so this frame is spot-checked only.
        let frame_older = serde_json::json!({
            "Turn": 1,
            "Snakes": [
                {
                    "ID": "s2",
                    "Name": "Older",
                    "URL": "https://snake2.example.com",
                    "ProxyURL": "https://proxy2.example.com",
                    "APIVersion": 1,
                    "Body": [{"X": 0, "Y": 0}],
                    "Health": 99,
                    "Latency": 15
                }
            ],
            "Food": []
        });

        let compressed =
            build_compressed_export(game, vec![frame_full.clone(), frame_older]).unwrap();

        let json = zstd::decode_all(compressed.as_slice()).unwrap();

        // Self-describing, and the identifying fields come FIRST: serde emits
        // struct fields in declaration order and serde_json::to_vec is compact,
        // so the decompressed bytes must open with exactly this prefix.
        assert!(
            json.starts_with(br#"{"archive_version":2,"game_id":"g-v2","#),
            "archive_version and game_id must be the first serialized fields"
        );

        let export: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(export["archive_version"], 2);
        assert_eq!(export["game_id"], "g-v2");
        assert_eq!(export["game"]["ID"], "g-v2");

        // Frame 0: everything except the three stripped fields is preserved.
        // Value equality is order-insensitive but shape- and value-exact, so
        // any field the typed round-trip dropped, renamed, or altered fails.
        let mut expected = frame_full;
        for snake in expected["Snakes"].as_array_mut().unwrap() {
            let obj = snake.as_object_mut().unwrap();
            obj.remove("URL");
            obj.remove("ProxyURL");
            obj.remove("APIVersion");
        }
        assert_eq!(export["frames"][0], expected);

        // Frame 1 (older-engine shape): the three keys are absent here too...
        let older = &export["frames"][1]["Snakes"][0];
        assert!(older.get("URL").is_none());
        assert!(older.get("ProxyURL").is_none());
        assert!(older.get("APIVersion").is_none());
        // ...and int inputs normalize to strings exactly like v1 did.
        assert_eq!(older["Latency"], "15");
    }

    /// SC: new objects are compressed with zstd level 9. Byte-equality with a
    /// re-encode of the same JSON at a LITERAL 9 pins the level exactly; the
    /// `assert_ne!` against a literal 3 keeps the check from passing vacuously
    /// on input where the levels agree (measured: they agree byte-for-byte on
    /// low-entropy fixtures) and doubles as the fixture-richness canary.
    #[test]
    fn export_is_compressed_at_zstd_level_9() {
        let game: EngineGame = serde_json::from_value(serde_json::json!({
            "ID": "g-level",
            "Status": "complete",
            "Width": 11,
            "Height": 11,
            "Source": null,
            "RulesetName": "standard",
            "RulesStages": null,
            "Map": "standard",
        }))
        .unwrap();

        // Varied multi-frame fixture (~60 turns x 4 snakes, ~110 KB JSON): all
        // values derive from (turn, snake, body-index) arithmetic so content is
        // deterministic but non-repetitive — required for levels 3 and 9 to
        // actually differ (12,340 B vs 10,140 B on this generator).
        let frames: Vec<serde_json::Value> = (0..60)
            .map(|turn| {
                let snakes: Vec<serde_json::Value> = (0..4)
                    .map(|s| {
                        serde_json::json!({
                            "ID": format!("snake-{s}"),
                            "Name": format!("Snake {s} variant {turn}"),
                            "URL": format!("https://snake{s}.example{turn}.io/move?token=t{s}{turn}"),
                            "Body": (0..(8 + ((turn * 3 + s) % 17)))
                                .map(|b| serde_json::json!({
                                    "X": (b * 7 + turn + s * 11) % 21,
                                    "Y": (b * 5 + turn * 3 + s) % 21,
                                }))
                                .collect::<Vec<_>>(),
                            "Health": 100 - ((turn + s * 13) % 40),
                            "Latency": 90 + (turn * 7 + s * 31) % 800,
                            "TimingMicros": {
                                "Connect": 800 + (turn * 11 + s * 17) % 2000,
                                "DNS": 100 + (turn * 5 + s * 3) % 400,
                                "FirstByte": 5000 + (turn * 13 + s * 7) % 9000,
                                "TLS": 1500 + (turn * 3 + s) % 900,
                            },
                        })
                    })
                    .collect();
                serde_json::json!({
                    "Turn": turn,
                    "Snakes": snakes,
                    "Food": (0..(turn % 5))
                        .map(|f| serde_json::json!({ "X": (f * 3 + turn) % 21, "Y": (f * 7 + turn * 2) % 21 }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();

        let compressed = build_compressed_export(game, frames).unwrap();
        let json = zstd::decode_all(compressed.as_slice()).unwrap();

        // zstd is deterministic and chunking-independent for identical input,
        // level, and library (verified: single write_all + finish ==
        // zstd::stream::encode_all == 8 KiB-chunked writes, byte-identical).
        let reencoded_9 = zstd::stream::encode_all(json.as_slice(), 9).unwrap();
        assert_eq!(compressed, reencoded_9, "archive must be zstd level 9");

        let reencoded_3 = zstd::stream::encode_all(json.as_slice(), 3).unwrap();
        assert_ne!(compressed, reencoded_3);
    }

    /// SC: `games.archive_version` is 2 for every game archived after deploy —
    /// on both the fresh-INSERT path and the ON CONFLICT (engine_game_id)
    /// re-archive path. Runtime queries (not the query! macro) so the .sqlx
    /// offline cache stays valid without `cargo sqlx prepare`.
    #[sqlx::test(migrations = "../migrations")]
    async fn upsert_game_record_sets_archive_version_2(pool: sqlx::PgPool) -> cja::Result<()> {
        fn fixture_game(id: &str) -> EngineGame {
            serde_json::from_value(serde_json::json!({
                "ID": id,
                "Status": "complete",
                "Width": 11,
                "Height": 11,
                "Source": null,
                "RulesetName": "standard",
                "RulesStages": null,
                "Map": "standard",
            }))
            .unwrap()
        }

        // Fresh insert: no pre-existing row, INSERT branch of the upsert.
        let fresh = fixture_game("g-upsert-fresh");
        upsert_game_record(&pool, &fresh, "games/2026/10/31/g-upsert-fresh.json.zst").await?;
        let version = sqlx::query_scalar::<_, i32>(
            "SELECT archive_version FROM games WHERE engine_game_id = $1",
        )
        .bind(&fresh.id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(version, 2);

        // ON CONFLICT branch: a v1-era row (archive_version = 1) is bumped to 2
        // when the game is re-archived.
        let conflicted = fixture_game("g-upsert-v1");
        sqlx::query(
            r#"
            INSERT INTO games (board_size, game_type, engine_game_id, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'g-upsert-v1', NOW(), 'games/2026/01/01/g-upsert-v1.json.zst', 1)
            "#,
        )
        .execute(&pool)
        .await?;
        upsert_game_record(&pool, &conflicted, "games/2026/10/31/g-upsert-v1.json.zst").await?;
        let version = sqlx::query_scalar::<_, i32>(
            "SELECT archive_version FROM games WHERE engine_game_id = $1",
        )
        .bind(&conflicted.id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(version, 2);

        Ok(())
    }

    #[test]
    fn malformed_frame_fails_the_export() {
        let game: EngineGame = serde_json::from_value(serde_json::json!({
            "ID": "g1", "Status": "complete", "Source": null,
            "RulesetName": null, "RulesStages": null, "Map": null,
        }))
        .unwrap();

        let result = build_compressed_export(game, vec![serde_json::json!({ "Turn": "nope" })]);

        assert!(result.is_err());
    }

    /// A claimed backup waits on BACKUP_SLOT while holding its worker, so a
    /// burst must sort below game runners or new games queue behind it.
    #[sqlx::test(migrations = "../migrations")]
    async fn backups_queue_below_default_priority_jobs(pool: sqlx::PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        enqueue_backup_job(&app_state, "engine-game-1", None).await?;
        enqueue_backup_job(&app_state, "engine-game-2", Some(7)).await?;
        crate::jobs::GameRunnerJob {
            game_id: uuid::Uuid::new_v4(),
        }
        .enqueue(app_state.clone(), "game".to_string(), None)
        .await?;
        let rows = sqlx::query!("SELECT name, priority FROM jobs ORDER BY created_at")
            .fetch_all(&pool)
            .await?;
        let priority_of = |name: &str| -> Vec<i32> {
            rows.iter()
                .filter(|r| r.name == name)
                .map(|r| r.priority)
                .collect()
        };
        assert_eq!(
            priority_of("BackupSingleGameJob"),
            vec![BACKUP_JOB_PRIORITY, BACKUP_JOB_PRIORITY],
            "both discovery and batch backfill backups use the backup priority"
        );
        assert!(
            priority_of("GameRunnerJob")
                .iter()
                .all(|&p| p > BACKUP_JOB_PRIORITY),
            "a game runner must out-rank queued backups"
        );
        Ok(())
    }

    /// Backups are serialized process-wide so an hourly burst can't
    /// monopolize the CPU live games run on.
    #[tokio::test]
    async fn backup_slot_admits_one_backup_at_a_time() {
        let held = BACKUP_SLOT.acquire().await.unwrap();
        assert!(BACKUP_SLOT.try_acquire().is_err());
        drop(held);
        assert!(BACKUP_SLOT.try_acquire().is_ok());
    }
}
