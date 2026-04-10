use crate::board::eliminate_snake;
use crate::food::maybe_spawn_food;
use crate::standard::*;
use crate::types::*;
use rand::SeedableRng;
use rand::rngs::StdRng;

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

fn make_board(width: i32, height: i32, snakes: Vec<Snake>) -> BoardState {
    BoardState {
        turn: 0,
        width,
        height,
        food: Vec::new(),
        snakes,
        hazards: Vec::new(),
    }
}

/// Port of Go `TestSanity`
#[test]
fn test_sanity() {
    // Two alive snakes => not game over
    let board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(8, 8), (8, 7), (8, 6)], 100),
        ],
    );
    assert!(!is_game_over(&board));

    // Single alive snake => game over (standard rules: ≤1 alive)
    let board2 = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    assert!(is_game_over(&board2));

    // No snakes => game over
    let board3 = make_board(11, 11, vec![]);
    assert!(is_game_over(&board3));
}

/// Port of Go `TestStandardCreateNextBoardState` — parameterized cases.
///
/// Tests full turn execution with various scenarios.
#[test]
fn test_standard_cases() {
    let settings = StandardSettings::default();

    // "bystander" snake keeps the game alive (≥2 snakes needed)
    let bystander = make_snake("bystander", &[(0, 0), (0, 1), (0, 2)], 100);

    // Case 1: snake moves and loses health
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            bystander.clone(),
        ],
    );
    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Up,
        },
        SnakeMove {
            id: "bystander".to_string(),
            direction: Direction::Down,
        },
    ];
    let game_over = execute_turn(&mut board, &moves, &settings).unwrap();
    assert!(!game_over);
    assert_eq!(board.snakes[0].health, 99);
    assert_eq!(board.snakes[0].head(), Point::new(5, 6));

    // Case 2: snake eats food, health restored
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 50),
            bystander.clone(),
        ],
    );
    board.food.push(Point::new(5, 6));
    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Up,
        },
        SnakeMove {
            id: "bystander".to_string(),
            direction: Direction::Down,
        },
    ];
    let game_over = execute_turn(&mut board, &moves, &settings).unwrap();
    assert!(!game_over);
    assert_eq!(board.snakes[0].health, 100);
    // Grew by 1
    assert_eq!(board.snakes[0].body.len(), 4);
    assert!(board.food.is_empty());

    // Case 3: snake starves
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 1),
            bystander.clone(),
        ],
    );
    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Up,
        },
        SnakeMove {
            id: "bystander".to_string(),
            direction: Direction::Down,
        },
    ];
    let game_over = execute_turn(&mut board, &moves, &settings).unwrap();
    assert!(!game_over);
    assert!(board.snakes[0].eliminated_cause.is_eliminated());
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfHealth
    );

    // Case 4: snake goes out of bounds
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(0, 5), (1, 5), (2, 5)], 100),
            bystander.clone(),
        ],
    );
    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Left,
        },
        SnakeMove {
            id: "bystander".to_string(),
            direction: Direction::Down,
        },
    ];
    let game_over = execute_turn(&mut board, &moves, &settings).unwrap();
    assert!(!game_over);
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfBounds
    );
}

/// Port of Go `TestEatingOnLastMove`
#[test]
fn test_eating_on_last_move() {
    let settings = StandardSettings::default();

    // Snake at health 1, food at next position — should eat BEFORE elimination
    // (feed_snakes runs before eliminate_snakes in pipeline)
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 1),
            make_snake("bystander", &[(0, 0), (0, 1), (0, 2)], 100),
        ],
    );
    board.food.push(Point::new(5, 6));
    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Up,
        },
        SnakeMove {
            id: "bystander".to_string(),
            direction: Direction::Down,
        },
    ];

    let game_over = execute_turn(&mut board, &moves, &settings).unwrap();
    assert!(!game_over);
    // Health reduced to 0, then restored to 100 by feeding
    assert_eq!(board.snakes[0].health, 100);
    assert!(!board.snakes[0].eliminated_cause.is_eliminated());
    assert_eq!(board.snakes[0].body.len(), 4);
}

/// Port of Go `TestHeadToHeadOnFood`
#[test]
fn test_head_to_head_on_food() {
    let settings = StandardSettings::default();

    // Two equal-length snakes collide head-to-head on food
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(4, 5), (3, 5), (2, 5)], 100),
            make_snake("two", &[(6, 5), (7, 5), (8, 5)], 100),
        ],
    );
    board.food.push(Point::new(5, 5));

    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Right,
        },
        SnakeMove {
            id: "two".to_string(),
            direction: Direction::Left,
        },
    ];

    let game_over = execute_turn(&mut board, &moves, &settings).unwrap();
    assert!(!game_over);

    // Both should eat the food (grow + heal) but then die by head-to-head
    // because equal length (both grew to 4, still equal)
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::HeadToHeadCollision
    );
    assert_eq!(
        board.snakes[1].eliminated_cause,
        EliminationCause::HeadToHeadCollision
    );
    assert!(board.food.is_empty());
}

/// Port of Go `TestRegressionIssue19`
///
/// A snake eating food on its last move should survive — the pipeline order
/// ensures feeding happens before elimination.
#[test]
fn test_regression_issue_19() {
    let settings = StandardSettings::default();

    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 1),
            make_snake("bystander", &[(0, 0), (0, 1), (0, 2)], 100),
        ],
    );
    board.food.push(Point::new(5, 6));

    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Up,
        },
        SnakeMove {
            id: "bystander".to_string(),
            direction: Direction::Down,
        },
    ];

    execute_turn(&mut board, &moves, &settings).unwrap();

    assert!(!board.snakes[0].eliminated_cause.is_eliminated());
    assert_eq!(board.snakes[0].health, 100);
}

/// Port of Go `TestMoveSnakes`
#[test]
fn test_move_snakes() {
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );

    let moves = vec![SnakeMove {
        id: "one".to_string(),
        direction: Direction::Up,
    }];

    move_snakes(&mut board, &moves).unwrap();
    assert_eq!(board.snakes[0].head(), Point::new(5, 6));
    assert_eq!(board.snakes[0].body.len(), 3);
    assert_eq!(
        board.snakes[0].body,
        vec![Point::new(5, 6), Point::new(5, 5), Point::new(5, 4),]
    );
}

/// Port of Go `TestMoveSnakesWrongID`
#[test]
fn test_move_snakes_wrong_id() {
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );

    let moves = vec![SnakeMove {
        id: "wrong".to_string(),
        direction: Direction::Up,
    }];

    let result = move_snakes(&mut board, &moves);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        RulesError::NoMoveFound("one".to_string())
    );
}

/// Port of Go `TestMoveSnakesNotEnoughMoves`
#[test]
fn test_move_snakes_not_enough_moves() {
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(8, 8), (8, 7), (8, 6)], 100),
        ],
    );

    let moves = vec![SnakeMove {
        id: "one".to_string(),
        direction: Direction::Up,
    }];

    let result = move_snakes(&mut board, &moves);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        RulesError::NoMoveFound("two".to_string())
    );
}

/// Port of Go `TestMoveSnakesExtraMovesIgnored`
#[test]
fn test_move_snakes_extra_moves_ignored() {
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );

    let moves = vec![
        SnakeMove {
            id: "one".to_string(),
            direction: Direction::Up,
        },
        SnakeMove {
            id: "extra".to_string(),
            direction: Direction::Down,
        },
    ];

    move_snakes(&mut board, &moves).unwrap();
    assert_eq!(board.snakes[0].head(), Point::new(5, 6));
}

/// Port of Go `TestMoveSnakesDefault` — test all 4 directions.
#[test]
fn test_move_snakes_all_directions() {
    let directions = [
        (Direction::Up, (5, 6)),
        (Direction::Down, (5, 4)),
        (Direction::Left, (4, 5)),
        (Direction::Right, (6, 5)),
    ];

    for (dir, (expected_x, expected_y)) in directions {
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        let moves = vec![SnakeMove {
            id: "one".to_string(),
            direction: dir,
        }];
        move_snakes(&mut board, &moves).unwrap();
        assert_eq!(
            board.snakes[0].head(),
            Point::new(expected_x, expected_y),
            "direction {dir:?} should produce head at ({expected_x}, {expected_y})"
        );
    }
}

/// Port of Go `TestReduceSnakeHealth`
#[test]
fn test_reduce_snake_health() {
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(8, 8), (8, 7), (8, 6)], 50),
        ],
    );

    reduce_snake_health(&mut board);
    assert_eq!(board.snakes[0].health, 99);
    assert_eq!(board.snakes[1].health, 49);

    // Eliminated snakes are untouched
    eliminate_snake(&mut board.snakes[1], EliminationCause::OutOfBounds, "", 0);
    reduce_snake_health(&mut board);
    assert_eq!(board.snakes[0].health, 98);
    assert_eq!(board.snakes[1].health, 49); // unchanged
}

/// Port of Go `TestSnakeIsOutOfHealth`
#[test]
fn test_snake_is_out_of_health() {
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 0)],
    );

    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfHealth
    );
}

/// Port of Go `TestSnakeIsOutOfBounds`
#[test]
fn test_snake_is_out_of_bounds() {
    // Head out of bounds (left)
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(-1, 5), (0, 5), (1, 5)], 100)],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfBounds
    );

    // Head out of bounds (right)
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(11, 5), (10, 5), (9, 5)], 100)],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfBounds
    );

    // Head out of bounds (down)
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, -1), (5, 0), (5, 1)], 100)],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfBounds
    );

    // Head out of bounds (up)
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 11), (5, 10), (5, 9)], 100)],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfBounds
    );

    // Body segment out of bounds (tail hanging off edge)
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(0, 0), (-1, 0), (-2, 0)], 100)],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfBounds
    );
}

/// Port of Go `TestSnakeHasBodyCollidedSelf`
#[test]
fn test_snake_self_collision() {
    // Snake coiled on itself
    let mut board = make_board(
        11,
        11,
        vec![make_snake(
            "one",
            &[(5, 5), (5, 6), (6, 6), (6, 5), (5, 5)],
            100,
        )],
    );

    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::SelfCollision
    );
    assert_eq!(board.snakes[0].eliminated_by, "one");
}

/// Port of Go `TestSnakeHasBodyCollidedOther`
#[test]
fn test_snake_body_collision() {
    let mut board = make_board(
        11,
        11,
        vec![
            // "one" has head at (5,5), which is on "two"'s body
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(5, 6), (5, 5), (5, 4)], 100),
        ],
    );

    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::Collision
    );
    assert_eq!(board.snakes[0].eliminated_by, "two");
    // "two" is fine
    assert!(!board.snakes[1].eliminated_cause.is_eliminated());
}

/// Port of Go `TestSnakeHasLostHeadToHead`
#[test]
fn test_snake_head_to_head() {
    // Equal length: both eliminated
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(5, 5), (6, 5), (7, 5)], 100),
        ],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::HeadToHeadCollision
    );
    assert_eq!(
        board.snakes[1].eliminated_cause,
        EliminationCause::HeadToHeadCollision
    );

    // Smaller snake loses
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("small", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("big", &[(5, 5), (6, 5), (7, 5), (8, 5)], 100),
        ],
    );
    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::HeadToHeadCollision
    );
    assert_eq!(board.snakes[0].eliminated_by, "big");
    assert!(!board.snakes[1].eliminated_cause.is_eliminated());
}

/// Port of Go `TestMaybeEliminateSnakes`
#[test]
fn test_eliminate_snakes() {
    // Multiple simultaneous eliminations
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("out_of_health", &[(5, 5), (5, 4), (5, 3)], 0),
            make_snake("out_of_bounds", &[(-1, 5), (0, 5), (1, 5)], 100),
            make_snake("alive", &[(8, 8), (8, 7), (8, 6)], 100),
        ],
    );

    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::OutOfHealth
    );
    assert_eq!(
        board.snakes[1].eliminated_cause,
        EliminationCause::OutOfBounds
    );
    assert!(!board.snakes[2].eliminated_cause.is_eliminated());
}

/// Port of Go `TestMaybeEliminateSnakesPriority`
///
/// Self-collision takes priority over body collision and head-to-head.
#[test]
fn test_eliminate_snakes_priority() {
    // Snake "one" has self-collision AND would also collide with "two"
    // Self-collision should be the cause
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 6), (6, 6), (6, 5), (5, 5)], 100),
            make_snake("two", &[(5, 5), (4, 5), (3, 5)], 100),
        ],
    );

    eliminate_snakes(&mut board).unwrap();
    assert_eq!(
        board.snakes[0].eliminated_cause,
        EliminationCause::SelfCollision
    );
    assert_eq!(board.snakes[0].eliminated_by, "one");
}

/// Port of Go `TestMaybeDamageHazards`
#[test]
fn test_damage_hazards() {
    let settings = StandardSettings {
        hazard_damage_per_turn: 14,
        ..StandardSettings::default()
    };

    // Snake head on hazard, no food
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    board.hazards.push(Point::new(5, 5));

    damage_hazards(&mut board, &settings);
    assert_eq!(board.snakes[0].health, 86); // 100 - 14

    // Snake head on hazard with food — no damage
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    board.hazards.push(Point::new(5, 5));
    board.food.push(Point::new(5, 5));

    damage_hazards(&mut board, &settings);
    assert_eq!(board.snakes[0].health, 100); // no damage

    // Snake body on hazard but not head — no damage
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    board.hazards.push(Point::new(5, 4));

    damage_hazards(&mut board, &settings);
    assert_eq!(board.snakes[0].health, 100); // no damage
}

/// Port of Go `TestHazardDamagePerTurn`
#[test]
fn test_hazard_damage_per_turn() {
    // Custom damage per turn
    let settings = StandardSettings {
        hazard_damage_per_turn: 50,
        ..StandardSettings::default()
    };

    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    board.hazards.push(Point::new(5, 5));

    damage_hazards(&mut board, &settings);
    assert_eq!(board.snakes[0].health, 50);

    // Stacked hazards (same coord twice) apply damage twice
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    board.hazards.push(Point::new(5, 5));
    board.hazards.push(Point::new(5, 5));

    damage_hazards(&mut board, &settings);
    assert_eq!(board.snakes[0].health, 0);
    assert_eq!(board.snakes[0].eliminated_cause, EliminationCause::Hazard);

    // Damage that would go below 0 clamps to 0
    let settings = StandardSettings {
        hazard_damage_per_turn: 200,
        ..StandardSettings::default()
    };
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    board.hazards.push(Point::new(5, 5));

    damage_hazards(&mut board, &settings);
    assert_eq!(board.snakes[0].health, 0);
    assert_eq!(board.snakes[0].eliminated_cause, EliminationCause::Hazard);
}

/// Port of Go `TestMaybeFeedSnakes`
#[test]
fn test_feed_snakes() {
    // Snake eats food
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 50)],
    );
    board.food.push(Point::new(5, 5));

    feed_snakes(&mut board);
    assert_eq!(board.snakes[0].health, 100);
    assert_eq!(board.snakes[0].body.len(), 4); // grew
    assert!(board.food.is_empty());

    // Snake not on food — no change
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 50)],
    );
    board.food.push(Point::new(0, 0));

    feed_snakes(&mut board);
    assert_eq!(board.snakes[0].health, 50);
    assert_eq!(board.snakes[0].body.len(), 3);
    assert_eq!(board.food.len(), 1);

    // Two snakes eating same food — both grow
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 50),
            make_snake("two", &[(5, 5), (6, 5), (7, 5)], 60),
        ],
    );
    board.food.push(Point::new(5, 5));

    feed_snakes(&mut board);
    assert_eq!(board.snakes[0].health, 100);
    assert_eq!(board.snakes[0].body.len(), 4);
    assert_eq!(board.snakes[1].health, 100);
    assert_eq!(board.snakes[1].body.len(), 4);
    assert!(board.food.is_empty());
}

/// Port of Go `TestMaybeSpawnFoodMinimum`
#[test]
fn test_spawn_food_minimum() {
    let settings = StandardSettings {
        minimum_food: 3,
        food_spawn_chance: 0,
        ..StandardSettings::default()
    };

    let mut rng = StdRng::seed_from_u64(42);
    let mut board = make_board(11, 11, vec![]);

    // No food — should spawn up to minimum
    maybe_spawn_food(&mut rng, &mut board, &settings);
    assert_eq!(board.food.len(), 3);

    // Already at minimum — no spawn (chance is 0)
    maybe_spawn_food(&mut rng, &mut board, &settings);
    assert_eq!(board.food.len(), 3);
}

/// Port of Go `TestMaybeSpawnFoodZeroChance`
#[test]
fn test_spawn_food_zero_chance() {
    let settings = StandardSettings {
        food_spawn_chance: 0,
        minimum_food: 0,
        ..StandardSettings::default()
    };

    let mut rng = StdRng::seed_from_u64(42);
    let mut board = make_board(11, 11, vec![]);

    for _ in 0..100 {
        maybe_spawn_food(&mut rng, &mut board, &settings);
    }
    assert_eq!(board.food.len(), 0);
}

/// Port of Go `TestMaybeSpawnFoodHundredChance`
///
/// `food_spawn_chance=100` is 99% per roll (fails when RNG returns 0).
/// Over 100 iterations, expect >= 99 spawns.
#[test]
fn test_spawn_food_hundred_chance() {
    let settings = StandardSettings {
        food_spawn_chance: 100,
        minimum_food: 0,
        ..StandardSettings::default()
    };

    let mut rng = StdRng::seed_from_u64(42);
    let mut board = make_board(100, 100, vec![]);

    for _ in 0..100 {
        maybe_spawn_food(&mut rng, &mut board, &settings);
    }
    // 99% chance per roll — expect at least 95 in practice
    assert!(
        board.food.len() >= 95,
        "expected >= 95 food spawns, got {}",
        board.food.len()
    );
}

/// Port of Go `TestMaybeSpawnFoodHalfChance`
#[test]
fn test_spawn_food_half_chance() {
    let settings = StandardSettings {
        food_spawn_chance: 50,
        minimum_food: 0,
        ..StandardSettings::default()
    };

    let mut rng = StdRng::seed_from_u64(42);
    let mut board = make_board(100, 100, vec![]);

    for _ in 0..1000 {
        maybe_spawn_food(&mut rng, &mut board, &settings);
    }

    // With 50% chance (actually 49% due to the formula), expect roughly 490
    // Board is 100x100 so won't saturate. Allow wide range: 350-650.
    let food_count = board.food.len();
    assert!(
        (350..=650).contains(&food_count),
        "expected ~490 food spawns from 1000 iterations, got {food_count}"
    );
}

/// Port of Go `TestIsGameOver`
#[test]
fn test_is_game_over() {
    // No snakes = game over
    let board = make_board(11, 11, vec![]);
    assert!(is_game_over(&board));

    // One alive snake = game over
    let board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    assert!(is_game_over(&board));

    // Two alive snakes = not game over
    let board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(8, 8), (8, 7), (8, 6)], 100),
        ],
    );
    assert!(!is_game_over(&board));

    // Two snakes but one eliminated = game over
    let mut board = make_board(
        11,
        11,
        vec![
            make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
            make_snake("two", &[(8, 8), (8, 7), (8, 6)], 100),
        ],
    );
    eliminate_snake(&mut board.snakes[1], EliminationCause::OutOfHealth, "", 1);
    assert!(is_game_over(&board));
}

/// Empty moves = no-op (distinct from missing move which errors).
#[test]
fn test_move_snakes_empty_moves() {
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
    );
    let original_head = board.snakes[0].head();

    move_snakes(&mut board, &[]).unwrap();
    assert_eq!(board.snakes[0].head(), original_head);
}

/// Health can go negative in `reduce_snake_health` — no clamping.
#[test]
fn test_reduce_snake_health_goes_negative() {
    let mut board = make_board(
        11,
        11,
        vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 0)],
    );

    reduce_snake_health(&mut board);
    assert_eq!(board.snakes[0].health, -1);

    reduce_snake_health(&mut board);
    assert_eq!(board.snakes[0].health, -2);
}
