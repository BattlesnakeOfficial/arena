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

    // Serialize concurrent update_ratings runs for the same game (DEV-2174).
    // This row lock is the transaction's FIRST statement: a concurrent run
    // blocks here — before its idempotency COUNT — until this run commits,
    // so the snapshot its COUNT takes afterwards includes this run's result
    // rows. Safe against deadlock: nothing acquires a game lock while
    // holding entry locks (this is always the first lock taken).
    let game_row = sqlx::query!(
        "SELECT 1 AS locked FROM leaderboard_games WHERE leaderboard_game_id = $1 FOR UPDATE",
        leaderboard_game_id,
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to lock leaderboard game row")?;

    if game_row.is_none() {
        tracing::warn!(
            leaderboard_game_id = %leaderboard_game_id,
            "Leaderboard game vanished while rating, skipping"
        );
        return Ok(());
    }

    // Authoritative idempotency check INSIDE the transaction, AFTER the game
    // row lock above: a READ COMMITTED snapshot taken here includes any
    // previous run's committed result rows, so a concurrent run that waited
    // on the lock now sees COUNT > 0 and skips.
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

    // Run all scoring algorithms. An algorithm reports `false` when the game
    // was already applied by a concurrent run — abort without committing.
    let mut already_applied = false;
    for algo in app_state.scoring.algorithms() {
        if !algo.process_game_result(&mut tx, &event).await? {
            already_applied = true;
            break;
        }
    }

    if already_applied {
        tx.rollback()
            .await
            .wrap_err("Failed to roll back rating update transaction")?;
        tracing::info!(
            leaderboard_game_id = %leaderboard_game_id,
            "Ratings already applied (result insert conflicted), rolled back"
        );
        return Ok(());
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
        GameResultEntry, GameResultEvent, ScoringAlgorithm, ScoringRegistry,
        food_eaten::FoodEatenScoring, weng_lin::WengLinScoring, win_rate::WinRateScoring,
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
        let leaderboard_game_id = Uuid::new_v4();
        finished_game_with_lb_game_id(pool, leaderboard_id, leaderboard_game_id, placements)
            .await?;
        Ok(leaderboard_game_id)
    }

    /// A leaderboard game the given entries finished in the given places,
    /// with an explicit leaderboard_game_id (the repair migration is keyed
    /// to one specific prod game id).
    async fn finished_game_with_lb_game_id(
        pool: &PgPool,
        leaderboard_id: Uuid,
        leaderboard_game_id: Uuid,
        placements: &[(Uuid, i32)],
    ) -> cja::Result<()> {
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
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_game_id, leaderboard_id, game_id)
             VALUES ($1, $2, $3)",
            leaderboard_game_id,
            leaderboard_id,
            game.game_id,
        )
        .execute(pool)
        .await?;
        Ok(())
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
            .expect("live snake joins")
            .leaderboard_entry_id;
        let b = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_b)
            .await?
            .expect("live snake joins")
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

    /// DEV-2174 regression: two concurrent update_ratings calls for the SAME
    /// game. On main, both idempotency COUNTs read 0 while the first run is
    /// still uncommitted, so the second run re-increments every counter and
    /// ON CONFLICT DO NOTHING silently drops its result rows: one game,
    /// games_played = 2. With the game-row lock, the second run blocks before
    /// its COUNT, sees the committed rows after the first commits, and skips.
    /// This is the interleaving that double-scored prod game
    /// 3c7eb192-5e4d-4a97-a62b-4ca69594ce81.
    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_update_ratings_for_one_game_applies_once(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 21740).await?;
        let snake_a = create_snake(&pool, user_id, "snake-a").await?;
        let snake_b = create_snake(&pool, user_id, "snake-b").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            "Double-Apply Board",
        )
        .fetch_one(&pool)
        .await?;
        let a = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_a)
            .await?
            .expect("live snake joins")
            .leaderboard_entry_id;
        let b = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_b)
            .await?
            .expect("live snake joins")
            .leaderboard_entry_id;

        let lb_game = finished_game(&pool, leaderboard_id, &[(a, 1), (b, 2)]).await?;

        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        scoring.register(Box::new(FoodEatenScoring));
        state.scoring = std::sync::Arc::new(scoring);

        // Hold entry a so the first run is parked mid-transaction (its COUNTs
        // already read 0), then start the second run: it also reads COUNT = 0
        // under READ COMMITTED because the first run is uncommitted.
        let mut holder = pool.begin().await?;
        leaderboard::get_entry_for_update_by_id(&mut *holder, a).await?;

        let first_state = state.clone();
        let first = tokio::spawn(async move { update_ratings(&first_state, lb_game).await });
        wait_for_lock_waiters(&pool, 1).await?;
        let second_state = state.clone();
        let second = tokio::spawn(async move { update_ratings(&second_state, lb_game).await });
        wait_for_lock_waiters(&pool, 2).await?;
        holder.rollback().await?;

        let (first, second) = tokio::time::timeout(Duration::from_secs(30), async {
            (first.await, second.await)
        })
        .await?;
        first??;
        second??;

        // One application: exactly one result row per snake...
        for id in [a, b] {
            assert_eq!(
                leaderboard::count_game_results_for_entry(&pool, id).await?,
                1
            );
        }
        // ...and every games_played counter incremented exactly once.
        let counters: Vec<(Uuid, i32, i32)> = sqlx::query_as(
            "SELECT le.leaderboard_entry_id, le.games_played, wr.games_played \
             FROM leaderboard_entries le \
             JOIN win_rate_stats wr USING (leaderboard_entry_id) \
             WHERE le.leaderboard_entry_id = ANY($1)",
        )
        .bind(&[a, b][..])
        .fetch_all(&pool)
        .await?;
        assert_eq!(counters.len(), 2);
        for (id, games, wr_games) in counters {
            assert_eq!((games, wr_games), (1, 1), "entry {id} scored exactly once");
        }
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
            let entry = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_id)
                .await?
                .expect("live snake joins");
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
            .expect("live snake joins")
            .leaderboard_entry_id;
        let b = leaderboard::get_or_create_entry(&pool, leaderboard_id, unrecorded)
            .await?
            .expect("live snake joins")
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
    /// DEV-2174, fix item 2: re-processing an already-rated game must
    /// report `false` so the caller rolls back, and must leave every
    /// counter untouched.
    #[sqlx::test(migrations = "../migrations")]
    async fn weng_lin_second_application_reports_false_and_writes_nothing(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = create_user(&pool, 21741).await?;
        let snake_a = create_snake(&pool, user_id, "snake-a").await?;
        let snake_b = create_snake(&pool, user_id, "snake-b").await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            "Already-Applied Board",
        )
        .fetch_one(&pool)
        .await?;
        let a = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_a)
            .await?
            .expect("live snake joins")
            .leaderboard_entry_id;
        let b = leaderboard::get_or_create_entry(&pool, leaderboard_id, snake_b)
            .await?
            .expect("live snake joins")
            .leaderboard_entry_id;

        let lb_game = finished_game(&pool, leaderboard_id, &[(a, 1), (b, 2)]).await?;

        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        state.scoring = std::sync::Arc::new(scoring);
        update_ratings(&state, lb_game).await?;

        let before: Vec<(Uuid, i32, i32, i32, f64)> = sqlx::query_as(
            "SELECT leaderboard_entry_id, games_played, first_place_finishes, \
                    non_first_finishes, mu \
             FROM leaderboard_entries WHERE leaderboard_entry_id = ANY($1) \
             ORDER BY leaderboard_entry_id",
        )
        .bind(&[a, b][..])
        .fetch_all(&pool)
        .await?;

        // Rebuild the event update_ratings built for the first application.
        let game_id: Uuid = sqlx::query_scalar(
            "SELECT game_id FROM leaderboard_games WHERE leaderboard_game_id = $1",
        )
        .bind(lb_game)
        .fetch_one(&pool)
        .await?;
        let rows: Vec<(Uuid, Uuid, Option<i32>, f64, f64, Uuid)> = sqlx::query_as(
            "SELECT gb.leaderboard_entry_id, le.battlesnake_id, gb.placement, \
                    le.mu, le.sigma, gb.game_battlesnake_id \
             FROM game_battlesnakes gb \
             JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id \
             WHERE gb.game_id = $1 \
             ORDER BY gb.leaderboard_entry_id",
        )
        .bind(game_id)
        .fetch_all(&pool)
        .await?;
        let event = GameResultEvent {
            leaderboard_game_id: lb_game,
            leaderboard_id,
            game_id,
            results: rows
                .into_iter()
                .map(
                    |(
                        leaderboard_entry_id,
                        battlesnake_id,
                        placement,
                        mu,
                        sigma,
                        game_battlesnake_id,
                    )| GameResultEntry {
                        leaderboard_entry_id,
                        battlesnake_id,
                        placement: placement.unwrap_or(2),
                        mu,
                        sigma,
                        game_battlesnake_id,
                    },
                )
                .collect(),
        };

        let mut tx = pool.begin().await?;
        let applied = WengLinScoring.process_game_result(&mut tx, &event).await?;
        assert!(
            !applied,
            "re-processing a rated game must report already-applied"
        );
        tx.rollback().await?;

        let after: Vec<(Uuid, i32, i32, i32, f64)> = sqlx::query_as(
            "SELECT leaderboard_entry_id, games_played, first_place_finishes, \
                    non_first_finishes, mu \
             FROM leaderboard_entries WHERE leaderboard_entry_id = ANY($1) \
             ORDER BY leaderboard_entry_id",
        )
        .bind(&[a, b][..])
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            after, before,
            "the rolled-back second application changed nothing"
        );
        for id in [a, b] {
            assert_eq!(
                leaderboard::count_game_results_for_entry(&pool, id).await?,
                1
            );
        }
        Ok(())
    }

    /// What the doubled prod game did to one of its four entries.
    #[derive(Debug, Clone, Copy)]
    struct DoubledEntrySpec {
        placement: i32,
        /// Food recorded on the doubled game's game_battlesnakes row
        /// (None = left NULL: the snake ate nothing / was never recorded).
        /// i32 because game_battlesnakes.food_eaten and
        /// leaderboard_game_results.food_eaten are both INT (Int4 in the
        /// sqlx macros); convert with i64::from when binding into the
        /// BIGINT food_score corruption macro.
        food_eaten: Option<i32>,
        /// Whether the simulated second application drifts this entry.
        /// In the 5fe9fddf fixture the placement-4 snake is left healthy
        /// as an in-game control; in d02f3e8a all four drifted.
        corrupted: bool,
    }

    /// Shared body of the repair-migration regression tests (DEV-2174 and
    /// DEV-2218). Recreates a doubled prod game under its real
    /// leaderboard_game_id, corrupts the counters by simulating the second
    /// application, then runs the real migration file and asserts every
    /// counter returns to the single-application snapshot, that the repair
    /// is idempotent, and that all five DEV-2174 invariants hold. a and b
    /// carry a prior opposite-outcome game so their scores are 50% (not the
    /// degenerate 100%/0% of a single-game entry) and the score assertions
    /// can actually fail.
    async fn assert_migration_backs_out_second_application(
        pool: &PgPool,
        migration_sql: &str,
        doubled_game_id: Uuid,
        specs: &[DoubledEntrySpec; 4],
    ) -> cja::Result<()> {
        let user_id = create_user(pool, 21742).await?;
        let mut snake_ids = Vec::new();
        for name in [
            "repair-a", "repair-b", "repair-c", "repair-d", "repair-e", "repair-f",
        ] {
            snake_ids.push(create_snake(pool, user_id, name).await?);
        }
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            "Repair Board",
        )
        .fetch_one(pool)
        .await?;
        let mut ids = Vec::new();
        for snake_id in &snake_ids {
            ids.push(
                leaderboard::get_or_create_entry(pool, leaderboard_id, *snake_id)
                    .await?
                    .expect("live snake joins")
                    .leaderboard_entry_id,
            );
        }
        let [a, b, c, d, e, f] = [ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]];

        assert_eq!(
            specs.iter().filter(|s| s.placement == 1).count(),
            1,
            "fixture needs a single outright winner"
        );
        assert!(
            specs[0].corrupted && specs[1].corrupted,
            "prior-game entries must be corrupted so the score check bites"
        );

        // A prior game with the OPPOSITE outcome for a and b (b beat a),
        // rated through update_ratings. Without it, every doubled-game
        // entry has played exactly one game: a sits at 100% and b at 0%
        // both before and after the double-apply, so `after == before` on
        // score would pass even if the migration never recomputed it. With
        // the prior game a is at 50% before / 66.67% corrupted / 50%
        // repaired, and b at 50% / 33.33% / 50% — the score check bites.
        // No food is set on this game, so every food value in the DB got
        // there through update_ratings (prior rows stay NULL — food_eaten
        // is NULL by default and FoodEatenScoring skips NULL/0). a's and
        // b's result-row count is now 2, so the migration's
        // `games_played = count + 1` guard still matches after corruption
        // (3 = 2 + 1).
        let prior = finished_game(pool, leaderboard_id, &[(b, 1), (a, 2)]).await?;

        // The doubled prod game, recreated with its real leaderboard_game_id
        // so the migration's keyed guards match it: placements and food
        // come from the specs (the prod evidence for that game).
        finished_game_with_lb_game_id(
            pool,
            leaderboard_id,
            doubled_game_id,
            &[
                (a, specs[0].placement),
                (b, specs[1].placement),
                (c, specs[2].placement),
                (d, specs[3].placement),
            ],
        )
        .await?;
        for (i, spec) in specs.iter().enumerate() {
            if let Some(food) = spec.food_eaten {
                // Scoped to the doubled game's game_id: a and b each also
                // have a prior-game game_battlesnakes row, and an unscoped
                // UPDATE would hand-set food on that row too.
                sqlx::query!(
                    "UPDATE game_battlesnakes SET food_eaten = $2 \
                     WHERE leaderboard_entry_id = $1 \
                       AND game_id = (SELECT game_id FROM leaderboard_games \
                                      WHERE leaderboard_game_id = $3)",
                    ids[i],
                    food,
                    doubled_game_id,
                )
                .execute(pool)
                .await?;
            }
        }
        // A healthy game elsewhere: e wins and eats 5, f loses.
        let healthy = finished_game(pool, leaderboard_id, &[(e, 1), (f, 2)]).await?;
        sqlx::query!(
            "UPDATE game_battlesnakes SET food_eaten = 5 WHERE leaderboard_entry_id = $1",
            e,
        )
        .execute(pool)
        .await?;

        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        scoring.register(Box::new(FoodEatenScoring));
        state.scoring = std::sync::Arc::new(scoring);
        update_ratings(&state, prior).await?;
        update_ratings(&state, doubled_game_id).await?;
        update_ratings(&state, healthy).await?;

        // Every counter the double-apply bumped, for one entry. Compared by
        // exact equality — the repaired score is recomputed with the same
        // double-precision expression the writer uses, over the same values.
        #[derive(Debug, Clone, Copy, PartialEq)]
        struct Counters {
            games_played: i32,
            first_place_finishes: i32,
            non_first_finishes: i32,
            wr_games_played: i32,
            wins: i32,
            losses: i32,
            score: f64,
            food_score: Option<i64>,
        }

        #[derive(Debug, sqlx::FromRow)]
        struct CounterRow {
            leaderboard_entry_id: Uuid,
            games_played: i32,
            first_place_finishes: i32,
            non_first_finishes: i32,
            wr_games_played: i32,
            wins: i32,
            losses: i32,
            score: f64,
            food_score: Option<i64>,
        }

        async fn read_counters(
            pool: &PgPool,
            ids: &[Uuid],
        ) -> cja::Result<std::collections::HashMap<Uuid, Counters>> {
            let rows: Vec<CounterRow> = sqlx::query_as(
                "SELECT le.leaderboard_entry_id, le.games_played, \
                            le.first_place_finishes, le.non_first_finishes, \
                            wr.games_played AS wr_games_played, wr.wins, wr.losses, wr.score, fe.food_score \
                     FROM leaderboard_entries le \
                     JOIN win_rate_stats wr USING (leaderboard_entry_id) \
                     LEFT JOIN food_eaten_stats fe USING (leaderboard_entry_id) \
                     WHERE le.leaderboard_entry_id = ANY($1)",
            )
            .bind(ids)
            .fetch_all(pool)
            .await?;
            Ok(rows
                .into_iter()
                .map(|row| {
                    (
                        row.leaderboard_entry_id,
                        Counters {
                            games_played: row.games_played,
                            first_place_finishes: row.first_place_finishes,
                            non_first_finishes: row.non_first_finishes,
                            wr_games_played: row.wr_games_played,
                            wins: row.wins,
                            losses: row.losses,
                            score: row.score,
                            food_score: row.food_score,
                        },
                    )
                })
                .collect())
        }

        // Before the double-apply: a and b are each 1W-1L (50%), c/d lost
        // the doubled game, e won the healthy game.
        let before = read_counters(pool, &ids).await?;

        // Simulate main's double-apply on the corrupted entries only (a
        // non-corrupted doubled-game entry stays healthy as the in-game
        // consistent control; e, f are the other-game controls). This is
        // what the second concurrent run wrote before its result inserts
        // were swallowed by ON CONFLICT DO NOTHING.
        let winners: Vec<Uuid> = specs
            .iter()
            .zip(&ids)
            .filter(|(s, _)| s.corrupted && s.placement == 1)
            .map(|(_, id)| *id)
            .collect();
        let losers: Vec<Uuid> = specs
            .iter()
            .zip(&ids)
            .filter(|(s, _)| s.corrupted && s.placement != 1)
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(winners.len(), 1, "fixture must corrupt the outright winner");
        sqlx::query!(
            "UPDATE leaderboard_entries SET games_played = games_played + 1, \
             first_place_finishes = first_place_finishes + 1 \
             WHERE leaderboard_entry_id = $1",
            winners[0],
        )
        .execute(pool)
        .await?;
        sqlx::query!(
            "UPDATE leaderboard_entries SET games_played = games_played + 1, \
             non_first_finishes = non_first_finishes + 1 \
             WHERE leaderboard_entry_id = ANY($1)",
            &losers[..],
        )
        .execute(pool)
        .await?;
        sqlx::query!(
            "UPDATE win_rate_stats SET games_played = games_played + 1, wins = wins + 1, \
             score = (wins + 1)::double precision / (games_played + 1)::double precision * 100.0, \
             updated_at = NOW() WHERE leaderboard_entry_id = $1",
            winners[0],
        )
        .execute(pool)
        .await?;
        sqlx::query!(
            "UPDATE win_rate_stats SET games_played = games_played + 1, losses = losses + 1, \
             score = wins::double precision / (games_played + 1)::double precision * 100.0, \
             updated_at = NOW() WHERE leaderboard_entry_id = ANY($1)",
            &losers[..],
        )
        .execute(pool)
        .await?;
        for (i, spec) in specs.iter().enumerate() {
            if let Some(food) = spec.food_eaten {
                if spec.corrupted {
                    sqlx::query!(
                        "UPDATE food_eaten_stats SET food_score = food_score + $2 \
                         WHERE leaderboard_entry_id = $1",
                        ids[i],
                        i64::from(food),
                    )
                    .execute(pool)
                    .await?;
                }
            }
        }

        let corrupted = read_counters(pool, &ids).await?;
        // The simulation must really have moved every corrupted counter —
        // otherwise the repair assertions below are vacuous.
        for (i, spec) in specs.iter().enumerate() {
            if spec.corrupted {
                assert_ne!(
                    corrupted[&ids[i]], before[&ids[i]],
                    "corruption applied to {}",
                    ids[i]
                );
            }
        }
        // Score must have moved too (a: 66.67% vs 50%; b: 33.33% vs 50%),
        // or the migration's score recomputation could never fail the
        // final equality check.
        assert_ne!(corrupted[&a].score, before[&a].score, "a's score moved");
        assert_ne!(corrupted[&b].score, before[&b].score, "b's score moved");
        for (i, spec) in specs.iter().enumerate() {
            if !spec.corrupted {
                assert_eq!(
                    corrupted[&ids[i]], before[&ids[i]],
                    "in-game control {} untouched by corruption",
                    ids[i]
                );
            }
        }
        assert_eq!(
            corrupted[&e], before[&e],
            "other-game control e untouched by corruption"
        );

        // The REAL migration file — not a copy.
        sqlx::raw_sql(migration_sql).execute(pool).await?;

        let after = read_counters(pool, &ids).await?;
        assert_eq!(
            after, before,
            "every counter repaired to the single-application snapshot"
        );

        // Idempotent: a second run changes nothing.
        sqlx::raw_sql(migration_sql).execute(pool).await?;
        let again = read_counters(pool, &ids).await?;
        assert_eq!(again, before, "repair is idempotent");

        // The tasks' success-criteria invariants hold for all six entries.
        let drifted_entries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM leaderboard_entries le \
             WHERE le.leaderboard_entry_id = ANY($1) \
               AND le.games_played <> (SELECT count(*) FROM leaderboard_game_results r \
                                       WHERE r.leaderboard_entry_id = le.leaderboard_entry_id)",
        )
        .bind(&ids[..])
        .fetch_one(pool)
        .await?;
        assert_eq!(
            drifted_entries, 0,
            "leaderboard_entries.games_played = count(results)"
        );

        let drifted_wr: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM win_rate_stats wr \
             WHERE wr.leaderboard_entry_id = ANY($1) \
               AND wr.games_played <> (SELECT count(*) FROM leaderboard_game_results r \
                                       WHERE r.leaderboard_entry_id = wr.leaderboard_entry_id)",
        )
        .bind(&ids[..])
        .fetch_one(pool)
        .await?;
        assert_eq!(
            drifted_wr, 0,
            "win_rate_stats.games_played = count(results)"
        );

        let drifted_split: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM leaderboard_entries le \
             WHERE le.leaderboard_entry_id = ANY($1) \
               AND le.games_played <> le.first_place_finishes + le.non_first_finishes",
        )
        .bind(&ids[..])
        .fetch_one(pool)
        .await?;
        assert_eq!(
            drifted_split, 0,
            "leaderboard_entries.games_played = first_place_finishes + non_first_finishes"
        );

        let drifted_wl: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM win_rate_stats wr \
             WHERE wr.leaderboard_entry_id = ANY($1) \
               AND wr.wins + wr.losses <> wr.games_played",
        )
        .bind(&ids[..])
        .fetch_one(pool)
        .await?;
        assert_eq!(drifted_wl, 0, "win_rate_stats.wins + losses = games_played");

        let drifted_food: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM leaderboard_entries le \
             LEFT JOIN food_eaten_stats fe USING (leaderboard_entry_id) \
             WHERE le.leaderboard_entry_id = ANY($1) \
               AND COALESCE(fe.food_score, 0) <> COALESCE( \
                     (SELECT SUM(r.food_eaten) FROM leaderboard_game_results r \
                      WHERE r.leaderboard_entry_id = le.leaderboard_entry_id), 0)",
        )
        .bind(&ids[..])
        .fetch_one(pool)
        .await?;
        assert_eq!(
            drifted_food, 0,
            "food_eaten_stats.food_score = sum(food_eaten) (missing row counts as 0)"
        );
        Ok(())
    }

    /// The repair migration backs out exactly the second application of the
    /// doubled game, on every counter, and is idempotent. Executes the real
    /// migration file, not a copy. a and b carry a prior opposite-outcome
    /// game so their scores are 50% (not the degenerate 100%/0% of a
    /// single-game entry) and the score assertions can actually fail.
    /// (DEV-2174's game; the body lives in the shared helper.)
    #[sqlx::test(migrations = "../migrations")]
    async fn repair_migration_backs_out_second_application_exactly(
        pool: PgPool,
    ) -> cja::Result<()> {
        assert_migration_backs_out_second_application(
            &pool,
            include_str!("../../migrations/20261107170600_repair_double_applied_ratings.up.sql"),
            Uuid::parse_str("5fe9fddf-5a77-4c88-9ddd-05a3fbf9e175")
                .wrap_err("DEV-2174 fixture UUID")?,
            &[
                DoubledEntrySpec {
                    placement: 1,
                    food_eaten: Some(10),
                    corrupted: true,
                },
                DoubledEntrySpec {
                    placement: 2,
                    food_eaten: Some(8),
                    corrupted: true,
                },
                DoubledEntrySpec {
                    placement: 3,
                    food_eaten: Some(1),
                    corrupted: true,
                },
                DoubledEntrySpec {
                    placement: 4,
                    food_eaten: None,
                    corrupted: false,
                },
            ],
        )
        .await
    }

    /// DEV-2218: the second doubled prod game, d02f3e8a-e8fe-47fd-87d1-
    /// e2e2ac8a2256 (scored twice 2026-11-08T00:55:10Z, just before #322
    /// deployed). All four entries drifted this time — everyone ate
    /// (36/32/10/4) — so all four are corrupted; e and f remain the
    /// healthy-game controls.
    #[sqlx::test(migrations = "../migrations")]
    async fn second_repair_migration_backs_out_d02f3e8a_exactly(pool: PgPool) -> cja::Result<()> {
        assert_migration_backs_out_second_application(
            &pool,
            include_str!(
                "../../migrations/20261108020156_repair_second_double_applied_ratings.up.sql"
            ),
            Uuid::parse_str("d02f3e8a-e8fe-47fd-87d1-e2e2ac8a2256")
                .wrap_err("DEV-2218 fixture UUID")?,
            &[
                DoubledEntrySpec {
                    placement: 1,
                    food_eaten: Some(36),
                    corrupted: true,
                },
                DoubledEntrySpec {
                    placement: 2,
                    food_eaten: Some(32),
                    corrupted: true,
                },
                DoubledEntrySpec {
                    placement: 3,
                    food_eaten: Some(10),
                    corrupted: true,
                },
                DoubledEntrySpec {
                    placement: 4,
                    food_eaten: Some(4),
                    corrupted: true,
                },
            ],
        )
        .await
    }
}
