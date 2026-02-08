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
        api_token::validate_token,
        session::{
            SESSION_COOKIE_NAME, SESSION_EXPIRATION_SECONDS, Session, create_session,
            get_session_with_user,
        },
        user::{User, get_user_by_id},
    },
    state::AppState,
};

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
                let mut cookie =
                    Cookie::new(SESSION_COOKIE_NAME, new_session.session_id.to_string());
                cookie.set_http_only(true);
                cookie.set_secure(true);
                cookie.set_same_site(cja::server::cookies::SameSite::Lax);
                cookie.set_max_age(time::Duration::seconds(SESSION_EXPIRATION_SECONDS));
                cookie_jar.add(cookie);

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
            Some((session, user)) => Ok(CurrentSession { session, user }),
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
                let mut cookie =
                    Cookie::new(SESSION_COOKIE_NAME, new_session.session_id.to_string());
                cookie.set_http_only(true);
                cookie.set_secure(true);
                cookie.set_same_site(cja::server::cookies::SameSite::Lax);
                cookie.set_max_age(time::Duration::seconds(SESSION_EXPIRATION_SECONDS));
                cookie_jar.add(cookie);

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
        let user = session.user.ok_or_else(|| {
            ServerError(eyre!("Not authenticated"), StatusCode::UNAUTHORIZED).into_response()
        })?;

        Ok(CurrentUser(user))
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
        let user = current_session.user.ok_or_else(|| {
            ServerError(eyre!("Not authenticated"), StatusCode::UNAUTHORIZED).into_response()
        })?;

        Ok(CurrentUserWithSession {
            user,
            session: current_session.session,
        })
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
///    ApiUser(user): ApiUser,
/// ) -> impl IntoResponse {
///    // User is authenticated via API token or session
///    Json(user)
/// }
/// ```
pub struct ApiUser(pub User);

/// Result of attempting Bearer token authentication
enum BearerAuthResult {
    /// Successfully authenticated user
    Authenticated(User),
    /// Authorization header present but token invalid/revoked
    InvalidToken,
    /// No Authorization header present
    NoHeader,
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

    // validate_token hashes the token internally
    let user_id = match validate_token(&state.db, token).await {
        Ok(Some(id)) => id,
        _ => return BearerAuthResult::InvalidToken,
    };

    match get_user_by_id(&state.db, user_id).await {
        Ok(Some(user)) => BearerAuthResult::Authenticated(user),
        _ => BearerAuthResult::InvalidToken,
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
/// Returns 401 if not authenticated, 403 if authenticated but not admin.
pub struct AdminApiUser(pub User);

impl FromRequestParts<AppState> for AdminApiUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let ApiUser(user) = ApiUser::from_request_parts(parts, state).await?;
        if !user.is_admin {
            return Err((StatusCode::FORBIDDEN, "Admin access required").into_response());
        }
        Ok(AdminApiUser(user))
    }
}

impl FromRequestParts<AppState> for ApiUser {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match try_bearer_auth(parts, state).await {
            BearerAuthResult::Authenticated(user) => return Ok(ApiUser(user)),
            BearerAuthResult::InvalidToken => {
                return Err((StatusCode::UNAUTHORIZED, "Invalid or revoked token").into_response());
            }
            BearerAuthResult::NoHeader => {
                // Fall through to session auth
            }
        }

        // No Bearer token, try session auth
        let session = CurrentSession::from_request_parts(parts, state).await?;

        session
            .user
            .map(ApiUser)
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Authentication required").into_response())
    }
}
