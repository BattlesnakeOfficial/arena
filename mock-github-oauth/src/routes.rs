use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect},
};

use crate::{state::MockOAuthState, types::*};

/// POST /_admin/set-user-for-state
///
/// Pre-register a user for a specific OAuth state value.
/// When authorize is called with this state, the pre-registered user will be used.
/// This allows tests to control which user is returned without passing mock params through the app.
pub async fn set_user_for_state(
    State(state): State<MockOAuthState>,
    Json(request): Json<PreRegisterRequest>,
) -> impl IntoResponse {
    tracing::info!(
        oauth_state = %request.state,
        user_login = %request.user.login,
        "Pre-registering user for OAuth state"
    );

    state.pre_register_user(request.state, request.user).await;

    StatusCode::OK
}

/// GET /login/oauth/authorize
///
/// Simulates GitHub's OAuth authorization page.
/// Immediately redirects to the callback with a code.
///
/// User resolution priority:
/// 1. Pre-registered user for this state (via /_admin/set-user-for-state)
/// 2. Mock user config from query params (legacy, for backwards compat)
/// 3. Default mock user
pub async fn authorize(
    State(state): State<MockOAuthState>,
    Query(params): Query<AuthorizeParams>,
) -> impl IntoResponse {
    // Generate a unique authorization code
    let code = format!("mock_code_{}", uuid::Uuid::new_v4());

    // First, check for a pre-registered user for this state
    let user = if let Some(pre_registered) = state.take_pre_registered(&params.state).await {
        tracing::info!(
            oauth_state = %params.state,
            user_login = %pre_registered.login,
            "Using pre-registered user for OAuth state"
        );
        pre_registered
    } else {
        // Fall back to query params or defaults
        MockUserConfig {
            id: params.mock_user_id.unwrap_or(12345),
            login: params
                .mock_user_login
                .unwrap_or_else(|| "mock_user".to_string()),
            name: params.mock_user_name.or(Some("Mock User".to_string())),
            email: params
                .mock_user_email
                .or(Some("mock@example.com".to_string())),
            avatar_url: "https://example.com/avatar.png".to_string(),
        }
    };

    tracing::info!(
        code = %code,
        user_id = user.id,
        user_login = %user.login,
        "Storing auth code for mock user"
    );

    // Store the code with user config
    state.store_code(code.clone(), user).await;

    // Redirect back to the app's callback
    let redirect_url = format!(
        "{}?code={}&state={}",
        params.redirect_uri, code, params.state
    );

    tracing::info!(redirect_url = %redirect_url, "Redirecting to callback");

    Redirect::to(&redirect_url)
}

/// POST /login/oauth/access_token
///
/// Exchanges authorization code for access token.
/// Accepts both form-urlencoded and JSON bodies.
pub async fn access_token(
    State(state): State<MockOAuthState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    // Parse the body - could be form-urlencoded or JSON
    let params: TokenParams = if headers
        .get("content-type")
        .is_some_and(|v| v.to_str().is_ok_and(|s| s.contains("application/json")))
    {
        match serde_json::from_str(&body) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse JSON body");
                return StatusCode::BAD_REQUEST.into_response();
            }
        }
    } else {
        match serde_urlencoded::from_str(&body) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "Failed to parse form body");
                return StatusCode::BAD_REQUEST.into_response();
            }
        }
    };

    tracing::info!(code = %params.code, "Exchanging code for token");

    // Exchange the code for a token
    match state.exchange_code(&params.code).await {
        Some((token, _user)) => {
            let response = TokenResponse {
                access_token: token,
                token_type: "bearer".to_string(),
                scope: String::new(),
            };

            // Check Accept header to determine response format
            let accept = headers
                .get("accept")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            if accept.contains("application/json") {
                tracing::info!("Returning JSON token response");
                Json(response).into_response()
            } else {
                // Form-urlencoded response (GitHub's default)
                tracing::info!("Returning form-urlencoded token response");
                format!(
                    "access_token={}&token_type={}&scope={}",
                    response.access_token, response.token_type, response.scope
                )
                .into_response()
            }
        }
        None => {
            tracing::warn!(code = %params.code, "Invalid or expired code");
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

/// GET /user
///
/// Returns the mock user for the provided access token.
pub async fn get_user(
    State(state): State<MockOAuthState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Extract token from Authorization header
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .map(|s| s.to_string());

    match token {
        Some(t) => match state.get_user(&t).await {
            Some(user) => {
                tracing::info!(user_login = %user.login, "Returning mock user");
                Json(UserResponse {
                    id: user.id,
                    login: user.login,
                    name: user.name,
                    email: user.email,
                    avatar_url: user.avatar_url,
                })
                .into_response()
            }
            None => {
                tracing::warn!("Invalid or unknown token");
                StatusCode::UNAUTHORIZED.into_response()
            }
        },
        None => {
            tracing::warn!("Missing Authorization header");
            StatusCode::UNAUTHORIZED.into_response()
        }
    }
}
