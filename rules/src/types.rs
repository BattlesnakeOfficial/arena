use serde::{Deserialize, Serialize};

/// A 2D coordinate on the game board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    /// The arena frame/viewer vocabulary: the death-cause string persisted
    /// in game frames (and consumed by the board viewer, game pages and
    /// highlight detectors). MUST NOT change — stored frames depend on it.
    /// `NotEliminated` is `""` (alive snakes carry no `Death`).
    pub fn arena_slug(&self) -> &'static str {
        match self {
            EliminationCause::NotEliminated => "",
            EliminationCause::OutOfHealth => "out-of-health",
            EliminationCause::OutOfBounds => "wall-collision",
            EliminationCause::SelfCollision => "self-collision",
            EliminationCause::Collision => "snake-collision",
            EliminationCause::HeadToHeadCollision => "head-collision",
            EliminationCause::Hazard => "hazard",
        }
    }

    /// The Go rules vocabulary (`rules/constants.go`), used by the
    /// battlesnake CLI for board parity with play.battlesnake.com.
    /// Identical to [`Self::arena_slug`] except `SelfCollision` →
    /// `"snake-self-collision"`.
    pub fn go_slug(&self) -> &'static str {
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

impl BoardState {
    /// Hazard entries that are actually on the board.
    ///
    /// Snail Mode (see [`crate::snail`]) stores pending-trail bookkeeping as
    /// out-of-bounds points inside `hazards` (Go map parity). Anything
    /// user-facing -- snake /move payloads, board-viewer frames -- must use
    /// this view instead of raw `hazards` so bookkeeping points never leak.
    /// Duplicates (stacked hazards) are preserved.
    pub fn on_board_hazards(&self) -> impl Iterator<Item = &Point> {
        self.hazards
            .iter()
            .filter(|p| p.x >= 0 && p.x < self.width && p.y >= 0 && p.y < self.height)
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Royale games can't shrink more frequently than every turn
    /// (`shrink_every_n_turns < 1`).
    InvalidShrinkFrequency,
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

    #[test]
    fn arena_slug_table() {
        use EliminationCause::*;
        assert_eq!(NotEliminated.arena_slug(), "");
        assert_eq!(OutOfHealth.arena_slug(), "out-of-health");
        assert_eq!(OutOfBounds.arena_slug(), "wall-collision");
        assert_eq!(SelfCollision.arena_slug(), "self-collision");
        assert_eq!(Collision.arena_slug(), "snake-collision");
        assert_eq!(HeadToHeadCollision.arena_slug(), "head-collision");
        assert_eq!(Hazard.arena_slug(), "hazard");
    }

    #[test]
    fn go_slug_table() {
        use EliminationCause::*;
        assert_eq!(NotEliminated.go_slug(), "");
        assert_eq!(OutOfHealth.go_slug(), "out-of-health");
        assert_eq!(OutOfBounds.go_slug(), "wall-collision");
        assert_eq!(SelfCollision.go_slug(), "snake-self-collision");
        assert_eq!(Collision.go_slug(), "snake-collision");
        assert_eq!(HeadToHeadCollision.go_slug(), "head-collision");
        assert_eq!(Hazard.go_slug(), "hazard");
    }

    /// The two vocabularies differ ONLY for SelfCollision: arena frames
    /// say "self-collision", Go's rules (and the CLI's parity output) say
    /// "snake-self-collision".
    #[test]
    fn slugs_differ_only_for_self_collision() {
        use EliminationCause::*;
        for cause in [
            NotEliminated,
            OutOfHealth,
            OutOfBounds,
            SelfCollision,
            Collision,
            HeadToHeadCollision,
            Hazard,
        ] {
            let expected = if cause == SelfCollision {
                cause.arena_slug() != cause.go_slug()
            } else {
                cause.arena_slug() == cause.go_slug()
            };
            assert!(expected, "{cause:?}: slug agreement is wrong");
        }
    }
}
