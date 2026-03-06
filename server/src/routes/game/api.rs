use axum::{
    Json,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::IntoResponse,
};
use color_eyre::eyre::Context as _;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    errors::ServerResult,
    models::game::{GameStatus, get_game_by_id},
    models::turn::get_turns_by_game_id,
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
    pub width: u32,
    pub height: u32,
}

/// GET /api/games/{id}
/// Returns game info for the Battlesnake board viewer
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
        game: BoardViewerGame { width, height },
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
                width: 11,
                height: 11,
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"Game":{"Width":11,"Height":11}}"#);
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
}
