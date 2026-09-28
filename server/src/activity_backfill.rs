use color_eyre::eyre::{Context as _, eyre};
use uuid::Uuid;

use crate::state::AppState;

const BATCH_SIZE: i64 = 1_000;

#[derive(Debug)]
struct BatchResult {
    source_row_count: i64,
    last_source_key: Option<Uuid>,
    inserted_count: i64,
}

/// Process one bounded keyset page for each source. Returns true while work remains.
pub async fn run_batch(state: &AppState) -> cja::Result<bool> {
    let epoch = sqlx::query!(
        "SELECT tracking_started_at, backfill_completed_at FROM stats_tracking_start WHERE singleton = TRUE"
    )
    .fetch_optional(&state.db)
    .await
    .wrap_err("Failed to read activity tracking epoch")?
    .ok_or_else(|| eyre!("Missing stats tracking singleton"))?;
    if epoch.backfill_completed_at.is_some() {
        return Ok(false);
    }

    for source in [
        "sessions",
        "games",
        "battlesnakes",
        "saved_games",
        "tournament_registrations",
        "leaderboard_entries",
    ] {
        let mut tx = state
            .db
            .begin()
            .await
            .wrap_err("Failed to start activity backfill transaction")?;
        sqlx::query!("SET LOCAL statement_timeout = '10s'")
            .execute(&mut *tx)
            .await
            .wrap_err("Failed to set activity backfill timeout")?;
        let progress = sqlx::query!(
            "SELECT cursor_id, done FROM stats_activity_backfill_progress WHERE source = $1 FOR UPDATE SKIP LOCKED",
            source
        )
        .fetch_optional(&mut *tx)
        .await
        .wrap_err("Failed to lock activity backfill progress")?;
        let Some(progress) = progress else {
            continue;
        };
        if progress.done {
            continue;
        }
        let cursor = progress.cursor_id;
        let result: BatchResult = match source {
            "sessions" => {
                let row = sqlx::query!(
                    r#"WITH batch AS MATERIALIZED (
    SELECT s.session_id AS key, s.user_id AS user_id, s.created_at AS event_at
    FROM sessions s
    WHERE ($1::uuid IS NULL OR s.session_id > $1) AND s.user_id IS NOT NULL
    ORDER BY s.session_id
    LIMIT $2
), inserted AS (
    INSERT INTO user_activity_days(user_id, day)
    SELECT DISTINCT user_id, (event_at AT TIME ZONE 'UTC')::date
    FROM batch
    WHERE user_id IS NOT NULL AND event_at < $3
    ON CONFLICT (user_id, day) DO NOTHING
    RETURNING 1
)
SELECT (SELECT COUNT(*) FROM batch) AS "source_row_count!: i64",
       (SELECT MAX(key::text)::uuid FROM batch) AS "last_source_key?: Uuid",
       (SELECT COUNT(*) FROM inserted) AS "inserted_count!: i64""#,
                    cursor,
                    BATCH_SIZE,
                    epoch.tracking_started_at
                )
                .fetch_one(&mut *tx)
                .await
                .wrap_err("Failed to backfill sessions")?;
                BatchResult {
                    source_row_count: row.source_row_count,
                    last_source_key: row.last_source_key,
                    inserted_count: row.inserted_count,
                }
            }
            "games" => {
                let row = sqlx::query!(
                    r#"WITH batch AS MATERIALIZED (
    SELECT s.game_id AS key, s.created_by_user_id AS user_id, s.created_at AS event_at
    FROM games s
    WHERE ($1::uuid IS NULL OR s.game_id > $1) AND TRUE
    ORDER BY s.game_id
    LIMIT $2
), inserted AS (
    INSERT INTO user_activity_days(user_id, day)
    SELECT DISTINCT user_id, (event_at AT TIME ZONE 'UTC')::date
    FROM batch
    WHERE user_id IS NOT NULL AND event_at < $3
    ON CONFLICT (user_id, day) DO NOTHING
    RETURNING 1
)
SELECT (SELECT COUNT(*) FROM batch) AS "source_row_count!: i64",
       (SELECT MAX(key::text)::uuid FROM batch) AS "last_source_key?: Uuid",
       (SELECT COUNT(*) FROM inserted) AS "inserted_count!: i64""#,
                    cursor,
                    BATCH_SIZE,
                    epoch.tracking_started_at
                )
                .fetch_one(&mut *tx)
                .await
                .wrap_err("Failed to backfill games")?;
                BatchResult {
                    source_row_count: row.source_row_count,
                    last_source_key: row.last_source_key,
                    inserted_count: row.inserted_count,
                }
            }
            "battlesnakes" => {
                let row = sqlx::query!(
                    r#"WITH batch AS MATERIALIZED (
    SELECT s.battlesnake_id AS key, s.user_id AS user_id, s.created_at AS event_at
    FROM battlesnakes s
    WHERE ($1::uuid IS NULL OR s.battlesnake_id > $1) AND TRUE
    ORDER BY s.battlesnake_id
    LIMIT $2
), inserted AS (
    INSERT INTO user_activity_days(user_id, day)
    SELECT DISTINCT user_id, (event_at AT TIME ZONE 'UTC')::date
    FROM batch
    WHERE user_id IS NOT NULL AND event_at < $3
    ON CONFLICT (user_id, day) DO NOTHING
    RETURNING 1
)
SELECT (SELECT COUNT(*) FROM batch) AS "source_row_count!: i64",
       (SELECT MAX(key::text)::uuid FROM batch) AS "last_source_key?: Uuid",
       (SELECT COUNT(*) FROM inserted) AS "inserted_count!: i64""#,
                    cursor,
                    BATCH_SIZE,
                    epoch.tracking_started_at
                )
                .fetch_one(&mut *tx)
                .await
                .wrap_err("Failed to backfill battlesnakes")?;
                BatchResult {
                    source_row_count: row.source_row_count,
                    last_source_key: row.last_source_key,
                    inserted_count: row.inserted_count,
                }
            }
            "saved_games" => {
                let row = sqlx::query!(
                    r#"WITH batch AS MATERIALIZED (
    SELECT s.saved_game_id AS key, s.user_id AS user_id, s.created_at AS event_at
    FROM saved_games s
    WHERE ($1::uuid IS NULL OR s.saved_game_id > $1) AND TRUE
    ORDER BY s.saved_game_id
    LIMIT $2
), inserted AS (
    INSERT INTO user_activity_days(user_id, day)
    SELECT DISTINCT user_id, (event_at AT TIME ZONE 'UTC')::date
    FROM batch
    WHERE user_id IS NOT NULL AND event_at < $3
    ON CONFLICT (user_id, day) DO NOTHING
    RETURNING 1
)
SELECT (SELECT COUNT(*) FROM batch) AS "source_row_count!: i64",
       (SELECT MAX(key::text)::uuid FROM batch) AS "last_source_key?: Uuid",
       (SELECT COUNT(*) FROM inserted) AS "inserted_count!: i64""#,
                    cursor,
                    BATCH_SIZE,
                    epoch.tracking_started_at
                )
                .fetch_one(&mut *tx)
                .await
                .wrap_err("Failed to backfill saved_games")?;
                BatchResult {
                    source_row_count: row.source_row_count,
                    last_source_key: row.last_source_key,
                    inserted_count: row.inserted_count,
                }
            }
            "tournament_registrations" => {
                let row = sqlx::query!(
                    r#"WITH batch AS MATERIALIZED (
    SELECT s.registration_id AS key, s.user_id AS user_id, s.registered_at AS event_at
    FROM tournament_registrations s
    WHERE ($1::uuid IS NULL OR s.registration_id > $1) AND TRUE
    ORDER BY s.registration_id
    LIMIT $2
), inserted AS (
    INSERT INTO user_activity_days(user_id, day)
    SELECT DISTINCT user_id, (event_at AT TIME ZONE 'UTC')::date
    FROM batch
    WHERE user_id IS NOT NULL AND event_at < $3
    ON CONFLICT (user_id, day) DO NOTHING
    RETURNING 1
)
SELECT (SELECT COUNT(*) FROM batch) AS "source_row_count!: i64",
       (SELECT MAX(key::text)::uuid FROM batch) AS "last_source_key?: Uuid",
       (SELECT COUNT(*) FROM inserted) AS "inserted_count!: i64""#,
                    cursor,
                    BATCH_SIZE,
                    epoch.tracking_started_at
                )
                .fetch_one(&mut *tx)
                .await
                .wrap_err("Failed to backfill tournament_registrations")?;
                BatchResult {
                    source_row_count: row.source_row_count,
                    last_source_key: row.last_source_key,
                    inserted_count: row.inserted_count,
                }
            }
            "leaderboard_entries" => {
                let row = sqlx::query!(
                    r#"WITH batch AS MATERIALIZED (
    SELECT s.leaderboard_entry_id AS key, b.user_id AS user_id, s.created_at AS event_at
    FROM leaderboard_entries s LEFT JOIN battlesnakes b ON b.battlesnake_id = s.battlesnake_id
    WHERE ($1::uuid IS NULL OR s.leaderboard_entry_id > $1) AND TRUE
    ORDER BY s.leaderboard_entry_id
    LIMIT $2
), inserted AS (
    INSERT INTO user_activity_days(user_id, day)
    SELECT DISTINCT user_id, (event_at AT TIME ZONE 'UTC')::date
    FROM batch
    WHERE user_id IS NOT NULL AND event_at < $3
    ON CONFLICT (user_id, day) DO NOTHING
    RETURNING 1
)
SELECT (SELECT COUNT(*) FROM batch) AS "source_row_count!: i64",
       (SELECT MAX(key::text)::uuid FROM batch) AS "last_source_key?: Uuid",
       (SELECT COUNT(*) FROM inserted) AS "inserted_count!: i64""#,
                    cursor,
                    BATCH_SIZE,
                    epoch.tracking_started_at
                )
                .fetch_one(&mut *tx)
                .await
                .wrap_err("Failed to backfill leaderboard_entries")?;
                BatchResult {
                    source_row_count: row.source_row_count,
                    last_source_key: row.last_source_key,
                    inserted_count: row.inserted_count,
                }
            }
            _ => return Err(eyre!("Unknown activity backfill source: {source}")),
        };
        let done = result.source_row_count < BATCH_SIZE;
        sqlx::query!(
            "UPDATE stats_activity_backfill_progress SET cursor_id = COALESCE($2, cursor_id), done = $3 WHERE source = $1",
            source,
            result.last_source_key,
            done
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to advance activity backfill cursor")?;
        tx.commit()
            .await
            .wrap_err("Failed to commit activity backfill batch")?;
        tracing::info!(
            source,
            rows = result.source_row_count,
            inserted = result.inserted_count,
            done,
            "Activity backfill batch committed"
        );
    }

    let remaining = sqlx::query_scalar!(
        "SELECT COUNT(*) AS \"count!: i64\" FROM stats_activity_backfill_progress WHERE NOT done"
    )
    .fetch_one(&state.db)
    .await
    .wrap_err("Failed to read activity backfill completion")?;
    if remaining == 0 {
        sqlx::query!(
            "UPDATE stats_tracking_start SET backfill_completed_at = NOW() WHERE singleton = TRUE AND backfill_completed_at IS NULL"
        )
        .execute(&state.db)
        .await
        .wrap_err("Failed to mark activity backfill complete")?;
        Ok(false)
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test(migrations = "../migrations")]
    async fn backfills_all_six_durable_signals_once_per_user_day(db: PgPool) {
        let user = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'backfill', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes(user_id, name, url, created_at) VALUES ($1, 'backfill', 'https://example.com', '2020-01-02 08:00+00') RETURNING battlesnake_id",
            user
        ).fetch_one(&db).await.unwrap();
        let game = sqlx::query_scalar!(
            "INSERT INTO games(board_size, game_type, created_by_user_id, created_at) VALUES ('11x11', 'Standard', $1, '2020-01-02 22:00+00') RETURNING game_id",
            user
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO sessions(user_id, created_at) VALUES ($1, '2020-01-02 09:00+00'), ($1, '2020-01-02 09:01+00'), (NULL, '2020-01-02 09:02+00'), ($1, '2020-01-06 23:59:59+00'), ($1, '2020-01-07 00:00:00+00'), ($1, '2099-01-02 09:00+00')",
            user
        ).execute(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO saved_games(user_id, game_id, created_at) VALUES ($1, $2, '2020-01-03 01:00+00')",
            user, game
        ).execute(&db).await.unwrap();
        let tournament = sqlx::query_scalar!(
            "INSERT INTO tournaments(name, user_id) VALUES ('Backfill', $1) RETURNING tournament_id",
            user
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO tournament_registrations(tournament_id, battlesnake_id, user_id, seed, registered_at) VALUES ($1, $2, $3, 1, '2020-01-04 01:00+00')",
            tournament, snake, user
        ).execute(&db).await.unwrap();
        let leaderboard = sqlx::query_scalar!(
            "INSERT INTO leaderboards(name) VALUES ('Backfill') RETURNING leaderboard_id"
        )
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO leaderboard_entries(leaderboard_id, battlesnake_id, created_at) VALUES ($1, $2, '2020-01-05 01:00+00')",
            leaderboard, snake
        ).execute(&db).await.unwrap();
        // Participation alone is not a person-activity signal.
        sqlx::query!(
            "INSERT INTO game_battlesnakes(game_id, battlesnake_id) VALUES ($1, $2)",
            game,
            snake
        )
        .execute(&db)
        .await
        .unwrap();

        let state = AppState::test_from_pool(db.clone());
        assert!(!run_batch(&state).await.unwrap());
        assert!(!run_batch(&state).await.unwrap());
        let days = sqlx::query_scalar!(
            "SELECT day FROM user_activity_days WHERE user_id = $1 ORDER BY day",
            user
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            days.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "2020-01-02",
                "2020-01-03",
                "2020-01-04",
                "2020-01-05",
                "2020-01-06",
                "2020-01-07"
            ]
        );
        let completed = sqlx::query_scalar!(
            "SELECT backfill_completed_at FROM stats_tracking_start WHERE singleton = TRUE"
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(completed.is_some());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sessions_cursor_counts_source_rows_not_inserted_days(db: PgPool) {
        let user = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'pages', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO sessions(user_id, created_at) SELECT $1, '2020-01-01 00:00+00'::timestamptz FROM generate_series(1, 1001)",
            user
        ).execute(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO sessions(user_id, created_at) SELECT NULL, '2020-01-01 00:00+00'::timestamptz FROM generate_series(1, 2000)"
        ).execute(&db).await.unwrap();
        let thousandth = sqlx::query_scalar!(
            "SELECT session_id FROM sessions WHERE user_id = $1 ORDER BY session_id LIMIT 1 OFFSET 999",
            user
        ).fetch_one(&db).await.unwrap();
        let state = AppState::test_from_pool(db.clone());
        assert!(run_batch(&state).await.unwrap());
        let progress = sqlx::query!(
            "SELECT cursor_id, done FROM stats_activity_backfill_progress WHERE source = 'sessions'"
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(progress.cursor_id, Some(thousandth));
        assert!(!progress.done);
        assert!(!run_batch(&state).await.unwrap());
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!: i64\" FROM user_activity_days WHERE user_id = $1",
            user
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn exactly_full_page_needs_empty_follow_up(db: PgPool) {
        let user = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'full-page', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO sessions(user_id, created_at) SELECT $1, '2020-01-01 00:00+00'::timestamptz FROM generate_series(1, 1000)",
            user
        ).execute(&db).await.unwrap();
        let state = AppState::test_from_pool(db.clone());
        assert!(run_batch(&state).await.unwrap());
        let first = sqlx::query!(
            "SELECT done, cursor_id FROM stats_activity_backfill_progress WHERE source = 'sessions'"
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(!first.done);
        assert!(first.cursor_id.is_some());
        assert!(!run_batch(&state).await.unwrap());
        let final_progress = sqlx::query!(
            "SELECT done, cursor_id FROM stats_activity_backfill_progress WHERE source = 'sessions'"
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(final_progress.done);
        assert_eq!(final_progress.cursor_id, first.cursor_id);
    }

    /// A stalled source must not starve the other five. `sessions` is the
    /// largest backfill source and the one most likely to blow the batch
    /// statement timeout in production (its measured page does ~20k buffer
    /// reads against a 10s budget). When it does, the five cheap sources still
    /// hold most of the reconstructable history and must still make progress.
    #[sqlx::test(migrations = "../migrations")]
    async fn one_stalled_source_does_not_block_the_other_sources(db: PgPool) {
        let user = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'stalled-source', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO battlesnakes(user_id, name, url, created_at) VALUES ($1, 'stalled-source', 'https://example.com', '2020-02-03 08:00+00')",
            user
        ).execute(&db).await.unwrap();

        // Hold `sessions` so its batch blocks and trips the per-batch
        // statement timeout, exactly as an oversized production page would.
        let mut blocker = db.begin().await.unwrap();
        sqlx::query!("LOCK TABLE sessions IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();

        let state = AppState::test_from_pool(db.clone());
        let _ = run_batch(&state).await;
        blocker.rollback().await.unwrap();

        let days = sqlx::query_scalar!(
            "SELECT day FROM user_activity_days WHERE user_id = $1 ORDER BY day",
            user
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            days.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["2020-02-03"],
            "battlesnakes must still be backfilled when the sessions batch stalls"
        );
    }
}
