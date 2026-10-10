use axum::{
    extract::{Path, Query, RawForm, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::{Context as _, eyre};
use maud::{Markup, html};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    components::{
        avatar::user_avatar, latency_chart::latency_chart, page_factory::PageFactory,
        snake_tags::snake_tag_chips,
    },
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::battlesnake::{self, CreateBattlesnake, EngineRegion, UpdateBattlesnake, Visibility},
    models::game_battlesnake,
    models::leaderboard,
    models::leaderboard_entry_health,
    models::session,
    models::snake_latency,
    models::tag,
    models::user::get_user_by_id,
    routes::UuidPath,
    routes::auth::{CurrentUser, CurrentUserWithSession, OptionalUser},
    routes::pagination::resolve_page,
    snake_health,
    state::AppState,
};

// Parsed new/edit battlesnake form. Parsed by hand from the urlencoded body
// because the tag checkboxes submit a repeated `tags` key, which
// `axum::Form` (serde_urlencoded) can't deserialize into a Vec.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct BattlesnakeFormData {
    name: String,
    url: String,
    visibility: Visibility,
    #[serde(default)]
    engine_region: EngineRegion,
    tag_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingBattlesnakeForm {
    target: BattlesnakeFormTarget,
    form: BattlesnakeFormData,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum BattlesnakeFormTarget {
    New,
    Edit(Uuid),
}

const MAX_DRAFT_URL_CHARS: usize = 2_048;

fn bounded_form_copy(form: &BattlesnakeFormData) -> BattlesnakeFormData {
    BattlesnakeFormData {
        name: form.name.chars().take(battlesnake::MAX_NAME_LEN).collect(),
        url: form.url.chars().take(MAX_DRAFT_URL_CHARS).collect(),
        visibility: form.visibility,
        engine_region: form.engine_region,
        tag_ids: form
            .tag_ids
            .iter()
            .take(tag::MAX_TAGS_PER_SNAKE * 4)
            .copied()
            .collect(),
    }
}

fn parse_battlesnake_form(bytes: &[u8]) -> Result<BattlesnakeFormData, String> {
    let mut name = None;
    let mut url = None;
    let mut visibility = None;
    let mut engine_region = None;
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
            "engine_region" => {
                engine_region = Some(
                    value
                        .parse::<EngineRegion>()
                        .map_err(|_| format!("Invalid engine region: {value}"))?,
                );
            }
            "tags" => tag_ids
                .push(Uuid::parse_str(&value).map_err(|_| "Invalid tag selection".to_string())?),
            _ => {}
        }
    }

    Ok(BattlesnakeFormData {
        name: name.ok_or_else(|| "Name is required".to_string())?,
        url: url
            .filter(|u| !u.is_empty())
            .ok_or_else(|| "URL is required".to_string())?,
        visibility: visibility.ok_or_else(|| "Visibility is required".to_string())?,
        engine_region: engine_region.ok_or_else(|| "Engine region is required".to_string())?,
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

const URL_NORMALIZATION_SCRIPT: &str = r#"
(function() {
  var el = document.getElementById('url');
  el.addEventListener('change', function() {
    var v = el.value.trim();
    if (v && v.indexOf('://') === -1) {
      el.value = 'https://' + v;
    }
  });
})();
"#;

fn battlesnake_form(
    action: &str,
    submit_label: &str,
    form: &BattlesnakeFormData,
    catalog: &tag::TagCatalog,
) -> Markup {
    html! {
        form class="form-stack" action=(action) method="post" {
            div class="field" {
                label for="name" { "Name" }
                input type="text" id="name" name="name" required maxlength=(battlesnake::MAX_NAME_LEN) value=(form.name);
            }
            div class="field" {
                label for="url" { "URL" }
                input type="url" id="url" name="url" required
                    placeholder="https://your-battlesnake-server.com" value=(form.url);
                p class="help" { "The URL of your Battlesnake server" }
            }
            div class="field" {
                label for="visibility" { "Visibility" }
                select id="visibility" name="visibility" required {
                    option value="public" selected[form.visibility == Visibility::Public] { "Public — anyone can add it to their games" }
                    option value="private" selected[form.visibility == Visibility::Private] { "Private — only you can add it to games" }
                }
                p class="help" { "Only controls who can pick this snake in Create Game. It always shows on your profile and in the games it plays, and you can still enter it in leaderboards and tournaments." }
            }
            div class="field" {
                label for="engine_region" { "Engine region" }
                select id="engine_region" name="engine_region" required style="min-height:44px;font-size:16px;" {
                    @for region in EngineRegion::ALL {
                        option value=(region.as_str()) selected[form.engine_region == region] { (region.label()) }
                    }
                }
                p class="help" { "Pick the region closest to where your snake is hosted." }
            }
            (tag_form_fields(catalog, &form.tag_ids))
            script { (maud::PreEscaped(URL_NORMALIZATION_SCRIPT)) }
            div class="form-cta" {
                button type="submit" class="btn solid" { (submit_label) }
                a href="/battlesnakes" class="btn" { "Cancel" }
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
    #[serde(default)]
    pub q: String,
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
    search: &str,
) -> cja::Result<PublicBattlesnakePage> {
    let mut query = battlesnake::PublicBattlesnakeQuery {
        search,
        excluded_owner_id: None,
        page: 0,
        per_page: PUBLIC_SNAKES_PER_PAGE,
    };
    let total = battlesnake::count_public_battlesnakes(pool, &query).await?;
    let (page, total_pages) = resolve_page(requested, total, PUBLIC_SNAKES_PER_PAGE);
    query.page = page;
    let snakes = battlesnake::get_public_battlesnakes_paginated(pool, &query).await?;

    Ok(PublicBattlesnakePage {
        snakes,
        page,
        total_pages,
        total,
    })
}

fn public_snakes_href(search: &str, page: i64) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("page", &page.to_string());
    if !search.is_empty() {
        query.append_pair("q", search);
    }
    format!("/snakes?{}", query.finish())
}

fn profile_history_href(battlesnake_id: Uuid, page: i64) -> String {
    format!("/battlesnakes/{battlesnake_id}/profile?page={page}")
}

fn render_public_battlesnake_list(
    snakes: &[battlesnake::PublicBattlesnakeListItem],
    is_authenticated: bool,
    search: &str,
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

        form action="/snakes" method="get" role="search" class="directory-search" {
            div class="field" {
                label for="snake-search" { "Search public battlesnakes" }
                input type="search" id="snake-search" name="q" value=(search)
                    placeholder="Snake name or owner handle";
            }
            button type="submit" class="btn solid" { "Search" }
            @if !search.is_empty() {
                a class="btn" href="/snakes" { "Clear" }
            }
        }

        @if total == 0 {
            p class="empty" {
                @if search.is_empty() {
                    "No public battlesnakes are available yet."
                } @else {
                    "No public battlesnakes match your search."
                }
            }
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
                                                    a href={"/users/"(snake.owner_login)} { (snake.owner_name) }
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
                        a href=(public_snakes_href(search, page - 1)) { "‹ Prev" }
                    }
                    @if total_pages > 1 {
                        span class="cur" { "Page " (page + 1) " of " (total_pages) }
                    }
                    @if page < total_pages - 1 {
                        a href=(public_snakes_href(search, page + 1)) { "Next ›" }
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
    let search = pagination.q.trim();
    let PublicBattlesnakePage {
        snakes,
        page,
        total_pages,
        total,
    } = load_public_battlesnake_page(&state.db, pagination.page, search)
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
                search,
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
                    div class="table-scroll" {
                        table class="data" {
                            thead {
                                tr {
                                    th { "Snake" }
                                    th { "URL" }
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
                                        td class="url-cell" {
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
            }
        }),
        flash,
    ))
}

// Show the form to create a new battlesnake
pub async fn new_battlesnake(
    State(state): State<AppState>,
    CurrentUserWithSession { session, .. }: CurrentUserWithSession,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let catalog = tag::get_tag_catalog(&state.db)
        .await
        .wrap_err("Failed to get tag catalog")?;

    let form = match session::take_pending_form_data(&state.db, session.session_id)
        .await
        .wrap_err("Failed to take pending battlesnake form data")?
    {
        Some(value) => {
            let pending: PendingBattlesnakeForm = serde_json::from_value(value)
                .wrap_err("Failed to deserialize pending battlesnake form data")?;
            if pending.target == BattlesnakeFormTarget::New {
                pending.form
            } else {
                BattlesnakeFormData::default()
            }
        }
        None => BattlesnakeFormData::default(),
    };

    let flash = page_factory.flash.clone();

    Ok(page_factory.create_page_with_flash(
        "Add New Battlesnake".to_string(),
        Box::new(html! {
            div class="crumb" { a href="/battlesnakes" { "Your Battlesnakes" } " / new" }
            div class="page-head" {
                h1 { "Add New Battlesnake" }
                div class="sub" { "Point the Arena at your snake server and pick who can play it." }
            }

            (battlesnake_form("/battlesnakes", "Create Battlesnake", &form, &catalog))
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

/// Set an error flash and bounce back to `to` — the web form's channel for
/// every validation failure (name, URL, tag cap), so the user never lands on
/// a bare error page.
async fn flash_form_and_redirect(
    pool: &PgPool,
    session_id: Uuid,
    message: String,
    form: &BattlesnakeFormData,
    target: BattlesnakeFormTarget,
    to: &str,
) -> cja::Result<axum::response::Response> {
    let pending = PendingBattlesnakeForm {
        target,
        form: bounded_form_copy(form),
    };
    let form_data = serde_json::to_value(pending)
        .wrap_err("Failed to serialize pending battlesnake form data")?;
    session::set_error_flash_with_form_data(pool, session_id, message, form_data)
        .await
        .wrap_err("Failed to set flash message and battlesnake form data")?;
    Ok(Redirect::to(to).into_response())
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

    let name = match battlesnake::validate_name(&form.name) {
        Ok(name) => name,
        Err(msg) => {
            return Ok(flash_form_and_redirect(
                &state.db,
                session.session_id,
                msg,
                &form,
                BattlesnakeFormTarget::New,
                "/battlesnakes/new",
            )
            .await?);
        }
    };

    // Enforce the tag cap before creating anything
    if form.tag_ids.len() > tag::MAX_TAGS_PER_SNAKE {
        let msg = format!(
            "A battlesnake can have at most {} tags",
            tag::MAX_TAGS_PER_SNAKE
        );
        return Ok(flash_form_and_redirect(
            &state.db,
            session.session_id,
            msg,
            &form,
            BattlesnakeFormTarget::New,
            "/battlesnakes/new",
        )
        .await?);
    }

    let url = normalize_snake_url(&form.url);
    if let Err(msg) = battlesnake::validate_url(&url) {
        return Ok(flash_form_and_redirect(
            &state.db,
            session.session_id,
            msg.to_string(),
            &form,
            BattlesnakeFormTarget::New,
            "/battlesnakes/new",
        )
        .await?);
    }

    // Moderation runs outside any DB transaction and before the insert, so
    // a flagged/unchecked row may reference a snake that was never created —
    // the row records what was submitted.
    let decision = crate::moderation::moderate_field(
        &state.db,
        &state.moderation,
        user.user_id,
        None,
        crate::moderation::FieldKind::SnakeName,
        &name,
        form.visibility == Visibility::Public,
    )
    .await;
    if decision == crate::moderation::Decision::Block {
        return Ok(flash_form_and_redirect(
            &state.db,
            session.session_id,
            crate::moderation::FieldKind::SnakeName
                .rejection_message()
                .to_string(),
            &form,
            BattlesnakeFormTarget::New,
            "/battlesnakes/new",
        )
        .await?);
    }

    let create_data = CreateBattlesnake {
        name,
        url,
        visibility: form.visibility,
        engine_region: form.engine_region,
    };

    // Create the new battlesnake in the database
    let battlesnake_result =
        battlesnake::create_battlesnake(&state.db, user.user_id, create_data.clone()).await;

    match battlesnake_result {
        Ok(snake) => {
            tag::set_tags_for_battlesnake(&state.db, snake.battlesnake_id, &form.tag_ids)
                .await
                .wrap_err("Failed to set battlesnake tags")?;

            session::take_pending_form_data(&state.db, session.session_id)
                .await
                .wrap_err("Failed to clear stale battlesnake form data")?;

            // Relay only clean names: flagged, blocked, and unchecked
            // names never reach Discord.
            if snake.visibility == Visibility::Public
                && decision == crate::moderation::Decision::Allow
            {
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
                Ok(flash_form_and_redirect(
                    &state.db,
                    session.session_id,
                    err.to_string(),
                    &form,
                    BattlesnakeFormTarget::New,
                    "/battlesnakes/new",
                )
                .await
                .wrap_err("Failed to preserve invalid battlesnake form")?)
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
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    UuidPath(battlesnake_id): UuidPath,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the battlesnake by ID
    let Some(battlesnake) = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake")?
    else {
        return Ok(crate::routes::render_not_found(page_factory));
    };

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

    let fallback = BattlesnakeFormData {
        name: battlesnake.name.clone(),
        url: battlesnake.url.clone(),
        visibility: battlesnake.visibility,
        engine_region: battlesnake.engine_region,
        tag_ids: selected_tag_ids,
    };
    let form = match session::take_pending_form_data(&state.db, session.session_id)
        .await
        .wrap_err("Failed to take pending battlesnake form data")?
    {
        Some(value) => {
            let pending: PendingBattlesnakeForm = serde_json::from_value(value)
                .wrap_err("Failed to deserialize pending battlesnake form data")?;
            if pending.target == BattlesnakeFormTarget::Edit(battlesnake_id) {
                pending.form
            } else {
                fallback
            }
        }
        None => fallback,
    };

    // Use flash from page_factory (already extracted and cleared from DB)
    let flash = page_factory.flash.clone();

    Ok(page_factory
        .create_page_with_flash(
            format!("Edit Battlesnake: {}", battlesnake.name),
            Box::new(html! {
                div class="crumb" { a href="/battlesnakes" { "Your Battlesnakes" } " / edit" }
                div class="page-head" {
                    h1 { "Edit Battlesnake: " (battlesnake.name) }
                }

                (battlesnake_form(
                    &format!("/battlesnakes/{battlesnake_id}/update"),
                    "Update Battlesnake",
                    &form,
                    &catalog,
                ))
            }),
            flash,
        )
        .into_response())
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
    let edit_path = format!("/battlesnakes/{battlesnake_id}/edit");

    // Snakes created before name validation existed may have names that
    // wouldn't pass it today. Leaving the name untouched must still let the
    // owner edit everything else, so only validate when it actually changes.
    let existing = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")?
        .ok_or_else(|| {
            eyre!("Battlesnake {battlesnake_id} vanished between ownership check and update")
        })?;
    // Compute BEFORE the name-resolution match below moves existing.name.
    // Trimmed comparison so "Foo " on an existing "Foo" doesn't burn a Jev
    // call (validate_name trims; the update result equals the existing name).
    let name_changed = form.name.trim() != existing.name;
    let name = if form.name == existing.name {
        existing.name
    } else {
        match battlesnake::validate_name(&form.name) {
            Ok(name) => name,
            Err(msg) => {
                return Ok(flash_form_and_redirect(
                    &state.db,
                    session.session_id,
                    msg,
                    &form,
                    BattlesnakeFormTarget::Edit(battlesnake_id),
                    &edit_path,
                )
                .await?);
            }
        }
    };

    // Enforce the tag cap before writing anything
    if form.tag_ids.len() > tag::MAX_TAGS_PER_SNAKE {
        let msg = format!(
            "A battlesnake can have at most {} tags",
            tag::MAX_TAGS_PER_SNAKE
        );
        return Ok(flash_form_and_redirect(
            &state.db,
            session.session_id,
            msg,
            &form,
            BattlesnakeFormTarget::Edit(battlesnake_id),
            &edit_path,
        )
        .await?);
    }

    let url = normalize_snake_url(&form.url);
    if let Err(msg) = battlesnake::validate_url(&url) {
        return Ok(flash_form_and_redirect(
            &state.db,
            session.session_id,
            msg.to_string(),
            &form,
            BattlesnakeFormTarget::Edit(battlesnake_id),
            &edit_path,
        )
        .await?);
    }

    // Only re-moderate when the name actually changes.
    if name_changed {
        let decision = crate::moderation::moderate_field(
            &state.db,
            &state.moderation,
            user.user_id,
            Some(battlesnake_id),
            crate::moderation::FieldKind::SnakeName,
            &name,
            form.visibility == Visibility::Public,
        )
        .await;
        if decision == crate::moderation::Decision::Block {
            return Ok(flash_form_and_redirect(
                &state.db,
                session.session_id,
                crate::moderation::FieldKind::SnakeName
                    .rejection_message()
                    .to_string(),
                &form,
                BattlesnakeFormTarget::Edit(battlesnake_id),
                &edit_path,
            )
            .await?);
        }
    }

    let update_data = UpdateBattlesnake {
        name,
        url,
        visibility: form.visibility,
        engine_region: Some(form.engine_region),
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

            session::take_pending_form_data(&state.db, session.session_id)
                .await
                .wrap_err("Failed to clear stale battlesnake form data")?;

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
                Ok(flash_form_and_redirect(
                    &state.db,
                    session.session_id,
                    err.to_string(),
                    &form,
                    BattlesnakeFormTarget::Edit(battlesnake_id),
                    &edit_path,
                )
                .await
                .wrap_err("Failed to preserve invalid battlesnake form")?)
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
    match battlesnake::delete_battlesnake(&state.db, battlesnake_id, user.user_id)
        .await
        .wrap_err("Failed to delete battlesnake")?
    {
        battlesnake::DeleteBattlesnakeOutcome::Deleted => {}
        battlesnake::DeleteBattlesnakeOutcome::NotFound => {
            return Err(
                "Battlesnake not found or you don't have permission to delete it".to_string(),
            )
            .with_status(StatusCode::FORBIDDEN);
        }
        battlesnake::DeleteBattlesnakeOutcome::InActiveTournament => {
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
    }

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

const PROFILE_HISTORY_PER_PAGE: i64 = 50;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ProfilePagination {
    #[serde(default)]
    page: Option<i64>,
}

/// POST /battlesnakes/{id}/reactivate — owner recovery from a health-sweeper
/// deactivation (BS-3534). Re-enables exactly the leaderboard entries the
/// sweeper disabled (manual pauses stay paused) and resets their health
/// streaks so the next sweep starts fresh.
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

    let resumed = leaderboard_entry_health::reactivate_snake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to reactivate battlesnake")?;

    if resumed == 0 {
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

    tracing::info!(
        battlesnake_id = %battlesnake_id,
        user_id = %user.user_id,
        resumed_entries = resumed,
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

/// Who is looking at a snake profile, which decides the header actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileViewer {
    Anonymous,
    Owner,
    Visitor,
}

/// Everything the snake profile renders, fetched up front by the handler.
struct ProfileView<'a> {
    snake: &'a battlesnake::Battlesnake,
    owner_login: &'a str,
    owner_name: &'a str,
    owner_avatar_url: Option<&'a str>,
    owner_exists: bool,
    owner_pronouns: &'a str,
    viewer: ProfileViewer,
    history: &'a [game_battlesnake::GameHistoryEntry],
    stats: &'a game_battlesnake::GameHistoryStats,
    page: i64,
    total_pages: i64,
    leaderboard_entries: &'a [leaderboard::BattlesnakeLeaderboardSummary],
    /// Entries the health sweeper pulled; only ever shown to the owner.
    health_paused: &'a [leaderboard_entry_health::HealthPausedEntry],
    tags: &'a [tag::Tag],
    latency: &'a snake_latency::RecentLatency,
}

fn placement_badge(placement: i32) -> Markup {
    html! {
        @match placement {
            1 => span class="badge ok" { "🥇 1st" },
            2 => span class="badge" { "🥈 2nd" },
            3 => span class="badge" { "🥉 3rd" },
            p => span class="badge" { (p) "th" },
        }
    }
}

#[allow(clippy::too_many_lines)]
fn render_battlesnake_profile(view: &ProfileView<'_>) -> Markup {
    let snake = view.snake;
    let battlesnake_id = snake.battlesnake_id;
    let stats = view.stats;
    let is_owner = view.viewer == ProfileViewer::Owner;
    let is_public = snake.visibility == Visibility::Public;

    let display_head = if snake.head.is_empty() {
        "default"
    } else {
        snake.head.as_str()
    };
    let display_tail = if snake.tail.is_empty() {
        "default"
    } else {
        snake.tail.as_str()
    };
    let raw_color = if snake.color.is_empty() {
        "#888888"
    } else {
        snake.color.as_str()
    };
    let url_color = raw_color
        .strip_prefix('#')
        .map_or_else(|| raw_color.to_string(), |hex| format!("%23{hex}"));
    let preview_url = format!(
        "https://exporter.battlesnake.com/avatars/head:{display_head}/tail:{display_tail}/color:{url_color}/320x100.svg"
    );

    let health_paused: &[_] = if is_owner { view.health_paused } else { &[] };
    let overall_latency = view.latency.overall;

    html! {
        div class="snake-profile" {
            div class="crumb" {
                @if is_owner {
                    a href="/battlesnakes" { "Your Battlesnakes" }
                } @else {
                    a href="/snakes" { "Snakes" }
                }
                " / " span { (snake.name) }
            }

            div class="page-head" {
                img class="snake-preview" src=(preview_url) alt=(format!("{} snake preview", snake.name))
                    width="128" height="40";
                div class="snake-id" {
                    h1 { (snake.name) }
                    div class="sub owner-line" {
                        "by "
                        (user_avatar(view.owner_avatar_url, view.owner_name, "owner-avatar"))
                        @if view.owner_exists {
                            a href={"/users/"(view.owner_login)} { (view.owner_name) }
                        } @else {
                            (view.owner_name)
                        }
                        @if !view.owner_pronouns.is_empty() {
                            " · " (view.owner_pronouns)
                        }
                    }
                    div class="snake-meta" {
                        @if is_public {
                            span class="badge ok" { "Public" }
                        } @else {
                            span class="badge" { "Private" }
                        }
                        span class="badge" { (snake.engine_region.label()) }
                        span { "created " (snake.created_at.format("%b %-d, %Y")) }
                        span { "head " (display_head) }
                        span { "tail " (display_tail) }
                        span {
                            span class="chip" style={"background:" (chip_color(&snake.color))} {}
                            (raw_color)
                        }
                    }
                    (snake_tag_chips(view.tags))
                    @if is_owner {
                        div class="url-cell snake-url" {
                            a href=(snake.url) target="_blank" rel="noopener" { (snake.url) }
                        }
                    }
                }
                div class="head-actions" {
                    @match view.viewer {
                        ProfileViewer::Owner => {
                            form action={"/battlesnakes/"(battlesnake_id)"/test"} method="post" {
                                button type="submit" class="btn" { "Test Snake" }
                            }
                            a href={"/battlesnakes/"(battlesnake_id)"/edit"} class="btn" { "Edit" }
                            form action={"/battlesnakes/"(battlesnake_id)"/delete"} method="post" {
                                button type="submit" class="btn danger"
                                    onclick="return confirm('Are you sure you want to delete this battlesnake?');" { "Delete" }
                            }
                        }
                        ProfileViewer::Visitor if is_public => {
                            form action={"/battlesnakes/"(battlesnake_id)"/challenge"} method="post" {
                                button type="submit" class="btn solid" { "Challenge" }
                            }
                        }
                        ProfileViewer::Anonymous if is_public => {
                            a href="/auth/github" class="btn" { "Sign in to challenge" }
                        }
                        _ => {}
                    }
                }
            }

            // Auto-deactivation notice: the health sweeper pulled this snake
            // from matchmaking on some leaderboards; the owner can resume
            // once it's fixed.
            @if !health_paused.is_empty() {
                div class="form-error snake-paused" {
                    p {
                        strong { "Paused from leaderboard matchmaking. " }
                        "This snake kept failing our health checks, so we stopped matching it to protect its rating on:"
                    }
                    ul class="fine" {
                        @for entry in health_paused {
                            li {
                                strong { (entry.leaderboard_name) }
                                " — failed " (entry.consecutive_failures) " checks in a row"
                                @if let Some(failure) = entry.last_failure.as_ref() {
                                    ". Most recent problem: " (failure)
                                }
                            }
                        }
                    }
                    p class="fine" {
                        "Fix your snake (Test Snake plays the same games, one per leaderboard), then resume."
                    }
                    form action={"/battlesnakes/"(battlesnake_id)"/reactivate"} method="post" {
                        button type="submit" class="btn solid sm" { "Resume Matchmaking" }
                    }
                }
            }

            div class="stats" {
                div class="stat" {
                    div class="label" { "Games" }
                    div class="value" {
                        (stats.total_games)
                        small { (stats.wins) " won" }
                    }
                    div class="stat-detail" { "all modes" }
                }
                div class="stat" {
                    div class="label" { "Win Rate" }
                    @if stats.finished_games > 0 {
                        div class="value" { (format!("{:.1}%", stats.win_rate)) }
                    } @else {
                        div class="value" { "—" }
                    }
                }
                div class="stat" {
                    div class="label" { "Avg. Placement" }
                    @if stats.placement_count > 0 {
                        div class="value" { (format!("{:.1}", stats.average_placement)) }
                    } @else {
                        div class="value" { "—" }
                    }
                    @if stats.finished_games > 0 {
                        div class="stat-detail" title="Finishes by placement" {
                            span { "🥇 " (stats.wins) }
                            span { "🥈 " (stats.second_places) }
                            span { "🥉 " (stats.third_places) }
                            span { "4th " (stats.fourth_places) }
                        }
                    }
                }
                div class="stat" {
                    div class="label" { "p95 Latency" }
                    @if let Some(p95) = overall_latency.p95_ms {
                        div class="value" { (format!("{p95:.0}ms")) }
                        div class="stat-detail" {
                            @if let Some(p50) = overall_latency.p50_ms {
                                span { "p50 " (format!("{p50:.0}ms")) }
                            }
                            @if overall_latency.timeouts > 0 {
                                span class="warn" { (overall_latency.timeouts) " timeouts" }
                            }
                        }
                    } @else {
                        div class="value" { "—" }
                    }
                }
            }

            section class="section" {
                h2 { "Move Latency" }
                (latency_chart(view.latency, crate::engine::MOVE_TIMEOUT_MS))
            }

            @if !view.leaderboard_entries.is_empty() {
                section class="section" {
                    h2 { "Leaderboards" }
                    div class="table-scroll" {
                        table class="data" {
                            thead {
                                tr {
                                    th { "Leaderboard" }
                                    th class="r" { "Rating" }
                                    th class="r" { "Games" }
                                    th class="r" { "1st Place" }
                                    th class="r" { "Status" }
                                }
                            }
                            tbody {
                                @for entry in view.leaderboard_entries {
                                    tr {
                                        td {
                                            a href={"/leaderboards/"(entry.leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} {
                                                (entry.leaderboard_name)
                                            }
                                        }
                                        td class="r num" { (format!("{:.1}", entry.display_score)) }
                                        td class="r num" { (entry.games_played) }
                                        td class="r num" {
                                            @if entry.games_played > 0 {
                                                (format!("{:.0}%", f64::from(entry.first_place_finishes) / f64::from(entry.games_played) * 100.0))
                                            } @else {
                                                "—"
                                            }
                                        }
                                        td class="r" {
                                            @if entry.disabled_at.is_some()
                                                && entry.disabled_reason.as_deref() == Some(leaderboard_entry_health::DISABLED_REASON_HEALTH)
                                            {
                                                span class="badge warn" title="Automatically paused: this snake is failing health checks on this leaderboard." { "Auto-paused" }
                                            } @else if entry.disabled_at.is_some() {
                                                span class="badge" { "Paused" }
                                            } @else {
                                                span class="badge ok" { "Active" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            section class="section" {
                h2 { "Game History" }
                @if stats.total_games == 0 {
                    p class="empty" { "No games played yet." }
                } @else {
                    div class="table-scroll" {
                        table class="data" {
                            thead {
                                tr {
                                    th { "Date" }
                                    th { "Game" }
                                    th class="r" { "Snakes" }
                                    th { "Placement" }
                                    th { "Winner" }
                                    th class="r" { "Replay" }
                                }
                            }
                            tbody {
                                @for entry in view.history {
                                    @let finished = entry.status == crate::models::game::GameStatus::Finished;
                                    tr {
                                        td class="when" {
                                            (entry.created_at.format("%b %-d"))
                                            span class="sub" { (entry.created_at.format("%H:%M")) }
                                        }
                                        td {
                                            (entry.game_type.as_str())
                                            span class="sub" { (entry.board_size.as_str()) }
                                        }
                                        td class="r num" { (entry.snake_count) }
                                        td {
                                            @if let Some(placement) = entry.placement {
                                                (placement_badge(placement))
                                            } @else if finished {
                                                span class="badge" { "—" }
                                            } @else {
                                                span class="badge warn" { "Live" }
                                            }
                                        }
                                        td {
                                            @if let Some(winner) = &entry.winner_name {
                                                (winner)
                                            } @else if finished {
                                                span class="sub" { "No winner" }
                                            } @else {
                                                span class="sub" { "In progress" }
                                            }
                                        }
                                        td class="r" {
                                            a href={"/games/"(entry.game_id)} class="btn sm" {
                                                @if finished { "Watch" } @else { "Live" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if view.total_pages > 1 {
                    div class="pager" {
                        @if view.page > 0 {
                            a href=(profile_history_href(battlesnake_id, view.page - 1)) { "‹ Prev" }
                        }
                        span class="cur" { "Page " (view.page + 1) " of " (view.total_pages) }
                        @if view.page + 1 < view.total_pages {
                            a href=(profile_history_href(battlesnake_id, view.page + 1)) { "Next ›" }
                        }
                        @if !view.history.is_empty() {
                            span class="spacer" {}
                            span { "Showing " (view.page * PROFILE_HISTORY_PER_PAGE + 1) "–"
                                (view.page * PROFILE_HISTORY_PER_PAGE + view.history.len() as i64)
                                " of " (stats.total_games) " games" }
                        }
                    }
                }
            }
        }
    }
}

// View a battlesnake's profile with game history and stats.
// Public to everyone: visibility only controls whether a snake can be
// matchmade against, not who can see it.
pub async fn view_battlesnake_profile(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    UuidPath(battlesnake_id): UuidPath,
    Query(params): Query<ProfilePagination>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let Some(snake) = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake")?
    else {
        return Ok(
            crate::routes::render_missing_snake(&state.db, battlesnake_id, page_factory).await?,
        );
    };

    let viewer = match user.as_ref() {
        None => ProfileViewer::Anonymous,
        Some(u) if u.user_id == snake.user_id => ProfileViewer::Owner,
        Some(_) => ProfileViewer::Visitor,
    };

    let owner = get_user_by_id(&state.db, snake.user_id)
        .await
        .wrap_err("Failed to get owner user")?;

    let stats = game_battlesnake::get_game_stats_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get game stats")?;
    let (page, total_pages) =
        resolve_page(params.page, stats.total_games, PROFILE_HISTORY_PER_PAGE);
    let history = game_battlesnake::get_game_history_for_battlesnake(
        &state.db,
        battlesnake_id,
        PROFILE_HISTORY_PER_PAGE,
        page * PROFILE_HISTORY_PER_PAGE,
    )
    .await
    .wrap_err("Failed to get game history")?;

    let leaderboard_entries = leaderboard::get_entries_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get leaderboard entries")?;

    // Health-sweeper pauses, for the owner-facing deactivation banner
    let health_paused = leaderboard_entry_health::health_paused_entries(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get health-paused entries")?;

    // Curated language/platform tags for this snake
    let snake_tags = tag::get_tags_for_battlesnake(&state.db, battlesnake_id)
        .await
        .wrap_err("Failed to get battlesnake tags")?;

    let recent_latency = snake_latency::get_recent_latency_for_battlesnake(
        &state.db,
        battlesnake_id,
        snake_latency::RECENT_GAMES_LIMIT,
    )
    .await
    .wrap_err("Failed to get recent latency")?;

    let (owner_login, owner_name) = owner.as_ref().map_or_else(
        || ("Unknown User".to_string(), "Unknown User".to_string()),
        |o| (o.github_login.clone(), o.public_name().to_string()),
    );
    let owner_pronouns = owner
        .as_ref()
        .map(|o| o.pronouns.clone())
        .unwrap_or_default();

    let content = render_battlesnake_profile(&ProfileView {
        snake: &snake,
        owner_login: &owner_login,
        owner_name: &owner_name,
        owner_avatar_url: owner.as_ref().and_then(|o| o.github_avatar_url.as_deref()),
        owner_exists: owner.is_some(),
        owner_pronouns: &owner_pronouns,
        viewer,
        history: &history,
        stats: &stats,
        page,
        total_pages,
        leaderboard_entries: &leaderboard_entries,
        health_paused: &health_paused,
        tags: &snake_tags,
        latency: &recent_latency,
    });

    // The page shell renders the flash; don't render it again in the body.
    let flash = page_factory.flash.clone();
    Ok(page_factory
        .create_page_with_flash(
            format!("Battlesnake: {}", snake.name),
            Box::new(content),
            flash,
        )
        .into_response())
}

// Run an on-demand health check against a battlesnake's URL (BS-015).
//
// Owner-only: the snake URL may be publicly visible, but the test makes the
// server poke the user's infrastructure on demand, so only the owner can
// trigger it. Plays one test game per active leaderboard (enrolled or not),
// shaped like that leaderboard's matches, so owners see exactly where their
// snake would break. Renders the results page directly from the POST (a
// flash + redirect would lose the per-call details).
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

    let mut specs = leaderboard::get_active_leaderboards(&state.db)
        .await
        .wrap_err("Failed to get active leaderboards")?
        .iter()
        .map(|lb| {
            snake_health::TestGameSpec::for_leaderboard(
                &lb.name,
                &lb.game_type,
                &lb.board_size,
                lb.match_size,
            )
        })
        .collect::<cja::Result<Vec<_>>>()?;
    if specs.is_empty() {
        specs.push(snake_health::TestGameSpec::fallback());
    }

    // Dedicated client: the shared snake client enforces the real in-game
    // budget (600ms hard timeout); the test is deliberately more forgiving
    // so a slow snake's answer is still visible, and flags moves over the
    // game budget. Redirect handling matches the game client (reqwest
    // defaults).
    let client = reqwest::Client::builder()
        .timeout(snake_health::HEALTH_CHECK_TIMEOUT)
        .build()
        .wrap_err("Failed to build HTTP client for snake test")?;
    let clients = crate::snake_client::ProxyClients {
        direct: &client,
        east: &state.proxy_east_health_client,
        europe: &state.proxy_europe_health_client,
        config: &state.config.engine_proxy,
    };
    let report =
        snake_health::run_health_check(&clients, &snake, specs, snake_health::FailureMode::RunAll)
            .await
            .wrap_err("Failed to run snake health check")?;

    Ok(page_factory.create_page(
        format!("Test Results: {}", snake.name),
        Box::new(render_test_results(&snake, &report)),
    ))
}

fn render_test_results(
    snake: &battlesnake::Battlesnake,
    report: &snake_health::HealthCheckReport,
) -> Markup {
    let battlesnake_id = snake.battlesnake_id;
    let failures = report.failure_count();
    let warnings = report.warning_count();
    let proxy_faults = report.proxy_fault_count();
    let total = report.calls().count();
    let failing_games: Vec<&str> = report
        .games
        .iter()
        .filter(|g| {
            g.calls
                .iter()
                .any(|c| c.status == snake_health::HealthCallStatus::SnakeFailure)
        })
        .map(|g| g.spec.label.as_str())
        .collect();

    html! {
        div class="container" {
            h1 { "Test Results: " (snake.name) }
            p {
                "Played a short game against "
                a href=(snake.url) target="_blank" { (snake.url) }
                " for each leaderboard, shaped like its matches (same mode, board size and number of snakes). "
                "The other snakes turn into their own necks on turn 1, so your snake wins and gets a real "
                code { "/end" } "."
            }

            @if proxy_faults > 0 {
                div class="alert alert-warning" {
                    p { (proxy_faults) " engine proxy calls failed. Snake health is unknown; try again shortly." }
                }
            } @else if failures > 0 {
                div class="alert alert-danger" {
                    p {
                        (failures) " of " (total) " checks failed — real games would break on: "
                        @if failing_games.is_empty() {
                            "every leaderboard"
                        } @else {
                            (failing_games.join(", "))
                        }
                        ". See details below."
                    }
                }
            } @else if warnings > 0 {
                div class="alert alert-warning" {
                    p {
                        "Games will run, but " (warnings) " of " (total)
                        " responses don't follow the Battlesnake API spec. See details below."
                    }
                }
            } @else {
                div class="alert alert-success" {
                    p { "All " (total) " checks passed. This snake looks ready to play on every leaderboard!" }
                }
            }

            (test_results_table(std::slice::from_ref(&report.identity), report.game_timeout_ms))

            @for game in &report.games {
                h2 { (game.spec.label) }
                p class="text-muted" { (game.spec.describe()) }
                (test_results_table(&game.calls, report.game_timeout_ms))
            }

            p class="text-muted" {
                "Each test call was allowed "
                (snake_health::HEALTH_CHECK_TIMEOUT.as_secs())
                " seconds so you can see slow answers, but real games only allow "
                (report.game_timeout_ms)
                " ms per move — a slower move counts as a failure."
            }

            div class="mt-4" {
                form action={"/battlesnakes/"(battlesnake_id)"/test"} method="post" class="inline" style="display: inline;" {
                    button type="submit" class="btn btn-primary" { "Run Test Again" }
                }
                a href={"/battlesnakes/"(battlesnake_id)"/profile"} class="btn btn-secondary ms-2" { "Back to Profile" }
            }
        }
    }
}

fn test_results_table(calls: &[snake_health::HealthCheckCall], game_timeout_ms: i64) -> Markup {
    html! {
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
                @for call in calls {
                    tr {
                        td { code { (call.name) } }
                        td {
                            @match call.status {
                                snake_health::HealthCallStatus::Healthy => span class="badge ok" { "OK" },
                                snake_health::HealthCallStatus::Warning => span class="badge warn" { "Warning" },
                                snake_health::HealthCallStatus::ProxyFault => span class="badge warn" { "Proxy error" },
                                snake_health::HealthCallStatus::SnakeFailure => span class="badge warn" { "Failed" },
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
                                @if i64::try_from(latency).is_ok_and(|l| l > game_timeout_ms) {
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
    }
}

#[cfg(test)]
mod form_tests {
    use super::{
        BattlesnakeFormData, MAX_DRAFT_URL_CHARS, bounded_form_copy, parse_battlesnake_form,
    };
    use crate::models::{battlesnake, battlesnake::EngineRegion, battlesnake::Visibility, tag};
    use uuid::Uuid;

    #[test]
    fn bounded_form_copy_preserves_order_and_unicode_prefixes() {
        let name: String = (0..battlesnake::MAX_NAME_LEN + 10)
            .map(|i| char::from_u32(0x400 + i as u32).unwrap())
            .collect();
        let url: String = (0..2_100)
            .map(|i| char::from_u32(0x500 + i as u32).unwrap())
            .collect();
        let tag_ids: Vec<Uuid> = (0..25).map(|_| Uuid::new_v4()).collect();
        let form = BattlesnakeFormData {
            name: name.clone(),
            url: url.clone(),
            visibility: Visibility::Private,
            engine_region: EngineRegion::UsEast4,
            tag_ids: tag_ids.clone(),
        };

        let bounded = bounded_form_copy(&form);

        assert_eq!(bounded.name.chars().count(), battlesnake::MAX_NAME_LEN);
        assert_eq!(bounded.url.chars().count(), MAX_DRAFT_URL_CHARS);
        assert_eq!(bounded.tag_ids.len(), tag::MAX_TAGS_PER_SNAKE * 4);
        assert_eq!(
            bounded.name,
            name.chars()
                .take(battlesnake::MAX_NAME_LEN)
                .collect::<String>()
        );
        assert_eq!(
            bounded.url,
            url.chars().take(MAX_DRAFT_URL_CHARS).collect::<String>()
        );
        assert_eq!(bounded.tag_ids, tag_ids[..tag::MAX_TAGS_PER_SNAKE * 4]);
        assert_eq!(bounded.visibility, Visibility::Private);
        assert_eq!(bounded.engine_region, EngineRegion::UsEast4);
    }

    #[test]
    fn form_requires_a_valid_engine_region() {
        let base = b"name=Example&url=https%3A%2F%2Fexample.com&visibility=public";
        assert_eq!(
            parse_battlesnake_form(base).unwrap_err(),
            "Engine region is required"
        );
        let invalid =
            b"name=Example&url=https%3A%2F%2Fexample.com&visibility=public&engine_region=moon";
        assert_eq!(
            parse_battlesnake_form(invalid).unwrap_err(),
            "Invalid engine region: moon"
        );
        let valid = b"name=Example&url=https%3A%2F%2Fexample.com&visibility=public&engine_region=europe-west4";
        assert_eq!(
            parse_battlesnake_form(valid).unwrap().engine_region,
            EngineRegion::EuropeWest4
        );
    }

    #[test]
    fn old_pending_form_defaults_to_west() {
        let old =
            r#"{"name":"Example","url":"https://example.com","visibility":"public","tag_ids":[]}"#;
        let form: BattlesnakeFormData = serde_json::from_str(old).unwrap();
        assert_eq!(form.engine_region, EngineRegion::UsWest1);
    }
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
            owner_name: owner.to_string(),
        }
    }

    // Page-number clamping is covered by `routes::pagination`'s own tests;
    // this module only covers what the directory renders.

    #[test]
    fn empty_directory_renders_message_without_table_or_pager() {
        let html = render_public_battlesnake_list(&[], true, "", 0, 1, 0).into_string();

        assert!(html.contains("No public battlesnakes are available yet."));
        assert!(!html.contains("<table"));
        assert!(!html.contains("pager"));
    }

    #[test]
    fn raced_empty_page_keeps_pager_navigation() {
        let html = render_public_battlesnake_list(&[], true, "", 1, 2, 60).into_string();

        assert!(html.contains("No public battlesnakes remain on this page."));
        assert!(html.contains(r#"href="/snakes?page=0""#));
        // No row range to show when the batch came back empty.
        assert!(!html.contains("Showing"));
    }

    #[test]
    fn rows_link_to_snake_profile_and_owner() {
        let mut snake = item("Solid Snake", "kojima");
        snake.battlesnake_id = Uuid::from_u128(1);
        snake.owner_name = "Hideo Kojima".to_string();

        let html = render_public_battlesnake_list(&[snake], true, "", 0, 1, 1).into_string();

        assert!(html.contains(&format!(
            r#"href="/battlesnakes/{}/profile""#,
            Uuid::from_u128(1)
        )));
        assert!(html.contains(r#"<a href="/users/kojima">Hideo Kojima</a>"#));
        assert!(html.contains("Solid Snake"));
        assert!(html.contains("Showing 1–1 of 1 public snakes"));
    }

    #[test]
    fn authenticated_rows_post_a_challenge_form() {
        let mut snake = item("Challenger", "owner");
        snake.battlesnake_id = Uuid::from_u128(2);

        let html = render_public_battlesnake_list(&[snake], true, "", 0, 1, 1).into_string();

        assert!(html.contains(&format!(
            r#"action="/battlesnakes/{}/challenge" method="post""#,
            Uuid::from_u128(2)
        )));
        assert!(html.contains("Challenge"));
        assert!(!html.contains("Sign in to challenge"));
    }

    #[test]
    fn anonymous_rows_offer_a_sign_in_link() {
        let html =
            render_public_battlesnake_list(&[item("Challenger", "owner")], false, "", 0, 1, 1)
                .into_string();

        assert!(html.contains(r#"href="/auth/github""#));
        assert!(html.contains("Sign in to challenge"));
        assert!(!html.contains("/challenge"));
    }

    #[test]
    fn middle_page_renders_both_prev_and_next_links() {
        let html = render_public_battlesnake_list(&[item("Middle", "owner")], true, "", 1, 3, 120)
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

        let second = load_public_battlesnake_page(&pool, Some(1), "").await?;
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
        let negative = load_public_battlesnake_page(&pool, Some(-1), "").await?;
        assert_eq!(negative.page, 0);
        assert_eq!(negative.snakes.len(), 50);
        assert_eq!(negative.snakes[0].name, "Loader Snake 000");

        // Oversized requests land on the final page.
        let oversized = load_public_battlesnake_page(&pool, Some(9_999), "").await?;
        assert_eq!(oversized.page, 1);
        assert_eq!(oversized.snakes.len(), 5);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn loader_handles_an_empty_directory(pool: PgPool) -> cja::Result<()> {
        let empty = load_public_battlesnake_page(&pool, Some(4), "").await?;

        assert_eq!(empty.total, 0);
        assert_eq!((empty.page, empty.total_pages), (0, 1));
        assert!(empty.snakes.is_empty());

        Ok(())
    }
}

#[cfg(test)]
mod deleted_snake_route_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::models::battlesnake::{self, DeleteBattlesnakeOutcome};
    use crate::state::AppState;

    fn app(pool: &PgPool) -> axum::Router {
        crate::routes::routes(AppState::test_from_pool(pool.clone()))
            .layer(tower_cookies::CookieManagerLayer::new())
    }

    async fn send(
        app: &axum::Router,
        method: Method,
        path: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(path);
        if path.starts_with("/api/") {
            builder = builder.header(header::ORIGIN, "https://example.com");
        }
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn owner_with_snake(pool: &PgPool, github_id: i64, name: &str) -> (Uuid, Uuid) {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, '') RETURNING user_id",
        )
        .bind(github_id)
        .bind(format!("route-owner-{github_id}"))
        .fetch_one(pool)
        .await
        .unwrap();
        let snake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'http://localhost:8000') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
        (user_id, snake_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_snake_pages_render_the_deleted_page(pool: PgPool) {
        let (owner, snake) = owner_with_snake(&pool, 9101, "Ghost Snake").await;
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('Route Test') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let entry_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
             VALUES ($1, $2) RETURNING leaderboard_entry_id",
        )
        .bind(leaderboard_id)
        .bind(snake)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            battlesnake::delete_battlesnake(&pool, snake, owner)
                .await
                .unwrap(),
            DeleteBattlesnakeOutcome::Deleted
        );
        let app = app(&pool);

        for path in [
            format!("/battlesnakes/{snake}/profile"),
            format!("/leaderboards/{leaderboard_id}/entries/{entry_id}"),
        ] {
            let (status, body) = send(&app, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::GONE, "{path}");
            assert!(body.contains("Ghost Snake"), "{path}");
            assert!(body.contains("deleted by its owner"), "{path}");
        }

        // A snake that never existed is still a plain 404.
        let (status, body) = send(
            &app,
            Method::GET,
            &format!("/battlesnakes/{}/profile", Uuid::new_v4()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.contains("deleted by its owner"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn api_delete_refuses_active_tournaments_then_soft_deletes(pool: PgPool) {
        let (owner, snake) = owner_with_snake(&pool, 9201, "Api Snake").await;
        let token = crate::models::api_token::create_api_token(&pool, owner, "delete-test")
            .await
            .unwrap();
        let tournament_id: Uuid = sqlx::query_scalar(
            "INSERT INTO tournaments (name, user_id, status)
             VALUES ('Open', $1, 'registration') RETURNING tournament_id",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        crate::models::tournament::create_registration(&pool, tournament_id, snake, owner, 1)
            .await
            .unwrap();
        let app = app(&pool);
        let path = format!("/api/snakes/{snake}");

        let (status, _) = send(&app, Method::DELETE, &path, Some(&token.secret)).await;
        assert_eq!(status, StatusCode::CONFLICT);

        sqlx::query("UPDATE tournaments SET status = 'completed' WHERE tournament_id = $1")
            .bind(tournament_id)
            .execute(&pool)
            .await
            .unwrap();
        let (status, _) = send(&app, Method::DELETE, &path, Some(&token.secret)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _) = send(&app, Method::GET, &path, Some(&token.secret)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&app, Method::DELETE, &path, Some(&token.secret)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod profile_page_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use tower::ServiceExt as _;

    use super::*;
    use crate::routes::test_support::{
        create_user_session, session_user_id, signed_session_cookie,
    };

    struct Fixture {
        app: axum::Router,
        state: AppState,
        db: PgPool,
        owner_session: Uuid,
        visitor_session: Uuid,
        snake_id: Uuid,
    }

    impl Fixture {
        async fn new(db: PgPool, visibility: &str) -> Self {
            let state = AppState::test_from_pool(db.clone());
            let app = crate::routes::routes(state.clone())
                .layer(tower_cookies::CookieManagerLayer::new());
            let owner_session = create_user_session(&db, 51001, false).await;
            let visitor_session = create_user_session(&db, 51002, false).await;
            let owner_id = session_user_id(&db, owner_session).await;
            let snake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url, visibility)
                 VALUES ($1, 'Profile Snake', 'https://snake.example.com/api', $2)
                 RETURNING battlesnake_id",
            )
            .bind(owner_id)
            .bind(visibility)
            .fetch_one(&db)
            .await
            .unwrap();
            Self {
                app,
                state,
                db,
                owner_session,
                visitor_session,
                snake_id,
            }
        }

        async fn get(&self, session: Option<Uuid>) -> (StatusCode, String) {
            self.get_page(session, None).await
        }

        async fn get_page(&self, session: Option<Uuid>, page: Option<i64>) -> (StatusCode, String) {
            let uri = if let Some(page) = page {
                profile_history_href(self.snake_id, page)
            } else {
                format!("/battlesnakes/{}/profile", self.snake_id)
            };
            let mut builder = Request::builder().uri(uri);
            if let Some(session_id) = session {
                builder = builder.header(
                    header::COOKIE,
                    format!(
                        "{}={}",
                        session::SESSION_COOKIE_NAME,
                        signed_session_cookie(&self.state, session_id)
                    ),
                );
            }
            let response = self
                .app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }

        async fn post(&self, session: Uuid, action: &str) -> (StatusCode, String) {
            let request = Request::builder()
                .method(axum::http::Method::POST)
                .uri(format!("/battlesnakes/{}/{action}", self.snake_id))
                .header(
                    header::COOKIE,
                    format!(
                        "{}={}",
                        session::SESSION_COOKIE_NAME,
                        signed_session_cookie(&self.state, session)
                    ),
                )
                .body(Body::empty())
                .unwrap();
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }

        async fn deactivate(&self) {
            sqlx::query(
                "INSERT INTO leaderboard_entries
                     (leaderboard_id, battlesnake_id, disabled_at, disabled_reason,
                      health_consecutive_failures, health_last_failure)
                 SELECT leaderboard_id, $1, NOW(), 'health', 4, 'POST /move timed out'
                 FROM leaderboards ORDER BY created_at, leaderboard_id LIMIT 1",
            )
            .bind(self.snake_id)
            .execute(&self.db)
            .await
            .unwrap();
        }
    }

    /// Test Snake plays one game per active leaderboard — enrolled or not —
    /// and names the leaderboards a real game would break on.
    #[sqlx::test(migrations = "../migrations")]
    async fn test_snake_plays_every_active_leaderboard(db: PgPool) {
        use wiremock::matchers::{any, body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"apiversion":"1"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .and(body_partial_json(
                serde_json::json!({"game": {"map": "royale"}}),
            ))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&server)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let fx = Fixture::new(db, "public").await;
        sqlx::query("UPDATE battlesnakes SET url = $1 WHERE battlesnake_id = $2")
            .bind(server.uri())
            .bind(fx.snake_id)
            .execute(&fx.db)
            .await
            .unwrap();
        sqlx::query("UPDATE leaderboards SET disabled_at = NOW() WHERE name = 'Constrictor 11x11'")
            .execute(&fx.db)
            .await
            .unwrap();
        let active: Vec<String> =
            sqlx::query_scalar("SELECT name FROM leaderboards WHERE disabled_at IS NULL")
                .fetch_all(&fx.db)
                .await
                .unwrap();
        assert!(active.iter().any(|name| name == "Royale 11x11"));

        let (status, _) = fx.post(fx.visitor_session, "test").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(server.received_requests().await.unwrap().is_empty());

        let (status, html) = fx.post(fx.owner_session, "test").await;

        assert_eq!(status, StatusCode::OK);
        for name in &active {
            assert!(html.contains(&format!("<h2>{name}</h2>")), "missing {name}");
        }
        assert!(
            !html.contains("Constrictor 11x11"),
            "retired boards aren't tested"
        );
        assert!(html.contains("4 snakes · Royale · 11x11"));
        assert!(html.contains("real games would break on: Royale 11x11."));
        assert!(html.contains("POST /move (turn 1)"));
        let gets = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "GET")
            .count();
        assert_eq!(gets, 1, "GET / once, not once per leaderboard");
    }

    fn challenge_form(snake_id: Uuid) -> String {
        format!(r#"action="/battlesnakes/{snake_id}/challenge""#)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn profile_history_paginates_leaderboard_and_direct_games(db: PgPool) {
        let fx = Fixture::new(db, "public").await;
        let (_, empty) = fx.get(None).await;
        assert!(empty.contains("No games played yet."));
        assert!(!empty.contains("class=\"pager\""));
        let entry_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, games_played)
             SELECT leaderboard_id, $1, 7 FROM leaderboards ORDER BY created_at LIMIT 1
             RETURNING leaderboard_entry_id",
        )
        .bind(fx.snake_id)
        .fetch_one(&fx.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO games (game_id, board_size, game_type, status, created_at)
             SELECT md5('profile-route-' || n)::uuid, '11x11', 'Standard', 'finished',
                    now() - n * interval '1 minute'
             FROM generate_series(0, 52) n",
        )
        .execute(&fx.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO game_battlesnakes
                 (game_id, battlesnake_id, leaderboard_entry_id, placement, created_at)
             SELECT g.game_id, CASE WHEN n >= 51 THEN $1 ELSE NULL END,
                    CASE WHEN n < 51 THEN $2 ELSE NULL END,
                    CASE WHEN n IN (0, 51) THEN 1 ELSE 2 END, g.created_at
             FROM generate_series(0, 52) n
             JOIN games g ON g.game_id = md5('profile-route-' || n)::uuid",
        )
        .bind(fx.snake_id)
        .bind(entry_id)
        .execute(&fx.db)
        .await
        .unwrap();

        for (requested, expected_rows, pager_link, label, showing) in [
            (None, 50, 1, "Page 1 of 2", "Showing 1–50 of 53 games"),
            (Some(1), 3, 0, "Page 2 of 2", "Showing 51–53 of 53 games"),
            (Some(-1), 50, 1, "Page 1 of 2", "Showing 1–50 of 53 games"),
            (Some(999), 3, 0, "Page 2 of 2", "Showing 51–53 of 53 games"),
        ] {
            let (status, html) = fx.get_page(None, requested).await;
            assert_eq!(status, StatusCode::OK);
            let history = html.split("Game History").nth(1).unwrap();
            let tbody = history.split("</tbody>").next().unwrap();
            assert_eq!(tbody.matches("class=\"btn sm\"").count(), expected_rows);
            assert!(html.contains("53<small>2 won</small>"));
            assert!(html.contains("all modes"));
            assert!(html.contains("<span class=\"cur\">"));
            assert!(html.contains(label));
            assert!(html.contains(showing));
            assert!(html.contains(&format!(
                "href=\"/battlesnakes/{}/profile?page={pager_link}\"",
                fx.snake_id
            )));
            assert!(html.contains("<td class=\"r num\">7</td>"));
            if pager_link == 1 {
                assert!(html.contains("Next ›"));
                assert!(!html.contains("‹ Prev"));
                assert!(history.contains("Profile Snake"));
            } else {
                assert!(html.contains("‹ Prev"));
                assert!(!html.contains("Next ›"));
            }
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn finished_unplaced_game_has_no_average_placement(db: PgPool) {
        let fx = Fixture::new(db, "public").await;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'finished') RETURNING game_id",
        )
        .fetch_one(&fx.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
            .bind(game_id)
            .bind(fx.snake_id)
            .execute(&fx.db)
            .await
            .unwrap();

        let stats = game_battlesnake::get_game_stats_for_battlesnake(&fx.db, fx.snake_id)
            .await
            .unwrap();
        assert_eq!(stats.finished_games, 1);
        assert_eq!(stats.placement_count, 0);

        let (status, html) = fx.get(None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Avg. Placement</div><div class=\"value\">—</div>"));
        assert!(!html.contains("Avg. Placement</div><div class=\"value\">0.0</div>"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn owner_sees_management_actions_url_and_pause_notice(db: PgPool) {
        let fx = Fixture::new(db, "public").await;
        fx.deactivate().await;

        let (status, html) = fx.get(Some(fx.owner_session)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"<a href="/battlesnakes">Your Battlesnakes</a>"#));
        for action in ["test", "delete", "reactivate"] {
            assert!(
                html.contains(&format!(
                    r#"action="/battlesnakes/{}/{action}""#,
                    fx.snake_id
                )),
                "owner should get the {action} form"
            );
        }
        assert!(html.contains(&format!(r#"href="/battlesnakes/{}/edit""#, fx.snake_id)));
        assert!(html.contains("https://snake.example.com/api"));
        assert!(html.contains("Paused from leaderboard matchmaking."));
        assert!(html.contains("POST /move timed out"));
        assert!(!html.contains(&challenge_form(fx.snake_id)));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn visitors_can_challenge_but_never_see_owner_details(db: PgPool) {
        let fx = Fixture::new(db, "public").await;
        fx.deactivate().await;

        let (status, html) = fx.get(Some(fx.visitor_session)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(&challenge_form(fx.snake_id)));
        assert!(html.contains(r#"<a href="/snakes">Snakes</a>"#));
        assert!(
            !html.contains("https://snake.example.com/api"),
            "URL is owner-only"
        );
        assert!(!html.contains("Paused from leaderboard matchmaking."));
        assert!(!html.contains(&format!(r#"action="/battlesnakes/{}/delete""#, fx.snake_id)));
        assert!(!html.contains(&format!(r#"href="/battlesnakes/{}/edit""#, fx.snake_id)));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn anonymous_visitors_are_asked_to_sign_in_to_challenge(db: PgPool) {
        let fx = Fixture::new(db, "public").await;

        let (status, html) = fx.get(None).await;

        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Sign in to challenge"));
        assert!(!html.contains(&challenge_form(fx.snake_id)));
        assert!(!html.contains("https://snake.example.com/api"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn private_snakes_offer_no_challenge(db: PgPool) {
        let fx = Fixture::new(db, "private").await;

        let (_, visitor) = fx.get(Some(fx.visitor_session)).await;
        let (_, anonymous) = fx.get(None).await;

        for html in [visitor, anonymous] {
            assert!(html.contains("Private"));
            assert!(!html.contains(&challenge_form(fx.snake_id)));
            assert!(!html.contains("Sign in to challenge"));
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn flash_message_renders_once(db: PgPool) {
        let fx = Fixture::new(db, "public").await;
        session::set_flash_message(
            &fx.db,
            fx.owner_session,
            "Battlesnake updated successfully!".to_string(),
            session::FLASH_TYPE_SUCCESS,
        )
        .await
        .unwrap();

        let (_, html) = fx.get(Some(fx.owner_session)).await;

        assert_eq!(html.matches("Battlesnake updated successfully!").count(), 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_games_feed_the_latency_chart(db: PgPool) {
        let fx = Fixture::new(db, "public").await;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished')
             RETURNING game_id",
        )
        .fetch_one(&fx.db)
        .await
        .unwrap();
        let game_battlesnake_id: Uuid = sqlx::query_scalar(
            "WITH entry AS (
                 INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
                 SELECT leaderboard_id, $2 FROM leaderboards ORDER BY created_at LIMIT 1
                 RETURNING leaderboard_entry_id
             )
             INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id)
             SELECT $1, leaderboard_entry_id FROM entry
             RETURNING game_battlesnake_id",
        )
        .bind(game_id)
        .bind(fx.snake_id)
        .fetch_one(&fx.db)
        .await
        .unwrap();
        for (turn, latency) in [(0, Some(40)), (1, Some(60)), (2, None)] {
            let turn_id: Uuid = sqlx::query_scalar(
                "INSERT INTO turns (game_id, turn_number) VALUES ($1, $2) RETURNING turn_id",
            )
            .bind(game_id)
            .bind(turn)
            .fetch_one(&fx.db)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO snake_turns (turn_id, game_battlesnake_id, direction, latency_ms, timed_out)
                 VALUES ($1, $2, 'up', $3, $4)",
            )
            .bind(turn_id)
            .bind(game_battlesnake_id)
            .bind(latency)
            .bind(latency.is_none())
            .execute(&fx.db)
            .await
            .unwrap();
        }

        let (_, html) = fx.get(None).await;

        assert!(html.contains(r#"class="latency-chart""#));
        assert!(html.contains(&format!(r#"href="/games/{game_id}""#)));
        assert!(html.contains("1 of 3 moves timed out"));
        assert!(html.contains("p95 Latency"));
    }
}
