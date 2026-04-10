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
