use arena::design_kit::{AssetKind, catalog_shapes::CatalogShape};
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect},
    routing::{delete, get, post, put},
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

use crate::{
    components::{
        avatar::user_avatar,
        page_factory::PageFactory,
        snake_board::{Direction, snake_geometry},
    },
    customizations::chip_color,
    errors::ServerResult,
    models::{
        battlesnake as battlesnake_model,
        leaderboard::{self as leaderboard_model, ActivityFeedEntry, Leaderboard, RankedEntry},
    },
    state::AppState,
};

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
#[cfg(test)]
mod branding_tests;
pub mod claim;
pub mod customizations;
pub mod game;
pub mod github_auth;
#[cfg(test)]
mod github_auth_tests;
pub mod health;
pub mod leaderboard;
#[cfg(test)]
mod notification_tests;
pub mod notifications;
pub mod oauth;
#[cfg(test)]
mod oauth_tests;
pub mod og;
pub mod pagination;
pub mod play_notices;
pub mod policy;
#[cfg(test)]
mod private_leaderboard_tests;
#[cfg(test)]
mod public_name_tests;
pub mod redirects;
pub mod saved_games;
pub mod settings;
pub mod stats;
pub mod studio;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tournament;
pub mod users;
pub mod welcome_back;

pub fn routes(app_state: AppState) -> cja::Result<axum::Router> {
    // CORS layer for API routes - allows board.battlesnake.com to access our API
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // API routes with CORS enabled (for board viewer and CLI/programmatic access)
    //
    // NOTE: every authenticated /api route must appear in ROUTES in
    // server/src/routes/api/route_coverage_tests.rs (zero-permission token
    // must get 403).
    // API-ROUTES-START
    let api_routes = axum::Router::new()
        .route("/games/{id}", get(game::get_game_info))
        .route("/games/{id}/events", get(game::game_events_websocket))
        // Engine-compatible frame history (public, used by the GIF exporter)
        .route("/games/{id}/frames", get(game::get_game_frames))
        // One turn's `/move` request body, as a chosen snake (public)
        .route("/games/{id}/move-request", get(game::get_move_request))
        .route("/tokens", post(api::tokens::create_token))
        .route("/tokens", get(api::tokens::list_tokens))
        .route("/tokens/{id}", delete(api::tokens::revoke_token))
        .route("/me", get(api::me::me))
        // Snake management endpoints
        .route("/snakes", get(api::snakes::list_snakes))
        .route("/snakes", post(api::snakes::create_snake))
        .route("/snakes/{id}", get(api::snakes::get_snake))
        .route("/snakes/{id}", put(api::snakes::update_snake))
        .route("/snakes/{id}", delete(api::snakes::delete_snake))
        .route("/snakes/{id}/hosting", put(api::snakes::mark_hosting))
        // Games API endpoints (list, create, details)
        .route("/games", post(api::games::create_game))
        .route("/games", get(api::games::list_games))
        .route("/games/{id}/details", get(api::games::show_game))
        .route("/games/status", post(api::games::batch_game_status))
        .route("/admin/stats", get(admin::stats_json))
        .route("/stats", get(stats::stats_json))
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
    // API-ROUTES-END

    let router = axum::Router::new()
        // Public pages
        .route("/", get(root_page))
        .route("/robots.txt", get(robots_txt))
        .route("/health", get(health::health))
        .route("/favicon.ico", get(serve_favicon))
        // Social cards (og:image)
        .route(og::DEFAULT_CARD_PATH, get(og::default_card))
        .route(
            "/og/customizations/{kind}/{file}",
            get(og::customization_png),
        )
        .route("/og/games/{file}", get(og::game_card))
        .route("/og/battlesnakes/{file}", get(og::snake_card))
        .route("/og/users/{file}", get(og::player_card))
        .route("/og/leaderboards/{id}", get(og::leaderboard_card))
        .route("/og/leaderboards/{id}/entries/{file}", get(og::entry_card))
        .route("/og/tournaments/{file}", get(og::tournament_card))
        // Policy pages
        .route("/conduct", get(policy::conduct_page))
        .route("/privacy", get(policy::privacy_page))
        .route("/terms", get(policy::terms_page))
        // Public player directory
        .route("/players", get(users::list_players))
        .route("/rankings", get(users::list_global_rankings))
        // Public user profiles. The two-segment form is the stable one — the
        // UUID is authoritative and the login is a cosmetic slug — while the
        // login-only form stays for compatibility and hand-typed URLs.
        .route("/users/{login}", get(users::show_user_profile))
        .route(
            "/users/{login}/{user_id}",
            get(users::show_user_profile_by_id),
        )
        // Profile page - requires authentication
        .route("/me", get(profile_page).post(update_profile))
        .route(
            "/settings/notifications",
            post(settings::update_notification_preference),
        )
        .route(
            "/settings/notification-email",
            post(settings::save_notification_email),
        )
        .route(
            "/settings/notification-email/resend",
            post(settings::resend_notification_email),
        )
        .route(
            "/settings/notification-email/confirm",
            get(settings::confirm_notification_email_page)
                .post(settings::confirm_notification_email),
        )
        .route("/notifications", get(notifications::page))
        .route("/notifications/drawer", get(notifications::drawer))
        .route("/notifications/more", get(notifications::more))
        .route("/notifications/count", get(notifications::count))
        .route("/notifications/{id}/open", post(notifications::open))
        .route("/notifications/read-all", post(notifications::read_all))
        .route(
            "/notifications/unsubscribe/{token}",
            get(settings::unsubscribe_get).post(settings::unsubscribe_post),
        )
        .route(
            "/play-notices/unsubscribe/{token}",
            get(play_notices::unsubscribe_get).post(play_notices::unsubscribe_post),
        )
        // Appearance (theme) preference - requires authentication
        .route("/settings/appearance", post(settings::update_appearance))
        // GitHub OAuth routes
        .route("/auth/github", get(github_auth::github_auth))
        .route(
            "/auth/github/callback",
            get(github_auth::github_auth_callback),
        )
        .route("/auth/logout", get(github_auth::logout))
        // OAuth provider (main router, NOT the /api nest)
        .route("/oauth/authorize", get(oauth::authorize))
        .route("/oauth/authorize/decision", post(oauth::authorize_decision))
        .route("/oauth/token", post(oauth::token))
        // Authorized apps (player-facing OAuth grant management)
        .route(
            "/settings/authorized-apps",
            get(settings::authorized_apps_page),
        )
        .route(
            "/settings/authorized-apps/{id}/revoke",
            post(settings::revoke_authorized_app),
        )
        .route(
            "/settings/authorized-apps/{id}/snakes",
            post(settings::edit_authorized_app_snakes),
        )
        // Battlesnake routes
        .route("/customizations", get(customizations::list_customizations))
        .route(
            "/customizations/achievements",
            get(customizations::achievements_page),
        )
        .route("/customizations/redeem", post(customizations::redeem_code))
        .route(
            "/customizations/unlock",
            post(customizations::unlock_customization),
        )
        // Head & Tail Studio, and its guide (`/studio` redirects to the studio)
        .route("/customizations/studio", get(studio::studio_page))
        .route("/customizations/studio/guide", get(studio::guide_page))
        .route(
            "/customizations/studio/process",
            studio::process_route(app_state.clone()),
        )
        // Landing page for the play.battlesnake.com shutdown banner
        .route(welcome_back::PATH, get(welcome_back::welcome_back_page))
        .route("/claim", get(claim::claim_page))
        .route("/claim", post(claim::submit_claim))
        .route("/claim/dismiss-prompt", post(claim::dismiss_claim_prompt))
        .route("/claim/email", get(claim::email_claim_page))
        .route("/claim/email", post(claim::submit_email_claim))
        .route(
            "/claim/email/verify",
            get(claim::email_claim_verify_page).post(claim::complete_email_claim),
        )
        // Public snake directory (browse + challenge)
        .route("/snakes", get(battlesnake::list_public_battlesnakes))
        .route(
            "/battlesnakes/{id}/challenge",
            post(game::challenge_battlesnake),
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
        .route(
            "/games/{id}/rematch",
            axum::routing::post(game::rematch_game),
        )
        .route(
            "/games/{id}/save",
            axum::routing::post(saved_games::save_game),
        )
        .route(
            "/saved-games/{id}/delete",
            axum::routing::post(saved_games::delete_saved_game),
        )
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
            "/games/flow/{id}/configure",
            axum::routing::post(game::configure_game),
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
        .route(
            "/admin/codes",
            get(admin::list_codes).post(admin::create_code),
        )
        .route("/admin/codes/{id}/disable", post(admin::disable_code))
        .route(
            "/admin/leaderboards",
            get(admin::list_leaderboards).post(admin::create_private_leaderboard),
        )
        .route(
            "/admin/leaderboards/{id}/invite-code",
            post(admin::regenerate_invite_code),
        )
        .route("/admin/emails", get(admin::email_catalog))
        .route("/admin/emails/{kind}", get(admin::email_preview))
        .route(
            "/admin/emails/{kind}/send-test",
            post(admin::send_test_email),
        )
        .route("/admin/play-notices", get(play_notices::admin_page))
        .route(
            "/admin/play-notices/{wave}/send-test",
            post(play_notices::send_test),
        )
        .route("/stats", get(stats::stats_page))
        .route("/admin/moderation", get(admin::moderation_queue))
        .route(
            "/admin/oauth-clients",
            get(admin::list_oauth_clients).post(admin::create_oauth_client),
        )
        .route(
            "/admin/oauth-clients/{id}/rotate",
            post(admin::rotate_oauth_client_secret),
        )
        .route(
            "/admin/oauth-clients/{id}/disable",
            post(admin::disable_oauth_client),
        )
        .route(
            "/admin/oauth-clients/{id}/enable",
            post(admin::enable_oauth_client),
        )
        // Game API routes for board viewer (with CORS)
        .nest("/api", api_routes)
        // Internal routes
        .route("/_/version", get(version_page))
        // Unknown routes get a branded 404 instead of an empty response
        .fallback(not_found_page);

    // Community short links (/docs, /discord, ...) carried over from play
    let assets = cja::assets::router::<AppState>(&[&crate::arena_assets::MANIFEST])
        .wrap_err("asset router collision")?;
    Ok(redirects::register(router.merge(assets))
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            inject_trace_context,
        ))
        .with_state(app_state))
}

async fn serve_favicon() -> impl IntoResponse {
    favicon_response(&crate::arena_assets::MANIFEST)
}

fn favicon_response(manifest: &cja::assets::Manifest) -> axum::response::Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    match manifest
        .entries
        .iter()
        .find(|asset| asset.logical_name == "favicon.svg")
    {
        Some(asset) => (
            [
                (CONTENT_TYPE, "image/svg+xml"),
                (CACHE_CONTROL, "public, max-age=3600"),
            ],
            asset.bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn not_found_page(page_factory: PageFactory) -> impl IntoResponse {
    render_not_found(page_factory)
}

/// Render the branded 404 page. Shared by the router fallback (unknown paths)
/// and the [`UuidPath`] extractor (malformed `{id}` segments) so a bad URL
/// always lands on the same friendly page instead of a raw framework error.
pub fn render_not_found(page_factory: PageFactory) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        page_factory.create_page(
            "Page Not Found".to_string(),
            Box::new(html! {
                div class="home" {
                    section class="section" {
                        h1 { "404 — Page not found" }
                        p class="empty" {
                            "This page doesn't exist. It may have been eliminated, "
                            "or the link points at something from the old site."
                        }
                        div class="cta-row" {
                            a class="btn solid" href="/" { "Back to Home" }
                            a class="btn" href="/leaderboards" { "View Leaderboards" }
                        }
                    }
                }
            }),
        ),
    )
        .into_response()
}

/// Stand-in for the profile of a battlesnake its owner deleted. Past games
/// still link here, so say what happened instead of a bare 404.
pub fn render_deleted_snake(page_factory: PageFactory, name: &str) -> axum::response::Response {
    (
        StatusCode::GONE,
        page_factory.create_page(
            format!("{name} (deleted)"),
            Box::new(html! {
                div class="home" {
                    section class="section" {
                        h1 { (name) }
                        p class="empty" {
                            "This battlesnake was deleted by its owner. "
                            "Games it played are still viewable."
                        }
                        div class="cta-row" {
                            a class="btn solid" href="/" { "Back to Home" }
                            a class="btn" href="/leaderboards" { "View Leaderboards" }
                        }
                    }
                }
            }),
        ),
    )
        .into_response()
}

/// Response for a snake page whose live snake lookup came back empty: the
/// deleted page if the owner deleted it, otherwise a plain 404.
pub async fn render_missing_snake(
    db: &sqlx::PgPool,
    battlesnake_id: uuid::Uuid,
    page_factory: PageFactory,
) -> cja::Result<axum::response::Response> {
    Ok(
        match crate::models::battlesnake::get_deleted_battlesnake_name(db, battlesnake_id).await? {
            Some(name) => render_deleted_snake(page_factory, &name),
            None => render_not_found(page_factory),
        },
    )
}

/// Path extractor for a single `{id}` UUID segment on user-facing HTML pages.
///
/// Behaves like [`axum::extract::Path<Uuid>`] on success, but when the segment
/// isn't a valid UUID it renders the branded 404 page instead of axum's raw
/// `Invalid URL: Cannot parse ...` plaintext (which a player sees whenever a
/// shared link is truncated or a URL is fat-fingered). Use this on GET routes
/// that render a page; the JSON `/api` routes keep `Path<Uuid>` since a
/// plaintext 400 is the right answer for API clients.
pub struct UuidPath(pub uuid::Uuid);

impl axum::extract::FromRequestParts<AppState> for UuidPath {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<uuid::Uuid>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(id)) => Ok(UuidPath(id)),
            Err(_) => {
                let page_factory = PageFactory::from_request_parts(parts, state).await?;
                Err(render_not_found(page_factory))
            }
        }
    }
}

/// Two-segment variant of [`UuidPath`] for routes like
/// `/leaderboards/{id}/entries/{battlesnake_id}`. Renders the branded 404 when
/// either segment isn't a valid UUID.
pub struct UuidPath2(pub uuid::Uuid, pub uuid::Uuid);

impl axum::extract::FromRequestParts<AppState> for UuidPath2 {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<(uuid::Uuid, uuid::Uuid)>::from_request_parts(parts, state)
            .await
        {
            Ok(axum::extract::Path((a, b))) => Ok(UuidPath2(a, b)),
            Err(_) => {
                let page_factory = PageFactory::from_request_parts(parts, state).await?;
                Err(render_not_found(page_factory))
            }
        }
    }
}

/// Crawlers are welcome on the public pages; keep them out of auth flows,
/// account management, and the JSON API.
async fn robots_txt() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        concat!(
            "User-agent: *\n",
            "Disallow: /api/\n",
            "Disallow: /auth/\n",
            "Disallow: /admin\n",
            "Disallow: /me\n",
            "Disallow: /settings/\n",
            "Disallow: /claim\n",
            "Disallow: /battlesnakes\n",
            "Disallow: /games/flow/\n",
            "Disallow: /games/new\n",
        ),
    )
}

async fn root_page(
    State(state): State<AppState>,
    auth::OptionalUser(user): auth::OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // The home page features the first active public leaderboard: its recent games
    // feed the rail, and its top five make the ladder preview.
    // User-independent, so it sits behind a short TTL cache — this is the
    // highest-traffic anonymous page.
    let feed = match state.home_feed_cache.get() {
        Some(feed) => feed,
        None => state.home_feed_cache.put(
            leaderboard_model::load_home_feed(&state.db)
                .await
                .wrap_err("Failed to load home feed")?,
        ),
    };
    let featured = feed.featured.as_ref();
    let (activity, top_entries) = (&feed.activity, &feed.top_entries);

    let user_snakes = if let Some(user) = &user {
        battlesnake_model::get_battlesnakes_by_user_id(&state.db, user.user_id)
            .await
            .wrap_err("Failed to fetch user's battlesnakes")?
    } else {
        Vec::new()
    };

    Ok(page_factory.create_page(
        "Home".to_string(),
        Box::new(html! {
            div class="home" {
                @if let Some(user) = &user {
                    section class="welcome" {
                        (user_avatar(user.github_avatar_url.as_deref(), user.public_name(), "welcome-avatar"))
                        div class="who" {
                            h1 { "Welcome, " (user.public_name()) "!" }
                            p class="sub" {
                                @if user_snakes.is_empty() {
                                    "No snakes in your stable yet — deploy a server and claim a spot on the ladder."
                                } @else if user_snakes.len() == 1 {
                                    "One snake in your stable, playing around the clock."
                                } @else {
                                    (user_snakes.len()) " snakes in your stable, playing around the clock."
                                }
                            }
                        }
                        div class="cta-row" {
                            a class="btn" href="/me" { "Profile" }
                            a class="btn" href="/battlesnakes" { "My snakes" }
                            a class="btn" href="/auth/logout" { "Logout" }
                        }
                    }

                    @if user_snakes.is_empty() {
                        section class="section" {
                            h2 { "Get on the board" }
                            p class="empty" { "Register your first snake to start playing ranked games." }
                            a class="btn solid" href="/battlesnakes/new" { "Register a snake" }
                        }
                    } @else {
                        section class="section snakes" {
                            h2 { "Your snakes" }
                            div class="rows" {
                                @for snake in &user_snakes {
                                    a class="srow" href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} {
                                        span class="chip" style={"background:"(chip_color(&snake.color))} {}
                                        span class="sname" { (snake.name) }
                                        span class="surl hide-sm" { (snake.url) }
                                        @if snake.visibility == battlesnake_model::Visibility::Public {
                                            span class="badge ok" { "Public" }
                                        } @else {
                                            span class="badge" { "Private" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } @else {
                    section class="hero" {
                        div class="copy" {
                            div class="kicker" { "A game for programmers" }
                            h1 { "Battlesnake. Your code is the " em { "controller." } }
                            p class="lede" {
                                "Write a web server that plays snake. Deploy it in any language. "
                                "Climb the leaderboards while your code competes around the clock — "
                                "even while you sleep."
                            }
                            div class="cta-row" {
                                a class="btn solid" href="/auth/github" { "Sign in with GitHub" }
                                a class="btn" href="/leaderboards" { "Browse leaderboards" }
                            }
                            div class="cta-note" { code { "$ curl -X POST your-server.dev/move → {\"move\": \"up\"}" } }
                        }
                        div class="board-frame" {
                            (home_board())
                            div class="board-caption" {
                                span class="live-dot" {}
                                @if let Some(lb) = featured {
                                    (lb.name) " · ranked games around the clock"
                                } @else {
                                    "Standard 11×11 · ranked games around the clock"
                                }
                            }
                        }
                    }
                }

                @if user.is_none() {
                    section class="features" {
                        div class="feature" {
                            div class="num" { "01" }
                            h2 { "Automated leaderboards" }
                            p {
                                "Ranked matches run continuously. Your snake earns its rating in "
                                "thousands of games against developers worldwide, not a handful of "
                                "showcase matches."
                            }
                            a class="more" href="/leaderboards" { "See the rankings →" }
                        }
                        div class="feature" {
                            div class="num" { "02" }
                            h2 { "Any language, any stack" }
                            p {
                                "If it can answer an HTTP request in 500ms, it can play. Rust, "
                                "Python, TypeScript, COBOL if you're feeling dangerous — starter "
                                "projects for a dozen languages."
                            }
                            a class="more" href="https://docs.battlesnake.com" { "Read the docs →" }
                        }
                        div class="feature" {
                            div class="num" { "03" }
                            h2 { "Tournaments & community" }
                            p {
                                "Compete in seasonal championships, run brackets for your team or "
                                "classroom, and replay any game move-by-move to study exactly where "
                                "it went wrong."
                            }
                            a class="more" href="/tournaments" { "Follow the brackets →" }
                        }
                    }
                }

                (home_ladder_grid(featured, top_entries, activity))

                @if user.is_none() {
                    section class="statement" {
                        h2 { "As simple — or as " em { "unhinged" } " — as you want it to be." }
                        p {
                            "Start with an if-statement that avoids walls. End up with a minimax "
                            "search you think about in the shower. Battlesnake is open-ended by "
                            "design: how far you take it is up to you."
                        }
                        a class="btn solid" href="/auth/github" { "Get started" }
                    }
                }
            }
        }),
    )
    .with_description(
        "Write your own Battlesnake, join ranked leaderboards, and watch your \
         code battle other snakes around the clock.",
    ))
}

/// The hero board's scene, in game-API cells (head first, `(0, 0)` bottom-left):
/// the same two snakes and food the default OG card draws
/// (`crate::og::cards::home_board`); a test keeps the two in sync.
const HERO_PINK_BODY: &[(i32, i32)] = &[
    (3, 8),
    (3, 7),
    (4, 7),
    (4, 6),
    (4, 5),
    (5, 5),
    (5, 4),
    (5, 3),
];
const HERO_CREAM_BODY: &[(i32, i32)] = &[(8, 3), (8, 4), (7, 4), (7, 5), (7, 6), (8, 6), (9, 6)];
const HERO_FOOD: &[(i32, i32)] = &[(1, 9), (6, 8), (9, 1), (2, 2)];
/// `og::canvas::palette::pink()` / `palette::ink()` as CSS hex.
const HERO_PINK: &str = "#FF3D8A";
const HERO_CREAM: &str = "#F4EFEA";

/// Decorative 11×11 board for the logged-out hero. Intentionally dark-framed
/// in both themes (hardcoded colors match the mockup's board panel).
///
/// The heads and tails are the real catalog art — `smile`/`curled` for the
/// pink snake, `fang`/`sharp` for the cream one, as on the default OG card —
/// drawn as inline SVG from the embedded shape table (`og::board::shape`), so
/// nothing requests media.battlesnake.com. Each art cell keeps the plain dark
/// `.cell` background: it shows through the art's cut-out eyes and teeth.
fn home_board() -> Markup {
    const N: usize = 11;
    let pink_head = crate::og::board::shape(AssetKind::Head, "smile");
    let pink_tail = crate::og::board::shape(AssetKind::Tail, "curled");
    let cream_head = crate::og::board::shape(AssetKind::Head, "fang");
    let cream_tail = crate::og::board::shape(AssetKind::Tail, "sharp");

    // (class, inline style, head/tail art) per grid cell.
    let mut cells: Vec<(&'static str, Option<String>, Markup)> =
        vec![("cell", None, html! {}); N * N];
    let mut draw_snake = |body: &[(i32, i32)],
                          color: &str,
                          head: Option<&'static CatalogShape>,
                          tail: Option<&'static CatalogShape>| {
        let geometry = snake_geometry(N as i32, N as i32, body);
        let last = body.len() - 1;
        for (i, &(x, y)) in body.iter().enumerate() {
            // Game-API y grows up; CSS grid rows grow down.
            let at = (N as i32 - 1 - y) as usize * N + x as usize;
            match geometry.as_ref() {
                Some(g) if i == 0 => {
                    cells[at] = (
                        "cell",
                        None,
                        hero_art(AssetKind::Head, head, g.head.dir, color, false),
                    );
                }
                Some(g) if i == last => {
                    let dir = g.tail.map_or(Direction::Right, |t| t.dir);
                    cells[at] = (
                        "cell",
                        None,
                        hero_art(AssetKind::Tail, tail, dir, color, true),
                    );
                }
                _ => cells[at] = ("cell seg", Some(format!("background:{color}")), html! {}),
            }
        }
    };
    draw_snake(HERO_PINK_BODY, HERO_PINK, pink_head, pink_tail);
    draw_snake(HERO_CREAM_BODY, HERO_CREAM, cream_head, cream_tail);
    for &(x, y) in HERO_FOOD {
        cells[(N as i32 - 1 - y) as usize * N + x as usize] = ("cell food", None, html! {});
    }

    html! {
        div class="board" aria-hidden="true" {
            @for (class, style, art) in &cells {
                div class=(class) style=[style.as_deref()] { (art) }
            }
        }
    }
}

/// One catalog shape as inline SVG filling its hero-board cell: the shape's
/// `d` path verbatim in its 100×100 box, turned by the direction's transform
/// (head art faces right, tail art joins the body on its left edge — the
/// studio board's transforms), filled with the snake's colour. `faded` keeps
/// the old tail square's .55 opacity.
fn hero_art(
    kind: AssetKind,
    shape: Option<&'static CatalogShape>,
    dir: Direction,
    color: &str,
    faded: bool,
) -> Markup {
    let Some(shape) = shape else {
        return html! {};
    };
    let (class, transform) = match kind {
        AssetKind::Head => ("head", dir.head_transform()),
        AssetKind::Tail => ("tail", dir.tail_transform()),
    };
    html! {
        svg class=(format!("{class} {}", dir.as_str())) viewBox="0 0 100 100"
            opacity=[faded.then_some("0.55")] {
            g transform=[(!transform.is_empty()).then_some(transform)] {
                path d=(shape.d) fill-rule=(shape.fill_rule.as_svg()) fill=(color) {}
            }
        }
    }
}

/// Ladder preview (top five by rating) + recent games rail, shared by the
/// logged-in and logged-out home page.
fn home_ladder_grid(
    featured: Option<&Leaderboard>,
    top_entries: &[RankedEntry],
    activity: &[ActivityFeedEntry],
) -> Markup {
    html! {
        div class="grid" {
            section class="ladder" {
                h2 { "Top of the ladder" }
                @if let Some(lb) = featured {
                    p class="ladder-sub" { (lb.name) " — current standings, updated every game." }
                }
                @if top_entries.is_empty() {
                    p class="empty" { "No ranked snakes yet. The ladder is wide open." }
                } @else {
                    table class="data" {
                        thead {
                            tr {
                                th { "Rank" }
                                th { "Snake" }
                                th class="r" { "Rating" }
                            }
                        }
                        tbody {
                            @for (i, entry) in top_entries.iter().enumerate() {
                                tr class=[(i == 0).then_some("top")] {
                                    td class="rank" { "#" (i + 1) }
                                    td {
                                        div class="snake-cell" {
                                            span class="chip" style={"background:"(chip_color(&entry.snake_color))} {}
                                            span {
                                                @if let Some(lb) = featured {
                                                    a class="name" href={"/leaderboards/"(lb.leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} { (entry.snake_name) }
                                                } @else {
                                                    span class="name" { (entry.snake_name) }
                                                }
                                                span class="owner" { "by " (crate::components::flag::country_flag(&entry.owner_country)) (entry.owner_name) }
                                            }
                                        }
                                    }
                                    td class="r rating" { (format!("{:.1}", entry.display_score)) }
                                }
                            }
                        }
                    }
                    @if let Some(lb) = featured {
                        a class="more" href={"/leaderboards/"(lb.leaderboard_id)} { "Full standings →" }
                    }
                }
            }

            aside class="rail" {
                div class="block" {
                    h3 { span class="live-dot" {} "Recent games" }
                    @if activity.is_empty() {
                        p class="railp" { "No games yet — be the first on the board." }
                    } @else {
                        ul class="feed" {
                            @for event in &activity[..activity.len().min(8)] {
                                li {
                                    span class="t" { (home_fmt_ago(event.created_at)) }
                                    span {
                                        @if let Some(lb) = featured {
                                            a href={"/leaderboards/"(lb.leaderboard_id)"/entries/"(event.leaderboard_entry_id)} {
                                                b { (event.snake_name) }
                                            }
                                        } @else {
                                            b { (event.snake_name) }
                                        }
                                        " "
                                        span class={"place p"(event.placement)} { (home_ordinal(event.placement)) }
                                        " "
                                        @match home_delta(event.display_score_change) {
                                            (true, delta) => span class="delta up" { (delta) },
                                            (false, delta) => span class="delta down" { (delta) },
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Compact relative time for the home rail feed ("2m ago").
fn home_fmt_ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - t).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn home_ordinal(n: i32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Rating delta for the home activity rail, rounded to display
/// precision *before* the sign check so a −0.04 change can't render as a red
/// "-0.0". Returns the up/down flag alongside the formatted string so the CSS
/// class always agrees with the printed sign.
fn home_delta(change: f64) -> (bool, String) {
    let rounded = (change * 10.0).round() / 10.0;
    // -0.0 == 0.0, so a change that rounds to zero counts as up; normalize
    // the value too so it formats as "+0.0", never "-0.0".
    let up = rounded >= 0.0;
    let value = if rounded == 0.0 { 0.0 } else { rounded };
    (up, format!("{value:+.1}"))
}

/// Profile page that requires authentication
#[allow(clippy::too_many_lines)]
async fn profile_page(
    State(state): State<AppState>,
    auth::CurrentUser(user): auth::CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let flash = page_factory.flash.clone();
    let show_claim_prompt =
        crate::models::imported_account::should_show_claim_prompt(&state.db, user.user_id)
            .await
            .wrap_err("Failed to check claim prompt state")?;
    let notification_preferences =
        crate::models::notification::preferences(&state.db, user.user_id).await?;
    let email_settings =
        crate::models::notification_email::settings(&state.db, user.user_id).await?;
    let mut email_connection = state.db.acquire().await?;
    let resolved_email =
        crate::models::notification_email::resolved_recipient(&mut email_connection, user.user_id)
            .await?;

    Ok(page_factory.create_page_with_flash(
        "My Profile".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "My Profile" }
                div class="sub" { "How you appear across Battlesnake — and the account behind it." }
            }

            @if show_claim_prompt {
                (claim::claim_prompt())
            }

            header class="profile-head" {
                (user_avatar(user.github_avatar_url.as_deref(), user.public_name(), "avatar"))
                div class="who" {
                    h2 { (user.public_name()) }
                    div class="meta" {
                        "@" (user.github_login)
                        @if !user.pronouns.is_empty() { " · " (user.pronouns) }
                        @if !user.country.is_empty() { " · " (crate::components::flag::country_meta(&user.country)) }
                        " · joined " (user.created_at.format("%b %Y"))
                    }
                    @if !user.backstory.is_empty() {
                        p class="bio" { (user.backstory) }
                    }
                }
            }

            div class="profile-actions" {
                a href={"/users/"(user.github_login)} class="btn" { "View Public Profile" }
                a href="/battlesnakes" class="btn" { "Manage Battlesnakes" }
                a href="/settings/authorized-apps" class="btn" { "Authorized Apps" }
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
                            select id="country" name="country" data-country-select {
                                option value="" selected[user.country.is_empty()] { "None" }
                                @for c in crate::countries::COUNTRIES {
                                    option value=(c.code) selected[c.code == user.country] { (c.name) }
                                }
                            }
                            p class="help" { "Shown as a flag next to your snakes on leaderboards" }
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
                    script src=(crate::arena_assets::asset_url("country-select.js")) {}
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
            section class="block" style="margin: 1rem 0; font-size: 16px" {
                h2 { "Notifications" }
                form action="/settings/notification-email" method="post" style="display: flex; flex-wrap: wrap; gap: 0.75rem; align-items: end; width: 100%; max-width: 100%" {
                    div style="flex: 1 1 240px; min-width: 0" {
                        label for="notification-email" style="display: block; font-size: 16px" { "Notification email" }
                        input type="email" id="notification-email" name="email" autocomplete="email" maxlength="254"
                            value=(email_settings.pending_email.as_deref().or(email_settings.confirmed_email.as_deref()).unwrap_or(""))
                            style="box-sizing: border-box; min-height: 44px; width: 100%; min-width: 0; font-size: 16px";
                    }
                    button type="submit" class="btn solid" style="min-height: 44px; font-size: 16px" { "Save" }
                }
                @if let Some(confirmed) = &email_settings.confirmed_email {
                    p style="overflow-wrap: anywhere" { "Confirmed: " (confirmed) }
                }
                @if let Some(pending) = &email_settings.pending_email {
                    p style="overflow-wrap: anywhere" { (pending) " — pending confirmation" }
                    form action="/settings/notification-email/resend" method="post" {
                        button type="submit" class="btn" style="min-height: 44px; font-size: 16px" { "Resend" }
                    }
                }
                @if email_settings.confirmed_email.is_some() || email_settings.pending_email.is_some() {
                    form action="/settings/notification-email" method="post" {
                        input type="hidden" name="email" value="";
                        button type="submit" class="btn" style="min-height: 44px; font-size: 16px" { "Clear" }
                    }
                }
                @if resolved_email.is_none() { p { "Add an email to get notifications" } }
                @for kind in crate::notifications::NotificationKind::ALL {
                    div style="padding: 0.75rem 0; border-bottom: 1px solid #555" {
                        h3 { (kind.name()) }
                        p { (kind.description()) }
                        @if kind.toggleable() {
                            @for channel in crate::notifications::NotificationChannel::ALL {
                                @if kind.supports(channel) {
                                    @let enabled = notification_preferences.iter()
                                        .find(|(k, c, _)| k == kind.key() && c == channel.key())
                                        .map_or(kind.default_enabled(channel), |(_, _, value)| *value);
                                    form action="/settings/notifications" method="post" style="display: flex; flex-wrap: wrap; align-items: center; gap: 1rem; min-height: 44px" {
                                        input type="hidden" name="kind" value=(kind.key());
                                        input type="hidden" name="channel" value=(channel.key());
                                        input type="hidden" name="enabled" value=(if enabled { "false" } else { "true" });
                                        span { (channel.name()) ": " (if enabled { "On" } else { "Off" }) }
                                        button type="submit" role="switch" aria-checked=(enabled) class="btn" style="min-height: 44px; font-size: 16px" { (if enabled { "Turn off" } else { "Turn on" }) }
                                    }
                                }
                            }
                        } @else {
                            p { "Email: Always on" }
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

    // Validation above guarantees '' or a known alpha-2 code.
    let country = country.to_ascii_uppercase();

    crate::models::user::update_profile_fields(
        &state.db,
        user.user_id,
        display_name,
        pronouns,
        &country,
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
                h1 { "Version Info" }
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

#[cfg(test)]
mod home_delta_tests {
    use super::home_delta;

    #[test]
    fn tiny_negative_never_renders_minus_zero() {
        // -0.04 rounds to zero at display precision: must show "+0.0" with
        // the up styling, not a red "-0.0".
        assert_eq!(home_delta(-0.04), (true, "+0.0".to_string()));
        assert_eq!(home_delta(0.04), (true, "+0.0".to_string()));
        assert_eq!(home_delta(0.0), (true, "+0.0".to_string()));
    }

    #[test]
    fn real_changes_keep_sign_and_class() {
        assert_eq!(home_delta(1.26), (true, "+1.3".to_string()));
        assert_eq!(home_delta(-0.05), (false, "-0.1".to_string()));
        assert_eq!(home_delta(-2.34), (false, "-2.3".to_string()));
    }
}

#[cfg(test)]
mod home_board_tests {
    use super::{HERO_CREAM, HERO_CREAM_BODY, HERO_FOOD, HERO_PINK, HERO_PINK_BODY, home_board};
    use crate::og::board::shape;
    use arena::design_kit::AssetKind;

    #[test]
    fn hero_board_draws_real_heads_and_tails() {
        let out = home_board().into_string();

        // Both snakes' head and tail art, straight from the embedded catalog —
        // the same shapes the default OG card draws.
        for (kind, slug) in [
            (AssetKind::Head, "smile"),
            (AssetKind::Tail, "curled"),
            (AssetKind::Head, "fang"),
            (AssetKind::Tail, "sharp"),
        ] {
            let s = shape(kind, slug).unwrap_or_else(|| panic!("{slug} is in the catalog"));
            assert!(
                out.contains(&format!("d=\"{}\"", s.d)),
                "{kind:?} {slug} art missing from the hero board"
            );
        }

        // Heads point away from the neck, tails away from the body: pink
        // (3,8) heads up over its neck (3,7) and its tail (5,3) points down
        // below (5,4); cream (8,3) heads down from (8,4), tail (9,6) points right of (8,6).
        assert!(out.contains("class=\"head up\""));
        assert!(out.contains("class=\"head down\""));
        assert!(out.contains("class=\"tail down\""));
        assert!(out.contains("class=\"tail right\""));

        // Body squares, food and colours unchanged; still decorative and
        // self-contained (no external art requests).
        assert_eq!(out.matches("class=\"cell seg\"").count(), 11);
        assert_eq!(out.matches("class=\"cell food\"").count(), 4);
        assert!(out.contains("background:#FF3D8A"));
        assert!(out.contains("background:#F4EFEA"));
        assert!(out.contains("fill=\"#FF3D8A\""));
        assert!(out.contains("fill=\"#F4EFEA\""));
        assert!(out.contains("aria-hidden=\"true\""));
        assert!(!out.contains("media.battlesnake.com"));
        assert!(!out.contains("<img"));

        // maud renders a `;`-terminated SVG element as an unclosed start tag;
        // unclosed shapes then nest and collapse to 0×0 in the browser.
        // Guard: exactly four svg/path pairs, all closed.
        assert_eq!(out.matches("<svg").count(), 4);
        assert_eq!(out.matches("</svg>").count(), 4);
        assert_eq!(out.matches("<path").count(), 4);
        assert_eq!(out.matches("</path>").count(), 4);
    }

    #[test]
    fn hero_scene_matches_the_default_og_card() {
        let card = crate::og::cards::home_board();
        let hex = |c: tiny_skia::Color| {
            let c = c.to_color_u8();
            format!("#{:02X}{:02X}{:02X}", c.red(), c.green(), c.blue())
        };
        assert_eq!((card.width, card.height), (11, 11));
        assert_eq!(card.snakes[0].body, HERO_PINK_BODY);
        assert_eq!(card.snakes[1].body, HERO_CREAM_BODY);
        assert_eq!(card.food, HERO_FOOD);
        assert_eq!(hex(card.snakes[0].color), HERO_PINK);
        assert_eq!(hex(card.snakes[1].color), HERO_CREAM);
        assert_eq!(card.snakes[0].head.map(|s| s.file), Some("smile"));
        assert_eq!(card.snakes[0].tail.map(|s| s.file), Some("curled"));
        assert_eq!(card.snakes[1].head.map(|s| s.file), Some("fang"));
        assert_eq!(card.snakes[1].tail.map(|s| s.file), Some("sharp"));
    }
}

#[cfg(test)]
mod asset_tests {
    use axum::response::IntoResponse as _;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn compiled_and_static_assets_are_hashed_immutable_and_exact() {
        let app = cja::assets::router::<()>(&[&crate::arena_assets::MANIFEST])
            .expect("unique asset paths");
        for (logical, mime) in [
            ("theme.js", "text/javascript"),
            ("theme.js.map", "application/json"),
            ("arena.css", "text/css"),
            ("favicon.svg", "image/svg+xml"),
            ("design-kit/example-drawing.png", "image/png"),
            (
                "design-kit/battlesnake-head-template.psd",
                "application/octet-stream",
            ),
        ] {
            let asset = crate::arena_assets::MANIFEST
                .entries
                .iter()
                .find(|asset| asset.logical_name == logical)
                .expect("asset is embedded");
            let response = app
                .clone()
                .oneshot(
                    Request::get(crate::arena_assets::asset_url(logical))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{logical}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime, "{logical}");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "public, max-age=31536000, immutable"
            );
            let bytes = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            assert_eq!(bytes.as_ref(), asset.bytes, "{logical}");
            if matches!(
                logical,
                "arena.css"
                    | "favicon.svg"
                    | "design-kit/example-drawing.png"
                    | "design-kit/battlesnake-head-template.psd"
            ) {
                let source = std::fs::read(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("static")
                        .join(logical),
                )
                .expect("committed static asset");
                assert_eq!(bytes.as_ref(), source, "{logical}");
            }
        }
        for path in ["/assets/missing.js", "/assets/theme.js"] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn stable_favicon_is_svg_with_short_cache() {
        let response = super::serve_favicon().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/svg+xml");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=3600"
        );
        let expected = crate::arena_assets::MANIFEST
            .entries
            .iter()
            .find(|asset| asset.logical_name == "favicon.svg")
            .expect("favicon");
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(bytes.as_ref(), expected.bytes);
    }

    #[test]
    fn missing_favicon_is_404() {
        static EMPTY: cja::assets::Manifest = cja::assets::Manifest::new("test", &[]);
        assert_eq!(
            super::favicon_response(&EMPTY).status(),
            StatusCode::NOT_FOUND
        );
    }
}
