//! On-demand snake URL health checks (BS-015).
//!
//! Runs the same four HTTP calls a real game makes — `GET /` (identity),
//! `POST /start`, `POST /move`, and `POST /end` — against a snake's URL and
//! reports per-call results. The POST payloads are built through the exact
//! same paths the game runner uses (`engine::create_initial_game` +
//! `wire::Game::from_engine_game`), so a snake that passes the test sees the
//! same wire format a real game would send it.

use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::engine::EngineGame;
use crate::models::battlesnake::Battlesnake;
use crate::models::game::{GameBoardSize, GameType};
use crate::models::game_battlesnake::GameBattlesnakeWithDetails;
use crate::snake_client::{MoveResponse, build_endpoint_url, parse_direction};
use crate::wire;

/// Generous per-call budget for on-demand tests.
///
/// Real games only allow `engine_game.meta.timeout` (500ms) per request; the
/// test is deliberately forgiving so users can see a slow-but-working snake,
/// and the latency column shows whether they'd fit the real budget.
pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of characters of raw response body shown in results.
const BODY_EXCERPT_MAX_CHARS: usize = 500;

/// Result of a single test call against a snake endpoint.
pub struct HealthCheckCall {
    /// Human-readable endpoint name, e.g. `"GET /"` or `"POST /move"`.
    pub name: &'static str,
    pub ok: bool,
    /// HTTP status code, when a response was received at all.
    pub http_status: Option<u16>,
    /// Round-trip latency, when a response was received.
    pub latency_ms: Option<u64>,
    /// Human-readable outcome details (parsed fields on success, the error
    /// in plain terms on failure).
    pub summary: String,
    /// Truncated raw response body, shown for failed calls to aid debugging.
    pub body_excerpt: Option<String>,
}

/// Full report of a snake health check run.
pub struct HealthCheckReport {
    pub calls: Vec<HealthCheckCall>,
    /// The per-request timeout real games enforce, for display next to the
    /// measured latencies.
    pub game_timeout_ms: i64,
}

impl HealthCheckReport {
    pub fn failure_count(&self) -> usize {
        self.calls.iter().filter(|c| !c.ok).count()
    }
}

/// What we expect back from a given endpoint.
enum Expectation {
    /// `GET /`: JSON with `apiversion == "1"` (plus author/version metadata).
    Info,
    /// `POST /move`: JSON with a valid `move` field.
    Move,
    /// `POST /start` / `POST /end`: any 2xx; real games ignore the body.
    Ack,
}

/// Raw result of executing one HTTP call.
enum CallOutcome {
    Response {
        status: u16,
        latency_ms: u64,
        body: String,
    },
    Failed {
        latency_ms: Option<u64>,
        summary: String,
    },
}

/// The identity response from `GET /`.
///
/// All fields are optional at the serde level so we can distinguish
/// "missing apiversion" from "unparseable JSON" and report each clearly.
#[derive(Debug, Deserialize)]
struct InfoResponse {
    #[serde(default)]
    apiversion: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

/// Build the single-snake test game used for the health check payloads.
///
/// This goes through `engine::create_initial_game` — the exact function real
/// games use — with a standard 11x11 board, so the `/start` and `/move`
/// payloads are structurally identical to what a real game sends on turn 0.
/// The game id is a fresh UUID that never touches the games table.
///
/// Returns the engine game and the generated snake id (the `you.id` the
/// snake will see).
pub fn build_test_game(snake: &Battlesnake) -> (EngineGame, String) {
    let now = chrono::Utc::now();
    let game_battlesnake_id = Uuid::new_v4();

    // Fabricate the row shape a real game would load from the DB so we can
    // reuse the engine's board initialization verbatim.
    let details = GameBattlesnakeWithDetails {
        game_battlesnake_id,
        game_id: Uuid::new_v4(),
        battlesnake_id: snake.battlesnake_id,
        placement: None,
        created_at: now,
        updated_at: now,
        name: snake.name.clone(),
        url: snake.url.clone(),
        user_id: snake.user_id,
        leaderboard_entry_id: None,
        color: snake.color.clone(),
        head: snake.head.clone(),
        tail: snake.tail.clone(),
    };

    let engine_game = crate::engine::create_initial_game(
        Uuid::new_v4(),
        GameBoardSize::Medium,
        GameType::Standard,
        &[details],
    );

    (engine_game, game_battlesnake_id.to_string())
}

/// Run the four test calls sequentially against the snake's URL.
///
/// The caller supplies the HTTP client (with its own timeout policy) and the
/// test game built by [`build_test_game`].
pub async fn run_health_check(
    client: &Client,
    url: &str,
    engine_game: &EngineGame,
    snake_id: &str,
) -> HealthCheckReport {
    // No previous-turn context, exactly like a real game's /start and first
    // /move calls.
    let contexts: HashMap<String, wire::SnakeContext> = HashMap::new();
    let payload = wire::Game::from_engine_game(engine_game, snake_id, &contexts);

    let mut calls = Vec::with_capacity(4);

    let outcome = execute_call(client.get(url), HEALTH_CHECK_TIMEOUT).await;
    calls.push(evaluate_call("GET /", &Expectation::Info, outcome));

    let start_url = build_endpoint_url(url, "start");
    let outcome = execute_call(client.post(&start_url).json(&payload), HEALTH_CHECK_TIMEOUT).await;
    calls.push(evaluate_call("POST /start", &Expectation::Ack, outcome));

    let move_url = build_endpoint_url(url, "move");
    let outcome = execute_call(client.post(&move_url).json(&payload), HEALTH_CHECK_TIMEOUT).await;
    calls.push(evaluate_call("POST /move", &Expectation::Move, outcome));

    // Call /end to be a good citizen: the wire protocol calls it, and snakes
    // may have allocated per-game state on /start.
    let end_url = build_endpoint_url(url, "end");
    let outcome = execute_call(client.post(&end_url).json(&payload), HEALTH_CHECK_TIMEOUT).await;
    calls.push(evaluate_call("POST /end", &Expectation::Ack, outcome));

    HealthCheckReport {
        calls,
        game_timeout_ms: engine_game.meta.timeout,
    }
}

/// Execute one HTTP call with a timeout, mirroring how `snake_client` wraps
/// its requests in `tokio::time::timeout`.
async fn execute_call(builder: reqwest::RequestBuilder, timeout: Duration) -> CallOutcome {
    let start = Instant::now();

    match tokio::time::timeout(timeout, builder.send()).await {
        Ok(Ok(response)) => {
            let status = response.status().as_u16();
            match response.text().await {
                Ok(body) => CallOutcome::Response {
                    status,
                    latency_ms: start.elapsed().as_millis() as u64,
                    body,
                },
                Err(e) => CallOutcome::Failed {
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                    summary: format!(
                        "Received HTTP {status} but failed to read the response body: {}",
                        describe_request_error(&e)
                    ),
                },
            }
        }
        Ok(Err(e)) => CallOutcome::Failed {
            latency_ms: Some(start.elapsed().as_millis() as u64),
            summary: describe_request_error(&e),
        },
        Err(_) => CallOutcome::Failed {
            latency_ms: None,
            summary: format!(
                "Timed out: no response within {} seconds",
                timeout.as_secs()
            ),
        },
    }
}

/// Turn a reqwest error into a human-readable explanation.
fn describe_request_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Timed out waiting for a response".to_string()
    } else if e.is_connect() {
        format!("Could not connect to the server (is it running and reachable?): {e}")
    } else if e.is_redirect() {
        format!("Redirect problem (too many redirects?): {e}")
    } else {
        format!("Request failed: {e}")
    }
}

/// Judge a raw call outcome against what the endpoint is expected to return.
fn evaluate_call(
    name: &'static str,
    expectation: &Expectation,
    outcome: CallOutcome,
) -> HealthCheckCall {
    match outcome {
        CallOutcome::Failed {
            latency_ms,
            summary,
        } => HealthCheckCall {
            name,
            ok: false,
            http_status: None,
            latency_ms,
            summary,
            body_excerpt: None,
        },
        CallOutcome::Response {
            status,
            latency_ms,
            body,
        } => {
            if !(200..300).contains(&status) {
                return HealthCheckCall {
                    name,
                    ok: false,
                    http_status: Some(status),
                    latency_ms: Some(latency_ms),
                    summary: format!("Returned non-success HTTP status {status}"),
                    body_excerpt: Some(truncate_excerpt(&body)),
                };
            }

            let (ok, summary) = match expectation {
                Expectation::Info => evaluate_info_body(&body),
                Expectation::Move => evaluate_move_body(&body),
                Expectation::Ack => (
                    true,
                    "Acknowledged. Real games ignore this response body.".to_string(),
                ),
            };

            let body_excerpt = if ok {
                None
            } else {
                Some(truncate_excerpt(&body))
            };

            HealthCheckCall {
                name,
                ok,
                http_status: Some(status),
                latency_ms: Some(latency_ms),
                summary,
                body_excerpt,
            }
        }
    }
}

/// Validate the `GET /` identity response body.
///
/// The arena (like the official engine) requires `apiversion` to be `"1"`.
fn evaluate_info_body(body: &str) -> (bool, String) {
    match serde_json::from_str::<InfoResponse>(body) {
        Ok(info) => match info.apiversion.as_deref() {
            Some("1") => {
                let author = info.author.as_deref().unwrap_or("(not set)");
                let version = info.version.as_deref().unwrap_or("(not set)");
                (
                    true,
                    format!("apiversion: 1 | author: {author} | version: {version}"),
                )
            }
            Some(other) => (
                false,
                format!("Unsupported apiversion {other:?} — the arena requires \"1\""),
            ),
            None => (
                false,
                "Response JSON is missing the required \"apiversion\" field (must be \"1\")"
                    .to_string(),
            ),
        },
        Err(e) => (false, format!("Response body was not valid JSON: {e}")),
    }
}

/// Validate a `POST /move` response body.
fn evaluate_move_body(body: &str) -> (bool, String) {
    match serde_json::from_str::<MoveResponse>(body) {
        Ok(mv) => match parse_direction(&mv.direction) {
            Some(direction) => {
                let mut summary = format!("Move: {direction}");
                if let Some(shout) = &mv.shout {
                    summary.push_str(&format!(" | Shout: {shout:?}"));
                }
                (true, summary)
            }
            None => (
                false,
                format!(
                    "Invalid move {:?} — must be one of up, down, left, right \
                     (real games would fall back to the snake's previous direction)",
                    mv.direction
                ),
            ),
        },
        Err(e) => (
            false,
            format!("Response body was not valid JSON with a \"move\" field: {e}"),
        ),
    }
}

/// Truncate a raw response body to a sane display length (char-boundary safe).
fn truncate_excerpt(body: &str) -> String {
    let mut chars = body.chars();
    let mut excerpt: String = chars.by_ref().take(BODY_EXCERPT_MAX_CHARS).collect();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::battlesnake::Visibility;

    fn test_snake() -> Battlesnake {
        let now = chrono::Utc::now();
        Battlesnake {
            battlesnake_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "Test Snake".to_string(),
            url: "http://localhost:8000".to_string(),
            visibility: Visibility::Private,
            color: "#ff0000".to_string(),
            head: "default".to_string(),
            tail: "default".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    // === evaluate_info_body ===

    #[test]
    fn info_body_valid() {
        let (ok, summary) = evaluate_info_body(
            r##"{"apiversion":"1","author":"coreyja","version":"0.1.0","color":"#888888"}"##,
        );
        assert!(ok);
        assert!(summary.contains("apiversion: 1"));
        assert!(summary.contains("author: coreyja"));
        assert!(summary.contains("version: 0.1.0"));
    }

    #[test]
    fn info_body_missing_optional_metadata_still_passes() {
        let (ok, summary) = evaluate_info_body(r#"{"apiversion":"1"}"#);
        assert!(ok);
        assert!(summary.contains("author: (not set)"));
        assert!(summary.contains("version: (not set)"));
    }

    #[test]
    fn info_body_wrong_apiversion_fails() {
        let (ok, summary) = evaluate_info_body(r#"{"apiversion":"2","author":"x"}"#);
        assert!(!ok);
        assert!(summary.contains("Unsupported apiversion"));
        assert!(summary.contains('2'));
    }

    #[test]
    fn info_body_missing_apiversion_fails() {
        let (ok, summary) = evaluate_info_body(r#"{"author":"x"}"#);
        assert!(!ok);
        assert!(summary.contains("missing the required \"apiversion\""));
    }

    #[test]
    fn info_body_invalid_json_fails() {
        let (ok, summary) = evaluate_info_body("<html>not json</html>");
        assert!(!ok);
        assert!(summary.contains("not valid JSON"));
    }

    // === evaluate_move_body ===

    #[test]
    fn move_body_valid() {
        let (ok, summary) = evaluate_move_body(r#"{"move":"up"}"#);
        assert!(ok);
        assert!(summary.contains("Move: up"));
        assert!(!summary.contains("Shout"));
    }

    #[test]
    fn move_body_valid_with_shout() {
        let (ok, summary) = evaluate_move_body(r#"{"move":"left","shout":"hello!"}"#);
        assert!(ok);
        assert!(summary.contains("Move: left"));
        assert!(summary.contains("Shout: \"hello!\""));
    }

    #[test]
    fn move_body_case_insensitive_direction() {
        let (ok, summary) = evaluate_move_body(r#"{"move":"DOWN"}"#);
        assert!(ok, "real games accept any-cased directions: {summary}");
    }

    #[test]
    fn move_body_invalid_direction_fails() {
        let (ok, summary) = evaluate_move_body(r#"{"move":"north"}"#);
        assert!(!ok);
        assert!(summary.contains("Invalid move"));
        assert!(summary.contains("north"));
    }

    #[test]
    fn move_body_missing_move_field_fails() {
        let (ok, summary) = evaluate_move_body(r#"{"shout":"no move here"}"#);
        assert!(!ok);
        assert!(summary.contains("\"move\" field"));
    }

    #[test]
    fn move_body_invalid_json_fails() {
        let (ok, summary) = evaluate_move_body("Internal Server Error");
        assert!(!ok);
        assert!(summary.contains("not valid JSON"));
    }

    // === evaluate_call ===

    #[test]
    fn non_2xx_status_fails_with_body_excerpt() {
        let call = evaluate_call(
            "POST /move",
            &Expectation::Move,
            CallOutcome::Response {
                status: 404,
                latency_ms: 12,
                body: "Not Found".to_string(),
            },
        );
        assert!(!call.ok);
        assert_eq!(call.http_status, Some(404));
        assert_eq!(call.latency_ms, Some(12));
        assert!(call.summary.contains("404"));
        assert_eq!(call.body_excerpt.as_deref(), Some("Not Found"));
    }

    #[test]
    fn ack_endpoint_passes_on_2xx_regardless_of_body() {
        let call = evaluate_call(
            "POST /start",
            &Expectation::Ack,
            CallOutcome::Response {
                status: 200,
                latency_ms: 5,
                body: "whatever".to_string(),
            },
        );
        assert!(call.ok);
        assert_eq!(call.http_status, Some(200));
        assert!(call.body_excerpt.is_none());
    }

    #[test]
    fn failed_outcome_maps_to_failed_call() {
        let call = evaluate_call(
            "GET /",
            &Expectation::Info,
            CallOutcome::Failed {
                latency_ms: None,
                summary: "Timed out: no response within 5 seconds".to_string(),
            },
        );
        assert!(!call.ok);
        assert_eq!(call.http_status, None);
        assert_eq!(call.latency_ms, None);
        assert!(call.summary.contains("Timed out"));
    }

    #[test]
    fn successful_info_call_has_no_body_excerpt() {
        let call = evaluate_call(
            "GET /",
            &Expectation::Info,
            CallOutcome::Response {
                status: 200,
                latency_ms: 8,
                body: r#"{"apiversion":"1"}"#.to_string(),
            },
        );
        assert!(call.ok);
        assert!(call.body_excerpt.is_none());
    }

    // === truncate_excerpt ===

    #[test]
    fn truncate_short_body_unchanged() {
        assert_eq!(truncate_excerpt("hello"), "hello");
    }

    #[test]
    fn truncate_long_body() {
        let body = "x".repeat(2000);
        let excerpt = truncate_excerpt(&body);
        assert_eq!(excerpt.chars().count(), BODY_EXCERPT_MAX_CHARS + 1);
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let body = "é".repeat(600);
        let excerpt = truncate_excerpt(&body);
        assert_eq!(excerpt.chars().count(), BODY_EXCERPT_MAX_CHARS + 1);
    }

    // === build_test_game ===

    #[test]
    fn test_game_matches_real_game_shape() {
        let snake = test_snake();
        let (game, snake_id) = build_test_game(&snake);

        // Standard 11x11 board with exactly our one snake on it.
        assert_eq!(game.board.width, 11);
        assert_eq!(game.board.height, 11);
        assert_eq!(game.board.snakes.len(), 1);
        assert_eq!(game.board.snakes[0].id, snake_id);
        assert_eq!(game.board.turn, 0);

        // Same meta a real standard game gets.
        assert_eq!(game.meta.ruleset_name, "standard");
        assert_eq!(game.meta.timeout, 500);
        assert!(game.meta.royale.is_none());

        // Snake name flows through to the wire payload.
        assert_eq!(game.snake_names.get(&snake_id), Some(&snake.name));
    }

    #[test]
    fn test_game_wire_payload_is_strict() {
        let snake = test_snake();
        let (game, snake_id) = build_test_game(&snake);
        let contexts = HashMap::new();
        let payload = wire::Game::from_engine_game(&game, &snake_id, &contexts);
        let json = serde_json::to_value(&payload).unwrap();

        // All top-level wire fields present, `you` is our snake.
        assert!(json.get("game").is_some());
        assert!(json.get("turn").is_some());
        assert!(json.get("board").is_some());
        assert_eq!(json["you"]["id"], snake_id);
        assert_eq!(json["you"]["name"], "Test Snake");
        assert_eq!(json["game"]["ruleset"]["name"], "standard");
        assert_eq!(json["game"]["timeout"], 500);
    }
}
