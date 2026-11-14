//! Game backup module for archiving games from the Engine database to GCS.
//!
//! The hourly discovery (`run_backup_discovery`) covers fresh games; the
//! historical backfill is the resumable walk in `play_history_walk.rs`
//! (DEV-1984), which enqueues `BackupSingleGameJob`s through
//! [`enqueue_backup_job`]. The old fork-join batch chain
//! (`backup_batches` + `HistoricalBackupDiscoveryJob`) is gone: its
//! completion arithmetic could never be satisfied once a job's count was
//! lost (batch 11 stalled 2026-01-27 with `jobs_enqueued = 501` against a
//! discovery `LIMIT 500`).

use std::io::Write;

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Context as _, eyre};
use sqlx::{FromRow, PgPool};

use crate::engine_models::{ARCHIVE_VERSION, EngineGame, EngineGameFrame};
use crate::jobs::BackupSingleGameJob;
use crate::state::AppState;
use cja::jobs::Job;

/// Backups run one at a time per process. Discovery enqueues a burst of
/// jobs every hour, and prod has a single vCPU shared with live games: a
/// burst of concurrent frame parsing + zstd stalled every running game for
/// ~300ms, long enough to time out snakes mid-turn.
///
/// Shared by Play (backup.rs) and native (native_backup.rs) archive jobs —
/// one backup at a time process-wide. Held as an `Arc` so native jobs can
/// take owned permits into `spawn_blocking` (a borrowed permit would be
/// freed early if a deploy drains the job future mid-compress).
pub(crate) static BACKUP_SLOT: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)));

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
pub(crate) const ZSTD_COMPRESSION_LEVEL: i32 = 9;

/// Enqueue one game's backup at [`BACKUP_JOB_PRIORITY`]. Used by the hourly
/// discovery and by the play-history walk (DEV-1984).
pub(crate) async fn enqueue_backup_job(
    app_state: &AppState,
    engine_game_id: &str,
) -> cja::Result<()> {
    BackupSingleGameJob {
        engine_game_id: engine_game_id.to_owned(),
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

/// Row from Engine's games table
#[derive(FromRow)]
pub(crate) struct EngineGameRow {
    pub(crate) id: String,
    pub(crate) value: serde_json::Value,
    /// Engine DB uses TIMESTAMP (no timezone), not TIMESTAMPTZ. sqlx pins
    /// pooled sessions' TimeZone to UTC, so the naive value decodes as UTC
    /// deterministically.
    pub(crate) created: chrono::NaiveDateTime,
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
pub(crate) async fn fetch_game_by_id(
    engine_db: &PgPool,
    game_id: &str,
) -> cja::Result<Option<EngineGameRow>> {
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

/// Frames per engine page during export build. Bounds peak memory: the 285
/// games over 1,000 frames (max 5,567) build in ≤28 pages instead of
/// materializing every frame at once on the 1 GiB prod box.
pub(crate) const EXPORT_FRAME_PAGE_SIZE: i64 = 200;

#[derive(FromRow)]
pub(crate) struct FrameTextRow {
    pub(crate) turn: i32,
    /// `game_frames.value` is nullable in the engine schema: NULL maps to
    /// None here and is classified Permanent ("frame_null_value"), never
    /// a transient fetch error.
    pub(crate) value_text: Option<String>,
}

/// One page of a game's frames as raw JSON text (keyset on turn). `::text`
/// makes Postgres render the JSONB server-side; the String decode is a
/// memcpy. No serde work on the async worker.
pub(crate) async fn fetch_game_frame_texts_page(
    engine_db: &PgPool,
    game_id: &str,
    after_turn: i32,
    limit: i64,
) -> cja::Result<Vec<FrameTextRow>> {
    let rows: Vec<FrameTextRow> = sqlx::query_as(
        r#"
        SELECT turn, value::text AS value_text
        FROM game_frames
        WHERE id = $1 AND turn > $2
        ORDER BY turn ASC
        LIMIT $3
        "#,
    )
    .bind(game_id)
    .bind(after_turn)
    .bind(limit)
    .fetch_all(engine_db)
    .await
    .wrap_err("Failed to fetch game frames page from Engine")?;

    Ok(rows)
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

/// Generate the GCS path for a game based on its creation date. The date is
/// the engine `created` COLUMN (DEV-1984): pre-2023 `value` rows often lack
/// a `Created` key, which used to date every such object 1970-01-01.
pub(crate) fn gcs_path(game_id: &str, created_at: DateTime<Utc>) -> String {
    format!(
        "games/{}/{:02}/{:02}/{}.json.zst",
        created_at.format("%Y"),
        created_at.format("%m"),
        created_at.format("%d"),
        game_id
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

/// Why [`ExportWriter::push_frame_texts`] failed. Only [`PushFrameError::Parse`]
/// is structural — the frame text can never parse, so it becomes a
/// permanent `play_archive_exceptions` row. [`PushFrameError::Internal`]
/// failures (frame re-serialization, zstd writes) are internal, never bad
/// data: classifying them permanent would record a false exception and
/// hide the bug (DEV-1984 req 2), so they stay transient and retry.
#[derive(Debug)]
pub(crate) enum PushFrameError {
    /// The frame text's JSON or types don't fit `EngineGameFrame` — can
    /// never succeed on retry.
    Parse(color_eyre::Report),
    /// Internal serialize/zstd failure — code-bug territory, never bad
    /// data; retry and surface loudly.
    Internal(color_eyre::Report),
}

/// Incremental builder for a v2 archive object: writes the exact byte
/// layout of `serde_json::to_vec(&GameExport{..})` — prefix
/// `{"archive_version":..,"game_id":..,"game":..,"frames":[`, frames
/// joined by `,`, suffix `],"exported_at":..}` — without ever holding
/// every frame. serde emits struct fields in declaration order and
/// serde_json::to_vec is compact, so incremental typed-struct writes are
/// byte-identical to the one-shot reference builder (pinned by a test).
/// The zstd encoder must never be flushed mid-stream: a flush terminates a
/// zstd block and changes the bytes.
pub(crate) struct ExportWriter {
    encoder: zstd::Encoder<'static, Vec<u8>>,
    frame_count: usize,
    json_len: usize,
}

impl ExportWriter {
    pub(crate) fn new(game: &EngineGame) -> cja::Result<Self> {
        let encoder = zstd::Encoder::new(Vec::new(), ZSTD_COMPRESSION_LEVEL)
            .wrap_err("Failed to create zstd encoder")?;
        let mut writer = Self {
            encoder,
            frame_count: 0,
            json_len: 0,
        };
        writer.write_json_fragment(b"{\"archive_version\":")?;
        writer.write_json_fragment(&serde_json::to_vec(&ARCHIVE_VERSION)?)?;
        writer.write_json_fragment(b",\"game_id\":")?;
        writer.write_json_fragment(&serde_json::to_vec(&game.id)?)?;
        writer.write_json_fragment(b",\"game\":")?;
        writer.write_json_fragment(&serde_json::to_vec(game)?)?;
        writer.write_json_fragment(b",\"frames\":[")?;
        Ok(writer)
    }

    fn write_json_fragment(&mut self, bytes: &[u8]) -> cja::Result<()> {
        self.encoder
            .write_all(bytes)
            .wrap_err("Failed to write to zstd encoder")?;
        self.json_len += bytes.len();
        Ok(())
    }

    pub(crate) fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// Parse each text into [`EngineGameFrame`], strip secret snake fields,
    /// serialize, and write the frames into the encoder, joined by `,`.
    /// Pure CPU — blocking pool only. Parse failures are structural
    /// ([`PushFrameError::Parse`]); serialize/zstd failures are internal
    /// ([`PushFrameError::Internal`]) and must never become permanent
    /// exceptions.
    pub(crate) fn push_frame_texts(&mut self, page: &[FrameTextRow]) -> Result<(), PushFrameError> {
        for row in page {
            let text = row.value_text.as_deref().unwrap_or_default();
            let mut frame: EngineGameFrame = serde_json::from_str(text)
                .wrap_err("Failed to deserialize game frame")
                .map_err(PushFrameError::Parse)?;
            strip_secret_snake_fields(std::slice::from_mut(&mut frame));
            let frame_bytes = serde_json::to_vec(&frame)
                .wrap_err("Failed to serialize game frame")
                .map_err(PushFrameError::Internal)?;
            if self.frame_count > 0 {
                self.write_json_fragment(b",")
                    .map_err(PushFrameError::Internal)?;
            }
            self.write_json_fragment(&frame_bytes)
                .map_err(PushFrameError::Internal)?;
            self.frame_count += 1;
        }
        Ok(())
    }

    /// Close the frames array and the export, finish the zstd stream.
    /// `exported_at` is a parameter so the byte-equality test can pin both
    /// builders to one timestamp.
    pub(crate) fn finish(mut self, exported_at: DateTime<Utc>) -> cja::Result<Vec<u8>> {
        self.write_json_fragment(b"],\"exported_at\":")?;
        self.write_json_fragment(&serde_json::to_vec(&exported_at)?)?;
        self.write_json_fragment(b"}")?;
        let compressed = self
            .encoder
            .finish()
            .wrap_err("Failed to finish zstd compression")?;
        tracing::debug!(
            frames = self.frame_count,
            json_size = self.json_len,
            compressed_size = compressed.len(),
            ratio = format!(
                "{:.1}%",
                (compressed.len() as f64 / self.json_len as f64) * 100.0
            ),
            "Compressed game for upload"
        );
        Ok(compressed)
    }
}

/// Why an export build failed. Structural (Permanent) failures can never
/// succeed on retry and are recorded in `play_archive_exceptions`;
/// everything else retries through cja's normal backoff.
pub(crate) enum ExportBuildError {
    /// Cannot succeed on retry: recorded as a play_archive_exceptions row.
    Permanent(String),
    /// Retry via normal job backoff (DB/engine errors, blocking-task join
    /// failure).
    Transient(color_eyre::Report),
}

/// Build a game's v2 export object by streaming frames from the engine DB
/// in [`EXPORT_FRAME_PAGE_SIZE`] pages through the incremental
/// [`ExportWriter`] — peak memory is ~one page regardless of game length
/// (the max 5,567-frame game is 28 pages; the old whole-game fetch was the
/// OOM-correlated shape on the 1 GiB prod box). Parse/serialize/compress
/// runs on the blocking pool under the backup slot; only the slot holder
/// reaches this function.
pub(crate) async fn build_export_streamed(
    engine_db: &PgPool,
    game: EngineGame,
) -> Result<Vec<u8>, ExportBuildError> {
    // The game header just parsed, so only an internal serialize/zstd
    // failure lands here — never bad data. That is transient by
    // classification (retry cannot fix it, but it also never happens
    // without a code bug; an exception row would hide the bug).
    let mut writer = ExportWriter::new(&game).map_err(ExportBuildError::Transient)?;
    let mut after_turn = i32::MIN;
    loop {
        let page =
            fetch_game_frame_texts_page(engine_db, &game.id, after_turn, EXPORT_FRAME_PAGE_SIZE)
                .await
                .map_err(ExportBuildError::Transient)?;
        if page.is_empty() {
            break;
        }
        if let Some(last) = page.last() {
            after_turn = last.turn;
        }
        if page.iter().any(|row| row.value_text.is_none()) {
            return Err(ExportBuildError::Permanent("frame_null_value".to_string()));
        }
        let (returned_writer, result) = tokio::task::spawn_blocking(move || {
            let mut writer = writer;
            let result = writer.push_frame_texts(&page);
            (writer, result)
        })
        .await
        .wrap_err("Export build task panicked")
        .map_err(ExportBuildError::Transient)?;
        writer = returned_writer;
        if let Err(error) = result {
            return Err(match error {
                PushFrameError::Parse(error) => {
                    ExportBuildError::Permanent(format!("frame_unparseable: {error:#}"))
                }
                // Internal serialize/zstd failure: never bad data, so never
                // an exception — retry (and the error surfaces loudly).
                PushFrameError::Internal(error) => ExportBuildError::Transient(error),
            });
        }
    }
    if writer.frame_count() == 0 {
        return Err(ExportBuildError::Permanent("no_frames".to_string()));
    }
    let bytes = tokio::task::spawn_blocking(move || writer.finish(Utc::now()))
        .await
        .wrap_err("Export finish task panicked")
        .map_err(ExportBuildError::Transient)?
        .map_err(ExportBuildError::Transient)?;
    Ok(bytes)
}

/// Record a game as permanently unarchivable, with the reason. The walk
/// treats an exceptions row as done. Re-arming a game after a fix takes
/// BOTH deleting its row AND getting the walk to see the game again —
/// rewind `play_history_walk_cursor` behind it (a rewind to the seed
/// re-walks ~270k done rows at 2,000/tick, ~11 h of cheap no-op pages) or
/// enqueue a `BackupSingleGameJob` for it directly. Once the cursor has
/// passed a game no tick revisits it, so deleting the row alone does
/// nothing — and erases the only record of why the game is missing
/// (see docs/game-archive.md).
async fn record_permanent_failure(
    db: &PgPool,
    engine_game_id: &str,
    reason: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO play_archive_exceptions (engine_game_id, reason)
        VALUES ($1, $2)
        ON CONFLICT (engine_game_id) DO UPDATE SET reason = $2, recorded_at = NOW()
        "#,
        engine_game_id,
        reason
    )
    .execute(db)
    .await
    .wrap_err("Failed to record archive exception")?;
    tracing::warn!(
        engine_game_id,
        reason,
        "Recorded permanent archive exception"
    );
    Ok(())
}

/// Parse frames, build the export, and zstd-compress its JSON. Pure CPU
/// work: callers run it on the blocking pool, never on an async worker.
/// Test-only reference builder since DEV-1984 — production streams through
/// [`ExportWriter`], which must stay byte-identical to this output.
#[cfg(test)]
pub(crate) fn build_compressed_export(
    game: EngineGame,
    frame_values: Vec<serde_json::Value>,
    exported_at: DateTime<Utc>,
) -> cja::Result<Vec<u8>> {
    let mut frames: Vec<EngineGameFrame> = frame_values
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<Vec<_>, _>>()
        .wrap_err("Failed to deserialize game frames")?;

    strip_secret_snake_fields(&mut frames);

    let export = crate::engine_models::GameExport {
        archive_version: ARCHIVE_VERSION,
        game_id: game.id.clone(),
        game,
        frames,
        exported_at,
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

/// Insert or update a game record in the local database after archiving.
/// `created_at` is the engine `created` column (DEV-1984). Only freshly
/// archived games go through here; the walk's 1970-row corrections use
/// their own UPDATE (see `play_history_walk.rs`) because this upsert
/// rewrites `gcs_path`/`archive_version`/`archived_at` and must not touch
/// rows whose objects stay as-is.
///
/// Imported games get no `game_battlesnakes` rows. `models::snake_latency`
/// relies on that to skip an `engine_game_id IS NULL` filter that wrecks its
/// query plan; linking snakes to imported games would need that filter back.
async fn upsert_game_record(
    db: &PgPool,
    game: &EngineGame,
    created_at: DateTime<Utc>,
    gcs_path: &str,
) -> cja::Result<()> {
    let now = Utc::now();
    let board_size = game.board_size();
    let game_type = game.game_type();

    sqlx::query!(
        r#"
        INSERT INTO games (engine_game_id, board_size, game_type, status, created_at, archived_at, gcs_path, archive_version)
        VALUES ($1, $2, $3, 'finished', $4, $5, $6, $7)
        ON CONFLICT (engine_game_id) DO UPDATE SET
            game_type = $3,
            created_at = $4,
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

        enqueue_backup_job(app_state, &game_row.id).await?;

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
/// Called by `BackupSingleGameJob`, shared by the hourly discovery and the
/// play-history walk (DEV-1984). Slot-first (like native backup): only the
/// slot holder fetches per-game data, the already-archived re-check happens
/// UNDER the slot (a duplicate job no-ops instead of double-putting), and a
/// game that can never be archived is recorded in `play_archive_exceptions`
/// and returns `Ok` so the walk moves on. Transient failures return `Err`
/// and retry through cja's backoff.
pub async fn backup_single_game(
    app_state: &AppState,
    engine_game_id: &str,
) -> Result<(), BackupError> {
    let engine_db = match &app_state.engine_db {
        Some(db) => db,
        None => return Err(eyre!("Engine database not configured").into()),
    };

    // SLOT FIRST — only the holder fetches per-game data, and the archived
    // re-check below runs under the slot so a duplicate enqueued behind
    // this job sees its completed mark. The permit lives to the end of the
    // function (slot held through upload + record, like native backup).
    let _permit = std::sync::Arc::clone(&BACKUP_SLOT)
        .acquire_owned()
        .await
        .wrap_err("Backup slot semaphore closed")?;

    // Req 6 (DEV-1984): re-check archived state under the slot — no put
    // for an already-archived game, ever.
    if is_already_archived(&app_state.db, engine_game_id).await? {
        tracing::debug!(game_id = %engine_game_id, "Game already archived, skipping");
        return Ok(());
    }

    if !app_state.object_store.is_configured() {
        return Err(eyre!("GCS bucket not configured").into());
    }

    // Fetch the game from Engine
    let game_row = match fetch_game_by_id(engine_db, engine_game_id).await? {
        Some(row) => row,
        None => {
            // The engine row is gone (7-day `expiry` on recent games): the
            // game can never be archived. Explain it and move on.
            record_permanent_failure(&app_state.db, engine_game_id, "engine_row_missing").await?;
            return Ok(());
        }
    };

    // Parse the game data. A row that can't parse can never parse.
    let mut game: EngineGame = match serde_json::from_value(game_row.value) {
        Ok(game) => game,
        Err(error) => {
            let reason = format!("game_header_unparseable: {error}");
            record_permanent_failure(&app_state.db, engine_game_id, &reason).await?;
            return Ok(());
        }
    };

    // Dates come from the engine `created` column (DEV-1984): the JSON
    // `Created` key is absent on pre-2023 rows and used to date everything
    // 1970-01-01.
    let created_at = game_row.created.and_utc();
    if game.created == 0 {
        game.created = created_at.timestamp_micros();
    }

    let compressed = match build_export_streamed(engine_db, game.clone()).await {
        Ok(bytes) => bytes,
        Err(ExportBuildError::Permanent(reason)) => {
            record_permanent_failure(&app_state.db, engine_game_id, &reason).await?;
            return Ok(());
        }
        Err(ExportBuildError::Transient(error)) => return Err(error.into()),
    };

    // Generate path and upload
    let path = gcs_path(engine_game_id, created_at);
    app_state
        .object_store
        .put(&path, compressed)
        .await
        .wrap_err_with(|| format!("Failed to upload archive for {engine_game_id}"))?;

    // Record in local database
    upsert_game_record(&app_state.db, &game, created_at, &path).await?;

    tracing::info!(game_id = %engine_game_id, path = %path, "Archived engine game");

    Ok(())
}

/// Shared engine-DB fixture for backup and play-history-walk tests
/// (DEV-1984): an `engine` schema with the minimal `games`/`game_frames`
/// tables, an AppState whose `engine_db` is bound to that schema, and a
/// separately-held concrete `InMemoryObjectStore` so tests can read
/// `put_count()`.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::state::AppState;

    pub(crate) async fn engine_fixture(
        pool: &sqlx::PgPool,
    ) -> cja::Result<(
        AppState,
        std::sync::Arc<crate::object_store::InMemoryObjectStore>,
        sqlx::PgPool,
    )> {
        sqlx::raw_sql(
            r#"
            CREATE SCHEMA engine;
            CREATE TABLE engine.games(id TEXT PRIMARY KEY, value JSONB, created TIMESTAMP);
            CREATE TABLE engine.game_frames(id TEXT, turn INT, value JSONB);
            "#,
        )
        .execute(pool)
        .await?;

        let options = pool
            .connect_options()
            .as_ref()
            .clone()
            .options([("search_path", "engine")]);
        let engine_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;

        let store = std::sync::Arc::new(crate::object_store::InMemoryObjectStore::default());
        let mut state = AppState::test_from_pool(pool.clone());
        state.object_store = store.clone();
        state.archive = crate::archive::ArchiveReader::new(
            store.clone(),
            crate::archive::DEFAULT_ARCHIVE_CACHE_MAX_BYTES,
        );
        state.engine_db = Some(engine_pool.clone());
        Ok((state, store, engine_pool))
    }

    /// Insert an engine `games` row. `value` is bound as JSONB.
    pub(crate) async fn insert_engine_game(
        engine_pool: &sqlx::PgPool,
        id: &str,
        value: serde_json::Value,
        created: chrono::NaiveDateTime,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO games (id, value, created) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(value)
            .bind(created)
            .execute(engine_pool)
            .await?;
        Ok(())
    }

    /// Insert an engine `game_frames` row; `None` writes SQL NULL.
    pub(crate) async fn insert_engine_frame(
        engine_pool: &sqlx::PgPool,
        id: &str,
        turn: i32,
        value: Option<serde_json::Value>,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO game_frames (id, turn, value) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(turn)
            .bind(value)
            .execute(engine_pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::test_support::{engine_fixture, insert_engine_frame, insert_engine_game};
    use crate::object_store::ObjectStore as _;
    use chrono::SubsecRound as _;

    fn fixture_game_json(id: &str, created_micros: i64) -> serde_json::Value {
        serde_json::json!({
            "ID": id,
            "Status": "complete",
            "Width": 11,
            "Height": 11,
            "Ruleset": {},
            "SnakeTimeout": 600,
            "MaxTurns": 2500,
            "FoodSpawns": [],
            "HazardSpawns": [],
            "Source": null,
            "RulesetName": "standard",
            "RulesStages": null,
            "Map": "standard",
            "Created": created_micros,
        })
    }

    fn fixture_frame(turn: i32) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [],
            "Food": [{"X": 1, "Y": 2}],
        })
    }

    const FIXED_EXPORTED_AT: &str = "2026-11-05T00:00:00Z";

    fn fixed_exported_at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(FIXED_EXPORTED_AT)
            .expect("fixed timestamp parses")
            .with_timezone(&Utc)
    }

    /// The off-thread export step must produce exactly what the old inline
    /// path uploaded: zstd-compressed JSON of the game plus every frame, in
    /// order.
    #[test]
    fn compressed_export_round_trips_game_and_frames() {
        let game: EngineGame = serde_json::from_value(fixture_game_json("g1", 0)).unwrap();
        let frames: Vec<_> = (0..3).map(fixture_frame).collect();

        let compressed = build_compressed_export(game, frames, fixed_exported_at()).unwrap();

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
        assert_eq!(export["exported_at"], FIXED_EXPORTED_AT);
    }

    #[test]
    fn v2_export_is_self_describing_and_strips_snake_urls() {
        let game: EngineGame = serde_json::from_value(fixture_game_json("g-v2", 0)).unwrap();

        // Frame 0: MUST list every one of EngineSnake's 21 fields in canonical
        // serialized form — ID, Name, URL, Body, Health, Death, Color, HeadType,
        // TailType, Latency (string), Shout, Squad, Team, APIVersion (string),
        // Author, StatusCode, Error, TimingMicros, IsBot, IsEnvironment,
        // ProxyURL — plus every EngineGameFrame field (Turn, Snakes, Food,
        // Hazards, GameState, PointState). Only a complete canonical fixture
        // survives the typed round-trip unchanged, so "input minus the three
        // stripped keys == output" is an exact Value equality below. The
        // equality doubles as a canary: if a field is later added to
        // EngineSnake without updating this fixture, the round-trip
        // materializes it (null/default) and the assert fails loudly.
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
                    "Team": "Orb Weavers",
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

        let compressed = build_compressed_export(
            game,
            vec![frame_full.clone(), frame_older],
            fixed_exported_at(),
        )
        .unwrap();

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
        let game: EngineGame = serde_json::from_value(fixture_game_json("g-level", 0)).unwrap();

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

        let compressed = build_compressed_export(game, frames, fixed_exported_at()).unwrap();
        let json = zstd::decode_all(compressed.as_slice()).unwrap();

        // zstd is deterministic and chunking-independent for identical input,
        // level, and library (verified: single write_all + finish ==
        // zstd::stream::encode_all == 8 KiB-chunked writes, byte-identical).
        let reencoded_9 = zstd::stream::encode_all(json.as_slice(), 9).unwrap();
        assert_eq!(compressed, reencoded_9, "archive must be zstd level 9");

        let reencoded_3 = zstd::stream::encode_all(json.as_slice(), 3).unwrap();
        assert_ne!(compressed, reencoded_3);
    }

    /// DEV-1984: the streamed production writer must be byte-identical to
    /// the one-shot reference builder — same game, frames, and timestamp —
    /// both as a single page and one-frame-per-page (page boundaries are
    /// where an accidental mid-stream flush or join-comma bug would
    /// appear).
    #[test]
    fn streamed_writer_matches_reference_builder() {
        let game: EngineGame = serde_json::from_value(fixture_game_json("g-stream", 0)).unwrap();
        let frame_values: Vec<serde_json::Value> = (0..5).map(fixture_frame).collect();
        let exported_at = fixed_exported_at();

        let reference =
            build_compressed_export(game.clone(), frame_values.clone(), exported_at).unwrap();

        let frame_rows: Vec<FrameTextRow> = frame_values
            .iter()
            .enumerate()
            .map(|(idx, value)| FrameTextRow {
                turn: idx as i32,
                value_text: Some(value.to_string()),
            })
            .collect();

        // Single page.
        let mut writer = ExportWriter::new(&game).unwrap();
        writer.push_frame_texts(&frame_rows).unwrap();
        assert_eq!(writer.frame_count(), frame_rows.len());
        let streamed_single_page = writer.finish(exported_at).unwrap();
        assert_eq!(
            streamed_single_page, reference,
            "streamed single-page build must equal the reference builder byte-for-byte"
        );

        // One frame per page.
        let mut writer = ExportWriter::new(&game).unwrap();
        for row in &frame_rows {
            writer.push_frame_texts(std::slice::from_ref(row)).unwrap();
        }
        let streamed_per_page = writer.finish(exported_at).unwrap();
        assert_eq!(
            streamed_per_page, reference,
            "streamed multi-page build must equal the reference builder byte-for-byte"
        );

        // Both decompress to the same JSON as the reference.
        let json_reference = zstd::decode_all(reference.as_slice()).unwrap();
        let json_streamed = zstd::decode_all(streamed_single_page.as_slice()).unwrap();
        assert_eq!(json_streamed, json_reference);
    }

    /// SC: `games.archive_version` is 2 for every game archived after deploy —
    /// on both the fresh-INSERT path and the ON CONFLICT (engine_game_id)
    /// re-archive path. Runtime queries (not the query! macro) so the .sqlx
    /// offline cache stays valid without `cargo sqlx prepare`.
    #[sqlx::test(migrations = "../migrations")]
    async fn upsert_game_record_sets_archive_version_2(pool: sqlx::PgPool) -> cja::Result<()> {
        fn fixture_game(id: &str) -> EngineGame {
            serde_json::from_value(fixture_game_json(id, 0)).unwrap()
        }

        // Fresh insert: no pre-existing row, INSERT branch of the upsert.
        let fresh = fixture_game("g-upsert-fresh");
        let created_at = fixed_exported_at();
        upsert_game_record(
            &pool,
            &fresh,
            created_at,
            "games/2026/10/31/g-upsert-fresh.json.zst",
        )
        .await?;
        let row = sqlx::query_as::<_, (i32, chrono::DateTime<chrono::Utc>)>(
            "SELECT archive_version, created_at FROM games WHERE engine_game_id = $1",
        )
        .bind(&fresh.id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.0, 2);
        assert_eq!(row.1, created_at, "created_at comes from the engine column");

        // ON CONFLICT branch: a v1-era row (archive_version = 1) is bumped to 2
        // when the game is re-archived.
        let conflicted = fixture_game("g-upsert-v1");
        sqlx::query(
            r#"
            INSERT INTO games (board_size, game_type, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'g-upsert-v1', '2026-01-01T00:00:00Z', NOW(), 'games/2026/01/01/g-upsert-v1.json.zst', 1)
            "#,
        )
        .execute(&pool)
        .await?;
        upsert_game_record(
            &pool,
            &conflicted,
            created_at,
            "games/2026/10/31/g-upsert-v1.json.zst",
        )
        .await?;
        let row = sqlx::query_as::<_, (i32, chrono::DateTime<chrono::Utc>)>(
            "SELECT archive_version, created_at FROM games WHERE engine_game_id = $1",
        )
        .bind(&conflicted.id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.0, 2);
        assert_eq!(
            row.1, created_at,
            "ON CONFLICT rewrites created_at from the column"
        );

        Ok(())
    }

    #[test]
    fn malformed_frame_fails_the_export() {
        let game: EngineGame = serde_json::from_value(fixture_game_json("g1", 0)).unwrap();

        let result = build_compressed_export(
            game,
            vec![serde_json::json!({ "Turn": "nope" })],
            fixed_exported_at(),
        );

        assert!(result.is_err());
    }

    /// DEV-1984 req 6: a game already archived in arena causes NO put and
    /// its stored bytes stay unchanged. Control: an unarchived twin runs
    /// the full path and puts exactly one object.
    #[sqlx::test(migrations = "../migrations")]
    async fn archived_game_is_not_rewritten(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 3, 10)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        let archived_at = (Utc::now() - Duration::hours(3)).trunc_subsecs(6);

        for id in ["A-archived", "B-fresh"] {
            insert_engine_game(
                &engine_pool,
                id,
                fixture_game_json(id, created.and_utc().timestamp_micros()),
                created,
            )
            .await?;
            insert_engine_frame(&engine_pool, id, 0, Some(fixture_frame(0))).await?;
            insert_engine_frame(&engine_pool, id, 1, Some(fixture_frame(1))).await?;
        }

        let a_path = "games/1970/01/01/A-archived.json.zst";
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', 'A-archived', '1970-01-01T00:00:00Z', $1, $2, 1)
            "#,
            archived_at,
            a_path
        )
        .execute(&pool)
        .await?;
        store.put(a_path, vec![1, 2, 3]).await?;
        let bytes_before = store.get(a_path).await?.expect("pre-put bytes exist");
        let puts_before = store.put_count();

        backup_single_game(&state, "A-archived").await?;

        assert_eq!(
            store.put_count() - puts_before,
            0,
            "archived game must not be re-put"
        );
        assert_eq!(
            store.get(a_path).await?.as_deref(),
            Some(bytes_before.as_slice()),
            "stored bytes must be unchanged"
        );
        let a_row =
            sqlx::query!("SELECT archived_at FROM games WHERE engine_game_id = 'A-archived'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(a_row.archived_at, Some(archived_at));

        // Control: the unarchived twin takes the full path.
        backup_single_game(&state, "B-fresh").await?;
        assert_eq!(store.put_count() - puts_before, 1);
        let b_row = sqlx::query!(
            r#"SELECT gcs_path, archive_version FROM games WHERE engine_game_id = 'B-fresh'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            b_row.gcs_path.as_deref(),
            Some("games/2020/03/10/B-fresh.json.zst")
        );
        assert_eq!(b_row.archive_version, Some(2));
        Ok(())
    }

    /// DEV-1984 req 2: an unparseable engine header is recorded as an
    /// exception and the job returns Ok. Control: a well-formed twin
    /// archives normally in the same test.
    #[sqlx::test(migrations = "../migrations")]
    async fn bad_game_recorded_as_exception_and_passed(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2021, 5, 5)
            .expect("date")
            .and_hms_opt(12, 34, 56)
            .expect("time");

        let mut bad = fixture_game_json("bad-header", 0);
        bad["Width"] = serde_json::json!("not-a-number");
        insert_engine_game(&engine_pool, "bad-header", bad, created).await?;
        insert_engine_frame(&engine_pool, "bad-header", 0, Some(fixture_frame(0))).await?;

        insert_engine_game(
            &engine_pool,
            "good-twin",
            fixture_game_json("good-twin", created.and_utc().timestamp_micros()),
            created,
        )
        .await?;
        insert_engine_frame(&engine_pool, "good-twin", 0, Some(fixture_frame(0))).await?;

        backup_single_game(&state, "bad-header")
            .await
            .expect("a permanently bad game returns Ok so the walk moves on");

        let exception = sqlx::query!(
            "SELECT reason FROM play_archive_exceptions WHERE engine_game_id = 'bad-header'"
        )
        .fetch_one(&pool)
        .await?;
        assert!(exception.reason.starts_with("game_header_unparseable"));
        let bad_games_row = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM games WHERE engine_game_id = 'bad-header'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(bad_games_row, 0, "no arena row for a bad game");

        // Control.
        backup_single_game(&state, "good-twin").await?;
        let good = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM games WHERE engine_game_id = 'good-twin' AND archived_at IS NOT NULL"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(good, 1);
        Ok(())
    }

    /// DEV-1984: a NULL frame value is structural (Permanent), not a
    /// transient fetch error. Control: with the NULL row and the exception
    /// removed, the same game archives normally.
    #[sqlx::test(migrations = "../migrations")]
    async fn null_frame_value_is_permanent_not_transient(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 6, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");

        insert_engine_game(
            &engine_pool,
            "null-frame",
            fixture_game_json("null-frame", created.and_utc().timestamp_micros()),
            created,
        )
        .await?;
        insert_engine_frame(&engine_pool, "null-frame", 0, Some(fixture_frame(0))).await?;
        insert_engine_frame(&engine_pool, "null-frame", 1, None).await?;

        backup_single_game(&state, "null-frame")
            .await
            .expect("NULL frame value is permanent, not a retryable error");

        let exception = sqlx::query!(
            "SELECT reason FROM play_archive_exceptions WHERE engine_game_id = 'null-frame'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(exception.reason, "frame_null_value");
        assert_eq!(store.put_count(), 0, "no object for a permanent failure");

        // Control: delete the NULL frame and the exception row; a direct
        // re-run archives the game. (A direct call bypasses the cursor;
        // re-arming the WALK itself also needs the cursor rewound behind
        // the game — see docs/game-archive.md.)
        sqlx::query("DELETE FROM game_frames WHERE id = 'null-frame' AND turn = 1")
            .execute(&engine_pool)
            .await?;
        sqlx::query!("DELETE FROM play_archive_exceptions WHERE engine_game_id = 'null-frame'")
            .execute(&pool)
            .await?;
        backup_single_game(&state, "null-frame").await?;
        assert_eq!(store.put_count(), 1);
        Ok(())
    }

    /// DEV-1984 req 2: a parseable-JSON frame whose types don't fit
    /// `EngineGameFrame` is structural (Permanent) — retrying can never fix
    /// it. This drives the real streamed path (`build_export_streamed` /
    /// `backup_single_game`), unlike `malformed_frame_fails_the_export`,
    /// which only pins the test-only reference builder. Control: with the
    /// bad frame replaced and the exception cleared, the game archives
    /// normally.
    #[sqlx::test(migrations = "../migrations")]
    async fn unparseable_frame_is_permanent_not_transient(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 6, 15)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");

        insert_engine_game(
            &engine_pool,
            "bad-frame",
            fixture_game_json("bad-frame", created.and_utc().timestamp_micros()),
            created,
        )
        .await?;
        insert_engine_frame(&engine_pool, "bad-frame", 0, Some(fixture_frame(0))).await?;
        // Parseable JSON, wrong types ("Turn" must be an i32): serde fails
        // inside `push_frame_texts` — the streamed path's frame exception.
        insert_engine_frame(
            &engine_pool,
            "bad-frame",
            1,
            Some(serde_json::json!({"Turn": "nope", "Snakes": [], "Food": []})),
        )
        .await?;

        backup_single_game(&state, "bad-frame")
            .await
            .expect("an unparseable frame is permanent, not a retryable error");

        let exception = sqlx::query!(
            "SELECT reason FROM play_archive_exceptions WHERE engine_game_id = 'bad-frame'"
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            exception.reason.starts_with("frame_unparseable"),
            "reason was: {}",
            exception.reason
        );
        assert_eq!(store.put_count(), 0, "no object for a permanent failure");

        // Control: replace the bad frame, clear the exception, re-run.
        sqlx::query("DELETE FROM game_frames WHERE id = 'bad-frame' AND turn = 1")
            .execute(&engine_pool)
            .await?;
        insert_engine_frame(&engine_pool, "bad-frame", 1, Some(fixture_frame(1))).await?;
        sqlx::query!("DELETE FROM play_archive_exceptions WHERE engine_game_id = 'bad-frame'")
            .execute(&pool)
            .await?;
        backup_single_game(&state, "bad-frame").await?;
        assert_eq!(store.put_count(), 1);
        Ok(())
    }

    /// DEV-1984 req 2: a missing engine row (7-day `expiry` on recent
    /// games) is permanent — the game can never be archived. The job
    /// returns Ok so the walk moves on; nothing is uploaded and no arena
    /// row is created.
    #[sqlx::test(migrations = "../migrations")]
    async fn missing_engine_row_is_permanent(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, _engine_pool) = engine_fixture(&pool).await?;

        backup_single_game(&state, "never-existed")
            .await
            .expect("a missing engine row is permanent, not a retryable error");

        let exception = sqlx::query!(
            "SELECT reason FROM play_archive_exceptions WHERE engine_game_id = 'never-existed'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(exception.reason, "engine_row_missing");
        assert_eq!(store.put_count(), 0);
        let games_row = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM games WHERE engine_game_id = 'never-existed'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(games_row, 0, "no arena row for a missing engine game");
        Ok(())
    }

    /// DEV-1984 req 3: with no `Created` JSON key, `created_at`, the GCS
    /// date path, and the archived header `Created` all come from the
    /// engine `created` column. Control: a row WITH `Created` set to a
    /// different nonzero value still gets path+row from the column.
    #[sqlx::test(migrations = "../migrations")]
    async fn header_without_created_uses_engine_column_for_path_row_and_header(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2021, 5, 5)
            .expect("date")
            .and_hms_opt(12, 34, 56)
            .expect("time");
        let created_utc = created.and_utc();

        // No Created key at all (pre-2023 shape).
        let mut no_created = fixture_game_json("no-created", 0);
        no_created
            .as_object_mut()
            .expect("game json is an object")
            .remove("Created");
        insert_engine_game(&engine_pool, "no-created", no_created, created).await?;
        insert_engine_frame(&engine_pool, "no-created", 0, Some(fixture_frame(0))).await?;

        backup_single_game(&state, "no-created").await?;

        let row = sqlx::query!(
            r#"SELECT created_at, gcs_path, archive_version FROM games WHERE engine_game_id = 'no-created'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.created_at, created_utc);
        assert_eq!(
            row.gcs_path.as_deref(),
            Some("games/2021/05/05/no-created.json.zst")
        );
        assert_eq!(row.archive_version, Some(2));

        let bytes = store
            .get("games/2021/05/05/no-created.json.zst")
            .await?
            .expect("object exists");
        let json = zstd::decode_all(bytes.as_slice())?;
        let export: serde_json::Value = serde_json::from_slice(&json)?;
        assert_eq!(export["archive_version"], 2);
        assert_eq!(
            export["game"]["Created"].as_i64(),
            Some(created_utc.timestamp_micros()),
            "header Created is the engine column value"
        );

        // Control: Created present and different — the column still wins
        // for row and path, and the header keeps its JSON value.
        let different_created = created_utc - Duration::hours(48);
        insert_engine_game(
            &engine_pool,
            "with-created",
            fixture_game_json("with-created", different_created.timestamp_micros()),
            created,
        )
        .await?;
        insert_engine_frame(&engine_pool, "with-created", 0, Some(fixture_frame(0))).await?;

        backup_single_game(&state, "with-created").await?;

        let row = sqlx::query!(
            r#"SELECT created_at, gcs_path FROM games WHERE engine_game_id = 'with-created'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.created_at, created_utc);
        assert_eq!(
            row.gcs_path.as_deref(),
            Some("games/2021/05/05/with-created.json.zst")
        );
        let bytes = store
            .get("games/2021/05/05/with-created.json.zst")
            .await?
            .expect("object exists");
        let json = zstd::decode_all(bytes.as_slice())?;
        let export: serde_json::Value = serde_json::from_slice(&json)?;
        assert_eq!(
            export["game"]["Created"].as_i64(),
            Some(different_created.timestamp_micros()),
            "header keeps its JSON value when both are present"
        );
        Ok(())
    }

    /// DEV-1984 req 2: zero frames is permanent — nothing is ever uploaded.
    #[sqlx::test(migrations = "../migrations")]
    async fn zero_frames_is_permanent(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 7, 4)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");

        insert_engine_game(
            &engine_pool,
            "no-frames",
            fixture_game_json("no-frames", created.and_utc().timestamp_micros()),
            created,
        )
        .await?;

        backup_single_game(&state, "no-frames")
            .await
            .expect("zero frames is permanent, not a retryable error");

        let exception = sqlx::query!(
            "SELECT reason FROM play_archive_exceptions WHERE engine_game_id = 'no-frames'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(exception.reason, "no_frames");
        assert_eq!(store.put_count(), 0);
        Ok(())
    }

    /// A claimed backup waits on BACKUP_SLOT while holding its worker, so a
    /// burst must sort below game runners or new games queue behind it.
    #[sqlx::test(migrations = "../migrations")]
    async fn backups_queue_below_default_priority_jobs(pool: sqlx::PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        enqueue_backup_job(&app_state, "engine-game-1").await?;
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
            vec![BACKUP_JOB_PRIORITY],
            "backups use the backup priority"
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
        // Race-free re-admission: the slot is shared process-wide with
        // native_backup's tests, which may hold a permit at this instant —
        // so await one under a timeout instead of asserting try_acquire
        // (which flakes when another test holds the slot right after drop).
        let readmitted =
            tokio::time::timeout(std::time::Duration::from_secs(10), BACKUP_SLOT.acquire())
                .await
                .expect("backup slot stayed busy for 10s")
                .unwrap();
        drop(readmitted);
    }
}
