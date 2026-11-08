//! The CLI login callback must never put the minted API token secret in a
//! URL. The token page renders as the callback's own 200 response (marked
//! `no-store`), the old `/auth/cli-token` route is gone, and replaying the
//! callback (refresh/back) fails the OAuth state check instead of re-showing
//! or re-minting a token.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

use crate::{
    github::auth::GitHubOAuthConfig,
    models::session::{self, SESSION_COOKIE_NAME},
    routes::test_support::{create_user_session, signed_session_cookie},
    state::AppState,
};

const MOCK_GITHUB_ID: i64 = 424242;

/// Static GitHub endpoints on a wiremock server: token exchange and user
/// fetch. Both live under the server root; the callback only ever hits
/// `token_url` and `{api_url}/user`.
async fn github_mocks() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "mock_token_xyz",
            "token_type": "bearer",
            "scope": "",
        })))
        .mount(&server)
        .await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": MOCK_GITHUB_ID,
            "login": "cli-login",
            "name": "CLI User",
            "email": null,
            "avatar_url": "https://example.com/a.png",
        })))
        .mount(&server)
        .await;
    server
}

struct Fixture {
    state: AppState,
    session_id: Uuid,
    app: axum::Router,
}

impl Fixture {
    /// Real router plus an anonymous session primed with the OAuth state.
    /// `is_cli` picks the CLI or web login branch.
    async fn new(pool: PgPool, github: &MockServer, is_cli: bool) -> Self {
        let mut state = AppState::test_from_pool(pool.clone());
        // test_from_pool builds a uniquely-owned Arc<AppConfig>, so get_mut
        // succeeds (precedent: snake_health_sweeper tests).
        let config = std::sync::Arc::get_mut(&mut state.config).expect("unique config Arc");
        config.github = Some(GitHubOAuthConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://arena.example/auth/github/callback".into(),
            oauth_url: github.uri(),
            token_url: format!("{}/login/oauth/access_token", github.uri()),
            api_url: github.uri(),
        });
        let session = session::create_session(&pool).await.unwrap();
        let session_id = session.session_id;
        session::set_github_oauth_state_with_cli(
            &pool,
            session_id,
            "test-state".to_string(),
            is_cli,
        )
        .await
        .unwrap();

        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router();
        Self {
            state,
            session_id,
            app,
        }
    }

    fn callback_request(&self) -> Request<Body> {
        Request::get("/auth/github/callback?code=mock_code_1&state=test-state")
            .header(
                header::COOKIE,
                format!(
                    "{}={}",
                    SESSION_COOKIE_NAME,
                    signed_session_cookie(&self.state, self.session_id)
                ),
            )
            .body(Body::empty())
            .unwrap()
    }
}

/// All maximal runs of exactly 64 ASCII hex characters in `body`.
///
/// The token secret is a 64-hex string bracketed by HTML tag characters in
/// the token box, so it appears as exactly one such run. Everything else on
/// the page (CSS colors, UUIDs, timestamps) is too short or hex-broken.
fn hex_runs(body: &str) -> Vec<&str> {
    body.split(|c: char| !c.is_ascii_hexdigit())
        .filter(|run| run.len() == 64)
        .collect()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[sqlx::test(migrations = "../migrations")]
async fn cli_callback_renders_token_page_without_url_secret(pool: PgPool) {
    let github = github_mocks().await;
    let fx = Fixture::new(pool.clone(), &github, true).await;

    let response = fx.app.clone().oneshot(fx.callback_request()).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get(header::LOCATION).is_none(),
        "token must never travel in a redirect URL"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

    let body = body_string(response).await;
    assert!(body.contains("CLI Authentication Successful"), "{body}");
    assert!(body.contains("only be shown once"), "{body}");
    assert!(!body.contains("/auth/cli-token"), "{body}");
    // The nav renders logged in: the inline PageFactory carries the user.
    assert!(body.contains("cli-login"), "{body}");

    let runs = hex_runs(&body);
    assert_eq!(
        runs.len(),
        1,
        "expected exactly the token secret in the page: {runs:?}"
    );
    let secret = runs[0];

    // The shown secret is the freshly minted, valid token.
    let user_id: Uuid = sqlx::query_scalar!(
        "SELECT user_id FROM users WHERE external_github_id = $1",
        MOCK_GITHUB_ID
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let validated = crate::models::api_token::authenticate_token(&pool, secret)
        .await
        .unwrap();
    let (validated_user, validated_permissions) = validated
        .map(|auth| (Some(auth.user_id), auth.permissions.len()))
        .unwrap_or((None, 0));
    assert_eq!(validated_user, Some(user_id));
    // CLI tokens are full-power: all 12 non-admin permissions (the fixture
    // user is not an admin).
    assert_eq!(validated_permissions, 12);
}

#[sqlx::test(migrations = "../migrations")]
async fn cli_callback_refresh_does_not_regrant(pool: PgPool) {
    let github = github_mocks().await;
    let fx = Fixture::new(pool.clone(), &github, true).await;

    let first = fx.app.clone().oneshot(fx.callback_request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    // Refreshing the callback re-submits code+state; the state was cleared
    // by the first request, so this must fail without minting or showing a
    // token.
    let replay = fx.app.clone().oneshot(fx.callback_request()).await.unwrap();
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let body = body_string(replay).await;
    assert!(
        hex_runs(&body).is_empty(),
        "no token secret may appear on refresh: {body}"
    );

    // Exactly one token was ever minted.
    let tokens = sqlx::query_scalar!("SELECT COUNT(*) FROM api_tokens WHERE name = 'arena-cli'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, Some(1));
}

#[sqlx::test(migrations = "../migrations")]
async fn cli_token_route_is_gone(pool: PgPool) {
    let github = github_mocks().await;
    let fx = Fixture::new(pool, &github, true).await;

    let response = fx
        .app
        .clone()
        .oneshot(
            Request::get("/auth/cli-token?token=abc123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// DEV-2185: login rotates the session — new id, old row deleted, fresh
/// 30-day cookie re-issued on the response, saved post-login redirect
/// carried, form draft deliberately NOT carried.
#[sqlx::test(migrations = "../migrations")]
async fn web_login_rotates_session_and_reissues_cookie(pool: PgPool) {
    let github = github_mocks().await;
    let fx = Fixture::new(pool.clone(), &github, false).await;

    // Park state on the OLD session: a saved redirect plus a sentinel form
    // draft (nothing clears it before login, so its absence on the new row
    // pins rotation's own omission).
    session::set_post_login_redirect(&pool, fx.session_id, "/welcome-back".to_string())
        .await
        .unwrap();
    session::set_error_flash_with_form_data(
        &pool,
        fx.session_id,
        "validation failed".to_string(),
        serde_json::json!({"name": "sentinel"}),
    )
    .await
    .unwrap();

    let response = fx.app.clone().oneshot(fx.callback_request()).await.unwrap();

    // The redirect was carried by rotation and consumed by the callback.
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/welcome-back");

    // Find the rotated row.
    let new_session_id: Uuid = sqlx::query_scalar!(
        "SELECT s.session_id FROM sessions s
         JOIN users u ON u.user_id = s.user_id
         WHERE u.external_github_id = $1
         ORDER BY s.created_at DESC
         LIMIT 1",
        MOCK_GITHUB_ID
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(new_session_id, fx.session_id, "login must rotate the id");

    let rotated = session::get_active_session_by_id(&pool, new_session_id)
        .await
        .unwrap()
        .expect("rotated session exists");
    assert!(rotated.user_id.is_some());
    assert!(rotated.expires_at > chrono::Utc::now() + chrono::Duration::days(29));
    let (redirect, draft): (Option<String>, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT post_login_redirect::text, pending_form_data::jsonb FROM sessions WHERE session_id = $1",
    )
    .bind(new_session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(redirect.is_none(), "redirect consumed by the callback: {redirect:?}");
    assert!(draft.is_none(), "form draft must NOT carry across rotation");

    // The old row is gone.
    let old: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM sessions WHERE session_id = $1"#,
        fx.session_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old, 0);

    // A fresh cookie naming the new session rides the response.
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("session Set-Cookie on login");
    assert!(set_cookie.starts_with("arena_session_id="), "{set_cookie}");
    assert!(set_cookie.contains("Max-Age=2592000"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/"), "{set_cookie}");
    let parsed = tower_cookies::Cookie::parse(set_cookie.to_string()).unwrap();
    let cookies = tower_cookies::Cookies::default();
    cookies.private(&fx.state.cookie_key.0).add(parsed);
    let decoded = cookies
        .private(&fx.state.cookie_key.0)
        .get(SESSION_COOKIE_NAME)
        .expect("decodable signed cookie");
    assert_eq!(decoded.value(), new_session_id.to_string());

    // The rotation actually logged the browser in: /me renders the mock
    // user's login when driven with the new session id.
    let cookie = signed_session_cookie(&fx.state, new_session_id);
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::get("/me")
                .header(header::COOKIE, format!("{SESSION_COOKIE_NAME}={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("cli-login"), "{body}");
}

/// DEV-2185: a failed sign-out must not show "You have been logged out".
/// The trigger scopes the failure to the sign-out transition ONLY
/// (user_id -> NULL): set_flash_message is also an UPDATE on sessions and
/// must keep working — that's what makes this test discriminate.
#[sqlx::test(migrations = "../migrations")]
async fn logout_failure_logs_and_shows_no_success_flash(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let app = crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router();
    let session_id = create_user_session(&pool, 424243, false).await;
    let cookie = signed_session_cookie(&state, session_id);

    sqlx::query(
        "CREATE FUNCTION sessions_signout_boom() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'boom'; END $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_sessions_signout BEFORE UPDATE ON sessions
         FOR EACH ROW
         WHEN (OLD.user_id IS NOT NULL AND NEW.user_id IS NULL)
         EXECUTE FUNCTION sessions_signout_boom()",
    )
    .execute(&pool)
    .await
    .unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::get("/auth/logout")
                .header(header::COOKIE, format!("{SESSION_COOKIE_NAME}={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/");

    // Sign-out really failed, and no success flash was written.
    let (user_id, flash): (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT user_id::uuid, flash_message FROM sessions WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(user_id.is_some(), "sign-out failed, user must still be attached");
    assert_eq!(flash, None, "no 'You have been logged out' flash on failure");
}

/// DEV-2185 happy-path sibling: a successful logout clears the user, the
/// form draft, and writes the flash.
#[sqlx::test(migrations = "../migrations")]
async fn logout_success_writes_flash(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let app = crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router();
    let session_id = create_user_session(&pool, 424244, false).await;
    let cookie = signed_session_cookie(&state, session_id);

    let response = app
        .clone()
        .oneshot(
            Request::get("/auth/logout")
                .header(header::COOKIE, format!("{SESSION_COOKIE_NAME}={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/");

    let (user_id, flash, draft): (Option<Uuid>, Option<String>, Option<serde_json::Value>) =
        sqlx::query_as(
            "SELECT user_id::uuid, flash_message, pending_form_data::jsonb FROM sessions WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(user_id.is_none());
    assert_eq!(flash.as_deref(), Some("You have been logged out"));
    assert!(draft.is_none());
}

#[sqlx::test(migrations = "../migrations")]
async fn web_callback_still_redirects_home(pool: PgPool) {
    let github = github_mocks().await;
    let fx = Fixture::new(pool.clone(), &github, false).await;

    let response = fx.app.clone().oneshot(fx.callback_request()).await.unwrap();

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/");

    // Web login mints no arena-cli token.
    let tokens = sqlx::query_scalar!("SELECT COUNT(*) FROM api_tokens WHERE name = 'arena-cli'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, Some(0));
}

/// DEV-2061: the `user_authenticated` event carries `new_user` — `true` on
/// the first callback for a GitHub identity (INSERT path: created_at ==
/// updated_at), `false` on the second (the UPDATE trigger has since moved
/// updated_at). Asserted through the shared global capture, filtered by
/// the user's id.
#[sqlx::test(migrations = "../migrations")]
async fn user_authenticated_event_carries_new_user(pool: PgPool) {
    let capture = crate::test_capture::Capture::shared();
    let github = github_mocks().await;

    // First callback: fresh user.
    let fx = Fixture::new(pool.clone(), &github, false).await;
    let response = fx.app.clone().oneshot(fx.callback_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let user_id: Uuid = sqlx::query_scalar!(
        "SELECT user_id FROM users WHERE external_github_id = $1",
        MOCK_GITHUB_ID
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let events = capture.events("user_authenticated", user_id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].get("new_user").map(String::as_str),
        Some("true"),
        "{events:?}"
    );

    // Second callback for the same GitHub identity: a fresh session+state,
    // but an existing user.
    let fx2 = Fixture::new(pool.clone(), &github, false).await;
    let response = fx2
        .app
        .clone()
        .oneshot(fx2.callback_request())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let events = capture.events("user_authenticated", user_id);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(
        events[1].get("new_user").map(String::as_str),
        Some("false"),
        "{events:?}"
    );
}
