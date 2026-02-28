use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use maud::html;
use tower_http::cors::{Any, CorsLayer};

use crate::{components::page_factory::PageFactory, errors::ServerResult, state::AppState};

// Include route modules
pub mod admin;
pub mod api;
pub mod auth;
pub mod battlesnake;
pub mod game;
pub mod github_auth;
pub mod leaderboard;

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
        // Profile page - requires authentication
        .route("/me", get(profile_page))
        // GitHub OAuth routes
        .route("/auth/github", get(github_auth::github_auth))
        .route(
            "/auth/github/callback",
            get(github_auth::github_auth_callback),
        )
        .route("/auth/logout", get(github_auth::logout))
        .route("/auth/cli-token", get(github_auth::cli_token_page))
        // Battlesnake routes
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
        // Game routes
        .route("/games", get(game::list_games))
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
        .layer(axum::middleware::from_fn(inject_trace_context))
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
                        div class="user-actions" style="margin-top: 10px;" {
                            a href="/me" class="btn btn-primary" { "Profile" }
                            a href="/battlesnakes" class="btn btn-primary" { "Battlesnakes" }
                            a href="/leaderboards" class="btn btn-primary" { "Leaderboards" }
                            a href="/auth/logout" class="btn btn-secondary" { "Logout" }
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
async fn profile_page(
    auth::CurrentUser(user): auth::CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page(
        "My Profile".to_string(),
        Box::new(html! {
            div {
                h1 { "My Profile" }

                div class="profile-card" style="border: 1px solid #ddd; border-radius: 8px; padding: 20px; margin: 20px 0; max-width: 600px;" {
                    div class="profile-header" style="display: flex; align-items: center; margin-bottom: 20px;" {
                        img src=(user.github_avatar_url.unwrap_or_default()) alt="Avatar" style="width: 100px; height: 100px; border-radius: 50%; margin-right: 20px;" {}

                        div {
                            h2 style="margin: 0 0 10px 0;" { (user.github_login) }
                            @if let Some(name) = user.github_name.as_ref() {
                                p style="margin: 0; color: #666;" { (name) }
                            }
                            @if let Some(email) = user.github_email.as_ref() {
                                p style="margin: 0; color: #666;" { (email) }
                            }
                        }
                    }

                    div class="profile-details" {
                        h3 { "Account Details" }
                        p { "GitHub ID: " (user.external_github_id) }
                        p { "Account created: " (user.created_at.format("%Y-%m-%d %H:%M:%S")) }
                        p { "Last updated: " (user.updated_at.format("%Y-%m-%d %H:%M:%S")) }
                    }

                    div class="profile-actions" style="margin-top: 20px;" {
                        h3 { "Your Battlesnakes" }
                        p { "Manage your Battlesnake collection." }
                        a href="/battlesnakes" class="btn btn-primary" { "Manage Battlesnakes" }

                        h3 class="mt-4" { "Games" }
                        p { "Create and view games with your Battlesnakes." }
                        div {
                            a href="/games/new" class="btn btn-primary" { "Create New Game" }
                            a href="/games" class="btn btn-secondary ms-2" { "View All Games" }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/" { "Back to Home" }
                    span { " | " }
                    a href="/auth/logout" { "Logout" }
                }
            }
        }),
    ))
}

/// Middleware to extract GCP trace context from the `X-Cloud-Trace-Context` header
/// and store it in the current span's extensions for the GCP JSON formatter.
async fn inject_trace_context(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(header) = request.headers().get("x-cloud-trace-context")
        && let Ok(value) = header.to_str()
        && let Some(trace_path) = crate::telemetry::extract_trace_context(value)
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
