use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    models::api_token::{self, Permission},
    models::battlesnake::{
        self, Battlesnake, CreateBattlesnake, EngineRegion, UpdateBattlesnake, Visibility,
    },
    models::oauth,
    routes::auth::{ApiAuth, ApiUser},
    state::AppState,
};

/// Response format for snake endpoints
///
/// Fields are private so [`SnakeResponse::new`] is the only constructor: the
/// `url` field is included only when the auth context may read it.
#[derive(Debug, Serialize)]
pub struct SnakeResponse {
    id: Uuid,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    is_public: bool,
    engine_region: EngineRegion,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl SnakeResponse {
    /// url is included only for the caller's own snake, and only when the
    /// auth context may read it (snake.url.read + snake in access; sessions
    /// always). Never another user's URL, whatever the scope.
    pub fn new(auth: &ApiAuth, snake: Battlesnake) -> Self {
        let url = (snake.user_id == auth.user_id
            && auth.has(Permission::SnakeUrlRead)
            && auth.can_access_snake(snake.battlesnake_id))
        .then_some(snake.url);
        Self {
            id: snake.battlesnake_id,
            name: snake.name,
            url,
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
    ApiUser(auth): ApiUser,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::SnakesRead)?;

    let snakes = battlesnake::get_battlesnakes_by_user_id(&state.db, auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list snakes: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to list snakes".to_string(),
            )
        })?;

    // All of the user's snakes, with per-snake URL redaction.
    let response: Vec<SnakeResponse> = snakes
        .into_iter()
        .map(|s| SnakeResponse::new(&auth, s))
        .collect();
    Ok(Json(response))
}

/// POST /api/snakes - Create snake
pub async fn create_snake(
    State(state): State<AppState>,
    ApiUser(mut auth): ApiUser,
    Json(request): Json<CreateSnakeRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::SnakesCreate)?;

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
        auth.user_id,
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

    // A snake created through a `selected` token joins that token's list in
    // the same transaction: the snake never exists outside the access of
    // the token that created it.
    let mut tx = state.db.begin().await.map_err(|e| {
        tracing::error!("Failed to begin snake creation transaction: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create snake".to_string(),
        )
    })?;

    let snake = battlesnake::create_battlesnake(&mut *tx, auth.user_id, create_data)
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

    if let Some(scope) = auth.token_scope()
        && scope.snake_access == api_token::SnakeAccess::Selected
    {
        api_token::add_snake_to_token(&mut *tx, scope.token_id, snake.battlesnake_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to add snake to token {}: {:#}", scope.token_id, e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create snake".to_string(),
                )
            })?;
    }

    // A snake created through an OAuth token is stamped with that client:
    // `snake.delete` for OAuth tokens applies only to snakes their own
    // client created.
    if let Some(client_id) = auth.token_scope().and_then(|scope| scope.oauth_client_id) {
        battlesnake::set_created_by_client(&mut *tx, snake.battlesnake_id, client_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to stamp snake's creating client: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create snake".to_string(),
                )
            })?;
    }

    tx.commit().await.map_err(|e| {
        tracing::error!("Failed to commit snake creation: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create snake".to_string(),
        )
    })?;

    // The scope was loaded at auth time; mirror the row just committed so
    // the response below sees the freshly created snake.
    auth.record_selected_snake(snake.battlesnake_id);

    // Relay only clean names: flagged, blocked, and unchecked names never
    // reach Discord.
    if snake.visibility == Visibility::Public && decision == crate::moderation::Decision::Allow {
        state
            .discord
            .notify_snake_registered(&snake.name, &auth.github_login);
    }

    Ok((StatusCode::CREATED, Json(SnakeResponse::new(&auth, snake))))
}

/// GET /api/snakes/{id} - Get snake details
pub async fn get_snake(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Path(snake_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require(Permission::SnakesRead)?;

    let snake = battlesnake::get_battlesnake_by_id(&state.db, snake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get snake: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to get snake".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Snake not found".to_string()))?;

    // Enforce ownership - users can only view their own snakes via this endpoint
    if snake.user_id != auth.user_id {
        return Err((StatusCode::NOT_FOUND, "Snake not found".to_string()));
    }

    Ok(Json(SnakeResponse::new(&auth, snake)))
}

/// PUT /api/snakes/{id} - Update snake
pub async fn update_snake(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Path(snake_id): Path<Uuid>,
    Json(request): Json<UpdateSnakeRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Field-presence permission checks BEFORE any fetch/moderation: if any
    // touched field fails, the whole request is rejected and nothing applies.
    // An empty body is still a per-snake action — it falls through to the
    // `snake.edit` gate so a zero-scope credential can neither probe nor
    // no-op-write a snake outside its reach.
    let touches_url = request.url.is_some();
    let touches_edit =
        request.name.is_some() || request.is_public.is_some() || request.engine_region.is_some();
    if touches_url {
        auth.require_snake(snake_id, Permission::SnakeUrlWrite)?;
    }
    if touches_edit || !touches_url {
        auth.require_snake(snake_id, Permission::SnakeEdit)?;
    }

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
    if existing.user_id != auth.user_id {
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
            auth.user_id,
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

    let snake = battlesnake::update_battlesnake(&state.db, snake_id, auth.user_id, update_data)
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

    Ok(Json(SnakeResponse::new(&auth, snake)))
}

/// DELETE /api/snakes/{id} - Delete snake
pub async fn delete_snake(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Path(snake_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require_snake(snake_id, Permission::SnakeDelete)?;

    // OAuth tokens may only delete snakes their own client created. A
    // player-made snake is 403; a missing snake — or one owned by another
    // user — falls through so the response is `delete_battlesnake`'s 404
    // (matching personal-token behavior, and leaking no existence oracle).
    if let Some(client_id) = auth.token_scope().and_then(|scope| scope.oauth_client_id) {
        let creator = battlesnake::snake_creator_client(&state.db, snake_id, auth.user_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to load snake creator client: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to delete snake".to_string(),
                )
            })?;
        let created_by_this_client = matches!(creator, Some(Some(c)) if c == client_id);
        if !created_by_this_client && creator.is_some() {
            return Err((
                StatusCode::FORBIDDEN,
                "battlesnake was not created by this app".to_string(),
            ));
        }
    }

    let outcome = battlesnake::delete_battlesnake(&state.db, snake_id, auth.user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete snake: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to delete snake".to_string(),
            )
        })?;

    match outcome {
        battlesnake::DeleteBattlesnakeOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        battlesnake::DeleteBattlesnakeOutcome::NotFound => {
            Err((StatusCode::NOT_FOUND, "Snake not found".to_string()))
        }
        // Registered in a tournament that is open or running: withdraw first.
        battlesnake::DeleteBattlesnakeOutcome::InActiveTournament => Err((
            StatusCode::CONFLICT,
            "Snake is in an active tournament".to_string(),
        )),
    }
}

/// PUT /api/snakes/{id}/hosting — a hosting-provider client marks a
/// snake as hosted by it. Empty body; 204 on success.
///
/// Check order is pinned by route_coverage_tests: permission 403 first,
/// then snake existence/ownership 404 (the coverage table drives a
/// random UUID + exactly-listed personal token through here), then the
/// client-kind 403, then the domain 400, then the mark.
pub async fn mark_hosting(
    State(state): State<AppState>,
    ApiUser(auth): ApiUser,
    Path(snake_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    auth.require_snake(snake_id, Permission::SnakeUrlWrite)?;

    let snake = battlesnake::get_battlesnake_by_id(&state.db, snake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get snake for hosting mark: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mark hosting".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Snake not found".to_string()))?;

    // Ownership: no existence oracle for another user's snake.
    if snake.user_id != auth.user_id {
        return Err((StatusCode::NOT_FOUND, "Snake not found".to_string()));
    }

    // Only hosting-provider clients may mark; a personal token or a
    // standard client gets the same 403.
    let Some(client_id) = auth.token_scope().and_then(|scope| scope.oauth_client_id) else {
        return Err((
            StatusCode::FORBIDDEN,
            "only hosting provider apps can mark hosting".to_string(),
        ));
    };
    let client = oauth::get_client_by_id(&state.db, client_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to load OAuth client for hosting mark: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mark hosting".to_string(),
            )
        })?;
    // A dangling client id can't be a hosting client — same 403.
    let client = match client {
        Some(c) if c.kind == oauth::OauthClientKind::HostingProvider => c,
        _ => {
            return Err((
                StatusCode::FORBIDDEN,
                "only hosting provider apps can mark hosting".to_string(),
            ));
        }
    };

    // The mark is accepted only when the snake's CURRENT URL sits on one
    // of this client's registered domains.
    if !oauth::client_domain_covers_url(&state.db, client.id, &snake.url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check hosting domains: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mark hosting".to_string(),
            )
        })?
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "snake URL is not on a domain registered to this hosting provider".to_string(),
        ));
    }

    match battlesnake::mark_hosted_by(&state.db, snake_id, auth.user_id, client.id, &snake.url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to mark snake hosted: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mark hosting".to_string(),
            )
        })? {
        battlesnake::MarkHostingOutcome::Marked => Ok(StatusCode::NO_CONTENT),
        battlesnake::MarkHostingOutcome::AlreadyHosted => Err((
            StatusCode::CONFLICT,
            "battlesnake is already hosted by another provider".to_string(),
        )),
        battlesnake::MarkHostingOutcome::UrlChanged => Err((
            StatusCode::CONFLICT,
            "snake URL changed since the request; retry".to_string(),
        )),
        battlesnake::MarkHostingOutcome::NotFound => {
            Err((StatusCode::NOT_FOUND, "Snake not found".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::api_token::{NewApiToken, SnakeAccess, create_api_token};
    use crate::routes::test_support::{api_app, create_user, send_api};
    use crate::state::AppState;
    use sqlx::PgPool;

    async fn make_snake(db: &PgPool, user: Uuid, name: &str) -> Battlesnake {
        battlesnake::create_battlesnake(
            db,
            user,
            CreateBattlesnake {
                name: name.to_string(),
                url: format!("https://{name}.example"),
                visibility: Visibility::Private,
                engine_region: Default::default(),
            },
        )
        .await
        .unwrap()
    }

    async fn mint(
        db: &PgPool,
        user: Uuid,
        permissions: &[Permission],
        access: SnakeAccess,
        snake_ids: &[Uuid],
    ) -> String {
        create_api_token(db, user, "test", permissions, access, snake_ids)
            .await
            .unwrap()
            .secret
    }

    async fn request(
        app: &axum::Router,
        method: &str,
        path: &str,
        bearer: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let (status, body) = send_api(app, method, path, Some(bearer), body).await;
        let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body));
        (status, json)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn url_redacted_in_list_and_get_without_url_read(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        // create_user_session makes both the user and a session for it (one
        // INSERT path; github_id stays unique).
        let session = crate::routes::test_support::create_user_session(&db, 9921900, false).await;
        let user = crate::routes::test_support::session_user_id(&db, session).await;
        let snake_a = make_snake(&db, user, "redact-a").await;
        let _snake_b = make_snake(&db, user, "redact-b").await;

        // snakes.read, selected [A], no snake.url.read: both snakes listed
        // (snakes.read is account-wide), url key absent on both.
        let token = mint(
            &db,
            user,
            &[Permission::SnakesRead],
            SnakeAccess::Selected,
            &[snake_a.battlesnake_id],
        )
        .await;
        let (status, json) = request(&app, "GET", "/api/snakes", &token, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json.as_array().unwrap().len(), 2);
        for snake in json.as_array().unwrap() {
            assert!(snake.get("url").is_none(), "{snake}");
        }

        // GET A: metadata visible, url absent.
        let (status, json) = request(
            &app,
            "GET",
            &format!("/api/snakes/{}", snake_a.battlesnake_id),
            &token,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(json.get("url").is_none());
        assert_eq!(json["name"], "redact-a");

        // Adding snake.url.read: visible on A (in access), still absent on B.
        let token = mint(
            &db,
            user,
            &[Permission::SnakesRead, Permission::SnakeUrlRead],
            SnakeAccess::Selected,
            &[snake_a.battlesnake_id],
        )
        .await;
        let (status, json) = request(&app, "GET", "/api/snakes", &token, None).await;
        assert_eq!(status, StatusCode::OK);
        let a = json
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "redact-a")
            .unwrap();
        let b = json
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "redact-b")
            .unwrap();
        assert_eq!(a["url"], "https://redact-a.example");
        assert!(b.get("url").is_none(), "{b}");

        // Session (full power): every url visible.
        let cookie = crate::routes::test_support::signed_session_cookie(&state, session);
        let (status, body) = crate::routes::test_support::send_api_with_cookie(
            &app,
            "GET",
            "/api/snakes",
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.as_array()
                .unwrap()
                .iter()
                .all(|s| s.get("url").is_some())
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn put_response_redacts_url_without_url_read(db: PgPool) {
        // The regression test for the update-response leak: a token with
        // snake.edit (no snake.url.read) PUTs is_public and must get a
        // response with no url key.
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921901, false).await;
        let snake = make_snake(&db, user, "put-redact").await;

        let token = mint(&db, user, &[Permission::SnakeEdit], SnakeAccess::All, &[]).await;
        let (status, json) = request(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            &token,
            Some(serde_json::json!({"is_public": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json.get("url").is_none(), "{json}");
        assert_eq!(json["is_public"], true);

        // Same for a url-write-only token after a PUT {"url": ...}.
        let token = mint(
            &db,
            user,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;
        let (status, json) = request(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            &token,
            Some(serde_json::json!({"url": "https://changed.example"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json.get("url").is_none(), "{json}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn selected_token_created_snake_joins_its_list(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921902, false).await;

        let token = mint(
            &db,
            user,
            &[Permission::SnakesCreate, Permission::SnakesRead],
            SnakeAccess::Selected,
            &[],
        )
        .await;
        let token_id = sqlx::query_scalar!(
            "SELECT id FROM api_tokens WHERE user_id = $1 AND name = 'test'",
            user
        )
        .fetch_one(&db)
        .await
        .unwrap();

        let (status, json) = request(
            &app,
            "POST",
            "/api/snakes",
            &token,
            Some(serde_json::json!({
                "name": "born-selected",
                "url": "https://born.example",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        let created: Uuid = json["id"].as_str().unwrap().parse().unwrap();

        let rows: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM api_token_snakes WHERE api_token_id = $1"#,
            token_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(rows, 1, "the new snake must join the token's list");

        // And GET through the same token works.
        let (status, _json) =
            request(&app, "GET", &format!("/api/snakes/{created}"), &token, None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failed_join_rolls_back_the_created_snake(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921903, false).await;
        let token = mint(
            &db,
            user,
            &[Permission::SnakesCreate],
            SnakeAccess::Selected,
            &[],
        )
        .await;

        // Make the join insert fail after the snake insert succeeded.
        sqlx::query!(
            "CREATE FUNCTION fail_token_snake_insert() RETURNS trigger
             LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''injected failure''; END'"
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query!(
            "CREATE TRIGGER fail_token_snake_insert BEFORE INSERT ON api_token_snakes
             FOR EACH ROW EXECUTE FUNCTION fail_token_snake_insert()"
        )
        .execute(&db)
        .await
        .unwrap();

        let body = serde_json::json!({"name": "rolled-back", "url": "https://rb.example"});
        let (status, json) = request(&app, "POST", "/api/snakes", &token, Some(body.clone())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json}");
        let snakes: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM battlesnakes WHERE user_id = $1"#,
            user
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(snakes, 0, "the snake insert must roll back with the join");

        // Once the join can succeed, the same request creates the snake (no
        // 409 from a half-created orphan).
        sqlx::query!("DROP TRIGGER fail_token_snake_insert ON api_token_snakes")
            .execute(&db)
            .await
            .unwrap();
        let (status, json) = request(&app, "POST", "/api/snakes", &token, Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn snake_response_never_includes_another_users_url(db: PgPool) {
        let owner = create_user(&db, 9921904, false).await;
        let viewer = create_user(&db, 9921905, false).await;
        let snake_id = make_snake(&db, owner, "someone-elses").await.battlesnake_id;
        let fetch = || async {
            battlesnake::get_battlesnake_by_id(&db, snake_id)
                .await
                .unwrap()
                .unwrap()
        };
        let viewer = crate::models::user::get_user_by_id(&db, viewer)
            .await
            .unwrap()
            .unwrap();
        let owner = crate::models::user::get_user_by_id(&db, owner)
            .await
            .unwrap()
            .unwrap();

        // Sessions hold snake.url.read and every snake is "in access", so
        // only the ownership check keeps the URL out.
        let json =
            serde_json::to_value(SnakeResponse::new(&ApiAuth::session(viewer), fetch().await))
                .unwrap();
        assert!(json.get("url").is_none(), "{json}");

        let json =
            serde_json::to_value(SnakeResponse::new(&ApiAuth::session(owner), fetch().await))
                .unwrap();
        assert_eq!(json["url"], "https://someone-elses.example");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn selected_create_with_url_read_echoes_url(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921907, false).await;

        // The new snake joins the token's list in this same request, so a
        // selected token holding snake.url.read must see the URL it sent
        // echoed back — not a redacted 201.
        let token = mint(
            &db,
            user,
            &[Permission::SnakesCreate, Permission::SnakeUrlRead],
            SnakeAccess::Selected,
            &[],
        )
        .await;
        let (status, json) = request(
            &app,
            "POST",
            "/api/snakes",
            &token,
            Some(serde_json::json!({
                "name": "echo-url",
                "url": "https://echo.example",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        assert_eq!(json["url"], "https://echo.example", "{json}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn selected_create_without_url_read_omits_url(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921908, false).await;

        // Same shape as above minus snake.url.read: 201, url key absent.
        let token = mint(
            &db,
            user,
            &[Permission::SnakesCreate],
            SnakeAccess::Selected,
            &[],
        )
        .await;
        let (status, json) = request(
            &app,
            "POST",
            "/api/snakes",
            &token,
            Some(serde_json::json!({
                "name": "no-echo",
                "url": "https://no-echo.example",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        assert!(json.get("url").is_none(), "{json}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn selected_token_edit_outside_selection_403(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921903, false).await;
        let inside = make_snake(&db, user, "edit-inside").await;
        let outside = make_snake(&db, user, "edit-outside").await;

        let token = mint(
            &db,
            user,
            &[Permission::SnakeEdit],
            SnakeAccess::Selected,
            &[inside.battlesnake_id],
        )
        .await;

        let (status, body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", outside.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({"name": "hijack"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            format!(
                "battlesnake {} is not in this token's access",
                outside.battlesnake_id
            )
        );
        // Nothing applied.
        let name: String = sqlx::query_scalar!(
            "SELECT name FROM battlesnakes WHERE battlesnake_id = $1",
            outside.battlesnake_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(name, "edit-outside");

        // Inside: 200.
        let (status, _body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", inside.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({"is_public": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mixed_field_put_rejects_whole_request(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921904, false).await;
        let snake = make_snake(&db, user, "mixed").await;

        // snake.edit without snake.url.write: name+url PUT → 403, nothing applied.
        let token = mint(&db, user, &[Permission::SnakeEdit], SnakeAccess::All, &[]).await;
        let (status, body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({"name": "renamed", "url": "https://evil.example"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: snake.url.write");
        let name = snake_name(&db, snake.battlesnake_id).await;
        assert_eq!(name, "mixed");
        assert_eq!(
            snake_url(&db, snake.battlesnake_id).await,
            "https://mixed.example"
        );

        // Inverse: url-write without edit on name+url → 403, nothing applied.
        let token = mint(
            &db,
            user,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;
        let (status, body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({"name": "renamed", "url": "https://evil.example"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: snake.edit");
        let name = snake_name(&db, snake.battlesnake_id).await;
        assert_eq!(name, "mixed");
        assert_eq!(
            snake_url(&db, snake.battlesnake_id).await,
            "https://mixed.example"
        );

        // Empty body PUT is still a per-snake action: without `snake.edit`
        // it is a 403 (a zero-permission credential must not read back snake
        // metadata or bump `updated_at`).
        let token = mint(&db, user, &[], SnakeAccess::All, &[]).await;
        let (status, body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: snake.edit");

        // With `snake.edit` the empty PUT keeps today's behavior: 200 with
        // the revalidated-unchanged snake.
        let token = mint(&db, user, &[Permission::SnakeEdit], SnakeAccess::All, &[]).await;
        let (status, _body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn delete_outside_selection_403_inside_204(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921905, false).await;
        let inside = make_snake(&db, user, "del-inside").await;
        let outside = make_snake(&db, user, "del-outside").await;

        let token = mint(
            &db,
            user,
            &[Permission::SnakeDelete],
            SnakeAccess::Selected,
            &[inside.battlesnake_id],
        )
        .await;

        let (status, body) = send_api(
            &app,
            "DELETE",
            &format!("/api/snakes/{}", outside.battlesnake_id),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            format!(
                "battlesnake {} is not in this token's access",
                outside.battlesnake_id
            )
        );

        let (status, _body) = send_api(
            &app,
            "DELETE",
            &format!("/api/snakes/{}", inside.battlesnake_id),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn create_requires_snakes_create_permission(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921906, false).await;
        let token = mint(&db, user, &[], SnakeAccess::All, &[]).await;

        let (status, body) = send_api(
            &app,
            "POST",
            "/api/snakes",
            Some(&token),
            Some(serde_json::json!({
                "name": "blocked",
                "url": "https://blocked.example",
            })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: snakes.create");
    }

    async fn snake_name(db: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar!(
            "SELECT name FROM battlesnakes WHERE battlesnake_id = $1",
            id
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn snake_url(db: &PgPool, id: Uuid) -> String {
        sqlx::query_scalar!("SELECT url FROM battlesnakes WHERE battlesnake_id = $1", id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    // Pinned by PR review round 1: an empty-body PUT is still a per-snake
    // action. The task: "A per-snake action on a snake outside the token's
    // access returns 403 saying so", and a zero-permission token must not
    // get a 200 (with snake metadata) from an authenticated /api route.
    #[sqlx::test(migrations = "../migrations")]
    async fn empty_put_outside_selection_or_without_permissions_is_403(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921990, false).await;
        let inside = make_snake(&db, user, "empty-put-in").await;
        let outside = make_snake(&db, user, "empty-put-out").await;

        let selected = mint(
            &db,
            user,
            &[Permission::SnakeEdit],
            SnakeAccess::Selected,
            &[inside.battlesnake_id],
        )
        .await;
        let (status, json) = request(
            &app,
            "PUT",
            &format!("/api/snakes/{}", outside.battlesnake_id),
            &selected,
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "body: {json}");
        assert_eq!(
            json,
            serde_json::Value::String(format!(
                "battlesnake {} is not in this token's access",
                outside.battlesnake_id
            ))
        );

        let zero = mint(&db, user, &[], SnakeAccess::All, &[]).await;
        let (status, json) = request(
            &app,
            "PUT",
            &format!("/api/snakes/{}", inside.battlesnake_id),
            &zero,
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "body: {json}");
    }

    // ---- Hosting mark endpoint (PUT /api/snakes/{id}/hosting) ----

    async fn make_hosting_client(
        db: &PgPool,
        name: &str,
        kind: oauth::OauthClientKind,
        domains: &[&str],
    ) -> Uuid {
        let created = oauth::create_client(
            db,
            oauth::CreateOauthClientInput {
                name: name.to_string(),
                logo_url: None,
                homepage_url: Some(format!("https://{name}.example")),
                onboarding_url: Some(format!("https://{name}.example/start")),
                description: None,
                kind,
                redirect_uris: vec![format!("https://{name}.example/cb")],
                hosting_domains: domains.iter().map(|d| d.to_string()).collect(),
            },
        )
        .await
        .unwrap();
        created.client.id
    }

    /// An OAuth-client token: a grant row + a token bound to it (the
    /// established pattern — see authenticate_token_loads_scope and the
    /// oauth_tests grant inserts).
    async fn mint_client_token(
        db: &PgPool,
        user_id: Uuid,
        client_id: Uuid,
        permissions: &[Permission],
        access: SnakeAccess,
        snake_ids: &[Uuid],
    ) -> String {
        let mut tx = db.begin().await.unwrap();
        let grant_id: Uuid = sqlx::query_scalar(
            "INSERT INTO oauth_grants (user_id, oauth_client_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(client_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let NewApiToken { secret, .. } = crate::models::api_token::create_api_token_on(
            &mut tx,
            user_id,
            "client-token",
            permissions,
            access,
            snake_ids,
            Some(grant_id),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        secret
    }

    async fn hosted_mark(db: &PgPool, snake_id: Uuid) -> Option<Uuid> {
        sqlx::query_scalar!(
            "SELECT hosted_by_oauth_client_id FROM battlesnakes WHERE battlesnake_id = $1",
            snake_id
        )
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn make_snake_with_url(db: &PgPool, user: Uuid, name: &str, url: &str) -> Battlesnake {
        battlesnake::create_battlesnake(
            db,
            user,
            CreateBattlesnake {
                name: name.to_string(),
                url: url.to_string(),
                visibility: Visibility::Private,
                engine_region: Default::default(),
            },
        )
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mark_accepted_on_domain_and_subdomain(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921910, false).await;
        let client = make_hosting_client(
            &db,
            "Softmax",
            oauth::OauthClientKind::HostingProvider,
            &["softmax.com"],
        )
        .await;

        let token = mint_client_token(
            &db,
            user,
            client,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;

        // The accepted rows of the shared vector table: equal host,
        // subdomain, userinfo-before-host, port-dropped, uppercase-folded.
        let vectors = [
            ("softmax-accept-eq", "https://softmax.com/s"),
            ("softmax-accept-sub", "https://api.softmax.com/s"),
            ("softmax-accept-user", "https://user@api.softmax.com/s"),
            ("softmax-accept-port", "https://api.softmax.com:8443/s"),
            ("softmax-accept-upper", "https://API.SoftMax.com/s"),
        ];
        for (name, url) in vectors {
            let snake = make_snake_with_url(&db, user, name, url).await;
            let (status, body) = send_api(
                &app,
                "PUT",
                &format!("/api/snakes/{}/hosting", snake.battlesnake_id),
                Some(&token),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{url}: body: {body}");
            assert_eq!(hosted_mark(&db, snake.battlesnake_id).await, Some(client));
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mark_rejected_on_non_matching_and_lookalike_hosts(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921911, false).await;
        let client = make_hosting_client(
            &db,
            "Softmax",
            oauth::OauthClientKind::HostingProvider,
            &["softmax.com"],
        )
        .await;
        let token = mint_client_token(
            &db,
            user,
            client,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;

        // Lookalikes (label boundary), userinfo trick, trailing dot,
        // IDN-punycode, unrelated host, IP literal.
        let vectors = [
            ("reject-unrelated", "https://example.com/s"),
            ("reject-suffix", "https://softmax.com.evil.io/s"),
            ("reject-prefix", "https://evilsoftmax.com/s"),
            ("reject-userinfo", "https://softmax.com@evil.io/s"),
            ("reject-trailing-dot", "https://softmax.com./s"),
            (
                "reject-idn",
                "https://softm\u{0430}x.com/s", // Cyrillic а → xn--softmx-7nf.com
            ),
            ("reject-ip", "https://1.2.3.4/s"),
        ];
        for (name, url) in vectors {
            let snake = make_snake_with_url(&db, user, name, url).await;
            let (status, body) = send_api(
                &app,
                "PUT",
                &format!("/api/snakes/{}/hosting", snake.battlesnake_id),
                Some(&token),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{url}: body: {body}");
            assert_eq!(hosted_mark(&db, snake.battlesnake_id).await, None);
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mark_rejected_for_standard_client(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921912, false).await;
        let client =
            make_hosting_client(&db, "Regular", oauth::OauthClientKind::Standard, &[]).await;
        let snake =
            make_snake_with_url(&db, user, "standard-client-snake", "https://softmax.com/s").await;
        let token = mint_client_token(
            &db,
            user,
            client,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;

        let (status, body) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}/hosting", snake.battlesnake_id),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "only hosting provider apps can mark hosting");
        assert_eq!(hosted_mark(&db, snake.battlesnake_id).await, None);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mark_rejected_without_url_write(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921913, false).await;
        let client = make_hosting_client(
            &db,
            "Softmax",
            oauth::OauthClientKind::HostingProvider,
            &["softmax.com"],
        )
        .await;
        let snake =
            make_snake_with_url(&db, user, "no-url-write-snake", "https://softmax.com/s").await;
        let path = format!("/api/snakes/{}/hosting", snake.battlesnake_id);

        // Hosting-client token WITHOUT snake.url.write (only snakes.read).
        let token = mint_client_token(
            &db,
            user,
            client,
            &[Permission::SnakesRead],
            SnakeAccess::All,
            &[],
        )
        .await;
        let (status, body) = send_api(&app, "PUT", &path, Some(&token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: snake.url.write");

        // Personal token with snake.url.write but no client: 403 (kind).
        let token = mint(
            &db,
            user,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;
        let (status, body) = send_api(&app, "PUT", &path, Some(&token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "only hosting provider apps can mark hosting");

        // Zero-permission personal token: 403 naming the permission.
        let token = mint(&db, user, &[], SnakeAccess::All, &[]).await;
        let (status, body) = send_api(&app, "PUT", &path, Some(&token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "missing permission: snake.url.write");

        assert_eq!(hosted_mark(&db, snake.battlesnake_id).await, None);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mark_conflict_idempotency_and_not_found(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921914, false).await;
        let other = create_user(&db, 9921915, false).await;
        let client_a = make_hosting_client(
            &db,
            "HostA",
            oauth::OauthClientKind::HostingProvider,
            &["a.example"],
        )
        .await;
        let client_b = make_hosting_client(
            &db,
            "HostB",
            oauth::OauthClientKind::HostingProvider,
            &["b.example"],
        )
        .await;
        let snake = make_snake_with_url(&db, user, "conflict-snake", "https://a.example/s").await;
        let path = format!("/api/snakes/{}/hosting", snake.battlesnake_id);

        let token_a = mint_client_token(
            &db,
            user,
            client_a,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;
        let token_b = mint_client_token(
            &db,
            user,
            client_b,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;

        // Displacement needs two clients that BOTH cover the URL: two
        // clients sharing one registered domain.
        let shared_snake =
            make_snake_with_url(&db, user, "shared-domain-snake", "https://shared.example/s").await;
        let shared_a = make_hosting_client(
            &db,
            "SharedA",
            oauth::OauthClientKind::HostingProvider,
            &["shared.example"],
        )
        .await;
        let shared_b = make_hosting_client(
            &db,
            "SharedB",
            oauth::OauthClientKind::HostingProvider,
            &["shared.example"],
        )
        .await;
        let shared_path = format!("/api/snakes/{}/hosting", shared_snake.battlesnake_id);
        let token_sa = mint_client_token(
            &db,
            user,
            shared_a,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;
        let token_sb = mint_client_token(
            &db,
            user,
            shared_b,
            &[Permission::SnakeUrlWrite],
            SnakeAccess::All,
            &[],
        )
        .await;

        // Idempotency + conflict: A marks 204, B 409, A re-marks 204.
        let (status, _) = send_api(&app, "PUT", &shared_path, Some(&token_sa), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, body) = send_api(&app, "PUT", &shared_path, Some(&token_sb), None).await;
        assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
        assert_eq!(body, "battlesnake is already hosted by another provider");
        let (status, _) = send_api(&app, "PUT", &shared_path, Some(&token_sa), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            hosted_mark(&db, shared_snake.battlesnake_id).await,
            Some(shared_a)
        );

        // B's token on A's snake (domain not covered): 400, not 409.
        let (status, body) = send_api(&app, "PUT", &path, Some(&token_b), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

        // A's own mark on its own-domain snake still works after all that.
        let (status, _) = send_api(&app, "PUT", &path, Some(&token_a), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Unknown snake and another user's snake: 404.
        let (status, _) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}/hosting", Uuid::new_v4()),
            Some(&token_a),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let foreign = make_snake_with_url(&db, other, "foreign-snake", "https://a.example/s").await;
        let (status, _) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}/hosting", foreign.battlesnake_id),
            Some(&token_a),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn url_update_clears_hosting_mark_through_api(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);
        let user = create_user(&db, 9921916, false).await;
        let client = make_hosting_client(
            &db,
            "Softmax",
            oauth::OauthClientKind::HostingProvider,
            &["softmax.com"],
        )
        .await;
        let snake =
            make_snake_with_url(&db, user, "api-clear-snake", "https://softmax.com/s").await;
        assert_eq!(
            battlesnake::mark_hosted_by(
                &db,
                snake.battlesnake_id,
                user,
                client,
                "https://softmax.com/s",
            )
            .await
            .unwrap(),
            battlesnake::MarkHostingOutcome::Marked
        );

        // API update to an off-domain URL: 200, mark cleared.
        let token = mint(
            &db,
            user,
            &[Permission::SnakeUrlWrite, Permission::SnakeEdit],
            SnakeAccess::All,
            &[],
        )
        .await;
        let (status, _) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({"url": "https://elsewhere.example"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hosted_mark(&db, snake.battlesnake_id).await, None);

        // API update back on-domain: still unmarked (marks only come from
        // the endpoint).
        let (status, _) = send_api(
            &app,
            "PUT",
            &format!("/api/snakes/{}", snake.battlesnake_id),
            Some(&token),
            Some(serde_json::json!({"url": "https://softmax.com/back"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hosted_mark(&db, snake.battlesnake_id).await, None);
    }
}
