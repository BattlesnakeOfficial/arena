//! Dropped-fields repair (DEV-2344): re-archive the 1,024 v1 Play objects
//! whose typed export silently dropped three engine JSON keys — snake
//! `Team` (string, non-empty in 403 team-mode games 2020-03-27 to
//! 2020-07-17), header `Mode` (string, 994 games) and header
//! `MaxTurnsToNextFoodSpawn` (number, always 12; 815 games) — because the
//! `engine_models.rs` structs lacked them and serde ignores unknown keys.
//!
//! The engine DB is the only remaining copy of these fields and is torn
//! down after Play closes 2026-11-30.
//!
//! Evidence: memory repo
//! `projects/arena/coordinators/arena-play-history-archive/finish-2026-11-13/`
//! (`team_games.txt` 403 ids ∪ `mode_ids.txt` 994 ids = 1,024 unique
//! engine game ids, checked in as `dropped_fields_repair_ids.txt`).
//!
//! Flow: `DroppedFieldsRepairSweepJob` (cron, 5 min) fills the
//! `dropped_fields_repair_queue` table ONCE from the census list (rows
//! are never deleted, so "empty" means "never filled"; no pending rows =
//! done, and the sweep stops by itself), then trickles
//! `DroppedFieldsRepairGameJob`s under an in-flight ceiling at
//! `BACKUP_JOB_PRIORITY`. Each repair: fix the structs (done in
//! `engine_models.rs`), re-export through the normal v2 writer, VERIFY the
//! built bytes against the engine BEFORE any write (a mismatch writes
//! nothing and marks the queue row `failed`), create-only put
//! (`put_if_absent` — DEV-2128: no existing object is ever overwritten or
//! deleted, and the old v1 objects stay at `games/1970/01/01/` so
//! rollback is a single UPDATE restoring `old_gcs_path` /
//! `old_archive_version`), read back, and repoint the arena row with one
//! predicate-guarded UPDATE.
//!
//! `Err` is retryable (timeouts, GCS 5xx, slot waits, read-back
//! anomalies); `Ok` + a `failed` queue row is terminal. **Any `failed`
//! queue row blocks engine-DB teardown sign-off** — a `verify_mismatch`
//! most likely means another unknown key was dropped, which is in scope.
//! The done signal distinguishes failed=0 (info line) from failed>0
//! (ERROR line, the escalation trigger) and is emitted on EVERY tick
//! while no pending rows remain.
//!
//! Re-arming a `failed` row after fixing the cause (there is no cursor to
//! rewind, unlike the walk):
//!
//! ```sql
//! UPDATE dropped_fields_repair_queue
//! SET status = 'pending', reason = NULL
//! WHERE engine_game_id = '…';
//! ```

use cja::jobs::Job as _;
use color_eyre::eyre::{Context as _, eyre};
use std::collections::HashSet;

use crate::state::AppState;

/// The census-defined repair set (DEV-2344): engine game ids whose v1
/// objects dropped snake `Team` and/or header
/// `Mode`/`MaxTurnsToNextFoodSpawn`. Generated once from the
/// coordinator's census evidence (403 team ids + 994 mode ids = 1,024
/// unique); count pinned by a test. This list decides WHICH games get
/// re-archived — per-game correctness is the verify step against the
/// engine DB, not the list. `#`-prefixed lines are comments.
pub(crate) const CENSUS_IDS: &str = include_str!("dropped_fields_repair_ids.txt");

pub(crate) fn census_ids() -> Vec<String> {
    let mut ids: Vec<String> = CENSUS_IDS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Ceiling on pending (queued + running) `DroppedFieldsRepairGameJob`
/// rows — a claimed repair job holds its worker while waiting on
/// `BACKUP_SLOT`, and this bounds how many workers can be pinned (worst
/// case walk 25 + repair 25 = 50 of `ARENA_JOB_WORKERS=100`; game runners
/// still claim at priority 0). 0 pauses the sweep (pause lever).
pub(crate) const DROPPED_FIELDS_INFLIGHT_CEILING: i64 = 25;

/// The v2 export strips these snake keys on purpose (backup.rs
/// `strip_secret_snake_fields`); the comparator strips exactly these from
/// both sides and nothing else.
pub(crate) const STRIPPED_SNAKE_KEYS: [&str; 3] = ["URL", "ProxyURL", "APIVersion"];

/// A value that reads as "not there": JSON null, `false`, `0`, `""`,
/// `[]`, `{}` — the Python verifier's absent-y set.
fn is_absent(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Bool(false) => true,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f == 0.0),
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
        serde_json::Value::Bool(true) => false,
    }
}

/// Normalize children first, then drop absent-y values — exactly the
/// Python verifier's `norm` (recursion happens before the absent check,
/// so `{"A": {}}` disappears entirely). Works on `serde_json::Value`
/// trees, never typed structs, so a key the typed round-trip drops shows
/// up as a mismatch.
pub(crate) fn normalize_for_compare(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, inner) in map {
                if STRIPPED_SNAKE_KEYS.contains(&key.as_str()) {
                    continue;
                }
                let normalized = normalize_for_compare(inner);
                if !is_absent(&normalized) {
                    out.insert(key.clone(), normalized);
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize_for_compare).collect())
        }
        scalar => scalar.clone(),
    }
}

/// Compare an engine header key against the typed field's value, both as
/// JSON Values: absent, JSON null and `None` are all equal, and a type
/// difference (number vs string) is a mismatch (DEV-2344: an `as_str`
/// read would coerce or silently drop the number-typed field).
fn header_field_matches(
    engine_header: &serde_json::Value,
    key: &str,
    typed: &serde_json::Value,
) -> bool {
    let engine = engine_header.get(key).filter(|v| !v.is_null());
    let typed = if typed.is_null() { None } else { Some(typed) };
    match (engine, typed) {
        (None, None) => true,
        (Some(engine), Some(typed)) => engine == typed,
        _ => false,
    }
}

/// Verify a decoded archive object against the engine DB: every frame
/// Value-equal (after [`normalize_for_compare`]) to the engine's raw frame
/// JSON, frame counts equal, header carrying
/// `Mode`/`MaxTurnsToNextFoodSpawn` exactly when the engine does
/// (compared as JSON Values), self-describing as v2 for this game.
/// Returns content problems (empty = pass); transient failures are `Err`.
///
/// Engine-DB queries are runtime `sqlx::query_as`/`query` on purpose —
/// the engine schema is unknown to the compile-time macros (same reason
/// `fetch_completed_games` is non-macro).
pub(crate) async fn verify_object_against_engine(
    engine_db: &sqlx::PgPool,
    engine_game_id: &str,
    engine_header: &serde_json::Value,
    archived: &crate::archive::ArchivedGame,
) -> cja::Result<Vec<String>> {
    let mut problems = Vec::new();

    let game = archived
        .parse_game()
        .wrap_err("Failed to parse read-back header")?;
    if archived.archive_version() != Some(crate::engine_models::ARCHIVE_VERSION) {
        problems.push(format!(
            "archive_version {:?} want {}",
            archived.archive_version(),
            crate::engine_models::ARCHIVE_VERSION
        ));
    }
    if archived.archive_game_id() != Some(engine_game_id) {
        problems.push(format!("game_id {:?}", archived.archive_game_id()));
    }
    // Header fields: JSON-Value comparison. to_value on Option<String> /
    // Option<i32> is infallible in practice, but handle the Result.
    let mode_json = serde_json::to_value(&game.mode)
        .wrap_err("Failed to serialize header Mode for comparison")?;
    if !header_field_matches(engine_header, "Mode", &mode_json) {
        problems.push(format!(
            "header Mode {mode_json:?} vs engine {:?}",
            engine_header.get("Mode")
        ));
    }
    let mttnfs_json = serde_json::to_value(game.max_turns_to_next_food_spawn)
        .wrap_err("Failed to serialize header MaxTurnsToNextFoodSpawn for comparison")?;
    if !header_field_matches(engine_header, "MaxTurnsToNextFoodSpawn", &mttnfs_json) {
        problems.push(format!(
            "header MaxTurnsToNextFoodSpawn {mttnfs_json:?} vs engine {:?}",
            engine_header.get("MaxTurnsToNextFoodSpawn")
        ));
    }

    // Frames: page the engine exactly like the export build did, in the
    // same (turn ASC) order the object stores them.
    let mut after_turn = i32::MIN;
    let mut index = 0usize;
    loop {
        let page = crate::backup::fetch_game_frame_texts_page(
            engine_db,
            engine_game_id,
            after_turn,
            crate::backup::EXPORT_FRAME_PAGE_SIZE,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        if let Some(last) = page.last() {
            after_turn = last.turn;
        }
        for row in &page {
            let Some(text) = row.value_text.as_deref() else {
                problems.push(format!("engine frame turn {} is NULL", row.turn));
                index += 1;
                continue;
            };
            let Some(archived_raw) = archived.frames().get(index) else {
                problems.push(format!(
                    "more engine frames than archived (at turn {})",
                    row.turn
                ));
                break;
            };
            let engine_frame: serde_json::Value =
                serde_json::from_str(text).wrap_err("Failed to parse engine frame")?;
            let archived_frame: serde_json::Value = serde_json::from_str(archived_raw.get())
                .wrap_err("Failed to parse archived frame")?;
            if normalize_for_compare(&engine_frame) != normalize_for_compare(&archived_frame) {
                problems.push(format!("frame mismatch at turn {}", row.turn));
                break;
            }
            index += 1;
        }
        if !problems.is_empty() {
            break;
        }
    }
    if problems.is_empty() && index != archived.frame_count() {
        problems.push(format!(
            "frame count {} vs engine {}",
            archived.frame_count(),
            index
        ));
    }
    Ok(problems)
}

/// Count frames where any snake carries a non-empty Team — for the
/// per-game success log. A frame that fails to parse counts as no-team
/// (a verified object's frames always parse; this is defensive only).
fn count_frames_with_team(archived: &crate::archive::ArchivedGame) -> usize {
    archived
        .frames()
        .iter()
        .filter(|raw| {
            serde_json::from_str::<serde_json::Value>(raw.get())
                .ok()
                .and_then(|f| f.get("Snakes").and_then(|s| s.as_array()).cloned())
                .is_some_and(|snakes| {
                    snakes.iter().any(|s| {
                        s.get("Team")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|t| !t.is_empty())
                    })
                })
        })
        .count()
}

/// Update one queue row's status/reason.
async fn mark_queue_row(
    db: &sqlx::PgPool,
    engine_game_id: &str,
    status: &str,
    reason: Option<&str>,
) -> cja::Result<()> {
    sqlx::query!(
        "UPDATE dropped_fields_repair_queue
         SET status = $2, reason = $3, updated_at = NOW()
         WHERE engine_game_id = $1",
        engine_game_id,
        status,
        reason
    )
    .execute(db)
    .await
    .wrap_err("Failed to update dropped_fields_repair_queue")?;
    Ok(())
}

/// Repair one game: new v2 object (create-only) with
/// Team/Mode/MaxTurnsToNextFoodSpawn restored, the built export verified
/// against the engine BEFORE any write, the arena row repointed with one
/// predicate-guarded UPDATE, the archive cache invalidated. Slot-first,
/// mirroring `backup_single_game`.
pub(crate) async fn repair_game(app_state: &AppState, engine_game_id: &str) -> cja::Result<()> {
    let engine_db = app_state
        .engine_db
        .as_ref()
        .ok_or_else(|| eyre!("Engine database not configured"))?;
    if !app_state.object_store.is_configured() {
        return Err(eyre!("GCS bucket not configured"));
    }

    // SLOT FIRST — the repair MUST finish before engine-DB teardown, and
    // this serializes against all other backup-family work process-wide.
    let _permit = std::sync::Arc::clone(&crate::backup::BACKUP_SLOT)
        .acquire_owned()
        .await
        .wrap_err("Backup slot semaphore closed")?;

    // Idempotency re-check under the slot: the arena row decides.
    let arena_row = sqlx::query!(
        "SELECT game_id, gcs_path, archive_version FROM games WHERE engine_game_id = $1",
        engine_game_id
    )
    .fetch_optional(&app_state.db)
    .await
    .wrap_err("Failed to load arena game row")?;
    let Some(arena_row) = arena_row else {
        mark_queue_row(
            &app_state.db,
            engine_game_id,
            "failed",
            Some("no_arena_row"),
        )
        .await?;
        return Ok(());
    };
    let Some(old_path) = arena_row.gcs_path else {
        mark_queue_row(&app_state.db, engine_game_id, "failed", Some("no_gcs_path")).await?;
        return Ok(());
    };
    if !old_path.starts_with("games/1970/01/01/") {
        // Already repointed (duplicate job / rerun): done, no write.
        mark_queue_row(&app_state.db, engine_game_id, "done", None).await?;
        return Ok(());
    }

    // Fetch + parse the engine game.
    let game_row = match crate::backup::fetch_game_by_id(engine_db, engine_game_id).await? {
        Some(row) => row,
        None => {
            mark_queue_row(
                &app_state.db,
                engine_game_id,
                "failed",
                Some("engine_row_missing"),
            )
            .await?;
            return Ok(());
        }
    };
    // The raw header (with Mode/MaxTurnsToNextFoodSpawn) is what
    // verification compares against; the parse below consumes the value.
    let engine_header = game_row.value.clone();
    let mut game: crate::engine_models::EngineGame = match serde_json::from_value(game_row.value) {
        Ok(game) => game,
        Err(error) => {
            let reason = format!("game_header_unparseable: {error}");
            mark_queue_row(&app_state.db, engine_game_id, "failed", Some(&reason)).await?;
            return Ok(());
        }
    };

    // Date handling, copied from backup_single_game: dates come from the
    // engine `created` column (the JSON `Created` key is absent pre-2023).
    let created_at = game_row.created.and_utc();
    if game.created == 0 {
        game.created = created_at.timestamp_micros();
    }

    let bytes = match crate::backup::build_export_streamed(engine_db, game.clone()).await {
        Ok(bytes) => bytes,
        Err(crate::backup::ExportBuildError::Permanent(reason)) => {
            mark_queue_row(&app_state.db, engine_game_id, "failed", Some(&reason)).await?;
            return Ok(());
        }
        Err(crate::backup::ExportBuildError::Transient(error)) => return Err(error),
    };

    let new_path = crate::backup::gcs_path(engine_game_id, created_at);

    // Verify the built bytes in memory, BEFORE any write: a wrong object
    // at the target path would strand the path until a human deletes it
    // from the bucket (the bucket has no versioning, DEV-2128).
    let decoded = tokio::task::spawn_blocking({
        let bytes = bytes.clone();
        move || crate::archive::decode_archive_object(&bytes, crate::archive::ArchiveRowKind::Play)
    })
    .await
    .wrap_err("Decode-before-write task panicked")?
    .wrap_err("Failed to decode built export for verification")?;
    let problems =
        verify_object_against_engine(engine_db, engine_game_id, &engine_header, &decoded).await?;
    if !problems.is_empty() {
        tracing::error!(game_id = engine_game_id, ?problems, path = %new_path,
            "dropped-fields repair export verification failed; nothing written");
        let reason = format!("verify_mismatch: {}", problems.join("; "));
        mark_queue_row(&app_state.db, engine_game_id, "failed", Some(&reason)).await?;
        return Ok(());
    }

    // Create-only write: never overwrites an existing object.
    let created = app_state
        .object_store
        .put_if_absent(&new_path, bytes.clone())
        .await
        .wrap_err_with(|| format!("Failed to create archive object for {engine_game_id}"))?;

    let decoded = if created {
        // The object is ours and was just verified in memory; the
        // read-back only needs byte equality with what we wrote.
        let read_back = app_state
            .object_store
            .get(&new_path)
            .await
            .wrap_err("Failed to read back archive object")?;
        match read_back {
            Some(b) if b == bytes => decoded,
            Some(_) => {
                return Err(eyre!(
                    "archive object at {new_path} differs from the bytes just written"
                ));
            }
            None => {
                return Err(eyre!(
                    "archive object missing right after write: {new_path}"
                ));
            }
        }
    } else {
        // An object already exists at the target (crash recovery or a
        // duplicate run): verify THAT object against the engine and
        // repoint only on pass — never overwrite.
        let existing = app_state
            .object_store
            .get(&new_path)
            .await
            .wrap_err("Failed to read existing archive object")?
            .ok_or_else(|| {
                eyre!("put_if_absent reported an existing object but get found none: {new_path}")
            })?;
        let decoded = tokio::task::spawn_blocking(move || {
            crate::archive::decode_archive_object(&existing, crate::archive::ArchiveRowKind::Play)
        })
        .await
        .wrap_err("Decode-existing task panicked")?
        .wrap_err("Failed to decode existing archive object for verification")?;
        let problems =
            verify_object_against_engine(engine_db, engine_game_id, &engine_header, &decoded)
                .await?;
        if !problems.is_empty() {
            tracing::error!(game_id = engine_game_id, ?problems, path = %new_path,
                "dropped-fields repair: existing object at target fails verification; \
                 refusing to overwrite");
            let reason = format!("verify_mismatch: {}", problems.join("; "));
            mark_queue_row(&app_state.db, engine_game_id, "failed", Some(&reason)).await?;
            return Ok(());
        }
        decoded
    };

    // The single repoint UPDATE, predicate-guarded on the old path (the
    // trigger bumps updated_at; created_at/game_type/board_size/archived_at
    // are untouched).
    let result = sqlx::query!(
        "UPDATE games SET gcs_path = $2, archive_version = $3
         WHERE engine_game_id = $1 AND gcs_path = $4",
        engine_game_id,
        new_path,
        crate::engine_models::ARCHIVE_VERSION,
        old_path
    )
    .execute(&app_state.db)
    .await
    .wrap_err("Failed to repoint arena game row")?;
    if result.rows_affected() == 0 {
        tracing::debug!(
            game_id = engine_game_id,
            "dropped-fields repair: row already repointed by another run"
        );
    }

    mark_queue_row(&app_state.db, engine_game_id, "done", None).await?;

    // Invalidate the archive cache for the arena UUID AFTER the UPDATE:
    // path-aware peek/insert already make stale serves impossible; this
    // just frees the cache weight promptly.
    app_state.archive.invalidate(arena_row.game_id).await;

    let decoded_game = decoded
        .parse_game()
        .wrap_err("Failed to parse decoded header for success log")?;
    let team_frames = count_frames_with_team(&decoded);
    tracing::info!(
        game_id = engine_game_id,
        old = %old_path,
        new = %new_path,
        created,
        mode = ?decoded_game.mode,
        max_turns_to_next_food_spawn = ?decoded_game.max_turns_to_next_food_spawn,
        team_frames,
        "Re-archived game with dropped fields restored"
    );

    Ok(())
}

/// What one sweep tick did — returned so tests can assert without
/// scraping logs.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RepairSweepStats {
    pub(crate) filled: usize,
    pub(crate) enqueued: usize,
    pub(crate) pending_rows: i64,
    pub(crate) failed_rows: i64,
    pub(crate) done: bool,
}

pub(crate) async fn run_sweep_tick(app_state: &AppState) -> cja::Result<()> {
    run_sweep_tick_with(app_state, DROPPED_FIELDS_INFLIGHT_CEILING)
        .await
        .map(|_| ())
}

/// One sweep tick. `ceiling` is the test seam (`ceiling == 0` pauses).
pub(crate) async fn run_sweep_tick_with(
    app_state: &AppState,
    ceiling: i64,
) -> cja::Result<RepairSweepStats> {
    // Pause lever: 0 skips the whole tick, including the fill.
    if ceiling == 0 {
        return Ok(RepairSweepStats::default());
    }

    // Post-teardown no-op, same as the walk: an unconfigured engine DB or
    // store must not crash-loop the cron.
    if app_state.engine_db.is_none() {
        tracing::warn!("Engine database not configured, skipping dropped-fields repair sweep");
        return Ok(RepairSweepStats::default());
    }
    if !app_state.object_store.is_configured() {
        tracing::warn!("GCS bucket not configured, skipping dropped-fields repair sweep");
        return Ok(RepairSweepStats::default());
    }

    // Fill once — only when the queue is empty, from the census list,
    // filtered against arena rows still on the v1 1970 path. NO engine-DB
    // query at all (the engine pool is max_connections(2)).
    let is_empty = sqlx::query_scalar!(
        r#"SELECT NOT EXISTS(SELECT 1 FROM dropped_fields_repair_queue) AS "empty!""#
    )
    .fetch_one(&app_state.db)
    .await
    .wrap_err("Failed to check dropped_fields_repair_queue")?;
    let mut filled = 0usize;
    if is_empty {
        let ids = census_ids();
        let result = sqlx::query!(
            r#"
            INSERT INTO dropped_fields_repair_queue
                (engine_game_id, old_gcs_path, old_archive_version)
            SELECT g.engine_game_id, g.gcs_path, g.archive_version
            FROM games g
            WHERE g.engine_game_id = ANY($1)
              AND g.gcs_path LIKE 'games/1970/01/01/%'
              AND g.archived_at IS NOT NULL
            ON CONFLICT (engine_game_id) DO NOTHING
            "#,
            &ids
        )
        .execute(&app_state.db)
        .await
        .wrap_err("Failed to fill dropped_fields_repair_queue")?;
        filled = result.rows_affected() as usize;
        // Compute the missing set from the queue's ACTUAL contents (not
        // rows_affected) so a concurrent fill causes no false alarm.
        let queued: HashSet<String> =
            sqlx::query_scalar!("SELECT engine_game_id FROM dropped_fields_repair_queue")
                .fetch_all(&app_state.db)
                .await
                .wrap_err("Failed to read dropped_fields_repair_queue after fill")?
                .into_iter()
                .collect();
        let missing: Vec<&str> = ids
            .iter()
            .filter(|id| !queued.contains(*id))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            tracing::info!(
                listed = ids.len(),
                filled,
                "dropped-fields repair queue filled from census list"
            );
        } else {
            // At most 1,024 ids — list them.
            tracing::error!(
                listed = ids.len(),
                filled,
                missing = ?missing,
                "dropped-fields repair fill: listed games not queued \
                 (already repointed, missing arena row, or not on a v1 1970 path)"
            );
        }
    }

    // Budget: ceiling minus pending repair jobs. Failed cja jobs stay in
    // `jobs` backing off — they hold no worker but ARE counted
    // (deliberate back-pressure, walk precedent).
    let pending_jobs: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM jobs
           WHERE name = 'DroppedFieldsRepairGameJob'"#
    )
    .fetch_one(&app_state.db)
    .await
    .wrap_err("Failed to count pending repair jobs")?;
    let budget = (ceiling - pending_jobs).max(0);

    // Enqueue up to `budget` pending rows that don't already have a job.
    let candidates: Vec<String> = sqlx::query_scalar!(
        r#"SELECT q.engine_game_id AS "engine_game_id!"
           FROM dropped_fields_repair_queue q
           WHERE q.status = 'pending'
             AND NOT EXISTS (
                 SELECT 1 FROM jobs j
                 WHERE j.name = 'DroppedFieldsRepairGameJob'
                   AND j.payload->>'engine_game_id' = q.engine_game_id
             )
           ORDER BY q.engine_game_id
           LIMIT $1"#,
        budget
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to select repair candidates")?;
    for engine_game_id in &candidates {
        crate::jobs::DroppedFieldsRepairGameJob {
            engine_game_id: engine_game_id.to_owned(),
        }
        .enqueue(
            app_state.clone(),
            format!("dropped-fields repair {engine_game_id}"),
            Some(crate::backup::BACKUP_JOB_PRIORITY),
        )
        .await
        .wrap_err_with(|| format!("Failed to enqueue repair job for {engine_game_id}"))?;
    }
    if !candidates.is_empty() {
        sqlx::query!(
            "UPDATE dropped_fields_repair_queue
             SET enqueued_at = NOW(), updated_at = NOW()
             WHERE engine_game_id = ANY($1)",
            &candidates
        )
        .execute(&app_state.db)
        .await
        .wrap_err("Failed to mark repair rows enqueued")?;
    }

    // Done signal — on EVERY tick where no pending rows remain, no
    // once-only state (continuous signal for log-based reconciliation).
    let pending_rows = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM dropped_fields_repair_queue WHERE status = 'pending'"#
    )
    .fetch_one(&app_state.db)
    .await
    .wrap_err("Failed to count pending repair rows")?;
    let mut failed_rows = 0i64;
    let mut done_rows = 0i64;
    let mut queued_rows = 0i64;
    let mut done = false;
    if pending_rows == 0 {
        done = true;
        let rows = sqlx::query!(
            r#"SELECT status, COUNT(*) AS "count!" FROM dropped_fields_repair_queue GROUP BY status"#
        )
        .fetch_all(&app_state.db)
        .await
        .wrap_err("Failed to count dropped_fields_repair_queue by status")?;
        for row in &rows {
            queued_rows += row.count;
            match row.status.as_str() {
                "done" => done_rows = row.count,
                "failed" => failed_rows = row.count,
                _ => {}
            }
        }
        let listed = census_ids().len();
        if failed_rows == 0 {
            tracing::info!(
                listed,
                filled = queued_rows,
                done = done_rows,
                pending = pending_rows,
                "dropped-fields repair complete"
            );
        } else {
            // Design rule: failed rows block teardown sign-off — this
            // ERROR line is the escalation trigger.
            tracing::error!(
                listed,
                filled = queued_rows,
                done = done_rows,
                failed = failed_rows,
                "dropped-fields repair complete with failures — every failed row blocks engine-DB teardown sign-off"
            );
        }
    }

    Ok(RepairSweepStats {
        filled,
        enqueued: candidates.len(),
        pending_rows,
        failed_rows,
        done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::test_support::{engine_fixture, insert_engine_frame, insert_engine_game};
    use crate::object_store::ObjectStore as _;

    #[test]
    fn census_list_has_1024_unique_ids() {
        assert_eq!(census_ids().len(), 1024);
    }

    fn fixture_game_header(
        id: &str,
        created_micros: i64,
        mode: Option<&str>,
        max_turns_to_next_food_spawn: Option<i64>,
    ) -> serde_json::Value {
        let mut header = serde_json::json!({
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
        });
        if let Some(mode) = mode {
            header["Mode"] = serde_json::json!(mode);
        }
        if let Some(max) = max_turns_to_next_food_spawn {
            header["MaxTurnsToNextFoodSpawn"] = serde_json::json!(max);
        }
        header
    }

    fn fixture_team_frame(turn: i32) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": "snake-1",
                "Name": "Orb Weaver One",
                "Team": "Orb Weavers",
                "Body": [{"X": 1, "Y": 2}],
                "Health": 90,
            }],
            "Food": [],
        })
    }

    fn created_march_2020() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2020, 3, 15)
            .expect("date")
            .and_hms_opt(12, 0, 0)
            .expect("time")
    }

    async fn seed_engine_game_with_team_frames(
        engine_pool: &sqlx::PgPool,
        id: &str,
    ) -> cja::Result<()> {
        let created = created_march_2020();
        insert_engine_game(
            engine_pool,
            id,
            fixture_game_header(
                id,
                created.and_utc().timestamp_micros(),
                Some("team"),
                Some(12),
            ),
            created,
        )
        .await?;
        insert_engine_frame(engine_pool, id, 0, Some(fixture_team_frame(0))).await?;
        insert_engine_frame(engine_pool, id, 1, Some(fixture_team_frame(1))).await?;
        Ok(())
    }

    async fn seed_v1_arena_row(pool: &sqlx::PgPool, id: &str) -> cja::Result<()> {
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', $1, $2, NOW(), $3, 1)
            "#,
            id,
            created_march_2020().and_utc(),
            format!("games/1970/01/01/{id}.json.zst")
        )
        .execute(pool)
        .await?;
        sqlx::query!(
            "INSERT INTO dropped_fields_repair_queue (engine_game_id) VALUES ($1)",
            id
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn queue_status(
        pool: &sqlx::PgPool,
        id: &str,
    ) -> cja::Result<Option<(String, Option<String>)>> {
        Ok(sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, reason FROM dropped_fields_repair_queue WHERE engine_game_id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?)
    }

    fn new_path_for(id: &str) -> String {
        format!("games/2020/03/15/{id}.json.zst")
    }

    fn decode(bytes: &[u8]) -> crate::archive::ArchivedGame {
        crate::archive::decode_archive_object(bytes, crate::archive::ArchiveRowKind::Play)
            .expect("fixture object decodes")
    }

    /// A v1-shaped object (no archive_version/game_id) built straight from
    /// raw JSON values — the tool for pre-putting WRONG bytes at a target.
    fn v1_shaped_object(header: &serde_json::Value, frames: &[serde_json::Value]) -> Vec<u8> {
        let object = serde_json::json!({
            "game": header,
            "frames": frames,
            "exported_at": "2026-01-01T00:00:00Z",
        });
        zstd::stream::encode_all(
            serde_json::to_vec(&object).expect("serialize").as_slice(),
            3,
        )
        .expect("zstd")
    }

    /// 1. The happy path: new object at the correct date path with the
    /// fields restored, old object untouched, row repointed, queue done.
    #[sqlx::test(migrations = "../migrations")]
    async fn repair_writes_new_object_repoints_row_and_keeps_old(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-a").await?;
        seed_v1_arena_row(&pool, "game-a").await?;
        let old_path = "games/1970/01/01/game-a.json.zst";
        store.put(old_path, vec![1, 2, 3]).await?;

        repair_game(&state, "game-a").await?;

        // New object: v2, Team on frames, Mode + NUMBER MaxTurns in header.
        let new_path = new_path_for("game-a");
        let bytes = store.get(&new_path).await?.expect("new object exists");
        let decoded = decode(&bytes);
        assert_eq!(decoded.archive_version(), Some(2));
        assert_eq!(decoded.archive_game_id(), Some("game-a"));
        let game = decoded.parse_game()?;
        assert_eq!(game.mode.as_deref(), Some("team"));
        assert_eq!(game.max_turns_to_next_food_spawn, Some(12));
        for frame in decoded.frames() {
            let value: serde_json::Value = serde_json::from_str(frame.get())?;
            assert_eq!(value["Snakes"][0]["Team"], "Orb Weavers");
        }
        // The header JSON carries MaxTurnsToNextFoodSpawn as a NUMBER.
        let json = zstd::decode_all(bytes.as_slice())?;
        let value: serde_json::Value = serde_json::from_slice(&json)?;
        assert!(value["game"]["MaxTurnsToNextFoodSpawn"].is_number());

        // Old object untouched.
        assert_eq!(
            store.get(old_path).await?.as_deref(),
            Some(&[1u8, 2, 3][..])
        );

        // Row repointed; created_at untouched.
        let row = sqlx::query!(
            "SELECT gcs_path, archive_version, created_at FROM games WHERE engine_game_id = 'game-a'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.gcs_path.as_deref(), Some(new_path.as_str()));
        assert_eq!(row.archive_version, Some(2));
        assert_eq!(row.created_at, created_march_2020().and_utc());

        // Queue done.
        assert_eq!(
            queue_status(&pool, "game-a").await?,
            Some(("done".to_string(), None))
        );
        Ok(())
    }

    /// 2. A duplicate run after the repoint performs no create and leaves
    /// everything as the first run left it.
    #[sqlx::test(migrations = "../migrations")]
    async fn repair_is_idempotent_for_repointed_row(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-b").await?;
        seed_v1_arena_row(&pool, "game-b").await?;

        repair_game(&state, "game-b").await?;
        let puts = store.put_count();
        let bytes = store.get(&new_path_for("game-b")).await?.expect("exists");

        repair_game(&state, "game-b").await?;

        assert_eq!(
            store.put_count(),
            puts,
            "second run creates nothing (put_if_absent counts only creates)"
        );
        assert_eq!(
            store.get(&new_path_for("game-b")).await?.as_deref(),
            Some(bytes.as_slice()),
            "bytes unchanged"
        );
        assert_eq!(
            queue_status(&pool, "game-b").await?,
            Some(("done".to_string(), None))
        );
        Ok(())
    }

    /// 3. Crash recovery: an object already at the target (correct bytes)
    /// is verified against the engine and the row repointed — nothing is
    /// overwritten.
    #[sqlx::test(migrations = "../migrations")]
    async fn existing_target_is_verified_and_repointed_not_overwritten(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-c").await?;
        seed_v1_arena_row(&pool, "game-c").await?;

        // Build the correct object with a first run, then roll the row
        // back to its pre-repair state (the crash happened between the
        // create-only put and the UPDATE).
        repair_game(&state, "game-c").await?;
        let bytes = store.get(&new_path_for("game-c")).await?.expect("exists");
        sqlx::query!(
            r#"
            UPDATE games SET gcs_path = $2, archive_version = 1
            WHERE engine_game_id = $1
            "#,
            "game-c",
            "games/1970/01/01/game-c.json.zst"
        )
        .execute(&pool)
        .await?;
        sqlx::query!(
            "UPDATE dropped_fields_repair_queue SET status = 'pending', reason = NULL WHERE engine_game_id = $1",
            "game-c"
        )
        .execute(&pool)
        .await?;
        let puts = store.put_count();

        repair_game(&state, "game-c").await?;

        assert_eq!(store.put_count(), puts, "no new create");
        assert_eq!(
            store.get(&new_path_for("game-c")).await?.as_deref(),
            Some(bytes.as_slice()),
            "existing bytes untouched"
        );
        let row = sqlx::query!(
            "SELECT gcs_path, archive_version FROM games WHERE engine_game_id = 'game-c'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.gcs_path.as_deref(),
            Some(new_path_for("game-c").as_str())
        );
        assert_eq!(row.archive_version, Some(2));
        assert_eq!(
            queue_status(&pool, "game-c").await?,
            Some(("done".to_string(), None))
        );
        Ok(())
    }

    /// 4. WRONG bytes already at the target: the row is marked failed, the
    /// bytes are left alone, the arena row stays on the 1970 path.
    #[sqlx::test(migrations = "../migrations")]
    async fn existing_target_mismatch_marks_failed_without_overwrite(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-d").await?;
        seed_v1_arena_row(&pool, "game-d").await?;

        // A v1-shaped export of the same game WITHOUT Team on the snakes.
        let wrong_frames: Vec<serde_json::Value> = (0..2)
            .map(|turn| {
                let mut frame = fixture_team_frame(turn);
                frame["Snakes"][0]
                    .as_object_mut()
                    .expect("snake object")
                    .remove("Team");
                frame
            })
            .collect();
        let wrong = v1_shaped_object(
            &fixture_game_header("game-d", 0, Some("team"), Some(12)),
            &wrong_frames,
        );
        store.put(&new_path_for("game-d"), wrong.clone()).await?;
        let puts = store.put_count();

        repair_game(&state, "game-d")
            .await
            .expect("a terminal mismatch returns Ok so the job does not retry");

        let status = queue_status(&pool, "game-d").await?.expect("queue row");
        assert_eq!(status.0, "failed");
        assert!(
            status
                .1
                .as_deref()
                .unwrap_or_default()
                .starts_with("verify_mismatch")
        );
        assert_eq!(store.put_count(), puts, "no create");
        assert_eq!(
            store.get(&new_path_for("game-d")).await?.as_deref(),
            Some(wrong.as_slice()),
            "wrong bytes are never overwritten"
        );
        let row = sqlx::query!("SELECT gcs_path FROM games WHERE engine_game_id = 'game-d'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            row.gcs_path.as_deref(),
            Some("games/1970/01/01/game-d.json.zst")
        );
        Ok(())
    }

    /// 5. The pre-write control: an engine frame carrying a key the typed
    /// struct still lacks (`Foo`) — serde drops it on export, the
    /// in-memory comparator catches it, NOTHING is written.
    #[sqlx::test(migrations = "../migrations")]
    async fn export_mismatch_writes_nothing(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-e").await?;
        // One extra engine frame with an unknown snake key.
        let mut frame = fixture_team_frame(2);
        frame["Snakes"][0]["Foo"] = serde_json::json!("bar");
        insert_engine_frame(&engine_pool, "game-e", 2, Some(frame)).await?;
        seed_v1_arena_row(&pool, "game-e").await?;

        repair_game(&state, "game-e")
            .await
            .expect("a terminal mismatch returns Ok");

        let status = queue_status(&pool, "game-e").await?.expect("queue row");
        assert_eq!(status.0, "failed");
        let reason = status.1.expect("reason set");
        assert!(
            reason.starts_with("verify_mismatch"),
            "reason was: {reason}"
        );
        assert_eq!(store.put_count(), 0, "nothing written");
        assert!(
            store.get(&new_path_for("game-e")).await?.is_none(),
            "no object at the target path"
        );
        let row = sqlx::query!("SELECT gcs_path FROM games WHERE engine_game_id = 'game-e'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            row.gcs_path.as_deref(),
            Some("games/1970/01/01/game-e.json.zst")
        );
        Ok(())
    }

    /// 6. A queued game with no engine row is terminal, not retryable.
    #[sqlx::test(migrations = "../migrations")]
    async fn missing_engine_row_marks_failed(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, _engine_pool) = engine_fixture(&pool).await?;
        seed_v1_arena_row(&pool, "game-f").await?;

        repair_game(&state, "game-f")
            .await
            .expect("a missing engine row is terminal, not a retryable error");

        assert_eq!(
            queue_status(&pool, "game-f").await?,
            Some(("failed".to_string(), Some("engine_row_missing".to_string())))
        );
        assert_eq!(store.put_count(), 0);
        Ok(())
    }

    /// 7. The shown control: take the verified object from the happy
    /// path, delete Team from one snake in one frame, re-encode — the
    /// comparator MUST report the frame mismatch. The unmodified object
    /// against the same engine data MUST report none.
    #[sqlx::test(migrations = "../migrations")]
    async fn comparator_catches_missing_team(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-g").await?;
        seed_v1_arena_row(&pool, "game-g").await?;
        repair_game(&state, "game-g").await?;

        let good_bytes = store.get(&new_path_for("game-g")).await?.expect("exists");
        let engine_header = fixture_game_header("game-g", 0, Some("team"), Some(12));

        // Unmodified: no problems.
        let good = decode(&good_bytes);
        let problems =
            verify_object_against_engine(&engine_pool, "game-g", &engine_header, &good).await?;
        assert!(problems.is_empty(), "problems: {problems:?}");

        // Modified: delete Team from one snake in one frame, re-encode.
        let mut value: serde_json::Value =
            serde_json::from_slice(&zstd::decode_all(good_bytes.as_slice())?)?;
        value["frames"][1]["Snakes"][0]
            .as_object_mut()
            .expect("snake object")
            .remove("Team");
        let bad_bytes = zstd::stream::encode_all(serde_json::to_vec(&value)?.as_slice(), 9)?;
        let bad = decode(&bad_bytes);

        let problems =
            verify_object_against_engine(&engine_pool, "game-g", &engine_header, &bad).await?;
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("frame mismatch at turn")),
            "problems: {problems:?}"
        );
        Ok(())
    }

    /// 8. A header type change is a mismatch: engine header carrying
    /// MaxTurnsToNextFoodSpawn as a STRING against an object carrying the
    /// number (an `as_str` comparator would have passed it).
    #[sqlx::test(migrations = "../migrations")]
    async fn header_type_mismatch_is_a_problem(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        // Engine frames without snakes keep the frame comparison trivial.
        let created = created_march_2020();
        let header_number = fixture_game_header("game-h", 0, Some("team"), Some(12));
        insert_engine_game(&engine_pool, "game-h", header_number.clone(), created).await?;
        let frame = serde_json::json!({"Turn": 0, "Snakes": [], "Food": []});
        insert_engine_frame(&engine_pool, "game-h", 0, Some(frame)).await?;
        // Build the sanity object as a REAL v2 export (the repair's own
        // build path), not a v1-shaped fixture: v1 objects carry no game
        // id, which would surface an unrelated `game_id` problem.
        let mut game: crate::engine_models::EngineGame =
            serde_json::from_value(header_number.clone())?;
        if game.created == 0 {
            game.created = created.and_utc().timestamp_micros();
        }
        let object = crate::backup::build_export_streamed(&engine_pool, game)
            .await
            .map_err(|e| match e {
                crate::backup::ExportBuildError::Permanent(reason) => eyre!(reason),
                crate::backup::ExportBuildError::Transient(error) => error,
            })?;
        let decoded = decode(&object);

        // Sanity: the number-typed engine header matches the object.
        let problems =
            verify_object_against_engine(&engine_pool, "game-h", &header_number, &decoded).await?;
        assert!(problems.is_empty(), "problems: {problems:?}");

        // The engine header with a STRING 12 vs the object's NUMBER 12 is
        // a header mismatch.
        let mut header_string = header_number.clone();
        header_string["MaxTurnsToNextFoodSpawn"] = serde_json::json!("12");
        let problems =
            verify_object_against_engine(&engine_pool, "game-h", &header_string, &decoded).await?;
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("header MaxTurnsToNextFoodSpawn")),
            "problems: {problems:?}"
        );
        let _ = state; // engine_fixture wired; state unused beyond setup
        Ok(())
    }

    /// 9. The fill is census-listed v1 rows only: an unlisted v1 game gets
    /// no row and no job; a second tick does not refill or duplicate.
    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_fills_from_census_list_for_v1_rows_only(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, _engine_pool) = engine_fixture(&pool).await?;
        let ids = census_ids();
        let listed_a = ids[0].clone();
        let listed_b = ids[1].clone();

        let seed_row = |id: String| {
            let pool = pool.clone();
            async move {
                sqlx::query!(
                    r#"
                    INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
                    VALUES ('11x11', 'Standard', 'finished', $1, '2020-03-15T12:00:00Z', NOW(), $2, 1)
                    "#,
                    id,
                    format!("games/1970/01/01/{id}.json.zst")
                )
                .execute(&pool)
                .await?;
                Ok::<(), color_eyre::Report>(())
            }
        };
        seed_row(listed_a.clone()).await?;
        seed_row(listed_b.clone()).await?;
        seed_row("not-on-census-list".to_string()).await?;

        let stats = run_sweep_tick_with(&state, 25).await?;
        assert_eq!(stats.filled, 2);

        let queued: Vec<(String, Option<String>, Option<i32>)> = sqlx::query_as(
            "SELECT engine_game_id, old_gcs_path, old_archive_version \
             FROM dropped_fields_repair_queue",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(queued.len(), 2);
        for (engine_game_id, old_gcs_path, old_archive_version) in &queued {
            assert!(engine_game_id == &listed_a || engine_game_id == &listed_b);
            let expected_path = format!("games/1970/01/01/{engine_game_id}.json.zst");
            assert_eq!(old_gcs_path.as_deref(), Some(expected_path.as_str()));
            assert_eq!(*old_archive_version, Some(1));
        }

        // Both listed ids got a repair job at BACKUP_JOB_PRIORITY.
        let jobs: Vec<(String, i32)> = sqlx::query_as(
            "SELECT payload->>'engine_game_id', priority FROM jobs \
             WHERE name = 'DroppedFieldsRepairGameJob'",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(jobs.len(), 2);
        for (job_id, priority) in &jobs {
            assert!(job_id == &listed_a || job_id == &listed_b);
            assert_eq!(*priority, crate::backup::BACKUP_JOB_PRIORITY);
        }
        assert_eq!(stats.enqueued, 2);

        // The unlisted control: no queue row, no job.
        let unlisted: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!"
               FROM dropped_fields_repair_queue
               WHERE engine_game_id = 'not-on-census-list'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(unlisted, 0);

        // Second tick: no refill, no duplicates.
        let stats = run_sweep_tick_with(&state, 25).await?;
        assert_eq!(stats.filled, 0);
        let job_count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM jobs WHERE name = 'DroppedFieldsRepairGameJob'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(job_count, 2);
        Ok(())
    }

    /// 10. Budget: ceiling minus already-pending jobs; 0 pauses entirely.
    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_budget_respects_pending_jobs(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, _engine_pool) = engine_fixture(&pool).await?;
        let ids = census_ids();

        // Seed 10 arena rows from the census list (so the fill queues
        // them), then fake k pending repair jobs.
        for id in ids.iter().take(10) {
            sqlx::query!(
                r#"
                INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
                VALUES ('11x11', 'Standard', 'finished', $1, '2020-03-15T12:00:00Z', NOW(), $2, 1)
                "#,
                id,
                format!("games/1970/01/01/{id}.json.zst")
            )
            .execute(&pool)
            .await?;
        }
        for number in 0..7 {
            sqlx::query!(
                r#"
                INSERT INTO jobs (job_id, name, payload, priority, context)
                VALUES (gen_random_uuid(), 'DroppedFieldsRepairGameJob', $1::jsonb, -10, 'fake backlog')
                "#,
                serde_json::json!({"engine_game_id": format!("fake-{number}")})
            )
            .execute(&pool)
            .await?;
        }

        // Ceiling 25 with 7 pending jobs -> 10 candidates, budget 18, so
        // all 10 enqueue.
        let stats = run_sweep_tick_with(&state, 25).await?;
        assert_eq!(stats.filled, 10);
        assert_eq!(stats.enqueued, 10);

        // Ceiling below the backlog: nothing new.
        sqlx::query!("DELETE FROM jobs WHERE context = 'fake backlog'")
            .execute(&pool)
            .await?;
        for number in 0..30 {
            sqlx::query!(
                r#"
                INSERT INTO jobs (job_id, name, payload, priority, context)
                VALUES (gen_random_uuid(), 'DroppedFieldsRepairGameJob', $1::jsonb, -10, 'fake backlog')
                "#,
                serde_json::json!({"engine_game_id": format!("fake-{number}")})
            )
            .execute(&pool)
            .await?;
        }
        let jobs_before: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM jobs WHERE name = 'DroppedFieldsRepairGameJob'"#
        )
        .fetch_one(&pool)
        .await?;
        let stats = run_sweep_tick_with(&state, 25).await?;
        assert_eq!(stats.enqueued, 0, "budget = max(0, 25 - 30) = 0");
        let jobs_after: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!" FROM jobs WHERE name = 'DroppedFieldsRepairGameJob'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(jobs_before, jobs_after);

        // Ceiling 0 pauses entirely — even the fill.
        sqlx::query!("DELETE FROM dropped_fields_repair_queue")
            .execute(&pool)
            .await?;
        let stats = run_sweep_tick_with(&state, 0).await?;
        assert_eq!(stats, RepairSweepStats::default());
        let queue_rows: i64 =
            sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM dropped_fields_repair_queue"#)
                .fetch_one(&pool)
                .await?;
        assert_eq!(queue_rows, 0, "ceiling 0 skips the fill");
        Ok(())
    }

    /// 11. Done signal: no pending rows -> done, failed_rows counted,
    /// nothing enqueued.
    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_done_when_no_pending_rows(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, _engine_pool) = engine_fixture(&pool).await?;
        for (id, status) in [
            ("done-1", "done"),
            ("done-2", "done"),
            ("failed-1", "failed"),
        ] {
            sqlx::query!(
                r#"
                INSERT INTO dropped_fields_repair_queue (engine_game_id, status, reason)
                VALUES ($1, $2, CASE WHEN $2 = 'failed' THEN 'boom' ELSE NULL END)
                "#,
                id,
                status
            )
            .execute(&pool)
            .await?;
        }

        let stats = run_sweep_tick_with(&state, 25).await?;
        assert!(stats.done);
        assert_eq!(stats.pending_rows, 0);
        assert_eq!(stats.failed_rows, 1);
        assert_eq!(stats.enqueued, 0, "nothing to enqueue");
        Ok(())
    }

    /// 12. The repair invalidates a primed archive cache entry.
    #[sqlx::test(migrations = "../migrations")]
    async fn repair_invalidates_archive_cache(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        seed_engine_game_with_team_frames(&engine_pool, "game-i").await?;
        seed_v1_arena_row(&pool, "game-i").await?;

        // Prime the cache with a valid (old, team-less) v1 object at the
        // old path.
        let old_header = fixture_game_header("game-i", 0, None, None);
        let old_frames: Vec<serde_json::Value> = (0..2).map(fixture_team_frame).collect();
        let old_object = v1_shaped_object(&old_header, &old_frames);
        let old_path = "games/1970/01/01/game-i.json.zst";
        store.put(old_path, old_object).await?;
        let row = sqlx::query!("SELECT game_id FROM games WHERE engine_game_id = 'game-i'")
            .fetch_one(&pool)
            .await?;
        let arena_uuid = row.game_id;
        state
            .archive
            .load(arena_uuid, old_path, crate::archive::ArchiveRowKind::Play)
            .await
            .expect("prime the archive cache");
        assert!(state.archive.cached(arena_uuid), "cache primed");

        repair_game(&state, "game-i").await?;

        assert!(!state.archive.cached(arena_uuid), "entry invalidated");
        // A fresh load under the new path serves the NEW object.
        let served = state
            .archive
            .load(
                arena_uuid,
                &new_path_for("game-i"),
                crate::archive::ArchiveRowKind::Play,
            )
            .await
            .expect("load the repointed object");
        let frame: serde_json::Value = serde_json::from_str(served.frames()[0].get())?;
        assert_eq!(frame["Snakes"][0]["Team"], "Orb Weavers");
        Ok(())
    }

    /// 13. Real-data rehearsal for DEV-2344 — run by the coordinator, not CI.
    ///
    /// Fixtures cannot pin old-data compatibility (real `games.id` is
    /// VARCHAR(255), old rows lack keys, Go `omitempty` drops zero
    /// coordinates), so this runs the REAL repair path over real engine
    /// rows exported read-only from prod. It no-ops unless
    /// `DEV2344_REHEARSAL_DIR` is set; with it set, it reads two CSVs
    /// with exactly the columns `engine_fixture` creates:
    ///
    /// * `games.csv` (columns `id,value,created`), exported with:
    ///   `\copy (SELECT id, value, created FROM games WHERE id = ANY(ARRAY['d0ebdb91-65e1-40da-b58d-5de972c77e9e','9f56e4c9-bfa5-485c-b20f-9d61ff7f9644','79881d67-e882-42b4-9920-7a46bfe81ccf', …])) TO 'games.csv' CSV`
    /// * `game_frames.csv` (columns `id,turn,value`), exported with:
    ///   `\copy (SELECT id, turn, value FROM game_frames WHERE id = ANY(ARRAY[…same ids…]) ORDER BY id, turn) TO 'game_frames.csv' CSV`
    ///
    /// ~25 games: the three sample200 failures (d0ebdb91, 9f56e4c9,
    /// 79881d67 — the games that failed DEV-1984's sample verification
    /// for exactly the missing-Team reason) plus random census picks.
    /// Asserts every queue row is `done`, every new object's frames
    /// carry `Team` wherever the engine frame has a non-empty `Team`,
    /// and each header carries `Mode`/`MaxTurnsToNextFoodSpawn` exactly
    /// when the engine's does. The implementer never touches prod; the
    /// coordinator exports the CSVs and runs:
    /// `DEV2344_REHEARSAL_DIR=<dir> cargo test -p arena --bin arena rehearse_real_engine_rows -- --ignored --nocapture`
    #[ignore = "DEV-2344 real-data rehearsal; run by the coordinator"]
    #[sqlx::test(migrations = "../migrations")]
    async fn rehearse_real_engine_rows(pool: sqlx::PgPool) -> cja::Result<()> {
        let Some(dir) = std::env::var("DEV2344_REHEARSAL_DIR")
            .ok()
            .filter(|d| !d.is_empty())
        else {
            eprintln!(
                "DEV2344_REHEARSAL_DIR is not set; skipping the real-data rehearsal. \
                 Export games.csv and game_frames.csv per the doc comment and set it \
                 to run."
            );
            return Ok(());
        };

        let (state, store, engine_pool) = engine_fixture(&pool).await?;

        // games.csv: id,value,created (psql CSV quoting per RFC 4180).
        let games_path = std::path::Path::new(&dir).join("games.csv");
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(&games_path)
            .wrap_err_with(|| format!("Failed to open {}", games_path.display()))?;
        let mut engine_headers: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        let mut created_values: std::collections::HashMap<String, chrono::NaiveDateTime> =
            std::collections::HashMap::new();
        for record in reader.records() {
            let record = record.wrap_err("Failed to parse games.csv record")?;
            if record.len() < 3 {
                return Err(eyre!(
                    "games.csv record has {} columns, want 3 (id,value,created)",
                    record.len()
                ));
            }
            let (id, value, created) = (&record[0], &record[1], &record[2]);
            let header: serde_json::Value =
                serde_json::from_str(value).wrap_err("games.csv value is not JSON")?;
            let created = chrono::NaiveDateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S%.f")
                .wrap_err("games.csv created does not parse")?;
            insert_engine_game(&engine_pool, id, header.clone(), created).await?;
            engine_headers.insert(id.to_string(), header);
            created_values.insert(id.to_string(), created);
        }

        // game_frames.csv: id,turn,value (empty value cell = SQL NULL).
        let frames_path = std::path::Path::new(&dir).join("game_frames.csv");
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(&frames_path)
            .wrap_err_with(|| format!("Failed to open {}", frames_path.display()))?;
        // Engine frames kept per game for the Team comparison below.
        let mut engine_frames: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        for record in reader.records() {
            let record = record.wrap_err("Failed to parse game_frames.csv record")?;
            if record.len() < 3 {
                return Err(eyre!(
                    "game_frames.csv record has {} columns, want 3 (id,turn,value)",
                    record.len()
                ));
            }
            let (id, turn, value) = (&record[0], &record[1], &record[2]);
            let turn: i32 = turn.parse().wrap_err("turn does not parse")?;
            let value = if value.is_empty() {
                None
            } else {
                Some(
                    serde_json::from_str::<serde_json::Value>(value)
                        .wrap_err("game_frames.csv value is not JSON")?,
                )
            };
            insert_engine_frame(&engine_pool, id, turn, value.clone()).await?;
            if let Some(value) = value {
                engine_frames.entry(id.to_string()).or_default().push(value);
            }
        }

        assert!(
            !engine_headers.is_empty(),
            "rehearsal CSVs contained no games"
        );
        for (id, engine_header) in &engine_headers {
            // Seed the arena row (1970 path, v1, archived) + pending queue row.
            sqlx::query!(
                r#"
                INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
                VALUES ('11x11', 'Standard', 'finished', $1, $2, NOW(), $3, 1)
                "#,
                id,
                created_values[id].and_utc(),
                format!("games/1970/01/01/{id}.json.zst")
            )
            .execute(&pool)
            .await?;
            sqlx::query!(
                "INSERT INTO dropped_fields_repair_queue (engine_game_id) VALUES ($1)",
                id
            )
            .execute(&pool)
            .await?;

            repair_game(&state, id)
                .await
                .wrap_err_with(|| format!("repair_game failed for {id}"))?;

            let status = queue_status(&pool, id).await?.expect("queue row");
            assert_eq!(status.0, "done", "{id}: {:?}", status.1);

            let row = sqlx::query!(
                "SELECT gcs_path, archive_version FROM games WHERE engine_game_id = $1",
                id
            )
            .fetch_one(&pool)
            .await?;
            assert_eq!(row.archive_version, Some(2), "{id}");
            let new_path = row.gcs_path.expect("repointed");
            let bytes = store.get(&new_path).await?.expect("new object exists");
            let decoded = decode(&bytes);

            // Header carries Mode/MaxTurnsToNextFoodSpawn exactly when the
            // engine's does (as JSON Values).
            let game = decoded.parse_game()?;
            assert!(
                header_field_matches(engine_header, "Mode", &serde_json::to_value(game.mode)?),
                "{id}: Mode"
            );
            assert!(
                header_field_matches(
                    engine_header,
                    "MaxTurnsToNextFoodSpawn",
                    &serde_json::to_value(game.max_turns_to_next_food_spawn)?
                ),
                "{id}: MaxTurnsToNextFoodSpawn"
            );

            // Every frame whose engine counterpart has a snake with a
            // non-empty Team carries it.
            let engine_game_frames = engine_frames.get(id).cloned().unwrap_or_default();
            assert_eq!(
                engine_game_frames.len(),
                decoded.frame_count(),
                "{id}: frame count"
            );
            for (turn, (engine_frame, raw)) in
                engine_game_frames.iter().zip(decoded.frames()).enumerate()
            {
                let archived_frame: serde_json::Value = serde_json::from_str(raw.get())?;
                let engine_has_team = engine_frame["Snakes"].as_array().is_some_and(|snakes| {
                    snakes.iter().any(|s| {
                        s.get("Team")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|t| !t.is_empty())
                    })
                });
                if engine_has_team {
                    assert_eq!(
                        normalize_for_compare(&archived_frame["Snakes"]),
                        normalize_for_compare(&engine_frame["Snakes"]),
                        "{id}: frame {turn} team mismatch"
                    );
                }
            }
            eprintln!("{id}: rehearsed OK ({})", decoded.frame_count());
        }
        Ok(())
    }
}
