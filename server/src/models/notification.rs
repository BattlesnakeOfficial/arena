use sqlx::PgConnection;
use uuid::Uuid;

use crate::notifications::{NotificationChannel, NotificationKind};

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
