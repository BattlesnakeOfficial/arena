use color_eyre::eyre::Context as _;
use rules::{Direction, EliminationCause};
use std::collections::HashMap;
use uuid::Uuid;

use crate::customizations;
use crate::engine::MAX_TURNS;
use crate::engine::frame::{DeathInfo, SnakeCustomizations, game_to_frame};
use crate::game_progress::{Phase, phase};
use crate::models::game::{
    GameStatus, StartClaim, claim_game_start, get_game_by_id, get_game_source,
};
use crate::snake_client::{
    ProxyClients, SnakeEndpoint, request_end_routed_parallel, request_info_routed_parallel,
    request_moves_routed_parallel, request_start_routed_parallel,
};
use crate::state::AppState;
use crate::wire;

/// Run a game with turn-by-turn DB persistence and WebSocket notifications
///
/// This function calls the actual snake APIs to get moves, with timeout handling.
/// On timeout, snakes continue in the same direction as their last move.
#[tracing::instrument(name = "arena.game", skip(app_state), fields(game_id = %game_id), err(Debug))]
pub async fn run_game(app_state: &AppState, game_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;
    let proxy_clients = ProxyClients {
        direct: &app_state.http_client,
        east: &app_state.proxy_east_client,
        europe: &app_state.proxy_europe_client,
        config: &app_state.config.engine_proxy,
    };

    tracing::info!(game_id = %game_id, "Starting run_game");

    // Get the game details
    let (game, game_source) = phase(game_id, Phase::LoadGame, None, async {
        let game = get_game_by_id(pool, game_id)
            .await?
            .ok_or_else(|| cja::color_eyre::eyre::eyre!("Game not found"))?;
        let source = get_game_source(pool, game_id).await?;
        Ok((game, source))
    })
    .await?;

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
            phase(
                game_id,
                Phase::PostCompletion,
                None,
                enqueue_post_completion_jobs(app_state, game_id),
            )
            .await?;
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
            phase(
                game_id,
                Phase::ResetGame,
                None,
                crate::models::game::reset_game_state_for_retry(pool, game_id),
            )
            .await?;
        }
        GameStatus::Waiting => {
            let claim = phase(game_id, Phase::ClaimStart, None, async {
                claim_game_start(pool, game_id, app_state.config.ladder_start_deadline_secs)
                    .await
                    .wrap_err("Failed to claim waiting game start")
            })
            .await?;
            match claim {
                StartClaim::Busy | StartClaim::AlreadyRunning | StartClaim::Terminal => {
                    return Ok(());
                }
                StartClaim::Started {
                    leaderboard_id,
                    wait_ms,
                    via,
                    enqueued_at,
                } => {
                    emit_start_events(game_id, leaderboard_id, wait_ms, via, enqueued_at);
                }
            }
        }
        GameStatus::Failed => {
            // Terminal: an operator (or the stuck-game sweeper) declared
            // this game dead. A straggling retry must not resurrect it —
            // its snakes may have moved on and its "live" window is long
            // gone.
            tracing::warn!(
                game_id = %game_id,
                "Game is marked failed; skipping re-run"
            );
            return Ok(());
        }
    }

    let (battlesnakes, snake_urls, customizations) =
        phase(game_id, Phase::PrepareSnakes, None, async {
            // Get all the battlesnakes in the game with their URLs
            let battlesnakes =
                crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id)
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
            let snake_urls: Vec<SnakeEndpoint> = battlesnakes
                .iter()
                .map(|bs| SnakeEndpoint {
                    snake_id: bs.game_battlesnake_id.to_string(),
                    url: bs.url.clone(),
                    engine_region: bs.engine_region,
                })
                .collect();

            // Fetch snake customizations from all root endpoints in parallel (1s timeout)
            let info_timeout = std::time::Duration::from_millis(1000);
            let info_results =
                request_info_routed_parallel(&proxy_clients, &snake_urls, info_timeout).await;

            // Build customization map and update DB records. Declared head/tail are
            // honored only if the snake's owner is allowed to use them (free, or
            // granted); anything else falls back to the default.
            let mut customizations: HashMap<String, SnakeCustomizations> = HashMap::new();
            for bs in &battlesnakes {
                let snake_id = bs.game_battlesnake_id.to_string();
                // The owner's public name always goes in, even when the snake's /info fetch
                // fails below: the board scoreboard renders "by {Author}" from frame
                // data, and empty visual fields fall back to defaults in
                // game_to_frame.
                customizations.insert(
                    snake_id.clone(),
                    SnakeCustomizations {
                        color: String::new(),
                        head: String::new(),
                        tail: String::new(),
                        author: bs.owner_name.clone(),
                    },
                );
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
                    let head =
                        customizations::resolve_head(pool, bs.user_id, &declared_head).await?;
                    let tail =
                        customizations::resolve_tail(pool, bs.user_id, &declared_tail).await?;

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

                    customizations.insert(
                        snake_id,
                        SnakeCustomizations {
                            color,
                            head,
                            tail,
                            author: bs.owner_name.clone(),
                        },
                    );
                }
            }

            Ok((battlesnakes, snake_urls, customizations))
        })
        .await?;

    // Create the initial game state
    let mut engine_game =
        crate::engine::create_initial_game(game_id, game.board_size, game.game_type, &battlesnakes);
    engine_game.meta.source = game_source;

    // Get timeout from game settings
    let timeout = std::time::Duration::from_millis(engine_game.meta.timeout as u64);

    let mut death_info: Vec<DeathInfo> = Vec::new();
    let mut food_eaten: HashMap<String, i32> = HashMap::new();
    let mut last_moves: HashMap<String, Direction> = HashMap::new();
    let mut snake_contexts: HashMap<String, wire::SnakeContext> = HashMap::new();

    // Call /start for all snakes in parallel (fire and forget)
    tracing::info!(game_id = %game_id, "Calling /start for all snakes");
    phase(
        game_id,
        Phase::StartSnakes,
        Some(engine_game.board.turn),
        async {
            let _: () = request_start_routed_parallel(
                &proxy_clients,
                &engine_game,
                &snake_urls,
                timeout,
                &snake_contexts,
                &customizations,
            )
            .await;
            Ok(())
        },
    )
    .await?;

    // Store turn 0 (initial state, no moves yet)
    let frame_0 = game_to_frame(&engine_game, &death_info, &[], &customizations);
    let frame_0_json =
        serde_json::to_value(&frame_0).wrap_err("Failed to serialize initial frame")?;

    tracing::info!(game_id = %game_id, "Storing turn 0");
    phase(game_id, Phase::PersistTurn, Some(0), async {
        crate::models::turn::create_turn(pool, game_id, 0, Some(frame_0_json)).await?;
        Ok(())
    })
    .await?;
    tracing::info!(game_id = %game_id, "Turn 0 stored successfully");

    // Track timing for processing_overhead metric
    let game_start = std::time::Instant::now();
    let mut total_snake_wait = std::time::Duration::ZERO;

    // Run the game turn by turn
    while !crate::engine::is_game_over(&engine_game) && engine_game.board.turn < MAX_TURNS {
        // Request moves from all alive snakes in parallel
        let move_wait_start = std::time::Instant::now();
        let move_results = phase(
            game_id,
            Phase::RequestMoves,
            Some(engine_game.board.turn),
            async {
                Ok(request_moves_routed_parallel(
                    &proxy_clients,
                    &engine_game,
                    &snake_urls,
                    timeout,
                    &last_moves,
                    &snake_contexts,
                    &customizations,
                )
                .await)
            },
        )
        .await?;

        // Requests overlap: subtract elapsed wait, not summed snake latencies.
        total_snake_wait += move_wait_start.elapsed();

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
                    latency_ms: wire::reported_latency_ms(
                        result.latency_ms,
                        result.timed_out,
                        engine_game.meta.timeout,
                    ),
                    shout: result.shout.clone(),
                },
            );
        }

        // Apply the moves using the engine
        let fed = crate::engine::apply_turn(&mut engine_game, &moves)?;
        for snake_id in fed {
            *food_eaten.entry(snake_id).or_default() += 1;
        }
        engine_game.board.turn += 1;
        // Spawn food for the next turn before the frame is recorded, so the
        // viewer and the snakes' next /move requests both see it.
        crate::engine::spawn_food(&mut engine_game);

        // Track newly eliminated snakes
        for snake in &engine_game.board.snakes {
            if snake.eliminated_cause.is_eliminated()
                && !death_info.iter().any(|death| death.snake_id == snake.id)
            {
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

        phase(
            game_id,
            Phase::PersistTurn,
            Some(engine_game.board.turn),
            async {
                let turn = crate::models::turn::create_turn(
                    pool,
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
                            &turn,
                            game_battlesnake_id,
                            &result.direction.to_string(),
                            result.latency_ms,
                            result.timed_out,
                            result.errored,
                        )
                        .await?;
                    }
                }

                Ok(())
            },
        )
        .await?;

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
    let total_time_ms = total_time.as_millis() as u64;
    let overhead = total_time.saturating_sub(total_snake_wait);
    tracing::info!(
        metric_type = "processing_overhead",
        timing_basis = "elapsed_move_wait",
        game_id = %game_id,
        duration_ms = overhead.as_millis() as u64,
        total_ms = total_time_ms,
        snake_wait_ms = total_snake_wait.as_millis() as u64,
        "game processing overhead"
    );

    // Call /end for all snakes in parallel (fire and forget)
    tracing::info!(game_id = %game_id, "Calling /end for all snakes");
    phase(
        game_id,
        Phase::EndSnakes,
        Some(engine_game.board.turn),
        async {
            let _: () = request_end_routed_parallel(
                &proxy_clients,
                &engine_game,
                &snake_urls,
                timeout,
                &snake_contexts,
                &customizations,
            )
            .await;
            Ok(())
        },
    )
    .await?;

    // Snakes eliminated on the same turn share a placement, so a final
    // head-to-head where both die is a draw with no winner.
    let placements = crate::placement::from_final_snakes(&engine_game.board.snakes);
    let winner_snake_id =
        crate::placement::outright_winner(&placements, |(_, placement)| Some(*placement))
            .map(|(snake_id, _)| snake_id.as_str());

    phase(
        game_id,
        Phase::FinishGame,
        Some(engine_game.board.turn),
        async {
            // Resolve the tournament match result (if any) before the finish
            // transaction.
            let resolved_match_game = crate::tournament_match::resolve_finished_match_game(
                pool,
                game_id,
                winner_snake_id,
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

            for (snake_id, placement) in &placements {
                let game_battlesnake_id: Uuid = snake_id
                    .parse()
                    .wrap_err_with(|| format!("Invalid game_battlesnake ID: {}", snake_id))?;

                crate::models::game_battlesnake::set_game_result_by_id(
                    &mut tx,
                    game_battlesnake_id,
                    *placement,
                    food_eaten.get(snake_id).copied().unwrap_or(0),
                )
                .await
                .wrap_err_with(|| {
                    format!(
                        "Failed to set game result for game_battlesnake {}",
                        game_battlesnake_id
                    )
                })?;
            }

            crate::models::game::update_game_status_tx(&mut tx, game_id, GameStatus::Finished)
                .await?;
            let finished_at = chrono::Utc::now();
            sqlx::query!(
                "UPDATE games SET finished_at = $2 WHERE game_id = $1",
                game_id,
                finished_at,
            )
            .execute(&mut *tx)
            .await
            .wrap_err("Failed to persist game finish instant")?;

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

            crate::customizations::record_active_week_for_game(&mut tx, game_id, finished_at)
                .await
                .wrap_err("Failed to credit game participants; retrying finish transaction")?;

            tx.commit()
                .await
                .wrap_err("Failed to commit game finish transaction")?;

            Ok(())
        },
    )
    .await?;

    tracing::info!(
        event_type = "game_completed",
        game_id = %game_id,
        final_turn = engine_game.board.turn,
        total_ms = total_time_ms,
        winner_battlesnake_id = ?winner_snake_id,
        "game completed"
    );

    phase(
        game_id,
        Phase::PostCompletion,
        None,
        enqueue_post_completion_jobs(app_state, game_id),
    )
    .await?;

    Ok(())
}

fn emit_start_events(
    game_id: Uuid,
    leaderboard_id: Option<Uuid>,
    wait_ms: i64,
    via: Option<&'static str>,
    enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    if let Some(leaderboard_id) = leaderboard_id {
        tracing::info!(event_type = "ladder_game_started", game_id = %game_id,
            leaderboard_id = %leaderboard_id, wait_ms, via = via.unwrap_or("free"),
            "ladder game started");
    }
    if let Some(enqueued_at) = enqueued_at {
        tracing::info!(metric_type = "queue_wait", game_id = %game_id,
            duration_ms = chrono::Utc::now().signed_duration_since(enqueued_at).num_milliseconds(),
            "game queue wait time");
    }
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
            None,
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
            None,
        )
        .await
        .wrap_err("Failed to enqueue match evaluation after game completion")?;

        tracing::info!(
            game_id = %game_id,
            match_id = %match_game.match_id,
            "Enqueued tournament match evaluation"
        );
    }

    // Post-game shout screening (DEV-1297). Idempotent via shout_screenings;
    // no-op while TYPESAFE_API_KEY is unset.
    cja::jobs::Job::enqueue(
        crate::jobs::ScreenShoutsJob { game_id },
        app_state.clone(),
        format!("Screen shouts for game {game_id}"),
        None,
    )
    .await
    .wrap_err("Failed to enqueue shout screening job")?;

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
    use std::sync::{Arc, Mutex};
    use tracing::{field::Visit, instrument::WithSubscriber};
    use tracing_subscriber::{Layer, prelude::*};

    #[derive(Clone, Default)]
    struct EventCapture(Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>);

    #[derive(Default)]
    struct EventFields(std::collections::HashMap<String, String>);

    impl Visit for EventFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for EventCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = EventFields::default();
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }

    #[test]
    fn successful_claim_emits_one_start_and_queue_wait_event() {
        let capture = EventCapture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let game_id = Uuid::new_v4();
        let leaderboard_id = Uuid::new_v4();
        tracing::subscriber::with_default(subscriber, || {
            emit_start_events(
                game_id,
                Some(leaderboard_id),
                1234,
                Some("deadline"),
                Some(chrono::Utc::now()),
            );
        });
        let events = capture.0.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e
                    .get("event_type")
                    .is_some_and(|v| v == "ladder_game_started"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.get("metric_type").is_some_and(|v| v == "queue_wait"))
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .all(|e| e.get("game_id").is_some_and(|v| v == &game_id.to_string()))
        );
        assert!(
            events
                .iter()
                .any(|e| e.get("via").is_some_and(|v| v == "deadline"))
        );
    }

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

    #[sqlx::test(migrations = "../migrations")]
    async fn completed_game_emits_final_turn_once(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "waiting").await?;
        let snake_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "apiversion": "1" })),
            )
            .mount(&snake_server)
            .await;

        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (1626, 'completion-test', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'completion-snake', $2) RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(snake_server.uri())
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
            .bind(game_id)
            .bind(battlesnake_id)
            .execute(&pool)
            .await?;

        let capture = EventCapture::default();
        async { run_game(&app_state, game_id).await }
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");
        let credited = sqlx::query!(
            "SELECT g.finished_at, aw.week_start FROM games g JOIN customization_active_weeks aw ON aw.user_id = $2 WHERE g.game_id = $1",
            game_id,
            user_id,
        )
        .fetch_one(&pool)
        .await?;
        let finished_at = credited
            .finished_at
            .expect("finished game has exact finish instant");
        use chrono::Datelike as _;
        let monday = finished_at.date_naive()
            - chrono::Duration::days(i64::from(finished_at.weekday().num_days_from_monday()));
        assert_eq!(credited.week_start, monday);
        assert_eq!(
            crate::customizations::token_balance(&pool, user_id).await?,
            1
        );
        run_game(&app_state, game_id).await?;
        assert_eq!(
            crate::customizations::token_balance(&pool, user_id).await?,
            1
        );

        let events = capture.0.lock().unwrap();
        let final_turn_events: Vec<_> = events
            .iter()
            .filter(|event| event.contains_key("final_turn"))
            .collect();
        assert_eq!(final_turn_events.len(), 1, "{final_turn_events:?}");
        let event = final_turn_events[0];
        assert_eq!(
            event.get("event_type").map(String::as_str),
            Some("game_completed")
        );
        assert_eq!(
            event.get("message").map(String::as_str),
            Some("game completed")
        );
        assert_eq!(event.get("game_id"), Some(&game_id.to_string()));
        assert_eq!(event.get("final_turn").map(String::as_str), Some("0"));
        assert!(event.contains_key("total_ms"));
        assert!(event.contains_key("winner_battlesnake_id"));
        Ok(())
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

    /// A retry on a failed game must not resurrect it: no re-run, no
    /// post-completion hooks, status stays failed. The fixture game has no
    /// battlesnakes, so reaching the normal run path would fail loudly —
    /// returning Ok proves the short-circuit.
    #[sqlx::test(migrations = "../migrations")]
    async fn failed_game_is_terminal_and_skips_rerun(pool: PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "failed").await?;

        run_game(&app_state, game_id).await?;

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "failed");
        assert_eq!(count_jobs(&pool, "LeaderboardRatingUpdateJob").await?, 0);
        assert_eq!(count_jobs(&pool, "RunMatchJob").await?, 0);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn running_retry_does_not_emit_start_metrics(pool: PgPool) -> cja::Result<()> {
        let app = AppState::test_from_pool(pool.clone());
        // An empty roster makes the retry fail after its Running branch, so
        // the test never reaches snake HTTP calls or a full game simulation.
        let game_id = fixture_game(&pool, "running").await?;
        let capture = EventCapture::default();
        let result = run_game(&app, game_id)
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await;
        assert!(
            result.is_err(),
            "invalid retry fixture unexpectedly completed"
        );
        assert!(capture.0.lock().unwrap().iter().all(|event| {
            event.get("game_id") != Some(&game_id.to_string())
                || (event.get("event_type") != Some(&"ladder_game_started".to_owned())
                    && event.get("metric_type") != Some(&"queue_wait".to_owned()))
        }));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn busy_ladder_runner_makes_no_turns(pool: PgPool) -> cja::Result<()> {
        let owner = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (161302, 'runner-owner', 'test') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'runner-snake', 'http://127.0.0.1:9') RETURNING battlesnake_id",
            owner
        )
        .fetch_one(&pool)
        .await?;
        let running = fixture_game(&pool, "running").await?;
        let waiting = fixture_game(&pool, "waiting").await?;
        for game_id in [running, waiting] {
            sqlx::query!(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)",
                game_id,
                snake
            )
            .execute(&pool)
            .await?;
        }
        let leaderboard = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
            leaderboard,
            waiting
        )
        .execute(&pool)
        .await?;
        let app = AppState::test_from_pool(pool.clone());
        let capture = EventCapture::default();
        run_game(&app, waiting)
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;
        assert!(capture.0.lock().unwrap().iter().all(|event| {
            event.get("game_id") != Some(&waiting.to_string())
                || (event.get("event_type") != Some(&"ladder_game_started".to_owned())
                    && event.get("metric_type") != Some(&"queue_wait".to_owned()))
        }));
        assert_eq!(
            sqlx::query_scalar!("SELECT status FROM games WHERE game_id = $1", waiting)
                .fetch_one(&pool)
                .await?,
            "waiting"
        );
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) FROM turns WHERE game_id = $1", waiting)
                .fetch_one(&pool)
                .await?
                .unwrap_or(0),
            0
        );
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

    /// Every finished game enqueues shout screening (DEV-1297); the job
    /// itself no-ops when the judge is disabled or the game was already
    /// screened.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_game_enqueues_shout_screening(pool: PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        run_game(&app_state, game_id).await?;

        assert_eq!(count_jobs(&pool, "ScreenShoutsJob").await?, 1);

        Ok(())
    }

    /// Scripted wiremock Battlesnake. Moves cautiously (any in-bounds cell
    /// not on a body or next to another head) until `suicide_from_turn`,
    /// then always "left" into the wall. `timeout_on_turn` delays that
    /// turn's /move past the 500ms budget. With `seek_food` it takes the
    /// cautious move closest to the nearest food.
    struct ScriptedSnake {
        suicide_from_turn: i64,
        timeout_on_turn: Option<i64>,
        seek_food: bool,
    }

    impl ScriptedSnake {
        fn choose_move(&self, request: &serde_json::Value) -> &'static str {
            let turn = request["turn"].as_i64().unwrap_or(0);
            if turn >= self.suicide_from_turn {
                return "left";
            }
            let width = request["board"]["width"].as_i64().unwrap_or(0);
            let height = request["board"]["height"].as_i64().unwrap_or(0);
            let you_id = request["you"]["id"].as_str().unwrap_or_default();
            let point = |p: &serde_json::Value| {
                (
                    p["x"].as_i64().unwrap_or_default(),
                    p["y"].as_i64().unwrap_or_default(),
                )
            };
            let mut blocked = std::collections::HashSet::new();
            for snake in request["board"]["snakes"].as_array().into_iter().flatten() {
                for segment in snake["body"].as_array().into_iter().flatten() {
                    blocked.insert(point(segment));
                }
                if snake["id"].as_str() != Some(you_id) {
                    let (x, y) = point(&snake["head"]);
                    blocked.extend([(x, y + 1), (x + 1, y), (x, y - 1), (x - 1, y)]);
                }
            }
            let (x, y) = point(&request["you"]["head"]);
            let mut safe = [
                ("up", (x, y + 1)),
                ("right", (x + 1, y)),
                ("down", (x, y - 1)),
                ("left", (x - 1, y)),
            ]
            .into_iter()
            .filter(|(_, (nx, ny))| {
                (0..width).contains(nx)
                    && (0..height).contains(ny)
                    && !blocked.contains(&(*nx, *ny))
            });
            let food: Vec<(i64, i64)> = request["board"]["food"]
                .as_array()
                .into_iter()
                .flatten()
                .map(point)
                .collect();
            let choice = if self.seek_food && !food.is_empty() {
                safe.min_by_key(|(_, (nx, ny))| {
                    food.iter()
                        .map(|(fx, fy)| (fx - nx).abs() + (fy - ny).abs())
                        .min()
                })
            } else {
                safe.next()
            };
            choice.map_or("up", |(direction, _)| direction)
        }
    }

    impl wiremock::Respond for ScriptedSnake {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            if !request.url.path().ends_with("/move") {
                return wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "apiversion": "1" }));
            }
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let response = wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "move": self.choose_move(&body) }));
            if self.timeout_on_turn == body["turn"].as_i64() {
                response.set_delay(std::time::Duration::from_millis(800))
            } else {
                response
            }
        }
    }

    /// Request bodies a scripted snake received on `endpoint`.
    async fn received_bodies(
        server: &wiremock::MockServer,
        endpoint: &str,
    ) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with(endpoint))
            .map(|r| serde_json::from_slice(&r.body).expect("request body is JSON"))
            .collect()
    }

    /// End-to-end snake API parity with the official engine (DEV-1496),
    /// checked on the bodies real snakes receive from a full ladder game:
    /// - `board.snakes` lists exactly the snakes alive in that turn's frame,
    ///   so no eliminated snake (or off-board head) is ever sent;
    /// - after a timed-out move the next request reports `you.latency` as
    ///   the timeout ("500"), matching the frame (Latency "500" plus the
    ///   engine's timeout Error, DEV-1502);
    /// - `game.map` / `game.source` are filled in.
    #[sqlx::test(migrations = "../migrations")]
    async fn snake_requests_match_official_engine(pool: PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "waiting").await?;
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (424242, 'test-user', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('parity') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
            .bind(leaderboard_id)
            .bind(game_id)
            .execute(&pool)
            .await?;

        // Doomed: times out on turn 0, then drives into the wall (dead by
        // turn 11 on 11x11). Late: cautious, then suicidal from turn 15, so
        // two snakes keep receiving /move after the first death. Survivor:
        // cautious throughout.
        let doomed = wiremock::MockServer::start().await;
        let late = wiremock::MockServer::start().await;
        let survivor = wiremock::MockServer::start().await;
        for (name, server, script) in [
            (
                "doomed",
                &doomed,
                ScriptedSnake {
                    suicide_from_turn: 1,
                    timeout_on_turn: Some(0),
                    seek_food: false,
                },
            ),
            (
                "late",
                &late,
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ),
            (
                "survivor",
                &survivor,
                ScriptedSnake {
                    suicide_from_turn: i64::MAX,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ),
        ] {
            wiremock::Mock::given(wiremock::matchers::any())
                .respond_with(script)
                .mount(server)
                .await;
            let battlesnake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, $3)
                 RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(name)
            .bind(server.uri())
            .fetch_one(&pool)
            .await?;
            sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
                .bind(game_id)
                .bind(battlesnake_id)
                .execute(&pool)
                .await?;
        }

        run_game(&app_state, game_id).await?;

        // Living snake IDs per turn, from the persisted frames.
        let frames: Vec<(i32, serde_json::Value)> = sqlx::query_as(
            "SELECT turn_number, frame_data FROM turns WHERE game_id = $1 ORDER BY turn_number",
        )
        .bind(game_id)
        .fetch_all(&pool)
        .await?;
        let alive_at = |turn: i64| -> Vec<String> {
            let (_, frame) = frames
                .iter()
                .find(|(t, _)| i64::from(*t) == turn)
                .expect("a frame exists for every requested turn");
            let mut ids: Vec<String> = frame["Snakes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["EliminatedCause"].as_str().unwrap_or_default().is_empty())
                .filter_map(|s| s["ID"].as_str().map(String::from))
                .collect();
            ids.sort();
            ids
        };

        let mut saw_dead_snake_filtered = false;
        for server in [&doomed, &late, &survivor] {
            for endpoint in ["/move", "/end"] {
                for request in received_bodies(server, endpoint).await {
                    assert_eq!(request["game"]["source"], "arena");
                    assert_eq!(request["game"]["map"], "standard");
                    assert_eq!(request["game"]["ruleset"]["settings"]["hazardMap"], "");

                    let turn = request["turn"].as_i64().expect("turn is a number");
                    let mut on_board: Vec<String> = request["board"]["snakes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s["id"].as_str().map(String::from))
                        .collect();
                    on_board.sort();
                    let alive = alive_at(turn);
                    assert_eq!(on_board, alive, "{endpoint} at turn {turn}");
                    if alive.len() < 3 {
                        saw_dead_snake_filtered = true;
                    }
                }
            }
        }
        assert!(
            saw_dead_snake_filtered,
            "some request must have been sent after an elimination"
        );

        let doomed_moves = received_bodies(&doomed, "/move").await;
        let turn_one = doomed_moves
            .iter()
            .find(|r| r["turn"] == 1)
            .expect("the doomed snake survives turn 0");
        assert_eq!(turn_one["you"]["latency"], "500");
        let doomed_id = turn_one["you"]["id"].as_str().unwrap_or_default();
        let (_, frame_one) = frames
            .iter()
            .find(|(t, _)| *t == 1)
            .expect("frame 1 exists");
        let frame_snake = frame_one["Snakes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["ID"] == doomed_id)
            .cloned()
            .unwrap_or_default();
        assert_eq!(frame_snake["Latency"], "500");
        assert_eq!(frame_snake["Error"], crate::engine::frame::TIMEOUT_ERROR);

        // /end goes to every snake, dead or alive, and each sees itself.
        for server in [&doomed, &late, &survivor] {
            let ends = received_bodies(server, "/end").await;
            assert_eq!(ends.len(), 1);
        }

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
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement, food_eaten)
             VALUES ($1, $2, 1, 4) RETURNING game_battlesnake_id",
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
        let (placement, food_eaten): (Option<i32>, Option<i32>) = sqlx::query_as(
            "SELECT placement, food_eaten FROM game_battlesnakes WHERE game_battlesnake_id = $1",
        )
        .bind(game_battlesnake_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(placement, None);
        assert_eq!(food_eaten, None);

        Ok(())
    }

    /// Food eaten is counted from the engine's feed stage and stored with the
    /// placement, never inferred from body growth (#233). In Standard each
    /// food is exactly +1 length, so the count must equal every snake's
    /// growth. Constrictor grows every snake every turn with no food on the
    /// board, so the count must stay 0 while the snakes still grow.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_game_records_food_eaten(pool: PgPool) -> cja::Result<()> {
        let app_state = crate::state::AppState::test_from_pool(pool.clone());
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (233, 'food-test', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;

        for game_type in ["Standard", "Constrictor"] {
            let game_id: Uuid = sqlx::query_scalar(
                "INSERT INTO games (board_size, game_type, status)
                 VALUES ('11x11', $1, 'waiting') RETURNING game_id",
            )
            .bind(game_type)
            .fetch_one(&pool)
            .await?;

            // One snake suicides from turn 25, which ends the game; both chase
            // food until then.
            let mut servers = vec![];
            for suicide_from_turn in [25, i64::MAX] {
                let server = wiremock::MockServer::start().await;
                wiremock::Mock::given(wiremock::matchers::any())
                    .respond_with(ScriptedSnake {
                        suicide_from_turn,
                        timeout_on_turn: None,
                        seek_food: true,
                    })
                    .mount(&server)
                    .await;
                let battlesnake_id: Uuid = sqlx::query_scalar(
                    "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, $3)
                     RETURNING battlesnake_id",
                )
                .bind(user_id)
                .bind(format!("{game_type}-{suicide_from_turn}"))
                .bind(server.uri())
                .fetch_one(&pool)
                .await?;
                sqlx::query(
                    "INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)",
                )
                .bind(game_id)
                .bind(battlesnake_id)
                .execute(&pool)
                .await?;
                servers.push(server);
            }

            run_game(&app_state, game_id).await?;

            // Each snake's final length, from the last frame it appears in.
            let frames: Vec<serde_json::Value> = sqlx::query_scalar(
                "SELECT frame_data FROM turns WHERE game_id = $1 ORDER BY turn_number",
            )
            .bind(game_id)
            .fetch_all(&pool)
            .await?;
            let mut final_length: HashMap<String, i32> = HashMap::new();
            for frame in &frames {
                for snake in frame["Snakes"].as_array().into_iter().flatten() {
                    let id = snake["ID"].as_str().expect("frame snake has an ID");
                    let length = snake["Body"].as_array().map_or(0, Vec::len);
                    final_length.insert(id.to_string(), i32::try_from(length)?);
                }
            }

            let recorded: Vec<(Uuid, Option<i32>)> = sqlx::query_as(
                "SELECT game_battlesnake_id, food_eaten FROM game_battlesnakes WHERE game_id = $1",
            )
            .bind(game_id)
            .fetch_all(&pool)
            .await?;
            assert_eq!(recorded.len(), 2);
            let mut total_food = 0;
            for (game_battlesnake_id, food_eaten) in recorded {
                let food_eaten = food_eaten.expect("finished game records food eaten");
                let grown = final_length[&game_battlesnake_id.to_string()] - 3;
                if game_type == "Constrictor" {
                    assert!(grown > 0, "constrictor snakes grow every turn");
                    assert_eq!(food_eaten, 0, "constrictor growth is not eating");
                } else {
                    assert_eq!(food_eaten, grown, "standard food eaten == growth");
                }
                total_food += food_eaten;
            }
            if game_type == "Standard" {
                assert!(total_food > 0, "food seekers should eat in Standard");
            }
        }

        Ok(())
    }
}
