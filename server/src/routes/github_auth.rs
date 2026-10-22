use axum::{
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use color_eyre::eyre::{Context as _, eyre};
use maud::html;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;

use crate::{
    components::{flash::Flash, page_factory::PageFactory},
    errors::{ServerError, ServerResult},
    flasher::Flasher,
    github::auth::{GitHubAuthParams, GitHubTokenResponse, GitHubUser},
    models::{
        api_token::{self, Permission, SnakeAccess},
        session::{
            associate_user_with_session, clear_github_oauth_state, clear_post_login_redirect,
            disassociate_user_from_session, set_github_oauth_state_with_cli,
            take_post_login_redirect,
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
    /// If true, this login was started by /oauth/authorize: keep (rather
    /// than clear) any saved post-login redirect.
    #[serde(default)]
    pub resume_oauth: bool,
}

// Route handler for initiating GitHub OAuth flow
pub async fn github_auth(
    State(state): State<AppState>,
    current_session: CurrentSession,
    Query(query): Query<GitHubAuthQuery>,
) -> ServerResult<Redirect, StatusCode> {
    // Check if OAuth is configured
    let oauth_config = state.config.github.as_ref().ok_or_else(|| {
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

    // Unless this login resumes an OAuth consent flow, drop any saved
    // post-login redirect so an abandoned consent can't hijack this login.
    if !query.resume_oauth
        && let Err(e) =
            clear_post_login_redirect(&state.db, current_session.session.session_id).await
    {
        tracing::warn!(?e, "Failed to clear post-login redirect");
    }

    let auth_url = authorization_url(oauth_config, &oauth_state);

    Ok(Redirect::to(&auth_url))
}

fn authorization_url(
    oauth_config: &crate::github::auth::GitHubOAuthConfig,
    oauth_state: &str,
) -> String {
    format!(
        "{}?client_id={}&redirect_uri={}&state={}",
        oauth_config.oauth_url,
        oauth_config.client_id,
        urlencoding::encode(&oauth_config.redirect_uri),
        oauth_state,
    )
}

// Route handler for GitHub OAuth callback
pub async fn github_auth_callback(
    State(state): State<AppState>,
    Query(params): Query<GitHubAuthParams>,
    current_session: CurrentSession,
    flasher: Flasher,
) -> ServerResult<Response, StatusCode> {
    // Check if OAuth is configured
    let oauth_config = state.config.github.as_ref().ok_or_else(|| {
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

    // Detect first-time signup: on INSERT both created_at and updated_at
    // default to NOW() (same transaction); the BEFORE UPDATE trigger changes
    // updated_at on every subsequent login, so equality means new user.
    let is_new_user = user.created_at == user.updated_at;

    // Zero-friction migration path: if an imported play account carries
    // this GitHub ID, claim it now. Errors are logged, not fatal — login
    // must not break because migration data is off, and the claim retries
    // on the next login since the account stays unclaimed.
    let auto_claim = match crate::models::imported_account::try_auto_claim(
        &state.db,
        user.user_id,
        user.external_github_id,
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            tracing::error!(
                user_id = %user.user_id,
                error = %e,
                "Auto-claim of imported play account failed"
            );
            None
        }
    };
    if let Some(summary) = &auto_claim {
        tracing::info!(
            event_type = "play_claim_succeeded",
            user_id = %user.user_id,
            play_username = %summary.username,
            snakes = summary.snakes_created,
            grants = summary.grants_created,
            "play account auto-claimed via GitHub link"
        );
        state
            .discord
            .notify_account_claimed(&summary.username, summary.snakes_created);
    }

    if is_new_user {
        state.discord.notify_user_signup(&user.github_login);
    }

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

    // If CLI auth, create an API token and render the token page directly as
    // this response. Never put the secret in a URL (redirect Location, query,
    // or fragment): browser history, proxies, and Cloud Run request logs all
    // capture URLs, and this token can call every /api/* route.
    if is_cli_auth {
        // Full power: all access, every permission; `admin` only for admins.
        let mut permissions = Permission::ALL.to_vec();
        if !user.is_admin {
            permissions.retain(|p| *p != Permission::Admin);
        }
        let new_token = api_token::create_api_token(
            &state.db,
            user.user_id,
            "arena-cli",
            &permissions,
            SnakeAccess::All,
            &[],
        )
        .await
        .wrap_err("Failed to create API token for CLI")?;

        let secret = new_token.secret;
        let page_factory = PageFactory {
            flash: Flash {
                message: None,
                flash_type: None,
            },
            user: Some(user),
            path: "/auth/github/callback".to_string(),
            base_url: state.config.base_url.clone(),
            unread_count: None,
        };
        let page = page_factory.create_page(
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
                            (secret)
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
        );
        return Ok(([(header::CACHE_CONTROL, "no-store")], page).into_response());
    }

    // Redirect to home page with success message
    if let Some(summary) = auto_claim {
        flasher
            .add_flash(format!(
                "Welcome back, {}! Your play.battlesnake.com account was linked: {} snake(s) and {} customization unlock(s) migrated.",
                summary.username, summary.snakes_created, summary.grants_created
            ))
            .await?;
    } else {
        flasher
            .add_flash("Successfully logged in with GitHub!")
            .await?;
    }

    // Resume an OAuth consent flow: if /oauth/authorize parked its request
    // on this session (and it still looks like one — second half of the
    // open-redirect guard), send the user back to the same authorize URL.
    // CLI logins never get here (they returned above).
    let resume = take_post_login_redirect(&state.db, current_session.session.session_id)
        .await
        .wrap_err("Failed to take post-login redirect")?;
    if let Some(path) = resume
        && path.starts_with("/oauth/authorize")
    {
        return Ok(Redirect::to(&path).into_response());
    }

    Ok(Redirect::to("/").into_response())
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

#[cfg(test)]
mod private_email_guard {
    use std::path::Path;

    fn inspect(path: &Path) {
        for entry in std::fs::read_dir(path).expect("read server source") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                inspect(&path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path).expect("read Rust source");
                assert!(
                    !source.contains(&["/user", "/emails"].concat()),
                    "private email endpoint in {}",
                    path.display()
                );
                assert!(
                    !source.contains(&["user", ":email"].concat()),
                    "private email scope in {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn production_source_never_requests_private_github_email() {
        inspect(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
        let config = crate::github::auth::GitHubOAuthConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://arena.example/callback".into(),
            oauth_url: "https://github.example/authorize".into(),
            token_url: String::new(),
            api_url: String::new(),
        };
        let url = super::authorization_url(&config, "state");
        assert!(url.contains("state=state"));
        assert!(!url.contains("scope="));
    }
}
