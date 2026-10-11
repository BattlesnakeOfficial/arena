use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::{Context as _, eyre};
use maud::html;
use serde::Deserialize;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerError, ServerResult},
    models::notification_email::{self, ConfirmationRequestResult},
    models::user::{SITE_THEMES, THEATER_THEMES, update_theme_preferences},
    notifications::{self, NotificationChannel, NotificationKind},
    routes::auth::{CurrentUser, OptionalUser},
    state::AppState,
};

#[derive(Deserialize)]
pub struct NotificationEmailForm {
    email: String,
}

fn valid_mailbox(email: &str) -> bool {
    if email.len() > 254
        || email
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    let mut parts = email.split('@');
    matches!((parts.next(), parts.next(), parts.next()), (Some(local), Some(domain), None) if !local.is_empty() && !domain.is_empty() && !local.contains([',', '<', '>']) && !domain.contains([',', '<', '>']))
}

fn request_status(result: &ConfirmationRequestResult) -> StatusCode {
    match result {
        ConfirmationRequestResult::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ConfirmationRequestResult::NothingPending => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::SEE_OTHER,
    }
}

pub async fn save_notification_email(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<NotificationEmailForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let email = form.email.trim();
    if form
        .email
        .bytes()
        .any(|byte| byte == b'\r' || byte == b'\n')
    {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    }
    let mut tx = state.db.begin().await?;
    if email.is_empty() {
        notification_email::clear(&mut tx, user.user_id).await?;
        tx.commit().await?;
        return Ok(Redirect::to("/me").into_response());
    }
    if !valid_mailbox(email) {
        return Ok(StatusCode::UNPROCESSABLE_ENTITY.into_response());
    }
    let result = notification_email::request_confirmation(&mut tx, user.user_id, email).await?;
    let status = request_status(&result);
    tx.commit().await?;
    if let ConfirmationRequestResult::Requested { receipt, .. } = result {
        receipt.log_enqueued();
    }
    if status == StatusCode::SEE_OTHER {
        Ok(Redirect::to("/me").into_response())
    } else {
        Ok(status.into_response())
    }
}

pub async fn resend_notification_email(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let mut tx = state.db.begin().await?;
    let result = notification_email::resend(&mut tx, user.user_id).await?;
    let status = request_status(&result);
    tx.commit().await?;
    if let ConfirmationRequestResult::Requested { receipt, .. } = result {
        receipt.log_enqueued();
    }
    if status == StatusCode::SEE_OTHER {
        Ok(Redirect::to("/me").into_response())
    } else {
        Ok(status.into_response())
    }
}

#[derive(Deserialize)]
pub struct ConfirmationToken {
    token: String,
}

pub async fn confirm_notification_email_page(
    OptionalUser(user): OptionalUser,
    page_factory: PageFactory,
    Query(query): Query<ConfirmationToken>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page("Confirm notification email".into(), Box::new(html! {
        div class="form-page" style="font-size: 16px; max-width: 375px; overflow-wrap: anywhere" {
            h1 { "Confirm notification email" }
            @if user.is_some() {
                p { "Confirm this address for notification emails on your signed-in Arena account." }
                form method="post" action="/settings/notification-email/confirm" {
                    input type="hidden" name="token" value=(query.token);
                    button type="submit" class="btn solid" style="min-height: 44px; font-size: 16px" { "Confirm email" }
                }
            } @else {
                p { "Sign in to the same Arena account that requested this change, then reopen the link from your email." }
                a href="/auth/github" class="btn" style="min-height: 44px; font-size: 16px" { "Sign in with GitHub" }
            }
        }
    })))
}

#[derive(Deserialize)]
pub struct ConfirmationPost {
    token: String,
}

pub async fn confirm_notification_email(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<ConfirmationPost>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    if notification_email::consume(&state.db, user.user_id, &form.token).await? {
        Ok(Redirect::to("/me").into_response())
    } else {
        Ok((
            StatusCode::BAD_REQUEST,
            "Invalid or expired confirmation link",
        )
            .into_response())
    }
}

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

async fn apply_unsubscribe(
    state: &AppState,
    token: &str,
) -> ServerResult<NotificationKind, StatusCode> {
    let (user_id, kind) = notifications::verify_unsubscribe_token(&state.cookie_key, token)
        .ok_or_else(|| {
            ServerError(
                eyre!("Invalid notification unsubscribe link"),
                StatusCode::BAD_REQUEST,
            )
        })?;
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin notification unsubscribe transaction")?;
    let exists = sqlx::query_scalar!(
        "SELECT user_id FROM users WHERE user_id = $1 FOR KEY SHARE",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to check notification unsubscribe user")?;
    if exists.is_none() {
        return Err(ServerError(
            eyre!("Invalid notification unsubscribe link"),
            StatusCode::BAD_REQUEST,
        ));
    }
    crate::models::notification::set_preference(
        &mut tx,
        user_id,
        kind,
        NotificationChannel::Email,
        false,
    )
    .await
    .wrap_err("Failed to save notification unsubscribe preference")?;
    tx.commit()
        .await
        .wrap_err("Failed to commit notification unsubscribe preference")?;
    Ok(kind)
}

pub async fn unsubscribe_get(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ServerResult<impl IntoResponse, StatusCode> {
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
) -> ServerResult<StatusCode, StatusCode> {
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

    async fn request(
        app: &axum::Router,
        cookie: Option<&str>,
        method: Method,
        uri: &str,
        body: &str,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_string())).expect("request"))
            .await
            .expect("route")
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn notification_email_routes_require_auth_and_expose_pending_state(
        pool: PgPool,
    ) -> cja::Result<()> {
        use crate::routes::test_support::{
            create_user_session, session_user_id, signed_session_cookie,
        };
        let state = AppState::test_from_pool(pool.clone());
        let session = create_user_session(&pool, 1674093, false).await;
        let user_id = session_user_id(&pool, session).await;
        let cookie = format!(
            "{}={}",
            crate::models::session::SESSION_COOKIE_NAME,
            signed_session_cookie(&state, session)
        );
        let app = crate::routes::routes(state).layer(tower_cookies::CookieManagerLayer::new());
        let response = request(
            &app,
            None,
            Method::POST,
            "/settings/notification-email",
            "email=one%40example.com",
        )
        .await;
        assert_ne!(response.status(), StatusCode::SEE_OTHER);
        let response = request(
            &app,
            None,
            Method::GET,
            "/settings/notification-email/confirm?token=secret",
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains("reopen the link"));
        assert!(!html.contains("secret"));
        let response = request(
            &app,
            Some(&cookie),
            Method::GET,
            "/settings/notification-email/confirm?token=secret",
            "",
        )
        .await;
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains("name=\"token\""));
        assert!(html.contains("value=\"secret\""));
        for invalid in [
            "email=bad%0D%0A%40example.com",
            "email=bad%40example.com%2Cother%40example.com",
            "email=no-address",
        ] {
            assert_eq!(
                request(
                    &app,
                    Some(&cookie),
                    Method::POST,
                    "/settings/notification-email",
                    invalid
                )
                .await
                .status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email",
                "email=one%40example.com"
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        let before = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email",
                "email=ONE%40example.com"
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        let after = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(before, after);
        let response = request(&app, Some(&cookie), Method::GET, "/me", "").await;
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains("pending confirmation"));
        assert!(html.contains("Add an email to get notifications"));
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email/resend",
                ""
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        let secret = crate::models::notification_email::new_secret();
        sqlx::query!("UPDATE notification_email_requests SET token_hash = $2, expires_at = NOW() + INTERVAL '24 hours' WHERE request_id = (SELECT pending_notification_email_request_id FROM users WHERE user_id = $1)", user_id, crate::models::notification_email::hash_secret(&secret)).execute(&pool).await?;
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email/confirm",
                "token=bad"
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email/confirm",
                &format!("token={secret}")
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        let email = sqlx::query_scalar!(
            "SELECT notification_email FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(email.as_deref(), Some("one@example.com"));
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email",
                "email="
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        assert!(
            sqlx::query_scalar!(
                "SELECT notification_email FROM users WHERE user_id = $1",
                user_id
            )
            .fetch_one(&pool)
            .await?
            .is_none()
        );
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notifications",
                "kind=notification_email_confirmation&channel=email&enabled=false"
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        for address in ["two", "three", "four"] {
            assert_eq!(
                request(
                    &app,
                    Some(&cookie),
                    Method::POST,
                    "/settings/notification-email",
                    &format!("email={address}%40example.com")
                )
                .await
                .status(),
                StatusCode::SEE_OTHER
            );
        }
        let before = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(before, 5);
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email",
                "email=sixth%40example.com"
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email/resend",
                ""
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            sqlx::query_scalar!(
                "SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1",
                user_id
            )
            .fetch_one(&pool)
            .await?,
            before
        );
        assert_eq!(
            crate::models::notification_email::settings(&pool, user_id)
                .await?
                .pending_email
                .as_deref(),
            Some("four@example.com")
        );
        Ok(())
    }
}
