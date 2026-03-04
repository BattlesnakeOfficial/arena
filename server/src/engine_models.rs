//! Types for deserializing game data from the legacy Battlesnake Engine database.
//!
//! The Engine stores games and frames as JSONB with PascalCase field names (from protobuf).

use std::collections::HashMap;
use std::fmt;

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
fn normalize_game_type(engine_type: &str) -> String {
    match engine_type.to_lowercase().as_str() {
        "standard" => "Standard".to_string(),
        "royale" => "Royale".to_string(),
        "constrictor" => "Constrictor".to_string(),
        "snail_mode" | "snail mode" => "Snail Mode".to_string(),
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
    /// Convert the Created timestamp (microseconds) to a DateTime
    pub fn created_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(self.created).unwrap_or(DateTime::UNIX_EPOCH)
    }

    /// Get the board size as a string like "11x11"
    pub fn board_size(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    /// Get the game type from the ruleset name, normalized to canonical form
    pub fn game_type(&self) -> String {
        let raw = self.ruleset_name.as_deref().unwrap_or("standard");
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
    #[serde(rename = "URL")]
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
    #[serde(
        rename = "APIVersion",
        default,
        deserialize_with = "deserialize_string_or_int"
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
    #[serde(rename = "ProxyURL")]
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

/// Combined export format for archiving a complete game to GCS.
#[derive(Debug, Clone, Serialize)]
pub struct GameExport {
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
    }

    #[test]
    fn normalize_game_type_unknown_preserved() {
        assert_eq!(normalize_game_type("solo"), "solo");
        assert_eq!(normalize_game_type("wrapped"), "wrapped");
        assert_eq!(normalize_game_type("custom_ruleset"), "custom_ruleset");
    }
}
