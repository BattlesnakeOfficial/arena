use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use axum_macros::debug_handler;
use color_eyre::eyre::Context as _;
use maud::html;
use uuid::Uuid;

use crate::{
    components::flash::Flash,
    components::page_factory::PageFactory,
    errors::{ServerResult, WithStatus},
    models::game::GameStatus,
    models::game_battlesnake,
    routes::auth::CurrentUser,
    state::AppState,
};

// Display game details
#[debug_handler]
pub async fn view_game(
    State(state): State<AppState>,
    CurrentUser(_): CurrentUser,
    Path(game_id): Path<Uuid>,
    page_factory: PageFactory,
    flash: Flash,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the game with its battlesnakes
    let (game, battlesnakes) = game_battlesnake::get_game_with_battlesnakes(&state.db, game_id)
        .await
        .wrap_err("Failed to get game details")
        .with_status(StatusCode::NOT_FOUND)?;

    // Render the game details page
    Ok(page_factory.create_page_with_flash(
        format!("Game Details: {}", game_id),
        Box::new(html! {
            div class="container" {
                h1 { "Game Details" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                div class="card mb-4" {
                    div class="card-header d-flex justify-content-between align-items-center" {
                        h2 class="mb-0" { "Game " (game_id) }
                        @match game.status {
                            GameStatus::Waiting => span class="badge bg-secondary" { "Waiting" },
                            GameStatus::Running => span class="badge bg-primary" { "Running..." },
                            GameStatus::Finished => span class="badge bg-success" { "Finished" },
                        }
                    }
                    div class="card-body" {
                        // Board viewer iframe - always show, it handles waiting/empty games gracefully
                        div class="board-viewer-container mb-4" style="width: 100%; max-width: 600px;" {
                            iframe
                                id="board-viewer"
                                src={ "https://board.battlesnake.com/?engine=" (format!("{}/api", std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string()))) "&game=" (game_id) }
                                style="width: 100%; border: 1px solid #ccc; border-radius: 8px;"
                                title="Battlesnake Board Viewer"
                                allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
                                allowfullscreen {}
                        }

                        div class="game-info" {
                            p { "Board Size: " (game.board_size.as_str()) }
                            p { "Game Type: " (game.game_type.as_str()) }
                            p { "Status: " (game.status.as_str()) }
                            p { "Created: " (game.created_at.format("%Y-%m-%d %H:%M:%S")) }
                        }
                    }
                }

                @if game.status == GameStatus::Waiting {
                    div class="alert alert-info mb-4" {
                        p class="mb-0" {
                            "This game is waiting to start. "
                            a href="" onclick="location.reload(); return false;" { "Refresh" }
                            " to check for updates."
                        }
                    }
                }

                h3 { "Game Results" }

                div class="table-responsive" {
                    table class="table table-striped" {
                        thead {
                            tr {
                                th { "Place" }
                                th { "Snake Name" }
                                th { "Owner" }
                                th { "URL" }
                            }
                        }
                        tbody {
                            @for battlesnake in battlesnakes {
                                tr {
                                    td {
                                        @if let Some(placement) = battlesnake.placement {
                                            @match placement {
                                                1 => span class="badge bg-warning text-dark" { "🥇 1st Place" },
                                                2 => span class="badge bg-secondary text-white" { "🥈 2nd Place" },
                                                3 => span class="badge bg-danger text-white" { "🥉 3rd Place" },
                                                _ => span class="badge bg-dark text-white" { (placement) "th Place" },
                                            }
                                        } @else {
                                            span class="badge bg-info text-dark" { "In Progress" }
                                        }
                                    }
                                    td { (battlesnake.name) }
                                    td { "User " (battlesnake.user_id) }
                                    td {
                                        a href=(battlesnake.url) target="_blank" { (battlesnake.url) }
                                    }
                                }
                            }
                        }
                    }
                }

                div class="mt-4" {
                    a href="/games" class="btn btn-primary" { "All Games" }
                    a href="/games/new" class="btn btn-secondary ms-2" { "Create Another Game" }
                    a href="/me" class="btn btn-secondary ms-2" { "Back to Profile" }
                }
            }
        }),
        flash,
    ))
}

// List all games
#[debug_handler]
pub async fn list_games(
    State(state): State<AppState>,
    CurrentUser(_): CurrentUser,
    page_factory: PageFactory,
    flash: Flash,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get all games with winners
    let games_with_winners = crate::models::game::get_all_games_with_winners(&state.db)
        .await
        .wrap_err("Failed to get games list with winners")?;

    // Render the games list page
    Ok(page_factory.create_page_with_flash(
        "All Games".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "All Games" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                @if games_with_winners.is_empty() {
                    div class="alert alert-info" {
                        p { "No games have been created yet." }
                    }
                } @else {
                    div class="table-responsive" {
                        table class="table table-striped" {
                            thead {
                                tr {
                                    th { "Game ID" }
                                    th { "Board Size" }
                                    th { "Game Type" }
                                    th { "Winner" }
                                    th { "Status" }
                                    th { "Created" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                @for (game, winner) in &games_with_winners {
                                    tr {
                                        td { (game.game_id) }
                                        td { (game.board_size.as_str()) }
                                        td { (game.game_type.as_str()) }
                                        td {
                                            @if let Some(winner_name) = winner {
                                                span class="badge bg-warning text-dark" { "🏆 " (winner_name) }
                                            } @else {
                                                @if game.status == crate::models::game::GameStatus::Finished {
                                                    span class="badge bg-secondary text-white" { "No Winner" }
                                                } @else {
                                                    span class="badge bg-info text-dark" { "In Progress" }
                                                }
                                            }
                                        }
                                        td { (game.status.as_str()) }
                                        td { (game.created_at.format("%Y-%m-%d %H:%M:%S")) }
                                        td {
                                            a href={"/games/"(game.game_id)} class="btn btn-sm btn-primary" { "View" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div class="mt-4" {
                    a href="/games/new" class="btn btn-primary" { "Create New Game" }
                    a href="/me" class="btn btn-secondary" { "Back to Profile" }
                }
            }
        }),
        flash,
    ))
}
