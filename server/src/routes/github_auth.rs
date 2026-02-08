use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::{Context as _, eyre};
use maud::html;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerError, ServerResult},
    flasher::Flasher,
    github::auth::{GitHubAuthParams, GitHubTokenResponse, GitHubUser},
    models::{
        api_token,
        session::{
            associate_user_with_session, clear_github_oauth_state, disassociate_user_from_session,
            set_github_oauth_state_with_cli,
        },
        user::create_or_update_user,
    },
    state::AppState,
};

use super::auth::CurrentSession;

/// Query parameters for initiating GitHub OAuth
#[derive(Debug, Deserialize)]
pub struct GitHubAuthQuery {
    /// If true, this is a CLI authentication request
    #[serde(default)]
    pub cli: bool,
}

// Route handler for initiating GitHub OAuth flow
pub async fn github_auth(
    State(state): State<AppState>,
    current_session: CurrentSession,
    Query(query): Query<GitHubAuthQuery>,
) -> ServerResult<Redirect, StatusCode> {
    // Check if OAuth is configured
    let oauth_config = state.github_oauth_config.as_ref().ok_or_else(|| {
        ServerError(
            eyre!("GitHub OAuth is not configured"),
            StatusCode::SERVICE_UNAVAILABLE,
        )
    })?;

    // Generate a random state for CSRF protection
    let oauth_state = format!("{}", uuid::Uuid::new_v4());

    // Store the state in the session along with CLI auth flag
    set_github_oauth_state_with_cli(
        &state.db,
        current_session.session.session_id,
        oauth_state.clone(),
        query.cli,
    )
    .await
    .wrap_err("Failed to store OAuth state in session")?;

    // Build OAuth URL using the AppState's github_oauth_config
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&state={}&scope={}",
        oauth_config.oauth_url,
        oauth_config.client_id,
        urlencoding::encode(&oauth_config.redirect_uri),
        oauth_state,
        "user:email" // auth.oauth.scope: requesting user:email scope
    );

    Ok(Redirect::to(&auth_url))
}

// Route handler for GitHub OAuth callback
pub async fn github_auth_callback(
    State(state): State<AppState>,
    Query(params): Query<GitHubAuthParams>,
    current_session: CurrentSession,
    flasher: Flasher,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Check if OAuth is configured
    let oauth_config = state.github_oauth_config.as_ref().ok_or_else(|| {
        ServerError(
            eyre!("GitHub OAuth is not configured"),
            StatusCode::SERVICE_UNAVAILABLE,
        )
    })?;

    // Verify the state parameter to prevent CSRF attacks
    let session_oauth_state = current_session.session.github_oauth_state;

    let session_state = match session_oauth_state {
        Some(state) => state,
        None => {
            return Err(ServerError(
                eyre!("GitHub OAuth state not found in session"),
                StatusCode::BAD_REQUEST,
            ));
        }
    };

    if params.state != session_state {
        return Err(ServerError(
            eyre!("GitHub OAuth state mismatch"),
            StatusCode::BAD_REQUEST,
        ));
    }

    // Check if this is a CLI authentication request
    let is_cli_auth = current_session.session.is_cli_auth;

    // Clear the state from the session since it's no longer needed
    clear_github_oauth_state(&state.db, current_session.session.session_id)
        .await
        .wrap_err("Failed to clear OAuth state from session")?;

    // Exchange code for access token
    let client = reqwest::Client::new();
    let token_response = client
        .post(&oauth_config.token_url)
        .json(&serde_json::json!({
            "client_id": oauth_config.client_id,
            "client_secret": oauth_config.client_secret,
            "code": params.code,
            "redirect_uri": oauth_config.redirect_uri,
        }))
        .header(ACCEPT, "application/json")
        .send()
        .await
        .wrap_err("Failed to send token request to GitHub")?
        .json::<GitHubTokenResponse>()
        .await
        .wrap_err("Failed to parse GitHub token response")?;

    // Get user data from GitHub
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", token_response.access_token))
            .wrap_err("Failed to create Authorization header")?,
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github.v3+json"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("arena-app"));

    let github_user = client
        .get(format!("{}/user", oauth_config.api_url))
        .headers(headers.clone())
        .send()
        .await
        .wrap_err("Failed to send user request to GitHub")?
        .json::<GitHubUser>()
        .await
        .wrap_err("Failed to parse GitHub user response")?;

    // Create or update user in the database
    let user = create_or_update_user(&state.db, github_user, token_response)
        .await
        .wrap_err("Failed to create or update user")?;

    // Associate the user with the current session
    associate_user_with_session(&state.db, current_session.session.session_id, user.user_id)
        .await
        .wrap_err("Failed to associate user with session")?;

    tracing::info!(
        event_type = "user_authenticated",
        user_id = %user.user_id,
        github_login = %user.github_login,
        auth_type = if is_cli_auth { "cli" } else { "web" },
        "user authenticated via GitHub OAuth"
    );

    // If CLI auth, create an API token and redirect to the token display page
    if is_cli_auth {
        let new_token = api_token::create_api_token(&state.db, user.user_id, "arena-cli")
            .await
            .wrap_err("Failed to create API token for CLI")?;

        // Redirect to the CLI token display page with the token as a query param
        return Ok(Redirect::to(&format!(
            "/auth/cli-token?token={}",
            new_token.secret
        )));
    }

    // Redirect to home page with success message
    flasher
        .add_flash("Successfully logged in with GitHub!")
        .await?;
    Ok(Redirect::to("/"))
}

// Route handler for logging out
pub async fn logout(
    State(state): State<AppState>,
    current_session: CurrentSession,
    flasher: Flasher,
) -> impl IntoResponse {
    // Disassociate user from the session (if logged in)
    if current_session.user.is_some() {
        let _ = disassociate_user_from_session(&state.db, current_session.session.session_id).await;
    }

    // Add flash message, but don't fail the request if it doesn't work
    if let Err(err) = flasher.add_flash("You have been logged out").await {
        tracing::warn!(?err, "Failed to set logout flash message");
    }

    Redirect::to("/")
}

/// Query parameters for CLI token display
#[derive(Debug, Deserialize)]
pub struct CliTokenQuery {
    pub token: String,
}

/// Route handler for displaying CLI token after OAuth
pub async fn cli_token_page(
    Query(query): Query<CliTokenQuery>,
    page_factory: PageFactory,
) -> impl IntoResponse {
    page_factory.create_page(
        "CLI Authentication Complete".to_string(),
        Box::new(html! {
            div style="max-width: 600px; margin: 40px auto; padding: 20px;" {
                h1 { "CLI Authentication Successful" }

                div class="alert alert-success" style="margin: 20px 0;" {
                    "You have successfully authenticated with GitHub!"
                }

                div style="background: #f5f5f5; border: 1px solid #ddd; border-radius: 8px; padding: 20px; margin: 20px 0;" {
                    h3 style="margin-top: 0;" { "Your API Token" }
                    p style="color: #666; margin-bottom: 10px;" {
                        "Copy this token and paste it into the CLI when prompted:"
                    }

                    div style="background: #fff; border: 1px solid #ccc; border-radius: 4px; padding: 15px; font-family: monospace; word-break: break-all; font-size: 14px;" {
                        (query.token)
                    }

                    p style="color: #856404; background: #fff3cd; border: 1px solid #ffc107; border-radius: 4px; padding: 10px; margin-top: 15px;" {
                        strong { "Important: " }
                        "This token will only be shown once. Make sure to copy it now!"
                    }
                }

                div style="margin-top: 20px;" {
                    p { "You can close this browser tab and return to the CLI." }
                    a href="/" class="btn btn-secondary" { "Go to Home" }
                }
            }
        }),
    )
}
