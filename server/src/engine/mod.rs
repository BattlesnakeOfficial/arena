//! Game engine module using the `rules` crate for game simulation
//!
//! This module provides game simulation using the rules crate's standard mode.
//! Internal state is `rules::BoardState`; conversion to JSON wire format for
//! the Battlesnake API happens at the HTTP boundary (see `wire.rs` and
//! `snake_client.rs`).

pub mod frame;

use rules::{BoardState, Direction, Point, SnakeMove, StandardSettings};
use uuid::Uuid;

use crate::models::game::{GameBoardSize, GameType};
use crate::models::game_battlesnake::GameBattlesnakeWithDetails;

pub const MAX_TURNS: i32 = 5000;

/// Metadata about the game that lives alongside the board state.
///
/// This replaces the `NestedGame` / `Game` wrapper from `battlesnake-game-types`.
#[derive(Debug, Clone)]
pub struct GameMeta {
    pub game_id: String,
    pub ruleset_name: String,
    pub timeout: i64,
    pub settings: StandardSettings,
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

/// Create the initial game state from database models
pub fn create_initial_game(
    game_id: Uuid,
    board_size: GameBoardSize,
    _game_type: GameType,
    battlesnakes: &[GameBattlesnakeWithDetails],
) -> EngineGame {
    let (w, h) = board_size.dimensions();
    let (width, height) = (w as i32, h as i32);

    let snake_ids: Vec<String> = battlesnakes
        .iter()
        .map(|bs| bs.game_battlesnake_id.to_string())
        .collect();

    let mut rng = rand::thread_rng();
    let board = rules::board::create_default_board_state(&mut rng, width, height, &snake_ids)
        .expect("Failed to create initial board state");

    let mut snake_names = std::collections::HashMap::new();
    for bs in battlesnakes {
        snake_names.insert(bs.game_battlesnake_id.to_string(), bs.name.clone());
    }

    let settings = StandardSettings {
        food_spawn_chance: 15,
        minimum_food: 1,
        hazard_damage_per_turn: 15,
    };

    EngineGame {
        board,
        meta: GameMeta {
            game_id: game_id.to_string(),
            ruleset_name: "standard".to_string(),
            timeout: 500,
            settings,
        },
        snake_names,
    }
}

/// Run a complete game with random moves, returning placements
pub fn run_game_with_random_moves(mut game: EngineGame) -> GameResult {
    let mut rng = rand::thread_rng();
    let mut elimination_order: Vec<String> = Vec::new();

    while !rules::standard::is_game_over(&game.board) && game.board.turn < MAX_TURNS {
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
                    *all_dirs.choose(&mut rng).unwrap()
                } else {
                    *reasonable.choose(&mut rng).unwrap()
                };

                SnakeMove {
                    id: s.id.clone(),
                    direction: dir,
                }
            })
            .collect();

        // Apply the turn
        let _game_over =
            rules::standard::execute_turn(&mut game.board, &moves, &game.meta.settings)
                .expect("execute_turn failed");

        // Spawn food after turn
        rules::food::maybe_spawn_food(&mut rng, &mut game.board, &game.meta.settings);

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

    GameResult {
        placements,
        final_turn: game.board.turn,
    }
}

/// Check if the game is over (1 or fewer snakes alive)
pub fn is_game_over(game: &EngineGame) -> bool {
    rules::standard::is_game_over(&game.board)
}

/// Apply a single turn: move snakes, reduce health, feed, eliminate
///
/// Note: Unlike the rules crate's `execute_turn`, this does NOT increment
/// `board.turn` internally -- the caller must do that (for compatibility with
/// game_runner.rs which increments after recording frames).
pub fn apply_turn(game: &mut EngineGame, moves: &[(String, Direction)]) {
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
    rules::standard::feed_snakes(&mut game.board);
    let _ = rules::standard::eliminate_snakes(&mut game.board);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rules::{EliminationCause, SNAKE_MAX_HEALTH, Snake};

    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    // --- Strategy functions ---

    fn arb_point(width: i32, height: i32) -> impl Strategy<Value = Point> {
        (0..width, 0..height).prop_map(|(x, y)| Point::new(x, y))
    }

    fn arb_snake(id: String, width: i32, height: i32) -> impl Strategy<Value = Snake> {
        (arb_point(width, height), 3..=8usize, 0..=100i32).prop_flat_map(
            move |(head, body_len, health)| {
                let id = id.clone();
                prop_vec(0..4u8, body_len - 1).prop_map(move |directions| {
                    let mut body = Vec::with_capacity(body_len);
                    body.push(head);

                    let deltas = [
                        (0, 1),  // Up
                        (0, -1), // Down
                        (-1, 0), // Left
                        (1, 0),  // Right
                    ];

                    let mut current = head;
                    for dir_start in &directions {
                        let mut placed = false;
                        for offset in 0..4u8 {
                            let idx = ((*dir_start + offset) % 4) as usize;
                            let (dx, dy) = deltas[idx];
                            let nx = current.x + dx;
                            let ny = current.y + dy;
                            if nx >= 0 && nx < width && ny >= 0 && ny < height {
                                let next = Point::new(nx, ny);
                                body.push(next);
                                current = next;
                                placed = true;
                                break;
                            }
                        }
                        if !placed {
                            // Fallback: duplicate current position
                            body.push(current);
                        }
                    }

                    Snake {
                        id: id.clone(),
                        body,
                        health,
                        eliminated_cause: if health <= 0 {
                            EliminationCause::OutOfHealth
                        } else {
                            EliminationCause::NotEliminated
                        },
                        eliminated_by: String::new(),
                        eliminated_on_turn: 0,
                    }
                })
            },
        )
    }

    fn arb_engine_game() -> impl Strategy<Value = EngineGame> {
        (3..=19i32, 3..=19i32, 1..=4usize).prop_flat_map(|(width, height, snake_count)| {
            let snakes: Vec<_> = (0..snake_count)
                .map(|i| arb_snake(format!("snake-{}", i), width, height))
                .collect();

            (
                snakes,
                prop_vec(arb_point(width, height), 0..=5),
                Just(width),
                Just(height),
            )
                .prop_map(|(snakes, food, width, height)| {
                    let mut snake_names = std::collections::HashMap::new();
                    for s in &snakes {
                        snake_names.insert(s.id.clone(), s.id.clone());
                    }
                    EngineGame {
                        board: BoardState {
                            turn: 0,
                            width,
                            height,
                            food,
                            snakes,
                            hazards: vec![],
                        },
                        meta: GameMeta {
                            game_id: "prop-test".to_string(),
                            ruleset_name: "standard".to_string(),
                            timeout: 500,
                            settings: StandardSettings::default(),
                        },
                        snake_names,
                    }
                })
        })
    }

    fn arb_moves(alive_ids: Vec<String>) -> impl Strategy<Value = Vec<(String, Direction)>> {
        let strategies: Vec<_> = alive_ids
            .into_iter()
            .map(|id| {
                prop::sample::select(
                    &[
                        Direction::Up,
                        Direction::Down,
                        Direction::Left,
                        Direction::Right,
                    ][..],
                )
                .prop_map(move |m| (id.clone(), m))
            })
            .collect();
        strategies
    }

    fn arb_game_and_moves() -> impl Strategy<Value = (EngineGame, Vec<(String, Direction)>)> {
        arb_engine_game().prop_flat_map(|game| {
            let alive_ids: Vec<String> = game
                .board
                .snakes
                .iter()
                .filter(|s| !s.eliminated_cause.is_eliminated())
                .map(|s| s.id.clone())
                .collect();
            let moves = arb_moves(alive_ids);
            (Just(game), moves)
        })
    }

    /// Helper: determine if a snake "ate" this turn.
    fn snake_ate(old_snake: &Snake, new_snake: &Snake, old_food: &[Point]) -> bool {
        !old_snake.eliminated_cause.is_eliminated() && old_food.contains(&new_snake.head())
    }

    // --- Property tests ---

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // === Conservation ===

        #[test]
        fn test_snake_count_conserved((ref game, ref moves) in arb_game_and_moves()) {
            let old_count = game.board.snakes.len();
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            prop_assert_eq!(new_game.board.snakes.len(), old_count);
        }

        #[test]
        fn test_non_eating_alive_snake_body_length_conserved(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                if !snake_ate(old_snake, new_snake, &old_food) {
                    prop_assert_eq!(
                        new_snake.body.len(),
                        old_snake.body.len(),
                        "Non-eating alive snake {} body length changed: {} -> {}",
                        old_snake.id,
                        old_snake.body.len(),
                        new_snake.body.len()
                    );
                }
            }
        }

        #[test]
        fn test_eating_snake_grows_by_one(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                if snake_ate(old_snake, new_snake, &old_food) {
                    prop_assert_eq!(
                        new_snake.body.len(),
                        old_snake.body.len() + 1,
                        "Eating snake {} body length: {} -> {} (expected +1)",
                        old_snake.id,
                        old_snake.body.len(),
                        new_snake.body.len()
                    );
                }
            }
        }

        #[test]
        fn test_food_only_disappears(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            for food_pos in &new_game.board.food {
                prop_assert!(
                    game.board.food.contains(food_pos),
                    "New food {:?} appeared that wasn't in old food",
                    food_pos
                );
            }
        }

        #[test]
        fn test_food_disappears_only_from_alive_snake_head(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            let old_food = &game.board.food;

            for food_pos in old_food {
                if !new_game.board.food.contains(food_pos) {
                    // This food was eaten — some alive snake's new head must be on it
                    let eaten_by_someone = game.board.snakes.iter()
                        .zip(new_game.board.snakes.iter())
                        .any(|(old_s, new_s)| {
                            !old_s.eliminated_cause.is_eliminated() && new_s.head() == *food_pos
                        });
                    prop_assert!(
                        eaten_by_someone,
                        "Food at {:?} disappeared but no alive snake landed on it",
                        food_pos
                    );
                }
            }
        }

        // === Bounds ===

        #[test]
        fn test_health_in_valid_range(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            for snake in &new_game.board.snakes {
                // The rules crate allows health to go negative before elimination,
                // but eliminated snakes' health values are not meaningful.
                // Only check alive snakes.
                if !snake.eliminated_cause.is_eliminated() {
                    prop_assert!(
                        snake.health >= 0 && snake.health <= 100,
                        "Alive snake {} health {} out of range [0, 100]",
                        snake.id,
                        snake.health
                    );
                }
            }
        }

        // === Monotonicity ===

        #[test]
        fn test_dead_snake_stays_dead(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    prop_assert!(
                        new_snake.eliminated_cause.is_eliminated(),
                        "Dead snake {} came back to life",
                        old_snake.id
                    );
                }
            }
        }

        #[test]
        fn test_dead_snake_unchanged(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    prop_assert_eq!(
                        new_snake.head(),
                        old_snake.head(),
                        "Dead snake {} head changed",
                        old_snake.id
                    );
                    prop_assert!(
                        new_snake.body == old_snake.body,
                        "Dead snake {} body changed",
                        old_snake.id
                    );
                }
            }
        }

        // === Movement ===

        #[test]
        fn test_alive_snake_head_moves_one_manhattan(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);
            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                let old_head = old_snake.head();
                let new_head = new_snake.head();
                let dx = (new_head.x - old_head.x).abs();
                let dy = (new_head.y - old_head.y).abs();
                prop_assert_eq!(
                    dx + dy,
                    1,
                    "Alive snake {} head moved manhattan distance {} (expected 1): {:?} -> {:?}",
                    old_snake.id,
                    dx + dy,
                    old_head,
                    new_head
                );
            }
        }

        #[test]
        fn test_body_follows_head_non_growing(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                if snake_ate(old_snake, new_snake, &old_food) {
                    continue;
                }
                // new_body[0] == new_head
                prop_assert_eq!(
                    new_snake.body[0],
                    new_snake.head(),
                    "Snake {} body[0] != head",
                    old_snake.id
                );
                // new_body[i] == old_body[i-1] for i in 1..len
                for i in 1..new_snake.body.len() {
                    prop_assert_eq!(
                        new_snake.body[i],
                        old_snake.body[i - 1],
                        "Snake {} body[{}] = {:?}, expected old body[{}] = {:?}",
                        old_snake.id,
                        i,
                        new_snake.body[i],
                        i - 1,
                        old_snake.body[i - 1]
                    );
                }
            }
        }

        #[test]
        fn test_body_follows_head_growing(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                if !snake_ate(old_snake, new_snake, &old_food) {
                    continue;
                }
                // new_body[0] == new_head
                prop_assert_eq!(
                    new_snake.body[0],
                    new_snake.head(),
                    "Growing snake {} body[0] != head",
                    old_snake.id
                );
                // new_body[i] == old_body[i-1] for i in 1..old_body.len()
                for i in 1..old_snake.body.len() {
                    prop_assert_eq!(
                        new_snake.body[i],
                        old_snake.body[i - 1],
                        "Growing snake {} body[{}] = {:?}, expected old body[{}] = {:?}",
                        old_snake.id,
                        i,
                        new_snake.body[i],
                        i - 1,
                        old_snake.body[i - 1]
                    );
                }
                // Last two elements equal (tail duplication)
                let len = new_snake.body.len();
                prop_assert_eq!(
                    new_snake.body[len - 1],
                    new_snake.body[len - 2],
                    "Growing snake {} tail not duplicated: body[{}]={:?} != body[{}]={:?}",
                    old_snake.id,
                    len - 1,
                    new_snake.body[len - 1],
                    len - 2,
                    new_snake.body[len - 2]
                );
                // Length increased by exactly 1
                prop_assert_eq!(
                    new_snake.body.len(),
                    old_snake.body.len() + 1,
                    "Growing snake {} body length: {} -> {} (expected +1)",
                    old_snake.id,
                    old_snake.body.len(),
                    new_snake.body.len()
                );
            }
        }

        // === Collision ===

        #[test]
        fn test_out_of_bounds_eliminated(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let width = game.board.width;
            let height = game.board.height;
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                let h = new_snake.head();
                if h.x < 0 || h.x >= width || h.y < 0 || h.y >= height {
                    prop_assert!(
                        new_snake.eliminated_cause.is_eliminated(),
                        "Snake {} out of bounds at {:?} but not eliminated",
                        old_snake.id,
                        h
                    );
                }
            }
        }

        #[test]
        fn test_self_collision_eliminated(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                let self_collision = new_snake.body[1..].contains(&new_snake.head());
                if self_collision {
                    prop_assert!(
                        new_snake.eliminated_cause.is_eliminated(),
                        "Snake {} self-collided at {:?} but not eliminated",
                        old_snake.id,
                        new_snake.head()
                    );
                }
            }
        }

        #[test]
        fn test_body_collision_with_other_snake_eliminated(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (idx, (old_snake, new_snake)) in game.board.snakes.iter()
                .zip(new_game.board.snakes.iter())
                .enumerate()
            {
                if old_snake.eliminated_cause.is_eliminated() {
                    continue;
                }
                // Check if new head is in any other SURVIVING snake's body[1..].
                // A snake eliminated for other reasons (out-of-bounds, out-of-health)
                // does not cause body collisions in the official rules.
                let body_collision = game.board.snakes.iter()
                    .zip(new_game.board.snakes.iter())
                    .enumerate()
                    .any(|(other_idx, (old_other, new_other))| {
                        other_idx != idx
                            && !old_other.eliminated_cause.is_eliminated()
                            && !new_other.eliminated_cause.is_eliminated()
                            && new_other.body[1..].contains(&new_snake.head())
                    });
                if body_collision {
                    prop_assert!(
                        new_snake.eliminated_cause.is_eliminated(),
                        "Snake {} body-collided with another snake at {:?} but not eliminated",
                        old_snake.id,
                        new_snake.head()
                    );
                }
            }
        }

        #[test]
        fn test_head_to_head_equal_both_die(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (i, (old_a, new_a)) in game.board.snakes.iter()
                .zip(new_game.board.snakes.iter())
                .enumerate()
            {
                if old_a.eliminated_cause.is_eliminated() {
                    continue;
                }
                for (j, (old_b, new_b)) in game.board.snakes.iter()
                    .zip(new_game.board.snakes.iter())
                    .enumerate()
                {
                    if j <= i || old_b.eliminated_cause.is_eliminated() {
                        continue;
                    }
                    if new_a.head() == new_b.head() && new_a.body.len() == new_b.body.len() {
                        prop_assert!(
                            new_a.eliminated_cause.is_eliminated(),
                            "Snake {} equal head-to-head at {:?} but not eliminated",
                            old_a.id,
                            new_a.head()
                        );
                        prop_assert!(
                            new_b.eliminated_cause.is_eliminated(),
                            "Snake {} equal head-to-head at {:?} but not eliminated",
                            old_b.id,
                            new_b.head()
                        );
                    }
                }
            }
        }

        #[test]
        fn test_head_to_head_smaller_dies(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (i, (old_a, new_a)) in game.board.snakes.iter()
                .zip(new_game.board.snakes.iter())
                .enumerate()
            {
                if old_a.eliminated_cause.is_eliminated() {
                    continue;
                }
                for (j, (old_b, new_b)) in game.board.snakes.iter()
                    .zip(new_game.board.snakes.iter())
                    .enumerate()
                {
                    if j == i || old_b.eliminated_cause.is_eliminated() {
                        continue;
                    }
                    if new_a.head() == new_b.head() && new_a.body.len() < new_b.body.len() {
                        prop_assert!(
                            new_a.eliminated_cause.is_eliminated(),
                            "Snake {} (len {}) lost head-to-head against {} (len {}) at {:?} but not eliminated",
                            old_a.id,
                            new_a.body.len(),
                            old_b.id,
                            new_b.body.len(),
                            new_a.head()
                        );
                    }
                }
            }
        }

        // === Feeding ===

        #[test]
        fn test_eating_restores_max_health(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let mut new_game = game.clone();
            apply_turn(&mut new_game, moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if !old_snake.eliminated_cause.is_eliminated()
                    && old_food.contains(&new_snake.head())
                    && !new_snake.eliminated_cause.is_eliminated()
                {
                    prop_assert_eq!(
                        new_snake.health, 100,
                        "Snake {} ate food but health is {} (expected 100)",
                        old_snake.id,
                        new_snake.health
                    );
                }
            }
        }
    }

    #[test]
    fn test_is_game_over() {
        let game = create_test_game(2);
        assert!(!is_game_over(&game));

        let mut game_one_alive = create_test_game(2);
        game_one_alive.board.snakes[0].eliminated_cause = EliminationCause::OutOfHealth;
        assert!(is_game_over(&game_one_alive));
    }

    #[test]
    fn test_run_full_game() {
        // Run multiple games to ensure consistency
        for _ in 0..10 {
            let game = create_test_game(4);
            let result = run_game_with_random_moves(game);

            // Should have placements for all 4 snakes
            assert_eq!(
                result.placements.len(),
                4,
                "All snakes should have placements"
            );

            // All snake IDs should be unique
            let mut ids = result.placements.clone();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), 4, "All placements should be unique snakes");

            // Game should end within MAX_TURNS
            assert!(
                result.final_turn <= MAX_TURNS,
                "Game should end within MAX_TURNS"
            );

            // Game should have progressed at least a few turns
            assert!(
                result.final_turn > 0,
                "Game should have run for at least one turn"
            );
        }
    }

    #[test]
    fn test_apply_turn_movement() {
        let mut game = create_test_game(1);
        game.board.snakes[0].body = vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)];

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Head should have moved up
        assert_eq!(game.board.snakes[0].head(), Point::new(5, 6));
        // Body should follow
        assert_eq!(game.board.snakes[0].body[0], Point::new(5, 6));
        assert_eq!(game.board.snakes[0].body[1], Point::new(5, 5));
        assert_eq!(game.board.snakes[0].body[2], Point::new(5, 4));
    }

    #[test]
    fn test_apply_turn_health_decrease() {
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 100;

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Health should decrease by 1
        assert_eq!(game.board.snakes[0].health, 99);
    }

    #[test]
    fn test_apply_turn_eating_food() {
        let mut game = create_test_game(1);
        game.board.snakes[0].body = vec![Point::new(5, 4), Point::new(5, 3), Point::new(5, 2)];
        game.board.snakes[0].health = 50;
        game.board.food = vec![Point::new(5, 5)];

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Health should be restored to max
        assert_eq!(game.board.snakes[0].health, SNAKE_MAX_HEALTH);
        // Snake should have grown
        assert_eq!(game.board.snakes[0].body.len(), 4);
        // Food should be consumed
        assert!(game.board.food.is_empty());
    }

    #[test]
    fn test_wall_collision_elimination() {
        let mut game = create_test_game(1);
        // Position snake at edge, moving into wall
        game.board.snakes[0].body = vec![Point::new(0, 5), Point::new(1, 5), Point::new(2, 5)];

        let moves = vec![("snake-0".to_string(), Direction::Left)];
        apply_turn(&mut game, &moves);

        // Snake should be eliminated
        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
    }

    #[test]
    fn test_head_to_head_collision_on_food() {
        // Regression test: two snakes colliding head-to-head on a food tile
        // should not panic due to double-removal of the same food index
        let mut game = create_test_game(2);

        // Position both snakes to collide on the food at (5, 5)
        game.board.snakes[0].body = vec![Point::new(5, 4), Point::new(5, 3), Point::new(5, 2)];

        game.board.snakes[1].body = vec![Point::new(5, 6), Point::new(5, 7), Point::new(5, 8)];

        game.board.food = vec![Point::new(5, 5)];

        // Both snakes move toward the food
        let moves = vec![
            ("snake-0".to_string(), Direction::Up),
            ("snake-1".to_string(), Direction::Down),
        ];

        // This should not panic - both snakes try to eat the same food
        apply_turn(&mut game, &moves);

        // Food should be consumed
        assert!(game.board.food.is_empty(), "Food should be consumed");

        // Both snakes should be eliminated (same size head-to-head)
        assert!(
            game.board.snakes[0].eliminated_cause.is_eliminated(),
            "Snake 0 should be eliminated in head-to-head"
        );
        assert!(
            game.board.snakes[1].eliminated_cause.is_eliminated(),
            "Snake 1 should be eliminated in head-to-head"
        );
    }

    #[test]
    fn test_self_collision_elimination() {
        let mut game = create_test_game(1);
        // Create a snake that will collide with itself
        game.board.snakes[0].body = vec![
            Point::new(5, 5),
            Point::new(5, 4),
            Point::new(6, 4),
            Point::new(6, 5),
            Point::new(6, 6),
        ];

        // Moving right will hit the body at (6, 5)
        let moves = vec![("snake-0".to_string(), Direction::Right)];
        apply_turn(&mut game, &moves);

        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
    }

    #[test]
    fn test_body_collision_with_other_snake() {
        let mut game = create_test_game(2);
        // Position snake-0 to collide with snake-1's body
        game.board.snakes[0].body = vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)];

        // Make snake-1 longer so (5,6) stays in body after it moves
        game.board.snakes[1].body = vec![
            Point::new(6, 6),
            Point::new(5, 6),
            Point::new(4, 6),
            Point::new(3, 6),
        ];

        // Snake-0 moves up into snake-1's body
        // Snake-1 moves right, body becomes [(7,6), (6,6), (5,6), (4,6)]
        let moves = vec![
            ("snake-0".to_string(), Direction::Up),
            ("snake-1".to_string(), Direction::Right),
        ];
        apply_turn(&mut game, &moves);

        // Snake-0 should be eliminated (hit snake-1's body at (5,6))
        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
        // Snake-1 should survive
        assert!(!game.board.snakes[1].eliminated_cause.is_eliminated());
    }

    #[test]
    fn test_head_to_head_smaller_loses() {
        let mut game = create_test_game(2);
        game.board.snakes[0].body = vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)]; // Length 3

        game.board.snakes[1].body = vec![
            Point::new(5, 7),
            Point::new(5, 8),
            Point::new(5, 9),
            Point::new(5, 10),
        ]; // Length 4

        // Both move to (5, 6)
        let moves = vec![
            ("snake-0".to_string(), Direction::Up),
            ("snake-1".to_string(), Direction::Down),
        ];
        apply_turn(&mut game, &moves);

        // Smaller snake loses
        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
        // Larger snake survives
        assert!(!game.board.snakes[1].eliminated_cause.is_eliminated());
    }

    #[test]
    fn test_head_to_head_equal_size_both_die() {
        let mut game = create_test_game(2);
        game.board.snakes[0].body = vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)];

        game.board.snakes[1].body = vec![Point::new(5, 7), Point::new(5, 8), Point::new(5, 9)];

        // Both move to (5, 6)
        let moves = vec![
            ("snake-0".to_string(), Direction::Up),
            ("snake-1".to_string(), Direction::Down),
        ];
        apply_turn(&mut game, &moves);

        // Both snakes should die
        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
        assert!(game.board.snakes[1].eliminated_cause.is_eliminated());
    }

    #[test]
    fn test_starvation_elimination() {
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 1; // Will reach 0 after move

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Snake should starve (health becomes 0)
        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
    }

    #[test]
    fn test_eating_restores_health() {
        let mut game = create_test_game(1);
        // Health needs to be > 1 so snake survives the health reduction step before eating
        game.board.snakes[0].health = 2;
        game.board.snakes[0].body = vec![Point::new(5, 4), Point::new(5, 3), Point::new(5, 2)];
        game.board.food = vec![Point::new(5, 5)];

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Snake should eat and restore health to max
        assert_eq!(game.board.snakes[0].health, SNAKE_MAX_HEALTH);
        assert!(game.board.food.is_empty());
    }

    #[test]
    fn test_health_1_snake_eats_food_and_survives() {
        // In the official Battlesnake rules, the pipeline is:
        //   move -> reduce_health -> damage_hazards -> feed -> eliminate
        // A snake with health=1 that moves onto food:
        //   - reduce_health: health goes 1->0
        //   - feed: eliminated_cause is still NotEliminated, head on food -> health=100, grow
        //   - eliminate: health is 100, not eliminated
        // So the snake survives. This matches the official Go implementation.
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 1;
        game.board.snakes[0].body = vec![Point::new(5, 4), Point::new(5, 3), Point::new(5, 2)];
        game.board.food = vec![Point::new(5, 5)];

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Snake eats food and health is restored to max
        assert!(!game.board.snakes[0].eliminated_cause.is_eliminated());
        assert_eq!(game.board.snakes[0].health, SNAKE_MAX_HEALTH);
        // Food was eaten
        assert!(game.board.food.is_empty());
        // Snake grew
        assert_eq!(game.board.snakes[0].body.len(), 4);
    }

    #[test]
    fn test_apply_turn_all_directions() {
        // Test all four movement directions
        for (direction, expected_head) in [
            (Direction::Up, Point::new(5, 6)),
            (Direction::Down, Point::new(5, 4)),
            (Direction::Left, Point::new(4, 5)),
            (Direction::Right, Point::new(6, 5)),
        ] {
            let mut game = create_test_game(1);
            game.board.snakes[0].body = vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)];

            let moves = vec![("snake-0".to_string(), direction)];
            apply_turn(&mut game, &moves);

            assert_eq!(
                game.board.snakes[0].head(),
                expected_head,
                "Failed for direction {:?}",
                direction
            );
        }
    }

    #[test]
    fn test_max_turns_constant() {
        assert_eq!(MAX_TURNS, 5000);
    }

    #[test]
    fn test_dead_snake_doesnt_move() {
        let mut game = create_test_game(1);
        game.board.snakes[0].eliminated_cause = EliminationCause::OutOfHealth;
        let original_head = game.board.snakes[0].head();
        let original_body = game.board.snakes[0].body.clone();

        let moves = vec![("snake-0".to_string(), Direction::Up)];
        apply_turn(&mut game, &moves);

        // Dead snake shouldn't move (head and body unchanged)
        assert!(game.board.snakes[0].eliminated_cause.is_eliminated());
        assert_eq!(game.board.snakes[0].head(), original_head);
        assert_eq!(game.board.snakes[0].body, original_body);
    }

    fn create_test_game(num_snakes: usize) -> EngineGame {
        let snakes: Vec<Snake> = (0..num_snakes)
            .map(|i| Snake {
                id: format!("snake-{}", i),
                body: vec![Point::new(i as i32 * 2 + 1, i as i32 * 2 + 1); 3],
                health: 100,
                eliminated_cause: EliminationCause::NotEliminated,
                eliminated_by: String::new(),
                eliminated_on_turn: 0,
            })
            .collect();

        let mut snake_names = std::collections::HashMap::new();
        for s in &snakes {
            snake_names.insert(s.id.clone(), format!("Snake {}", s.id));
        }

        EngineGame {
            board: BoardState {
                turn: 0,
                width: 11,
                height: 11,
                food: vec![Point::new(5, 5)],
                snakes,
                hazards: vec![],
            },
            meta: GameMeta {
                game_id: "test-game".to_string(),
                ruleset_name: "standard".to_string(),
                timeout: 500,
                settings: StandardSettings::default(),
            },
            snake_names,
        }
    }

    /// Test that create_initial_game assigns unique IDs when the same battlesnake
    /// appears multiple times (duplicate snakes in a game)
    #[test]
    fn test_create_initial_game_duplicate_snakes_have_unique_ids() {
        use crate::models::game::{GameBoardSize, GameType};
        use crate::models::game_battlesnake::GameBattlesnakeWithDetails;
        use uuid::Uuid;

        // Same battlesnake_id but different game_battlesnake_ids (as would happen with duplicates)
        let shared_battlesnake_id = Uuid::new_v4();
        let battlesnakes = vec![
            GameBattlesnakeWithDetails {
                game_battlesnake_id: Uuid::new_v4(),
                game_id: Uuid::new_v4(),
                battlesnake_id: shared_battlesnake_id,
                placement: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                name: "Duplicate Snake".to_string(),
                url: "https://example.com/snake".to_string(),
                user_id: Uuid::new_v4(),
                leaderboard_entry_id: None,
                color: String::new(),
                head: String::new(),
                tail: String::new(),
            },
            GameBattlesnakeWithDetails {
                game_battlesnake_id: Uuid::new_v4(),
                game_id: Uuid::new_v4(),
                battlesnake_id: shared_battlesnake_id, // Same battlesnake_id
                placement: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                name: "Duplicate Snake".to_string(),
                url: "https://example.com/snake".to_string(),
                user_id: Uuid::new_v4(),
                leaderboard_entry_id: None,
                color: String::new(),
                head: String::new(),
                tail: String::new(),
            },
        ];

        let game = create_initial_game(
            Uuid::new_v4(),
            GameBoardSize::Medium,
            GameType::Standard,
            &battlesnakes,
        );

        // Verify we have 2 snakes
        assert_eq!(game.board.snakes.len(), 2);

        // Verify the snake IDs are unique (they should be game_battlesnake_ids)
        let snake_ids: Vec<&str> = game.board.snakes.iter().map(|s| s.id.as_str()).collect();
        assert_ne!(
            snake_ids[0], snake_ids[1],
            "Duplicate snakes should have unique IDs (game_battlesnake_id)"
        );

        // Verify the IDs are the game_battlesnake_ids, not the battlesnake_ids
        assert_eq!(
            snake_ids[0],
            battlesnakes[0].game_battlesnake_id.to_string()
        );
        assert_eq!(
            snake_ids[1],
            battlesnakes[1].game_battlesnake_id.to_string()
        );
    }
}
