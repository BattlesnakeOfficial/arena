use std::collections::BTreeMap;

use crate::watched_games::{Readiness, WatchedGameUpdate};
use axum::{
    Json,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::IntoResponse,
};
use color_eyre::eyre::Context as _;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    errors::ServerResult,
    models::game::{GameStatus, GameType, get_game_by_id, get_game_source},
    models::game_battlesnake::get_battlesnakes_by_game_id,
    models::turn::{get_latest_frame, get_turn_frames_page, get_turns_by_game_id},
    state::AppState,
};

/// Snake-ID → owner public name map for filling in frame `Author` fields.
///
/// Frames persisted before authors were threaded through the game runner have
/// `Author: ""`, which the board viewer's scoreboard renders as a dangling
/// "by ". Frame snake IDs are `game_battlesnake_id` strings, so this joins
/// back to the owner at serve time. Games without `game_battlesnakes` rows
/// (archived imports) yield an empty map and enrichment is a no-op.
async fn frame_author_map(
    db: &sqlx::PgPool,
    game_id: Uuid,
) -> std::collections::HashMap<String, String> {
    match get_battlesnakes_by_game_id(db, game_id).await {
        Ok(snakes) => snakes
            .into_iter()
            .map(|bs| (bs.game_battlesnake_id.to_string(), bs.owner_name))
            .collect(),
        Err(e) => {
            // Author enrichment is cosmetic; never fail a frames request over it.
            tracing::warn!(error = ?e, %game_id, "Failed to load authors for frame enrichment");
            std::collections::HashMap::new()
        }
    }
}

/// Fill in missing/empty `Author` fields on a persisted frame's `Snakes`.
fn fill_frame_authors(
    frame: &mut serde_json::Value,
    authors: &std::collections::HashMap<String, String>,
) {
    if authors.is_empty() {
        return;
    }
    let Some(snakes) = frame.get_mut("Snakes").and_then(|s| s.as_array_mut()) else {
        return;
    };
    for snake in snakes {
        let has_author = snake
            .get("Author")
            .and_then(|a| a.as_str())
            .is_some_and(|a| !a.is_empty());
        if has_author {
            continue;
        }
        let Some(author) = snake
            .get("ID")
            .and_then(|id| id.as_str())
            .and_then(|id| authors.get(id))
        else {
            continue;
        };
        snake["Author"] = serde_json::Value::String(author.clone());
    }
}

/// Everything a persisted frame needs before it is served publicly: owner
/// names filled in, moderated shouts stripped, and the legacy engine's public
/// frame shape (see `normalize_public_frame`).
fn prepare_public_frame(
    frame: &mut serde_json::Value,
    authors: &std::collections::HashMap<String, String>,
    suppressed: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) {
    fill_frame_authors(frame, authors);
    crate::moderation::shouts::strip_suppressed_shouts(frame, suppressed);
    crate::engine::frame::normalize_public_frame(frame, crate::engine::MOVE_TIMEOUT_MS);
}

/// `GET /api/games/{id}`: the legacy engine's `GET /games/{id}` shape,
/// `{Game, LastFrame}` (sampled live from engine.battlesnake.com). Read by
/// the board viewer (Width/Height), the GIF exporter, and third-party tools
/// written against the engine.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardViewerGameResponse {
    pub game: BoardViewerGame,
    /// Latest persisted frame; omitted before turn 0 exists, like the engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_frame: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardViewerGame {
    /// Game ID — required by the GIF exporter, which echoes it back into
    /// the frames requests it makes.
    #[serde(rename = "ID")]
    pub id: String,
    /// Legacy-engine status string ("pending" | "running" | "complete").
    pub status: String,
    /// Lossless Arena status, additive to the legacy engine contract.
    pub arena_status: String,
    pub width: u32,
    pub height: u32,
    /// Rules settings as strings keyed by the Go rules param names
    /// (`name`, `foodSpawnChance`, `minimumFood`, `damagePerTurn`,
    /// `shrinkEveryNTurns`), like the engine's `Game.Ruleset` map.
    pub ruleset: BTreeMap<String, String>,
    pub ruleset_name: String,
    pub rules_stages: Vec<String>,
    pub map: String,
    pub source: String,
    pub snake_timeout: i64,
    pub max_turns: i32,
    /// Pre-placed spawns; arena games never have any.
    pub food_spawns: Vec<serde_json::Value>,
    pub hazard_spawns: Vec<serde_json::Value>,
    /// Unix microseconds as a string, like the engine.
    pub created: String,
}

/// The legacy engine's `Game.Ruleset`, `RulesetName`, `Map`, and
/// `RulesStages` for a game. Settings come from `engine::mode_rules` and the
/// public ruleset/map pair from `wire::wire_ruleset_and_map`, the same
/// sources as the snake request payload, so the API and what snakes are told
/// can't drift apart.
fn engine_rules(
    game_type: GameType,
    game_id: Uuid,
) -> (BTreeMap<String, String>, String, String, Vec<String>) {
    let (internal, settings, royale) = crate::engine::mode_rules(game_type, game_id);
    let (ruleset_name, map) = crate::wire::wire_ruleset_and_map(internal);

    let mut ruleset = BTreeMap::from([
        ("name".to_string(), ruleset_name.to_string()),
        (
            "foodSpawnChance".to_string(),
            settings.food_spawn_chance.to_string(),
        ),
        ("minimumFood".to_string(), settings.minimum_food.to_string()),
        (
            "damagePerTurn".to_string(),
            settings.hazard_damage_per_turn.to_string(),
        ),
    ]);
    if let Some(royale) = royale {
        ruleset.insert(
            "shrinkEveryNTurns".to_string(),
            royale.shrink_every_n_turns.to_string(),
        );
    }

    // Stage pipelines from play `core/game_stages.py`: the game-over stage
    // depends on the snake count (Solo is the single-snake mode), and only
    // Constrictor adds a stage to the standard pipeline; Royale's shrinking
    // and Snail Mode's trails came from their maps.
    let game_over = if internal == "solo" {
        "game_over.solo_snake"
    } else {
        "game_over.standard"
    };
    let mut stages = vec![
        game_over,
        "movement.standard",
        "starvation.standard",
        "hazard_damage.standard",
        "feed_snakes.standard",
    ];
    if ruleset_name == "constrictor" {
        stages.push("modify_snakes.always_grow");
    }
    stages.push("elimination.standard");

    (
        ruleset,
        ruleset_name.to_string(),
        map.to_string(),
        stages.into_iter().map(String::from).collect(),
    )
}

/// Map arena's game status to the legacy engine's status strings
/// ("pending", "running", "complete") that engine API consumers expect.
fn engine_status(status: GameStatus) -> &'static str {
    match status {
        GameStatus::Waiting => "pending",
        GameStatus::Running => "running",
        // "complete" for failed games too: the board's only concern is
        // whether more frames are coming, and they never are.
        GameStatus::Finished | GameStatus::Failed => "complete",
    }
}

/// GET /api/games/{id}
/// Returns game info for the Battlesnake board viewer and the GIF exporter
pub async fn get_game_info(
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let game = get_game_by_id(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch game")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Game not found"),
                StatusCode::NOT_FOUND,
            )
        })?;

    let (width, height) = game.board_size.dimensions();
    let (ruleset, ruleset_name, map, rules_stages) = engine_rules(game.game_type, game_id);
    let source = get_game_source(&state.db, game_id)
        .await
        .wrap_err("Failed to load game source")?;

    let last_frame = match get_latest_frame(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch latest frame")?
    {
        Some(mut frame) => {
            let authors = frame_author_map(&state.db, game_id).await;
            let suppressed =
                crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &game.status)
                    .await;
            prepare_public_frame(&mut frame, &authors, &suppressed);
            Some(frame)
        }
        None => None,
    };

    Ok(Json(BoardViewerGameResponse {
        game: BoardViewerGame {
            id: game.game_id.to_string(),
            status: engine_status(game.status).to_string(),
            arena_status: game.status.as_str().to_string(),
            width,
            height,
            ruleset,
            ruleset_name,
            rules_stages,
            map,
            source: source.as_str().to_string(),
            snake_timeout: crate::engine::MOVE_TIMEOUT_MS,
            max_turns: crate::engine::MAX_TURNS,
            food_spawns: Vec::new(),
            hazard_spawns: Vec::new(),
            created: game.created_at.timestamp_micros().to_string(),
        },
        last_frame,
    }))
}

/// Default and maximum page size for the frames endpoint.
///
/// The GIF exporter (github.com/BattlesnakeOfficial/exporter) fetches frames
/// in batches of exactly 100 and treats a short page as "no more frames", so
/// the cap must be >= its batch size. Games can have up to ~5000 turns —
/// this cap (applied in SQL) is what keeps the endpoint bounded.
const MAX_FRAMES_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct FramesQuery {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

/// Clamp pagination params to sane, non-negative, bounded values.
/// Matches the legacy engine's semantics: missing limit defaults to the max.
fn clamp_frames_pagination(offset: Option<i64>, limit: Option<i64>) -> (i64, i64) {
    let offset = offset.unwrap_or(0).max(0);
    let limit = limit.unwrap_or(MAX_FRAMES_LIMIT).clamp(0, MAX_FRAMES_LIMIT);
    (offset, limit)
}

/// Engine-compatible frames list envelope: `{"Count", "Frames"}`, PascalCase
/// like the rest of the legacy engine API (verified live against
/// engine.battlesnake.com). `Count` is the number of frames in this page. The
/// GIF exporter's lowercase `count`/`frames` tags still match because Go's
/// JSON decoding is case-insensitive; JS/Python clients are not.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GameFramesResponse {
    pub count: usize,
    pub frames: Vec<serde_json::Value>,
}

/// GET /api/games/{id}/frames?offset=&limit=
///
/// Engine-compatible paginated frame history, served from the `turns` table.
/// Each frame is the same PascalCase JSON blob the websocket path streams
/// (`turns.frame_data`, produced by `engine::frame::game_to_frame`).
/// Public: game data is public, matching the legacy engine.
pub async fn get_game_frames(
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
    Query(query): Query<FramesQuery>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // 404 for unknown games, like the legacy engine (the exporter maps this
    // through to its own 404).
    let game = get_game_by_id(&state.db, game_id)
        .await
        .wrap_err("Failed to fetch game")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Game not found"),
                StatusCode::NOT_FOUND,
            )
        })?;

    let (offset, limit) = clamp_frames_pagination(query.offset, query.limit);

    let turns = get_turn_frames_page(&state.db, game_id, offset, limit)
        .await
        .wrap_err("Failed to fetch turn frames")?;

    let authors = frame_author_map(&state.db, game_id).await;
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &game.status).await;
    let mut frames: Vec<serde_json::Value> =
        turns.into_iter().filter_map(|t| t.frame_data).collect();
    for frame in &mut frames {
        prepare_public_frame(frame, &authors, &suppressed);
    }

    Ok(Json(GameFramesResponse {
        count: frames.len(),
        frames,
    }))
}

/// WebSocket message types for the board viewer
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct WebSocketMessage {
    #[serde(rename = "Type")]
    pub message_type: String,
    #[serde(rename = "Data")]
    pub data: serde_json::Value,
}

/// GET /api/games/{id}/events
/// WebSocket endpoint for streaming game frames
pub async fn game_events_websocket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(game_id): Path<Uuid>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_game_websocket(socket, state, game_id))
}

/// Send a WebSocket close frame and wait for the client to acknowledge.
///
/// The board viewer uses ReconnectingWebSocket which auto-reconnects on any
/// server-initiated close. If we just drop the socket (by returning), the TCP
/// connection resets before the client processes buffered messages like game_end.
/// By sending a proper Close frame and waiting for the client's response, we give
/// the client time to process all messages and close its side first.
async fn graceful_close(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    receiver: &mut futures::stream::SplitStream<WebSocket>,
) {
    let _ = sender.send(Message::Close(None)).await;
    // Drain until the client sends Close back or the connection drops
    while let Some(msg) = receiver.next().await {
        if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
            break;
        }
    }
}

type WsSender = futures::stream::SplitSink<WebSocket, Message>;
type WsReceiver = futures::stream::SplitStream<WebSocket>;

struct FrameContext<'a> {
    authors: &'a std::collections::HashMap<String, String>,
    suppressed: &'a std::collections::HashMap<String, std::collections::HashSet<String>>,
}

async fn send_message(sender: &mut WsSender, kind: &str, data: serde_json::Value) -> bool {
    let message = WebSocketMessage {
        message_type: kind.to_string(),
        data,
    };
    sender
        .send(Message::Text(
            serde_json::to_string(&message).unwrap().into(),
        ))
        .await
        .is_ok()
}

async fn send_error_and_close(sender: &mut WsSender, receiver: &mut WsReceiver, message: &str) {
    let _ = send_message(sender, "error", serde_json::json!({"message": message})).await;
    graceful_close(sender, receiver).await;
}

async fn send_frames(
    sender: &mut WsSender,
    turns: &[crate::models::turn::Turn],
    last_sent_turn: &mut i32,
    context: &FrameContext<'_>,
) -> bool {
    for turn in turns {
        if turn.turn_number <= *last_sent_turn {
            continue;
        }
        if let Some(mut frame) = turn.frame_data.clone() {
            prepare_public_frame(&mut frame, context.authors, context.suppressed);
            if !send_message(sender, "frame", frame).await {
                return false;
            }
            *last_sent_turn = turn.turn_number;
        }
    }
    true
}

async fn send_terminal(
    sender: &mut WsSender,
    receiver: &mut WsReceiver,
    state: &AppState,
    game_id: Uuid,
    status: GameStatus,
    last_sent_turn: &mut i32,
    context: &FrameContext<'_>,
) {
    let turns = match crate::models::turn::get_turns_from(
        &state.db,
        game_id,
        last_sent_turn.saturating_add(1),
    )
    .await
    {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed terminal frame drain");
            send_error_and_close(sender, receiver, "Failed to fetch game frames").await;
            return;
        }
    };
    if !send_frames(sender, &turns, last_sent_turn, context).await {
        return;
    }
    match status {
        GameStatus::Finished => {
            let _ = send_message(sender, "game_end", serde_json::json!({})).await;
        }
        GameStatus::Failed => {
            let _ = send_message(
                sender,
                "error",
                serde_json::json!({"message":"Game failed"}),
            )
            .await;
            // The board stops its reconnecting socket only on game_end.
            let _ = send_message(sender, "game_end", serde_json::json!({})).await;
        }
        _ => {
            send_error_and_close(sender, receiver, "Game changed, please reconnect").await;
            return;
        }
    }
    graceful_close(sender, receiver).await;
}

async fn handle_game_websocket(socket: WebSocket, state: AppState, game_id: Uuid) {
    let (mut sender, mut receiver) = socket.split();
    let game = match get_game_by_id(&state.db, game_id).await {
        Ok(Some(game)) => game,
        Ok(None) => {
            send_error_and_close(&mut sender, &mut receiver, "Game not found").await;
            return;
        }
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed to fetch game for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Internal server error").await;
            return;
        }
    };
    let mut subscription = state.watched_games.subscribe(game_id).await;
    let mut retired_before_history = false;
    if !subscription.created {
        loop {
            let readiness = *subscription.readiness.borrow_and_update();
            match readiness {
                Readiness::Active { .. } => break,
                Readiness::Retired => {
                    // A terminal update may already be queued. Read history and
                    // authoritative status before deciding how to close.
                    retired_before_history = true;
                    break;
                }
                Readiness::Pending => {}
            }
            // Subscription owns the entry and therefore its watch sender.
            let _ = subscription.readiness.changed().await;
        }
    }
    let existing_turns = match get_turns_by_game_id(&state.db, game_id).await {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed to fetch turns for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Failed to fetch game frames").await;
            return;
        }
    };
    if subscription.created {
        let last = existing_turns
            .iter()
            .rev()
            .find(|turn| turn.frame_data.is_some());
        state
            .watched_games
            .seed_with_turn_id(
                game_id,
                &mut subscription,
                last.map_or(-1, |turn| turn.turn_number),
                last.map(|turn| turn.turn_id),
            )
            .await;
    }
    let authors = frame_author_map(&state.db, game_id).await;
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &game.status).await;
    let context = FrameContext {
        authors: &authors,
        suppressed: &suppressed,
    };
    let mut last_sent_turn = -1;
    if !send_frames(&mut sender, &existing_turns, &mut last_sent_turn, &context).await {
        return;
    }
    match get_game_by_id(&state.db, game_id).await {
        Ok(Some(game)) if matches!(game.status, GameStatus::Finished | GameStatus::Failed) => {
            send_terminal(
                &mut sender,
                &mut receiver,
                &state,
                game_id,
                game.status,
                &mut last_sent_turn,
                &context,
            )
            .await;
            return;
        }
        Ok(Some(_)) if retired_before_history => {
            send_error_and_close(&mut sender, &mut receiver, "Game changed, please reconnect")
                .await;
            return;
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            send_error_and_close(&mut sender, &mut receiver, "Game not found").await;
            return;
        }
        Err(error) => {
            tracing::error!(%game_id, error = %format_args!("{error:#}"), "Failed to reread game for WebSocket");
            send_error_and_close(&mut sender, &mut receiver, "Internal server error").await;
            return;
        }
    }
    loop {
        tokio::select! {
            msg = receiver.next() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Ping(data))) => { if sender.send(Message::Pong(data)).await.is_err() { return; } }
                Some(Ok(_)) => {}
            },
            update = subscription.updates.recv() => match update {
                Ok(WatchedGameUpdate::Frames(turns)) => {
                    if !send_frames(&mut sender, &turns, &mut last_sent_turn, &context).await { return; }
                }
                Ok(WatchedGameUpdate::Terminal(status)) => {
                    send_terminal(&mut sender, &mut receiver, &state, game_id, status, &mut last_sent_turn, &context).await;
                    return;
                }
                Ok(WatchedGameUpdate::Missing) => {
                    send_error_and_close(&mut sender, &mut receiver, "Game not found").await;
                    return;
                }
                Ok(WatchedGameUpdate::Reset) => {
                    send_error_and_close(&mut sender, &mut receiver, "Game restarted, please reconnect").await;
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(%game_id, lagged = count, "WebSocket lagged");
                    send_error_and_close(&mut sender, &mut receiver, "Connection lagged, please reconnect").await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Subscription owns the broadcast sender, so this is unreachable
                    // during normal operation. Close once if that invariant changes.
                    send_error_and_close(&mut sender, &mut receiver, "Game changed, please reconnect").await;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_game(status: GameStatus, game_type: GameType) -> BoardViewerGame {
        let game_id = Uuid::nil();
        let (ruleset, ruleset_name, map, rules_stages) = engine_rules(game_type, game_id);
        BoardViewerGame {
            id: "abc-123".to_string(),
            status: engine_status(status).to_string(),
            arena_status: status.as_str().to_string(),
            width: 11,
            height: 11,
            ruleset,
            ruleset_name,
            rules_stages,
            map,
            source: "arena".to_string(),
            snake_timeout: crate::engine::MOVE_TIMEOUT_MS,
            max_turns: crate::engine::MAX_TURNS,
            food_spawns: Vec::new(),
            hazard_spawns: Vec::new(),
            created: "1790859473028997".to_string(),
        }
    }

    /// `Game` carries every key of the legacy engine's `GET /games/{id}`
    /// (sampled live from engine.battlesnake.com) plus arena's additive
    /// `ArenaStatus`; `LastFrame` is omitted until a frame exists.
    #[test]
    fn test_board_viewer_response_matches_legacy_engine_shape() {
        let response = BoardViewerGameResponse {
            game: sample_game(GameStatus::Finished, GameType::Standard),
            last_frame: None,
        };

        let json = serde_json::to_value(&response).unwrap();
        let mut top: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        top.sort();
        assert_eq!(top, ["Game"]);
        let mut game: Vec<&str> = json["Game"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        game.sort();
        assert_eq!(
            game,
            [
                "ArenaStatus",
                "Created",
                "FoodSpawns",
                "HazardSpawns",
                "Height",
                "ID",
                "Map",
                "MaxTurns",
                "RulesStages",
                "Ruleset",
                "RulesetName",
                "SnakeTimeout",
                "Source",
                "Status",
                "Width",
            ]
        );
        assert_eq!(json["Game"]["Created"], "1790859473028997");
        assert_eq!(json["Game"]["SnakeTimeout"], 500);

        let with_frame = BoardViewerGameResponse {
            game: sample_game(GameStatus::Running, GameType::Standard),
            last_frame: Some(serde_json::json!({"Turn": 7})),
        };
        let json = serde_json::to_value(&with_frame).unwrap();
        assert_eq!(json["LastFrame"]["Turn"], 7);
    }

    /// Ruleset/map/stages match what play.battlesnake.com created for each
    /// mode (play `ui/maps.py`, `leaderboards_setup.py`, `game_stages.py`),
    /// with arena's real settings values.
    #[test]
    fn test_engine_rules_match_play_per_mode() {
        let standard_pipeline = [
            "movement.standard",
            "starvation.standard",
            "hazard_damage.standard",
            "feed_snakes.standard",
            "elimination.standard",
        ];
        let with_game_over = |game_over: &str| {
            std::iter::once(game_over.to_string())
                .chain(standard_pipeline.iter().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        };

        let (ruleset, name, map, stages) = engine_rules(GameType::Standard, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "standard"));
        assert_eq!(ruleset["name"], "standard");
        assert_eq!(ruleset["foodSpawnChance"], "15");
        assert_eq!(ruleset["minimumFood"], "1");
        assert!(!ruleset.contains_key("shrinkEveryNTurns"));
        assert_eq!(stages, with_game_over("game_over.standard"));

        let (ruleset, name, map, stages) = engine_rules(GameType::Royale, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "royale"));
        assert_eq!(ruleset["name"], "standard");
        assert_eq!(ruleset["damagePerTurn"], "14");
        assert_eq!(ruleset["shrinkEveryNTurns"], "25");
        assert_eq!(stages, with_game_over("game_over.standard"));

        let (ruleset, name, map, stages) = engine_rules(GameType::Constrictor, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("constrictor", "empty"));
        assert_eq!(ruleset["name"], "constrictor");
        assert_eq!(
            stages,
            [
                "game_over.standard",
                "movement.standard",
                "starvation.standard",
                "hazard_damage.standard",
                "feed_snakes.standard",
                "modify_snakes.always_grow",
                "elimination.standard",
            ]
        );

        let (_, name, map, stages) = engine_rules(GameType::SnailMode, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "snail_mode"));
        assert_eq!(stages, with_game_over("game_over.standard"));

        let (_, name, map, stages) = engine_rules(GameType::Solo, Uuid::nil());
        assert_eq!((name.as_str(), map.as_str()), ("standard", "standard"));
        assert_eq!(stages, with_game_over("game_over.solo_snake"));
    }

    #[test]
    fn test_engine_status_mapping() {
        assert_eq!(engine_status(GameStatus::Waiting), "pending");
        assert_eq!(engine_status(GameStatus::Running), "running");
        assert_eq!(engine_status(GameStatus::Finished), "complete");
        assert_eq!(engine_status(GameStatus::Failed), "complete");
    }

    #[test]
    fn arena_status_serializes_all_lossless_states() {
        for (status, expected) in [
            (GameStatus::Waiting, "waiting"),
            (GameStatus::Running, "running"),
            (GameStatus::Finished, "finished"),
            (GameStatus::Failed, "failed"),
        ] {
            let response = BoardViewerGameResponse {
                game: sample_game(status, GameType::Standard),
                last_frame: None,
            };
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value["Game"]["ArenaStatus"], expected);
            assert_eq!(
                value["Game"]["Status"],
                if matches!(status, GameStatus::Finished | GameStatus::Failed) {
                    "complete"
                } else {
                    engine_status(status)
                }
            );
        }
    }

    #[test]
    fn test_frames_response_serialization() {
        // Engine envelope: PascalCase Count/Frames wrapping the frame blobs,
        // exactly like engine.battlesnake.com.
        let response = GameFramesResponse {
            count: 1,
            frames: vec![serde_json::json!({
                "Turn": 0,
                "Snakes": [],
                "Food": [{"X": 1, "Y": 2}],
                "Hazards": [],
            })],
        };

        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            json,
            r#"{"Count":1,"Frames":[{"Food":[{"X":1,"Y":2}],"Hazards":[],"Snakes":[],"Turn":0}]}"#
        );
    }

    #[test]
    fn test_clamp_frames_pagination_defaults() {
        assert_eq!(clamp_frames_pagination(None, None), (0, MAX_FRAMES_LIMIT));
    }

    #[test]
    fn test_clamp_frames_pagination_clamps_limit_to_max() {
        assert_eq!(
            clamp_frames_pagination(Some(200), Some(5000)),
            (200, MAX_FRAMES_LIMIT)
        );
    }

    #[test]
    fn test_clamp_frames_pagination_rejects_negatives() {
        assert_eq!(clamp_frames_pagination(Some(-5), Some(-10)), (0, 0));
    }

    #[test]
    fn test_clamp_frames_pagination_passes_through_valid_values() {
        assert_eq!(clamp_frames_pagination(Some(300), Some(50)), (300, 50));
    }

    #[test]
    fn test_websocket_message_serialization() {
        let msg = WebSocketMessage {
            message_type: "frame".to_string(),
            data: serde_json::json!({"Turn": 5, "Snakes": []}),
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"Type\":\"frame\""));
        assert!(json.contains("\"Data\""));
    }

    use sqlx::PgPool;

    /// Insert a bare game row (no snakes) with the given status.
    async fn fixture_game(pool: &PgPool, status: &str) -> cja::Result<Uuid> {
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', $1) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await?;
        Ok(game_id)
    }

    async fn fixture_turn(
        pool: &PgPool,
        game_id: Uuid,
        turn_number: i32,
        frame_data: Option<serde_json::Value>,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(turn_number)
            .bind(frame_data)
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// `GET /api/games/{id}` serves the legacy engine's `{Game, LastFrame}`:
    /// a ladder game reports `Source: "arena"`, `LastFrame` is the latest
    /// turn, and a frame persisted before DEV-1502 (Latency "timeout", no
    /// engine fields) is served in the engine's shape.
    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_serves_engine_game_and_last_frame(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('info') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
            .bind(leaderboard_id)
            .bind(game_id)
            .execute(&pool)
            .await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            Some(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            1,
            Some(serde_json::json!({
                "Turn": 1,
                "Snakes": [{"ID": "s1", "Latency": "timeout", "APIVersion": "1"}],
                "Food": [],
                "Hazards": []
            })),
        )
        .await?;

        let response = get_game_info(State(state), Path(game_id))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        let game = &json["Game"];
        assert_eq!(game["Status"], "complete");
        assert_eq!(game["Source"], "arena");
        assert_eq!(game["RulesetName"], "standard");
        assert_eq!(game["Map"], "standard");
        assert_eq!(game["Ruleset"]["name"], "standard");
        assert_eq!(game["SnakeTimeout"], 500);
        assert!(
            game["Created"]
                .as_str()
                .is_some_and(|c| c.parse::<i64>().is_ok())
        );

        let last = &json["LastFrame"];
        assert_eq!(last["Turn"], 1);
        assert_eq!(last["PointState"], serde_json::json!([]));
        let snake = &last["Snakes"][0];
        assert_eq!(snake["Latency"], "500");
        assert_eq!(snake["Error"], crate::engine::frame::TIMEOUT_ERROR);
        assert_eq!(snake["APIVersion"], "");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_omits_last_frame_before_turn_zero(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "waiting").await?;

        let response = get_game_info(State(state), Path(game_id))
            .await
            .unwrap()
            .into_response();
        let json = response_json(response).await;

        assert_eq!(json["Game"]["Status"], "pending");
        assert_eq!(json["Game"]["Source"], "custom");
        assert!(json.get("LastFrame").is_none());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_returns_engine_envelope(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        // Insert out of order to prove ordering comes from SQL, plus a
        // NULL-frame turn that must be filtered out.
        fixture_turn(
            &pool,
            game_id,
            1,
            Some(serde_json::json!({"Turn": 1, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            Some(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;
        fixture_turn(&pool, game_id, 2, None).await?;

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await
        .unwrap()
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        assert_eq!(json["Count"], 2);
        let frames = json["Frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["Turn"], 0);
        assert_eq!(frames[1]["Turn"], 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_paginates_with_offset_and_limit(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        for turn in 0..5 {
            fixture_turn(
                &pool,
                game_id,
                turn,
                Some(serde_json::json!({"Turn": turn, "Snakes": [], "Food": [], "Hazards": []})),
            )
            .await?;
        }

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: Some(2),
                limit: Some(2),
            }),
        )
        .await
        .unwrap()
        .into_response();

        let json = response_json(response).await;
        assert_eq!(json["Count"], 2);
        assert_eq!(json["Frames"][0]["Turn"], 2);
        assert_eq!(json["Frames"][1]["Turn"], 3);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_offset_past_end_returns_empty(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;
        fixture_turn(
            &pool,
            game_id,
            0,
            Some(serde_json::json!({"Turn": 0, "Snakes": [], "Food": [], "Hazards": []})),
        )
        .await?;

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: Some(100),
                limit: Some(100),
            }),
        )
        .await
        .unwrap()
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["Count"], 0);
        assert_eq!(json["Frames"].as_array().unwrap().len(), 0);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_unknown_game_is_404(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool);

        let result = get_game_frames(
            State(state),
            Path(Uuid::new_v4()),
            Query(FramesQuery {
                offset: None,
                limit: None,
            }),
        )
        .await;

        let response = result.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    /// A finished Solo game's frames flow through the unchanged endpoint
    /// with the full progression: ordered from turn 0 through the final
    /// death frame, with the death cause present on the last frame only.
    #[sqlx::test(migrations = "../migrations")]
    async fn frames_endpoint_serves_full_solo_progression(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Solo', 'finished') RETURNING game_id",
        )
        .fetch_one(&pool)
        .await?;

        let solo_frame = |turn: i32, death: Option<&str>| {
            serde_json::json!({
                "Turn": turn,
                "Snakes": [{
                    "ID": "snake-1",
                    "Death": death.map(|c| serde_json::json!({"Cause": c, "Turn": turn})),
                }],
                "Food": [],
                "Hazards": [],
            })
        };
        for turn in 0..3 {
            fixture_turn(&pool, game_id, turn, Some(solo_frame(turn, None))).await?;
        }
        fixture_turn(
            &pool,
            game_id,
            3,
            Some(solo_frame(3, Some("out-of-health"))),
        )
        .await?;

        let response = get_game_frames(
            State(state),
            Path(game_id),
            Query(FramesQuery {
                offset: Some(0),
                limit: Some(100),
            }),
        )
        .await
        .unwrap()
        .into_response();

        let json = response_json(response).await;
        let frames = json["Frames"].as_array().unwrap();
        assert_eq!(json["Count"], 4);
        assert_eq!(frames[0]["Turn"], 0, "progression starts at turn 0");
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame["Turn"], i, "frames stay ordered");
        }
        let last = frames.last().unwrap();
        assert_eq!(last["Snakes"][0]["Death"]["Cause"], "out-of-health");
        for frame in &frames[..frames.len() - 1] {
            assert!(
                frame["Snakes"][0]["Death"].is_null(),
                "no death before the end"
            );
        }

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn game_info_includes_id_and_status(pool: PgPool) -> cja::Result<()> {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        let game_id = fixture_game(&pool, "finished").await?;

        let response = get_game_info(State(state), Path(game_id))
            .await
            .unwrap()
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;
        assert_eq!(json["Game"]["ID"], game_id.to_string());
        assert_eq!(json["Game"]["Status"], "complete");
        assert_eq!(json["Game"]["Width"], 11);
        assert_eq!(json["Game"]["Height"], 11);

        Ok(())
    }

    #[test]
    fn fill_frame_authors_fills_empty_and_missing_only() {
        let mut authors = std::collections::HashMap::new();
        authors.insert("gb-1".to_string(), "corey".to_string());
        authors.insert("gb-2".to_string(), "brandi".to_string());

        let mut frame = serde_json::json!({
            "Turn": 3,
            "Snakes": [
                {"ID": "gb-1", "Author": ""},          // legacy empty → filled
                {"ID": "gb-2"},                          // missing → filled
                {"ID": "gb-3", "Author": ""},          // unknown ID → untouched
                {"ID": "gb-1", "Author": "already"},   // present → preserved
            ]
        });
        fill_frame_authors(&mut frame, &authors);

        assert_eq!(frame["Snakes"][0]["Author"], "corey");
        assert_eq!(frame["Snakes"][1]["Author"], "brandi");
        assert_eq!(frame["Snakes"][2]["Author"], "");
        assert_eq!(frame["Snakes"][3]["Author"], "already");
    }

    #[test]
    fn fill_frame_authors_tolerates_shapeless_frames() {
        let mut authors = std::collections::HashMap::new();
        authors.insert("gb-1".to_string(), "corey".to_string());

        // No Snakes key, wrong type, empty map: all must be silent no-ops.
        let mut no_snakes = serde_json::json!({"Turn": 0});
        fill_frame_authors(&mut no_snakes, &authors);
        assert_eq!(no_snakes, serde_json::json!({"Turn": 0}));

        let mut wrong_type = serde_json::json!({"Snakes": "nope"});
        fill_frame_authors(&mut wrong_type, &authors);
        assert_eq!(wrong_type, serde_json::json!({"Snakes": "nope"}));

        let mut frame = serde_json::json!({"Snakes": [{"ID": "gb-1"}]});
        fill_frame_authors(&mut frame, &std::collections::HashMap::new());
        assert_eq!(frame, serde_json::json!({"Snakes": [{"ID": "gb-1"}]}));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::watched_games::run_watched_games;
    use futures::StreamExt;
    use sqlx::PgPool;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn start(state: AppState) -> (String, CancellationToken, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let web_shutdown = shutdown.clone();
        let web = tokio::spawn(async move {
            axum::serve(listener, crate::routes::routes(state.clone()))
                .with_graceful_shutdown(web_shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        (format!("ws://{address}"), shutdown, web)
    }

    async fn connect(base: &str, id: Uuid) -> Client {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(format!("{base}/api/games/{id}/events")),
        )
        .await
        .unwrap()
        .unwrap()
        .0
    }

    async fn next(client: &mut Client) -> serde_json::Value {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            panic!("Expected text: {message:?}");
        };
        serde_json::from_str(&text).unwrap()
    }

    async fn close(client: &mut Client) {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            message,
            tokio_tungstenite::tungstenite::Message::Close(_)
        ));
        let _ = client.close(None).await;
    }

    async fn game(pool: &PgPool) -> Uuid {
        sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Solo', 'running') RETURNING game_id")
            .fetch_one(pool).await.unwrap()
    }

    async fn turn(pool: &PgPool, _state: &AppState, id: Uuid, number: i32) {
        crate::models::turn::create_turn(
            pool,
            id,
            number,
            Some(serde_json::json!({"Turn": number})),
        )
        .await
        .unwrap();
    }

    async fn wait_active(watched: &crate::watched_games::WatchedGames, id: Uuid) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !watched.is_active(id).await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn outage_admin(pool: &PgPool) -> (PgPool, String) {
        let db_name = sqlx::query_scalar!("SELECT current_database()")
            .fetch_one(pool)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(db_name, "postgres");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone().database("postgres"))
            .await
            .unwrap();
        (admin, db_name.replace('"', "\"\""))
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn different_process_receives_every_frame_and_finish(pool: PgPool) {
        let id = game(&pool).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        assert!(!reader.watched_games.same_registry(&writer.watched_games));
        let poll_shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            poll_shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        for number in 0..8 {
            turn(&pool, &writer, id, number).await;
        }
        for number in 0..8 {
            let message = next(&mut client).await;
            assert_eq!(message["Type"], "frame");
            assert_eq!(message["Data"]["Turn"], number);
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            next(&mut client).await,
            serde_json::json!({"Type":"game_end","Data":{}})
        );
        close(&mut client).await;
        poll_shutdown.cancel();
        web_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn different_process_failure_and_reconnect(pool: PgPool) {
        let id = game(&pool).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        turn(&pool, &writer, id, 0).await;
        sqlx::query!("UPDATE games SET status = 'failed' WHERE game_id = $1", id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(next(&mut client).await["Data"]["Turn"], 0);
        assert_eq!(
            next(&mut client).await,
            serde_json::json!({"Type":"error","Data":{"message":"Game failed"}})
        );
        assert_eq!(
            next(&mut client).await,
            serde_json::json!({"Type":"game_end","Data":{}})
        );
        close(&mut client).await;
        let mut replay = connect(&base, id).await;
        assert_eq!(next(&mut replay).await["Data"]["Turn"], 0);
        assert_eq!(next(&mut replay).await["Data"]["message"], "Game failed");
        assert_eq!(next(&mut replay).await["Type"], "game_end");
        close(&mut replay).await;
        shutdown.cancel();
        web_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn terminal_with_stale_cursor_drains_frames_before_game_end(pool: PgPool) {
        use axum::{Router, extract::WebSocketUpgrade, routing::get};

        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        for number in 0..3 {
            turn(&pool, &state, id, number).await;
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/stale",
            get(move |upgrade: WebSocketUpgrade| {
                let state = state.clone();
                async move {
                    upgrade.on_upgrade(move |socket| async move {
                        let (mut sender, mut receiver) = socket.split();
                        let authors = std::collections::HashMap::new();
                        let suppressed = std::collections::HashMap::new();
                        let context = FrameContext {
                            authors: &authors,
                            suppressed: &suppressed,
                        };
                        let mut cursor = -1;
                        send_terminal(
                            &mut sender,
                            &mut receiver,
                            &state,
                            id,
                            GameStatus::Finished,
                            &mut cursor,
                            &context,
                        )
                        .await;
                    })
                }
            }),
        );
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await
                .unwrap()
        });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stale"))
            .await
            .unwrap();
        for number in 0..3 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        shutdown.cancel();
        server.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn same_process_reconnect_replays_then_streams(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut first = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        turn(&pool, &state, id, 0).await;
        let persisted_at = std::time::Instant::now();
        assert_eq!(next(&mut first).await["Data"]["Turn"], 0);
        assert!(persisted_at.elapsed() < Duration::from_millis(500));
        first.close(None).await.unwrap();
        turn(&pool, &state, id, 1).await;
        let mut second = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        for number in 0..2 {
            assert_eq!(next(&mut second).await["Data"]["Turn"], number);
        }
        turn(&pool, &state, id, 2).await;
        assert_eq!(next(&mut second).await["Data"]["Turn"], 2);
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(next(&mut second).await["Type"], "game_end");
        close(&mut second).await;
        shutdown.cancel();
        web_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn listener_disconnect_catches_up_all_frames(pool: PgPool) {
        let id = game(&pool).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        for number in 0..3 {
            turn(&pool, &writer, id, number).await;
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        let terminated = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = sqlx::query_scalar!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'arena-watched-games-listener' AND pid <> pg_backend_pid() LIMIT 1"
                ).fetch_optional(&pool).await.unwrap();
                if result.is_some() { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        terminated.unwrap();
        for number in 3..8 {
            turn(&pool, &writer, id, number).await;
        }
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            id
        )
        .execute(&pool)
        .await
        .unwrap();
        for number in 3..8 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while reader.watched_games.reconnect_catchups() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failed_listener_rebuild_catches_up_terminal(pool: PgPool) {
        let id = game(&pool).await;
        let reader = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            reader.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(reader.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&reader.watched_games, id).await;
        turn(&pool, &reader, id, 0).await;
        assert_eq!(next(&mut client).await["Data"]["Turn"], 0);

        // Keep write and admin sessions open while new connections are denied.
        let (admin_pool, quoted_name) = outage_admin(&pool).await;
        let mut admin = admin_pool.acquire().await.unwrap();
        let mut writer = pool.acquire().await.unwrap();
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS false"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        let outage = async {
            sqlx::query!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'arena-watched-games-listener' AND pid <> pg_backend_pid() LIMIT 1"
            )
            .fetch_one(&mut *writer)
            .await?;
            for number in 1..8 {
                sqlx::query!(
                    "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)",
                    id,
                    number,
                    serde_json::json!({"Turn": number}),
                )
                .execute(&mut *writer)
                .await?;
            }
            sqlx::query!("UPDATE games SET status = 'finished' WHERE game_id = $1", id)
                .execute(&mut *writer)
                .await?;
            tokio::time::timeout(Duration::from_secs(5), async {
                while reader.watched_games.listener_errors() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok::<(), color_eyre::Report>(())
        }
        .await;
        // Restore even when an outage operation fails. No assertions occur above.
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS true"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        outage.unwrap();
        for number in 1..8 {
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        assert_eq!(next(&mut client).await["Type"], "game_end");
        close(&mut client).await;
        assert!(reader.watched_games.listener_errors() > 0);
        assert!(reader.watched_games.reconnect_catchups() > 0);
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn listener_outage_rewind_and_regrow_closes_old_stream(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        for number in 0..6 {
            turn(&pool, &state, id, number).await;
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        let (admin_pool, quoted_name) = outage_admin(&pool).await;
        let mut admin = admin_pool.acquire().await.unwrap();
        let mut writer = pool.acquire().await.unwrap();
        let spare = pool.acquire().await.unwrap();
        drop(spare);
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS false"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        let outage = async {
            sqlx::query!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND application_name = 'arena-watched-games-listener' AND pid <> pg_backend_pid() LIMIT 1"
            )
            .fetch_one(&mut *writer)
            .await?;
            crate::models::game::reset_game_state_for_retry(&pool, id).await?;
            for number in 0..8 {
                sqlx::query!(
                    "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)",
                    id,
                    number,
                    serde_json::json!({"Turn": number}),
                )
                .execute(&mut *writer)
                .await?;
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.watched_games.listener_errors() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            Ok::<(), color_eyre::Report>(())
        }
        .await;
        sqlx::query(&format!(
            "ALTER DATABASE \"{quoted_name}\" WITH ALLOW_CONNECTIONS true"
        ))
        .execute(&mut *admin)
        .await
        .unwrap();
        outage.unwrap();
        assert_eq!(
            next(&mut client).await["Data"]["message"],
            "Game restarted, please reconnect"
        );
        close(&mut client).await;
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn retry_reset_closes_old_stream_before_regrowth(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        for number in 0..6 {
            turn(&pool, &state, id, number).await;
            assert_eq!(next(&mut client).await["Data"]["Turn"], number);
        }
        crate::models::game::reset_game_state_for_retry(&pool, id)
            .await
            .unwrap();
        for number in 0..8 {
            turn(&pool, &state, id, number).await;
        }
        assert_eq!(
            next(&mut client).await["Data"]["message"],
            "Game restarted, please reconnect"
        );
        close(&mut client).await;
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rolled_back_turn_has_no_frame(pool: PgPool) {
        let id = game(&pool).await;
        let state = AppState::test_from_pool(pool.clone());
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            state.watched_games.clone(),
            shutdown.clone(),
        ));
        let (base, web_shutdown, web) = start(state.clone()).await;
        let mut client = connect(&base, id).await;
        wait_active(&state.watched_games, id).await;
        let mut transaction = pool.begin().await.unwrap();
        sqlx::query!(
            "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, 0, $2)",
            id,
            serde_json::json!({"Turn":0})
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        transaction.rollback().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), client.next())
                .await
                .is_err()
        );
        turn(&pool, &state, id, 0).await;
        assert_eq!(next(&mut client).await["Data"]["Turn"], 0);
        client.close(None).await.unwrap();
        shutdown.cancel();
        web_shutdown.cancel();
        listener.await.unwrap().unwrap();
        web.await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "controlled local load run: 500 sockets and a job worker"]
    async fn watched_games_local_load_harness(pool: PgPool) {
        use std::{collections::HashMap, sync::Arc, time::Instant};
        use tokio::sync::{Mutex, mpsc};
        let ids = futures::future::join_all((0..50).map(|_| game(&pool))).await;
        let writer = AppState::test_from_pool(pool.clone());
        let reader = AppState::test_from_pool(pool.clone());
        let watched = reader.watched_games.clone();
        let poll_shutdown = CancellationToken::new();
        let poller = tokio::spawn(run_watched_games(
            pool.clone(),
            pool.connect_options().as_ref().clone(),
            watched.clone(),
            poll_shutdown.clone(),
        ));
        let job_shutdown = CancellationToken::new();
        let job_config = reader.config.job.clone();
        let job_interval_ms = job_config.poll_interval_ms;
        let job_state = reader.clone();
        let job_cancel = job_shutdown.clone();
        let job_worker = tokio::spawn(async move {
            cja::jobs::worker::job_worker(
                job_state,
                crate::jobs::Jobs,
                Duration::from_millis(job_config.poll_interval_ms),
                job_config.max_retries,
                job_cancel,
                job_config.worker_config(Duration::from_secs(job_config.shutdown_drain_secs)),
            )
            .await
        });
        let (base, web_shutdown, web) = start(reader).await;
        let (tx, mut rx) = mpsc::unbounded_channel::<(Uuid, i32, Instant)>();
        let mut clients = tokio::task::JoinSet::new();
        for id in &ids {
            for _ in 0..10 {
                let mut socket = connect(&base, *id).await;
                let tx = tx.clone();
                let id = *id;
                clients.spawn(async move {
                    for expected in 0..2 {
                        let message = next(&mut socket).await;
                        assert_eq!(message["Type"], "frame");
                        let number = message["Data"]["Turn"].as_i64().unwrap() as i32;
                        assert_eq!(number, expected);
                        tx.send((id, number, Instant::now())).unwrap();
                    }
                    socket.close(None).await.unwrap();
                });
            }
        }
        drop(tx);
        tokio::time::timeout(Duration::from_secs(30), async {
            while watched.subscriber_count().await != 500 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let timestamps = Arc::new(Mutex::new(HashMap::<(Uuid, i32), Instant>::new()));
        let mut unwatched = 0;
        for number in 0..2 {
            for id in &ids {
                turn(&pool, &writer, *id, number).await;
                timestamps
                    .lock()
                    .await
                    .insert((*id, number), Instant::now());
                let other = game(&pool).await;
                turn(&pool, &writer, other, 0).await;
                unwatched += 1;
            }
        }
        let mut latencies = Vec::new();
        for _ in 0..1000 {
            let (id, number, received) = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let persisted = timestamps.lock().await[&(id, number)];
            latencies.push(received.saturating_duration_since(persisted));
        }
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
        latencies.sort();
        let p50 = latencies[latencies.len() / 2];
        let p95 = latencies[latencies.len() * 95 / 100];
        let (status_reads, range_reads) = watched.read_counts().await;
        println!(
            "samples={} p50={p50:?} p95={p95:?} status_reads={status_reads} range_reads={range_reads} watched_inserts=100 unwatched_inserts={unwatched} configured_job_claim_interval_ms={}",
            latencies.len(),
            job_interval_ms
        );
        assert!(p95 <= Duration::from_secs(1));
        poll_shutdown.cancel();
        web_shutdown.cancel();
        job_shutdown.cancel();
        poller.await.unwrap().unwrap();
        web.await.unwrap();
        job_worker.await.unwrap().unwrap();
    }
}
