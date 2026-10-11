use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::notifications::{NotificationChannel, NotificationKind};

#[derive(Debug, Clone)]
pub struct NotificationRecord {
    pub notification_id: Uuid,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
pub struct NotificationCursor {
    pub created_at: DateTime<Utc>,
    pub notification_id: Uuid,
}

pub struct NotificationPage {
    pub items: Vec<NotificationRecord>,
    pub next: Option<NotificationCursor>,
}

pub async fn unread_badge_count(pool: &PgPool, user_id: Uuid) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM (
            SELECT 1 FROM notifications AS n
            WHERE n.user_id = $1 AND n.in_app_visible AND n.read_at IS NULL
              AND NOT EXISTS (
                SELECT 1 FROM notification_preferences AS p
                WHERE p.user_id = n.user_id AND p.kind = n.kind
                  AND p.channel = 'in_app' AND NOT p.enabled
              )
            ORDER BY n.created_at DESC, n.notification_id DESC LIMIT 10
        ) AS eligible"#,
        user_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count unread notifications")?;
    Ok(count)
}

pub async fn list_visible(
    pool: &PgPool,
    user_id: Uuid,
    before: Option<NotificationCursor>,
) -> cja::Result<NotificationPage> {
    let mut items = if let Some(before) = before {
        sqlx::query_as!(
            NotificationRecord,
            r#"SELECT n.notification_id, n.kind, n.title, n.body, n.link, n.created_at, n.read_at
               FROM notifications AS n
               WHERE n.user_id = $1 AND n.in_app_visible
                 AND NOT EXISTS (
                   SELECT 1 FROM notification_preferences AS p
                   WHERE p.user_id = n.user_id AND p.kind = n.kind
                     AND p.channel = 'in_app' AND NOT p.enabled
                 )
                 AND (n.created_at, n.notification_id) < ($2, $3)
               ORDER BY n.created_at DESC, n.notification_id DESC LIMIT 21"#,
            user_id,
            before.created_at,
            before.notification_id
        )
        .fetch_all(pool)
        .await
        .wrap_err("Failed to list notifications after cursor")?
    } else {
        sqlx::query_as!(
            NotificationRecord,
            r#"SELECT n.notification_id, n.kind, n.title, n.body, n.link, n.created_at, n.read_at
               FROM notifications AS n
               WHERE n.user_id = $1 AND n.in_app_visible
                 AND NOT EXISTS (
                   SELECT 1 FROM notification_preferences AS p
                   WHERE p.user_id = n.user_id AND p.kind = n.kind
                     AND p.channel = 'in_app' AND NOT p.enabled
                 )
               ORDER BY n.created_at DESC, n.notification_id DESC LIMIT 21"#,
            user_id
        )
        .fetch_all(pool)
        .await
        .wrap_err("Failed to list notifications")?
    };
    let has_more = items.len() > 20;
    items.truncate(20);
    let next = if has_more {
        items.last().map(|item| NotificationCursor {
            created_at: item.created_at,
            notification_id: item.notification_id,
        })
    } else {
        None
    };
    Ok(NotificationPage { items, next })
}

pub async fn mark_one_read(
    pool: &PgPool,
    user_id: Uuid,
    id: Uuid,
) -> cja::Result<Option<Option<String>>> {
    let row = sqlx::query!(
        r#"UPDATE notifications AS n SET read_at = COALESCE(n.read_at, now())
           WHERE n.notification_id = $1 AND n.user_id = $2 AND n.in_app_visible
             AND NOT EXISTS (
               SELECT 1 FROM notification_preferences AS p
               WHERE p.user_id = n.user_id AND p.kind = n.kind
                 AND p.channel = 'in_app' AND NOT p.enabled
             )
           RETURNING n.link"#,
        id,
        user_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to mark notification read")?;
    Ok(row.map(|row| row.link))
}

pub async fn mark_all_read(pool: &PgPool, user_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        "UPDATE notifications SET read_at = now() WHERE user_id = $1 AND read_at IS NULL",
        user_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to mark all notifications read")?;
    Ok(())
}

pub async fn effective_enabled(
    connection: &mut PgConnection,
    user_id: Uuid,
    kind: NotificationKind,
    channel: NotificationChannel,
) -> cja::Result<bool> {
    if !kind.supports(channel) {
        return Ok(false);
    }
    if !kind.toggleable() {
        return Ok(kind.default_enabled(channel));
    }
    let value = sqlx::query_scalar!(
        "SELECT enabled FROM notification_preferences WHERE user_id = $1 AND kind = $2 AND channel = $3",
        user_id,
        kind.key(),
        channel.key(),
    )
    .fetch_optional(connection)
    .await?;
    Ok(value.unwrap_or(kind.default_enabled(channel)))
}

pub async fn set_preference(
    connection: &mut PgConnection,
    user_id: Uuid,
    kind: NotificationKind,
    channel: NotificationChannel,
    enabled: bool,
) -> cja::Result<()> {
    if !kind.toggleable() || !kind.supports(channel) {
        return Err(color_eyre::eyre::eyre!(
            "Notification setting is not changeable"
        ));
    }
    sqlx::query!(
        "INSERT INTO notification_preferences (user_id, kind, channel, enabled) VALUES ($1, $2, $3, $4) ON CONFLICT (user_id, kind, channel) DO UPDATE SET enabled = EXCLUDED.enabled",
        user_id,
        kind.key(),
        channel.key(),
        enabled,
    )
    .execute(connection)
    .await?;
    Ok(())
}

pub async fn preferences(
    pool: &sqlx::PgPool,
    user_id: Uuid,
) -> cja::Result<Vec<(String, String, bool)>> {
    Ok(sqlx::query!(
        "SELECT kind, channel, enabled FROM notification_preferences WHERE user_id = $1",
        user_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.kind, r.channel, r.enabled))
    .collect())
}
