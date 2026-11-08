use std::ops::Deref;

use axum::{
    extract::FromRequestParts,
    http::{StatusCode, header::AUTHORIZATION, request::Parts},
    response::{IntoResponse as _, Response},
};
use cja::server::cookies::{Cookie, CookieJar};
use color_eyre::eyre::eyre;
use uuid::Uuid;

use crate::{
    errors::ServerError,
    models::{
        api_token::{Permission, TokenAuth, authenticate_token},
        session::{
            SESSION_COOKIE_NAME, SESSION_EXPIRATION_SECONDS, Session, create_session,
            get_session_with_user,
        },
        user::{User, get_user_by_id},
    },
    state::AppState,
};

/// The session cookie exactly as the extractors and the login rotation
/// issue it: host-only path, Secure, HttpOnly, Lax, 30-day max-age
/// matching the DB `expires_at` set at creation/login. One source for the
/// attributes so the login-rotation re-issue can't drift from the
/// extractor issue (DEV-2185).
pub(crate) fn build_session_cookie(session_id: Uuid) -> Cookie {
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, session_id.to_string());
    cookie.set_http_only(true);
    cookie.set_secure(true);
    // Without an explicit path, RFC 6265 scopes the cookie to the
    // directory of whatever request created the session. A visitor
    // whose first request is /auth/github gets a session pinned to
    // /auth — the authenticated session is then never sent for
    // normal pages and login silently doesn't stick.
    cookie.set_path("/");
    cookie.set_same_site(cja::server::cookies::SameSite::Lax);
    cookie.set_max_age(time::Duration::seconds(SESSION_EXPIRATION_SECONDS));
    cookie
}

/// Current session and optional user
///
/// This struct contains the current session and optional user.
/// The session is always present, but the user may be None if not logged in.
pub struct CurrentSession {
    pub session: Session,
    pub user: Option<User>,
}
/// Extract the current session with optional user
impl FromRequestParts<AppState> for CurrentSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        app_state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookie_jar = match CookieJar::from_request_parts(parts, app_state).await {
            Ok(jar) => jar,
            Err(_) => {
                tracing::error!("Cookie jar extraction failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        };

        // Try to get session_id from cookie
        let session_id = cookie_jar
            .get(SESSION_COOKIE_NAME)
            .and_then(|cookie| cookie.value().parse::<Uuid>().ok());

        // If no session_id in cookie, create a new session
        let session_id = match session_id {
            Some(id) => id,
            None => {
                // No session found, create a new one
                let new_session = match create_session(&app_state.db).await {
                    Ok(session) => session,
                    Err(_e) => {
                        tracing::error!("Session creation failed: {}", _e);
                        return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                    }
                };

                // Set the session cookie
                cookie_jar.add(build_session_cookie(new_session.session_id));

                new_session.session_id
            }
        };

        // Get session and user from database
        let result = match get_session_with_user(&app_state.db, session_id).await {
            Ok(result) => result,
            Err(_e) => {
                tracing::error!("Session fetch failed: {}", _e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        };

        // If session doesn't exist, create a new one
        match result {
            Some((session, user)) => {
                if let Some(user) = &user {
                    // DEV-2076: rides the `finished processing request`
                    // event (cja task-local). Returns false only outside a
                    // request scope; the direct-extractor unit tests hit
                    // that, so the result is intentionally dropped here —
                    // the event-level tests pin the real path.
                    let _ = cja::server::trace::record_request_user(user.user_id, "session");
                    app_state
                        .activity_recorder
                        .record(&app_state.db, user.user_id)
                        .await;
                }
                Ok(CurrentSession { session, user })
            }
            None => {
                // Session expired or doesn't exist, create a new one
                let new_session = match create_session(&app_state.db).await {
                    Ok(session) => session,
                    Err(_e) => {
                        tracing::error!("Session creation failed: {}", _e);
                        return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                    }
                };

                // Set the session cookie
                cookie_jar.add(build_session_cookie(new_session.session_id));

                Ok(CurrentSession {
                    session: new_session,
                    user: None,
                })
            }
        }
    }
}

/// Extractor for requiring an authenticated user
///
/// This extractor will return a 401 Unauthorized error if the user is not logged in.
/// Use this extractor in route handlers that require authentication.
///
/// Example:
/// ```
/// async fn protected_route(
///    CurrentUser(user): CurrentUser,
/// ) -> impl IntoResponse {
///    // User is guaranteed to be logged in here
///    format!("Hello, {}!", user.github_login)
/// }
/// ```
pub struct CurrentUser(pub User);

/// Extractor for optionally getting the current user
///
/// Unlike CurrentUser, this won't return an error if the user is not logged in.
/// Instead, it will return Option<User> which will be None if not logged in.
pub struct OptionalUser(pub Option<User>);

impl FromRequestParts<AppState> for OptionalUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = CurrentSession::from_request_parts(parts, state).await?;

        Ok(OptionalUser(session.user))
    }
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = CurrentSession::from_request_parts(parts, state).await?;

        // Check if user is logged in
        match session.user {
            Some(user) => Ok(CurrentUser(user)),
            None => Err(sign_in_required_response(parts, state).await),
        }
    }
}

/// Extractor for getting both the authenticated user and their session
///
/// This extractor will return a 401 Unauthorized error if the user is not logged in.
/// Use this extractor in route handlers that require authentication and need the session.
///
/// Example:
/// ```
/// async fn protected_route(
///    CurrentUserWithSession { user, session }: CurrentUserWithSession,
/// ) -> impl IntoResponse {
///    // User is guaranteed to be logged in here and you have access to their session
///    format!("Hello, {}! Your session ID is {}", user.github_login, session.session_id)
/// }
/// ```
pub struct CurrentUserWithSession {
    pub user: User,
    pub session: Session,
}

impl FromRequestParts<AppState> for CurrentUserWithSession {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let current_session = CurrentSession::from_request_parts(parts, state).await?;

        // Check if user is logged in
        match current_session.user {
            Some(user) => Ok(CurrentUserWithSession {
                user,
                session: current_session.session,
            }),
            None => Err(sign_in_required_response(parts, state).await),
        }
    }
}

/// Response for an unauthenticated request to a page that requires sign-in.
///
/// Renders the branded "sign in required" page (with a GitHub sign-in CTA)
/// instead of a bare `401` with an empty body — which rendered as a blank
/// white screen for a signed-out visitor hitting `/me`, `/games/new`,
/// `/battlesnakes`, etc. The `401` status is preserved, so htmx/API callers
/// (which check the status, not the body) are unaffected.
async fn sign_in_required_response(parts: &mut Parts, state: &AppState) -> Response {
    use crate::components::page_factory::PageFactory;

    match PageFactory::from_request_parts(parts, state).await {
        Ok(page_factory) => (
            StatusCode::UNAUTHORIZED,
            page_factory.create_page(
                "Sign in required".to_string(),
                Box::new(maud::html! {
                    div class="home" {
                        section class="section" {
                            h1 { "Sign in required" }
                            p class="empty" {
                                "You need to sign in to view this page."
                            }
                            div class="cta-row" {
                                a class="btn solid" href="/auth/github" { "Sign in with GitHub" }
                                a class="btn" href="/" { "Back to Home" }
                            }
                        }
                    }
                }),
            ),
        )
            .into_response(),
        // If the page shell itself can't be built, fall back to a bare 401
        // rather than surfacing a 500 on an already-unauthenticated request.
        Err(_) => ServerError(eyre!("Not authenticated"), StatusCode::UNAUTHORIZED).into_response(),
    }
}

/// The authorization context for `/api/*` handlers: the authenticated user
/// plus what their credential (browser session or API token) may do.
///
/// A session scope has full power: every permission, all snakes. A token
/// scope carries the token's permission set and snake access, loaded at auth
/// time.
///
/// Deref covers `User` field access (`auth.user_id`, `auth.github_login`);
/// when a `&User` is needed use `auth.user()`. Never clone the whole context.
pub struct ApiAuth {
    user: User,
    scope: AuthScope,
}

pub enum AuthScope {
    /// Browser session on /api/*: every permission, all snakes.
    Session,
    Token(crate::models::api_token::TokenAuth),
}

impl ApiAuth {
    pub fn session(user: User) -> ApiAuth {
        ApiAuth {
            user,
            scope: AuthScope::Session,
        }
    }

    pub fn token(user: User, auth: TokenAuth) -> ApiAuth {
        ApiAuth {
            user,
            scope: AuthScope::Token(auth),
        }
    }

    pub fn user(&self) -> &User {
        &self.user
    }

    pub fn token_scope(&self) -> Option<&TokenAuth> {
        match &self.scope {
            AuthScope::Session => None,
            AuthScope::Token(auth) => Some(auth),
        }
    }

    /// Does this credential hold `permission`? Sessions hold everything.
    pub fn has(&self, permission: Permission) -> bool {
        match &self.scope {
            AuthScope::Session => true,
            AuthScope::Token(auth) => auth.permissions.contains(&permission),
        }
    }

    /// May this credential touch `snake_id`? Sessions and `all`-access
    /// tokens may touch any of the user's snakes; `selected` tokens only
    /// the snakes in their list.
    pub fn can_access_snake(&self, snake_id: Uuid) -> bool {
        match &self.scope {
            AuthScope::Session => true,
            AuthScope::Token(auth) => match auth.snake_access {
                crate::models::api_token::SnakeAccess::All => true,
                crate::models::api_token::SnakeAccess::Selected => {
                    auth.snake_ids.contains(&snake_id)
                }
            },
        }
    }

    /// Require an account-wide permission, else 403 naming it.
    pub fn require(&self, permission: Permission) -> Result<(), (StatusCode, String)> {
        if self.has(permission) {
            Ok(())
        } else {
            Err((
                StatusCode::FORBIDDEN,
                format!("missing permission: {}", permission.as_str()),
            ))
        }
    }

    /// Require a per-snake permission for `snake_id`: access first (the
    /// message must match what actually blocked), then the permission.
    pub fn require_snake(
        &self,
        snake_id: Uuid,
        permission: Permission,
    ) -> Result<(), (StatusCode, String)> {
        if !self.can_access_snake(snake_id) {
            return Err((
                StatusCode::FORBIDDEN,
                format!("battlesnake {snake_id} is not in this token's access"),
            ));
        }
        self.require(permission)
    }

    /// Record that `snake_id` now belongs to this token's `selected` list.
    ///
    /// Called after `add_snake_to_token` persists the row, so the response
    /// built in the same request sees the freshly created snake (the scope
    /// was loaded at auth time and predates it). No-op for session and
    /// `all`-access scopes.
    pub fn record_selected_snake(&mut self, snake_id: Uuid) {
        if let AuthScope::Token(auth) = &mut self.scope
            && auth.snake_access == crate::models::api_token::SnakeAccess::Selected
        {
            auth.snake_ids.insert(snake_id);
        }
    }
}

impl Deref for ApiAuth {
    type Target = User;

    fn deref(&self) -> &Self::Target {
        &self.user
    }
}

/// Extractor for API authentication via Bearer token OR session cookie
///
/// This extractor tries Bearer token auth first, then falls back to session auth.
/// Use this for API endpoints that should work from both CLI (token) and web UI (session).
///
/// Example:
/// ```
/// async fn api_route(
///    ApiUser(auth): ApiUser,
/// ) -> impl IntoResponse {
///    auth.require(Permission::SnakesRead)?;
///    // User is authenticated via API token or session, with permission checked
/// }
/// ```
pub struct ApiUser(pub ApiAuth);

/// Result of attempting Bearer token authentication
enum BearerAuthResult {
    /// Successfully authenticated user with token scope
    Authenticated(Box<User>, Box<TokenAuth>),
    /// Authorization header present but token invalid/revoked
    InvalidToken,
    /// No Authorization header present
    NoHeader,
    /// The lookup itself failed (DB error): a 500, not a 401 — a revoked
    /// token and an outage must not look the same.
    LookupFailed,
}

/// Attempt Bearer token authentication
async fn try_bearer_auth(parts: &Parts, state: &AppState) -> BearerAuthResult {
    let Some(auth_header) = parts.headers.get(AUTHORIZATION) else {
        return BearerAuthResult::NoHeader;
    };

    let Ok(auth_str) = auth_header.to_str() else {
        return BearerAuthResult::InvalidToken;
    };

    let Some(token) = auth_str.strip_prefix("Bearer ") else {
        return BearerAuthResult::InvalidToken;
    };

    // authenticate_token hashes the token internally
    let token_auth = match authenticate_token(&state.db, token).await {
        Ok(Some(auth)) => auth,
        Ok(None) => return BearerAuthResult::InvalidToken,
        Err(e) => {
            tracing::error!("Failed to authenticate API token: {e:#}");
            return BearerAuthResult::LookupFailed;
        }
    };

    match get_user_by_id(&state.db, token_auth.user_id).await {
        Ok(Some(user)) => {
            let _ = cja::server::trace::record_request_user(user.user_id, "token");
            state
                .activity_recorder
                .record(&state.db, user.user_id)
                .await;
            BearerAuthResult::Authenticated(Box::new(user), Box::new(token_auth))
        }
        Ok(None) => BearerAuthResult::InvalidToken,
        Err(e) => {
            tracing::error!("Failed to load API token user: {e:#}");
            BearerAuthResult::LookupFailed
        }
    }
}

/// Extractor for requiring an authenticated admin user (session auth only)
///
/// Returns 401 if not authenticated, 403 if authenticated but not admin.
pub struct AdminUser(pub User);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let CurrentUser(user) = CurrentUser::from_request_parts(parts, state).await?;
        if !user.is_admin {
            return Err((StatusCode::FORBIDDEN, "Admin access required").into_response());
        }
        Ok(AdminUser(user))
    }
}

/// Extractor for requiring an authenticated admin user (Bearer token OR session auth)
///
/// Returns 401 if not authenticated, 403 (`Admin access required`) if the
/// user is not an admin, 403 (`missing permission: admin`) for a scoped
/// token without the `admin` permission.
pub struct AdminApiUser(pub ApiAuth);

impl FromRequestParts<AppState> for AdminApiUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ApiUser(auth) = ApiUser::from_request_parts(parts, state).await?;
        if !auth.is_admin {
            return Err((StatusCode::FORBIDDEN, "Admin access required").into_response());
        }
        auth.require(Permission::Admin)
            .map_err(|rejection| rejection.into_response())?;
        Ok(AdminApiUser(auth))
    }
}

impl FromRequestParts<AppState> for ApiUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match try_bearer_auth(parts, state).await {
            BearerAuthResult::Authenticated(user, token_auth) => {
                return Ok(ApiUser(ApiAuth::token(*user, *token_auth)));
            }
            BearerAuthResult::InvalidToken => {
                return Err((StatusCode::UNAUTHORIZED, "Invalid or revoked token").into_response());
            }
            BearerAuthResult::LookupFailed => {
                return Err(
                    (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
                );
            }
            BearerAuthResult::NoHeader => {
                // Fall through to session auth
            }
        }

        // No Bearer token, try session auth
        let session = CurrentSession::from_request_parts(parts, state).await?;

        session
            .user
            .map(ApiAuth::session)
            .map(ApiUser)
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Authentication required").into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::api_token::create_api_token;
    use axum::http::Request;

    fn add_session_cookie(parts: &mut Parts, state: &AppState, session_id: Uuid) {
        let cookies = tower_cookies::Cookies::default();
        cookies
            .private(&state.cookie_key.0)
            .add(tower_cookies::Cookie::new(
                SESSION_COOKIE_NAME,
                session_id.to_string(),
            ));
        parts.extensions.insert(cookies);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn session_and_bearer_extractors_record_authenticated_activity_only(db: sqlx::PgPool) {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'extractor', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let session_id = sqlx::query_scalar!(
            "INSERT INTO sessions(user_id) VALUES ($1) RETURNING session_id",
            user_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let token = create_api_token(
            &db,
            user_id,
            "extractor",
            crate::models::api_token::Permission::ALL.as_slice(),
            crate::models::api_token::SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();
        let state = AppState::test_from_pool(db.clone());
        for _ in 0..3 {
            let (mut parts, ()) = Request::builder().uri("/me").body(()).unwrap().into_parts();
            add_session_cookie(&mut parts, &state, session_id);
            let session = CurrentSession::from_request_parts(&mut parts, &state)
                .await
                .unwrap();
            assert_eq!(session.user.unwrap().user_id, user_id);
        }
        let (mut parts, ()) = Request::builder()
            .uri("/api/snakes")
            .header(AUTHORIZATION, format!("Bearer {}", token.secret))
            .body(())
            .unwrap()
            .into_parts();
        let api_user = ApiUser::from_request_parts(&mut parts, &state)
            .await
            .unwrap();
        assert_eq!(api_user.0.user_id, user_id);

        let (mut anonymous, ()) = Request::builder()
            .uri("/stats")
            .body(())
            .unwrap()
            .into_parts();
        anonymous
            .extensions
            .insert(tower_cookies::Cookies::default());
        assert!(
            CurrentSession::from_request_parts(&mut anonymous, &state)
                .await
                .unwrap()
                .user
                .is_none()
        );
        let (mut invalid, ()) = Request::builder()
            .uri("/api/snakes")
            .header(AUTHORIZATION, "Bearer invalid")
            .body(())
            .unwrap()
            .into_parts();
        assert!(
            ApiUser::from_request_parts(&mut invalid, &state)
                .await
                .is_err()
        );

        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!: i64\" FROM user_activity_days WHERE user_id = $1",
            user_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(count, 1);
        assert!(!state.activity_recorder.record(&db, user_id).await);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn token_lookup_failure_is_500_not_401(db: sqlx::PgPool) {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'lookup', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let token = create_api_token(
            &db,
            user_id,
            "lookup",
            &[],
            crate::models::api_token::SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();
        let state = AppState::test_from_pool(db.clone());
        let bearer = |secret: &str| {
            Request::builder()
                .uri("/api/me")
                .header(AUTHORIZATION, format!("Bearer {secret}"))
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        // An unknown token is still a 401.
        let rejection = ApiUser::from_request_parts(&mut bearer("unknown"), &state)
            .await
            .err()
            .unwrap();
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);

        // A failing lookup is a 500, not "Invalid or revoked token".
        sqlx::query!("DROP TABLE api_token_permissions")
            .execute(&db)
            .await
            .unwrap();
        let rejection = ApiUser::from_request_parts(&mut bearer(&token.secret), &state)
            .await
            .err()
            .unwrap();
        assert_eq!(rejection.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn activity_write_failure_does_not_fail_auth_or_retry_today(db: sqlx::PgPool) {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'failure', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let session_id = sqlx::query_scalar!(
            "INSERT INTO sessions(user_id) VALUES ($1) RETURNING session_id",
            user_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let state = AppState::test_from_pool(db.clone());
        sqlx::query!("DROP TABLE user_activity_days")
            .execute(&db)
            .await
            .unwrap();
        let (mut parts, ()) = Request::builder().uri("/me").body(()).unwrap().into_parts();
        add_session_cookie(&mut parts, &state, session_id);
        assert!(
            CurrentSession::from_request_parts(&mut parts, &state)
                .await
                .unwrap()
                .user
                .is_some()
        );
        assert!(!state.activity_recorder.record(&db, user_id).await);
    }

    /// DEV-2076: the session path records through
    /// `cja::server::trace::record_request_user`, so the request's one
    /// `finished processing request` event carries `user_id` +
    /// `auth_type = session`. The probe URI is unique to this test run —
    /// no other request in this process shares it — so the counts are
    /// exact, not filtered against sibling tests' traffic.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_request_event_carries_session_user(db: sqlx::PgPool) {
        use crate::routes::test_support::{
            api_app, create_user_session, session_user_id, signed_session_cookie,
        };
        use tower::ServiceExt as _;

        let capture = crate::test_capture::Capture::shared();
        let session_id = create_user_session(&db, 62076, false).await;
        let user_id = session_user_id(&db, session_id).await;
        let state = AppState::test_from_pool(db);
        let app = api_app(&state);

        // root_page takes PageFactory + OptionalUser, so CurrentSession
        // runs at least twice per request — the task-local API is
        // last-write-wins and still emits exactly ONE close event, which
        // the count assertions below pin.
        let probe = format!("/?probe={}", Uuid::new_v4());
        let cookie = signed_session_cookie(&state, session_id);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(probe.clone())
                    .header(
                        axum::http::header::COOKIE,
                        format!("{SESSION_COOKIE_NAME}={cookie}"),
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());

        let groups = capture.request_close_event_groups(&probe);
        assert_eq!(
            groups.len(),
            1,
            "exactly one `{probe}` request expected: {groups:?}"
        );
        assert_eq!(
            groups[0].len(),
            1,
            "one close event per request: {groups:?}"
        );
        let event = &groups[0][0];
        assert_eq!(
            event.get("user_id").map(String::as_str),
            Some(user_id.to_string().as_str()),
            "session close event must carry user_id={user_id}: {event:?}"
        );
        assert_eq!(
            event.get("auth_type").map(String::as_str),
            Some("session"),
            "session close event must carry auth_type=session: {event:?}"
        );
    }

    /// DEV-2076: the bearer-token path records through
    /// `cja::server::trace::record_request_user`, so the request's one
    /// `finished processing request` event carries `user_id` +
    /// `auth_type = token`. Own test (own DB, own probe URI) so its
    /// failing-first run is independent of the session probe — a panic
    /// in one test function never exercises another.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_request_event_carries_token_user(db: sqlx::PgPool) {
        use crate::routes::test_support::{api_app, create_user_session, session_user_id};
        use tower::ServiceExt as _;

        let capture = crate::test_capture::Capture::shared();
        let session_id = create_user_session(&db, 62077, false).await;
        let user_id = session_user_id(&db, session_id).await;
        let token = create_api_token(
            &db,
            user_id,
            "dev-2076",
            Permission::ALL.as_slice(),
            crate::models::api_token::SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();
        let state = AppState::test_from_pool(db);
        let app = api_app(&state);

        let probe = format!("/api/me?probe={}", Uuid::new_v4());
        let response = app
            .oneshot(
                Request::builder()
                    .uri(probe.clone())
                    .header(AUTHORIZATION, format!("Bearer {}", token.secret))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let groups = capture.request_close_event_groups(&probe);
        assert_eq!(
            groups.len(),
            1,
            "exactly one `{probe}` request expected: {groups:?}"
        );
        assert_eq!(
            groups[0].len(),
            1,
            "one close event per request: {groups:?}"
        );
        let event = &groups[0][0];
        assert_eq!(
            event.get("user_id").map(String::as_str),
            Some(user_id.to_string().as_str()),
            "token close event must carry user_id={user_id}: {event:?}"
        );
        assert_eq!(
            event.get("auth_type").map(String::as_str),
            Some("token"),
            "token close event must carry auth_type=token: {event:?}"
        );
    }

    /// One anonymous request through the full stack: exactly ONE close
    /// event, carrying neither `user_id` nor `auth_type` (cja records `None`
    /// as no field at all). Each caller's probe URI is unique, so the count
    /// assertions are exact: an anonymous request that stopped emitting the
    /// event fails the `len` assertion, not just the key one.
    async fn assert_anonymous_close_event(
        app: &axum::Router,
        capture: &crate::test_capture::Capture,
        probe: &str,
        cookie: Option<String>,
    ) {
        use tower::ServiceExt as _;

        let mut builder = Request::builder().uri(probe);
        if let Some(cookie) = cookie {
            builder = builder.header(axum::http::header::COOKIE, cookie);
        }
        let response = app
            .clone()
            .oneshot(builder.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "`{probe}` probe failed: {}",
            response.status()
        );
        let groups = capture.request_close_event_groups(probe);
        assert_eq!(
            groups.len(),
            1,
            "exactly one `{probe}` request expected: {groups:?}"
        );
        assert_eq!(
            groups[0].len(),
            1,
            "one close event per request: {groups:?}"
        );
        let event = &groups[0][0];
        assert!(
            event.contains_key("status"),
            "close event shape must include status: {event:?}"
        );
        assert!(
            !event.contains_key("user_id") && !event.contains_key("auth_type"),
            "anonymous `{probe}` close event must carry neither key: {event:?}"
        );
    }

    /// DEV-2076: a request with no cookie at all (a brand-new session is
    /// created) is anonymous on its close event. This test cannot fail
    /// against unmigrated code, since nothing is recorded anywhere there;
    /// its failure-capability is proven by the plan's mutation control,
    /// recorded in the PR.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_request_event_is_anonymous_without_cookie(db: sqlx::PgPool) {
        use crate::routes::test_support::api_app;

        let capture = crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(db);
        let app = api_app(&state);

        assert_anonymous_close_event(
            &app,
            capture,
            &format!("/privacy?probe={}", Uuid::new_v4()),
            None,
        )
        .await;
    }

    /// DEV-2076: a signed cookie for a session with no user
    /// (`Some((session, None))`, the shape the record code must skip) is
    /// anonymous on its close event. Same mutation-control note as the
    /// cookieless test. A separate function because a panic in the first
    /// probe would otherwise leave this shape unexercised.
    #[sqlx::test(migrations = "../migrations")]
    async fn finished_request_event_is_anonymous_with_userless_session(db: sqlx::PgPool) {
        use crate::routes::test_support::{api_app, signed_session_cookie};

        let capture = crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(db.clone());
        let app = api_app(&state);

        let userless = crate::models::session::create_session(&db).await.unwrap();
        let cookie = signed_session_cookie(&state, userless.session_id);
        assert_anonymous_close_event(
            &app,
            capture,
            &format!("/privacy?probe={}", Uuid::new_v4()),
            Some(format!("{SESSION_COOKIE_NAME}={cookie}")),
        )
        .await;
    }
}
