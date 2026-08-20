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
    models::game::{GameStatus, get_game_by_id},
    models::game_battlesnake::get_battlesnakes_by_game_id,
    models::turn::{get_turn_frames_page, get_turns_by_game_id},
    state::AppState,
};

/// Snake-ID → owner-login map for filling in frame `Author` fields.
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
            .map(|bs| (bs.game_battlesnake_id.to_string(), bs.owner_login))
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

/// Response format for the board viewer's game info endpoint
/// Uses PascalCase to match the Battlesnake board viewer expectations
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct BoardViewerGameResponse {
    pub game: BoardViewerGame,
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
    pub width: u32,
    pub height: u32,
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

    Ok(Json(BoardViewerGameResponse {
        game: BoardViewerGame {
            id: game.game_id.to_string(),
            status: engine_status(game.status).to_string(),
            width,
            height,
        },
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

/// Engine-compatible frames list envelope. The legacy engine used lowercase
/// keys here (unlike the PascalCase frame contents) and the exporter's
/// `gameFramesResponse` deserializes exactly `count` + `frames`.
#[derive(Debug, Serialize)]
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
    get_game_by_id(&state.db, game_id)
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
    let mut frames: Vec<serde_json::Value> =
        turns.into_iter().filter_map(|t| t.frame_data).collect();
    for frame in &mut frames {
        fill_frame_authors(frame, &authors);
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

    // Send all existing frames
    for turn in existing_turns {
        if let Some(mut frame_data) = turn.frame_data {
            fill_frame_authors(&mut frame_data, &authors);
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
                                    fill_frame_authors(&mut frame_data, &authors);
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

    #[test]
    fn test_board_viewer_response_serialization() {
        let response = BoardViewerGameResponse {
            game: BoardViewerGame {
                id: "abc-123".to_string(),
                status: "complete".to_string(),
                width: 11,
                height: 11,
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            json,
            r#"{"Game":{"ID":"abc-123","Status":"complete","Width":11,"Height":11}}"#
        );
    }

    #[test]
    fn test_engine_status_mapping() {
        assert_eq!(engine_status(GameStatus::Waiting), "pending");
        assert_eq!(engine_status(GameStatus::Running), "running");
        assert_eq!(engine_status(GameStatus::Finished), "complete");
        assert_eq!(engine_status(GameStatus::Failed), "complete");
    }

    #[test]
    fn test_frames_response_serialization() {
        // Engine envelope: lowercase count/frames keys wrapping PascalCase
        // frame blobs — exactly what the exporter's gameFramesResponse expects.
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
            r#"{"count":1,"frames":[{"Food":[{"X":1,"Y":2}],"Hazards":[],"Snakes":[],"Turn":0}]}"#
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

        assert_eq!(json["count"], 2);
        let frames = json["frames"].as_array().unwrap();
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
        assert_eq!(json["count"], 2);
        assert_eq!(json["frames"][0]["Turn"], 2);
        assert_eq!(json["frames"][1]["Turn"], 3);

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
        assert_eq!(json["count"], 0);
        assert_eq!(json["frames"].as_array().unwrap().len(), 0);

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
        let frames = json["frames"].as_array().unwrap();
        assert_eq!(json["count"], 4);
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
