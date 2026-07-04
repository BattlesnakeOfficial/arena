//! Game backup module for archiving games from the Engine database to GCS.

use std::io::Write;

use chrono::{Duration, Utc};
use color_eyre::eyre::{Context as _, eyre};
use google_cloud_storage::{
    client::{Client as GcsClient, ClientConfig},
    http::objects::upload::{Media, UploadObjectRequest, UploadType},
};
use sqlx::{FromRow, PgPool};

use crate::engine_models::{EngineGame, EngineGameFrame, GameExport};
use crate::jobs::{BackupSingleGameJob, HistoricalBackupDiscoveryJob};
use crate::state::AppState;
use cja::jobs::Job;

/// Batch size for historical backfill discovery
const HISTORICAL_BATCH_SIZE: i32 = 500;

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

/// Fetch all frames for a game from the Engine database.
async fn fetch_game_frames(engine_db: &PgPool, game_id: &str) -> cja::Result<Vec<EngineGameFrame>> {
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

    let frames: Vec<EngineGameFrame> = rows
        .into_iter()
        .map(|(value,)| serde_json::from_value(value))
        .collect::<Result<Vec<_>, _>>()
        .wrap_err("Failed to deserialize game frames")?;

    Ok(frames)
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

/// Compress JSON with zstd and upload to GCS.
async fn compress_and_upload_to_gcs(
    client: &GcsClient,
    bucket: &str,
    path: &str,
    export: &GameExport,
) -> cja::Result<()> {
    // Serialize to JSON
    let json = serde_json::to_vec(export).wrap_err("Failed to serialize game export")?;

    // Compress with zstd (level 3 is a good balance of speed/compression)
    let mut encoder =
        zstd::Encoder::new(Vec::new(), 3).wrap_err("Failed to create zstd encoder")?;
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

    // Upload to GCS
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

/// Current archive format version. Increment when changing the export format.
const ARCHIVE_VERSION: i32 = 1;

/// Insert or update a game record in the local database after archiving.
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
        BackupSingleGameJob {
            engine_game_id: game_row.id.clone(),
            batch_id: None,
        }
        .enqueue(app_state.clone(), format!("backup game {}", game_row.id))
        .await
        .wrap_err_with(|| format!("Failed to enqueue backup job for game {}", game_row.id))?;

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

    // Fetch the game from Engine
    let game_row = fetch_game_by_id(engine_db, engine_game_id)
        .await?
        .ok_or_else(|| eyre!("Game {} not found in Engine database", engine_game_id))?;

    // Parse the game data
    let game: EngineGame = serde_json::from_value(game_row.value)
        .wrap_err_with(|| format!("Failed to parse game data for {}", engine_game_id))?;

    // Fetch frames
    let frames = fetch_game_frames(engine_db, &game.id).await?;

    // Build export
    let export = GameExport {
        game: game.clone(),
        frames,
        exported_at: Utc::now(),
    };

    // Initialize GCS client
    let config = ClientConfig::default()
        .with_auth()
        .await
        .wrap_err("Failed to configure GCS client")?;
    let gcs_client = GcsClient::new(config);

    // Generate path and upload
    let path = gcs_path(&game);
    compress_and_upload_to_gcs(&gcs_client, &bucket, &path, &export).await?;

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
        .enqueue(app_state.clone(), "historical backup discovery".to_string())
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
        .enqueue(app_state.clone(), "historical backup discovery".to_string())
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
        BackupSingleGameJob {
            engine_game_id: game.id.clone(),
            batch_id: Some(batch_id),
        }
        .enqueue(app_state.clone(), format!("backup game {}", game.id))
        .await
        .wrap_err_with(|| format!("Failed to enqueue backup job for game {}", game.id))?;
    }

    tracing::info!(batch_id = batch_id, "Enqueued all backup jobs for batch");

    Ok(())
}
