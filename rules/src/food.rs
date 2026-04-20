use rand::Rng;
use rand::seq::SliceRandom;

use crate::board::get_unoccupied_points;
use crate::types::*;

/// Mid-game food spawning.
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

/// Place N food at random unoccupied positions, re-checking occupancy after each.
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
    use crate::test_utils::make_board;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn spawn_food_minimum() {
        let settings = StandardSettings {
            minimum_food: 3,
            food_spawn_chance: 0,
            ..StandardSettings::default()
        };

        let mut rng = StdRng::seed_from_u64(42);
        let mut board = make_board(11, 11, vec![]);

        // No food -- should spawn up to minimum
        maybe_spawn_food(&mut rng, &mut board, &settings);
        assert_eq!(board.food.len(), 3);

        // Already at minimum -- no spawn (chance is 0)
        maybe_spawn_food(&mut rng, &mut board, &settings);
        assert_eq!(board.food.len(), 3);
    }

    #[test]
    fn spawn_food_zero_chance() {
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

    /// `food_spawn_chance=100` is 99% per roll (fails when RNG returns 0).
    /// Over 100 iterations, expect >= 95 spawns.
    #[test]
    fn spawn_food_hundred_chance() {
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
        assert!(
            board.food.len() >= 95,
            "expected >= 95 food spawns, got {}",
            board.food.len()
        );
    }

    #[test]
    fn spawn_food_half_chance() {
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

        let food_count = board.food.len();
        assert!(
            (350..=650).contains(&food_count),
            "expected ~490 food spawns from 1000 iterations, got {food_count}"
        );
    }
}
