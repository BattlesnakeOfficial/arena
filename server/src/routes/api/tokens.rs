use std::collections::BTreeSet;
use std::str::FromStr;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    models::api_token::{self, Permission, SnakeAccess, TokenSummary},
    routes::auth::ApiUser,
    state::AppState,
};

/// Request body for creating a new token. All fields are optional so that a
/// missing required field is *our* 400, not axum's Json-rejection 422.
#[derive(Debug, Deserialize)]
pub struct CreateTokenRequest {
    pub name: Option<String>,
    pub permissions: Option<Vec<String>>,
    pub snake_access: Option<String>,
    /// Snake IDs as strings: a malformed UUID must be our 400 too.
    pub snake_ids: Option<Vec<String>>,
}

/// Response for a newly created token (includes the secret)
#[derive(Debug, Serialize)]
pub struct CreateTokenResponse {
    pub id: Uuid,
    pub name: String,
    pub secret: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Canonical `Permission::ALL` order, de-duplicated (same as `GET`).
    pub permissions: BTreeSet<Permission>,
    pub snake_access: SnakeAccess,
    /// Sorted, de-duplicated (same as `GET`); empty when access is `all`.
    pub snake_ids: BTreeSet<Uuid>,
}

/// Response for listing tokens (no secrets)
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub id: Uuid,
    pub name: String,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Canonical `Permission::ALL` order (same as `POST`).
    pub permissions: BTreeSet<Permission>,
    pub snake_access: SnakeAccess,
    /// Sorted (same as `POST`); empty when access is `all`.
    pub snake_ids: BTreeSet<Uuid>,
}

impl From<TokenSummary> for TokenResponse {
    fn from(summary: TokenSummary) -> Self {
        Self {
            id: summary.token.id,
            name: summary.token.name,
            last_used_at: summary.token.last_used_at,
            created_at: summary.token.created_at,
            permissions: summary.permissions,
            snake_access: summary.token.snake_access,
            snake_ids: summary.snake_ids,
        }
    }
}

/// POST /api/v1/tokens - Create a new API token
///
/// Validation order is fixed and tested: authorization 403 first, then shape
/// 400s, then ownership 400, then admin/subset 403s.
pub async fn create_token(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Json(request): Json<CreateTokenRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::TokensManage)?;

    let name = request
        .name
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing required field: name".to_string(),
            )
        })?;
    let permission_names = request.permissions.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required field: permissions".to_string(),
        )
    })?;
    let snake_access_str = request.snake_access.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "missing required field: snake_access".to_string(),
        )
    })?;
    let snake_access = match snake_access_str.as_str() {
        "all" => SnakeAccess::All,
        "selected" => SnakeAccess::Selected,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid snake_access: {other} (expected \"all\" or \"selected\")"),
            ));
        }
    };

    // Parse permissions strictly (unknown name → 400 naming it). Duplicates
    // collapse: the composite PK must never see them.
    let permissions = permission_names
        .iter()
        .map(|permission| {
            Permission::from_str(permission).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown permission: {permission}"),
                )
            })
        })
        .collect::<Result<BTreeSet<Permission>, _>>()?;

    // `selected` requires the snake_ids key (an explicit empty list is fine);
    // non-empty snake_ids with `all` is a 400.
    let snake_id_strings = match snake_access {
        SnakeAccess::Selected => request.snake_ids.ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "missing required field: snake_ids (required when snake_access is \"selected\")"
                    .to_string(),
            )
        })?,
        SnakeAccess::All => request.snake_ids.unwrap_or_default(),
    };
    if snake_access == SnakeAccess::All && !snake_id_strings.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "snake_ids must be empty when snake_access is \"all\"".to_string(),
        ));
    }
    let snake_ids = snake_id_strings
        .iter()
        .map(|id| {
            Uuid::parse_str(id)
                .map_err(|_| (StatusCode::BAD_REQUEST, format!("invalid snake id: {id}")))
        })
        .collect::<Result<BTreeSet<Uuid>, _>>()?;

    // Ownership: every selected snake must belong to the user (counted against
    // the de-duplicated list — duplicates must not produce a false 400).
    if !snake_ids.is_empty() {
        let snake_id_list: Vec<Uuid> = snake_ids.iter().copied().collect();
        let owned: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64"
               FROM battlesnakes
               WHERE battlesnake_id = ANY($1) AND user_id = $2 AND deleted_at IS NULL"#,
            &snake_id_list,
            auth.user_id
        )
        .fetch_one(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check snake ownership: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;
        if owned != snake_ids.len() as i64 {
            return Err((
                StatusCode::BAD_REQUEST,
                "all selected snakes must be owned by you".to_string(),
            ));
        }
    }

    // Only admins can hold `admin` — applies to sessions too.
    if permissions.contains(&Permission::Admin) && !auth.is_admin {
        return Err((
            StatusCode::FORBIDDEN,
            "cannot grant admin permission".to_string(),
        ));
    }

    // A token may only mint a token within its own scope. Sessions pass.
    if let Some(scope) = auth.token_scope()
        && !scope.covers(&permissions, snake_access, &snake_ids)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "requested permissions or snake access exceed this token's".to_string(),
        ));
    }

    let new_token = api_token::create_api_token(
        &state.db,
        auth.user_id,
        &name,
        &permissions.iter().copied().collect::<Vec<_>>(),
        snake_access,
        &snake_ids.iter().copied().collect::<Vec<_>>(),
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to create API token: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create API token".to_string(),
        )
    })?;

    if let Some(scope) = auth.token_scope() {
        revoke_if_creator_revoked(&state, scope.token_id, &new_token).await?;
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResponse {
            id: new_token.token.id,
            name: new_token.token.name,
            secret: new_token.secret,
            created_at: new_token.token.created_at,
            permissions,
            snake_access,
            snake_ids,
        }),
    ))
}

/// A token revoked while it was minting must not leave a live copy behind.
/// The copy's INSERT can queue behind a revoke's table lock (the enforce down
/// migration takes one) and commit right after it. Its secret is only ever
/// disclosed in this response, so revoking it here, before responding, means
/// no one can use a copy that escaped. Any error also withholds the secret.
async fn revoke_if_creator_revoked(
    state: &AppState,
    creator_token_id: Uuid,
    new_token: &api_token::NewApiToken,
) -> Result<(), (StatusCode, String)> {
    let internal = |e: color_eyre::Report| {
        tracing::error!("Failed to confirm the minting token is live: {e:#}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create API token".to_string(),
        )
    };
    let user_id = new_token.token.user_id;
    if api_token::get_user_token(&state.db, user_id, creator_token_id)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Ok(());
    }
    api_token::revoke_token(&state.db, new_token.token.id, user_id)
        .await
        .map_err(internal)?;
    Err((
        StatusCode::UNAUTHORIZED,
        "Invalid or revoked token".to_string(),
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
            tracing::error!("Failed to list API tokens: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to list API tokens".to_string(),
            )
        })?;

    let response: Vec<TokenResponse> = tokens.into_iter().map(TokenResponse::from).collect();
    Ok(Json(response))
}

/// DELETE /api/v1/tokens/:id - Revoke a token
///
/// A token may revoke only tokens within its own scope (itself included), so
/// a narrow token can't log out a wider one. Sessions may revoke any.
pub async fn revoke_token(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Path(token_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::TokensManage)?;

    if let Some(scope) = auth.token_scope() {
        let target = api_token::get_user_token(&state.db, auth.user_id, token_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to load API token: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to revoke API token".to_string(),
                )
            })?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Token not found".to_string()))?;
        if !scope.covers(
            &target.permissions,
            target.token.snake_access,
            &target.snake_ids,
        ) {
            return Err((
                StatusCode::FORBIDDEN,
                "cannot revoke a token with wider permissions or snake access than this token's"
                    .to_string(),
            ));
        }
    }

    let revoked = api_token::revoke_token(&state.db, token_id, auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to revoke API token: {e}");
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
    use super::*;
    use crate::routes::test_support::{api_app, send_api, send_api_with_cookie};
    use sqlx::PgPool;

    async fn create_user(db: &PgPool, github_id: i64, is_admin: bool) -> Uuid {
        crate::routes::test_support::create_user(db, github_id, is_admin).await
    }

    async fn create_snake(db: &PgPool, user_id: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar(
            r#"INSERT INTO battlesnakes (user_id, name, url)
               VALUES ($1, $2, 'https://snake.example') RETURNING battlesnake_id"#,
        )
        .bind(user_id)
        .bind(name)
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

    async fn post(
        app: &axum::Router,
        bearer: &str,
        body: serde_json::Value,
    ) -> (StatusCode, String) {
        send_api(app, "POST", "/api/tokens", Some(bearer), Some(body)).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn zero_permission_token_cannot_manage_tokens(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921800, false).await;
        let token = mint(&db, user, &[], SnakeAccess::All, &[]).await;

        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": [], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: tokens.manage");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn missing_fields_return_400(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921801, false).await;
        let token = mint(
            &db,
            user,
            &[Permission::TokensManage],
            SnakeAccess::All,
            &[],
        )
        .await;

        // name missing
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"permissions": [], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("name"));

        // permissions missing
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("permissions"));

        // snake_access missing
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": []}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("snake_access"));

        // selected without the snake_ids key
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": [], "snake_access": "selected"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("snake_ids"));

        // unknown permission
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": ["not.a.permission"], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("not.a.permission"));

        // invalid snake_access value
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": [], "snake_access": "some"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // non-empty snake_ids with all
        let snake = create_snake(&db, user, "four-hundred").await;
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": [], "snake_access": "all", "snake_ids": [snake.to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // malformed UUID
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "x", "permissions": [], "snake_access": "selected", "snake_ids": ["not-a-uuid"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("not-a-uuid"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn escalation_matrix(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921802, false).await;
        let snake_a = create_snake(&db, user, "matrix-a").await;
        let snake_b = create_snake(&db, user, "matrix-b").await;

        // Token: tokens.manage + snakes.read, selected [A].
        let token = mint(
            &db,
            user,
            &[Permission::TokensManage, Permission::SnakesRead],
            SnakeAccess::Selected,
            &[snake_a],
        )
        .await;

        // A permission the creating token lacks → 403 (subset rule).
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "up", "permissions": ["profile.read"], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(
            body,
            "requested permissions or snake access exceed this token's"
        );

        // Subset permission with narrower access → 201.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "ok", "permissions": ["snakes.read"], "snake_access": "selected", "snake_ids": [snake_a.to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["permissions"], serde_json::json!(["snakes.read"]));
        assert_eq!(json["snake_access"], "selected");
        assert_eq!(json["snake_ids"], serde_json::json!([snake_a.to_string()]));

        // Wider selection (snake B not in scope) → 403.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "wider", "permissions": ["snakes.read"], "snake_access": "selected", "snake_ids": [snake_b.to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        // `all` from a `selected` token → 403.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "all-out", "permissions": ["snakes.read"], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        // Non-admin requesting admin → 403, exact body (everything else here
        // is within scope: subset of permissions, selected within [A]).
        let everything: Vec<&str> = Permission::ALL.iter().map(|p| p.as_str()).collect();
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "admin-try", "permissions": everything, "snake_access": "selected", "snake_ids": [snake_a.to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body, "cannot grant admin permission");

        // A token with every non-admin permission still cannot mint admin.
        let powerful: Vec<Permission> = Permission::ALL
            .into_iter()
            .filter(|p| *p != Permission::Admin)
            .collect();
        let token2 = mint(&db, user, &powerful, SnakeAccess::All, &[]).await;
        let (status, body) = post(
            &app,
            &token2,
            serde_json::json!({"name": "admin-try-2", "permissions": ["admin"], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body, "cannot grant admin permission");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn ownership_and_dedup(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921803, false).await;
        let other = create_user(&db, 9921804, false).await;
        let own = create_snake(&db, user, "own-snake").await;
        let foreign = create_snake(&db, other, "foreign-snake").await;
        // profile.read must be in scope for the dup-perm row below.
        let token = mint(
            &db,
            user,
            &[Permission::TokensManage, Permission::ProfileRead],
            SnakeAccess::All,
            &[],
        )
        .await;

        // Foreign snake → 400 (ownership precedes the subset check — this
        // token is `all` so the subset check would pass).
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "foreign", "permissions": [], "snake_access": "selected", "snake_ids": [foreign.to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body, "all selected snakes must be owned by you");

        // Duplicate owned snake_ids → 201 with a single row.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "dedup", "permissions": [], "snake_access": "selected", "snake_ids": [own.to_string(), own.to_string()]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["snake_ids"], serde_json::json!([own.to_string()]));

        // Duplicate permissions → 201, one row per distinct name.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "dup-perm", "permissions": ["profile.read", "profile.read"], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["permissions"], serde_json::json!(["profile.read"]));

        // Permissions echo in canonical order regardless of request order,
        // and GET /api/tokens reports the same ordering for the same token.
        let out_of_order = ["tokens.manage", "profile.read"];
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "canonical", "permissions": out_of_order, "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let canonical: Vec<&str> = Permission::ALL
            .iter()
            .filter(|p| out_of_order.contains(&p.as_str()))
            .map(|p| p.as_str())
            .collect();
        assert_eq!(json["permissions"], serde_json::json!(canonical));
        let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
        let (status, body) = send_api(&app, "GET", "/api/tokens", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let listed = json
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"].as_str() == Some(id.to_string().as_str()))
            .unwrap();
        assert_eq!(listed["permissions"], serde_json::json!(canonical));

        // Explicit empty selection → 201, zero api_token_snakes rows.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "snake-less", "permissions": [], "snake_access": "selected", "snake_ids": []}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
        let rows: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM api_token_snakes WHERE api_token_id = $1"#,
            id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(rows, 0);

        // Zero permissions is a valid token.
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "zero", "permissions": [], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn admin_grant_rules(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);

        // Non-admin session → 403 with the exact body.
        let non_admin_session =
            crate::routes::test_support::create_user_session(&db, 9921806, false).await;
        let non_admin_cookie =
            crate::routes::test_support::signed_session_cookie(&state, non_admin_session);
        let (status, body) = send_api_with_cookie(
            &app,
            "POST",
            "/api/tokens",
            &non_admin_cookie,
            Some(serde_json::json!({"name": "s", "permissions": ["admin"], "snake_access": "all"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body, "cannot grant admin permission");

        // Admin session → 201. (create_user_session makes the admin user too.)
        let admin_session =
            crate::routes::test_support::create_user_session(&db, 9921805, true).await;
        let admin = crate::routes::test_support::session_user_id(&db, admin_session).await;
        let admin_cookie =
            crate::routes::test_support::signed_session_cookie(&state, admin_session);
        let (status, body) = send_api_with_cookie(
            &app,
            "POST",
            "/api/tokens",
            &admin_cookie,
            Some(serde_json::json!({"name": "a", "permissions": ["admin"], "snake_access": "all"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // Admin token without admin permission requesting admin → subset 403
        // (the admin check passes: the user IS admin).
        let admin_token = mint(
            &db,
            admin,
            &[Permission::TokensManage],
            SnakeAccess::All,
            &[],
        )
        .await;
        let (status, body) = post(
            &app,
            &admin_token,
            serde_json::json!({"name": "sub", "permissions": ["admin"], "snake_access": "all"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(
            body,
            "requested permissions or snake access exceed this token's"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn list_tokens_shows_scope(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921808, false).await;
        let snake = create_snake(&db, user, "listed-snake").await;
        let token = mint(
            &db,
            user,
            &[Permission::TokensManage, Permission::SnakesRead],
            SnakeAccess::Selected,
            &[snake],
        )
        .await;

        let (status, body) = send_api(&app, "GET", "/api/tokens", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let listed = json.iter().find(|t| t["name"] == "test").unwrap();
        assert_eq!(
            listed["permissions"],
            serde_json::json!(["snakes.read", "tokens.manage"])
        );
        assert_eq!(listed["snake_access"], "selected");
        assert_eq!(listed["snake_ids"], serde_json::json!([snake.to_string()]));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn revoke_requires_permission_and_404s_for_others(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921809, false).await;
        let other = create_user(&db, 9921810, false).await;
        let zero = mint(&db, user, &[], SnakeAccess::All, &[]).await;
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

        let (status, body) = send_api(
            &app,
            "DELETE",
            &format!("/api/tokens/{}", target.token.id),
            Some(&zero),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body, "missing permission: tokens.manage");

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
    #[sqlx::test(migrations = "../migrations")]
    async fn a_token_revoked_mid_mint_leaves_no_live_copy(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921705, false).await;
        let creator = api_token::create_api_token(
            &db,
            user,
            "creator",
            &[Permission::TokensManage],
            SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();

        // The revoke lands after the request authenticated but before the copy
        // commits, as when the copy's INSERT queues behind a revoke's lock.
        sqlx::raw_sql(&format!(
            r"CREATE FUNCTION revoke_creator() RETURNS trigger LANGUAGE plpgsql AS $$
              BEGIN
                UPDATE api_tokens SET revoked_at = NOW()
                WHERE id = '{}' AND revoked_at IS NULL;
                RETURN NULL;
              END $$;
              CREATE TRIGGER revoke_creator AFTER INSERT ON api_tokens
              FOR EACH ROW EXECUTE FUNCTION revoke_creator();",
            creator.token.id
        ))
        .execute(&db)
        .await
        .unwrap();

        let (status, body) = post(
            &app,
            &creator.secret,
            serde_json::json!({
                "name": "copy",
                "permissions": ["tokens.manage"],
                "snake_access": "all",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body, "Invalid or revoked token");

        let live: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM api_tokens
               WHERE user_id = $1 AND revoked_at IS NULL"#,
            user
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(live, 0, "neither the creator nor its copy is live");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn narrow_token_cannot_revoke_a_wider_one(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921811, false).await;
        let snake_a = create_snake(&db, user, "revoke-a").await;
        let snake_b = create_snake(&db, user, "revoke-b").await;
        let mint_row = |permissions: &'static [Permission], access, snakes: Vec<Uuid>| {
            let db = db.clone();
            async move {
                api_token::create_api_token(&db, user, "row", permissions, access, &snakes)
                    .await
                    .unwrap()
            }
        };

        // The narrow token: tokens.manage, selected [A].
        let narrow = mint_row(
            &[Permission::TokensManage],
            SnakeAccess::Selected,
            vec![snake_a],
        )
        .await;
        let delete = |id: Uuid, bearer: String| {
            let app = app.clone();
            async move {
                send_api(
                    &app,
                    "DELETE",
                    &format!("/api/tokens/{id}"),
                    Some(&bearer),
                    None,
                )
                .await
            }
        };

        // Wider by permission, by `all` access, and by selection → 403, and
        // each target stays live.
        let more_perms = mint_row(
            &[Permission::TokensManage, Permission::ProfileRead],
            SnakeAccess::Selected,
            vec![snake_a],
        )
        .await;
        let all_access = mint_row(&[Permission::TokensManage], SnakeAccess::All, vec![]).await;
        let wider_selection = mint_row(
            &[Permission::TokensManage],
            SnakeAccess::Selected,
            vec![snake_a, snake_b],
        )
        .await;
        for target in [&more_perms, &all_access, &wider_selection] {
            let (status, body) = delete(target.token.id, narrow.secret.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(
                body,
                "cannot revoke a token with wider permissions or snake access than this token's"
            );
            assert!(
                api_token::get_user_token(&db, user, target.token.id)
                    .await
                    .unwrap()
                    .is_some()
            );
        }

        // Narrower (fewer permissions, empty selection) → 204.
        let narrower = mint_row(&[], SnakeAccess::Selected, vec![]).await;
        let (status, body) = delete(narrower.token.id, narrow.secret.clone()).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

        // A wider token revokes the narrow one; then the narrow token is dead.
        let (status, body) = delete(narrow.token.id, all_access.secret.clone()).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let (status, _) = delete(more_perms.token.id, narrow.secret.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A token can always revoke itself.
        let (status, body) = delete(wider_selection.token.id, wider_selection.secret.clone()).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

        // Sessions revoke anything.
        let session = crate::routes::test_support::create_user_session(&db, 9921812, false).await;
        let session_user = crate::routes::test_support::session_user_id(&db, session).await;
        let cookie = crate::routes::test_support::signed_session_cookie(&state, session);
        let full = api_token::create_api_token(
            &db,
            session_user,
            "full",
            Permission::ALL.as_slice(),
            SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();
        let (status, body) = send_api_with_cookie(
            &app,
            "DELETE",
            &format!("/api/tokens/{}", full.token.id),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn post_and_get_serialize_snake_ids_identically(db: PgPool) {
        let state = crate::state::AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921813, false).await;
        let mut snakes = [
            create_snake(&db, user, "order-1").await,
            create_snake(&db, user, "order-2").await,
            create_snake(&db, user, "order-3").await,
        ];
        let token = mint(
            &db,
            user,
            &[Permission::TokensManage],
            SnakeAccess::All,
            &[],
        )
        .await;

        // Request in descending order, with a duplicate.
        snakes.sort_unstable();
        let request: Vec<String> = snakes
            .iter()
            .rev()
            .chain(snakes.first())
            .map(Uuid::to_string)
            .collect();
        let (status, body) = post(
            &app,
            &token,
            serde_json::json!({"name": "ordered", "permissions": [], "snake_access": "selected", "snake_ids": request}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        let sorted: Vec<String> = snakes.iter().map(Uuid::to_string).collect();
        assert_eq!(created["snake_ids"], serde_json::json!(sorted));

        let (status, body) = send_api(&app, "GET", "/api/tokens", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        let listed = listed.iter().find(|t| t["id"] == created["id"]).unwrap();
        assert_eq!(listed["snake_ids"], created["snake_ids"]);
    }
}
