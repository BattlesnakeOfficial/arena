use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::Context as _;
use maud::html;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerResult, WithRedirect, WithStatus},
    models::{
        battlesnake::{self, Visibility},
        leaderboard::{self, MIN_GAMES_FOR_RANKING},
    },
    routes::auth::{CurrentUser, OptionalUser},
    state::AppState,
};

/// GET /leaderboards — list all leaderboards
pub async fn list_leaderboards(
    State(state): State<AppState>,
    OptionalUser(_user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let leaderboards = leaderboard::get_all_leaderboards(&state.db)
        .await
        .wrap_err("Failed to fetch leaderboards")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(page_factory.create_page(
        "Leaderboards".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Leaderboards" }

                @if leaderboards.is_empty() {
                    p { "No leaderboards available yet." }
                } @else {
                    div class="leaderboards-list" {
                        @for lb in &leaderboards {
                            div class="card" style="border: 1px solid #ddd; border-radius: 8px; padding: 20px; margin-bottom: 16px;" {
                                h2 {
                                    a href={"/leaderboards/"(lb.leaderboard_id)} { (lb.name) }
                                }
                                @if lb.disabled_at.is_some() {
                                    span class="badge bg-secondary text-white" { "Inactive" }
                                } @else {
                                    span class="badge bg-success text-white" { "Active" }
                                }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/" { "Back to Home" }
                }
            }
        }),
    ))
}

/// GET /leaderboards/:id — leaderboard detail with rankings
pub async fn show_leaderboard(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    Path(leaderboard_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard not found"),
                StatusCode::NOT_FOUND,
            )
        })?;

    let ranked = leaderboard::get_ranked_entries(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch ranked entries")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)?;

    let placement = leaderboard::get_placement_entries(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch placement entries")
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Get user's snakes for the join form
    let user_snakes = if let Some(ref u) = user {
        battlesnake::get_battlesnakes_by_user_id(&state.db, u.user_id)
            .await
            .unwrap_or_default()
    } else {
        vec![]
    };

    // Get user's entries in this leaderboard
    let user_entries = if let Some(ref u) = user {
        leaderboard::get_user_entries(&state.db, leaderboard_id, u.user_id)
            .await
            .unwrap_or_default()
    } else {
        vec![]
    };

    let user_entry_snake_ids: Vec<Uuid> = user_entries.iter().map(|e| e.battlesnake_id).collect();

    Ok(page_factory.create_page(
        format!("Leaderboard: {}", lb.name),
        Box::new(html! {
            div class="container" {
                h1 { "Leaderboard: " (lb.name) }

                // Join/leave section for logged-in users
                @if user.is_some() {
                    div style="margin-bottom: 20px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                        h3 { "Your Snakes" }

                        // Show currently joined snakes with leave button
                        @for entry in &user_entries {
                            @if let Some(snake) = user_snakes.iter().find(|s| s.battlesnake_id == entry.battlesnake_id) {
                                div style="display: flex; align-items: center; gap: 10px; margin-bottom: 8px;" {
                                    span { (snake.name) }
                                    @if entry.disabled_at.is_some() {
                                        span class="badge bg-secondary text-white" { "Paused" }
                                        form action={"/leaderboards/"(leaderboard_id)"/join"} method="post" style="display: inline;" {
                                            input type="hidden" name="battlesnake_id" value=(snake.battlesnake_id);
                                            button type="submit" class="btn btn-sm btn-success" { "Resume" }
                                        }
                                    } @else {
                                        span class="badge bg-success text-white" { "Active" }
                                        form action={"/leaderboards/"(leaderboard_id)"/leave"} method="post" style="display: inline;" {
                                            input type="hidden" name="battlesnake_id" value=(snake.battlesnake_id);
                                            button type="submit" class="btn btn-sm btn-warning" { "Pause" }
                                        }
                                    }
                                    span style="color: #666;" {
                                        "Score: " (format!("{:.1}", entry.display_score))
                                        " | Games: " (entry.games_played)
                                    }
                                }
                            }
                        }

                        // Show joinable snakes (public, not already joined)
                        @let joinable: Vec<_> = user_snakes.iter()
                            .filter(|s| s.visibility == Visibility::Public && !user_entry_snake_ids.contains(&s.battlesnake_id))
                            .collect();
                        @if !joinable.is_empty() {
                            form action={"/leaderboards/"(leaderboard_id)"/join"} method="post" style="margin-top: 10px;" {
                                label { "Join with: " }
                                select name="battlesnake_id" {
                                    @for snake in joinable {
                                        option value=(snake.battlesnake_id) { (snake.name) }
                                    }
                                }
                                button type="submit" class="btn btn-sm btn-primary" style="margin-left: 8px;" { "Join" }
                            }
                        }
                    }
                }

                // Rankings table
                h2 { "Rankings" }
                @if ranked.is_empty() {
                    p { "No snakes have completed enough games to be ranked yet. (Minimum: " (MIN_GAMES_FOR_RANKING) " games)" }
                } @else {
                    table class="table" {
                        thead {
                            tr {
                                th { "Rank" }
                                th { "Snake" }
                                th { "Owner" }
                                th { "Score" }
                                th { "Games" }
                                th { "Win Rate" }
                            }
                        }
                        tbody {
                            @for (i, entry) in ranked.iter().enumerate() {
                                tr {
                                    td { (i + 1) }
                                    td { (entry.snake_name) }
                                    td { (entry.owner_login) }
                                    td { (format!("{:.1}", entry.display_score)) }
                                    td { (entry.games_played) }
                                    td {
                                        @if entry.games_played > 0 {
                                            (format!("{:.0}%", (entry.wins as f64 / entry.games_played as f64) * 100.0))
                                        } @else {
                                            "N/A"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Placement section
                @if !placement.is_empty() {
                    h2 { "In Placement" }
                    p style="color: #666;" { "These snakes need more games before appearing in rankings." }
                    table class="table" {
                        thead {
                            tr {
                                th { "Snake" }
                                th { "Owner" }
                                th { "Games Played" }
                                th { "Games Remaining" }
                            }
                        }
                        tbody {
                            @for entry in &placement {
                                tr {
                                    td { (entry.snake_name) }
                                    td { (entry.owner_login) }
                                    td { (entry.games_played) }
                                    td { (MIN_GAMES_FOR_RANKING - entry.games_played) }
                                }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/leaderboards" { "Back to Leaderboards" }
                    span { " | " }
                    a href="/" { "Home" }
                }
            }
        }),
    ))
}

#[derive(serde::Deserialize)]
pub struct JoinLeaveForm {
    pub battlesnake_id: Uuid,
}

/// POST /leaderboards/:id/join — opt-in a snake
pub async fn join_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<JoinLeaveForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}"));

    // Verify leaderboard exists and is active
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")
        .with_redirect(redirect.clone())?;

    let lb = lb.ok_or_else(|| {
        crate::errors::ServerError(
            color_eyre::eyre::eyre!("Leaderboard not found"),
            redirect.clone(),
        )
    })?;

    if lb.disabled_at.is_some() {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Leaderboard is not active"),
            redirect,
        ));
    }

    // Verify snake belongs to user and is public
    let snake = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Battlesnake not found"),
                redirect.clone(),
            )
        })?;

    if snake.user_id != user.user_id {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You don't own this battlesnake"),
            redirect,
        ));
    }

    if snake.visibility != Visibility::Public {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Only public snakes can join leaderboards"),
            redirect,
        ));
    }

    // Opt-in (or resume if paused)
    leaderboard::get_or_create_entry(&state.db, leaderboard_id, form.battlesnake_id)
        .await
        .wrap_err("Failed to join leaderboard")
        .with_redirect(redirect.clone())?;

    Ok(redirect)
}

/// POST /leaderboards/:id/leave — pause a snake
pub async fn leave_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<JoinLeaveForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}"));

    // Verify snake belongs to user
    let snake = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Battlesnake not found"),
                redirect.clone(),
            )
        })?;

    if snake.user_id != user.user_id {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You don't own this battlesnake"),
            redirect,
        ));
    }

    // Find the entry and pause it
    let entry = leaderboard::get_entry(&state.db, leaderboard_id, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch entry")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Snake is not in this leaderboard"),
                redirect.clone(),
            )
        })?;

    leaderboard::set_disabled(
        &state.db,
        entry.leaderboard_entry_id,
        Some(chrono::Utc::now()),
    )
    .await
    .wrap_err("Failed to pause entry")
    .with_redirect(redirect.clone())?;

    Ok(redirect)
}
