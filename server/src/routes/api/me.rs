use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use uuid::Uuid;

use crate::{routes::auth::ApiUser, state::AppState};

/// Response for GET /api/me — the authenticated user's public profile.
///
/// Deliberately excludes email, GitHub OAuth token, is_admin, themes, and
/// backstory (MeResponse is a projection, never `User` itself).
#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub id: Uuid,
    pub github_login: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub pronouns: Option<String>,
    pub country: Option<String>,
    pub profile_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// GET /api/me - profile of the authenticated user (Bearer token or session).
pub async fn me(State(state): State<AppState>, ApiUser(user): ApiUser) -> impl IntoResponse {
    let base_url = state.config.base_url.trim_end_matches('/');
    Json(MeResponse {
        id: user.user_id,
        github_login: user.github_login.clone(),
        display_name: user.public_name().to_string(),
        avatar_url: user.github_avatar_url.clone(),
        pronouns: non_empty(&user.pronouns),
        country: non_empty(&user.country),
        profile_url: format!("{base_url}/users/{}", user.github_login),
        created_at: user.created_at,
    })
}

/// The DB stores NOT NULL DEFAULT '' for profile fields; expose empties as null.
fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::models::api_token::{create_api_token, revoke_token};
    use crate::models::session::SESSION_COOKIE_NAME;
    use crate::routes::test_support::{create_user_session, signed_session_cookie};
    use crate::state::AppState;

    fn app(state: &AppState) -> axum::Router {
        crate::routes::routes(state.clone())
            .expect("asset router")
            .layer(tower_cookies::CookieManagerLayer::new())
    }

    async fn send(
        app: &axum::Router,
        path: &str,
        bearer: Option<&str>,
        session_cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .uri(path)
            .header(header::ORIGIN, "https://example.com");
        if let Some(token) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        // `session_cookie` is the *signed (encrypted) value only* — the helper
        // owns the cookie name (same convention as stats.rs's route_response).
        if let Some(value) = session_cookie {
            builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE_NAME}={value}"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// Insert a fully-populated user; returns the user id.
    async fn create_full_user(pool: &PgPool) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_avatar_url, github_email, display_name, pronouns, country, github_access_token)
             VALUES (9961001, 'gh-me', 'https://avatars.example/octo.png', 'octo@example.com', 'Display Person', 'they/them', 'CA', '')
             RETURNING user_id",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn valid_bearer_token_returns_full_profile(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let app = app(&state);
        let user_id = create_full_user(&pool).await;
        let token = create_api_token(&pool, user_id, "test").await.unwrap();

        let (status, body) = send(&app, "/api/me", Some(&token.secret), None).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["id"].as_str().unwrap(), user_id.to_string());
        assert_eq!(json["github_login"], "gh-me");
        assert_eq!(json["display_name"], "Display Person");
        assert_eq!(json["avatar_url"], "https://avatars.example/octo.png");
        assert_eq!(json["pronouns"], "they/them");
        assert_eq!(json["country"], "CA");
        assert_eq!(json["profile_url"], "http://localhost:3000/users/gh-me");
        let created_at = json["created_at"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(created_at).is_ok());

        // The user has `github_email` set — it must not leak into the response.
        assert!(json.get("email").is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn session_cookie_returns_profile(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let app = app(&state);
        let session = create_user_session(&pool, 9961002, false).await;
        let cookie = signed_session_cookie(&state, session);

        let (status, body) = send(&app, "/api/me", None, Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["github_login"], "test-user-9961002");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn no_auth_returns_401(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let app = app(&state);

        let (status, _) = send(&app, "/api/me", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn revoked_token_returns_401(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let app = app(&state);
        let user_id = create_full_user(&pool).await;
        let token = create_api_token(&pool, user_id, "test").await.unwrap();
        revoke_token(&pool, token.token.id, user_id).await.unwrap();

        let (status, _) = send(&app, "/api/me", Some(&token.secret), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn empty_pronouns_and_country_serialize_as_null(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let app = app(&state);
        // Rely on column defaults: display_name NULL, pronouns '', country ''.
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (9961003, 'gh-bare', '')
             RETURNING user_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let token = create_api_token(&pool, user_id, "test").await.unwrap();

        let (status, body) = send(&app, "/api/me", Some(&token.secret), None).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(json["pronouns"].is_null());
        assert!(json["country"].is_null());
        // Empty display_name falls back to the GitHub login.
        assert_eq!(json["display_name"], "gh-bare");
    }
}
