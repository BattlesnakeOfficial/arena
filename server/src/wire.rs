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
    /// Deprecated upstream (replaced by `game.map`) but still always present
    /// as "" in the official engine's payload.
    #[serde(rename = "hazardMap")]
    pub hazard_map: String,
    /// Deprecated upstream; always present as "".
    #[serde(rename = "hazardMapAuthor")]
    pub hazard_map_author: String,
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

/// The public `(ruleset name, map)` pair for an internal ruleset name.
///
/// Internal `GameMeta::ruleset_name` drives engine dispatch; everything
/// public (snake payloads, the engine-compatible game API) matches
/// play.battlesnake.com exactly (play `ui/maps.py` + `leaderboards_setup.py`).
/// Royale and Snail Mode were *maps* on the standard ruleset, and
/// single-snake games were plain standard games, so only Constrictor has its
/// own ruleset name.
pub fn wire_ruleset_and_map(internal_ruleset: &str) -> (&'static str, &'static str) {
    match internal_ruleset {
        "royale" => ("standard", "royale"),
        "snail_mode" => ("standard", "snail_mode"),
        "constrictor" => ("constrictor", "empty"),
        _ => ("standard", "standard"),
    }
}

/// Extra per-snake context from the previous turn's MoveResults.
pub struct SnakeContext {
    pub latency_ms: Option<i64>,
    pub shout: Option<String>,
}

/// The `latency` a snake sees for its previous move, matching the official
/// engine (`convertBoardStatetoGameFrame` in BattlesnakeOfficial/engine):
/// measured milliseconds clamped to `[1, timeout]`, so a timed-out move
/// reports the full timeout rather than "0". `None` (no measurement, e.g. an
/// engine-proxy fault that was not the snake's doing) serializes as "0".
pub fn reported_latency_ms(
    latency_ms: Option<i64>,
    timed_out: bool,
    timeout_ms: i64,
) -> Option<i64> {
    if timed_out {
        return Some(timeout_ms);
    }
    latency_ms.map(|ms| ms.clamp(1, timeout_ms.max(1)))
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
            hazard_map: String::new(),
            hazard_map_author: String::new(),
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
            .map(convert_snake)
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

        let (wire_ruleset_name, wire_map) = wire_ruleset_and_map(&engine_game.meta.ruleset_name);

        Game {
            game: NestedGame {
                id: engine_game.meta.game_id.clone(),
                ruleset: Ruleset {
                    name: wire_ruleset_name.to_string(),
                    version: "v1.0.0".to_string(),
                    settings: RulesetSettings {
                        food_spawn_chance: settings.food_spawn_chance,
                        minimum_food: settings.minimum_food,
                        hazard_damage_per_turn: settings.hazard_damage_per_turn,
                        hazard_map: String::new(),
                        hazard_map_author: String::new(),
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
                map: wire_map.to_string(),
                source: engine_game.meta.source.as_str().to_string(),
            },
            turn: engine_game.board.turn,
            board: Board {
                height: engine_game.board.height as u32,
                width: engine_game.board.width as u32,
                food: engine_game.board.food.iter().map(Position::from).collect(),
                // Only living snakes, like the official engine
                // (`frame.FilteredSnakes().Alive()`). Eliminated snakes keep
                // their final body in engine state -- a wall death leaves the
                // head off the board -- so sending them breaks snakes that
                // index a grid, and on /end it hides who won. `you` is still
                // the requesting snake's own state, dead or alive.
                snakes: engine_game
                    .board
                    .snakes
                    .iter()
                    .filter(|s| !s.eliminated_cause.is_eliminated())
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
                source: crate::engine::GameSource::Custom,
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
                author: "byte-owner".to_string(),
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

        // Play parity: Royale is the "royale" map on the standard ruleset.
        assert_eq!(json["game"]["ruleset"]["name"], "standard");
        assert_eq!(json["game"]["map"], "royale");
        let settings = &json["game"]["ruleset"]["settings"];
        assert_eq!(
            settings["royale"]["shrinkEveryNTurns"], 25,
            "royale games must serialize their real shrink cadence"
        );
        assert_eq!(settings["hazardDamagePerTurn"], 14);
    }

    /// Constrictor games announce themselves on the wire exactly like the
    /// official engine: `ruleset.name = "constrictor"`, no food spawning
    /// (foodSpawnChance/minimumFood = 0), and default royale/squad blocks.
    #[test]
    fn test_constrictor_game_serializes_constrictor_ruleset() {
        let mut engine_game = create_test_engine_game();
        engine_game.meta.ruleset_name = "constrictor".to_string();
        engine_game.meta.settings = StandardSettings {
            food_spawn_chance: 0,
            minimum_food: 0,
            hazard_damage_per_turn: 15,
        };
        engine_game.board.food.clear();

        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["game"]["ruleset"]["name"], "constrictor");
        let settings = &json["game"]["ruleset"]["settings"];
        assert_eq!(settings["foodSpawnChance"], 0);
        assert_eq!(settings["minimumFood"], 0);
        assert_eq!(settings["hazardDamagePerTurn"], 15);
        assert_eq!(
            settings["royale"]["shrinkEveryNTurns"], 0,
            "constrictor games serialize the default royale block"
        );
        assert!(settings.get("squad").is_some());
        assert_eq!(
            json["board"]["food"].as_array().map(|a| a.len()),
            Some(0),
            "constrictor boards never contain food"
        );
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

    /// Every mode sends exactly the ruleset + map pair play.battlesnake.com
    /// sent, so `game.map` is never empty.
    #[test]
    fn test_modes_send_play_ruleset_and_map() {
        for (internal, ruleset, map) in [
            ("standard", "standard", "standard"),
            ("royale", "standard", "royale"),
            ("snail_mode", "standard", "snail_mode"),
            ("constrictor", "constrictor", "empty"),
            ("solo", "standard", "standard"),
        ] {
            let mut engine_game = create_test_engine_game();
            engine_game.meta.ruleset_name = internal.to_string();

            let contexts = HashMap::new();
            let customizations = HashMap::new();
            let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
            let json: Value = serde_json::to_value(&wire).unwrap();

            assert_eq!(
                json["game"]["ruleset"]["name"], ruleset,
                "ruleset for {internal}"
            );
            assert_eq!(json["game"]["map"], map, "map for {internal}");
        }
    }

    /// `game.source` uses play's source values: ladder for leaderboard
    /// games, tournament for bracket games, custom for everything else.
    #[test]
    fn test_source_reflects_game_origin() {
        for (source, expected) in [
            (crate::engine::GameSource::Ladder, "arena"),
            (crate::engine::GameSource::Tournament, "tournament"),
            (crate::engine::GameSource::Custom, "custom"),
        ] {
            let mut engine_game = create_test_engine_game();
            engine_game.meta.source = source;

            let wire = Game::from_engine_game(&engine_game, "s1", &HashMap::new(), &HashMap::new());
            let json: Value = serde_json::to_value(&wire).unwrap();

            assert_eq!(json["game"]["source"], expected);
        }
    }

    /// Every `game.source` arena sends is in the documented set. Strict
    /// parsers (e.g. `battlesnake-game-types`' `Source` enum) 400 on anything
    /// else, which made a ladder snake lose every game when arena sent
    /// Play's undocumented "ladder" (DEV-1505).
    #[test]
    fn test_source_values_are_documented() {
        const DOCUMENTED: [&str; 5] = ["tournament", "league", "arena", "challenge", "custom"];
        for source in [
            crate::engine::GameSource::Ladder,
            crate::engine::GameSource::Tournament,
            crate::engine::GameSource::Custom,
        ] {
            assert!(
                DOCUMENTED.contains(&source.as_str()),
                "{source:?} serializes as undocumented {:?}",
                source.as_str()
            );
        }
    }

    /// A board with one living snake (s1) and one that died running into
    /// the wall on the previous turn: its head is off the board.
    fn engine_game_with_wall_death() -> EngineGame {
        let mut engine_game = create_test_engine_game();
        engine_game.board.snakes.push(Snake {
            id: "s2".to_string(),
            body: vec![Point::new(-1, 7), Point::new(0, 7), Point::new(1, 7)],
            health: 80,
            eliminated_cause: EliminationCause::OutOfBounds,
            eliminated_by: String::new(),
            eliminated_on_turn: 10,
        });
        engine_game
            .snake_names
            .insert("s2".to_string(), "Snake 2".to_string());
        engine_game
    }

    /// Official-engine parity: `board.snakes` only lists living snakes, so a
    /// dead snake's off-board head never reaches anyone's /move.
    #[test]
    fn test_eliminated_snakes_are_not_on_the_board() {
        let engine_game = engine_game_with_wall_death();

        let wire = Game::from_engine_game(&engine_game, "s1", &HashMap::new(), &HashMap::new());

        let ids: Vec<&str> = wire.board.snakes.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1"]);
        for snake in &wire.board.snakes {
            for p in &snake.body {
                assert!(
                    p.x >= 0 && p.x < 11 && p.y >= 0 && p.y < 11,
                    "off-board segment ({}, {}) on the wire",
                    p.x,
                    p.y
                );
            }
        }
    }

    /// /end for the loser: `you` is its own final state, while the board
    /// shows only the survivor -- so the snake can tell who won.
    #[test]
    fn test_end_request_for_dead_snake_shows_winner_only() {
        let engine_game = engine_game_with_wall_death();

        let wire = Game::from_engine_game(&engine_game, "s2", &HashMap::new(), &HashMap::new());

        assert_eq!(wire.you.id, "s2");
        assert_eq!(wire.you.length, 3);
        let ids: Vec<&str> = wire.board.snakes.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1"]);
    }

    #[test]
    fn test_reported_latency_matches_official_engine() {
        // Timeouts report the full budget, never "0".
        assert_eq!(reported_latency_ms(None, true, 500), Some(500));
        assert_eq!(reported_latency_ms(Some(731), true, 500), Some(500));
        // Measured latency is clamped to [1, timeout].
        assert_eq!(reported_latency_ms(Some(0), false, 500), Some(1));
        assert_eq!(reported_latency_ms(Some(123), false, 500), Some(123));
        assert_eq!(reported_latency_ms(Some(512), false, 500), Some(500));
        // No measurement (engine-side fault) stays unknown.
        assert_eq!(reported_latency_ms(None, false, 500), None);
    }

    /// The request after a timeout carries `you.latency = "<timeout>"`,
    /// the same value the frame records.
    #[test]
    fn test_latency_after_timeout_is_timeout_value() {
        let engine_game = create_test_engine_game();
        let mut contexts = HashMap::new();
        contexts.insert(
            "s1".to_string(),
            SnakeContext {
                latency_ms: reported_latency_ms(None, true, engine_game.meta.timeout),
                shout: None,
            },
        );

        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &HashMap::new());

        assert_eq!(wire.you.latency, "500");
    }

    /// Collect every leaf of a JSON value as `path:type`, descending into
    /// the first element of arrays as `path[]`.
    fn leaf_paths(value: &Value, path: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    leaf_paths(child, &child_path, out);
                }
            }
            Value::Array(items) => {
                let first = items.first().expect("golden fixture arrays are non-empty");
                leaf_paths(first, &format!("{path}[]"), out);
            }
            Value::String(_) => out.push(format!("{path}:string")),
            Value::Number(_) => out.push(format!("{path}:number")),
            Value::Bool(_) => out.push(format!("{path}:bool")),
            Value::Null => out.push(format!("{path}:null")),
        }
    }

    /// Golden schema: the serialized request has exactly the keys (and JSON
    /// types) of the official `client.SnakeRequest` in
    /// BattlesnakeOfficial/rules `client/models.go` -- nothing renamed,
    /// nothing missing, nothing extra.
    #[test]
    fn test_request_schema_matches_official_client_models() {
        let mut engine_game = engine_game_with_wall_death();
        engine_game.board.hazards = vec![Point::new(0, 0)];
        engine_game.meta.ruleset_name = "royale".to_string();
        engine_game.meta.royale = Some(rules::RoyaleSettings {
            shrink_every_n_turns: 25,
            seed: 1,
        });

        let wire = Game::from_engine_game(&engine_game, "s1", &HashMap::new(), &HashMap::new());
        let json: Value = serde_json::to_value(&wire).unwrap();

        let snake_fields = |prefix: &str| {
            [
                "id:string",
                "name:string",
                "latency:string",
                "health:number",
                "body[].x:number",
                "body[].y:number",
                "head.x:number",
                "head.y:number",
                "length:number",
                "shout:string",
                "squad:string",
                "customizations.color:string",
                "customizations.head:string",
                "customizations.tail:string",
            ]
            .map(|f| format!("{prefix}.{f}"))
        };
        let mut expected: Vec<String> = [
            "game.id:string",
            "game.ruleset.name:string",
            "game.ruleset.version:string",
            "game.ruleset.settings.foodSpawnChance:number",
            "game.ruleset.settings.minimumFood:number",
            "game.ruleset.settings.hazardDamagePerTurn:number",
            "game.ruleset.settings.hazardMap:string",
            "game.ruleset.settings.hazardMapAuthor:string",
            "game.ruleset.settings.royale.shrinkEveryNTurns:number",
            "game.ruleset.settings.squad.allowBodyCollisions:bool",
            "game.ruleset.settings.squad.sharedElimination:bool",
            "game.ruleset.settings.squad.sharedHealth:bool",
            "game.ruleset.settings.squad.sharedLength:bool",
            "game.map:string",
            "game.timeout:number",
            "game.source:string",
            "turn:number",
            "board.height:number",
            "board.width:number",
            "board.food[].x:number",
            "board.food[].y:number",
            "board.hazards[].x:number",
            "board.hazards[].y:number",
        ]
        .map(String::from)
        .into_iter()
        .chain(snake_fields("board.snakes[]"))
        .chain(snake_fields("you"))
        .collect();
        expected.sort();

        let mut actual = Vec::new();
        leaf_paths(&json, "", &mut actual);
        actual.sort();

        assert_eq!(actual, expected);
        assert_eq!(
            json["game"]["ruleset"]["settings"]["hazardDamagePerTurn"],
            15
        );
        assert_eq!(
            json["game"]["ruleset"]["settings"]["royale"]["shrinkEveryNTurns"],
            25
        );
    }

    /// Solo games look exactly like play's single-snake games: the standard
    /// ruleset on the standard map with standard food/hazard settings (set
    /// by `create_initial_game`). Only the engine's game-over check differs.
    #[test]
    fn test_solo_game_serializes_as_standard() {
        let mut engine_game = create_test_engine_game();
        engine_game.meta.ruleset_name = "solo".to_string();

        let contexts = HashMap::new();
        let customizations = HashMap::new();
        let wire = Game::from_engine_game(&engine_game, "s1", &contexts, &customizations);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert_eq!(json["game"]["ruleset"]["name"], "standard");
        assert_eq!(json["game"]["map"], "standard");
        let settings = &json["game"]["ruleset"]["settings"];
        assert_eq!(settings["foodSpawnChance"], 15);
        assert_eq!(settings["minimumFood"], 1);
        assert_eq!(settings["hazardDamagePerTurn"], 15);
        assert_eq!(settings["royale"]["shrinkEveryNTurns"], 0);
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
            hazard_map: String::new(),
            hazard_map_author: String::new(),
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
