//! Periodic sweep that fails games stuck in a live state.
//!
//! A game whose `GameRunnerJob` died (OOM, exhausted retries — cja deletes a
//! job after its retries) never leaves `waiting`/`running` on its own, so it
//! shows as "live" on leaderboard pages forever. This sweep marks any
//! non-tournament game older than
//! [`crate::config::AppConfig::stuck_game_max_age_hours`] as
//! [`crate::models::game::GameStatus::Failed`].
//!
//! Tournament match games are excluded: [`crate::tournament_match::run_match`]
//! treats any non-`Finished` match game as re-runnable, so failing one would
//! livelock the stall path (re-enqueue after 15 minutes without activity,
//! runner short-circuits). Lifting that exclusion is blocked on `run_match`
//! gaining a `Failed` error/forfeit path (separate task).
//!
//! Idempotent: the status predicate means a second sweep matches nothing, so
//! duplicate cron enqueues and cja retries converge.

use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::state::AppState;

/// AppState entry point invoked by [`crate::jobs::StuckGameSweeperJob`].
pub async fn run_sweep(app_state: &AppState) -> cja::Result<()> {
    let max_age_hours = app_state.config.stuck_game_max_age_hours;
    let failed = fail_stuck_games(&app_state.db, max_age_hours).await?;

    tracing::info!(
        max_age_hours,
        failed_count = failed.len(),
        failed_ids = ?failed,
        "Stuck-game sweep complete"
    );

    Ok(())
}

/// Atomically mark all eligible stuck games `failed`, returning their IDs.
/// Eligibility = live status AND older than `max_age_hours` AND not a
/// tournament match game. Age counts from `enqueued_at`, falling back to
/// `created_at`: a ladder game held before dispatch (DEV-1613) has a NULL
/// `enqueued_at` and ages from creation, but once dispatched its runtime is
/// measured from the enqueue, so a long hold can't get a live game failed.
async fn fail_stuck_games(pool: &sqlx::PgPool, max_age_hours: i32) -> cja::Result<Vec<Uuid>> {
    let ids = sqlx::query_scalar!(
        r#"UPDATE games
           SET status = 'failed', updated_at = NOW()
           WHERE games.status IN ('waiting', 'running')
             AND COALESCE(games.enqueued_at, games.created_at) < NOW() - make_interval(hours => $1)
             AND games.game_id NOT IN (SELECT game_id FROM match_games)
             AND NOT (games.status = 'waiting' AND EXISTS (
               SELECT 1 FROM leaderboard_games waiting_lg
               JOIN leaderboard_games running_lg ON running_lg.leaderboard_id = waiting_lg.leaderboard_id
               JOIN games running_game ON running_game.game_id = running_lg.game_id AND running_game.status = 'running'
               JOIN game_battlesnakes waiting_gb ON waiting_gb.game_id = games.game_id
               LEFT JOIN leaderboard_entries waiting_le ON waiting_le.leaderboard_entry_id = waiting_gb.leaderboard_entry_id
               JOIN game_battlesnakes running_gb ON running_gb.game_id = running_game.game_id
               LEFT JOIN leaderboard_entries running_le ON running_le.leaderboard_entry_id = running_gb.leaderboard_entry_id
               WHERE waiting_lg.game_id = games.game_id
                 AND COALESCE(waiting_gb.battlesnake_id, waiting_le.battlesnake_id)
                     = COALESCE(running_gb.battlesnake_id, running_le.battlesnake_id)
             ))
           RETURNING game_id"#,
        max_age_hours,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to sweep stuck games")?;

    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{DateTime, Duration, Utc};
    use sqlx::PgPool;

    /// Insert a game with an explicit `created_at`, so eligibility can be
    /// exercised without sleeping or disabling the `updated_at` trigger.
    async fn insert_game(
        pool: &PgPool,
        status: &str,
        created_at: DateTime<Utc>,
    ) -> cja::Result<Uuid> {
        let game_id = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status, created_at, updated_at)
             VALUES ('11x11', 'Standard', $1, $2, $2)
             RETURNING game_id",
            status,
            created_at,
        )
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    async fn game_status(pool: &PgPool, game_id: Uuid) -> cja::Result<String> {
        let status = sqlx::query_scalar!("SELECT status FROM games WHERE game_id = $1", game_id)
            .fetch_one(pool)
            .await?;
        Ok(status)
    }

    async fn game_updated_at(pool: &PgPool, game_id: Uuid) -> cja::Result<DateTime<Utc>> {
        let updated_at =
            sqlx::query_scalar!("SELECT updated_at FROM games WHERE game_id = $1", game_id)
                .fetch_one(pool)
                .await?;
        Ok(updated_at)
    }

    /// A `running` game left behind by a dead runner is failed on the next
    /// sweep, and its `updated_at` is bumped.
    #[sqlx::test(migrations = "../migrations")]
    async fn stale_running_game_is_failed(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let game_id = insert_game(&pool, "running", base_time - Duration::hours(3)).await?;

        let failed = fail_stuck_games(&pool, 2).await?;

        assert_eq!(failed, vec![game_id]);
        assert_eq!(game_status(&pool, game_id).await?, "failed");
        assert!(game_updated_at(&pool, game_id).await? > base_time - Duration::hours(3));

        Ok(())
    }

    /// A game that never left the queue is stuck just the same.
    #[sqlx::test(migrations = "../migrations")]
    async fn stale_waiting_game_is_failed(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let game_id = insert_game(&pool, "waiting", base_time - Duration::hours(3)).await?;

        let failed = fail_stuck_games(&pool, 2).await?;

        assert_eq!(failed, vec![game_id]);
        assert_eq!(game_status(&pool, game_id).await?, "failed");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn same_ladder_held_waiter_survives_until_running_game_fails(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (1613, 'sweeper-owner', 'test') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'sweeper-snake', 'http://example.com') RETURNING battlesnake_id", user
        ).fetch_one(&pool).await?;
        let ladder = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
        )
        .fetch_one(&pool)
        .await?;
        let old = Utc::now() - Duration::hours(3);
        let running = insert_game(&pool, "running", old).await?;
        let waiting = insert_game(&pool, "waiting", old).await?;
        for game_id in [running, waiting] {
            sqlx::query!(
                "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
                ladder,
                game_id
            )
            .execute(&pool)
            .await?;
            sqlx::query!(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)",
                game_id,
                snake
            )
            .execute(&pool)
            .await?;
        }
        let first = fail_stuck_games(&pool, 2).await?;
        assert!(first.contains(&running));
        assert!(!first.contains(&waiting));
        assert_eq!(game_status(&pool, waiting).await?, "waiting");
        assert_eq!(fail_stuck_games(&pool, 2).await?, vec![waiting]);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn ladder_waiter_age_and_disabled_leaderboard_do_not_exempt_sweep(
        pool: PgPool,
    ) -> cja::Result<()> {
        let ladder: Uuid = sqlx::query_scalar(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'",
        )
        .fetch_one(&pool)
        .await?;
        let now = Utc::now();
        let recent = insert_game(&pool, "waiting", now - Duration::minutes(30)).await?;
        let old = insert_game(&pool, "waiting", now - Duration::hours(3)).await?;
        for game_id in [recent, old] {
            sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
                .bind(ladder)
                .bind(game_id)
                .execute(&pool)
                .await?;
        }
        sqlx::query(
            "UPDATE leaderboards SET disabled_at = clock_timestamp() WHERE leaderboard_id = $1",
        )
        .bind(ladder)
        .execute(&pool)
        .await?;
        assert_eq!(fail_stuck_games(&pool, 2).await?, vec![old]);
        assert_eq!(game_status(&pool, recent).await?, "waiting");
        assert_eq!(game_status(&pool, old).await?, "failed");
        Ok(())
    }

    /// Games inside the window are live work, and terminal states are
    /// already done — the positive `IN ('waiting','running')` predicate must
    /// leave all three alone.
    #[sqlx::test(migrations = "../migrations")]
    async fn fresh_and_terminal_games_are_untouched(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let fresh = insert_game(&pool, "running", base_time - Duration::minutes(10)).await?;
        let finished = insert_game(&pool, "finished", base_time - Duration::hours(3)).await?;
        let already_failed = insert_game(&pool, "failed", base_time - Duration::hours(3)).await?;

        let failed = fail_stuck_games(&pool, 2).await?;

        assert!(failed.is_empty());
        assert_eq!(game_status(&pool, fresh).await?, "running");
        assert_eq!(game_status(&pool, finished).await?, "finished");
        assert_eq!(game_status(&pool, already_failed).await?, "failed");

        Ok(())
    }

    /// Failing a tournament match game would livelock `run_match` (it
    /// re-enqueues any non-finished match game, and the runner short-circuits
    /// on failed), so match games are excluded no matter how stale.
    #[sqlx::test(migrations = "../migrations")]
    async fn tournament_match_games_are_excluded(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let match_game = insert_game(&pool, "running", base_time - Duration::hours(3)).await?;
        let control = insert_game(&pool, "running", base_time - Duration::hours(3)).await?;

        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (424242, 'test-user', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let tournament_id = sqlx::query_scalar!(
            "INSERT INTO tournaments (name, user_id) VALUES ('t', $1) RETURNING tournament_id",
            user_id,
        )
        .fetch_one(&pool)
        .await?;
        let match_id = sqlx::query_scalar!(
            "INSERT INTO tournament_matches (tournament_id, round, position, visual_column, visual_row)
             VALUES ($1, 1, 0, 0, 0) RETURNING match_id",
            tournament_id,
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query!(
            "INSERT INTO match_games (match_id, game_id, game_number) VALUES ($1, $2, 1)",
            match_id,
            match_game,
        )
        .execute(&pool)
        .await?;

        let failed = fail_stuck_games(&pool, 2).await?;

        assert_eq!(failed, vec![control]);
        assert_eq!(game_status(&pool, match_game).await?, "running");
        assert_eq!(game_status(&pool, control).await?, "failed");

        Ok(())
    }

    /// Duplicate cron enqueues and cja retries converge: the second sweep
    /// matches nothing and leaves `updated_at` alone.
    #[sqlx::test(migrations = "../migrations")]
    async fn second_sweep_is_a_no_op(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let game_id = insert_game(&pool, "running", base_time - Duration::hours(3)).await?;

        assert_eq!(fail_stuck_games(&pool, 2).await?, vec![game_id]);
        let after_first = game_updated_at(&pool, game_id).await?;

        assert!(fail_stuck_games(&pool, 2).await?.is_empty());
        assert_eq!(game_status(&pool, game_id).await?, "failed");
        assert_eq!(game_updated_at(&pool, game_id).await?, after_first);

        Ok(())
    }

    /// Age counts from the enqueue once a game has one: a ladder game held for a
    /// long time before dispatch must not be failed mid-run, while a game with no
    /// enqueue (held, never dispatched) still ages from creation.
    #[sqlx::test(migrations = "../migrations")]
    async fn age_counts_from_enqueue_when_present(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let recently_started =
            insert_game(&pool, "running", base_time - Duration::hours(3)).await?;
        sqlx::query!(
            "UPDATE games SET enqueued_at = $2 WHERE game_id = $1",
            recently_started,
            base_time - Duration::minutes(10),
        )
        .execute(&pool)
        .await?;
        let never_enqueued = insert_game(&pool, "waiting", base_time - Duration::hours(3)).await?;

        let failed = fail_stuck_games(&pool, 2).await?;

        assert_eq!(failed, vec![never_enqueued]);
        assert_eq!(game_status(&pool, recently_started).await?, "running");
        Ok(())
    }

    /// The user-visible payoff: a swept game stops showing as "live now" on
    /// its leaderboard page, without changing the total game count. Driven
    /// through `run_sweep` so the AppState/config path is covered too.
    #[sqlx::test(migrations = "../migrations")]
    async fn swept_games_drop_out_of_the_leaderboard_live_count(pool: PgPool) -> cja::Result<()> {
        let base_time = Utc::now();
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('sweep-test') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        let game_id = insert_game(&pool, "running", base_time - Duration::hours(3)).await?;
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
            leaderboard_id,
            game_id,
        )
        .execute(&pool)
        .await?;

        let before =
            crate::models::leaderboard::get_leaderboard_status(&pool, leaderboard_id).await?;
        assert_eq!(before.games_in_progress, 1);
        assert_eq!(before.total_games, 1);

        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        run_sweep(&app_state).await?;

        let after =
            crate::models::leaderboard::get_leaderboard_status(&pool, leaderboard_id).await?;
        assert_eq!(after.games_in_progress, 0);
        assert_eq!(after.total_games, 1);

        Ok(())
    }
}
