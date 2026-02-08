use color_eyre::eyre::Context as _;
use std::collections::HashMap;
use uuid::Uuid;

use battlesnake_game_types::types::Move;

use crate::engine::MAX_TURNS;
use crate::engine::frame::{DeathInfo, game_to_frame};
use crate::models::game::{GameStatus, get_game_by_id, update_game_status};
use crate::snake_client::{request_end_parallel, request_moves_parallel, request_start_parallel};
use crate::state::AppState;

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

    // Create the initial game state
    let mut engine_game =
        crate::engine::create_initial_game(game_id, game.board_size, game.game_type, &battlesnakes);

    // Get timeout from game settings (default 500ms)
    let timeout = std::time::Duration::from_millis(engine_game.game.timeout as u64);

    // Call /start for all snakes in parallel (fire and forget)
    tracing::info!(game_id = %game_id, "Calling /start for all snakes");
    request_start_parallel(http_client, &engine_game, &snake_urls, timeout).await;

    let mut death_info: Vec<DeathInfo> = Vec::new();
    let mut elimination_order: Vec<String> = Vec::new();
    let mut last_moves: HashMap<String, Move> = HashMap::new();

    // Helper to check if game is over
    let is_game_over = |g: &battlesnake_game_types::wire_representation::Game| {
        g.board.snakes.iter().filter(|s| s.health > 0).count() <= 1
    };

    // Store turn 0 (initial state, no moves yet)
    let frame_0 = game_to_frame(&engine_game, &death_info, &[]);
    let frame_0_json =
        serde_json::to_value(&frame_0).wrap_err("Failed to serialize initial frame")?;

    tracing::info!(game_id = %game_id, "Storing turn 0");
    crate::models::turn::create_turn(pool, game_channels, game_id, 0, Some(frame_0_json)).await?;
    tracing::info!(game_id = %game_id, "Turn 0 stored successfully");

    // Track timing for processing_overhead metric
    let game_start = std::time::Instant::now();
    let mut total_snake_wait_ms: i64 = 0;

    // Run the game turn by turn
    while !is_game_over(&engine_game) && engine_game.turn < MAX_TURNS {
        // Request moves from all alive snakes in parallel
        let move_results =
            request_moves_parallel(http_client, &engine_game, &snake_urls, timeout, &last_moves)
                .await;

        // Accumulate snake wait time from latency measurements
        for result in &move_results {
            if let Some(latency) = result.latency_ms {
                total_snake_wait_ms += latency;
            }
        }

        // Convert to move vector for engine
        let moves: Vec<(String, Move)> = move_results
            .iter()
            .map(|r| (r.snake_id.clone(), r.direction))
            .collect();

        // Store last moves for timeout fallback on next turn
        for result in &move_results {
            last_moves.insert(result.snake_id.clone(), result.direction);
        }

        // Apply the moves using the engine
        engine_game = crate::engine::apply_turn(engine_game, &moves);
        engine_game.turn += 1;

        // Track newly eliminated snakes
        for snake in &engine_game.board.snakes {
            if snake.health <= 0 && !elimination_order.contains(&snake.id) {
                elimination_order.push(snake.id.clone());
                death_info.push(DeathInfo {
                    snake_id: snake.id.clone(),
                    turn: engine_game.turn,
                    cause: "eliminated".to_string(),
                    eliminated_by: String::new(),
                });
            }
        }

        // Store the turn frame with latency info and notify subscribers
        let frame = game_to_frame(&engine_game, &death_info, &move_results);
        let frame_json = serde_json::to_value(&frame)
            .wrap_err_with(|| format!("Failed to serialize frame {}", engine_game.turn))?;

        // Measure DB write latency
        let db_write_start = std::time::Instant::now();

        tracing::debug!(game_id = %game_id, turn = engine_game.turn, "Storing turn");
        let turn = crate::models::turn::create_turn(
            pool,
            game_channels,
            game_id,
            engine_game.turn,
            Some(frame_json),
        )
        .await?;

        // Store individual snake moves with latency
        // The snake_id in move_results is now the game_battlesnake_id (UUID string)
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
            turn = engine_game.turn,
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
            turn = engine_game.turn,
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
    request_end_parallel(http_client, &engine_game, &snake_urls, timeout).await;

    tracing::info!(
        game_id = %game_id,
        final_turn = engine_game.turn,
        "Game completed with persistence"
    );

    // Build placements: last eliminated = winner (placement 1)
    // Snakes still alive at the end go first
    let mut placements: Vec<String> = engine_game
        .board
        .snakes
        .iter()
        .filter(|s| s.health > 0)
        .map(|s| s.id.clone())
        .collect();

    // Then add eliminated snakes in reverse order (last eliminated = better placement)
    elimination_order.reverse();
    placements.extend(elimination_order);

    // Assign placements to database
    // snake_id is now game_battlesnake_id (unique per game instance)
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
        final_turn = engine_game.turn,
        total_ms = total_time_ms,
        winner_battlesnake_id = ?placements.first(),
        "game completed"
    );

    // Clean up game channel (will be removed when no subscribers)
    game_channels.cleanup(game_id).await;

    Ok(())
}
