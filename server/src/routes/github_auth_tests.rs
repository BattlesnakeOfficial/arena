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
    routes::test_support::signed_session_cookie,
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
