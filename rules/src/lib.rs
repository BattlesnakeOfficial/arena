pub mod board;
pub mod food;
pub mod standard;
pub mod types;

pub use types::{
    BOARD_SIZE_MEDIUM, BoardState, Direction, EliminationCause, Point, RulesError,
    SNAKE_MAX_HEALTH, SNAKE_START_SIZE, Snake, SnakeMove, StandardSettings,
};

#[cfg(test)]
mod tests;
