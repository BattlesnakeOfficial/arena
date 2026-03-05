//! Arena-owned wire types matching the official Battlesnake API schema.
//!
//! These types are serialized when calling snake `/start`, `/move`, `/end` endpoints.
//! The engine continues using `battlesnake-game-types` internally; conversion happens
//! at the HTTP boundary.

use battlesnake_game_types::wire_representation;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub royale: Option<RoyaleSettings>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoyaleSettings {
    #[serde(rename = "shrinkEveryNTurns")]
    pub shrink_every_n_turns: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ruleset {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<RulesetSettings>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NestedGame {
    pub id: String,
    pub ruleset: Ruleset,
    pub timeout: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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

impl From<&wire_representation::Position> for Position {
    fn from(p: &wire_representation::Position) -> Self {
        Position { x: p.x, y: p.y }
    }
}

/// Extra per-snake context from the previous turn's MoveResults.
pub struct SnakeContext {
    pub latency_ms: Option<i64>,
    pub shout: Option<String>,
}

impl BattleSnake {
    pub fn from_engine_snake(
        snake: &wire_representation::BattleSnake,
        context: Option<&SnakeContext>,
    ) -> Self {
        BattleSnake {
            id: snake.id.clone(),
            name: snake.name.clone(),
            health: snake.health,
            body: snake.body.iter().map(Position::from).collect(),
            head: Position::from(&snake.head),
            length: snake.body.len() as i32,
            latency: context
                .and_then(|c| c.latency_ms)
                .map_or_else(|| "0".to_string(), |ms| ms.to_string()),
            shout: context
                .and_then(|c| c.shout.clone())
                .or_else(|| snake.shout.clone())
                .unwrap_or_default(),
            squad: String::new(),
            customizations: Customizations {
                color: String::new(),
                head: String::new(),
                tail: String::new(),
            },
        }
    }
}

impl RulesetSettings {
    fn from_engine_settings(settings: &wire_representation::Settings) -> Self {
        RulesetSettings {
            food_spawn_chance: settings.food_spawn_chance,
            minimum_food: settings.minimum_food,
            hazard_damage_per_turn: settings.hazard_damage_per_turn,
            hazard_map: settings.hazard_map.clone(),
            hazard_map_author: settings.hazard_map_author.clone(),
            royale: settings.royale.map(|r| RoyaleSettings {
                shrink_every_n_turns: r.shrink_every_n_turns,
            }),
        }
    }
}

impl Game {
    pub fn from_engine_game(
        game: &wire_representation::Game,
        you: &wire_representation::BattleSnake,
        snake_contexts: &HashMap<String, SnakeContext>,
    ) -> Self {
        let convert_snake = |s: &wire_representation::BattleSnake| {
            BattleSnake::from_engine_snake(s, snake_contexts.get(&s.id))
        };

        Game {
            game: NestedGame {
                id: game.game.id.clone(),
                ruleset: Ruleset {
                    name: game.game.ruleset.name.clone(),
                    version: game.game.ruleset.version.clone(),
                    settings: game
                        .game
                        .ruleset
                        .settings
                        .as_ref()
                        .map(RulesetSettings::from_engine_settings),
                },
                timeout: game.game.timeout,
                map: game.game.map.clone(),
                source: game.game.source.clone(),
            },
            turn: game.turn,
            board: Board {
                height: game.board.height,
                width: game.board.width,
                food: game.board.food.iter().map(Position::from).collect(),
                snakes: game.board.snakes.iter().map(&convert_snake).collect(),
                hazards: game.board.hazards.iter().map(Position::from).collect(),
            },
            you: convert_snake(you),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn test_game_has_all_required_fields() {
        let game = Game {
            game: NestedGame {
                id: "game-1".to_string(),
                ruleset: Ruleset {
                    name: "standard".to_string(),
                    version: "v1.0.0".to_string(),
                    settings: None,
                },
                timeout: 500,
                map: None,
                source: None,
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
        use battlesnake_game_types::wire_representation as engine;
        use std::collections::VecDeque;

        let snake = engine::BattleSnake {
            id: "s1".to_string(),
            name: "Snake 1".to_string(),
            head: engine::Position::new(3, 4),
            body: VecDeque::from([
                engine::Position::new(3, 4),
                engine::Position::new(3, 3),
                engine::Position::new(3, 2),
            ]),
            health: 95,
            shout: None,
            actual_length: None,
        };

        let engine_game = engine::Game {
            you: snake.clone(),
            board: engine::Board {
                height: 11,
                width: 11,
                food: vec![engine::Position::new(5, 5)],
                snakes: vec![snake.clone()],
                hazards: vec![],
            },
            turn: 10,
            game: engine::NestedGame {
                id: "g1".to_string(),
                ruleset: engine::Ruleset {
                    name: "standard".to_string(),
                    version: "v1.0.0".to_string(),
                    settings: None,
                },
                timeout: 500,
                map: None,
                source: None,
            },
        };

        // No context — simulates /start or first turn
        let contexts: HashMap<String, SnakeContext> = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, &snake, &contexts);

        assert_eq!(wire.you.length, 3);
        assert_eq!(wire.you.latency, "0");
        assert_eq!(wire.you.squad, "");
        assert_eq!(wire.you.customizations.color, "");
        assert_eq!(wire.you.head.x, 3);
        assert_eq!(wire.you.head.y, 4);
        assert_eq!(wire.you.shout, "");

        // With context — simulates mid-game turn
        let mut contexts = HashMap::new();
        contexts.insert(
            "s1".to_string(),
            SnakeContext {
                latency_ms: Some(123),
                shout: Some("go!".to_string()),
            },
        );
        let wire2 = Game::from_engine_game(&engine_game, &snake, &contexts);

        assert_eq!(wire2.you.latency, "123");
        assert_eq!(wire2.you.shout, "go!");
    }
}
