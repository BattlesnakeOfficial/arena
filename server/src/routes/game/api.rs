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
    models::turn::{get_turn_frames_page, get_turns_by_game_id},
    state::AppState,
};

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

    let frames: Vec<serde_json::Value> = turns.into_iter().filter_map(|t| t.frame_data).collect();

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

    // Send all existing frames
    for turn in existing_turns {
        if let Some(frame_data) = turn.frame_data {
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
                                if let Some(frame_data) = turn.frame_data {
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
}
