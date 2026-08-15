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
        "Stuck-game sweep complete"
    );

    Ok(())
}

/// Atomically mark all eligible stuck games `failed`, returning their IDs.
/// Eligibility = live status AND older than `max_age_hours` (by `created_at`)
/// AND not a tournament match game.
async fn fail_stuck_games(pool: &sqlx::PgPool, max_age_hours: i32) -> cja::Result<Vec<Uuid>> {
    let ids = sqlx::query_scalar!(
        r#"UPDATE games
           SET status = 'failed', updated_at = NOW()
           WHERE status IN ('waiting', 'running')
             AND created_at < NOW() - make_interval(hours => $1)
             AND game_id NOT IN (SELECT game_id FROM match_games)
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
