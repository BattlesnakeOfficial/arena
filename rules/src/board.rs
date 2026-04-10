use rand::Rng;
use rand::seq::SliceRandom;
use std::collections::HashSet;

use crate::types::*;

/// Equivalent to Go's `CreateDefaultBoardState`.
///
/// Creates a board with snakes placed at fixed spawn positions and
/// initial food placed using the fixed algorithm.
pub fn create_default_board_state(
    rng: &mut impl Rng,
    width: i32,
    height: i32,
    snake_ids: &[String],
) -> Result<BoardState, RulesError> {
    let mut board = BoardState {
        turn: 0,
        width,
        height,
        food: Vec::new(),
        snakes: Vec::new(),
        hazards: Vec::new(),
    };

    place_snakes_fixed(rng, &mut board, snake_ids);
    place_food_fixed(rng, &mut board)?;

    Ok(board)
}

/// Equivalent to Go's `isSquareBoard`.
pub fn is_square_board(board: &BoardState) -> bool {
    board.width == board.height
}

/// Equivalent to Go's `EliminateSnake` in `board.go:595`.
pub fn eliminate_snake(snake: &mut Snake, cause: EliminationCause, by: &str, turn: i32) {
    snake.eliminated_cause = cause;
    snake.eliminated_by = by.to_string();
    snake.eliminated_on_turn = turn;
}

/// Get all board points not currently occupied.
///
/// - `include_possible_moves=false`: 4 squares adjacent to each alive snake head
///   are ALSO considered occupied
/// - `include_hazards=true`: hazard squares are marked occupied
///
/// Food + alive snake body are ALWAYS occupied.
pub fn get_unoccupied_points(
    board: &BoardState,
    include_possible_moves: bool,
    include_hazards: bool,
) -> Vec<Point> {
    let mut occupied: HashSet<Point> = HashSet::new();

    // All food is occupied
    for f in &board.food {
        occupied.insert(*f);
    }

    // All alive snake bodies are occupied
    for snake in &board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }
        for p in &snake.body {
            occupied.insert(*p);
        }
        // If not including possible moves, adjacent-to-head squares are also occupied
        if !include_possible_moves {
            let head = snake.head();
            occupied.insert(Point::new(head.x, head.y + 1));
            occupied.insert(Point::new(head.x, head.y - 1));
            occupied.insert(Point::new(head.x - 1, head.y));
            occupied.insert(Point::new(head.x + 1, head.y));
        }
    }

    // If including hazards as occupied
    if include_hazards {
        for h in &board.hazards {
            occupied.insert(*h);
        }
    }

    let mut unoccupied = Vec::new();
    for y in 0..board.height {
        for x in 0..board.width {
            let p = Point::new(x, y);
            if !occupied.contains(&p) {
                unoccupied.push(p);
            }
        }
    }

    unoccupied
}

/// Unoccupied points on even coords (`(x+y) % 2 == 0`).
///
/// Calls `get_unoccupied_points(board, true, false)` then filters.
pub fn get_even_unoccupied_points(board: &BoardState) -> Vec<Point> {
    get_unoccupied_points(board, true, false)
        .into_iter()
        .filter(|p| (p.x + p.y) % 2 == 0)
        .collect()
}

/// Place snakes at fixed spawn positions (`PlaceSnakesFixed`).
///
/// - `mn=1`, `md=(width-1)/2`, `mx=width-2`
/// - 4 corner points: `(mn,mn), (mn,mx), (mx,mn), (mx,mx)`
/// - 4 cardinal points: `(mn,md), (md,mn), (md,mx), (mx,md)`
/// - Shuffle both lists, coin-flip which comes first
/// - Take `num_snakes` positions from combined list
/// - Each snake: body = 3 copies of spawn point, health = 100
fn place_snakes_fixed(rng: &mut impl Rng, board: &mut BoardState, snake_ids: &[String]) {
    let mn = 1;
    let md = (board.width - 1) / 2;
    let mx = board.width - 2;

    let mut corner_points = vec![
        Point::new(mn, mn),
        Point::new(mn, mx),
        Point::new(mx, mn),
        Point::new(mx, mx),
    ];

    let mut cardinal_points = vec![
        Point::new(mn, md),
        Point::new(md, mn),
        Point::new(md, mx),
        Point::new(mx, md),
    ];

    corner_points.shuffle(rng);
    cardinal_points.shuffle(rng);

    let start_points = if rng.gen_bool(0.5) {
        let mut points = corner_points;
        points.extend(cardinal_points);
        points
    } else {
        let mut points = cardinal_points;
        points.extend(corner_points);
        points
    };

    for (i, id) in snake_ids.iter().enumerate() {
        if i >= start_points.len() {
            break;
        }
        let pos = start_points[i];
        let body = vec![pos; SNAKE_START_SIZE];
        board.snakes.push(Snake {
            id: id.clone(),
            body,
            health: SNAKE_MAX_HEALTH,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        });
    }
}

/// Place initial food using the fixed algorithm (`PlaceFoodFixed`).
///
/// Phase 1 — per-snake food (conditional):
/// - `is_small_board = width * height < BOARD_SIZE_MEDIUM * BOARD_SIZE_MEDIUM` (area 121)
/// - If `num_snakes <= 4 || !is_small_board`: place 1 food per snake at a diagonal from head
/// - If `num_snakes > 4 && is_small_board`: skip per-snake food entirely
///
/// Phase 2 — center food (always):
/// - Place food at center if unoccupied
fn place_food_fixed(rng: &mut impl Rng, board: &mut BoardState) -> Result<(), RulesError> {
    let num_snakes = board.snakes.len();
    let is_small_board = board.width * board.height < BOARD_SIZE_MEDIUM * BOARD_SIZE_MEDIUM;
    let center = Point::new((board.width - 1) / 2, (board.height - 1) / 2);

    // Phase 1: per-snake food
    if num_snakes <= 4 || !is_small_board {
        // Collect snake heads first to avoid borrow issues
        let snake_heads: Vec<Point> = board.snakes.iter().map(|s| s.head()).collect();

        for head in snake_heads {
            let diagonals = [
                Point::new(head.x - 1, head.y - 1),
                Point::new(head.x - 1, head.y + 1),
                Point::new(head.x + 1, head.y - 1),
                Point::new(head.x + 1, head.y + 1),
            ];

            let valid: Vec<Point> = diagonals
                .iter()
                .filter(|p| {
                    // Not the center
                    if **p == center {
                        return false;
                    }
                    // Not already food
                    if board.food.contains(p) {
                        return false;
                    }
                    // "Away from center" on at least one axis (strict ordering)
                    let away = (p.x < head.x && head.x < center.x)
                        || (center.x < head.x && head.x < p.x)
                        || (p.y < head.y && head.y < center.y)
                        || (center.y < head.y && head.y < p.y);
                    if !away {
                        return false;
                    }
                    // Not a corner of the board
                    let is_corner = (p.x == 0 || p.x == board.width - 1)
                        && (p.y == 0 || p.y == board.height - 1);
                    !is_corner
                })
                .copied()
                .collect();

            if valid.is_empty() {
                return Err(RulesError::NoRoomForFood);
            }

            let chosen = valid[rng.gen_range(0..valid.len())];
            board.food.push(chosen);
        }
    }

    // Phase 2: center food (always)
    let unoccupied = get_unoccupied_points(board, true, false);
    if unoccupied.contains(&center) {
        board.food.push(center);
    } else {
        return Err(RulesError::NoRoomForFood);
    }

    Ok(())
}
