use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect},
    routing::{delete, get, post, put},
};
use color_eyre::eyre::Context as _;
use maud::html;
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

use crate::{components::page_factory::PageFactory, errors::ServerResult, state::AppState};

#[derive(Deserialize)]
pub struct UpdateProfileForm {
    pub display_name: String,
    pub pronouns: String,
    pub country: String,
    pub backstory: String,
}

// Include route modules
pub mod admin;
pub mod api;
pub mod auth;
pub mod battlesnake;
pub mod claim;
pub mod customizations;
pub mod game;
pub mod github_auth;
pub mod leaderboard;
pub mod policy;
pub mod settings;
pub mod tournament;

pub fn routes(app_state: AppState) -> axum::Router {
    // CORS layer for API routes - allows board.battlesnake.com to access our API
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // API routes with CORS enabled (for board viewer and CLI/programmatic access)
    let api_routes = axum::Router::new()
        .route("/games/{id}", get(game::get_game_info))
        .route("/games/{id}/events", get(game::game_events_websocket))
        .route("/tokens", post(api::tokens::create_token))
        .route("/tokens", get(api::tokens::list_tokens))
        .route("/tokens/{id}", delete(api::tokens::revoke_token))
        // Snake management endpoints
        .route("/snakes", get(api::snakes::list_snakes))
        .route("/snakes", post(api::snakes::create_snake))
        .route("/snakes/{id}", get(api::snakes::get_snake))
        .route("/snakes/{id}", put(api::snakes::update_snake))
        .route("/snakes/{id}", delete(api::snakes::delete_snake))
        // Games API endpoints (list, create, details)
        .route("/games", post(api::games::create_game))
        .route("/games", get(api::games::list_games))
        .route("/games/{id}/details", get(api::games::show_game))
        .route("/games/status", post(api::games::batch_game_status))
        .route("/admin/stats", get(admin::stats_json))
        // Leaderboard API endpoints
        .route("/leaderboards", get(api::leaderboards::list_leaderboards))
        .route(
            "/leaderboards/{id}/rankings",
            get(api::leaderboards::get_rankings),
        )
        .route(
            "/leaderboards/{id}/entries",
            post(api::leaderboards::create_entry),
        )
        .route(
            "/leaderboards/{id}/entries/{battlesnake_id}",
            delete(api::leaderboards::delete_entry),
        )
        .layer(cors);

    axum::Router::new()
        // Public pages
        .route("/", get(root_page))
        // Policy pages
        .route("/conduct", get(policy::conduct_page))
        .route("/privacy", get(policy::privacy_page))
        .route("/terms", get(policy::terms_page))
        // Profile page - requires authentication
        .route("/me", get(profile_page).post(update_profile))
        // Appearance (theme) preference - requires authentication
        .route("/settings/appearance", post(settings::update_appearance))
        // GitHub OAuth routes
        .route("/auth/github", get(github_auth::github_auth))
        .route(
            "/auth/github/callback",
            get(github_auth::github_auth_callback),
        )
        .route("/auth/logout", get(github_auth::logout))
        .route("/auth/cli-token", get(github_auth::cli_token_page))
        // Battlesnake routes
        .route("/customizations", get(customizations::list_customizations))
        .route("/claim", get(claim::claim_page))
        .route("/claim", post(claim::submit_claim))
        .route("/claim/email", get(claim::email_claim_page))
        .route("/claim/email", post(claim::submit_email_claim))
        .route(
            "/claim/email/verify",
            get(claim::email_claim_verify_page).post(claim::complete_email_claim),
        )
        .route("/battlesnakes", get(battlesnake::list_battlesnakes))
        .route("/battlesnakes/new", get(battlesnake::new_battlesnake))
        .route(
            "/battlesnakes",
            axum::routing::post(battlesnake::create_battlesnake),
        )
        .route(
            "/battlesnakes/{id}/edit",
            get(battlesnake::edit_battlesnake),
        )
        .route(
            "/battlesnakes/{id}/update",
            axum::routing::post(battlesnake::update_battlesnake),
        )
        .route(
            "/battlesnakes/{id}/delete",
            axum::routing::post(battlesnake::delete_battlesnake),
        )
        .route(
            "/battlesnakes/{id}/profile",
            get(battlesnake::view_battlesnake_profile),
        )
        .route(
            "/battlesnakes/{id}/test",
            axum::routing::post(battlesnake::test_battlesnake),
        )
        .route(
            "/battlesnakes/{id}/reactivate",
            axum::routing::post(battlesnake::reactivate_battlesnake),
        )
        // Game routes
        .route("/games/new", get(game::new_game))
        .route("/games/{id}", get(game::view_game))
        .route("/games/flow/{id}", get(game::show_game_flow))
        .route(
            "/games/flow/{id}/reset",
            axum::routing::post(game::reset_snake_selections),
        )
        .route(
            "/games/flow/{id}/create",
            axum::routing::post(game::create_game),
        )
        .route(
            "/games/flow/{id}/add-snake/{snake_id}",
            axum::routing::post(game::add_battlesnake),
        )
        .route(
            "/games/flow/{id}/remove-snake/{snake_id}",
            axum::routing::post(game::remove_battlesnake),
        )
        .route("/games/flow/{id}/search", get(game::search_battlesnakes))
        // Leaderboard routes
        .route("/leaderboards", get(leaderboard::list_leaderboards))
        .route("/leaderboards/{id}", get(leaderboard::show_leaderboard))
        .route(
            "/leaderboards/{id}/join",
            axum::routing::post(leaderboard::join_leaderboard),
        )
        .route(
            "/leaderboards/{id}/leave",
            axum::routing::post(leaderboard::leave_leaderboard),
        )
        .route(
            "/leaderboards/{id}/entries/{entry_id}",
            get(leaderboard::show_leaderboard_entry),
        )
        // Tournament routes
        .route("/tournaments", get(tournament::list_tournaments))
        .route(
            "/tournaments",
            axum::routing::post(tournament::create_tournament),
        )
        .route("/tournaments/new", get(tournament::new_tournament))
        .route("/tournaments/{id}", get(tournament::show_tournament))
        .route("/tournaments/{id}/edit", get(tournament::edit_tournament))
        .route(
            "/tournaments/{id}/settings",
            axum::routing::post(tournament::update_settings),
        )
        .route(
            "/tournaments/{id}/register",
            axum::routing::post(tournament::register_snake),
        )
        .route(
            "/tournaments/{id}/unregister",
            axum::routing::post(tournament::unregister_snake),
        )
        .route(
            "/tournaments/{id}/seed",
            axum::routing::post(tournament::move_seed),
        )
        .route(
            "/tournaments/{id}/status",
            axum::routing::post(tournament::update_status),
        )
        .route(
            "/tournaments/{id}/start",
            axum::routing::post(tournament::start_tournament),
        )
        .route(
            "/tournaments/{id}/run-round",
            axum::routing::post(tournament::run_round),
        )
        .route(
            "/tournaments/{id}/reset",
            axum::routing::post(tournament::reset_tournament),
        )
        .route(
            "/tournaments/{id}/import-leaderboard",
            axum::routing::post(tournament::import_leaderboard),
        )
        // Admin routes
        .route("/admin", get(admin::dashboard))
        // Game API routes for board viewer (with CORS)
        .nest("/api", api_routes)
        // Static files
        .route(
            "/static/{*path}",
            get(crate::static_assets::serve_static_file),
        )
        // Internal routes
        .route("/_/version", get(version_page))
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            inject_trace_context,
        ))
        .with_state(app_state)
}

async fn root_page(
    _: State<AppState>,
    auth::OptionalUser(user): auth::OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page(
        "Home".to_string(),
        Box::new(html! {
            div {
                @if let Some(user) = user {
                    div class="user-info" {
                        img src=(user.github_avatar_url.unwrap_or_default()) alt="Avatar" style="width: 50px; height: 50px; border-radius: 50%;" {}
                        p { "Welcome, " (user.github_login) "!" }
                        @if let Some(name) = user.github_name {
                            p { "Name: " (name) }
                        }
                        // Section links live in the global nav now; only
                        // actions the nav doesn't offer stay here.
                        div class="user-actions" style="margin-top: 10px;" {
                            a href="/me" class="btn" { "Profile" }
                            a href="/auth/logout" class="btn" { "Logout" }
                        }
                    }
                } @else {
                    div class="login" {
                        p { "You are not logged in." }
                        a href="/auth/github" { "Login with GitHub" }
                    }
                }
                div class="content" style="margin-top: 20px;" {
                    h1 { "Hello, world!" }
                    p { "Welcome to the Arena application!" }
                }
            }
        }),
    ))
}

/// Profile page that requires authentication
#[allow(clippy::too_many_lines)]
async fn profile_page(
    auth::CurrentUser(user): auth::CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        "My Profile".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "My Profile" }
                div class="sub" { "How you appear across the Arena — and the account behind it." }
            }

            header class="profile-head" {
                img class="avatar" src=(user.github_avatar_url.clone().unwrap_or_default()) alt="";
                div class="who" {
                    @if let Some(name) = user.display_name.as_ref().filter(|n| !n.is_empty()) {
                        h2 { (name) }
                    } @else {
                        h2 { (user.github_login) }
                    }
                    div class="meta" {
                        "@" (user.github_login)
                        @if !user.pronouns.is_empty() { " · " (user.pronouns) }
                        @if !user.country.is_empty() { " · " (user.country) }
                        " · joined " (user.created_at.format("%b %Y"))
                    }
                    @if !user.backstory.is_empty() {
                        p class="bio" { (user.backstory) }
                    }
                }
            }

            div class="profile-actions" {
                a href="/battlesnakes" class="btn" { "Manage Battlesnakes" }
                a href="/games/new" class="btn" { "Create New Game" }
                a href="/" class="btn" { "Back to Home" }
                a href="/auth/logout" class="btn" { "Logout" }
            }

            div class="grid" {
                div {
                    h2 { "Edit Profile" }
                    form class="form-stack" action="/me" method="post" {
                        div class="field" {
                            label for="display_name" { "Display Name" }
                            input type="text" id="display_name" name="display_name" maxlength="100"
                                value=(user.display_name.as_deref().unwrap_or(""));
                            p class="help" { "Shown instead of your GitHub login" }
                        }

                        div class="field" {
                            label for="pronouns" { "Pronouns" }
                            input type="text" id="pronouns" name="pronouns" maxlength="50"
                                value=(user.pronouns);
                            p class="help" { "Max 50 characters" }
                        }

                        div class="field" {
                            label for="country" { "Country" }
                            input type="text" id="country" name="country" maxlength="100"
                                value=(user.country);
                            p class="help" { "Max 100 characters" }
                        }

                        div class="field" {
                            label for="backstory" { "Backstory" }
                            textarea id="backstory" name="backstory" maxlength="2000" { (user.backstory) }
                            p class="help" { "Max 2000 characters. Plain text, no markdown." }
                        }

                        div class="form-cta" {
                            button type="submit" class="btn solid" { "Save Changes" }
                        }
                    }
                }

                aside class="rail" {
                    div class="block" {
                        h3 { "Account Details" }
                        dl class="meta-list" {
                            @if let Some(name) = user.github_name.as_ref() {
                                div { dt { "Name:" } dd { (name) } }
                            }
                            @if let Some(email) = user.github_email.as_ref() {
                                div { dt { "Email:" } dd { (email) } }
                            }
                            div { dt { "GitHub ID:" } dd { (user.external_github_id) } }
                            div { dt { "Account created:" } dd { (user.created_at.format("%Y-%m-%d")) } }
                            div { dt { "Last updated:" } dd { (user.updated_at.format("%Y-%m-%d")) } }
                        }
                    }
                }
            }
        }),
        flash,
    ))
}

/// POST handler for updating profile fields
async fn update_profile(
    State(state): State<AppState>,
    auth::CurrentUserWithSession { user, session }: auth::CurrentUserWithSession,
    Form(form): Form<UpdateProfileForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let display_name = form.display_name.trim();
    let pronouns = form.pronouns.trim();
    let country = form.country.trim();
    let backstory = form.backstory.trim();

    if let Err(msg) =
        crate::models::user::validate_profile_fields(display_name, pronouns, country, backstory)
    {
        crate::models::session::set_flash_message(
            &state.db,
            session.session_id,
            msg,
            crate::models::session::FLASH_TYPE_ERROR,
        )
        .await
        .wrap_err("Failed to set flash message")?;

        return Ok(Redirect::to("/me").into_response());
    }

    crate::models::user::update_profile_fields(
        &state.db,
        user.user_id,
        display_name,
        pronouns,
        country,
        backstory,
    )
    .await
    .wrap_err("Failed to update profile")?;

    crate::models::session::set_flash_message(
        &state.db,
        session.session_id,
        "Profile updated successfully!".to_string(),
        crate::models::session::FLASH_TYPE_SUCCESS,
    )
    .await
    .wrap_err("Failed to set flash message")?;

    Ok(Redirect::to("/me").into_response())
}

/// Middleware to extract GCP trace context from the `X-Cloud-Trace-Context` header
/// and store it in the current span's extensions for the GCP JSON formatter.
async fn inject_trace_context(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(header) = request.headers().get("x-cloud-trace-context")
        && let Ok(value) = header.to_str()
        && let Some(trace_path) =
            crate::telemetry::extract_trace_context(value, state.config.gcp_project_id.as_deref())
    {
        crate::telemetry::insert_trace_context_into_current_span(trace_path);
    }
    next.run(request).await
}

/// Version info page showing build metadata
async fn version_page() -> impl IntoResponse {
    html! {
        html {
            head {
                title { "Version Info" }
                style {
                    "body { font-family: monospace; padding: 20px; max-width: 800px; margin: 0 auto; }"
                    "h1 { color: #333; }"
                    "table { border-collapse: collapse; width: 100%; }"
                    "th, td { text-align: left; padding: 8px; border-bottom: 1px solid #ddd; }"
                    "th { background-color: #f5f5f5; }"
                    ".value { font-family: monospace; color: #0066cc; }"
                }
            }
            body {
                h1 { "Arena Version Info" }
                table {
                    tr {
                        th { "Property" }
                        th { "Value" }
                    }
                    tr {
                        td { "Git SHA" }
                        td class="value" { (env!("VERGEN_GIT_SHA")) }
                    }
                    tr {
                        td { "Git Branch" }
                        td class="value" { (option_env!("VERGEN_GIT_BRANCH").unwrap_or("unknown")) }
                    }
                    tr {
                        td { "Git Commit Date" }
                        td class="value" { (option_env!("VERGEN_GIT_COMMIT_TIMESTAMP").unwrap_or("unknown")) }
                    }
                    tr {
                        td { "Build Timestamp" }
                        td class="value" { (option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or("unknown")) }
                    }
                    tr {
                        td { "Rust Version" }
                        td class="value" { (option_env!("VERGEN_RUSTC_SEMVER").unwrap_or("unknown")) }
                    }
                    tr {
                        td { "Cargo Profile" }
                        td class="value" { (option_env!("VERGEN_CARGO_PROFILE").unwrap_or("unknown")) }
                    }
                }
            }
        }
    }
}
