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
