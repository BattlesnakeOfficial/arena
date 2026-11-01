//! Helpers for driving the real router as a signed-in user in route tests.

use uuid::Uuid;

use crate::state::AppState;

/// Insert a user; returns the user id.
pub async fn create_user(db: &sqlx::PgPool, github_id: i64, is_admin: bool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (external_github_id, github_login, github_access_token, is_admin) VALUES ($1, $2, '', $3) RETURNING user_id",
    )
    .bind(github_id)
    .bind(format!("test-user-{github_id}"))
    .bind(is_admin)
    .fetch_one(db)
    .await
    .unwrap()
}

/// The real app router — cja's full stack (TraceLayer + CookieManagerLayer)
/// applied to each host branch by `into_router()`, so tests exercise
/// exactly what production serves.
pub fn api_app(state: &AppState) -> axum::Router {
    crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router()
}

/// Send a JSON (or empty) request to the API app. `bearer` sets the
/// Authorization header; `body` sends a JSON body with the content type.
pub async fn send_api(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (axum::http::StatusCode, String) {
    send_api_request(app, method, path, bearer, None, body).await
}

/// Send a request with an explicit Host header through the full app
/// router (cja stack + host dispatch) via tower oneshot. `headers` are
/// extra request headers (Origin, preflight headers, ...). Returns the
/// raw response so tests can assert status, body, and response headers.
pub async fn send_with_host(
    app: &axum::Router,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&'static str, &str)],
) -> axum::response::Response {
    use tower::ServiceExt as _;

    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::HOST, host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(axum::body::Body::empty()).unwrap();
    app.clone().oneshot(request).await.unwrap()
}

/// Like [`send_api`] but authenticates with a signed session cookie instead
/// of a Bearer token.
pub async fn send_api_with_cookie(
    app: &axum::Router,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<serde_json::Value>,
) -> (axum::http::StatusCode, String) {
    send_api_request(app, method, path, None, Some(cookie), body).await
}

async fn send_api_request(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    cookie: Option<&str>,
    body: Option<serde_json::Value>,
) -> (axum::http::StatusCode, String) {
    use tower::ServiceExt as _;

    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::ORIGIN, "https://example.com");
    if let Some(token) = bearer {
        builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(value) = cookie {
        builder = builder.header(
            axum::http::header::COOKIE,
            format!("{}={value}", crate::models::session::SESSION_COOKIE_NAME),
        );
    }
    if body.is_some() {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let request = if let Some(body) = body {
        builder
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    } else {
        builder.body(axum::body::Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The private (encrypted) session cookie value the auth extractors accept
/// for `session_id`.
pub fn signed_session_cookie(state: &AppState, session_id: Uuid) -> String {
    let cookies = tower_cookies::Cookies::default();
    cookies
        .private(&state.cookie_key.0)
        .add(tower_cookies::Cookie::new(
            crate::models::session::SESSION_COOKIE_NAME,
            session_id.to_string(),
        ));
    cookies
        .get(crate::models::session::SESSION_COOKIE_NAME)
        .unwrap()
        .value()
        .to_string()
}

/// Insert a user plus a session logged in as them; returns the session ID.
pub async fn create_user_session(db: &sqlx::PgPool, github_id: i64, is_admin: bool) -> Uuid {
    let user_id = create_user(db, github_id, is_admin).await;
    let session_id: Uuid =
        sqlx::query_scalar("INSERT INTO sessions (user_id) VALUES ($1) RETURNING session_id")
            .bind(user_id)
            .fetch_one(db)
            .await
            .unwrap();
    session_id
}

/// The user a session is logged in as.
pub async fn session_user_id(db: &sqlx::PgPool, session_id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT user_id FROM sessions WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(db)
        .await
        .unwrap()
}
