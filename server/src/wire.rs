//! Arena-owned wire types matching the official Battlesnake API schema.
//!
//! These types are serialized when calling snake `/start`, `/move`, `/end` endpoints.
//! The engine uses `rules::BoardState` internally; conversion happens at the HTTP
//! boundary.

use serde::Serialize;
use std::collections::HashMap;

use crate::engine::EngineGame;
use crate::engine::frame::SnakeCustomizations;

#[derive(Debug, Clone, Serialize)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Customizations {
    pub color: String,
    pub head: String,
    pub tail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BattleSnake {
    pub id: String,
    pub name: String,
    pub health: i32,
    pub body: Vec<Position>,
    pub head: Position,
    pub length: i32,
    pub latency: String,
    pub shout: String,
    pub squad: String,
    pub customizations: Customizations,
}

#[derive(Debug, Clone, Serialize)]
pub struct RulesetSettings {
    #[serde(rename = "foodSpawnChance")]
    pub food_spawn_chance: i32,
    #[serde(rename = "minimumFood")]
    pub minimum_food: i32,
    #[serde(rename = "hazardDamagePerTurn")]
    pub hazard_damage_per_turn: i32,
    #[serde(rename = "hazardMap", skip_serializing_if = "Option::is_none")]
    pub hazard_map: Option<String>,
    #[serde(rename = "hazardMapAuthor", skip_serializing_if = "Option::is_none")]
    pub hazard_map_author: Option<String>,
    pub royale: RoyaleSettings,
    pub squad: SquadSettings,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoyaleSettings {
    #[serde(rename = "shrinkEveryNTurns")]
    pub shrink_every_n_turns: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SquadSettings {
    #[serde(rename = "allowBodyCollisions")]
    pub allow_body_collisions: bool,
    #[serde(rename = "sharedElimination")]
    pub shared_elimination: bool,
    #[serde(rename = "sharedHealth")]
    pub shared_health: bool,
    #[serde(rename = "sharedLength")]
    pub shared_length: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ruleset {
    pub name: String,
    pub version: String,
    pub settings: RulesetSettings,
}

#[derive(Debug, Clone, Serialize)]
pub struct NestedGame {
    pub id: String,
    pub ruleset: Ruleset,
    pub timeout: i64,
    pub map: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Board {
    pub height: u32,
    pub width: u32,
    pub food: Vec<Position>,
    pub snakes: Vec<BattleSnake>,
    pub hazards: Vec<Position>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub game: NestedGame,
    pub turn: i32,
    pub board: Board,
    pub you: BattleSnake,
}

// --- Conversion from engine types ---

impl From<&rules::Point> for Position {
    fn from(p: &rules::Point) -> Self {
        Position { x: p.x, y: p.y }
    }
}

/// Extra per-snake context from the previous turn's MoveResults.
pub struct SnakeContext {
    pub latency_ms: Option<i64>,
    pub shout: Option<String>,
}

impl BattleSnake {
    pub fn from_rules_snake(
        snake: &rules::Snake,
        name: &str,
        context: Option<&SnakeContext>,
        customization: Option<&SnakeCustomizations>,
    ) -> Self {
        let head = snake
            .body
            .first()
            .map_or(Position { x: 0, y: 0 }, |p| Position { x: p.x, y: p.y });
        BattleSnake {
            id: snake.id.clone(),
            name: name.to_string(),
            health: snake.health,
            body: snake.body.iter().map(Position::from).collect(),
            head,
            length: snake.body.len() as i32,
            latency: context
                .and_then(|c| c.latency_ms)
                .map_or_else(|| "0".to_string(), |ms| ms.to_string()),
            shout: context.and_then(|c| c.shout.clone()).unwrap_or_default(),
            squad: String::new(),
            customizations: customization.map_or_else(Customizations::default, |c| {
                Customizations {
                    color: c.color.clone(),
                    head: c.head.clone(),
                    tail: c.tail.clone(),
                }
            }),
        }
    }
}

impl Default for RulesetSettings {
    fn default() -> Self {
        RulesetSettings {
            food_spawn_chance: 0,
            minimum_food: 0,
            hazard_damage_per_turn: 0,
            hazard_map: None,
            hazard_map_author: None,
            royale: RoyaleSettings {
                shrink_every_n_turns: 0,
            },
            squad: SquadSettings {
                allow_body_collisions: false,
                shared_elimination: false,
                shared_health: false,
                shared_length: false,
            },
        }
    }
}

impl Game {
    pub fn from_engine_game(
        engine_game: &EngineGame,
        you_snake_id: &str,
        snake_contexts: &HashMap<String, SnakeContext>,
        customizations: &HashMap<String, SnakeCustomizations>,
    ) -> Self {
        let convert_snake = |s: &rules::Snake| {
            let name = engine_game
                .snake_names
                .get(&s.id)
                .map(|n| n.as_str())
                .unwrap_or(&s.id);
            BattleSnake::from_rules_snake(
                s,
                name,
                snake_contexts.get(&s.id),
                customizations.get(&s.id),
            )
        };

        let you = engine_game
            .board
            .snakes
            .iter()
            .find(|s| s.id == you_snake_id)
            .map(&convert_snake)
            .unwrap_or_else(|| BattleSnake {
                id: "dummy".to_string(),
                name: "Dummy".to_string(),
                health: 0,
                body: vec![],
                head: Position { x: 0, y: 0 },
                length: 0,
                latency: "0".to_string(),
                shout: String::new(),
                squad: String::new(),
                customizations: Customizations::default(),
            });

        let settings = &engine_game.meta.settings;

        // Internal `meta.ruleset_name` drives engine dispatch; the wire
        // protocol mirrors play.battlesnake.com. Snail Mode upstream is a
        // community *map* on the standard ruleset, so snakes see
        // `ruleset.name = "standard"` with `game.map = "snail_mode"`
        // (existing community snakes key off `game.map`). Other modes keep
        // their ruleset name and an empty map, unchanged.
        let (wire_ruleset_name, wire_map) = match engine_game.meta.ruleset_name.as_str() {
            "snail_mode" => ("standard".to_string(), "snail_mode".to_string()),
            name => (name.to_string(), String::new()),
        };

        Game {
            game: NestedGame {
                id: engine_game.meta.game_id.clone(),
                ruleset: Ruleset {
                    name: wire_ruleset_name,
                    version: "v1.0.0".to_string(),
                    settings: RulesetSettings {
                        food_spawn_chance: settings.food_spawn_chance,
                        minimum_food: settings.minimum_food,
                        hazard_damage_per_turn: settings.hazard_damage_per_turn,
                        hazard_map: None,
                        hazard_map_author: None,
                        royale: RoyaleSettings {
                            // Real shrink cadence for Royale games; 0 for
                            // standard/other modes (board-viewer convention).
                            shrink_every_n_turns: engine_game
                                .meta
                                .royale
                                .as_ref()
                                .map_or(0, |r| r.shrink_every_n_turns),
                        },
                        squad: SquadSettings {
                            allow_body_collisions: false,
                            shared_elimination: false,
                            shared_health: false,
                            shared_length: false,
                        },
                    },
                },
                timeout: engine_game.meta.timeout,
                map: wire_map,
                source: String::new(),
            },
            turn: engine_game.board.turn,
            board: Board {
                height: engine_game.board.height as u32,
                width: engine_game.board.width as u32,
                food: engine_game.board.food.iter().map(Position::from).collect(),
                snakes: engine_game
                    .board
                    .snakes
                    .iter()
                    .map(&convert_snake)
                    .collect(),
                // Snail Mode stores pending-trail bookkeeping as off-board
                // points inside `board.hazards`; snakes must only ever see
                // real, on-board hazards (stacked duplicates included).
                hazards: engine_game
                    .board
                    .on_board_hazards()
                    .map(Position::from)
                    .collect(),
            },
            you,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rules::{BoardState, EliminationCause, Point, Snake, StandardSettings};
    use serde_json::Value;

    fn create_test_engine_game() -> EngineGame {
        let snake = Snake {
            id: "s1".to_string(),
            body: vec![Point::new(3, 4), Point::new(3, 3), Point::new(3, 2)],
            health: 95,
            eliminated_cause: EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        };

        let mut snake_names = HashMap::new();
        snake_names.insert("s1".to_string(), "Snake 1".to_string());

        EngineGame {
            board: BoardState {
                turn: 10,
                width: 11,
                height: 11,
                food: vec![Point::new(5, 5)],
                snakes: vec![snake],
                hazards: vec![],
            },
            meta: crate::engine::GameMeta {
                game_id: "g1".to_string(),
                ruleset_name: "standard".to_string(),
                timeout: 500,
                settings: StandardSettings {
                    food_spawn_chance: 15,
                    minimum_food: 1,
                    hazard_damage_per_turn: 15,
                },
                royale: None,
            },
            snake_names,
        }
    }

    #[test]
    fn test_game_has_all_required_fields() {
        let game = Game {
            game: NestedGame {
                id: "game-1".to_string(),
                ruleset: Ruleset {
                    name: "standard".to_string(),
                    version: "v1.0.0".to_string(),
                    settings: RulesetSettings::default(),
                },
                timeout: 500,
                map: String::new(),
                source: String::new(),
            },
            turn: 3,
            board: Board {
                height: 11,
                width: 11,
                food: vec![Position { x: 5, y: 5 }],
                snakes: vec![],
                hazards: vec![],
            },
            you: BattleSnake {
                id: "snake-1".to_string(),
                name: "Test Snake".to_string(),
                health: 100,
                body: vec![Position { x: 1, y: 1 }, Position { x: 1, y: 0 }],
                head: Position { x: 1, y: 1 },
                length: 2,
                latency: "45".to_string(),
                shout: "hello".to_string(),
                squad: "".to_string(),
                customizations: Customizations {
                    color: "".to_string(),
                    head: "".to_string(),
                    tail: "".to_string(),
                },
            },
        };

        let json: Value = serde_json::to_value(&game).unwrap();

        assert!(json.get("game").is_some());
        assert!(json.get("turn").is_some());
        assert!(json.get("board").is_some());
        assert!(json.get("you").is_some());

        let you = &json["you"];
        assert_eq!(you["id"], "snake-1");
        assert_eq!(you["length"], 2);
        assert_eq!(you["latency"], "45");
        assert_eq!(you["shout"], "hello");
        assert_eq!(you["squad"], "");
        assert!(you.get("customizations").is_some());
        assert_eq!(you["customizations"]["color"], "");
        assert_eq!(you["customizations"]["head"], "");
        assert_eq!(you["customizations"]["tail"], "");
    }

    #[test]
    fn test_from_engine_game_populates_derived_fields() {
        let engine_game = create_test_engine_game();

        // No context -- simulates /start or first turn
        let contexts: HashMap<String, SnakeContext> = HashMap::new();
        let customizations: HashMap<String, SnakeCustomizations> = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);

        assert_eq!(wire.you.length, 3);
        assert_eq!(wire.you.latency, "0");
        assert_eq!(wire.you.squad, "");
        assert_eq!(wire.you.customizations.color, "");
        assert_eq!(wire.you.head.x, 3);
        assert_eq!(wire.you.head.y, 4);
        assert_eq!(wire.you.shout, "");

        // With context -- simulates mid-game turn
        let mut contexts = HashMap::new();
        contexts.insert(
            "s1".to_string(),
            SnakeContext {
                latency_ms: Some(123),
                shout: Some("go!".to_string()),
            },
        );
        let mut customizations = HashMap::new();
        customizations.insert(
            "s1".to_string(),
            SnakeCustomizations {
                color: "#ff8800".to_string(),
                head: "beluga".to_string(),
                tail: "bolt".to_string(),
            },
        );
        let wire2 = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);

        assert_eq!(wire2.you.latency, "123");
        assert_eq!(wire2.you.shout, "go!");
        assert_eq!(wire2.you.customizations.color, "#ff8800");
        assert_eq!(wire2.you.customizations.head, "beluga");
        assert_eq!(wire2.you.customizations.tail, "bolt");
    }

    #[test]
    fn test_standard_game_has_default_royale_and_squad() {
        let engine_game = create_test_engine_game();
        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
        let json: Value = serde_json::to_value(&wire).unwrap();

        let settings = &json["game"]["ruleset"]["settings"];

        assert!(
            settings.get("royale").is_some(),
            "royale field must be present in settings even for standard games"
        );
        assert_eq!(
            settings["royale"]["shrinkEveryNTurns"], 0,
            "shrinkEveryNTurns must default to 0 for non-royale games"
        );

        assert!(
            settings.get("squad").is_some(),
            "squad field must be present in settings even for non-squad games"
        );
        assert_eq!(settings["squad"]["allowBodyCollisions"], false);
        assert_eq!(settings["squad"]["sharedElimination"], false);
        assert_eq!(settings["squad"]["sharedHealth"], false);
        assert_eq!(settings["squad"]["sharedLength"], false);
    }

    #[test]
    fn test_royale_game_serializes_real_royale_settings() {
        let mut engine_game = create_test_engine_game();
        engine_game.meta.ruleset_name = "royale".to_string();
        engine_game.meta.settings.hazard_damage_per_turn = 14;
        engine_game.meta.royale = Some(rules::RoyaleSettings {
            shrink_every_n_turns: 25,
            seed: 7,
        });

        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["game"]["ruleset"]["name"], "royale");
        let settings = &json["game"]["ruleset"]["settings"];
        assert_eq!(
            settings["royale"]["shrinkEveryNTurns"], 25,
            "royale games must serialize their real shrink cadence"
        );
        assert_eq!(settings["hazardDamagePerTurn"], 14);
    }

    /// Snail Mode wire parity with play.battlesnake.com: upstream it is a
    /// community map on the standard ruleset, so snakes must see ruleset
    /// "standard" with `game.map = "snail_mode"` (community snakes key off
    /// `game.map`), even though the engine dispatches on the internal
    /// ruleset name "snail_mode".
    #[test]
    fn test_snail_mode_wire_sends_standard_ruleset_and_snail_map() {
        let mut engine_game = create_test_engine_game();
        engine_game.meta.ruleset_name = "snail_mode".to_string();
        engine_game.meta.settings.hazard_damage_per_turn = 14;

        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["game"]["ruleset"]["name"], "standard");
        assert_eq!(json["game"]["map"], "snail_mode");
        let settings = &json["game"]["ruleset"]["settings"];
        assert_eq!(settings["hazardDamagePerTurn"], 14);
        assert_eq!(settings["royale"]["shrinkEveryNTurns"], 0);
    }

    /// Other modes are unchanged by the map-field wiring: ruleset name
    /// passes through and the map stays empty.
    #[test]
    fn test_non_snail_modes_keep_ruleset_name_and_empty_map() {
        for ruleset in ["standard", "royale"] {
            let mut engine_game = create_test_engine_game();
            engine_game.meta.ruleset_name = ruleset.to_string();

            let contexts = HashMap::new();
            let customizations = HashMap::new();
            let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
            let json: Value = serde_json::to_value(&wire).unwrap();

            assert_eq!(json["game"]["ruleset"]["name"], ruleset);
            assert_eq!(json["game"]["map"], "");
        }
    }

    /// HARD requirement: snakes must never receive out-of-bounds hazard
    /// points in /move payloads. Snail Mode's pending-trail bookkeeping
    /// lives as off-board points in `board.hazards` and must be filtered
    /// out, while on-board stacked duplicates pass through intact.
    #[test]
    fn test_off_board_hazard_bookkeeping_never_reaches_snakes() {
        let mut engine_game = create_test_engine_game();
        engine_game.meta.ruleset_name = "snail_mode".to_string();
        engine_game.board.hazards = vec![
            rules::Point::new(2, 3),
            rules::Point::new(2, 3),
            rules::Point::new(2, 3),
            // Pending tails stored at y + height (board is 11x11).
            rules::Point::new(2, 14),
            rules::Point::new(2, 14),
            rules::Point::new(2, 14),
        ];

        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);

        assert_eq!(
            wire.board.hazards.len(),
            3,
            "only on-board hazard entries may be serialized"
        );
        for h in &wire.board.hazards {
            assert!(
                h.x >= 0 && h.x < 11 && h.y >= 0 && h.y < 11,
                "out-of-bounds hazard ({}, {}) leaked to the wire",
                h.x,
                h.y
            );
        }
    }

    #[test]
    fn test_missing_engine_fields_produce_defaults() {
        let engine_game = create_test_engine_game();
        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
        let json: Value = serde_json::to_value(&wire).unwrap();

        let game = &json["game"];
        assert!(
            game.get("map").is_some(),
            "map must always be present in serialized JSON"
        );
        assert!(
            game.get("source").is_some(),
            "source must always be present in serialized JSON"
        );
        assert!(
            game["ruleset"].get("settings").is_some(),
            "settings must always be present in serialized JSON"
        );
    }

    #[test]
    fn test_squad_settings_struct_exists() {
        let squad = SquadSettings {
            allow_body_collisions: true,
            shared_elimination: true,
            shared_health: false,
            shared_length: false,
        };
        let json: Value = serde_json::to_value(&squad).unwrap();
        assert_eq!(json["allowBodyCollisions"], true);
        assert_eq!(json["sharedElimination"], true);
        assert_eq!(json["sharedHealth"], false);
        assert_eq!(json["sharedLength"], false);
    }

    #[test]
    fn test_ruleset_settings_royale_is_non_optional() {
        let settings = RulesetSettings {
            food_spawn_chance: 15,
            minimum_food: 1,
            hazard_damage_per_turn: 15,
            hazard_map: None,
            hazard_map_author: None,
            royale: RoyaleSettings {
                shrink_every_n_turns: 0,
            },
            squad: SquadSettings {
                allow_body_collisions: false,
                shared_elimination: false,
                shared_health: false,
                shared_length: false,
            },
        };
        let json: Value = serde_json::to_value(&settings).unwrap();
        assert!(
            json.get("royale").is_some(),
            "royale must always be serialized"
        );
        assert!(
            json.get("squad").is_some(),
            "squad must always be serialized"
        );
    }
}
