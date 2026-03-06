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
                        // Default aspect-ratio is 16/9; the board sends a RESIZE postMessage with its actual dimensions
                        div #board-viewer-container class="board-viewer-container mb-4" style="width: 100%; max-width: 600px; aspect-ratio: 16 / 9;" {
                            iframe
                                id="board-viewer"
                                src={ "https://board.battlesnake.com/?engine=" (format!("{}/api", std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string()))) "&game=" (game_id) }
                                style="width: 100%; height: 100%; border: 1px solid #ccc; border-radius: 8px;"
                                title="Battlesnake Board Viewer"
                                allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
                                allowfullscreen {}
                        }

                        script {
                            "window.addEventListener('message', function(e) {"
                                "if (e.origin !== 'https://board.battlesnake.com') return;"
                                "var evt = e.data;"
                                "if (evt.event === 'RESIZE') {"
                                    "document.getElementById('board-viewer-container').style"
                                        ".setProperty('aspect-ratio', evt.data.width + ' / ' + evt.data.height);"
                                "}"
                            "});"
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
                                }
                            }
                        }
                    }
                }

                div class="mt-4" {
                    a href="/games/new" class="btn btn-primary" { "Create Another Game" }
                    a href="/me" class="btn btn-secondary ms-2" { "Back to Profile" }
                }
            }
        }),
        flash,
    ))
}
