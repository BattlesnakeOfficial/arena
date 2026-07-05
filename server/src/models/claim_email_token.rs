//! One-time magic-link tokens for the email-recovery claim path (BS-7e38).
//!
//! Same secret handling as [`crate::models::api_token`]: a 32-byte random
//! secret goes into the email link, only its SHA-256 lands in the database.
//! Consumption is a compare-and-set on `used_at`, so a link works exactly
//! once, and only for the arena user who requested it.

use color_eyre::eyre::Context as _;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// How long a recovery link stays valid. Short on purpose: the owner is
/// sitting in the flow when it's sent.
pub const CLAIM_TOKEN_TTL_MINUTES: i64 = 30;

fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Mint a recovery token binding `imported_account_id` to the requesting
/// arena user. Returns the raw secret for the email link — it is never
/// stored and can't be recovered later.
pub async fn create(
    pool: &PgPool,
    imported_account_id: Uuid,
    requested_by_user_id: Uuid,
) -> cja::Result<String> {
    let secret = generate_secret();

    sqlx::query!(
        r#"INSERT INTO claim_email_tokens
            (token_hash, imported_account_id, requested_by_user_id, expires_at)
         VALUES ($1, $2, $3, NOW() + make_interval(mins => $4))"#,
        hash_secret(&secret),
        imported_account_id,
        requested_by_user_id,
        CLAIM_TOKEN_TTL_MINUTES as i32,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to create claim email token")?;

    Ok(secret)
}

/// Redeem a token: compare-and-set `used_at` and return the imported
/// account it unlocks. `None` when the token is unknown, expired, already
/// used, or was requested by a different user — the caller shows one
/// uniform "invalid or expired" message for all four, so the response
/// doesn't reveal which check failed.
pub async fn consume(pool: &PgPool, secret: &str, user_id: Uuid) -> cja::Result<Option<Uuid>> {
    let imported_account_id = sqlx::query_scalar!(
        r#"UPDATE claim_email_tokens
         SET used_at = NOW()
         WHERE token_hash = $1
           AND requested_by_user_id = $2
           AND used_at IS NULL
           AND expires_at > NOW()
         RETURNING imported_account_id"#,
        hash_secret(secret),
        user_id,
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to consume claim email token")?;

    Ok(imported_account_id)
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

    async fn create_imported_account(pool: &PgPool, n: u32) -> cja::Result<Uuid> {
        let id = sqlx::query_scalar!(
            "INSERT INTO imported_accounts (play_user_id, play_account_id, email, username)
             VALUES ($1, $2, $3, $4)
             RETURNING imported_account_id",
            format!("usr_t{n}"),
            format!("act_t{n}"),
            format!("player{n}@example.com"),
            format!("player{n}"),
        )
        .fetch_one(pool)
        .await?;
        Ok(id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn token_consumes_exactly_once_for_the_requesting_user(pool: PgPool) -> cja::Result<()> {
        let user = create_user(&pool, 8001).await?;
        let account = create_imported_account(&pool, 1).await?;

        let secret = create(&pool, account, user).await?;
        // The raw secret never hits the DB.
        let stored: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) as \"c!\" FROM claim_email_tokens WHERE token_hash = $1",
            secret
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored, 0);

        assert_eq!(consume(&pool, &secret, user).await?, Some(account));
        // Single-use: the CAS refuses a replay.
        assert_eq!(consume(&pool, &secret, user).await?, None);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn token_is_bound_to_the_requesting_user(pool: PgPool) -> cja::Result<()> {
        let requester = create_user(&pool, 8002).await?;
        let someone_else = create_user(&pool, 8003).await?;
        let account = create_imported_account(&pool, 2).await?;

        let secret = create(&pool, account, requester).await?;

        // A different logged-in user can't redeem a forwarded link…
        assert_eq!(consume(&pool, &secret, someone_else).await?, None);
        // …and the failed attempt didn't burn it for the real requester.
        assert_eq!(consume(&pool, &secret, requester).await?, Some(account));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn expired_and_bogus_tokens_are_rejected(pool: PgPool) -> cja::Result<()> {
        let user = create_user(&pool, 8004).await?;
        let account = create_imported_account(&pool, 3).await?;

        let secret = create(&pool, account, user).await?;
        sqlx::query!("UPDATE claim_email_tokens SET expires_at = NOW() - INTERVAL '1 minute'",)
            .execute(&pool)
            .await?;
        assert_eq!(consume(&pool, &secret, user).await?, None);

        assert_eq!(consume(&pool, "not-a-real-token", user).await?, None);

        Ok(())
    }
}
