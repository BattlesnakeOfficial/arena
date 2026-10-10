use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::{
    models::{
        game_battlesnake,
        leaderboard::{self, LeaderboardGame},
    },
    scoring::{GameResultEntry, GameResultEvent},
    state::AppState,
};

/// Update ratings for all snakes in a completed leaderboard game.
/// Idempotent: safe to call multiple times (e.g. job retries).
/// Uses a database transaction with row locking (FOR UPDATE) to prevent
/// race conditions when concurrent games finish for the same snakes.
pub async fn update_ratings(app_state: &AppState, leaderboard_game_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;

    // Idempotency check: bail if ratings were already applied for this game
    let existing: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM leaderboard_game_results WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check existing game results")?;

    if existing.0 > 0 {
        tracing::info!(
            leaderboard_game_id = %leaderboard_game_id,
            "Ratings already applied for this game, skipping"
        );
        return Ok(());
    }

    // Fetch the leaderboard game (outside transaction — immutable data)
    let lb_game = sqlx::query_as::<_, LeaderboardGame>(
        "SELECT leaderboard_game_id, leaderboard_id, game_id, created_at
         FROM leaderboard_games
         WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to fetch leaderboard game")?;

    // Fetch all game_battlesnakes with their placements (outside transaction — immutable after game finishes)
    let game_snakes = game_battlesnake::get_battlesnakes_by_game_id(pool, lb_game.game_id).await?;

    if game_snakes.is_empty() {
        tracing::warn!(
            game_id = %lb_game.game_id,
            "No snakes found for leaderboard game"
        );
        return Ok(());
    }

    // Start a transaction for the rating update (locks entries to prevent concurrent overwrites)
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start transaction for rating update")?;

    // Authoritative idempotency check INSIDE the transaction.
    // The early check above is a fast-path optimization; this is the real guard
    // against concurrent job execution (e.g., timeout-triggered retry while original still runs).
    let existing_in_tx: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM leaderboard_game_results WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to check existing game results inside transaction")?;

    if existing_in_tx.0 > 0 {
        tracing::info!(
            leaderboard_game_id = %leaderboard_game_id,
            "Ratings already applied (detected inside transaction), skipping"
        );
        return Ok(());
    }

    // Look up each snake's leaderboard entry with FOR UPDATE to lock the rows.
    // Lock in one fixed order (entry id, then snake id), never placement order:
    // two games with the same snakes that finish together in different orders
    // would otherwise lock the rows in opposite orders and deadlock (DEV-1519).
    let mut lock_order: Vec<_> = game_snakes.iter().collect();
    lock_order.sort_by_key(|gs| (gs.leaderboard_entry_id, gs.battlesnake_id));

    let mut entries_with_placements: Vec<(leaderboard::LeaderboardEntry, i32, Uuid)> = Vec::new();

    for gs in lock_order {
        let placement = gs.placement.unwrap_or(game_snakes.len() as i32);

        // Use leaderboard_entry_id if stored (deterministic lookup by PK).
        // Fall back to battlesnake_id lookup for games created before this column was added.
        let entry = if let Some(entry_id) = gs.leaderboard_entry_id {
            leaderboard::get_entry_for_update_by_id(&mut *tx, entry_id)
                .await
                .wrap_err_with(|| {
                    format!("Failed to get leaderboard entry {entry_id} for update")
                })?
        } else {
            leaderboard::get_entry_for_update(&mut *tx, lb_game.leaderboard_id, gs.battlesnake_id)
                .await
                .wrap_err_with(|| {
                    format!(
                        "Failed to get leaderboard entry for snake {}",
                        gs.battlesnake_id
                    )
                })?
        };

        if let Some(entry) = entry {
            entries_with_placements.push((entry, placement, gs.game_battlesnake_id));
        } else {
            tracing::warn!(
                battlesnake_id = %gs.battlesnake_id,
                leaderboard_id = %lb_game.leaderboard_id,
                "Snake has no leaderboard entry, skipping"
            );
        }
    }

    if entries_with_placements.len() < 2 {
        tracing::warn!(
            game_id = %lb_game.game_id,
            "Fewer than 2 snakes with leaderboard entries, skipping rating update"
        );
        return Ok(());
    }

    // Build a GameResultEvent for the scoring algorithms
    let event = GameResultEvent {
        leaderboard_game_id,
        leaderboard_id: lb_game.leaderboard_id,
        game_id: lb_game.game_id,
        results: entries_with_placements
            .iter()
            .map(|(entry, placement, game_battlesnake_id)| GameResultEntry {
                leaderboard_entry_id: entry.leaderboard_entry_id,
                battlesnake_id: entry.battlesnake_id,
                placement: *placement,
                mu: entry.mu,
                sigma: entry.sigma,
                game_battlesnake_id: *game_battlesnake_id,
            })
            .collect(),
    };

    // Run all scoring algorithms
    for algo in app_state.scoring.algorithms() {
        algo.process_game_result(&mut tx, &event).await?;
    }

    // Commit the transaction — all rating updates are atomic
    tx.commit()
        .await
        .wrap_err("Failed to commit rating update transaction")?;

    tracing::info!(
        leaderboard_game_id = %leaderboard_game_id,
        game_id = %lb_game.game_id,
        snakes_updated = entries_with_placements.len(),
        "Ratings updated for leaderboard game"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::game::{self, CreateGame, GameBoardSize, GameType};
    use crate::scoring::{
        ScoringAlgorithm, ScoringRegistry, food_eaten::FoodEatenScoring, weng_lin::WengLinScoring,
        win_rate::WinRateScoring,
    };
    use sqlx::PgPool;
    use std::time::Duration;

    async fn create_user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
             RETURNING user_id",
            github_id,
            format!("gh-user-{github_id}"),
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    async fn create_snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
        let id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'http://example.com/snake')
             RETURNING battlesnake_id",
            user_id,
            name,
        )
        .fetch_one(pool)
        .await?;
        Ok(id)
    }

    /// A leaderboard game the given entries finished in the given places.
    async fn finished_game(
        pool: &PgPool,
        leaderboard_id: Uuid,
        placements: &[(Uuid, i32)],
    ) -> cja::Result<Uuid> {
        let game = game::create_game(
            pool,
            CreateGame {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
            },
        )
        .await?;
        for &(entry_id, placement) in placements {
            sqlx::query!(
                "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id, placement)
                 VALUES ($1, $2, $3)",
                game.game_id,
                entry_id,
                placement,
            )
            .execute(pool)
            .await?;
        }
        let lb_game =
            leaderboard::create_leaderboard_game(pool, leaderboard_id, game.game_id).await?;
        Ok(lb_game.leaderboard_game_id)
    }

    /// Wait until `n` sessions in this test's database are blocked on a lock.
    async fn wait_for_lock_waiters(pool: &PgPool, n: i64) -> cja::Result<()> {
        for _ in 0..500 {
            let waiting = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "count!" FROM pg_stat_activity
                   WHERE datname = current_database() AND wait_event_type = 'Lock'"#
            )
            .fetch_one(pool)
            .await?;
            if waiting >= n {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(cja::color_eyre::eyre::eyre!(
            "timed out waiting for {n} lock waiters"
        ))
    }

    /// Replays the 2026-10-02 prod deadlock: two games with the same snakes
    /// finished in opposite orders, rated at the same moment. Locking in
    /// placement order made one job take A then B and the other B then A.
    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_games_with_opposite_placements_do_not_deadlock(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = create_user(&pool, 15190).await?;
        let snake_a = create_snake(&pool, user_id, "snake-a").await?;
        let snake_b = create_snake(&pool, user_id, "snake-b").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            "Deadlock Board",
        )
        .fetch_one(&pool)
        .await?;
        let a = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_a)
            .await?
            .leaderboard_entry_id;
        let b = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_b)
            .await?
            .leaderboard_entry_id;

        let a_won = finished_game(&pool, leaderboard_id, &[(a, 1), (b, 2)]).await?;
        let b_won = finished_game(&pool, leaderboard_id, &[(b, 1), (a, 2)]).await?;
        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        scoring.register(Box::new(FoodEatenScoring));
        state.scoring = std::sync::Arc::new(scoring);

        // Hold A so both jobs queue up behind it, the A-won job first. Under
        // placement-order locking the B-won job grabs B before it blocks, and
        // releasing A closes the cycle.
        let mut holder = pool.begin().await?;
        leaderboard::get_entry_for_update_by_id(&mut *holder, a).await?;

        let first_state = state.clone();
        let first = tokio::spawn(async move { update_ratings(&first_state, a_won).await });
        wait_for_lock_waiters(&pool, 1).await?;
        let second_state = state.clone();
        let second = tokio::spawn(async move { update_ratings(&second_state, b_won).await });
        wait_for_lock_waiters(&pool, 2).await?;
        holder.rollback().await?;

        let (first, second) = tokio::time::timeout(Duration::from_secs(30), async {
            (first.await, second.await)
        })
        .await?;
        first??;
        second??;

        assert_eq!(
            leaderboard::count_game_results_for_entry(&pool, a).await?,
            2
        );
        assert_eq!(
            leaderboard::count_game_results_for_entry(&pool, b).await?,
            2
        );
        Ok(())
    }

    /// Two snakes that die on the same final turn share first place. That is a
    /// draw: neither gets a win, and equally rated snakes keep their mu.
    #[sqlx::test(migrations = "../migrations")]
    async fn shared_first_place_rates_as_a_draw(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 16400).await?;
        let snake_a = create_snake(&pool, user_id, "snake-a").await?;
        let snake_b = create_snake(&pool, user_id, "snake-b").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            "Draw Board",
        )
        .fetch_one(&pool)
        .await?;
        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        state.scoring = std::sync::Arc::new(scoring);
        let mut entries = Vec::new();
        for snake_id in [snake_a, snake_b] {
            let entry = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_id).await?;
            for algo in state.scoring.algorithms() {
                algo.initialize_entry(&pool, entry.leaderboard_entry_id)
                    .await?;
            }
            entries.push(entry);
        }
        let (a, b) = (
            entries[0].leaderboard_entry_id,
            entries[1].leaderboard_entry_id,
        );

        let draw = finished_game(&pool, leaderboard_id, &[(a, 1), (b, 1)]).await?;
        update_ratings(&state, draw).await?;

        for entry in &entries {
            let id = entry.leaderboard_entry_id;
            let row = sqlx::query!(
                "SELECT le.mu, le.games_played, le.first_place_finishes, wr.wins, wr.games_played AS wr_games
                 FROM leaderboard_entries le
                 JOIN win_rate_stats wr USING (leaderboard_entry_id)
                 WHERE le.leaderboard_entry_id = $1",
                id,
            )
            .fetch_one(&pool)
            .await?;
            assert_eq!(row.games_played, 1);
            assert_eq!(row.first_place_finishes, 0, "a draw is not a win");
            assert_eq!((row.wins, row.wr_games), (0, 1), "a draw is not a win");
            assert!(
                (row.mu - entry.mu).abs() < 1e-9,
                "equals who draw keep their mu: {} -> {}",
                entry.mu,
                row.mu
            );
        }

        let a_won = finished_game(&pool, leaderboard_id, &[(a, 1), (b, 2)]).await?;
        update_ratings(&state, a_won).await?;
        let wins = sqlx::query!(
            "SELECT le.leaderboard_entry_id, le.first_place_finishes, wr.wins
             FROM leaderboard_entries le
             JOIN win_rate_stats wr USING (leaderboard_entry_id)
             WHERE le.leaderboard_entry_id = ANY($1)",
            &[a, b][..],
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.leaderboard_entry_id,
                (row.first_place_finishes, row.wins),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(wins[&a], (1, 1));
        assert_eq!(wins[&b], (0, 0));
        Ok(())
    }

    /// Food scoring credits exactly the count the game runner recorded on
    /// `game_battlesnakes`, and nothing for a snake with no recorded count.
    #[sqlx::test(migrations = "../migrations")]
    async fn food_score_comes_from_recorded_food_eaten(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 233).await?;
        let eater = create_snake(&pool, user_id, "eater").await?;
        let unrecorded = create_snake(&pool, user_id, "unrecorded").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            "Food Board",
        )
        .fetch_one(&pool)
        .await?;
        let a = leaderboard::get_or_create_entry(&pool, leaderboard_id, eater)
            .await?
            .leaderboard_entry_id;
        let b = leaderboard::get_or_create_entry(&pool, leaderboard_id, unrecorded)
            .await?
            .leaderboard_entry_id;

        let lb_game = finished_game(&pool, leaderboard_id, &[(a, 1), (b, 2)]).await?;
        sqlx::query!(
            "UPDATE game_battlesnakes SET food_eaten = 7 WHERE leaderboard_entry_id = $1",
            a,
        )
        .execute(&pool)
        .await?;

        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(FoodEatenScoring));
        state.scoring = std::sync::Arc::new(scoring);
        update_ratings(&state, lb_game).await?;

        let eater_score = FoodEatenScoring.get_entry_score(&pool, a).await?;
        assert_eq!(eater_score.map(|s| s.score), Some(7.0));
        let unrecorded_score = FoodEatenScoring.get_entry_score(&pool, b).await?;
        assert_eq!(unrecorded_score.map_or(0.0, |s| s.score), 0.0);

        let per_game: Vec<(Uuid, i32)> = sqlx::query_as(
            "SELECT leaderboard_entry_id, food_eaten FROM leaderboard_game_results
             WHERE leaderboard_game_id = $1 ORDER BY food_eaten DESC",
        )
        .bind(lb_game)
        .fetch_all(&pool)
        .await?;
        assert_eq!(per_game, vec![(a, 7), (b, 0)]);
        Ok(())
    }
}
