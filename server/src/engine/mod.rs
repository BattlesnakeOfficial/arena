//! Game engine module using battlesnake-game-types wire representation
//!
//! This module provides game simulation using the official Battlesnake rules.
//! It uses the wire representation types directly for simplicity.

pub mod frame;

use battlesnake_game_types::types::{Move, RandomReasonableMovesGame};
use battlesnake_game_types::wire_representation::{
    BattleSnake, Board, Game, NestedGame, Position, Ruleset, Settings,
};
use rand::Rng;
use rand::seq::SliceRandom;
use std::collections::VecDeque;
use uuid::Uuid;

use crate::models::game::{GameBoardSize, GameType};
use crate::models::game_battlesnake::GameBattlesnakeWithDetails;

const SNAKE_MAX_HEALTH: i32 = 100;
const SNAKE_START_SIZE: usize = 3;
pub const MAX_TURNS: i32 = 5000;

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
    game_type: GameType,
    battlesnakes: &[GameBattlesnakeWithDetails],
) -> Game {
    let (width, height) = match board_size {
        GameBoardSize::Small => (7, 7),
        GameBoardSize::Medium => (11, 11),
        GameBoardSize::Large => (19, 19),
    };

    let ruleset_name = match game_type {
        GameType::Standard => "standard",
        GameType::Royale => "royale",
        GameType::Constrictor => "constrictor",
        GameType::SnailMode => "snail_mode",
    };

    // Generate spawn positions
    let spawn_positions = generate_spawn_positions(width, height, battlesnakes.len());

    // Create snakes at spawn positions
    // Use game_battlesnake_id as the snake ID to ensure uniqueness when the same
    // battlesnake appears multiple times in a game (duplicate snakes)
    let snakes: Vec<BattleSnake> = battlesnakes
        .iter()
        .zip(spawn_positions.iter())
        .map(|(bs, pos)| {
            let body: VecDeque<Position> = (0..SNAKE_START_SIZE).map(|_| *pos).collect();
            BattleSnake {
                id: bs.game_battlesnake_id.to_string(),
                name: bs.name.clone(),
                head: *pos,
                body,
                health: SNAKE_MAX_HEALTH,
                shout: None,
                actual_length: None,
            }
        })
        .collect();

    // Place initial food - one near each snake plus center
    let food = generate_initial_food(width, height, &snakes);

    let board = Board {
        height: height as u32,
        width: width as u32,
        food,
        snakes: snakes.clone(),
        hazards: vec![],
    };

    // Use first snake as "you" (arbitrary for simulation purposes)
    let you = snakes.first().cloned().unwrap_or_else(|| BattleSnake {
        id: "dummy".to_string(),
        name: "Dummy".to_string(),
        head: Position::new(0, 0),
        body: VecDeque::new(),
        health: 0,
        shout: None,
        actual_length: None,
    });

    Game {
        you,
        board,
        turn: 0,
        game: NestedGame {
            id: game_id.to_string(),
            ruleset: Ruleset {
                name: ruleset_name.to_string(),
                version: "v1.0.0".to_string(),
                settings: Some(Settings {
                    food_spawn_chance: 15,
                    minimum_food: 1,
                    hazard_damage_per_turn: 15,
                    hazard_map: None,
                    hazard_map_author: None,
                    royale: None,
                }),
            },
            timeout: 500,
            map: None,
            source: None,
        },
    }
}

/// Generate spawn positions using the official Battlesnake algorithm
/// For <=8 snakes on boards >=7x7, uses fixed corner/cardinal positions
fn generate_spawn_positions(width: i32, _height: i32, num_snakes: usize) -> Vec<Position> {
    let mut rng = rand::thread_rng();

    // mn = 1, md = (width-1)/2, mx = width-2
    let mn = 1;
    let md = (width - 1) / 2;
    let mx = width - 2;

    // Corner positions
    let mut corner_points = vec![
        Position::new(mn, mn),
        Position::new(mn, mx),
        Position::new(mx, mn),
        Position::new(mx, mx),
    ];

    // Cardinal positions (edges)
    let mut cardinal_points = vec![
        Position::new(mn, md),
        Position::new(md, mn),
        Position::new(md, mx),
        Position::new(mx, md),
    ];

    // Shuffle both lists
    corner_points.shuffle(&mut rng);
    cardinal_points.shuffle(&mut rng);

    // Randomly decide whether to prioritize corners or cardinals
    let mut start_points = if rng.gen_bool(0.5) {
        let mut points = corner_points;
        points.extend(cardinal_points);
        points
    } else {
        let mut points = cardinal_points;
        points.extend(corner_points);
        points
    };

    // Take as many positions as we need
    start_points.truncate(num_snakes);
    start_points
}

/// Generate initial food positions
fn generate_initial_food(width: i32, height: i32, snakes: &[BattleSnake]) -> Vec<Position> {
    let mut rng = rand::thread_rng();
    let mut food: Vec<Position> = Vec::new();
    let center = Position::new((width - 1) / 2, (height - 1) / 2);

    // Place food near each snake (diagonal from head, away from center)
    for snake in snakes {
        let head = snake.head;
        let possible_food_locations = [
            Position::new(head.x - 1, head.y - 1),
            Position::new(head.x - 1, head.y + 1),
            Position::new(head.x + 1, head.y - 1),
            Position::new(head.x + 1, head.y + 1),
        ];

        // Filter valid positions
        let available: Vec<Position> = possible_food_locations
            .iter()
            .filter(|p| {
                // Must be on board
                p.x >= 0 && p.x < width && p.y >= 0 && p.y < height
                    // Not the center
                    && **p != center
                    // Not already food
                    && !food.contains(p)
                    // Not a corner
                    && !((p.x == 0 || p.x == width - 1) && (p.y == 0 || p.y == height - 1))
            })
            .copied()
            .collect();

        if let Some(pos) = available.choose(&mut rng) {
            food.push(*pos);
        }
    }

    // Always place food in center
    if !snakes.iter().any(|s| s.body.contains(&center)) {
        food.push(center);
    }

    food
}

/// Run a complete game with random moves, returning placements
pub fn run_game_with_random_moves(mut game: Game) -> GameResult {
    let mut rng = rand::thread_rng();
    let mut elimination_order: Vec<String> = Vec::new();

    while !is_game_over(&game) && game.turn < MAX_TURNS {
        // Get random reasonable moves for each alive snake
        let moves: Vec<(String, Move)> = game
            .random_reasonable_move_for_each_snake(&mut rng)
            .collect();

        // Apply the moves
        game = apply_turn(game, &moves);
        game.turn += 1;

        // Track newly eliminated snakes
        for snake in &game.board.snakes {
            if snake.health <= 0 && !elimination_order.contains(&snake.id) {
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
        .filter(|s| s.health > 0)
        .map(|s| s.id.clone())
        .collect();

    // Then add eliminated snakes in reverse order (last eliminated = better placement)
    elimination_order.reverse();
    placements.extend(elimination_order);

    GameResult {
        placements,
        final_turn: game.turn,
    }
}

/// Check if the game is over (1 or fewer snakes alive)
fn is_game_over(game: &Game) -> bool {
    let alive_count = game.board.snakes.iter().filter(|s| s.health > 0).count();
    alive_count <= 1
}

/// Apply a single turn: move snakes, reduce health, feed, eliminate
pub fn apply_turn(mut game: Game, moves: &[(String, Move)]) -> Game {
    // 1. Move snakes
    for snake in &mut game.board.snakes {
        if snake.health <= 0 {
            continue;
        }

        // Find the move for this snake
        let snake_move = moves
            .iter()
            .find(|(id, _)| id == &snake.id)
            .map(|(_, m)| *m)
            .unwrap_or(Move::Up);

        // Calculate new head position
        let new_head = snake.head.add_vec(snake_move.to_vector());

        // Move: add new head, remove tail
        snake.body.push_front(new_head);
        snake.body.pop_back();
        snake.head = new_head;
    }

    // 2. Reduce health
    for snake in &mut game.board.snakes {
        if snake.health > 0 {
            snake.health -= 1;
        }
    }

    // 3. Feed snakes (before elimination check)
    let mut eaten_food = Vec::new();
    for snake in &mut game.board.snakes {
        if snake.health <= 0 {
            continue;
        }

        // Check if head is on food
        if let Some(food_idx) = game.board.food.iter().position(|f| *f == snake.head) {
            // Eat the food
            eaten_food.push(food_idx);
            snake.health = SNAKE_MAX_HEALTH;
            // Grow by duplicating tail
            if let Some(tail) = snake.body.back().copied() {
                snake.body.push_back(tail);
            }
        }
    }

    // Remove eaten food (in reverse order to preserve indices)
    // Deduplicate in case multiple snakes ate the same food (head-to-head on food)
    eaten_food.sort();
    eaten_food.dedup();
    eaten_food.reverse();
    for idx in eaten_food {
        game.board.food.remove(idx);
    }

    // 4. Eliminate snakes
    eliminate_snakes(&mut game);

    // Update "you" to match the board state
    if let Some(you_snake) = game.board.snakes.iter().find(|s| s.id == game.you.id) {
        game.you = you_snake.clone();
    }

    game
}

/// Eliminate snakes that are out of health, out of bounds, or have collided
fn eliminate_snakes(game: &mut Game) {
    let width = game.board.width as i32;
    let height = game.board.height as i32;

    // Collect elimination info first (can't mutate while iterating)
    let mut eliminations: Vec<(String, &'static str)> = Vec::new();

    // Check each snake
    for snake in &game.board.snakes {
        if snake.health <= 0 {
            continue; // Already eliminated
        }

        let head = snake.head;

        // Out of bounds check
        if head.x < 0 || head.x >= width || head.y < 0 || head.y >= height {
            eliminations.push((snake.id.clone(), "wall-collision"));
            continue;
        }

        // Out of health check (should already be 0 if starved)
        if snake.health <= 0 {
            eliminations.push((snake.id.clone(), "out-of-health"));
            continue;
        }

        // Self collision check (head hitting own body, excluding head position)
        let self_collision = snake.body.iter().skip(1).any(|p| *p == head);
        if self_collision {
            eliminations.push((snake.id.clone(), "snake-self-collision"));
            continue;
        }

        // Body collision with other snakes
        let body_collision = game.board.snakes.iter().any(|other| {
            other.id != snake.id
                && other.health > 0
                && other.body.iter().skip(1).any(|p| *p == head)
        });
        if body_collision {
            eliminations.push((snake.id.clone(), "snake-collision"));
            continue;
        }

        // Head-to-head collision (lose if same size or smaller)
        let head_collision = game.board.snakes.iter().any(|other| {
            other.id != snake.id
                && other.health > 0
                && other.head == head
                && snake.body.len() <= other.body.len()
        });
        if head_collision {
            eliminations.push((snake.id.clone(), "head-collision"));
        }
    }

    // Apply eliminations
    for (snake_id, _cause) in eliminations {
        if let Some(snake) = game.board.snakes.iter_mut().find(|s| s.id == snake_id) {
            snake.health = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    // --- Strategy functions ---

    fn arb_position(width: u32, height: u32) -> impl Strategy<Value = Position> {
        (0..width as i32, 0..height as i32).prop_map(|(x, y)| Position::new(x, y))
    }

    fn arb_snake(id: String, width: u32, height: u32) -> impl Strategy<Value = BattleSnake> {
        (arb_position(width, height), 3..=8usize, 0..=100i32).prop_flat_map(
            move |(head, body_len, health)| {
                let id = id.clone();
                prop_vec(0..4u8, body_len - 1).prop_map(move |directions| {
                    let mut body = VecDeque::with_capacity(body_len);
                    body.push_back(head);

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
                            if nx >= 0 && nx < width as i32 && ny >= 0 && ny < height as i32 {
                                let next = Position::new(nx, ny);
                                body.push_back(next);
                                current = next;
                                placed = true;
                                break;
                            }
                        }
                        if !placed {
                            // Fallback: duplicate current position
                            body.push_back(current);
                        }
                    }

                    BattleSnake {
                        id: id.clone(),
                        name: id.clone(),
                        head,
                        body,
                        health,
                        shout: None,
                        actual_length: None,
                    }
                })
            },
        )
    }

    fn arb_game() -> impl Strategy<Value = Game> {
        (3..=19u32, 3..=19u32, 1..=4usize).prop_flat_map(|(width, height, snake_count)| {
            let snakes: Vec<_> = (0..snake_count)
                .map(|i| arb_snake(format!("snake-{}", i), width, height))
                .collect();

            (
                snakes,
                prop_vec(arb_position(width, height), 0..=5),
                Just(width),
                Just(height),
            )
                .prop_map(|(snakes, food, width, height)| {
                    let you = snakes[0].clone();
                    Game {
                        you,
                        board: Board {
                            height,
                            width,
                            food,
                            snakes,
                            hazards: vec![],
                        },
                        turn: 0,
                        game: NestedGame {
                            id: "prop-test".to_string(),
                            ruleset: Ruleset {
                                name: "standard".to_string(),
                                version: "v1.0.0".to_string(),
                                settings: None,
                            },
                            timeout: 500,
                            map: None,
                            source: None,
                        },
                    }
                })
        })
    }

    fn arb_moves(alive_ids: Vec<String>) -> impl Strategy<Value = Vec<(String, Move)>> {
        let strategies: Vec<_> = alive_ids
            .into_iter()
            .map(|id| {
                prop::sample::select(&[Move::Up, Move::Down, Move::Left, Move::Right][..])
                    .prop_map(move |m| (id.clone(), m))
            })
            .collect();
        strategies
    }

    fn arb_game_and_moves() -> impl Strategy<Value = (Game, Vec<(String, Move)>)> {
        arb_game().prop_flat_map(|game| {
            let alive_ids: Vec<String> = game
                .board
                .snakes
                .iter()
                .filter(|s| s.health > 0)
                .map(|s| s.id.clone())
                .collect();
            let moves = arb_moves(alive_ids);
            (Just(game), moves)
        })
    }

    /// Helper: determine if a snake "ate" this turn.
    /// Snake ate = new head on old food position AND original health >= 2.
    fn snake_ate(old_snake: &BattleSnake, new_snake: &BattleSnake, old_food: &[Position]) -> bool {
        old_snake.health >= 2 && old_food.contains(&new_snake.head)
    }

    // --- Property tests ---

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // === Conservation ===

        #[test]
        fn test_snake_count_conserved((ref game, ref moves) in arb_game_and_moves()) {
            let old_count = game.board.snakes.len();
            let new_game = apply_turn(game.clone(), moves);
            prop_assert_eq!(new_game.board.snakes.len(), old_count);
        }

        #[test]
        fn test_non_eating_alive_snake_body_length_conserved(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
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
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
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
            let new_game = apply_turn(game.clone(), moves);
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
            let new_game = apply_turn(game.clone(), moves);
            let old_food = &game.board.food;

            for food_pos in old_food {
                if !new_game.board.food.contains(food_pos) {
                    // This food was eaten — some alive snake's new head must be on it
                    // with original health >= 2
                    let eaten_by_someone = game.board.snakes.iter()
                        .zip(new_game.board.snakes.iter())
                        .any(|(old_s, new_s)| {
                            old_s.health >= 2 && new_s.head == *food_pos
                        });
                    prop_assert!(
                        eaten_by_someone,
                        "Food at {:?} disappeared but no alive snake (health>=2) landed on it",
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
            let new_game = apply_turn(game.clone(), moves);
            for snake in &new_game.board.snakes {
                prop_assert!(
                    snake.health >= 0 && snake.health <= 100,
                    "Snake {} health {} out of range [0, 100]",
                    snake.id,
                    snake.health
                );
            }
        }

        #[test]
        fn test_head_equals_body_zero(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let new_game = apply_turn(game.clone(), moves);
            for snake in &new_game.board.snakes {
                prop_assert_eq!(
                    snake.head,
                    snake.body[0],
                    "Snake {} head {:?} != body[0] {:?}",
                    snake.id,
                    snake.head,
                    snake.body[0]
                );
            }
        }

        // === Monotonicity ===

        #[test]
        fn test_dead_snake_stays_dead(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let new_game = apply_turn(game.clone(), moves);
            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health == 0 {
                    prop_assert_eq!(
                        new_snake.health, 0,
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
            let new_game = apply_turn(game.clone(), moves);
            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health == 0 {
                    prop_assert_eq!(
                        new_snake.head,
                        old_snake.head,
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
            let new_game = apply_turn(game.clone(), moves);
            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
                    continue;
                }
                let dx = (new_snake.head.x - old_snake.head.x).abs();
                let dy = (new_snake.head.y - old_snake.head.y).abs();
                prop_assert_eq!(
                    dx + dy,
                    1,
                    "Alive snake {} head moved manhattan distance {} (expected 1): {:?} -> {:?}",
                    old_snake.id,
                    dx + dy,
                    old_snake.head,
                    new_snake.head
                );
            }
        }

        #[test]
        fn test_body_follows_head_non_growing(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
                    continue;
                }
                if snake_ate(old_snake, new_snake, &old_food) {
                    continue;
                }
                // new_body[0] == new_head
                prop_assert_eq!(
                    new_snake.body[0],
                    new_snake.head,
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
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
                    continue;
                }
                if !snake_ate(old_snake, new_snake, &old_food) {
                    continue;
                }
                // new_body[0] == new_head
                prop_assert_eq!(
                    new_snake.body[0],
                    new_snake.head,
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
            let width = game.board.width as i32;
            let height = game.board.height as i32;
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
                    continue;
                }
                let h = new_snake.head;
                if h.x < 0 || h.x >= width || h.y < 0 || h.y >= height {
                    prop_assert_eq!(
                        new_snake.health, 0,
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
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health <= 0 {
                    continue;
                }
                let self_collision = new_snake.body.iter().skip(1).any(|p| *p == new_snake.head);
                if self_collision {
                    prop_assert_eq!(
                        new_snake.health, 0,
                        "Snake {} self-collided at {:?} but not eliminated",
                        old_snake.id,
                        new_snake.head
                    );
                }
            }
        }

        #[test]
        fn test_body_collision_with_other_snake_eliminated(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let new_game = apply_turn(game.clone(), moves);

            for (idx, (old_snake, new_snake)) in game.board.snakes.iter()
                .zip(new_game.board.snakes.iter())
                .enumerate()
            {
                if old_snake.health <= 0 {
                    continue;
                }
                // Check if new head is in any other snake's body[1..]
                // where the other snake had original health >= 2
                let body_collision = game.board.snakes.iter()
                    .zip(new_game.board.snakes.iter())
                    .enumerate()
                    .any(|(other_idx, (old_other, new_other))| {
                        other_idx != idx
                            && old_other.health >= 2
                            && new_other.body.iter().skip(1).any(|p| *p == new_snake.head)
                    });
                if body_collision {
                    prop_assert_eq!(
                        new_snake.health, 0,
                        "Snake {} body-collided with another snake at {:?} but not eliminated",
                        old_snake.id,
                        new_snake.head
                    );
                }
            }
        }

        #[test]
        fn test_head_to_head_equal_both_die(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let new_game = apply_turn(game.clone(), moves);

            for (i, (old_a, new_a)) in game.board.snakes.iter()
                .zip(new_game.board.snakes.iter())
                .enumerate()
            {
                if old_a.health <= 0 {
                    continue;
                }
                for (j, (old_b, new_b)) in game.board.snakes.iter()
                    .zip(new_game.board.snakes.iter())
                    .enumerate()
                {
                    if j <= i || old_b.health <= 0 {
                        continue;
                    }
                    if new_a.head == new_b.head && new_a.body.len() == new_b.body.len() {
                        prop_assert_eq!(
                            new_a.health, 0,
                            "Snake {} equal head-to-head at {:?} but not eliminated",
                            old_a.id,
                            new_a.head
                        );
                        prop_assert_eq!(
                            new_b.health, 0,
                            "Snake {} equal head-to-head at {:?} but not eliminated",
                            old_b.id,
                            new_b.head
                        );
                    }
                }
            }
        }

        #[test]
        fn test_head_to_head_smaller_dies(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let new_game = apply_turn(game.clone(), moves);

            for (i, (old_a, new_a)) in game.board.snakes.iter()
                .zip(new_game.board.snakes.iter())
                .enumerate()
            {
                if old_a.health <= 0 {
                    continue;
                }
                for (j, (old_b, new_b)) in game.board.snakes.iter()
                    .zip(new_game.board.snakes.iter())
                    .enumerate()
                {
                    if j == i || old_b.health <= 0 {
                        continue;
                    }
                    if new_a.head == new_b.head && new_a.body.len() < new_b.body.len() {
                        prop_assert_eq!(
                            new_a.health, 0,
                            "Snake {} (len {}) lost head-to-head against {} (len {}) at {:?} but not eliminated",
                            old_a.id,
                            new_a.body.len(),
                            old_b.id,
                            new_b.body.len(),
                            new_a.head
                        );
                    }
                }
            }
        }

        // === Feeding ===

        #[test]
        fn test_starve_before_eat(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health == 1 && game.board.food.contains(&new_snake.head) {
                    prop_assert_eq!(
                        new_snake.health, 0,
                        "Snake {} with health=1 landed on food but didn't starve",
                        old_snake.id
                    );
                }
            }
        }

        #[test]
        fn test_eating_restores_max_health(
            (ref game, ref moves) in arb_game_and_moves()
        ) {
            let old_food = game.board.food.clone();
            let new_game = apply_turn(game.clone(), moves);

            for (old_snake, new_snake) in game.board.snakes.iter().zip(new_game.board.snakes.iter()) {
                if old_snake.health >= 2
                    && old_food.contains(&new_snake.head)
                    && new_snake.health > 0
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
    fn test_generate_spawn_positions() {
        let positions = generate_spawn_positions(11, 11, 4);
        assert_eq!(positions.len(), 4);

        // All positions should be unique
        for (i, p1) in positions.iter().enumerate() {
            for (j, p2) in positions.iter().enumerate() {
                if i != j {
                    assert_ne!(p1, p2, "Positions should be unique");
                }
            }
        }

        // All positions should be on the board
        for pos in &positions {
            assert!(pos.x >= 0 && pos.x < 11);
            assert!(pos.y >= 0 && pos.y < 11);
        }
    }

    #[test]
    fn test_is_game_over() {
        let game = create_test_game(2);
        assert!(!is_game_over(&game));

        let mut game_one_alive = create_test_game(2);
        game_one_alive.board.snakes[0].health = 0;
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
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(5, 3),
        ]);

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // Head should have moved up
        assert_eq!(game.board.snakes[0].head, Position::new(5, 6));
        // Body should follow
        assert_eq!(game.board.snakes[0].body[0], Position::new(5, 6));
        assert_eq!(game.board.snakes[0].body[1], Position::new(5, 5));
        assert_eq!(game.board.snakes[0].body[2], Position::new(5, 4));
    }

    #[test]
    fn test_apply_turn_health_decrease() {
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 100;

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // Health should decrease by 1
        assert_eq!(game.board.snakes[0].health, 99);
    }

    #[test]
    fn test_apply_turn_eating_food() {
        let mut game = create_test_game(1);
        game.board.snakes[0].head = Position::new(5, 4);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 4),
            Position::new(5, 3),
            Position::new(5, 2),
        ]);
        game.board.snakes[0].health = 50;
        game.board.food = vec![Position::new(5, 5)];

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

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
        game.board.snakes[0].head = Position::new(0, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(0, 5),
            Position::new(1, 5),
            Position::new(2, 5),
        ]);

        let moves = vec![("snake-0".to_string(), Move::Left)];
        let game = apply_turn(game, &moves);

        // Snake should be eliminated (health = 0)
        assert_eq!(game.board.snakes[0].health, 0);
    }

    #[test]
    fn test_head_to_head_collision_on_food() {
        // Regression test: two snakes colliding head-to-head on a food tile
        // should not panic due to double-removal of the same food index
        let mut game = create_test_game(2);

        // Position both snakes to collide on the food at (5, 5)
        game.board.snakes[0].head = Position::new(5, 4);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 4),
            Position::new(5, 3),
            Position::new(5, 2),
        ]);

        game.board.snakes[1].head = Position::new(5, 6);
        game.board.snakes[1].body = VecDeque::from([
            Position::new(5, 6),
            Position::new(5, 7),
            Position::new(5, 8),
        ]);

        game.board.food = vec![Position::new(5, 5)];

        // Both snakes move toward the food
        let moves = vec![
            ("snake-0".to_string(), Move::Up),
            ("snake-1".to_string(), Move::Down),
        ];

        // This should not panic - both snakes try to eat the same food
        let game = apply_turn(game, &moves);

        // Food should be consumed
        assert!(game.board.food.is_empty(), "Food should be consumed");

        // Both snakes should be eliminated (same size head-to-head)
        assert_eq!(
            game.board.snakes[0].health, 0,
            "Snake 0 should be eliminated in head-to-head"
        );
        assert_eq!(
            game.board.snakes[1].health, 0,
            "Snake 1 should be eliminated in head-to-head"
        );
    }

    #[test]
    fn test_self_collision_elimination() {
        let mut game = create_test_game(1);
        // Create a snake that will collide with itself
        // Snake body forms an L shape, moving into its own body
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(6, 4),
            Position::new(6, 5),
            Position::new(6, 6),
        ]);

        // Moving right will hit the body at (6, 5)
        let moves = vec![("snake-0".to_string(), Move::Right)];
        let game = apply_turn(game, &moves);

        assert_eq!(game.board.snakes[0].health, 0);
    }

    #[test]
    fn test_body_collision_with_other_snake() {
        let mut game = create_test_game(2);
        // Position snake-0 to collide with snake-1's body
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(5, 3),
        ]);

        // Make snake-1 longer so (5,6) stays in body after it moves
        game.board.snakes[1].head = Position::new(6, 6);
        game.board.snakes[1].body = VecDeque::from([
            Position::new(6, 6),
            Position::new(5, 6),
            Position::new(4, 6),
            Position::new(3, 6),
        ]);

        // Snake-0 moves up into snake-1's body
        // Snake-1 moves right, body becomes [(7,6), (6,6), (5,6), (4,6)]
        let moves = vec![
            ("snake-0".to_string(), Move::Up),
            ("snake-1".to_string(), Move::Right),
        ];
        let game = apply_turn(game, &moves);

        // Snake-0 should be eliminated (hit snake-1's body at (5,6))
        assert_eq!(game.board.snakes[0].health, 0);
        // Snake-1 should survive
        assert!(game.board.snakes[1].health > 0);
    }

    #[test]
    fn test_head_to_head_smaller_loses() {
        let mut game = create_test_game(2);
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(5, 3),
        ]); // Length 3

        game.board.snakes[1].head = Position::new(5, 7);
        game.board.snakes[1].body = VecDeque::from([
            Position::new(5, 7),
            Position::new(5, 8),
            Position::new(5, 9),
            Position::new(5, 10),
        ]); // Length 4

        // Both move to (5, 6)
        let moves = vec![
            ("snake-0".to_string(), Move::Up),
            ("snake-1".to_string(), Move::Down),
        ];
        let game = apply_turn(game, &moves);

        // Smaller snake loses
        assert_eq!(game.board.snakes[0].health, 0);
        // Larger snake survives
        assert!(game.board.snakes[1].health > 0);
    }

    #[test]
    fn test_head_to_head_equal_size_both_die() {
        let mut game = create_test_game(2);
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(5, 3),
        ]);

        game.board.snakes[1].head = Position::new(5, 7);
        game.board.snakes[1].body = VecDeque::from([
            Position::new(5, 7),
            Position::new(5, 8),
            Position::new(5, 9),
        ]);

        // Both move to (5, 6)
        let moves = vec![
            ("snake-0".to_string(), Move::Up),
            ("snake-1".to_string(), Move::Down),
        ];
        let game = apply_turn(game, &moves);

        // Both snakes should die
        assert_eq!(game.board.snakes[0].health, 0);
        assert_eq!(game.board.snakes[1].health, 0);
    }

    #[test]
    fn test_starvation_elimination() {
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 1; // Will reach 0 after move

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // Snake should starve (health becomes 0)
        assert_eq!(game.board.snakes[0].health, 0);
    }

    #[test]
    fn test_eating_restores_health() {
        let mut game = create_test_game(1);
        // Health needs to be > 1 so snake survives the health reduction step before eating
        game.board.snakes[0].health = 2;
        game.board.snakes[0].head = Position::new(5, 4);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 4),
            Position::new(5, 3),
            Position::new(5, 2),
        ]);
        game.board.food = vec![Position::new(5, 5)];

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // Snake should eat and restore health to max
        assert_eq!(game.board.snakes[0].health, SNAKE_MAX_HEALTH);
        assert!(game.board.food.is_empty());
    }

    #[test]
    fn test_starve_before_eating() {
        // Snake with health=1 reaching food still starves
        // because health is reduced before feeding check
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 1;
        game.board.snakes[0].head = Position::new(5, 4);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 4),
            Position::new(5, 3),
            Position::new(5, 2),
        ]);
        game.board.food = vec![Position::new(5, 5)];

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // Snake starves before eating (health reduced 1->0 before food check)
        assert_eq!(game.board.snakes[0].health, 0);
        // Food was NOT eaten (snake was already dead)
        assert_eq!(game.board.food.len(), 1);
    }

    #[test]
    fn test_apply_turn_all_directions() {
        // Test all four movement directions
        for (direction, expected_head) in [
            (Move::Up, Position::new(5, 6)),
            (Move::Down, Position::new(5, 4)),
            (Move::Left, Position::new(4, 5)),
            (Move::Right, Position::new(6, 5)),
        ] {
            let mut game = create_test_game(1);
            game.board.snakes[0].head = Position::new(5, 5);
            game.board.snakes[0].body = VecDeque::from([
                Position::new(5, 5),
                Position::new(5, 4),
                Position::new(5, 3),
            ]);

            let moves = vec![("snake-0".to_string(), direction)];
            let game = apply_turn(game, &moves);

            assert_eq!(
                game.board.snakes[0].head, expected_head,
                "Failed for direction {:?}",
                direction
            );
        }
    }

    #[test]
    fn test_apply_turn_updates_you() {
        let mut game = create_test_game(1);
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(5, 3),
        ]);
        game.you = game.board.snakes[0].clone();

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // "you" should be updated to match the snake in board.snakes
        assert_eq!(game.you.head, Position::new(5, 6));
        assert_eq!(game.you.health, 99);
    }

    #[test]
    fn test_apply_turn_default_move() {
        // If no move provided for a snake, it should default to Up
        let mut game = create_test_game(1);
        game.board.snakes[0].head = Position::new(5, 5);
        game.board.snakes[0].body = VecDeque::from([
            Position::new(5, 5),
            Position::new(5, 4),
            Position::new(5, 3),
        ]);

        let moves: Vec<(String, Move)> = vec![]; // No moves provided
        let game = apply_turn(game, &moves);

        // Should default to Up
        assert_eq!(game.board.snakes[0].head, Position::new(5, 6));
    }

    #[test]
    fn test_max_turns_constant() {
        assert_eq!(MAX_TURNS, 5000);
    }

    #[test]
    fn test_dead_snake_doesnt_move() {
        let mut game = create_test_game(1);
        game.board.snakes[0].health = 0; // Already dead
        let original_head = game.board.snakes[0].head;
        let original_body = game.board.snakes[0].body.clone();

        let moves = vec![("snake-0".to_string(), Move::Up)];
        let game = apply_turn(game, &moves);

        // Dead snake shouldn't move (head and body unchanged)
        assert_eq!(game.board.snakes[0].health, 0);
        assert_eq!(game.board.snakes[0].head, original_head);
        assert_eq!(game.board.snakes[0].body, original_body);
    }

    fn create_test_game(num_snakes: usize) -> Game {
        let snakes: Vec<BattleSnake> = (0..num_snakes)
            .map(|i| BattleSnake {
                id: format!("snake-{}", i),
                name: format!("Snake {}", i),
                head: Position::new(i as i32 * 2 + 1, i as i32 * 2 + 1),
                body: VecDeque::from([Position::new(i as i32 * 2 + 1, i as i32 * 2 + 1); 3]),
                health: 100,
                shout: None,
                actual_length: None,
            })
            .collect();

        Game {
            you: snakes[0].clone(),
            board: Board {
                height: 11,
                width: 11,
                food: vec![Position::new(5, 5)],
                snakes,
                hazards: vec![],
            },
            turn: 0,
            game: NestedGame {
                id: "test-game".to_string(),
                ruleset: Ruleset {
                    name: "standard".to_string(),
                    version: "v1.0.0".to_string(),
                    settings: None,
                },
                timeout: 500,
                map: None,
                source: None,
            },
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
