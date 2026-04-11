use rand::Rng;
use rand::seq::SliceRandom;

use crate::board::get_unoccupied_points;
use crate::types::*;

/// Mid-game food spawning. Go: `maps/standard.go PostUpdateBoard`.
///
/// Logic:
/// 1. If food count < `minimum_food`: spawn `(minimum_food - count)` food
/// 2. Else if `food_spawn_chance > 0`:
///    Roll: `(100 - rng.gen_range(0..100)) < food_spawn_chance`
///    If true: spawn 1 food
///
/// Spawning uses `get_unoccupied_points(board, false, false)`, shuffled, then
/// takes first N. Note: `food_spawn_chance=100` is 99% per roll (fails when
/// RNG returns 0).
pub fn maybe_spawn_food(rng: &mut impl Rng, board: &mut BoardState, settings: &StandardSettings) {
    let current_food = board.food.len() as i32;
    let food_needed;

    if current_food < settings.minimum_food {
        food_needed = settings.minimum_food - current_food;
    } else if settings.food_spawn_chance > 0
        && (100 - rng.gen_range(0..100)) < settings.food_spawn_chance
    {
        food_needed = 1;
    } else {
        return;
    }

    let mut unoccupied = get_unoccupied_points(board, false, false);
    unoccupied.shuffle(rng);

    let to_place = (food_needed as usize).min(unoccupied.len());
    for p in unoccupied.into_iter().take(to_place) {
        board.food.push(p);
    }
}

/// Legacy per-food placement. Go: `board.go PlaceFoodRandomly`.
///
/// Calls `get_unoccupied_points(board, false, false)` FRESH for each food
/// placement (unlike `maybe_spawn_food` which shuffles once). Exists for
/// test parity.
pub fn place_food_randomly(rng: &mut impl Rng, board: &mut BoardState, n: usize) {
    for _ in 0..n {
        let mut unoccupied = get_unoccupied_points(board, false, false);
        if unoccupied.is_empty() {
            return;
        }
        unoccupied.shuffle(rng);
        board.food.push(unoccupied[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::test_helpers::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use std::collections::HashSet;

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
}
