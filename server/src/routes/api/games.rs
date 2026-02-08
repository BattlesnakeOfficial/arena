use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    jobs::GameRunnerJob,
    models::{
        game::{self, CreateGameWithSnakes, Game, GameBoardSize, GameStatus, GameType},
        game_battlesnake::{self, GameBattlesnakeWithDetails},
        turn,
    },
    routes::auth::ApiUser,
    state::AppState,
};

/// Request body for creating a game
#[derive(Debug, Deserialize)]
pub struct CreateGameRequest {
    /// Snake IDs to include in the game (1-4 required)
    pub snakes: Vec<Uuid>,
    /// Board size: "7x7", "11x11", or "19x19" (default: "11x11")
    #[serde(default = "default_board")]
    pub board: String,
    /// Game type: "standard", "royale", "constrictor", or "snail" (default: "standard")
    #[serde(default = "default_game_type")]
    pub game_type: String,
}

fn default_board() -> String {
    "11x11".to_string()
}

fn default_game_type() -> String {
    "standard".to_string()
}

/// Parse game_type string case-insensitively
fn parse_game_type(s: &str) -> Result<GameType, &'static str> {
    match s.to_lowercase().as_str() {
        "standard" => Ok(GameType::Standard),
        "royale" => Ok(GameType::Royale),
        "constrictor" => Ok(GameType::Constrictor),
        "snail" | "snailmode" | "snail_mode" | "snail mode" => Ok(GameType::SnailMode),
        _ => Err("Invalid game type. Use standard, royale, constrictor, or snail"),
    }
}

/// Parse board size string
fn parse_board_size(s: &str) -> Result<GameBoardSize, &'static str> {
    match s.to_lowercase().as_str() {
        "7x7" => Ok(GameBoardSize::Small),
        "11x11" => Ok(GameBoardSize::Medium),
        "19x19" => Ok(GameBoardSize::Large),
        _ => Err("Invalid board size. Use 7x7, 11x11, or 19x19"),
    }
}

/// Response for a created game (minimal)
#[derive(Debug, Serialize)]
pub struct CreateGameResponse {
    pub id: Uuid,
    pub status: String,
}

/// Snake info in game responses
#[derive(Debug, Serialize)]
pub struct SnakeInfo {
    pub id: Uuid,
    pub name: String,
    pub url: String,
}

impl From<&GameBattlesnakeWithDetails> for SnakeInfo {
    fn from(snake: &GameBattlesnakeWithDetails) -> Self {
        Self {
            id: snake.battlesnake_id,
            name: snake.name.clone(),
            url: snake.url.clone(),
        }
    }
}

/// Response for game list items (without frames)
#[derive(Debug, Serialize)]
pub struct GameListItem {
    pub id: Uuid,
    pub status: String,
    pub winner: Option<Uuid>,
    pub snakes: Vec<SnakeInfo>,
    pub board: String,
    pub game_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Response for full game details (with frames)
#[derive(Debug, Serialize)]
pub struct GameResponse {
    pub id: Uuid,
    pub status: String,
    pub winner: Option<Uuid>,
    pub snakes: Vec<SnakeInfo>,
    pub frames: Vec<serde_json::Value>,
    pub board: String,
    pub game_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Query parameters for listing games
#[derive(Debug, Deserialize)]
pub struct ListGamesQuery {
    pub snake_id: Option<Uuid>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

/// Build a GameListItem from game and battlesnakes
fn build_game_list_item(game: &Game, battlesnakes: &[GameBattlesnakeWithDetails]) -> GameListItem {
    let winner = battlesnakes
        .iter()
        .find(|b| b.placement == Some(1))
        .map(|b| b.battlesnake_id);

    let snakes: Vec<SnakeInfo> = battlesnakes.iter().map(SnakeInfo::from).collect();

    GameListItem {
        id: game.game_id,
        status: game.status.as_str().to_string(),
        winner,
        snakes,
        board: game.board_size.as_str().to_string(),
        game_type: game.game_type.as_str().to_string(),
        created_at: game.created_at,
        updated_at: game.updated_at,
        enqueued_at: game.enqueued_at,
    }
}

/// POST /api/games - Create a new game
pub async fn create_game(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(request): Json<CreateGameRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Parse board size
    let board_size =
        parse_board_size(&request.board).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Parse game type
    let game_type = parse_game_type(&request.game_type)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    // Validate snake count
    if request.snakes.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "At least one snake is required".to_string(),
        ));
    }
    if request.snakes.len() > 4 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Maximum of 4 snakes allowed".to_string(),
        ));
    }

    // Get unique snake IDs to validate (duplicates are allowed but we only need to check each once)
    let unique_snake_ids: Vec<Uuid> = {
        let mut ids = request.snakes.clone();
        ids.sort();
        ids.dedup();
        ids
    };

    // Validate that all unique snakes exist and are accessible to the user
    // (owned by user OR public)
    let accessible_snakes = sqlx::query!(
        r#"
        SELECT battlesnake_id
        FROM battlesnakes
        WHERE battlesnake_id = ANY($1)
          AND (user_id = $2 OR visibility = 'public')
        "#,
        &unique_snake_ids as &[Uuid],
        user.user_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to validate snakes: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error".to_string(),
        )
    })?;

    // Check if all requested snakes were found and accessible
    let accessible_ids: Vec<Uuid> = accessible_snakes.iter().map(|r| r.battlesnake_id).collect();
    for snake_id in &unique_snake_ids {
        if !accessible_ids.contains(snake_id) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Snake {} not found or not accessible", snake_id),
            ));
        }
    }

    let snake_count = request.snakes.len();

    // Create the game
    let create_request = CreateGameWithSnakes {
        board_size,
        game_type,
        battlesnake_ids: request.snakes,
    };

    let game = game::create_game_with_snakes(&state.db, create_request)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create game: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create game".to_string(),
            )
        })?;

    // Set enqueued_at timestamp before enqueueing the job
    game::set_game_enqueued_at(&state.db, game.game_id, chrono::Utc::now())
        .await
        .map_err(|e| {
            tracing::error!("Failed to set enqueued_at: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to prepare game".to_string(),
            )
        })?;

    // Enqueue the game runner job
    let job = GameRunnerJob {
        game_id: game.game_id,
    };
    cja::jobs::Job::enqueue(job, state, format!("Game {} created via API", game.game_id))
        .await
        .map_err(|e| {
            tracing::error!("Failed to enqueue game runner job: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to start game".to_string(),
            )
        })?;

    tracing::info!(
        event_type = "game_created",
        game_id = %game.game_id,
        board_size = board_size.as_str(),
        game_type = game_type.as_str(),
        snake_count = snake_count,
        user_id = %user.user_id,
        source = "api",
        "game created via API"
    );

    Ok((
        StatusCode::CREATED,
        Json(CreateGameResponse {
            id: game.game_id,
            status: game.status.as_str().to_string(),
        }),
    ))
}

/// GET /api/games - List games
pub async fn list_games(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Query(query): Query<ListGamesQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let limit = query.limit.min(100) as i64;

    // If filtering by snake_id, validate access first
    if let Some(snake_id) = query.snake_id {
        let accessible = sqlx::query!(
            r#"
            SELECT battlesnake_id
            FROM battlesnakes
            WHERE battlesnake_id = $1
              AND (user_id = $2 OR visibility = 'public')
            "#,
            snake_id,
            user.user_id
        )
        .fetch_optional(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to validate snake: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

        if accessible.is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                "Snake not found or not accessible".to_string(),
            ));
        }
    }

    // Build query based on whether we're filtering by snake
    let games: Vec<Game> = if let Some(snake_id) = query.snake_id {
        // Filter by specific snake
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT g.game_id, g.board_size, g.game_type, g.status, g.enqueued_at, g.created_at, g.updated_at
            FROM games g
            JOIN game_battlesnakes gb ON g.game_id = gb.game_id
            WHERE gb.battlesnake_id = $1
            ORDER BY g.created_at DESC
            LIMIT $2
            "#,
            snake_id,
            limit
        )
        .fetch_all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list games: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string())
        })?;

        rows.into_iter()
            .filter_map(|row| {
                let board_size = GameBoardSize::from_str(&row.board_size).ok()?;
                let game_type = GameType::from_str(&row.game_type).ok()?;
                let status = GameStatus::from_str(&row.status).ok()?;
                Some(Game {
                    game_id: row.game_id,
                    board_size,
                    game_type,
                    status,
                    enqueued_at: row.enqueued_at,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    } else {
        // List games where user has a snake participating
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT g.game_id, g.board_size, g.game_type, g.status, g.enqueued_at, g.created_at, g.updated_at
            FROM games g
            JOIN game_battlesnakes gb ON g.game_id = gb.game_id
            JOIN battlesnakes b ON gb.battlesnake_id = b.battlesnake_id
            WHERE b.user_id = $1
            ORDER BY g.created_at DESC
            LIMIT $2
            "#,
            user.user_id,
            limit
        )
        .fetch_all(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list games: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".to_string())
        })?;

        rows.into_iter()
            .filter_map(|row| {
                let board_size = GameBoardSize::from_str(&row.board_size).ok()?;
                let game_type = GameType::from_str(&row.game_type).ok()?;
                let status = GameStatus::from_str(&row.status).ok()?;
                Some(Game {
                    game_id: row.game_id,
                    board_size,
                    game_type,
                    status,
                    enqueued_at: row.enqueued_at,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    };

    // Fetch battlesnakes for each game
    let mut response: Vec<GameListItem> = Vec::with_capacity(games.len());
    for game in &games {
        let battlesnakes = game_battlesnake::get_battlesnakes_by_game_id(&state.db, game.game_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    "Failed to get battlesnakes for game {}: {}",
                    game.game_id,
                    e
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            })?;
        response.push(build_game_list_item(game, &battlesnakes));
    }

    Ok(Json(response))
}

/// GET /api/games/{id}/details - Show game details with frames
pub async fn show_game(
    State(state): State<AppState>,
    ApiUser(_user): ApiUser,
    Path(game_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Fetch the game
    let game = game::get_game_by_id(&state.db, game_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get game: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Game not found".to_string()))?;

    // Fetch battlesnakes
    let battlesnakes = game_battlesnake::get_battlesnakes_by_game_id(&state.db, game_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get battlesnakes: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

    // Fetch all turns
    let turns = turn::get_turns_by_game_id(&state.db, game_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get turns: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

    // Extract frames from turns
    let frames: Vec<serde_json::Value> = turns.into_iter().filter_map(|t| t.frame_data).collect();

    // Find winner
    let winner = battlesnakes
        .iter()
        .find(|b| b.placement == Some(1))
        .map(|b| b.battlesnake_id);

    let snakes: Vec<SnakeInfo> = battlesnakes.iter().map(SnakeInfo::from).collect();

    Ok(Json(GameResponse {
        id: game.game_id,
        status: game.status.as_str().to_string(),
        winner,
        snakes,
        frames,
        board: game.board_size.as_str().to_string(),
        game_type: game.game_type.as_str().to_string(),
        created_at: game.created_at,
    }))
}

/// Request body for batch game status lookup
#[derive(Debug, Deserialize)]
pub struct BatchGameStatusRequest {
    pub game_ids: Vec<Uuid>,
}

/// Response item for batch game status
#[derive(Debug, Serialize)]
pub struct GameStatusItem {
    pub id: Uuid,
    pub status: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// POST /api/games/status - Batch lookup game statuses
pub async fn batch_game_status(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(request): Json<BatchGameStatusRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if request.game_ids.len() > 500 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Maximum of 500 game IDs allowed per request".to_string(),
        ));
    }

    if request.game_ids.is_empty() {
        return Ok(Json(Vec::<GameStatusItem>::new()));
    }

    let rows = sqlx::query!(
        r#"
        SELECT g.game_id, g.status, g.updated_at, g.enqueued_at, g.created_at
        FROM games g
        WHERE g.game_id = ANY($1)
          AND g.game_id IN (
            SELECT gb.game_id FROM game_battlesnakes gb
            JOIN battlesnakes b ON gb.battlesnake_id = b.battlesnake_id
            WHERE b.user_id = $2
          )
        "#,
        &request.game_ids as &[Uuid],
        user.user_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
        tracing::error!("Failed to fetch game statuses: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error".to_string(),
        )
    })?;

    let items: Vec<GameStatusItem> = rows
        .into_iter()
        .map(|row| GameStatusItem {
            id: row.game_id,
            status: row.status,
            updated_at: row.updated_at,
            enqueued_at: row.enqueued_at,
            created_at: row.created_at,
        })
        .collect();

    Ok(Json(items))
}

// Import FromStr for parsing enums
use std::str::FromStr;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_game_type() {
        // Standard cases
        assert!(matches!(
            parse_game_type("standard"),
            Ok(GameType::Standard)
        ));
        assert!(matches!(
            parse_game_type("Standard"),
            Ok(GameType::Standard)
        ));
        assert!(matches!(
            parse_game_type("STANDARD"),
            Ok(GameType::Standard)
        ));

        // Royale
        assert!(matches!(parse_game_type("royale"), Ok(GameType::Royale)));
        assert!(matches!(parse_game_type("Royale"), Ok(GameType::Royale)));

        // Constrictor
        assert!(matches!(
            parse_game_type("constrictor"),
            Ok(GameType::Constrictor)
        ));

        // Snail mode variants
        assert!(matches!(parse_game_type("snail"), Ok(GameType::SnailMode)));
        assert!(matches!(
            parse_game_type("snailmode"),
            Ok(GameType::SnailMode)
        ));
        assert!(matches!(
            parse_game_type("snail_mode"),
            Ok(GameType::SnailMode)
        ));
        assert!(matches!(
            parse_game_type("Snail Mode"),
            Ok(GameType::SnailMode)
        ));

        // Invalid
        assert!(parse_game_type("invalid").is_err());
    }

    #[test]
    fn test_parse_board_size() {
        assert!(matches!(parse_board_size("7x7"), Ok(GameBoardSize::Small)));
        assert!(matches!(
            parse_board_size("11x11"),
            Ok(GameBoardSize::Medium)
        ));
        assert!(matches!(
            parse_board_size("19x19"),
            Ok(GameBoardSize::Large)
        ));

        // Invalid
        assert!(parse_board_size("10x10").is_err());
        assert!(parse_board_size("invalid").is_err());
    }

    #[test]
    fn test_create_game_request_defaults() {
        let json = r#"{"snakes": ["550e8400-e29b-41d4-a716-446655440000"]}"#;
        let request: CreateGameRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.board, "11x11");
        assert_eq!(request.game_type, "standard");
    }

    #[test]
    fn test_snake_info_serialization() {
        let snake = SnakeInfo {
            id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            name: "Test Snake".to_string(),
            url: "http://example.com".to_string(),
        };

        let json = serde_json::to_string(&snake).unwrap();
        assert!(json.contains("\"id\":"));
        assert!(json.contains("\"name\":\"Test Snake\""));
        assert!(json.contains("\"url\":\"http://example.com\""));
    }

    #[test]
    fn test_game_response_serialization() {
        let response = GameResponse {
            id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            status: "waiting".to_string(),
            winner: None,
            snakes: vec![],
            frames: vec![],
            board: "11x11".to_string(),
            game_type: "Standard".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"status\":\"waiting\""));
        assert!(json.contains("\"board\":\"11x11\""));
        assert!(json.contains("\"game_type\":\"Standard\""));
    }
}
