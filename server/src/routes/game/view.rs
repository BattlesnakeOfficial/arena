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
    components::page_factory::PageFactory,
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::game::GameStatus,
    models::game_battlesnake,
    routes::auth::OptionalUser,
    state::AppState,
};

// Display game details in the game theater (themed by the theater axis)
#[debug_handler]
pub async fn view_game(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    Path(game_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the game with its battlesnakes
    let (game, battlesnakes) = game_battlesnake::get_game_with_battlesnakes(&state.db, game_id)
        .await
        .wrap_err("Failed to get game details")
        .with_status(StatusCode::NOT_FOUND)?;

    let finished = game.status == GameStatus::Finished;

    Ok(page_factory.create_theater_page(
        format!("Game {game_id}"),
        Box::new(html! {
            h1 class="vh" { "Game Details" }
            div class="crumb" {
                a href="/leaderboards" { "Leaderboards" }
                " / " span { "Game " (game_id) }
                @match game.status {
                    GameStatus::Waiting => span class="live-pill quiet" { "Waiting" },
                    GameStatus::Running => span class="live-pill" { span class="live-dot" {} "Live" },
                    GameStatus::Finished => span class="live-pill quiet" { "Replay" },
                }
            }

            @if game.status == GameStatus::Waiting {
                p class="empty" {
                    "This game is waiting to start. "
                    a href="" onclick="location.reload(); return false;" class="refresh-link" { "Refresh" }
                    " to check for updates."
                }
            }

            div class="theater" {
                div {
                    div class="board-wrap" {
                        // Board viewer iframe - always show, it handles waiting/empty games
                        // gracefully. Default aspect-ratio is 16/9; the board sends a RESIZE
                        // postMessage with its actual dimensions.
                        div #board-viewer-container style="width: 100%; aspect-ratio: 16 / 9;" {
                            iframe
                                id="board-viewer"
                                src={ "https://board.battlesnake.com/?engine=" (format!("{}/api", state.config.base_url)) "&game=" (game_id) }
                                title="Battlesnake Board Viewer"
                                allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
                                allowfullscreen {}
                        }
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

                    div class="theater-actions" {
                        @if user.is_some() {
                            @if finished {
                                form action={"/games/"(game_id)"/rematch"} method="post" style="display: inline;" {
                                    button type="submit" class="btn" { "Rematch" }
                                }
                            }
                            a href="/games/new" class="btn" { "Create Another Game" }
                            a href="/me" class="btn" { "Back to Profile" }
                        } @else {
                            a href="/leaderboards" class="btn" { "View Leaderboards" }
                            a href="/" class="btn" { "Back to Home" }
                        }
                    }
                }

                aside {
                    h2 class="theater-rail-head" {
                        "Game Results"
                        span class="sub-count" { (battlesnakes.len()) " snakes" }
                    }
                    div class="snakes" {
                        @for battlesnake in &battlesnakes {
                            div .scard .p1[battlesnake.placement == Some(1)] {
                                div class="top" {
                                    span class="chip" style={"background:"(chip_color(&battlesnake.color))} {}
                                    div {
                                        div class="name" { (battlesnake.name) }
                                        div class="owner" { "by " (battlesnake.owner_login) }
                                    }
                                    div class="place" {
                                        @if let Some(placement) = battlesnake.placement {
                                            (ordinal_place(placement))
                                        } @else if finished {
                                            "—"
                                        } @else {
                                            "In Progress"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div class="gmeta" {
                        h3 { "Details" }
                        dl class="meta-list" {
                            div { dt { "Board" } dd { (game.board_size.as_str()) } }
                            div { dt { "Mode" } dd { (game.game_type.as_str()) } }
                            div { dt { "Status" } dd { (capitalize(game.status.as_str())) } }
                            div { dt { "Created" } dd { (game.created_at.format("%Y-%m-%d %H:%M UTC")) } }
                        }
                    }
                }
            }
        }),
    ))
}

fn ordinal_place(n: i32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix} Place")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
