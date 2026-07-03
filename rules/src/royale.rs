//! Royale game mode: standard rules plus a shrinking board.
//!
//! Ported from the canonical Go implementation
//! (`BattlesnakeOfficial/rules`, `royale.go`). Every `shrink_every_n_turns`
//! turns one board edge is chosen at random and the hazard region grows
//! inward from it by one row/column. Snakes whose head ends a turn in a
//! hazard take `hazard_damage_per_turn` extra damage (see
//! [`crate::standard::damage_hazards`]) unless there is food on that square.
//!
//! # Determinism
//!
//! The Go implementation derives hazard randomness from the game seed only
//! (`settings.GetRand(0)`, i.e. `rand.New(rand.NewSource(seed))`): each turn
//! it re-creates the generator from the seed and replays `turn /
//! shrink_every_n_turns` draws. This makes the hazard sequence a pure
//! function of `(seed, board size, turn)` and guarantees hazards only ever
//! grow.
//!
//! This port keeps that structure but uses [`rand_chacha::ChaCha8Rng`]
//! (seeded from a `u64` game seed) instead of Go's `math/rand`, because the
//! crate's other RNG use (`food.rs`) already goes through the `rand` traits
//! and Go's generator is not practical to reproduce bit-for-bit. The result
//! is NOT the same shrink sequence as the Go engine for a given seed, but it
//! IS fully deterministic for a given seed: replaying a game with the same
//! seed produces the same hazards on every turn. `ChaCha8Rng` is documented
//! as a reproducible, portable stream, which matters for the upcoming
//! engine-verifier work (the arena engine passes a seed derived from the
//! game UUID; a verifier with the same UUID recomputes identical hazards).

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::standard;
use crate::types::*;

/// Settings for the Royale game mode (used alongside [`StandardSettings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoyaleSettings {
    /// Number of turns between board shrinks. Default 25, matching the
    /// official Battlesnake CLI default (`royale.go`'s internal fallback is
    /// 20, but every shipped configuration uses 25).
    pub shrink_every_n_turns: i32,
    /// Game seed for hazard placement. The engine derives this from the game
    /// UUID so replays are deterministic. See the module docs on determinism.
    pub seed: u64,
}

impl Default for RoyaleSettings {
    fn default() -> Self {
        Self {
            shrink_every_n_turns: 25,
            seed: 0,
        }
    }
}

/// Recompute the royale hazard border for the upcoming turn.
///
/// Faithful port of Go `PopulateHazardsRoyale`:
/// - Hazards are cleared and rebuilt from scratch every call.
/// - Uses `board.turn + 1` (the turn currently being resolved), not the
///   previous turn stored in the board state.
/// - Errors if `shrink_every_n_turns < 1`.
/// - Replays `turn / shrink_every_n_turns` random edge choices from a fresh
///   seed-derived RNG, shrinking the safe rectangle by one row/column per
///   choice; everything outside the rectangle becomes hazard.
///
/// Because the RNG is re-seeded identically each call, earlier draws repeat
/// and the hazard region grows monotonically over the course of a game.
pub fn populate_hazards(
    board: &mut BoardState,
    settings: &RoyaleSettings,
) -> Result<(), RulesError> {
    board.hazards.clear();

    // Royale uses the current turn to generate hazards, not the previous
    // turn that's in the board state.
    let turn = board.turn + 1;

    if settings.shrink_every_n_turns < 1 {
        return Err(RulesError::InvalidShrinkFrequency);
    }

    if turn < settings.shrink_every_n_turns {
        return Ok(());
    }

    let mut rng = ChaCha8Rng::seed_from_u64(settings.seed);

    let num_shrinks = turn / settings.shrink_every_n_turns;
    let (mut min_x, mut max_x) = (0, board.width - 1);
    let (mut min_y, mut max_y) = (0, board.height - 1);
    for _ in 0..num_shrinks {
        match rng.gen_range(0..4) {
            0 => min_x += 1,
            1 => max_x -= 1,
            2 => min_y += 1,
            _ => max_y -= 1,
        }
    }

    for x in 0..board.width {
        for y in 0..board.height {
            if x < min_x || x > max_x || y < min_y || y > max_y {
                board.hazards.push(Point::new(x, y));
            }
        }
    }

    Ok(())
}

/// Execute one turn of the royale rules pipeline.
///
/// Returns `true` if the game was already over BEFORE processing (early
/// exit), mirroring [`standard::execute_turn`].
///
/// Pipeline order (matches Go's `royaleRulesetStages`):
///   1. `is_game_over` check
///   2. `move_snakes`
///   3. `reduce_snake_health`
///   4. `damage_hazards` (damages snakes on hazards placed in PREVIOUS turns)
///   5. `feed_snakes`
///   6. `eliminate_snakes`
///   7. `populate_hazards` (spawns hazards that take effect NEXT turn)
///   8. `board.turn += 1`
///
/// Note the order of operations around food: hazard damage is applied before
/// feeding, but a food square inside a hazard negates that hazard entry's
/// damage entirely (Go `DamageHazardsStandard` skips the square), and the
/// snake then eats and restores to full health in `feed_snakes`.
///
/// NOTE: food spawning is NOT in this pipeline -- caller invokes it after,
/// same as with `standard::execute_turn`.
pub fn execute_turn(
    board: &mut BoardState,
    moves: &[SnakeMove],
    settings: &StandardSettings,
    royale_settings: &RoyaleSettings,
) -> Result<bool, RulesError> {
    if standard::is_game_over(board) {
        return Ok(true);
    }

    standard::move_snakes(board, moves)?;
    standard::reduce_snake_health(board);
    standard::damage_hazards(board, settings);
    standard::feed_snakes(board);
    standard::eliminate_snakes(board)?;
    populate_hazards(board, royale_settings)?;

    board.turn += 1;

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{make_board, make_snake};
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn settings(shrink: i32, seed: u64) -> RoyaleSettings {
        RoyaleSettings {
            shrink_every_n_turns: shrink,
            seed,
        }
    }

    /// Compute hazards for a given stored board turn.
    fn hazards_at(width: i32, height: i32, board_turn: i32, s: &RoyaleSettings) -> Vec<Point> {
        let mut board = make_board(width, height, vec![]);
        board.turn = board_turn;
        populate_hazards(&mut board, s).unwrap();
        board.hazards
    }

    #[test]
    fn no_hazards_before_first_shrink() {
        let s = settings(25, 42);
        // board.turn = t resolves turn t+1; turns 1..=24 have no hazards.
        for board_turn in 0..24 {
            assert!(
                hazards_at(11, 11, board_turn, &s).is_empty(),
                "expected no hazards while resolving turn {}",
                board_turn + 1
            );
        }
    }

    #[test]
    fn hazards_appear_at_exact_turn_boundaries() {
        let s = settings(25, 42);

        // Resolving turn 25 (board.turn 24): exactly one shrink = one full
        // edge row or column of an 11x11 board.
        let first = hazards_at(11, 11, 24, &s);
        assert_eq!(first.len(), 11);

        // Turns 25..=49 all have exactly one shrink.
        for board_turn in 24..49 {
            assert_eq!(
                hazards_at(11, 11, board_turn, &s),
                first,
                "hazards should be stable between shrink boundaries (turn {})",
                board_turn + 1
            );
        }

        // Resolving turn 50 (board.turn 49): two shrinks. Two distinct edges
        // overlap in one corner (21 cells); the same edge twice gives 22.
        let second = hazards_at(11, 11, 49, &s);
        assert!(
            second.len() == 21 || second.len() == 22,
            "expected 21 or 22 hazards after two shrinks, got {}",
            second.len()
        );

        // Resolving turn 75: three shrinks.
        let third = hazards_at(11, 11, 74, &s);
        assert!(third.len() > second.len());
    }

    #[test]
    fn hazards_only_grow() {
        let s = settings(25, 7);
        let mut prev: HashSet<Point> = HashSet::new();
        for board_turn in 0..300 {
            let current: HashSet<Point> =
                hazards_at(11, 11, board_turn, &s).into_iter().collect();
            assert!(
                prev.is_subset(&current),
                "hazards shrank while resolving turn {}",
                board_turn + 1
            );
            prev = current;
        }
    }

    #[test]
    fn hazards_stay_within_board_bounds() {
        let s = settings(5, 99);
        for board_turn in 0..200 {
            for p in hazards_at(7, 7, board_turn, &s) {
                assert!(
                    p.x >= 0 && p.x < 7 && p.y >= 0 && p.y < 7,
                    "hazard {p:?} out of bounds"
                );
            }
        }
    }

    #[test]
    fn hazards_have_no_duplicates() {
        let s = settings(5, 3);
        for board_turn in 0..200 {
            let hazards = hazards_at(11, 11, board_turn, &s);
            let unique: HashSet<Point> = hazards.iter().copied().collect();
            assert_eq!(hazards.len(), unique.len());
        }
    }

    #[test]
    fn deterministic_same_seed_same_sequence() {
        let s = settings(25, 1234);
        for board_turn in 0..200 {
            assert_eq!(
                hazards_at(11, 11, board_turn, &s),
                hazards_at(11, 11, board_turn, &s),
                "same seed must produce identical hazards (including order)"
            );
        }
    }

    #[test]
    fn different_seeds_diverge() {
        // Deterministic check: with several shrinks applied these two seeds
        // produce different hazard sets.
        let a = hazards_at(11, 11, 124, &settings(25, 1));
        let b = hazards_at(11, 11, 124, &settings(25, 2));
        assert_ne!(a, b);
    }

    #[test]
    fn hazards_recomputed_not_accumulated() {
        let s = settings(25, 42);
        let mut board = make_board(11, 11, vec![]);
        board.turn = 24;
        populate_hazards(&mut board, &s).unwrap();
        let first = board.hazards.clone();
        populate_hazards(&mut board, &s).unwrap();
        assert_eq!(board.hazards, first, "repeated calls must not accumulate");
    }

    #[test]
    fn board_fully_covered_after_enough_shrinks() {
        // width + height shrinks guarantee one axis is fully collapsed.
        let s = settings(1, 42);
        let mut board = make_board(7, 7, vec![]);
        board.turn = 13; // resolving turn 14 = 14 shrinks >= 7 + 7
        populate_hazards(&mut board, &s).unwrap();
        assert_eq!(board.hazards.len(), 49);
    }

    #[test]
    fn shrink_frequency_below_one_is_error() {
        let mut board = make_board(11, 11, vec![]);
        assert_eq!(
            populate_hazards(&mut board, &settings(0, 42)),
            Err(RulesError::InvalidShrinkFrequency)
        );
        assert_eq!(
            populate_hazards(&mut board, &settings(-3, 42)),
            Err(RulesError::InvalidShrinkFrequency)
        );
    }

    #[test]
    fn execute_turn_populates_hazards_and_increments_turn() {
        let std_settings = StandardSettings::default();
        let s = settings(25, 42);
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
                make_snake("two", &[(8, 8), (8, 7), (8, 6)], 100),
            ],
        );
        board.turn = 24;
        let moves = vec![
            SnakeMove {
                id: "one".to_string(),
                direction: Direction::Up,
            },
            SnakeMove {
                id: "two".to_string(),
                direction: Direction::Up,
            },
        ];

        let game_over = execute_turn(&mut board, &moves, &std_settings, &s).unwrap();
        assert!(!game_over);
        assert_eq!(board.turn, 25);
        assert_eq!(board.hazards.len(), 11, "first shrink lands on turn 25");
    }

    /// Hazards present on the board (spawned at the end of a previous turn)
    /// damage a snake whose head ends this turn in one, on top of the normal
    /// 1-health reduction. Hazards are then recomputed for the next turn.
    #[test]
    fn hazard_damage_applies_the_turn_after_spawn() {
        let std_settings = StandardSettings::default(); // hazard damage 14
        let s = settings(25, 42);
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
                make_snake("two", &[(1, 1), (1, 2), (1, 3)], 100),
            ],
        );
        board.turn = 25;
        // Hazard as if spawned at the end of turn 25.
        board.hazards.push(Point::new(5, 6));

        let moves = vec![
            SnakeMove {
                id: "one".to_string(),
                direction: Direction::Up,
            },
            SnakeMove {
                id: "two".to_string(),
                direction: Direction::Down,
            },
        ];
        execute_turn(&mut board, &moves, &std_settings, &s).unwrap();

        // 1 (starvation) + 14 (hazard) = 15 total damage.
        assert_eq!(board.snakes[0].health, 85);
        // Snake two was not in the hazard.
        assert_eq!(board.snakes[1].health, 99);

        // Hazards were recomputed for turn 26 from the seed (one shrink),
        // replacing the manually placed square.
        assert_eq!(board.turn, 26);
        assert_eq!(board.hazards, hazards_at(11, 11, 25, &s));
        assert_eq!(board.hazards.len(), 11);
    }

    /// Go order of operations: hazard damage is skipped entirely for a
    /// hazard square containing food, then the snake eats and restores to
    /// full health.
    #[test]
    fn eating_food_in_hazard_restores_full_health() {
        let std_settings = StandardSettings::default();
        let s = settings(25, 42);
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 10),
                make_snake("two", &[(1, 1), (1, 2), (1, 3)], 100),
            ],
        );
        board.turn = 25;
        let target = Point::new(5, 6);
        board.hazards.push(target);
        board.food.push(target);

        let moves = vec![
            SnakeMove {
                id: "one".to_string(),
                direction: Direction::Up,
            },
            SnakeMove {
                id: "two".to_string(),
                direction: Direction::Down,
            },
        ];
        execute_turn(&mut board, &moves, &std_settings, &s).unwrap();

        assert!(!board.snakes[0].eliminated_cause.is_eliminated());
        assert_eq!(board.snakes[0].health, SNAKE_MAX_HEALTH);
        assert_eq!(board.snakes[0].body.len(), 4);
        assert!(!board.food.contains(&target));
    }

    // === Property tests ===

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn prop_hazards_in_bounds_and_unique(
            width in 1..=25i32,
            height in 1..=25i32,
            shrink in 1..=50i32,
            seed in any::<u64>(),
            board_turn in 0..=2000i32,
        ) {
            let s = settings(shrink, seed);
            let hazards = hazards_at(width, height, board_turn, &s);
            let unique: HashSet<Point> = hazards.iter().copied().collect();
            prop_assert_eq!(hazards.len(), unique.len(), "duplicate hazards");
            for p in &hazards {
                prop_assert!(
                    p.x >= 0 && p.x < width && p.y >= 0 && p.y < height,
                    "hazard {:?} out of {}x{} bounds", p, width, height
                );
            }
        }

        #[test]
        fn prop_hazard_count_monotonic_and_growing_sets(
            width in 1..=15i32,
            height in 1..=15i32,
            shrink in 1..=10i32,
            seed in any::<u64>(),
        ) {
            let s = settings(shrink, seed);
            let mut prev: HashSet<Point> = HashSet::new();
            for board_turn in 0..150 {
                let current: HashSet<Point> =
                    hazards_at(width, height, board_turn, &s).into_iter().collect();
                prop_assert!(
                    current.len() >= prev.len(),
                    "hazard count decreased at turn {}", board_turn + 1
                );
                prop_assert!(
                    prev.is_subset(&current),
                    "hazard set shrank at turn {}", board_turn + 1
                );
                prev = current;
            }
        }

        #[test]
        fn prop_no_hazards_before_first_shrink(
            width in 1..=25i32,
            height in 1..=25i32,
            shrink in 2..=50i32,
            seed in any::<u64>(),
        ) {
            let s = settings(shrink, seed);
            for board_turn in 0..(shrink - 1) {
                prop_assert!(hazards_at(width, height, board_turn, &s).is_empty());
            }
        }

        #[test]
        fn prop_not_fully_covered_before_min_dimension_shrinks(
            width in 2..=25i32,
            height in 2..=25i32,
            shrink in 1..=10i32,
            seed in any::<u64>(),
        ) {
            // Fully covering the board requires collapsing one axis, which
            // takes at least min(width, height) shrinks on that axis.
            let s = settings(shrink, seed);
            let max_safe_shrinks = width.min(height) - 1;
            let board_turn = shrink * max_safe_shrinks - 1; // resolves exactly max_safe_shrinks shrinks
            let hazards = hazards_at(width, height, board_turn, &s);
            prop_assert!(
                (hazards.len() as i32) < width * height,
                "board fully covered after only {} shrinks", max_safe_shrinks
            );
        }

        #[test]
        fn prop_fully_covered_after_width_plus_height_shrinks(
            width in 1..=15i32,
            height in 1..=15i32,
            shrink in 1..=5i32,
            seed in any::<u64>(),
        ) {
            let s = settings(shrink, seed);
            let board_turn = shrink * (width + height) - 1;
            let hazards = hazards_at(width, height, board_turn, &s);
            prop_assert_eq!(hazards.len() as i32, width * height);
        }

        #[test]
        fn prop_deterministic(
            width in 1..=25i32,
            height in 1..=25i32,
            shrink in 1..=50i32,
            seed in any::<u64>(),
            board_turn in 0..=2000i32,
        ) {
            let s = settings(shrink, seed);
            prop_assert_eq!(
                hazards_at(width, height, board_turn, &s),
                hazards_at(width, height, board_turn, &s)
            );
        }
    }
}
