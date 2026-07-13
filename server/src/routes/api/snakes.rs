use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{
    models::battlesnake::{self, Battlesnake, CreateBattlesnake, UpdateBattlesnake, Visibility},
    routes::auth::ApiUser,
    state::AppState,
};

/// Response format for snake endpoints
#[derive(Debug, Serialize)]
pub struct SnakeResponse {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    pub is_public: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<Battlesnake> for SnakeResponse {
    fn from(snake: Battlesnake) -> Self {
        Self {
            id: snake.battlesnake_id,
            name: snake.name,
            url: snake.url,
            is_public: snake.visibility == Visibility::Public,
            created_at: snake.created_at,
            updated_at: snake.updated_at,
        }
    }
}

/// Request body for creating a snake
#[derive(Debug, Deserialize)]
pub struct CreateSnakeRequest {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub is_public: bool,
}

/// Request body for updating a snake
#[derive(Debug, Deserialize)]
pub struct UpdateSnakeRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub is_public: Option<bool>,
}

/// Validate that a URL is a valid HTTP or HTTPS URL
fn validate_url(url: &str) -> Result<(), &'static str> {
    match Url::parse(url) {
        Ok(parsed) => {
            if parsed.scheme() == "http" || parsed.scheme() == "https" {
                Ok(())
            } else {
                Err("URL must use HTTP or HTTPS scheme")
            }
        }
        Err(_) => Err("Invalid URL format"),
    }
}

/// GET /api/snakes - List user's snakes
pub async fn list_snakes(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
) -> Result<impl IntoResponse, StatusCode> {
    let snakes = battlesnake::get_battlesnakes_by_user_id(&state.db, user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list snakes: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let response: Vec<SnakeResponse> = snakes.into_iter().map(SnakeResponse::from).collect();
    Ok(Json(response))
}

/// POST /api/snakes - Create snake
pub async fn create_snake(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(request): Json<CreateSnakeRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Validate URL
    if let Err(e) = validate_url(&request.url) {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    let create_data = CreateBattlesnake {
        name: request.name,
        url: request.url,
        visibility: if request.is_public {
            Visibility::Public
        } else {
            Visibility::Private
        },
        language: String::new(),
        platform: String::new(),
    };

    let snake = battlesnake::create_battlesnake(&state.db, user.user_id, create_data)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create snake: {}", e);
            // Return the error message for unique constraint violations
            let msg = e.to_string();
            if msg.contains("already have a battlesnake named") {
                (StatusCode::CONFLICT, msg)
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create snake".to_string(),
                )
            }
        })?;

    if snake.visibility == Visibility::Public {
        state
            .discord
            .notify_snake_registered(&snake.name, &user.github_login);
    }

    Ok((StatusCode::CREATED, Json(SnakeResponse::from(snake))))
}

/// GET /api/snakes/{id} - Get snake details
pub async fn get_snake(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(snake_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    let snake = battlesnake::get_battlesnake_by_id(&state.db, snake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get snake: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Enforce ownership - users can only view their own snakes via this endpoint
    if snake.user_id != user.user_id {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(SnakeResponse::from(snake)))
}

/// PUT /api/snakes/{id} - Update snake
pub async fn update_snake(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(snake_id): Path<Uuid>,
    Json(request): Json<UpdateSnakeRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Get the existing snake first
    let existing = battlesnake::get_battlesnake_by_id(&state.db, snake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get snake: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get snake".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Snake not found".to_string()))?;

    // Enforce ownership
    if existing.user_id != user.user_id {
        return Err((StatusCode::NOT_FOUND, "Snake not found".to_string()));
    }

    // Build update with existing values as defaults
    let new_url = request.url.unwrap_or(existing.url);

    // Validate URL if it changed
    if let Err(e) = validate_url(&new_url) {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    let update_data = UpdateBattlesnake {
        name: request.name.unwrap_or(existing.name),
        url: new_url,
        visibility: match request.is_public {
            Some(true) => Visibility::Public,
            Some(false) => Visibility::Private,
            None => existing.visibility,
        },
        language: existing.language,
        platform: existing.platform,
    };

    let snake = battlesnake::update_battlesnake(&state.db, snake_id, user.user_id, update_data)
        .await
        .map_err(|e| {
            tracing::error!("Failed to update snake: {}", e);
            let msg = e.to_string();
            if msg.contains("already have a battlesnake named") {
                (StatusCode::CONFLICT, msg)
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to update snake".to_string(),
                )
            }
        })?;

    Ok(Json(SnakeResponse::from(snake)))
}

/// DELETE /api/snakes/{id} - Delete snake
pub async fn delete_snake(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(snake_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    // Check ownership first
    let exists = battlesnake::belongs_to_user(&state.db, snake_id, user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check snake ownership: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !exists {
        return Err(StatusCode::NOT_FOUND);
    }

    battlesnake::delete_battlesnake(&state.db, snake_id, user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete snake: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(StatusCode::NO_CONTENT)
}
