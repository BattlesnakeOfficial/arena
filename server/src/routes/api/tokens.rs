use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    models::api_token::{self, ApiToken},
    routes::auth::ApiUser,
    state::AppState,
};

/// Request body for creating a new token
#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    /// Scoped tokens ship in a later deploy. Until then a request naming a
    /// scope is refused, never ignored: ignoring it would mint a full-power
    /// token the caller asked to be narrow.
    pub permissions: Option<serde_json::Value>,
    pub snake_access: Option<serde_json::Value>,
    pub snake_ids: Option<serde_json::Value>,
}

impl CreateTokenRequest {
    fn names_a_scope(&self) -> bool {
        self.permissions.is_some() || self.snake_access.is_some() || self.snake_ids.is_some()
    }
}

/// Response for a newly created token (includes the secret)
#[derive(Debug, Serialize)]
pub struct CreateTokenResponse {
    pub id: Uuid,
    pub name: String,
    pub secret: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Response for listing tokens (no secrets)
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub id: Uuid,
    pub name: String,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<ApiToken> for TokenResponse {
    fn from(token: ApiToken) -> Self {
        Self {
            id: token.id,
            name: token.name,
            last_used_at: token.last_used_at,
            created_at: token.created_at,
        }
    }
}

/// POST /api/v1/tokens - Create a new API token
pub async fn create_token(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Json(request): Json<CreateTokenRequest>,
) -> Result<impl IntoResponse, (StatusCode, &'static str)> {
    if request.names_a_scope() {
        return Err((
            StatusCode::BAD_REQUEST,
            "scoped tokens are not supported by this server yet",
        ));
    }

    let new_token = api_token::create_api_token(&state.db, user.user_id, &request.name)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create API token: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create API token",
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResponse {
            id: new_token.token.id,
            name: new_token.token.name,
            secret: new_token.secret,
            created_at: new_token.token.created_at,
        }),
    ))
}

/// GET /api/v1/tokens - List all active tokens for the current user
pub async fn list_tokens(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
) -> Result<impl IntoResponse, StatusCode> {
    let tokens = api_token::list_user_tokens(&state.db, user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list API tokens: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let response: Vec<TokenResponse> = tokens.into_iter().map(TokenResponse::from).collect();
    Ok(Json(response))
}

/// DELETE /api/v1/tokens/:id - Revoke a token
pub async fn revoke_token(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(token_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    let revoked = api_token::revoke_token(&state.db, token_id, user.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to revoke API token: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::models::api_token::create_api_token;
    use crate::routes::test_support::{api_app, create_user, send_api};
    use crate::state::AppState;

    #[sqlx::test(migrations = "../migrations")]
    async fn a_scoped_request_is_refused_not_ignored(db: sqlx::PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921701, false).await;
        let bearer = create_api_token(&db, user, "caller").await.unwrap().secret;

        for body in [
            serde_json::json!({"name": "narrow", "permissions": ["profile.read"]}),
            serde_json::json!({"name": "narrow", "snake_access": "selected"}),
            serde_json::json!({"name": "narrow", "snake_ids": []}),
        ] {
            let (status, response) =
                send_api(&app, "POST", "/api/tokens", Some(&bearer), Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
            assert_eq!(
                response,
                "scoped tokens are not supported by this server yet"
            );
        }
        let minted: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM api_tokens WHERE user_id = $1"#,
            user
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(minted, 1, "only the caller's own token exists");

        let (status, response) = send_api(
            &app,
            "POST",
            "/api/tokens",
            Some(&bearer),
            Some(serde_json::json!({"name": "plain"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{response}");
    }
}
