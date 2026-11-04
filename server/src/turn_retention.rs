//! Prune archived native games' `turns`/`snake_turns` rows from Postgres
//! (DEV-1981). `turns` (26 GB) and `snake_turns` (11 GB) are ~90% of the
//! arena DB and nothing else ever deletes them; once DEV-1980 archived a
//! game and DEV-1981 confirmed the archive object's CONTENT, the DB rows
//! are redundant and this module deletes them.
//!
//! Pipeline: archive (DEV-1980) → confirm the object's content matches
//! `turns` now → prune DB rows → serve from archive (DEV-1979 + the
//! move-request and shout-extraction fallbacks).
//!
//! Operational facts, pinned here so they survive the code:
//!
//! - **NOTIFY behavior**: each prune's `DELETE FROM turns` fires the
//!   `watched_game_turn_delete` trigger once — it materializes a
//!   transition table of the deleted rows (`frame_data` included, ~7 MB
//!   transient per 5000-turn game) and queues one
//!   `pg_notify('arena_watched_games', 'reset:<game_id>')` at commit.
//!   `watched_games.rs` turns that into `WatchedGameUpdate::Reset` — a
//!   no-op for unwatched games, the correct signal for a viewer on an
//!   archive-served page. The shared 1-permit `BACKUP_SLOT` keeps at most
//!   one prune commit in flight.
//! - **`updated_at` bump**: the `turns_pruned_at` UPDATE bumps
//!   `games.updated_at` via the `update_games_updated_at` trigger, exposed
//!   by public list/detail APIs — a pruned month-old game reports
//!   `updated_at` ≈ prune time (precedent: `archived_at` marks bump it
//!   too; accepted).
//! - **No resurrection**: `mark_game_finished_tx` and
//!   `reset_game_state_for_retry` only run from the live game runner on
//!   waiting/running games; pruning requires terminal + archived + ≥ the
//!   retention age, so a pruned game is never re-archived from empty
//!   turns. If a future feature adds a resurrect/replay-old-game path, it
//!   must refuse on `turns_pruned_at IS NOT NULL` (or force a full re-run
//!   that re-inserts turns).
//! - **NULL-`frame_data` turns** (created but never completed — the last
//!   incomplete turn of a terminal game) and their `snake_turns` are
//!   deleted unarchived: the writer's selection is `frame_data IS NOT
//!   NULL`, so no archive ever contained them, and every reader of old
//!   games' frames requires `frame_data`, so such rows are invisible to
//!   all of them.
//! - **Fail-closed retention**: a game whose object is missing or
//!   content-mismatched keeps `turns_pruned_at IS NULL`, stays in
//!   `games_turns_prunable_idx`, is re-enqueued hourly, re-fails, and
//!   re-logs the ERROR hourly. `PRUNE_DISCOVERY_CAP`+ such games stall
//!   pruning entirely (visible as discovery candidates that never
//!   shrink); the remedy is clearing the archive pointers
//!   (`UPDATE games SET archived_at = NULL, gcs_path = NULL,
//!   archive_version = NULL WHERE game_id = …`) so DEV-1980 discovery
//!   re-archives — `native_gcs_path` is deterministic, so the re-archive
//!   overwrites the bad object at the same key.
//! - **Cache discipline**: the confirm decodes via
//!   `archive::decode_archive_object` directly and never touches the
//!   `ArchiveReader` cache (routing 2000 cold games/hour through
//!   `ArchiveReader::load` would evict hot entries).

use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::state::AppState;
use cja::jobs::Job as _;

/// Prune-eligible games per hourly discovery run (~3.6x Play-scale
/// steady-state need; 2000/h drains ~100k backlog in ~2 days). Also the
/// drain-rate lever: lower it if the hourly backup burst plus prunes
/// overcommit the shared slot.
const PRUNE_DISCOVERY_CAP: i64 = 2_000;

/// Job priority for [`crate::jobs::PruneGameTurnsJob`] — BELOW
/// `BACKUP_JOB_PRIORITY` (-10). Archiving gates pruning and carries a 2h
/// SLA; when hourly bursts collide, prunes yield. cja claims
/// `ORDER BY priority DESC, run_at, created_at`.
pub(crate) const PRUNE_JOB_PRIORITY: i32 = -20;

/// Hourly discovery of archived v2 native games past turn retention,
/// enqueuing per-game prune jobs oldest-first. A disabled object store can
/// never confirm content, so discovery no-ops (mirroring
/// `run_native_backup_discovery`).
pub(crate) async fn run_retention_discovery(app_state: &AppState) -> cja::Result<()> {
    run_discovery_with_cap(
        app_state,
        app_state.config.turn_retention_days,
        PRUNE_DISCOVERY_CAP,
    )
    .await
}

/// Testable core: discovery with explicit retention/cap, mirroring
/// `run_discovery_with_caps` in `native_backup.rs`.
async fn run_discovery_with_cap(app_state: &AppState, days: i32, cap: i64) -> cja::Result<()> {
    if !app_state.object_store.is_configured() {
        tracing::warn!("GCS bucket not configured, skipping turn retention discovery");
        return Ok(());
    }

    let candidates: Vec<Uuid> = sqlx::query_scalar!(
        r#"
        SELECT game_id FROM games
        WHERE engine_game_id IS NULL
          AND status IN ('finished', 'failed')
          AND archived_at IS NOT NULL
          AND archive_version = 2
          AND turns_pruned_at IS NULL
          AND gcs_path IS NOT NULL
          AND created_at < NOW() - make_interval(days => $1)
          AND NOT EXISTS (
              SELECT 1 FROM game_battlesnakes gb
              WHERE gb.game_id = games.game_id
                AND gb.created_at >= (SELECT scanned_through - INTERVAL '1 day'
                                      FROM customization_active_week_backfill_cursor
                                      WHERE singleton)
          )
        ORDER BY created_at ASC, game_id ASC
        LIMIT $2
        "#,
        days,
        cap
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to fetch turn-retention candidates")?;

    for game_id in candidates.iter() {
        crate::jobs::PruneGameTurnsJob { game_id: *game_id }
            .enqueue(
                app_state.clone(),
                format!("prune turns for game {game_id}"),
                Some(PRUNE_JOB_PRIORITY),
            )
            .await
            .wrap_err_with(|| format!("Failed to enqueue prune job for {game_id}"))?;
    }

    tracing::info!(
        candidates = candidates.len(),
        "Turn retention discovery complete"
    );
    Ok(())
}

/// Job-facing entry: configured retention, default lock timeout; any Ok
/// outcome maps to Ok(()) — the job retries only on Err; Skipped/Retained
/// are decided outcomes, not failures. The outcome enum is for tests and
/// observability.
pub(crate) async fn prune_game_turns(app_state: &AppState, game_id: Uuid) -> cja::Result<()> {
    prune_game_turns_with_lock_timeout(
        app_state,
        game_id,
        app_state.config.turn_retention_days,
        "5s",
    )
    .await
    .map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PruneOutcome {
    /// Turns deleted and turns_pruned_at committed.
    Pruned { turns_deleted: u64 },
    /// Failed the eligibility re-check under the slot: already pruned
    /// (duplicate job), not terminal-native, too young, no object path,
    /// or backfill cursor behind. A decided no-op — logged at debug.
    Skipped,
    /// Turns retained on purpose (fail-closed) — see RetainReason.
    /// Always logged at ERROR.
    Retained { reason: RetainReason },
}

/// Why turns were retained (log discrimination + test assertions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetainReason {
    /// The object at gcs_path does not exist (archive data loss).
    ObjectMissing,
    /// The object — or one of its frames — does not decode.
    Undecodable,
    /// Self-declared archive_version != 2.
    WrongVersion,
    /// Self-declared game_id != games.game_id.
    WrongGameId,
    /// Frame count differs from turns (stale/incomplete archive — e.g.
    /// the sweeper race).
    FrameCountMismatch,
    /// Frame content differs from turns at equal count.
    FrameContentMismatch,
}

/// Verdict of the pre-delete archive confirmation.
#[derive(Debug)]
enum PruneConfirm {
    Confirmed,
    /// Retain turns and log loudly. `reason` is the test-facing
    /// discriminator; `detail` is the specifics for the ERROR log line.
    Mismatch {
        reason: RetainReason,
        detail: String,
        archived_frames: usize,
        db_frames: usize,
    },
}

/// Pre-delete archive confirmation (DEV-1981): decode the object and
/// require it to hold EXACTLY what `turns` holds now — version 2, this
/// game's id, frame-for-frame equal. Pure CPU, called inside
/// `spawn_blocking` under the backup slot; decodes via
/// `archive::decode_archive_object` directly, never through
/// `ArchiveReader::load` (which would pollute the replay cache). Frames
/// are compared pairwise as `serde_json::Value`, one pair at a time, so
/// peak memory tracks the frame text, not a full tree; `Value` equality
/// is the right comparison because both sides normalize through `Value`,
/// so formatting/key-order differences can't produce false mismatches.
fn confirm_archive_content(game_id: Uuid, compressed: &[u8], db_texts: &[String]) -> PruneConfirm {
    let mismatch =
        |reason: RetainReason, detail: String, archived_frames: usize| PruneConfirm::Mismatch {
            reason,
            detail,
            archived_frames,
            db_frames: db_texts.len(),
        };
    let archived = match crate::archive::decode_archive_object(
        compressed,
        crate::archive::ArchiveRowKind::Native,
    ) {
        Ok(archived) => archived,
        Err(e) => {
            return mismatch(
                RetainReason::Undecodable,
                format!("object does not decode: {e:#}"),
                0,
            );
        }
    };
    if archived.archive_version() != Some(2) {
        return mismatch(
            RetainReason::WrongVersion,
            format!(
                "archive_version {:?}, expected 2",
                archived.archive_version()
            ),
            archived.frame_count(),
        );
    }
    if archived.archive_game_id() != Some(game_id.to_string().as_str()) {
        return mismatch(
            RetainReason::WrongGameId,
            format!(
                "game_id {:?}, expected {game_id}",
                archived.archive_game_id()
            ),
            archived.frame_count(),
        );
    }
    if archived.frame_count() != db_texts.len() {
        return mismatch(
            RetainReason::FrameCountMismatch,
            "frame count differs from turns (stale or incomplete archive)".to_string(),
            archived.frame_count(),
        );
    }
    for (raw, text) in archived.frames().iter().zip(db_texts.iter()) {
        let archived_frame: serde_json::Value = match serde_json::from_str(raw.get()) {
            Ok(v) => v,
            Err(e) => {
                return mismatch(
                    RetainReason::Undecodable,
                    format!("archived frame does not parse: {e}"),
                    archived.frame_count(),
                );
            }
        };
        let db_frame: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return mismatch(
                    RetainReason::Undecodable,
                    format!("turns frame does not parse: {e}"),
                    archived.frame_count(),
                );
            }
        };
        if archived_frame != db_frame {
            return mismatch(
                RetainReason::FrameContentMismatch,
                "frame content differs from turns".to_string(),
                archived.frame_count(),
            );
        }
    }
    PruneConfirm::Confirmed
}

/// Whether `turns_pruned_at` is already set for this game — the
/// cross-instance duplicate signal. `BACKUP_SLOT` is per-process, so
/// another instance's prune can commit between this job's under-slot
/// re-check and its frame fetch, leaving 0 DB frames against the object's
/// N. Called only for a 0-frame count mismatch: a single-SELECT snapshot
/// means rows vanish all-or-nothing, so that count is exactly the
/// signature; the winner's committed marker settles it.
async fn pruned_elsewhere(db: &sqlx::PgPool, game_id: Uuid) -> cja::Result<bool> {
    let marker = sqlx::query_scalar!(
        r#"SELECT turns_pruned_at AS "pruned!" FROM games
           WHERE game_id = $1 AND turns_pruned_at IS NOT NULL"#,
        game_id
    )
    .fetch_optional(db)
    .await
    .wrap_err("Failed to re-check turns_pruned_at after count mismatch")?;
    Ok(marker.is_some())
}

/// Prune one archived native game's `turns` rows (its `snake_turns`
/// cascade via the FK) after confirming the archive object's content.
///
/// Ordering is `archive_native_game`'s (`native_backup.rs`): slot FIRST,
/// predicate re-check UNDER the slot. The under-slot re-check is the
/// in-instance dedup — a duplicate job serializes on the slot, sees the
/// winner's committed `turns_pruned_at`, and returns `Skipped` before any
/// GET, instead of comparing 0 DB frames against the object's N frames
/// and logging a false data-loss ERROR. The slot is per-process, so a
/// duplicate racing a DIFFERENT instance's prune is caught after the
/// confirm instead: a 0-frame count mismatch re-reads `turns_pruned_at`
/// and skips silently when the other instance already committed. The
/// permit rides into the confirm's `spawn_blocking` and back out, so a
/// dropped future cannot free the slot while the CPU work runs.
///
/// Never deletes without a content-confirmed object (fail-closed: the
/// turns are the only complete copy otherwise). The delete and the
/// `turns_pruned_at` mark commit in one transaction, so a crash can never
/// leave turns gone without the marker (or vice versa).
async fn prune_game_turns_with_lock_timeout(
    app_state: &AppState,
    game_id: Uuid,
    days: i32,
    lock_timeout: &str,
) -> cja::Result<PruneOutcome> {
    // 1. Slot FIRST — before the predicate re-check (dedup: a duplicate
    //    serializes here and sees the winner's committed turns_pruned_at;
    //    memory: only the slot holder holds frames; drain safety: the
    //    permit rides into spawn_blocking and back out).
    let permit = std::sync::Arc::clone(&crate::backup::BACKUP_SLOT)
        .acquire_owned()
        .await
        .wrap_err("Backup slot semaphore closed")?;

    // 2. Re-check the full prune predicate for this one game, UNDER the
    //    slot. gcs_path is non-NULL by the predicate, so "!" is safe.
    let Some(gcs_path) = sqlx::query_scalar!(
        r#"
        SELECT gcs_path AS "gcs_path!"
        FROM games
        WHERE game_id = $1
          AND engine_game_id IS NULL
          AND status IN ('finished', 'failed')
          AND archived_at IS NOT NULL
          AND archive_version = 2
          AND turns_pruned_at IS NULL
          AND gcs_path IS NOT NULL
          AND created_at < NOW() - make_interval(days => $2)
          AND NOT EXISTS (
              SELECT 1 FROM game_battlesnakes gb
              WHERE gb.game_id = games.game_id
                AND gb.created_at >= (SELECT scanned_through - INTERVAL '1 day'
                                      FROM customization_active_week_backfill_cursor
                                      WHERE singleton)
          )
        "#,
        game_id,
        days
    )
    .fetch_optional(&app_state.db)
    .await
    .wrap_err("Failed to fetch prune candidate")?
    else {
        tracing::debug!(%game_id,
            "Prune skipped: not eligible (duplicate job already pruned, not \
             terminal-native, too young, no object path, or cursor behind)");
        return Ok(PruneOutcome::Skipped);
    };

    // 3. What the archiver would write NOW, then the stored object.
    //    fetch_native_frame_texts is the writer's own selection, so the
    //    writer's and confirmer's frame selections can never drift.
    let db_texts = crate::native_backup::fetch_native_frame_texts(&app_state.db, game_id).await?;
    // A hung GET must not pin the backup slot: 10s cap, same as the
    // reader's fetch timeout. Timeout/storage error => Err => job retry.
    let compressed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app_state.object_store.get(&gcs_path),
    )
    .await
    .map_err(|_| cja::color_eyre::eyre::eyre!("Archive fetch timed out for game {game_id}"))??;
    let Some(compressed) = compressed else {
        tracing::error!(%game_id, path = %gcs_path,
            "Archive object missing; retaining turns — archive data loss");
        return Ok(PruneOutcome::Retained {
            reason: RetainReason::ObjectMissing,
        }); // never delete; hourly discovery re-logs this
    };

    // 4. Confirm content on the blocking pool; permit rides INTO the
    //    closure and back OUT.
    let (permit, verdict) = tokio::task::spawn_blocking(move || {
        let verdict = confirm_archive_content(game_id, &compressed, &db_texts);
        (permit, verdict)
    })
    .await
    .wrap_err("Prune confirm task panicked")?;
    match verdict {
        PruneConfirm::Confirmed => {}
        PruneConfirm::Mismatch {
            reason,
            detail,
            archived_frames,
            db_frames,
        } => {
            // Cross-instance duplicate (PR review r1): `BACKUP_SLOT` is
            // per-process, so another instance's prune can commit between
            // this job's under-slot re-check and its frame fetch — this job
            // then compares 0 DB frames against the object's N and would
            // log the false data-loss ERROR. `fetch_native_frame_texts`
            // is a single SELECT (one snapshot), so rows vanish
            // all-or-nothing and a 0-frame count mismatch is exactly that
            // signature; the winner's committed marker settles it. A game
            // with a genuinely stale object still has a NULL marker here
            // and falls through to the ERROR below.
            if reason == RetainReason::FrameCountMismatch
                && db_frames == 0
                && pruned_elsewhere(&app_state.db, game_id).await?
            {
                tracing::debug!(%game_id,
                    "Prune skipped: another instance pruned this game between \
                     the re-check and the frame fetch");
                return Ok(PruneOutcome::Skipped);
            }
            tracing::error!(
                %game_id, path = %gcs_path, reason = ?reason, detail = %detail,
                archived_frames, db_frames,
                "Archive content mismatch; retaining turns — clear archived_at/gcs_path/\
                 archive_version to force a re-archive"
            );
            return Ok(PruneOutcome::Retained { reason }); // fail closed
        }
    }

    // 5. Delete + mark in one transaction. Row locks only conflict with
    //    writes to the same game; pruned games are terminal, so no live
    //    runner inserts into them, and the confirm reads happened before
    //    the tx opened.
    let mut tx = app_state.db.begin().await?;

    // One statement per query! call. set_config(..., true) = SET LOCAL,
    // and unlike SET it accepts bind parameters. statement_timeout
    // applies per statement (not per transaction).
    sqlx::query!("SELECT set_config('lock_timeout', $1, true)", lock_timeout)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query!("SELECT set_config('statement_timeout', '60s', true)")
        .fetch_optional(&mut *tx)
        .await?;

    let deleted = sqlx::query!("DELETE FROM turns WHERE game_id = $1", game_id)
        .execute(&mut *tx)
        .await // snake_turns cascade via FK; fires watched_game_turn_delete
        // once (transition table + one reset NOTIFY)
        .wrap_err_with(|| format!("Failed to delete turns for game {game_id}"))?;
    sqlx::query!(
        "UPDATE games SET turns_pruned_at = NOW()
         WHERE game_id = $1 AND turns_pruned_at IS NULL",
        game_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    drop(permit);

    let turns_deleted = deleted.rows_affected();
    tracing::info!(%game_id, turns_deleted, "Pruned game turns");
    Ok(PruneOutcome::Pruned { turns_deleted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    /// Insert a games row with explicit timestamps (the updated_at trigger
    /// only fires on UPDATE, so old rows must set both in the INSERT).
    async fn fixture_game(
        pool: &sqlx::PgPool,
        status: &str,
        created_at: chrono::DateTime<chrono::Utc>,
        engine_game_id: Option<&str>,
    ) -> cja::Result<Uuid> {
        let id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO games (board_size, game_type, status, created_at, updated_at, engine_game_id)
               VALUES ('11x11', 'Standard', $1, $2, $2, $3) RETURNING game_id"#,
        )
        .bind(status)
        .bind(created_at)
        .bind(engine_game_id)
        .fetch_one(pool)
        .await?;
        Ok(id)
    }

    /// A user + battlesnake (unique name — `unique_battlesnake_name_per_user`
    /// per user, but unique across the test anyway) joined to the game with
    /// an explicit link created_at (the cursor guard probes it). Returns
    /// the game_battlesnake_id.
    async fn join_snake(
        pool: &sqlx::PgPool,
        game_id: Uuid,
        name: &str,
        link_created_at: chrono::DateTime<chrono::Utc>,
    ) -> cja::Result<Uuid> {
        let login = format!("prune-user-{}", Uuid::new_v4().simple());
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, '') RETURNING user_id",
        )
        .bind(Uuid::new_v4().as_u128() as i64)
        .bind(&login)
        .fetch_one(pool)
        .await?;
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, $2, 'http://snake', 'public') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .fetch_one(pool)
        .await?;
        let gb_id: Uuid = sqlx::query_scalar(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at)
             VALUES ($1, $2, $3) RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(snake_id)
        .bind(link_created_at)
        .fetch_one(pool)
        .await?;
        Ok(gb_id)
    }

    /// A persisted frame shaped like the engine's PascalCase output. The
    /// snake id is the game_battlesnake_id — the same key author fill and
    /// shout extraction read.
    fn frame(turn: i32, snake_id: &str) -> serde_json::Value {
        serde_json::json!({
            "Turn": turn,
            "Snakes": [{
                "ID": snake_id,
                "Name": "Prune Snek",
                "Body": [{"X": 5, "Y": 5}],
                "Health": 100,
                "Latency": "42",
                "Shout": "hello",
                "Death": null,
                "URL": "",
                "ProxyURL": "",
                "APIVersion": "1",
                "Author": "",
            }],
            "Food": [{"X": 1, "Y": 2}],
            "Hazards": [],
        })
    }

    /// Insert one turn with a frame; returns its turn_id.
    async fn fixture_turn(
        pool: &sqlx::PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame_data: serde_json::Value,
    ) -> cja::Result<Uuid> {
        let turn_id: Uuid = sqlx::query_scalar(
            "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3) RETURNING turn_id",
        )
        .bind(game_id)
        .bind(turn_number)
        .bind(frame_data)
        .fetch_one(pool)
        .await?;
        Ok(turn_id)
    }

    async fn turn_count(pool: &sqlx::PgPool, game_id: Uuid) -> cja::Result<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(pool)
            .await?;
        Ok(count)
    }

    async fn snake_turn_count(pool: &sqlx::PgPool, game_id: Uuid) -> cja::Result<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM snake_turns st
             JOIN turns t ON t.turn_id = st.turn_id
             JOIN game_battlesnakes gb ON gb.game_battlesnake_id = st.game_battlesnake_id
             WHERE gb.game_id = $1",
        )
        .bind(game_id)
        .fetch_one(pool)
        .await?;
        Ok(count)
    }

    async fn turns_pruned_at(
        pool: &sqlx::PgPool,
        game_id: Uuid,
    ) -> cja::Result<Option<chrono::DateTime<chrono::Utc>>> {
        let value = sqlx::query_scalar("SELECT turns_pruned_at FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(pool)
            .await?;
        Ok(value)
    }

    /// The shared fixture: an archived old native game with framed turns +
    /// snake_turns and a genuine v2 object written by the real archiver.
    /// The `game_battlesnakes` link is seeded at the game's created_at —
    /// the fresh-test-DB cursor is seeded to NOW(), so a NOW() link would
    /// silently block every prune via the cursor guard. Returns
    /// (game_id, game_battlesnake_id).
    async fn archived_old_native_game(
        state: &AppState,
        days_old: i64,
        turn_count: usize,
    ) -> cja::Result<(Uuid, Uuid)> {
        let pool = &state.db;
        let old = chrono::Utc::now() - chrono::Duration::days(days_old);
        let game_id = fixture_game(pool, "finished", old, None).await?;
        let gb_id = join_snake(pool, game_id, &format!("Snek {game_id}"), old).await?;
        for turn in 0..turn_count {
            let turn_id = fixture_turn(
                pool,
                game_id,
                turn as i32,
                frame(turn as i32, &gb_id.to_string()),
            )
            .await?;
            sqlx::query(
                "INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction, latency_ms, timed_out)
                 VALUES ($1, $2, 'up', 42, false)",
            )
            .bind(turn_id)
            .bind(gb_id)
            .execute(pool)
            .await?;
        }
        crate::native_backup::archive_native_game(state, game_id).await?;
        Ok((game_id, gb_id))
    }

    /// Overwrite the game's object with a hand-built one. `mutate` adjusts
    /// the frames array / header after copying the genuine DB frames.
    async fn rewrite_object(
        state: &AppState,
        game_id: Uuid,
        mutate: &dyn Fn(&mut serde_json::Value),
    ) -> cja::Result<String> {
        let pool = &state.db;
        let gcs_path: String = sqlx::query_scalar("SELECT gcs_path FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(pool)
            .await?;
        let rows: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT frame_data FROM turns
             WHERE game_id = $1 AND frame_data IS NOT NULL
             ORDER BY turn_number ASC",
        )
        .bind(game_id)
        .fetch_all(pool)
        .await?;
        let mut object = serde_json::json!({
            "archive_version": 2,
            "game_id": game_id.to_string(),
            "game": {
                "game_type": "Standard",
                "board_size": "11x11",
                "status": "finished",
                "created_at": chrono::Utc::now(),
                "finished_at": null,
            },
            "frames": rows,
            "exported_at": chrono::Utc::now(),
        });
        mutate(&mut object);
        let compressed = zstd::stream::encode_all(
            serde_json::to_vec(&object)?.as_slice(),
            crate::backup::ZSTD_COMPRESSION_LEVEL,
        )?;
        state.object_store.put(&gcs_path, compressed).await?;
        Ok(gcs_path)
    }

    /// game_ids of every queued PruneGameTurnsJob row, in insert order.
    async fn enqueued_prune_ids(pool: &sqlx::PgPool) -> cja::Result<Vec<String>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT payload->>'game_id' FROM jobs WHERE name = 'PruneGameTurnsJob'",
        )
        .fetch_all(pool)
        .await?;
        Ok(ids)
    }

    /// Set the active-week backfill cursor explicitly.
    async fn set_cursor(
        pool: &sqlx::PgPool,
        scanned_through: chrono::DateTime<chrono::Utc>,
    ) -> cja::Result<()> {
        sqlx::query(
            "UPDATE customization_active_week_backfill_cursor
             SET scanned_through = $1 WHERE singleton = TRUE",
        )
        .bind(scanned_through)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// SC: a prune deletes the game's turns, cascades its snake_turns,
    /// sets the marker, and never populates the replay cache.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_deletes_turns_and_cascades_snake_turns(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 3).await?;
        assert_eq!(turn_count(&pool, game_id).await?, 3);
        assert_eq!(snake_turn_count(&pool, game_id).await?, 3);

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(outcome, PruneOutcome::Pruned { turns_deleted: 3 });

        assert_eq!(turn_count(&pool, game_id).await?, 0);
        assert_eq!(snake_turn_count(&pool, game_id).await?, 0);
        assert!(turns_pruned_at(&pool, game_id).await?.is_some());
        // The confirm decoded via decode_archive_object directly, never
        // through the ArchiveReader cache.
        assert!(!state.archive.cached(game_id));
        Ok(())
    }

    /// SC: every control dimension skips — young, unarchived, imported
    /// (engine), live, and pathless — and none are discovered.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_skips_young_unarchived_imported_live_and_pathless(
        pool: PgPool,
    ) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let now = chrono::Utc::now();

        // (a) archived but only 29 days old (1 full day from the boundary).
        let young = fixture_game(&pool, "finished", now - chrono::Duration::days(29), None).await?;
        join_snake(&pool, young, "Young Snek", now - chrono::Duration::days(29)).await?;
        for turn in 0..2 {
            fixture_turn(&pool, young, turn, frame(turn, "any")).await?;
        }
        crate::native_backup::archive_native_game(&state, young).await?;

        // (b) 40 days old but never archived.
        let unarchived =
            fixture_game(&pool, "finished", now - chrono::Duration::days(40), None).await?;
        join_snake(
            &pool,
            unarchived,
            "Unarchived Snek",
            now - chrono::Duration::days(40),
        )
        .await?;
        fixture_turn(&pool, unarchived, 0, frame(0, "any")).await?;

        // (c) 40 days old, marked archived, but imported (engine id set) —
        // with turns rows, proving the native-only gate.
        let imported = fixture_game(
            &pool,
            "finished",
            now - chrono::Duration::days(40),
            Some("55555555-5555-5555-5555-555555555555"),
        )
        .await?;
        sqlx::query(
            "UPDATE games SET archived_at = NOW(), archive_version = 2,
             gcs_path = 'games/2026/09/01/x.json.zst' WHERE game_id = $1",
        )
        .bind(imported)
        .execute(&pool)
        .await?;
        fixture_turn(&pool, imported, 0, frame(0, "any")).await?;

        // (d) 40 days old, archived, but still running.
        let running =
            fixture_game(&pool, "running", now - chrono::Duration::days(40), None).await?;
        join_snake(
            &pool,
            running,
            "Running Snek",
            now - chrono::Duration::days(40),
        )
        .await?;
        sqlx::query(
            "UPDATE games SET archived_at = NOW(), archive_version = 2,
             gcs_path = 'native/2026/09/01/y.json.zst' WHERE game_id = $1",
        )
        .bind(running)
        .execute(&pool)
        .await?;

        // (e) 40 days old, archived v2, but gcs_path NULL — the filter
        // that makes the re-check's "gcs_path!" override safe.
        let pathless =
            fixture_game(&pool, "finished", now - chrono::Duration::days(40), None).await?;
        join_snake(
            &pool,
            pathless,
            "Pathless Snek",
            now - chrono::Duration::days(40),
        )
        .await?;
        sqlx::query("UPDATE games SET archived_at = NOW(), archive_version = 2 WHERE game_id = $1")
            .bind(pathless)
            .execute(&pool)
            .await?;

        for game_id in [young, unarchived, imported, running, pathless] {
            assert_eq!(
                prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?,
                PruneOutcome::Skipped,
                "game {game_id} must skip"
            );
            assert!(turns_pruned_at(&pool, game_id).await?.is_none());
        }

        run_discovery_with_cap(&state, 30, 100).await?;
        let ids = enqueued_prune_ids(&pool).await?;
        for game_id in [young, unarchived, imported, running, pathless] {
            assert!(
                !ids.contains(&game_id.to_string()),
                "discovery must skip {game_id}"
            );
        }
        // Turns retained where present.
        assert_eq!(turn_count(&pool, young).await?, 2);
        assert_eq!(turn_count(&pool, unarchived).await?, 1);
        assert_eq!(turn_count(&pool, imported).await?, 1);
        Ok(())
    }

    /// SC: a missing object retains the turns (archive data loss is
    /// fail-closed) with no marker.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_retains_turns_when_object_missing(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;
        // Point the row at a key with no object.
        sqlx::query(
            "UPDATE games SET gcs_path = 'native/2026/01/01/gone.json.zst' WHERE game_id = $1",
        )
        .bind(game_id)
        .execute(&pool)
        .await?;

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(
            outcome,
            PruneOutcome::Retained {
                reason: RetainReason::ObjectMissing
            }
        );
        assert_eq!(turn_count(&pool, game_id).await?, 2);
        assert!(turns_pruned_at(&pool, game_id).await?.is_none());
        Ok(())
    }

    /// SC: the sweeper race (game archived mid-run, more turns written
    /// after) leaves a stale object — the count mismatch retains ALL
    /// turns, including the new one.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_retains_turns_on_frame_count_mismatch(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, gb) = archived_old_native_game(&state, 40, 2).await?;
        // The straggler turn lands after the archive was written.
        fixture_turn(&pool, game_id, 2, frame(2, &gb.to_string())).await?;

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(
            outcome,
            PruneOutcome::Retained {
                reason: RetainReason::FrameCountMismatch
            }
        );
        assert_eq!(turn_count(&pool, game_id).await?, 3, "no turn is deleted");
        assert!(turns_pruned_at(&pool, game_id).await?.is_none());
        Ok(())
    }

    /// SC: equal counts with altered content still retains (a presence
    /// probe would have deleted the only complete copy).
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_retains_turns_on_content_mismatch_with_equal_counts(
        pool: PgPool,
    ) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;
        rewrite_object(&state, game_id, &|object| {
            object["frames"][0]["Turn"] = serde_json::json!(99);
        })
        .await?;

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(
            outcome,
            PruneOutcome::Retained {
                reason: RetainReason::FrameContentMismatch
            }
        );
        assert_eq!(turn_count(&pool, game_id).await?, 2);
        assert!(turns_pruned_at(&pool, game_id).await?.is_none());
        Ok(())
    }

    /// SC: an object carrying a different game's id is not this game's
    /// archive — retain.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_retains_turns_on_wrong_game_id_in_object(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;
        let other = Uuid::new_v4();
        rewrite_object(&state, game_id, &|object| {
            object["game_id"] = serde_json::json!(other.to_string());
        })
        .await?;

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(
            outcome,
            PruneOutcome::Retained {
                reason: RetainReason::WrongGameId
            }
        );
        assert_eq!(turn_count(&pool, game_id).await?, 2);
        Ok(())
    }

    /// SC: only archive_version 2 is confirmed — a v1 declaration retains.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_retains_turns_on_wrong_archive_version(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;
        rewrite_object(&state, game_id, &|object| {
            object["archive_version"] = serde_json::json!(1);
        })
        .await?;

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(
            outcome,
            PruneOutcome::Retained {
                reason: RetainReason::WrongVersion
            }
        );
        assert_eq!(turn_count(&pool, game_id).await?, 2);
        Ok(())
    }

    /// SC: an undecodable object is a decided Retained (an Ok outcome, not
    /// an Err — corruption never gets retried into a delete).
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_retains_turns_on_corrupt_object(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;
        let gcs_path: String = sqlx::query_scalar("SELECT gcs_path FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        let genuine = state
            .object_store
            .get(&gcs_path)
            .await?
            .expect("genuine object exists");
        // Truncated zstd: decodable header, fatal tail.
        let truncated = genuine[..genuine.len() / 2].to_vec();
        state.object_store.put(&gcs_path, truncated).await?;

        let outcome = prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?;
        assert_eq!(
            outcome,
            PruneOutcome::Retained {
                reason: RetainReason::Undecodable
            }
        );
        assert_eq!(turn_count(&pool, game_id).await?, 2);
        Ok(())
    }

    /// SC: retention is the configured days parameter — 31 days prunes at
    /// 30, 29 skips, and a 10-day-old game prunes once retention drops to
    /// 7.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_respects_retention_days(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (old_enough, _gb1) = archived_old_native_game(&state, 31, 1).await?;
        let (too_young, _gb2) = archived_old_native_game(&state, 29, 1).await?;

        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, old_enough, 30, "5s").await?,
            PruneOutcome::Pruned { turns_deleted: 1 }
        );
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, too_young, 30, "5s").await?,
            PruneOutcome::Skipped
        );

        let (medium, _gb3) = archived_old_native_game(&state, 10, 1).await?;
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, medium, 7, "5s").await?,
            PruneOutcome::Pruned { turns_deleted: 1 }
        );
        Ok(())
    }

    /// SC: the cursor guard blocks a prune whose link the backfill's
    /// 1-day rescan window would still reach, and unblocks once the
    /// cursor passes. Pins the 1-day margin, not just the cursor.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_blocked_while_backfill_cursor_rescan_would_reach_link(
        pool: PgPool,
    ) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let now = chrono::Utc::now();
        let (game_id, _gb) = archived_old_native_game(&state, 40, 1).await?;

        // Cursor at 39.5 days ago: threshold = scanned_through - 1 day =
        // 40.5 days ago; the 40-day link is inside [40.5d ago, 39.5d ago)
        // → the rescan would re-read it → guard blocks.
        set_cursor(
            &pool,
            now - chrono::Duration::minutes(39 * 24 * 60 + 12 * 60),
        )
        .await?;
        run_discovery_with_cap(&state, 30, 100).await?;
        assert!(
            !enqueued_prune_ids(&pool)
                .await?
                .contains(&game_id.to_string())
        );
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?,
            PruneOutcome::Skipped
        );

        // Cursor at 38 days ago: threshold 39 days ago; the 40-day link is
        // older than the whole rescan window → prune proceeds.
        set_cursor(&pool, now - chrono::Duration::days(38)).await?;
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?,
            PruneOutcome::Pruned { turns_deleted: 1 }
        );
        Ok(())
    }

    /// SC: discovery enqueues oldest-first within the cap, at the prune
    /// priority (below backup jobs, below game runners), and re-runs
    /// enqueue only what is still unpruned.
    #[sqlx::test(migrations = "../migrations")]
    async fn discovery_enqueues_oldest_first_within_cap_and_is_idempotent(
        pool: PgPool,
    ) -> cja::Result<()> {
        use cja::jobs::Job as _;

        let state = AppState::test_from_pool(pool.clone());
        let (oldest, _a) = archived_old_native_game(&state, 40, 1).await?;
        let (middle, _b) = archived_old_native_game(&state, 35, 1).await?;
        let (youngest, _c) = archived_old_native_game(&state, 30, 1).await?;

        run_discovery_with_cap(&state, 30, 2).await?;
        let ids = enqueued_prune_ids(&pool).await?;
        assert_eq!(ids, vec![oldest.to_string(), middle.to_string()]);
        assert!(!ids.contains(&youngest.to_string()));

        // Priorities: prune below backup (-10), runner keeps default 0.
        crate::jobs::NativeBackupSingleGameJob {
            game_id: Uuid::new_v4(),
        }
        .enqueue(
            state.clone(),
            "control".to_string(),
            Some(crate::backup::BACKUP_JOB_PRIORITY),
        )
        .await?;
        crate::jobs::GameRunnerJob {
            game_id: Uuid::new_v4(),
        }
        .enqueue(state.clone(), "control".to_string(), None)
        .await?;
        let rows: Vec<(String, i32)> = sqlx::query_as("SELECT name, priority FROM jobs")
            .fetch_all(&pool)
            .await?;
        let prune: Vec<i32> = rows
            .iter()
            .filter(|(name, _)| name == "PruneGameTurnsJob")
            .map(|(_, p)| *p)
            .collect();
        assert!(prune.iter().all(|&p| p == PRUNE_JOB_PRIORITY));
        const { assert!(PRUNE_JOB_PRIORITY < crate::backup::BACKUP_JOB_PRIORITY) };
        assert!(rows.contains(&("NativeBackupSingleGameJob".to_string(), -10)));
        assert!(rows.contains(&("GameRunnerJob".to_string(), 0)));

        // Prune both enqueued games; re-running discovery enqueues only
        // the third (the pruned games' existing job rows are not
        // duplicated — one row per game after both runs).
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, oldest, 30, "5s").await?,
            PruneOutcome::Pruned { turns_deleted: 1 }
        );
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, middle, 30, "5s").await?,
            PruneOutcome::Pruned { turns_deleted: 1 }
        );
        run_discovery_with_cap(&state, 30, 2).await?;
        let counts: std::collections::HashMap<String, i64> = sqlx::query_as(
            "SELECT payload->>'game_id' AS id, COUNT(*)::BIGINT AS n
                 FROM jobs WHERE name = 'PruneGameTurnsJob' GROUP BY 1",
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .collect();
        assert_eq!(
            counts.get(&oldest.to_string()),
            Some(&1),
            "pruned game not re-enqueued"
        );
        assert_eq!(
            counts.get(&middle.to_string()),
            Some(&1),
            "pruned game not re-enqueued"
        );
        assert_eq!(
            counts.get(&youngest.to_string()),
            Some(&1),
            "youngest now enqueued"
        );
        Ok(())
    }

    /// SC: a lock conflict errors (lock_timeout) instead of hanging, the
    /// transaction rolls back leaving turns intact, and a retry after the
    /// blocker releases succeeds.
    #[sqlx::test(migrations = "../migrations")]
    async fn prune_lock_timeout_errors_instead_of_hanging(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;

        // Hold row locks on the game's turns in an open external tx.
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT turn_id FROM turns WHERE game_id = $1 FOR UPDATE")
            .bind(game_id)
            .execute(&mut *blocker)
            .await?;

        let prune_state = state.clone();
        let prune = tokio::spawn(async move {
            prune_game_turns_with_lock_timeout(&prune_state, game_id, 30, "1s").await
        });

        // Deterministic: wait until the DELETE is actually blocked, then
        // assert on the outcome (never wall-clock).
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let blocked: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_stat_activity
                     WHERE datname = current_database() AND wait_event_type = 'Lock'",
                )
                .fetch_one(&pool)
                .await?;
                if blocked >= 1 {
                    break;
                }
            }
            cja::Result::<()>::Ok(())
        })
        .await??;

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), prune).await??;
        assert!(
            outcome.is_err(),
            "lock_timeout must surface as Err, got {outcome:?}"
        );
        // Rolled back: turns intact, marker unset.
        assert_eq!(turn_count(&pool, game_id).await?, 2);
        assert!(turns_pruned_at(&pool, game_id).await?.is_none());

        // Release and retry: succeeds.
        blocker.rollback().await?;
        assert_eq!(
            prune_game_turns_with_lock_timeout(&state, game_id, 30, "5s").await?,
            PruneOutcome::Pruned { turns_deleted: 2 }
        );
        Ok(())
    }

    /// SC: duplicate prune jobs are silent no-ops — the loser serializes
    /// on the slot, sees the winner's committed marker, and Skips (never
    /// a false data-loss Retained from comparing 0 DB frames against the
    /// object's N).
    #[sqlx::test(migrations = "../migrations")]
    async fn duplicate_prune_jobs_are_silent_noops(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let (game_id, _gb) = archived_old_native_game(&state, 40, 2).await?;

        // Hold the slot so neither prune can start, spawn both, settle,
        // then release — the winner prunes, the duplicate no-ops.
        let permit = std::sync::Arc::clone(&crate::backup::BACKUP_SLOT)
            .acquire_owned()
            .await
            .unwrap();
        let a = state.clone();
        let b = state.clone();
        let prune_a =
            tokio::spawn(
                async move { prune_game_turns_with_lock_timeout(&a, game_id, 30, "5s").await },
            );
        let prune_b =
            tokio::spawn(
                async move { prune_game_turns_with_lock_timeout(&b, game_id, 30, "5s").await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        drop(permit);

        let a = tokio::time::timeout(std::time::Duration::from_secs(30), prune_a).await???;
        let b = tokio::time::timeout(std::time::Duration::from_secs(30), prune_b).await???;
        let mut outcomes = vec![a, b];
        outcomes.sort_by_key(|o| match o {
            PruneOutcome::Pruned { .. } => 0,
            PruneOutcome::Skipped => 1,
            PruneOutcome::Retained { .. } => 2,
        });
        assert_eq!(
            outcomes,
            vec![
                PruneOutcome::Pruned { turns_deleted: 2 },
                PruneOutcome::Skipped
            ],
            "never Retained"
        );
        assert_eq!(turn_count(&pool, game_id).await?, 0);
        assert!(turns_pruned_at(&pool, game_id).await?.is_some());
        Ok(())
    }

    /// PR review r1: `BACKUP_SLOT` is per-process, so a duplicate racing
    /// ANOTHER instance's prune slips past the under-slot re-check and
    /// surfaces as a 0-frame count mismatch; the `pruned_elsewhere`
    /// marker re-read is what keeps that case a silent Skipped. The full
    /// interleaving needs two processes — this pins the re-read itself:
    /// marker set ⇒ true (skip), marker NULL ⇒ false (fail-closed ERROR
    /// path still runs).
    #[sqlx::test(migrations = "../migrations")]
    async fn count_mismatch_recheck_distinguishes_cross_instance_prune(
        pool: PgPool,
    ) -> cja::Result<()> {
        let old = chrono::Utc::now() - chrono::Duration::days(40);
        let winner = fixture_game(&pool, "finished", old, None).await?;
        sqlx::query("UPDATE games SET turns_pruned_at = NOW() WHERE game_id = $1")
            .bind(winner)
            .execute(&pool)
            .await?;
        let stale = fixture_game(&pool, "finished", old, None).await?;

        assert!(pruned_elsewhere(&pool, winner).await?);
        assert!(!pruned_elsewhere(&pool, stale).await?);
        Ok(())
    }
}
