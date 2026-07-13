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
    components::page_factory::PageFactory,
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
pub mod claim;
pub mod customizations;
pub mod game;
pub mod github_auth;
pub mod leaderboard;
pub mod policy;
pub mod redirects;
pub mod settings;
pub mod tournament;
pub mod users;

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

    let router = axum::Router::new()
        // Public pages
        .route("/", get(root_page))
        .route("/robots.txt", get(robots_txt))
        // Policy pages
        .route("/conduct", get(policy::conduct_page))
        .route("/privacy", get(policy::privacy_page))
        .route("/terms", get(policy::terms_page))
        // Public user profiles
        .route("/users/{login}", get(users::show_user_profile))
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
        // Unknown routes get a branded 404 instead of an empty response
        .fallback(not_found_page);

    // Community short links (/docs, /discord, ...) carried over from play
    redirects::register(router)
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            inject_trace_context,
        ))
        .with_state(app_state)
}

async fn not_found_page(page_factory: PageFactory) -> impl IntoResponse {
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
    // The home page features the first active leaderboard: its recent games
    // feed the ticker + rail, and its top five make the ladder preview.
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
                        img src=(user.github_avatar_url.clone().unwrap_or_default()) alt="" width="64" height="64";
                        div class="who" {
                            h1 { "Welcome, " (user.github_login) "!" }
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
                            h1 { "Your code is the " em { "controller." } }
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

                @if let (Some(lb), false) = (featured, activity.is_empty()) {
                    div class="strip" aria-hidden="true" {
                        div class="inner" {
                            (home_ticker_items(activity, &lb.name))
                            (home_ticker_items(activity, &lb.name))
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
                        a class="btn solid" href="/auth/github" { "Get started free" }
                    }
                }
            }
        }),
    ))
}

/// Decorative 11×11 board for the logged-out hero. Intentionally dark-framed
/// in both themes (hardcoded colors match the mockup's board panel).
fn home_board() -> Markup {
    const N: usize = 11;
    let pink: &[(usize, usize)] = &[
        (3, 2),
        (3, 3),
        (4, 3),
        (4, 4),
        (4, 5),
        (5, 5),
        (5, 6),
        (5, 7),
    ];
    let cream: &[(usize, usize)] = &[(8, 7), (8, 6), (7, 6), (7, 5), (7, 4), (8, 4), (9, 4)];
    let food: &[(usize, usize)] = &[(1, 1), (6, 2), (9, 9), (2, 8)];

    let mut cells: Vec<(&'static str, Option<String>)> = vec![("cell", None); N * N];
    let mut draw_snake = |coords: &[(usize, usize)], color: &str| {
        for (i, &(x, y)) in coords.iter().enumerate() {
            let class = if i == 0 { "cell seg head" } else { "cell seg" };
            let mut style = format!("background:{color}");
            if i == coords.len() - 1 {
                style.push_str(";opacity:.55");
            }
            cells[y * N + x] = (class, Some(style));
        }
    };
    draw_snake(pink, "#FF3D8A");
    draw_snake(cream, "#F4EFEA");
    for &(x, y) in food {
        cells[y * N + x] = ("cell food", None);
    }

    html! {
        div class="board" aria-hidden="true" {
            @for (class, style) in &cells {
                div class=(class) style=[style.as_deref()] {}
            }
        }
    }
}

/// One pass of ticker copy; rendered twice so the -50% loop is seamless.
fn home_ticker_items(activity: &[ActivityFeedEntry], leaderboard_name: &str) -> Markup {
    html! {
        @for event in activity {
            b { (event.snake_name) }
            @if event.placement == 1 {
                " won on "
            } @else {
                " placed " (home_ordinal(event.placement)) " on "
            }
            (leaderboard_name)
            " "
            @if event.display_score_change >= 0.0 {
                span class="win" { (format!("{:+.1}", event.display_score_change)) }
            } @else {
                span class="lose" { (format!("{:+.1}", event.display_score_change)) }
            }
            span class="sep" { "/" }
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
                                                span class="owner" { "by " (entry.owner_login) }
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
                                        @if event.display_score_change >= 0.0 {
                                            span class="delta up" { (format!("{:+.1}", event.display_score_change)) }
                                        } @else {
                                            span class="delta down" { (format!("{:+.1}", event.display_score_change)) }
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
                a href={"/users/"(user.github_login)} class="btn" { "View Public Profile" }
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
