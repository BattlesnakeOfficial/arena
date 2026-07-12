use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use color_eyre::eyre::Context as _;
use maud::html;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerResult, WithStatus},
    models::{
        battlesnake::{self, Visibility},
        leaderboard,
        user::{self, User},
    },
    routes::auth::OptionalUser,
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

/// GET /users/{login} — public user profile.
///
/// Everything here is visible to anonymous visitors, matching the public
/// profiles on play.battlesnake.com: identity fields the user chose to share,
/// their snakes, and where those snakes sit on the leaderboards. Snake
/// visibility only controls matchmaking eligibility, not who can see it, so
/// all snakes are listed (private ones badged).
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
        }),
    ))
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
}
