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
#[derive(Debug, Clone)]
pub struct GameMeta {
    pub game_id: String,
    pub ruleset_name: String,
    pub timeout: i64,
    pub settings: StandardSettings,
    /// `Some` when this is a Royale game; drives hazard population each turn
    /// and the `royale` block of the wire-protocol ruleset settings.
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
#[derive(Debug, Clone)]
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

/// Per-mode rules: the internal ruleset name (drives engine dispatch), the
/// standard settings, and Royale's shrink settings. Single source for both
/// game creation and everything that describes a game's rules (snake
/// payloads via `GameMeta`, the engine-compatible game API).
pub fn mode_rules(
    mode: GameMode,
    game_id: Uuid,
) -> (&'static str, StandardSettings, Option<RoyaleSettings>) {
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
            "royale",
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
        // Internal name only: on the wire snakes see ruleset "standard" with
        // game.map = "snail_mode", matching play.battlesnake.com where Snail
        // Mode was a community map on the standard ruleset (see wire.rs).
        GameMode::SnailMode => (
            "snail_mode",
            StandardSettings {
                food_spawn_chance: 15,
                minimum_food: 1,
                hazard_damage_per_turn: 14,
            },
            None,
        ),
        GameMode::Constrictor => (
            "constrictor",
            StandardSettings {
                food_spawn_chance: 0,
                minimum_food: 0,
                hazard_damage_per_turn: 15,
            },
            None,
        ),
        // Solo: single-snake survival. Runs the standard pipeline with
        // standard food settings; only the game-over predicate differs
        // (see `is_game_over` and `rules::solo`). Kept as an explicit arm --
        // folding it into the catch-all below would silently give it
        // "standard" rules and the standard game-over check.
        GameMode::Solo => (
            "solo",
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
            "standard",
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
    use color_eyre::eyre::Context as _;

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

    let (ruleset_name, settings, royale) = mode_rules(mode, game_id);

    if ruleset_name == "constrictor" {
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

    Ok(EngineGame {
        board,
        meta: GameMeta {
            game_id: game_id.to_string(),
            ruleset_name: ruleset_name.to_string(),
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

/// Run a complete game with random moves, returning placements
pub fn run_game_with_random_moves(
    mut game: EngineGame,
    rng: &mut impl Rng,
) -> color_eyre::Result<GameResult> {
    let royale = (game.meta.ruleset_name == "royale")
        .then(|| royale_settings(&game))
        .transpose()?;
    let mut elimination_order: Vec<String> = Vec::new();

    while !is_game_over(&game) && game.board.turn < MAX_TURNS {
        // Build random moves for each alive snake
        let moves: Vec<SnakeMove> = game
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

                SnakeMove {
                    id: s.id.clone(),
                    direction: dir,
                }
            })
            .collect();

        // Apply the turn. Dispatch on the ruleset name so each mode's arm
        // stays independent (parallel game-mode PRs union their arms here).
        let _game_over = match game.meta.ruleset_name.as_str() {
            "royale" => {
                let Some(settings) = royale.as_ref() else {
                    unreachable!("royale settings were validated before game execution")
                };
                rules::royale::execute_turn(&mut game.board, &moves, &game.meta.settings, settings)
            }
            "constrictor" => {
                rules::constrictor::execute_turn(&mut game.board, &moves, &game.meta.settings)
            }
            "snail_mode" => {
                rules::snail::execute_turn(&mut game.board, &moves, &game.meta.settings)
            }
            "solo" => rules::solo::execute_turn(&mut game.board, &moves, &game.meta.settings),
            _ => rules::standard::execute_turn(&mut game.board, &moves, &game.meta.settings),
        }
        .map_err(|error| {
            eyre!(
                "execute_turn failed for ruleset {}: {error:?}",
                game.meta.ruleset_name
            )
        })?;

        // Spawn food after turn. Constrictor games never spawn food (their
        // settings are food_spawn_chance=0 / minimum_food=0, which would make
        // this a no-op anyway, but skip it outright to keep the invariant
        // explicit).
        if game.meta.ruleset_name != "constrictor" {
            rules::food::maybe_spawn_food(&mut *rng, &mut game.board, &game.meta.settings);
        }

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

/// Check if the game is over.
///
/// Mode-aware: Solo games (ruleset "solo") end only when no snake is alive;
/// every other mode ends when 1 or fewer snakes remain alive.
pub fn is_game_over(game: &EngineGame) -> bool {
    match game.meta.ruleset_name.as_str() {
        "solo" => rules::solo::is_game_over(&game.board),
        _ => rules::standard::is_game_over(&game.board),
    }
}

/// Game-over for a game whose ruleset name may not match its snake count.
///
/// `play` (the battlesnake CLI) runs `-g standard` with a single snake too:
/// the solo game-over check is chosen by how many snakes entered the game,
/// not by the mode label, exactly like the Go CLI (`WithSolo(len(urls) < 2)`).
/// The server keeps calling [`is_game_over`], whose ruleset-name dispatch
/// matches every game it creates.
pub fn is_game_over_for(game: &EngineGame, total_snakes: usize) -> bool {
    if total_snakes < 2 {
        rules::solo::is_game_over(&game.board)
    } else {
        is_game_over(game)
    }
}

/// Apply a single turn: move snakes, reduce health, feed, eliminate
///
/// Returns the IDs of the snakes that ate food this turn. This is the only
/// reliable food signal: body growth isn't, because Constrictor grows every
/// snake every turn without any food on the board.
///
/// Note: Unlike the rules crate's `execute_turn`, this does NOT increment
/// `board.turn` internally -- the caller must do that (for compatibility with
/// game_runner.rs which increments after recording frames).
pub fn apply_turn(
    game: &mut EngineGame,
    moves: &[(String, Direction)],
) -> color_eyre::Result<Vec<String>> {
    let royale = (game.meta.ruleset_name == "royale")
        .then(|| royale_settings(game))
        .transpose()?;
    let snake_moves: Vec<SnakeMove> = moves
        .iter()
        .map(|(id, dir)| SnakeMove {
            id: id.clone(),
            direction: *dir,
        })
        .collect();

    // Run the standard pipeline steps individually (so we can control turn increment)
    let _ = rules::standard::move_snakes(&mut game.board, &snake_moves);
    rules::standard::reduce_snake_health(&mut game.board);
    rules::standard::damage_hazards(&mut game.board, &game.meta.settings);
    let fed = rules::standard::feed_snakes(&mut game.board);
    let _ = rules::standard::eliminate_snakes(&mut game.board);

    // Mode-specific post-elimination stages. Dispatch on the ruleset name so
    // each mode's arm stays independent (parallel game-mode PRs union their
    // arms here).
    match game.meta.ruleset_name.as_str() {
        // Royale: spawn/grow hazards for the next turn (matches Go's
        // StageSpawnHazardsShrinkMap, which runs after elimination). Uses
        // `board.turn + 1` internally, so this must run before the caller
        // increments `board.turn`.
        "royale" => {
            let Some(settings) = royale.as_ref() else {
                unreachable!("royale settings were validated before turn application")
            };
            rules::royale::populate_hazards(&mut game.board, settings)
                .map_err(|error| eyre!("royale hazard population failed: {error:?}"))?;
        }
        // Constrictor: clear all food and grow every snake (matches Go's
        // StageSpawnFoodNoFood + StageModifySnakesAlwaysGrow, which run after
        // elimination).
        "constrictor" => {
            rules::constrictor::remove_food(&mut game.board);
            rules::constrictor::grow_snakes(&mut game.board)
                .expect("constrictor growth failed: zero-length snake");
        }
        // Snail Mode: decay hazard trails and record vacated tail squares
        // (matches Go's SnailModeMap.PostUpdateBoard, which runs after all
        // ruleset stages).
        "snail_mode" => rules::snail::post_update_board(&mut game.board),
        _ => {}
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
    if game.meta.ruleset_name == "constrictor" {
        return;
    }
    rules::food::maybe_spawn_food(&mut *rng, &mut game.board, &game.meta.settings);
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
        while !is_game_over(&game) && game.board.turn < MAX_TURNS {
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
        let rules_a = mode_rules(GameMode::Royale, id_a).2;
        let rules_b = mode_rules(GameMode::Royale, id_b).2;

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

    // --- is_game_over_for (single-snake games with any mode label) ---

    fn game_with_snakes(ruleset_name: &str, alive: usize, dead: usize) -> EngineGame {
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
                ruleset_name: ruleset_name.into(),
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

    #[test]
    fn is_game_over_for_single_snake_uses_solo_check() {
        // 1 alive of 1 entered, standard ruleset: the standard check would
        // end the game at turn 0; the solo check keeps it running.
        let game = game_with_snakes("standard", 1, 0);
        assert!(!is_game_over_for(&game, 1));
        // Once that snake dies, over.
        let game = game_with_snakes("standard", 0, 1);
        assert!(is_game_over_for(&game, 1));
    }

    #[test]
    fn is_game_over_for_multi_snake_uses_standard_check() {
        let game = game_with_snakes("standard", 2, 0);
        assert!(!is_game_over_for(&game, 2));
        let game = game_with_snakes("standard", 1, 1);
        assert!(is_game_over_for(&game, 2));
        let game = game_with_snakes("solo", 0, 2);
        assert!(is_game_over_for(&game, 2));
    }
}
