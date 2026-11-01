use axum::{
    Form,
    extract::{Path, Query, RawForm, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::{Context as _, eyre};
use maud::html;
use serde::Deserialize;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerError, ServerResult, WithStatus},
    flasher::Flasher,
    models::api_token::{Permission, SnakeAccess},
    models::notification_email::{self, ConfirmationRequestResult},
    models::oauth,
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

fn notification_email_error_page(
    page_factory: PageFactory,
    status: StatusCode,
    copy: &'static str,
) -> axum::response::Response {
    (
        status,
        page_factory.create_page("Notification email".into(), Box::new(html! {
            div class="form-page" style="font-size: 16px; max-width: 375px; overflow-wrap: anywhere" {
                p { (copy) }
                a href="/me" class="btn" style="min-height: 44px; font-size: 16px" { "Back to profile" }
            }
        })),
    )
        .into_response()
}

pub async fn save_notification_email(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
    Form(form): Form<NotificationEmailForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let email = form.email.trim();
    if form
        .email
        .bytes()
        .any(|byte| byte == b'\r' || byte == b'\n')
    {
        return Ok(notification_email_error_page(
            page_factory,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Enter a valid email address.",
        ));
    }
    let mut tx = state.db.begin().await?;
    if email.is_empty() {
        notification_email::clear(&mut tx, user.user_id).await?;
        tx.commit().await?;
        return Ok(Redirect::to("/me").into_response());
    }
    if !valid_mailbox(email) {
        return Ok(notification_email_error_page(
            page_factory,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Enter a valid email address.",
        ));
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
        Ok(notification_email_error_page(
            page_factory,
            status,
            "You've requested 5 confirmation emails in the last 24 hours. Try again tomorrow.",
        ))
    }
}

pub async fn resend_notification_email(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
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
        let copy = if status == StatusCode::TOO_MANY_REQUESTS {
            "You've requested 5 confirmation emails in the last 24 hours. Try again tomorrow."
        } else {
            "There's no pending email to resend."
        };
        Ok(notification_email_error_page(page_factory, status, copy))
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
                p { "Confirm this address for notification emails on your signed-in Battlesnake account." }
                form method="post" action="/settings/notification-email/confirm" {
                    input type="hidden" name="token" value=(query.token);
                    button type="submit" class="btn solid" style="min-height: 44px; font-size: 16px" { "Confirm email" }
                }
            } @else {
                p { "Sign in to the same Battlesnake account that requested this change, then reopen the link from your email." }
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
/// the logged-in user. Called via fetch from the theme bundle; anonymous
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

// ---------------------------------------------------------------------------
// Authorized apps (player-facing OAuth grant management)
// ---------------------------------------------------------------------------

/// GET /settings/authorized-apps — every active OAuth grant with its
/// permissions and snake access, plus revoke and edit-snakes forms.
/// Carries anti-framing headers: the snake editor can widen access
/// (selected → all), so clickjacking matters here too.
pub async fn authorized_apps_page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let grants = oauth::list_active_grants(&state.db, user.user_id).await?;

    let snakes =
        crate::models::battlesnake::get_battlesnakes_by_user_id(&state.db, user.user_id).await?;

    let flash = page_factory.flash.clone();
    let page = page_factory.create_page_with_flash(
        "Authorized Apps".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "Authorized Apps" }
                div class="sub" {
                    "Applications you've authorized to act as you. Revoking signs them "
                    "out everywhere, immediately."
                }
            }

            @if grants.is_empty() {
                p class="empty-state" { "You haven't authorized any applications." }
            }

            @for grant in &grants {
                article class="section" aria-label=(grant.client_name) {
                    div style="display: flex; align-items: center; gap: 12px; flex-wrap: wrap;" {
                        @if let Some(logo) = oauth::renderable_https_url(&grant.client_logo_url) {
                            img src=(logo) alt="" width="44" height="44"
                                style="width: 44px; height: 44px; border-radius: 8px; object-fit: cover;";
                        }
                        h3 style="margin: 0;" { (grant.client_name) }
                        span class="badge" { "authorized " (grant.created_at.format("%b %d, %Y")) }
                    }

                    h4 style="margin: 18px 0 6px;" { "Can:" }
                    ul style="list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 6px;" {
                        @if grant.permissions.is_empty() {
                            li { "Nothing (no permissions granted)." }
                        }
                        @for permission in Permission::ALL {
                            @if grant.permissions.contains(&permission) {
                                li { (permission.description()) }
                            }
                        }
                    }

                    form method="post" class="form-stack" style="max-width: 480px; margin-top: 20px;"
                        action=(format!("/settings/authorized-apps/{}/snakes", grant.grant_id)) {
                        div class="field" {
                            label { "Snake access" }
                            label style="display: flex; gap: 12px; align-items: center; font-size: 16px; min-height: 44px; cursor: pointer; margin-top: 8px;" {
                                input type="radio" name="snake_access" value="all"
                                    checked[grant.snake_access == SnakeAccess::All]
                                    style="width: 22px; height: 22px; flex: none;";
                                span { "All of my snakes" }
                            }
                            label style="display: flex; gap: 12px; align-items: center; font-size: 16px; min-height: 44px; cursor: pointer;" {
                                input type="radio" name="snake_access" value="selected"
                                    checked[grant.snake_access == SnakeAccess::Selected]
                                    style="width: 22px; height: 22px; flex: none;";
                                span { "Only the snakes I pick" }
                            }
                        }
                        @if snakes.is_empty() {
                            p class="help" { "You have no battlesnakes." }
                        } @else {
                            ul style="list-style: none; padding: 0; margin: 12px 0 0; display: flex; flex-direction: column; gap: 4px;" {
                                @for snake in &snakes {
                                    li {
                                        label style="display: flex; gap: 12px; align-items: center; font-size: 16px; min-height: 44px; cursor: pointer;" {
                                            input type="checkbox" name="snake_ids" value=(snake.battlesnake_id)
                                                checked[grant.snake_access == SnakeAccess::All
                                                    || grant.snake_ids.contains(&snake.battlesnake_id)]
                                                style="width: 22px; height: 22px; flex: none;";
                                            span { (snake.name) }
                                        }
                                    }
                                }
                            }
                            p class="help" {
                                "Changing the snake selection takes effect immediately — the app's "
                                "token can only act on the checked snakes from its next request."
                            }
                        }
                        button type="submit" class="btn" style="min-height: 44px; font-size: 16px;" {
                            "Save snake selection"
                        }
                    }

                    form method="post" style="margin-top: 18px;"
                        action=(format!("/settings/authorized-apps/{}/revoke", grant.grant_id)) {
                        button type="submit" class="btn danger" style="min-height: 44px; font-size: 16px;" {
                            "Revoke " (grant.client_name)
                        }
                    }
                }
            }
        }),
        flash,
    );
    Ok((
        [
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        ],
        page,
    )
        .into_response())
}

/// Redirect back to the Authorized apps page carrying the anti-framing
/// headers — the page and its POSTs all refuse framing (the snake editor
/// can widen access, so clickjacking matters here too).
fn back_to_authorized_apps() -> axum::response::Response {
    (
        [
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
        ],
        Redirect::to("/settings/authorized-apps"),
    )
        .into_response()
}

/// POST /settings/authorized-apps/{id}/revoke — revoke a grant and its
/// tokens immediately.
pub async fn revoke_authorized_app(
    State(state): State<AppState>,
    CurrentUser(_user): CurrentUser,
    flasher: Flasher,
    Path(grant_id): Path<uuid::Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    if !oauth::revoke_grant(&state.db, grant_id, _user.user_id).await? {
        return Err("Authorized app not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }
    flasher
        .success("Application revoked. Its access stops working immediately.")
        .await
        .wrap_err("Failed to flash")?;
    Ok(back_to_authorized_apps())
}

/// POST /settings/authorized-apps/{id}/snakes — edit a grant's snake
/// selection (both directions: all ↔ selected). Repeated `snake_ids` keys
/// parsed manually (axum's `Form` can't do repeated keys). Does not revoke
/// or re-issue the token: the new selection takes effect on its next use.
pub async fn edit_authorized_app_snakes(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    flasher: Flasher,
    Path(grant_id): Path<uuid::Uuid>,
    RawForm(form_bytes): RawForm,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&form_bytes)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    let snake_access = match field("snake_access") {
        Some("all") => SnakeAccess::All,
        Some("selected") => SnakeAccess::Selected,
        _ => {
            flasher
                .error("Pick a snake access option.")
                .await
                .wrap_err("Failed to flash")?;
            return Ok(back_to_authorized_apps());
        }
    };

    let mut snake_ids: std::collections::BTreeSet<uuid::Uuid> = Default::default();
    let mut malformed = false;
    for (_, value) in form.iter().filter(|(key, _)| key == "snake_ids") {
        match uuid::Uuid::parse_str(value) {
            Ok(id) => {
                snake_ids.insert(id);
            }
            Err(_) => malformed = true,
        }
    }

    let id_list: Vec<uuid::Uuid> = snake_ids.iter().copied().collect();
    let owned = if id_list.is_empty() {
        true
    } else {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64"
               FROM battlesnakes
               WHERE battlesnake_id = ANY($1) AND user_id = $2 AND deleted_at IS NULL"#,
            &id_list,
            user.user_id
        )
        .fetch_one(&state.db)
        .await?
            == snake_ids.len() as i64
    };

    let snake_ids: Vec<uuid::Uuid> = if snake_access == SnakeAccess::All {
        Vec::new()
    } else {
        snake_ids.into_iter().collect()
    };

    let updated = !malformed
        && owned
        && oauth::set_grant_snakes(&state.db, grant_id, user.user_id, snake_access, &snake_ids)
            .await?;

    if updated {
        flasher
            .success("Snake selection updated. It takes effect immediately.")
            .await
            .wrap_err("Failed to flash")?;
    } else {
        flasher
            .error("Couldn't update the snake selection — is the app still authorized, and are all snakes yours?")
            .await
            .wrap_err("Failed to flash")?;
    }
    Ok(back_to_authorized_apps())
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
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router();
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
        assert_eq!(html.matches("Turn off").count(), 6);
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
            NotificationKind::AchievementEarned,
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
        let mut connection = pool.acquire().await?;
        assert!(
            !crate::models::notification::effective_enabled(
                &mut connection,
                user_id,
                NotificationKind::AchievementEarned,
                NotificationChannel::Email,
            )
            .await?
        );
        assert!(
            crate::models::notification::effective_enabled(
                &mut connection,
                user_id,
                NotificationKind::SnakeMatchmakingResumed,
                NotificationChannel::Email,
            )
            .await?
        );
        drop(connection);
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
        let bad = token.replace("achievement_earned", "snake_matchmaking_paused");
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

    async fn assert_notification_email_error(
        response: axum::response::Response,
        status: StatusCode,
        copy: &str,
    ) -> cja::Result<()> {
        assert_eq!(response.status(), status);
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains(copy), "missing copy: {copy}");
        assert!(html.contains("href=\"/me\""));
        Ok(())
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
        let app = crate::routes::routes(state)
            .expect("asset router")
            .into_router();
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
        assert_notification_email_error(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email/resend",
                "",
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "There's no pending email to resend.",
        )
        .await?;
        for invalid in [
            "email=bad%0D%0A%40example.com",
            "email=bad%40example.com%2Cother%40example.com",
            "email=no-address",
        ] {
            assert_notification_email_error(
                request(
                    &app,
                    Some(&cookie),
                    Method::POST,
                    "/settings/notification-email",
                    invalid,
                )
                .await,
                StatusCode::UNPROCESSABLE_ENTITY,
                "Enter a valid email address.",
            )
            .await?;
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
        let response = request(&app, Some(&cookie), Method::GET, "/me", "").await;
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains("id=\"notification-email\""));
        assert!(html.contains("value=\"one@example.com\""));
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
        assert_eq!(
            crate::models::notification_email::settings(&pool, user_id)
                .await?
                .pending_email
                .as_deref(),
            Some("one@example.com")
        );
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
                "email=replacement%40example.com"
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        let response = request(&app, Some(&cookie), Method::GET, "/me", "").await;
        let html = String::from_utf8(to_bytes(response.into_body(), usize::MAX).await?.to_vec())?;
        assert!(html.contains("value=\"replacement@example.com\""));
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
                "email=replacement%40example.com"
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
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
            Some("replacement@example.com")
        );
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
        for address in ["three", "four"] {
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
        assert_notification_email_error(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email",
                "email=sixth%40example.com",
            )
            .await,
            StatusCode::TOO_MANY_REQUESTS,
            "You've requested 5 confirmation emails in the last 24 hours. Try again tomorrow.",
        )
        .await?;
        assert_notification_email_error(
            request(
                &app,
                Some(&cookie),
                Method::POST,
                "/settings/notification-email/resend",
                "",
            )
            .await,
            StatusCode::TOO_MANY_REQUESTS,
            "You've requested 5 confirmation emails in the last 24 hours. Try again tomorrow.",
        )
        .await?;
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
