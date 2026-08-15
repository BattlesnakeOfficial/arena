//! Solo mode: a single-snake survival game.
//!
//! Ported from the canonical Go implementation
//! (`BattlesnakeOfficial/rules`, `solo.go`). Upstream `GameTypeSolo =
//! "solo"` is a first-class ruleset whose only difference from standard is
//! the pre-turn game-over stage (`StageGameOverSoloSnake`): the game runs
//! the full standard movement / starvation / hazard-damage / feed /
//! elimination pipeline and ends only when NO snake is still alive (in a
//! Solo game there is exactly one) instead of when fewer than two remain.
//!
//! The snake survives as long as possible; starvation, wall collision, and
//! self-collision are the reachable death causes (there is no other snake
//! to collide with and no hazard-producing mode).

use crate::standard;
use crate::types::*;

/// Check if the game is over (no live snakes remaining).
///
/// Solo variant of the game-over predicate: `standard::is_game_over` ends a
/// game once `alive <= 1`, which would end a single-snake game before its
/// first turn. Solo games end only when every snake has been eliminated.
pub fn is_game_over(board: &BoardState) -> bool {
    board
        .snakes
        .iter()
        .all(|s| s.eliminated_cause.is_eliminated())
}

/// Execute one turn of the Solo pipeline.
///
/// Returns `true` if the game was already over BEFORE processing (early
/// exit), mirroring [`standard::execute_turn`].
///
/// Pipeline order (Go `soloRulesetStages` -- the standard stages with the
/// solo game-over predicate):
///   1. Solo `is_game_over` check
///   2. `move_snakes`
///   3. `reduce_snake_health`
///   4. `damage_hazards`
///   5. `feed_snakes`
///   6. `eliminate_snakes`
///   7. `board.turn += 1`
///
/// NOTE: food spawning (`maybe_spawn_food`) is NOT in this pipeline --
/// caller invokes it after, exactly as with `standard::execute_turn`.
pub fn execute_turn(
    board: &mut BoardState,
    moves: &[SnakeMove],
    settings: &StandardSettings,
) -> Result<bool, RulesError> {
    if is_game_over(board) {
        return Ok(true);
    }

    standard::move_snakes(board, moves)?;
    standard::reduce_snake_health(board);
    standard::damage_hazards(board, settings);
    standard::feed_snakes(board);
    standard::eliminate_snakes(board)?;

    board.turn += 1;

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::eliminate_snake;
    use crate::test_utils::{make_board, make_snake};

    fn up(id: &str) -> SnakeMove {
        SnakeMove {
            id: id.to_string(),
            direction: Direction::Up,
        }
    }

    #[test]
    fn one_live_snake_is_not_game_over() {
        // The core Solo property: a single live snake does NOT trigger the
        // game-over predicate (standard rules would already be over).
        let board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        assert!(!is_game_over(&board));
    }

    #[test]
    fn all_eliminated_is_game_over() {
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );
        eliminate_snake(&mut board.snakes[0], EliminationCause::OutOfHealth, "", 1);
        assert!(is_game_over(&board));

        // Multiple snakes, all eliminated (Go parity: {} and all-eliminated
        // boards are over).
        let mut board = make_board(
            11,
            11,
            vec![
                make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100),
                make_snake("two", &[(8, 8), (8, 7), (8, 6)], 100),
            ],
        );
        eliminate_snake(&mut board.snakes[0], EliminationCause::OutOfBounds, "", 1);
        eliminate_snake(&mut board.snakes[1], EliminationCause::OutOfBounds, "", 1);
        assert!(is_game_over(&board));

        // No snakes at all: over (Go's empty-board case).
        assert!(is_game_over(&make_board(11, 11, vec![])));
    }

    #[test]
    fn live_snake_processes_a_turn() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 100)],
        );

        let over = execute_turn(&mut board, &[up("one")], &settings).unwrap();
        assert!(!over);
        assert_eq!(board.turn, 1);
        assert_eq!(board.snakes[0].head(), Point::new(5, 6));
        assert_eq!(board.snakes[0].health, 99);
    }

    #[test]
    fn starvation_eliminates_sole_snake_and_ends_game() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(5, 5), (5, 4), (5, 3)], 1)],
        );

        let over = execute_turn(&mut board, &[up("one")], &settings).unwrap();
        assert!(!over, "the death turn itself still processes");
        assert_eq!(
            board.snakes[0].eliminated_cause,
            EliminationCause::OutOfHealth
        );
        assert!(is_game_over(&board));

        // The next call early-exits without advancing the turn.
        let already_over = execute_turn(&mut board, &[up("one")], &settings).unwrap();
        assert!(already_over);
        assert_eq!(board.turn, 1, "turn must not advance after game over");
    }

    #[test]
    fn wall_collision_eliminates_sole_snake() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            11,
            vec![make_snake("one", &[(0, 5), (1, 5), (2, 5)], 100)],
        );

        let moves = vec![SnakeMove {
            id: "one".to_string(),
            direction: Direction::Left,
        }];
        execute_turn(&mut board, &moves, &settings).unwrap();
        assert_eq!(
            board.snakes[0].eliminated_cause,
            EliminationCause::OutOfBounds
        );
        assert!(is_game_over(&board));
    }

    #[test]
    fn self_collision_eliminates_sole_snake() {
        let settings = StandardSettings::default();
        let mut board = make_board(
            11,
            11,
            vec![make_snake(
                "one",
                &[(5, 5), (5, 6), (6, 6), (6, 5), (5, 5)],
                100,
            )],
        );

        let moves = vec![SnakeMove {
            id: "one".to_string(),
            direction: Direction::Right,
        }];
        execute_turn(&mut board, &moves, &settings).unwrap();
        assert_eq!(
            board.snakes[0].eliminated_cause,
            EliminationCause::SelfCollision
        );
        assert!(is_game_over(&board));
    }

    /// Port of Go's `soloCaseNotOver`: a lone snake moves, eats, grows, and
    /// the game is NOT over afterwards.
    #[test]
    fn solo_case_not_over() {
        let settings = StandardSettings::default();
        let mut board = make_board(10, 10, vec![make_snake("one", &[(1, 1), (1, 2)], 100)]);
        board.food = vec![Point::new(0, 0), Point::new(1, 0)];

        let moves = vec![SnakeMove {
            id: "one".to_string(),
            direction: Direction::Down,
        }];
        let over = execute_turn(&mut board, &moves, &settings).unwrap();

        assert!(!over);
        assert!(!is_game_over(&board));
        // Head moved down onto the food at (1, 0) and ate it.
        assert_eq!(board.snakes[0].head(), Point::new(1, 0));
        assert_eq!(board.snakes[0].health, SNAKE_MAX_HEALTH);
        assert_eq!(board.snakes[0].body.len(), 3);
        assert_eq!(board.food, vec![Point::new(0, 0)]);
    }

    /// Port of Go's `TestSoloCreateNextBoardStateSanity`: an empty board is
    /// immediately over.
    #[test]
    fn empty_board_is_immediately_over() {
        let settings = StandardSettings::default();
        let mut board = make_board(11, 11, vec![]);

        let over = execute_turn(&mut board, &[], &settings).unwrap();
        assert!(over);
    }
}
