//! Conversion between rules types and `battlesnake_game_types::wire_representation`.

use std::collections::VecDeque;

use battlesnake_game_types::wire_representation;

use crate::types::*;

impl From<wire_representation::Position> for Point {
    fn from(p: wire_representation::Position) -> Self {
        Point::new(p.x, p.y)
    }
}

impl From<Point> for wire_representation::Position {
    fn from(p: Point) -> Self {
        wire_representation::Position { x: p.x, y: p.y }
    }
}

impl BoardState {
    /// Build a `BoardState` from a wire `Game`.
    ///
    /// `wire_representation::Board` uses `u32` for `width`/`height`; cast as
    /// `i32` (same pattern as `server/src/engine/mod.rs:335-336`).
    pub fn from_wire_game(game: &wire_representation::Game) -> Self {
        let board = &game.board;
        let snakes = board
            .snakes
            .iter()
            .map(|ws| Snake {
                id: ws.id.clone(),
                body: ws.body.iter().map(|p| Point::new(p.x, p.y)).collect(),
                health: ws.health,
                eliminated_cause: EliminationCause::NotEliminated,
                eliminated_by: String::new(),
                eliminated_on_turn: 0,
            })
            .collect();

        BoardState {
            turn: game.turn,
            width: board.width as i32,
            height: board.height as i32,
            food: board.food.iter().map(|p| Point::new(p.x, p.y)).collect(),
            snakes,
            hazards: board.hazards.iter().map(|p| Point::new(p.x, p.y)).collect(),
        }
    }

    /// Convert to a wire `Board`.
    ///
    /// `i32` `width`/`height` cast as `u32`.
    pub fn to_wire_board(&self) -> wire_representation::Board {
        let snakes = self
            .snakes
            .iter()
            .map(|s| {
                let body: VecDeque<wire_representation::Position> = s
                    .body
                    .iter()
                    .map(|p| wire_representation::Position { x: p.x, y: p.y })
                    .collect();
                let head = body
                    .front()
                    .copied()
                    .unwrap_or(wire_representation::Position { x: 0, y: 0 });
                wire_representation::BattleSnake {
                    id: s.id.clone(),
                    name: s.id.clone(),
                    head,
                    body,
                    health: s.health,
                    shout: None,
                    actual_length: None,
                }
            })
            .collect();

        wire_representation::Board {
            height: self.height as u32,
            width: self.width as u32,
            food: self
                .food
                .iter()
                .map(|p| wire_representation::Position { x: p.x, y: p.y })
                .collect(),
            snakes,
            hazards: self
                .hazards
                .iter()
                .map(|p| wire_representation::Position { x: p.x, y: p.y })
                .collect(),
        }
    }
}

impl From<battlesnake_game_types::types::Move> for Direction {
    fn from(m: battlesnake_game_types::types::Move) -> Self {
        match m {
            battlesnake_game_types::types::Move::Up => Direction::Up,
            battlesnake_game_types::types::Move::Down => Direction::Down,
            battlesnake_game_types::types::Move::Left => Direction::Left,
            battlesnake_game_types::types::Move::Right => Direction::Right,
        }
    }
}

impl From<Direction> for battlesnake_game_types::types::Move {
    fn from(d: Direction) -> Self {
        match d {
            Direction::Up => battlesnake_game_types::types::Move::Up,
            Direction::Down => battlesnake_game_types::types::Move::Down,
            Direction::Left => battlesnake_game_types::types::Move::Left,
            Direction::Right => battlesnake_game_types::types::Move::Right,
        }
    }
}
