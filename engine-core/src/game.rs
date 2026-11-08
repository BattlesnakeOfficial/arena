//! Game engine using the `rules` crate for game simulation
//!
//! This module provides game simulation using the rules crate's standard mode.
//! Internal state is `rules::BoardState`; conversion to JSON wire format for
//! the Battlesnake API happens at the HTTP boundary (see `wire.rs` and
//! `move_response.rs`).

use color_eyre::eyre::eyre;
use rules::{BoardState, Direction, Point, RoyaleSettings, SnakeMove, StandardSettings};
use uuid::Uuid;

use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;

pub const MAX_TURNS: i32 = 5000;

/// Per-request budget every game gives a snake to answer `/move`, in ms.
pub const MOVE_TIMEOUT_MS: i64 = 500;

/// Metadata about the game that lives alongside the board state.
///
/// This replaces the `NestedGame` / `Game` wrapper from `battlesnake-game-types`.
#[derive(Debug, Clone, PartialEq)]
pub struct GameMeta {
    pub game_id: String,
    /// What kind of game this is. The single dispatch input for rules:
    /// every engine path matches on it exhaustively, so adding a mode is a
    /// compile error at every site instead of a silent standard fallback.
    /// The public `(ruleset name, map)` wire pair is derived from it at the
    /// HTTP boundary (`wire::wire_ruleset_and_map`).
    pub mode: GameMode,
    /// Which game-over predicate ends this game; recorded at creation so
    /// the rule is data, not a re-derivation (see [`GameOverRule`]).
    pub game_over_rule: GameOverRule,
    pub timeout: i64,
    pub settings: StandardSettings,
    /// Hazard parameters: required when `mode == Royale` (consumed by
    /// royale hazard population); other modes may carry it only for the
    /// wire `settings.royale` block (Go shows the `--shrinkEveryNTurns`
    /// flag value even on standard games). Never read to determine the
    /// mode — dispatch on `mode`, always.
    pub royale: Option<RoyaleSettings>,
    /// Where the game came from; sent to snakes as `game.source`.
    pub source: GameSource,
}

/// Origin of a game, sent to snakes as `game.source` (and as the engine
/// API's `Game.Source`).
///
/// Values come from the documented set (docs.battlesnake.com, Game object:
/// `tournament`, `league`, `arena`, `challenge`, `custom`), NOT what Play
/// actually sent: Play sent an undocumented `"ladder"` for ladder games, and
/// strict doc-following parsers (e.g. `battlesnake-game-types`' `Source`
/// enum) reject it with a 400 on every request (DEV-1505).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GameSource {
    /// A leaderboard (ladder) matchmaker game; documented as `arena`.
    Ladder,
    /// A tournament match game.
    Tournament,
    /// Anything a user created directly (game builder, API, rematch).
    #[default]
    Custom,
}

impl GameSource {
    pub fn as_str(self) -> &'static str {
        match self {
            GameSource::Ladder => "arena",
            GameSource::Tournament => "tournament",
            GameSource::Custom => "custom",
        }
    }

    /// Analytics label for Eyes product events. Matches
    /// `StatsSnapshot::fetch`'s precedence (tournament before ladder) but uses
    /// product vocabulary: `ladder`, not the stats page's `leaderboard` column
    /// label. Distinct from [`Self::as_str`], the documented engine wire value
    /// (`arena`), which must not change.
    pub fn analytics_label(self) -> &'static str {
        match self {
            GameSource::Ladder => "ladder",
            GameSource::Tournament => "tournament",
            GameSource::Custom => "custom",
        }
    }
}

/// Full engine game state: board + metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineGame {
    pub board: BoardState,
    pub meta: GameMeta,
    /// Snake names keyed by snake ID (needed for wire format).
    pub snake_names: std::collections::HashMap<String, String>,
}

/// Result of running a game
#[derive(Debug)]
pub struct GameResult {
    /// Snake IDs in order of placement (index 0 = winner/last alive)
    pub placements: Vec<String>,
    /// Final turn number
    pub final_turn: i32,
}

/// Engine-dispatch mode. The server converts its DB-facing `GameType`
/// (`Other(_)` runs standard rules, as today); the battlesnake CLI maps its
/// `-g` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameMode {
    Standard,
    Royale,
    Constrictor,
    SnailMode,
    Solo,
}

/// Which game-over predicate ends this game. Recorded at creation so the
/// rule is data, not a dispatch re-derived from a mode string (#147: a
/// 1-snake standard *server* game ends immediately; a 1-snake standard
/// *CLI* game runs to the snake's death, like Go's `WithSolo(len < 2)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameOverRule {
    /// Over once at most one snake remains alive (rules::standard::is_game_over).
    LastSnakeStanding,
    /// Over only once no snake remains alive (rules::solo::is_game_over).
    AllSnakesDead,
}

impl GameMode {
    /// The game-over rule a game of this mode gets at creation. Every
    /// server path that rebuilds `GameMeta` (resume restore, move-request
    /// rebuild) and the public engine-API stage list use the same mapping —
    /// one place to update per new mode.
    pub fn default_game_over_rule(self) -> GameOverRule {
        match self {
            GameMode::Solo => GameOverRule::AllSnakesDead,
            GameMode::Standard
            | GameMode::Royale
            | GameMode::Constrictor
            | GameMode::SnailMode => GameOverRule::LastSnakeStanding,
        }
    }
}

/// Why a game's turn loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    GameOver,
    TurnCap,
}

impl StopReason {
    /// Stable label for tracing/log fields.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::GameOver => "game_over",
            StopReason::TurnCap => "turn_cap",
        }
    }
}

/// The single turn-loop condition: `None` = keep playing, checked *before*
/// each iteration's body (a resumed-but-already-over game issues zero
/// requests). `turn_cap` of `None` means uncapped (the CLI's default;
/// engine `MAX_TURNS` stays server/prod-only). A game that is both over
/// and at the cap reports `GameOver`.
pub fn stop_reason(game: &EngineGame, turn_cap: Option<i32>) -> Option<StopReason> {
    if is_game_over(game) {
        Some(StopReason::GameOver)
    } else if turn_cap.is_some_and(|cap| game.board.turn >= cap) {
        Some(StopReason::TurnCap)
    } else {
        None
    }
}

/// A snake entering a game: engine ID (arena's game_battlesnake_id string)
/// and display name. Named fields so id/name can't be swapped silently.
pub struct InitialSnake {
    pub id: String,
    pub name: String,
}

/// Derive a deterministic RNG seed from the game's UUID.
///
/// Royale hazard placement must be reproducible from stored game data (see
/// the determinism notes in `rules::royale`), so the seed is a pure function
/// of the game ID rather than a fresh random value.
fn game_seed(game_id: Uuid) -> u64 {
    let bytes = game_id.as_bytes();
    u64::from_le_bytes(bytes[..8].try_into().expect("uuid has 16 bytes"))
}

/// Per-mode rules: the standard settings and Royale's shrink settings.
/// Single source for both game creation and everything that describes a
/// game's rules (snake payloads via `GameMeta`, the engine-compatible game
/// API). The engine dispatches on `GameMeta.mode`, never on a string.
pub fn mode_rules(mode: GameMode, game_id: Uuid) -> (StandardSettings, Option<RoyaleSettings>) {
    // Hazard damage: the arena has historically used 15 for standard games
    // (where it is inert -- standard boards never spawn hazards), and we keep
    // that unchanged. Royale and Snail Mode use 14, the default in the
    // canonical Go rules (`hazardDamagePerTurn` in BattlesnakeOfficial/rules
    // cli, which is what map-driven hazards like snail_mode ran with on
    // play.battlesnake.com). Constrictor advertises no food spawning
    // (chance 0 / minimum 0) on the wire; its hazard damage is inert like
    // standard's.
    match mode {
        GameMode::Royale => (
            StandardSettings {
                food_spawn_chance: 15,
                minimum_food: 1,
                hazard_damage_per_turn: 14,
            },
            Some(RoyaleSettings {
                shrink_every_n_turns: 25,
                seed: game_seed(game_id),
            }),
        ),
        // Internal mode only: on the wire snakes see ruleset "standard" with
        // game.map = "snail_mode", matching play.battlesnake.com where Snail
        // Mode was a community map on the standard ruleset (see wire.rs).
        GameMode::SnailMode => (
            StandardSettings {
                food_spawn_chance: 15,
                minimum_food: 1,
                hazard_damage_per_turn: 14,
            },
            None,
        ),
        GameMode::Constrictor => (
            StandardSettings {
                food_spawn_chance: 0,
                minimum_food: 0,
                hazard_damage_per_turn: 15,
            },
            None,
        ),
        // Solo: single-snake survival. Runs the standard pipeline with
        // standard food settings; only the game-over predicate differs
        // (see `GameOverRule`). Kept as an explicit arm -- folding it into
        // a shared arm below would silently change its food settings.
        GameMode::Solo => (
            StandardSettings {
                food_spawn_chance: 15,
                minimum_food: 1,
                hazard_damage_per_turn: 15,
            },
            None,
        ),
        // Standard rules. An explicit arm, not a catch-all: adding a mode
        // later must be a compile error here, never a silent standard
        // fallback.
        GameMode::Standard => (
            StandardSettings {
                food_spawn_chance: 15,
                minimum_food: 1,
                hazard_damage_per_turn: 15,
            },
            None,
        ),
    }
}

/// Create the initial game state. `rng` drives spawn placement and initial
/// food; `game_id` drives Royale's hazard seed (see `mode_rules`) and the
/// wire `game.id`.
pub fn create_initial_game(
    rng: &mut impl Rng,
    game_id: Uuid,
    width: i32,
    height: i32,
    mode: GameMode,
    snakes: &[InitialSnake],
) -> color_eyre::Result<EngineGame> {
    let snake_ids: Vec<String> = snakes.iter().map(|s| s.id.clone()).collect();

    let mut board = rules::board::create_default_board_state(&mut *rng, width, height, &snake_ids)
        .map_err(|error| {
            eyre!(
                "failed to create initial board state ({width}x{height}, {} snakes): {error:?}",
                snakes.len()
            )
        })?;

    let mut snake_names = std::collections::HashMap::new();
    for s in snakes {
        snake_names.insert(s.id.clone(), s.name.clone());
    }

    let (settings, royale) = mode_rules(mode, game_id);

    match mode {
        GameMode::Constrictor => {
            // Match the Go engine's initialization pass (the constrictor
            // pipeline runs once with no moves at game start): strip all food
            // and pin snakes at max health. Initial bodies are stacked, so no
            // growth happens here.
            rules::constrictor::modify_initial_board(&mut board).map_err(|error| {
                eyre!(
                    "constrictor initial board modification failed ({width}x{height}, {} snakes): {error:?}",
                    snakes.len()
                )
            })?;
        }
        GameMode::Standard | GameMode::Royale | GameMode::SnailMode | GameMode::Solo => {}
    }

    Ok(EngineGame {
        board,
        meta: GameMeta {
            game_id: game_id.to_string(),
            mode,
            game_over_rule: mode.default_game_over_rule(),
            timeout: MOVE_TIMEOUT_MS,
            settings,
            royale,
            source: GameSource::default(),
        },
        snake_names,
    })
}

/// Deterministic game inputs for a seed: the RNG that drives placement and
/// food, and the game ID (which drives Royale's hazard seed via `game_seed`
/// and appears in the wire JSON as `game.id`). Drawing the ID from the same
/// stream makes the whole game a pure function of the seed. The server does
/// NOT use this — it passes its DB game ID and a fresh per-game OS RNG.
pub fn seeded_game(seed: u64) -> (ChaCha8Rng, Uuid) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut bytes = [0u8; 16];
    rng.fill_bytes(&mut bytes);
    let game_id = uuid::Builder::from_random_bytes(bytes).into_uuid();
    (rng, game_id)
}

fn royale_settings(game: &EngineGame) -> color_eyre::Result<RoyaleSettings> {
    game.meta
        .royale
        .clone()
        .ok_or_else(|| eyre!("missing Royale settings for royale game"))
}

/// Run one complete turn: apply the moves, advance the turn counter, then
/// spawn food for the next turn. This sequence is the engine's contract —
/// every game loop must run all three, in this order (arena once shipped
/// with food spawning missing from the live path) — so it lives here, not
/// in each caller. Returns the IDs of snakes that ate this turn
/// (see [`apply_turn`]).
pub fn advance_turn(
    game: &mut EngineGame,
    moves: &[(String, Direction)],
    rng: &mut impl Rng,
) -> color_eyre::Result<Vec<String>> {
    let fed = apply_turn(game, moves)?;
    game.board.turn += 1;
    spawn_food(game, rng);
    Ok(fed)
}

/// Run a complete game with random moves, returning placements.
///
/// The runner drives the production `advance_turn` pipeline, so tests of it
/// exercise the exact path the server game runner and CLI `play` use.
pub fn run_game_with_random_moves(
    mut game: EngineGame,
    rng: &mut impl Rng,
) -> color_eyre::Result<GameResult> {
    let mut elimination_order: Vec<String> = Vec::new();

    while stop_reason(&game, Some(MAX_TURNS)).is_none() {
        // Build random moves for each alive snake
        let moves: Vec<(String, Direction)> = game
            .board
            .snakes
            .iter()
            .filter(|s| !s.eliminated_cause.is_eliminated())
            .map(|s| {
                let head = s.head();
                // Pick a random reasonable direction (not back into neck)
                let all_dirs = [
                    Direction::Up,
                    Direction::Down,
                    Direction::Left,
                    Direction::Right,
                ];
                let neck = s.body.get(1);
                let reasonable: Vec<Direction> = all_dirs
                    .iter()
                    .copied()
                    .filter(|d| {
                        let (dx, dy) = d.to_delta();
                        let new_head = Point::new(head.x + dx, head.y + dy);
                        // Don't move into neck
                        if let Some(n) = neck {
                            new_head != *n
                        } else {
                            true
                        }
                    })
                    .collect();

                use rand::seq::SliceRandom;
                let dir = if reasonable.is_empty() {
                    *all_dirs.choose(&mut *rng).unwrap()
                } else {
                    *reasonable.choose(&mut *rng).unwrap()
                };

                (s.id.clone(), dir)
            })
            .collect();

        advance_turn(&mut game, &moves, rng).map_err(|error| {
            eyre!(
                "advance_turn failed for mode {:?} on turn {}: {error:#}",
                game.meta.mode,
                game.board.turn + 1
            )
        })?;

        // Track newly eliminated snakes
        for snake in &game.board.snakes {
            if snake.eliminated_cause.is_eliminated() && !elimination_order.contains(&snake.id) {
                elimination_order.push(snake.id.clone());
            }
        }
    }

    // Build placements: last eliminated = winner (placement 1)
    // Snakes still alive at the end go first
    let mut placements: Vec<String> = game
        .board
        .snakes
        .iter()
        .filter(|s| !s.eliminated_cause.is_eliminated())
        .map(|s| s.id.clone())
        .collect();

    // Then add eliminated snakes in reverse order (last eliminated = better placement)
    elimination_order.reverse();
    placements.extend(elimination_order);

    Ok(GameResult {
        placements,
        final_turn: game.board.turn,
    })
}

/// Check if the game is over: dispatch on the recorded game-over rule, not
/// the mode — the rule is chosen at creation (`create_initial_game` for
/// server games, the CLI's snake-count override for `play`), and #147 pins
/// a deliberate divergence: a 1-snake *standard* server game ends at turn
/// 0 (`LastSnakeStanding`), while a 1-snake standard *CLI* game runs to
/// the snake's death (`AllSnakesDead`, Go's `WithSolo`).
pub fn is_game_over(game: &EngineGame) -> bool {
    match game.meta.game_over_rule {
        GameOverRule::AllSnakesDead => rules::solo::is_game_over(&game.board),
        GameOverRule::LastSnakeStanding => rules::standard::is_game_over(&game.board),
    }
}

/// Apply a single turn: move snakes, reduce health, feed, eliminate
///
/// Returns the IDs of the snakes that ate food this turn. This is the only
/// reliable food signal: body growth isn't, because Constrictor grows every
/// snake every turn without any food on the board.
///
/// Stage errors (`move_snakes`, `eliminate_snakes`) propagate as `Err` — no
/// stage result is discarded, so a malformed move set can never leave a
/// half-moved board silently.
///
/// Note: Unlike the rules crate's `execute_turn`, this does NOT increment
/// `board.turn` internally -- the caller must do that (for compatibility with
/// game_runner.rs which increments after recording frames).
pub fn apply_turn(
    game: &mut EngineGame,
    moves: &[(String, Direction)],
) -> color_eyre::Result<Vec<String>> {
    let royale = match game.meta.mode {
        GameMode::Royale => Some(royale_settings(game)?),
        GameMode::Standard | GameMode::Constrictor | GameMode::SnailMode | GameMode::Solo => None,
    };
    let snake_moves: Vec<SnakeMove> = moves
        .iter()
        .map(|(id, dir)| SnakeMove {
            id: id.clone(),
            direction: *dir,
        })
        .collect();

    // Run the standard pipeline steps individually (so we can control turn increment)
    rules::standard::move_snakes(&mut game.board, &snake_moves)
        .map_err(|error| eyre!("move_snakes failed: {error:?}"))?;
    rules::standard::reduce_snake_health(&mut game.board);
    rules::standard::damage_hazards(&mut game.board, &game.meta.settings);
    let fed = rules::standard::feed_snakes(&mut game.board);
    // Unreachable today: an empty-bodied alive snake panics earlier in the
    // pipeline (damage_hazards/feed_snakes call Snake::head, which is body[0])
    // before eliminate_snakes runs. The propagation is structural — no stage
    // result is discarded (DEV-2182) — so there is intentionally no test for
    // this error path while stage order is unchanged.
    rules::standard::eliminate_snakes(&mut game.board)
        .map_err(|error| eyre!("eliminate_snakes failed: {error:?}"))?;

    // Mode-specific post-elimination stages. Dispatch on the mode so each
    // mode's arm stays independent (parallel game-mode PRs union their arms
    // here) and a new mode fails to compile here instead of silently
    // running standard rules.
    match game.meta.mode {
        // Royale: spawn/grow hazards for the next turn (matches Go's
        // StageSpawnHazardsShrinkMap, which runs after elimination). Uses
        // `board.turn + 1` internally, so this must run before the caller
        // increments `board.turn`.
        GameMode::Royale => {
            let Some(settings) = royale.as_ref() else {
                unreachable!("royale settings were validated before turn application")
            };
            rules::royale::populate_hazards(&mut game.board, settings)
                .map_err(|error| eyre!("royale hazard population failed: {error:?}"))?;
        }
        // Constrictor: clear all food and grow every snake (matches Go's
        // StageSpawnFoodNoFood + StageModifySnakesAlwaysGrow, which run after
        // elimination).
        GameMode::Constrictor => {
            rules::constrictor::remove_food(&mut game.board);
            rules::constrictor::grow_snakes(&mut game.board)
                .expect("constrictor growth failed: zero-length snake");
        }
        // Snail Mode: decay hazard trails and record vacated tail squares
        // (matches Go's SnailModeMap.PostUpdateBoard, which runs after all
        // ruleset stages).
        GameMode::SnailMode => rules::snail::post_update_board(&mut game.board),
        // Standard and Solo have no post-elimination stage: the standard
        // pipeline above already covered them.
        GameMode::Standard | GameMode::Solo => {}
    }

    Ok(fed)
}

/// Spawn food for the next turn in modes that spawn food.
///
/// Food spawning lives outside the per-turn rules pipeline (matching the Go
/// engine, where the game loop spawns food between turns), so every caller of
/// `apply_turn` must also call this. Constrictor games never spawn food; all
/// other modes use the standard chance/minimum settings.
pub fn spawn_food(game: &mut EngineGame, rng: &mut impl Rng) {
    match game.meta.mode {
        GameMode::Constrictor => {}
        GameMode::Standard | GameMode::Royale | GameMode::SnailMode | GameMode::Solo => {
            rules::food::maybe_spawn_food(&mut *rng, &mut game.board, &game.meta.settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [GameMode; 5] = [
        GameMode::Standard,
        GameMode::Royale,
        GameMode::Constrictor,
        GameMode::SnailMode,
        GameMode::Solo,
    ];

    /// Scripted moves cycle up/right/down/left — a stable 2x2 square walk.
    const CYCLE: [Direction; 4] = [
        Direction::Up,
        Direction::Right,
        Direction::Down,
        Direction::Left,
    ];

    fn initial_snakes(mode: GameMode) -> Vec<InitialSnake> {
        if mode == GameMode::Solo {
            vec![InitialSnake {
                id: "snake-0".into(),
                name: "Snake 0".into(),
            }]
        } else {
            vec![
                InitialSnake {
                    id: "snake-0".into(),
                    name: "Snake 0".into(),
                },
                InitialSnake {
                    id: "snake-1".into(),
                    name: "Snake 1".into(),
                },
            ]
        }
    }

    /// Play a full game with deterministic scripted moves from `seed`, through
    /// the same shared advance_turn the game runner uses. Returns the board
    /// snapshot after every turn (turn 0 included).
    fn play_scripted_game(mode: GameMode, seed: u64) -> Vec<BoardState> {
        let (mut rng, game_id) = seeded_game(seed);
        let mut game = create_initial_game(&mut rng, game_id, 11, 11, mode, &initial_snakes(mode))
            .expect("valid initial game");
        let mut boards = vec![game.board.clone()];
        while stop_reason(&game, Some(MAX_TURNS)).is_none() {
            let moves: Vec<(String, Direction)> = game
                .board
                .snakes
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.eliminated_cause.is_eliminated())
                .map(|(i, s)| (s.id.clone(), CYCLE[(game.board.turn as usize + i) % 4]))
                .collect();
            advance_turn(&mut game, &moves, &mut rng).expect("valid scripted turn");
            boards.push(game.board.clone());
        }
        boards
    }

    #[test]
    fn same_seed_and_moves_replay_identically() {
        for mode in MODES {
            for seed in 0..20u64 {
                assert_eq!(
                    play_scripted_game(mode, seed),
                    play_scripted_game(mode, seed),
                    "mode {mode:?} seed {seed}: replay diverged"
                );
            }
        }
    }

    #[test]
    fn different_seeds_change_the_outcome() {
        for mode in MODES {
            let outcomes: Vec<(i32, BoardState)> = (0..20u64)
                .map(|seed| {
                    let boards = play_scripted_game(mode, seed);
                    let final_turn = boards.len() as i32 - 1;
                    let final_board = boards.last().unwrap().clone();
                    (final_turn, final_board)
                })
                .collect();
            assert!(
                outcomes.iter().any(|o| o != &outcomes[0]),
                "mode {mode:?}: 20 seeds produced identical outcomes — \
                 the harness cannot discriminate"
            );
        }
    }

    fn run_seeded_random_game(mode: GameMode, seed: u64) -> GameResult {
        let (mut rng, game_id) = seeded_game(seed);
        let game = create_initial_game(&mut rng, game_id, 11, 11, mode, &initial_snakes(mode))
            .expect("valid initial game");
        run_game_with_random_moves(game, &mut rng).expect("valid game")
    }

    #[test]
    fn random_move_games_replay_identically_per_seed() {
        for mode in MODES {
            for seed in 0..5u64 {
                let first = run_seeded_random_game(mode, seed);
                let second = run_seeded_random_game(mode, seed);
                assert_eq!(
                    first.placements, second.placements,
                    "mode {mode:?} seed {seed}: placements diverged on replay"
                );
                assert_eq!(
                    first.final_turn, second.final_turn,
                    "mode {mode:?} seed {seed}: final turn diverged on replay"
                );
            }
        }
    }

    // --- create_initial_game reachability (DEV-2144 carry-over #1) ---

    /// 3×3 can never be populated: every snake spawn point collapses to the
    /// center (1,1), every diagonal of (1,1) is a board corner, so food
    /// placement has no valid square and the center itself is occupied.
    #[test]
    fn create_initial_game_3x3_is_err() {
        let (mut rng, game_id) = seeded_game(0);
        let snakes = vec![InitialSnake {
            id: "s0".into(),
            name: "S0".into(),
        }];
        assert!(create_initial_game(&mut rng, game_id, 3, 3, GameMode::Standard, &snakes).is_err());
    }

    /// Every input `play` accepts (7/11/19 square boards, 1–8 snakes) can
    /// always be populated — `NoRoomForFood` is unreachable for validated
    /// inputs. Bounded seed sweep (iteration-bounded, not wall-clock).
    #[test]
    fn create_initial_game_ok_for_every_valid_play_input() {
        for &(width, height) in &[(7, 7), (11, 11), (19, 19)] {
            for count in 1..=8 {
                let snakes: Vec<InitialSnake> = (0..count)
                    .map(|i| InitialSnake {
                        id: format!("s{i}"),
                        name: format!("S{i}"),
                    })
                    .collect();
                for mode in MODES {
                    for seed in 0..200u64 {
                        let (mut rng, game_id) = seeded_game(seed);
                        let result =
                            create_initial_game(&mut rng, game_id, width, height, mode, &snakes);
                        assert!(
                            result.is_ok(),
                            "{width}x{height} {count} snakes {mode:?} seed {seed}: \n{result:?}"
                        );
                    }
                }
            }
        }
    }

    // --- seeded_game determinism (DEV-2144 carry-over #2) ---

    /// The game ID is a pure function of the seed: same seed, same UUID,
    /// different seeds, different UUIDs — and the UUID is v4-shaped
    /// (`Builder::from_random_bytes`).
    #[test]
    fn seeded_game_id_is_seed_derived() {
        const SEED_A: u64 = 12345;
        const SEED_B: u64 = 67890;

        let (_, id_a1) = seeded_game(SEED_A);
        let (_, id_a2) = seeded_game(SEED_A);
        assert_eq!(id_a1, id_a2, "same seed must give the same game ID");

        let (_, id_b) = seeded_game(SEED_B);
        assert_ne!(id_a1, id_b, "different seeds must give different IDs");

        assert_eq!(id_a1.get_version_num(), 4);
        assert_eq!(id_b.get_version_num(), 4);
    }

    /// Royale hazards derive from the game ID's seed: two seeds whose games
    /// get different IDs must produce different hazard regions, and the same
    /// ID must always produce the same one. Pinned at turn 24 —
    /// `populate_hazards` uses `turn + 1 = 25`, exactly one shrink at
    /// shrink-every-25; at turn 0 both hazard sets are empty (no
    /// discrimination) and with one shrink two seeds agree 1 time in 4, so
    /// the turn AND the seed pair are pinned.
    #[test]
    fn royale_hazards_diverge_across_seed_derived_game_ids() {
        const SEED_A: u64 = 12345;
        const SEED_B: u64 = 67890;
        const PINNED_TURN: i32 = 24;

        let (_, id_a) = seeded_game(SEED_A);
        let (_, id_b) = seeded_game(SEED_B);
        let rules_a = mode_rules(GameMode::Royale, id_a).1;
        let rules_b = mode_rules(GameMode::Royale, id_b).1;

        let hazards = |rules: &Option<RoyaleSettings>| {
            let mut board = BoardState {
                turn: PINNED_TURN,
                width: 11,
                height: 11,
                food: vec![],
                snakes: vec![],
                hazards: vec![],
            };
            rules::royale::populate_hazards(&mut board, rules.as_ref().unwrap()).unwrap();
            board.hazards
        };

        // Same ID: deterministic.
        assert_eq!(hazards(&rules_a), hazards(&rules_a));
        // Different seed-derived IDs: different hazard regions. SEED_A and
        // SEED_B were verified locally to differ at this turn (one shrink
        // draws one edge from the seed; these two draw different edges).
        assert_ne!(
            hazards(&rules_a),
            hazards(&rules_b),
            "seeded_game must feed game_seed; hazards identical means the ID is not seed-derived"
        );
    }

    // --- game-over rule (#147: rule as data, not mode dispatch) ---

    fn game_with_snakes(mode: GameMode, alive: usize, dead: usize) -> EngineGame {
        let mut snakes = Vec::new();
        for i in 0..alive {
            snakes.push(rules::Snake {
                id: format!("a{i}"),
                body: vec![Point::new(i as i32, 0)],
                health: 100,
                eliminated_cause: rules::EliminationCause::NotEliminated,
                eliminated_by: String::new(),
                eliminated_on_turn: 0,
            });
        }
        for i in 0..dead {
            snakes.push(rules::Snake {
                id: format!("d{i}"),
                body: vec![Point::new(0, 10 - i as i32)],
                health: 0,
                eliminated_cause: rules::EliminationCause::OutOfHealth,
                eliminated_by: String::new(),
                eliminated_on_turn: 3,
            });
        }
        EngineGame {
            board: BoardState {
                turn: 5,
                width: 11,
                height: 11,
                food: vec![],
                snakes,
                hazards: vec![],
            },
            meta: GameMeta {
                game_id: "g".into(),
                mode,
                game_over_rule: mode.default_game_over_rule(),
                timeout: 500,
                settings: rules::StandardSettings {
                    food_spawn_chance: 15,
                    minimum_food: 1,
                    hazard_damage_per_turn: 14,
                },
                royale: None,
                source: GameSource::Custom,
            },
            snake_names: std::collections::HashMap::new(),
        }
    }

    /// #147: the same 1-alive-snake standard board is over under
    /// `LastSnakeStanding` (a server standard game) and running under
    /// `AllSnakesDead` (a CLI single-snake game). The rule — not the mode
    /// — decides.
    #[test]
    fn game_over_rule_decides_for_the_same_board() {
        let mut server = game_with_snakes(GameMode::Standard, 1, 0);
        server.meta.game_over_rule = GameOverRule::LastSnakeStanding;
        assert!(is_game_over(&server), "1 alive snake ends a LastSnakeStanding game");

        let mut cli = game_with_snakes(GameMode::Standard, 1, 0);
        cli.meta.game_over_rule = GameOverRule::AllSnakesDead;
        assert!(!is_game_over(&cli), "1 alive snake keeps an AllSnakesDead game running");

        // Once that snake dies, over under either rule.
        let mut dead = game_with_snakes(GameMode::Standard, 0, 1);
        dead.meta.game_over_rule = GameOverRule::AllSnakesDead;
        assert!(is_game_over(&dead));
    }

    /// The mode→rule mapping a server game gets at creation: only Solo runs
    /// the all-dead check.
    #[test]
    fn default_game_over_rule_maps_only_solo_to_all_dead() {
        assert_eq!(
            GameMode::Solo.default_game_over_rule(),
            GameOverRule::AllSnakesDead
        );
        for mode in [
            GameMode::Standard,
            GameMode::Royale,
            GameMode::Constrictor,
            GameMode::SnailMode,
        ] {
            assert_eq!(
                mode.default_game_over_rule(),
                GameOverRule::LastSnakeStanding,
                "{mode:?}"
            );
        }
    }

    /// `stop_reason` checks over before the cap, so a game that is both
    /// over and at its cap reports `GameOver`.
    #[test]
    fn stop_reason_reports_game_over_before_turn_cap() {
        let game = game_with_snakes(GameMode::Standard, 0, 2); // over at turn 5
        assert_eq!(stop_reason(&game, Some(0)), Some(StopReason::GameOver));
        // Not over, under the cap: keep playing.
        let game = game_with_snakes(GameMode::Standard, 2, 0);
        assert_eq!(stop_reason(&game, Some(6)), None);
        // Not over, at the cap.
        assert_eq!(stop_reason(&game, Some(5)), Some(StopReason::TurnCap));
        // Not over, uncapped.
        assert_eq!(stop_reason(&game, None), None);
        // Over, uncapped.
        let game = game_with_snakes(GameMode::Standard, 0, 2);
        assert_eq!(stop_reason(&game, None), Some(StopReason::GameOver));
    }

    // --- eliminated_on_turn contract (rules sets it pre-increment) ---

    /// `rules` sets `eliminated_on_turn = board.turn + 1` *before* the
    /// engine increments `board.turn` (documented at
    /// `rules/src/standard.rs`), so after `advance_turn` a newly dead snake
    /// carries `eliminated_on_turn == board.turn`. Frame `Death.Turn`
    /// derives from that field, so a future reordering of the increment
    /// would silently shift every frame's death turn — pin it here for two
    /// elimination paths (wall death and snake collision).
    #[test]
    fn advance_turn_pins_eliminated_on_turn_to_post_increment_turn() {
        // Wall death: snake 0 at x=0 walking Left.
        let (mut rng, game_id) = seeded_game(0);
        let mut game = create_initial_game(
            &mut rng,
            game_id,
            11,
            11,
            GameMode::Standard,
            &[InitialSnake {
                id: "wall".into(),
                name: "Wall".into(),
            }, InitialSnake {
                id: "shield".into(),
                name: "Shield".into(),
            }],
        )
        .expect("valid initial game");
        // Place the walker on the left edge with its body behind it.
        let walker = game.board.snakes.iter_mut().find(|s| s.id == "wall").unwrap();
        walker.body = vec![Point::new(0, 9), Point::new(1, 9), Point::new(2, 9)];
        let moves = vec![
            ("wall".to_string(), Direction::Left),
            ("shield".to_string(), Direction::Up),
        ];
        advance_turn(&mut game, &moves, &mut rng).expect("valid turn");
        let walker = game.board.snakes.iter().find(|s| s.id == "wall").unwrap();
        assert_eq!(walker.eliminated_cause, rules::EliminationCause::OutOfBounds);
        assert_eq!(
            walker.eliminated_on_turn, game.board.turn,
            "wall death: eliminated_on_turn must equal the post-increment turn"
        );

        // Snake collision: a steps Right onto b's body (b's post-move
        // tail), a separate elimination path from the wall death above.
        let (mut rng, game_id) = seeded_game(1);
        let mut game = create_initial_game(
            &mut rng,
            game_id,
            11,
            11,
            GameMode::Standard,
            &[InitialSnake {
                id: "a".into(),
                name: "A".into(),
            }, InitialSnake {
                id: "b".into(),
                name: "B".into(),
            }],
        )
        .expect("valid initial game");
        // b: head (6,4), body trailing left; moving Up leaves (5,4) as its
        // tail. a: head (4,4) stepping Right onto exactly that tail.
        let a = game.board.snakes.iter_mut().find(|s| s.id == "a").unwrap();
        a.body = vec![Point::new(4, 4), Point::new(4, 5), Point::new(4, 6)];
        let b = game.board.snakes.iter_mut().find(|s| s.id == "b").unwrap();
        b.body = vec![Point::new(6, 4), Point::new(5, 4), Point::new(5, 5)];
        let moves = vec![
            ("a".to_string(), Direction::Right), // (4,4) -> (5,4): b's tail
            ("b".to_string(), Direction::Up),    // (6,4) -> (6,5), survives
        ];
        advance_turn(&mut game, &moves, &mut rng).expect("valid turn");
        let a = game.board.snakes.iter().find(|s| s.id == "a").unwrap();
        assert_eq!(
            a.eliminated_cause,
            rules::EliminationCause::Collision,
            "expected a snake-collision elimination"
        );
        assert_eq!(a.eliminated_by, "b");
        assert_eq!(
            a.eliminated_on_turn, game.board.turn,
            "collision death turn must equal the post-increment turn"
        );
        let b = game.board.snakes.iter().find(|s| s.id == "b").unwrap();
        assert!(!b.eliminated_cause.is_eliminated(), "b must survive");
    }

    #[test]
    fn apply_turn_propagates_move_snakes_no_move_found_error() {
        let mut game = game_with_snakes(GameMode::Standard, 2, 0);
        // Move for a0 only: non-empty (so move_snakes skips its empty-slice
        // no-op) but missing a1's entry.
        let moves = vec![("a0".to_string(), Direction::Up)];
        let error = apply_turn(&mut game, &moves).expect_err("missing move must fail");
        let msg = error.to_string();
        assert!(msg.contains("move_snakes failed"), "got: {msg}");
        assert!(msg.contains("NoMoveFound"), "got: {msg}");
    }

    #[test]
    fn apply_turn_propagates_move_snakes_zero_length_error() {
        let mut game = game_with_snakes(GameMode::Standard, 2, 0);
        game.board.snakes[1].body = vec![];
        let moves = vec![("a0".to_string(), Direction::Up)];
        let error = apply_turn(&mut game, &moves).expect_err("zero-length snake must fail");
        let msg = error.to_string();
        assert!(msg.contains("move_snakes failed"), "got: {msg}");
        assert!(msg.contains("ZeroLengthSnake"), "got: {msg}");
    }

    #[test]
    fn advance_turn_refills_minimum_food_from_empty_board() {
        for mode in MODES {
            if mode == GameMode::Constrictor {
                continue; // minimum_food = 0: no refill contract
            }
            for seed in 0..5u64 {
                let (mut rng, game_id) = seeded_game(seed);
                let mut game =
                    create_initial_game(&mut rng, game_id, 11, 11, mode, &initial_snakes(mode))
                        .expect("valid initial game");
                game.board.food.clear();
                let moves: Vec<(String, Direction)> = game
                    .board
                    .snakes
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.id.clone(), CYCLE[i % 4]))
                    .collect();
                advance_turn(&mut game, &moves, &mut rng).expect("valid turn");
                assert!(
                    !game.board.food.is_empty(),
                    "mode {mode:?} seed {seed}: advance_turn spawned no food"
                );
            }
        }
    }

    #[test]
    fn scripted_games_maintain_minimum_food() {
        for mode in MODES {
            if mode == GameMode::Constrictor {
                continue; // minimum_food = 0
            }
            for seed in 0..5u64 {
                let boards = play_scripted_game(mode, seed);
                // Anti-vacuity: the invariant must cover a real game, not a
                // game that ended after a turn or two. Observed board counts
                // (turns + 1) at 21450c8, seeds 0..5: Standard 101; Royale
                // [101, 35, 101, 35, 35, 35]; SnailMode 7 every seed (the CYCLE
                // walk self-collides on its own snail trail at turn 6); Solo
                // [101, 101, 101, 101, 103, 101]. Standard/Solo/Snail games end
                // by starvation/trail death around turn 100/6 — not MAX_TURNS
                // (5000). The floor is 5, one turn of headroom under the
                // observed minimum of 7.
                assert!(
                    boards.len() >= 5,
                    "mode {mode:?} seed {seed}: only {} boards — game ended too early \
                     for the food invariant to mean anything",
                    boards.len()
                );
                for board in &boards[1..] {
                    assert!(
                        !board.food.is_empty(),
                        "mode {mode:?} seed {seed}: mid-game board has no food"
                    );
                }
            }
        }
    }
}
