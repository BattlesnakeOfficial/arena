//! Frame data serialization for the Battlesnake board viewer
//!
//! This module converts the internal game state to the PascalCase JSON format
//! expected by the board viewer.

use rules::Point;
use serde::Serialize;

use super::EngineGame;

/// Information about a snake's death
#[derive(Debug, Clone)]
pub struct DeathInfo {
    /// The snake's ID
    pub snake_id: String,
    /// The turn on which the snake died
    pub turn: i32,
    /// The cause of death (e.g., "wall-collision", "head-collision")
    pub cause: String,
    /// The ID of the snake that eliminated this snake (if applicable)
    pub eliminated_by: String,
}

/// Frame data in PascalCase format for the board viewer
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct EngineGameFrame {
    pub turn: i32,
    pub snakes: Vec<FrameSnake>,
    pub food: Vec<FrameCoord>,
    pub hazards: Vec<FrameCoord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FrameSnake {
    #[serde(rename = "ID")]
    pub id: String,
    pub name: String,
    pub body: Vec<FrameCoord>,
    pub health: i32,
    pub color: String,
    pub head_type: String,
    pub tail_type: String,
    pub latency: String,
    pub shout: String,
    pub squad: String,
    #[serde(rename = "APIVersion")]
    pub api_version: String,
    pub author: String,
    pub death: Option<FrameDeath>,
    pub eliminated_cause: String,
    pub eliminated_by: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FrameCoord {
    #[serde(rename = "X")]
    pub x: i32,
    #[serde(rename = "Y")]
    pub y: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct FrameDeath {
    pub cause: String,
    pub turn: i32,
    pub eliminated_by: String,
}

impl From<Point> for FrameCoord {
    fn from(pos: Point) -> Self {
        FrameCoord { x: pos.x, y: pos.y }
    }
}

pub struct SnakeCustomizations {
    pub color: String,
    pub head: String,
    pub tail: String,
}

use crate::snake_client::MoveResult;

/// Convert an EngineGame state to a frame for the board viewer
///
/// Includes latency info from move results when provided.
pub fn game_to_frame(
    game: &EngineGame,
    death_info: &[DeathInfo],
    move_results: &[MoveResult],
    customizations: &std::collections::HashMap<String, SnakeCustomizations>,
) -> EngineGameFrame {
    EngineGameFrame {
        turn: game.board.turn,
        snakes: game
            .board
            .snakes
            .iter()
            .map(|s| {
                let death = death_info
                    .iter()
                    .find(|d| d.snake_id == s.id)
                    .map(|d| FrameDeath {
                        cause: d.cause.clone(),
                        turn: d.turn,
                        eliminated_by: d.eliminated_by.clone(),
                    });

                let (eliminated_cause, eliminated_by) = if s.eliminated_cause.is_eliminated() {
                    death_info
                        .iter()
                        .find(|d| d.snake_id == s.id)
                        .map(|d| (d.cause.clone(), d.eliminated_by.clone()))
                        .unwrap_or_default()
                } else {
                    Default::default()
                };

                // Find latency for this snake from move results
                let latency = move_results
                    .iter()
                    .find(|r| r.snake_id == s.id)
                    .map(|r| {
                        if r.timed_out {
                            "timeout".to_string()
                        } else {
                            r.latency_ms
                                .map(|ms| ms.to_string())
                                .unwrap_or_else(|| "0".to_string())
                        }
                    })
                    .unwrap_or_else(|| "0".to_string());

                // Get shout from move result if available
                let shout = move_results
                    .iter()
                    .find(|r| r.snake_id == s.id)
                    .and_then(|r| r.shout.clone())
                    .unwrap_or_default();

                let name = game
                    .snake_names
                    .get(&s.id)
                    .cloned()
                    .unwrap_or_else(|| s.id.clone());

                FrameSnake {
                    id: s.id.clone(),
                    name,
                    body: s
                        .body
                        .iter()
                        .map(|p| FrameCoord { x: p.x, y: p.y })
                        .collect(),
                    health: s.health,
                    color: customizations
                        .get(&s.id)
                        .map(|c| c.color.clone())
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| generate_snake_color(&s.id)),
                    head_type: customizations
                        .get(&s.id)
                        .map(|c| c.head.clone())
                        .filter(|h| !h.is_empty())
                        .unwrap_or_else(|| "default".to_string()),
                    tail_type: customizations
                        .get(&s.id)
                        .map(|c| c.tail.clone())
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| "default".to_string()),
                    latency,
                    shout,
                    squad: "".to_string(),
                    api_version: "1".to_string(),
                    author: "".to_string(),
                    death,
                    eliminated_cause,
                    eliminated_by,
                }
            })
            .collect(),
        food: game.board.food.iter().map(|p| (*p).into()).collect(),
        hazards: game.board.hazards.iter().map(|p| (*p).into()).collect(),
    }
}

/// Generate a consistent color for a snake based on its ID
fn generate_snake_color(id: &str) -> String {
    // Generate a color from the hash of the ID
    let hash: u32 = id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));

    // Use the hash to generate a hue
    let hue = (hash % 360) as f32;
    let saturation: f32 = 0.7;
    let lightness: f32 = 0.5;

    // HSL to RGB conversion
    let c: f32 = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let x: f32 = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m: f32 = lightness - c / 2.0;

    let (r, g, b) = match hue as i32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    let r = ((r + m) * 255.0) as u8;
    let g = ((g + m) * 255.0) as u8;
    let b = ((b + m) * 255.0) as u8;

    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::GameMeta;
    use rules::{BoardState, EliminationCause, Snake, StandardSettings};

    #[test]
    fn test_frame_coord_serialization() {
        let coord = FrameCoord { x: 5, y: 10 };
        let json = serde_json::to_string(&coord).unwrap();
        assert!(json.contains("\"X\":5"));
        assert!(json.contains("\"Y\":10"));
    }

    #[test]
    fn test_generate_snake_color() {
        let color1 = generate_snake_color("snake-1");
        let color2 = generate_snake_color("snake-2");

        // Colors should be different for different IDs
        assert_ne!(color1, color2);

        // Color should be consistent
        assert_eq!(color1, generate_snake_color("snake-1"));

        // Should be a valid hex color
        assert!(color1.starts_with('#'));
        assert_eq!(color1.len(), 7);
    }

    #[test]
    fn test_death_info_struct() {
        let death = DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 42,
            cause: "wall-collision".to_string(),
            eliminated_by: "snake-2".to_string(),
        };

        assert_eq!(death.snake_id, "snake-1");
        assert_eq!(death.turn, 42);
        assert_eq!(death.cause, "wall-collision");
        assert_eq!(death.eliminated_by, "snake-2");
    }

    #[test]
    fn test_death_info_clone() {
        let death = DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 10,
            cause: "head-collision".to_string(),
            eliminated_by: "snake-2".to_string(),
        };

        let cloned = death.clone();
        assert_eq!(death.snake_id, cloned.snake_id);
        assert_eq!(death.turn, cloned.turn);
        assert_eq!(death.cause, cloned.cause);
        assert_eq!(death.eliminated_by, cloned.eliminated_by);
    }

    #[test]
    fn test_game_to_frame_basic() {
        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];

        let frame = game_to_frame(&game, &death_info, &[], &std::collections::HashMap::new());

        assert_eq!(frame.turn, 0);
        assert_eq!(frame.snakes.len(), 1);
        assert_eq!(frame.snakes[0].id, "snake-1");
        assert_eq!(frame.snakes[0].name, "Test Snake");
        assert_eq!(frame.snakes[0].health, 100);
        assert!(frame.snakes[0].death.is_none());
        assert_eq!(frame.snakes[0].eliminated_cause, "");
        assert_eq!(frame.snakes[0].eliminated_by, "");
    }

    #[test]
    fn test_game_to_frame_with_death_info() {
        let mut game = create_test_game();
        game.board.snakes[0].eliminated_cause = EliminationCause::OutOfBounds;

        let death_info = vec![DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 5,
            cause: "wall-collision".to_string(),
            eliminated_by: "".to_string(),
        }];

        let frame = game_to_frame(&game, &death_info, &[], &std::collections::HashMap::new());

        assert_eq!(frame.snakes.len(), 1);
        assert!(frame.snakes[0].death.is_some());
        let death = frame.snakes[0].death.as_ref().unwrap();
        assert_eq!(death.cause, "wall-collision");
        assert_eq!(death.turn, 5);
        assert_eq!(frame.snakes[0].eliminated_cause, "wall-collision");
    }

    #[test]
    fn test_game_to_frame_with_eliminated_by() {
        let mut game = create_test_game();
        game.board.snakes[0].eliminated_cause = EliminationCause::HeadToHeadCollision;

        let death_info = vec![DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 10,
            cause: "head-collision".to_string(),
            eliminated_by: "snake-2".to_string(),
        }];

        let frame = game_to_frame(&game, &death_info, &[], &std::collections::HashMap::new());

        let death = frame.snakes[0].death.as_ref().unwrap();
        assert_eq!(death.eliminated_by, "snake-2");
        assert_eq!(frame.snakes[0].eliminated_by, "snake-2");
    }

    #[test]
    fn test_game_to_frame_multiple_snakes() {
        let mut game = create_test_game();
        // Add a second snake
        game.board.snakes.push(Snake {
            id: "snake-2".to_string(),
            body: vec![Point::new(3, 3), Point::new(3, 2), Point::new(3, 1)],
            health: 80,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        });
        game.snake_names
            .insert("snake-2".to_string(), "Second Snake".to_string());

        let death_info: Vec<DeathInfo> = vec![];
        let frame = game_to_frame(&game, &death_info, &[], &std::collections::HashMap::new());

        assert_eq!(frame.snakes.len(), 2);
        assert_eq!(frame.snakes[0].id, "snake-1");
        assert_eq!(frame.snakes[1].id, "snake-2");
        assert_eq!(frame.snakes[1].health, 80);
    }

    #[test]
    fn test_game_to_frame_with_food() {
        let mut game = create_test_game();
        game.board.food = vec![Point::new(5, 5), Point::new(7, 7)];

        let frame = game_to_frame(&game, &[], &[], &std::collections::HashMap::new());

        assert_eq!(frame.food.len(), 2);
        assert_eq!(frame.food[0].x, 5);
        assert_eq!(frame.food[0].y, 5);
        assert_eq!(frame.food[1].x, 7);
        assert_eq!(frame.food[1].y, 7);
    }

    #[test]
    fn test_game_to_frame_with_hazards() {
        let mut game = create_test_game();
        game.board.hazards = vec![Point::new(0, 0), Point::new(10, 10)];

        let frame = game_to_frame(&game, &[], &[], &std::collections::HashMap::new());

        assert_eq!(frame.hazards.len(), 2);
        assert_eq!(frame.hazards[0].x, 0);
        assert_eq!(frame.hazards[0].y, 0);
    }

    #[test]
    fn test_game_to_frame_snake_body_coords() {
        let game = create_test_game();
        let frame = game_to_frame(&game, &[], &[], &std::collections::HashMap::new());

        // Snake body should be converted to FrameCoords
        assert_eq!(frame.snakes[0].body.len(), 3);
        assert_eq!(frame.snakes[0].body[0].x, 5);
        assert_eq!(frame.snakes[0].body[0].y, 5);
    }

    #[test]
    fn test_game_to_frame_alive_snake_no_death() {
        let game = create_test_game();
        // Even if there's death_info for this snake, if not eliminated, no eliminated fields
        let death_info = vec![DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 5,
            cause: "test".to_string(),
            eliminated_by: "".to_string(),
        }];

        let frame = game_to_frame(&game, &death_info, &[], &std::collections::HashMap::new());

        // Death info is still attached (for replay purposes)
        assert!(frame.snakes[0].death.is_some());
        // But eliminated_cause/eliminated_by are empty since snake is alive
        assert_eq!(frame.snakes[0].eliminated_cause, "");
        assert_eq!(frame.snakes[0].eliminated_by, "");
    }

    #[test]
    fn test_frame_snake_serialization() {
        let game = create_test_game();
        let frame = game_to_frame(&game, &[], &[], &std::collections::HashMap::new());

        let json = serde_json::to_string(&frame).unwrap();

        // Check PascalCase serialization
        assert!(json.contains("\"Turn\":"));
        assert!(json.contains("\"Snakes\":"));
        assert!(json.contains("\"Food\":"));
        assert!(json.contains("\"Hazards\":"));
        assert!(json.contains("\"ID\":"));
        assert!(json.contains("\"Name\":"));
        assert!(json.contains("\"Body\":"));
        assert!(json.contains("\"Health\":"));
    }

    #[test]
    fn test_frame_death_serialization() {
        let death = FrameDeath {
            cause: "wall-collision".to_string(),
            turn: 42,
            eliminated_by: "snake-2".to_string(),
        };

        let json = serde_json::to_string(&death).unwrap();

        assert!(json.contains("\"Cause\":\"wall-collision\""));
        assert!(json.contains("\"Turn\":42"));
        assert!(json.contains("\"EliminatedBy\":\"snake-2\""));
    }

    #[test]
    fn test_game_to_frame_latency_basic() {
        use crate::snake_client::MoveResult;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: rules::Direction::Up,
            latency_ms: Some(42),
            timed_out: false,
            shout: None,
        }];

        let frame = game_to_frame(
            &game,
            &death_info,
            &move_results,
            &std::collections::HashMap::new(),
        );

        assert_eq!(frame.snakes[0].latency, "42");
    }

    #[test]
    fn test_game_to_frame_latency_timeout() {
        use crate::snake_client::MoveResult;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: rules::Direction::Up,
            latency_ms: None,
            timed_out: true,
            shout: None,
        }];

        let frame = game_to_frame(
            &game,
            &death_info,
            &move_results,
            &std::collections::HashMap::new(),
        );

        assert_eq!(frame.snakes[0].latency, "timeout");
    }

    #[test]
    fn test_game_to_frame_shout_from_move_result() {
        use crate::snake_client::MoveResult;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: rules::Direction::Up,
            latency_ms: Some(100),
            timed_out: false,
            shout: Some("Hello from move!".to_string()),
        }];

        let frame = game_to_frame(
            &game,
            &death_info,
            &move_results,
            &std::collections::HashMap::new(),
        );

        // Shout from move result should be used
        assert_eq!(frame.snakes[0].shout, "Hello from move!");
    }

    #[test]
    fn test_game_to_frame_no_matching_latency_result() {
        use crate::snake_client::MoveResult;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        // Move result for a different snake
        let move_results = vec![MoveResult {
            snake_id: "other-snake".to_string(),
            direction: rules::Direction::Down,
            latency_ms: Some(50),
            timed_out: false,
            shout: None,
        }];

        let frame = game_to_frame(
            &game,
            &death_info,
            &move_results,
            &std::collections::HashMap::new(),
        );

        // Should default to "0" when no matching result
        assert_eq!(frame.snakes[0].latency, "0");
    }

    fn create_test_game() -> EngineGame {
        let snake = Snake {
            id: "snake-1".to_string(),
            body: vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)],
            health: 100,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        };

        let mut snake_names = std::collections::HashMap::new();
        snake_names.insert("snake-1".to_string(), "Test Snake".to_string());

        EngineGame {
            board: BoardState {
                turn: 0,
                width: 11,
                height: 11,
                food: vec![],
                snakes: vec![snake],
                hazards: vec![],
            },
            meta: GameMeta {
                game_id: "test-game".to_string(),
                ruleset_name: "standard".to_string(),
                timeout: 500,
                settings: StandardSettings::default(),
            },
            snake_names,
        }
    }

    // === Snake customization tests ===

    #[test]
    fn test_game_to_frame_uses_customizations() {
        use std::collections::HashMap;
        let game = create_test_game();
        let mut custom = HashMap::new();
        custom.insert(
            "snake-1".to_string(),
            SnakeCustomizations {
                color: "#ff0000".to_string(),
                head: "bendr".to_string(),
                tail: "fat-rattle".to_string(),
            },
        );
        let frame = game_to_frame(&game, &[], &[], &custom);
        assert_eq!(frame.snakes[0].color, "#ff0000", "should use custom color");
        assert_eq!(frame.snakes[0].head_type, "bendr", "should use custom head");
        assert_eq!(
            frame.snakes[0].tail_type, "fat-rattle",
            "should use custom tail"
        );
    }

    #[test]
    fn test_game_to_frame_falls_back_when_no_customizations() {
        use std::collections::HashMap;
        let game = create_test_game();
        let frame = game_to_frame(&game, &[], &[], &HashMap::new());
        assert!(
            !frame.snakes[0].color.is_empty(),
            "fallback color should not be empty"
        );
        assert!(
            frame.snakes[0].color.starts_with('#'),
            "fallback color should be a hex color"
        );
        assert_eq!(
            frame.snakes[0].head_type, "default",
            "fallback head should be 'default'"
        );
        assert_eq!(
            frame.snakes[0].tail_type, "default",
            "fallback tail should be 'default'"
        );
    }

    #[test]
    fn test_game_to_frame_empty_customization_fields_use_defaults() {
        use std::collections::HashMap;
        let game = create_test_game();
        let mut custom = HashMap::new();
        custom.insert(
            "snake-1".to_string(),
            SnakeCustomizations {
                color: "".to_string(),
                head: "".to_string(),
                tail: "".to_string(),
            },
        );
        let frame = game_to_frame(&game, &[], &[], &custom);
        assert!(
            !frame.snakes[0].color.is_empty(),
            "empty color should fall back to generated color"
        );
        assert!(
            frame.snakes[0].color.starts_with('#'),
            "fallback color should be a hex color"
        );
        assert_eq!(
            frame.snakes[0].head_type, "default",
            "empty head should fall back to 'default'"
        );
        assert_eq!(
            frame.snakes[0].tail_type, "default",
            "empty tail should fall back to 'default'"
        );
    }

    #[test]
    fn test_game_to_frame_partial_customization() {
        use std::collections::HashMap;
        let game = create_test_game();
        let mut custom = HashMap::new();
        custom.insert(
            "snake-1".to_string(),
            SnakeCustomizations {
                color: "#00ff00".to_string(),
                head: "".to_string(),
                tail: "curled".to_string(),
            },
        );
        let frame = game_to_frame(&game, &[], &[], &custom);
        assert_eq!(
            frame.snakes[0].color, "#00ff00",
            "non-empty color should be used"
        );
        assert_eq!(
            frame.snakes[0].head_type, "default",
            "empty head should fall back to 'default'"
        );
        assert_eq!(
            frame.snakes[0].tail_type, "curled",
            "non-empty tail should be used"
        );
    }

    #[test]
    fn test_game_to_frame_customizations_dont_affect_other_fields() {
        use std::collections::HashMap;
        let game = create_test_game();
        let mut custom = HashMap::new();
        custom.insert(
            "snake-1".to_string(),
            SnakeCustomizations {
                color: "#ff0000".to_string(),
                head: "bendr".to_string(),
                tail: "fat-rattle".to_string(),
            },
        );
        let frame = game_to_frame(&game, &[], &[], &custom);
        assert_eq!(frame.snakes[0].id, "snake-1", "id should be preserved");
        assert_eq!(
            frame.snakes[0].name, "Test Snake",
            "name should be preserved"
        );
        assert_eq!(frame.snakes[0].health, 100, "health should be preserved");
        assert_eq!(
            frame.snakes[0].body.len(),
            3,
            "body length should be preserved"
        );
    }
}
