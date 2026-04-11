use color_eyre::eyre::Context as _;
use rules::Direction;
use std::collections::HashMap;
use uuid::Uuid;

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

    // Build customization map and update DB records
    let mut customizations: HashMap<String, SnakeCustomizations> = HashMap::new();
    for bs in &battlesnakes {
        let snake_id = bs.game_battlesnake_id.to_string();
        if let Some(info) = info_results.get(&snake_id) {
            let color = info
                .customizations
                .as_ref()
                .map(|c| c.color.clone())
                .or_else(|| info.color.clone())
                .unwrap_or_default();
            let head = info
                .customizations
                .as_ref()
                .map(|c| c.head.clone())
                .or_else(|| info.head.clone())
                .unwrap_or_default();
            let tail = info
                .customizations
                .as_ref()
                .map(|c| c.tail.clone())
                .or_else(|| info.tail.clone())
                .unwrap_or_default();

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
                    cause: snake.eliminated_cause.as_str().to_string(),
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
    )
    .await;

    tracing::info!(
        game_id = %game_id,
        final_turn = engine_game.board.turn,
        "Game completed with persistence"
    );

    // Build placements: last eliminated = winner (placement 1)
    // Snakes still alive at the end go first
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

    // Assign placements to database
    for (i, snake_id) in placements.iter().enumerate() {
        let placement = (i + 1) as i32;

        let game_battlesnake_id: Uuid = snake_id
            .parse()
            .wrap_err_with(|| format!("Invalid game_battlesnake ID: {}", snake_id))?;

        crate::models::game_battlesnake::set_game_result_by_id(
            pool,
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

    // Update status to finished
    update_game_status(pool, game_id, GameStatus::Finished).await?;

    tracing::info!(
        event_type = "game_completed",
        game_id = %game_id,
        final_turn = engine_game.board.turn,
        total_ms = total_time_ms,
        winner_battlesnake_id = ?placements.first(),
        "game completed"
    );

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

    // Clean up game channel (will be removed when no subscribers)
    game_channels.cleanup(game_id).await;

    Ok(())
}
