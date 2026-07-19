//! Constrictor game mode: standard rules, no food, guaranteed growth.
//!
//! Ported from the canonical Go implementation
//! (`BattlesnakeOfficial/rules`, `constrictor.go`). Constrictor is the
//! standard pipeline plus two extra stages that run after elimination every
//! turn:
//!
//! - [`remove_food`] (Go `RemoveFoodConstrictor` / `StageSpawnFoodNoFood`):
//!   clears ALL food from the board.
//! - [`grow_snakes`] (Go `GrowSnakesConstrictor` /
//!   `StageModifySnakesAlwaysGrow`): pins every snake's health at
//!   [`SNAKE_MAX_HEALTH`] and duplicates its tail segment so it grows by one
//!   each turn -- unless the tail is already stacked (tail == sub-tail), in
//!   which case the pending growth from the stack is enough.
//!
//! Like Go, `grow_snakes` does NOT skip eliminated snakes: a snake that dies
//! this turn still ends the turn with health 100 and a grown body (see the
//! ported `constrictorMoveAndCollideMAD` test case).
//!
//! No food ever spawns (callers must not run food spawning for constrictor
//! games), so snakes never starve -- games end via collisions and space
//! exhaustion as ever-growing bodies fill the board. Hazards are inert
//! (nothing places them in this mode).
//!
//! # Initial board state
//!
//! At game start the Go engine runs the full ruleset pipeline once with no
//! moves (`ruleset.Execute(boardState, nil)`). The standard stages all
//! short-circuit via `IsInitialization`, but the two constrictor stages do
//! not, so the initial board has its food stripped and its snakes set to
//! full health (initial snakes are three stacked segments, so no growth
//! happens yet). [`modify_initial_board`] reproduces exactly that.

use crate::standard;
use crate::types::*;

/// Remove all food from the board.
///
/// Faithful port of Go `RemoveFoodConstrictor`: runs after feeding and
/// elimination each turn, and once at game initialization.
pub fn remove_food(board: &mut BoardState) {
    board.food.clear();
}

/// Pin every snake at max health and make it grow.
///
/// Faithful port of Go `GrowSnakesConstrictor`:
/// - Errors on a zero-length snake.
/// - Applies to EVERY snake, eliminated or not (Go does not filter).
/// - Sets health to [`SNAKE_MAX_HEALTH`].
/// - Duplicates the tail segment, but only if the tail is not already
///   stacked (tail != sub-tail) -- an already-stacked tail means growth is
///   still pending from a previous stack, so stacking again would double up.
///
/// A single-segment snake has no sub-tail and therefore no stacked tail, so
/// it grows. (Go would panic indexing `body[len-2]`; real snakes always have
/// at least two segments, starting at three.)
pub fn grow_snakes(board: &mut BoardState) -> Result<(), RulesError> {
    for snake in &mut board.snakes {
        if snake.body.is_empty() {
            return Err(RulesError::ZeroLengthSnake(snake.id.clone()));
        }

        snake.health = SNAKE_MAX_HEALTH;

        let len = snake.body.len();
        let tail = snake.body[len - 1];
        let tail_stacked = len >= 2 && snake.body[len - 2] == tail;
        if !tail_stacked {
            snake.body.push(tail);
        }
    }

    Ok(())
}

/// Apply constrictor's game-start modifications to a freshly created board.
///
/// Mirrors the Go engine executing the constrictor pipeline once at
/// initialization (turn 0, no moves): the standard stages no-op via
/// `IsInitialization`, leaving exactly `RemoveFoodConstrictor` +
/// `GrowSnakesConstrictor`. The initial board therefore has no food and all
/// snakes at full health; initial bodies are fully stacked, so no snake
/// grows yet.
pub fn modify_initial_board(board: &mut BoardState) -> Result<(), RulesError> {
    remove_food(board);
    grow_snakes(board)
}

/// Execute one turn of the constrictor rules pipeline.
///
/// Returns `true` if the game was already over BEFORE processing (early
/// exit), mirroring [`standard::execute_turn`].
///
/// Pipeline order (matches Go's `constrictorRulesetStages`):
///   1. `is_game_over` check
///   2. `move_snakes`
///   3. `reduce_snake_health`
///   4. `damage_hazards` (inert: constrictor never places hazards)
///   5. `feed_snakes`
///   6. `eliminate_snakes`
///   7. [`remove_food`]
///   8. [`grow_snakes`]
///   9. `board.turn += 1`
///
/// NOTE: unlike standard/royale, callers must NOT run food spawning after
/// this -- constrictor boards never have food.
pub fn execute_turn(
    board: &mut BoardState,
    moves: &[SnakeMove],
    settings: &StandardSettings,
) -> Result<bool, RulesError> {
    if standard::is_game_over(board) {
        return Ok(true);
    }

    standard::move_snakes(board, moves)?;
    standard::reduce_snake_health(board);
    standard::damage_hazards(board, settings);
    standard::feed_snakes(board);
    standard::eliminate_snakes(board)?;
    remove_food(board);
    grow_snakes(board)?;

    board.turn += 1;

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{make_board, make_snake};
    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    fn moves(entries: &[(&str, Direction)]) -> Vec<SnakeMove> {
        entries
            .iter()
            .map(|(id, direction)| SnakeMove {
                id: id.to_string(),
                direction: *direction,
            })
            .collect()
    }

    /// Ported from Go `constrictorMoveAndCollideMAD` ("Constrictor Case Move
    /// and Collide"): two equal snakes swap through each other, both are
    /// eliminated by body collision, and BOTH still end the turn grown and at
    /// max health; all food is removed.
    #[test]
    fn go_case_move_and_collide_mutual_elimination() {
        let mut board = make_board(
            10,
            10,
            vec![
                make_snake("one", &[(1, 1), (2, 1)], 99),
                make_snake("two", &[(1, 2), (2, 2)], 99),
            ],
        );
        board.turn = 41;
        board.food = vec![Point::new(10, 10), Point::new(9, 9), Point::new(8, 8)];

        let game_over = execute_turn(
            &mut board,
            &moves(&[("one", Direction::Up), ("two", Direction::Down)]),
            &StandardSettings::default(),
        )
        .unwrap();

        assert!(!game_over, "game was not over before the turn");
        assert_eq!(board.turn, 42);
        assert!(board.food.is_empty(), "all food must be removed");
        assert!(board.hazards.is_empty());

        let one = &board.snakes[0];
        assert_eq!(
            one.body,
            vec![Point::new(1, 2), Point::new(1, 1), Point::new(1, 1)]
        );
        assert_eq!(one.health, 100);
        assert_eq!(one.eliminated_cause, EliminationCause::Collision);
        assert_eq!(one.eliminated_by, "two");
        assert_eq!(one.eliminated_on_turn, 42);

        let two = &board.snakes[1];
        assert_eq!(
            two.body,
            vec![Point::new(1, 1), Point::new(1, 2), Point::new(1, 2)]
        );
        assert_eq!(two.health, 100);
        assert_eq!(two.eliminated_cause, EliminationCause::Collision);
        assert_eq!(two.eliminated_by, "one");
        assert_eq!(two.eliminated_on_turn, 42);
    }

    /// Ported from Go `standardCaseErrNoMoveFound` (constrictor runs the same
    /// standard movement stage).
    #[test]
    fn go_case_missing_move_is_error() {
        let mut board = make_board(
            10,
            10,
            vec![
                make_snake("one", &[(1, 1), (2, 1)], 100),
                make_snake("two", &[(3, 4), (3, 3)], 100),
            ],
        );

        let result = execute_turn(
            &mut board,
            &moves(&[("one", Direction::Up)]),
            &StandardSettings::default(),
        );
        assert_eq!(result, Err(RulesError::NoMoveFound("two".to_string())));
    }

    /// Ported from Go `standardCaseErrZeroLengthSnake`.
    #[test]
    fn go_case_zero_length_snake_is_error() {
        let mut board = make_board(
            10,
            10,
            vec![
                make_snake("one", &[(1, 1), (2, 1)], 100),
                make_snake("two", &[], 100),
            ],
        );

        let result = execute_turn(
            &mut board,
            &moves(&[("one", Direction::Up), ("two", Direction::Down)]),
            &StandardSettings::default(),
        );
        assert_eq!(result, Err(RulesError::ZeroLengthSnake("two".to_string())));
    }

    #[test]
    fn remove_food_clears_all_food() {
        let mut board = make_board(11, 11, vec![]);
        board.food = vec![Point::new(1, 1), Point::new(5, 5), Point::new(9, 9)];
        remove_food(&mut board);
        assert!(board.food.is_empty());
    }

    #[test]
    fn grow_snakes_sets_health_and_duplicates_tail() {
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 37)],
        );
        grow_snakes(&mut board).unwrap();

        let snake = &board.snakes[0];
        assert_eq!(snake.health, SNAKE_MAX_HEALTH);
        assert_eq!(
            snake.body,
            vec![
                Point::new(5, 5),
                Point::new(5, 4),
                Point::new(5, 3),
                Point::new(5, 3)
            ]
        );
    }

    #[test]
    fn grow_snakes_skips_already_stacked_tail() {
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 4)], 50)],
        );
        grow_snakes(&mut board).unwrap();

        let snake = &board.snakes[0];
        assert_eq!(snake.health, SNAKE_MAX_HEALTH);
        assert_eq!(
            snake.body,
            vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 4)],
            "already-stacked tail must not be stacked again"
        );
    }

    /// Go parity: `GrowSnakesConstrictor` does not filter eliminated snakes.
    #[test]
    fn grow_snakes_applies_to_eliminated_snakes() {
        let mut snake = make_snake("dead", &[(5, 5), (5, 4), (5, 3)], 0);
        snake.eliminated_cause = EliminationCause::OutOfBounds;
        let mut board = make_board(11, 11, vec![snake]);
        grow_snakes(&mut board).unwrap();

        let snake = &board.snakes[0];
        assert_eq!(snake.health, SNAKE_MAX_HEALTH);
        assert_eq!(snake.body.len(), 4);
        assert_eq!(snake.body[3], Point::new(5, 3));
    }

    #[test]
    fn grow_snakes_zero_length_snake_is_error() {
        let mut board = make_board(11, 11, vec![make_snake("empty", &[], 100)]);
        assert_eq!(
            grow_snakes(&mut board),
            Err(RulesError::ZeroLengthSnake("empty".to_string()))
        );
    }

    #[test]
    fn modify_initial_board_strips_food_and_pins_health() {
        // Shape of a real initial board: stacked 3-segment snakes plus food.
        let mut one = make_snake("one", &[(1, 1), (1, 1), (1, 1)], 100);
        one.health = 100;
        let two = make_snake("two", &[(9, 9), (9, 9), (9, 9)], 100);
        let mut board = make_board(11, 11, vec![one, two]);
        board.food = vec![Point::new(5, 5), Point::new(0, 2), Point::new(10, 8)];

        modify_initial_board(&mut board).unwrap();

        assert!(board.food.is_empty(), "initial food must be stripped");
        for snake in &board.snakes {
            assert_eq!(snake.health, SNAKE_MAX_HEALTH);
            assert_eq!(
                snake.body.len(),
                3,
                "stacked initial bodies must not grow at init"
            );
        }
        assert_eq!(board.turn, 0, "initialization does not advance the turn");
    }

    /// Initial snakes are three stacked segments. The stack unwinds over the
    /// first turns exactly as in Go: length stays 3 on turn 1 (tail still
    /// stacked after the move), then grows by one every turn.
    #[test]
    fn stacked_initial_snakes_start_growing_on_second_turn() {
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(1, 1), (1, 1), (1, 1)], 100),
                make_snake("two", &[(9, 9), (9, 9), (9, 9)], 100),
            ],
        );
        let settings = StandardSettings::default();

        let expected_lengths = [3usize, 4, 5, 6];
        for (i, expected_len) in expected_lengths.iter().enumerate() {
            execute_turn(
                &mut board,
                &moves(&[("one", Direction::Up), ("two", Direction::Down)]),
                &settings,
            )
            .unwrap();
            for snake in &board.snakes {
                assert!(!snake.eliminated_cause.is_eliminated());
                assert_eq!(
                    snake.body.len(),
                    *expected_len,
                    "unexpected length on turn {}",
                    i + 1
                );
                assert_eq!(snake.health, SNAKE_MAX_HEALTH, "health pinned at 100");
            }
        }
    }

    /// Snakes never starve: health is re-pinned to 100 every turn, so
    /// starvation elimination is impossible no matter how long the game runs.
    #[test]
    fn snakes_never_starve() {
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(1, 1), (1, 1), (1, 1)], 100),
                make_snake("two", &[(9, 9), (9, 9), (9, 9)], 100),
            ],
        );
        let settings = StandardSettings::default();

        // Walk both snakes along straight lines until they hit a wall and die.
        for _ in 0..8 {
            execute_turn(
                &mut board,
                &moves(&[("one", Direction::Up), ("two", Direction::Down)]),
                &settings,
            )
            .unwrap();
            for snake in &board.snakes {
                assert_eq!(snake.health, SNAKE_MAX_HEALTH);
                assert_ne!(snake.eliminated_cause, EliminationCause::OutOfHealth);
            }
        }
    }

    /// Even a snake that eats (leftover food placed manually) grows by exactly
    /// one: feeding duplicates the tail, which makes `grow_snakes` skip it.
    #[test]
    fn eating_does_not_double_grow() {
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 80),
                make_snake("two", &[(0, 0), (0, 1), (0, 2)], 80),
            ],
        );
        board.food = vec![Point::new(5, 6)];

        execute_turn(
            &mut board,
            &moves(&[("one", Direction::Up), ("two", Direction::Down)]),
            &StandardSettings::default(),
        )
        .unwrap();

        assert!(board.food.is_empty());
        // "one" ate: feed grew it, grow_snakes skipped the stacked tail.
        assert_eq!(board.snakes[0].body.len(), 4);
        assert_eq!(board.snakes[0].health, SNAKE_MAX_HEALTH);
        // "two" did not eat: grow_snakes grew it.
        assert_eq!(board.snakes[1].body.len(), 4);
        assert_eq!(board.snakes[1].health, SNAKE_MAX_HEALTH);
    }

    #[test]
    fn food_removed_every_turn_even_without_eating() {
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
                make_snake("two", &[(0, 0), (0, 1), (0, 2)], 100),
            ],
        );
        board.food = vec![Point::new(9, 9), Point::new(8, 8)];

        execute_turn(
            &mut board,
            &moves(&[("one", Direction::Up), ("two", Direction::Down)]),
            &StandardSettings::default(),
        )
        .unwrap();

        assert!(board.food.is_empty(), "untouched food is removed too");
    }

    #[test]
    fn execute_turn_early_exit_when_game_over() {
        let mut board = make_board(11, 11, vec![make_snake("solo", &[(5, 5), (5, 4)], 100)]);
        board.food = vec![Point::new(1, 1)];
        let before = board.clone();

        let game_over = execute_turn(
            &mut board,
            &moves(&[("solo", Direction::Up)]),
            &StandardSettings::default(),
        )
        .unwrap();

        assert!(game_over);
        assert_eq!(board, before, "early exit must not modify the board");
    }

    // === Property tests ===

    fn arb_point(width: i32, height: i32) -> impl Strategy<Value = Point> {
        (0..width, 0..height).prop_map(|(x, y)| Point::new(x, y))
    }

    /// Random-walk snake body that stays in bounds; when the walk is blocked
    /// it duplicates the current position, organically producing stacked
    /// segments (exercising both `grow_snakes` paths).
    fn arb_snake(id: String, width: i32, height: i32) -> impl Strategy<Value = Snake> {
        (arb_point(width, height), 2..=6usize, 1..=100i32).prop_flat_map(
            move |(head, body_len, health)| {
                let id = id.clone();
                prop_vec(0..5u8, body_len - 1).prop_map(move |directions| {
                    let mut body = Vec::with_capacity(body_len);
                    body.push(head);

                    let deltas = [(0, 1), (0, -1), (-1, 0), (1, 0)];

                    let mut current = head;
                    for dir_start in &directions {
                        if *dir_start == 4 {
                            // Explicit stacked segment.
                            body.push(current);
                            continue;
                        }
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
                            body.push(current);
                        }
                    }

                    Snake {
                        id: id.clone(),
                        body,
                        health,
                        eliminated_cause: EliminationCause::NotEliminated,
                        eliminated_by: String::new(),
                        eliminated_on_turn: 0,
                    }
                })
            },
        )
    }

    fn arb_direction() -> impl Strategy<Value = Direction> {
        prop::sample::select(
            &[
                Direction::Up,
                Direction::Down,
                Direction::Left,
                Direction::Right,
            ][..],
        )
    }

    /// A board with 2-4 alive snakes (so `execute_turn` never early-exits),
    /// some food, no hazards -- plus one move per snake.
    fn arb_board_and_moves() -> impl Strategy<Value = (BoardState, Vec<SnakeMove>)> {
        (5..=11i32, 5..=11i32, 2..=4usize).prop_flat_map(|(width, height, snake_count)| {
            let snakes: Vec<_> = (0..snake_count)
                .map(|i| arb_snake(format!("snake-{i}"), width, height))
                .collect();
            let dirs = prop_vec(arb_direction(), snake_count);
            let food = prop_vec(arb_point(width, height), 0..=5);

            (snakes, dirs, food).prop_map(move |(snakes, dirs, food)| {
                let moves = snakes
                    .iter()
                    .zip(dirs)
                    .map(|(s, direction)| SnakeMove {
                        id: s.id.clone(),
                        direction,
                    })
                    .collect();
                let board = BoardState {
                    turn: 0,
                    width,
                    height,
                    food,
                    snakes,
                    hazards: vec![],
                };
                (board, moves)
            })
        })
    }

    proptest! {
        #[test]
        fn prop_food_always_cleared((ref board, ref moves) in arb_board_and_moves()) {
            let mut board = board.clone();
            let game_over = execute_turn(&mut board, moves, &StandardSettings::default()).unwrap();
            prop_assert!(!game_over);
            prop_assert!(board.food.is_empty());
        }

        #[test]
        fn prop_all_snakes_health_pinned_at_max((ref board, ref moves) in arb_board_and_moves()) {
            let mut board = board.clone();
            execute_turn(&mut board, moves, &StandardSettings::default()).unwrap();
            for snake in &board.snakes {
                // Eliminated snakes too: Go grows every snake.
                prop_assert_eq!(snake.health, SNAKE_MAX_HEALTH);
            }
        }

        #[test]
        fn prop_no_starvation((ref board, ref moves) in arb_board_and_moves()) {
            // Health is re-pinned before the turn ends, but a health-1 snake
            // still hits 0 mid-turn and is eliminated by the standard
            // starvation rule -- exactly as in Go, where the health reset
            // happens in a later stage. What can never happen is an
            // elimination when the snake entered the turn at full health.
            let full_health: Vec<bool> = board
                .snakes
                .iter()
                .map(|s| s.health == SNAKE_MAX_HEALTH)
                .collect();
            let mut board = board.clone();
            execute_turn(&mut board, moves, &StandardSettings::default()).unwrap();
            for (snake, was_full) in board.snakes.iter().zip(full_health) {
                if was_full {
                    prop_assert_ne!(
                        &snake.eliminated_cause,
                        &EliminationCause::OutOfHealth,
                        "full-health snake {} starved", snake.id
                    );
                }
            }
        }

        #[test]
        fn prop_tail_stacked_after_every_turn((ref board, ref moves) in arb_board_and_moves()) {
            let mut board = board.clone();
            execute_turn(&mut board, moves, &StandardSettings::default()).unwrap();
            for snake in &board.snakes {
                let len = snake.body.len();
                prop_assert!(len >= 2);
                prop_assert_eq!(
                    snake.body[len - 1],
                    snake.body[len - 2],
                    "snake {} tail not stacked after grow phase", snake.id
                );
            }
        }

        /// Every snake grows by exactly one segment or keeps its length --
        /// and the outcome is fully determined by feeding and pre-move tail
        /// stacking. Elimination is irrelevant: eliminated snakes are grown
        /// too (Go parity). Note phase-1 eliminations (starvation) still
        /// follow the same length rule, so this property doesn't trip over
        /// the two-phase elimination ordering.
        #[test]
        fn prop_growth_is_exactly_determined((ref board, ref moves) in arb_board_and_moves()) {
            let old = board.clone();
            let mut board = board.clone();
            execute_turn(&mut board, moves, &StandardSettings::default()).unwrap();

            for (old_snake, new_snake) in old.snakes.iter().zip(board.snakes.iter()) {
                let old_len = old_snake.body.len();

                // Where does this snake's head land? (All generated snakes are
                // alive before the turn, and all have a move.)
                let mv = moves.iter().find(|m| m.id == old_snake.id).unwrap();
                let (dx, dy) = mv.direction.to_delta();
                let new_head = Point::new(old_snake.head().x + dx, old_snake.head().y + dy);
                let ate = old.food.contains(&new_head);

                // Tail after the move (and before feed/grow) is old_body[len-2];
                // its sub-tail is old_body[len-3], or the new head for len 2.
                let tail_stacked_after_move = if old_len >= 3 {
                    old_snake.body[old_len - 2] == old_snake.body[old_len - 3]
                } else {
                    // len == 2: post-move body is [new_head, old_head]; the head
                    // always moves, so the tail can't be stacked.
                    false
                };

                let expected_len = if ate || !tail_stacked_after_move {
                    old_len + 1
                } else {
                    old_len
                };
                prop_assert_eq!(
                    new_snake.body.len(),
                    expected_len,
                    "snake {} length: {} -> {} (ate={}, stacked={})",
                    old_snake.id, old_len, new_snake.body.len(), ate, tail_stacked_after_move
                );
            }
        }

        #[test]
        fn prop_turn_increments((ref board, ref moves) in arb_board_and_moves()) {
            let mut board = board.clone();
            let old_turn = board.turn;
            execute_turn(&mut board, moves, &StandardSettings::default()).unwrap();
            prop_assert_eq!(board.turn, old_turn + 1);
        }

        #[test]
        fn prop_game_over_early_exit_is_a_no_op(
            snake in arb_snake("solo".to_string(), 11, 11),
            food in prop_vec(arb_point(11, 11), 0..=5),
        ) {
            // One alive snake: the game is already over.
            let mut board = make_board(11, 11, vec![snake]);
            board.food = food;
            let before = board.clone();

            let game_over = execute_turn(
                &mut board,
                &[SnakeMove { id: "solo".to_string(), direction: Direction::Up }],
                &StandardSettings::default(),
            ).unwrap();

            prop_assert!(game_over);
            prop_assert_eq!(board, before);
        }
    }
}
