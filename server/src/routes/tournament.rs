use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr as _;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerResult, WithStatus},
    models::{
        battlesnake,
        game::{GameBoardSize, GameType},
        leaderboard, session,
        tournament::{
            self, CreateTournament, MatchStyle, RegistrationStatus, Tournament, TournamentStatus,
            TournamentVisibility, UpdateTournamentSettings,
        },
        user,
    },
    routes::auth::{CurrentUser, CurrentUserWithSession, OptionalUser},
    state::AppState,
};

/// Cap for the leaderboard import qualifier flow.
const MAX_IMPORT_COUNT: i64 = 32;

// --- Pure business rules (unit tested below) ---

/// Registrations can only be added/removed/reseeded before the bracket exists.
fn registrations_editable(status: TournamentStatus) -> bool {
    matches!(
        status,
        TournamentStatus::Created | TournamentStatus::Registration
    )
}

/// Registration permission matrix: the tournament must be in a pre-start
/// status, and the registration_status must allow the caller.
fn can_register(tournament: &Tournament, is_owner: bool) -> bool {
    if !registrations_editable(tournament.status) {
        return false;
    }
    match tournament.registration_status {
        RegistrationStatus::Open => true,
        RegistrationStatus::OwnerOnly => is_owner,
        RegistrationStatus::Closed => false,
    }
}

/// Who can view a tournament page. `participants_only` tournaments are only
/// visible to the owner and users with a registered snake.
fn can_view(
    tournament: &Tournament,
    viewer_user_id: Option<Uuid>,
    participant_user_ids: &[Uuid],
) -> bool {
    match tournament.visibility {
        TournamentVisibility::Public => true,
        TournamentVisibility::ParticipantsOnly => viewer_user_id
            .is_some_and(|id| id == tournament.user_id || participant_user_ids.contains(&id)),
    }
}

/// Shared validation for create + settings update.
fn validate_tournament_params(
    name: &str,
    required_participants: i32,
    max_snakes_per_user: i32,
) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Tournament name cannot be empty".to_string());
    }
    if required_participants < 2 {
        return Err("Required participants must be at least 2".to_string());
    }
    if max_snakes_per_user < 1 {
        return Err("Max snakes per user must be at least 1".to_string());
    }
    Ok(())
}

/// Settings-change rules: only editable before start, and game_type/board_size
/// are frozen once any snake is registered.
fn validate_settings_update(
    tournament: &Tournament,
    has_registrations: bool,
    new_game_type: &GameType,
    new_board_size: &GameBoardSize,
) -> Result<(), String> {
    if !registrations_editable(tournament.status) {
        return Err("Tournament settings can only be edited before the tournament starts".into());
    }
    if has_registrations
        && (*new_game_type != tournament.game_type || *new_board_size != tournament.board_size)
    {
        return Err(
            "Game type and board size cannot be changed after snakes have registered".into(),
        );
    }
    Ok(())
}

/// Parse a game type from a form value, rejecting anything outside the
/// supported dropdown options (GameType::from_str is a catch-all).
fn parse_game_type(s: &str) -> Result<GameType, String> {
    match GameType::from_str(s) {
        Ok(GameType::Other(_)) | Err(_) => Err(format!("Invalid game type: {s}")),
        Ok(game_type) => Ok(game_type),
    }
}

/// Parse a board size from a form value, rejecting custom sizes.
fn parse_board_size(s: &str) -> Result<GameBoardSize, String> {
    match GameBoardSize::from_str(s) {
        Ok(GameBoardSize::Custom(_)) | Err(_) => Err(format!("Invalid board size: {s}")),
        Ok(board_size) => Ok(board_size),
    }
}

// --- Shared rendering helpers ---

fn status_badge(status: TournamentStatus) -> Markup {
    let (class, label) = match status {
        TournamentStatus::Created => ("badge bg-secondary text-white", "Created"),
        TournamentStatus::Registration => ("badge bg-info text-dark", "Registration Open"),
        TournamentStatus::InProgress => ("badge bg-success text-white", "In Progress"),
        TournamentStatus::Completed => ("badge bg-primary text-white", "Completed"),
        TournamentStatus::Canceled => ("badge bg-danger text-white", "Canceled"),
    };
    html! { span class=(class) { (label) } }
}

/// Form fields shared by the create and edit pages. When `current` is Some,
/// fields are pre-filled with the tournament's existing values.
#[allow(clippy::too_many_lines)]
fn tournament_form_fields(current: Option<&Tournament>) -> Markup {
    let name = current.map(|t| t.name.clone()).unwrap_or_default();
    let description = current
        .and_then(|t| t.description.clone())
        .unwrap_or_default();
    let game_type = current.map_or(GameType::Standard, |t| t.game_type.clone());
    let board_size = current.map_or(GameBoardSize::Medium, |t| t.board_size.clone());
    let match_style = current.map_or(MatchStyle::SingleGame, |t| t.match_style);
    let registration_status = current.map_or(RegistrationStatus::Open, |t| t.registration_status);
    let visibility = current.map_or(TournamentVisibility::Public, |t| t.visibility);
    let max_snakes_per_user = current.map_or(1, |t| t.max_snakes_per_user);
    let required_participants = current.map_or(2, |t| t.required_participants);

    html! {
        div class="form-group" {
            label for="name" { "Name" }
            input type="text" id="name" name="name" class="form-control" required value=(name) {}
        }

        div class="form-group" {
            label for="description" { "Description" }
            textarea id="description" name="description" class="form-control" rows="3" { (description) }
        }

        div class="form-group" {
            label for="game_type" { "Game Type" }
            select id="game_type" name="game_type" class="form-control" required {
                option value="Standard" selected[game_type == GameType::Standard] { "Standard" }
                option value="Royale" selected[game_type == GameType::Royale] { "Royale" }
                option value="Constrictor" selected[game_type == GameType::Constrictor] { "Constrictor" }
                option value="Snail Mode" selected[game_type == GameType::SnailMode] { "Snail Mode" }
            }
            small class="form-text text-muted" { "Cannot be changed once snakes have registered" }
        }

        div class="form-group" {
            label for="board_size" { "Board Size" }
            select id="board_size" name="board_size" class="form-control" required {
                option value="7x7" selected[board_size == GameBoardSize::Small] { "7x7 (Small)" }
                option value="11x11" selected[board_size == GameBoardSize::Medium] { "11x11 (Medium)" }
                option value="19x19" selected[board_size == GameBoardSize::Large] { "19x19 (Large)" }
            }
            small class="form-text text-muted" { "Cannot be changed once snakes have registered" }
        }

        div class="form-group" {
            label for="match_style" { "Match Style" }
            select id="match_style" name="match_style" class="form-control" required {
                option value="single_game" selected[match_style == MatchStyle::SingleGame] { "Single Game" }
                option value="best_of_3" selected[match_style == MatchStyle::BestOf3] { "Best of 3" }
                option value="first_to_3" selected[match_style == MatchStyle::FirstTo3] { "First to 3" }
            }
        }

        div class="form-group" {
            label for="registration_status" { "Registration" }
            select id="registration_status" name="registration_status" class="form-control" required {
                option value="open" selected[registration_status == RegistrationStatus::Open] { "Open (anyone can register)" }
                option value="closed" selected[registration_status == RegistrationStatus::Closed] { "Closed (no registrations)" }
                option value="owner_only" selected[registration_status == RegistrationStatus::OwnerOnly] { "Owner Only" }
            }
        }

        div class="form-group" {
            label for="visibility" { "Visibility" }
            select id="visibility" name="visibility" class="form-control" required {
                option value="public" selected[visibility == TournamentVisibility::Public] { "Public" }
                option value="participants_only" selected[visibility == TournamentVisibility::ParticipantsOnly] { "Participants Only" }
            }
        }

        div class="form-group" {
            label for="max_snakes_per_user" { "Max Snakes per User" }
            input type="number" id="max_snakes_per_user" name="max_snakes_per_user"
                class="form-control" min="1" required value=(max_snakes_per_user) {}
        }

        div class="form-group" {
            label for="required_participants" { "Required Participants" }
            input type="number" id="required_participants" name="required_participants"
                class="form-control" min="2" required value=(required_participants) {}
        }
    }
}

/// Set a flash message and redirect. Shared tail for the POST handlers.
async fn flash_redirect(
    state: &AppState,
    session_id: Uuid,
    message: String,
    flash_type: &str,
    to: &str,
) -> ServerResult<Response, StatusCode> {
    session::set_flash_message(&state.db, session_id, message, flash_type)
        .await
        .wrap_err("Failed to set flash message")?;
    Ok(Redirect::to(to).into_response())
}

// --- Form payloads ---

/// Shared by POST /tournaments (create) and POST /tournaments/{id}/settings.
/// game_type/board_size arrive as strings and are validated via
/// parse_game_type/parse_board_size since those enums have catch-all variants.
#[derive(Debug, Deserialize)]
pub struct TournamentSettingsForm {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub game_type: String,
    pub board_size: String,
    pub match_style: MatchStyle,
    pub registration_status: RegistrationStatus,
    pub visibility: TournamentVisibility,
    pub max_snakes_per_user: i32,
    pub required_participants: i32,
}

impl TournamentSettingsForm {
    fn description_opt(&self) -> Option<String> {
        let trimmed = self.description.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterForm {
    pub battlesnake_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct UnregisterForm {
    pub registration_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct SeedForm {
    pub registration_id: Uuid,
    pub new_seed: i32,
}

#[derive(Debug, Deserialize)]
pub struct StatusForm {
    pub action: String,
}

#[derive(Debug, Deserialize)]
pub struct ImportLeaderboardForm {
    pub leaderboard_id: Uuid,
    pub count: i64,
}

// --- Handlers ---

/// GET /tournaments — public tournaments plus the viewer's own.
pub async fn list_tournaments(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let viewer_id = viewer.as_ref().map(|u| u.user_id);
    let tournaments = tournament::list_visible_tournaments(&state.db, viewer_id)
        .await
        .wrap_err("Failed to list tournaments")?;

    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        "Tournaments".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Tournaments" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                @if viewer.is_some() {
                    div style="margin-bottom: 20px;" {
                        a href="/tournaments/new" class="btn btn-primary" { "Create Tournament" }
                    }
                }

                @if tournaments.is_empty() {
                    div class="empty-state" {
                        p { "No tournaments yet." }
                    }
                } @else {
                    div class="tournaments-list" {
                        @for t in &tournaments {
                            div class="card" style="border: 1px solid #ddd; border-radius: 8px; padding: 20px; margin-bottom: 16px;" {
                                div class="d-flex justify-content-between align-items-center" {
                                    div {
                                        h2 style="margin-bottom: 4px;" {
                                            a href={"/tournaments/"(t.tournament_id)} { (t.name) }
                                        }
                                        span style="color: #666;" { "by " (t.owner_login) }
                                    }
                                    (status_badge(t.status))
                                }
                                div class="d-flex" style="gap: 24px; margin-top: 12px; flex-wrap: wrap; color: #444;" {
                                    span { strong { "Game: " } (t.game_type.as_str()) }
                                    span { strong { "Snakes: " } (t.registration_count) }
                                    span { strong { "Created: " } (t.created_at.format("%Y-%m-%d")) }
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
        flash,
    ))
}

/// GET /tournaments/new — creation form (auth required).
pub async fn new_tournament(
    CurrentUser(_): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        "Create Tournament".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Create Tournament" }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                form action="/tournaments" method="post" {
                    (tournament_form_fields(None))

                    div class="form-group" style="margin-top: 20px;" {
                        button type="submit" class="btn btn-primary" { "Create Tournament" }
                        a href="/tournaments" class="btn btn-secondary" { "Cancel" }
                    }
                }
            }
        }),
        flash,
    ))
}

/// POST /tournaments — create (auth required).
pub async fn create_tournament(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Form(form): Form<TournamentSettingsForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let parsed = validate_tournament_params(
        &form.name,
        form.required_participants,
        form.max_snakes_per_user,
    )
    .and_then(|()| {
        Ok((
            parse_game_type(&form.game_type)?,
            parse_board_size(&form.board_size)?,
        ))
    });

    let (game_type, board_size) = match parsed {
        Ok(values) => values,
        Err(message) => {
            return flash_redirect(
                &state,
                session.session_id,
                message,
                session::FLASH_TYPE_ERROR,
                "/tournaments/new",
            )
            .await;
        }
    };

    let created = tournament::create_tournament(
        &state.db,
        user.user_id,
        CreateTournament {
            name: form.name.trim().to_string(),
            description: form.description_opt(),
            game_type,
            board_size,
            registration_status: form.registration_status,
            visibility: form.visibility,
            match_style: form.match_style,
            max_snakes_per_user: form.max_snakes_per_user,
            required_participants: form.required_participants,
        },
    )
    .await
    .wrap_err("Failed to create tournament")?;

    flash_redirect(
        &state,
        session.session_id,
        "Tournament created successfully!".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &format!("/tournaments/{}", created.tournament_id),
    )
    .await
}

/// GET /tournaments/{id} — detail page.
#[allow(clippy::too_many_lines)]
pub async fn show_tournament(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path(tournament_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let registrations = tournament::get_registrations_with_details(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch registrations")?;

    let viewer_id = viewer.as_ref().map(|u| u.user_id);
    let participant_user_ids: Vec<Uuid> = registrations.iter().map(|r| r.user_id).collect();

    // participants_only tournaments 404 for outsiders (don't reveal existence)
    if !can_view(&t, viewer_id, &participant_user_ids) {
        return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }

    let owner = user::get_user_by_id(&state.db, t.user_id)
        .await
        .wrap_err("Failed to fetch tournament owner")?;
    let owner_login = owner
        .as_ref()
        .map(|o| o.github_login.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let is_owner = viewer_id == Some(t.user_id);

    // Snakes the viewer could register: their own, not yet in this tournament,
    // and only while they are under the per-user cap.
    let registerable_snakes = if let Some(u) = viewer.as_ref() {
        if can_register(&t, is_owner) {
            let user_reg_count = registrations
                .iter()
                .filter(|r| r.user_id == u.user_id)
                .count() as i32;
            if user_reg_count < t.max_snakes_per_user {
                let registered_ids: Vec<Uuid> =
                    registrations.iter().map(|r| r.battlesnake_id).collect();
                battlesnake::get_battlesnakes_by_user_id(&state.db, u.user_id)
                    .await
                    .wrap_err("Failed to fetch viewer's battlesnakes")?
                    .into_iter()
                    .filter(|s| !registered_ids.contains(&s.battlesnake_id))
                    .collect()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // Leaderboards for the owner's import form
    let leaderboards = if is_owner && registrations_editable(t.status) {
        leaderboard::get_all_leaderboards(&state.db)
            .await
            .wrap_err("Failed to fetch leaderboards")?
    } else {
        vec![]
    };

    let can_edit_registrations = registrations_editable(t.status);
    let max_seed = registrations.len();
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        format!("Tournament: {}", t.name),
        Box::new(html! {
            div class="container" {
                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                // Info header
                div class="card mb-4" {
                    div class="card-body" {
                        div class="d-flex justify-content-between align-items-center" {
                            div {
                                h1 class="mb-2" { (t.name) }
                                span style="color: #666;" { "by " (owner_login) }
                            }
                            (status_badge(t.status))
                        }
                        @if let Some(ref description) = t.description {
                            p style="margin-top: 12px;" { (description) }
                        }
                        div class="d-flex" style="gap: 24px; margin-top: 12px; flex-wrap: wrap; color: #444;" {
                            span { strong { "Game: " } (t.game_type.as_str()) }
                            span { strong { "Board: " } (t.board_size.as_str()) }
                            span { strong { "Match Style: " } (t.match_style.as_str()) }
                            span { strong { "Registration: " } (t.registration_status.as_str()) }
                            span { strong { "Visibility: " } (t.visibility.as_str()) }
                            span { strong { "Max Snakes/User: " } (t.max_snakes_per_user) }
                            span { strong { "Required Participants: " } (t.required_participants) }
                            span { strong { "Created: " } (t.created_at.format("%Y-%m-%d")) }
                        }
                    }
                }

                // Owner controls
                @if is_owner {
                    div class="card mb-4" {
                        div class="card-body" {
                            h3 { "Owner Controls" }
                            div class="d-flex" style="gap: 8px; flex-wrap: wrap; align-items: center;" {
                                @if t.status == TournamentStatus::Created {
                                    form action={"/tournaments/"(t.tournament_id)"/status"} method="post" style="display: inline;" {
                                        input type="hidden" name="action" value="open_registration";
                                        button type="submit" class="btn btn-success" { "Open Registration" }
                                    }
                                }
                                @if t.status.can_transition_to(TournamentStatus::Canceled) {
                                    form action={"/tournaments/"(t.tournament_id)"/status"} method="post" style="display: inline;" {
                                        input type="hidden" name="action" value="cancel";
                                        button type="submit" class="btn btn-danger"
                                            onclick="return confirm('Are you sure you want to cancel this tournament?');" { "Cancel Tournament" }
                                    }
                                }
                                @if can_edit_registrations {
                                    a href={"/tournaments/"(t.tournament_id)"/edit"} class="btn btn-primary" { "Edit Settings" }
                                }
                            }

                            @if !leaderboards.is_empty() {
                                div style="margin-top: 16px;" {
                                    h4 { "Import from Leaderboard" }
                                    p style="color: #666;" {
                                        "Register the top-ranked snakes from a leaderboard, seeded by rank."
                                    }
                                    form action={"/tournaments/"(t.tournament_id)"/import-leaderboard"} method="post"
                                        class="d-flex" style="gap: 8px; align-items: center; flex-wrap: wrap;" {
                                        select name="leaderboard_id" class="form-control" style="width: auto;" {
                                            @for lb in &leaderboards {
                                                option value=(lb.leaderboard_id) { (lb.name) }
                                            }
                                        }
                                        label for="import_count" { "Top" }
                                        input type="number" id="import_count" name="count" class="form-control"
                                            style="width: 90px;" min="1" max=(MAX_IMPORT_COUNT) value="8" {}
                                        button type="submit" class="btn btn-primary" { "Import" }
                                    }
                                }
                            }
                        }
                    }
                }

                // Registered snakes
                h2 { "Registered Snakes (" (registrations.len()) ")" }
                @if registrations.is_empty() {
                    p { "No snakes registered yet." }
                } @else {
                    table class="table" {
                        thead {
                            tr {
                                th { "Seed" }
                                th { "Snake" }
                                th { "Owner" }
                                @if can_edit_registrations && viewer.is_some() {
                                    th { "Actions" }
                                }
                            }
                        }
                        tbody {
                            @for reg in &registrations {
                                tr {
                                    td { (reg.seed) }
                                    td {
                                        a href={"/battlesnakes/"(reg.battlesnake_id)"/profile"} { (reg.snake_name) }
                                    }
                                    td { (reg.owner_login) }
                                    @if can_edit_registrations && viewer.is_some() {
                                        td class="actions" {
                                            @if is_owner {
                                                form action={"/tournaments/"(t.tournament_id)"/seed"} method="post"
                                                    style="display: inline-flex; gap: 4px; align-items: center;" {
                                                    input type="hidden" name="registration_id" value=(reg.registration_id);
                                                    input type="number" name="new_seed" class="form-control"
                                                        style="width: 80px;" min="1" max=(max_seed) value=(reg.seed) {}
                                                    button type="submit" class="btn btn-sm btn-secondary" { "Move" }
                                                }
                                            }
                                            @if is_owner || viewer_id == Some(reg.user_id) {
                                                form action={"/tournaments/"(t.tournament_id)"/unregister"} method="post" style="display: inline;" {
                                                    input type="hidden" name="registration_id" value=(reg.registration_id);
                                                    button type="submit" class="btn btn-sm btn-danger"
                                                        onclick="return confirm('Remove this snake from the tournament?');" { "Unregister" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Register form
                @if !registerable_snakes.is_empty() {
                    div class="card mb-4" style="margin-top: 20px;" {
                        div class="card-body" {
                            h3 { "Register a Snake" }
                            form action={"/tournaments/"(t.tournament_id)"/register"} method="post"
                                class="d-flex" style="gap: 8px; align-items: center;" {
                                select name="battlesnake_id" class="form-control" style="width: auto;" {
                                    @for snake in &registerable_snakes {
                                        option value=(snake.battlesnake_id) { (snake.name) }
                                    }
                                }
                                button type="submit" class="btn btn-primary" { "Register" }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/tournaments" { "Back to Tournaments" }
                    span { " | " }
                    a href="/" { "Home" }
                }
            }
        }),
        flash,
    ))
}

/// GET /tournaments/{id}/edit — settings form (owner only).
pub async fn edit_tournament(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(tournament_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        return Err("You don't have permission to edit this tournament".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let registration_count = tournament::count_registrations(&state.db, tournament_id)
        .await
        .wrap_err("Failed to count registrations")?;

    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        format!("Edit Tournament: {}", t.name),
        Box::new(html! {
            div class="container" {
                h1 { "Edit Tournament: " (t.name) }

                @if let Some(message) = flash.message() {
                    div class=(flash.class()) {
                        p { (message) }
                    }
                }

                @if registration_count > 0 {
                    div class="alert alert-info" {
                        p {
                            (registration_count) " snake(s) are registered — game type and board size can no longer be changed."
                        }
                    }
                }

                form action={"/tournaments/"(tournament_id)"/settings"} method="post" {
                    (tournament_form_fields(Some(&t)))

                    div class="form-group" style="margin-top: 20px;" {
                        button type="submit" class="btn btn-primary" { "Update Tournament" }
                        a href={"/tournaments/"(tournament_id)} class="btn btn-secondary" { "Cancel" }
                    }
                }
            }
        }),
        flash,
    ))
}

/// POST /tournaments/{id}/settings — update settings (owner only).
pub async fn update_settings(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<TournamentSettingsForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        return Err("You don't have permission to edit this tournament".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let edit_url = format!("/tournaments/{tournament_id}/edit");

    let registration_count = tournament::count_registrations(&state.db, tournament_id)
        .await
        .wrap_err("Failed to count registrations")?;

    let parsed = validate_tournament_params(
        &form.name,
        form.required_participants,
        form.max_snakes_per_user,
    )
    .and_then(|()| {
        Ok((
            parse_game_type(&form.game_type)?,
            parse_board_size(&form.board_size)?,
        ))
    })
    .and_then(|(game_type, board_size)| {
        validate_settings_update(&t, registration_count > 0, &game_type, &board_size)?;
        Ok((game_type, board_size))
    });

    let (game_type, board_size) = match parsed {
        Ok(values) => values,
        Err(message) => {
            return flash_redirect(
                &state,
                session.session_id,
                message,
                session::FLASH_TYPE_ERROR,
                &edit_url,
            )
            .await;
        }
    };

    tournament::update_tournament_settings(
        &state.db,
        tournament_id,
        UpdateTournamentSettings {
            name: form.name.trim().to_string(),
            description: form.description_opt(),
            game_type,
            board_size,
            match_style: form.match_style,
            registration_status: form.registration_status,
            visibility: form.visibility,
            max_snakes_per_user: form.max_snakes_per_user,
            required_participants: form.required_participants,
        },
    )
    .await
    .wrap_err("Failed to update tournament settings")?;

    flash_redirect(
        &state,
        session.session_id,
        "Tournament settings updated!".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &format!("/tournaments/{tournament_id}"),
    )
    .await
}

/// POST /tournaments/{id}/register — register one of the caller's snakes.
pub async fn register_snake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<RegisterForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let detail_url = format!("/tournaments/{tournament_id}");
    let is_owner = t.user_id == user.user_id;

    if !can_register(&t, is_owner) {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration is not open for this tournament".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let snake = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")?;

    let Some(snake) = snake.filter(|s| s.user_id == user.user_id) else {
        return flash_redirect(
            &state,
            session.session_id,
            "You can only register your own battlesnakes".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    };

    if tournament::is_battlesnake_registered(&state.db, tournament_id, snake.battlesnake_id)
        .await
        .wrap_err("Failed to check existing registration")?
    {
        return flash_redirect(
            &state,
            session.session_id,
            format!("{} is already registered in this tournament", snake.name),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let user_reg_count =
        tournament::count_registrations_for_user(&state.db, tournament_id, user.user_id)
            .await
            .wrap_err("Failed to count user registrations")?;

    if user_reg_count >= i64::from(t.max_snakes_per_user) {
        return flash_redirect(
            &state,
            session.session_id,
            format!(
                "You have reached the limit of {} snake(s) for this tournament",
                t.max_snakes_per_user
            ),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let registration = tournament::register_snake_with_next_seed(
        &state.db,
        tournament_id,
        snake.battlesnake_id,
        user.user_id,
    )
    .await
    .wrap_err("Failed to register snake")?;

    flash_redirect(
        &state,
        session.session_id,
        format!("Registered {} (seed {})", snake.name, registration.seed),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/unregister — remove a registration (snake owner or
/// tournament owner) and renumber remaining seeds.
pub async fn unregister_snake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<UnregisterForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let detail_url = format!("/tournaments/{tournament_id}");

    if !registrations_editable(t.status) {
        return flash_redirect(
            &state,
            session.session_id,
            "Snakes can no longer be unregistered from this tournament".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let registration = tournament::get_registration_by_id(&state.db, form.registration_id)
        .await
        .wrap_err("Failed to fetch registration")?
        .filter(|r| r.tournament_id == tournament_id);

    let Some(registration) = registration else {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration not found".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    };

    let is_tournament_owner = t.user_id == user.user_id;
    let is_snake_owner = registration.user_id == user.user_id;
    if !is_tournament_owner && !is_snake_owner {
        return flash_redirect(
            &state,
            session.session_id,
            "You don't have permission to remove this registration".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    tournament::delete_registration_and_renumber(
        &state.db,
        tournament_id,
        registration.registration_id,
    )
    .await
    .wrap_err("Failed to unregister snake")?;

    flash_redirect(
        &state,
        session.session_id,
        "Snake unregistered".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/seed — move a registration to a new seed (owner only).
pub async fn move_seed(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<SeedForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        return Err("Only the tournament owner can change seeds".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    if !registrations_editable(t.status) {
        return flash_redirect(
            &state,
            session.session_id,
            "Seeds can only be changed before the tournament starts".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let registration = tournament::get_registration_by_id(&state.db, form.registration_id)
        .await
        .wrap_err("Failed to fetch registration")?
        .filter(|r| r.tournament_id == tournament_id);

    if registration.is_none() {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration not found".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    tournament::move_registration_seed(
        &state.db,
        tournament_id,
        form.registration_id,
        form.new_seed,
    )
    .await
    .wrap_err("Failed to move seed")?;

    flash_redirect(
        &state,
        session.session_id,
        "Seed updated".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/status — lifecycle transitions (owner only).
///
/// NOTE: `start` (registration -> in_progress) is intentionally not
/// implemented here — bracket generation lands in a separate PR.
pub async fn update_status(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<StatusForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        return Err("Only the tournament owner can change its status".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    let next_status = match form.action.as_str() {
        "open_registration" => TournamentStatus::Registration,
        "cancel" => TournamentStatus::Canceled,
        other => {
            return flash_redirect(
                &state,
                session.session_id,
                format!("Unknown action: {other}"),
                session::FLASH_TYPE_ERROR,
                &detail_url,
            )
            .await;
        }
    };

    if !t.status.can_transition_to(next_status) {
        return flash_redirect(
            &state,
            session.session_id,
            format!(
                "Cannot move tournament from {} to {}",
                t.status.as_str(),
                next_status.as_str()
            ),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    tournament::set_tournament_status(&state.db, tournament_id, next_status)
        .await
        .wrap_err("Failed to update tournament status")?;

    let message = match next_status {
        TournamentStatus::Registration => "Registration is now open!".to_string(),
        TournamentStatus::Canceled => "Tournament canceled".to_string(),
        _ => format!("Tournament moved to {}", next_status.as_str()),
    };

    flash_redirect(
        &state,
        session.session_id,
        message,
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/import-leaderboard — the "leaderboards feed
/// tournaments" qualifier flow. Registers the top N ranked snakes that aren't
/// already registered, respecting max_snakes_per_user, seeded in rank order
/// after existing registrations.
#[allow(clippy::too_many_lines)]
pub async fn import_leaderboard(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<ImportLeaderboardForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        return Err("Only the tournament owner can import from a leaderboard".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    if !registrations_editable(t.status) {
        return flash_redirect(
            &state,
            session.session_id,
            "Snakes can only be imported before the tournament starts".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let Some(lb) = leaderboard::get_leaderboard_by_id(&state.db, form.leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")?
    else {
        return flash_redirect(
            &state,
            session.session_id,
            "Leaderboard not found".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    };

    let count = form.count.clamp(1, MAX_IMPORT_COUNT);

    let ranked = leaderboard::get_ranked_entries(
        &state.db,
        lb.leaderboard_id,
        leaderboard::LeaderboardSort::Rating,
    )
    .await
    .wrap_err("Failed to fetch ranked leaderboard entries")?;

    // Snapshot the current registrations so we can skip duplicates and enforce
    // the per-user cap while walking down the rankings.
    let existing = tournament::get_registrations_for_tournament(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch existing registrations")?;
    let mut registered_snakes: Vec<Uuid> = existing.iter().map(|r| r.battlesnake_id).collect();
    let mut per_user_counts: HashMap<Uuid, i64> = HashMap::new();
    for reg in &existing {
        *per_user_counts.entry(reg.user_id).or_insert(0) += 1;
    }

    // Select candidates in rank order: skip already-registered snakes and
    // owners at their limit; continue until N selected or rankings exhausted.
    let mut candidates: Vec<(Uuid, Uuid)> = Vec::new(); // (battlesnake_id, user_id)
    for entry in &ranked {
        if candidates.len() as i64 >= count {
            break;
        }
        if registered_snakes.contains(&entry.battlesnake_id) {
            continue;
        }
        let Some(snake) = battlesnake::get_battlesnake_by_id(&state.db, entry.battlesnake_id)
            .await
            .wrap_err("Failed to fetch ranked battlesnake")?
        else {
            continue;
        };
        let owner_count = per_user_counts.entry(snake.user_id).or_insert(0);
        if *owner_count >= i64::from(t.max_snakes_per_user) {
            continue;
        }
        *owner_count += 1;
        registered_snakes.push(snake.battlesnake_id);
        candidates.push((snake.battlesnake_id, snake.user_id));
    }

    if candidates.is_empty() {
        return flash_redirect(
            &state,
            session.session_id,
            format!("No eligible snakes to import from {}", lb.name),
            session::FLASH_TYPE_INFO,
            &detail_url,
        )
        .await;
    }

    // Register all selected snakes in one transaction, appended after the
    // existing registrations in rating-rank order.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin import transaction")?;
    let imported = candidates.len();
    for (battlesnake_id, owner_user_id) in candidates {
        let seed = tournament::next_seed(&mut *tx, tournament_id)
            .await
            .wrap_err("Failed to compute seed during import")?;
        tournament::create_registration(
            &mut *tx,
            tournament_id,
            battlesnake_id,
            owner_user_id,
            seed,
        )
        .await
        .wrap_err("Failed to register imported snake")?;
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit import transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        format!("Imported {imported} snake(s) from {}", lb.name),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tournament(
        status: TournamentStatus,
        registration_status: RegistrationStatus,
        visibility: TournamentVisibility,
    ) -> Tournament {
        Tournament {
            tournament_id: Uuid::new_v4(),
            name: "Test Tournament".to_string(),
            description: None,
            user_id: Uuid::new_v4(),
            game_type: GameType::Standard,
            board_size: GameBoardSize::Medium,
            registration_status,
            visibility,
            status,
            match_style: MatchStyle::SingleGame,
            max_snakes_per_user: 1,
            required_participants: 2,
            current_round: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn registrations_editable_only_before_start() {
        assert!(registrations_editable(TournamentStatus::Created));
        assert!(registrations_editable(TournamentStatus::Registration));
        assert!(!registrations_editable(TournamentStatus::InProgress));
        assert!(!registrations_editable(TournamentStatus::Completed));
        assert!(!registrations_editable(TournamentStatus::Canceled));
    }

    #[test]
    fn can_register_permission_matrix() {
        use RegistrationStatus::{Closed, Open, OwnerOnly};
        use TournamentStatus::{Canceled, Completed, Created, InProgress, Registration};

        // (status, registration_status, is_owner, expected)
        let cases = [
            // Open: anyone, but only while created/registration
            (Created, Open, false, true),
            (Created, Open, true, true),
            (Registration, Open, false, true),
            (Registration, Open, true, true),
            (InProgress, Open, false, false),
            (InProgress, Open, true, false),
            (Completed, Open, true, false),
            (Canceled, Open, true, false),
            // OwnerOnly: only the owner, still gated by status
            (Created, OwnerOnly, false, false),
            (Created, OwnerOnly, true, true),
            (Registration, OwnerOnly, false, false),
            (Registration, OwnerOnly, true, true),
            (InProgress, OwnerOnly, true, false),
            (Canceled, OwnerOnly, true, false),
            // Closed: nobody, not even the owner
            (Created, Closed, false, false),
            (Created, Closed, true, false),
            (Registration, Closed, false, false),
            (Registration, Closed, true, false),
            (InProgress, Closed, true, false),
        ];

        for (status, registration_status, is_owner, expected) in cases {
            let t = test_tournament(status, registration_status, TournamentVisibility::Public);
            assert_eq!(
                can_register(&t, is_owner),
                expected,
                "status={status:?} registration_status={registration_status:?} is_owner={is_owner}"
            );
        }
    }

    #[test]
    fn can_view_public_tournaments() {
        let t = test_tournament(
            TournamentStatus::Created,
            RegistrationStatus::Open,
            TournamentVisibility::Public,
        );
        assert!(can_view(&t, None, &[]));
        assert!(can_view(&t, Some(Uuid::new_v4()), &[]));
    }

    #[test]
    fn can_view_participants_only_tournaments() {
        let t = test_tournament(
            TournamentStatus::Created,
            RegistrationStatus::Open,
            TournamentVisibility::ParticipantsOnly,
        );
        let participant = Uuid::new_v4();
        let stranger = Uuid::new_v4();

        // Anonymous and non-participants are denied
        assert!(!can_view(&t, None, &[participant]));
        assert!(!can_view(&t, Some(stranger), &[participant]));

        // The owner and registered participants can view
        assert!(can_view(&t, Some(t.user_id), &[participant]));
        assert!(can_view(&t, Some(participant), &[participant]));
    }

    #[test]
    fn validate_tournament_params_rules() {
        assert!(validate_tournament_params("Snake Cup", 2, 1).is_ok());
        assert!(validate_tournament_params("Snake Cup", 8, 3).is_ok());

        assert!(validate_tournament_params("", 2, 1).is_err());
        assert!(validate_tournament_params("   ", 2, 1).is_err());
        assert!(validate_tournament_params("Snake Cup", 1, 1).is_err());
        assert!(validate_tournament_params("Snake Cup", 0, 1).is_err());
        assert!(validate_tournament_params("Snake Cup", 2, 0).is_err());
        assert!(validate_tournament_params("Snake Cup", 2, -1).is_err());
    }

    #[test]
    fn settings_update_blocked_after_start() {
        for status in [
            TournamentStatus::InProgress,
            TournamentStatus::Completed,
            TournamentStatus::Canceled,
        ] {
            let t = test_tournament(
                status,
                RegistrationStatus::Open,
                TournamentVisibility::Public,
            );
            assert!(
                validate_settings_update(&t, false, &t.game_type.clone(), &t.board_size.clone())
                    .is_err(),
                "settings should be locked in status {status:?}"
            );
        }
    }

    #[test]
    fn settings_update_freezes_game_config_once_registered() {
        let t = test_tournament(
            TournamentStatus::Registration,
            RegistrationStatus::Open,
            TournamentVisibility::Public,
        );

        // No registrations: everything editable
        assert!(
            validate_settings_update(&t, false, &GameType::Royale, &GameBoardSize::Large).is_ok()
        );

        // With registrations: game_type/board_size are frozen
        assert!(
            validate_settings_update(&t, true, &GameType::Royale, &GameBoardSize::Medium).is_err()
        );
        assert!(
            validate_settings_update(&t, true, &GameType::Standard, &GameBoardSize::Large).is_err()
        );

        // With registrations but unchanged game config: fine
        assert!(
            validate_settings_update(&t, true, &GameType::Standard, &GameBoardSize::Medium).is_ok()
        );
    }

    #[test]
    fn parse_game_type_accepts_dropdown_values_only() {
        assert_eq!(parse_game_type("Standard").unwrap(), GameType::Standard);
        assert_eq!(parse_game_type("Royale").unwrap(), GameType::Royale);
        assert_eq!(
            parse_game_type("Constrictor").unwrap(),
            GameType::Constrictor
        );
        assert_eq!(parse_game_type("Snail Mode").unwrap(), GameType::SnailMode);
        assert!(parse_game_type("Wrapped").is_err());
        assert!(parse_game_type("").is_err());
    }

    #[test]
    fn parse_board_size_accepts_dropdown_values_only() {
        assert_eq!(parse_board_size("7x7").unwrap(), GameBoardSize::Small);
        assert_eq!(parse_board_size("11x11").unwrap(), GameBoardSize::Medium);
        assert_eq!(parse_board_size("19x19").unwrap(), GameBoardSize::Large);
        assert!(parse_board_size("25x25").is_err());
        assert!(parse_board_size("").is_err());
    }
}
