pub mod board;
pub mod food;
pub mod standard;
pub mod types;

pub use types::{
    BOARD_SIZE_MEDIUM, BoardState, Direction, EliminationCause, Point, RulesError,
    SNAKE_MAX_HEALTH, SNAKE_START_SIZE, Snake, SnakeMove, StandardSettings,
};

#[cfg(test)]
pub(crate) mod test_utils {
    use super::*;

    pub fn make_snake(id: &str, body: &[(i32, i32)], health: i32) -> Snake {
        Snake {
            id: id.to_string(),
            body: body.iter().map(|(x, y)| Point::new(*x, *y)).collect(),
            health,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        }
    }

    pub fn make_board(width: i32, height: i32, snakes: Vec<Snake>) -> BoardState {
        BoardState {
            turn: 0,
            width,
            height,
            food: Vec::new(),
            snakes,
            hazards: Vec::new(),
        }
    }
}
