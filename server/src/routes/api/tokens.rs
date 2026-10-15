use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    models::api_token::{self, ApiToken, Permission, SnakeAccess},
    routes::auth::ApiUser,
    state::AppState,
};

/// Request body for creating a new token
#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    /// Scoped tokens ship in a later deploy. Until then a request naming a
    /// scope is refused, never ignored: ignoring it would mint a token wider
    /// than the caller asked for.
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

/// What a session's token gets: everything a token could do before scopes
/// existed. `admin` only for admins; `email.read` is not a default.
fn default_permissions(is_admin: bool) -> Vec<Permission> {
    Permission::ALL
        .into_iter()
        .filter(|p| match p {
            Permission::EmailRead => false,
            Permission::Admin => is_admin,
            _ => true,
        })
        .collect()
}

/// POST /api/v1/tokens - Create a new API token
///
/// A token mints a copy of its own scope, so it can never mint a wider one.
/// A session mints the default set.
pub async fn create_token(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Json(request): Json<CreateTokenRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::TokensManage)?;

    if request.names_a_scope() {
        return Err((
            StatusCode::BAD_REQUEST,
            "scoped tokens are not supported by this server yet".to_string(),
        ));
    }

    let (permissions, snake_access, snake_ids): (Vec<Permission>, SnakeAccess, Vec<Uuid>) =
        match auth.token_scope() {
            Some(scope) => (
                scope.permissions.iter().copied().collect(),
                scope.snake_access,
                scope.snake_ids.iter().copied().collect(),
            ),
            None => (
                default_permissions(auth.is_admin),
                SnakeAccess::All,
                Vec::new(),
            ),
        };

    let new_token = api_token::create_api_token(
        &state.db,
        auth.user_id,
        &request.name,
        &permissions,
        snake_access,
        &snake_ids,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to create API token: {e:#}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create API token".to_string(),
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
    ApiUser(auth): ApiUser,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::TokensManage)?;

    let tokens = api_token::list_user_tokens(&state.db, auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list API tokens: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to list API tokens".to_string(),
            )
        })?;

    let response: Vec<TokenResponse> = tokens.into_iter().map(TokenResponse::from).collect();
    Ok(Json(response))
}

/// DELETE /api/v1/tokens/:id - Revoke a token
pub async fn revoke_token(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Path(token_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::TokensManage)?;

    let revoked = api_token::revoke_token(&state.db, token_id, auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to revoke API token: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to revoke API token".to_string(),
            )
        })?;

    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "Token not found".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::routes::test_support::{
        api_app, create_user, create_user_session, send_api, send_api_with_cookie, session_user_id,
        signed_session_cookie,
    };
    use sqlx::PgPool;

    async fn create_snake(db: &PgPool, user_id: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar!(
            r#"INSERT INTO battlesnakes (user_id, name, url)
               VALUES ($1, $2, 'https://snake.example') RETURNING battlesnake_id"#,
            user_id,
            name
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn mint(
        db: &PgPool,
        user_id: Uuid,
        permissions: &[Permission],
        access: SnakeAccess,
        snake_ids: &[Uuid],
    ) -> String {
        api_token::create_api_token(db, user_id, "test", permissions, access, snake_ids)
            .await
            .unwrap()
            .secret
    }

    /// The scope stored for the token named `name` (the newest one).
    async fn stored_scope(
        db: &PgPool,
        user_id: Uuid,
        name: &str,
    ) -> (BTreeSet<Permission>, SnakeAccess, BTreeSet<Uuid>) {
        let row = sqlx::query!(
            r#"SELECT t.snake_access AS "snake_access: SnakeAccess",
                   ARRAY(SELECT p.permission FROM api_token_permissions p WHERE p.api_token_id = t.id) AS "permissions!: Vec<String>",
                   ARRAY(SELECT s.battlesnake_id FROM api_token_snakes s WHERE s.api_token_id = t.id) AS "snake_ids!: Vec<Uuid>"
               FROM api_tokens t
               WHERE t.user_id = $1 AND t.name = $2
               ORDER BY t.created_at DESC
               LIMIT 1"#,
            user_id,
            name
        )
        .fetch_one(db)
        .await
        .unwrap();
        (
            row.permissions.iter().map(|p| p.parse().unwrap()).collect(),
            row.snake_access,
            row.snake_ids.into_iter().collect(),
        )
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn zero_permission_token_cannot_manage_tokens(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921800, false).await;
        let token = mint(&db, user, &[], SnakeAccess::All, &[]).await;

        for (method, path, body) in [
            (
                "POST",
                "/api/tokens".to_string(),
                Some(serde_json::json!({"name": "x"})),
            ),
            ("GET", "/api/tokens".to_string(), None),
            ("DELETE", format!("/api/tokens/{}", Uuid::new_v4()), None),
        ] {
            let (status, response) = send_api(&app, method, &path, Some(&token), body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {response}");
            assert_eq!(response, "missing permission: tokens.manage");
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn a_scoped_request_is_refused_not_ignored(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921701, false).await;
        let token = mint(
            &db,
            user,
            &[Permission::TokensManage],
            SnakeAccess::All,
            &[],
        )
        .await;

        for body in [
            serde_json::json!({"name": "narrow", "permissions": ["profile.read"]}),
            serde_json::json!({"name": "narrow", "snake_access": "selected"}),
            serde_json::json!({"name": "narrow", "snake_ids": []}),
        ] {
            let (status, response) =
                send_api(&app, "POST", "/api/tokens", Some(&token), Some(body)).await;
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
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn a_token_mints_a_copy_of_its_own_scope(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921702, false).await;
        let snake = create_snake(&db, user, "copied").await;
        let permissions = [Permission::TokensManage, Permission::SnakesRead];
        let token = mint(&db, user, &permissions, SnakeAccess::Selected, &[snake]).await;

        let (status, body) = send_api(
            &app,
            "POST",
            "/api/tokens",
            Some(&token),
            Some(serde_json::json!({"name": "child"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            stored_scope(&db, user, "child").await,
            (
                permissions.into_iter().collect(),
                SnakeAccess::Selected,
                BTreeSet::from([snake]),
            )
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn a_session_mints_the_default_set(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);

        for (github_id, is_admin) in [(9921703, false), (9921704, true)] {
            let session = create_user_session(&db, github_id, is_admin).await;
            let user = session_user_id(&db, session).await;
            let cookie = signed_session_cookie(&state, session);
            let (status, body) = send_api_with_cookie(
                &app,
                "POST",
                "/api/tokens",
                &cookie,
                Some(serde_json::json!({"name": "from-session"})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");

            let (permissions, access, snake_ids) = stored_scope(&db, user, "from-session").await;
            assert_eq!(access, SnakeAccess::All);
            assert!(snake_ids.is_empty());
            assert!(!permissions.contains(&Permission::EmailRead));
            assert_eq!(permissions.contains(&Permission::Admin), is_admin);
            assert_eq!(permissions.len(), if is_admin { 12 } else { 11 });
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn revoke_requires_permission_and_404s_for_others(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921809, false).await;
        let other = create_user(&db, 9921810, false).await;
        let target = api_token::create_api_token(
            &db,
            other,
            "someone-elses",
            &[Permission::ProfileRead],
            SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();

        let manager = mint(
            &db,
            user,
            &[Permission::TokensManage],
            SnakeAccess::All,
            &[],
        )
        .await;
        // Another user's token: 404 (ownership rule), not 204.
        let (status, _body) = send_api(
            &app,
            "DELETE",
            &format!("/api/tokens/{}", target.token.id),
            Some(&manager),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
