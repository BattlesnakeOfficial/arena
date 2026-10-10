use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use rand::{Rng, rngs::OsRng};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::customizations::{Head, Tail};

const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub &'static str);

#[derive(Debug)]
pub struct CodeListing {
    pub code_id: Uuid,
    pub customization_type: String,
    pub slug: String,
    pub max_redemptions: i32,
    pub redemptions_used: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

pub struct CreateCode {
    pub customization_type: String,
    pub slug: String,
    pub max_redemptions: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub note: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RedeemOutcome {
    Granted {
        name: &'static str,
        kind: &'static str,
        slug: &'static str,
    },
    Unknown,
    Expired,
    Disabled,
    FullyRedeemed,
    AlreadyRedeemed,
    AlreadyOwned,
    RateLimited,
}

fn catalog_item(
    kind: &str,
    slug: &str,
) -> Option<(&'static str, &'static str, &'static str, bool)> {
    match kind {
        Head::KIND => Head::from_slug(slug).map(|h| {
            (
                h.def().display_name,
                Head::KIND,
                h.slug(),
                h.def().is_free(),
            )
        }),
        Tail::KIND => Tail::from_slug(slug).map(|t| {
            (
                t.def().display_name,
                Tail::KIND,
                t.slug(),
                t.def().is_free(),
            )
        }),
        _ => None,
    }
}

fn hash(code: &str) -> String {
    hex::encode(Sha256::digest(code.as_bytes()))
}

fn generate_code() -> String {
    (0..12)
        .map(|_| ALPHABET[OsRng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

pub async fn create(
    pool: &PgPool,
    created_by_user_id: Uuid,
    input: &CreateCode,
) -> cja::Result<String> {
    let Some((_, _, _, free)) = catalog_item(&input.customization_type, &input.slug) else {
        return Err(ValidationError("Unknown catalog item").into());
    };
    if free {
        return Err(ValidationError("Codes require an unlockable item").into());
    }
    if input.max_redemptions < 1 {
        return Err(ValidationError("Maximum redemptions must be at least 1").into());
    }
    if input.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
        return Err(ValidationError("Expiry must be in the future").into());
    }
    let note = input
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    if note.is_some_and(|n| n.chars().count() > 500) {
        return Err(ValidationError("Note must be 500 characters or fewer").into());
    }
    for _ in 0..8 {
        let code = generate_code();
        let code_hash = hash(&code);
        let inserted = sqlx::query!(
            "INSERT INTO customization_unlock_codes (code_hash, customization_type, slug, max_redemptions, expires_at, note, created_by_user_id) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (code_hash) DO NOTHING RETURNING code_id",
            code_hash, input.customization_type, input.slug, input.max_redemptions, input.expires_at, note, created_by_user_id,
        ).fetch_optional(pool).await.wrap_err("Failed to create unlock code")?;
        if inserted.is_some() {
            return Ok(code);
        }
    }
    Err(color_eyre::eyre::eyre!(
        "Failed to generate a unique unlock code"
    ))
}

pub async fn list(pool: &PgPool) -> cja::Result<Vec<CodeListing>> {
    let rows = sqlx::query_as!(CodeListing,
        "SELECT code_id, customization_type, slug, max_redemptions, redemptions_used, expires_at, note, created_at, disabled_at FROM customization_unlock_codes ORDER BY created_at DESC, code_id DESC"
    ).fetch_all(pool).await.wrap_err("Failed to list unlock codes")?;
    Ok(rows)
}

pub async fn disable(pool: &PgPool, code_id: Uuid) -> cja::Result<bool> {
    let result = sqlx::query!("UPDATE customization_unlock_codes SET disabled_at = NOW() WHERE code_id = $1 AND disabled_at IS NULL", code_id)
        .execute(pool).await.wrap_err("Failed to disable unlock code")?;
    Ok(result.rows_affected() == 1)
}

async fn record_failure(tx: &mut Transaction<'_, Postgres>, user_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        "INSERT INTO customization_code_failed_attempts (user_id) VALUES ($1)",
        user_id
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to record unlock code attempt")?;
    Ok(())
}

pub async fn redeem(pool: &PgPool, user_id: Uuid, submitted: &str) -> cja::Result<RedeemOutcome> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin unlock code redemption")?;
    let user = sqlx::query!(
        "SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to lock code redeemer")?;
    if user.is_none() {
        return Ok(RedeemOutcome::Unknown);
    }
    let attempts = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM customization_code_failed_attempts WHERE user_id = $1 AND attempted_at > NOW() - INTERVAL '1 hour'", user_id)
        .fetch_one(&mut *tx).await.wrap_err("Failed to count unlock code attempts")?;
    if attempts >= 10 {
        return Ok(RedeemOutcome::RateLimited);
    }

    let normalized = submitted.trim().to_ascii_uppercase();
    let valid = normalized.len() == 12 && normalized.bytes().all(|c| ALPHABET.contains(&c));
    let code = if valid {
        sqlx::query!("SELECT code_id, customization_type, slug, redemptions_used, max_redemptions, expires_at, disabled_at FROM customization_unlock_codes WHERE code_hash = $1 FOR UPDATE", hash(&normalized))
            .fetch_optional(&mut *tx).await.wrap_err("Failed to lock unlock code")?
    } else {
        None
    };
    let Some(code) = code else {
        record_failure(&mut tx, user_id).await?;
        tx.commit().await?;
        return Ok(RedeemOutcome::Unknown);
    };
    let redeemed = sqlx::query!("SELECT 1 AS marker FROM customization_code_redemptions WHERE code_id = $1 AND user_id = $2", code.code_id, user_id)
        .fetch_optional(&mut *tx).await?;
    let owned = sqlx::query!("SELECT 1 AS marker FROM customization_grants WHERE user_id = $1 AND customization_type = $2 AND slug = $3", user_id, code.customization_type, code.slug)
        .fetch_optional(&mut *tx).await?;
    let outcome = if redeemed.is_some() {
        Some(RedeemOutcome::AlreadyRedeemed)
    } else if code.disabled_at.is_some() {
        Some(RedeemOutcome::Disabled)
    } else if code.expires_at.is_some_and(|expiry| expiry <= Utc::now()) {
        Some(RedeemOutcome::Expired)
    } else if owned.is_some() {
        Some(RedeemOutcome::AlreadyOwned)
    } else if code.redemptions_used >= code.max_redemptions {
        Some(RedeemOutcome::FullyRedeemed)
    } else {
        None
    };
    if let Some(outcome) = outcome {
        record_failure(&mut tx, user_id).await?;
        tx.commit().await?;
        return Ok(outcome);
    }
    let inserted = sqlx::query!("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, $2, $3, 'code') ON CONFLICT (user_id, customization_type, slug) DO NOTHING", user_id, code.customization_type, code.slug)
        .execute(&mut *tx).await.wrap_err("Failed to grant code customization")?;
    if inserted.rows_affected() == 0 {
        record_failure(&mut tx, user_id).await?;
        tx.commit().await?;
        return Ok(RedeemOutcome::AlreadyOwned);
    }
    sqlx::query!(
        "INSERT INTO customization_code_redemptions (code_id, user_id) VALUES ($1, $2)",
        code.code_id,
        user_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to record code redemption")?;
    sqlx::query!("UPDATE customization_unlock_codes SET redemptions_used = redemptions_used + 1 WHERE code_id = $1", code.code_id)
        .execute(&mut *tx).await.wrap_err("Failed to count code redemption")?;
    tx.commit()
        .await
        .wrap_err("Failed to commit code redemption")?;
    let (name, kind, slug, _) = catalog_item(&code.customization_type, &code.slug)
        .ok_or_else(|| color_eyre::eyre::eyre!("Stored code references unknown catalog item"))?;
    Ok(RedeemOutcome::Granted { name, kind, slug })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::customizations::token_balance;
    use sqlx::Row as _;
    use std::time::Duration;

    async fn user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES ($1, $2, '') RETURNING user_id")
            .bind(github_id).bind(format!("code-{github_id}"))
            .fetch_one(pool).await?)
    }

    fn input(kind: &str, slug: &str, max: i32) -> CreateCode {
        CreateCode {
            customization_type: kind.into(),
            slug: slug.into(),
            max_redemptions: max,
            expires_at: None,
            note: None,
        }
    }

    async fn failed_count(pool: &PgPool, user_id: Uuid) -> cja::Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM customization_code_failed_attempts WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_one(pool)
        .await?)
    }

    async fn used(pool: &PgPool, code: &str) -> cja::Result<i32> {
        Ok(sqlx::query_scalar(
            "SELECT redemptions_used FROM customization_unlock_codes WHERE code_hash = $1",
        )
        .bind(hash(code))
        .fetch_one(pool)
        .await?)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn creation_validation_and_secret_storage(pool: PgPool) -> cja::Result<()> {
        let admin = user(&pool, 164501).await?;
        for bad in [
            input("wrong", "hydra", 1),
            input("head", "unknown", 1),
            input("head", "default", 1),
            input("head", "hydra", 0),
        ] {
            assert!(
                create(&pool, admin, &bad).await.is_err(),
                "invalid input should fail"
            );
        }
        let mut past = input("head", "hydra", 1);
        past.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(create(&pool, admin, &past).await.is_err());
        let mut long_note = input("head", "hydra", 1);
        long_note.note = Some("x".repeat(501));
        assert!(create(&pool, admin, &long_note).await.is_err());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn creates_hash_only_and_redeems_case_insensitively(pool: PgPool) -> cja::Result<()> {
        let admin = user(&pool, 164502).await?;
        let player = user(&pool, 164503).await?;
        let code = create(&pool, admin, &input("head", "hydra", 2)).await?;
        assert_eq!(code.len(), 12);
        assert!(code.bytes().all(|c| ALPHABET.contains(&c)));
        let row = sqlx::query(
            "SELECT code_hash, customization_type, slug FROM customization_unlock_codes",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.get::<String, _>("code_hash"), hash(&code));
        assert!(!row.get::<String, _>("code_hash").contains(&code));
        let columns: Vec<String> = sqlx::query_scalar("SELECT column_name FROM information_schema.columns WHERE table_name = 'customization_unlock_codes'").fetch_all(&pool).await?;
        assert!(
            !columns
                .iter()
                .any(|name| name == "code" || name == "plaintext")
        );
        let before = token_balance(&pool, player).await?;
        assert_eq!(
            redeem(&pool, player, &format!(" {} ", code.to_ascii_lowercase())).await?,
            RedeemOutcome::Granted {
                name: "Community Hydra",
                kind: "head",
                slug: "hydra"
            }
        );
        assert_eq!(token_balance(&pool, player).await?, before);
        let source: String = sqlx::query_scalar("SELECT source FROM customization_grants WHERE user_id = $1 AND customization_type = 'head' AND slug = 'hydra'").bind(player).fetch_one(&pool).await?;
        assert_eq!(source, "code");
        assert_eq!(used(&pool, &code).await?, 1);
        let preview = create(&pool, admin, &input("head", "turtle", 1)).await?;
        assert!(matches!(
            redeem(&pool, player, &preview).await?,
            RedeemOutcome::Granted {
                kind: "head",
                slug: "turtle",
                ..
            }
        ));
        assert_eq!(token_balance(&pool, player).await?, before);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failures_count_once_and_never_spend_slot(pool: PgPool) -> cja::Result<()> {
        let admin = user(&pool, 164504).await?;
        let a = user(&pool, 164505).await?;
        let b = user(&pool, 164506).await?;
        assert_eq!(redeem(&pool, a, "bad").await?, RedeemOutcome::Unknown);
        let expired = create(&pool, admin, &input("head", "hydra", 1)).await?;
        sqlx::query("UPDATE customization_unlock_codes SET expires_at = NOW() - INTERVAL '1 second' WHERE code_hash = $1").bind(hash(&expired)).execute(&pool).await?;
        assert_eq!(redeem(&pool, a, &expired).await?, RedeemOutcome::Expired);
        let disabled = create(&pool, admin, &input("head", "hydra", 1)).await?;
        let id: Uuid = sqlx::query_scalar(
            "SELECT code_id FROM customization_unlock_codes WHERE code_hash = $1",
        )
        .bind(hash(&disabled))
        .fetch_one(&pool)
        .await?;
        assert!(disable(&pool, id).await?);
        assert_eq!(redeem(&pool, a, &disabled).await?, RedeemOutcome::Disabled);
        let full = create(&pool, admin, &input("head", "hydra", 1)).await?;
        assert!(matches!(
            redeem(&pool, a, &full).await?,
            RedeemOutcome::Granted { .. }
        ));
        // AlreadyRedeemed deliberately counts as a failed submission.
        assert_eq!(
            redeem(&pool, a, &full).await?,
            RedeemOutcome::AlreadyRedeemed
        );
        assert_eq!(redeem(&pool, b, &full).await?, RedeemOutcome::FullyRedeemed);
        let same_item = create(&pool, admin, &input("head", "hydra", 1)).await?;
        // AlreadyOwned deliberately counts as a failed submission and leaves this code unused.
        assert_eq!(
            redeem(&pool, a, &same_item).await?,
            RedeemOutcome::AlreadyOwned
        );
        assert_eq!(used(&pool, &same_item).await?, 0);
        assert_eq!(failed_count(&pool, a).await?, 5);
        assert_eq!(failed_count(&pool, b).await?, 1);
        for (index, source) in ["admin", "token", "play_import", "achievement"]
            .into_iter()
            .enumerate()
        {
            let player = user(&pool, 164510 + i64::try_from(index)?).await?;
            sqlx::query("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'tail', 'hydra', $2)")
                .bind(player).bind(source).execute(&pool).await?;
            let code = create(&pool, admin, &input("tail", "hydra", 1)).await?;
            assert_eq!(
                redeem(&pool, player, &code).await?,
                RedeemOutcome::AlreadyOwned
            );
            assert_eq!(used(&pool, &code).await?, 0);
        }
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rate_limit_slides_and_success_does_not_count(pool: PgPool) -> cja::Result<()> {
        let admin = user(&pool, 164530).await?;
        let player = user(&pool, 164531).await?;
        for _ in 0..10 {
            assert_eq!(redeem(&pool, player, "bad").await?, RedeemOutcome::Unknown);
        }
        assert_eq!(
            redeem(&pool, player, "bad").await?,
            RedeemOutcome::RateLimited
        );
        assert_eq!(failed_count(&pool, player).await?, 10);
        sqlx::query("UPDATE customization_code_failed_attempts SET attempted_at = attempted_at - INTERVAL '61 minutes' WHERE user_id = $1").bind(player).execute(&pool).await?;
        let code = create(&pool, admin, &input("head", "hydra", 1)).await?;
        assert!(matches!(
            redeem(&pool, player, &code).await?,
            RedeemOutcome::Granted { .. }
        ));
        assert_eq!(failed_count(&pool, player).await?, 10);
        Ok(())
    }

    async fn wait_for_locks(pool: &PgPool, query: &str, count: i64) -> cja::Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE $1")
                    .bind(query).fetch_one(pool).await?;
                if waiting >= count { break Ok::<(), cja::color_eyre::Report>(()); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn last_slot_race_serializes_on_code_row(pool: PgPool) -> cja::Result<()> {
        let admin = user(&pool, 164540).await?;
        let a = user(&pool, 164541).await?;
        let b = user(&pool, 164542).await?;
        let code = create(&pool, admin, &input("head", "hydra", 1)).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query(
            "SELECT code_id FROM customization_unlock_codes WHERE code_hash = $1 FOR UPDATE",
        )
        .bind(hash(&code))
        .fetch_one(&mut *blocker)
        .await?;
        let tasks: Vec<_> = [a, b]
            .into_iter()
            .map(|player| {
                let pool = pool.clone();
                let code = code.clone();
                tokio::spawn(async move { redeem(&pool, player, &code).await })
            })
            .collect();
        wait_for_locks(
            &pool,
            "SELECT code_id, customization_type, slug, redemptions_used%",
            2,
        )
        .await?;
        blocker.commit().await?;
        let mut tasks = tasks.into_iter();
        let outcomes = (tasks.next().unwrap().await??, tasks.next().unwrap().await??);
        assert!(matches!(
            outcomes,
            (RedeemOutcome::Granted { .. }, RedeemOutcome::FullyRedeemed)
                | (RedeemOutcome::FullyRedeemed, RedeemOutcome::Granted { .. })
        ));
        assert_eq!(used(&pool, &code).await?, 1);
        let grants: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM customization_grants WHERE source = 'code'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(grants, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn double_submit_serializes_on_user_row(pool: PgPool) -> cja::Result<()> {
        let admin = user(&pool, 164550).await?;
        let player = user(&pool, 164551).await?;
        let code = create(&pool, admin, &input("head", "hydra", 2)).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE")
            .bind(player)
            .fetch_one(&mut *blocker)
            .await?;
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let pool = pool.clone();
                let code = code.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    redeem(&pool, player, &code).await
                })
            })
            .collect();
        barrier.wait().await;
        wait_for_locks(
            &pool,
            "SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE%",
            2,
        )
        .await?;
        blocker.commit().await?;
        let mut tasks = tasks.into_iter();
        let outcomes = (tasks.next().unwrap().await??, tasks.next().unwrap().await??);
        assert!(matches!(
            outcomes,
            (
                RedeemOutcome::Granted { .. },
                RedeemOutcome::AlreadyRedeemed
            ) | (
                RedeemOutcome::AlreadyRedeemed,
                RedeemOutcome::Granted { .. }
            )
        ));
        assert_eq!(used(&pool, &code).await?, 1);
        let grants: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM customization_grants WHERE user_id = $1 AND source = 'code'",
        )
        .bind(player)
        .fetch_one(&pool)
        .await?;
        assert_eq!(grants, 1);
        Ok(())
    }
}
