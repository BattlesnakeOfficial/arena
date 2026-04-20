/// A 2D coordinate on the game board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Manhattan distance between two points.
    pub fn manhattan_distance(self, other: Point) -> i32 {
        (self.x - other.x).abs() + (self.y - other.y).abs()
    }
}

/// Why a snake was eliminated.
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
    /// Delta for each direction: Up=(0,1), Down=(0,-1), Left=(-1,0), Right=(1,0).
    pub fn to_delta(self) -> (i32, i32) {
        match self {
            Direction::Up => (0, 1),
            Direction::Down => (0, -1),
            Direction::Left => (-1, 0),
            Direction::Right => (1, 0),
        }
    }
}

impl std::str::FromStr for Direction {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Up => write!(f, "up"),
            Direction::Down => write!(f, "down"),
            Direction::Left => write!(f, "left"),
            Direction::Right => write!(f, "right"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnakeMove {
    pub id: String,
    pub direction: Direction,
}

/// Settings for the standard game rules.
#[derive(Debug, Clone)]
pub struct StandardSettings {
    /// Percent chance of spawning food each turn (default 15).
    pub food_spawn_chance: i32,
    /// Minimum food on the board (default 1).
    pub minimum_food: i32,
    /// Health damage per hazard tile per turn (default 14).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manhattan_distance_same_point() {
        assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(0, 0)), 0);
    }

    #[test]
    fn manhattan_distance_adjacent() {
        assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(1, 0)), 1);
        assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(0, 1)), 1);
    }

    #[test]
    fn manhattan_distance_diagonal_and_far() {
        assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(1, 1)), 2);
        assert_eq!(Point::new(0, 0).manhattan_distance(Point::new(5, 5)), 10);
        assert_eq!(Point::new(3, 4).manhattan_distance(Point::new(7, 2)), 6);
    }

    #[test]
    fn manhattan_distance_negative_coords() {
        assert_eq!(Point::new(-1, -1).manhattan_distance(Point::new(1, 1)), 4);
    }
}
