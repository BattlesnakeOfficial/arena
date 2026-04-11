use crate::board::eliminate_snake;
use crate::types::*;

/// Go: `GameOverStandard`. 0 or 1 alive snakes = game over.
pub fn is_game_over(board: &BoardState) -> bool {
    let alive = board
        .snakes
        .iter()
        .filter(|s| !s.eliminated_cause.is_eliminated())
        .count();
    alive <= 1
}

/// Go: `MoveSnakesStandard`.
///
/// - Empty `moves` slice: no-op (returns `Ok(())`), even if alive snakes exist
/// - Validates all non-eliminated snakes have non-empty body and a matching move entry
/// - Applies: new head = old head + delta, insert at front, pop tail
/// - Extra moves for unknown IDs are silently ignored
/// - Eliminated snakes are not moved
pub fn move_snakes(board: &mut BoardState, moves: &[SnakeMove]) -> Result<(), RulesError> {
    if moves.is_empty() {
        return Ok(());
    }

    for snake in &mut board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }

        if snake.body.is_empty() {
            return Err(RulesError::ZeroLengthSnake(snake.id.clone()));
        }

        let snake_move = moves.iter().find(|m| m.id == snake.id);
        let Some(snake_move) = snake_move else {
            return Err(RulesError::NoMoveFound(snake.id.clone()));
        };

        let head = snake.head();
        let (dx, dy) = snake_move.direction.to_delta();
        let new_head = Point::new(head.x + dx, head.y + dy);

        snake.body.insert(0, new_head);
        snake.body.pop();
    }

    Ok(())
}

/// Go: `ReduceSnakeHealthStandard`.
///
/// Health decrements by 1. DO NOT clamp — health can go negative.
/// Eliminated snakes untouched.
pub fn reduce_snake_health(board: &mut BoardState) {
    for snake in &mut board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }
        snake.health -= 1;
    }
}

/// Go: `DamageHazardsStandard`.
///
/// Iterates EVERY ENTRY in `board.hazards` (including duplicates — stacked hazards
/// apply N times). For each non-eliminated snake, for each hazard point: if snake's
/// HEAD matches and no food at that point, apply damage. Clamps health to
/// `[0, SNAKE_MAX_HEALTH]`. Eliminates with `EliminationCause::Hazard` if health
/// reaches 0. Does NOT break after elimination.
pub fn damage_hazards(board: &mut BoardState, settings: &StandardSettings) {
    // Snapshot hazards and food to avoid borrow issues
    let hazards: Vec<Point> = board.hazards.clone();
    let food: Vec<Point> = board.food.clone();

    for snake in &mut board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }

        let head = snake.head();

        for hazard in &hazards {
            if head != *hazard {
                continue;
            }

            // Food on hazard tile negates damage for this entry
            if food.contains(&head) {
                continue;
            }

            snake.health -= settings.hazard_damage_per_turn;
            snake.health = snake.health.clamp(0, SNAKE_MAX_HEALTH);

            if snake.health == 0 {
                eliminate_snake(snake, EliminationCause::Hazard, "", board.turn + 1);
                // Do NOT break — continue inner loop for Go parity
            }
        }
    }
}

/// Go: `FeedSnakesStandard`.
///
/// For each non-eliminated snake whose head is on food:
///   - grow: push last body element again (tail duplicate)
///   - set health = `SNAKE_MAX_HEALTH` (100)
///
/// Remove eaten food from `board.food`.
/// Multiple snakes CAN eat the same food tile (both grow/heal).
pub fn feed_snakes(board: &mut BoardState) {
    let food_set: std::collections::HashSet<Point> = board.food.iter().copied().collect();
    let mut eaten: std::collections::HashSet<Point> = std::collections::HashSet::new();

    for snake in &mut board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }

        let head = snake.head();
        if food_set.contains(&head) {
            // Grow: duplicate tail
            let tail = *snake.body.last().expect("non-empty body for alive snake");
            snake.body.push(tail);
            snake.health = SNAKE_MAX_HEALTH;
            eaten.insert(head);
        }
    }

    board.food.retain(|f| !eaten.contains(f));
}

/// Go: `EliminateSnakesStandard`.
///
/// Phase 1 — Immediate (natural order): out-of-health, out-of-bounds.
/// Phase 2 — Deferred collisions: self-collision, body collision, head-to-head.
///
/// All eliminations use `eliminated_on_turn = board.turn + 1`.
pub fn eliminate_snakes(board: &mut BoardState) -> Result<(), RulesError> {
    // Phase 1: Immediate eliminations
    for snake in &mut board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }

        if snake.body.is_empty() {
            return Err(RulesError::ZeroLengthSnake(snake.id.clone()));
        }

        // Out of health
        if snake.health <= 0 {
            eliminate_snake(snake, EliminationCause::OutOfHealth, "", board.turn + 1);
            continue;
        }

        // Out of bounds — check ALL body segments
        let out_of_bounds = snake
            .body
            .iter()
            .any(|p| p.x < 0 || p.x >= board.width || p.y < 0 || p.y >= board.height);
        if out_of_bounds {
            eliminate_snake(snake, EliminationCause::OutOfBounds, "", board.turn + 1);
        }
    }

    // Phase 2: Deferred collisions
    // Build snakeIndicesByLength sorted by body length DESCENDING
    let mut snake_indices_by_length: Vec<usize> = (0..board.snakes.len()).collect();
    snake_indices_by_length.sort_by(|a, b| {
        board.snakes[*b]
            .body
            .len()
            .cmp(&board.snakes[*a].body.len())
    });

    // Collect deferred eliminations: (snake_index, cause, eliminated_by)
    let mut deferred: Vec<(usize, EliminationCause, String)> = Vec::new();

    // Outer loop: natural order
    for i in 0..board.snakes.len() {
        let snake = &board.snakes[i];

        // Skip already eliminated (Phase 1 or prior)
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }

        let head = snake.head();

        // Priority 1: Self-collision (head in body[1..])
        if snake.body[1..].contains(&head) {
            deferred.push((i, EliminationCause::SelfCollision, snake.id.clone()));
            continue;
        }

        // Priority 2: Body collision (iterate others in length-desc order)
        let mut body_collision_found = false;
        for &j in &snake_indices_by_length {
            if j == i {
                continue;
            }
            let other = &board.snakes[j];
            if other.eliminated_cause.is_eliminated() {
                continue;
            }
            // Check head against other's body[1..]
            if other.body[1..].contains(&head) {
                deferred.push((i, EliminationCause::Collision, other.id.clone()));
                body_collision_found = true;
                break;
            }
        }
        if body_collision_found {
            continue;
        }

        // Priority 3: Head-to-head (iterate others in length-desc order)
        for &j in &snake_indices_by_length {
            if j == i {
                continue;
            }
            let other = &board.snakes[j];
            if other.eliminated_cause.is_eliminated() {
                continue;
            }
            if head == other.head() && snake.body.len() <= other.body.len() {
                deferred.push((i, EliminationCause::HeadToHeadCollision, other.id.clone()));
                break;
            }
        }
    }

    // Apply all deferred eliminations together
    let turn = board.turn + 1;
    for (idx, cause, by) in deferred {
        eliminate_snake(&mut board.snakes[idx], cause, &by, turn);
    }

    Ok(())
}

/// High-level: execute one turn.
///
/// Returns `true` if the game was already over BEFORE processing (early exit).
///
/// Pipeline order:
///   1. `is_game_over` check
///   2. `move_snakes`
///   3. `reduce_snake_health`
///   4. `damage_hazards`
///   5. `feed_snakes`
///   6. `eliminate_snakes`
///   7. `board.turn += 1`
///
/// NOTE: food spawning (`maybe_spawn_food`) is NOT in this pipeline — caller
/// invokes it after.
pub fn execute_turn(
    board: &mut BoardState,
    moves: &[SnakeMove],
    settings: &StandardSettings,
) -> Result<bool, RulesError> {
    if is_game_over(board) {
        return Ok(true);
    }

    move_snakes(board, moves)?;
    reduce_snake_health(board);
    damage_hazards(board, settings);
    feed_snakes(board);
    eliminate_snakes(board)?;

    board.turn += 1;

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::test_helpers::*;

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

        // Single alive snake => game over (standard rules: <=1 alive)
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

    /// Port of Go `TestStandardCreateNextBoardState` -- parameterized cases.
    ///
    /// Tests full turn execution with various scenarios.
    #[test]
    fn test_standard_cases() {
        let settings = StandardSettings::default();

        // "bystander" snake keeps the game alive (>=2 snakes needed)
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

        // Snake at health 1, food at next position -- should eat BEFORE elimination
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
    /// A snake eating food on its last move should survive -- the pipeline order
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

    /// Port of Go `TestMoveSnakesDefault` -- test all 4 directions.
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

        // Snake head on hazard with food -- no damage
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        board.hazards.push(Point::new(5, 5));
        board.food.push(Point::new(5, 5));

        damage_hazards(&mut board, &settings);
        assert_eq!(board.snakes[0].health, 100); // no damage

        // Snake body on hazard but not head -- no damage
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

        // Snake not on food -- no change
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

        // Two snakes eating same food -- both grow
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

    /// Health can go negative in `reduce_snake_health` -- no clamping.
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
}
