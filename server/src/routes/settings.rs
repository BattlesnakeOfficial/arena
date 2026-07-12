use axum::{Form, extract::State, http::StatusCode};
use serde::Deserialize;

use crate::{
    errors::ServerResult,
    models::user::{SITE_THEMES, THEATER_THEMES, update_theme_preferences},
    routes::auth::CurrentUser,
    state::AppState,
};

#[derive(Deserialize)]
pub struct AppearanceForm {
    pub site: String,
    pub theater: String,
}

/// POST /settings/appearance — persist the two-axis theme preference for
/// the logged-in user. Called via fetch from /static/theme.js; anonymous
/// visitors keep their choice in localStorage only.
pub async fn update_appearance(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<AppearanceForm>,
) -> ServerResult<StatusCode, StatusCode> {
    if !SITE_THEMES.contains(&form.site.as_str())
        || !THEATER_THEMES.contains(&form.theater.as_str())
    {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY);
    }

    update_theme_preferences(&state.db, user.user_id, &form.site, &form.theater).await?;

    Ok(StatusCode::NO_CONTENT)
}
