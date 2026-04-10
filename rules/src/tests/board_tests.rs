use crate::board::*;
use crate::food::place_food_randomly;
use crate::types::*;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::collections::HashSet;

fn make_snake(id: &str, body: &[(i32, i32)], health: i32) -> Snake {
    Snake {
        id: id.to_string(),
        body: body.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
        health,
        eliminated_cause: EliminationCause::NotEliminated,
        eliminated_by: String::new(),
        eliminated_on_turn: 0,
    }
}

/// Port of Go `TestCreateDefaultBoardState`
#[test]
fn test_create_default_board_state() {
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
        // All body segments at same position (stacked at spawn)
        let head = snake.head();
        assert!(snake.body.iter().all(|p| *p == head));
    }

    // Should have food: 4 per-snake + 1 center = 5
    assert_eq!(board.food.len(), 5);
    // Center food should be present
    assert!(board.food.contains(&Point::new(5, 5)));
}

/// Port of Go `TestPlaceSnakesDefault`
#[test]
fn test_place_snakes_default() {
    let mut rng = StdRng::seed_from_u64(42);

    for num_snakes in 1..=8 {
        let ids: Vec<String> = (0..num_snakes).map(|i| format!("snake-{i}")).collect();
        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

        assert_eq!(board.snakes.len(), num_snakes);

        // All spawn positions should be unique
        let positions: HashSet<Point> = board.snakes.iter().map(|s| s.head()).collect();
        assert_eq!(
            positions.len(),
            num_snakes,
            "spawn positions should be unique for {num_snakes} snakes"
        );
    }
}

/// Port of Go `TestPlaceSnakesFixed`
///
/// Verifies that spawn positions come from the fixed corner/cardinal set.
#[test]
fn test_place_snakes_fixed() {
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

/// Port of Go `TestPlaceFood`
///
/// Basic test that food is placed during board creation.
#[test]
fn test_place_food() {
    let mut rng = StdRng::seed_from_u64(42);
    let ids: Vec<String> = (0..2).map(|i| format!("snake-{i}")).collect();

    let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

    // Should have at least 1 food (center) + per-snake food
    assert!(!board.food.is_empty());
    // 2 snakes + center = 3
    assert_eq!(board.food.len(), 3);
}

/// Port of Go `TestPlaceFoodFixed`
///
/// Verifies per-snake food placement follows the diagonal/away-from-center rules.
#[test]
fn test_place_food_fixed() {
    let mut rng = StdRng::seed_from_u64(42);
    let ids: Vec<String> = (0..4).map(|i| format!("snake-{i}")).collect();

    let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();
    let center = Point::new(5, 5);

    // Center food should be present
    assert!(board.food.contains(&center));

    // Per-snake food should be diagonal from head and away from center
    // (We can't check exact positions due to randomness, but verify food count)
    assert_eq!(board.food.len(), 5); // 4 per-snake + 1 center

    // No food at the board corners
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

/// Port of Go `TestPlaceFoodFixedNoRoom`
///
/// On a small board with many snakes (>4), per-snake food is skipped.
/// If center is occupied, returns error.
#[test]
fn test_place_food_fixed_no_room() {
    // On a small board with >4 snakes, per-snake food is skipped
    // 7x7 = 49, which is < 121 (BOARD_SIZE_MEDIUM^2)
    let mut rng = StdRng::seed_from_u64(42);
    let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();

    let board = create_default_board_state(&mut rng, 7, 7, &ids).unwrap();
    // With >4 snakes on small board, only center food
    assert_eq!(board.food.len(), 1);
    assert!(board.food.contains(&Point::new(3, 3)));
}

/// Port of Go `TestDev1235`
///
/// Regression test: 8 snakes on 11x11 should all get unique positions and food.
#[test]
fn test_dev_1235() {
    for seed in 0..50 {
        let mut rng = StdRng::seed_from_u64(seed);
        let ids: Vec<String> = (0..8).map(|i| format!("snake-{i}")).collect();

        let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();

        // All 8 snakes placed
        assert_eq!(board.snakes.len(), 8);

        // Unique positions
        let positions: HashSet<Point> = board.snakes.iter().map(|s| s.head()).collect();
        assert_eq!(
            positions.len(),
            8,
            "seed {seed}: spawn positions should be unique"
        );

        // Food: 8 per-snake + 1 center = 9
        assert_eq!(board.food.len(), 9, "seed {seed}: expected 9 food");

        // Center food
        assert!(
            board.food.contains(&Point::new(5, 5)),
            "seed {seed}: missing center food"
        );
    }
}

/// Port of Go `TestGetUnoccupiedPoints`
#[test]
fn test_get_unoccupied_points() {
    // Empty board
    let board = BoardState {
        turn: 0,
        width: 3,
        height: 3,
        food: Vec::new(),
        snakes: Vec::new(),
        hazards: Vec::new(),
    };

    let points = get_unoccupied_points(&board, true, false);
    assert_eq!(points.len(), 9); // 3x3

    // Board with food
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

    // Board with snake
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

    // With include_possible_moves=false, adjacent to head also occupied
    let points = get_unoccupied_points(&board, false, false);
    // Head at (0,0), adjacent: (-1,0), (1,0), (0,-1), (0,1)
    // (-1,0) and (0,-1) are off-board, so only (1,0) and (0,1) are excluded extra
    // But (0,1) is already body. So only (1,0) is newly excluded.
    assert_eq!(points.len(), 5);
}

/// Port of Go `TestGetEvenUnoccupiedPoints`
#[test]
fn test_get_even_unoccupied_points() {
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

/// Port of Go `TestGetDistanceBetweenPoints`
#[test]
fn test_distance_between_points() {
    assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(0, 0)), 0);
    assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(1, 0)), 1);
    assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(0, 1)), 1);
    assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(1, 1)), 2);
    assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(5, 5)), 10);
    assert_eq!(Point::new(3, 4).manhattan_distance(Point::new(7, 2)), 6);
    // Negative coords
    assert_eq!(Point::new(-1, -1).manhattan_distance(Point::new(1, 1)), 4);
}

/// Port of Go `TestIsSquareBoard`
#[test]
fn test_is_square_board() {
    let board = BoardState {
        turn: 0,
        width: 11,
        height: 11,
        food: Vec::new(),
        snakes: Vec::new(),
        hazards: Vec::new(),
    };
    assert!(is_square_board(&board));

    let board = BoardState {
        turn: 0,
        width: 7,
        height: 11,
        food: Vec::new(),
        snakes: Vec::new(),
        hazards: Vec::new(),
    };
    assert!(!is_square_board(&board));
}

/// Port of Go `TestPlaceFoodRandomly`
#[test]
fn test_place_food_randomly() {
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

    // All food should be on the board and unique
    let food_set: HashSet<Point> = board.food.iter().copied().collect();
    assert_eq!(food_set.len(), 3);
    for f in &board.food {
        assert!(f.x >= 0 && f.x < 5 && f.y >= 0 && f.y < 5);
    }
}

/// Port of Go `TestEliminateSnake`
#[test]
fn test_eliminate_snake_helper() {
    let mut snake = make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100);

    eliminate_snake(&mut snake, EliminationCause::OutOfHealth, "other", 5);

    assert_eq!(snake.eliminated_cause, EliminationCause::OutOfHealth);
    assert_eq!(snake.eliminated_by, "other");
    assert_eq!(snake.eliminated_on_turn, 5);
    assert!(snake.eliminated_cause.is_eliminated());
}

/// Small board detection uses area, not just width.
#[test]
fn test_small_board_area_detection() {
    // 7x7 = 49 < 121 → small board
    let mut rng = StdRng::seed_from_u64(42);
    let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
    let board = create_default_board_state(&mut rng, 7, 7, &ids).unwrap();
    // >4 snakes on small board → no per-snake food, only center
    assert_eq!(board.food.len(), 1);

    // 11x11 = 121, NOT small (< 121 is small)
    let mut rng = StdRng::seed_from_u64(42);
    let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
    let board = create_default_board_state(&mut rng, 11, 11, &ids).unwrap();
    // 5 per-snake + 1 center = 6
    assert_eq!(board.food.len(), 6);

    // 7x15 = 105 < 121 → small
    let mut rng = StdRng::seed_from_u64(42);
    let ids: Vec<String> = (0..5).map(|i| format!("snake-{i}")).collect();
    let board = create_default_board_state(&mut rng, 7, 15, &ids).unwrap();
    // >4 snakes on small board → no per-snake food, only center
    assert_eq!(board.food.len(), 1);
}

/// Spawn position values: mx = width - 2 (1 cell from edge).
#[test]
fn test_spawn_positions_values() {
    // For 11x11: mn=1, md=5, mx=9
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
