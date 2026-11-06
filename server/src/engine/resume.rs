//! Live per-game resume state: the typed contract for
//! `game_resume_states` / `game_resume_snakes` plus capture/restore.
//!
//! Frames are the viewer contract and are lossy (`game_to_frame` drops
//! Snail Mode's off-board pending-trail bookkeeping; latency/shout in
//! frames are display strings), so resume state is captured from the live
//! engine + runner maps — never reconstructed from frames. All SQL lives in
//! [`crate::models::resume_state`].

use std::collections::HashMap;

use color_eyre::eyre::Context as _;
use rules::{BoardState, Direction, EliminationCause, Point, Snake};
use uuid::Uuid;

use crate::engine::frame::DeathInfo;
use crate::engine::{EngineGame, GameSource, MOVE_TIMEOUT_MS};
use crate::models::game::Game;
use crate::models::game_battlesnake::GameBattlesnakeWithDetails;
use engine_core::GameMeta;
use engine_core::wire::SnakeContext;

/// The per-snake resume record — one row of `game_resume_snakes`. Used by
/// both the write path (capture) and the read path (load), so the two
/// cannot disagree about the shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeSnakeRecord {
    pub game_battlesnake_id: Uuid,
    pub health: i32,
    /// serde snake_case text of `rules::EliminationCause`.
    pub eliminated_cause: String,
    pub eliminated_by: String,
    pub eliminated_on_turn: i32,
    /// Direction wire text; `None` = no `last_moves` entry.
    pub last_move: Option<String>,
    /// The runner's `snake_contexts` holds an entry for this snake.
    pub context_present: bool,
    /// `SnakeContext.latency_ms` (i64; the column narrows to i32 like
    /// `snake_turns.latency_ms`).
    pub latency_ms: Option<i64>,
    pub shout: Option<String>,
    pub food_eaten: i32,
    /// `death_*` are all-`None` as a group while the snake has no
    /// `death_info` entry; `death_order` is its position in the runner's
    /// `death_info` vector.
    pub death_turn: Option<i32>,
    pub death_cause: Option<String>,
    pub death_eliminated_by: Option<String>,
    pub death_order: Option<i32>,
    /// JSONB array of `{x,y}` `rules::Point`.
    pub body: serde_json::Value,
}

/// The full resume state — one `game_resume_states` row plus its snake rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeStateData {
    pub turn_number: i32,
    /// JSONB array of `{x,y}` `rules::Point`.
    pub food: serde_json::Value,
    /// The RAW `board.hazards` list: Snail Mode off-board bookkeeping
    /// points included, order and duplicates preserved — deliberately NOT
    /// the `on_board_hazards()` view frames serialize.
    pub hazards: serde_json::Value,
    pub snakes: Vec<ResumeSnakeRecord>,
}

/// Serialize an `EliminationCause` to its stable serde snake_case text.
fn cause_text(cause: &EliminationCause) -> cja::Result<String> {
    let value = serde_json::to_value(cause)
        .wrap_err_with(|| format!("failed to serialize elimination cause {cause:?}"))?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| cja::color_eyre::eyre::eyre!("elimination cause serialized to non-string"))
}

/// Capture the runner's live state at persist time. `last_moves`,
/// `snake_contexts`, `food_eaten` and the death group are the runner caches;
/// they must be the ones built from THIS turn's move results (the runner
/// updates them before persisting). Fails only when a snake id is not a UUID.
pub fn capture_resume_state(
    engine_game: &EngineGame,
    death_info: &[DeathInfo],
    food_eaten: &HashMap<String, i32>,
    last_moves: &HashMap<String, Direction>,
    snake_contexts: &HashMap<String, SnakeContext>,
) -> cja::Result<ResumeStateData> {
    let mut snakes = Vec::with_capacity(engine_game.board.snakes.len());
    for snake in &engine_game.board.snakes {
        let game_battlesnake_id = Uuid::parse_str(&snake.id)
            .wrap_err_with(|| format!("snake id {} is not a game_battlesnake UUID", snake.id))?;
        let context = snake_contexts.get(&snake.id);
        // Position in the runner's death_info vector, when it has one.
        let death = death_info.iter().position(|d| d.snake_id == snake.id);
        let death = death.map(|index| (index as i32, &death_info[index]));
        snakes.push(ResumeSnakeRecord {
            game_battlesnake_id,
            health: snake.health,
            eliminated_cause: cause_text(&snake.eliminated_cause)?,
            eliminated_by: snake.eliminated_by.clone(),
            eliminated_on_turn: snake.eliminated_on_turn,
            last_move: last_moves.get(&snake.id).map(Direction::to_string),
            context_present: context.is_some(),
            latency_ms: context.and_then(|c| c.latency_ms),
            shout: context.and_then(|c| c.shout.clone()),
            food_eaten: food_eaten.get(&snake.id).copied().unwrap_or(0),
            death_turn: death.map(|(_, d)| d.turn),
            death_cause: death.map(|(_, d)| d.cause.clone()),
            death_eliminated_by: death.map(|(_, d)| d.eliminated_by.clone()),
            death_order: death.map(|(index, _)| index),
            body: serde_json::to_value(&snake.body)
                .wrap_err_with(|| format!("failed to serialize body of snake {}", snake.id))?,
        });
    }

    Ok(ResumeStateData {
        turn_number: engine_game.board.turn,
        food: serde_json::to_value(&engine_game.board.food)
            .wrap_err("failed to serialize food points")?,
        hazards: serde_json::to_value(&engine_game.board.hazards)
            .wrap_err("failed to serialize hazard points")?,
        snakes,
    })
}

/// Everything `run_game` needs to continue a game, restored from
/// [`ResumeStateData`]. Meta, Royale settings, and board dimensions are
/// rebuilt from `mode_rules` + the game row (the same inputs
/// `create_initial_game` uses), NOT from the stored state.
#[derive(Debug, PartialEq)]
pub struct ResumedState {
    pub engine_game: EngineGame,
    pub death_info: Vec<DeathInfo>,
    pub food_eaten: HashMap<String, i32>,
    pub last_moves: HashMap<String, Direction>,
    pub snake_contexts: HashMap<String, SnakeContext>,
    pub resumed_from_turn: i32,
}

/// Why a restore failed. [`fallback_reason`] maps these to the stable
/// `game_resume_fallback` WARN slugs.
#[derive(Debug)]
pub enum RestoreError {
    /// Stored point JSON or enum text failed to parse.
    Parse(String),
    /// The state parsed but does not describe a game we trust.
    Validation(String),
}

impl RestoreError {
    fn parse(context: &str, error: impl std::fmt::Display) -> Self {
        RestoreError::Parse(format!("{context}: {error}"))
    }

    fn validation(context: &str) -> Self {
        RestoreError::Validation(context.to_string())
    }
}

/// The stable fallback reason slug plus the free-text error chain for the
/// `game_resume_fallback` WARN.
pub fn fallback_reason(error: &RestoreError) -> (&'static str, String) {
    match error {
        RestoreError::Parse(detail) => ("unparseable_resume_state", detail.clone()),
        RestoreError::Validation(detail) => ("snapshot_validation_failed", detail.clone()),
    }
}

/// Parse a JSONB point list (`food`, `hazards`, `body`).
fn parse_points(value: &serde_json::Value, what: &str) -> Result<Vec<Point>, RestoreError> {
    serde_json::from_value::<Vec<Point>>(value.clone())
        .map_err(|error| RestoreError::parse(&format!("failed to parse {what} points"), error))
}

fn parse_elimination_cause(text: &str) -> Result<EliminationCause, RestoreError> {
    serde_json::from_value::<EliminationCause>(serde_json::Value::String(text.to_owned()))
        .map_err(|error| RestoreError::parse("failed to parse eliminated_cause", error))
}

/// Validate stored state against the game it claims to describe and rebuild
/// the full `EngineGame` + runner maps. Fails when:
///   - point JSON in food/hazards/body fails to parse, or eliminated_cause /
///     last_move text fails to parse          → `unparseable_resume_state`
///   - the record snake-id set and the roster's game_battlesnake_id set
///     differ in either direction (set equality)
///   - a snake's body is empty (a corrupt row that would panic the engine)
///   - the death_* columns are not all-NULL-or-all-present for a snake, or
///     death_order values collide (order must rebuild exactly)
///   - a `food` point, or a body point of a NOT-eliminated snake, lies
///     outside the board dimensions derived from the game row
///     → `snapshot_validation_failed`
///
/// Bounds are checked on exactly those two point sets and nothing else:
/// `hazards` are exempt (Snail Mode stores pending tails off-board at
/// `y + board.height`), and eliminated snakes' bodies are exempt (an
/// OutOfBounds elimination keeps the off-board head in `body`).
pub fn restore(
    state: ResumeStateData,
    game: &Game,
    game_source: GameSource,
    battlesnakes: &[GameBattlesnakeWithDetails],
) -> Result<ResumedState, RestoreError> {
    let food = parse_points(&state.food, "food")?;
    // Raw list: order and duplicates preserved, off-board points kept.
    let hazards = parse_points(&state.hazards, "hazards")?;

    // Meta is rebuilt from the same inputs create_initial_game uses — never
    // trusted from the stored state.
    let (ruleset_name, settings, royale) =
        crate::engine::mode_rules(game.game_type.clone(), game.game_id);
    let (width, height) = game.board_size.dimensions();
    let (width, height) = (width as i32, height as i32);

    let mut snake_names = HashMap::with_capacity(battlesnakes.len());
    for bs in battlesnakes {
        snake_names.insert(bs.game_battlesnake_id.to_string(), bs.name.clone());
    }

    // Set equality between record ids and roster ids, both directions.
    let record_ids: std::collections::HashSet<Uuid> =
        state.snakes.iter().map(|s| s.game_battlesnake_id).collect();
    let roster_ids: std::collections::HashSet<Uuid> = battlesnakes
        .iter()
        .map(|bs| bs.game_battlesnake_id)
        .collect();
    if record_ids != roster_ids {
        return Err(RestoreError::validation(
            "resume snake ids do not match the game roster",
        ));
    }
    let records_by_id: HashMap<Uuid, ResumeSnakeRecord> = state
        .snakes
        .into_iter()
        .map(|s| (s.game_battlesnake_id, s))
        .collect();

    // Snakes in roster order — the order create_initial_game shaped them in
    // (rules never reorders board.snakes, so this reproduces the live order).
    let mut snakes = Vec::with_capacity(battlesnakes.len());
    let mut last_moves = HashMap::new();
    let mut snake_contexts = HashMap::new();
    let mut food_eaten = HashMap::new();
    let mut deaths: Vec<(i32, DeathInfo)> = Vec::new();

    for bs in battlesnakes {
        let record = &records_by_id[&bs.game_battlesnake_id];
        let id = bs.game_battlesnake_id.to_string();

        let body = parse_points(&record.body, "body")?;
        if body.is_empty() {
            return Err(RestoreError::validation(&format!(
                "snake {id} has an empty body"
            )));
        }
        let eliminated_cause = parse_elimination_cause(&record.eliminated_cause)?;

        if eliminated_cause == EliminationCause::NotEliminated {
            let off_board = body
                .iter()
                .find(|p| p.x < 0 || p.x >= width || p.y < 0 || p.y >= height);
            if let Some(point) = off_board {
                return Err(RestoreError::validation(&format!(
                    "live snake {id} has body point ({}, {}) outside the {width}x{height} board",
                    point.x, point.y
                )));
            }
        }

        if let Some(last_move) = &record.last_move {
            let direction: Direction = last_move
                .parse()
                .map_err(|()| RestoreError::parse("failed to parse last_move", last_move))?;
            last_moves.insert(id.clone(), direction);
        }
        if record.context_present {
            snake_contexts.insert(
                id.clone(),
                SnakeContext {
                    latency_ms: record.latency_ms,
                    shout: record.shout.clone(),
                },
            );
        }
        if record.food_eaten > 0 {
            food_eaten.insert(id.clone(), record.food_eaten);
        }

        // The death group must be all-None or all-present together.
        let death_group = [
            record.death_turn.is_some(),
            record.death_cause.is_some(),
            record.death_eliminated_by.is_some(),
            record.death_order.is_some(),
        ];
        if death_group.iter().any(|b| *b) && !death_group.iter().all(|b| *b) {
            return Err(RestoreError::validation(&format!(
                "snake {id} has a half-present death group"
            )));
        }
        if let (Some(turn), Some(cause), Some(eliminated_by), Some(order)) = (
            record.death_turn,
            record.death_cause.clone(),
            record.death_eliminated_by.clone(),
            record.death_order,
        ) {
            deaths.push((
                order,
                DeathInfo {
                    snake_id: id.clone(),
                    turn,
                    cause,
                    eliminated_by,
                },
            ));
        }

        snakes.push(Snake {
            id,
            body,
            health: record.health,
            eliminated_cause,
            eliminated_by: record.eliminated_by.clone(),
            eliminated_on_turn: record.eliminated_on_turn,
        });
    }

    // death_order values must not collide: order must rebuild exactly.
    let mut orders: Vec<i32> = deaths.iter().map(|(order, _)| *order).collect();
    orders.sort_unstable();
    orders.dedup();
    if orders.len() != deaths.len() {
        return Err(RestoreError::validation("colliding death_order values"));
    }
    deaths.sort_by_key(|(order, _)| *order);
    let death_info: Vec<DeathInfo> = deaths.into_iter().map(|(_, d)| d).collect();

    // Food must be on the board.
    if let Some(point) = food
        .iter()
        .find(|p| p.x < 0 || p.x >= width || p.y < 0 || p.y >= height)
    {
        return Err(RestoreError::validation(&format!(
            "food point ({}, {}) outside the {width}x{height} board",
            point.x, point.y
        )));
    }

    let board = BoardState {
        turn: state.turn_number,
        width,
        height,
        food,
        snakes,
        hazards,
    };
    let engine_game = EngineGame {
        board,
        meta: GameMeta {
            game_id: game.game_id.to_string(),
            ruleset_name: ruleset_name.to_string(),
            timeout: MOVE_TIMEOUT_MS,
            settings,
            royale,
            source: game_source,
        },
        snake_names,
    };

    Ok(ResumedState {
        engine_game,
        death_info,
        food_eaten,
        last_moves,
        snake_contexts,
        resumed_from_turn: state.turn_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{GameSource, create_initial_game as server_create_initial_game};
    use crate::models::game::{GameBoardSize, GameStatus, GameType};
    use crate::models::game_battlesnake::GameBattlesnakeWithDetails;
    use engine_core::GameMode;
    use engine_core::move_response::MoveResult;
    use rules::{Direction, StandardSettings};

    /// CYCLE-indexed walk: the same 2x2 square walk engine-core's own tests
    /// use (`play_scripted_game`).
    const CYCLE: [Direction; 4] = [
        Direction::Up,
        Direction::Right,
        Direction::Down,
        Direction::Left,
    ];

    const MODES: [(GameMode, GameType); 5] = [
        (GameMode::Standard, GameType::Standard),
        (GameMode::Royale, GameType::Royale),
        (GameMode::Constrictor, GameType::Constrictor),
        (GameMode::SnailMode, GameType::SnailMode),
        (GameMode::Solo, GameType::Solo),
    ];

    fn fixture_game(game_type: GameType, game_id: Uuid) -> Game {
        Game {
            game_id,
            board_size: GameBoardSize::Medium,
            game_type,
            status: GameStatus::Running,
            enqueued_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn roster_from(ids: &[Uuid]) -> Vec<GameBattlesnakeWithDetails> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| GameBattlesnakeWithDetails {
                game_battlesnake_id: *id,
                game_id: Uuid::new_v4(),
                battlesnake_id: Uuid::new_v4(),
                placement: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                name: format!("Snake {i}"),
                url: String::new(),
                engine_region: crate::models::battlesnake::EngineRegion::UsWest1,
                user_id: Uuid::new_v4(),
                leaderboard_entry_id: None,
                color: String::new(),
                owner_login: String::new(),
                owner_name: String::new(),
                head: String::new(),
                tail: String::new(),
            })
            .collect()
    }

    /// The live runner state: engine game plus the four bookkeeping maps the
    /// runner holds, driven through the production apply_turn_results so the
    /// "interrupted run" cannot drift from the real loop.
    #[derive(Debug)]
    struct LiveRun {
        engine_game: EngineGame,
        death_info: Vec<DeathInfo>,
        food_eaten: HashMap<String, i32>,
        last_moves: HashMap<String, Direction>,
        snake_contexts: HashMap<String, SnakeContext>,
        snake_ids: Vec<Uuid>,
    }

    impl LiveRun {
        /// Start a seeded game (same seed → same board), like engine-core's
        /// own `play_scripted_game`.
        fn start(mode: GameMode, seed: u64, snake_count: usize) -> (Self, Uuid) {
            let (mut rng, game_id) = engine_core::seeded_game(seed);
            let snake_ids: Vec<Uuid> = (0..snake_count).map(|_| Uuid::new_v4()).collect();
            let snakes: Vec<engine_core::InitialSnake> = snake_ids
                .iter()
                .enumerate()
                .map(|(i, id)| engine_core::InitialSnake {
                    id: id.to_string(),
                    name: format!("Snake {i}"),
                })
                .collect();
            let engine_game =
                engine_core::create_initial_game(&mut rng, game_id, 11, 11, mode, &snakes)
                    .expect("valid initial game");
            (
                LiveRun {
                    engine_game,
                    death_info: Vec::new(),
                    food_eaten: HashMap::new(),
                    last_moves: HashMap::new(),
                    snake_contexts: HashMap::new(),
                    snake_ids,
                },
                game_id,
            )
        }

        fn alive_ids(&self) -> Vec<String> {
            self.engine_game
                .board
                .snakes
                .iter()
                .filter(|s| !s.eliminated_cause.is_eliminated())
                .map(|s| s.id.clone())
                .collect()
        }

        /// One turn's synthesized MoveResults. CYCLE-indexed per roster
        /// index for every mode except Snail Mode, which walks a monotone
        /// center-directed path so pending-trail bookkeeping points actually
        /// exist (a 2x2 cycle walk keeps the head on its pending-tail square
        /// and no trail ever appears).
        fn scripted_results(&self, monotone: &HashMap<String, Direction>) -> Vec<MoveResult> {
            self.engine_game
                .board
                .snakes
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.eliminated_cause.is_eliminated())
                .map(|(i, s)| {
                    let direction = monotone
                        .get(&s.id)
                        .copied()
                        .unwrap_or(CYCLE[(self.engine_game.board.turn as usize + i) % 4]);
                    MoveResult {
                        snake_id: s.id.clone(),
                        direction,
                        latency_ms: Some(50 + i as i64),
                        timed_out: false,
                        shout: (i % 2 == 0).then(|| format!("shout {i}")),
                        status_code: Some(200),
                        errored: false,
                    }
                })
                .collect()
        }

        fn drive(&mut self, results: &[MoveResult]) {
            crate::game_runner::apply_turn_results(
                &mut self.engine_game,
                results,
                &mut self.death_info,
                &mut self.food_eaten,
                &mut self.last_moves,
                &mut self.snake_contexts,
            )
            .expect("valid scripted turn");
        }

        fn capture(&self) -> ResumeStateData {
            capture_resume_state(
                &self.engine_game,
                &self.death_info,
                &self.food_eaten,
                &self.last_moves,
                &self.snake_contexts,
            )
            .expect("capture")
        }

        fn restore_from(&self, state: ResumeStateData, game: &Game, game_id: Uuid) -> ResumedState {
            restore(
                state,
                game,
                GameSource::Custom,
                &roster_from(&self.snake_ids),
            )
            .unwrap_or_else(|error| panic!("restore failed for game {game_id}: {error:?}"))
        }
    }

    /// For Snail Mode: a fixed monotone direction per snake, center-directed
    /// on the y axis so the walk has room for the whole test.
    fn snail_monotone(run: &LiveRun) -> HashMap<String, Direction> {
        run.engine_game
            .board
            .snakes
            .iter()
            .map(|s| {
                let head = s.head();
                let direction = if head.y < run.engine_game.board.height / 2 {
                    Direction::Up
                } else {
                    Direction::Down
                };
                (s.id.clone(), direction)
            })
            .collect()
    }

    fn turns_for(mode: GameMode) -> i32 {
        if mode == GameMode::Royale {
            // With populate_hazards first firing when turn + 1 >= 25, the
            // turn-26 state carries at least one shrink's hazards.
            26
        } else {
            8
        }
    }

    #[test]
    fn resume_round_trips_every_mode() {
        for (mode, game_type) in MODES {
            let k = turns_for(mode);
            let (mut run, game_id) =
                LiveRun::start(mode, 42, if mode == GameMode::Solo { 1 } else { 2 });
            let monotone = if mode == GameMode::SnailMode {
                snail_monotone(&run)
            } else {
                HashMap::new()
            };

            while run.engine_game.board.turn < k {
                let results = run.scripted_results(&monotone);
                run.drive(&results);
            }

            // Mode state must actually exist before asserting it round-trips.
            if mode == GameMode::SnailMode {
                assert!(
                    run.engine_game
                        .board
                        .hazards
                        .iter()
                        .any(|p| p.y >= run.engine_game.board.height),
                    "snail fixture must produce off-board bookkeeping points"
                );
            }
            if mode == GameMode::Royale {
                assert!(
                    !run.engine_game.board.hazards.is_empty(),
                    "royale fixture must carry shrink hazards at turn {k}"
                );
            }

            let state = run.capture();
            let game = fixture_game(game_type.clone(), game_id);
            let resumed = run.restore_from(state.clone(), &game, game_id);

            // The whole ResumedState, exactly: engine game (snakes, health,
            // bodies, food, RAW hazards, Royale shrink state, eliminations,
            // meta, snake_names), death_info, and every map's membership.
            assert_eq!(
                resumed.engine_game, run.engine_game,
                "{mode:?}: engine state diverged across capture/restore"
            );
            assert_eq!(
                resumed.death_info, run.death_info,
                "{mode:?}: death_info diverged"
            );
            assert_eq!(
                resumed.food_eaten, run.food_eaten,
                "{mode:?}: food_eaten diverged"
            );
            assert_eq!(
                resumed.last_moves, run.last_moves,
                "{mode:?}: last_moves diverged"
            );
            assert_eq!(
                resumed.snake_contexts, run.snake_contexts,
                "{mode:?}: snake_contexts diverged"
            );
            assert_eq!(resumed.resumed_from_turn, k);

            // Snail Mode bookkeeping points survive inside the stored RAW
            // hazards list, order and duplicates preserved.
            if mode == GameMode::SnailMode {
                let stored: Vec<Point> = serde_json::from_value(state.hazards.clone()).unwrap();
                assert!(stored.iter().any(|p| p.y >= run.engine_game.board.height));
                assert_eq!(stored, run.engine_game.board.hazards);
            }

            // The meta rebuild matches create_initial_game's exactly.
            // (resumed == run above already covers the full game; this and
            // the server-façade comparison below pin the meta rebuild
            // against the live creator's inputs.)
            assert_eq!(
                resumed.engine_game.meta, run.engine_game.meta,
                "{mode:?}: meta rebuild diverged"
            );

            // The server façade's create_initial_game agrees on meta for the
            // same game id + type (pins restore against the live creator).
            let server_game = server_create_initial_game(
                game_id,
                GameBoardSize::Medium,
                game_type.clone(),
                &roster_from(&run.snake_ids),
            );
            assert_eq!(resumed.engine_game.meta, server_game.meta);
        }
    }

    #[test]
    fn resumed_state_equals_uninterrupted_at_k() {
        for (mode, game_type) in MODES {
            let k = turns_for(mode);
            let (mut run, game_id) =
                LiveRun::start(mode, 7, if mode == GameMode::Solo { 1 } else { 2 });
            let monotone = if mode == GameMode::SnailMode {
                snail_monotone(&run)
            } else {
                HashMap::new()
            };
            while run.engine_game.board.turn < k {
                let results = run.scripted_results(&monotone);
                run.drive(&results);
            }

            let resumed =
                run.restore_from(run.capture(), &fixture_game(game_type, game_id), game_id);
            assert_eq!(
                resumed.engine_game, run.engine_game,
                "{mode:?}: resumed state at turn {k} != uninterrupted state"
            );
            assert_eq!(resumed.engine_game.meta, run.engine_game.meta);
        }
    }

    /// Constrictor never spawns food, so the game is deterministic given
    /// scripted moves: run to completion uninterrupted vs. resume-at-K; the
    /// final placements, death_info, and elimination order must match.
    ///
    /// Outcome by roster index (LiveRun mints fresh UUIDs per run):
    /// placements, deaths as (index, turn, cause), and elimination order.
    type ConstrictorOutcome = (Vec<(usize, i32)>, Vec<(usize, i32, String)>, Vec<usize>);

    #[test]
    fn resumed_game_matches_full_game_outcome_constrictor() {
        let play_full = |resume_at: Option<i32>| -> ConstrictorOutcome {
            let (mut run, game_id) = LiveRun::start(GameMode::Constrictor, 3, 2);
            // Both snakes suicide by walking left into the wall from turn 20
            // so the game actually finishes.
            let mut monotone: HashMap<String, Direction> = HashMap::new();
            while !engine_core::is_game_over(&run.engine_game)
                && run.engine_game.board.turn < engine_core::MAX_TURNS
            {
                if let Some(k) = resume_at
                    && run.engine_game.board.turn == k
                {
                    let resumed = run.restore_from(
                        run.capture(),
                        &fixture_game(GameType::Constrictor, game_id),
                        game_id,
                    );
                    run.engine_game = resumed.engine_game;
                    run.death_info = resumed.death_info;
                    run.food_eaten = resumed.food_eaten;
                    run.last_moves = resumed.last_moves;
                    run.snake_contexts = resumed.snake_contexts;
                }
                if run.engine_game.board.turn >= 20 {
                    monotone.clear();
                    for s in &run.engine_game.board.snakes {
                        monotone.insert(s.id.clone(), Direction::Left);
                    }
                }
                let results = run.scripted_results(&monotone);
                run.drive(&results);
            }
            let index_of = |id: &str| {
                run.snake_ids
                    .iter()
                    .position(|s| s.to_string() == id)
                    .expect("roster snake")
            };
            (
                engine_core::placement::from_final_snakes(&run.engine_game.board.snakes)
                    .iter()
                    .map(|(id, p)| (index_of(id), *p))
                    .collect(),
                run.death_info
                    .iter()
                    .map(|d| (index_of(&d.snake_id), d.turn, d.cause.clone()))
                    .collect(),
                run.death_info
                    .iter()
                    .map(|d| index_of(&d.snake_id))
                    .collect(),
            )
        };

        let uninterrupted = play_full(None);
        let resumed = play_full(Some(6));
        assert_eq!(resumed.0, uninterrupted.0, "placements diverged");
        assert_eq!(resumed.1, uninterrupted.1, "death_info diverged");
        assert_eq!(resumed.2, uninterrupted.2, "elimination order diverged");
    }

    /// The positive twin of the bounds check: an OutOfBounds elimination
    /// keeps its off-board head in `body`, and Snail Mode stores pending
    /// tails off-board at y + height — both must restore Ok and round-trip
    /// unchanged. The OOB snake is built by a real execute_turn walk into
    /// the wall, not hand-typed.
    #[test]
    fn restore_accepts_legitimate_off_board_points() {
        let (mut run, game_id) = LiveRun::start(GameMode::SnailMode, 5, 2);
        let monotone = snail_monotone(&run);
        // Seed 5 spawns both snakes at y=5, so the center-directed walk is
        // Down for both. Five turns put the heads on the bottom row (y=0,
        // still alive) with pending tails recorded off-board at the end of
        // turn 5. (A sixth turn would walk both into the wall at y=-1;
        // eliminating every snake restores all pending tails on-board and
        // records none, leaving no off-board points to assert on.)
        for _ in 0..5 {
            let results = run.scripted_results(&monotone);
            run.drive(&results);
        }
        assert!(
            run.engine_game
                .board
                .hazards
                .iter()
                .any(|p| p.y >= run.engine_game.board.height)
        );

        // Guarantee an off-board body: walk a real execute_turn into the
        // wall with a hand-built board, then splice it in as one roster
        // snake of the captured state. The bystander keeps the game from
        // being over (execute_turn early-exits on a finished board, and a
        // one-snake standard board counts as over); it walks harmlessly
        // upward, far from the wall walker.
        let oob_snake_id = run.engine_game.board.snakes[0].id.clone();
        let mut board = BoardState {
            turn: 1,
            width: 11,
            height: 11,
            food: vec![],
            snakes: vec![
                Snake {
                    id: oob_snake_id.clone(),
                    body: vec![Point::new(0, 5), Point::new(1, 5), Point::new(2, 5)],
                    health: 90,
                    eliminated_cause: EliminationCause::NotEliminated,
                    eliminated_by: String::new(),
                    eliminated_on_turn: 0,
                },
                Snake {
                    id: "bystander".to_owned(),
                    body: vec![Point::new(9, 7), Point::new(9, 6), Point::new(9, 5)],
                    health: 90,
                    eliminated_cause: EliminationCause::NotEliminated,
                    eliminated_by: String::new(),
                    eliminated_on_turn: 0,
                },
            ],
            hazards: vec![],
        };
        rules::standard::execute_turn(
            &mut board,
            &[
                rules::SnakeMove {
                    id: oob_snake_id.clone(),
                    direction: Direction::Left,
                },
                rules::SnakeMove {
                    id: "bystander".to_owned(),
                    direction: Direction::Up,
                },
            ],
            &StandardSettings::default(),
        )
        .expect("valid turn");
        let oob_snake = &board.snakes[0];
        assert_eq!(oob_snake.eliminated_cause, EliminationCause::OutOfBounds);
        assert_eq!(oob_snake.head(), Point::new(-1, 5));

        let mut state = run.capture();
        let record = state
            .snakes
            .iter_mut()
            .find(|s| s.game_battlesnake_id.to_string() == oob_snake_id)
            .expect("captured snake");
        record.body = serde_json::to_value(&oob_snake.body).unwrap();
        record.eliminated_cause = "out_of_bounds".to_owned();
        record.eliminated_on_turn = 1;

        let resumed = run.restore_from(state, &fixture_game(GameType::SnailMode, game_id), game_id);
        let restored_snake = resumed
            .engine_game
            .board
            .snakes
            .iter()
            .find(|s| s.id == oob_snake_id)
            .unwrap();
        assert_eq!(restored_snake.head(), Point::new(-1, 5));
        assert_eq!(
            restored_snake.eliminated_cause,
            EliminationCause::OutOfBounds
        );
        // Raw hazards round-trip unchanged, off-board pending tails included.
        assert_eq!(
            resumed.engine_game.board.hazards,
            run.engine_game.board.hazards
        );
    }

    fn restore_slug(state: ResumeStateData, roster: &[GameBattlesnakeWithDetails]) -> &'static str {
        let game = fixture_game(GameType::Standard, Uuid::new_v4());
        match restore(state, &game, GameSource::Custom, roster) {
            Err(error) => fallback_reason(&error).0,
            Ok(_) => panic!("restore must reject"),
        }
    }

    #[test]
    fn restore_rejects_bad_state() {
        let (mut run, _game_id) = LiveRun::start(GameMode::Standard, 11, 2);
        // Two turns so last_moves/contexts/food have entries; kill a snake
        // with a hand-set death group for the death-group cases.
        for _ in 0..2 {
            let results = run.scripted_results(&HashMap::new());
            run.drive(&results);
        }
        let roster = roster_from(&run.snake_ids);

        // Baseline: the unmutated state restores.
        let base = run.capture();
        let game = fixture_game(GameType::Standard, Uuid::new_v4());
        restore(base.clone(), &game, GameSource::Custom, &roster)
            .unwrap_or_else(|error| panic!("baseline must restore: {error:?}"));

        // A record snake id absent from the roster.
        let mut s = base.clone();
        s.snakes[0].game_battlesnake_id = Uuid::new_v4();
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "record id not in roster"
        );

        // A roster snake absent from the records.
        let mut s = base.clone();
        s.snakes.pop();
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "roster id not in records"
        );

        // Death columns half-present.
        let mut s = base.clone();
        s.snakes[0].death_turn = Some(3);
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "half-present death group"
        );

        // Colliding death_order values.
        let mut s = base.clone();
        for (i, record) in s.snakes.iter_mut().enumerate() {
            record.death_turn = Some(3);
            record.death_cause = Some("wall-collision".to_owned());
            record.death_eliminated_by = Some(String::new());
            record.death_order = Some(i as i32);
        }
        s.snakes[1].death_order = Some(0);
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "colliding death_order"
        );

        // Empty body.
        let mut s = base.clone();
        s.snakes[0].body = serde_json::json!([]);
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "empty body"
        );

        // A food point outside the board.
        let mut s = base.clone();
        s.food = serde_json::json!([{"x": 99, "y": 0}]);
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "off-board food"
        );

        // A NOT-eliminated snake's body point outside the board.
        let mut s = base.clone();
        s.snakes[0].body = serde_json::json!([{"x": -3, "y": 0}]);
        assert_eq!(
            restore_slug(s, &roster),
            "snapshot_validation_failed",
            "live snake off-board body"
        );

        // Non-snake_case eliminated_cause text.
        let mut s = base.clone();
        s.snakes[0].eliminated_cause = "OutOfHealth".to_owned();
        assert_eq!(
            restore_slug(s, &roster),
            "unparseable_resume_state",
            "non-snake_case eliminated_cause"
        );

        // Garbage food JSON.
        let mut s = base.clone();
        s.food = serde_json::json!({"not": "an array"});
        assert_eq!(
            restore_slug(s, &roster),
            "unparseable_resume_state",
            "garbage food json"
        );

        // Garbage body JSON.
        let mut s = base.clone();
        s.snakes[0].body = serde_json::json!("not points");
        assert_eq!(
            restore_slug(s, &roster),
            "unparseable_resume_state",
            "garbage body json"
        );

        // Non-direction last_move text.
        let mut s = base.clone();
        s.snakes[0].last_move = Some("diagonal".to_owned());
        assert_eq!(
            restore_slug(s, &roster),
            "unparseable_resume_state",
            "non-direction last_move"
        );
    }
}
