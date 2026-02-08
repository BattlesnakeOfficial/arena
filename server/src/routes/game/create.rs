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
    components::flash::Flash,
    components::page_factory::PageFactory,
    errors::{ServerResult, WithStatus},
    models::flow::GameCreationFlow,
    models::game::{GameBoardSize, GameType},
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

// Game create form - show the game creation form with the flow state
#[debug_handler]
pub async fn show_game_flow(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(flow_id): Path<Uuid>,
    page_factory: PageFactory,
    flash: Flash,
) -> ServerResult<impl IntoResponse, StatusCode> {
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

    // Render the game creation form
    Ok(page_factory.create_page_with_flash(
        "Create New Game".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Create New Game" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                form action={"/games/flow/"(flow_id)"/create"} method="post" class="mb-4" {
                    div class="form-group mb-3" {
                        label for="board_size" { "Board Size" }
                        select id="board_size" name="board_size" class="form-control" required {
                            option value="7x7" selected[flow.board_size == GameBoardSize::Small] { "Small (7x7)" }
                            option value="11x11" selected[flow.board_size == GameBoardSize::Medium] { "Medium (11x11)" }
                            option value="19x19" selected[flow.board_size == GameBoardSize::Large] { "Large (19x19)" }
                        }
                    }

                    div class="form-group mb-3" {
                        label for="game_type" { "Game Type" }
                        select id="game_type" name="game_type" class="form-control" required {
                            option value="Standard" selected[flow.game_type == GameType::Standard] { "Standard" }
                            option value="Royale" selected[flow.game_type == GameType::Royale] { "Royale" }
                            option value="Constrictor" selected[flow.game_type == GameType::Constrictor] { "Constrictor" }
                            option value="Snail Mode" selected[flow.game_type == GameType::SnailMode] { "Snail Mode" }
                        }
                    }

                    // Display current selection count if any
                    @if flow.selected_count() > 0 {
                        div class="alert alert-info mb-3" {
                            p { "You have selected " (flow.selected_count()) " of 4 possible battlesnakes." }

                            // Display the selected battlesnakes with their counts
                            @if !selected_battlesnakes.is_empty() {
                                div class="mt-2" {
                                    p class="mb-1 fw-bold" { "Selected Battlesnakes:" }
                                    ul class="list-group" {
                                        @for snake in &selected_battlesnakes {
                                            @let count = flow.battlesnake_count(&snake.battlesnake_id);
                                            li class="list-group-item d-flex justify-content-between align-items-center" {
                                                span {
                                                    (snake.name)
                                                    @if count > 1 {
                                                        " "
                                                        span class="badge bg-secondary" { "×" (count) }
                                                    }
                                                }
                                                form action={"/games/flow/"(flow_id)"/remove-snake/"(snake.battlesnake_id)} method="post" class="d-inline" {
                                                    button type="submit" class="btn btn-sm btn-danger" { "Remove" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            div class="mt-3" {
                                button type="submit" class="btn btn-success me-2" { "Create Game" }

                                form action={"/games/flow/"(flow_id)"/reset"} method="post" class="d-inline" {
                                    button type="submit" class="btn btn-secondary" { "Reset Selection" }
                                }
                            }
                        }
                    } @else {
                        div class="alert alert-warning mb-3" {
                            p { "Please select at least one battlesnake to create a game." }
                        }
                    }
                }

                h2 class="mt-4" { "Your Battlesnakes" }

                @if user_battlesnakes.is_empty() {
                    div class="alert alert-warning" {
                        p { "You don't have any battlesnakes yet." }
                        a href="/battlesnakes/new" class="btn btn-primary" { "Create a Battlesnake" }
                    }
                } @else {
                    div class="row row-cols-1 row-cols-md-3 g-4 mb-4" {
                        @for snake in &user_battlesnakes {
                            @let count = flow.battlesnake_count(&snake.battlesnake_id);
                            @let can_add = flow.selected_count() < 4;
                            div class="col" {
                                div class=(format!("card h-100 {}", if count > 0 { "border-primary" } else { "" })) {
                                    div class="card-body" {
                                        h5 class="card-title" {
                                            (snake.name)
                                            @if count > 0 {
                                                " "
                                                span class="badge bg-primary" { "×" (count) }
                                            }
                                        }
                                        p class="card-text" {
                                            a href=(snake.url) target="_blank" { (snake.url) }
                                        }
                                    }
                                    div class="card-footer d-flex gap-2" {
                                        // Always show Add button if under 4 total snakes
                                        @if can_add {
                                            form action={"/games/flow/"(flow_id)"/add-snake/"(snake.battlesnake_id)} method="post" class="flex-grow-1" {
                                                button type="submit" class="btn btn-primary w-100" { "Add to Game" }
                                            }
                                        }
                                        // Show Remove button if this snake is selected
                                        @if count > 0 {
                                            form action={"/games/flow/"(flow_id)"/remove-snake/"(snake.battlesnake_id)} method="post" class="flex-grow-1" {
                                                button type="submit" class="btn btn-danger w-100" { "Remove" }
                                            }
                                        }
                                        // If can't add and not selected, show disabled state
                                        @if !can_add && count == 0 {
                                            button type="button" class="btn btn-secondary w-100" disabled { "Max reached" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                h2 class="mt-4" { "Search for Public Battlesnakes" }

                form action={"/games/flow/"(flow_id)"/search"} method="get" class="mb-3" {
                    div class="input-group" {
                        input type="text" name="q" class="form-control" placeholder="Search by name..." value=(flow.search_query.as_deref().unwrap_or("")) {}
                        button type="submit" class="btn btn-outline-secondary" { "Search" }
                    }
                }

                // If we have search results from other users, show them
                @if let Some(query) = &flow.search_query {
                    @if !query.is_empty() {
                        (render_search_results(&flow, &state.db).await)
                    }
                }

                div class="mt-4" {
                    a href="/me" class="btn btn-secondary" { "Back to Profile" }
                }
            }
        }),
        flash,
    ))
}

// Configure the game (board size and game type)
#[derive(Debug, Deserialize)]
pub struct ConfigureGameForm {
    // Optional parameters since they might not be provided in the form
    pub board_size: String,
    pub game_type: String,
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

    let can_add = flow.selected_count() < 4;

    html! {
        @if search_results.is_empty() {
            div class="alert alert-info" {
                "No public battlesnakes found matching your search."
            }
        } @else {
            h3 { "Search Results" }
            div class="row row-cols-1 row-cols-md-3 g-4" {
                @for snake in &search_results {
                    @let count = flow.battlesnake_count(&snake.battlesnake_id);
                    div class="col" {
                        div class=(format!("card h-100 {}", if count > 0 { "border-primary" } else { "" })) {
                            div class="card-body" {
                                h5 class="card-title" {
                                    (snake.name)
                                    @if count > 0 {
                                        " "
                                        span class="badge bg-primary" { "×" (count) }
                                    }
                                }
                                p class="card-text" {
                                    a href=(snake.url) target="_blank" { (snake.url) }
                                }
                            }
                            div class="card-footer d-flex gap-2" {
                                // Always show Add button if under 4 total snakes
                                @if can_add {
                                    form action={"/games/flow/"(flow.flow_id)"/add-snake/"(snake.battlesnake_id)} method="post" class="flex-grow-1" {
                                        button type="submit" class="btn btn-primary w-100" { "Add to Game" }
                                    }
                                }
                                // Show Remove button if this snake is selected
                                @if count > 0 {
                                    form action={"/games/flow/"(flow.flow_id)"/remove-snake/"(snake.battlesnake_id)} method="post" class="flex-grow-1" {
                                        button type="submit" class="btn btn-danger w-100" { "Remove" }
                                    }
                                }
                                // If can't add and not selected, show disabled state
                                @if !can_add && count == 0 {
                                    button type="button" class="btn btn-secondary w-100" disabled { "Max reached" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
