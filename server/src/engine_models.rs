//! Types for deserializing game data from the legacy Battlesnake Engine database.
//!
//! The Engine stores games and frames as JSONB with PascalCase field names (from protobuf).

use std::collections::HashMap;
use std::fmt;

#[cfg(test)]
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};

/// Deserialize a value that could be either a string or an integer into an Option<String>.
/// Older games stored Latency as an integer, newer ones as a string.
fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrInt;

    impl<'de> de::Visitor<'de> for StringOrInt {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string, integer, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(value.to_string()))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(value))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(value.to_string()))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(value.to_string()))
        }
    }

    deserializer.deserialize_any(StringOrInt)
}

/// Normalize an engine game type string to its canonical form.
/// Known types are mapped to PascalCase; unknown types are passed through as-is.
pub(crate) fn normalize_game_type(engine_type: &str) -> String {
    match engine_type.to_lowercase().as_str() {
        "standard" => "Standard".to_string(),
        "royale" => "Royale".to_string(),
        "constrictor" => "Constrictor".to_string(),
        "snail_mode" | "snail mode" => "Snail Mode".to_string(),
        "solo" => "Solo".to_string(),
        _ => engine_type.to_string(),
    }
}

/// Game metadata from the Engine's `games` table `value` column.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineGame {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "Width", default)]
    pub width: i32,
    #[serde(rename = "Height", default)]
    pub height: i32,
    #[serde(rename = "Ruleset", default)]
    pub ruleset: HashMap<String, String>,
    #[serde(rename = "SnakeTimeout", default)]
    pub snake_timeout: i32,
    #[serde(rename = "MaxTurns", default)]
    pub max_turns: i32,
    /// "multi-player"/"team"/"single-player" — present on 994 pre-July-2020
    /// games (DEV-2344). Absent on everything else; `skip_serializing_if`
    /// keeps other games' serialized bytes identical.
    #[serde(rename = "Mode", default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// A JSON number in the engine (always 12; 815 headers, DEV-2344). `i32`
    /// matches the sibling `MaxTurns`.
    #[serde(
        rename = "MaxTurnsToNextFoodSpawn",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_turns_to_next_food_spawn: Option<i32>,
    #[serde(rename = "FoodSpawns", default)]
    pub food_spawns: Vec<PointOnTurn>,
    #[serde(rename = "HazardSpawns", default)]
    pub hazard_spawns: Vec<PointOnTurn>,
    #[serde(rename = "Source")]
    pub source: Option<String>,
    #[serde(rename = "RulesetName")]
    pub ruleset_name: Option<String>,
    #[serde(rename = "RulesStages")]
    pub rules_stages: Option<Vec<String>>,
    #[serde(rename = "Map")]
    pub map: Option<String>,
    /// Unix timestamp in microseconds
    #[serde(rename = "Created", default)]
    pub created: i64,
}

impl EngineGame {
    /// Get the board size as a string like "11x11"
    pub fn board_size(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    /// Get the game type from the ruleset name, normalized to canonical form.
    /// `RulesetName` wins when present; pre-2022 games usually carry only
    /// `Ruleset.name` (e.g. Eric's solo run), so fall back to that (DEV-1984).
    pub fn game_type(&self) -> String {
        let raw = self
            .ruleset_name
            .as_deref()
            .or_else(|| self.ruleset.get("name").map(String::as_str))
            .unwrap_or("standard");
        normalize_game_type(raw)
    }
}

/// A single frame/turn of a game from the Engine's `game_frames` table `value` column.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineGameFrame {
    #[serde(rename = "Turn", default)]
    pub turn: i32,
    #[serde(rename = "Snakes", default)]
    pub snakes: Vec<EngineSnake>,
    #[serde(rename = "Food", default)]
    pub food: Vec<Point>,
    #[serde(rename = "Hazards", default)]
    pub hazards: Vec<Point>,
    #[serde(rename = "GameState", default)]
    pub game_state: HashMap<String, String>,
    #[serde(rename = "PointState", default)]
    pub point_state: Vec<PointState>,
}

/// A snake in a game frame.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineSnake {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "URL", skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "Body", default)]
    pub body: Vec<Point>,
    #[serde(rename = "Health", default)]
    pub health: i32,
    #[serde(rename = "Death")]
    pub death: Option<Death>,
    #[serde(rename = "Color")]
    pub color: Option<String>,
    #[serde(rename = "HeadType")]
    pub head_type: Option<String>,
    #[serde(rename = "TailType")]
    pub tail_type: Option<String>,
    #[serde(
        rename = "Latency",
        default,
        deserialize_with = "deserialize_string_or_int"
    )]
    pub latency: Option<String>,
    #[serde(rename = "Shout")]
    pub shout: Option<String>,
    #[serde(rename = "Squad")]
    pub squad: Option<String>,
    /// Team-mode team name (DEV-2344): non-empty in 403 team-mode games
    /// (2020-03-27 to 2020-07-17), `""` elsewhere. Absent ⇒ `None` ⇒
    /// omitted on serialize, so other games' bytes don't change.
    #[serde(rename = "Team", default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(
        rename = "APIVersion",
        default,
        deserialize_with = "deserialize_string_or_int",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_version: Option<String>,
    #[serde(rename = "Author")]
    pub author: Option<String>,
    #[serde(rename = "StatusCode")]
    pub status_code: Option<i32>,
    #[serde(rename = "Error")]
    pub error: Option<String>,
    #[serde(rename = "TimingMicros", default)]
    pub timing_micros: HashMap<String, i32>,
    #[serde(rename = "IsBot")]
    pub is_bot: Option<bool>,
    #[serde(rename = "IsEnvironment")]
    pub is_environment: Option<bool>,
    #[serde(rename = "ProxyURL", skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
}

/// Death information for a snake.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Death {
    #[serde(rename = "Cause", default)]
    pub cause: String,
    #[serde(rename = "Turn", default)]
    pub turn: i32,
    #[serde(rename = "EliminatedBy")]
    pub eliminated_by: Option<String>,
}

/// A point on the board.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Point {
    #[serde(rename = "X", default)]
    pub x: i32,
    #[serde(rename = "Y", default)]
    pub y: i32,
}

/// A point with an associated turn number.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PointOnTurn {
    #[serde(rename = "X", default)]
    pub x: i32,
    #[serde(rename = "Y", default)]
    pub y: i32,
    #[serde(rename = "Turn", default)]
    pub turn: i32,
}

/// State associated with a point on the board.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PointState {
    #[serde(rename = "X", default)]
    pub x: i32,
    #[serde(rename = "Y", default)]
    pub y: i32,
    #[serde(rename = "Value", default)]
    pub value: i32,
}

/// Current archive object format version, written as the first field of every
/// new object and into `games.archive_version`. Increment when the export
/// format changes.
///
/// v2 (DEV-1978): top-level `archive_version` + `game_id`, zstd level 9,
/// snake `URL`/`ProxyURL`/`APIVersion` omitted.
pub const ARCHIVE_VERSION: i32 = 2;

/// Combined export format for archiving a complete game to GCS.
///
/// `archive_version` and `game_id` are declared first so they serialize
/// first: a reader that has only decompressed the object can identify the
/// format and the game before interpreting anything else.
///
/// Test-only since DEV-1984: production writes through the streamed
/// [`crate::backup::ExportWriter`], which must stay byte-identical to
/// `serde_json::to_vec(&GameExport{..})` (pinned by a byte-equality test).
#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
pub struct GameExport {
    pub archive_version: i32,
    pub game_id: String,
    pub game: EngineGame,
    pub frames: Vec<EngineGameFrame>,
    pub exported_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_game_type_known_values() {
        assert_eq!(normalize_game_type("standard"), "Standard");
        assert_eq!(normalize_game_type("royale"), "Royale");
        assert_eq!(normalize_game_type("constrictor"), "Constrictor");
        assert_eq!(normalize_game_type("snail_mode"), "Snail Mode");
        assert_eq!(normalize_game_type("STANDARD"), "Standard");
        assert_eq!(normalize_game_type("solo"), "Solo");
        assert_eq!(normalize_game_type("Solo"), "Solo");
    }

    #[test]
    fn normalize_game_type_unknown_preserved() {
        assert_eq!(normalize_game_type("wrapped"), "wrapped");
        assert_eq!(normalize_game_type("custom_ruleset"), "custom_ruleset");
    }

    fn game_from_json(ruleset_name: Option<&str>, ruleset: serde_json::Value) -> EngineGame {
        serde_json::from_value(serde_json::json!({
            "ID": "g-type",
            "Status": "complete",
            "Width": 11,
            "Height": 11,
            "Source": null,
            "RulesetName": ruleset_name,
            "Ruleset": ruleset,
            "RulesStages": null,
            "Map": "standard",
        }))
        .unwrap()
    }

    /// DEV-1984: `RulesetName` is absent on nearly all games before 2022,
    /// while `Ruleset.name` is usually present — the game type must fall
    /// back to it instead of defaulting everything to "Standard".
    #[test]
    fn game_type_falls_back_to_ruleset_name() {
        // (a) RulesetName absent, Ruleset.name present.
        let game = game_from_json(None, serde_json::json!({"name": "solo"}));
        assert_eq!(game.game_type(), "Solo");

        // (b) Both present: RulesetName wins (same precedence as the walk
        // tick's SQL COALESCE).
        let game = game_from_json(Some("standard"), serde_json::json!({"name": "solo"}));
        assert_eq!(game.game_type(), "Standard");

        // (c) Neither key: default.
        let game = game_from_json(None, serde_json::json!({}));
        assert_eq!(game.game_type(), "Standard");

        // (d) Unknown value passes through, like normalize_game_type.
        let game = game_from_json(None, serde_json::json!({"name": "wrapped"}));
        assert_eq!(game.game_type(), "wrapped");
    }

    /// Go's `omitempty` omits zero coordinates, so archived frames look like
    /// `{"X":1}` or `{}`. `Point`'s `#[serde(default)]` handles it — pinned
    /// here because every frame comparison depends on it.
    #[test]
    fn point_omitted_y_parses_as_zero() {
        let point: Point = serde_json::from_str(r#"{"X":1}"#).unwrap();
        assert_eq!((point.x, point.y), (1, 0));

        let point: Point = serde_json::from_str("{}").unwrap();
        assert_eq!((point.x, point.y), (0, 0));
    }

    /// DEV-2344: snake `Team`, header `Mode`, and header
    /// `MaxTurnsToNextFoodSpawn` parse and re-serialize with their values
    /// intact — `Team`/`Mode` as strings, `MaxTurnsToNextFoodSpawn` as a
    /// JSON NUMBER (an `Option<String>` would fail all 815 real headers).
    #[test]
    fn team_mode_maxturns_round_trip() {
        let game: EngineGame = serde_json::from_value(serde_json::json!({
            "ID": "g-2344",
            "Status": "complete",
            "Source": null,
            "RulesetName": "standard",
            "RulesStages": null,
            "Map": "standard",
            "Mode": "team",
            "MaxTurnsToNextFoodSpawn": 12,
        }))
        .unwrap();
        assert_eq!(game.mode.as_deref(), Some("team"));
        assert_eq!(game.max_turns_to_next_food_spawn, Some(12));

        let game_value = serde_json::to_value(&game).unwrap();
        assert_eq!(game_value["Mode"], "team");
        assert!(game_value["MaxTurnsToNextFoodSpawn"].is_number());
        assert_eq!(game_value["MaxTurnsToNextFoodSpawn"], serde_json::json!(12));

        let snake: EngineSnake = serde_json::from_value(serde_json::json!({
            "ID": "s1", "Name": "Snek", "Team": "Orb Weavers",
        }))
        .unwrap();
        assert_eq!(snake.team.as_deref(), Some("Orb Weavers"));
        let snake_value = serde_json::to_value(&snake).unwrap();
        assert_eq!(snake_value["Team"], "Orb Weavers");

        // The engine also writes `""` outside team mode: parses to Some("")
        // and serializes as an empty string.
        let snake: EngineSnake = serde_json::from_value(serde_json::json!({
            "ID": "s2", "Name": "Solo", "Team": "",
        }))
        .unwrap();
        assert_eq!(snake.team.as_deref(), Some(""));
        assert_eq!(serde_json::to_value(&snake).unwrap()["Team"], "");
    }

    /// DEV-2344: without the three keys the fields are `None` and the
    /// serialized output contains none of them — the pin that other
    /// games' archived bytes stay byte-identical.
    #[test]
    fn team_mode_maxturns_absent_keys_are_omitted() {
        let game: EngineGame = serde_json::from_value(serde_json::json!({
            "ID": "g-absent",
            "Status": "complete",
            "Source": null,
            "RulesetName": "standard",
            "RulesStages": null,
            "Map": "standard",
        }))
        .unwrap();
        assert!(game.mode.is_none());
        assert!(game.max_turns_to_next_food_spawn.is_none());
        let game_json = serde_json::to_vec(&game).unwrap();
        let game_str = String::from_utf8(game_json).unwrap();
        assert!(!game_str.contains("\"Mode\""));
        assert!(!game_str.contains("\"MaxTurnsToNextFoodSpawn\""));

        let snake: EngineSnake = serde_json::from_value(serde_json::json!({
            "ID": "s1", "Name": "Snek",
        }))
        .unwrap();
        assert!(snake.team.is_none());
        let snake_json = String::from_utf8(serde_json::to_vec(&snake).unwrap()).unwrap();
        assert!(!snake_json.contains("\"Team\""));
    }
}
