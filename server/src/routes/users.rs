use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use uuid::Uuid;

use crate::{
    components::{page::Page, page_factory::PageFactory},
    errors::{ServerResult, WithStatus},
    models::{
        battlesnake::{self, Visibility},
        leaderboard, saved_game,
        user::{self, PlayerDirectoryEntry, User},
    },
    routes::auth::OptionalUser,
    routes::pagination::resolve_page,
    state::AppState,
};

/// Display name shown on the public profile: the chosen display name when
/// set, otherwise the GitHub login.
fn public_name(user: &User) -> &str {
    user.display_name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(&user.github_login)
}

/// GET /users/{login} — public user profile, looked up by GitHub login.
///
/// `users.github_login` is not unique (GitHub logins can be renamed and
/// reused, and lookup is case-insensitive), so this route can be ambiguous.
/// It is kept for compatibility and for hand-typed URLs. New links should
/// prefer the stable `/users/{login}/{user_id}` form; the `/snakes` directory,
/// the snake profile page, and "View Public Profile" on `/me` still emit
/// login-only URLs and are worth converting in a follow-up.
pub async fn show_user_profile(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path(login): Path<String>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let user = user::get_user_by_github_login(&state.db, &login)
        .await
        .wrap_err("Failed to fetch user")?
        .ok_or_else(|| "User not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    render_user_profile(&state, viewer, user, page_factory).await
}

/// GET /users/{login}/{user_id} — public user profile addressed by its stable
/// UUID.
///
/// The `_login` segment is cosmetic: it keeps the URL readable and shareable,
/// but the UUID is authoritative. A stale or differently-cased slug still
/// resolves to the right person rather than 404ing or picking a namesake.
pub async fn show_user_profile_by_id(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path((_login, user_id)): Path<(String, Uuid)>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let user = user::get_user_by_id(&state.db, user_id)
        .await
        .wrap_err("Failed to fetch user")?
        .ok_or_else(|| "User not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    render_user_profile(&state, viewer, user, page_factory).await
}

/// Render the public profile of an already-resolved user. Shared by both
/// profile routes so the login and stable-ID URLs can never drift apart.
///
/// Everything here is visible to anonymous visitors, matching the public
/// profiles on play.battlesnake.com: identity fields the user chose to share,
/// their snakes, and where those snakes sit on the leaderboards. Snake
/// visibility only controls matchmaking eligibility, not who can see it, so
/// all snakes are listed (private ones badged).
async fn render_user_profile(
    state: &AppState,
    viewer: Option<User>,
    user: User,
    page_factory: PageFactory,
) -> ServerResult<Page, StatusCode> {
    let is_self = viewer.as_ref().is_some_and(|v| v.user_id == user.user_id);

    let snakes = battlesnake::get_battlesnakes_by_user_id(&state.db, user.user_id)
        .await
        .wrap_err("Failed to fetch user's battlesnakes")?;

    // Snake counts per user are small, so per-snake entry lookups are fine.
    let mut snakes_with_entries = Vec::with_capacity(snakes.len());
    for snake in snakes {
        let entries = leaderboard::get_entries_for_battlesnake(&state.db, snake.battlesnake_id)
            .await
            .wrap_err("Failed to fetch leaderboard entries")?;
        snakes_with_entries.push((snake, entries));
    }

    let saved_games = saved_game::list_saved_games_for_user(&state.db, user.user_id)
        .await
        .wrap_err("Failed to fetch saved games")?;

    let name = public_name(&user).to_string();

    Ok(page_factory.create_page(
        name.clone(),
        Box::new(html! {
            header class="profile-head" {
                img class="avatar" src=(user.github_avatar_url.clone().unwrap_or_default()) alt="";
                div class="who" {
                    h1 { (name) }
                    div class="meta" {
                        "@" (user.github_login)
                        @if !user.pronouns.is_empty() { " · " (user.pronouns) }
                        @if !user.country.is_empty() { " · " (user.country) }
                        " · joined " (user.created_at.format("%b %Y"))
                    }
                    @if !user.backstory.is_empty() {
                        p class="bio" { (user.backstory) }
                    }
                }
            }

            @if is_self {
                div class="profile-actions" {
                    a href="/me" class="btn" { "Edit Profile" }
                    a href="/battlesnakes" class="btn" { "Manage Battlesnakes" }
                }
            }

            section class="section" {
                h2 { "Battlesnakes" }
                @if snakes_with_entries.is_empty() {
                    p class="empty" { "No snakes yet." }
                } @else {
                    div class="snakes" {
                        @for (snake, entries) in &snakes_with_entries {
                            div class="scard" {
                                div class="top" {
                                    div {
                                        div class="name" {
                                            a href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} {
                                                (snake.name)
                                            }
                                            @if snake.visibility == Visibility::Private {
                                                " "
                                                span class="live-pill quiet" { "Private" }
                                            }
                                        }
                                    }
                                }
                                @if entries.is_empty() {
                                    p class="empty" { "Not on any leaderboards." }
                                } @else {
                                    dl class="meta-list" {
                                        @for entry in entries {
                                            div {
                                                dt {
                                                    a href={"/leaderboards/"(entry.leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} {
                                                        (entry.leaderboard_name)
                                                    }
                                                }
                                                dd {
                                                    (format!("{:.1}", entry.display_score))
                                                    " · " (entry.games_played) " games"
                                                    @if entry.disabled_at.is_some() {
                                                        " · paused"
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
            }

            section class="section" {
                h2 { "Saved Games" }
                @if saved_games.is_empty() {
                    p class="empty" { "No saved games yet." }
                } @else {
                    dl class="meta-list" {
                        @for saved in &saved_games {
                            div {
                                dt {
                                    a href={"/games/"(saved.game_id)} { (saved.display_title()) }
                                }
                                dd {
                                    (saved.game_created_at.format("%b %-d, %Y"))
                                    @if is_self {
                                        " "
                                        form
                                            action={"/saved-games/"(saved.saved_game_id)"/delete"}
                                            method="post"
                                            style="display:inline" {
                                            button type="submit" class="btn" { "Remove" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
    ))
}

/// Rows per page in the public `/players` directory.
const PLAYERS_PER_PAGE: i64 = 50;

#[derive(Debug, serde::Deserialize)]
pub struct PlayerDirectoryParams {
    #[serde(default)]
    pub page: Option<i64>,
    /// Restrict the listing to players with at least one active snake.
    #[serde(default, deserialize_with = "de_lenient_bool")]
    pub active: bool,
}

/// Deserialize `?active=…` leniently. This is a public, crawlable, linkable
/// page, so a hand-edited `?active=1` or a bare `?active` should show a page
/// rather than a plain-text 400. Anything unrecognised reads as "off".
fn de_lenient_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    // `?active` with no `=` deserializes as an empty string, which reads the
    // same way an HTML checkbox's presence does: on.
    let raw = String::deserialize(deserializer)?;

    Ok(matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "true" | "t" | "1" | "yes" | "y" | "on"
    ))
}

/// Base URL for the directory in a given filter mode. Switching filters always
/// drops `page`, since page N of "all players" has nothing to do with page N
/// of "active players".
fn players_filter_href(active_only: bool) -> &'static str {
    if active_only {
        "/players?active=true"
    } else {
        "/players"
    }
}

/// Pager link for a page within the current filter mode.
fn players_page_href(active_only: bool, page: i64) -> String {
    if active_only {
        format!("/players?active=true&page={page}")
    } else {
        format!("/players?page={page}")
    }
}

fn render_player_directory(
    players: &[PlayerDirectoryEntry],
    active_only: bool,
    page: i64,
    total_pages: i64,
    total: i64,
) -> Markup {
    html! {
        div class="page-head" {
            h1 { "Players" }
            div class="sub" {
                "Everyone who has joined the arena. Follow a name through to see their snakes."
            }
        }

        nav class="modes" aria-label="Player filter" {
            @if active_only {
                a class="mode" href=(players_filter_href(false)) { "All players" }
                span class="mode on" aria-current="page" { "With active snakes" }
            } @else {
                span class="mode on" aria-current="page" { "All players" }
                a class="mode" href=(players_filter_href(true)) { "With active snakes" }
            }
        }

        @if total == 0 {
            @if active_only {
                p class="empty" { "No players currently have an active snake." }
            } @else {
                p class="empty" { "No players have joined yet." }
            }
        } @else {
        div class="section" {
            @if players.is_empty() {
                // The count and the page fetch are separate statements, so a
                // player can drop out of the filter between them and empty
                // this page. Keep the pager so the visitor can step back to a
                // page that still has rows.
                p class="empty" { "No players remain on this page." }
            } @else {
                table class="data" {
                    thead {
                        tr { th { "Player" } }
                    }
                    tbody {
                        @for player in players {
                            tr {
                                td {
                                    div class="snake-cell" {
                                        a class="name" href={"/users/"(player.github_login)"/"(player.user_id)} {
                                            (player.public_name)
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
                    a href=(players_page_href(active_only, page - 1)) { "‹ Prev" }
                }
                @if total_pages > 1 {
                    span class="cur" { "Page " (page + 1) " of " (total_pages) }
                }
                @if page < total_pages - 1 {
                    a href=(players_page_href(active_only, page + 1)) { "Next ›" }
                }
                @if !players.is_empty() {
                    span class="spacer" {}
                    span {
                        "Showing " (page * PLAYERS_PER_PAGE + 1)
                        "–" (page * PLAYERS_PER_PAGE + players.len() as i64)
                        " of " (total) " players"
                    }
                }
            }
        }
        }
    }
}

/// GET /players — browsable directory of everyone in the arena.
pub async fn list_players(
    State(state): State<AppState>,
    Query(params): Query<PlayerDirectoryParams>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let active_only = params.active;

    let total = user::count_players(&state.db, active_only)
        .await
        .wrap_err("Failed to count players")?;
    let (page, total_pages) = resolve_page(params.page, total, PLAYERS_PER_PAGE);
    let players = user::get_players_paginated(&state.db, active_only, page, PLAYERS_PER_PAGE)
        .await
        .wrap_err("Failed to fetch players")?;

    Ok(page_factory
        .create_page(
            "Players".to_string(),
            Box::new(render_player_directory(
                &players,
                active_only,
                page,
                total_pages,
                total,
            )),
        )
        .with_description("Browse the players competing in the Battlesnake arena."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_user(display_name: Option<&str>) -> User {
        User {
            user_id: uuid::Uuid::nil(),
            external_github_id: 1,
            github_login: "coreyja".to_string(),
            github_avatar_url: None,
            github_name: None,
            github_email: None,
            display_name: display_name.map(str::to_string),
            pronouns: String::new(),
            country: String::new(),
            backstory: String::new(),
            is_admin: false,
            site_theme: "system".to_string(),
            theater_theme: "dark".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn public_name_prefers_display_name() {
        assert_eq!(public_name(&test_user(Some("Corey"))), "Corey");
    }

    #[test]
    fn public_name_falls_back_to_login_when_unset_or_empty() {
        assert_eq!(public_name(&test_user(None)), "coreyja");
        assert_eq!(public_name(&test_user(Some(""))), "coreyja");
    }

    fn directory_entry(user_id: Uuid, login: &str, public_name: &str) -> PlayerDirectoryEntry {
        PlayerDirectoryEntry {
            user_id,
            github_login: login.to_string(),
            public_name: public_name.to_string(),
        }
    }

    /// Parse a query string through the same extractor the route uses.
    fn parse_params(query: &str) -> PlayerDirectoryParams {
        let uri: axum::http::Uri = format!("/players?{query}")
            .parse()
            .expect("test URI should parse");

        Query::<PlayerDirectoryParams>::try_from_uri(&uri)
            .expect("query should deserialize")
            .0
    }

    #[test]
    fn active_filter_accepts_the_shapes_people_actually_type() {
        assert!(!parse_params("").active);
        assert!(parse_params("active=true").active);

        // A public page shouldn't 400 on a plausible hand-edit.
        assert!(parse_params("active=1").active);
        assert!(parse_params("active=yes").active);
        assert!(parse_params("active=On").active);
        assert!(parse_params("active").active);

        // Anything else reads as "off" rather than rejecting the request.
        assert!(!parse_params("active=false").active);
        assert!(!parse_params("active=0").active);
        assert!(!parse_params("active=maybe").active);
    }

    #[test]
    fn rows_link_by_stable_uuid_even_when_logins_collide() {
        // Two distinct accounts whose logins differ only by case: the login
        // alone can't tell them apart, so the UUID has to.
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let players = vec![
            directory_entry(first, "twin", "First Twin"),
            directory_entry(second, "TWIN", "Second Twin"),
        ];

        let html = render_player_directory(&players, false, 0, 1, 2).into_string();

        assert!(html.contains(&format!(r#"href="/users/twin/{first}""#)));
        assert!(html.contains(&format!(r#"href="/users/TWIN/{second}""#)));
        assert!(html.contains("First Twin"));
        assert!(html.contains("Second Twin"));
    }

    #[test]
    fn rows_label_with_the_public_name_fallback() {
        // The model already collapses NULL/empty display names to the login;
        // the row just renders whatever it was handed.
        let players = vec![directory_entry(Uuid::from_u128(3), "nameless", "nameless")];

        let html = render_player_directory(&players, false, 0, 1, 1).into_string();

        assert!(html.contains(r#"class="name""#));
        assert!(html.contains(">nameless</a>"));
    }

    #[test]
    fn empty_states_are_mode_specific_and_omit_a_row_range() {
        let all = render_player_directory(&[], false, 0, 1, 0).into_string();
        assert!(all.contains("No players have joined yet."));
        assert!(!all.contains("Showing"));
        assert!(!all.contains("<table"));
        assert!(!all.contains(r#"class="pager""#));

        let active = render_player_directory(&[], true, 0, 1, 0).into_string();
        assert!(active.contains("No players currently have an active snake."));
        assert!(!active.contains("Showing"));

        // Both modes still offer the toggle, so a visitor who filtered
        // themselves into an empty listing can get back out.
        assert!(active.contains(r#"href="/players">All players</a>"#));
    }

    #[test]
    fn a_page_emptied_by_a_racing_count_keeps_its_pager() {
        // The count said 101 players, but by the time the page was fetched
        // they'd dropped out of the filter. That is not "nobody has joined".
        let html = render_player_directory(&[], true, 1, 3, 101).into_string();

        assert!(html.contains("No players remain on this page."));
        assert!(!html.contains("No players currently have an active snake."));
        assert!(html.contains(r#"href="/players?active=true&amp;page=0""#));
        assert!(!html.contains("Showing"));
    }

    #[test]
    fn filter_toggle_switches_modes_and_drops_the_page() {
        let all = render_player_directory(&[], false, 0, 1, 0).into_string();
        assert!(all.contains(r#"<span class="mode on" aria-current="page">All players</span>"#));
        assert!(all.contains(r#"href="/players?active=true"#));
        assert!(!all.contains("active=true&amp;page="));

        let active = render_player_directory(&[], true, 0, 1, 0).into_string();
        assert!(
            active
                .contains(r#"<span class="mode on" aria-current="page">With active snakes</span>"#)
        );
        assert!(active.contains(r#"href="/players">All players</a>"#));
    }

    #[test]
    fn pager_links_preserve_the_active_filter() {
        let players = vec![directory_entry(Uuid::from_u128(4), "middle", "Middle")];

        // Page 1 of 3, active mode: both Prev and Next carry `active=true`.
        // Maud escapes the query separator, so expect `&amp;`.
        let html = render_player_directory(&players, true, 1, 3, 101).into_string();
        assert!(html.contains(r#"href="/players?active=true&amp;page=0""#));
        assert!(html.contains(r#"href="/players?active=true&amp;page=2""#));
        assert!(html.contains("Page 2 of 3"));
        assert!(html.contains("Showing 51–51 of 101 players"));

        // The same page in "all" mode has no filter to carry.
        let html = render_player_directory(&players, false, 1, 3, 101).into_string();
        assert!(html.contains(r#"href="/players?page=0""#));
        assert!(html.contains(r#"href="/players?page=2""#));
    }
}
