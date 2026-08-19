use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use axum_macros::debug_handler;
use color_eyre::eyre::Context as _;
use maud::html;
use serde::Deserialize;
use std::str::FromStr;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::battlesnake::{self, Visibility},
    models::flow::GameCreationFlow,
    models::game::{self, GameBoardSize, GameType},
    models::game_battlesnake,
    models::rate_limit,
    models::session,
    routes::auth::{CurrentUser, CurrentUserWithSession},
    state::AppState,
};

// Initial game creation page - redirect to a new flow
#[debug_handler]
pub async fn new_game(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Create a new flow for this user
    let flow = GameCreationFlow::create_for_user(&state.db, user.user_id)
        .await
        .wrap_err("Failed to create game flow")?;

    // Redirect to the flow page
    Ok(Redirect::to(&format!("/games/flow/{}", flow.flow_id)).into_response())
}

// Rematch - create a new flow pre-filled from an existing game's snakes and
// settings, then send the user through the normal builder for confirmation
// (which reuses the flow's validation and the create-time rate limits).
#[debug_handler]
pub async fn rematch_game(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(game_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the source game for its board size and game type
    let game = game::get_game_by_id(&state.db, game_id)
        .await
        .wrap_err("Failed to get game")?
        .ok_or_else(|| "Game not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Get the source game's participants. The query joins against the
    // battlesnakes table, so snakes deleted since the game ran are skipped
    // gracefully; duplicates of the same snake are preserved as separate rows.
    let battlesnakes = game_battlesnake::get_battlesnakes_by_game_id(&state.db, game_id)
        .await
        .wrap_err("Failed to get game battlesnakes")?;

    // Create a new flow for this user and pre-fill it
    let mut flow = GameCreationFlow::create_for_user(&state.db, user.user_id)
        .await
        .wrap_err("Failed to create game flow")?;

    flow.board_size = game.board_size;
    flow.game_type = game.game_type;
    for battlesnake in &battlesnakes {
        // add_battlesnake enforces the 4-snake cap, matching the flow's rules
        flow.add_battlesnake(battlesnake.battlesnake_id);
    }

    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    // Redirect to the flow page so the user confirms through the builder
    Ok(Redirect::to(&format!("/games/flow/{}", flow.flow_id)).into_response())
}

/// POST /battlesnakes/{id}/challenge — start a flow with this public snake selected.
///
/// Visibility is re-checked here: the listing that surfaced this snake is not
/// an authorization boundary (a snake can go private between the page load and
/// the POST, and the request can be hand-crafted).
#[debug_handler]
pub async fn challenge_battlesnake(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(battlesnake_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let snake = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake")?;

    // Missing and non-public collapse to the same 404 — don't leak which.
    let snake = snake
        .filter(|snake| snake.visibility == Visibility::Public)
        .ok_or_else(|| "Public battlesnake not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let flow =
        GameCreationFlow::create_for_challenge(&state.db, user.user_id, snake.battlesnake_id)
            .await
            .wrap_err("Failed to create challenge game flow")?;

    // Send the user through the normal builder to pick settings and confirm
    Ok(Redirect::to(&format!("/games/flow/{}", flow.flow_id)).into_response())
}

// Game create form - show the game creation form with the flow state
#[debug_handler]
pub async fn show_game_flow(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(flow_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Use flash from page_factory (already extracted and cleared from DB;
    // a separate Flash extractor arg would read an already-cleared flash)
    let flash = page_factory.flash.clone();

    // Get the flow state, ensuring it belongs to the current user
    let flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Get user's battlesnakes
    let user_battlesnakes = flow
        .get_user_battlesnakes(&state.db)
        .await
        .wrap_err("Failed to get user's battlesnakes")?;

    // Get the selected battlesnakes
    let selected_battlesnakes = flow
        .get_selected_battlesnakes(&state.db)
        .await
        .wrap_err("Failed to get selected battlesnakes")?;

    let selected_count = flow.selected_count();

    // Render the game creation form
    Ok(page_factory.create_page_with_flash(
        "Create New Game".to_string(),
        Box::new(html! {
            div class="crumb" { a href="/me" { "My Profile" } " / new game" }
            div class="page-head" {
                h1 { "Create New Game" }
                div class="sub" { "Pick up to four battlesnakes, choose the board and rules, then send them in." }
            }

            div class="grid gc" {
                div {
                    div class="section gc-section" {
                        h2 { "Your Battlesnakes" }

                        @if user_battlesnakes.is_empty() {
                            div class="gc-empty" {
                                p { "You don't have any battlesnakes yet." }
                                p class="gc-sub" { "Point the Arena at your snake server first — then it can play." }
                                a href="/battlesnakes/new" class="btn solid" { "Create a Battlesnake" }
                            }
                        } @else {
                            div class="gc-rows" {
                                @for snake in &user_battlesnakes {
                                    (snake_row(&flow, snake))
                                }
                            }
                        }
                    }

                    div class="section gc-section" {
                        h2 { "Public Battlesnakes" }
                        p class="gc-sub" { "Search the community's public snakes to fill out the board." }

                        form action={"/games/flow/"(flow_id)"/search"} method="get" class="gc-search" {
                            input type="search" name="q" placeholder="Search by name..." aria-label="Search public battlesnakes" value=(flow.search_query.as_deref().unwrap_or(""));
                            button type="submit" class="btn" { "Search" }
                        }

                        // If we have search results from other users, show them
                        @if let Some(query) = &flow.search_query {
                            @if !query.is_empty() {
                                (render_search_results(&flow, &state.db).await)
                            }
                        }
                    }
                }

                aside class="rail" {
                    div class="block" {
                        h3 { "Lineup" }
                        div class="gc-slots" {
                            @for snake in &selected_battlesnakes {
                                @let count = flow.battlesnake_count(&snake.battlesnake_id);
                                div class="gc-slot" {
                                    span class="chip" style={"background:" (chip_color(&snake.color))} {}
                                    span class="gc-slot-name" { (snake.name) }
                                    @if count > 1 {
                                        span class="badge" { "×" (count) }
                                    }
                                    form action={"/games/flow/"(flow_id)"/remove-snake/"(snake.battlesnake_id)} method="post" {
                                        button type="submit" class="gc-x" aria-label={"Remove " (snake.name) " from lineup"} title="Remove from lineup" { "✕" }
                                    }
                                }
                            }
                            @for _ in selected_count..4 {
                                div class="gc-slot empty" { "Open slot" }
                            }
                        }
                        @if selected_count > 0 {
                            p class="gc-hint" { "You have selected " (selected_count) " of 4 possible battlesnakes." }
                            @if selected_count == 1 && flow.game_type != GameType::Solo {
                                p class="gc-hint gc-solo-warn" {
                                    "A lone snake wins the moment the game starts — add an "
                                    "opponent below, or switch the game type to Solo for a "
                                    "survival run."
                                }
                            }
                            form action={"/games/flow/"(flow_id)"/reset"} method="post" class="gc-reset" {
                                button type="submit" class="btn sm" { "Reset Selection" }
                            }
                        } @else {
                            p class="gc-hint" { "Please select at least one battlesnake to create a game." }
                        }
                    }

                    div class="block" {
                        h3 { "Game Settings" }
                        form id="game-settings" action={"/games/flow/"(flow_id)"/create"} method="post"
                            class="form-stack gc-settings" data-configure-url={"/games/flow/"(flow_id)"/configure"} {
                            div class="field" {
                                label for="board_size" { "Board Size" }
                                select id="board_size" name="board_size" required {
                                    option value="7x7" selected[flow.board_size == GameBoardSize::Small] { "Small (7x7)" }
                                    option value="11x11" selected[flow.board_size == GameBoardSize::Medium] { "Medium (11x11)" }
                                    option value="19x19" selected[flow.board_size == GameBoardSize::Large] { "Large (19x19)" }
                                }
                            }
                            div class="field" {
                                label for="game_type" { "Game Type" }
                                select id="game_type" name="game_type" required {
                                    option value="Standard" selected[flow.game_type == GameType::Standard] { "Standard" }
                                    option value="Royale" selected[flow.game_type == GameType::Royale] { "Royale" }
                                    option value="Constrictor" selected[flow.game_type == GameType::Constrictor] { "Constrictor" }
                                    option value="Snail Mode" selected[flow.game_type == GameType::SnailMode] { "Snail Mode" }
                                    option value="Solo" selected[flow.game_type == GameType::Solo] { "Solo" }
                                }
                            }
                            @if selected_count > 0 {
                                button type="submit" class="btn solid" { "Create Game" }
                            }
                        }
                    }
                }
            }

            // Persist settings changes immediately so they survive the
            // add/remove/search page reloads (no-JS fallback: the create
            // form still posts both fields).
            script {
                (maud::PreEscaped(r#"
                (function () {
                  var form = document.getElementById('game-settings');
                  if (!form) return;
                  form.querySelectorAll('select').forEach(function (el) {
                    el.addEventListener('change', function () {
                      fetch(form.dataset.configureUrl, {
                        method: 'POST',
                        body: new URLSearchParams(new FormData(form)),
                        keepalive: true,
                      });
                    });
                  });
                })();
                "#))
            }
        }),
        flash,
    ))
}

/// One selectable snake row — shared by "Your Battlesnakes" and search
/// results. The `card` class is load-bearing: e2e specs locate rows by it.
fn snake_row(flow: &GameCreationFlow, snake: &battlesnake::Battlesnake) -> maud::Markup {
    let count = flow.battlesnake_count(&snake.battlesnake_id);
    let can_add = flow.selected_count() < 4;
    html! {
        div class={"card gc-row" @if count > 0 { " sel" }} {
            span class="chip" style={"background:" (chip_color(&snake.color))} {}
            div class="gc-who" {
                span class="gc-name" {
                    (snake.name)
                    @if count > 0 {
                        span class="badge live" { "In lineup" @if count > 1 { " ×" (count) } }
                    }
                }
                span class="gc-url" { (snake.url) }
            }
            div class="gc-actions" {
                @if can_add {
                    form action={"/games/flow/"(flow.flow_id)"/add-snake/"(snake.battlesnake_id)} method="post" {
                        button type="submit" class="btn sm" { "Add to Game" }
                    }
                }
                @if count > 0 {
                    form action={"/games/flow/"(flow.flow_id)"/remove-snake/"(snake.battlesnake_id)} method="post" {
                        button type="submit" class="btn sm danger" { "Remove" }
                    }
                }
                @if !can_add && count == 0 {
                    button type="button" class="btn sm" disabled { "Max reached" }
                }
            }
        }
    }
}

// Configure the game (board size and game type)
#[derive(Debug, Deserialize)]
pub struct ConfigureGameForm {
    // Optional parameters since they might not be provided in the form
    pub board_size: String,
    pub game_type: String,
}

// Persist settings changes without creating the game. Called by the
// settings form's change listener so board size / game type survive the
// full-page reloads that add/remove/search cause.
#[debug_handler]
pub async fn configure_game(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(flow_id): Path<Uuid>,
    Form(data): Form<ConfigureGameForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let mut flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if let Ok(board_size) = GameBoardSize::from_str(&data.board_size) {
        flow.board_size = board_size;
    }

    if let Ok(game_type) = GameType::from_str(&data.game_type) {
        flow.game_type = game_type;
    }

    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response())
}

// Reset the snake selections in the flow
#[debug_handler]
pub async fn reset_snake_selections(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(flow_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the flow
    let mut flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Clear the selections
    flow.selected_battlesnake_ids.clear();

    // Update the flow
    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    // Redirect back to the flow page
    Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response())
}

// Add a battlesnake to the selection
#[debug_handler]
pub async fn add_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path((flow_id, battlesnake_id)): Path<(Uuid, Uuid)>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the flow
    let mut flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Add the battlesnake
    let added = flow.add_battlesnake(battlesnake_id);

    // Set appropriate flash message if the add fails
    if !added && flow.selected_count() >= 4 {
        // Set an error flash message in the session
        session::set_flash_message(
            &state.db,
            session.session_id,
            "Maximum of 4 battlesnakes allowed".to_string(),
            session::FLASH_TYPE_WARNING,
        )
        .await
        .wrap_err("Failed to set flash message")?;
    }

    // Update the flow
    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    // Redirect back to the flow page
    Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response())
}

// Remove a battlesnake from the selection
#[debug_handler]
pub async fn remove_battlesnake(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((flow_id, battlesnake_id)): Path<(Uuid, Uuid)>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the flow
    let mut flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Remove the battlesnake
    flow.remove_battlesnake(battlesnake_id);

    // Update the flow
    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    // Redirect back to the flow page
    Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response())
}

// Search for public battlesnakes
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
}

#[debug_handler]
pub async fn search_battlesnakes(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(flow_id): Path<Uuid>,
    Query(query): Query<SearchQuery>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the flow
    let mut flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Update search query
    flow.search_query = query.q;

    // Update the flow
    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    // Redirect back to the flow page
    Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response())
}

// Create the game with selected snakes
#[debug_handler]
pub async fn create_game(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(flow_id): Path<Uuid>,
    Form(data): Form<ConfigureGameForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Rate limit game creation per account (shared with the API). The
    // attempt is recorded before the check so concurrent requests see each
    // other, and the returned count includes this attempt — reject when it
    // exceeds the limit. The flow is left intact so nothing is lost.
    let limit = state.config.game_creation_rate_limit;
    let window_minutes = state.config.game_creation_rate_limit_window_minutes;
    let attempts = rate_limit::record_and_count_game_creation_attempts(
        &state.db,
        user.user_id,
        "web",
        window_minutes,
    )
    .await
    .wrap_err("Failed to record game creation attempt")?;
    if attempts > limit {
        tracing::warn!(
            event_type = "game_creation_rate_limited",
            user_id = %user.user_id,
            attempts = attempts,
            limit = limit,
            source = "web",
            "game creation rate limited"
        );

        // Set an error flash message in the session
        session::set_flash_message(
            &state.db,
            session.session_id,
            format!(
                "You're creating games too fast — max {limit} games per {window_minutes} minutes."
            ),
            session::FLASH_TYPE_ERROR,
        )
        .await
        .wrap_err("Failed to set flash message")?;

        // Redirect back to the flow page
        return Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response());
    }

    // Get the flow
    let mut flow = GameCreationFlow::get_by_id(&state.db, flow_id, user.user_id)
        .await
        .wrap_err("Failed to get game flow")?
        .ok_or_else(|| "Game flow not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Update with user's selections if provided
    if let Ok(board_size) = GameBoardSize::from_str(&data.board_size) {
        flow.board_size = board_size;
    }

    if let Ok(game_type) = GameType::from_str(&data.game_type) {
        flow.game_type = game_type;
    }

    // Update the flow with settings changes
    flow.update(&state.db)
        .await
        .wrap_err("Failed to update game flow")?;

    // Validate and create the game
    let validate_result = flow.validate();
    match validate_result {
        Ok(_) => {
            // Create the game and enqueue a job to run it
            let game_id = flow
                .create_game_and_enqueue(state.clone())
                .await
                .wrap_err("Failed to create game")?;

            tracing::info!(
                event_type = "game_created",
                game_id = %game_id,
                board_size = flow.board_size.as_str(),
                game_type = flow.game_type.as_str(),
                source = "web_ui",
                "game created via web UI"
            );

            // Delete the flow
            GameCreationFlow::delete(&state.db, flow_id, user.user_id)
                .await
                .wrap_err("Failed to delete game flow")?;

            // Set a success flash message in the session
            session::set_flash_message(
                &state.db,
                session.session_id,
                "Game created and queued for execution!".to_string(),
                session::FLASH_TYPE_SUCCESS,
            )
            .await
            .wrap_err("Failed to set flash message")?;

            // Redirect to the game details page
            Ok(Redirect::to(&format!("/games/{}", game_id)).into_response())
        }
        Err(error) => {
            // Set an error flash message in the session
            session::set_flash_message(
                &state.db,
                session.session_id,
                error.to_string(),
                session::FLASH_TYPE_ERROR,
            )
            .await
            .wrap_err("Failed to set flash message")?;

            // Redirect back to the flow page
            Ok(Redirect::to(&format!("/games/flow/{}", flow_id)).into_response())
        }
    }
}

// Helper function to render search results
async fn render_search_results(flow: &GameCreationFlow, db: &sqlx::PgPool) -> maud::Markup {
    // Execute the search
    let search_results = flow
        .search_public_battlesnakes(db)
        .await
        .unwrap_or_default();

    html! {
        @if search_results.is_empty() {
            p class="gc-none" { "No public battlesnakes found matching your search." }
        } @else {
            h3 class="gc-results-head" { "Search Results" }
            div class="gc-rows" {
                @for snake in &search_results {
                    (snake_row(flow, snake))
                }
            }
        }
    }
}
