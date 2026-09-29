//! Helpers for driving the real router as a signed-in user in route tests.

use uuid::Uuid;

use crate::state::AppState;

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
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (external_github_id, github_login, github_access_token, is_admin) VALUES ($1, $2, '', $3) RETURNING user_id",
    )
    .bind(github_id)
    .bind(format!("test-user-{github_id}"))
    .bind(is_admin)
    .fetch_one(db)
    .await
    .unwrap();
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
