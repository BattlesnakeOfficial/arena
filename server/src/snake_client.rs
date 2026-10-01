//! HTTP client for communicating with Battlesnake servers
//!
//! This module handles all HTTP communication with snake servers following
//! the official Battlesnake API specification.

use reqwest::{Client, Method, StatusCode, header::HeaderMap};
use rules::Direction;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use url::Url;

use crate::config::EngineProxyConfig;
use crate::engine::EngineGame;
use crate::engine::frame::SnakeCustomizations;
use crate::models::battlesnake::EngineRegion;
use crate::wire;

pub(crate) const BODY_READ_CAP_BYTES: usize = 64 * 1024;

const PROXY_TRANSIT_MARGIN: Duration = Duration::from_millis(300);
const LATENCY_HEADER: &str = "x-battlesnake-latency-ms";
const SERVER_ERROR_HEADER: &str = "x-battlesnake-server-error";

#[derive(Clone)]
pub struct SnakeEndpoint {
    pub snake_id: String,
    pub url: String,
    pub engine_region: EngineRegion,
}

pub struct ProxyClients<'a> {
    pub direct: &'a Client,
    pub east: &'a Client,
    pub europe: &'a Client,
    pub config: &'a EngineProxyConfig,
}

impl ProxyClients<'_> {
    fn client_for(&self, region: EngineRegion) -> &Client {
        match region {
            EngineRegion::UsEast4 => self.east,
            EngineRegion::EuropeWest4 => self.europe,
            EngineRegion::UsWest1 => self.direct,
        }
    }
}

pub(crate) enum SnakeRequestRoute {
    Direct(reqwest::RequestBuilder),
    Proxied(reqwest::RequestBuilder),
}

pub(crate) fn build_routed_request(
    clients: &ProxyClients<'_>,
    region: EngineRegion,
    method: Method,
    target: &str,
    timeout: Duration,
) -> SnakeRequestRoute {
    let Some(token) = clients.config.token.as_ref() else {
        return SnakeRequestRoute::Direct(clients.direct.request(method, target));
    };
    let proxy_url = match region {
        EngineRegion::UsWest1 => {
            return SnakeRequestRoute::Direct(clients.direct.request(method, target));
        }
        EngineRegion::UsEast4 => &clients.config.us_east4_url,
        EngineRegion::EuropeWest4 => &clients.config.europe_west4_url,
    };
    SnakeRequestRoute::Proxied(
        clients
            .client_for(region)
            .request(method, proxy_url)
            .header("X-Request-URI", target)
            .header("X-Battlesnake-Timeout-Ms", timeout.as_millis().to_string())
            .header("X-Proxy-Authorization", format!("token {token}")),
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProxyResponseClass {
    ProxyFault,
    SnakeTransportFailure { timed_out: bool, latency_ms: i64 },
    SnakeResponse { latency_ms: i64 },
}

pub(crate) fn classify_proxy_response(
    status: StatusCode,
    headers: &HeaderMap,
) -> ProxyResponseClass {
    let latency = headers
        .get(LATENCY_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| (0..=1_000_000).contains(v));
    let Some(latency_ms) = latency else {
        return ProxyResponseClass::ProxyFault;
    };
    if headers.contains_key(SERVER_ERROR_HEADER) {
        ProxyResponseClass::SnakeTransportFailure {
            timed_out: status == StatusCode::GATEWAY_TIMEOUT,
            latency_ms,
        }
    } else {
        ProxyResponseClass::SnakeResponse { latency_ms }
    }
}

pub(crate) fn log_proxy_fault(region: EngineRegion, kind: &str, status: Option<StatusCode>) {
    tracing::warn!(region = region.as_str(), kind, status = ?status,
        "Engine proxy fault, using fallback");
}

pub(crate) fn log_proxy_auth_failure(region: EngineRegion) {
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<&'static str, (Instant, u64)>>> = OnceLock::new();
    let now = Instant::now();
    let mut entries = LAST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let entry = entries
        .entry(region.as_str())
        .or_insert((now - Duration::from_secs(61), 0));
    if now.duration_since(entry.0) >= Duration::from_secs(60) {
        tracing::error!(
            region = region.as_str(),
            suppressed = entry.1,
            "Engine proxy rejected authorization; retrying snake directly"
        );
        *entry = (now, 0);
    } else {
        entry.1 += 1;
    }
}

pub(crate) enum ProxyCall {
    Response {
        class: ProxyResponseClass,
        status: StatusCode,
        body: String,
    },
    Fault {
        kind: &'static str,
    },
}

pub(crate) async fn execute_proxy(
    builder: reqwest::RequestBuilder,
    timeout: Duration,
) -> ProxyCall {
    let result = tokio::time::timeout(timeout + PROXY_TRANSIT_MARGIN, async {
        let response = builder.send().await?;
        let status = response.status();
        let class = classify_proxy_response(status, response.headers());
        // A proxy-auth rejection is a proxy response, not snake content.
        // Retry direct without waiting for a broken proxy's response body.
        if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED
            && class == ProxyResponseClass::ProxyFault
        {
            return Ok::<_, reqwest::Error>((class, status, String::new()));
        }
        let body = read_body_capped(response, BODY_READ_CAP_BYTES).await?;
        Ok::<_, reqwest::Error>((class, status, body))
    })
    .await;
    match result {
        Ok(Ok((class, status, body))) => ProxyCall::Response {
            class,
            status,
            body,
        },
        Ok(Err(_)) => ProxyCall::Fault { kind: "transport" },
        Err(_) => ProxyCall::Fault {
            kind: "local guard",
        },
    }
}

/// Maximum length of a shout kept after sanitization. Longer shouts are
/// truncated on a char boundary. 256 chars is far beyond any real shout
/// (the board viewer clips much shorter) while bounding what an arbitrary
/// third-party server can push into frame JSON (DEV-1297).
pub(crate) const MAX_SHOUT_CHARS: usize = 256;

/// Whitespace-like separators become a space rather than being dropped, so
/// word boundaries survive sanitization ("gg\nwp" → "gg wp", not "ggwp").
fn map_to_space(c: char) -> char {
    match c {
        '\t' | '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{2028}' | '\u{2029}' => ' ',
        _ => c,
    }
}

/// Invisible / disguising characters, stripped outright:
/// - remaining Cc (C0 minus mapped whitespace, DEL, C1) via `is_control`
/// - Cf format characters: soft hyphen, Arabic letter mark, Khmer inherent
///   vowels, Mongolian vowel separator, ZWSP/ZWNJ, LRM/RLM, bidi embedding
///   and overrides (U+202A–U+202E — the RLO spoofing class), word joiner /
///   invisible separators / bidi isolates (U+2060–U+206F), BOM (U+FEFF),
///   interlinear annotation (U+FFF9–U+FFFB), and Unicode tags
///   (U+E0000–U+E007F — "ASCII smuggling": invisible to humans, readable
///   by models)
/// - invisible non-Cf: combining grapheme joiner, blank-rendering Hangul
///   fillers
///
/// Deliberately KEPT: U+200D (ZWJ) and U+FE00–U+FE0F (variation selectors)
/// — stripping them breaks multi-codepoint emoji (👨‍👩‍👧, ❤️).
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(u32::from(c),
            0x00AD | 0x034F | 0x061C
            | 0x115F | 0x1160 | 0x17B4 | 0x17B5
            | 0x180E
            | 0x200B | 0x200C | 0x200E | 0x200F
            | 0x202A..=0x202E | 0x2060..=0x206F
            | 0x3164 | 0xFFA0
            | 0xFEFF | 0xFFF9..=0xFFFB
            | 0xE0000..=0xE007F)
}

/// Sanitize a shout from a move response: map separators to spaces, strip
/// invisible characters, collapse whitespace runs, trim, truncate to
/// [`MAX_SHOUT_CHARS`] on a char boundary (then trim again). Returns None
/// when nothing printable remains (frame code treats None as "no shout"
/// and serializes "").
pub(crate) fn sanitize_shout(raw: Option<String>) -> Option<String> {
    let s = raw?;
    let mut cleaned = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars().map(map_to_space) {
        if is_invisible(c) {
            continue;
        }
        if c == ' ' {
            if !last_was_space {
                cleaned.push(' ');
            }
            last_was_space = true;
        } else {
            cleaned.push(c);
            last_was_space = false;
        }
    }
    let mut cleaned = cleaned.trim().to_string();
    if cleaned.chars().count() > MAX_SHOUT_CHARS {
        cleaned = cleaned.chars().take(MAX_SHOUT_CHARS).collect();
        cleaned = cleaned.trim_end().to_string();
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

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
    customizations: &HashMap<String, SnakeCustomizations>,
) -> wire::Game {
    wire::Game::from_engine_game(game, snake_id, snake_contexts, customizations)
}

/// Parse a direction string into a Direction enum
///
/// Shared with the on-demand snake health check (`snake_health`) so test
/// results judge moves exactly like real games do.
pub(crate) fn parse_direction(s: &str) -> Option<Direction> {
    s.parse().ok()
}

/// Read a response body chunk by chunk, stopping at `cap` bytes.
///
/// Once the cap is reached the remainder is discarded (we stop reading rather
/// than draining the connection) and the body is marked as truncated, so a
/// hostile server streaming hundreds of megabytes cannot exhaust memory.
pub(crate) async fn read_body_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<String, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::new();
    let mut capped = false;
    while let Some(chunk) = response.chunk().await? {
        if accumulate_body_chunk(&mut buf, &chunk, cap) {
            capped = true;
            break;
        }
    }
    let mut body = String::from_utf8_lossy(&buf).into_owned();
    if capped {
        body.push_str("… [body truncated at read cap]");
    }
    Ok(body)
}

/// Append `chunk` to `buf` without letting `buf` grow past `cap` bytes.
///
/// Returns `true` once the cap is reached, meaning the caller must stop
/// reading and discard any remaining input.
fn accumulate_body_chunk(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let remaining = cap.saturating_sub(buf.len());
    buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    buf.len() >= cap
}

/// Build a URL for a snake endpoint, properly handling query parameters
///
/// This appends the endpoint path (e.g., "move", "start", "end") to the base URL
/// while preserving any query parameters in the correct position.
///
/// Shared with the on-demand snake health check (`snake_health`) so test
/// calls hit exactly the URLs real games would.
pub(crate) fn build_endpoint_url(base_url: &str, endpoint: &str) -> String {
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
#[allow(clippy::too_many_arguments)]
pub async fn request_move(
    client: &Client,
    url: &str,
    game: &EngineGame,
    snake_id: &str,
    timeout: Duration,
    last_direction: Option<Direction>,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) -> MoveResult {
    let request_body = build_request_for_snake(game, snake_id, snake_contexts, customizations);
    let move_url = build_endpoint_url(url, "move");

    let start = Instant::now();

    let result =
        tokio::time::timeout(timeout, client.post(&move_url).json(&request_body).send()).await;

    let elapsed = start.elapsed().as_millis() as i64;

    // `tokio::time::timeout` checks the inner future before its deadline, so
    // a completed send that still exceeds the budget means this task was
    // polled late: an arena-side stall, not a slow snake.
    if result.is_ok() && elapsed > timeout.as_millis() as i64 {
        tracing::warn!(
            metric_type = "late_poll",
            snake_id = %snake_id,
            elapsed_ms = elapsed,
            timeout_ms = timeout.as_millis() as u64,
            "Move request polled after its deadline"
        );
    }

    match result {
        Ok(Ok(response)) => match read_body_capped(response, BODY_READ_CAP_BYTES).await {
            Ok(body) => match serde_json::from_str::<MoveResponse>(&body) {
                Ok(move_response) => {
                    let direction = parse_direction(&move_response.direction)
                        .unwrap_or_else(|| last_direction.unwrap_or(Direction::Up));
                    MoveResult {
                        snake_id: snake_id.to_string(),
                        direction,
                        latency_ms: Some(elapsed),
                        timed_out: false,
                        shout: sanitize_shout(move_response.shout),
                    }
                }
                Err(e) => invalid_move_response(snake_id, last_direction, elapsed, &e),
            },
            Err(e) => invalid_move_response(snake_id, last_direction, elapsed, &e),
        },
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

fn invalid_move_response(
    snake_id: &str,
    last_direction: Option<Direction>,
    elapsed: i64,
    error: &dyn std::fmt::Display,
) -> MoveResult {
    tracing::warn!(
        snake_id = %snake_id,
        error = %error,
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

/// Call /start endpoint (fire and forget, no response expected)
pub async fn request_start(
    client: &Client,
    url: &str,
    game: &EngineGame,
    snake_id: &str,
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) {
    let request_body = build_request_for_snake(game, snake_id, snake_contexts, customizations);
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
    customizations: &HashMap<String, SnakeCustomizations>,
) {
    let request_body = build_request_for_snake(game, snake_id, snake_contexts, customizations);
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
    customizations: &HashMap<String, SnakeCustomizations>,
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
                        customizations,
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
    customizations: &HashMap<String, SnakeCustomizations>,
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
                    request_start(
                        client,
                        url,
                        game,
                        &snake.id,
                        timeout,
                        snake_contexts,
                        customizations,
                    )
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
    customizations: &HashMap<String, SnakeCustomizations>,
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
                    request_end(
                        client,
                        url,
                        game,
                        &snake.id,
                        timeout,
                        snake_contexts,
                        customizations,
                    )
                })
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
        Ok(Ok(response)) => match read_body_capped(response, BODY_READ_CAP_BYTES).await {
            Ok(body) => match serde_json::from_str::<SnakeInfoResponse>(&body) {
                Ok(info) => Some(info),
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "Failed to parse snake info response");
                    None
                }
            },
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

pub async fn request_move_routed(
    clients: &ProxyClients<'_>,
    endpoint: &SnakeEndpoint,
    game: &EngineGame,
    timeout: Duration,
    last_direction: Option<Direction>,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) -> MoveResult {
    let target = build_endpoint_url(&endpoint.url, "move");
    let route = build_routed_request(
        clients,
        endpoint.engine_region,
        Method::POST,
        &target,
        timeout,
    );
    let SnakeRequestRoute::Proxied(builder) = route else {
        return request_move(
            clients.direct,
            &endpoint.url,
            game,
            &endpoint.snake_id,
            timeout,
            last_direction,
            snake_contexts,
            customizations,
        )
        .await;
    };
    let body = build_request_for_snake(game, &endpoint.snake_id, snake_contexts, customizations);
    let fallback = |latency_ms, timed_out| MoveResult {
        snake_id: endpoint.snake_id.clone(),
        direction: last_direction.unwrap_or(Direction::Up),
        latency_ms,
        timed_out,
        shout: None,
    };
    match execute_proxy(builder.json(&body), timeout).await {
        ProxyCall::Response {
            class: ProxyResponseClass::SnakeResponse { latency_ms },
            body,
            ..
        } => match serde_json::from_str::<MoveResponse>(&body) {
            Ok(moved) => MoveResult {
                snake_id: endpoint.snake_id.clone(),
                direction: parse_direction(&moved.direction)
                    .unwrap_or_else(|| last_direction.unwrap_or(Direction::Up)),
                latency_ms: Some(latency_ms),
                timed_out: false,
                shout: sanitize_shout(moved.shout),
            },
            Err(error) => {
                invalid_move_response(&endpoint.snake_id, last_direction, latency_ms, &error)
            }
        },
        ProxyCall::Response {
            class: ProxyResponseClass::SnakeTransportFailure { timed_out, .. },
            status,
            ..
        } => {
            if timed_out {
                tracing::warn!(
                    snake_id = %endpoint.snake_id,
                    timeout_ms = timeout.as_millis(),
                    region = endpoint.engine_region.as_str(),
                    "Snake timed out, using fallback"
                );
            } else {
                tracing::warn!(
                    snake_id = %endpoint.snake_id,
                    region = endpoint.engine_region.as_str(),
                    %status,
                    "Network error calling snake, using fallback"
                );
            }
            fallback(None, true)
        }
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            log_proxy_auth_failure(endpoint.engine_region);
            request_move(
                clients.direct,
                &endpoint.url,
                game,
                &endpoint.snake_id,
                timeout,
                last_direction,
                snake_contexts,
                customizations,
            )
            .await
        }
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } => {
            log_proxy_fault(endpoint.engine_region, "response", Some(status));
            fallback(None, false)
        }
        ProxyCall::Fault { kind } => {
            log_proxy_fault(endpoint.engine_region, kind, None);
            fallback(None, false)
        }
    }
}

async fn request_lifecycle_routed(
    clients: &ProxyClients<'_>,
    endpoint: &SnakeEndpoint,
    game: &EngineGame,
    path: &str,
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) {
    let target = build_endpoint_url(&endpoint.url, path);
    let route = build_routed_request(
        clients,
        endpoint.engine_region,
        Method::POST,
        &target,
        timeout,
    );
    let SnakeRequestRoute::Proxied(builder) = route else {
        if path == "start" {
            request_start(
                clients.direct,
                &endpoint.url,
                game,
                &endpoint.snake_id,
                timeout,
                snake_contexts,
                customizations,
            )
            .await;
        } else {
            request_end(
                clients.direct,
                &endpoint.url,
                game,
                &endpoint.snake_id,
                timeout,
                snake_contexts,
                customizations,
            )
            .await;
        }
        return;
    };
    let body = build_request_for_snake(game, &endpoint.snake_id, snake_contexts, customizations);
    match execute_proxy(builder.json(&body), timeout).await {
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            log_proxy_auth_failure(endpoint.engine_region);
            if path == "start" {
                request_start(
                    clients.direct,
                    &endpoint.url,
                    game,
                    &endpoint.snake_id,
                    timeout,
                    snake_contexts,
                    customizations,
                )
                .await;
            } else {
                request_end(
                    clients.direct,
                    &endpoint.url,
                    game,
                    &endpoint.snake_id,
                    timeout,
                    snake_contexts,
                    customizations,
                )
                .await;
            }
        }
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } => {
            log_proxy_fault(endpoint.engine_region, path, Some(status));
        }
        ProxyCall::Fault { kind } => log_proxy_fault(endpoint.engine_region, kind, None),
        ProxyCall::Response { .. } => {}
    }
}

pub async fn request_info_routed(
    clients: &ProxyClients<'_>,
    endpoint: &SnakeEndpoint,
    timeout: Duration,
) -> Option<SnakeInfoResponse> {
    let route = build_routed_request(
        clients,
        endpoint.engine_region,
        Method::GET,
        &endpoint.url,
        timeout,
    );
    let SnakeRequestRoute::Proxied(builder) = route else {
        return request_info(clients.direct, &endpoint.url, timeout).await;
    };
    match execute_proxy(builder, timeout).await {
        ProxyCall::Response {
            class: ProxyResponseClass::SnakeResponse { .. },
            body,
            ..
        } => match serde_json::from_str(&body) {
            Ok(info) => Some(info),
            Err(error) => {
                tracing::warn!(
                    url = %endpoint.url,
                    region = endpoint.engine_region.as_str(),
                    %error,
                    "Failed to parse snake info response"
                );
                None
            }
        },
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            log_proxy_auth_failure(endpoint.engine_region);
            request_info(clients.direct, &endpoint.url, timeout).await
        }
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } => {
            log_proxy_fault(endpoint.engine_region, "info", Some(status));
            None
        }
        ProxyCall::Fault { kind } => {
            log_proxy_fault(endpoint.engine_region, kind, None);
            None
        }
        ProxyCall::Response {
            class: ProxyResponseClass::SnakeTransportFailure { timed_out, .. },
            status,
            ..
        } => {
            if timed_out {
                tracing::warn!(
                    url = %endpoint.url,
                    region = endpoint.engine_region.as_str(),
                    "Timeout fetching snake info"
                );
            } else {
                tracing::warn!(
                    url = %endpoint.url,
                    region = endpoint.engine_region.as_str(),
                    %status,
                    "Network error fetching snake info"
                );
            }
            None
        }
    }
}

pub async fn request_info_routed_parallel(
    clients: &ProxyClients<'_>,
    endpoints: &[SnakeEndpoint],
    timeout: Duration,
) -> HashMap<String, SnakeInfoResponse> {
    let results = futures::future::join_all(endpoints.iter().map(|endpoint| async move {
        (
            endpoint.snake_id.clone(),
            request_info_routed(clients, endpoint, timeout).await,
        )
    }))
    .await;
    results
        .into_iter()
        .filter_map(|(id, result)| result.map(|r| (id, r)))
        .collect()
}

pub async fn request_start_routed_parallel(
    clients: &ProxyClients<'_>,
    game: &EngineGame,
    endpoints: &[SnakeEndpoint],
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) {
    futures::future::join_all(endpoints.iter().map(|endpoint| {
        request_lifecycle_routed(
            clients,
            endpoint,
            game,
            "start",
            timeout,
            snake_contexts,
            customizations,
        )
    }))
    .await;
}

pub async fn request_end_routed_parallel(
    clients: &ProxyClients<'_>,
    game: &EngineGame,
    endpoints: &[SnakeEndpoint],
    timeout: Duration,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) {
    futures::future::join_all(endpoints.iter().map(|endpoint| {
        request_lifecycle_routed(
            clients,
            endpoint,
            game,
            "end",
            timeout,
            snake_contexts,
            customizations,
        )
    }))
    .await;
}

pub async fn request_moves_routed_parallel(
    clients: &ProxyClients<'_>,
    game: &EngineGame,
    endpoints: &[SnakeEndpoint],
    timeout: Duration,
    last_moves: &HashMap<String, Direction>,
    snake_contexts: &HashMap<String, wire::SnakeContext>,
    customizations: &HashMap<String, SnakeCustomizations>,
) -> Vec<MoveResult> {
    futures::future::join_all(
        game.board
            .snakes
            .iter()
            .filter(|snake| !snake.eliminated_cause.is_eliminated())
            .filter_map(|snake| {
                endpoints
                    .iter()
                    .find(|e| e.snake_id == snake.id)
                    .map(|endpoint| {
                        request_move_routed(
                            clients,
                            endpoint,
                            game,
                            timeout,
                            last_moves.get(&snake.id).copied(),
                            snake_contexts,
                            customizations,
                        )
                    })
            }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire;
    use proptest::prelude::*;
    use tokio::io::AsyncWriteExt as _;
    use wiremock::matchers::{header, method, path};

    #[test]
    fn proxy_response_classification_requires_measured_latency() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            classify_proxy_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, &headers),
            ProxyResponseClass::ProxyFault
        );
        headers.insert(LATENCY_HEADER, "28".parse().unwrap());
        assert_eq!(
            classify_proxy_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, &headers),
            ProxyResponseClass::SnakeResponse { latency_ms: 28 }
        );
        headers.insert(SERVER_ERROR_HEADER, "true".parse().unwrap());
        assert_eq!(
            classify_proxy_response(StatusCode::GATEWAY_TIMEOUT, &headers),
            ProxyResponseClass::SnakeTransportFailure {
                timed_out: true,
                latency_ms: 28
            }
        );
        assert_eq!(
            classify_proxy_response(StatusCode::BAD_GATEWAY, &headers),
            ProxyResponseClass::SnakeTransportFailure {
                timed_out: false,
                latency_ms: 28
            }
        );
    }

    #[tokio::test]
    async fn routed_move_uses_proxy_headers_and_measured_latency() {
        let snake = MockServer::start().await;
        let proxy = MockServer::start().await;
        let target = format!("{}/move", snake.uri());
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("X-Request-URI", target.as_str()))
            .and(header("X-Battlesnake-Timeout-Ms", "500"))
            .and(header("X-Proxy-Authorization", "token sample-secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(LATENCY_HEADER, "28")
                    .set_body_string(r#"{"move":"right"}"#),
            )
            .expect(1)
            .mount(&proxy)
            .await;
        let config = EngineProxyConfig {
            token: Some("sample-secret".to_string()),
            us_east4_url: proxy.uri(),
            europe_west4_url: proxy.uri(),
        };
        let client = Client::new();
        let clients = ProxyClients {
            direct: &client,
            east: &client,
            europe: &client,
            config: &config,
        };
        let endpoint = SnakeEndpoint {
            snake_id: "snake-1".to_string(),
            url: snake.uri(),
            engine_region: EngineRegion::UsEast4,
        };
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        let result = request_move_routed(
            &clients,
            &endpoint,
            &game,
            Duration::from_millis(500),
            Some(Direction::Left),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;
        assert_eq!(result.direction, Direction::Right);
        assert_eq!(result.latency_ms, Some(28));
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn proxy_failure_classes_and_auth_retry() {
        let snake = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&snake)
            .await;
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        for (status, measured, error, direction, timed_out, latency) in [
            (504, true, true, Direction::Left, true, None),
            (502, true, true, Direction::Left, true, None),
            (407, false, false, Direction::Up, false, Some(0)),
        ] {
            let proxy = MockServer::start().await;
            let mut response = ResponseTemplate::new(status);
            if measured {
                response = response.insert_header(LATENCY_HEADER, "35");
            }
            if error {
                response = response.insert_header(SERVER_ERROR_HEADER, "true");
            }
            Mock::given(method("POST"))
                .respond_with(response)
                .expect(1)
                .mount(&proxy)
                .await;
            let config = EngineProxyConfig {
                token: Some("sample-secret".to_string()),
                us_east4_url: proxy.uri(),
                europe_west4_url: proxy.uri(),
            };
            let client = Client::new();
            let clients = ProxyClients {
                direct: &client,
                east: &client,
                europe: &client,
                config: &config,
            };
            let endpoint = SnakeEndpoint {
                snake_id: "snake-1".to_string(),
                url: snake.uri(),
                engine_region: EngineRegion::UsEast4,
            };
            let result = request_move_routed(
                &clients,
                &endpoint,
                &game,
                Duration::from_millis(500),
                Some(Direction::Left),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await;
            assert_eq!(result.direction, direction);
            assert_eq!(result.timed_out, timed_out);
            if status != 407 {
                assert_eq!(result.latency_ms, latency);
            }
        }
    }

    #[tokio::test]
    async fn forwarded_snake_statuses_are_not_proxy_faults() {
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        for status in [407, 500, 504] {
            let proxy = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header(LATENCY_HEADER, "41")
                        .set_body_string(r#"{"move":"down"}"#),
                )
                .expect(1)
                .mount(&proxy)
                .await;
            let config = EngineProxyConfig {
                token: Some("secret".to_string()),
                us_east4_url: proxy.uri(),
                europe_west4_url: proxy.uri(),
            };
            let client = Client::new();
            let clients = ProxyClients {
                direct: &client,
                east: &client,
                europe: &client,
                config: &config,
            };
            let endpoint = SnakeEndpoint {
                snake_id: "snake-1".to_string(),
                url: "https://unreachable.example".to_string(),
                engine_region: EngineRegion::UsEast4,
            };
            let result = request_move_routed(
                &clients,
                &endpoint,
                &game,
                Duration::from_millis(500),
                None,
                &HashMap::new(),
                &HashMap::new(),
            )
            .await;
            assert_eq!(result.direction, Direction::Down);
            assert_eq!(result.latency_ms, Some(41));
            assert!(!result.timed_out);
        }
    }

    #[tokio::test]
    async fn west_and_unset_token_bypass_proxy() {
        let snake = MockServer::start().await;
        let proxy = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .expect(2)
            .mount(&snake)
            .await;
        let client = Client::new();
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        for (region, token) in [
            (EngineRegion::UsWest1, Some("secret".to_string())),
            (EngineRegion::UsEast4, None),
        ] {
            let config = EngineProxyConfig {
                token,
                us_east4_url: proxy.uri(),
                europe_west4_url: proxy.uri(),
            };
            let clients = ProxyClients {
                direct: &client,
                east: &client,
                europe: &client,
                config: &config,
            };
            let endpoint = SnakeEndpoint {
                snake_id: "snake-1".to_string(),
                url: snake.uri(),
                engine_region: region,
            };
            let result = request_move_routed(
                &clients,
                &endpoint,
                &game,
                Duration::from_millis(500),
                None,
                &HashMap::new(),
                &HashMap::new(),
            )
            .await;
            assert_eq!(result.direction, Direction::Up);
        }
        assert!(proxy.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn hanging_proxy_hits_local_guard_without_marking_snake_timed_out() {
        let proxy = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&proxy)
            .await;
        let config = EngineProxyConfig {
            token: Some("secret".to_string()),
            us_east4_url: proxy.uri(),
            europe_west4_url: proxy.uri(),
        };
        let client = Client::new();
        let clients = ProxyClients {
            direct: &client,
            east: &client,
            europe: &client,
            config: &config,
        };
        let endpoint = SnakeEndpoint {
            snake_id: "snake-1".to_string(),
            url: "https://snake.example".to_string(),
            engine_region: EngineRegion::UsEast4,
        };
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        let start = Instant::now();
        let result = request_move_routed(
            &clients,
            &endpoint,
            &game,
            Duration::from_millis(100),
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!result.timed_out);
        assert!(result.latency_ms.is_none());
    }

    #[tokio::test]
    async fn measured_450ms_move_survives_proxy_transit() {
        let proxy = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(LATENCY_HEADER, "450")
                    .set_body_string(r#"{"move":"up"}"#)
                    .set_delay(Duration::from_millis(640)),
            )
            .mount(&proxy)
            .await;
        let config = EngineProxyConfig {
            token: Some("secret".to_string()),
            us_east4_url: proxy.uri(),
            europe_west4_url: proxy.uri(),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let clients = ProxyClients {
            direct: &client,
            east: &client,
            europe: &client,
            config: &config,
        };
        let endpoint = SnakeEndpoint {
            snake_id: "snake-1".to_string(),
            url: "https://snake.example".to_string(),
            engine_region: EngineRegion::EuropeWest4,
        };
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        let result = request_move_routed(
            &clients,
            &endpoint,
            &game,
            Duration::from_millis(500),
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;
        assert_eq!(result.direction, Direction::Up);
        assert_eq!(result.latency_ms, Some(450));
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn stalled_proxy_body_is_caught_by_local_guard() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Battlesnake-Latency-Ms: 20\r\n\r\n")
                .await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let config = EngineProxyConfig {
            token: Some("secret".to_string()),
            us_east4_url: format!("http://{address}"),
            europe_west4_url: format!("http://{address}"),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let clients = ProxyClients {
            direct: &client,
            east: &client,
            europe: &client,
            config: &config,
        };
        let endpoint = SnakeEndpoint {
            snake_id: "snake-1".to_string(),
            url: "https://snake.example".to_string(),
            engine_region: EngineRegion::UsEast4,
        };
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        let started = Instant::now();
        let result = request_move_routed(
            &clients,
            &endpoint,
            &game,
            Duration::from_millis(100),
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(result.latency_ms.is_none());
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn bare_407_retries_direct_without_waiting_for_body() {
        let snake = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"right"}"#))
            .expect(1)
            .mount(&snake)
            .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 100\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let config = EngineProxyConfig {
            token: Some("bad-token".to_string()),
            us_east4_url: format!("http://{address}"),
            europe_west4_url: format!("http://{address}"),
        };
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let clients = ProxyClients {
            direct: &client,
            east: &client,
            europe: &client,
            config: &config,
        };
        let endpoint = SnakeEndpoint {
            snake_id: "snake-1".to_string(),
            url: snake.uri(),
            engine_region: EngineRegion::UsEast4,
        };
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);
        let started = Instant::now();
        let result = request_move_routed(
            &clients,
            &endpoint,
            &game,
            Duration::from_millis(500),
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(result.direction, Direction::Right);
    }
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn body_chunks_under_cap_accumulate_losslessly() {
        let mut buf = Vec::new();
        assert!(!accumulate_body_chunk(&mut buf, b"hello ", 64));
        assert!(!accumulate_body_chunk(&mut buf, b"world", 64));
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn body_chunk_exceeding_cap_is_cut_at_cap() {
        let mut buf = Vec::new();
        let big = vec![b'x'; BODY_READ_CAP_BYTES * 3];
        assert!(accumulate_body_chunk(&mut buf, &big, BODY_READ_CAP_BYTES));
        assert_eq!(buf.len(), BODY_READ_CAP_BYTES);
    }

    #[test]
    fn body_chunk_landing_exactly_on_cap_stops_reading() {
        let mut buf = vec![b'a'; 10];
        assert!(accumulate_body_chunk(&mut buf, b"bbbbbb", 16));
        assert_eq!(buf.len(), 16);
        assert!(accumulate_body_chunk(&mut buf, b"ccc", 16));
        assert_eq!(buf.len(), 16);
    }

    #[tokio::test]
    async fn oversized_info_response_is_rejected() {
        let server = MockServer::start().await;
        let body = format!(
            "{}{{\"color\":\"#ff00ff\"}}",
            " ".repeat(BODY_READ_CAP_BYTES)
        );
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let result = request_info(&Client::new(), &server.uri(), Duration::from_secs(2)).await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn oversized_move_response_uses_fallback() {
        let server = MockServer::start().await;
        let body = format!(
            "{}{{\"move\":\"up\",\"shout\":\"oops\"}}",
            " ".repeat(BODY_READ_CAP_BYTES)
        );
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let game = create_test_engine_game_with_snakes(vec!["snake-1"]);

        let result = request_move(
            &Client::new(),
            &server.uri(),
            &game,
            "snake-1",
            Duration::from_secs(2),
            Some(Direction::Left),
            &HashMap::new(),
            &HashMap::new(),
        )
        .await;

        assert_eq!(result.direction, Direction::Left);
        assert!(result.shout.is_none());
        assert!(!result.timed_out);
        assert!(result.latency_ms.is_some());
    }

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
                royale: None,
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
        let customizations = HashMap::new();
        let request = build_request_for_snake(&game, "snake-2", &contexts, &customizations);

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

        let customizations = HashMap::new();
        let request = build_request_for_snake(&game, "snake-1", &contexts, &customizations);

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

    // === Shout sanitization (DEV-1297) ===

    #[test]
    fn sanitize_shout_none_and_empty_stay_none() {
        assert_eq!(sanitize_shout(None), None);
        assert_eq!(sanitize_shout(Some(String::new())), None);
        assert_eq!(sanitize_shout(Some("   \t ".to_string())), None);
        // Zero-width-only input has nothing printable left.
        assert_eq!(
            sanitize_shout(Some("\u{200B}\u{FEFF}\u{2060}".to_string())),
            None
        );
    }

    #[test]
    fn sanitize_shout_strips_control_and_zero_width() {
        let out = sanitize_shout(Some("a\u{0000}b\u{001F}c\u{007F}d\u{009C}".to_string()));
        assert_eq!(out.as_deref(), Some("abcd"));
        let out = sanitize_shout(Some("a\u{200B}b\u{FEFF}c\u{2060}d".to_string()));
        assert_eq!(out.as_deref(), Some("abcd"));
        // A 4-byte emoji survives untouched.
        let out = sanitize_shout(Some("\u{1F40D} rules".to_string()));
        assert_eq!(out.as_deref(), Some("\u{1F40D} rules"));
    }

    #[test]
    fn sanitize_shout_strips_bidi_override_and_tag_characters() {
        // RLO + "kcuf": what a human sees (and what Jev must judge) is "kcuf".
        let out = sanitize_shout(Some("\u{202E}kcuf".to_string()));
        assert_eq!(out.as_deref(), Some("kcuf"));
        // A run of Unicode tag characters (ASCII smuggling) disappears.
        let out = sanitize_shout(Some("ok\u{E0041}\u{E0042}!".to_string()));
        assert_eq!(out.as_deref(), Some("ok!"));
    }

    #[test]
    fn sanitize_shout_maps_line_separators_to_spaces() {
        // U+2028/U+2029 are separators, not deletions.
        let out = sanitize_shout(Some("gg\u{2028}wp\u{2029}gg".to_string()));
        assert_eq!(out.as_deref(), Some("gg wp gg"));
    }

    #[test]
    fn sanitize_shout_preserves_multi_codepoint_emoji() {
        // Family emoji joined with ZWJ must survive byte-identical.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let out = sanitize_shout(Some(family.to_string()));
        assert_eq!(out.as_deref(), Some(family));
        // VS16 (heart + variation selector) survives too.
        let heart = "\u{2764}\u{FE0F}";
        let out = sanitize_shout(Some(heart.to_string()));
        assert_eq!(out.as_deref(), Some(heart));
    }

    #[test]
    fn sanitize_shout_collapses_whitespace_runs() {
        assert_eq!(
            sanitize_shout(Some("gg\nwp".to_string())).as_deref(),
            Some("gg wp")
        );
        assert_eq!(
            sanitize_shout(Some("a\t\tb".to_string())).as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn sanitize_shout_truncates_on_char_boundary() {
        let long = "x".repeat(300);
        let out = sanitize_shout(Some(long)).unwrap();
        assert_eq!(out.chars().count(), MAX_SHOUT_CHARS);

        // 300 chars where position 256 falls inside a 4-byte emoji: take()
        // operates on chars, so no panic and the result is exactly 256
        // chars (the truncation cuts cleanly before the emoji that starts
        // at 256; nothing straddles).
        let mut mixed = String::new();
        for _ in 0..255 {
            mixed.push('x');
        }
        for _ in 0..45 {
            mixed.push('\u{1F40D}');
        }
        assert_eq!(mixed.chars().count(), 300);
        let out = sanitize_shout(Some(mixed)).unwrap();
        assert_eq!(out.chars().count(), MAX_SHOUT_CHARS);
        assert!(out.chars().take(255).all(|c| c == 'x'));
        assert_eq!(out.chars().nth(255), Some('\u{1F40D}'));
    }

    #[test]
    fn sanitize_shout_trims_before_counting_budget() {
        // Leading/trailing whitespace does not eat into the 256 budget.
        let mut padded = " ".repeat(50);
        padded.push_str(&"x".repeat(MAX_SHOUT_CHARS));
        padded.push_str(&" ".repeat(50));
        let out = sanitize_shout(Some(padded)).unwrap();
        assert_eq!(out.chars().count(), MAX_SHOUT_CHARS);
    }

    #[test]
    fn sanitize_shout_keeps_playful_trash_talk_verbatim() {
        // Regression guard for the trash-talk requirement (DEV-1297): the
        // canonical playful shouts must pass through sanitization intact.
        for shout in ["get rekt", "I'm coming for you", "gg ez"] {
            assert_eq!(
                sanitize_shout(Some(shout.to_string())).as_deref(),
                Some(shout)
            );
        }
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
