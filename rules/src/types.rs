/// Board coordinate, matching Go `Point{X, Y int}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Equivalent to Go's `getDistanceBetweenPoints` in `board.go`.
    pub fn manhattan_distance(self, other: Point) -> i32 {
        (self.x - other.x).abs() + (self.y - other.y).abs()
    }
}

/// Elimination cause, matching Go constants from `constants.go:22-29`.
///
/// - `NotEliminated` -> `""` (Go `NotEliminated`)
/// - `OutOfHealth` -> `"out-of-health"` (Go `EliminatedByOutOfHealth`)
/// - `OutOfBounds` -> `"wall-collision"` (Go `EliminatedByOutOfBounds`)
/// - `SelfCollision` -> `"snake-self-collision"` (Go `EliminatedBySelfCollision`)
/// - `Collision` -> `"snake-collision"` (Go `EliminatedByCollision`)
/// - `HeadToHeadCollision` -> `"head-collision"` (Go `EliminatedByHeadToHeadCollision`)
/// - `Hazard` -> `"hazard"` (Go `EliminatedByHazard`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EliminationCause {
    NotEliminated,
    OutOfHealth,
    OutOfBounds,
    SelfCollision,
    Collision,
    HeadToHeadCollision,
    Hazard,
}

impl EliminationCause {
    pub fn is_eliminated(&self) -> bool {
        !matches!(self, EliminationCause::NotEliminated)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EliminationCause::NotEliminated => "",
            EliminationCause::OutOfHealth => "out-of-health",
            EliminationCause::OutOfBounds => "wall-collision",
            EliminationCause::SelfCollision => "snake-self-collision",
            EliminationCause::Collision => "snake-collision",
            EliminationCause::HeadToHeadCollision => "head-collision",
            EliminationCause::Hazard => "hazard",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snake {
    pub id: String,
    pub body: Vec<Point>,
    pub health: i32,
    pub eliminated_cause: EliminationCause,
    pub eliminated_by: String,
    pub eliminated_on_turn: i32,
}

impl Snake {
    /// # Panics
    ///
    /// Panics on empty body. `move_snakes` returns
    /// `Err(RulesError::ZeroLengthSnake)` before calling this.
    pub fn head(&self) -> Point {
        self.body[0]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardState {
    pub turn: i32,
    pub width: i32,
    pub height: i32,
    pub food: Vec<Point>,
    pub snakes: Vec<Snake>,
    pub hazards: Vec<Point>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    /// Matches Go: Up=(0,1), Down=(0,-1), Left=(-1,0), Right=(1,0).
    pub fn to_delta(self) -> (i32, i32) {
        match self {
            Direction::Up => (0, 1),
            Direction::Down => (0, -1),
            Direction::Left => (-1, 0),
            Direction::Right => (1, 0),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnakeMove {
    pub id: String,
    pub direction: Direction,
}

/// Settings for standard game mode.
#[derive(Debug, Clone)]
pub struct StandardSettings {
    /// Percent chance of spawning food each turn (default 15).
    pub food_spawn_chance: i32,
    /// Minimum food on the board (default 1).
    pub minimum_food: i32,
    /// Health damage per hazard tile per turn (default 14).
    ///
    /// Go CLI defaults to 14; the arena uses 15 — a known discrepancy.
    pub hazard_damage_per_turn: i32,
}

impl Default for StandardSettings {
    fn default() -> Self {
        Self {
            food_spawn_chance: 15,
            minimum_food: 1,
            hazard_damage_per_turn: 14,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesError {
    NoMoveFound(String),
    ZeroLengthSnake(String),
    NoRoomForFood,
}

pub const SNAKE_MAX_HEALTH: i32 = 100;
pub const SNAKE_START_SIZE: usize = 3;
pub const BOARD_SIZE_MEDIUM: i32 = 11;
