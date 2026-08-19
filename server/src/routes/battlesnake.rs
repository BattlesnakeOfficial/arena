use axum::{
    extract::{Path, Query, RawForm, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    components::{page_factory::PageFactory, snake_tags::snake_tag_chips},
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::battlesnake::{self, CreateBattlesnake, UpdateBattlesnake, Visibility},
    models::game_battlesnake,
    models::leaderboard,
    models::session,
    models::snake_health_status,
    models::tag,
    models::tournament,
    models::user::get_user_by_id,
    routes::auth::{CurrentUser, CurrentUserWithSession, OptionalUser},
    routes::pagination::resolve_page,
    snake_health,
    state::AppState,
};

// Parsed new/edit battlesnake form. Parsed by hand from the urlencoded body
// because the tag checkboxes submit a repeated `tags` key, which
// `axum::Form` (serde_urlencoded) can't deserialize into a Vec.
struct BattlesnakeFormData {
    name: String,
    url: String,
    visibility: Visibility,
    tag_ids: Vec<Uuid>,
}

fn parse_battlesnake_form(bytes: &[u8]) -> Result<BattlesnakeFormData, String> {
    let mut name = None;
    let mut url = None;
    let mut visibility = None;
    let mut tag_ids = Vec::new();

    for (key, value) in url::form_urlencoded::parse(bytes) {
        match key.as_ref() {
            "name" => name = Some(value.into_owned()),
            "url" => url = Some(value.into_owned()),
            "visibility" => {
                visibility = Some(
                    value
                        .parse::<Visibility>()
                        .map_err(|_| format!("Invalid visibility: {value}"))?,
                );
            }
            "tags" => tag_ids
                .push(Uuid::parse_str(&value).map_err(|_| "Invalid tag selection".to_string())?),
            _ => {}
        }
    }

    Ok(BattlesnakeFormData {
        name: name
            .filter(|n| !n.is_empty())
            .ok_or_else(|| "Name is required".to_string())?,
        url: url
            .filter(|u| !u.is_empty())
            .ok_or_else(|| "URL is required".to_string())?,
        visibility: visibility.ok_or_else(|| "Visibility is required".to_string())?,
        tag_ids,
    })
}

// One category's worth of tag checkboxes for the new/edit forms
fn tag_checkbox_group(title: &str, tags: &[tag::Tag], selected: &[Uuid]) -> Markup {
    html! {
        div {
            strong { (title) }
            div style="display:flex; flex-wrap:wrap; gap:6px 16px; margin:6px 0 12px;" {
                @for t in tags {
                    label style="display:inline-flex; align-items:center; gap:6px; font-weight:normal; margin:0;" {
                        input type="checkbox" name="tags" value=(t.tag_id)
                            checked[selected.contains(&t.tag_id)];
                        (t.name)
                    }
                }
            }
        }
    }
}

// Shared tag picker for the new + edit battlesnake forms: checkbox chips
// from the curated tag catalog, grouped by category. Multiple selections in
// the same category are fine (e.g. a snake written in two languages).
fn tag_form_fields(catalog: &tag::TagCatalog, selected: &[Uuid]) -> Markup {
    html! {
        div class="field" {
            label { "Tags" }
            (tag_checkbox_group("Language", &catalog.languages, selected))
            (tag_checkbox_group("Platform", &catalog.platforms, selected))
            p class="help" {
                "Pick up to " (tag::MAX_TAGS_PER_SNAKE)
                " tags — choosing several from one category is fine if your snake uses more than one. "
                "Missing a tag? "
                a href="/discord" { "Request it on Discord" }
                "."
            }
        }
    }
}

/// Rows per page in the public `/snakes` directory.
const PUBLIC_SNAKES_PER_PAGE: i64 = 50;

#[derive(Debug, serde::Deserialize)]
pub struct PublicBattlesnakePagination {
    #[serde(default)]
    pub page: Option<i64>,
}

struct PublicBattlesnakePage {
    snakes: Vec<battlesnake::PublicBattlesnakeListItem>,
    page: i64,
    total_pages: i64,
    total: i64,
}

/// Count public snakes, resolve the requested page against that count, and
/// fetch the matching batch.
async fn load_public_battlesnake_page(
    pool: &PgPool,
    requested: Option<i64>,
) -> cja::Result<PublicBattlesnakePage> {
    let total = battlesnake::count_public_battlesnakes(pool).await?;
    let (page, total_pages) = resolve_page(requested, total, PUBLIC_SNAKES_PER_PAGE);
    let snakes =
        battlesnake::get_public_battlesnakes_paginated(pool, page, PUBLIC_SNAKES_PER_PAGE).await?;

    Ok(PublicBattlesnakePage {
        snakes,
        page,
        total_pages,
        total,
    })
}

fn render_public_battlesnake_list(
    snakes: &[battlesnake::PublicBattlesnakeListItem],
    is_authenticated: bool,
    page: i64,
    total_pages: i64,
    total: i64,
) -> Markup {
    html! {
        div class="page-head" {
            h1 { "Public Battlesnakes" }
            div class="sub" {
                "Every snake its owner has made public. Pick one and challenge it to a match."
            }
        }

        @if total == 0 {
            p class="empty" { "No public battlesnakes are available yet." }
        } @else {
            div class="section" {
                @if snakes.is_empty() {
                    // The count and the page fetch can race (a snake going
                    // private between them). Keep the pager so the visitor
                    // can step back to a page that still has rows.
                    p class="empty" { "No public battlesnakes remain on this page." }
                } @else {
                    table class="data" {
                        thead {
                            tr {
                                th { "Battlesnake" }
                                th class="r" { "Actions" }
                            }
                        }
                        tbody {
                            @for snake in snakes {
                                tr {
                                    td {
                                        div class="snake-cell" {
                                            span class="chip" style={"background:"(chip_color(&snake.color))} {}
                                            span {
                                                a class="name" href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} {
                                                    (snake.name)
                                                }
                                                span class="owner" {
                                                    "by "
                                                    a href={"/users/"(snake.owner_login)} { (snake.owner_login) }
                                                }
                                            }
                                        }
                                    }
                                    td class="r" {
                                        div class="row-actions" {
                                            @if is_authenticated {
                                                form action={"/battlesnakes/"(snake.battlesnake_id)"/challenge"} method="post" {
                                                    button type="submit" class="btn sm solid" { "Challenge" }
                                                }
                                            } @else {
                                                a href="/auth/github" class="btn sm" { "Sign in to challenge" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div class="pager" {
                    @if page > 0 {
                        a href={"/snakes?page="(page - 1)} { "‹ Prev" }
                    }
                    @if total_pages > 1 {
                        span class="cur" { "Page " (page + 1) " of " (total_pages) }
                    }
                    @if page < total_pages - 1 {
                        a href={"/snakes?page="(page + 1)} { "Next ›" }
                    }
                    @if !snakes.is_empty() {
                        span class="spacer" {}
                        span {
                            "Showing " (page * PUBLIC_SNAKES_PER_PAGE + 1)
                            "–" (page * PUBLIC_SNAKES_PER_PAGE + snakes.len() as i64)
                            " of " (total) " public snakes"
                        }
                    }
                }
            }
        }
    }
}

/// GET /snakes — browse public battlesnakes.
pub async fn list_public_battlesnakes(
    State(state): State<AppState>,
    Query(pagination): Query<PublicBattlesnakePagination>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let PublicBattlesnakePage {
        snakes,
        page,
        total_pages,
        total,
    } = load_public_battlesnake_page(&state.db, pagination.page)
        .await
        .wrap_err("Failed to load public battlesnakes page")?;

    // Captured before `page_factory` is consumed by `create_page`.
    let is_authenticated = page_factory.user.is_some();

    Ok(page_factory
        .create_page(
            "Public Battlesnakes".to_string(),
            Box::new(render_public_battlesnake_list(
                &snakes,
                is_authenticated,
                page,
                total_pages,
                total,
            )),
        )
        .with_description("Browse public Battlesnakes and challenge an opponent to a match."))
}

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
            div class="crumb" { a href="/me" { "My Profile" } " / battlesnakes" }
            div class="page-head" {
                h1 { "Your Battlesnakes" }
                div class="sub" { "The snake servers the Arena calls when your games run." }
                div class="head-actions" {
                    a href="/battlesnakes/new" class="btn solid" { "Add New Battlesnake" }
                }
            }

            @if battlesnakes.is_empty() {
                p class="empty" { "You don't have any battlesnakes yet." }
            } @else {
                div class="section" {
                    table class="data" {
                        thead {
                            tr {
                                th { "Snake" }
                                th class="hide-sm" { "URL" }
                                th { "Visibility" }
                                th class="r" { "Actions" }
                            }
                        }
                        tbody {
                            @for snake in &battlesnakes {
                                tr {
                                    td {
                                        div class="snake-cell" {
                                            span class="chip" style={"background:" (chip_color(&snake.color))} {}
                                            a class="name" href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} { (snake.name) }
                                        }
                                    }
                                    td class="url-cell hide-sm" {
                                        a href=(snake.url) target="_blank" rel="noopener" { (snake.url) }
                                    }
                                    td {
                                        @if snake.visibility == Visibility::Public {
                                            span class="badge ok" { "Public" }
                                        } @else {
                                            span class="badge" { "Private" }
                                        }
                                    }
                                    td class="r" {
                                        div class="row-actions" {
                                            form action={"/battlesnakes/"(snake.battlesnake_id)"/test"} method="post" {
                                                button type="submit" class="btn sm" { "Test" }
                                            }
                                            a href={"/battlesnakes/"(snake.battlesnake_id)"/edit"} class="btn sm" { "Edit" }
                                            form action={"/battlesnakes/"(snake.battlesnake_id)"/delete"} method="post" {
                                                button type="submit" class="btn sm danger" onclick="return confirm('Are you sure you want to delete this battlesnake?');" { "Delete" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
        flash,
    ))
}

// Show the form to create a new battlesnake
pub async fn new_battlesnake(
    State(state): State<AppState>,
    CurrentUser(_): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let catalog = tag::get_tag_catalog(&state.db)
        .await
        .wrap_err("Failed to get tag catalog")?;

    // Use flash from page_factory (already extracted and cleared from DB)
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        "Add New Battlesnake".to_string(),
        Box::new(html! {
            div class="crumb" { a href="/battlesnakes" { "Your Battlesnakes" } " / new" }
            div class="page-head" {
                h1 { "Add New Battlesnake" }
                div class="sub" { "Point the Arena at your snake server and pick who can play it." }
            }

            form class="form-stack" action="/battlesnakes" method="post" {
                div class="field" {
                    label for="name" { "Name" }
                    input type="text" id="name" name="name" required;
                }

                div class="field" {
                    label for="url" { "URL" }
                    input type="url" id="url" name="url" required placeholder="https://your-battlesnake-server.com";
                    p class="help" { "The URL of your Battlesnake server" }
                }

                div class="field" {
                    label for="visibility" { "Visibility" }
                    select id="visibility" name="visibility" required {
                        option value="public" selected { "Public (Available to all users)" }
                        option value="private" { "Private (Only available to you)" }
                    }
                    p class="help" { "Control who can add this snake to games" }
                }

                (tag_form_fields(&catalog, &[]))

                // The url input rejects bare hostnames before the server can help,
                // so add the scheme as soon as the field loses focus.
                script {
                    (maud::PreEscaped(r#"
                    (function() {
                      var el = document.getElementById('url');
                      el.addEventListener('change', function() {
                        var v = el.value.trim();
                        if (v && v.indexOf('://') === -1) {
                          el.value = 'https://' + v;
                        }
                      });
                    })();
                    "#))
                }

                div class="form-cta" {
                    button type="submit" class="btn solid" { "Create Battlesnake" }
                    a href="/battlesnakes" class="btn" { "Cancel" }
                }
            }
        }),
        flash,
    ))
}

// Handle the creation of a new battlesnake
/// Users regularly paste a bare hostname ("mysnake.fly.dev"). Assume https
/// instead of bouncing them off the form with a URL validation error.
fn normalize_snake_url(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() || url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

pub async fn create_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    RawForm(form_bytes): RawForm,
) -> ServerResult<impl IntoResponse, StatusCode> {
    tracing::info!(
        "create_battlesnake: session_id={}, user_id={}, has_flash={:?}",
        session.session_id,
        user.user_id,
        session.flash_message.is_some()
    );

    let form = parse_battlesnake_form(&form_bytes).with_status(StatusCode::BAD_REQUEST)?;

    // Enforce the tag cap before creating anything
    if form.tag_ids.len() > tag::MAX_TAGS_PER_SNAKE {
        session::set_flash_message(
            &state.db,
            session.session_id,
            format!(
                "A battlesnake can have at most {} tags",
                tag::MAX_TAGS_PER_SNAKE
            ),
            session::FLASH_TYPE_ERROR,
        )
        .await
        .wrap_err("Failed to set flash message")?;

        return Ok(Redirect::to("/battlesnakes/new").into_response());
    }

    let create_data = CreateBattlesnake {
        name: form.name,
        url: normalize_snake_url(&form.url),
        visibility: form.visibility,
    };

    // Create the new battlesnake in the database
    let battlesnake_result =
        battlesnake::create_battlesnake(&state.db, user.user_id, create_data.clone()).await;

    match battlesnake_result {
        Ok(snake) => {
            tag::set_tags_for_battlesnake(&state.db, snake.battlesnake_id, &form.tag_ids)
                .await
                .wrap_err("Failed to set battlesnake tags")?;

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

    let catalog = tag::get_tag_catalog(&state.db)
        .await
        .wrap_err("Failed to get tag catalog")?;
    let selected_tag_ids: Vec<Uuid> = tag::get_tags_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake tags")?
        .iter()
        .map(|t| t.tag_id)
        .collect();

    // Use flash from page_factory (already extracted and cleared from DB)
    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        format!("Edit Battlesnake: {}", battlesnake.name),
        Box::new(html! {
            div class="crumb" { a href="/battlesnakes" { "Your Battlesnakes" } " / edit" }
            div class="page-head" {
                h1 { "Edit Battlesnake: " (battlesnake.name) }
            }

            form class="form-stack" action={"/battlesnakes/"(battlesnake_id)"/update"} method="post" {
                div class="field" {
                    label for="name" { "Name" }
                    input type="text" id="name" name="name" required value=(battlesnake.name);
                }

                div class="field" {
                    label for="url" { "URL" }
                    input type="url" id="url" name="url" required value=(battlesnake.url);
                    p class="help" { "The URL of your Battlesnake server" }
                }

                div class="field" {
                    label for="visibility" { "Visibility" }
                    select id="visibility" name="visibility" required {
                        option value="public" selected[battlesnake.visibility == Visibility::Public] { "Public (Available to all users)" }
                        option value="private" selected[battlesnake.visibility == Visibility::Private] { "Private (Only available to you)" }
                    }
                    p class="help" { "Control who can add this snake to games" }
                }

                (tag_form_fields(&catalog, &selected_tag_ids))

                // The url input rejects bare hostnames before the server can help,
                // so add the scheme as soon as the field loses focus.
                script {
                    (maud::PreEscaped(r#"
                    (function() {
                      var el = document.getElementById('url');
                      el.addEventListener('change', function() {
                        var v = el.value.trim();
                        if (v && v.indexOf('://') === -1) {
                          el.value = 'https://' + v;
                        }
                      });
                    })();
                    "#))
                }

                div class="form-cta" {
                    button type="submit" class="btn solid" { "Update Battlesnake" }
                    a href="/battlesnakes" class="btn" { "Cancel" }
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
    RawForm(form_bytes): RawForm,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // First check if the battlesnake exists and belongs to the user
    let exists = battlesnake::belongs_to_user(&state.db, battlesnake_id, user.user_id)
        .await
        .wrap_err("Failed to check battlesnake ownership")?;

    if !exists {
        return Err("Battlesnake not found or you don't have permission to update it".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let form = parse_battlesnake_form(&form_bytes).with_status(StatusCode::BAD_REQUEST)?;

    // Enforce the tag cap before writing anything
    if form.tag_ids.len() > tag::MAX_TAGS_PER_SNAKE {
        session::set_flash_message(
            &state.db,
            session.session_id,
            format!(
                "A battlesnake can have at most {} tags",
                tag::MAX_TAGS_PER_SNAKE
            ),
            session::FLASH_TYPE_ERROR,
        )
        .await
        .wrap_err("Failed to set flash message")?;

        return Ok(Redirect::to(&format!("/battlesnakes/{battlesnake_id}/edit")).into_response());
    }

    let update_data = UpdateBattlesnake {
        name: form.name,
        url: normalize_snake_url(&form.url),
        visibility: form.visibility,
    };

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
            tag::set_tags_for_battlesnake(&state.db, battlesnake_id, &form.tag_ids)
                .await
                .wrap_err("Failed to set battlesnake tags")?;

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
    use crate::models::game::{GameStatus, GameType};

    let total_games = history.len();
    let mut finished_games = 0usize;
    let mut wins = 0usize;
    let mut second_places = 0usize;
    let mut third_places = 0usize;
    let mut fourth_places = 0usize;
    let mut placement_sum = 0i64;
    let mut placement_count = 0usize;

    for entry in history {
        // Solo games are single-snake survival runs: placement is always 1 by
        // construction, so counting them would make every Solo run a free win.
        // They stay in `total_games` and the history table; they don't feed the
        // competitive accumulators.
        if entry.game_type == GameType::Solo {
            continue;
        }

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

// View a battlesnake's profile with game history and stats.
// Public to everyone: visibility only controls whether a snake can be
// matchmade against, not who can see it.
#[allow(clippy::too_many_lines)]
pub async fn view_battlesnake_profile(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    Path(battlesnake_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Fetch the battlesnake
    let snake = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake")?
        .ok_or_else(|| "Battlesnake not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let is_owner = user.as_ref().is_some_and(|u| u.user_id == snake.user_id);

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

    // Curated language/platform tags for this snake
    let snake_tags = tag::get_tags_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake tags")?;

    let flash = page_factory.flash.clone();

    // Compute stats
    let stats = compute_stats(&history);

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
                                    @if owner.is_some() {
                                        a href={"/users/"(owner_login)} { (owner_login) }
                                    } @else {
                                        span { (owner_login) }
                                    }
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
                                (snake_tag_chips(&snake_tags))
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
                    @if owner.is_some() {
                        a href={"/users/"(owner_login)} class="btn btn-secondary ms-2" { "Owner Profile" }
                    }
                    @if user.is_some() {
                        a href="/me" class="btn btn-secondary ms-2" { "My Profile" }
                    }
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
                                        span class="badge ok" { "OK" }
                                    } @else {
                                        span class="badge warn" { "Failed" }
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

#[cfg(test)]
mod public_list_tests {
    use super::*;
    use battlesnake::PublicBattlesnakeListItem;

    fn item(name: &str, owner: &str) -> PublicBattlesnakeListItem {
        PublicBattlesnakeListItem {
            battlesnake_id: Uuid::nil(),
            name: name.to_string(),
            color: "#ff0000".to_string(),
            owner_login: owner.to_string(),
        }
    }

    // Page-number clamping is covered by `routes::pagination`'s own tests;
    // this module only covers what the directory renders.

    #[test]
    fn empty_directory_renders_message_without_table_or_pager() {
        let html = render_public_battlesnake_list(&[], true, 0, 1, 0).into_string();

        assert!(html.contains("No public battlesnakes are available yet."));
        assert!(!html.contains("<table"));
        assert!(!html.contains("pager"));
    }

    #[test]
    fn raced_empty_page_keeps_pager_navigation() {
        let html = render_public_battlesnake_list(&[], true, 1, 2, 60).into_string();

        assert!(html.contains("No public battlesnakes remain on this page."));
        assert!(html.contains(r#"href="/snakes?page=0""#));
        // No row range to show when the batch came back empty.
        assert!(!html.contains("Showing"));
    }

    #[test]
    fn rows_link_to_snake_profile_and_owner() {
        let mut snake = item("Solid Snake", "kojima");
        snake.battlesnake_id = Uuid::from_u128(1);

        let html = render_public_battlesnake_list(&[snake], true, 0, 1, 1).into_string();

        assert!(html.contains(&format!(
            r#"href="/battlesnakes/{}/profile""#,
            Uuid::from_u128(1)
        )));
        assert!(html.contains(r#"href="/users/kojima""#));
        assert!(html.contains("Solid Snake"));
        assert!(html.contains("Showing 1–1 of 1 public snakes"));
    }

    #[test]
    fn authenticated_rows_post_a_challenge_form() {
        let mut snake = item("Challenger", "owner");
        snake.battlesnake_id = Uuid::from_u128(2);

        let html = render_public_battlesnake_list(&[snake], true, 0, 1, 1).into_string();

        assert!(html.contains(&format!(
            r#"action="/battlesnakes/{}/challenge" method="post""#,
            Uuid::from_u128(2)
        )));
        assert!(html.contains("Challenge"));
        assert!(!html.contains("Sign in to challenge"));
    }

    #[test]
    fn anonymous_rows_offer_a_sign_in_link() {
        let html = render_public_battlesnake_list(&[item("Challenger", "owner")], false, 0, 1, 1)
            .into_string();

        assert!(html.contains(r#"href="/auth/github""#));
        assert!(html.contains("Sign in to challenge"));
        assert!(!html.contains("/challenge"));
    }

    #[test]
    fn middle_page_renders_both_prev_and_next_links() {
        let html = render_public_battlesnake_list(&[item("Middle", "owner")], true, 1, 3, 120)
            .into_string();

        assert!(html.contains(r#"href="/snakes?page=0""#));
        assert!(html.contains(r#"href="/snakes?page=2""#));
        assert!(html.contains("Page 2 of 3"));
        assert!(html.contains("Showing 51–51 of 120 public snakes"));
    }

    async fn seed_public_snakes(pool: &PgPool, count: usize) -> cja::Result<Uuid> {
        let user = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (9001, 'loader-owner', 'test-token')
             RETURNING user_id"
        )
        .fetch_one(pool)
        .await?;

        for i in 0..count {
            sqlx::query!(
                "INSERT INTO battlesnakes (user_id, name, url, visibility)
                 VALUES ($1, $2, 'http://localhost:8000', 'public')",
                user.user_id,
                format!("Loader Snake {i:03}")
            )
            .execute(pool)
            .await?;
        }

        Ok(user.user_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn loader_resolves_pages_against_public_snake_count(pool: PgPool) -> cja::Result<()> {
        let user_id = seed_public_snakes(&pool, 55).await?;
        sqlx::query!(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, 'Loader Hidden', 'http://localhost:8000', 'private')",
            user_id
        )
        .execute(&pool)
        .await?;

        let second = load_public_battlesnake_page(&pool, Some(1)).await?;
        assert_eq!(second.total, 55);
        assert_eq!((second.page, second.total_pages), (1, 2));
        let names: Vec<String> = second.snakes.iter().map(|s| s.name.clone()).collect();
        assert_eq!(
            names,
            (50..55)
                .map(|i| format!("Loader Snake {i:03}"))
                .collect::<Vec<_>>()
        );

        // Negative requests fall back to the first page.
        let negative = load_public_battlesnake_page(&pool, Some(-1)).await?;
        assert_eq!(negative.page, 0);
        assert_eq!(negative.snakes.len(), 50);
        assert_eq!(negative.snakes[0].name, "Loader Snake 000");

        // Oversized requests land on the final page.
        let oversized = load_public_battlesnake_page(&pool, Some(9_999)).await?;
        assert_eq!(oversized.page, 1);
        assert_eq!(oversized.snakes.len(), 5);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn loader_handles_an_empty_directory(pool: PgPool) -> cja::Result<()> {
        let empty = load_public_battlesnake_page(&pool, Some(4)).await?;

        assert_eq!(empty.total, 0);
        assert_eq!((empty.page, empty.total_pages), (0, 1));
        assert!(empty.snakes.is_empty());

        Ok(())
    }
}

#[cfg(test)]
mod stats_tests {
    use super::compute_stats;
    use crate::models::game::{GameBoardSize, GameStatus, GameType};
    use crate::models::game_battlesnake::GameHistoryEntry;

    fn entry(game_type: GameType, placement: Option<i32>) -> GameHistoryEntry {
        GameHistoryEntry {
            game_id: uuid::Uuid::new_v4(),
            board_size: GameBoardSize::Medium,
            game_type,
            status: GameStatus::Finished,
            placement,
            snake_count: 1,
            winner_name: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn mixed_history_excludes_solo_from_competitive_stats() {
        let history = vec![
            entry(GameType::Solo, Some(1)),
            entry(GameType::Standard, Some(3)),
        ];

        let stats = compute_stats(&history);
        assert_eq!(stats.total_games, 2, "total_games still counts Solo");
        assert_eq!(stats.finished_games, 1);
        assert_eq!(stats.wins, 0);
        assert_eq!(stats.win_rate, 0.0);
        assert_eq!(stats.average_placement, 3.0);
    }

    #[test]
    fn solo_only_history_has_no_competitive_games() {
        let history = vec![
            entry(GameType::Solo, Some(1)),
            entry(GameType::Solo, Some(1)),
        ];

        let stats = compute_stats(&history);
        assert_eq!(stats.total_games, history.len());
        assert_eq!(stats.finished_games, 0);
        assert_eq!(stats.wins, 0);
        assert_eq!(stats.win_rate, 0.0);
    }

    #[test]
    fn standard_only_history_unchanged() {
        let history = vec![
            entry(GameType::Standard, Some(1)),
            entry(GameType::Standard, Some(2)),
        ];

        let stats = compute_stats(&history);
        assert_eq!(stats.total_games, 2);
        assert_eq!(stats.finished_games, 2);
        assert_eq!(stats.wins, 1);
        assert_eq!(stats.win_rate, 50.0);
        assert_eq!(stats.average_placement, 1.5);
        assert_eq!(stats.second_places, 1);
    }
}
