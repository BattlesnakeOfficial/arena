//! Snail Mode: snakes leave a decaying trail of stacked hazards behind
//! their tail as they move.
//!
//! Ported from the canonical Go implementation
//! (`BattlesnakeOfficial/rules`, `maps/snail_mode.go`, authored by coreyja
//! and jlafayette). Upstream this is a community *map* (`game.map =
//! "snail_mode"`) running on the standard ruleset, not a ruleset of its own;
//! the arena engine has no map layer, so it is wired up as a mode alongside
//! royale but keeps the map's exact semantics.
//!
//! # Mechanic
//!
//! After the standard pipeline resolves each turn:
//! 1. **Decay**: every on-board hazard stack loses one entry (hazard
//!    stacking = repeated [`Point`]s in `board.hazards`;
//!    [`crate::standard::damage_hazards`] applies damage once per entry, so a
//!    stack of N deals N x `hazard_damage_per_turn`).
//! 2. **Store**: each live snake's tail square is recorded for *next* turn
//!    with a stack count equal to the snake's length -- unless the tail is
//!    doubled (tail == sub-tail, i.e. the snake just ate or hasn't unstacked
//!    from spawn), in which case the tail isn't vacating and no trail is
//!    recorded. Eliminated snakes record nothing.
//! 3. **Restore**: the *previous* turn's recorded tails become on-board
//!    hazard stacks -- except squares currently occupied by a live snake's
//!    head, which are skipped entirely (Go does this for board-viewer
//!    clarity; the skipped stack is dropped, not deferred).
//!
//! The net effect: one turn after a snake vacates a square, that square gets
//! a hazard stack equal to the snake's length, which then fades by one per
//! turn. Food on a hazard square negates that square's damage (see
//! `damage_hazards`), and food spawning is unchanged from standard.
//!
//! # State representation
//!
//! Pending tails must survive from one turn to the next. Like the Go map,
//! this port stores them as *out-of-bounds* points inside `board.hazards`
//! (`y + board.height`, guaranteed off-board since on-board `y <
//! height`). This keeps `BoardState` fully self-describing -- no side
//! channel to thread through the engine's game loop -- and matches Go
//! behaviour exactly, including during the damage phase (pending points are
//! present in `hazards` while `damage_hazards` runs, but can only ever match
//! a head that is itself out of bounds and about to be eliminated, exactly
//! as in Go).
//!
//! Consumers that expose hazards to the outside world (snake /move payloads,
//! board-viewer frames) MUST filter to on-board points; see
//! [`BoardState::on_board_hazards`].
//!
//! # Deviations from Go
//!
//! - Go's `doubleTail` indexes `body[len-2]` and would panic on a
//!   single-segment snake; this port treats bodies shorter than 2 as doubled
//!   (no trail). Unreachable in real games (snakes spawn at length 3).
//! - Go's map API rebuilds hazards through an editor with nondeterministic
//!   map iteration order for the decay phase; this port keeps first-seen
//!   order so results are deterministic. Hazard *multisets* are identical.

use std::collections::HashMap;

use crate::standard;
use crate::types::*;

/// Convert an on-board tail square into its off-board storage point.
fn store_tail_location(point: Point, height: i32) -> Point {
    Point::new(point.x, point.y + height)
}

/// Convert an off-board storage point back to the on-board tail square.
fn restore_tail_location(point: Point, height: i32) -> Point {
    Point::new(point.x, point.y - height)
}

/// Is the point outside the playable board?
fn out_of_bounds(p: Point, width: i32, height: i32) -> bool {
    p.x < 0 || p.y < 0 || p.x >= width || p.y >= height
}

/// Does the snake currently have a doubled (stacked) tail?
///
/// True when tail == sub-tail: the snake just ate (or hasn't unstacked from
/// its spawn point), so the tail square is not being vacated this turn.
/// Bodies shorter than 2 segments are treated as doubled (see module docs).
fn double_tail(snake: &Snake) -> bool {
    let len = snake.body.len();
    if len < 2 {
        return true;
    }
    snake.body[len - 1] == snake.body[len - 2]
}

/// Run the Snail Mode post-turn hazard update (decay / store / restore).
///
/// Faithful port of Go `SnailModeMap.PostUpdateBoard` (minus food spawning,
/// which the arena engine handles separately, same as every other mode):
///
/// 1. Split the previous turn's `board.hazards` into on-board stacks and
///    off-board pending tails.
/// 2. Re-add each on-board stack with count - 1 (decay).
/// 3. Store each live, non-double-tail snake's tail off-board with count =
///    snake length (applied next turn).
/// 4. Restore the pending tails on-board at full stack, skipping (dropping)
///    any square occupied by a live snake's head.
pub fn post_update_board(board: &mut BoardState) {
    let width = board.width;
    let height = board.height;

    // Split last turn's hazards: on-board stacks (counted, first-seen order)
    // vs off-board pending tails (converted back to their on-board squares).
    let mut pending_tails: Vec<Point> = Vec::new();
    let mut stack_order: Vec<Point> = Vec::new();
    let mut stack_counts: HashMap<Point, i32> = HashMap::new();
    for &hazard in &board.hazards {
        if out_of_bounds(hazard, width, height) {
            pending_tails.push(restore_tail_location(hazard, height));
        } else {
            let count = stack_counts.entry(hazard).or_insert(0);
            if *count == 0 {
                stack_order.push(hazard);
            }
            *count += 1;
        }
    }

    let mut new_hazards: Vec<Point> = Vec::new();

    // Decay: re-add existing on-board stacks with one entry fewer, so
    // trails fade as snakes move away.
    for point in stack_order {
        let count = stack_counts[&point];
        for _ in 0..count - 1 {
            new_hazards.push(point);
        }
    }

    // Store: record each live snake's tail off-board for next turn, stacked
    // to the snake's current length.
    for snake in &board.snakes {
        if snake.eliminated_cause.is_eliminated() {
            continue;
        }
        if double_tail(snake) {
            continue;
        }

        let tail = snake.body[snake.body.len() - 1];
        let off_board_tail = store_tail_location(tail, height);
        for _ in 0..snake.body.len() {
            new_hazards.push(off_board_tail);
        }
    }

    // Restore: last turn's pending tails become on-board stacks, unless a
    // live snake's head sits on the square (dropped, matching Go).
    for point in pending_tails {
        let is_head = board
            .snakes
            .iter()
            .filter(|s| !s.eliminated_cause.is_eliminated())
            .any(|s| s.head() == point);
        if !is_head {
            new_hazards.push(point);
        }
    }

    board.hazards = new_hazards;
}

/// Execute one turn of the Snail Mode pipeline.
///
/// Returns `true` if the game was already over BEFORE processing (early
/// exit), mirroring [`standard::execute_turn`].
///
/// Pipeline order (standard stages, then the snail post-update -- the same
/// position Go's `PostUpdateBoard` runs in):
///   1. `is_game_over` check
///   2. `move_snakes`
///   3. `reduce_snake_health`
///   4. `damage_hazards` (damages snakes on trail squares laid in PREVIOUS turns)
///   5. `feed_snakes`
///   6. `eliminate_snakes`
///   7. `post_update_board` (decay / store / restore for NEXT turn)
///   8. `board.turn += 1`
///
/// NOTE: food spawning is NOT in this pipeline -- caller invokes it after,
/// same as with `standard::execute_turn`. Food on a trail square negates
/// that square's hazard damage entirely (handled in `damage_hazards`).
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
    post_update_board(board);

    board.turn += 1;

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::eliminate_snake;
    use crate::test_utils::{make_board, make_snake};
    use proptest::prelude::*;

    const H: i32 = 11; // board height used throughout

    /// Multiset of the board's ON-board hazards, as point -> stack count.
    fn on_board_counts(board: &BoardState) -> HashMap<Point, i32> {
        let mut counts = HashMap::new();
        for h in board.on_board_hazards() {
            *counts.entry(*h).or_insert(0) += 1;
        }
        counts
    }

    /// Multiset of the board's OFF-board (pending tail) hazard entries.
    fn off_board_counts(board: &BoardState) -> HashMap<Point, i32> {
        let mut counts = HashMap::new();
        for &h in &board.hazards {
            if out_of_bounds(h, board.width, board.height) {
                *counts.entry(h).or_insert(0) += 1;
            }
        }
        counts
    }

    fn counts_of(points: &[(i32, i32, i32)]) -> HashMap<Point, i32> {
        points
            .iter()
            .map(|&(x, y, n)| (Point::new(x, y), n))
            .collect()
    }

    fn up(id: &str) -> SnakeMove {
        SnakeMove {
            id: id.to_string(),
            direction: Direction::Up,
        }
    }

    /// Run the standard stages + snail post-update the way the engine's
    /// `apply_turn` does (no game-over early exit, explicit turn increment),
    /// so single-snake fixtures can be driven without a bystander.
    fn run_stages(board: &mut BoardState, moves: &[SnakeMove], settings: &StandardSettings) {
        standard::move_snakes(board, moves).unwrap();
        standard::reduce_snake_health(board);
        standard::damage_hazards(board, settings);
        standard::feed_snakes(board);
        standard::eliminate_snakes(board).unwrap();
        post_update_board(board);
        board.turn += 1;
    }

    // === Unit tests ===

    #[test]
    fn trail_appears_one_turn_after_vacating_with_stack_equal_to_length() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );

        // Turn 1: body becomes [(5,6),(5,5),(5,4)]. Nothing visible yet --
        // the current tail square (5,4) is only recorded (off-board) for
        // next turn.
        run_stages(&mut board, &[up("one")], &settings);
        assert!(on_board_counts(&board).is_empty());
        assert_eq!(off_board_counts(&board), counts_of(&[(5, 4 + H, 3)]));

        // Turn 2: the snake vacates (5,4) and the record laid last turn
        // lands there as an on-board stack of 3 (= snake length).
        run_stages(&mut board, &[up("one")], &settings);
        assert_eq!(on_board_counts(&board), counts_of(&[(5, 4, 3)]));
        assert_eq!(off_board_counts(&board), counts_of(&[(5, 5 + H, 3)]));
    }

    #[test]
    fn trail_decays_by_exactly_one_per_turn_until_gone() {
        // Seed an on-board stack of 3 with no live snakes: pure decay.
        let mut board = make_board(11, H, vec![]);
        board.hazards = vec![Point::new(2, 2); 3];

        post_update_board(&mut board);
        assert_eq!(on_board_counts(&board), counts_of(&[(2, 2, 2)]));

        post_update_board(&mut board);
        assert_eq!(on_board_counts(&board), counts_of(&[(2, 2, 1)]));

        post_update_board(&mut board);
        assert!(board.hazards.is_empty());

        // Stays gone.
        post_update_board(&mut board);
        assert!(board.hazards.is_empty());
    }

    #[test]
    fn double_tail_spawns_no_trail() {
        // Tail == sub-tail (just ate / just spawned): tail isn't vacating.
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3), (5, 3)], 100)],
        );
        post_update_board(&mut board);
        assert!(
            board.hazards.is_empty(),
            "double-tailed snake must not record a trail"
        );

        // Spawn-stacked snake (3 copies of one point) is also doubled.
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 5), (5, 5)], 100)],
        );
        post_update_board(&mut board);
        assert!(board.hazards.is_empty());
    }

    #[test]
    fn short_bodies_do_not_panic_and_leave_no_trail() {
        // Go would panic indexing body[len-2]; we treat these as doubled.
        let mut board = make_board(11, H, vec![make_snake("one", &[(5, 5)], 100)]);
        post_update_board(&mut board);
        assert!(board.hazards.is_empty());
    }

    #[test]
    fn eliminated_snakes_leave_no_new_trail() {
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        eliminate_snake(&mut board.snakes[0], EliminationCause::OutOfHealth, "", 1);
        post_update_board(&mut board);
        assert!(board.hazards.is_empty());
    }

    #[test]
    fn eliminated_snake_existing_pending_tail_still_restores() {
        // A trail recorded while alive still lands after the snake dies:
        // only NEW records are gated on being alive.
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        board.hazards = vec![Point::new(5, 2 + H); 3];
        eliminate_snake(&mut board.snakes[0], EliminationCause::OutOfHealth, "", 1);

        post_update_board(&mut board);
        assert_eq!(on_board_counts(&board), counts_of(&[(5, 2, 3)]));
    }

    #[test]
    fn head_occupied_square_skips_placement_and_stack_is_dropped() {
        // Pending tail at (5,5); a live snake's head sits there.
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 5), (5, 5)], 100)],
        );
        board.hazards = vec![Point::new(5, 5 + H); 3];

        post_update_board(&mut board);
        assert!(
            on_board_counts(&board).is_empty(),
            "no hazard may be placed under a live head"
        );

        // The stack is dropped, not deferred: nothing appears later either.
        // Move the snake away and update again.
        board.snakes[0].body = vec![Point::new(7, 7), Point::new(7, 7), Point::new(7, 7)];
        post_update_board(&mut board);
        assert!(on_board_counts(&board).is_empty());
    }

    #[test]
    fn eliminated_snake_head_does_not_block_placement() {
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 5), (5, 5)], 100)],
        );
        board.hazards = vec![Point::new(5, 5 + H); 2];
        eliminate_snake(&mut board.snakes[0], EliminationCause::OutOfHealth, "", 1);

        post_update_board(&mut board);
        assert_eq!(on_board_counts(&board), counts_of(&[(5, 5, 2)]));
    }

    #[test]
    fn multiple_snakes_trails_stack_independently() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![
                make_snake("one", &[(2, 5), (2, 4), (2, 3)], 100),
                make_snake("two", &[(8, 5), (8, 4), (8, 3), (8, 2)], 100),
            ],
        );

        run_stages(&mut board, &[up("one"), up("two")], &settings);
        run_stages(&mut board, &[up("one"), up("two")], &settings);

        // Each snake's first vacated square carries its own length.
        assert_eq!(on_board_counts(&board), counts_of(&[(2, 4, 3), (8, 3, 4)]),);
    }

    #[test]
    fn trail_damage_applies_per_stack_entry() {
        // A fresh length-3 trail square deals 3 x hazard_damage on entry.
        let settings = StandardSettings::default(); // hazard damage 14
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        board.hazards = vec![Point::new(5, 6); 3];

        run_stages(&mut board, &[up("one")], &settings);

        // 1 (starvation) + 3 x 14 (stacked hazard) = 43 total damage.
        assert_eq!(board.snakes[0].health, 100 - 1 - 3 * 14);
        assert!(!board.snakes[0].eliminated_cause.is_eliminated());
    }

    #[test]
    fn lethal_stacked_trail_eliminates_with_hazard_cause() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 40)],
        );
        board.hazards = vec![Point::new(5, 6); 3]; // 42 damage > 39 remaining

        run_stages(&mut board, &[up("one")], &settings);
        assert_eq!(board.snakes[0].eliminated_cause, EliminationCause::Hazard);
    }

    #[test]
    fn food_on_trail_square_negates_hazard_damage() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 50)],
        );
        board.hazards = vec![Point::new(5, 6); 3];
        board.food.push(Point::new(5, 6));

        run_stages(&mut board, &[up("one")], &settings);

        // No hazard damage; the snake eats and restores to full.
        assert_eq!(board.snakes[0].health, SNAKE_MAX_HEALTH);
        assert_eq!(board.snakes[0].body.len(), 4);
        assert!(!board.food.contains(&Point::new(5, 6)));
    }

    #[test]
    fn snake_that_ate_skips_one_trail_then_resumes_with_new_length() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        board.food.push(Point::new(5, 6));

        // Turn 1: eats at (5,6) -> tail duplicated at (5,4). The doubled
        // tail isn't vacating next turn, so no trail is recorded this turn.
        run_stages(&mut board, &[up("one")], &settings);
        assert!(board.hazards.is_empty(), "no trail on the eating turn");
        assert_eq!(board.snakes[0].body.len(), 4);

        // Turn 2: tail unstacks; (5,4) is vacated and recorded at the NEW
        // length (4).
        run_stages(&mut board, &[up("one")], &settings);
        assert_eq!(off_board_counts(&board), counts_of(&[(5, 4 + H, 4)]));

        // Turn 3: stack of 4 lands on-board.
        run_stages(&mut board, &[up("one")], &settings);
        assert_eq!(on_board_counts(&board)[&Point::new(5, 4)], 4);
    }

    #[test]
    fn execute_turn_early_exits_when_game_over() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        board.hazards = vec![Point::new(2, 2); 3];

        let over = execute_turn(&mut board, &[up("one")], &settings).unwrap();
        assert!(over);
        assert_eq!(board.turn, 0, "turn must not advance after game over");
        assert_eq!(
            board.hazards,
            vec![Point::new(2, 2); 3],
            "hazards must not decay after game over"
        );
    }

    #[test]
    fn execute_turn_runs_full_pipeline_and_increments_turn() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
                make_snake("two", &[(8, 5), (8, 4), (8, 3)], 100),
            ],
        );

        let over = execute_turn(&mut board, &[up("one"), up("two")], &settings).unwrap();
        assert!(!over);
        assert_eq!(board.turn, 1);
        assert_eq!(board.snakes[0].health, 99);
        assert_eq!(
            off_board_counts(&board),
            counts_of(&[(5, 4 + H, 3), (8, 4 + H, 3)]),
        );
    }

    /// The worked 5-turn example: a single length-3 snake walking straight
    /// up column x=5 from body [(5,5),(5,4),(5,3)]. Each vacated square
    /// gains a stack of 3 one turn after vacating, then fades by 1 per turn.
    #[test]
    fn worked_five_turn_example() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            H,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );

        let expected_on_board: [&[(i32, i32, i32)]; 5] = [
            // After turn 1: tail (5,4) recorded off-board only.
            &[],
            // After turn 2: (5,4) lands with stack 3.
            &[(5, 4, 3)],
            // After turn 3: (5,4) decays to 2; (5,5) lands with 3.
            &[(5, 4, 2), (5, 5, 3)],
            // After turn 4: 1 / 2 / 3.
            &[(5, 4, 1), (5, 5, 2), (5, 6, 3)],
            // After turn 5: (5,4) fully gone; 1 / 2 / 3 behind the snake.
            &[(5, 5, 1), (5, 6, 2), (5, 7, 3)],
        ];

        for (turn, expected) in expected_on_board.iter().enumerate() {
            run_stages(&mut board, &[up("one")], &settings);
            assert_eq!(
                on_board_counts(&board),
                counts_of(expected),
                "on-board hazards after turn {}",
                turn + 1
            );
            // The just-recorded tail is always off-board with stack 3.
            let tail = board.snakes[0].body[2];
            assert_eq!(
                off_board_counts(&board),
                counts_of(&[(tail.x, tail.y + H, 3)]),
                "pending tail after turn {}",
                turn + 1
            );
            // The snake never touches its own trail: only starvation damage.
            assert_eq!(board.snakes[0].health, 100 - (turn as i32 + 1));
        }

        assert_eq!(board.turn, 5);
    }

    #[test]
    fn on_board_hazards_view_filters_bookkeeping_points() {
        let mut board = make_board(11, H, vec![]);
        board.hazards = vec![
            Point::new(5, 5),
            Point::new(5, 5),
            Point::new(5, 5 + H),
            Point::new(0, H),
            Point::new(10, 2 * H - 1),
        ];
        let visible: Vec<Point> = board.on_board_hazards().copied().collect();
        assert_eq!(visible, vec![Point::new(5, 5), Point::new(5, 5)]);
    }

    // === Property tests ===

    fn arb_point(width: i32, height: i32) -> impl Strategy<Value = Point> {
        (0..width, 0..height).prop_map(|(x, y)| Point::new(x, y))
    }

    /// Arbitrary snake for post-update properties: body points anywhere on
    /// the board (connectivity is irrelevant to the hazard update), length
    /// 1..=6, possibly eliminated, possibly double-tailed.
    fn arb_snake(idx: usize, width: i32, height: i32) -> impl Strategy<Value = Snake> {
        (
            proptest::collection::vec(arb_point(width, height), 1..=6),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(move |(mut body, eliminated, force_double_tail)| {
                if force_double_tail && body.len() >= 2 {
                    let tail = body[body.len() - 1];
                    let sub = body.len() - 2;
                    body[sub] = tail;
                }
                Snake {
                    id: format!("snake-{idx}"),
                    body,
                    health: 100,
                    eliminated_cause: if eliminated {
                        EliminationCause::OutOfHealth
                    } else {
                        EliminationCause::NotEliminated
                    },
                    eliminated_by: String::new(),
                    eliminated_on_turn: 0,
                }
            })
    }

    /// Arbitrary snail-mode board mid-game: on-board hazard stacks plus
    /// well-formed off-board pending tails (`y + height`).
    fn arb_board() -> impl Strategy<Value = BoardState> {
        (3..=15i32, 3..=15i32).prop_flat_map(|(width, height)| {
            let snakes: Vec<_> = (0..3).map(|i| arb_snake(i, width, height)).collect();
            (
                snakes,
                proptest::collection::vec((arb_point(width, height), 1..=4i32), 0..=6),
                proptest::collection::vec((arb_point(width, height), 1..=6i32), 0..=3),
                Just(width),
                Just(height),
            )
                .prop_map(|(snakes, stacks, pending, width, height)| {
                    let mut hazards = Vec::new();
                    for (p, n) in stacks {
                        for _ in 0..n {
                            hazards.push(p);
                        }
                    }
                    for (p, n) in pending {
                        for _ in 0..n {
                            hazards.push(store_tail_location(p, height));
                        }
                    }
                    BoardState {
                        turn: 0,
                        width,
                        height,
                        food: Vec::new(),
                        snakes,
                        hazards,
                    }
                })
        })
    }

    fn live_heads(board: &BoardState) -> Vec<Point> {
        board
            .snakes
            .iter()
            .filter(|s| !s.eliminated_cause.is_eliminated())
            .map(|s| s.head())
            .collect()
    }

    proptest! {
        /// Independent oracle for the whole update, computed per-square:
        /// new on-board count = max(old - 1, 0) + restored pending (unless a
        /// live head occupies the square, in which case the pending stack is
        /// dropped and only the decayed part remains).
        #[test]
        fn prop_on_board_counts_match_decay_plus_restore(mut board in arb_board()) {
            let old_on = on_board_counts(&board);
            let old_pending: HashMap<Point, i32> = {
                let mut m = HashMap::new();
                for &h in &board.hazards {
                    if out_of_bounds(h, board.width, board.height) {
                        *m.entry(restore_tail_location(h, board.height)).or_insert(0) += 1;
                    }
                }
                m
            };
            let heads = live_heads(&board);

            post_update_board(&mut board);
            let new_on = on_board_counts(&board);

            let mut squares: std::collections::HashSet<Point> = old_on.keys().copied().collect();
            squares.extend(old_pending.keys().copied());
            squares.extend(new_on.keys().copied());

            for p in squares {
                let decayed = (old_on.get(&p).copied().unwrap_or(0) - 1).max(0);
                let restored = if heads.contains(&p) {
                    0
                } else {
                    old_pending.get(&p).copied().unwrap_or(0)
                };
                prop_assert_eq!(
                    new_on.get(&p).copied().unwrap_or(0),
                    decayed + restored,
                    "square {:?}: decay+restore mismatch", p
                );
            }
        }

        /// Off-board entries after the update are exactly one stack per
        /// live, non-double-tail snake, at (tail.x, tail.y + height), with
        /// count = snake length.
        #[test]
        fn prop_off_board_entries_are_exactly_live_snake_tails(mut board in arb_board()) {
            let mut expected: HashMap<Point, i32> = HashMap::new();
            for snake in &board.snakes {
                if snake.eliminated_cause.is_eliminated() || double_tail(snake) {
                    continue;
                }
                let tail = snake.body[snake.body.len() - 1];
                *expected
                    .entry(store_tail_location(tail, board.height))
                    .or_insert(0) += snake.body.len() as i32;
            }

            post_update_board(&mut board);
            prop_assert_eq!(off_board_counts(&board), expected);
        }

        /// A square with no pending tail strictly decays: repeated updates
        /// with no live snakes empty the board in max-stack turns.
        #[test]
        fn prop_hazards_fully_decay_without_snakes(mut board in arb_board()) {
            board.snakes.clear();
            // One update flushes pending tails on-board; after that the
            // largest possible per-square stack is bounded by the generator
            // (up to 6 on-board stack entries of 4 plus 3 pending entries of
            // 6 on one square = 42), so 45 further updates must empty it.
            post_update_board(&mut board);
            for _ in 0..45 {
                post_update_board(&mut board);
            }
            prop_assert!(
                board.hazards.is_empty(),
                "hazards must fully decay, got {:?}", board.hazards
            );
        }

        /// The update is deterministic, including entry order.
        #[test]
        fn prop_deterministic(board in arb_board()) {
            let mut a = board.clone();
            let mut b = board;
            post_update_board(&mut a);
            post_update_board(&mut b);
            prop_assert_eq!(a.hazards, b.hazards);
        }

        /// After the update, no hazard entry sits in the dead zone: every
        /// entry is either on-board or a well-formed pending tail
        /// (x on-board, height <= y < 2 * height).
        #[test]
        fn prop_all_entries_on_board_or_well_formed_pending(mut board in arb_board()) {
            post_update_board(&mut board);
            for &h in &board.hazards {
                let on = !out_of_bounds(h, board.width, board.height);
                let pending = h.x >= 0
                    && h.x < board.width
                    && h.y >= board.height
                    && h.y < 2 * board.height;
                prop_assert!(on || pending, "malformed hazard entry {:?}", h);
            }
        }
    }
}
