use color_eyre::eyre::Context as _;
use rules::{Direction, EliminationCause};
use std::collections::HashMap;
use uuid::Uuid;

use crate::customizations;
use crate::engine::MAX_TURNS;
use crate::engine::frame::{DeathInfo, SnakeCustomizations, game_to_frame};
use crate::models::game::{GameStatus, get_game_by_id, update_game_status};
use crate::snake_client::{request_end_parallel, request_moves_parallel, request_start_parallel};
use crate::state::AppState;
use crate::wire;

/// Run a game with turn-by-turn DB persistence and WebSocket notifications
///
/// This function calls the actual snake APIs to get moves, with timeout handling.
/// On timeout, snakes continue in the same direction as their last move.
pub async fn run_game(app_state: &AppState, game_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;
    let game_channels = &app_state.game_channels;
    let http_client = &app_state.http_client;

    tracing::info!(game_id = %game_id, "Starting run_game");

    // Get the game details
    let game = get_game_by_id(pool, game_id)
        .await?
        .ok_or_else(|| cja::color_eyre::eyre::eyre!("Game not found"))?;

    // Re-entrancy for retries and crash recovery: run_game always plays a
    // game from turn 0, so a retry must never blindly re-run on top of a
    // previous attempt's state.
    match game.status {
        GameStatus::Finished => {
            // A previous attempt finished this game but may have died before
            // enqueueing the follow-up jobs. Everything past the finish
            // transaction is idempotent, so just re-run the post-completion
            // hooks and stop.
            tracing::info!(
                game_id = %game_id,
                "Game already finished; re-running post-completion hooks only"
            );
            enqueue_post_completion_jobs(app_state, game_id).await?;
            game_channels.cleanup(game_id).await;
            return Ok(());
        }
        GameStatus::Running => {
            // A previous attempt crashed mid-game or died before the atomic
            // finish committed. Wipe its partial state (turns, placements)
            // so the re-run starts from a clean turn 0 instead of dying on
            // the (game_id, turn_number) unique constraint.
            tracing::warn!(
                game_id = %game_id,
                "Game was already running; resetting partial state for a clean re-run"
            );
            crate::models::game::reset_game_state_for_retry(pool, game_id).await?;
        }
        GameStatus::Waiting => {}
    }

    // Emit queue_wait metric if enqueued_at is available
    if let Some(enqueued_at) = game.enqueued_at {
        let queue_wait = chrono::Utc::now().signed_duration_since(enqueued_at);
        tracing::info!(
            metric_type = "queue_wait",
            game_id = %game_id,
            duration_ms = queue_wait.num_milliseconds(),
            "game queue wait time"
        );
    }

    // Update status to running
    update_game_status(pool, game_id, GameStatus::Running).await?;

    // Get all the battlesnakes in the game with their URLs
    let battlesnakes = crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id)
        .await
        .wrap_err("Failed to get battlesnakes for game")?;

    tracing::info!(
        event_type = "game_started",
        game_id = %game_id,
        board_size = game.board_size.as_str(),
        game_type = game.game_type.as_str(),
        snake_count = battlesnakes.len(),
        "game started"
    );

    if battlesnakes.is_empty() {
        return Err(cja::color_eyre::eyre::eyre!("No battlesnakes in the game"));
    }

    // Build snake_id -> url mapping using game_battlesnake_id as the key
    // This ensures uniqueness when the same battlesnake appears multiple times
    let snake_urls: Vec<(String, String)> = battlesnakes
        .iter()
        .map(|bs| (bs.game_battlesnake_id.to_string(), bs.url.clone()))
        .collect();

    // Fetch snake customizations from all root endpoints in parallel (1s timeout)
    let info_timeout = std::time::Duration::from_millis(1000);
    let info_results =
        crate::snake_client::request_info_parallel(http_client, &snake_urls, info_timeout).await;

    // Build customization map and update DB records. Declared head/tail are
    // honored only if the snake's owner is allowed to use them (free, or
    // granted); anything else falls back to the default.
    let mut customizations: HashMap<String, SnakeCustomizations> = HashMap::new();
    for bs in &battlesnakes {
        let snake_id = bs.game_battlesnake_id.to_string();
        if let Some(info) = info_results.get(&snake_id) {
            let color = customizations::normalize_color(
                &info
                    .customizations
                    .as_ref()
                    .map(|c| c.color.clone())
                    .or_else(|| info.color.clone())
                    .unwrap_or_default(),
            );
            let declared_head = info
                .customizations
                .as_ref()
                .map(|c| c.head.clone())
                .or_else(|| info.head.clone())
                .unwrap_or_default();
            let declared_tail = info
                .customizations
                .as_ref()
                .map(|c| c.tail.clone())
                .or_else(|| info.tail.clone())
                .unwrap_or_default();
            let head = customizations::resolve_head(pool, bs.user_id, &declared_head).await?;
            let tail = customizations::resolve_tail(pool, bs.user_id, &declared_tail).await?;

            if let Err(e) = crate::models::battlesnake::update_battlesnake_customizations(
                pool,
                bs.battlesnake_id,
                &color,
                &head,
                &tail,
            )
            .await
            {
                tracing::warn!(
                    battlesnake_id = %bs.battlesnake_id,
                    error = %e,
                    "Failed to persist battlesnake customizations"
                );
            }

            customizations.insert(snake_id, SnakeCustomizations { color, head, tail });
        }
    }

    // Create the initial game state
    let mut engine_game =
        crate::engine::create_initial_game(game_id, game.board_size, game.game_type, &battlesnakes);

    // Get timeout from game settings
    let timeout = std::time::Duration::from_millis(engine_game.meta.timeout as u64);

    let mut death_info: Vec<DeathInfo> = Vec::new();
    let mut elimination_order: Vec<String> = Vec::new();
    let mut last_moves: HashMap<String, Direction> = HashMap::new();
    let mut snake_contexts: HashMap<String, wire::SnakeContext> = HashMap::new();

    // Call /start for all snakes in parallel (fire and forget)
    tracing::info!(game_id = %game_id, "Calling /start for all snakes");
    request_start_parallel(
        http_client,
        &engine_game,
        &snake_urls,
        timeout,
        &snake_contexts,
        &customizations,
    )
    .await;

    // Store turn 0 (initial state, no moves yet)
    let frame_0 = game_to_frame(&engine_game, &death_info, &[], &customizations);
    let frame_0_json =
        serde_json::to_value(&frame_0).wrap_err("Failed to serialize initial frame")?;

    tracing::info!(game_id = %game_id, "Storing turn 0");
    crate::models::turn::create_turn(pool, game_channels, game_id, 0, Some(frame_0_json)).await?;
    tracing::info!(game_id = %game_id, "Turn 0 stored successfully");

    // Track timing for processing_overhead metric
    let game_start = std::time::Instant::now();
    let mut total_snake_wait_ms: i64 = 0;

    // Run the game turn by turn
    while !crate::engine::is_game_over(&engine_game) && engine_game.board.turn < MAX_TURNS {
        // Request moves from all alive snakes in parallel
        let move_results = request_moves_parallel(
            http_client,
            &engine_game,
            &snake_urls,
            timeout,
            &last_moves,
            &snake_contexts,
            &customizations,
        )
        .await;

        // Accumulate snake wait time from latency measurements
        for result in &move_results {
            if let Some(latency) = result.latency_ms {
                total_snake_wait_ms += latency;
            }
        }

        // Convert to move vector for engine
        let moves: Vec<(String, Direction)> = move_results
            .iter()
            .map(|r| (r.snake_id.clone(), r.direction))
            .collect();

        // Store last moves for timeout fallback on next turn
        for result in &move_results {
            last_moves.insert(result.snake_id.clone(), result.direction);
        }

        // Update snake_contexts for NEXT turn
        snake_contexts.clear();
        for result in &move_results {
            snake_contexts.insert(
                result.snake_id.clone(),
                wire::SnakeContext {
                    latency_ms: result.latency_ms,
                    shout: result.shout.clone(),
                },
            );
        }

        // Apply the moves using the engine
        crate::engine::apply_turn(&mut engine_game, &moves);
        engine_game.board.turn += 1;

        // Track newly eliminated snakes
        for snake in &engine_game.board.snakes {
            if snake.eliminated_cause.is_eliminated() && !elimination_order.contains(&snake.id) {
                elimination_order.push(snake.id.clone());
                death_info.push(DeathInfo {
                    snake_id: snake.id.clone(),
                    turn: engine_game.board.turn,
                    cause: elimination_cause_label(&snake.eliminated_cause),
                    eliminated_by: snake.eliminated_by.clone(),
                });
            }
        }

        // Store the turn frame with latency info and notify subscribers
        let frame = game_to_frame(&engine_game, &death_info, &move_results, &customizations);
        let frame_json = serde_json::to_value(&frame)
            .wrap_err_with(|| format!("Failed to serialize frame {}", engine_game.board.turn))?;

        // Measure DB write latency
        let db_write_start = std::time::Instant::now();

        tracing::debug!(game_id = %game_id, turn = engine_game.board.turn, "Storing turn");
        let turn = crate::models::turn::create_turn(
            pool,
            game_channels,
            game_id,
            engine_game.board.turn,
            Some(frame_json),
        )
        .await?;

        // Store individual snake moves with latency
        for result in &move_results {
            if let Ok(game_battlesnake_id) = Uuid::parse_str(&result.snake_id) {
                crate::models::turn::create_snake_turn(
                    pool,
                    turn.turn_id,
                    game_battlesnake_id,
                    &result.direction.to_string(),
                    result.latency_ms,
                    result.timed_out,
                )
                .await?;
            }
        }

        let db_write_duration = db_write_start.elapsed();
        tracing::info!(
            metric_type = "db_write_latency",
            game_id = %game_id,
            turn = engine_game.board.turn,
            duration_ms = db_write_duration.as_millis() as u64,
            "turn persistence latency"
        );

        // Measure async scheduler jitter
        let before_yield = std::time::Instant::now();
        tokio::task::yield_now().await;
        let yield_duration = before_yield.elapsed();
        tracing::info!(
            metric_type = "scheduler_jitter",
            game_id = %game_id,
            turn = engine_game.board.turn,
            duration_us = yield_duration.as_micros() as u64,
            "async scheduler jitter"
        );
    }

    // Emit processing_overhead metric
    let total_time = game_start.elapsed();
    let total_time_ms = total_time.as_millis() as i64;
    let overhead_ms = total_time_ms - total_snake_wait_ms;
    tracing::info!(
        metric_type = "processing_overhead",
        game_id = %game_id,
        duration_ms = overhead_ms,
        total_ms = total_time_ms,
        snake_wait_ms = total_snake_wait_ms,
        "game processing overhead"
    );

    // Call /end for all snakes in parallel (fire and forget)
    tracing::info!(game_id = %game_id, "Calling /end for all snakes");
    request_end_parallel(
        http_client,
        &engine_game,
        &snake_urls,
        timeout,
        &snake_contexts,
        &customizations,
    )
    .await;

    tracing::info!(
        game_id = %game_id,
        final_turn = engine_game.board.turn,
        "Game completed with persistence"
    );

    // Build placements: last eliminated = winner (placement 1)
    // Snakes still alive at the end go first
    //
    // Note: placements cannot express ties. Snakes eliminated on the same
    // turn (e.g. a head-to-head where both die) still get distinct
    // placements in elimination order, so the game page can show a "winner"
    // for a game the tournament layer records as a tie. We keep it that way
    // because the rating pipeline can't express ties either: win_rate counts
    // exactly `placement == 1` as a win, so sharing placement 1 would credit
    // both snakes of a drawn game with a win. Tournament tie handling
    // instead derives the real result from the final snake states
    // (`game_winner_from_snakes`).
    let mut placements: Vec<String> = engine_game
        .board
        .snakes
        .iter()
        .filter(|s| !s.eliminated_cause.is_eliminated())
        .map(|s| s.id.clone())
        .collect();

    // Then add eliminated snakes in reverse order (last eliminated = better placement)
    elimination_order.reverse();
    placements.extend(elimination_order);

    // Resolve the tournament match result (if any) before the finish
    // transaction. Placements can't express ties, so the winner is derived
    // from the final snake states.
    let match_winner_snake_id =
        crate::tournament_match::game_winner_from_snakes(&engine_game.board.snakes);
    let resolved_match_game = crate::tournament_match::resolve_finished_match_game(
        pool,
        game_id,
        match_winner_snake_id.as_deref(),
    )
    .await
    .wrap_err("Failed to resolve tournament match game result")?;

    // Atomic finish: placements, the Finished status flip, and the
    // tournament match result all commit together, so a Finished game ALWAYS
    // has its match_games winner recorded (`winner_id` NULL on a finished
    // game unambiguously means a tie) — `run_match` relies on that
    // invariant. Follow-up jobs are enqueued after the commit because cja's
    // enqueue only takes a pool; if we die in between, the retry's
    // Finished short-circuit above and the stuck-match sweeper cron are the
    // safety nets that re-enqueue them.
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start game finish transaction")?;

    for (i, snake_id) in placements.iter().enumerate() {
        let placement = (i + 1) as i32;

        let game_battlesnake_id: Uuid = snake_id
            .parse()
            .wrap_err_with(|| format!("Invalid game_battlesnake ID: {}", snake_id))?;

        crate::models::game_battlesnake::set_game_result_by_id(
            &mut tx,
            game_battlesnake_id,
            placement,
        )
        .await
        .wrap_err_with(|| {
            format!(
                "Failed to set game result for game_battlesnake {}",
                game_battlesnake_id
            )
        })?;
    }

    crate::models::game::update_game_status_tx(&mut tx, game_id, GameStatus::Finished).await?;

    if let Some(resolved) = &resolved_match_game {
        crate::models::tournament::set_match_game_winner(
            &mut *tx,
            resolved.match_game_id,
            resolved.winner_battlesnake_id,
        )
        .await
        .wrap_err("Failed to record tournament match game result")?;

        tracing::info!(
            game_id = %game_id,
            match_id = %resolved.match_id,
            winner_battlesnake_id = ?resolved.winner_battlesnake_id,
            "Recording tournament match game result"
        );
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit game finish transaction")?;

    tracing::info!(
        event_type = "game_completed",
        game_id = %game_id,
        final_turn = engine_game.board.turn,
        total_ms = total_time_ms,
        winner_battlesnake_id = ?placements.first(),
        "game completed"
    );

    enqueue_post_completion_jobs(app_state, game_id).await?;

    // Clean up game channel (will be removed when no subscribers)
    game_channels.cleanup(game_id).await;

    Ok(())
}

/// Enqueue the follow-up jobs for a finished game: the leaderboard rating
/// update and the tournament match evaluation, as applicable.
///
/// Called after the finish transaction commits, and again by retries that
/// find the game already finished. Both targets are idempotent (the rating
/// job checks for already-applied results; match evaluation is re-entrant),
/// so duplicate enqueues are harmless.
async fn enqueue_post_completion_jobs(app_state: &AppState, game_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;

    // Check if this is a leaderboard game and enqueue rating update
    if let Some(lb_game) =
        crate::models::leaderboard::find_leaderboard_game_by_game_id(pool, game_id).await?
    {
        let job = crate::jobs::LeaderboardRatingUpdateJob {
            leaderboard_game_id: lb_game.leaderboard_game_id,
        };
        cja::jobs::Job::enqueue(
            job,
            app_state.clone(),
            format!("Rate leaderboard game {game_id}"),
        )
        .await
        .wrap_err("Failed to enqueue leaderboard rating update job")?;

        tracing::info!(
            game_id = %game_id,
            leaderboard_game_id = %lb_game.leaderboard_game_id,
            "Enqueued leaderboard rating update"
        );
    }

    // Check if this game belongs to a tournament match and re-enqueue the
    // match evaluation (the winner is already recorded on the match_games
    // row by the finish transaction).
    if let Some(match_game) =
        crate::models::tournament::find_match_game_by_game_id(pool, game_id).await?
    {
        cja::jobs::Job::enqueue(
            crate::jobs::RunMatchJob {
                match_id: match_game.match_id,
            },
            app_state.clone(),
            format!("Game {game_id} finished for match {}", match_game.match_id),
        )
        .await
        .wrap_err("Failed to enqueue match evaluation after game completion")?;

        tracing::info!(
            game_id = %game_id,
            match_id = %match_game.match_id,
            "Enqueued tournament match evaluation"
        );
    }

    Ok(())
}

/// Human-readable label for an elimination cause, used in frame data.
fn elimination_cause_label(cause: &EliminationCause) -> String {
    match cause {
        EliminationCause::NotEliminated => String::new(),
        EliminationCause::OutOfHealth => "out-of-health".to_string(),
        EliminationCause::OutOfBounds => "wall-collision".to_string(),
        EliminationCause::SelfCollision => "self-collision".to_string(),
        EliminationCause::Collision => "snake-collision".to_string(),
        EliminationCause::HeadToHeadCollision => "head-collision".to_string(),
        EliminationCause::Hazard => "hazard".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    async fn count_jobs(pool: &PgPool, name: &str) -> cja::Result<i64> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE name = $1")
                .bind(name)
                .fetch_one(pool)
                .await?,
        )
    }

    /// Insert a bare game row (no snakes) with the given status.
    async fn fixture_game(pool: &PgPool, status: &str) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', $1) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    /// A retry on an already-finished game must short-circuit to the
    /// (idempotent) post-completion hooks instead of re-running the game.
    /// The fixture game has no battlesnakes, so reaching the normal run path
    /// would fail loudly — returning Ok proves the short-circuit.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_game_short_circuits_to_post_completion_hooks(
        pool: PgPool,
    ) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        run_game(&app_state, game_id).await?;

        // Not a leaderboard or tournament game: nothing to enqueue.
        assert_eq!(count_jobs(&pool, "LeaderboardRatingUpdateJob").await?, 0);
        assert_eq!(count_jobs(&pool, "RunMatchJob").await?, 0);

        Ok(())
    }

    /// The finished-game short-circuit re-enqueues match evaluation for
    /// tournament games, covering a crash between the finish transaction and
    /// the original enqueue.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_tournament_game_reenqueues_match_evaluation(pool: PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        // Minimal tournament scaffolding for a match_games row.
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (424242, 'test-user', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let tournament_id: Uuid = sqlx::query_scalar(
            "INSERT INTO tournaments (name, user_id) VALUES ('t', $1) RETURNING tournament_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        let match_id: Uuid = sqlx::query_scalar(
            "INSERT INTO tournament_matches (tournament_id, round, position, visual_column, visual_row)
             VALUES ($1, 1, 0, 0, 0) RETURNING match_id",
        )
        .bind(tournament_id)
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO match_games (match_id, game_id, game_number) VALUES ($1, $2, 1)")
            .bind(match_id)
            .bind(game_id)
            .execute(&pool)
            .await?;

        run_game(&app_state, game_id).await?;

        assert_eq!(count_jobs(&pool, "RunMatchJob").await?, 1);

        Ok(())
    }

    /// Resetting a crashed run must clear everything the next attempt would
    /// trip over: turns (and their snake_turns) plus partial placements.
    #[sqlx::test(migrations = "../migrations")]
    async fn reset_clears_turns_and_placements(pool: PgPool) -> cja::Result<()> {
        let game_id = fixture_game(&pool, "running").await?;

        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (424242, 'test-user', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'snake', 'http://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        let game_battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement)
             VALUES ($1, $2, 1) RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(battlesnake_id)
        .fetch_one(&pool)
        .await?;
        let turn_id: Uuid = sqlx::query_scalar(
            "INSERT INTO turns (game_id, turn_number) VALUES ($1, 0) RETURNING turn_id",
        )
        .bind(game_id)
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction)
             VALUES ($1, $2, 'up')",
        )
        .bind(turn_id)
        .bind(game_battlesnake_id)
        .execute(&pool)
        .await?;

        crate::models::game::reset_game_state_for_retry(&pool, game_id).await?;

        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(turns, 0);
        let snake_turns: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM snake_turns WHERE game_battlesnake_id = $1")
                .bind(game_battlesnake_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(snake_turns, 0);
        let placement: Option<i32> = sqlx::query_scalar(
            "SELECT placement FROM game_battlesnakes WHERE game_battlesnake_id = $1",
        )
        .bind(game_battlesnake_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(placement, None);

        Ok(())
    }
}
