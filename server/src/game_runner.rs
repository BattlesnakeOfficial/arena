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

    /// How this run begins. A `Running` game resumes from its last committed
    /// turn when its live resume state is present and trusted (DEV-1569);
    /// every other path plays from turn 0.
    enum Start {
        /// Create the initial board, call /start, persist turn 0 — today's path.
        Fresh,
        /// Continue from the restored state: no /start, no turn-0 persist.
        /// Boxed: the restored engine state dwarfs the unit variant.
        Resumed(Box<crate::engine::resume::ResumedState>),
    }

    let start = match game.status {
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
            // A previous attempt died mid-game (deploy SIGKILL, OOM, panic)
            // or before the finish committed. Prefer resuming from the live
            // resume state; fall back to today's reset only when the state
            // is missing or untrusted. `reason` is a stable slug; `detail`
            // carries free text. DB errors propagate with `?` — a
            // connection blip retries the job; it must NEVER degrade into
            // the reset fallback.
            let resume: Option<crate::engine::resume::ResumedState> =
                phase(game_id, Phase::ResumeGame, None, async {
                    let battlesnakes =
                        crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id)
                            .await?;
                    let Some(state) =
                        crate::models::resume_state::load_resume_state(pool, game_id).await?
                    else {
                        // No state row: died before turn 0 committed, or a
                        // pre-DEV-1569 binary wrote these turns. Which one
                        // decides the slug.
                        let reason = if crate::models::turn::game_has_turns(pool, game_id).await? {
                            "no_resume_state"
                        } else {
                            "no_turns_persisted"
                        };
                        tracing::warn!(
                            event_type = "game_resume_fallback",
                            game_id = %game_id,
                            reason,
                            detail = "no resumable state found",
                            "cannot resume game; resetting from turn 0"
                        );
                        return Ok(None);
                    };
                    // The state commits with turn T, so turn T's row must
                    // exist; a missing row means corrupted/foreign state.
                    if !crate::models::turn::turn_row_exists(pool, game_id, state.turn_number)
                        .await?
                    {
                        tracing::warn!(
                            event_type = "game_resume_fallback",
                            game_id = %game_id,
                            reason = "snapshot_validation_failed",
                            detail = %format!("no turns row at turn {}", state.turn_number),
                            "cannot resume game; resetting from turn 0"
                        );
                        return Ok(None);
                    }
                    let resume_turn = state.turn_number; // restore takes state by value
                    match crate::engine::resume::restore(state, &game, game_source, &battlesnakes) {
                        Ok(resumed) => {
                            // Drop any turns rows above T (a kill mid-persist
                            // rolled back, but a foreign writer may have left
                            // a tail).
                            crate::models::turn::delete_turns_above(pool, game_id, resume_turn)
                                .await?;
                            // Best-effort log context; -1 when no locked row
                            // is visible (e.g. direct run_game calls).
                            let job_attempt = current_job_attempt(pool, game_id)
                                .await
                                .ok()
                                .flatten()
                                .unwrap_or(-1);
                            tracing::info!(
                                event_type = "game_resumed",
                                game_id = %game_id,
                                resumed_from_turn = resume_turn,
                                job_attempt,
                                "resuming game from last committed turn"
                            );
                            Ok(Some(resumed))
                        }
                        Err(error) => {
                            let (reason, detail) = crate::engine::resume::fallback_reason(&error);
                            tracing::warn!(
                                event_type = "game_resume_fallback",
                                game_id = %game_id,
                                reason,
                                detail = %detail,
                                "cannot resume game; resetting from turn 0"
                            );
                            Ok(None)
                        }
                    }
                })
                .await?;
            if resume.is_none() {
                phase(
                    game_id,
                    Phase::ResetGame,
                    None,
                    crate::models::game::reset_game_state_for_retry(pool, game_id),
                )
                .await?;
            }
            match resume {
                Some(resumed) => Start::Resumed(Box::new(resumed)),
                None => Start::Fresh,
            }
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
            Start::Fresh
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
    };

    let (battlesnakes, snake_urls, customizations) =
        phase(game_id, Phase::PrepareSnakes, None, async {
            // Get all the battlesnakes in the game with their URLs. Runs for
            // both fresh and resumed starts: the turn loop needs
            // snake_urls/customizations either way (a resume is a slow turn
            // from the snake's POV, but /info still feeds customizations).
            let battlesnakes =
                crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id)
                    .await
                    .wrap_err("Failed to get battlesnakes for game")?;

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
                    battlesnake_id: bs.battlesnake_id,
                    user_id: bs.user_id,
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

    let mut engine_game: crate::engine::EngineGame;
    let mut death_info: Vec<DeathInfo>;
    let mut food_eaten: HashMap<String, i32>;
    let mut last_moves: HashMap<String, Direction>;
    let mut snake_contexts: HashMap<String, wire::SnakeContext>;

    match start {
        Start::Fresh => {
            tracing::info!(
                event_type = "game_started",
                game_id = %game_id,
                board_size = game.board_size.as_str(),
                game_type = game.game_type.as_str(),
                snake_count = battlesnakes.len(),
                "game started"
            );

            // Create the initial game state
            engine_game = crate::engine::create_initial_game(
                game_id,
                game.board_size.clone(),
                game.game_type.clone(),
                &battlesnakes,
            );
            engine_game.meta.source = game_source;

            death_info = Vec::new();
            food_eaten = HashMap::new();
            last_moves = HashMap::new();
            snake_contexts = HashMap::new();

            let timeout = std::time::Duration::from_millis(engine_game.meta.timeout as u64);

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

            // Store turn 0 (initial state, no moves yet) atomically with
            // its (empty) resume snapshot, so even turn 0 is resumable.
            let frame_0 = game_to_frame(&engine_game, &death_info, &[], &customizations);
            let frame_0_json =
                serde_json::to_value(&frame_0).wrap_err("Failed to serialize initial frame")?;
            let resume_0 = crate::engine::resume::capture_resume_state(
                &engine_game,
                &death_info,
                &food_eaten,
                &last_moves,
                &snake_contexts,
            )?;

            tracing::info!(game_id = %game_id, "Storing turn 0");
            phase(game_id, Phase::PersistTurn, Some(0), async {
                crate::models::turn::persist_turn(pool, game_id, 0, frame_0_json, &resume_0, &[])
                    .await?;
                Ok(())
            })
            .await?;
            tracing::info!(game_id = %game_id, "Turn 0 stored successfully");
        }
        Start::Resumed(state) => {
            // No /start, no turn-0 persist: continue from the restored
            // state (source already set by restore).
            let state = *state;
            engine_game = state.engine_game;
            death_info = state.death_info;
            food_eaten = state.food_eaten;
            last_moves = state.last_moves;
            snake_contexts = state.snake_contexts;
        }
    }

    // Get timeout from game settings
    let timeout = std::time::Duration::from_millis(engine_game.meta.timeout as u64);

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

        apply_turn_results(
            &mut engine_game,
            &move_results,
            &mut death_info,
            &mut food_eaten,
            &mut last_moves,
            &mut snake_contexts,
        )?;

        // Store the turn frame with latency info and notify subscribers
        let frame = game_to_frame(&engine_game, &death_info, &move_results, &customizations);
        let frame_json = serde_json::to_value(&frame)
            .wrap_err_with(|| format!("Failed to serialize frame {}", engine_game.board.turn))?;

        // Captured after apply_turn_results, i.e. after the maps and
        // death_info are updated — the restored maps are exactly what an
        // uninterrupted run would hold at this turn.
        let resume = crate::engine::resume::capture_resume_state(
            &engine_game,
            &death_info,
            &food_eaten,
            &last_moves,
            &snake_contexts,
        )?;
        let moves: Vec<crate::models::turn::SnakeMoveRecord> = move_results
            .iter()
            .filter_map(|result| {
                Uuid::parse_str(&result.snake_id).ok().map(|id| {
                    crate::models::turn::SnakeMoveRecord {
                        game_battlesnake_id: id,
                        direction: result.direction.to_string(),
                        latency_ms: result.latency_ms,
                        timed_out: result.timed_out,
                        errored: result.errored,
                    }
                })
            })
            .collect();

        // Measure DB write latency
        let db_write_start = std::time::Instant::now();

        phase(
            game_id,
            Phase::PersistTurn,
            Some(engine_game.board.turn),
            async {
                crate::models::turn::persist_turn(
                    pool,
                    game_id,
                    engine_game.board.turn,
                    frame_json,
                    &resume,
                    &moves,
                )
                .await
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

            // Status flip + finished_at + archive-field invalidation in one
            // UPDATE: a game the sweeper failed mid-run may already carry a
            // failed-era archive; finishing invalidates it so native backup
            // discovery re-archives the complete game (DEV-1980).
            let finished_at = chrono::Utc::now();
            crate::models::game::mark_game_finished_tx(&mut tx, game_id, finished_at).await?;

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
        source = game_source.analytics_label(),
        game_type = game.game_type.as_str(),
        board_size = game.board_size.as_str(),
        snake_count = battlesnakes.len(),
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

/// Apply one turn's move results exactly as the live loop does: update
/// last_moves and snake_contexts from the results, advance the engine,
/// record food eaten, and append newly eliminated snakes to death_info.
/// Shared by run_game's turn loop and the resume test fixtures so the
/// "interrupted run" the fixtures build cannot drift from the real loop.
pub(crate) fn apply_turn_results(
    engine_game: &mut crate::engine::EngineGame,
    move_results: &[crate::snake_client::MoveResult],
    death_info: &mut Vec<DeathInfo>,
    food_eaten: &mut HashMap<String, i32>,
    last_moves: &mut HashMap<String, Direction>,
    snake_contexts: &mut HashMap<String, wire::SnakeContext>,
) -> cja::Result<()> {
    // Store last moves for timeout fallback on next turn
    for result in move_results {
        last_moves.insert(result.snake_id.clone(), result.direction);
    }

    // Update snake_contexts for NEXT turn
    snake_contexts.clear();
    for result in move_results {
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

    // One full turn: moves, turn counter, and food for the next turn —
    // spawned before the frame is recorded, so the viewer and the snakes'
    // next /move requests both see it.
    let moves: Vec<(String, Direction)> = move_results
        .iter()
        .map(|r| (r.snake_id.clone(), r.direction))
        .collect();
    let fed = crate::engine::advance_turn(engine_game, &moves)?;
    for snake_id in fed {
        *food_eaten.entry(snake_id).or_default() += 1;
    }

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

    Ok(())
}

/// The error_count of the currently locked GameRunnerJob row for this game
/// (cja's attempt counter: 0 on first run, +1 per retry/lease-reclaim).
/// Read-only — the jobs table is cja's; never "improve" this into an
/// UPDATE. `None` when no locked row is visible (e.g. direct `run_game`
/// calls with no job worker running).
async fn current_job_attempt(pool: &sqlx::PgPool, game_id: Uuid) -> cja::Result<Option<i32>> {
    let attempt = sqlx::query_scalar!(
        r#"SELECT error_count AS "error_count!" FROM jobs
           WHERE name = 'GameRunnerJob'
             AND payload->>'game_id' = $1
             AND locked_by IS NOT NULL
           ORDER BY locked_at DESC LIMIT 1"#,
        game_id.to_string()
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to read GameRunnerJob attempt")?;
    Ok(attempt)
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

    cja::jobs::Job::enqueue(
        crate::jobs::AwardAchievementsForGameJob { game_id },
        app_state.clone(),
        format!("Award achievements for game {game_id}"),
        None,
    )
    .await
    .wrap_err("Failed to enqueue achievement award job")?;

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

    /// Captures `snake.lifecycle` spans: creation fields plus a close count.
    /// `on_close` needs the span name, so the impl is `LookupSpan`-bound.
    #[derive(Clone, Default)]
    struct SpanCapture {
        lifecycle: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
        closed: Arc<Mutex<usize>>,
    }

    impl<S> tracing_subscriber::Layer<S> for SpanCapture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() == "snake.lifecycle" {
                let mut fields = EventFields::default();
                attrs.record(&mut fields);
                self.lifecycle.lock().unwrap().push(fields.0);
            }
        }

        fn on_close(&self, id: tracing::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
            if ctx
                .span(&id)
                .is_some_and(|span| span.name() == "snake.lifecycle")
            {
                *self.closed.lock().unwrap() += 1;
            }
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
        assert_eq!(count_jobs(&pool, "AwardAchievementsForGameJob").await?, 1);
        assert_eq!(count_jobs(&pool, "SendNotificationEmailJob").await?, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM customization_grants WHERE user_id = $1"
            )
            .bind(user_id)
            .fetch_one(&pool)
            .await?,
            0,
            "finish commits before the award job runs"
        );
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
        assert_eq!(count_jobs(&pool, "AwardAchievementsForGameJob").await?, 2);
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

        assert_eq!(count_jobs(&pool, "AwardAchievementsForGameJob").await?, 1);
        assert_eq!(count_jobs(&pool, "SendNotificationEmailJob").await?, 0);
        // Not a leaderboard or tournament game: no other follow-up jobs.
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
        // The deterministic failure errs on EVERY attempt through the
        // fallback path too — it never becomes a silent reset loop.
        assert!(run_game(&app, game_id).await.is_err());
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

    /// Ladder-shaped fixture: user → battlesnake → `leaderboards` row
    /// 'Standard 11x11' (seeded by migrations) → `leaderboard_entries` →
    /// game_battlesnakes referencing ONLY the entry (battlesnake_id NULL —
    /// the ladder shape) → `leaderboard_games` link. Returns
    /// (owner, battlesnake_id, game_id).
    async fn ladder_fixture(pool: &PgPool, snake_url: &str) -> cja::Result<(Uuid, Uuid, Uuid)> {
        let owner = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (1731, 'lifecycle-owner', 'test') RETURNING user_id"
        )
        .fetch_one(pool)
        .await?;
        let battlesnake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'lifecycle-snake', $2) RETURNING battlesnake_id",
            owner,
            snake_url
        )
        .fetch_one(pool)
        .await?;
        let leaderboard = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'"
        )
        .fetch_one(pool)
        .await?;
        let entry = sqlx::query_scalar!(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
             VALUES ($1, $2) RETURNING leaderboard_entry_id",
            leaderboard,
            battlesnake_id
        )
        .fetch_one(pool)
        .await?;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'waiting') RETURNING game_id",
        )
        .fetch_one(pool)
        .await?;
        sqlx::query!(
            "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id) VALUES ($1, $2)",
            game_id,
            entry
        )
        .execute(pool)
        .await?;
        sqlx::query!(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)",
            leaderboard,
            game_id
        )
        .execute(pool)
        .await?;
        Ok((owner, battlesnake_id, game_id))
    }

    /// DEV-2061: the `snake.lifecycle` span exists around each real /start
    /// and /end call and resolves the real snake and owner even for a
    /// ladder game, whose `game_battlesnakes` rows hold only
    /// `leaderboard_entry_id`. One snake finishes the game at turn 0
    /// (`is_game_over` is `alive <= 1`) with both lifecycle calls made.
    #[sqlx::test(migrations = "../migrations")]
    async fn ladder_game_lifecycle_spans_resolve_snakes_and_owners(
        pool: PgPool,
    ) -> cja::Result<()> {
        // A live global dispatcher keeps tracing-core off its
        // `Rebuilder::JustOne` path (`callsite.rs`, `has_just_one`,
        // `get_default`), which otherwise lets an unscoped test on another
        // thread cache `Interest::never()` for `snake.lifecycle` while the
        // scoped dispatcher below is the only one alive.
        let _ = crate::test_capture::Capture::shared();
        let app_state = AppState::test_from_pool(pool.clone());
        let snake_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "apiversion": "1" })),
            )
            .mount(&snake_server)
            .await;
        let (owner, battlesnake_id, game_id) = ladder_fixture(&pool, &snake_server.uri()).await?;

        let events = EventCapture::default();
        let spans = SpanCapture::default();
        run_game(&app_state, game_id)
            .with_subscriber(
                tracing_subscriber::registry()
                    .with(events.clone())
                    .with(spans.clone()),
            )
            .await?;

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");

        let lifecycle = spans.lifecycle.lock().unwrap().clone();
        assert_eq!(lifecycle.len(), 2, "{lifecycle:?}");
        let endpoints: Vec<_> = lifecycle.iter().map(|f| f["endpoint"].clone()).collect();
        assert!(endpoints.contains(&"start".to_owned()), "{lifecycle:?}");
        assert!(endpoints.contains(&"end".to_owned()), "{lifecycle:?}");
        for fields in &lifecycle {
            assert_eq!(fields["game_id"], game_id.to_string(), "{lifecycle:?}");
            assert_eq!(
                fields["battlesnake_id"],
                battlesnake_id.to_string(),
                "{lifecycle:?}"
            );
            assert_eq!(fields["user_id"], owner.to_string(), "{lifecycle:?}");
            assert_eq!(fields["source"], "ladder", "{lifecycle:?}");
        }
        assert_eq!(*spans.closed.lock().unwrap(), 2);

        let events = events.0.lock().unwrap();
        let completed = events
            .iter()
            .find(|e| e.get("event_type").map(String::as_str) == Some("game_completed"))
            .expect("game_completed event");
        assert_eq!(completed["source"], "ladder");
        assert_eq!(completed["game_type"], "Standard");
        assert_eq!(completed["board_size"], "11x11");
        assert_eq!(completed["snake_count"], "1");
        Ok(())
    }

    /// The `snake.lifecycle` span closes whether or not the snake answers:
    /// a refused port keeps serving nothing, so both the open and close
    /// paths must run without pinning exact counts (a start-only snake may
    /// or may not also get an /end).
    #[sqlx::test(migrations = "../migrations")]
    async fn lifecycle_spans_close_when_snake_does_not_answer(pool: PgPool) -> cja::Result<()> {
        let _ = crate::test_capture::Capture::shared();
        let app_state = AppState::test_from_pool(pool.clone());
        let (_owner, _battlesnake_id, game_id) =
            ladder_fixture(&pool, "http://127.0.0.1:9").await?;

        let spans = SpanCapture::default();
        run_game(&app_state, game_id)
            .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
            .await?;

        let opened = spans.lifecycle.lock().unwrap().len();
        let closed = *spans.closed.lock().unwrap();
        assert!(opened >= 1, "expected at least one lifecycle span");
        assert_eq!(opened, closed, "every opened lifecycle span must close");
        Ok(())
    }

    /// A custom game (direct `game_battlesnakes` rows, no ladder/tournament
    /// link) reports `source == "custom"` plus the new descriptive fields.
    #[sqlx::test(migrations = "../migrations")]
    async fn custom_game_completed_carries_source_fields(pool: PgPool) -> cja::Result<()> {
        let _ = crate::test_capture::Capture::shared();
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
             VALUES (1627, 'custom-source-test', 'test-token') RETURNING user_id",
        )
        .fetch_one(&pool)
        .await?;
        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'custom-source-snake', $2) RETURNING battlesnake_id",
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

        let events = capture.0.lock().unwrap();
        let completed = events
            .iter()
            .find(|e| e.get("event_type").map(String::as_str) == Some("game_completed"))
            .expect("game_completed event");
        assert_eq!(completed["source"], "custom");
        assert_eq!(completed["game_type"], "Standard");
        assert_eq!(completed["board_size"], "11x11");
        assert_eq!(completed["snake_count"], "1");
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
    #[derive(Clone)]
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
    /// trip over: turns (and their snake_turns), partial placements, and the
    /// live resume state.
    #[sqlx::test(migrations = "../migrations")]
    async fn reset_clears_turns_placements_and_resume_state(pool: PgPool) -> cja::Result<()> {
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
        sqlx::query(
            "INSERT INTO game_resume_states (game_id, turn_number, food, hazards)
             VALUES ($1, 0, '[]', '[]')",
        )
        .bind(game_id)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO game_resume_snakes (
                 game_id, game_battlesnake_id, health, eliminated_cause, eliminated_by,
                 eliminated_on_turn, context_present, food_eaten, body
             ) VALUES ($1, $2, 100, 'not_eliminated', '', 0, false, 0, '[{}]')",
        )
        .bind(game_id)
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
        let resume_states: i64 =
            sqlx::query_scalar("SELECT count(*) FROM game_resume_states WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(resume_states, 0, "reset deletes the resume state");
        let resume_snakes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM game_resume_snakes WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(resume_snakes, 0, "cascade removes the snake rows");

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

    // --- DEV-1569: resume instead of reset ---

    /// CYCLE-indexed directions for the no-HTTP fixtures — the same walk
    /// engine-core's own tests use.
    const CYCLE: [Direction; 4] = [
        Direction::Up,
        Direction::Right,
        Direction::Down,
        Direction::Left,
    ];

    /// Insert user/battlesnakes/game (status='running', distinct names) and
    /// mount one ScriptedSnake per snake. Returns (game_id, servers).
    async fn resume_fixture(
        pool: &PgPool,
        scripts: &[ScriptedSnake],
    ) -> cja::Result<(Uuid, Vec<wiremock::MockServer>)> {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (15691, 'resume-owner', 'test-token') RETURNING user_id",
        )
        .fetch_one(pool)
        .await?;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'running') RETURNING game_id",
        )
        .fetch_one(pool)
        .await?;
        let mut servers = Vec::new();
        for (i, script) in scripts.iter().enumerate() {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::any())
                .respond_with(script.clone())
                .mount(&server)
                .await;
            let battlesnake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, $3) RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(format!("resume-snake-{i}"))
            .bind(server.uri())
            .fetch_one(pool)
            .await?;
            sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
                .bind(game_id)
                .bind(battlesnake_id)
                .execute(pool)
                .await?;
            servers.push(server);
        }
        Ok((game_id, servers))
    }

    /// Play scripted turns through the production helpers — persist_turn,
    /// apply_turn_results, game_to_frame — WITHOUT any HTTP. This is the
    /// simulated interrupted run (a kill after turn k committed): you can't
    /// SIGKILL inside a function, so run its steps to the kill point by
    /// hand. Moves are CYCLE-indexed per roster index
    /// (`CYCLE[(turn + i) % 4]`); from `suicide_from_turn` every snake walks
    /// Left into the wall. Stops at turn `k`, or at game over when
    /// `until_game_over`. Returns the final engine game.
    async fn play_persisted_turns(
        pool: &PgPool,
        game_id: Uuid,
        k: i32,
        suicide_from_turn: i64,
        until_game_over: bool,
    ) -> cja::Result<crate::engine::EngineGame> {
        let game = get_game_by_id(pool, game_id)
            .await?
            .expect("fixture game exists");
        let battlesnakes =
            crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id).await?;
        let game_source = get_game_source(pool, game_id).await?;
        let mut engine_game = crate::engine::create_initial_game(
            game_id,
            game.board_size.clone(),
            game.game_type.clone(),
            &battlesnakes,
        );
        engine_game.meta.source = game_source;

        let mut death_info: Vec<DeathInfo> = Vec::new();
        let mut food_eaten: HashMap<String, i32> = HashMap::new();
        let mut last_moves: HashMap<String, Direction> = HashMap::new();
        let mut snake_contexts: HashMap<String, wire::SnakeContext> = HashMap::new();
        let customizations: HashMap<String, SnakeCustomizations> = battlesnakes
            .iter()
            .map(|bs| {
                (
                    bs.game_battlesnake_id.to_string(),
                    SnakeCustomizations {
                        color: String::new(),
                        head: String::new(),
                        tail: String::new(),
                        author: bs.owner_name.clone(),
                    },
                )
            })
            .collect();

        // Turn 0, exactly like the Fresh path persists it.
        let frame_0 = game_to_frame(&engine_game, &death_info, &[], &customizations);
        let resume_0 = crate::engine::resume::capture_resume_state(
            &engine_game,
            &death_info,
            &food_eaten,
            &last_moves,
            &snake_contexts,
        )?;
        crate::models::turn::persist_turn(
            pool,
            game_id,
            0,
            serde_json::to_value(&frame_0)?,
            &resume_0,
            &[],
        )
        .await?;

        loop {
            if until_game_over {
                if crate::engine::is_game_over(&engine_game)
                    || engine_game.board.turn >= crate::engine::MAX_TURNS
                {
                    break;
                }
            } else if engine_game.board.turn >= k {
                break;
            }

            let turn = engine_game.board.turn;
            let mut results = Vec::new();
            for (i, bs) in battlesnakes.iter().enumerate() {
                let id = bs.game_battlesnake_id.to_string();
                let alive = engine_game
                    .board
                    .snakes
                    .iter()
                    .any(|s| s.id == id && !s.eliminated_cause.is_eliminated());
                if !alive {
                    continue;
                }
                let direction = if i64::from(turn) >= suicide_from_turn {
                    Direction::Left
                } else {
                    CYCLE[(turn as usize + i) % 4]
                };
                results.push(crate::snake_client::MoveResult {
                    snake_id: id,
                    direction,
                    latency_ms: Some(50 + i as i64),
                    timed_out: false,
                    shout: None,
                    status_code: Some(200),
                    errored: false,
                });
            }

            apply_turn_results(
                &mut engine_game,
                &results,
                &mut death_info,
                &mut food_eaten,
                &mut last_moves,
                &mut snake_contexts,
            )?;

            let frame = game_to_frame(&engine_game, &death_info, &results, &customizations);
            let resume = crate::engine::resume::capture_resume_state(
                &engine_game,
                &death_info,
                &food_eaten,
                &last_moves,
                &snake_contexts,
            )?;
            let moves: Vec<crate::models::turn::SnakeMoveRecord> = results
                .iter()
                .filter_map(|result| {
                    Uuid::parse_str(&result.snake_id).ok().map(|id| {
                        crate::models::turn::SnakeMoveRecord {
                            game_battlesnake_id: id,
                            direction: result.direction.to_string(),
                            latency_ms: result.latency_ms,
                            timed_out: result.timed_out,
                            errored: result.errored,
                        }
                    })
                })
                .collect();
            crate::models::turn::persist_turn(
                pool,
                game_id,
                engine_game.board.turn,
                serde_json::to_value(&frame)?,
                &resume,
                &moves,
            )
            .await?;
        }

        Ok(engine_game)
    }

    /// Turn rows (turn_id, frame_data) for the game, ordered.
    async fn turn_rows(
        pool: &PgPool,
        game_id: Uuid,
    ) -> cja::Result<Vec<(i32, Uuid, serde_json::Value)>> {
        let rows: Vec<(i32, Uuid, serde_json::Value)> = sqlx::query_as(
            "SELECT turn_number, turn_id, frame_data FROM turns
             WHERE game_id = $1 AND frame_data IS NOT NULL ORDER BY turn_number",
        )
        .bind(game_id)
        .fetch_all(pool)
        .await?;
        Ok(rows)
    }

    /// The persisted frame at `turn`.
    async fn frame_at(pool: &PgPool, game_id: Uuid, turn: i32) -> cja::Result<serde_json::Value> {
        let frame: serde_json::Value = sqlx::query_scalar(
            "SELECT frame_data FROM turns WHERE game_id = $1 AND turn_number = $2",
        )
        .bind(game_id)
        .bind(turn)
        .fetch_one(pool)
        .await?;
        Ok(frame)
    }

    /// Alive-snake geometry comparison between a persisted frame and a /move
    /// request body: the frame's Snakes[] includes eliminated snakes (with
    /// Death); the wire board.snakes is alive snakes only. Bodies, health,
    /// and ids must match one-to-one. (Frame coords serialize PascalCase
    /// X/Y; wire coords snake_case x/y.)
    fn alive_geometry_matches(frame: &serde_json::Value, request: &serde_json::Value) -> bool {
        let wire_point = |p: &serde_json::Value| (p["x"].as_i64(), p["y"].as_i64());
        let frame_point = |p: &serde_json::Value| (p["X"].as_i64(), p["Y"].as_i64());
        let frame_alive: Vec<&serde_json::Value> = frame["Snakes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["EliminatedCause"].as_str().unwrap_or_default().is_empty())
            .collect();
        let wire_snakes: Vec<&serde_json::Value> = request["board"]["snakes"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        if frame_alive.len() != wire_snakes.len() {
            return false;
        }
        frame_alive.iter().all(|fs| {
            wire_snakes.iter().any(|ws| {
                ws["id"] == fs["ID"]
                    && ws["health"].as_i64() == fs["Health"].as_i64()
                    && ws["body"]
                        .as_array()
                        .map(|b| b.iter().map(wire_point).collect::<Vec<_>>())
                        == fs["Body"]
                            .as_array()
                            .map(|b| b.iter().map(frame_point).collect::<Vec<_>>())
            })
        })
    }

    /// The core acceptance test: a Running game with persisted turns 0..=5
    /// and live resume state continues from turn 5 — no deleted turns, no
    /// second /start, one game_resumed event, and the first resumed /move
    /// carries turn 5 with the persisted turn-5 frame's alive geometry.
    #[sqlx::test(migrations = "../migrations")]
    async fn running_game_resumes_from_last_persisted_turn(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ],
        )
        .await?;
        play_persisted_turns(&pool, game_id, 5, i64::MAX, false).await?;

        let before = turn_rows(&pool, game_id).await?;
        assert_eq!(before.len(), 6, "fixture persisted turns 0..=5");

        let capture = EventCapture::default();
        async { run_game(&app_state, game_id).await }
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;

        // (1) exactly one game_resumed, naming the game, turn 5, job_attempt
        // -1 (direct run_game call: no job worker, no locked row).
        {
            let events = capture.0.lock().unwrap();
            let resumed: Vec<_> = events
                .iter()
                .filter(|e| e.get("event_type").map(String::as_str) == Some("game_resumed"))
                .collect();
            assert_eq!(resumed.len(), 1, "{events:?}");
            assert_eq!(resumed[0].get("game_id"), Some(&game_id.to_string()));
            assert_eq!(
                resumed[0].get("resumed_from_turn").map(String::as_str),
                Some("5")
            );
            assert_eq!(
                resumed[0].get("job_attempt").map(String::as_str),
                Some("-1")
            );
            // (2) zero game_started events.
            assert!(
                events
                    .iter()
                    .all(|e| { e.get("event_type").map(String::as_str) != Some("game_started") })
            );
            // (5) finished, one game_completed.
            let completed: Vec<_> = events
                .iter()
                .filter(|e| e.get("event_type").map(String::as_str) == Some("game_completed"))
                .collect();
            assert_eq!(completed.len(), 1);
        }

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");
        let placements: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM game_battlesnakes WHERE game_id = $1 AND placement IS NOT NULL",
        )
        .bind(game_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(placements, 2, "placements set for every snake");

        // (3) turns 0..=5 unchanged (same turn_ids and frames).
        let after = turn_rows(&pool, game_id).await?;
        assert!(after.len() >= 6);
        for (i, row) in before.iter().enumerate() {
            assert_eq!(&after[i], row, "turn {} must keep its id and frame", row.0);
        }
        // (4) contiguous 0..=final.
        for (i, row) in after.iter().enumerate() {
            assert_eq!(row.0, i as i32, "turn numbers must be contiguous");
        }

        // (6) no /start was received by any snake server.
        for server in &servers {
            assert!(
                received_bodies(server, "/start").await.is_empty(),
                "resumed game must not send /start"
            );
        }
        // (7) post-completion jobs enqueued exactly once.
        assert_eq!(count_jobs(&pool, "AwardAchievementsForGameJob").await?, 1);
        assert_eq!(count_jobs(&pool, "ScreenShoutsJob").await?, 1);

        // (8) the first /move request carries turn 5 and the persisted
        // turn-5 frame's alive-snake geometry.
        let frame_5 = frame_at(&pool, game_id, 5).await?;
        for server in &servers {
            let moves = received_bodies(server, "/move").await;
            let first = moves
                .iter()
                .min_by_key(|r| r["turn"].as_i64().unwrap_or(i64::MAX))
                .expect("each snake received /move");
            assert_eq!(first["turn"].as_i64(), Some(5));
            assert!(
                alive_geometry_matches(&frame_5, first),
                "first resumed /move must match the persisted turn-5 frame"
            );
        }

        Ok(())
    }

    /// A kill mid-persist leaves a partial tail row (frame present, some
    /// snake_turns missing) — the production write path can no longer
    /// produce it, so the fixture hand-inserts the exact shape. The resume
    /// must discard turn 6 and resume from turn 5.
    #[sqlx::test(migrations = "../migrations")]
    async fn resume_discards_partially_persisted_tail(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ],
        )
        .await?;
        play_persisted_turns(&pool, game_id, 5, i64::MAX, false).await?;

        // The simulated kill point: turn 6 has a frame but only ONE of two
        // snake_turns rows.
        let frame_5 = frame_at(&pool, game_id, 5).await?;
        let snake_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT game_battlesnake_id FROM game_battlesnakes
             WHERE game_id = $1 ORDER BY created_at, game_battlesnake_id",
        )
        .bind(game_id)
        .fetch_all(&pool)
        .await?;
        let partial_turn_id: Uuid = sqlx::query_scalar(
            "INSERT INTO turns (game_id, turn_number, frame_data)
             VALUES ($1, 6, $2) RETURNING turn_id",
        )
        .bind(game_id)
        .bind(frame_5)
        .fetch_one(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction)
             VALUES ($1, $2, 'up')",
        )
        .bind(partial_turn_id)
        .bind(snake_ids[0])
        .execute(&pool)
        .await?;

        let capture = EventCapture::default();
        async { run_game(&app_state, game_id).await }
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;

        {
            let events = capture.0.lock().unwrap();
            let resumed: Vec<_> = events
                .iter()
                .filter(|e| e.get("event_type").map(String::as_str) == Some("game_resumed"))
                .collect();
            assert_eq!(resumed.len(), 1);
            assert_eq!(
                resumed[0].get("resumed_from_turn").map(String::as_str),
                Some("5")
            );
        }

        // The stale turn-6 row is gone, replaced by a new one with a
        // different turn_id and a full set of snake_turns.
        let new_turn_id: Uuid =
            sqlx::query_scalar("SELECT turn_id FROM turns WHERE game_id = $1 AND turn_number = 6")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_ne!(new_turn_id, partial_turn_id);
        let moves: i64 = sqlx::query_scalar("SELECT count(*) FROM snake_turns WHERE turn_id = $1")
            .bind(new_turn_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(moves, 2, "the re-persisted turn 6 is complete");

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");
        assert!(servers.len() >= 2);
        Ok(())
    }

    /// The timeout-fallback acceptance test, constructed to FAIL when resume
    /// drops last_moves: snake 1's wiremock times out on the first resumed
    /// /move (turn 5), and its turn-6 snake_turns row must record the
    /// direction it last moved BEFORE the interruption — CYCLE[(4+1)%4] =
    /// Right — not the "up" default an empty last_moves would produce.
    #[sqlx::test(migrations = "../migrations")]
    async fn resume_times_out_into_pre_resume_direction(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, _servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: 12,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: 12,
                    timeout_on_turn: Some(5),
                    seek_food: false,
                },
            ],
        )
        .await?;
        play_persisted_turns(&pool, game_id, 5, i64::MAX, false).await?;

        // Fixture control: snake 1's turn-5 row records the move requested
        // at board turn 4 = CYCLE[(4+1)%4] = Right.
        let snake_1: Uuid = sqlx::query_scalar(
            "SELECT gb.game_battlesnake_id FROM game_battlesnakes gb
             JOIN battlesnakes b ON b.battlesnake_id = gb.battlesnake_id
             WHERE gb.game_id = $1 AND b.name = 'resume-snake-1'",
        )
        .bind(game_id)
        .fetch_one(&pool)
        .await?;
        let turn_5_direction: String = sqlx::query_scalar(
            "SELECT st.direction FROM snake_turns st
             JOIN turns t ON t.turn_id = st.turn_id
             WHERE t.game_id = $1 AND t.turn_number = 5 AND st.game_battlesnake_id = $2",
        )
        .bind(game_id)
        .bind(snake_1)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            turn_5_direction, "right",
            "fixture control: pre-resume move"
        );

        let capture = EventCapture::default();
        async { run_game(&app_state, game_id).await }
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;
        {
            let events = capture.0.lock().unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| { e.get("event_type").map(String::as_str) == Some("game_resumed") })
            );
        }

        // The first resumed /move (turn 5) times out; the fallback direction
        // must be the restored last move, recorded on the turn-6 row.
        let (direction, timed_out): (String, bool) = sqlx::query_as(
            "SELECT st.direction, st.timed_out FROM snake_turns st
             JOIN turns t ON t.turn_id = st.turn_id
             WHERE t.game_id = $1 AND t.turn_number = 6 AND st.game_battlesnake_id = $2",
        )
        .bind(game_id)
        .bind(snake_1)
        .fetch_one(&pool)
        .await?;
        assert!(timed_out, "snake 1 timed out on the first resumed turn");
        assert_eq!(
            direction, "right",
            "timeout fallback must use the pre-resume direction, not the up default"
        );

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");
        Ok(())
    }

    /// Old-binary shape: turns exist but no resume state (exactly what a
    /// pre-DEV-1569 binary leaves). The fallback fires with
    /// reason=no_resume_state and today's reset behavior: all pre-existing
    /// turn_ids gone, /start once per snake, complete from turn 0.
    #[sqlx::test(migrations = "../migrations")]
    async fn resume_fallback_when_no_resume_state(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ],
        )
        .await?;

        // Old-style turns via create_turn + raw snake_turns: no resume rows.
        let battlesnakes =
            crate::models::game_battlesnake::get_battlesnakes_by_game_id(&pool, game_id).await?;
        let mut old_turn_ids = Vec::new();
        for turn in 0..=5i32 {
            let created = crate::models::turn::create_turn(
                &pool,
                game_id,
                turn,
                Some(serde_json::json!({"Turn": turn, "Snakes": []})),
            )
            .await?;
            old_turn_ids.push(created.turn_id);
            for bs in &battlesnakes {
                sqlx::query(
                    "INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction)
                     VALUES ($1, $2, 'up')",
                )
                .bind(created.turn_id)
                .bind(bs.game_battlesnake_id)
                .execute(&pool)
                .await?;
            }
        }

        let capture = EventCapture::default();
        async { run_game(&app_state, game_id).await }
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;

        {
            let events = capture.0.lock().unwrap();
            let fallbacks: Vec<_> = events
                .iter()
                .filter(|e| e.get("event_type").map(String::as_str) == Some("game_resume_fallback"))
                .collect();
            assert_eq!(fallbacks.len(), 1, "{events:?}");
            assert_eq!(fallbacks[0].get("game_id"), Some(&game_id.to_string()));
            assert_eq!(
                fallbacks[0].get("reason").map(String::as_str),
                Some("no_resume_state")
            );
        }

        // The reset wiped the old turn rows; the replay completed from 0.
        let rows = turn_rows(&pool, game_id).await?;
        assert!(!rows.is_empty());
        let surviving: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM turns WHERE game_id = $1 AND turn_id = ANY($2)",
        )
        .bind(game_id)
        .bind(&old_turn_ids)
        .fetch_one(&pool)
        .await?;
        assert_eq!(surviving, 0, "all pre-change turn_ids gone");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.0, i as i32);
        }

        // /start once per snake: the replayed game sends /start.
        for server in &servers {
            assert_eq!(received_bodies(server, "/start").await.len(), 1);
        }

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");
        Ok(())
    }

    /// Terminal-turn resume: the worker died after the final turn persisted
    /// (or after /end but before the finish committed). The resumed run
    /// makes zero /move calls, /end is sent (a duplicate accepted),
    /// placements and post-completion jobs run exactly once, and the resume
    /// state is deleted at finish.
    #[sqlx::test(migrations = "../migrations")]
    async fn resumed_game_finishes_from_terminal_turn(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: i64::MAX,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: i64::MAX,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ],
        )
        .await?;
        let final_board = play_persisted_turns(&pool, game_id, 0, 8, true).await?;
        assert!(
            crate::engine::is_game_over(&final_board),
            "fixture must reach game over"
        );
        let expected_placements = crate::placement::from_final_snakes(&final_board.board.snakes);

        // Simulate an attempt that died after /end but before the finish
        // committed: send one /end directly to snake 0's server.
        let client = reqwest::Client::new();
        client
            .post(format!("{}/end", servers[0].uri()))
            .json(&serde_json::json!({"game": {"id": game_id.to_string()}}))
            .send()
            .await?;

        let capture = EventCapture::default();
        async { run_game(&app_state, game_id).await }
            .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
            .await?;

        for server in &servers {
            assert!(
                received_bodies(server, "/move").await.is_empty(),
                "terminal resume makes zero /move calls"
            );
        }
        // The pre-messed snake has two /end (a duplicate accepted); the
        // other has one.
        assert_eq!(received_bodies(&servers[0], "/end").await.len(), 2);
        assert_eq!(received_bodies(&servers[1], "/end").await.len(), 1);

        {
            let events = capture.0.lock().unwrap();
            let completed: Vec<_> = events
                .iter()
                .filter(|e| e.get("event_type").map(String::as_str) == Some("game_completed"))
                .collect();
            assert_eq!(completed.len(), 1);
        }

        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "finished");
        assert_eq!(count_jobs(&pool, "AwardAchievementsForGameJob").await?, 1);

        let recorded: Vec<(Uuid, i32)> = sqlx::query_as(
            "SELECT game_battlesnake_id, placement FROM game_battlesnakes
             WHERE game_id = $1 ORDER BY placement",
        )
        .bind(game_id)
        .fetch_all(&pool)
        .await?;
        for (game_battlesnake_id, placement) in recorded {
            let expected = expected_placements
                .iter()
                .find(|(id, _)| id == &game_battlesnake_id.to_string())
                .map(|(_, p)| *p)
                .expect("expected placement for snake");
            assert_eq!(placement, expected);
        }
        Ok(())
    }

    /// An instance-wide DB failure inside the resume step propagates as a
    /// job error — it must NEVER degrade into the reset fallback — and a
    /// second attempt errs identically (deterministic failures keep
    /// dead-lettering through cja's error_count).
    #[sqlx::test(migrations = "../migrations")]
    async fn resume_step_db_error_propagates_without_reset(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, _servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ],
        )
        .await?;
        play_persisted_turns(&pool, game_id, 5, i64::MAX, false).await?;

        // Simulate the DB outage deterministically: every resume-step query
        // fails. Raw SQL (no sqlx macro, no .sqlx cache entry needed).
        sqlx::query("DROP TABLE game_resume_states CASCADE")
            .execute(&pool)
            .await?;

        let capture = EventCapture::default();
        let first = {
            let capture = capture.clone();
            async { run_game(&app_state, game_id).await }
                .with_subscriber(tracing_subscriber::registry().with(capture.clone()))
                .await
        };
        assert!(
            first.is_err(),
            "the DB error must propagate out of run_game"
        );

        // No fallback was logged, the turns are intact, and the game is
        // still running (no reset wiped them).
        {
            let events = capture.0.lock().unwrap();
            assert!(events.iter().all(|e| {
                e.get("event_type").map(String::as_str) != Some("game_resume_fallback")
            }));
        }
        let turns: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(turns, 6, "turns 0..=5 intact");
        let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
            .bind(game_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, "running");

        // A second run errs identically: resume never converts a
        // deterministic failure into a reset loop.
        let second = run_game(&app_state, game_id).await;
        assert!(second.is_err());

        Ok(())
    }

    /// Finishing deletes the resume state: after game_completed,
    /// game_resume_states and game_resume_snakes are empty.
    #[sqlx::test(migrations = "../migrations")]
    async fn finishing_game_deletes_resume_state(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (game_id, _servers) = resume_fixture(
            &pool,
            &[
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
                ScriptedSnake {
                    suicide_from_turn: 15,
                    timeout_on_turn: None,
                    seek_food: false,
                },
            ],
        )
        .await?;
        play_persisted_turns(&pool, game_id, 5, i64::MAX, false).await?;

        let state_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM game_resume_states WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(state_rows, 1, "fixture wrote live resume state");

        run_game(&app_state, game_id).await?;

        let state_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM game_resume_states WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(state_rows, 0);
        let snake_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM game_resume_snakes WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(snake_rows, 0);
        Ok(())
    }

    /// cja's attempt counter, read off the locked jobs row: None before a
    /// worker claim, the row's error_count after.
    #[sqlx::test(migrations = "../migrations")]
    async fn current_job_attempt_reads_the_locked_row(pool: PgPool) -> cja::Result<()> {
        use cja::jobs::Job as _;
        let app_state = AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "waiting").await?;

        crate::jobs::GameRunnerJob { game_id }
            .enqueue(app_state.clone(), "attempt-test".to_owned(), None)
            .await?;

        // Unlocked row: not visible.
        assert_eq!(current_job_attempt(&pool, game_id).await?, None);

        // Simulate a worker claim (an UPDATE is fine — the rule forbids
        // hand-written INSERTs that bypass cja's enqueue receipts).
        sqlx::query(
            "UPDATE jobs SET locked_by = 'attempt-test', locked_at = NOW(), error_count = 2
             WHERE name = 'GameRunnerJob' AND payload->>'game_id' = $1",
        )
        .bind(game_id.to_string())
        .execute(&pool)
        .await?;

        assert_eq!(current_job_attempt(&pool, game_id).await?, Some(2));
        Ok(())
    }
}
