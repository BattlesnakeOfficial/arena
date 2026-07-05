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
    models::snake_health_status,
    models::tournament,
    models::user::get_user_by_id,
    routes::auth::{CurrentUser, CurrentUserWithSession},
    snake_health,
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
        Ok(snake) => {
            if snake.visibility == Visibility::Public {
                state
                    .discord
                    .notify_snake_registered(&snake.name, &user.github_login);
            }
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

    // Refuse to delete a battlesnake that is registered in an active
    // tournament — the FK cascades would rip it out of a live bracket.
    let active_registrations =
        tournament::count_active_tournament_registrations(&state.db, battlesnake_id)
            .await
            .wrap_err("Failed to check tournament registrations")?;

    if active_registrations > 0 {
        session::set_flash_message(
            &state.db,
            session.session_id,
            "This battlesnake is registered in an active tournament and can't be deleted. Withdraw it from the tournament first.".to_string(),
            session::FLASH_TYPE_ERROR,
        )
        .await
        .wrap_err("Failed to set flash message")?;

        return Ok(Redirect::to("/battlesnakes").into_response());
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

/// POST /battlesnakes/{id}/reactivate — owner recovery from a health-sweeper
/// deactivation (BS-3534). Re-enables exactly the leaderboard entries the
/// sweeper disabled (manual pauses stay paused) and resets the failure
/// streak so the next sweep starts fresh.
pub async fn reactivate_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(battlesnake_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let owns = battlesnake::belongs_to_user(&state.db, battlesnake_id, user.user_id)
        .await
        .wrap_err("Failed to check battlesnake ownership")?;

    if !owns {
        return Err(
            "Battlesnake not found or you don't have permission to reactivate it".to_string(),
        )
        .with_status(StatusCode::FORBIDDEN);
    }

    let was_deactivated = snake_health_status::get(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get snake health status")?
        .is_some_and(|s| s.deactivated_at.is_some());

    if !was_deactivated {
        session::set_flash_message(
            &state.db,
            session.session_id,
            "This battlesnake isn't paused for health issues.".to_string(),
            session::FLASH_TYPE_ERROR,
        )
        .await
        .wrap_err("Failed to set flash message")?;

        return Ok(
            Redirect::to(&format!("/battlesnakes/{battlesnake_id}/profile")).into_response(),
        );
    }

    snake_health_status::reactivate(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to reactivate battlesnake")?;

    tracing::info!(
        battlesnake_id = %battlesnake_id,
        user_id = %user.user_id,
        "Owner reactivated snake for leaderboard matchmaking"
    );

    session::set_flash_message(
        &state.db,
        session.session_id,
        "Matchmaking resumed! Your snake will be picked up in upcoming matches.".to_string(),
        session::FLASH_TYPE_SUCCESS,
    )
    .await
    .wrap_err("Failed to set flash message")?;

    Ok(Redirect::to(&format!("/battlesnakes/{battlesnake_id}/profile")).into_response())
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

    // Health-sweeper state, for the owner-facing deactivation banner
    let health_status = snake_health_status::get(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get snake health status")?;

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
    let owner_pronouns = owner
        .as_ref()
        .map(|o| o.pronouns.clone())
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

                // Auto-deactivation banner: the health sweeper pulled this
                // snake from matchmaking; the owner can resume once fixed.
                @if is_owner {
                    @if let Some(status) = health_status.as_ref().filter(|s| s.deactivated_at.is_some()) {
                        div class="alert alert-warning" {
                            p {
                                strong { "Paused from leaderboard matchmaking. " }
                                "This snake failed " (status.consecutive_failures)
                                " health checks in a row, so we stopped matching it to protect its rating."
                            }
                            @if let Some(failure) = status.last_failure.as_ref() {
                                p class="small" { "Most recent problem: " (failure) }
                            }
                            p class="small" {
                                "Fix your snake (the Test Snake button runs the same checks), then resume."
                            }
                            form action={"/battlesnakes/"(battlesnake_id)"/reactivate"} method="post" style="display: inline;" {
                                button type="submit" class="btn btn-sm btn-success" { "Resume Matchmaking" }
                            }
                        }
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
                                    @if !owner_pronouns.is_empty() {
                                        span class="text-muted" { " · " (owner_pronouns) }
                                    }
                                }
                                @if snake.visibility == Visibility::Public {
                                    span class="badge bg-success text-white" { "Public" }
                                } @else {
                                    span class="badge bg-secondary text-white" { "Private" }
                                }
                                div class="mt-2" {
                                    @let display_head = if snake.head.is_empty() { "default" } else { snake.head.as_str() };
                                    @let display_tail = if snake.tail.is_empty() { "default" } else { snake.tail.as_str() };
                                    @let raw_color = if snake.color.is_empty() { "#888888" } else { snake.color.as_str() };
                                    @let url_color = if let Some(hex) = raw_color.strip_prefix('#') { format!("%23{hex}") } else { raw_color.to_string() };
                                    @let avatar_url = format!(
                                        "https://exporter.battlesnake.com/avatars/head:{}/tail:{}/color:{}/320x100.svg",
                                        display_head, display_tail, url_color
                                    );
                                    img src=(avatar_url) alt=(format!("{} snake preview", snake.name))
                                        style="max-width:320px;height:auto;display:block;margin-bottom:4px;" {}
                                    span class="text-muted small" {
                                        "Head: " (display_head) " · Tail: " (display_tail) " · Color: " (raw_color)
                                    }
                                }
                                @if is_owner {
                                    p class="mt-2" {
                                        "URL: "
                                        a href=(snake.url) target="_blank" { (snake.url) }
                                    }
                                }
                                p { "Created: " (snake.created_at.format("%Y-%m-%d %H:%M")) }
                            }
                            @if is_owner {
                                div {
                                    form action={"/battlesnakes/"(battlesnake_id)"/test"} method="post" class="inline" style="display: inline;" {
                                        button type="submit" class="btn btn-sm btn-info" { "Test Snake" }
                                    }
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

// Run an on-demand health check against a battlesnake's URL (BS-015).
//
// Owner-only: the snake URL may be publicly visible, but the test makes the
// server poke the user's infrastructure on demand, so only the owner can
// trigger it. Renders the results page directly from the POST (a flash +
// redirect would lose the per-call details).
#[allow(clippy::too_many_lines)]
pub async fn test_battlesnake(
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

    // Only the owner may trigger test calls against the snake's server
    if snake.user_id != user.user_id {
        return Err("You don't have permission to test this battlesnake".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    // Dedicated client: the shared snake client enforces the real in-game
    // budget (600ms hard timeout); the test is deliberately more forgiving
    // and reports latency so users can see whether they'd fit the budget.
    // Redirect handling matches the game client (reqwest defaults).
    let client = reqwest::Client::builder()
        .timeout(snake_health::HEALTH_CHECK_TIMEOUT)
        .build()
        .wrap_err("Failed to build HTTP client for snake test")?;

    let (engine_game, snake_id) = snake_health::build_test_game(&snake);
    let report = snake_health::run_health_check(
        &client,
        &snake.url,
        &engine_game,
        &snake_id,
        snake_health::FailureMode::RunAll,
    )
    .await;

    let failures = report.failure_count();
    let all_ok = failures == 0;

    Ok(page_factory.create_page(
        format!("Test Results: {}", snake.name),
        Box::new(html! {
            div class="container" {
                h1 { "Test Results: " (snake.name) }
                p {
                    "Tested "
                    a href=(snake.url) target="_blank" { (snake.url) }
                    " with the same calls a real game makes."
                }

                @if all_ok {
                    div class="alert alert-success" {
                        p { "All " (report.calls.len()) " checks passed. This snake looks ready to play!" }
                    }
                } @else {
                    div class="alert alert-danger" {
                        p { (failures) " of " (report.calls.len()) " checks failed. See details below." }
                    }
                }

                table class="table" {
                    thead {
                        tr {
                            th { "Call" }
                            th { "Result" }
                            th { "HTTP Status" }
                            th { "Latency" }
                            th { "Details" }
                        }
                    }
                    tbody {
                        @for call in &report.calls {
                            tr {
                                td { code { (call.name) } }
                                td {
                                    @if call.ok {
                                        span class="badge bg-success text-white" { "OK" }
                                    } @else {
                                        span class="badge bg-danger text-white" { "Failed" }
                                    }
                                }
                                td {
                                    @if let Some(status) = call.http_status {
                                        (status)
                                    } @else {
                                        "—"
                                    }
                                }
                                td {
                                    @if let Some(latency) = call.latency_ms {
                                        (latency) " ms"
                                        @if i64::try_from(latency).is_ok_and(|l| l > report.game_timeout_ms) {
                                            " "
                                            span class="badge bg-warning text-dark" { "over game budget" }
                                        }
                                    } @else {
                                        "—"
                                    }
                                }
                                td {
                                    (call.summary)
                                    @if let Some(excerpt) = &call.body_excerpt {
                                        pre style="white-space: pre-wrap; word-break: break-all; margin-top: 8px; font-size: 0.85em;" {
                                            (excerpt)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                p class="text-muted" {
                    "Each test call was allowed "
                    (snake_health::HEALTH_CHECK_TIMEOUT.as_secs())
                    " seconds, but real games only allow "
                    (report.game_timeout_ms)
                    " ms per request — check the latency column to see if your snake fits the in-game budget."
                }

                div class="mt-4" {
                    form action={"/battlesnakes/"(battlesnake_id)"/test"} method="post" class="inline" style="display: inline;" {
                        button type="submit" class="btn btn-primary" { "Run Test Again" }
                    }
                    a href={"/battlesnakes/"(battlesnake_id)"/profile"} class="btn btn-secondary ms-2" { "Back to Profile" }
                }
            }
        }),
    ))
}
