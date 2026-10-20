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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::api_token::{SnakeAccess, create_api_token};
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
}
