//! Cron-driven resumable walk over engine `games` (DEV-1984), replacing the
//! wedged fork-join chain. See docs/game-archive.md.
//!
//! Every tick pages engine games oldest-first past a durable cursor and
//! trickles at most `PLAY_WALK_INFLIGHT_CEILING` walk-originated
//! `BackupSingleGameJob`s into the queue (below game-runner priority).
//! The cursor only advances past rows that are done (archived in arena or
//! in `play_archive_exceptions`), so an in-flight, retrying, or dropped
//! job's game can never be lost; the enqueue scan still continues through
//! the page so one bad row pins nothing but the cursor. As it passes, the
//! walk corrects imported rows' `created_at` (epoch artifacts) and
//! `game_type`. Once the backlog is gone, ticks are cheap no-op pages that
//! also catch anything the hourly job's 4-hour window misses — there is no
//! "done" state until DEV-1545 removes the walk.

use chrono::{DateTime, NaiveDateTime, Utc};
use color_eyre::eyre::Context as _;
use std::collections::{HashMap, HashSet};

use crate::backup::{BackupError, enqueue_backup_job};
use crate::engine_models::normalize_game_type;
use crate::state::AppState;

/// Walk-originated backup jobs allowed in flight (queued + running) at
/// once. A CODE CONSTANT by design (coordinator DEV-1984): env/Terraform
/// changes are outside this PR's authority; ramping is a one-line PR, and 0
/// pauses the walk (tick no-ops before any work). A claimed backup job
/// holds its worker while waiting on `BACKUP_SLOT` and prod has 100
/// workers, so this ceiling — not the enqueue count alone — is what keeps
/// at least 75 workers free for game runners. Ships at 5 so the first prod
/// run is a small batch; the coordinator ramps it after checking that
/// batch. Math at 25: 25 x 12 ticks/h = 300/h theoretical; the hourly
/// archive drains (~:21-:45) zero the budget for ~25-30 min/h -> ~150-200/h
/// realistic during the backlog.
pub(crate) const PLAY_WALK_INFLIGHT_CEILING: i64 = 5;
/// Engine rows examined per tick — far above the ceiling so done rows
/// advance the cursor well past the enqueue frontier each tick.
const PLAY_WALK_PAGE_SIZE: i64 = 2_000;
/// The walk only looks at games older than this many hours. The cursor can
/// then never pass a game younger than 24h, so any game that flips terminal
/// within 24h of creation is guaranteed to be seen later — closing the
/// hourly job's 4-hour-window gap without extra machinery.
const PLAY_WALK_MIN_GAME_AGE_HOURS: i64 = 24;
/// `created_at` values strictly before 1971-01-01T00:00:00Z (unix second
/// 31,536,000) are the 1970 epoch artifact of a missing JSON `Created`
/// key. Real data starts 2019-11. The guard keeps ~211k already-correct
/// rows (whose JSON `Created` differs from the column by milliseconds)
/// untouched — and their trigger-bumped `updated_at` with them.
const EPOCH_ARTIFACT_CUTOFF_UNIX_SECS: i64 = 31_536_000;

fn is_epoch_artifact(created_at: DateTime<Utc>) -> bool {
    created_at.timestamp() < EPOCH_ARTIFACT_CUTOFF_UNIX_SECS
}

/// What one tick did — returned so tests can assert without scraping logs.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct WalkTickStats {
    pub(crate) page_len: usize,
    pub(crate) enqueued: usize,
    pub(crate) corrected: usize,
    pub(crate) cursor_advanced: bool,
    /// The first not-done row had an in-flight job: the cursor is pinned.
    pub(crate) pinned_by_pending: Option<PinnedJob>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PinnedJob {
    pub(crate) engine_game_id: String,
    pub(crate) error_count: i32,
    pub(crate) last_error_message: Option<String>,
}

#[derive(sqlx::FromRow)]
struct WalkPageRow {
    id: String,
    created: NaiveDateTime,
    raw_game_type: String,
}

pub(crate) async fn run_walk_tick(app_state: &AppState) -> Result<(), BackupError> {
    run_walk_tick_with(app_state, PLAY_WALK_INFLIGHT_CEILING, PLAY_WALK_PAGE_SIZE).await?;
    Ok(())
}

/// One walk tick. `ceiling` and `page_size` are parameters so tests can
/// exercise the tick without depending on the constants (the ramp PR must
/// stay one line). The test seam mirrors native_backup's
/// `run_discovery_with_caps`.
pub(crate) async fn run_walk_tick_with(
    app_state: &AppState,
    ceiling: i64,
    page_size: i64,
) -> Result<WalkTickStats, BackupError> {
    // Pause lever: 0 skips the whole tick, including corrections.
    if ceiling == 0 {
        return Ok(WalkTickStats::default());
    }

    let engine_db = match &app_state.engine_db {
        Some(db) => db,
        None => {
            tracing::warn!("Engine database not configured, skipping play history walk");
            return Ok(WalkTickStats::default());
        }
    };
    if !app_state.object_store.is_configured() {
        tracing::warn!("GCS bucket not configured, skipping play history walk");
        return Ok(WalkTickStats::default());
    }

    // Budget = ceiling minus walk-family backup jobs already pending
    // (queued + running; cja DELETEs finished rows). Counts by NAME only —
    // `PruneGameTurnsJob` legitimately holds ~2,000 queued rows/hour and
    // must never be read as backfill backlog.
    let pending_backup_jobs: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM jobs
           WHERE name IN ('BackupSingleGameJob', 'NativeBackupSingleGameJob')"#
    )
    .fetch_one(&app_state.db)
    .await
    .wrap_err("Failed to count pending backup jobs")?;
    let mut budget = (ceiling - pending_backup_jobs).max(0);
    if budget == 0 {
        // A silent no-op path is how the matchmaker froze invisibly for
        // days — say it.
        tracing::info!(
            pending = pending_backup_jobs,
            ceiling,
            "Play history walk budget is zero this tick; enqueueing nothing"
        );
    }

    let cursor = sqlx::query!(
        r#"
        SELECT last_created, last_id
        FROM play_history_walk_cursor
        WHERE singleton
        "#
    )
    .fetch_one(&app_state.db)
    .await
    .wrap_err("Failed to read play history walk cursor")?;

    // Runtime query on the engine pool (engine schema is unknown to the
    // arena macros); same shape as the hourly discovery's engine fetch.
    let max_created =
        (Utc::now() - chrono::Duration::hours(PLAY_WALK_MIN_GAME_AGE_HOURS)).naive_utc();
    let page: Vec<WalkPageRow> = sqlx::query_as(
        r#"
        SELECT id, created,
               COALESCE(value->>'RulesetName', value->'Ruleset'->>'name', 'standard') AS raw_game_type
        FROM games
        WHERE value->>'Status' IN ('complete', 'error')
          AND (created, id) > ($1, $2)
          AND created < $3
        ORDER BY created ASC, id ASC
        LIMIT $4
        "#,
    )
    .bind(cursor.last_created)
    .bind(&cursor.last_id)
    .bind(max_created)
    .bind(page_size)
    .fetch_all(engine_db)
    .await
    .wrap_err("Failed to fetch play history walk page")?;

    if page.is_empty() {
        tracing::info!("Play history walk caught up: empty page");
        return Ok(WalkTickStats::default());
    }

    let page_ids: Vec<String> = page.iter().map(|row| row.id.clone()).collect();

    // Batch arena lookups for the page. `game_type` is bound as String on
    // purpose: `GameType` has catch-all variants and can't derive
    // `sqlx::Type`, and the column stores display strings ('Standard',
    // 'Solo'), which is exactly what `normalize_game_type` produces.
    let archived_rows = sqlx::query!(
        r#"
        SELECT engine_game_id AS "engine_game_id!", created_at, game_type
        FROM games
        WHERE engine_game_id = ANY($1) AND archived_at IS NOT NULL
        "#,
        &page_ids
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to batch-check archived games")?;
    let archived: HashMap<String, (DateTime<Utc>, String)> = archived_rows
        .into_iter()
        .map(|row| (row.engine_game_id, (row.created_at, row.game_type)))
        .collect();

    let exception_rows = sqlx::query!(
        r#"SELECT engine_game_id AS "engine_game_id!" FROM play_archive_exceptions WHERE engine_game_id = ANY($1)"#,
        &page_ids
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to batch-check archive exceptions")?;
    let exceptions: HashSet<String> = exception_rows
        .into_iter()
        .map(|row| row.engine_game_id)
        .collect();

    // Pending BackupSingleGameJob rows for the page ids: per-game dedupe
    // for enqueues, and the WARN signal for a pinned cursor.
    let pending_rows = sqlx::query!(
        r#"
        SELECT payload->>'engine_game_id' AS "engine_game_id!",
               MAX(error_count) AS "error_count!",
               MAX(last_error_message) AS last_error_message
        FROM jobs
        WHERE name = 'BackupSingleGameJob'
          AND payload->>'engine_game_id' = ANY($1)
        GROUP BY payload->>'engine_game_id'
        "#,
        &page_ids
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("Failed to batch-check pending backup jobs")?;
    let pending: HashMap<String, (i32, Option<String>)> = pending_rows
        .into_iter()
        .map(|row| {
            (
                row.engine_game_id,
                (row.error_count, row.last_error_message),
            )
        })
        .collect();

    let mut frontier_open = true;
    let mut new_cursor: Option<(NaiveDateTime, String)> = None;
    let mut corrections: Vec<(String, DateTime<Utc>, String)> = Vec::new();
    let mut pinned_by_pending: Option<PinnedJob> = None;
    let mut enqueued = 0usize;

    for row in &page {
        let engine_created = row.created.and_utc();
        let normalized_type = normalize_game_type(&row.raw_game_type);

        if let Some((row_created_at, row_game_type)) = archived.get(&row.id) {
            // Done. Correct only epoch-artifact dates and wrong types —
            // not the ~211k rows whose JSON Created differs by
            // milliseconds.
            let date_is_epoch_artifact =
                is_epoch_artifact(*row_created_at) && *row_created_at != engine_created;
            let type_differs = row_game_type != &normalized_type;
            if date_is_epoch_artifact || type_differs {
                corrections.push((row.id.clone(), engine_created, normalized_type.clone()));
            }
            if frontier_open {
                new_cursor = Some((row.created, row.id.clone()));
            }
        } else if exceptions.contains(&row.id) {
            // Done (explained exception), no comparison.
            if frontier_open {
                new_cursor = Some((row.created, row.id.clone()));
            }
        } else {
            // Not done: the cursor stops here for this tick, but the scan
            // continues enqueuing other missing games below.
            if frontier_open {
                frontier_open = false;
                if let Some((error_count, last_error_message)) = pending.get(&row.id) {
                    pinned_by_pending = Some(PinnedJob {
                        engine_game_id: row.id.clone(),
                        error_count: *error_count,
                        last_error_message: last_error_message.clone(),
                    });
                }
            }
            if !pending.contains_key(&row.id) && budget > 0 {
                enqueue_backup_job(app_state, &row.id).await?;
                budget -= 1;
                enqueued += 1;
            }
        }
    }

    // Req 5: correct epoch-dated rows' created_at and wrong game_type ONLY
    // — never gcs_path, never the objects. The epoch guard in both the
    // CASE and the WHERE keeps already-correct rows (and their
    // trigger-bumped updated_at) untouched.
    if !corrections.is_empty() {
        let ids: Vec<String> = corrections.iter().map(|(id, _, _)| id.clone()).collect();
        let dates: Vec<DateTime<Utc>> =
            corrections.iter().map(|(_, created, _)| *created).collect();
        let types: Vec<String> = corrections
            .iter()
            .map(|(_, _, game_type)| game_type.clone())
            .collect();
        sqlx::query!(
            r#"
            UPDATE games g
            SET created_at = CASE
                    WHEN g.created_at < TIMESTAMPTZ '1971-01-01' THEN v.created_at
                    ELSE g.created_at
                END,
                game_type = v.game_type
            FROM unnest(CAST($1 AS text[]), CAST($2 AS timestamptz[]), CAST($3 AS text[]))
                 AS v(engine_game_id, created_at, game_type)
            WHERE g.engine_game_id = v.engine_game_id
              AND (
                    (g.created_at < TIMESTAMPTZ '1971-01-01' AND g.created_at <> v.created_at)
                 OR g.game_type <> v.game_type
              )
            "#,
            &ids,
            &dates,
            &types
        )
        .execute(&app_state.db)
        .await
        .wrap_err("Failed to correct imported game rows")?;
    }

    // Monotonic advance: the row comparison can never move the cursor
    // backward, so duplicate/concurrent ticks are safe.
    let mut cursor_advanced = false;
    if let Some((last_created, last_id)) = new_cursor {
        cursor_advanced = true;
        sqlx::query!(
            r#"
            UPDATE play_history_walk_cursor
            SET last_created = $1, last_id = $2, updated_at = NOW()
            WHERE singleton AND (last_created, last_id) < ($1, $2)
            "#,
            last_created,
            last_id
        )
        .execute(&app_state.db)
        .await
        .wrap_err("Failed to advance play history walk cursor")?;
    }

    let stats = WalkTickStats {
        page_len: page.len(),
        enqueued,
        corrected: corrections.len(),
        cursor_advanced,
        pinned_by_pending: pinned_by_pending.clone(),
    };

    // Stall signal: the cursor is pinned by an in-flight job. error_count
    // 0 means merely slow; > 0 means failing (cja dead-letters after ~20
    // retries, and a dead-lettered job's row leaves `jobs`, re-arming the
    // next tick).
    if let Some(pin) = pinned_by_pending.filter(|_| !stats.cursor_advanced) {
        tracing::warn!(
            game_id = %pin.engine_game_id,
            error_count = pin.error_count,
            last_error_message = ?pin.last_error_message,
            budget,
            "Play history walk cursor pinned by in-flight job"
        );
    }

    tracing::info!(
        enqueued = stats.enqueued,
        corrected = stats.corrected,
        page_len = stats.page_len,
        cursor_advanced = stats.cursor_advanced,
        budget,
        "Play history walk tick"
    );

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::test_support::{engine_fixture, insert_engine_frame, insert_engine_game};
    use crate::jobs::BackupSingleGameJob;
    use crate::object_store::ObjectStore as _;
    use cja::jobs::Job as _;

    fn engine_value(
        id: &str,
        ruleset_name: Option<&str>,
        ruleset_name_key: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "ID": id,
            "Status": "complete",
            "Width": 11,
            "Height": 11,
            "Ruleset": {"name": ruleset_name_key},
            "Source": null,
            "RulesetName": ruleset_name,
            "RulesStages": null,
            "Map": "standard",
        })
    }

    fn frame(turn: i32) -> serde_json::Value {
        serde_json::json!({"Turn": turn, "Snakes": [], "Food": []})
    }

    /// A dated, parseable engine game with one frame.
    async fn seed_engine_game(
        engine_pool: &sqlx::PgPool,
        id: &str,
        created: chrono::NaiveDateTime,
        ruleset_name: Option<&str>,
    ) -> cja::Result<()> {
        insert_engine_game(
            engine_pool,
            id,
            engine_value(id, ruleset_name, "solo"),
            created,
        )
        .await?;
        insert_engine_frame(engine_pool, id, 0, Some(frame(0))).await?;
        Ok(())
    }

    async fn read_cursor(pool: &sqlx::PgPool) -> cja::Result<(NaiveDateTime, String)> {
        let row = sqlx::query!(
            "SELECT last_created, last_id FROM play_history_walk_cursor WHERE singleton"
        )
        .fetch_one(pool)
        .await?;
        Ok((row.last_created, row.last_id))
    }

    async fn job_ids_for(
        pool: &sqlx::PgPool,
        engine_game_id: &str,
    ) -> cja::Result<Vec<uuid::Uuid>> {
        Ok(sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT job_id FROM jobs WHERE name = 'BackupSingleGameJob' AND payload->>'engine_game_id' = $1",
        )
        .bind(engine_game_id)
        .fetch_all(pool)
        .await?)
    }

    /// BLOCKER pin: the seeded cursor is finite and readable through the
    /// real decode path. With a '-infinity' seed this test dies with the
    /// chrono overflow panic (which would kill the whole process in prod —
    /// cja has no catch_unwind).
    #[sqlx::test(migrations = "../migrations")]
    async fn seeded_cursor_is_finite_and_readable(pool: sqlx::PgPool) -> cja::Result<()> {
        let (created, id) = read_cursor(&pool).await?;
        assert_eq!(
            (created, id.as_str()),
            (
                chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                    .expect("date")
                    .and_hms_opt(0, 0, 0)
                    .expect("time"),
                ""
            )
        );
        // A tick against an empty engine schema completes without panicking.
        let (state, _store, _engine_pool) = engine_fixture(&pool).await?;
        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats, WalkTickStats::default());
        assert_eq!(read_cursor(&pool).await?, (created, id));
        Ok(())
    }

    /// The core walk behavior: enqueue missing games (scan continues past
    /// the frontier), cursor freezes at the first not-done row, pending
    /// dedupe, cursor advances only past done rows, and an empty page is a
    /// no-op.
    #[sqlx::test(migrations = "../migrations")]
    async fn tick_enqueues_missing_scan_continues_and_cursor_freezes_at_frontier(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        let base = chrono::NaiveDate::from_ymd_opt(2020, 1, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        let created = |hours: i32| base + chrono::Duration::hours(i64::from(hours));

        // A (missing), B (missing), C (archived), D (missing), in that order.
        seed_engine_game(&engine_pool, "A", created(0), Some("standard")).await?;
        seed_engine_game(&engine_pool, "B", created(1), Some("standard")).await?;
        seed_engine_game(&engine_pool, "C", created(2), Some("standard")).await?;
        seed_engine_game(&engine_pool, "D", created(3), Some("standard")).await?;
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', 'C', NOW(), 'games/2020/01/01/C.json.zst', 1)
            "#
        )
        .execute(&pool)
        .await?;
        let cursor_before = read_cursor(&pool).await?;

        // Tick with ceiling 2: A and B get jobs (scan continues); D gets
        // nothing (budget); cursor unchanged (A not done).
        let stats = run_walk_tick_with(&state, 2, 2_000).await?;
        assert_eq!(stats.enqueued, 2);
        assert!(!stats.cursor_advanced);
        assert_eq!(read_cursor(&pool).await?, cursor_before);
        assert_eq!(job_ids_for(&pool, "A").await?.len(), 1);
        assert_eq!(job_ids_for(&pool, "B").await?.len(), 1);
        assert_eq!(job_ids_for(&pool, "D").await?.len(), 0, "budget exhausted");

        // Pending dedupe control: re-tick before running anything — no new
        // jobs, cursor still pinned.
        let stats = run_walk_tick_with(&state, 2, 2_000).await?;
        assert_eq!(stats.enqueued, 0);
        assert!(!stats.cursor_advanced);
        assert_eq!(job_ids_for(&pool, "A").await?.len(), 1);

        // Run A's and B's jobs, then clear their queue rows (cja deletes
        // finished rows; direct .run() does not, and their pending rows
        // would keep consuming the ceiling's budget).
        BackupSingleGameJob {
            engine_game_id: "A".to_string(),
        }
        .run(state.clone())
        .await?;
        BackupSingleGameJob {
            engine_game_id: "B".to_string(),
        }
        .run(state.clone())
        .await?;
        sqlx::query!("DELETE FROM jobs WHERE name = 'BackupSingleGameJob'")
            .execute(&pool)
            .await?;

        // Tick 2: cursor advances to C only (D not done — the cursor must
        // NOT reach D), and D now gets a job.
        let stats = run_walk_tick_with(&state, 2, 2_000).await?;
        assert!(stats.cursor_advanced);
        assert_eq!(stats.enqueued, 1);
        assert_eq!(read_cursor(&pool).await?, (created(2), "C".to_string()));

        // Run D; tick 3 passes D; tick 4 is an empty-page no-op.
        BackupSingleGameJob {
            engine_game_id: "D".to_string(),
        }
        .run(state.clone())
        .await?;
        sqlx::query!("DELETE FROM jobs WHERE name = 'BackupSingleGameJob'")
            .execute(&pool)
            .await?;
        let stats = run_walk_tick_with(&state, 2, 2_000).await?;
        assert!(stats.cursor_advanced);
        assert_eq!(stats.enqueued, 0);
        assert_eq!(read_cursor(&pool).await?, (created(3), "D".to_string()));
        let stats = run_walk_tick_with(&state, 2, 2_000).await?;
        assert_eq!(stats.page_len, 0);
        Ok(())
    }

    /// A dropped (dead-lettered) job's row leaves `jobs`, so the next tick
    /// sees not-done + not-pending and re-enqueues.
    #[sqlx::test(migrations = "../migrations")]
    async fn cursor_does_not_pass_a_dropped_job(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        let base = chrono::NaiveDate::from_ymd_opt(2020, 2, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        seed_engine_game(&engine_pool, "A", base, Some("standard")).await?;

        run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(job_ids_for(&pool, "A").await?.len(), 1);

        // Simulate dead-letter removal: the job row disappears.
        sqlx::query!("DELETE FROM jobs WHERE name = 'BackupSingleGameJob'")
            .execute(&pool)
            .await?;

        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats.enqueued, 1, "dropped job is re-enqueued");
        assert!(!stats.cursor_advanced);
        Ok(())
    }

    /// A retrying transient failure pins the cursor, records NO exception
    /// (req 2 — no transient-to-permanent conversion), and the scan still
    /// enqueues other missing games. The WARN signal fires with the
    /// error_count.
    #[sqlx::test(migrations = "../migrations")]
    async fn failing_transient_job_pins_cursor_but_records_no_exception_and_scan_continues(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        let base = chrono::NaiveDate::from_ymd_opt(2020, 3, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        seed_engine_game(&engine_pool, "A", base, Some("standard")).await?;
        seed_engine_game(
            &engine_pool,
            "B",
            base + chrono::Duration::hours(1),
            Some("standard"),
        )
        .await?;

        // A has a pending, failing job (error_count 5).
        sqlx::query!(
            r#"
            INSERT INTO jobs (job_id, name, payload, priority, context, error_count, last_error_message)
            VALUES (gen_random_uuid(), 'BackupSingleGameJob', $1::jsonb, -10, 'fake', 5, 'engine exploded')
            "#,
            serde_json::json!({"engine_game_id": "A"})
        )
        .execute(&pool)
        .await?;

        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats.enqueued, 1, "B is enqueued");
        assert!(!stats.cursor_advanced, "A pins the cursor");
        assert_eq!(
            stats.pinned_by_pending,
            Some(PinnedJob {
                engine_game_id: "A".to_string(),
                error_count: 5,
                last_error_message: Some("engine exploded".to_string()),
            })
        );

        // No exception row: a transient failure is never permanent.
        let exceptions =
            sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM play_archive_exceptions"#)
                .fetch_one(&pool)
                .await?;
        assert_eq!(exceptions, 0);

        // Control: A is not re-enqueued (it is pending).
        assert_eq!(job_ids_for(&pool, "A").await?.len(), 1);
        Ok(())
    }

    /// Req 5: the walk corrects a 1970-dated imported row (created_at +
    /// game_type only; gcs_path and objects untouched, no put). Controls:
    /// (a) an archived row 6 ms off the engine column is NOT touched (the
    /// near-miss pin — ~211k prod rows have this shape); (b) an
    /// already-correct row keeps its updated_at.
    #[sqlx::test(migrations = "../migrations")]
    async fn tick_corrects_epoch_dated_row_leaving_gcs_path(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 3, 10)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");

        // X: the 1970 artifact. Engine value has Ruleset.name = solo and no
        // RulesetName — stored game_type would be wrong ("Standard") too.
        seed_engine_game(&engine_pool, "X", created, None).await?;
        let x_path = "games/1970/01/01/X.json.zst";
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', 'X', '1970-01-01T00:00:00Z', NOW(), $1, 1)
            "#,
            x_path
        )
        .execute(&pool)
        .await?;
        store.put(x_path, vec![1, 2, 3]).await?;
        let puts_before = store.put_count();

        // Near-miss: 6 ms off the engine column — must NOT be touched.
        let near_miss_created = created.and_utc() + chrono::Duration::milliseconds(6);
        seed_engine_game(&engine_pool, "near-miss", created, Some("standard")).await?;
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', 'near-miss', $1, NOW(), 'games/2020/03/10/near-miss.json.zst', 1)
            "#,
            near_miss_created
        )
        .execute(&pool)
        .await?;

        // Already-correct: same values the walk would write.
        seed_engine_game(&engine_pool, "correct-row", created, Some("standard")).await?;
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', 'correct-row', $1, NOW(), 'games/2020/03/10/correct-row.json.zst', 1)
            "#,
            created.and_utc()
        )
        .execute(&pool)
        .await?;

        let updated_at_before: Vec<(String, chrono::DateTime<Utc>)> = sqlx::query_as(
            "SELECT engine_game_id, updated_at FROM games WHERE engine_game_id = ANY($1)",
        )
        .bind(vec!["near-miss", "correct-row"])
        .fetch_all(&pool)
        .await?;

        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats.corrected, 1, "only the epoch artifact is corrected");
        assert_eq!(
            store.put_count() - puts_before,
            0,
            "corrections never put objects"
        );

        let x = sqlx::query!(
            "SELECT created_at, game_type, gcs_path FROM games WHERE engine_game_id = 'X'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(x.created_at, created.and_utc());
        assert_eq!(
            x.game_type, "Solo",
            "Ruleset.name fallback corrects the type"
        );
        assert_eq!(x.gcs_path.as_deref(), Some(x_path));

        // Controls: near-miss and already-correct rows untouched, including
        // their trigger-bumped updated_at.
        for (engine_game_id, updated_at) in updated_at_before {
            let row = sqlx::query!(
                "SELECT created_at, game_type, updated_at FROM games WHERE engine_game_id = $1",
                engine_game_id
            )
            .fetch_one(&pool)
            .await?;
            let expected_created = if engine_game_id == "near-miss" {
                near_miss_created
            } else {
                created.and_utc()
            };
            assert_eq!(row.created_at, expected_created);
            assert_eq!(row.game_type, "Standard");
            assert_eq!(row.updated_at, updated_at, "{engine_game_id} untouched");
        }
        Ok(())
    }

    /// An exceptions row counts as done: the cursor passes it and no job is
    /// enqueued. Control: without the row, the game gets a job.
    #[sqlx::test(migrations = "../migrations")]
    async fn tick_treats_exceptions_as_done(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 4, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");
        seed_engine_game(&engine_pool, "X", created, Some("standard")).await?;

        sqlx::query!(
            "INSERT INTO play_archive_exceptions (engine_game_id, reason) VALUES ('X', 'no_frames')"
        )
        .execute(&pool)
        .await?;

        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats.enqueued, 0);
        assert!(stats.cursor_advanced, "the cursor passes an excepted game");
        assert_eq!(read_cursor(&pool).await?, (created, "X".to_string()));

        // Control: without the exception row, X gets a job. Re-arm by
        // deleting the row and rewinding the cursor.
        sqlx::query!("DELETE FROM play_archive_exceptions WHERE engine_game_id = 'X'")
            .execute(&pool)
            .await?;
        sqlx::query!(
            r#"UPDATE play_history_walk_cursor SET last_created = $1, last_id = $2"#,
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .expect("date")
                .and_hms_opt(0, 0, 0)
                .expect("time"),
            ""
        )
        .execute(&pool)
        .await?;
        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats.enqueued, 1);
        Ok(())
    }

    /// With a drain backlog present (more pending backup jobs than the
    /// ceiling), the tick enqueues nothing — but still advances the cursor
    /// past done rows and applies corrections. Control: with only 10 fake
    /// rows against a ceiling of 25, exactly 15 new jobs go out for 17
    /// missing games.
    #[sqlx::test(migrations = "../migrations")]
    async fn tick_enqueues_nothing_while_drain_backlog_present(
        pool: sqlx::PgPool,
    ) -> cja::Result<()> {
        let (state, store, engine_pool) = engine_fixture(&pool).await?;
        let created = chrono::NaiveDate::from_ymd_opt(2020, 5, 1)
            .expect("date")
            .and_hms_opt(0, 0, 0)
            .expect("time");

        // One archived epoch-dated row (correction must still apply).
        seed_engine_game(&engine_pool, "arch-1970", created, Some("standard")).await?;
        sqlx::query!(
            r#"
            INSERT INTO games (board_size, game_type, status, engine_game_id, created_at, archived_at, gcs_path, archive_version)
            VALUES ('11x11', 'Standard', 'finished', 'arch-1970', '1970-01-01T00:00:00Z', NOW(), 'games/1970/01/01/arch-1970.json.zst', 1)
            "#
        )
        .execute(&pool)
        .await?;
        // One missing game (so a tick would otherwise enqueue).
        seed_engine_game(
            &engine_pool,
            "missing-1",
            created + chrono::Duration::hours(1),
            Some("standard"),
        )
        .await?;

        // 30 fake pending BackupSingleGameJob rows: every NOT NULL column
        // satisfied (job_id, name, payload, priority, context).
        for number in 0..30 {
            sqlx::query!(
                r#"
                INSERT INTO jobs (job_id, name, payload, priority, context)
                VALUES (gen_random_uuid(), 'BackupSingleGameJob', $1::jsonb, -10, 'fake drain backlog')
                "#,
                serde_json::json!({"engine_game_id": format!("fake-{number}")})
            )
            .execute(&pool)
            .await?;
        }

        let stats = run_walk_tick_with(&state, 25, 2_000).await?;
        assert_eq!(stats.enqueued, 0, "drain backlog zeroes the budget");
        assert_eq!(stats.corrected, 1, "corrections still apply");
        assert!(
            stats.cursor_advanced,
            "cursor still advances past done rows"
        );
        assert_eq!(store.put_count(), 0);
        assert_eq!(
            job_ids_for(&pool, "missing-1").await?.len(),
            0,
            "no new job while the budget is zero"
        );

        // Control: 10 fake rows against a ceiling of 25 -> exactly 15
        // enqueues for the 17 now-missing games. Clear the backlog and the
        // earlier fixtures, rewind the cursor, seed 17 gaps.
        sqlx::query!("DELETE FROM jobs WHERE context LIKE 'fake%'")
            .execute(&pool)
            .await?;
        sqlx::query!("DELETE FROM jobs WHERE name = 'BackupSingleGameJob'")
            .execute(&pool)
            .await?;
        sqlx::raw_sql(
            "DELETE FROM game_frames WHERE id = 'missing-1'; DELETE FROM games WHERE id = 'missing-1';",
        )
        .execute(&engine_pool)
        .await?;
        sqlx::query!(
            r#"UPDATE play_history_walk_cursor SET last_created = $1, last_id = $2"#,
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .expect("date")
                .and_hms_opt(0, 0, 0)
                .expect("time"),
            ""
        )
        .execute(&pool)
        .await?;
        for number in 0..17 {
            seed_engine_game(
                &engine_pool,
                &format!("gap-{number:02}"),
                created + chrono::Duration::hours(i64::from(number)),
                Some("standard"),
            )
            .await?;
        }
        for number in 0..10 {
            sqlx::query!(
                r#"
                INSERT INTO jobs (job_id, name, payload, priority, context)
                VALUES (gen_random_uuid(), 'BackupSingleGameJob', $1::jsonb, -10, 'fake')
                "#,
                serde_json::json!({"engine_game_id": format!("ctrl-{number}")})
            )
            .execute(&pool)
            .await?;
        }
        let stats = run_walk_tick_with(&state, 25, 2_000).await?;
        assert_eq!(stats.enqueued, 15, "budget = ceiling - pending");
        Ok(())
    }

    /// The 24h floor: a terminal game created NOW is invisible to the walk
    /// (the cursor cannot pass it); one created 25h ago is walked.
    #[sqlx::test(migrations = "../migrations")]
    async fn young_games_are_not_walked_yet(pool: sqlx::PgPool) -> cja::Result<()> {
        let (state, _store, engine_pool) = engine_fixture(&pool).await?;
        let now = Utc::now().naive_utc();

        seed_engine_game(&engine_pool, "young", now, Some("standard")).await?;
        seed_engine_game(
            &engine_pool,
            "old-enough",
            now - chrono::Duration::hours(25),
            Some("standard"),
        )
        .await?;

        let stats = run_walk_tick_with(&state, 5, 2_000).await?;
        assert_eq!(stats.page_len, 1, "only the 25h-old game is in the page");
        assert_eq!(stats.enqueued, 1);
        assert_eq!(job_ids_for(&pool, "young").await?.len(), 0);
        assert_eq!(job_ids_for(&pool, "old-enough").await?.len(), 1);

        // The cursor cannot pass the young game: it is the only in-page
        // game and is not done, so the cursor stays frozen at the seed.
        let (cursor_created, cursor_id) = read_cursor(&pool).await?;
        assert_eq!(cursor_id, "");
        assert_eq!(
            cursor_created,
            chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
                .expect("date")
                .and_hms_opt(0, 0, 0)
                .expect("time")
        );
        Ok(())
    }
}
