//! Per-account rate-limit bookkeeping. Home for feature-level sliding
//! window limits; game creation lives here today, future per-feature
//! limits can sit alongside it.

use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

/// Record a game-creation attempt for `user_id` from `source` (`"web"` or
/// `"api"`), then return how many attempts that user has made within the
/// trailing `window_minutes` — including the just-recorded row, so callers
/// reject when the count is strictly greater than the limit.
///
/// Recording before the count (rather than after a separate check) makes
/// the rate limit race-safe: concurrent attempts each insert first, so the
/// count every request sees reflects the others instead of all reading a
/// stale count and sailing past the gate. Rejected attempts still count,
/// so hammering the endpoint never earns extra games.
///
/// The window is shared per account across both entry points — a web
/// attempt counts against the API budget and vice versa.
pub async fn record_and_count_game_creation_attempts(
    pool: &PgPool,
    user_id: Uuid,
    source: &str,
    window_minutes: i32,
) -> cja::Result<i64> {
    sqlx::query!(
        "INSERT INTO game_creation_attempts (user_id, source) VALUES ($1, $2)",
        user_id,
        source,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record game creation attempt")?;

    let row = sqlx::query!(
        r#"
        SELECT COUNT(*) as "count!"
        FROM game_creation_attempts
        WHERE user_id = $1
          AND attempted_at > NOW() - make_interval(mins => $2)
        "#,
        user_id,
        window_minutes,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count game creation attempts")?;

    Ok(row.count)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
             RETURNING user_id",
            github_id,
            format!("gh-user-{github_id}"),
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn count_includes_just_recorded_attempt_and_climbs(pool: PgPool) -> cja::Result<()> {
        let user = create_user(&pool, 9001).await?;

        // The very first attempt already counts itself.
        let count = record_and_count_game_creation_attempts(&pool, user, "api", 10).await?;
        assert_eq!(count, 1);

        // Web and API attempts share the same per-account window.
        let count = record_and_count_game_creation_attempts(&pool, user, "web", 10).await?;
        assert_eq!(count, 2);

        let count = record_and_count_game_creation_attempts(&pool, user, "api", 10).await?;
        assert_eq!(count, 3);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn attempts_are_isolated_per_user(pool: PgPool) -> cja::Result<()> {
        let user_a = create_user(&pool, 9002).await?;
        let user_b = create_user(&pool, 9003).await?;

        record_and_count_game_creation_attempts(&pool, user_a, "api", 10).await?;
        record_and_count_game_creation_attempts(&pool, user_a, "api", 10).await?;

        // User A's attempts don't count against user B.
        let count = record_and_count_game_creation_attempts(&pool, user_b, "api", 10).await?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn attempts_outside_window_do_not_count(pool: PgPool) -> cja::Result<()> {
        let user = create_user(&pool, 9004).await?;

        // Backdate an attempt to just outside a 10-minute window.
        sqlx::query!(
            "INSERT INTO game_creation_attempts (user_id, source, attempted_at)
             VALUES ($1, 'api', NOW() - INTERVAL '11 minutes')",
            user,
        )
        .execute(&pool)
        .await?;

        // And one just inside it.
        sqlx::query!(
            "INSERT INTO game_creation_attempts (user_id, source, attempted_at)
             VALUES ($1, 'api', NOW() - INTERVAL '9 minutes')",
            user,
        )
        .execute(&pool)
        .await?;

        // Fresh attempt + the 9-minute-old one; the 11-minute-old one has
        // slid out of the window.
        let count = record_and_count_game_creation_attempts(&pool, user, "api", 10).await?;
        assert_eq!(count, 2);

        Ok(())
    }
}
