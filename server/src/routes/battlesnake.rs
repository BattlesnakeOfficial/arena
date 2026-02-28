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
    errors::{ServerResult, WithStatus},
    models::battlesnake::{self, CreateBattlesnake, UpdateBattlesnake, Visibility},
    models::game_battlesnake,
    models::leaderboard,
    models::session,
    models::user::get_user_by_id,
    routes::auth::{CurrentUser, CurrentUserWithSession},
    state::AppState,
};

// List all battlesnakes for the current user
pub async fn list_battlesnakes(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get all battlesnakes for the current user
    let battlesnakes = battlesnake::get_battlesnakes_by_user_id(&state.db, user.user_id)
        .await
        .wrap_err("Failed to get battlesnakes")?;

    // Use flash from page_factory (already extracted and cleared from DB)
    let flash = page_factory.flash.clone();

    // Render the battlesnake list page
    Ok(page_factory.create_page_with_flash(
        "Your Battlesnakes".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Your Battlesnakes" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                @if battlesnakes.is_empty() {
                    div class="empty-state" {
                        p { "You don't have any battlesnakes yet." }
                    }
                } @else {
                    div class="battlesnakes-list" {
                        table class="table" {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "URL" }
                                    th { "Visibility" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                @for snake in &battlesnakes {
                                    tr {
                                        td { (snake.name) }
                                        td {
                                            a href=(snake.url) target="_blank" { (snake.url) }
                                        }
                                        td {
                                            @if snake.visibility == Visibility::Public {
                                                span class="badge bg-success text-white" { "Public" }
                                            } @else {
                                                span class="badge bg-secondary text-white" { "Private" }
                                            }
                                        }
                                        td class="actions" {
                                            a href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} class="btn btn-sm btn-info" { "View" }
                                            a href={"/battlesnakes/"(snake.battlesnake_id)"/edit"} class="btn btn-sm btn-primary" { "Edit" }
                                            form action={"/battlesnakes/"(snake.battlesnake_id)"/delete"} method="post" style="display: inline;" {
                                                button type="submit" class="btn btn-sm btn-danger" onclick="return confirm('Are you sure you want to delete this battlesnake?');" { "Delete" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div class="actions" style="margin-top: 20px;" {
                    a href="/battlesnakes/new" class="btn btn-primary" { "Add New Battlesnake" }
                    a href="/me" class="btn btn-secondary" { "Back to Profile" }
                }
            }
        }),
        flash,
    ))
}

// Show the form to create a new battlesnake
pub async fn new_battlesnake(
    CurrentUser(_): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Use flash from page_factory (already extracted and cleared from DB)
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        "Add New Battlesnake".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Add New Battlesnake" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                form action="/battlesnakes" method="post" {
                    div class="form-group" {
                        label for="name" { "Name" }
                        input type="text" id="name" name="name" class="form-control" required {}
                    }

                    div class="form-group" {
                        label for="url" { "URL" }
                        input type="url" id="url" name="url" class="form-control" required placeholder="https://your-battlesnake-server.com" {}
                        small class="form-text text-muted" { "The URL of your Battlesnake server" }
                    }

                    div class="form-group" {
                        label for="visibility" { "Visibility" }
                        select id="visibility" name="visibility" class="form-control" required {
                            option value="public" selected { "Public (Available to all users)" }
                            option value="private" { "Private (Only available to you)" }
                        }
                        small class="form-text text-muted" { "Control who can add this snake to games" }
                    }

                    div class="form-group" style="margin-top: 20px;" {
                        button type="submit" class="btn btn-primary" { "Create Battlesnake" }
                        a href="/battlesnakes" class="btn btn-secondary" { "Cancel" }
                    }
                }
            }
        }),
        flash,
    ))
}

// Handle the creation of a new battlesnake
pub async fn create_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Form(create_data): Form<CreateBattlesnake>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    tracing::info!(
        "create_battlesnake: session_id={}, user_id={}, has_flash={:?}",
        session.session_id,
        user.user_id,
        session.flash_message.is_some()
    );

    // Create the new battlesnake in the database
    let battlesnake_result =
        battlesnake::create_battlesnake(&state.db, user.user_id, create_data.clone()).await;

    match battlesnake_result {
        Ok(_) => {
            // Flash message for success and redirect
            let updated_session = session::set_flash_message(
                &state.db,
                session.session_id,
                "Battlesnake created successfully!".to_string(),
                session::FLASH_TYPE_SUCCESS,
            )
            .await
            .wrap_err("Failed to set flash message")?;

            tracing::info!(
                "Flash set: session_id={}, flash_message={:?}",
                updated_session.session_id,
                updated_session.flash_message
            );

            Ok(Redirect::to("/battlesnakes").into_response())
        }
        Err(err) => {
            // Check if it's a name uniqueness error
            if err.to_string().contains("already have a battlesnake named") {
                // Set error flash message
                session::set_flash_message(
                    &state.db,
                    session.session_id,
                    err.to_string(),
                    session::FLASH_TYPE_ERROR,
                )
                .await
                .wrap_err("Failed to set flash message")?;

                // Redirect back to the form
                Ok(Redirect::to("/battlesnakes/new").into_response())
            } else {
                // For other errors, propagate them
                Err(err).wrap_err("Failed to create battlesnake")?
            }
        }
    }
}

// Show the form to edit an existing battlesnake
pub async fn edit_battlesnake(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(battlesnake_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the battlesnake by ID
    let battlesnake = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake")?
        .ok_or_else(|| "Battlesnake not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Check if the battlesnake belongs to the current user
    if battlesnake.user_id != user.user_id {
        return Err("You don't have permission to edit this battlesnake".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    // Use flash from page_factory (already extracted and cleared from DB)
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        format!("Edit Battlesnake: {}", battlesnake.name),
        Box::new(html! {
            div class="container" {
                h1 { "Edit Battlesnake: " (battlesnake.name) }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                form action={"/battlesnakes/"(battlesnake_id)"/update"} method="post" {
                    div class="form-group" {
                        label for="name" { "Name" }
                        input type="text" id="name" name="name" class="form-control" required value=(battlesnake.name) {}
                    }

                    div class="form-group" {
                        label for="url" { "URL" }
                        input type="url" id="url" name="url" class="form-control" required value=(battlesnake.url) {}
                        small class="form-text text-muted" { "The URL of your Battlesnake server" }
                    }

                    div class="form-group" {
                        label for="visibility" { "Visibility" }
                        select id="visibility" name="visibility" class="form-control" required {
                            option value="public" selected=(battlesnake.visibility == Visibility::Public) { "Public (Available to all users)" }
                            option value="private" selected=(battlesnake.visibility == Visibility::Private) { "Private (Only available to you)" }
                        }
                        small class="form-text text-muted" { "Control who can add this snake to games" }
                    }

                    div class="form-group" style="margin-top: 20px;" {
                        button type="submit" class="btn btn-primary" { "Update Battlesnake" }
                        a href="/battlesnakes" class="btn btn-secondary" { "Cancel" }
                    }
                }
            }
        }),
        flash,
    ))
}

// Handle the update of an existing battlesnake
pub async fn update_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(battlesnake_id): Path<Uuid>,
    Form(update_data): Form<UpdateBattlesnake>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // First check if the battlesnake exists and belongs to the user
    let exists = battlesnake::belongs_to_user(&state.db, battlesnake_id, user.user_id)
        .await
        .wrap_err("Failed to check battlesnake ownership")?;

    if !exists {
        return Err("Battlesnake not found or you don't have permission to update it".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    // Update the battlesnake
    let update_result = battlesnake::update_battlesnake(
        &state.db,
        battlesnake_id,
        user.user_id,
        update_data.clone(),
    )
    .await;

    match update_result {
        Ok(_) => {
            // Flash message for success and redirect
            session::set_flash_message(
                &state.db,
                session.session_id,
                "Battlesnake updated successfully!".to_string(),
                session::FLASH_TYPE_SUCCESS,
            )
            .await
            .wrap_err("Failed to set flash message")?;

            Ok(Redirect::to("/battlesnakes").into_response())
        }
        Err(err) => {
            // Check if it's a name uniqueness error
            if err.to_string().contains("already have a battlesnake named") {
                // Set error flash message
                session::set_flash_message(
                    &state.db,
                    session.session_id,
                    err.to_string(),
                    session::FLASH_TYPE_ERROR,
                )
                .await
                .wrap_err("Failed to set flash message")?;

                // Redirect back to the edit form
                Ok(Redirect::to(&format!("/battlesnakes/{}/edit", battlesnake_id)).into_response())
            } else {
                // For other errors, propagate them
                Err(err).wrap_err("Failed to update battlesnake")?
            }
        }
    }
}

// Handle the deletion of a battlesnake
pub async fn delete_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(battlesnake_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // First check if the battlesnake exists and belongs to the user
    let exists = battlesnake::belongs_to_user(&state.db, battlesnake_id, user.user_id)
        .await
        .wrap_err("Failed to check battlesnake ownership")?;

    if !exists {
        return Err("Battlesnake not found or you don't have permission to delete it".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    // Delete the battlesnake
    battlesnake::delete_battlesnake(&state.db, battlesnake_id, user.user_id)
        .await
        .wrap_err("Failed to delete battlesnake")?;

    // Flash message for success and redirect
    session::set_flash_message(
        &state.db,
        session.session_id,
        "Battlesnake deleted successfully!".to_string(),
        session::FLASH_TYPE_SUCCESS,
    )
    .await
    .wrap_err("Failed to set flash message")?;

    Ok(Redirect::to("/battlesnakes").into_response())
}

struct BattlesnakeStats {
    total_games: usize,
    finished_games: usize,
    wins: usize,
    second_places: usize,
    third_places: usize,
    fourth_places: usize,
    win_rate: f64,
    average_placement: f64,
}

fn compute_stats(history: &[game_battlesnake::GameHistoryEntry]) -> BattlesnakeStats {
    use crate::models::game::GameStatus;

    let total_games = history.len();
    let mut finished_games = 0usize;
    let mut wins = 0usize;
    let mut second_places = 0usize;
    let mut third_places = 0usize;
    let mut fourth_places = 0usize;
    let mut placement_sum = 0i64;
    let mut placement_count = 0usize;

    for entry in history {
        if entry.status == GameStatus::Finished {
            finished_games += 1;
            if let Some(placement) = entry.placement {
                match placement {
                    1 => wins += 1,
                    2 => second_places += 1,
                    3 => third_places += 1,
                    4 => fourth_places += 1,
                    _ => {}
                }
                placement_sum += i64::from(placement);
                placement_count += 1;
            }
        }
    }

    let win_rate = if finished_games > 0 {
        (wins as f64 / finished_games as f64) * 100.0
    } else {
        0.0
    };

    let average_placement = if placement_count > 0 {
        placement_sum as f64 / placement_count as f64
    } else {
        0.0
    };

    BattlesnakeStats {
        total_games,
        finished_games,
        wins,
        second_places,
        third_places,
        fourth_places,
        win_rate,
        average_placement,
    }
}

// View a battlesnake's profile with game history and stats
#[allow(clippy::too_many_lines)]
pub async fn view_battlesnake_profile(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(battlesnake_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Fetch the battlesnake
    let snake = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake")?
        .ok_or_else(|| "Battlesnake not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Fetch the owner user info
    let owner = get_user_by_id(&state.db, snake.user_id)
        .await
        .wrap_err("Failed to get owner user")?;

    // Fetch game history
    let history = game_battlesnake::get_game_history_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get game history")?;

    // Fetch leaderboard entries
    let leaderboard_entries = leaderboard::get_entries_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get leaderboard entries")?;

    let flash = page_factory.flash.clone();

    // Compute stats
    let stats = compute_stats(&history);

    let is_owner = user.user_id == snake.user_id;

    // Owner display info
    let owner_login = owner
        .as_ref()
        .map(|o| o.github_login.clone())
        .unwrap_or_else(|| "Unknown User".to_string());
    let owner_avatar = owner
        .as_ref()
        .and_then(|o| o.github_avatar_url.clone())
        .unwrap_or_default();

    Ok(page_factory.create_page_with_flash(
        format!("Battlesnake: {}", snake.name),
        Box::new(html! {
            div class="container" {
                // Flash messages
                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                // Snake Header Section
                div class="card mb-4" {
                    div class="card-body" {
                        div class="d-flex justify-content-between align-items-center" {
                            div {
                                h1 class="mb-2" { (snake.name) }
                                div class="d-flex align-items-center mb-2" {
                                    img src=(owner_avatar) alt="Owner avatar" style="width: 24px; height: 24px; border-radius: 50%; margin-right: 8px;" {}
                                    span { (owner_login) }
                                }
                                @if snake.visibility == Visibility::Public {
                                    span class="badge bg-success text-white" { "Public" }
                                } @else {
                                    span class="badge bg-secondary text-white" { "Private" }
                                }
                                p class="mt-2" {
                                    "URL: "
                                    a href=(snake.url) target="_blank" { (snake.url) }
                                }
                                p { "Created: " (snake.created_at.format("%Y-%m-%d %H:%M")) }
                            }
                            @if is_owner {
                                div {
                                    a href={"/battlesnakes/"(battlesnake_id)"/edit"} class="btn btn-sm btn-primary" { "Edit" }
                                    form action={"/battlesnakes/"(battlesnake_id)"/delete"} method="post" class="inline" style="display: inline;" {
                                        button type="submit" class="btn btn-sm btn-danger" onclick="return confirm('Are you sure you want to delete this battlesnake?');" { "Delete" }
                                    }
                                }
                            }
                        }
                    }
                }

                // Statistics Section
                h2 { "Statistics" }

                div class="d-flex" style="gap: 16px; flex-wrap: wrap; margin-bottom: 20px;" {
                    div class="card mb-4" style="flex: 1; min-width: 150px;" {
                        div class="card-body" {
                            h5 { "Games Played" }
                            p style="font-size: 2em; margin: 0;" { (stats.total_games) }
                        }
                    }
                    div class="card mb-4" style="flex: 1; min-width: 150px;" {
                        div class="card-body" {
                            h5 { "Win Rate" }
                            p style="font-size: 2em; margin: 0;" {
                                @if stats.finished_games > 0 {
                                    (format!("{:.1}%", stats.win_rate))
                                } @else {
                                    "N/A"
                                }
                            }
                        }
                    }
                    div class="card mb-4" style="flex: 1; min-width: 150px;" {
                        div class="card-body" {
                            h5 { "Wins" }
                            p style="font-size: 2em; margin: 0;" {
                                span class="badge bg-success text-white" { (stats.wins) }
                            }
                        }
                    }
                    div class="card mb-4" style="flex: 1; min-width: 150px;" {
                        div class="card-body" {
                            h5 { "Avg. Placement" }
                            p style="font-size: 2em; margin: 0;" {
                                @if stats.finished_games > 0 {
                                    (format!("{:.1}", stats.average_placement))
                                } @else {
                                    "N/A"
                                }
                            }
                        }
                    }
                }

                // Placement Distribution
                @if stats.finished_games > 0 {
                    div class="card mb-4" {
                        div class="card-body" {
                            h5 { "Placement Distribution" }
                            div class="d-flex" style="gap: 16px;" {
                                span { "🥇 1st: " (stats.wins) }
                                span { "🥈 2nd: " (stats.second_places) }
                                span { "🥉 3rd: " (stats.third_places) }
                                span { "4th: " (stats.fourth_places) }
                            }
                        }
                    }
                }

                // Leaderboard Participation
                @if !leaderboard_entries.is_empty() {
                    h2 { "Leaderboard Participation" }
                    table class="table" {
                        thead {
                            tr {
                                th { "Leaderboard" }
                                th { "Rating" }
                                th { "Games" }
                                th { "1st Place %" }
                                th { "Status" }
                                th { "" }
                            }
                        }
                        tbody {
                            @for entry in &leaderboard_entries {
                                tr {
                                    td { (entry.leaderboard_name) }
                                    td { (format!("{:.1}", entry.display_score)) }
                                    td { (entry.games_played) }
                                    td {
                                        @if entry.games_played > 0 {
                                            (format!("{:.0}%", (entry.first_place_finishes as f64 / entry.games_played as f64) * 100.0))
                                        } @else {
                                            "N/A"
                                        }
                                    }
                                    td {
                                        @if entry.disabled_at.is_some() {
                                            span class="badge bg-secondary text-white" { "Paused" }
                                        } @else {
                                            span class="badge bg-success text-white" { "Active" }
                                        }
                                    }
                                    td {
                                        a href={"/leaderboards/"(entry.leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} class="btn btn-sm btn-info" { "Details" }
                                    }
                                }
                            }
                        }
                    }
                }

                // Game History Table
                h2 { "Game History" }

                @if history.is_empty() {
                    div class="alert alert-info" {
                        p { "No games played yet." }
                    }
                } @else {
                    div class="table-responsive" {
                        table class="table table-striped" {
                            thead {
                                tr {
                                    th { "Game Type" }
                                    th { "Board Size" }
                                    th { "Snakes" }
                                    th { "Placement" }
                                    th { "Winner" }
                                    th { "Date" }
                                    th { "Actions" }
                                }
                            }
                            tbody {
                                @for entry in &history {
                                    tr {
                                        td { (entry.game_type.as_str()) }
                                        td { (entry.board_size.as_str()) }
                                        td { (entry.snake_count) }
                                        td {
                                            @if let Some(placement) = entry.placement {
                                                @match placement {
                                                    1 => span class="badge bg-warning text-dark" { "🥇 1st" },
                                                    2 => span class="badge bg-secondary text-white" { "🥈 2nd" },
                                                    3 => span class="badge bg-danger text-white" { "🥉 3rd" },
                                                    _ => span class="badge bg-dark text-white" { (placement) "th" },
                                                }
                                            } @else {
                                                span class="badge bg-info text-dark" { "In Progress" }
                                            }
                                        }
                                        td {
                                            @if let Some(winner) = &entry.winner_name {
                                                (winner)
                                            } @else {
                                                @if entry.status == crate::models::game::GameStatus::Finished {
                                                    span class="badge bg-secondary text-white" { "No Winner" }
                                                } @else {
                                                    span class="badge bg-info text-dark" { "In Progress" }
                                                }
                                            }
                                        }
                                        td { (entry.created_at.format("%Y-%m-%d %H:%M")) }
                                        td {
                                            a href={"/games/"(entry.game_id)} class="btn btn-sm btn-primary" { "View" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Navigation Links
                div class="mt-4" {
                    a href="/games" class="btn btn-primary" { "All Games" }
                    @if is_owner {
                        a href="/battlesnakes" class="btn btn-secondary ms-2" { "Your Battlesnakes" }
                    }
                    a href="/me" class="btn btn-secondary ms-2" { "My Profile" }
                }
            }
        }),
        flash,
    ))
}
