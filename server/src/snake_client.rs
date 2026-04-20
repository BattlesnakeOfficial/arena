//! HTTP client for communicating with Battlesnake servers
//!
//! This module handles all HTTP communication with snake servers following
//! the official Battlesnake API specification.

use reqwest::Client;
use rules::Direction;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use url::Url;

use crate::engine::EngineGame;
use crate::wire;

/// Response from a snake's /move endpoint
#[derive(Debug, Deserialize)]
pub struct MoveResponse {
    #[serde(rename = "move")]
    pub direction: String,
    pub shout: Option<String>,
}

/// Result of a move request including timing info
#[derive(Debug, Clone)]
pub struct MoveResult {
    pub snake_id: String,
    pub direction: Direction,
    pub latency_ms: Option<i64>,
    pub timed_out: bool,
    pub shout: Option<String>,
}

/// Build the request body for a specific snake
///
/// The Battlesnake API expects the `you` field to be set to the snake
/// that the request is being sent to.
fn build_request_for_snake(
    game: &EngineGame,
    snake_id: &str,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) -> wire::Game {
    wire::Game::from_engine_game(game, snake_id, snake_contexts)
}

/// Parse a direction string into a Direction enum
fn parse_direction(s: &str) -> Option<Direction> {
    s.parse().ok()
}

/// Build a URL for a snake endpoint, properly handling query parameters
///
/// This appends the endpoint path (e.g., "move", "start", "end") to the base URL
/// while preserving any query parameters in the correct position.
fn build_endpoint_url(base_url: &str, endpoint: &str) -> String {
    // Try to parse as a proper URL
    if let Ok(mut url) = Url::parse(base_url) {
        // Get the current path, trim trailing slashes, and append the endpoint
        let current_path = url.path().trim_end_matches('/');
        let new_path = format!("{}/{}", current_path, endpoint);
        url.set_path(&new_path);
        url.to_string()
    } else {
        // Fallback to simple string concatenation if URL parsing fails
        format!("{}/{}", base_url.trim_end_matches('/'), endpoint)
    }
}

/// Call a snake's /move endpoint
///
/// On timeout or error, falls back to the last direction (or Up if no last direction).
pub async fn request_move(
    client: &Client,
    url: &str,
    game: &EngineGame,
    snake_id: &str,
    timeout: Duration,
    last_direction: Option<Direction>,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) -> MoveResult {
    let request_body = build_request_for_snake(game, snake_id, snake_contexts);
    let move_url = build_endpoint_url(url, "move");

    let start = Instant::now();

    let result =
        tokio::time::timeout(timeout, client.post(&move_url).json(&request_body).send()).await;

    let elapsed = start.elapsed().as_millis() as i64;

    match result {
        Ok(Ok(response)) => {
            match response.json::<MoveResponse>().await {
                Ok(move_response) => {
                    let direction = parse_direction(&move_response.direction)
                        .unwrap_or_else(|| last_direction.unwrap_or(Direction::Up));
                    MoveResult {
                        snake_id: snake_id.to_string(),
                        direction,
                        latency_ms: Some(elapsed),
                        timed_out: false,
                        shout: move_response.shout,
                    }
                }
                Err(e) => {
                    // JSON parse error - use fallback
                    tracing::warn!(
                        snake_id = %snake_id,
                        error = %e,
                        "Failed to parse move response, using fallback"
                    );
                    MoveResult {
                        snake_id: snake_id.to_string(),
                        direction: last_direction.unwrap_or(Direction::Up),
                        latency_ms: Some(elapsed),
                        timed_out: false,
                        shout: None,
                    }
                }
            }
        }
        Ok(Err(e)) => {
            // Network error - continue in same direction
            tracing::warn!(
                snake_id = %snake_id,
                error = %e,
                "Network error calling snake, using fallback"
            );
            MoveResult {
                snake_id: snake_id.to_string(),
                direction: last_direction.unwrap_or(Direction::Up),
                latency_ms: None,
                timed_out: true,
                shout: None,
            }
        }
        Err(_) => {
            // Timeout - continue in same direction
            tracing::warn!(
                snake_id = %snake_id,
                timeout_ms = timeout.as_millis(),
                "Snake timed out, using fallback"
            );
            MoveResult {
                snake_id: snake_id.to_string(),
                direction: last_direction.unwrap_or(Direction::Up),
                latency_ms: None,
                timed_out: true,
                shout: None,
            }
        }
    }
}

/// Call /start endpoint (fire and forget, no response expected)
pub async fn request_start(
    client: &Client,
    url: &str,
    game: &EngineGame,
    snake_id: &str,
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) {
    let request_body = build_request_for_snake(game, snake_id, snake_contexts);
    let start_url = build_endpoint_url(url, "start");

    // Fire and forget - ignore result but log errors
    match tokio::time::timeout(timeout, client.post(&start_url).json(&request_body).send()).await {
        Ok(Ok(_)) => {
            tracing::debug!(snake_id = %snake_id, "Called /start successfully");
        }
        Ok(Err(e)) => {
            tracing::warn!(snake_id = %snake_id, error = %e, "Failed to call /start");
        }
        Err(_) => {
            tracing::warn!(snake_id = %snake_id, "Timeout calling /start");
        }
    }
}

/// Call /end endpoint (fire and forget, no response expected)
pub async fn request_end(
    client: &Client,
    url: &str,
    game: &EngineGame,
    snake_id: &str,
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) {
    let request_body = build_request_for_snake(game, snake_id, snake_contexts);
    let end_url = build_endpoint_url(url, "end");

    // Fire and forget - ignore result but log errors
    match tokio::time::timeout(timeout, client.post(&end_url).json(&request_body).send()).await {
        Ok(Ok(_)) => {
            tracing::debug!(snake_id = %snake_id, "Called /end successfully");
        }
        Ok(Err(e)) => {
            tracing::warn!(snake_id = %snake_id, error = %e, "Failed to call /end");
        }
        Err(_) => {
            tracing::warn!(snake_id = %snake_id, "Timeout calling /end");
        }
    }
}

/// Request moves from all alive snakes in parallel
///
/// Returns a MoveResult for each alive snake.
pub async fn request_moves_parallel(
    client: &Client,
    game: &EngineGame,
    snake_urls: &[(String, String)], // (snake_id, url)
    timeout: Duration,
    last_moves: &HashMap<String, Direction>,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) -> Vec<MoveResult> {
    let futures: Vec<_> = game
        .board
        .snakes
        .iter()
        .filter(|s| !s.eliminated_cause.is_eliminated())
        .filter_map(|snake| {
            snake_urls
                .iter()
                .find(|(id, _)| id == &snake.id)
                .map(|(_, url)| {
                    let last_direction = last_moves.get(&snake.id).copied();
                    request_move(
                        client,
                        url,
                        game,
                        &snake.id,
                        timeout,
                        last_direction,
                        snake_contexts,
                    )
                })
        })
        .collect();

    futures::future::join_all(futures).await
}

/// Call /start for all snakes in parallel
pub async fn request_start_parallel(
    client: &Client,
    game: &EngineGame,
    snake_urls: &[(String, String)],
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) {
    let futures: Vec<_> = game
        .board
        .snakes
        .iter()
        .filter_map(|snake| {
            snake_urls
                .iter()
                .find(|(id, _)| id == &snake.id)
                .map(|(_, url)| {
                    request_start(client, url, game, &snake.id, timeout, snake_contexts)
                })
        })
        .collect();

    futures::future::join_all(futures).await;
}

/// Call /end for all snakes in parallel
pub async fn request_end_parallel(
    client: &Client,
    game: &EngineGame,
    snake_urls: &[(String, String)],
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
) {
    let futures: Vec<_> = game
        .board
        .snakes
        .iter()
        .filter_map(|snake| {
            snake_urls
                .iter()
                .find(|(id, _)| id == &snake.id)
                .map(|(_, url)| request_end(client, url, game, &snake.id, timeout, snake_contexts))
        })
        .collect();

    futures::future::join_all(futures).await;
}

#[derive(Debug, Deserialize, Default)]
pub struct InfoCustomizations {
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub tail: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct SnakeInfoResponse {
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub tail: Option<String>,
    #[serde(default)]
    pub customizations: Option<InfoCustomizations>,
}

pub async fn request_info(
    client: &Client,
    url: &str,
    timeout: Duration,
) -> Option<SnakeInfoResponse> {
    match tokio::time::timeout(timeout, client.get(url).send()).await {
        Ok(Ok(response)) => match response.json::<SnakeInfoResponse>().await {
            Ok(info) => Some(info),
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "Failed to parse snake info response");
                None
            }
        },
        Ok(Err(e)) => {
            tracing::warn!(url = %url, error = %e, "Network error fetching snake info");
            None
        }
        Err(_) => {
            tracing::warn!(url = %url, "Timeout fetching snake info");
            None
        }
    }
}

pub async fn request_info_parallel(
    client: &Client,
    snake_urls: &[(String, String)], // (snake_id, url)
    timeout: Duration,
) -> HashMap<String, SnakeInfoResponse> {
    let futures: Vec<_> = snake_urls
        .iter()
        .map(|(id, url)| {
            let id = id.clone();
            let url = url.clone();
            async move {
                let info = request_info(client, &url, timeout).await;
                (id, info)
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;
    results
        .into_iter()
        .filter_map(|(id, info)| info.map(|i| (id, i)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire;
    use proptest::prelude::*;

    #[test]
    fn test_build_endpoint_url_simple() {
        let url = build_endpoint_url("https://example.com", "move");
        assert_eq!(url, "https://example.com/move");
    }

    #[test]
    fn test_build_endpoint_url_with_trailing_slash() {
        let url = build_endpoint_url("https://example.com/", "move");
        assert_eq!(url, "https://example.com/move");
    }

    #[test]
    fn test_build_endpoint_url_with_path() {
        let url = build_endpoint_url("https://example.com/api/v1", "move");
        assert_eq!(url, "https://example.com/api/v1/move");
    }

    #[test]
    fn test_build_endpoint_url_with_query_params() {
        let url = build_endpoint_url("https://example.com?token=secret", "move");
        assert_eq!(url, "https://example.com/move?token=secret");
    }

    #[test]
    fn test_build_endpoint_url_with_path_and_query_params() {
        let url = build_endpoint_url("https://example.com/api?token=secret&version=2", "move");
        assert_eq!(url, "https://example.com/api/move?token=secret&version=2");
    }

    #[test]
    fn test_build_endpoint_url_with_trailing_slash_and_query_params() {
        let url = build_endpoint_url("https://example.com/api/?token=secret", "start");
        assert_eq!(url, "https://example.com/api/start?token=secret");
    }

    #[test]
    fn test_build_endpoint_url_all_endpoints() {
        let base = "https://snake.example.com?auth=abc123";
        assert_eq!(
            build_endpoint_url(base, "move"),
            "https://snake.example.com/move?auth=abc123"
        );
        assert_eq!(
            build_endpoint_url(base, "start"),
            "https://snake.example.com/start?auth=abc123"
        );
        assert_eq!(
            build_endpoint_url(base, "end"),
            "https://snake.example.com/end?auth=abc123"
        );
    }

    fn create_test_engine_game_with_snakes(snake_ids: Vec<&str>) -> EngineGame {
        use rules::{BoardState, EliminationCause, Point, Snake, StandardSettings};

        let snakes: Vec<Snake> = snake_ids
            .iter()
            .map(|id| Snake {
                id: id.to_string(),
                body: vec![Point::new(5, 5), Point::new(5, 4), Point::new(5, 3)],
                health: 100,
                eliminated_cause: EliminationCause::NotEliminated,
                eliminated_by: String::new(),
                eliminated_on_turn: 0,
            })
            .collect();

        let mut snake_names = std::collections::HashMap::new();
        for id in &snake_ids {
            snake_names.insert(id.to_string(), format!("Snake {}", id));
        }

        EngineGame {
            board: BoardState {
                turn: 5,
                width: 11,
                height: 11,
                food: vec![Point::new(3, 3)],
                snakes,
                hazards: vec![],
            },
            meta: crate::engine::GameMeta {
                game_id: "test-game".to_string(),
                ruleset_name: "standard".to_string(),
                timeout: 500,
                settings: StandardSettings::default(),
            },
            snake_names,
        }
    }

    #[test]
    fn test_parse_direction() {
        assert_eq!(parse_direction("up"), Some(Direction::Up));
        assert_eq!(parse_direction("UP"), Some(Direction::Up));
        assert_eq!(parse_direction("Down"), Some(Direction::Down));
        assert_eq!(parse_direction("left"), Some(Direction::Left));
        assert_eq!(parse_direction("RIGHT"), Some(Direction::Right));
        assert_eq!(parse_direction("invalid"), None);
        assert_eq!(parse_direction(""), None);
    }

    #[test]
    fn test_move_result_clone() {
        let result = MoveResult {
            snake_id: "test".to_string(),
            direction: Direction::Up,
            latency_ms: Some(100),
            timed_out: false,
            shout: Some("hello".to_string()),
        };
        let cloned = result.clone();
        assert_eq!(cloned.snake_id, "test");
        assert_eq!(cloned.direction, Direction::Up);
        assert_eq!(cloned.latency_ms, Some(100));
        assert!(!cloned.timed_out);
        assert_eq!(cloned.shout, Some("hello".to_string()));
    }

    #[test]
    fn test_build_request_for_snake_sets_you_field() {
        let game = create_test_engine_game_with_snakes(vec!["snake-1", "snake-2"]);
        let contexts = HashMap::<String, wire::SnakeContext>::new();

        // Build request for snake2 - the `you` field should be snake2
        let request = build_request_for_snake(&game, "snake-2", &contexts);

        assert_eq!(request.you.id, "snake-2");
        assert_eq!(request.you.name, "Snake snake-2");
        // Board should be preserved
        assert_eq!(request.board.snakes.len(), 2);
        assert_eq!(request.turn, 5);
        assert_eq!(request.game.id, "test-game");
    }

    #[test]
    fn test_build_request_for_snake_preserves_board() {
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        let contexts = HashMap::<String, wire::SnakeContext>::new();

        let request = build_request_for_snake(&game, "snake-1", &contexts);

        // All board properties should be preserved
        assert_eq!(request.board.height, 11);
        assert_eq!(request.board.width, 11);
        assert_eq!(request.board.food.len(), 1);
        assert_eq!(request.board.food[0].x, 3);
        assert_eq!(request.board.food[0].y, 3);
    }

    #[test]
    fn test_move_response_deserialization() {
        let json = r#"{"move": "up"}"#;
        let response: MoveResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.direction, "up");
        assert!(response.shout.is_none());
    }

    #[test]
    fn test_move_response_deserialization_with_shout() {
        let json = r#"{"move": "down", "shout": "I'm coming for you!"}"#;
        let response: MoveResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.direction, "down");
        assert_eq!(response.shout, Some("I'm coming for you!".to_string()));
    }

    #[test]
    fn test_move_response_deserialization_case_sensitivity() {
        let json = r#"{"move": "LEFT"}"#;
        let response: MoveResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.direction, "LEFT");
        assert_eq!(parse_direction(&response.direction), Some(Direction::Left));
    }

    // === Property-based tests ===

    /// Strategy that generates a valid Direction variant
    fn arb_direction() -> impl Strategy<Value = Direction> {
        prop_oneof![
            Just(Direction::Up),
            Just(Direction::Down),
            Just(Direction::Left),
            Just(Direction::Right),
        ]
    }

    /// Strategy that generates a valid base URL with optional path and query params
    fn arb_base_url() -> impl Strategy<Value = String> {
        let scheme = prop_oneof![Just("http"), Just("https")];
        let host = prop_oneof![
            Just("example.com".to_string()),
            Just("snake.io".to_string()),
            Just("localhost:8080".to_string()),
            Just("192.168.1.1:3000".to_string()),
        ];
        let path = prop_oneof![
            Just("".to_string()),
            Just("/".to_string()),
            Just("/api".to_string()),
            Just("/api/v1".to_string()),
            Just("/api/v1/".to_string()),
            Just("/snakes/my-snake".to_string()),
        ];
        let query = prop_oneof![
            Just("".to_string()),
            Just("?token=abc".to_string()),
            Just("?token=abc&version=2".to_string()),
            Just("?auth=secret123".to_string()),
        ];
        (scheme, host, path, query).prop_map(|(s, h, p, q)| format!("{}://{}{}{}", s, h, p, q))
    }

    /// Strategy for the three battlesnake endpoints
    fn arb_endpoint() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("move"), Just("start"), Just("end"),]
    }

    proptest! {
        // -- build_endpoint_url properties --

        #[test]
        fn prop_endpoint_url_is_parseable(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            prop_assert!(Url::parse(&result).is_ok(),
                "Result '{}' should be a valid URL", result);
        }

        #[test]
        fn prop_endpoint_url_preserves_scheme(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = Url::parse(&base).unwrap();
            let result_parsed = Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.scheme(), result_parsed.scheme());
        }

        #[test]
        fn prop_endpoint_url_preserves_host(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = Url::parse(&base).unwrap();
            let result_parsed = Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.host_str(), result_parsed.host_str());
        }

        #[test]
        fn prop_endpoint_url_preserves_query(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = Url::parse(&base).unwrap();
            let result_parsed = Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.query(), result_parsed.query());
        }

        #[test]
        fn prop_endpoint_url_contains_endpoint(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let result_parsed = Url::parse(&result).unwrap();
            prop_assert!(result_parsed.path().ends_with(&format!("/{}", endpoint)),
                "Path '{}' should end with '/{}'", result_parsed.path(), endpoint);
        }

        #[test]
        fn prop_endpoint_url_no_double_slashes(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let result_parsed = Url::parse(&result).unwrap();
            prop_assert!(!result_parsed.path().contains("//"),
                "Path '{}' should not contain double slashes", result_parsed.path());
        }

        #[test]
        fn prop_endpoint_url_preserves_port(
            base in arb_base_url(),
            endpoint in arb_endpoint()
        ) {
            let result = build_endpoint_url(&base, endpoint);
            let base_parsed = Url::parse(&base).unwrap();
            let result_parsed = Url::parse(&result).unwrap();
            prop_assert_eq!(base_parsed.port(), result_parsed.port());
        }

        // -- parse_direction properties --

        #[test]
        fn prop_parse_direction_round_trip(m in arb_direction()) {
            let s = m.to_string();
            prop_assert_eq!(parse_direction(&s), Some(m));
        }

        #[test]
        fn prop_parse_direction_case_insensitive(m in arb_direction()) {
            let s = m.to_string();
            // Lowercase
            prop_assert_eq!(parse_direction(&s.to_lowercase()), Some(m));
            // Uppercase
            prop_assert_eq!(parse_direction(&s.to_uppercase()), Some(m));
            // Title case
            let title: String = s.chars().enumerate()
                .map(|(i, c)| if i == 0 { c.to_uppercase().next().unwrap() } else { c })
                .collect();
            prop_assert_eq!(parse_direction(&title), Some(m));
        }

        #[test]
        fn prop_parse_direction_rejects_non_directions(s in "[a-z]{1,10}") {
            let lower = s.to_lowercase();
            if lower != "up" && lower != "down" && lower != "left" && lower != "right" {
                prop_assert_eq!(parse_direction(&s), None);
            }
        }

        #[test]
        fn prop_parse_direction_rejects_empty_and_whitespace(
            padding in "\\s{0,5}"
        ) {
            if !padding.is_empty() {
                prop_assert_eq!(parse_direction(&format!("{}up", padding)), None);
                prop_assert_eq!(parse_direction(&format!("up{}", padding)), None);
            }
        }
    }

    // === Test scaffold for BS-d6da131bea2c4868: Snake customization support ===

    #[test]
    fn test_snake_info_response_full_customizations() {
        let json =
            r##"{"customizations": {"color": "#ff0000", "head": "bendr", "tail": "fat-rattle"}}"##;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        let c = info.customizations.unwrap();
        assert_eq!(
            c.color, "#ff0000",
            "color should be parsed from customizations object"
        );
        assert_eq!(
            c.head, "bendr",
            "head style should be parsed from customizations object"
        );
        assert_eq!(
            c.tail, "fat-rattle",
            "tail style should be parsed from customizations object"
        );
    }

    #[test]
    fn test_snake_info_response_empty() {
        let json = r#"{}"#;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        assert!(
            info.customizations.is_none(),
            "missing customizations should deserialize as None"
        );
        assert!(
            info.color.is_none(),
            "missing top-level color should deserialize as None"
        );
    }

    #[test]
    fn test_snake_info_response_top_level_color() {
        let json = r##"{"color": "#00ff00"}"##;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            info.color,
            Some("#00ff00".to_string()),
            "top-level color should be captured"
        );
        assert!(
            info.customizations.is_none(),
            "customizations should be None when not present"
        );
    }

    #[test]
    fn test_snake_info_response_partial_customizations() {
        let json = r##"{"customizations": {"color": "#abcdef"}}"##;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        let c = info.customizations.unwrap();
        assert_eq!(c.color, "#abcdef", "color should be parsed");
        assert_eq!(c.head, "", "missing head should default to empty string");
        assert_eq!(c.tail, "", "missing tail should default to empty string");
    }

    #[test]
    fn test_snake_info_response_both_top_level_and_customizations_color() {
        let json = r##"{"color": "#111111", "customizations": {"color": "#222222", "head": "default", "tail": "default"}}"##;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            info.color,
            Some("#111111".to_string()),
            "top-level color should be captured"
        );
        let c = info.customizations.unwrap();
        assert_eq!(
            c.color, "#222222",
            "customizations color should be captured separately"
        );
    }

    #[test]
    fn test_info_customizations_defaults() {
        let c = InfoCustomizations::default();
        assert_eq!(c.color, "", "default color should be empty");
        assert_eq!(c.head, "", "default head should be empty");
        assert_eq!(c.tail, "", "default tail should be empty");
    }

    #[test]
    fn test_snake_info_response_top_level_head_and_tail() {
        let json = r##"{"apiversion":"1","author":"coreyja","color":"#AA66CC","head":"trans-rights-scarf","tail":"bolt","version":null}"##;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            info.color,
            Some("#AA66CC".to_string()),
            "top-level color should be captured"
        );
        assert_eq!(
            info.head,
            Some("trans-rights-scarf".to_string()),
            "top-level head should be captured"
        );
        assert_eq!(
            info.tail,
            Some("bolt".to_string()),
            "top-level tail should be captured"
        );
        assert!(
            info.customizations.is_none(),
            "customizations should be None when not present"
        );
    }

    #[test]
    fn test_snake_info_response_top_level_head_null_tail() {
        let json =
            r##"{"apiversion":"1","color":"#AA66CC","head":"trans-rights-scarf","tail":null}"##;
        let info: SnakeInfoResponse = serde_json::from_str(json).unwrap();
        assert_eq!(info.head, Some("trans-rights-scarf".to_string()));
        assert_eq!(info.tail, None, "null tail should deserialize as None");
    }
}
