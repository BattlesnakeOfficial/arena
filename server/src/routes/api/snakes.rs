use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    models::battlesnake::{
        self, Battlesnake, CreateBattlesnake, EngineRegion, UpdateBattlesnake, Visibility,
    },
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
    pub engine_region: EngineRegion,
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
            engine_region: snake.engine_region,
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
    #[serde(default, deserialize_with = "deserialize_present_region")]
    pub engine_region: Option<serde_json::Value>,
}

/// Request body for updating a snake
#[derive(Debug, Deserialize)]
pub struct UpdateSnakeRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub is_public: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present_region")]
    pub engine_region: Option<serde_json::Value>,
}

fn deserialize_present_region<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

fn parse_api_region(
    value: Option<serde_json::Value>,
) -> Result<Option<EngineRegion>, (StatusCode, String)> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(value) = value.as_str() else {
        return Err((StatusCode::BAD_REQUEST, "Invalid engine region".to_string()));
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid engine region".to_string()))
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
    if let Err(e) = battlesnake::validate_url(&request.url) {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }
    let name =
        battlesnake::validate_name(&request.name).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let engine_region = parse_api_region(request.engine_region)?.unwrap_or_default();

    // Moderation runs before the insert; a flagged/unchecked row may
    // reference a snake that was never created (it records the submission).
    let decision = crate::moderation::moderate_field(
        &state.db,
        &state.moderation,
        user.user_id,
        None,
        crate::moderation::FieldKind::SnakeName,
        &name,
        request.is_public,
    )
    .await;
    if decision == crate::moderation::Decision::Block {
        return Err((
            StatusCode::BAD_REQUEST,
            crate::moderation::FieldKind::SnakeName
                .rejection_message()
                .to_string(),
        ));
    }

    let create_data = CreateBattlesnake {
        name,
        url: request.url,
        engine_region,
        visibility: if request.is_public {
            Visibility::Public
        } else {
            Visibility::Private
        },
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

    // Relay only clean names: flagged, blocked, and unchecked names never
    // reach Discord.
    if snake.visibility == Visibility::Public && decision == crate::moderation::Decision::Allow {
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

    // Compute BEFORE the defaults below partially move existing.name (and
    // existing.url). Trimmed comparison: re-sending the same name should
    // not burn a Jev call.
    let name_changed = request
        .name
        .as_deref()
        .is_some_and(|n| n.trim() != existing.name);

    // Build update with existing values as defaults
    let new_url = request.url.unwrap_or(existing.url);

    // Validate the effective URL (new or existing)
    if let Err(e) = battlesnake::validate_url(&new_url) {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    let name = match request.name {
        Some(name) => {
            battlesnake::validate_name(&name).map_err(|e| (StatusCode::BAD_REQUEST, e))?
        }
        None => existing.name,
    };
    let engine_region = parse_api_region(request.engine_region)?;

    let new_visibility = match request.is_public {
        Some(true) => Visibility::Public,
        Some(false) => Visibility::Private,
        None => existing.visibility,
    };

    // Only re-moderate when the name actually changes. (No Discord relay on
    // the API update path.) Runs before the update builds so the resolved
    // name is still borrowable.
    if name_changed {
        let decision = crate::moderation::moderate_field(
            &state.db,
            &state.moderation,
            user.user_id,
            Some(snake_id),
            crate::moderation::FieldKind::SnakeName,
            &name,
            new_visibility == Visibility::Public,
        )
        .await;
        if decision == crate::moderation::Decision::Block {
            return Err((
                StatusCode::BAD_REQUEST,
                crate::moderation::FieldKind::SnakeName
                    .rejection_message()
                    .to_string(),
            ));
        }
    }

    let update_data = UpdateBattlesnake {
        name,
        url: new_url,
        engine_region,
        visibility: new_visibility,
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
    let outcome = battlesnake::delete_battlesnake(&state.db, snake_id, user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete snake: {:#}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    match outcome {
        battlesnake::DeleteBattlesnakeOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        battlesnake::DeleteBattlesnakeOutcome::NotFound => Err(StatusCode::NOT_FOUND),
        // Registered in a tournament that is open or running: withdraw first.
        battlesnake::DeleteBattlesnakeOutcome::InActiveTournament => Err(StatusCode::CONFLICT),
    }
}
