use rand::Rng;
use rand::seq::SliceRandom;
use std::collections::HashSet;

use crate::types::*;

/// Create a default board with snakes at fixed spawn positions and initial food.
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

/// Whether the board is square (width == height).
pub fn is_square_board(board: &BoardState) -> bool {
    board.width == board.height
}

/// Mark a snake as eliminated with the given cause, eliminator ID, and turn.
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
pub fn get_even_unoccupied_points(board: &BoardState) -> Vec<Point> {
    get_unoccupied_points(board, true, false)
        .into_iter()
        .filter(|p| (p.x + p.y) % 2 == 0)
        .collect()
}

/// Place snakes at fixed spawn positions.
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

/// Place initial food using the fixed algorithm.
///
/// Phase 1 — per-snake food (conditional):
/// - `is_small_board = width * height < BOARD_SIZE_MEDIUM^2` (area 121)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::food::place_food_randomly;
    use crate::test_utils::make_snake;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::collections::HashSet;

    #[test]
    fn create_default_board_state_basic() {
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..4).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

        assert_eq!(board.width, 11);
        assert_eq!(board.height, 11);
        assert_eq!(board.turn, 0);
        assert_eq!(board.snakes.len(), 4);
        assert!(board.hazards.is_empty());

        for snake in &board.snakes {
            assert_eq!(snake.health, SNAKE_MAX_HEALTH);
            assert_eq!(snake.body.len(), SNAKE_START_SIZE);
            let head = snake.head();
            assert!(snake.body.iter().all(|p| *p == head));
        }

        assert_eq!(board.food.len(), 5); // 4 per-snake + 1 center
        assert!(board.food.contains(&Point::new(5, 5)));
    }

    #[test]
    fn place_snakes_unique_positions() {
        let mut rng = StdRng::seed_from_u64(42);
        for num_snakes in 1..=8 {
            let ids: Vec<String> = (0..num_snakes).map(|i| format!("snake-{i}")).collect();
            let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

            assert_eq!(board.snakes.len(), num_snakes);
            let positions: HashSet<Point> = board.snakes.iter().map(|s| s.head()).collect();
            assert_eq!(
                positions.len(),
                num_snakes,
                "spawn positions should be unique for {num_snakes} snakes"
            );
        }
    }

    #[test]
    fn place_snakes_at_valid_fixed_positions() {
        let mn = 1;
        let md = 5;
        let mx = 9;

        let valid_positions: HashSet<Point> = [
            Point::new(mn, mn),
            Point::new(mn, mx),
            Point::new(mx, mn),
            Point::new(mx, mx),
            Point::new(mn, md),
            Point::new(md, mn),
            Point::new(md, mx),
            Point::new(mx, md),
        ]
        .into_iter()
        .collect();

        for seed in 0..20 {
            let mut rng = StdRng::seed_from_u64(seed);
            let ids: Vec<String> = (0..8).map(|i| format!("snake-{i}")).collect();
            let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

            for snake in &board.snakes {
                assert!(
                    valid_positions.contains(&snake.head()),
                    "snake {} at {:?} is not a valid fixed position",
                    snake.id,
                    snake.head()
                );
            }
        }
    }

    #[test]
    fn initial_food_placement() {
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..2).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

        assert!(!board.food.is_empty());
        assert_eq!(board.food.len(), 3); // 2 per-snake + center
    }

    #[test]
    fn food_placement_not_at_corners() {
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..4).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();
        let center = Point::new(5, 5);

        assert!(board.food.contains(&center));
        assert_eq!(board.food.len(), 5);

        let corners = [
            Point::new(0, 0),
            Point::new(0, 10),
            Point::new(10, 0),
            Point::new(10, 10),
        ];
        for corner in &corners {
            assert!(
                !board.food.contains(corner),
                "food should not be placed at corner {corner:?}"
            );
        }
    }

    #[test]
    fn small_board_skips_per_snake_food() {
        // 7x7 = 49, which is < 121 (area threshold)
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 7, 7, &ids).unwrap();
        // With >4 snakes on small board, only center food
        assert_eq!(board.food.len(), 1);
        assert!(board.food.contains(&Point::new(3, 3)));
    }

    /// Regression test: 8 snakes on 11x11 should all get unique positions and food.
    #[test]
    fn eight_snakes_unique_positions_and_food() {
        for seed in 0..50 {
            let mut rng = StdRng::seed_from_u64(seed);
            let ids: Vec<String> = (0..8).map(|i| format!("snake-{i}")).collect();
            let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

            assert_eq!(board.snakes.len(), 8);

            let positions: HashSet<Point> = board.snakes.iter().map(|s| s.head()).collect();
            assert_eq!(
                positions.len(),
                8,
                "seed {seed}: spawn positions should be unique"
            );

            assert_eq!(board.food.len(), 9, "seed {seed}: expected 9 food");
            assert!(
                board.food.contains(&Point::new(5, 5)),
                "seed {seed}: missing center food"
            );
        }
    }

    #[test]
    fn unoccupied_points_empty_board() {
        let board = BoardState {
            turn: 0,
            width: 3,
            height: 3,
            food: Vec::new(),
            snakes: Vec::new(),
            hazards: Vec::new(),
        };
        let points = get_unoccupied_points(&board, true, false);
        assert_eq!(points.len(), 9);
    }

    #[test]
    fn unoccupied_points_excludes_food() {
        let board = BoardState {
            turn: 0,
            width: 3,
            height: 3,
            food: vec![Point::new(1, 1)],
            snakes: Vec::new(),
            hazards: Vec::new(),
        };
        let points = get_unoccupied_points(&board, true, false);
        assert_eq!(points.len(), 8);
        assert!(!points.contains(&Point::new(1, 1)));
    }

    #[test]
    fn unoccupied_points_excludes_snake_body() {
        let board = BoardState {
            turn: 0,
            width: 3,
            height: 3,
            food: Vec::new(),
            snakes: vec![make_snake("one", &[(0, 0), (0, 1), (0, 2)], 100)],
            hazards: Vec::new(),
        };
        let points = get_unoccupied_points(&board, true, false);
        assert_eq!(points.len(), 6);
    }

    #[test]
    fn unoccupied_points_excludes_possible_moves() {
        let board = BoardState {
            turn: 0,
            width: 3,
            height: 3,
            food: Vec::new(),
            snakes: vec![make_snake("one", &[(0, 0), (0, 1), (0, 2)], 100)],
            hazards: Vec::new(),
        };
        // include_possible_moves=false excludes squares adjacent to head
        let points = get_unoccupied_points(&board, false, false);
        assert_eq!(points.len(), 5);
    }

    #[test]
    fn even_unoccupied_points() {
        let board = BoardState {
            turn: 0,
            width: 3,
            height: 3,
            food: Vec::new(),
            snakes: Vec::new(),
            hazards: Vec::new(),
        };
        let points = get_even_unoccupied_points(&board);
        // Even points where (x+y)%2==0: (0,0),(0,2),(1,1),(2,0),(2,2) = 5
        assert_eq!(points.len(), 5);
        for p in &points {
            assert_eq!((p.x + p.y) % 2, 0, "point {p:?} should have even coords");
        }
    }

    #[test]
    fn square_board_detection() {
        let square = BoardState {
            turn: 0,
            width: 11,
            height: 11,
            food: Vec::new(),
            snakes: Vec::new(),
            hazards: Vec::new(),
        };
        assert!(is_square_board(&square));

        let rect = BoardState {
            turn: 0,
            width: 7,
            height: 11,
            food: Vec::new(),
            snakes: Vec::new(),
            hazards: Vec::new(),
        };
        assert!(!is_square_board(&rect));
    }

    #[test]
    fn eliminate_snake_sets_fields() {
        let mut snake = make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100);
        eliminate_snake(&mut snake, EliminationCause::OutOfHealth, "other", 5);

        assert_eq!(snake.eliminated_cause, EliminationCause::OutOfHealth);
        assert_eq!(snake.eliminated_by, "other");
        assert_eq!(snake.eliminated_on_turn, 5);
        assert!(snake.eliminated_cause.is_eliminated());
    }

    #[test]
    fn place_food_randomly_basic() {
        let mut rng = StdRng::seed_from_u64(42);
        let mut board = BoardState {
            turn: 0,
            width: 5,
            height: 5,
            food: Vec::new(),
            snakes: Vec::new(),
            hazards: Vec::new(),
        };

        place_food_randomly(&mut rng, &mut board, 3);
        assert_eq!(board.food.len(), 3);

        let food_set: HashSet<Point> = board.food.iter().copied().collect();
        assert_eq!(food_set.len(), 3);
        for f in &board.food {
            assert!(f.x >= 0 && f.x < 5 && f.y >= 0 && f.y < 5);
        }
    }

    #[test]
    fn small_board_area_detection() {
        // 7x7 = 49 < 121 -> small board
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 7, 7, &ids).unwrap();
        assert_eq!(board.food.len(), 1);

        // 11x11 = 121, NOT small
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();
        assert_eq!(board.food.len(), 6);

        // 7x15 = 105 < 121 -> small
        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 7, 15, &ids).unwrap();
        assert_eq!(board.food.len(), 1);
    }

    #[test]
    fn spawn_position_values() {
        let valid: HashSet<Point> = [
            Point::new(1, 1),
            Point::new(1, 9),
            Point::new(9, 1),
            Point::new(9, 9),
            Point::new(1, 5),
            Point::new(5, 1),
            Point::new(5, 9),
            Point::new(9, 5),
        ]
        .into_iter()
        .collect();

        let mut rng = StdRng::seed_from_u64(42);
        let ids: Vec<String> = (0..8).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

        for snake in &board.snakes {
            assert!(
                valid.contains(&snake.head()),
                "spawn at {:?} not valid",
                snake.head()
            );
        }
    }
}
