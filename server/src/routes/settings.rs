use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use maud::html;
use serde::Deserialize;

use crate::{
    errors::ServerResult,
    models::user::{SITE_THEMES, THEATER_THEMES, update_theme_preferences},
    notifications::{self, NotificationChannel, NotificationKind},
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationPreferenceForm {
    kind: String,
    channel: String,
    enabled: String,
}

pub async fn update_notification_preference(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<NotificationPreferenceForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let Some(kind) = NotificationKind::parse(&form.kind) else {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    };
    let Some(channel) = NotificationChannel::parse(&form.channel) else {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    };
    let enabled = match form.enabled.as_str() {
        "true" => true,
        "false" => false,
        _ => return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response()),
    };
    if !kind.toggleable() || !kind.supports(channel) {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    }
    let mut connection = state.db.acquire().await?;
    crate::models::notification::set_preference(
        &mut connection,
        user.user_id,
        kind,
        channel,
        enabled,
    )
    .await?;
    Ok(Redirect::to("/me").into_response())
}

async fn apply_unsubscribe(state: &AppState, token: &str) -> Result<NotificationKind, StatusCode> {
    let (user_id, kind) = notifications::verify_unsubscribe_token(&state.cookie_key, token)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let exists = sqlx::query_scalar!(
        "SELECT user_id FROM users WHERE user_id = $1 FOR KEY SHARE",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if exists.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    crate::models::notification::set_preference(
        &mut tx,
        user_id,
        kind,
        NotificationChannel::Email,
        false,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(kind)
}

pub async fn unsubscribe_get(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let kind = apply_unsubscribe(&state, &token).await?;
    Ok(html! {
        html { head { meta name="viewport" content="width=device-width, initial-scale=1"; title { "Unsubscribed" } }
        body style="font: 16px system-ui; margin: 2rem auto; padding: 1rem; max-width: 38rem" {
            h1 { "Unsubscribed" }
            p { "Email for " (kind.name()) " is off." }
            a href="/me" { "Notification settings" }
        } }
    })
}

pub async fn unsubscribe_post(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<StatusCode, StatusCode> {
    apply_unsubscribe(&state, &token).await?;
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    #[sqlx::test(migrations = "../migrations")]
    async fn settings_and_unsubscribe_routes(pool: PgPool) -> cja::Result<()> {
        use crate::routes::test_support::{
            create_user_session, session_user_id, signed_session_cookie,
        };
        let state = AppState::test_from_pool(pool.clone());
        let session_id = create_user_session(&pool, 1673010, false).await;
        let user_id = session_user_id(&pool, session_id).await;
        let cookie = signed_session_cookie(&state, session_id);
        let app =
            crate::routes::routes(state.clone()).layer(tower_cookies::CookieManagerLayer::new());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/me")
                    .header(
                        header::COOKIE,
                        format!("{}={cookie}", crate::models::session::SESSION_COOKIE_NAME),
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        for kind in NotificationKind::ALL {
            assert!(html.contains(kind.name()));
        }
        assert!(html.contains("Always on"));
        assert_eq!(html.matches("Turn off").count(), 4);
        let form = "kind=snake_matchmaking_paused&channel=email&enabled=false";
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/settings/notifications")
                    .header(
                        header::COOKIE,
                        format!("{}={cookie}", crate::models::session::SESSION_COOKIE_NAME),
                    )
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let mut connection = pool.acquire().await?;
        assert!(
            !crate::models::notification::effective_enabled(
                &mut connection,
                user_id,
                NotificationKind::SnakeMatchmakingPaused,
                NotificationChannel::Email
            )
            .await?
        );
        drop(connection);
        let token = notifications::unsubscribe_token(
            &state.cookie_key,
            user_id,
            NotificationKind::SnakeMatchmakingResumed,
        );
        let before_sessions = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM sessions")
            .fetch_one(&pool)
            .await?;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/notifications/unsubscribe/{token}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/notifications/unsubscribe/{token}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains("/me"));
        let after_sessions = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM sessions")
            .fetch_one(&pool)
            .await?;
        assert_eq!(before_sessions, after_sessions);
        let bad = token.replace("resumed", "paused");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/notifications/unsubscribe/{bad}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        sqlx::query!("DELETE FROM sessions WHERE user_id = $1", user_id)
            .execute(&pool)
            .await?;
        sqlx::query!("DELETE FROM users WHERE user_id = $1", user_id)
            .execute(&pool)
            .await?;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/notifications/unsubscribe/{token}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let other_session = create_user_session(&pool, 1673011, false).await;
        let other_cookie = signed_session_cookie(&state, other_session);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/settings/notifications")
                    .header(
                        header::COOKIE,
                        format!(
                            "{}={other_cookie}",
                            crate::models::session::SESSION_COOKIE_NAME
                        ),
                    )
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "kind=claim_verification&channel=email&enabled=false",
                    ))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/settings/notifications")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))?,
            )
            .await?;
        assert_ne!(response.status(), StatusCode::SEE_OTHER);
        Ok(())
    }
}
