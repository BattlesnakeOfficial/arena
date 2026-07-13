use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    errors::{ServerResult, WithStatus},
    models::{game, saved_game, session},
    routes::auth::CurrentUserWithSession,
    state::AppState,
};

/// Longest title we store; anything past this is silently truncated.
pub const MAX_TITLE_LEN: usize = 100;

#[derive(Deserialize)]
pub struct SaveGameForm {
    pub title: Option<String>,
}

/// Normalize a user-supplied title: trim surrounding whitespace and cap the
/// length at [`MAX_TITLE_LEN`] characters (on a char boundary). A missing
/// title becomes the empty string, which renders as a game-description
/// fallback on the profile.
fn normalize_title(title: Option<&str>) -> String {
    let trimmed = title.unwrap_or_default().trim();
    trimmed.chars().take(MAX_TITLE_LEN).collect()
}

/// POST /games/{id}/save — save a game to the current user's profile (or
/// update the title if they already saved it).
pub async fn save_game(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(game_id): Path<Uuid>,
    Form(form): Form<SaveGameForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Make sure the game exists before upserting (a clean 404 beats an FK error).
    game::get_game_by_id(&state.db, game_id)
        .await?
        .ok_or_else(|| "Game not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let title = normalize_title(form.title.as_deref());

    saved_game::save_game(&state.db, user.user_id, game_id, &title).await?;

    session::set_flash_message(
        &state.db,
        session.session_id,
        "Game saved to your profile".to_string(),
        session::FLASH_TYPE_SUCCESS,
    )
    .await?;

    Ok(Redirect::to(&format!("/games/{game_id}")))
}

/// POST /saved-games/{id}/delete — remove a saved game from the current
/// user's profile. Owner-only: anyone else gets a 404.
pub async fn delete_saved_game(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(saved_game_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let deleted = saved_game::delete_saved_game(&state.db, saved_game_id, user.user_id).await?;
    if !deleted {
        return Err("Saved game not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }

    session::set_flash_message(
        &state.db,
        session.session_id,
        "Removed from your saved games".to_string(),
        session::FLASH_TYPE_SUCCESS,
    )
    .await?;

    Ok(Redirect::to(&format!("/users/{}", user.github_login)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_title_trims_whitespace() {
        assert_eq!(normalize_title(Some("  Epic game  ")), "Epic game");
        assert_eq!(normalize_title(Some("\t\n")), "");
    }

    #[test]
    fn normalize_title_handles_missing_title() {
        assert_eq!(normalize_title(None), "");
    }

    #[test]
    fn normalize_title_caps_length_at_100_chars() {
        let long = "x".repeat(150);
        let normalized = normalize_title(Some(&long));
        assert_eq!(normalized.chars().count(), MAX_TITLE_LEN);

        // Multi-byte characters are counted as chars, not bytes.
        let emoji = "🐍".repeat(150);
        let normalized = normalize_title(Some(&emoji));
        assert_eq!(normalized.chars().count(), MAX_TITLE_LEN);
    }

    #[test]
    fn normalize_title_keeps_short_titles_untouched() {
        assert_eq!(normalize_title(Some("Standard win")), "Standard win");
    }
}
