use std::collections::BTreeMap;

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

async fn handle_game_websocket(socket: WebSocket, state: AppState, game_id: Uuid) {
    let (mut sender, mut receiver) = socket.split();

    // Check if game exists
    let game = match get_game_by_id(&state.db, game_id).await {
        Ok(Some(game)) => game,
        Ok(None) => {
            let error_msg = WebSocketMessage {
                message_type: "error".to_string(),
                data: serde_json::json!({"message": "Game not found"}),
            };
            let _ = sender
                .send(Message::Text(
                    serde_json::to_string(&error_msg).unwrap().into(),
                ))
                .await;
            return;
        }
        Err(e) => {
            tracing::error!(error = ?e, "Failed to fetch game for WebSocket");
            let error_msg = WebSocketMessage {
                message_type: "error".to_string(),
                data: serde_json::json!({"message": "Internal server error"}),
            };
            let _ = sender
                .send(Message::Text(
                    serde_json::to_string(&error_msg).unwrap().into(),
                ))
                .await;
            return;
        }
    };

    // Subscribe to broadcast channel FIRST (buffer incoming notifications)
    let mut broadcast_receiver = state.game_channels.subscribe(game_id).await;

    // Fetch existing frames from database
    let existing_turns = match get_turns_by_game_id(&state.db, game_id).await {
        Ok(turns) => turns,
        Err(e) => {
            tracing::error!(error = ?e, "Failed to fetch turns for WebSocket");
            let error_msg = WebSocketMessage {
                message_type: "error".to_string(),
                data: serde_json::json!({"message": "Failed to fetch game frames"}),
            };
            let _ = sender
                .send(Message::Text(
                    serde_json::to_string(&error_msg).unwrap().into(),
                ))
                .await;
            return;
        }
    };

    // Track the last turn we sent
    let mut last_sent_turn = -1i32;

    // Owner logins for filling in `Author` on frames persisted before the
    // game runner threaded authors through (see fill_frame_authors).
    let authors = frame_author_map(&state.db, game_id).await;
    // Serve-time shout suppression (DEV-1297): empty for live games; a
    // reconnecting viewer of an already-screened finished game gets
    // stripped frames.
    let suppressed =
        crate::moderation::shouts::load_suppressed_set(&state.db, game_id, &game.status).await;

    // Send all existing frames
    for turn in existing_turns {
        if let Some(mut frame_data) = turn.frame_data {
            prepare_public_frame(&mut frame_data, &authors, &suppressed);
            let frame_msg = WebSocketMessage {
                message_type: "frame".to_string(),
                data: frame_data,
            };
            if sender
                .send(Message::Text(
                    serde_json::to_string(&frame_msg).unwrap().into(),
                ))
                .await
                .is_err()
            {
                // Client disconnected
                return;
            }
            last_sent_turn = turn.turn_number;
        }
    }

    // If game is finished, send game_end and do a proper close handshake
    if game.status == GameStatus::Finished {
        let end_msg = WebSocketMessage {
            message_type: "game_end".to_string(),
            data: serde_json::json!({}),
        };
        let _ = sender
            .send(Message::Text(
                serde_json::to_string(&end_msg).unwrap().into(),
            ))
            .await;
        graceful_close(&mut sender, &mut receiver).await;
        return;
    }

    // For running games, listen for new frames
    loop {
        tokio::select! {
            // Handle incoming WebSocket messages (mostly for ping/pong and close)
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        // Client disconnected
                        break;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {
                        // Ignore other messages
                    }
                    Some(Err(_)) => {
                        // Connection error
                        break;
                    }
                }
            }
            // Handle broadcast notifications
            notification = broadcast_receiver.recv() => {
                match notification {
                    Ok(turn_notification) => {
                        // Skip if we've already sent this turn
                        if turn_notification.turn_number <= last_sent_turn {
                            continue;
                        }

                        // Fetch the frame data from DB
                        if let Ok(turns) = crate::models::turn::get_turns_from(
                            &state.db,
                            game_id,
                            turn_notification.turn_number
                        ).await {
                            for turn in turns {
                                if turn.turn_number <= last_sent_turn {
                                    continue;
                                }
                                if let Some(mut frame_data) = turn.frame_data {
                                    prepare_public_frame(&mut frame_data, &authors, &suppressed);
                                    let frame_msg = WebSocketMessage {
                                        message_type: "frame".to_string(),
                                        data: frame_data,
                                    };
                                    if sender
                                        .send(Message::Text(serde_json::to_string(&frame_msg).unwrap().into()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    last_sent_turn = turn.turn_number;
                                }
                            }
                        }

                        // Check if game is now finished
                        if let Ok(Some(game)) = get_game_by_id(&state.db, game_id).await
                            && game.status == GameStatus::Finished {
                                let end_msg = WebSocketMessage {
                                    message_type: "game_end".to_string(),
                                    data: serde_json::json!({}),
                                };
                                let _ = sender
                                    .send(Message::Text(serde_json::to_string(&end_msg).unwrap().into()))
                                    .await;
                                graceful_close(&mut sender, &mut receiver).await;
                                return;
                            }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        // We fell behind - close and let client reconnect
                        tracing::warn!(game_id = %game_id, lagged = count, "WebSocket lagged, closing");
                        let error_msg = WebSocketMessage {
                            message_type: "error".to_string(),
                            data: serde_json::json!({"message": "Connection lagged, please reconnect"}),
                        };
                        let _ = sender
                            .send(Message::Text(serde_json::to_string(&error_msg).unwrap().into()))
                            .await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Channel closed (game ended or channel cleanup)
                        // Check final game state
                        if let Ok(Some(game)) = get_game_by_id(&state.db, game_id).await
                            && game.status == GameStatus::Finished {
                                let end_msg = WebSocketMessage {
                                    message_type: "game_end".to_string(),
                                    data: serde_json::json!({}),
                                };
                                let _ = sender
                                    .send(Message::Text(serde_json::to_string(&end_msg).unwrap().into()))
                                    .await;
                            }
                        graceful_close(&mut sender, &mut receiver).await;
                        return;
                    }
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
