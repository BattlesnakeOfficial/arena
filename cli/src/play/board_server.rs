//! The `play --browser` board server (DEV-2146): serves the legacy-engine
//! board API (`GET /games/{id}` + the `/games/{id}/events` websocket) on
//! `127.0.0.1` so board.battlesnake.com can play a local game live.
//!
//! Ported from the Go CLI's `board/server.go` + `board/api.go`. Three
//! deliberate divergences from Go:
//!
//! - **Append-only event log with full replay per connection** instead of
//!   Go's 1,000-slot buffered channel: Go blocks the game once 1,001 frames
//!   queue with no browser draining; here every client — first, late, or
//!   reconnecting (the board uses ReconnectingWebSocket and dedupes by
//!   `Turn`) — gets the complete history from turn 0, then the live tail,
//!   so no game length can ever lose frames.
//! - **Bounded, progress-aware shutdown** instead of Go's unbounded
//!   `<-done` wait: `finish` exits within `connect_grace` of game end when
//!   no browser ever connected (Go hangs forever), and within
//!   `drain_stall` of a client making no send progress.
//! - **First client to finish wins**: Go had a single competing consumer;
//!   with multiple tabs open, the CLI exits once one tab has everything
//!   (other tabs may be cut off mid-replay — accepted and documented).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use color_eyre::eyre::{Context as _, Result};
use futures::{SinkExt as _, StreamExt as _};
use serde::Serialize;

use crate::play::PlayConfig;
use engine_core::wire::SnakeCustomizations;

/// `writeln!` returns a `#[must_use]` io::Result; a bare `writeln!(log, …)`
/// trips `-D warnings`. `run_game` has the same pattern as a local closure.
fn write_log(log: &mut dyn Write, line: &str) {
    let _ = writeln!(log, "{line}");
}

// ---------------------------------------------------------------------------
// Wire types: field-for-field Go's `board/api.go`. Values follow the
// plan's Architecture Rule 4 (Go's shape, arena's values).
// ---------------------------------------------------------------------------

/// `GET /games/{id}` body: Go wraps the game in a one-field struct.
#[derive(Debug, Clone, Serialize)]
pub struct GameInfoResponse {
    #[serde(rename = "Game")]
    pub game: BoardGame,
}

/// Go `board.Game`: field set and names are byte-compatible with
/// `board/api.go`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardGame {
    #[serde(rename = "ID")]
    pub id: String,
    /// Always `"running"` (Go parity), including in `game_end`'s Data —
    /// the board ignores `game_end`'s Data.
    pub status: String,
    pub width: i32,
    pub height: i32,
    /// Go sends only the one `{"name": …}` key.
    pub ruleset: std::collections::BTreeMap<String, String>,
    pub snake_timeout: i64,
    /// `"custom"` (documented value; `game.meta.source` is already Custom).
    pub source: String,
    pub ruleset_name: String,
    /// Always `[]` (Go parity) — never null (the board maps over it).
    pub rules_stages: Vec<String>,
    pub map: String,
}

/// One websocket message: `{"Type": "frame"|"game_end", "Data": …}`.
#[derive(Debug, Clone, Serialize)]
pub struct GameEvent<T> {
    #[serde(rename = "Type")]
    pub message_type: &'static str,
    #[serde(rename = "Data")]
    pub data: T,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameFrame {
    pub turn: i32,
    pub snakes: Vec<BoardSnake>,
    pub food: Vec<FramePoint>,
    pub hazards: Vec<FramePoint>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardSnake {
    #[serde(rename = "ID")]
    pub id: String,
    pub name: String,
    pub body: Vec<FramePoint>,
    pub health: i32,
    /// `None` serializes as `null`, like Go's `*Death`.
    pub death: Option<BoardDeath>,
    pub color: String,
    pub head_type: String,
    pub tail_type: String,
    /// STRING milliseconds; `0`/no-measurement rounds up to `"1"` (Go's
    /// round-up rule), a timeout reports the timeout value.
    pub latency: String,
    /// The last sanitized shout (divergence: Go always sent `""`).
    pub shout: String,
    pub squad: String,
    /// From the ping's `Author`, via `SnakeCustomizations`.
    pub author: String,
    pub status_code: i32,
    /// `""` | `"0:Error communicating with server"` | `"7:Bad HTTP status
    /// code N"` (Rule 4 precedence: no-answer first, then status).
    pub error: String,
    pub is_bot: bool,
    pub is_environment: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardDeath {
    pub cause: String,
    pub turn: i32,
    pub eliminated_by: String,
}

/// Go's `rules.Point` marshals `X`/`Y` CAPITALIZED; the board's
/// `engineCoordsToPoint` reads `.X`/`.Y`. Deliberately a different type
/// from the snake-API's lowercase `wire::Position` — getting this wrong
/// blanks every cell on the board.
#[derive(Debug, Clone, Serialize)]
pub struct FramePoint {
    #[serde(rename = "X")]
    pub x: i32,
    #[serde(rename = "Y")]
    pub y: i32,
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Go's cause strings (`rules/constants.go`; NotEliminated is "" and never
/// used — `Death` is `None` there).
pub fn cause_str(cause: &rules::EliminationCause) -> &'static str {
    use rules::EliminationCause as Cause;
    match cause {
        Cause::NotEliminated => "",
        Cause::OutOfHealth => "out-of-health",
        Cause::OutOfBounds => "wall-collision",
        Cause::SelfCollision => "snake-self-collision",
        Cause::Collision => "snake-collision",
        Cause::HeadToHeadCollision => "head-collision",
        Cause::Hazard => "hazard",
    }
}

/// Go's `boardGame` construction with arena values: the resolved wire
/// ruleset/map pair (what we tell snakes), not the raw `-g` value.
pub fn board_game(cfg: &PlayConfig, game_id: &str) -> BoardGame {
    BoardGame {
        id: game_id.to_string(),
        status: "running".to_string(),
        width: cfg.width,
        height: cfg.height,
        ruleset: std::collections::BTreeMap::from([(
            "name".to_string(),
            cfg.resolved.wire_ruleset.clone(),
        )]),
        snake_timeout: cfg.timeout_ms,
        source: "custom".to_string(),
        ruleset_name: cfg.resolved.wire_ruleset.clone(),
        rules_stages: Vec::new(),
        map: cfg.resolved.wire_map.clone(),
    }
}

/// Go's `buildFrameEvent`: every snake on the engine board (alive and
/// eliminated — the board keeps eliminated snakes), names from
/// `snake_names`, display fields from `customizations`, food from
/// `board.food`, hazards from `board.on_board_hazards()` (never the raw
/// list — it can carry off-board bookkeeping points).
pub fn build_frame(
    game: &engine_core::EngineGame,
    customizations: &HashMap<String, SnakeCustomizations>,
    runtime: &HashMap<String, SnakeRuntime>,
) -> GameFrame {
    let point = |p: &rules::Point| FramePoint { x: p.x, y: p.y };
    let snakes = game
        .board
        .snakes
        .iter()
        .map(|snake| {
            let cust = customizations.get(&snake.id);
            let rt = runtime.get(&snake.id);
            let latency = rt.and_then(|r| r.latency).unwrap_or(1).max(1).to_string();
            let status_code = rt.map(|r| r.status_code).unwrap_or(0);
            // Rule 4 precedence: no usable answer first, then Go's `!= 200`
            // status check (a 2xx-but-not-200 answer still gets the 7-string).
            let error = if rt.is_some_and(|r| r.request_failed) {
                "0:Error communicating with server".to_string()
            } else if status_code != 0 && status_code != 200 {
                format!("7:Bad HTTP status code {status_code}")
            } else {
                String::new()
            };
            let death = snake.eliminated_cause.is_eliminated().then(|| BoardDeath {
                cause: cause_str(&snake.eliminated_cause).to_string(),
                turn: snake.eliminated_on_turn,
                eliminated_by: snake.eliminated_by.clone(),
            });
            BoardSnake {
                id: snake.id.clone(),
                name: game.snake_names.get(&snake.id).cloned().unwrap_or_default(),
                body: snake.body.iter().map(point).collect(),
                health: snake.health,
                death,
                color: cust.map(|c| c.color.clone()).unwrap_or_default(),
                head_type: cust.map(|c| c.head.clone()).unwrap_or_default(),
                tail_type: cust.map(|c| c.tail.clone()).unwrap_or_default(),
                latency,
                shout: rt.map(|r| r.shout.clone()).unwrap_or_default(),
                squad: String::new(),
                author: cust.map(|c| c.author.clone()).unwrap_or_default(),
                status_code,
                error,
                is_bot: false,
                is_environment: false,
            }
        })
        .collect();
    GameFrame {
        turn: game.board.turn,
        snakes,
        food: game.board.food.iter().map(point).collect(),
        hazards: game.board.on_board_hazards().map(point).collect(),
    }
}

/// Byte-exact Go URL (Go's `Sprintf` with `%s` — NOT url-encoded):
/// `{base}?engine={engine_url}&game={game_id}&autoplay=true`.
pub fn board_url(base: &str, engine_url: &str, game_id: &str) -> String {
    format!("{base}?engine={engine_url}&game={game_id}&autoplay=true")
}

// ---------------------------------------------------------------------------
// SnakeRuntime: per-snake last request outcome, persists across turns
// (like Go's `snakeStates` map — dead snakes keep their last values).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SnakeRuntime {
    /// `wire::reported_latency_ms` output: `Some(timeout)` on timeout,
    /// `Some(clamped ms)` on a measurement, `None` before the first
    /// `/move` (turn 0).
    pub latency: Option<i64>,
    /// 200 from the ping, then the last `/move` status; 0 only when the
    /// request never got an answer.
    pub status_code: i32,
    /// No usable answer: timeout/transport, or a 200 with an unparseable
    /// body or unrecognized direction.
    pub request_failed: bool,
    /// Last sanitized shout.
    pub shout: String,
}

impl SnakeRuntime {
    /// Go sets StatusCode from the `GET /` response; our client rejects
    /// non-2xx pings as fatal, so every started game pinged 2xx.
    pub fn from_ping() -> Self {
        Self {
            status_code: 200,
            ..Default::default()
        }
    }

    pub fn from_move_result(
        result: &engine_core::move_response::MoveResult,
        timeout_ms: i64,
    ) -> Self {
        Self {
            latency: engine_core::wire::reported_latency_ms(
                result.latency_ms,
                result.timed_out,
                timeout_ms,
            ),
            status_code: i32::from(result.status_code.unwrap_or(0)),
            request_failed: result.timed_out || (result.errored && result.status_code == Some(200)),
            shout: result.shout.clone().unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// BrowserOptions
// ---------------------------------------------------------------------------

/// All timeouts for the board server (production defaults; tests shrink
/// them — config-for-timings with good defaults).
#[derive(Debug, Clone)]
pub struct BrowserOptions {
    pub board_url: String,
    pub open_browser: bool,
    /// After `game_end` is appended, how long to wait (from that moment)
    /// for a first browser connection before exiting.
    pub connect_grace: Duration,
    /// A NO-PROGRESS deadline for the drain, not a total deadline: reset
    /// every time any client's send counter advances. A slow-but-
    /// progressing replay of any length completes; a client making no
    /// progress for this long is dead.
    pub drain_stall: Duration,
    /// Bound on waiting for a websocket client's close acknowledgement
    /// (matches the engine's observed ~5s grace).
    pub close_ack_timeout: Duration,
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            board_url: "https://board.battlesnake.com".to_string(),
            open_browser: true,
            connect_grace: Duration::from_secs(25),
            drain_stall: Duration::from_secs(20),
            close_ack_timeout: Duration::from_secs(5),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Single source of truth for the event log's shape. Written ONLY via
/// `send_modify`: `watch::Sender::send` errors and drops the update when
/// zero receivers exist (tokio `watch.rs`), and `send_frame` routinely
/// runs before any client connects.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct LogState {
    len: usize,
    ended: bool,
}

/// No `Default` derive: `Instant` has none. Construct explicitly for
/// `watch::channel`. `last_progress` is only read once a client exists
/// (accept sets it), and `finish`'s drain loop is gated on
/// `ever_connected`, so the initial value is inert.
#[derive(Debug, Clone, PartialEq)]
struct ConnState {
    active: usize,
    ever_connected: bool,
    any_finished: bool,
    last_progress: Instant,
}

struct SharedEvents {
    /// Serialized messages in order. `Utf8Bytes` is refcounted, so replay
    /// is a cheap clone and `Message::Text` takes it directly.
    log: std::sync::Mutex<Vec<Utf8Bytes>>,
    state: tokio::sync::watch::Sender<LogState>,
}

impl SharedEvents {
    fn new() -> Self {
        Self {
            log: std::sync::Mutex::new(Vec::new()),
            state: tokio::sync::watch::channel(LogState::default()).0,
        }
    }
}

/// `Drop` guard: decrement `active` exactly once per connection, on every
/// exit path — return, send failure, or a panicking handler (Drop runs
/// during unwind).
struct ConnGuard(tokio::sync::watch::Sender<ConnState>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0
            .send_modify(|s| s.active = s.active.saturating_sub(1));
    }
}

/// Go's `cors.Default()` shape. One definition shared by both routes (no
/// `Access-Control-Allow-Private-Network`: Go doesn't send one and its
/// flow works without it).
fn cors_headers() -> [(header::HeaderName, HeaderValue); 3] {
    [
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, OPTIONS"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("*"),
        ),
    ]
}

fn with_cors(mut response: Response) -> Response {
    for (name, value) in cors_headers() {
        response.headers_mut().insert(name, value);
    }
    response
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ServerState {
    shared: Arc<SharedEvents>,
    conn: tokio::sync::watch::Sender<ConnState>,
    game: BoardGame,
    close_ack_timeout: Duration,
}

async fn get_game(State(state): State<ServerState>, Path(id): Path<String>) -> Response {
    if id != state.game.id {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    with_cors(
        (
            StatusCode::OK,
            axum::Json(GameInfoResponse {
                game: state.game.clone(),
            }),
        )
            .into_response(),
    )
}

async fn options_game(Path(_id): Path<String>) -> Response {
    with_cors((StatusCode::NO_CONTENT, "").into_response())
}

/// No origin check: Go's upgrader was `CheckOrigin: return true`.
async fn get_events(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if id != state.game.id {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    ws.on_upgrade(move |socket| {
        handle_client(socket, state.shared, state.conn, state.close_ack_timeout)
    })
    .into_response()
}

/// Per-connection task: full replay from turn 0, then the live tail. On a
/// client that has everything (including `game_end`): graceful close
/// FIRST, then signal `any_finished` — never before (aborting the serve
/// task mid-close RSTs the final frames).
async fn handle_client(
    socket: WebSocket,
    shared: Arc<SharedEvents>,
    conn: tokio::sync::watch::Sender<ConnState>,
    close_ack_timeout: Duration,
) {
    conn.send_modify(|s| {
        s.active += 1;
        s.ever_connected = true;
        s.last_progress = Instant::now();
    });
    // Decrements `active` on EVERY exit path — return, send failure, or a
    // panic (Drop runs during unwind).
    let _active_guard = ConnGuard(conn.clone());
    let mut state_rx = shared.state.subscribe();
    let mut sent: usize = 0;
    let (mut sender, mut receiver) = socket.split();
    loop {
        let (len, ended) = {
            let st = state_rx.borrow_and_update();
            (st.len, st.ended)
        };
        while sent < len {
            // Clone the Utf8Bytes out of the lock, drop the guard, THEN
            // send (never hold a std Mutex across an await).
            let message = shared
                .log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())[sent]
                .clone();
            if sender.send(Message::Text(message)).await.is_err() {
                // Client vanished, NOT finished; ReconnectingWebSocket
                // retries with a full replay.
                return;
            }
            sent += 1;
            conn.send_modify(|s| s.last_progress = Instant::now());
        }
        if ended && sent == len {
            break;
        }
        if state_rx.changed().await.is_err() {
            return; // server dropped, NOT finished
        }
    }
    // Only reached when this client has everything, including game_end.
    graceful_close(&mut sender, &mut receiver, close_ack_timeout).await;
    conn.send_modify(|s| s.any_finished = true);
    // `active -= 1` happens ONLY via the guard's Drop — keep it that way.
}

/// Send a Close(1000) frame and wait for the client's acknowledgement,
/// bounded by `timeout`. Dropping early can RST the TCP connection and
/// destroy the final in-flight frames. Modeled on the server's
/// `graceful_close` (hence the `socket.split()` in `handle_client`).
async fn graceful_close(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
    timeout: Duration,
) {
    let _ = tokio::time::timeout(timeout, async {
        let _ = sender
            .send(Message::Close(Some(CloseFrame {
                code: 1000, // NORMAL, like the engine (absent reads as 1005)
                reason: "".into(),
            })))
            .await;
        // Drain until the client sends Close back or the connection drops.
        while let Some(msg) = receiver.next().await {
            if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    })
    .await;
}

pub struct BoardServer {
    opts: BrowserOptions,
    game: BoardGame,
    /// `http://127.0.0.1:{port}`
    engine_url: String,
    shared: Arc<SharedEvents>,
    conn_rx: tokio::sync::watch::Receiver<ConnState>,
    /// Set by `send_game_end`; anchors `finish`'s no-browser grace window
    /// at the true game end.
    game_ended_at: Option<Instant>,
    serve: tokio::task::JoinHandle<()>,
}

impl BoardServer {
    /// Bind 127.0.0.1:0, spawn the axum serve task, log the Go lines, and
    /// open the browser (`that_detached`) unless `opts.open_browser`.
    pub async fn launch(
        game: BoardGame,
        opts: &BrowserOptions,
        log: &mut dyn Write,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .wrap_err("Error starting HTTP server")?;
        let engine_url = format!("http://{}", listener.local_addr()?);

        let shared = Arc::new(SharedEvents::new());
        let (conn_tx, conn_rx) = tokio::sync::watch::channel(ConnState {
            active: 0,
            ever_connected: false,
            any_finished: false,
            last_progress: Instant::now(),
        });
        let state = ServerState {
            shared: shared.clone(),
            conn: conn_tx,
            game: game.clone(),
            close_ack_timeout: opts.close_ack_timeout,
        };
        // axum 0.8 path syntax — the `:id` form panics at router build.
        let router = axum::Router::new()
            .route(
                "/games/{id}",
                axum::routing::get(get_game).options(options_game),
            )
            .route("/games/{id}/events", axum::routing::get(get_events))
            .with_state(state);
        let serve = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                eprintln!("Error in board HTTP server: {error}");
            }
        });

        let url = board_url(&opts.board_url, &engine_url, &game.id);
        write_log(log, &format!("Board server listening on {engine_url}"));
        write_log(log, &format!("Opening board URL: {url}"));
        // Local Network Access: every first-time Chrome user sees this
        // prompt on the page's calls to 127.0.0.1; it prompts, it does not
        // block (measured 2026-11-05, Chromium 151).
        write_log(
            log,
            "If your browser asks to let this site access other apps and services on this \
             device, choose Allow. That's how the board reads the game from this CLI.",
        );
        if opts.open_browser {
            // `that_detached` (never `that`): `that` waits for the launcher
            // process to exit, so a foreground launcher (a cold-starting
            // xdg-open, a terminal browser) would freeze the game.
            if let Err(error) = open::that_detached(&url) {
                write_log(log, &format!("Failed to open browser: {error}"));
            }
        } else {
            write_log(log, &format!("Board URL: {url}"));
        }

        Ok(Self {
            opts: opts.clone(),
            game,
            engine_url,
            shared,
            conn_rx,
            game_ended_at: None,
            serve,
        })
    }

    pub fn engine_url(&self) -> &str {
        &self.engine_url
    }

    /// The full `?engine=…` URL (for tests and `finish`'s log lines).
    pub fn board_url(&self) -> String {
        board_url(&self.opts.board_url, &self.engine_url, &self.game.id)
    }

    /// Serialize + append a frame event. Never blocks the game.
    pub fn send_frame(
        &mut self,
        game: &engine_core::EngineGame,
        customizations: &HashMap<String, SnakeCustomizations>,
        runtime: &HashMap<String, SnakeRuntime>,
    ) -> Result<()> {
        self.append(&GameEvent {
            message_type: "frame",
            data: build_frame(game, customizations, runtime),
        })
    }

    /// Append `{"Type":"game_end","Data":{BoardGame}}`, set `ended`, and
    /// stamp `game_ended_at` (one atomic `send_modify`).
    pub fn send_game_end(&mut self) -> Result<()> {
        self.append(&GameEvent {
            message_type: "game_end",
            data: self.game.clone(),
        })?;
        self.game_ended_at = Some(Instant::now());
        self.shared.state.send_modify(|s| s.ended = true);
        Ok(())
    }

    fn append(&self, event: &GameEvent<impl Serialize>) -> Result<()> {
        let json = serde_json::to_string(event).wrap_err("serializing board event")?;
        let mut log = self
            .shared
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        log.push(json.into());
        let len = log.len();
        drop(log); // never hold the std Mutex across an await (none here, keep the habit)
        self.shared.state.send_modify(|s| s.len = len);
        Ok(())
    }

    /// The bounded, progress-aware shutdown (100ms poll):
    /// (a) any client finished its close handshake; (b) `game_end` was
    /// appended and no client is active (every tab closed); or (c) no
    /// send progress for `drain_stall`. The grace clock is anchored at
    /// `game_ended_at` — the sequential `/end` requests and the export
    /// write happen inside the window, so exit lands within
    /// `connect_grace` + one poll of the true game end.
    pub async fn finish(&mut self, log: &mut dyn Write) {
        let game_ended_at = self.game_ended_at.unwrap_or_else(Instant::now);
        // 1. Wait for a first connection, up to connect_grace after game
        //    end. Say so: otherwise a run with no browser sits silent.
        if !self.conn_rx.borrow().ever_connected {
            write_log(
                log,
                &format!(
                    "Waiting for a browser to connect. Board URL: {}",
                    self.board_url()
                ),
            );
        }
        while !self.conn_rx.borrow_and_update().ever_connected {
            if game_ended_at.elapsed() >= self.opts.connect_grace {
                write_log(
                    log,
                    &format!(
                        "No browser connected to the board server. Board URL: {}",
                        self.board_url()
                    ),
                );
                break; // fall through to teardown
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 2. Progress-aware drain (only if a client connected).
        while self.conn_rx.borrow().ever_connected {
            let st = self.conn_rx.borrow().clone();
            if st.any_finished || st.last_progress.elapsed() >= self.opts.drain_stall {
                break;
            }
            let ended = self.shared.state.borrow().ended;
            if ended && st.active == 0 {
                break; // every tab closed; nothing to drain
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 3. Teardown: dropping the JoinHandle detaches (does NOT
        //    cancel), so abort. A mid-connect abort is fine: the board's
        //    ReconnectingWebSocket reconnects and gets a full replay —
        //    and if we're exiting, the game is over and the first tab
        //    already has everything.
        self.serve.abort();
        let _ = (&mut self.serve).await;
    }
}

// The survivor policy is shared with the integration tests via `#[path]`;
// kept at the TOP LEVEL of this file (not inside `mod tests`, where
// `#[path]` would resolve relative to `cli/src/play/board_server/tests/`).
#[cfg(test)]
#[path = "../../tests/support/survivor_policy.rs"]
mod survivor_policy;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::play::tests::base_args;
    use crate::play::{PlayArgs, run_game, validate};
    use engine_core::wire::SnakeCustomizations;
    use engine_core::{GameMode, GameSource};
    use rules::{Point, RoyaleSettings, StandardSettings};

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn config(gametype: &str) -> PlayConfig {
        let args = PlayArgs {
            gametype: gametype.to_string(),
            ..base_args()
        };
        validate(&args, false).expect("valid config")
    }

    fn fixture_game(turn: i32) -> engine_core::EngineGame {
        let snake = rules::Snake {
            id: "snk_0".to_string(),
            body: vec![Point::new(1, 5); 3],
            health: 100,
            eliminated_cause: rules::EliminationCause::NotEliminated,
            eliminated_by: String::new(),
            eliminated_on_turn: 0,
        };
        engine_core::EngineGame {
            board: rules::BoardState {
                turn,
                width: 11,
                height: 11,
                food: vec![Point::new(0, 4)],
                snakes: vec![snake],
                hazards: vec![],
            },
            meta: engine_core::GameMeta {
                game_id: "GAME_ID".into(),
                ruleset_name: "standard".into(),
                timeout: 500,
                settings: StandardSettings {
                    food_spawn_chance: 15,
                    minimum_food: 1,
                    hazard_damage_per_turn: 14,
                },
                royale: Some(RoyaleSettings {
                    shrink_every_n_turns: 25,
                    seed: 0,
                }),
                source: GameSource::Custom,
            },
            snake_names: [("snk_0".into(), "example snake".into())].into(),
        }
    }

    fn fixture_customizations() -> HashMap<String, SnakeCustomizations> {
        [(
            "snk_0".to_string(),
            SnakeCustomizations {
                color: "#123456".into(),
                head: "safe".into(),
                tail: "curled".into(),
                author: "byte".into(),
            },
        )]
        .into()
    }

    fn fast_opts() -> BrowserOptions {
        BrowserOptions {
            open_browser: false,
            connect_grace: Duration::from_secs(1),
            drain_stall: Duration::from_secs(1),
            ..BrowserOptions::default()
        }
    }

    async fn launch_server(game: Option<BoardGame>) -> BoardServer {
        let game = game.unwrap_or_else(|| board_game(&config("standard"), "fixture-game"));
        let mut log = Vec::new();
        BoardServer::launch(game, &fast_opts(), &mut log)
            .await
            .expect("launch")
    }

    /// Append a frame event with the given turn, exactly what `run_game`
    /// does (append with no client connected).
    fn append_frame(srv: &mut BoardServer, turn: i32) {
        srv.send_frame(
            &fixture_game(turn),
            &fixture_customizations(),
            &HashMap::new(),
        )
        .expect("append frame");
    }

    // ------------------------------------------------------------------
    // A. Pure unit tests
    // ------------------------------------------------------------------

    /// A1: byte-exact Go URL.
    #[test]
    fn board_url_is_go_byte_exact() {
        assert_eq!(
            board_url(
                "https://board.battlesnake.com",
                "http://127.0.0.1:39281",
                "0f0a1111-2222-3333-4444-555566667777"
            ),
            "https://board.battlesnake.com?engine=http://127.0.0.1:39281\
             &game=0f0a1111-2222-3333-4444-555566667777&autoplay=true"
        );
        assert_eq!(
            board_url("https://example.test/board", "http://127.0.0.1:1", "g"),
            "https://example.test/board?engine=http://127.0.0.1:1&game=g&autoplay=true"
        );
    }

    /// A2: `GET /games/{id}` JSON matches `board/api.go`'s Game shape.
    #[test]
    fn game_info_json_matches_board_api_go() {
        let mut expected = [
            "ID",
            "Status",
            "Width",
            "Height",
            "Ruleset",
            "SnakeTimeout",
            "Source",
            "RulesetName",
            "RulesStages",
            "Map",
        ];
        expected.sort_unstable();
        for (gametype, want_ruleset, want_map) in [
            ("solo", "solo", "standard"),
            ("snail_mode", "standard", "snail_mode"),
            ("standard", "standard", "standard"),
        ] {
            let game = board_game(&config(gametype), "game-id");
            let json = serde_json::to_value(GameInfoResponse { game }).unwrap();
            let inner = &json["Game"];
            let mut keys: Vec<&str> = inner
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, expected, "{gametype}");
            assert_eq!(inner["ID"], "game-id", "{gametype}");
            assert_eq!(inner["Status"], "running", "{gametype}");
            assert_eq!(inner["Width"], 11, "{gametype}");
            assert_eq!(inner["Height"], 11, "{gametype}");
            assert_eq!(
                inner["Ruleset"],
                serde_json::json!({"name": want_ruleset}),
                "{gametype}"
            );
            assert_eq!(inner["SnakeTimeout"], 500, "{gametype}");
            assert_eq!(inner["Source"], "custom", "{gametype}");
            assert_eq!(inner["RulesetName"], want_ruleset, "{gametype}");
            assert_eq!(
                inner["RulesStages"],
                serde_json::json!([]),
                "{gametype}: never null"
            );
            assert_eq!(inner["Map"], want_map, "{gametype}");
        }
    }

    /// A3: frame JSON matches `board/api.go` (capitalized points, null vs
    /// `[]`, Death per EliminationCause).
    #[test]
    fn frame_json_matches_board_api_go() {
        let game = fixture_game(7);
        let json = serde_json::to_value(build_frame(
            &game,
            &fixture_customizations(),
            &HashMap::new(),
        ))
        .unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["Food", "Hazards", "Snakes", "Turn"]);
        assert_eq!(json["Turn"], 7);
        let snake = &json["Snakes"][0];
        // Capitalized point coordinates — the board reads `.X`/`.Y`.
        assert_eq!(snake["Body"][0], serde_json::json!({"X": 1, "Y": 5}));
        assert_eq!(json["Food"][0], serde_json::json!({"X": 0, "Y": 4}));
        // Empty collections serialize as `[]`, never `null` (the board
        // maps over them).
        assert_eq!(json["Hazards"], serde_json::json!([]));
        // Live snake: Death is null, like Go's *Death nil.
        assert_eq!(snake["Death"], serde_json::Value::Null);

        // Every eliminated cause maps to its Go string.
        let causes = [
            (rules::EliminationCause::OutOfHealth, "out-of-health"),
            (rules::EliminationCause::OutOfBounds, "wall-collision"),
            (
                rules::EliminationCause::SelfCollision,
                "snake-self-collision",
            ),
            (rules::EliminationCause::Collision, "snake-collision"),
            (
                rules::EliminationCause::HeadToHeadCollision,
                "head-collision",
            ),
            (rules::EliminationCause::Hazard, "hazard"),
        ];
        for (cause, want) in causes {
            let mut dead = fixture_game(12);
            dead.board.snakes[0].eliminated_cause = cause;
            dead.board.snakes[0].eliminated_on_turn = 12;
            let json = serde_json::to_value(build_frame(
                &dead,
                &fixture_customizations(),
                &HashMap::new(),
            ))
            .unwrap();
            assert_eq!(
                json["Snakes"][0]["Death"],
                serde_json::json!({"Cause": want, "Turn": 12, "EliminatedBy": ""}),
                "{want}"
            );
        }
    }

    /// A4: latency follows `reported_latency_ms` + Go's round-up.
    #[test]
    fn latency_matches_reported_latency_ms() {
        let cases = |rt: &SnakeRuntime| {
            build_frame(
                &fixture_game(0),
                &HashMap::new(),
                &[("snk_0".to_string(), rt.clone())].into(),
            )
            .snakes[0]
                .latency
                .clone()
        };
        // Missing/turn-0: no measurement -> "1".
        assert_eq!(cases(&SnakeRuntime::default()), "1");
        assert_eq!(cases(&SnakeRuntime::from_ping()), "1");
        // A clean measured move.
        assert_eq!(
            cases(&SnakeRuntime::from_move_result(
                &engine_core::move_response::MoveResult {
                    snake_id: "snk_0".into(),
                    direction: rules::Direction::Up,
                    latency_ms: Some(47),
                    timed_out: false,
                    status_code: Some(200),
                    shout: None,
                    errored: false,
                },
                500,
            )),
            "47"
        );
        // A timed-out move reports the timeout value, engine convention.
        assert_eq!(
            cases(&SnakeRuntime::from_move_result(
                &engine_core::move_response::MoveResult {
                    snake_id: "snk_0".into(),
                    direction: rules::Direction::Up,
                    latency_ms: None,
                    timed_out: true,
                    status_code: None,
                    shout: None,
                    errored: false,
                },
                500,
            )),
            "500"
        );
        // The JSON field is a string.
        let json = serde_json::to_value(build_frame(
            &fixture_game(0),
            &HashMap::new(),
            &[("snk_0".to_string(), SnakeRuntime::from_ping())].into(),
        ))
        .unwrap();
        assert!(json["Snakes"][0]["Latency"].is_string());
    }

    /// A5: the two Go error strings, with Rule 4's precedence.
    #[test]
    fn error_strings_match_go() {
        let move_result = |timed_out, errored, status| engine_core::move_response::MoveResult {
            snake_id: "snk_0".into(),
            direction: rules::Direction::Up,
            latency_ms: None,
            timed_out,
            status_code: status,
            shout: None,
            errored,
        };
        let error_of = |rt: &SnakeRuntime| {
            build_frame(
                &fixture_game(0),
                &HashMap::new(),
                &[("snk_0".to_string(), rt.clone())].into(),
            )
            .snakes[0]
                .error
                .clone()
        };
        // Timeout (no answer at all).
        assert_eq!(
            error_of(&SnakeRuntime::from_move_result(
                &move_result(true, false, None),
                500
            )),
            "0:Error communicating with server"
        );
        // Transport failure: every transport arm sets timed_out, so it
        // lands on the 0-string too.
        assert_eq!(
            error_of(&SnakeRuntime::from_move_result(
                &move_result(true, false, None),
                500
            )),
            "0:Error communicating with server"
        );
        // Answered 503: Go checks status != 200.
        assert_eq!(
            error_of(&SnakeRuntime::from_move_result(
                &move_result(false, true, Some(503)),
                500
            )),
            "7:Bad HTTP status code 503"
        );
        // 200 with an unparseable body (documented divergence: a
        // valid-JSON-invalid-move also lands here; Go sent "").
        assert_eq!(
            error_of(&SnakeRuntime::from_move_result(
                &move_result(false, true, Some(200)),
                500
            )),
            "0:Error communicating with server"
        );
        // Clean 200.
        assert_eq!(
            error_of(&SnakeRuntime::from_move_result(
                &move_result(false, false, Some(200)),
                500
            )),
            ""
        );
        // 201 with a valid move: Go's `!= 200` rule still gets the 7-string.
        assert_eq!(
            error_of(&SnakeRuntime::from_move_result(
                &move_result(false, false, Some(201)),
                500
            )),
            "7:Bad HTTP status code 201"
        );
    }

    /// A6: `game_end`'s Data is the full BoardGame, byte-equal to the
    /// info endpoint's Game value (through the real append path).
    #[tokio::test]
    async fn game_end_event_data_is_the_game_object() {
        let mut srv = launch_server(None).await;
        srv.send_game_end().expect("game end");
        let log = srv.shared.log.lock().unwrap();
        let last: serde_json::Value = serde_json::from_str(log.last().unwrap().as_str()).unwrap();
        assert_eq!(last["Type"], "game_end");
        let info_game = serde_json::to_value(GameInfoResponse {
            game: srv.game.clone(),
        })
        .unwrap()["Game"]
            .clone();
        assert_eq!(last["Data"], info_game);
    }

    /// A7: turn-0 snake fields (ping runtime).
    #[test]
    fn turn_zero_snake_fields() {
        let with_author = |author: &str| {
            let customizations = [(
                "snk_0".to_string(),
                SnakeCustomizations {
                    color: "#123456".into(),
                    head: "safe".into(),
                    tail: "curled".into(),
                    author: author.into(),
                },
            )]
            .into();
            serde_json::to_value(build_frame(
                &fixture_game(0),
                &customizations,
                &[("snk_0".to_string(), SnakeRuntime::from_ping())].into(),
            ))
            .unwrap()["Snakes"][0]
                .clone()
        };
        let snake = with_author("byte");
        assert_eq!(snake["StatusCode"], 200);
        assert_eq!(snake["Latency"], "1");
        assert_eq!(snake["IsBot"], false);
        assert_eq!(snake["IsEnvironment"], false);
        assert_eq!(snake["Squad"], "");
        assert_eq!(snake["Author"], "byte");
        assert_eq!(snake["Error"], "");
        // A snake that declares no author: "" (the board then renders a
        // dangling "by " — board rendering behavior, same for any
        // producer).
        assert_eq!(with_author("")["Author"], "");
    }

    /// A8: hazards reach Royale and Snail Mode frames (engine-direct, no
    /// HTTP); every other mode's frames carry none.
    #[test]
    fn hazards_reach_royale_and_snail_frames() {
        let build_game = |mode: GameMode, royale_shrink: i32| {
            let (mut rng, game_id) = engine_core::seeded_game(42);
            let initial = vec![engine_core::InitialSnake {
                id: "s1".to_string(),
                name: "A".to_string(),
            }];
            let mut game =
                engine_core::create_initial_game(&mut rng, game_id, 11, 11, mode, &initial)
                    .unwrap();
            game.meta.royale = Some(RoyaleSettings {
                shrink_every_n_turns: royale_shrink,
                seed: 0,
            });
            game
        };

        // Royale with an explicit small shrink (the default 25 can outlast
        // a snake that dies on turn 2).
        let mut royale = build_game(GameMode::Royale, 3);
        let (mut rng, _) = engine_core::seeded_game(7);
        let cycle = [
            rules::Direction::Up,
            rules::Direction::Right,
            rules::Direction::Down,
            rules::Direction::Left,
        ];
        let mut any_royale_hazards = false;
        for turn in 0..12 {
            let moves = royale
                .board
                .snakes
                .iter()
                .filter(|s| !s.eliminated_cause.is_eliminated())
                .map(|s| (s.id.clone(), cycle[turn % 4]))
                .collect::<Vec<_>>();
            if moves.is_empty() {
                break;
            }
            engine_core::advance_turn(&mut royale, &moves, &mut rng).unwrap();
            let frame = build_frame(&royale, &HashMap::new(), &HashMap::new());
            any_royale_hazards |= !frame.hazards.is_empty();
        }
        assert!(any_royale_hazards, "no Royale frame carried hazards");

        // Snail Mode: trails record vacated tail squares — but a restore
        // is DROPPED when a live head sits on the square, so the walk must
        // never revisit cells (the 2x2 cycle keeps a head on every trail
        // square: body length 4 covers the whole square). A monotone
        // walk (up until near the top wall, then right into it) vacates a
        // fresh cell every turn.
        let mut snail = build_game(GameMode::SnailMode, 25);
        let (mut rng, _) = engine_core::seeded_game(7);
        let mut any_snail_hazards = false;
        for _ in 0..12 {
            let head = snail.board.snakes[0].body[0];
            let dir = if head.y < 9 {
                rules::Direction::Up
            } else {
                rules::Direction::Right
            };
            let moves: Vec<_> = snail
                .board
                .snakes
                .iter()
                .filter(|s| !s.eliminated_cause.is_eliminated())
                .map(|s| (s.id.clone(), dir))
                .collect();
            if moves.is_empty() {
                break;
            }
            engine_core::advance_turn(&mut snail, &moves, &mut rng).unwrap();
            let frame = build_frame(&snail, &HashMap::new(), &HashMap::new());
            any_snail_hazards |= !frame.hazards.is_empty();
        }
        assert!(any_snail_hazards, "no Snail Mode frame carried hazards");

        // Standard, Solo, Constrictor: never.
        for mode in [GameMode::Standard, GameMode::Solo, GameMode::Constrictor] {
            let mut game = build_game(mode, 25);
            let (mut rng, _) = engine_core::seeded_game(7);
            for turn in 0..12 {
                let moves = game
                    .board
                    .snakes
                    .iter()
                    .filter(|s| !s.eliminated_cause.is_eliminated())
                    .map(|s| (s.id.clone(), cycle[turn % 4]))
                    .collect::<Vec<_>>();
                if moves.is_empty() {
                    break;
                }
                engine_core::advance_turn(&mut game, &moves, &mut rng).unwrap();
                let frame = build_frame(&game, &HashMap::new(), &HashMap::new());
                assert!(
                    frame.hazards.is_empty(),
                    "{mode:?} frame carried hazards: {:?}",
                    frame.hazards
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // A9: the Go-blocking regression — a >1,000-frame game driven by the
    // shared survivor policy (see the module-level `#[path]` include; no
    // second policy is written here).
    // ------------------------------------------------------------------

    /// Every one of these line prefixes is a move-path warning that
    /// replaces the snake's move with a fallback (copied from
    /// `cli/tests/play_cli.rs`); one fallback breaks determinism.
    fn assert_no_fallback_warnings(stderr: &str) {
        for line in stderr.lines() {
            for prefix in [
                "Request to",
                "Failed to read",
                "Got non-ok",
                "Failed to parse",
                "Failed to decode",
            ] {
                assert!(
                    !line.starts_with(prefix),
                    "a fallback warning changed the game: {line}"
                );
            }
        }
    }

    /// wiremock transport for the survivor snake: GET returns the same
    /// info JSON as `SurvivorSnake` in play_cli.rs, /move consults the
    /// shared policy, /start and /end return `{}`.
    struct SurvivorRespond;

    impl wiremock::Respond for SurvivorRespond {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let path = request.url.path();
            if request.method == "GET" || !path.ends_with("/move") {
                return wiremock::ResponseTemplate::new(200).set_body_string(
                    r##"{"apiversion":"1","color":"#888800","head":"safe","tail":"bolt"}"##,
                );
            }
            let body = String::from_utf8_lossy(request.body.as_slice()).into_owned();
            wiremock::ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"move": "{}"}}"#,
                survivor_policy::survivor_move(&body, 1_050)
            ))
        }
    }

    /// The exact game inputs of DEV-2164's
    /// `no_default_turn_limit_game_runs_past_5000_turns` (solo, 19x19,
    /// Survivor, seed 1, no food spawn, 15 minimum food, 60s timeout, no
    /// --max-turns), built through `validate()` so the defaults are the
    /// CLI's, with `browser` turned on. With `suicide_turn = 1_050` the
    /// same verified game survives past turn 1,000 and ends around
    /// 1,051..=1,070.
    fn survivor_config(browser: BrowserOptions, url: String) -> PlayConfig {
        let args = PlayArgs {
            width: 19,
            height: 19,
            names: vec!["Survivor".into()],
            urls: vec![url],
            gametype: "solo".into(),
            seed: Some(1),
            food_spawn_chance: 0,
            minimum_food: 15,
            timeout: 60_000,
            ..base_args()
        };
        let mut cfg = validate(&args, false).expect("valid config");
        cfg.browser = Some(browser);
        cfg
    }

    async fn survivor_server() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(SurvivorRespond)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(SurvivorRespond)
            .mount(&server)
            .await;
        server
    }

    /// A9a: no websocket client — the game runs ahead, past frame 1,001,
    /// and `run_game` still returns Ok. Go blocks forever here.
    #[tokio::test]
    async fn no_browser_does_not_block() {
        let server = survivor_server().await;
        let cfg = survivor_config(
            BrowserOptions {
                open_browser: false,
                connect_grace: Duration::from_secs(2),
                ..BrowserOptions::default()
            },
            server.uri(),
        );
        let client = crate::play::http::SnakeHttpClient::new(cfg.timeout_ms, false);
        let mut log = Vec::new();
        let mut board = Vec::new();
        let outcome = tokio::time::timeout(
            Duration::from_secs(120),
            run_game(&cfg, &client, None, &mut log, &mut board),
        )
        .await
        .expect("no hang without a browser (Go blocks forever)")
        .expect("game runs");
        assert!(
            outcome.final_turn > 1_000,
            "ended at {}",
            outcome.final_turn
        );
        let log = String::from_utf8(log).unwrap();
        assert_no_fallback_warnings(&log);
        assert!(
            log.contains("No browser connected to the board server."),
            "{log}"
        );
    }

    /// Test-only shared log writer: lets a ws client watch the log while
    /// `run_game` holds `&mut` to it (why: every existing caller awaits
    /// `run_game` inline — its `&mut dyn Write` params are not `Send`).
    #[derive(Clone, Default)]
    struct SharedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedLog {
        fn snapshot(&self) -> String {
            String::from_utf8_lossy(
                &self
                    .0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            )
            .into_owned()
        }
    }

    /// A9b: a live client during a >1,000-turn game receives every frame,
    /// exactly once and in order, ending with `game_end` and close 1000.
    #[tokio::test]
    async fn client_receives_every_frame_past_1000() {
        let server = survivor_server().await;
        let cfg = survivor_config(
            BrowserOptions {
                open_browser: false,
                connect_grace: Duration::from_secs(30),
                ..BrowserOptions::default()
            },
            server.uri(),
        );
        let shared = SharedLog::default();
        let client_log = shared.clone();

        let board_client = async {
            // Wait for the listening + URL lines (port and game id).
            let (port, game_id) = loop {
                let snapshot = client_log.snapshot();
                let Some(listening) = snapshot
                    .lines()
                    .find_map(|l| l.strip_prefix("Board server listening on http://127.0.0.1:"))
                else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let Some(url_line) = snapshot
                    .lines()
                    .find_map(|l| l.strip_prefix("Opening board URL: "))
                else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let port: u16 = listening.trim_end_matches('/').parse().unwrap();
                let game_id = url_line
                    .split("&game=")
                    .nth(1)
                    .and_then(|rest| rest.split('&').next())
                    .unwrap()
                    .to_string();
                break (port, game_id);
            };
            let url = format!("ws://127.0.0.1:{port}/games/{game_id}/events");
            let (mut ws, _) = tokio_tungstenite::connect_async(url)
                .await
                .expect("connect");
            let mut messages = Vec::new();
            let mut close_code = None;
            while let Some(message) = tokio::time::timeout(Duration::from_secs(60), ws.next())
                .await
                .unwrap()
            {
                use tokio_tungstenite::tungstenite::Message as Tm;
                match message.unwrap() {
                    Tm::Text(text) => {
                        messages.push(serde_json::from_str::<serde_json::Value>(&text).unwrap());
                    }
                    Tm::Close(frame) => {
                        close_code = frame.map(|f| u16::from(f.code));
                        break;
                    }
                    _ => {}
                }
            }
            (messages, close_code)
        };

        let client = crate::play::http::SnakeHttpClient::new(cfg.timeout_ms, false);
        let mut log = shared.clone();
        let mut board = Vec::new();
        // Current-thread runtime: join! of the non-Send run_game future
        // and the ws client is legal; BoardServer's internal tasks still
        // run at await points.
        let (outcome, (messages, close_code)) = tokio::join!(
            run_game(&cfg, &client, None, &mut log, &mut board),
            board_client,
        );
        let outcome = outcome.expect("game runs");
        assert!(
            outcome.final_turn > 1_000,
            "ended at {}",
            outcome.final_turn
        );

        // Every turn 0..=final exactly once, in order.
        let turns: Vec<i64> = messages
            .iter()
            .filter(|m| m["Type"] == "frame")
            .map(|m| m["Data"]["Turn"].as_i64().unwrap())
            .collect();
        let want: Vec<i64> = (0..=outcome.final_turn as i64).collect();
        assert_eq!(
            turns, want,
            "frames must cover every turn exactly once, in order"
        );
        // game_end is the final data message.
        assert_eq!(messages.last().unwrap()["Type"], "game_end");
        // Close code 1000 (a frame-absent close would read 1005).
        assert_eq!(close_code, Some(1000), "close code: {close_code:?}");

        assert_no_fallback_warnings(&shared.snapshot());
    }

    // ------------------------------------------------------------------
    // B. Server tests (standalone BoardServer + tokio-tungstenite client)
    // ------------------------------------------------------------------

    /// ws:// URL for a running server's game (tungstenite requires the
    /// ws scheme; the engine URL is http).
    fn events_url(srv: &BoardServer) -> String {
        format!(
            "{}/games/{}/events",
            srv.engine_url().replacen("http", "ws", 1),
            srv.game.id
        )
    }

    async fn ws_connect(
        url: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        tokio_tungstenite::connect_async(url.to_string())
            .await
            .expect("ws connect")
            .0
    }

    async fn next_json(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> serde_json::Value {
        use tokio_tungstenite::tungstenite::Message as Tm;
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("message within 5s")
            .expect("stream open")
            .expect("ws ok");
        let Tm::Text(text) = message else {
            panic!("expected text: {message:?}");
        };
        serde_json::from_str(&text).unwrap()
    }

    async fn next_close_code(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Option<u16> {
        use tokio_tungstenite::tungstenite::Message as Tm;
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("close within 5s")
            .expect("stream open")
            .expect("ws ok");
        match message {
            Tm::Close(frame) => frame.map(|f| u16::from(f.code)),
            other => panic!("expected close, got {other:?}"),
        }
    }

    /// B1: the info endpoint serves the game with CORS; wrong id 404s.
    #[tokio::test]
    async fn info_endpoint_serves_game_with_cors() {
        let mut srv = launch_server(None).await;
        let response = reqwest::get(format!("{}/games/fixture-game", srv.engine_url()))
            .await
            .expect("get");
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "*",
            "the board's origin must be allowed"
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["Game"]["ID"], "fixture-game");

        let wrong = reqwest::get(format!("{}/games/wrong-id", srv.engine_url()))
            .await
            .expect("get");
        assert_eq!(wrong.status(), 404);

        let mut log = Vec::new();
        srv.finish(&mut log).await;
    }

    /// B2: replay history, then stream live, then game_end + close 1000.
    /// The close assertion is load-bearing: a server that sends Close and
    /// drops without draining unread frames leaves data in the recv
    /// buffer and the kernel sends RST instead of FIN.
    #[tokio::test]
    async fn websocket_replays_history_then_streams_live() {
        let mut srv = launch_server(None).await;
        for turn in 0..5 {
            append_frame(&mut srv, turn);
        }
        let url = events_url(&srv);
        let mut ws = ws_connect(&url).await;

        // All 5 arrive in order.
        for turn in 0..5 {
            let message = next_json(&mut ws).await;
            assert_eq!(message["Type"], "frame");
            assert_eq!(message["Data"]["Turn"], turn);
        }

        // Append 2 more with the client connected: they arrive.
        append_frame(&mut srv, 5);
        append_frame(&mut srv, 6);
        for turn in 5..7 {
            let message = next_json(&mut ws).await;
            assert_eq!(message["Data"]["Turn"], turn);
        }

        // game_end arrives, then the server closes with code 1000.
        srv.send_game_end().unwrap();
        let message = next_json(&mut ws).await;
        assert_eq!(message["Type"], "game_end");
        assert_eq!(next_close_code(&mut ws).await, Some(1000));

        let mut log = Vec::new();
        srv.finish(&mut log).await;
    }

    /// B3: a second client connecting mid-stream gets everything from
    /// turn 0 too.
    #[tokio::test]
    async fn second_client_also_gets_full_history() {
        let mut srv = launch_server(None).await;
        for turn in 0..4 {
            append_frame(&mut srv, turn);
        }
        let url = events_url(&srv);
        let mut first = ws_connect(&url).await;
        for turn in 0..4 {
            let message = next_json(&mut first).await;
            assert_eq!(message["Data"]["Turn"], turn);
        }
        append_frame(&mut srv, 4);
        let message = next_json(&mut first).await;
        assert_eq!(message["Data"]["Turn"], 4);

        // Client 2 connects mid-way: full history from turn 0.
        let mut second = ws_connect(&url).await;
        for turn in 0..=4 {
            let message = next_json(&mut second).await;
            assert_eq!(message["Data"]["Turn"], turn);
        }
        srv.send_game_end().unwrap();
        for ws in [&mut first, &mut second] {
            let message = next_json(ws).await;
            assert_eq!(message["Type"], "game_end");
        }

        let mut log = Vec::new();
        srv.finish(&mut log).await;
    }

    /// B4: `finish` returns within the grace of the `send_game_end` call
    /// when no client ever connects.
    #[tokio::test]
    async fn finish_times_out_without_clients() {
        let mut srv = launch_server(Some(board_game(&config("standard"), "b4"))).await;
        for turn in 0..3 {
            append_frame(&mut srv, turn);
        }
        srv.send_game_end().unwrap();
        let started = Instant::now();
        let mut log = Vec::new();
        srv.finish(&mut log).await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "grace is anchored at game_end, not at finish entry: {:?}",
            started.elapsed()
        );
        let log = String::from_utf8(log).unwrap();
        assert!(
            log.contains("No browser connected to the board server."),
            "{log}"
        );
    }

    /// B5: `finish` returns only after a draining client's close
    /// handshake (the set-done-before-close race).
    #[tokio::test]
    async fn finish_returns_after_client_drains() {
        let mut srv = launch_server(Some(board_game(&config("standard"), "b5"))).await;
        for turn in 0..3 {
            append_frame(&mut srv, turn);
        }
        srv.send_game_end().unwrap();
        let url = events_url(&srv);

        let mut closed_at: Option<Instant> = None;
        let client = async {
            let mut ws = ws_connect(&url).await;
            for turn in 0..3 {
                assert_eq!(next_json(&mut ws).await["Data"]["Turn"], turn);
            }
            assert_eq!(next_json(&mut ws).await["Type"], "game_end");
            assert_eq!(next_close_code(&mut ws).await, Some(1000));
            closed_at = Some(Instant::now());
        };
        let mut finish_log = Vec::new();
        let finish_at = async {
            srv.finish(&mut finish_log).await;
            Instant::now()
        };
        let ((), finish_at) = tokio::join!(client, finish_at);
        let closed_at = closed_at.expect("client closed");
        // finish returned at or after the client observed the close.
        assert!(
            finish_at >= closed_at,
            "finish returned before the client saw the close"
        );
    }

    /// B6: the deterministic >1,000 delivery check — 2,000 frames
    /// appended with no client, then one connects after game_end.
    #[tokio::test]
    async fn late_client_gets_2000_events_after_game_end() {
        let mut srv = launch_server(Some(board_game(&config("standard"), "b6"))).await;
        for turn in 0..2_000 {
            append_frame(&mut srv, turn);
        }
        srv.send_game_end().unwrap();

        let url = events_url(&srv);
        let client = async {
            let mut ws = ws_connect(&url).await;
            let mut turns = Vec::new();
            let mut last_type = String::new();
            loop {
                let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
                    .await
                    .unwrap();
                let Some(message) = message else { break };
                use tokio_tungstenite::tungstenite::Message as Tm;
                match message.unwrap() {
                    Tm::Text(text) => {
                        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                        last_type = v["Type"].as_str().unwrap().to_string();
                        if last_type == "frame" {
                            turns.push(v["Data"]["Turn"].as_i64().unwrap());
                        }
                    }
                    Tm::Close(frame) => {
                        return (turns, last_type, frame.map(|f| u16::from(f.code)));
                    }
                    _ => {}
                }
            }
            (turns, last_type, None)
        };
        let mut log = Vec::new();
        let ((turns, last_type, close_code), ()) = tokio::join!(client, srv.finish(&mut log));
        assert_eq!(turns.len(), 2_000);
        assert_eq!(
            turns,
            (0..2_000).map(i64::from).collect::<Vec<_>>(),
            "every frame once, in order"
        );
        assert_eq!(last_type, "game_end");
        assert_eq!(close_code, Some(1000));
    }

    /// B7: a slow-but-progressing client beats the stall window — the
    /// drain deadline is no-progress, not total time. Frames are BIG
    /// (~260 KB): socket buffers hold under two of them, so the
    /// handler's sends block on the client's reads and `last_progress`
    /// tracks delivery — the regime a real board replaying a long game
    /// is in. 10 frames read at 300 ms each ≈ 3 s total (> the 1 s
    /// stall window) with progress every 300 ms (< the window).
    #[tokio::test]
    async fn slow_client_progress_beats_stall_window() {
        let mut srv = launch_server(Some(board_game(&config("standard"), "b7"))).await;
        let big_frame_game = |turn: i32| {
            let mut game = fixture_game(turn);
            // ~20,000 points ≈ 260 KB of JSON per frame.
            game.board.snakes[0].body = vec![Point::new(1, 1); 20_000];
            game
        };
        for turn in 0..10 {
            srv.send_frame(&big_frame_game(turn), &HashMap::new(), &HashMap::new())
                .expect("append frame");
        }
        srv.send_game_end().unwrap();
        let url = events_url(&srv);

        let client = async {
            let mut ws = ws_connect(&url).await;
            let mut got_end = false;
            let mut close_code = None;
            // 10 frames + game_end, then the Close.
            for _ in 0..12 {
                // One message per 300ms: total ~3s (drain_stall = 1s),
                // progress every 300ms (< the window).
                tokio::time::sleep(Duration::from_millis(300)).await;
                use tokio_tungstenite::tungstenite::Message as Tm;
                let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                match message {
                    Tm::Text(text) => {
                        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if v["Type"] == "game_end" {
                            got_end = true;
                        }
                    }
                    Tm::Close(frame) => {
                        close_code = frame.map(|f| u16::from(f.code));
                        break;
                    }
                    _ => {}
                }
            }
            (got_end, close_code)
        };
        let mut log = Vec::new();
        let ((got_end, close_code), ()) = tokio::join!(client, srv.finish(&mut log));
        assert!(got_end, "the slow client still received game_end");
        assert_eq!(close_code, Some(1000));
    }

    /// B8: a tab that closed mid-replay must not hold `finish` open to
    /// the stall window (the `ended && active == 0` path).
    #[tokio::test]
    async fn closed_tab_does_not_wait_out_the_stall() {
        let mut srv = launch_server(Some(board_game(&config("standard"), "b8"))).await;
        for turn in 0..10 {
            append_frame(&mut srv, turn);
        }
        // Client reads half the events, then drops its socket.
        let url = events_url(&srv);
        let mut ws = ws_connect(&url).await;
        for turn in 0..5 {
            assert_eq!(next_json(&mut ws).await["Data"]["Turn"], turn);
        }
        drop(ws);
        srv.send_game_end().unwrap();

        let started = Instant::now();
        let mut log = Vec::new();
        srv.finish(&mut log).await;
        // drain_stall is 1s here; if the closed-tab path were broken,
        // finish would wait the full window (plus): keep the bound
        // comfortably under it.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "closed tab held finish for {:?}",
            started.elapsed()
        );
    }

    /// B9: a vanished (reloading) client does not count as finished —
    /// client B still gets its full stream including game_end.
    #[tokio::test]
    async fn vanished_client_does_not_count_as_finished() {
        // drain_stall long enough that only A's-send-failure path (not the
        // stall clock) can let B complete... actually the load-bearing
        // assert is the direct `any_finished` check below; keep the stall
        // above B's total read time (~1.5 s of 150 ms reads).
        let opts = BrowserOptions {
            open_browser: false,
            connect_grace: Duration::from_secs(1),
            drain_stall: Duration::from_secs(10),
            ..BrowserOptions::default()
        };
        let mut log = Vec::new();
        let mut srv = BoardServer::launch(board_game(&config("standard"), "b9"), &opts, &mut log)
            .await
            .expect("launch");
        for turn in 0..6 {
            append_frame(&mut srv, turn);
        }
        // A connects, reads one event, drops.
        let url = events_url(&srv);
        let mut a = ws_connect(&url).await;
        assert_eq!(next_json(&mut a).await["Data"]["Turn"], 0);
        drop(a);

        // Keep appending until the handler notices A is gone (only a
        // later send surfaces the dead socket) — private state visible.
        let mut turn = 6;
        let deadline = Instant::now() + Duration::from_secs(10);
        while srv.conn_rx.borrow().active != 0 && Instant::now() < deadline {
            append_frame(&mut srv, turn);
            turn += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            srv.conn_rx.borrow().active,
            0,
            "client A never went inactive"
        );
        assert!(
            !srv.conn_rx.borrow().any_finished,
            "A must not count as finished"
        );

        // B connects BEFORE game_end (game still running), reads its
        // first frame, then game_end lands and finish runs concurrently:
        // A's vanish must not have set any_finished (which would make
        // finish return at once and cut B off mid-replay).
        let mut b = ws_connect(&url).await;
        assert_eq!(next_json(&mut b).await["Data"]["Turn"], 0);
        srv.send_game_end().unwrap();
        let client = async {
            let mut ws = b;
            let mut got_end = false;
            let mut close_code = None;
            // (frames appended while A drained) + game_end + Close.
            for _ in 0..60 {
                tokio::time::sleep(Duration::from_millis(150)).await;
                use tokio_tungstenite::tungstenite::Message as Tm;
                let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                match message {
                    Tm::Text(text) => {
                        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if v["Type"] == "game_end" {
                            got_end = true;
                        }
                    }
                    Tm::Close(frame) => {
                        close_code = frame.map(|f| u16::from(f.code));
                        break;
                    }
                    _ => {}
                }
            }
            (got_end, close_code)
        };
        let mut finish_log = Vec::new();
        let ((got_end, close_code), ()) = tokio::join!(client, srv.finish(&mut finish_log));
        assert!(got_end, "client B was cut off: no game_end");
        assert_eq!(close_code, Some(1000));
    }

    /// PR review round 1 pin: a caught-up client that went away while
    /// parked on `changed()` (a reloaded or closed tab — the browser sends
    /// Close on navigation) must not count as finished when `game_end`
    /// lands. Its handler only notices the dead peer after writing
    /// `game_end` + Close into the dead socket, then `graceful_close`
    /// reads the buffered Close/error and the handler sets
    /// `any_finished` — so `finish` exits while the live tab (B) is still
    /// mid-stream and has not completed its close handshake. Contract:
    /// Design Decision 9 / coordinator edit 1 ("a tab that vanished
    /// mid-stream can't set any_finished").
    #[tokio::test]
    async fn parked_vanished_client_does_not_count_as_finished() {
        let mut srv = launch_server(Some(board_game(&config("standard"), "pin-r1"))).await;
        for turn in 0..3 {
            append_frame(&mut srv, turn);
        }
        let url = events_url(&srv);

        // A reads everything so far (caught up; its handler parks on
        // `changed()`), then leaves like a navigating tab: Close + drop.
        let mut a = ws_connect(&url).await;
        for turn in 0..3 {
            assert_eq!(next_json(&mut a).await["Data"]["Turn"], turn);
        }
        let _ = a.close(None).await;
        drop(a);
        tokio::time::sleep(Duration::from_millis(200)).await;

        // B (the live tab) connects but has not read anything yet, so it
        // cannot have completed a close handshake.
        let b = ws_connect(&url).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        srv.send_game_end().unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert!(
            !srv.conn_rx.borrow().any_finished,
            "a vanished tab counted as finished: finish would exit and cut off the live tab"
        );
        drop(b);
    }
}
