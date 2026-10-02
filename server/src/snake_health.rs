//! Snake health checks (BS-015, DEV-1515): the on-demand "Test Snake"
//! button and the background sweeper's probes.
//!
//! A check plays a short but complete game against the snake, built through
//! the same paths the game runner uses (`engine::create_initial_game`,
//! `engine::apply_turn`, `wire::Game::from_engine_game`) and shaped like a
//! real leaderboard game: the leaderboard's mode, board size and snake count,
//! with synthetic opponents. The snake sees `/start`, a turn-0 `/move`, a
//! turn-1 `/move`, then `/end` with the final board: the opponents make one
//! safe move on turn 0 and turn back into their own neck on turn 1, so the
//! snake under test wins and the game ends the way a real one does.
//!
//! Results distinguish what breaks real games ([`HealthCallStatus::SnakeFailure`])
//! from spec problems games tolerate ([`HealthCallStatus::Warning`]) and from
//! engine-proxy trouble that isn't the snake's fault
//! ([`HealthCallStatus::ProxyFault`]).

use futures::future::join_all;
use rules::Direction;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::engine::frame::SnakeCustomizations;
use crate::engine::{EngineGame, GameSource};
use crate::models::battlesnake::Battlesnake;
use crate::models::battlesnake::EngineRegion;
use crate::models::game::{GameBoardSize, GameType};
use crate::models::game_battlesnake::GameBattlesnakeWithDetails;
use crate::snake_client::{
    BODY_READ_CAP_BYTES, MoveResponse, build_endpoint_url, parse_direction, read_body_capped,
};
use crate::snake_client::{
    ProxyCall, ProxyClients, ProxyResponseClass, SnakeRequestRoute, build_routed_request,
    execute_proxy, log_proxy_auth_failure, log_proxy_fault,
};
use crate::wire;
use reqwest::{Method, StatusCode};

/// Generous per-call budget so a slow-but-working snake still shows its
/// answer.
///
/// Real games only allow `engine_game.meta.timeout` (500ms) per `/move`; a
/// move that answers within this budget but over the game's is still a
/// [`HealthCallStatus::SnakeFailure`], because real games would time it out.
pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of characters of raw response body shown in results.
const BODY_EXCERPT_MAX_CHARS: usize = 500;

/// Result of a single test call against a snake endpoint.
pub struct HealthCheckCall {
    /// Human-readable call name, e.g. `"GET /"` or `"POST /move (turn 0)"`.
    pub name: &'static str,
    pub status: HealthCallStatus,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthCallStatus {
    Healthy,
    /// Answered, but not the way the API spec asks, in a way real games
    /// tolerate (a wrong `apiversion`, a non-2xx from `/start` or `/end`).
    /// Shown to the owner; never counts against the snake's matchmaking.
    Warning,
    /// Would break a real game: no answer at all, or a `/move` that errored,
    /// was unusable, or came back over the game's time budget.
    SnakeFailure,
    /// The engine proxy failed; the snake's health is unknown.
    ProxyFault,
}

/// The results of one test game.
pub struct GameCheck {
    pub spec: TestGameSpec,
    pub calls: Vec<HealthCheckCall>,
}

/// Full report of an on-demand "Test Snake" run.
pub struct HealthCheckReport {
    /// `GET /` — game-independent, called once.
    pub identity: HealthCheckCall,
    /// One test game per leaderboard.
    pub games: Vec<GameCheck>,
    /// The per-request timeout real games enforce, for display next to the
    /// measured latencies.
    pub game_timeout_ms: i64,
}

impl HealthCheckReport {
    pub fn calls(&self) -> impl Iterator<Item = &HealthCheckCall> {
        std::iter::once(&self.identity).chain(self.games.iter().flat_map(|g| g.calls.iter()))
    }

    fn count(&self, status: HealthCallStatus) -> usize {
        self.calls().filter(|c| c.status == status).count()
    }

    pub fn failure_count(&self) -> usize {
        self.count(HealthCallStatus::SnakeFailure)
    }

    pub fn warning_count(&self) -> usize {
        self.count(HealthCallStatus::Warning)
    }

    pub fn proxy_fault_count(&self) -> usize {
        self.count(HealthCallStatus::ProxyFault)
    }
}

/// The shape of a test game: everything about a leaderboard's games that a
/// snake might depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestGameSpec {
    /// What the owner sees this game called, e.g. the leaderboard name.
    pub label: String,
    pub game_type: GameType,
    pub board_size: GameBoardSize,
    /// Snakes on the board, including the one under test.
    pub snake_count: usize,
    pub source: GameSource,
}

impl TestGameSpec {
    /// A game shaped like the leaderboard's matches: its mode, board size and
    /// full match size (the matchmaker only goes smaller when the ladder is
    /// short of snakes).
    pub fn for_leaderboard(
        name: &str,
        game_type: &str,
        board_size: &str,
        match_size: i32,
    ) -> cja::Result<Self> {
        use color_eyre::eyre::Context as _;
        Ok(Self {
            label: name.to_string(),
            game_type: game_type
                .parse()
                .wrap_err_with(|| format!("Invalid game type for leaderboard {name}"))?,
            board_size: board_size
                .parse()
                .wrap_err_with(|| format!("Invalid board size for leaderboard {name}"))?,
            snake_count: usize::try_from(match_size).unwrap_or(2).clamp(2, 4),
            source: GameSource::Ladder,
        })
    }

    /// A Standard 11x11 duel, for when there's no leaderboard to imitate.
    pub fn fallback() -> Self {
        Self {
            label: "Standard 11x11".to_string(),
            game_type: GameType::Standard,
            board_size: GameBoardSize::Medium,
            snake_count: 2,
            source: GameSource::Custom,
        }
    }

    /// "4 snakes · Royale · 11x11"
    pub fn describe(&self) -> String {
        format!(
            "{} snakes · {} · {}",
            self.snake_count,
            self.game_type.as_str(),
            self.board_size.as_str()
        )
    }
}

/// What we expect back from a given endpoint.
enum Expectation {
    /// `GET /`: JSON with `apiversion == "1"` (plus author/version metadata).
    Info,
    /// `POST /move`: JSON with a valid `move` field, within the game's budget.
    Move { budget_ms: i64 },
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
    ProxyFault {
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

/// A test game ready to play: the engine state plus who's who.
pub struct TestGame {
    pub engine_game: EngineGame,
    /// The `you.id` the snake under test sees.
    pub snake_id: String,
    /// Each opponent with its turn-0 move; on turn 1 it reverses into its
    /// own neck and is eliminated.
    opponents: Vec<(String, Direction)>,
    customizations: HashMap<String, SnakeCustomizations>,
}

/// Build a test game shaped like `spec`, with the snake under test first and
/// `spec.snake_count - 1` synthetic opponents.
///
/// This goes through `engine::create_initial_game` — the exact function real
/// games use — so the payloads are structurally identical to what a real game
/// of that mode sends on turn 0. Game and snake ids are fresh UUIDs that never
/// touch the database.
pub fn build_test_game(snake: &Battlesnake, spec: &TestGameSpec) -> TestGame {
    let now = chrono::Utc::now();
    let snake_id = Uuid::new_v4();
    let opponent_ids: Vec<Uuid> = (1..spec.snake_count).map(|_| Uuid::new_v4()).collect();

    // Fabricate the row shape a real game would load from the DB so we can
    // reuse the engine's board initialization verbatim.
    let details = |game_battlesnake_id: Uuid, name: String| GameBattlesnakeWithDetails {
        game_battlesnake_id,
        game_id: Uuid::nil(),
        battlesnake_id: snake.battlesnake_id,
        placement: None,
        created_at: now,
        updated_at: now,
        name,
        url: snake.url.clone(),
        engine_region: EngineRegion::UsWest1,
        user_id: snake.user_id,
        leaderboard_entry_id: None,
        color: snake.color.clone(),
        // Synthetic health-check game; owner identity is never shown.
        owner_login: String::new(),
        owner_name: String::new(),
        head: snake.head.clone(),
        tail: snake.tail.clone(),
    };
    let mut snakes = vec![details(snake_id, snake.name.clone())];
    snakes.extend(
        opponent_ids
            .iter()
            .enumerate()
            .map(|(i, id)| details(*id, format!("Opponent {}", i + 1))),
    );

    let mut engine_game = crate::engine::create_initial_game(
        Uuid::new_v4(),
        spec.board_size.clone(),
        spec.game_type.clone(),
        &snakes,
    );
    engine_game.meta.source = spec.source;

    let snake_id = snake_id.to_string();
    let mut customizations = HashMap::new();
    customizations.insert(
        snake_id.clone(),
        SnakeCustomizations {
            color: snake.color.clone(),
            head: snake.head.clone(),
            tail: snake.tail.clone(),
            author: String::new(),
        },
    );
    let opponents = opponent_ids
        .iter()
        .map(|id| {
            let id = id.to_string();
            let customization = SnakeCustomizations {
                color: "#888888".to_string(),
                head: "default".to_string(),
                tail: "default".to_string(),
                author: String::new(),
            };
            customizations.insert(id.clone(), customization);
            let first_move = opening_move(&engine_game, &snake_id, &id);
            (id, first_move)
        })
        .collect();

    TestGame {
        engine_game,
        snake_id,
        opponents,
        customizations,
    }
}

/// An opponent's turn-0 move: the in-bounds step that ends farthest from the
/// snake under test, so it can never collide with it. (Spawn points are at
/// least four squares apart, so no turn-0 step can reach another snake.)
fn opening_move(game: &EngineGame, snake_id: &str, opponent_id: &str) -> Direction {
    let head = |id: &str| {
        game.board
            .snakes
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.body.first().copied())
    };
    let (Some(ours), Some(theirs)) = (head(snake_id), head(opponent_id)) else {
        return Direction::Up;
    };
    [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
    ]
    .into_iter()
    .filter_map(|direction| {
        let (dx, dy) = direction.to_delta();
        let (x, y) = (theirs.x + dx, theirs.y + dy);
        let in_bounds = (0..game.board.width).contains(&x) && (0..game.board.height).contains(&y);
        in_bounds.then_some((direction, (x - ours.x).abs() + (y - ours.y).abs()))
    })
    .max_by_key(|(_, distance)| *distance)
    .map_or(Direction::Up, |(direction, _)| direction)
}

fn reverse(direction: Direction) -> Direction {
    match direction {
        Direction::Up => Direction::Down,
        Direction::Down => Direction::Up,
        Direction::Left => Direction::Right,
        Direction::Right => Direction::Left,
    }
}

/// How a check handles a call that would break a real game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMode {
    /// Play the whole game regardless, falling back like a real game does:
    /// the Test Snake button, where the user wants the complete picture.
    RunAll,
    /// Stop at the first failure (or proxy fault): the background sweeper,
    /// where "unhealthy" is already decided and each further call against a
    /// dead host costs another full timeout.
    AbortOnFailure,
}

/// Call `GET /` once. Real games tolerate any answer (they fall back to the
/// snake's stored customizations), so only no answer at all is a failure.
pub async fn check_identity(clients: &ProxyClients<'_>, snake: &Battlesnake) -> HealthCheckCall {
    let outcome = execute_call_routed(clients, snake, Method::GET, &snake.url, None).await;
    evaluate_call("GET /", &Expectation::Info, outcome)
}

/// Run the on-demand test: `GET /` once, then one test game per spec,
/// concurrently (real snakes serve concurrent games all the time).
pub async fn run_health_check(
    clients: &ProxyClients<'_>,
    snake: &Battlesnake,
    specs: Vec<TestGameSpec>,
    failure_mode: FailureMode,
) -> cja::Result<HealthCheckReport> {
    let games = join_all(
        specs
            .iter()
            .map(|spec| play_test_game(clients, snake, spec, failure_mode)),
    );
    let (identity, games) = futures::join!(check_identity(clients, snake), games);
    let games = specs
        .into_iter()
        .zip(games)
        .map(|(spec, calls)| {
            Ok(GameCheck {
                spec,
                calls: calls?,
            })
        })
        .collect::<cja::Result<_>>()?;
    Ok(HealthCheckReport {
        identity,
        games,
        game_timeout_ms: crate::engine::MOVE_TIMEOUT_MS,
    })
}

const MOVE_CALLS: [&str; 2] = ["POST /move (turn 0)", "POST /move (turn 1)"];

/// Play one test game against the snake: `/start`, two `/move`s, `/end`.
///
/// Errors only on an engine failure (an arena bug, never the snake's fault).
pub async fn play_test_game(
    clients: &ProxyClients<'_>,
    snake: &Battlesnake,
    spec: &TestGameSpec,
    failure_mode: FailureMode,
) -> cja::Result<Vec<HealthCheckCall>> {
    let TestGame {
        mut engine_game,
        snake_id,
        opponents,
        customizations,
    } = build_test_game(snake, spec);
    let budget_ms = engine_game.meta.timeout;
    let mut calls = Vec::with_capacity(4);
    let should_stop = |call: &HealthCheckCall| {
        failure_mode == FailureMode::AbortOnFailure
            && matches!(
                call.status,
                HealthCallStatus::SnakeFailure | HealthCallStatus::ProxyFault
            )
    };
    // No previous-turn context on turn 0, exactly like a real game.
    let mut contexts: HashMap<String, wire::SnakeContext> = HashMap::new();
    let payload = |game: &EngineGame, contexts: &HashMap<String, wire::SnakeContext>| {
        wire::Game::from_engine_game(game, &snake_id, contexts, &customizations)
    };

    let start = build_endpoint_url(&snake.url, "start");
    let outcome = execute_call_routed(
        clients,
        snake,
        Method::POST,
        &start,
        Some(&payload(&engine_game, &contexts)),
    )
    .await;
    calls.push(evaluate_call("POST /start", &Expectation::Ack, outcome));
    if calls.last().is_some_and(should_stop) {
        return Ok(calls);
    }

    let move_url = build_endpoint_url(&snake.url, "move");
    let mut last_direction = None;
    for (turn, name) in MOVE_CALLS.into_iter().enumerate() {
        if crate::engine::is_game_over(&engine_game) {
            break;
        }
        let outcome = execute_call_routed(
            clients,
            snake,
            Method::POST,
            &move_url,
            Some(&payload(&engine_game, &contexts)),
        )
        .await;
        // Real games use any parseable move, whatever the status, and fall
        // back to the previous direction (or up) otherwise.
        let answered = parsed_move(&outcome);
        let call = evaluate_call(name, &Expectation::Move { budget_ms }, outcome);
        let stop = should_stop(&call);
        let latency = call.latency_ms.and_then(|ms| i64::try_from(ms).ok());
        calls.push(call);
        if stop {
            return Ok(calls);
        }

        let direction = answered
            .as_ref()
            .map(|(direction, _)| *direction)
            .unwrap_or_else(|| last_direction.unwrap_or(Direction::Up));
        last_direction = Some(direction);
        advance(&mut engine_game, &snake_id, direction, &opponents, turn)?;

        contexts.clear();
        contexts.insert(
            snake_id.clone(),
            wire::SnakeContext {
                latency_ms: wire::reported_latency_ms(latency, latency.is_none(), budget_ms),
                shout: answered.and_then(|(_, shout)| shout),
            },
        );
    }

    // Call /end with the final board, like a real game, so a snake that
    // allocated per-game state on /start can release it.
    let end = build_endpoint_url(&snake.url, "end");
    let outcome = execute_call_routed(
        clients,
        snake,
        Method::POST,
        &end,
        Some(&payload(&engine_game, &contexts)),
    )
    .await;
    calls.push(evaluate_call("POST /end", &Expectation::Ack, outcome));

    Ok(calls)
}

/// Apply one turn of a test game the way the game runner does: the snake's
/// move, each opponent's scripted move (its opening step on turn 0, then
/// straight back into its own neck), then the turn counter and food.
fn advance(
    game: &mut EngineGame,
    snake_id: &str,
    direction: Direction,
    opponents: &[(String, Direction)],
    turn: usize,
) -> cja::Result<()> {
    let mut moves = vec![(snake_id.to_string(), direction)];
    moves.extend(opponents.iter().map(|(id, opening)| {
        let scripted = if turn == 0 {
            *opening
        } else {
            reverse(*opening)
        };
        (id.clone(), scripted)
    }));
    crate::engine::apply_turn(game, &moves)?;
    game.board.turn += 1;
    crate::engine::spawn_food(game);
    Ok(())
}

/// The move a real game would take from this answer, if any.
fn parsed_move(outcome: &CallOutcome) -> Option<(Direction, Option<String>)> {
    let CallOutcome::Response { body, .. } = outcome else {
        return None;
    };
    let response = serde_json::from_str::<MoveResponse>(body).ok()?;
    Some((parse_direction(&response.direction)?, response.shout))
}

async fn execute_call_routed(
    clients: &ProxyClients<'_>,
    snake: &Battlesnake,
    method: Method,
    target: &str,
    payload: Option<&wire::Game>,
) -> CallOutcome {
    let region = snake.engine_region;
    let route = build_routed_request(
        clients,
        region,
        method.clone(),
        target,
        HEALTH_CHECK_TIMEOUT,
    );
    let with_payload = |builder: reqwest::RequestBuilder| match payload {
        Some(payload) => builder.json(payload),
        None => builder,
    };
    let direct = || with_payload(clients.direct.request(method.clone(), target));
    let SnakeRequestRoute::Proxied(builder) = route else {
        return execute_call(direct(), HEALTH_CHECK_TIMEOUT).await;
    };
    match execute_proxy(with_payload(builder), HEALTH_CHECK_TIMEOUT).await {
        ProxyCall::Response {
            class: ProxyResponseClass::SnakeResponse { latency_ms },
            status,
            body,
        } => CallOutcome::Response {
            status: status.as_u16(),
            latency_ms: latency_ms as u64,
            body,
        },
        ProxyCall::Response {
            class:
                ProxyResponseClass::SnakeTransportFailure {
                    timed_out,
                    latency_ms,
                },
            status,
            ..
        } => CallOutcome::Failed {
            latency_ms: Some(latency_ms as u64),
            summary: if timed_out {
                "Snake timed out at engine proxy".to_string()
            } else {
                format!("Snake server error at engine proxy (HTTP {status})")
            },
        },
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
            log_proxy_auth_failure(region);
            execute_call(direct(), HEALTH_CHECK_TIMEOUT).await
        }
        ProxyCall::Response {
            class: ProxyResponseClass::ProxyFault,
            status,
            ..
        } => {
            log_proxy_fault(
                &snake.battlesnake_id.to_string(),
                region,
                "health",
                "response",
                Some(status),
            );
            CallOutcome::ProxyFault {
                summary: format!("Engine proxy failed (HTTP {status})"),
            }
        }
        ProxyCall::Fault { kind } => {
            log_proxy_fault(
                &snake.battlesnake_id.to_string(),
                region,
                "health",
                kind,
                None,
            );
            CallOutcome::ProxyFault {
                summary: "Engine proxy did not respond".to_string(),
            }
        }
    }
}

/// Execute one HTTP call with a timeout, mirroring how `snake_client` wraps
/// its requests in `tokio::time::timeout`.
async fn execute_call(builder: reqwest::RequestBuilder, timeout: Duration) -> CallOutcome {
    let start = Instant::now();

    match tokio::time::timeout(timeout, builder.send()).await {
        Ok(Ok(response)) => {
            // Latency is measured at response-headers time, before the body
            // download — the same point real games record it (snake_client),
            // so the over-budget badge reflects what a game would measure.
            let latency_ms = start.elapsed().as_millis() as u64;
            let status = response.status().as_u16();
            match read_body_capped(response, BODY_READ_CAP_BYTES).await {
                Ok(body) => CallOutcome::Response {
                    status,
                    latency_ms,
                    body,
                },
                Err(e) => CallOutcome::Failed {
                    latency_ms: Some(latency_ms),
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
        CallOutcome::ProxyFault { summary } => HealthCheckCall {
            name,
            status: HealthCallStatus::ProxyFault,
            http_status: None,
            latency_ms: None,
            summary,
            body_excerpt: None,
        },
        CallOutcome::Failed {
            latency_ms,
            summary,
        } => HealthCheckCall {
            name,
            status: HealthCallStatus::SnakeFailure,
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
            // Only a /move answer decides a real game; games shrug off
            // everything else the snake sends back.
            let problem = match expectation {
                Expectation::Move { .. } => HealthCallStatus::SnakeFailure,
                Expectation::Info | Expectation::Ack => HealthCallStatus::Warning,
            };

            if !(200..300).contains(&status) {
                return HealthCheckCall {
                    name,
                    status: problem,
                    http_status: Some(status),
                    latency_ms: Some(latency_ms),
                    summary: format!("Returned non-success HTTP status {status}"),
                    body_excerpt: Some(truncate_excerpt(&body)),
                };
            }

            let (ok, mut summary) = match expectation {
                Expectation::Info => evaluate_info_body(&body),
                Expectation::Move { .. } => evaluate_move_body(&body),
                Expectation::Ack => (
                    true,
                    "Acknowledged. Real games ignore this response body.".to_string(),
                ),
            };

            let over_budget = match expectation {
                Expectation::Move { budget_ms } => i64::try_from(latency_ms)
                    .map_or(true, |latency| latency > *budget_ms)
                    .then_some(*budget_ms),
                _ => None,
            };
            if let Some(budget_ms) = over_budget {
                summary = format!(
                    "Answered in {latency_ms} ms, over the {budget_ms} ms game budget — \
                     real games would time this move out. ({summary})"
                );
            }

            let healthy = ok && over_budget.is_none();
            HealthCheckCall {
                name,
                status: if healthy {
                    HealthCallStatus::Healthy
                } else {
                    problem
                },
                http_status: Some(status),
                latency_ms: Some(latency_ms),
                summary,
                body_excerpt: (!ok).then(|| truncate_excerpt(&body)),
            }
        }
    }
}

/// Validate the `GET /` identity response body.
///
/// The Battlesnake API spec expects `apiversion` to be `"1"`. The arena's
/// game runner never checks it, so a wrong value is reported as a spec
/// compliance failure while making clear games will still run.
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
                format!(
                    "apiversion is {other:?} — the Battlesnake API spec expects \"1\" \
                     (games on this arena will still run)"
                ),
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
    use reqwest::Client;
    use tokio::io::AsyncWriteExt as _;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn test_snake() -> Battlesnake {
        let now = chrono::Utc::now();
        Battlesnake {
            battlesnake_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "Test Snake".to_string(),
            url: "http://localhost:8000".to_string(),
            visibility: Visibility::Private,
            engine_region: crate::models::battlesnake::EngineRegion::UsWest1,
            color: "#ff0000".to_string(),
            head: "default".to_string(),
            tail: "default".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    fn spec(game_type: &str, board_size: &str, match_size: i32) -> TestGameSpec {
        TestGameSpec::for_leaderboard("Test Board", game_type, board_size, match_size).unwrap()
    }

    fn all_specs() -> Vec<TestGameSpec> {
        let mut specs = vec![];
        for game_type in ["Standard", "Royale", "Constrictor", "Snail Mode"] {
            for board_size in ["7x7", "11x11", "19x19"] {
                for match_size in 2..=4 {
                    specs.push(spec(game_type, board_size, match_size));
                }
            }
        }
        specs
    }

    fn proxy_clients<'a>(
        client: &'a Client,
        config: &'a crate::config::EngineProxyConfig,
    ) -> ProxyClients<'a> {
        ProxyClients {
            direct: client,
            east: client,
            europe: client,
            config,
        }
    }

    fn proxy_config(uri: String) -> crate::config::EngineProxyConfig {
        crate::config::EngineProxyConfig {
            token: Some("test-secret".to_string()),
            us_east4_url: uri.clone(),
            europe_west4_url: uri,
        }
    }

    fn no_proxy() -> crate::config::EngineProxyConfig {
        crate::config::EngineProxyConfig {
            token: None,
            us_east4_url: String::new(),
            europe_west4_url: String::new(),
        }
    }

    /// A sensible snake server: `GET /` identifies itself and `/move` heads
    /// toward the middle of the board, so it never walks into a wall.
    struct CenterSeeker;

    impl Respond for CenterSeeker {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            if request.method.as_str() == "GET" {
                return ResponseTemplate::new(200).set_body_string(r#"{"apiversion":"1"}"#);
            }
            if !request.url.path().ends_with("/move") {
                return ResponseTemplate::new(200);
            }
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let (x, y) = (
                body["you"]["head"]["x"].as_i64().unwrap(),
                body["you"]["head"]["y"].as_i64().unwrap(),
            );
            let (cx, cy) = (
                body["board"]["width"].as_i64().unwrap() / 2,
                body["board"]["height"].as_i64().unwrap() / 2,
            );
            let direction = if (cx - x).abs() >= (cy - y).abs() {
                if cx > x { "right" } else { "left" }
            } else if cy > y {
                "up"
            } else {
                "down"
            };
            ResponseTemplate::new(200).set_body_string(format!(r#"{{"move":"{direction}"}}"#))
        }
    }

    async fn request_bodies(server: &MockServer, endpoint: &str) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == endpoint)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    // === specs and the test game ===

    #[test]
    fn leaderboard_spec_copies_mode_board_and_match_size() {
        let royale = spec("Royale", "19x19", 4);
        assert_eq!(royale.game_type, GameType::Royale);
        assert_eq!(royale.board_size, GameBoardSize::Large);
        assert_eq!(royale.snake_count, 4);
        assert_eq!(royale.source, GameSource::Ladder);
        assert_eq!(royale.describe(), "4 snakes · Royale · 19x19");
        assert_eq!(spec("Snail Mode", "7x7", 2).game_type, GameType::SnailMode);

        let fallback = TestGameSpec::fallback();
        assert_eq!(fallback.snake_count, 2, "never a solo game");
        assert_eq!(fallback.game_type, GameType::Standard);
        assert_eq!(fallback.source, GameSource::Custom);
    }

    #[test]
    fn test_game_matches_its_leaderboard() {
        let snake = test_snake();
        for spec in all_specs() {
            let game = build_test_game(&snake, &spec);
            let (width, height) = spec.board_size.dimensions();
            assert_eq!(game.engine_game.board.width, width as i32);
            assert_eq!(game.engine_game.board.height, height as i32);
            assert_eq!(game.engine_game.board.snakes.len(), spec.snake_count);
            assert_eq!(game.engine_game.board.snakes[0].id, game.snake_id);
            assert_eq!(game.opponents.len(), spec.snake_count - 1);
            assert_eq!(game.engine_game.board.turn, 0);
            assert_eq!(game.engine_game.meta.timeout, 500);
            assert_eq!(game.engine_game.meta.source, GameSource::Ladder);

            let real = crate::engine::create_initial_game(
                Uuid::new_v4(),
                spec.board_size.clone(),
                spec.game_type.clone(),
                &[],
            );
            assert_eq!(game.engine_game.meta.ruleset_name, real.meta.ruleset_name);
            let (ours, theirs) = (&game.engine_game.meta.settings, &real.meta.settings);
            assert_eq!(ours.food_spawn_chance, theirs.food_spawn_chance);
            assert_eq!(ours.minimum_food, theirs.minimum_food);
            assert_eq!(ours.hazard_damage_per_turn, theirs.hazard_damage_per_turn);
            assert_eq!(
                game.engine_game.meta.royale.is_some(),
                real.meta.royale.is_some()
            );
        }
    }

    #[test]
    fn test_game_wire_payload_shows_every_snake() {
        let snake = test_snake();
        let game = build_test_game(&snake, &spec("Royale", "11x11", 4));
        let payload = wire::Game::from_engine_game(
            &game.engine_game,
            &game.snake_id,
            &HashMap::new(),
            &game.customizations,
        );
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["you"]["id"], game.snake_id);
        assert_eq!(json["you"]["name"], "Test Snake");
        assert_eq!(json["you"]["customizations"]["color"], "#ff0000");
        assert_eq!(json["board"]["snakes"].as_array().unwrap().len(), 4);
        // Royale is a map on the standard ruleset, as on play.battlesnake.com.
        assert_eq!(json["game"]["ruleset"]["name"], "standard");
        assert_eq!(json["game"]["map"], "royale");
        assert_eq!(
            json["game"]["ruleset"]["settings"]["hazardDamagePerTurn"],
            14
        );
        assert_eq!(
            json["game"]["ruleset"]["settings"]["royale"]["shrinkEveryNTurns"],
            25
        );
        assert_eq!(json["game"]["source"], "arena");
        assert_eq!(json["game"]["timeout"], 500);
    }

    /// Whatever the snake does with its two moves, the opponents never touch
    /// it and are all gone after turn 1 — so a snake that plays sensibly wins
    /// and the game is over, on every board, mode and match size.
    #[test]
    fn opponents_die_on_turn_one_and_the_snake_wins() {
        let snake = test_snake();
        let directions = [
            Direction::Up,
            Direction::Down,
            Direction::Left,
            Direction::Right,
        ];
        for spec in all_specs() {
            for first in directions {
                for second in directions {
                    for _ in 0..10 {
                        let TestGame {
                            mut engine_game,
                            snake_id,
                            opponents,
                            ..
                        } = build_test_game(&snake, &spec);
                        advance(&mut engine_game, &snake_id, first, &opponents, 0).unwrap();
                        assert!(
                            engine_game
                                .board
                                .snakes
                                .iter()
                                .all(|s| !s.eliminated_cause.is_eliminated()),
                            "nobody dies on turn 0: {spec:?} {first:?}"
                        );
                        advance(&mut engine_game, &snake_id, second, &opponents, 1).unwrap();
                        for opponent in engine_game.board.snakes.iter().skip(1) {
                            assert!(
                                opponent.eliminated_cause.is_eliminated(),
                                "{spec:?}: opponent survived"
                            );
                        }
                        assert!(crate::engine::is_game_over(&engine_game));
                        let reversed = second == reverse(first);
                        let ours = &engine_game.board.snakes[0];
                        if !reversed {
                            // Short of running off the board or into its own
                            // neck, nothing can kill the snake.
                            let head = ours.body[0];
                            if (0..engine_game.board.width).contains(&head.x)
                                && (0..engine_game.board.height).contains(&head.y)
                            {
                                assert!(
                                    !ours.eliminated_cause.is_eliminated(),
                                    "{spec:?} {first:?} {second:?}: {:?}",
                                    ours.eliminated_cause
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // === playing a test game ===

    #[tokio::test]
    async fn plays_a_full_game_and_ends_it_like_a_real_one() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(CenterSeeker)
            .mount(&server)
            .await;
        let mut snake = test_snake();
        snake.url = server.uri();
        let client = Client::new();
        let config = no_proxy();

        let calls = play_test_game(
            &proxy_clients(&client, &config),
            &snake,
            &spec("Standard", "11x11", 4),
            FailureMode::AbortOnFailure,
        )
        .await
        .unwrap();

        let names: Vec<_> = calls.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "POST /start",
                "POST /move (turn 0)",
                "POST /move (turn 1)",
                "POST /end"
            ]
        );
        assert!(calls.iter().all(|c| c.status == HealthCallStatus::Healthy));

        let start = request_bodies(&server, "/start").await;
        let moves = request_bodies(&server, "/move").await;
        let end = request_bodies(&server, "/end").await;
        assert_eq!(start[0]["turn"], 0);
        assert_eq!(start[0]["board"]["snakes"].as_array().unwrap().len(), 4);
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0]["turn"], 0);
        assert_eq!(moves[1]["turn"], 1);
        assert_eq!(moves[1]["board"]["snakes"].as_array().unwrap().len(), 4);
        // Turn 1 reports the snake's measured turn-0 latency, like a real game.
        assert_ne!(moves[1]["you"]["latency"], "0");
        // /end carries the final board: only the winner is left.
        assert_eq!(end[0]["turn"], 2);
        let survivors = end[0]["board"]["snakes"].as_array().unwrap();
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0]["id"], end[0]["you"]["id"]);
        assert_eq!(start[0]["game"]["id"], end[0]["game"]["id"]);
    }

    #[tokio::test]
    async fn abort_stops_at_a_failed_move_but_run_all_finishes_the_game() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let mut snake = test_snake();
        snake.url = server.uri();
        let client = Client::new();
        let config = no_proxy();
        let clients = proxy_clients(&client, &config);
        let duel = TestGameSpec::fallback();

        let aborted = play_test_game(&clients, &snake, &duel, FailureMode::AbortOnFailure)
            .await
            .unwrap();
        assert_eq!(aborted.len(), 2);
        assert_eq!(aborted[1].status, HealthCallStatus::SnakeFailure);

        let full = play_test_game(&clients, &snake, &duel, FailureMode::RunAll)
            .await
            .unwrap();
        let statuses: Vec<_> = full.iter().map(|c| c.status).collect();
        assert_eq!(
            statuses,
            [
                HealthCallStatus::Healthy,
                HealthCallStatus::SnakeFailure,
                HealthCallStatus::SnakeFailure,
                HealthCallStatus::Healthy,
            ]
        );
    }

    #[tokio::test]
    async fn spec_problems_games_tolerate_are_warnings_not_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"apiversion":"2"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::any())
            .respond_with(CenterSeeker)
            .mount(&server)
            .await;
        let mut snake = test_snake();
        snake.url = server.uri();
        let client = Client::new();
        let config = no_proxy();

        let report = run_health_check(
            &proxy_clients(&client, &config),
            &snake,
            vec![TestGameSpec::fallback()],
            FailureMode::AbortOnFailure,
        )
        .await
        .unwrap();

        assert_eq!(report.identity.status, HealthCallStatus::Warning);
        assert_eq!(report.games[0].calls[0].status, HealthCallStatus::Warning);
        // A warning never aborts: the game still ran to /end.
        assert_eq!(report.games[0].calls.len(), 4);
        assert_eq!(report.warning_count(), 2);
        assert_eq!(report.failure_count(), 0);
    }

    #[tokio::test]
    async fn plays_one_game_per_spec_with_one_identity_call() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(CenterSeeker)
            .mount(&server)
            .await;
        let mut snake = test_snake();
        snake.url = server.uri();
        let client = Client::new();
        let config = no_proxy();
        let specs = vec![
            spec("Standard", "11x11", 4),
            spec("Royale", "11x11", 4),
            spec("Standard", "11x11", 2),
        ];

        let report = run_health_check(
            &proxy_clients(&client, &config),
            &snake,
            specs.clone(),
            FailureMode::RunAll,
        )
        .await
        .unwrap();

        assert_eq!(report.games.len(), 3);
        for (game, spec) in report.games.iter().zip(&specs) {
            assert_eq!(&game.spec, spec);
            assert_eq!(game.calls.len(), 4);
        }
        assert_eq!(report.calls().count(), 13);
        assert_eq!(report.failure_count(), 0);
        let gets = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "GET")
            .count();
        assert_eq!(gets, 1);
        let rulesets: std::collections::BTreeSet<String> = request_bodies(&server, "/start")
            .await
            .iter()
            .map(|b| {
                format!(
                    "{}/{}",
                    b["game"]["map"],
                    b["board"]["snakes"].as_array().unwrap().len()
                )
            })
            .collect();
        assert_eq!(
            rulesets,
            ["\"royale\"/4", "\"standard\"/2", "\"standard\"/4"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[tokio::test]
    async fn routed_checks_use_proxy_latency_for_every_call() {
        let proxy = MockServer::start().await;
        let snake = test_snake();
        for (verb, target, body, times) in [
            ("GET", snake.url.clone(), r#"{"apiversion":"1"}"#, 1),
            ("POST", build_endpoint_url(&snake.url, "start"), "{}", 1),
            (
                "POST",
                build_endpoint_url(&snake.url, "move"),
                r#"{"move":"up"}"#,
                2,
            ),
            ("POST", build_endpoint_url(&snake.url, "end"), "{}", 1),
        ] {
            Mock::given(method(verb))
                .and(header("X-Request-URI", target.as_str()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("X-Battlesnake-Latency-Ms", "28")
                        .set_body_string(body),
                )
                .expect(times)
                .mount(&proxy)
                .await;
        }
        let config = proxy_config(proxy.uri());
        let client = Client::new();
        let mut snake = snake;
        snake.engine_region = EngineRegion::EuropeWest4;

        let report = run_health_check(
            &proxy_clients(&client, &config),
            &snake,
            vec![TestGameSpec::fallback()],
            FailureMode::RunAll,
        )
        .await
        .unwrap();

        assert_eq!(report.calls().count(), 5);
        assert!(
            report
                .calls()
                .all(|call| call.status == HealthCallStatus::Healthy && call.latency_ms == Some(28))
        );
    }

    #[tokio::test]
    async fn proxy_fault_is_separate_from_snake_failure() {
        let proxy = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(502))
            .mount(&proxy)
            .await;
        let config = proxy_config(proxy.uri());
        let client = Client::new();
        let clients = proxy_clients(&client, &config);
        let mut snake = test_snake();
        snake.engine_region = EngineRegion::EuropeWest4;

        let all = run_health_check(
            &clients,
            &snake,
            vec![TestGameSpec::fallback()],
            FailureMode::RunAll,
        )
        .await
        .unwrap();
        assert_eq!(all.calls().count(), 5);
        assert_eq!(all.proxy_fault_count(), 5);
        assert_eq!(all.failure_count(), 0);

        let aborted = play_test_game(
            &clients,
            &snake,
            &TestGameSpec::fallback(),
            FailureMode::AbortOnFailure,
        )
        .await
        .unwrap();
        assert_eq!(aborted.len(), 1);
        assert_eq!(aborted[0].status, HealthCallStatus::ProxyFault);
    }

    #[tokio::test]
    async fn measured_proxy_timeout_counts_as_snake_failure() {
        let proxy = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(504)
                    .insert_header("X-Battlesnake-Latency-Ms", "500")
                    .insert_header("X-Battlesnake-Server-Error", "true"),
            )
            .mount(&proxy)
            .await;
        let config = proxy_config(proxy.uri());
        let client = Client::new();
        let mut snake = test_snake();
        snake.engine_region = EngineRegion::EuropeWest4;

        let calls = play_test_game(
            &proxy_clients(&client, &config),
            &snake,
            &TestGameSpec::fallback(),
            FailureMode::AbortOnFailure,
        )
        .await
        .unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].status, HealthCallStatus::SnakeFailure);
        assert_eq!(calls[0].latency_ms, Some(500));
    }

    #[tokio::test]
    async fn stalled_health_body_hits_guard_before_client_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Battlesnake-Latency-Ms: 28\r\n\r\n")
                .await.unwrap();
            tokio::time::sleep(Duration::from_secs(7)).await;
        });
        let config = proxy_config(format!("http://{address}"));
        let direct = Client::new();
        let health = Client::builder()
            .timeout(Duration::from_secs(6))
            .build()
            .unwrap();
        let clients = ProxyClients {
            direct: &direct,
            east: &health,
            europe: &health,
            config: &config,
        };
        let mut snake = test_snake();
        snake.engine_region = EngineRegion::UsEast4;
        let started = Instant::now();
        let identity = check_identity(&clients, &snake).await;
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(identity.status, HealthCallStatus::ProxyFault);
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
        assert!(summary.contains("apiversion is \"2\""));
        assert!(summary.contains("the Battlesnake API spec expects \"1\""));
        assert!(summary.contains("games on this arena will still run"));
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
    fn non_2xx_move_fails_with_body_excerpt() {
        let call = evaluate_call(
            "POST /move (turn 0)",
            &Expectation::Move { budget_ms: 500 },
            CallOutcome::Response {
                status: 404,
                latency_ms: 12,
                body: "Not Found".to_string(),
            },
        );
        assert_eq!(call.status, HealthCallStatus::SnakeFailure);
        assert_eq!(call.http_status, Some(404));
        assert_eq!(call.latency_ms, Some(12));
        assert!(call.summary.contains("404"));
        assert_eq!(call.body_excerpt.as_deref(), Some("Not Found"));
    }

    #[test]
    fn non_2xx_ack_is_only_a_warning() {
        let call = evaluate_call(
            "POST /start",
            &Expectation::Ack,
            CallOutcome::Response {
                status: 500,
                latency_ms: 12,
                body: "oops".to_string(),
            },
        );
        assert_eq!(call.status, HealthCallStatus::Warning);
        assert_eq!(call.body_excerpt.as_deref(), Some("oops"));
    }

    #[test]
    fn wrong_apiversion_is_only_a_warning() {
        let call = evaluate_call(
            "GET /",
            &Expectation::Info,
            CallOutcome::Response {
                status: 200,
                latency_ms: 12,
                body: r#"{"apiversion":"2"}"#.to_string(),
            },
        );
        assert_eq!(call.status, HealthCallStatus::Warning);
    }

    #[test]
    fn slow_move_fails_even_with_a_valid_answer() {
        let slow = evaluate_call(
            "POST /move (turn 0)",
            &Expectation::Move { budget_ms: 500 },
            CallOutcome::Response {
                status: 200,
                latency_ms: 501,
                body: r#"{"move":"up"}"#.to_string(),
            },
        );
        assert_eq!(slow.status, HealthCallStatus::SnakeFailure);
        assert!(slow.summary.contains("over the 500 ms game budget"));
        assert!(slow.summary.contains("Move: up"));
        assert!(slow.body_excerpt.is_none());

        let on_time = evaluate_call(
            "POST /move (turn 0)",
            &Expectation::Move { budget_ms: 500 },
            CallOutcome::Response {
                status: 200,
                latency_ms: 500,
                body: r#"{"move":"up"}"#.to_string(),
            },
        );
        assert_eq!(on_time.status, HealthCallStatus::Healthy);
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
        assert_eq!(call.status, HealthCallStatus::Healthy);
        assert_eq!(call.http_status, Some(200));
        assert!(call.body_excerpt.is_none());
    }

    #[test]
    fn no_answer_is_a_failure_for_every_call() {
        for expectation in [
            Expectation::Info,
            Expectation::Ack,
            Expectation::Move { budget_ms: 500 },
        ] {
            let call = evaluate_call(
                "GET /",
                &expectation,
                CallOutcome::Failed {
                    latency_ms: None,
                    summary: "Timed out: no response within 5 seconds".to_string(),
                },
            );
            assert_eq!(call.status, HealthCallStatus::SnakeFailure);
            assert_eq!(call.http_status, None);
            assert!(call.summary.contains("Timed out"));
        }
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
        assert_eq!(call.status, HealthCallStatus::Healthy);
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
}
