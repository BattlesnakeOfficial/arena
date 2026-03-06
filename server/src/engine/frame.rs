//! Frame data serialization for the Battlesnake board viewer
//!
//! This module converts the internal game state to the PascalCase JSON format
//! expected by the board viewer.

use battlesnake_game_types::wire_representation::{Game, Position};
use serde::Serialize;
use std::collections::VecDeque;

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
    /// TODO: Pass eliminated_by from the game engine once head-to-head collision tracking is implemented
    pub eliminated_by: String,
}

/// Convert a VecDeque of Positions to a Vec of FrameCoords
fn body_to_coords(body: &VecDeque<Position>) -> Vec<FrameCoord> {
    body.iter().map(|p| FrameCoord { x: p.x, y: p.y }).collect()
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

impl From<Position> for FrameCoord {
    fn from(pos: Position) -> Self {
        FrameCoord { x: pos.x, y: pos.y }
    }
}

use crate::snake_client::MoveResult;

/// Convert a Game state to a frame for the board viewer
///
/// Includes latency info from move results when provided.
pub fn game_to_frame(
    game: &Game,
    death_info: &[DeathInfo],
    move_results: &[MoveResult],
) -> EngineGameFrame {
    EngineGameFrame {
        turn: game.turn,
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

                let (eliminated_cause, eliminated_by) = if s.health <= 0 {
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
                    .or_else(|| s.shout.clone())
                    .unwrap_or_default();

                FrameSnake {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    body: body_to_coords(&s.body),
                    health: s.health,
                    color: generate_snake_color(&s.id),
                    head_type: "default".to_string(),
                    tail_type: "default".to_string(),
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
    use battlesnake_game_types::wire_representation::{
        BattleSnake, Board, Game, NestedGame, Ruleset,
    };

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

        let frame = game_to_frame(&game, &death_info, &[]);

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
        game.board.snakes[0].health = 0; // Snake is dead

        let death_info = vec![DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 5,
            cause: "wall-collision".to_string(),
            eliminated_by: "".to_string(),
        }];

        let frame = game_to_frame(&game, &death_info, &[]);

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
        game.board.snakes[0].health = 0;

        let death_info = vec![DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 10,
            cause: "head-collision".to_string(),
            eliminated_by: "snake-2".to_string(),
        }];

        let frame = game_to_frame(&game, &death_info, &[]);

        let death = frame.snakes[0].death.as_ref().unwrap();
        assert_eq!(death.eliminated_by, "snake-2");
        assert_eq!(frame.snakes[0].eliminated_by, "snake-2");
    }

    #[test]
    fn test_game_to_frame_multiple_snakes() {
        let mut game = create_test_game();
        // Add a second snake
        game.board.snakes.push(BattleSnake {
            id: "snake-2".to_string(),
            name: "Second Snake".to_string(),
            head: Position::new(3, 3),
            body: VecDeque::from([
                Position::new(3, 3),
                Position::new(3, 2),
                Position::new(3, 1),
            ]),
            health: 80,
            shout: None,
            actual_length: None,
        });

        let death_info: Vec<DeathInfo> = vec![];
        let frame = game_to_frame(&game, &death_info, &[]);

        assert_eq!(frame.snakes.len(), 2);
        assert_eq!(frame.snakes[0].id, "snake-1");
        assert_eq!(frame.snakes[1].id, "snake-2");
        assert_eq!(frame.snakes[1].health, 80);
    }

    #[test]
    fn test_game_to_frame_with_food() {
        let mut game = create_test_game();
        game.board.food = vec![Position::new(5, 5), Position::new(7, 7)];

        let frame = game_to_frame(&game, &[], &[]);

        assert_eq!(frame.food.len(), 2);
        assert_eq!(frame.food[0].x, 5);
        assert_eq!(frame.food[0].y, 5);
        assert_eq!(frame.food[1].x, 7);
        assert_eq!(frame.food[1].y, 7);
    }

    #[test]
    fn test_game_to_frame_with_hazards() {
        let mut game = create_test_game();
        game.board.hazards = vec![Position::new(0, 0), Position::new(10, 10)];

        let frame = game_to_frame(&game, &[], &[]);

        assert_eq!(frame.hazards.len(), 2);
        assert_eq!(frame.hazards[0].x, 0);
        assert_eq!(frame.hazards[0].y, 0);
    }

    #[test]
    fn test_game_to_frame_snake_body_coords() {
        let game = create_test_game();
        let frame = game_to_frame(&game, &[], &[]);

        // Snake body should be converted to FrameCoords
        assert_eq!(frame.snakes[0].body.len(), 3);
        assert_eq!(frame.snakes[0].body[0].x, 5);
        assert_eq!(frame.snakes[0].body[0].y, 5);
    }

    #[test]
    fn test_game_to_frame_alive_snake_no_death() {
        let game = create_test_game();
        // Even if there's death_info for this snake, if health > 0, no eliminated fields
        let death_info = vec![DeathInfo {
            snake_id: "snake-1".to_string(),
            turn: 5,
            cause: "test".to_string(),
            eliminated_by: "".to_string(),
        }];

        let frame = game_to_frame(&game, &death_info, &[]);

        // Death info is still attached (for replay purposes)
        assert!(frame.snakes[0].death.is_some());
        // But eliminated_cause/eliminated_by are empty since snake is alive
        assert_eq!(frame.snakes[0].eliminated_cause, "");
        assert_eq!(frame.snakes[0].eliminated_by, "");
    }

    #[test]
    fn test_frame_snake_serialization() {
        let game = create_test_game();
        let frame = game_to_frame(&game, &[], &[]);

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
        use battlesnake_game_types::types::Move;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: Move::Up,
            latency_ms: Some(42),
            timed_out: false,
            shout: None,
        }];

        let frame = game_to_frame(&game, &death_info, &move_results);

        assert_eq!(frame.snakes[0].latency, "42");
    }

    #[test]
    fn test_game_to_frame_latency_timeout() {
        use crate::snake_client::MoveResult;
        use battlesnake_game_types::types::Move;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: Move::Up,
            latency_ms: None,
            timed_out: true,
            shout: None,
        }];

        let frame = game_to_frame(&game, &death_info, &move_results);

        assert_eq!(frame.snakes[0].latency, "timeout");
    }

    #[test]
    fn test_game_to_frame_shout_from_move_result() {
        use crate::snake_client::MoveResult;
        use battlesnake_game_types::types::Move;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: Move::Up,
            latency_ms: Some(100),
            timed_out: false,
            shout: Some("Hello from move!".to_string()),
        }];

        let frame = game_to_frame(&game, &death_info, &move_results);

        // Shout from move result should be used
        assert_eq!(frame.snakes[0].shout, "Hello from move!");
    }

    #[test]
    fn test_game_to_frame_fallback_shout() {
        use crate::snake_client::MoveResult;
        use battlesnake_game_types::types::Move;

        let game = create_test_game(); // This game has a snake with shout "Hello!"
        let death_info: Vec<DeathInfo> = vec![];
        let move_results = vec![MoveResult {
            snake_id: "snake-1".to_string(),
            direction: Move::Up,
            latency_ms: Some(100),
            timed_out: false,
            shout: None, // No shout in move result
        }];

        let frame = game_to_frame(&game, &death_info, &move_results);

        // Should fall back to snake's existing shout
        assert_eq!(frame.snakes[0].shout, "Hello!");
    }

    #[test]
    fn test_game_to_frame_no_matching_latency_result() {
        use crate::snake_client::MoveResult;
        use battlesnake_game_types::types::Move;

        let game = create_test_game();
        let death_info: Vec<DeathInfo> = vec![];
        // Move result for a different snake
        let move_results = vec![MoveResult {
            snake_id: "other-snake".to_string(),
            direction: Move::Down,
            latency_ms: Some(50),
            timed_out: false,
            shout: None,
        }];

        let frame = game_to_frame(&game, &death_info, &move_results);

        // Should default to "0" when no matching result
        assert_eq!(frame.snakes[0].latency, "0");
    }

    fn create_test_game() -> Game {
        let snake = BattleSnake {
            id: "snake-1".to_string(),
            name: "Test Snake".to_string(),
            head: Position::new(5, 5),
            body: VecDeque::from([
                Position::new(5, 5),
                Position::new(5, 4),
                Position::new(5, 3),
            ]),
            health: 100,
            shout: Some("Hello!".to_string()),
            actual_length: None,
        };

        Game {
            you: snake.clone(),
            board: Board {
                height: 11,
                width: 11,
                food: vec![],
                snakes: vec![snake],
                hazards: vec![],
            },
            turn: 0,
            game: NestedGame {
                id: "test-game".to_string(),
                ruleset: Ruleset {
                    name: "standard".to_string(),
                    version: "v1.0.0".to_string(),
                    settings: None,
                },
                timeout: 500,
                map: None,
                source: None,
            },
        }
    }

    // === Test scaffold for BS-d6da131bea2c4868: Snake customization support ===
    // These tests verify that game_to_frame uses SnakeCustomizations when provided,
    // and falls back to defaults (generated color, "default" head/tail) when not.
    // The implementation agent should:
    // 1. Add SnakeCustomizations struct to this file
    // 2. Update game_to_frame to accept &HashMap<String, SnakeCustomizations> as 4th param
    // 3. Remove #[ignore] and replace todo!() with the commented-out test body

    #[test]
    #[ignore = "Requires SnakeCustomizations struct and updated game_to_frame signature"]
    fn test_game_to_frame_uses_customizations() {
        // use std::collections::HashMap;
        // let game = create_test_game();
        // let mut custom = HashMap::new();
        // custom.insert("snake-1".to_string(), SnakeCustomizations {
        //     color: "#ff0000".to_string(),
        //     head: "bendr".to_string(),
        //     tail: "fat-rattle".to_string(),
        // });
        // let frame = game_to_frame(&game, &[], &[], &custom);
        // assert_eq!(frame.snakes[0].color, "#ff0000", "should use custom color");
        // assert_eq!(frame.snakes[0].head_type, "bendr", "should use custom head");
        // assert_eq!(frame.snakes[0].tail_type, "fat-rattle", "should use custom tail");
        todo!("uncomment after SnakeCustomizations and updated game_to_frame are implemented");
    }

    #[test]
    #[ignore = "Requires SnakeCustomizations struct and updated game_to_frame signature"]
    fn test_game_to_frame_falls_back_when_no_customizations() {
        // use std::collections::HashMap;
        // let game = create_test_game();
        // let frame = game_to_frame(&game, &[], &[], &HashMap::new());
        // assert!(!frame.snakes[0].color.is_empty(), "fallback color should not be empty");
        // assert!(frame.snakes[0].color.starts_with('#'), "fallback color should be a hex color");
        // assert_eq!(frame.snakes[0].head_type, "default", "fallback head should be 'default'");
        // assert_eq!(frame.snakes[0].tail_type, "default", "fallback tail should be 'default'");
        todo!("uncomment after SnakeCustomizations and updated game_to_frame are implemented");
    }

    #[test]
    #[ignore = "Requires SnakeCustomizations struct and updated game_to_frame signature"]
    fn test_game_to_frame_empty_customization_fields_use_defaults() {
        // When customization exists but fields are empty strings, should fall back
        // use std::collections::HashMap;
        // let game = create_test_game();
        // let mut custom = HashMap::new();
        // custom.insert("snake-1".to_string(), SnakeCustomizations {
        //     color: "".to_string(), head: "".to_string(), tail: "".to_string(),
        // });
        // let frame = game_to_frame(&game, &[], &[], &custom);
        // assert!(!frame.snakes[0].color.is_empty(), "empty color should fall back to generated color");
        // assert!(frame.snakes[0].color.starts_with('#'), "fallback color should be a hex color");
        // assert_eq!(frame.snakes[0].head_type, "default", "empty head should fall back to 'default'");
        // assert_eq!(frame.snakes[0].tail_type, "default", "empty tail should fall back to 'default'");
        todo!("uncomment after SnakeCustomizations and updated game_to_frame are implemented");
    }

    #[test]
    #[ignore = "Requires SnakeCustomizations struct and updated game_to_frame signature"]
    fn test_game_to_frame_partial_customization() {
        // Only color and tail set, head should default
        // use std::collections::HashMap;
        // let game = create_test_game();
        // let mut custom = HashMap::new();
        // custom.insert("snake-1".to_string(), SnakeCustomizations {
        //     color: "#00ff00".to_string(), head: "".to_string(), tail: "curled".to_string(),
        // });
        // let frame = game_to_frame(&game, &[], &[], &custom);
        // assert_eq!(frame.snakes[0].color, "#00ff00", "non-empty color should be used");
        // assert_eq!(frame.snakes[0].head_type, "default", "empty head should fall back to 'default'");
        // assert_eq!(frame.snakes[0].tail_type, "curled", "non-empty tail should be used");
        todo!("uncomment after SnakeCustomizations and updated game_to_frame are implemented");
    }

    #[test]
    #[ignore = "Requires SnakeCustomizations struct and updated game_to_frame signature"]
    fn test_game_to_frame_customizations_dont_affect_other_fields() {
        // Verify that adding customizations doesn't break existing frame fields
        // use std::collections::HashMap;
        // let game = create_test_game();
        // let mut custom = HashMap::new();
        // custom.insert("snake-1".to_string(), SnakeCustomizations {
        //     color: "#ff0000".to_string(), head: "bendr".to_string(), tail: "fat-rattle".to_string(),
        // });
        // let frame = game_to_frame(&game, &[], &[], &custom);
        // assert_eq!(frame.snakes[0].id, "snake-1", "id should be preserved");
        // assert_eq!(frame.snakes[0].name, "Test Snake", "name should be preserved");
        // assert_eq!(frame.snakes[0].health, 100, "health should be preserved");
        // assert_eq!(frame.snakes[0].body.len(), 3, "body length should be preserved");
        todo!("uncomment after SnakeCustomizations and updated game_to_frame are implemented");
    }
}
