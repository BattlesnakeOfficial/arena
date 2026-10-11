use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use chrono::{DateTime, SecondsFormat, Utc};
use chrono_humanize::HumanTime;
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    errors::ServerResult,
    models::notification::{self, NotificationCursor, NotificationPage, NotificationRecord},
    notifications::{self as notification_logic, NotificationKind},
    routes::auth::CurrentUser,
    state::AppState,
};

#[derive(Deserialize)]
pub struct CursorQuery {
    before: Option<String>,
    before_id: Option<String>,
}

impl CursorQuery {
    fn parse(self) -> Result<Option<NotificationCursor>, StatusCode> {
        match (self.before, self.before_id) {
            (None, None) => Ok(None),
            (Some(before), Some(before_id)) => {
                let created_at = DateTime::parse_from_rfc3339(&before)
                    .map_err(|_| StatusCode::BAD_REQUEST)?
                    .with_timezone(&Utc);
                let notification_id =
                    Uuid::parse_str(&before_id).map_err(|_| StatusCode::BAD_REQUEST)?;
                Ok(Some(NotificationCursor {
                    created_at,
                    notification_id,
                }))
            }
            _ => Err(StatusCode::BAD_REQUEST),
        }
    }
}

fn cursor_parts(cursor: NotificationCursor) -> (String, String) {
    (
        cursor
            .created_at
            .to_rfc3339_opts(SecondsFormat::Micros, true),
        cursor.notification_id.to_string(),
    )
}

fn cursor_href(cursor: NotificationCursor) -> String {
    let (before, before_id) = cursor_parts(cursor);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("before", &before)
        .append_pair("before_id", &before_id)
        .finish();
    format!("/notifications?{query}")
}

fn no_store() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers
}

#[derive(Serialize)]
pub struct BadgeCount {
    count: i64,
}

pub async fn count(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let count = notification::unread_badge_count(&state.db, user.user_id)
        .await
        .wrap_err("Failed to count unread notifications")?;
    Ok((no_store(), Json(BadgeCount { count })))
}

pub async fn page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
    Query(query): Query<CursorQuery>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let before = query.parse().map_err(|status| {
        crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid notification cursor"),
            status,
        )
    })?;
    let list = notification::list_visible(&state.db, user.user_id, before)
        .await
        .wrap_err("Failed to list notifications")?;
    let content = html! {
        section class="notifications-page" {
            h1 { "Notifications" }
            @if list.items.is_empty() {
                @if before.is_some() {
                    p class="notification-empty" { "No older notifications." }
                    a href="/notifications" { "Back to notifications" }
                } @else {
                    p class="notification-empty" { "No notifications yet." }
                }
            } @else {
                (mark_all_form())
                (items(&list.items))
            }
            @if let Some(next) = list.next {
                a class="notification-next" href=(cursor_href(next)) { "Next page" }
            }
        }
    };
    Ok(page_factory.create_page("Notifications".into(), Box::new(content)))
}

pub async fn drawer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let list = notification::list_visible(&state.db, user.user_id, None)
        .await
        .wrap_err("Failed to list notifications")?;
    Ok((
        no_store(),
        html! {
            @if list.items.is_empty() {
                p class="notification-empty" { "No notifications yet." }
            } @else {
                (mark_all_form()) (batch(&list))
            }
        },
    ))
}

pub async fn more(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<CursorQuery>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let before = query.parse().map_err(|status| {
        crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid notification cursor"),
            status,
        )
    })?;
    let list = notification::list_visible(&state.db, user.user_id, before)
        .await
        .wrap_err("Failed to list notifications")?;
    Ok((no_store(), batch(&list)))
}

pub async fn open(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> ServerResult<Response, StatusCode> {
    let id = Uuid::parse_str(&id).map_err(|_| {
        crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid notification id"),
            StatusCode::NOT_FOUND,
        )
    })?;
    let Some(link) = notification::mark_one_read(&state.db, user.user_id, id)
        .await
        .wrap_err("Failed to mark notification read")?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let target = link
        .as_deref()
        .filter(|link| notification_logic::valid_local_link(link))
        .unwrap_or("/notifications");
    Ok(Redirect::to(target).into_response())
}

pub async fn read_all(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ServerResult<impl IntoResponse, StatusCode> {
    notification::mark_all_read(&state.db, user.user_id)
        .await
        .wrap_err("Failed to mark all notifications read")?;
    Ok(Redirect::to("/notifications"))
}

fn mark_all_form() -> Markup {
    html! {
        form class="notification-mark-all" action="/notifications/read-all" method="post" {
            button type="submit" { "Mark all as read" }
        }
    }
}

fn batch(list: &NotificationPage) -> Markup {
    html! {
        div class="notification-batch" {
            (items(&list.items))
            @if let Some(next) = list.next {
                @let (before, before_id) = cursor_parts(next);
                button class="notification-load-more" type="button" data-before=(before) data-before-id=(before_id) { "Load more" }
            }
        }
    }
}

fn items(items: &[NotificationRecord]) -> Markup {
    html! {
        ul class="notification-items" {
            @for item in items { (item_markup(item)) }
        }
    }
}

fn item_markup(item: &NotificationRecord) -> Markup {
    let icon = match NotificationKind::parse(&item.kind) {
        Some(NotificationKind::SnakeMatchmakingPaused) => "⏸",
        Some(NotificationKind::SnakeMatchmakingResumed) => "▶",
        Some(NotificationKind::PlayAccountClaimed) => "♟",
        Some(NotificationKind::ClaimVerification) => "✓",
        Some(NotificationKind::NotificationEmailConfirmation) => "✉",
        None => "●",
    };
    let body: String = item.body.chars().take(160).collect();
    let shortened = item.body.chars().count() > 160;
    html! {
        li class=(if item.read_at.is_none() { "notification-item unread" } else { "notification-item" }) {
            form action=(format!("/notifications/{}/open", item.notification_id)) method="post" {
                button type="submit" {
                    span class="visually-hidden" { "Open notification: " }
                    @if item.read_at.is_none() { span class="visually-hidden notification-unread-label" { "Unread. " } }
                    span class="notification-kind-icon" aria-hidden="true" { (icon) }
                    span class="notification-item-copy" {
                        span class="notification-item-title" { (item.title) }
                        span class="notification-item-body" { (body) @if shortened { "…" } }
                        time datetime=(item.created_at.to_rfc3339_opts(SecondsFormat::Micros, true)) {
                            (HumanTime::from(item.created_at).to_string())
                        }
                    }
                    @if item.read_at.is_none() { span class="notification-unread-marker" aria-hidden="true" { "New" } }
                }
            }
        }
    }
}
