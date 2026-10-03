use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Context as _, eyre};
use sqlx::PgPool;
use uuid::Uuid;

use crate::customizations::normalize_color;
use crate::models::battlesnake::EngineRegion;

/// A play account staged for migration. Inert until claimed.
#[derive(Debug, sqlx::FromRow)]
pub struct ImportedAccount {
    pub imported_account_id: Uuid,
    pub play_user_id: String,
    pub email: String,
    pub password_hash: String,
    pub is_email_verified: bool,
    pub username: String,
    pub display_name: String,
    pub github_uid: Option<i64>,
    pub github_login: Option<String>,
    pub claimed_by_user_id: Option<Uuid>,
}

/// Importer input for one play account (user + account + optional GitHub
/// social link, one row per play user).
#[derive(Debug, Clone)]
pub struct StageAccount {
    pub play_user_id: String,
    pub play_account_id: String,
    pub email: String,
    pub password_hash: String,
    pub is_email_verified: bool,
    pub username: String,
    pub display_name: String,
    pub pronouns: String,
    pub country: String,
    pub backstory: String,
    pub github_uid: Option<i64>,
    pub github_login: Option<String>,
    pub points: i32,
    pub points_high_score: i32,
    pub is_staff: bool,
    pub play_created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone)]
pub struct StageSnake {
    pub play_snake_id: String,
    pub play_account_id: String,
    pub name: String,
    pub url: String,
    pub head: String,
    pub tail: String,
    pub color: String,
    pub is_public: bool,
    pub engine_region: EngineRegion,
}

#[derive(Debug, Clone)]
pub struct StageGrant {
    pub customization_type: String,
    pub slug: String,
}

#[derive(Debug, Clone)]
pub struct StagePlayAccount {
    pub account: StageAccount,
    pub snakes: Vec<StageSnake>,
    pub grants: Vec<StageGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageStatus {
    Created,
    Refreshed,
    Unchanged,
    SkippedClaimed,
    SkippedIdentityConflict(IdentityConflict),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityConflict {
    GithubUid(i64),
    SnakeId(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct StageResult {
    pub imported_account_id: Uuid,
    pub status: StageStatus,
    pub snakes_processed: u64,
    pub grants_processed: u64,
}

async fn skip_identity_conflict(
    tx: sqlx::Transaction<'_, sqlx::Postgres>,
    conflict: IdentityConflict,
) -> cja::Result<StageResult> {
    tx.rollback()
        .await
        .wrap_err("Failed to roll back identity conflict")?;
    Ok(StageResult {
        imported_account_id: Uuid::nil(),
        status: StageStatus::SkippedIdentityConflict(conflict),
        snakes_processed: 0,
        grants_processed: 0,
    })
}

/// Publish one Play account and its complete active child sets in one commit.
pub async fn stage_play_account(
    pool: &PgPool,
    payload: &StagePlayAccount,
) -> cja::Result<StageResult> {
    let account = &payload.account;
    let mut snake_ids = HashSet::new();
    for snake in &payload.snakes {
        if snake.play_account_id != account.play_account_id {
            return Err(eyre!(
                "Snake {} belongs to {}, expected {}",
                snake.play_snake_id,
                snake.play_account_id,
                account.play_account_id
            ));
        }
        if !snake_ids.insert(snake.play_snake_id.as_str()) {
            return Err(eyre!("Duplicate Play snake ID {}", snake.play_snake_id));
        }
    }
    let mut grant_keys = HashSet::new();
    for grant in &payload.grants {
        if grant.customization_type != "head" && grant.customization_type != "tail" {
            return Err(eyre!(
                "Invalid Play grant type {}",
                grant.customization_type
            ));
        }
        grant_keys.insert((grant.customization_type.as_str(), grant.slug.as_str()));
    }

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin Play account staging tx")?;
    let existing = sqlx::query!(
        "SELECT imported_account_id, play_account_id, claimed_by_user_id FROM imported_accounts WHERE play_user_id = $1 FOR UPDATE",
        account.play_user_id
    ).fetch_optional(&mut *tx).await.wrap_err("Failed to lock staged account")?;
    let (id, stored_play_account_id, created, claimed) = if let Some(row) = existing {
        (
            row.imported_account_id,
            row.play_account_id,
            false,
            row.claimed_by_user_id.is_some(),
        )
    } else {
        let inserted = sqlx::query!(
            "INSERT INTO imported_accounts (play_user_id, play_account_id, email, password_hash, is_email_verified, username, display_name, pronouns, country, backstory, github_uid, github_login, points, points_high_score, is_staff, play_created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL, $11, $12, $13, $14, $15) ON CONFLICT (play_user_id) DO NOTHING RETURNING imported_account_id",
            account.play_user_id, account.play_account_id, account.email, account.password_hash,
            account.is_email_verified, account.username, account.display_name, account.pronouns,
            account.country, account.backstory, account.github_login, account.points,
            account.points_high_score, account.is_staff, account.play_created_at
        ).fetch_optional(&mut *tx).await.wrap_err("Failed to insert staged account")?;
        if let Some(row) = inserted {
            (
                row.imported_account_id,
                account.play_account_id.clone(),
                true,
                false,
            )
        } else {
            let row = sqlx::query!(
                "SELECT imported_account_id, play_account_id, claimed_by_user_id FROM imported_accounts WHERE play_user_id = $1 FOR UPDATE",
                account.play_user_id
            ).fetch_one(&mut *tx).await.wrap_err("Failed to lock concurrent staged account")?;
            (
                row.imported_account_id,
                row.play_account_id,
                false,
                row.claimed_by_user_id.is_some(),
            )
        }
    };
    if stored_play_account_id != account.play_account_id {
        return Err(eyre!(
            "Play user {} has staged account {}, incoming {}",
            account.play_user_id,
            stored_play_account_id,
            account.play_account_id
        ));
    }
    if claimed {
        for (kind, slug) in grant_keys {
            sqlx::query!("INSERT INTO imported_grants (imported_account_id, customization_type, slug) VALUES ($1, $2, $3) ON CONFLICT (imported_account_id, customization_type, slug) DO NOTHING", id, kind, slug)
                .execute(&mut *tx).await.wrap_err("Failed to stage claimed account grant")?;
        }
        tx.commit()
            .await
            .wrap_err("Failed to commit claimed account grants")?;
        return Ok(StageResult {
            imported_account_id: id,
            status: StageStatus::SkippedClaimed,
            snakes_processed: 0,
            grants_processed: payload.grants.len() as u64,
        });
    }

    if let Some(uid) = account.github_uid {
        let owner = sqlx::query!("SELECT imported_account_id, claimed_by_user_id FROM imported_accounts WHERE github_uid = $1 AND imported_account_id <> $2 FOR UPDATE", uid, id)
            .fetch_optional(&mut *tx).await.wrap_err("Failed to lock former GitHub UID owner")?;
        if let Some(owner) = owner {
            if owner.claimed_by_user_id.is_some() {
                return skip_identity_conflict(tx, IdentityConflict::GithubUid(uid)).await;
            }
            sqlx::query!("UPDATE imported_accounts SET github_uid = NULL WHERE imported_account_id = $1 AND github_uid = $2 AND claimed_by_user_id IS NULL", owner.imported_account_id, uid)
                .execute(&mut *tx).await.wrap_err("Failed to release stale GitHub UID")?;
        }
        let claimed_owner = sqlx::query!("SELECT imported_account_id FROM imported_accounts WHERE github_uid = $1 AND imported_account_id <> $2 AND claimed_by_user_id IS NOT NULL", uid, id)
            .fetch_optional(&mut *tx).await.wrap_err("Failed to recheck claimed GitHub UID owner")?;
        if claimed_owner.is_some() {
            return skip_identity_conflict(tx, IdentityConflict::GithubUid(uid)).await;
        }
    }

    let updated = sqlx::query!(
        r#"UPDATE imported_accounts SET email=$2, password_hash=$3, is_email_verified=$4,
            username=$5, display_name=$6, pronouns=$7, country=$8, backstory=$9,
            github_uid=$10, github_login=$11, points=$12, points_high_score=$13,
            is_staff=$14, play_created_at=$15
            WHERE imported_account_id=$1 AND
              (email, password_hash, is_email_verified, username, display_name, pronouns,
               country, backstory, github_uid, github_login, points, points_high_score,
               is_staff, play_created_at) IS DISTINCT FROM
              ($2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"#,
        id,
        account.email,
        account.password_hash,
        account.is_email_verified,
        account.username,
        account.display_name,
        account.pronouns,
        account.country,
        account.backstory,
        account.github_uid,
        account.github_login,
        account.points,
        account.points_high_score,
        account.is_staff,
        account.play_created_at
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to refresh staged account")?
    .rows_affected()
        > 0;
    let mut changed = updated;

    let incoming_ids: Vec<String> = payload
        .snakes
        .iter()
        .map(|s| s.play_snake_id.clone())
        .collect();
    let existing_snakes = {
        sqlx::query!(
            "SELECT imported_account_id, play_snake_id, name, url, head, tail, color, is_public, engine_region FROM imported_snakes WHERE imported_account_id = $1 OR play_snake_id = ANY($2) FOR UPDATE",
            id, &incoming_ids
        ).fetch_all(&mut *tx).await.wrap_err("Failed to lock staged snakes")?
    };
    let mut snakes_by_id: HashMap<String, _> = existing_snakes
        .into_iter()
        .map(|s| (s.play_snake_id.clone(), s))
        .collect();
    for snake in &payload.snakes {
        let color = normalize_color(&snake.color);
        if let Some(old) = snakes_by_id.remove(&snake.play_snake_id) {
            if old.imported_account_id != id {
                let claimed_owner: Option<Uuid> = sqlx::query_scalar!(
                    "SELECT claimed_by_user_id FROM imported_accounts WHERE imported_account_id = $1",
                    old.imported_account_id
                ).fetch_one(&mut *tx).await.wrap_err("Failed to check snake owner claim")?;
                if claimed_owner.is_some() {
                    return skip_identity_conflict(
                        tx,
                        IdentityConflict::SnakeId(snake.play_snake_id.clone()),
                    )
                    .await;
                }
                return Err(eyre!(
                    "Play snake {} belongs to imported account {}, not {}",
                    snake.play_snake_id,
                    old.imported_account_id,
                    id
                ));
            }
            if old.name != snake.name
                || old.url != snake.url
                || old.head != snake.head
                || old.tail != snake.tail
                || old.color != color
                || old.is_public != snake.is_public
                || old.engine_region != snake.engine_region.as_str()
            {
                sqlx::query!("UPDATE imported_snakes SET name=$2, url=$3, head=$4, tail=$5, color=$6, is_public=$7, engine_region=$8 WHERE play_snake_id=$1 AND imported_account_id=$9",
                    snake.play_snake_id, snake.name, snake.url, snake.head, snake.tail, color, snake.is_public, snake.engine_region.as_str(), id
                ).execute(&mut *tx).await.wrap_err("Failed to refresh staged snake")?;
                changed = true;
            }
        } else {
            sqlx::query!("INSERT INTO imported_snakes (imported_account_id, play_snake_id, name, url, head, tail, color, is_public, engine_region) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                id, snake.play_snake_id, snake.name, snake.url, snake.head, snake.tail, color, snake.is_public, snake.engine_region.as_str()
            ).execute(&mut *tx).await.wrap_err("Failed to insert staged snake")?;
            changed = true;
        }
    }
    for old in snakes_by_id.into_values() {
        if old.imported_account_id == id {
            sqlx::query!(
                "DELETE FROM imported_snakes WHERE play_snake_id=$1 AND imported_account_id=$2",
                old.play_snake_id,
                id
            )
            .execute(&mut *tx)
            .await
            .wrap_err("Failed to remove archived staged snake")?;
            changed = true;
        }
    }

    let existing_grants = sqlx::query!(
        "SELECT customization_type, slug FROM imported_grants WHERE imported_account_id=$1",
        id
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to read staged grants")?;
    let old_keys: HashSet<(String, String)> = existing_grants
        .into_iter()
        .map(|g| (g.customization_type, g.slug))
        .collect();
    for &(kind, slug) in &grant_keys {
        if !old_keys.contains(&(kind.to_string(), slug.to_string())) {
            sqlx::query!("INSERT INTO imported_grants (imported_account_id, customization_type, slug) VALUES ($1,$2,$3) ON CONFLICT (imported_account_id, customization_type, slug) DO NOTHING", id, kind, slug)
                .execute(&mut *tx).await.wrap_err("Failed to insert staged grant")?;
            changed = true;
        }
    }
    for (kind, slug) in old_keys {
        if !grant_keys.contains(&(kind.as_str(), slug.as_str())) {
            sqlx::query!("DELETE FROM imported_grants WHERE imported_account_id=$1 AND customization_type=$2 AND slug=$3", id, kind, slug)
                .execute(&mut *tx).await.wrap_err("Failed to remove obsolete staged grant")?;
            changed = true;
        }
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit Play account staging tx")?;
    Ok(StageResult {
        imported_account_id: id,
        status: if created {
            StageStatus::Created
        } else if changed {
            StageStatus::Refreshed
        } else {
            StageStatus::Unchanged
        },
        snakes_processed: payload.snakes.len() as u64,
        grants_processed: payload.grants.len() as u64,
    })
}

const IMPORTED_ACCOUNT_COLUMNS: &str = r#"
    imported_account_id, play_user_id, email, password_hash,
    is_email_verified, username, display_name, github_uid, github_login,
    claimed_by_user_id
"#;

pub async fn find_unclaimed_by_github_uid(
    pool: &PgPool,
    github_uid: i64,
) -> cja::Result<Option<ImportedAccount>> {
    let account = sqlx::query_as::<_, ImportedAccount>(&format!(
        "SELECT {IMPORTED_ACCOUNT_COLUMNS} FROM imported_accounts
         WHERE github_uid = $1 AND claimed_by_user_id IS NULL"
    ))
    .bind(github_uid)
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to look up imported account by GitHub ID")?;

    Ok(account)
}

/// Any one real imported password hash, for the claim endpoint's timing
/// decoy — verifying against a genuine play hash makes the no-candidate
/// path cost the same as an existing account (matching play's real
/// iteration count) instead of a hardcoded guess. Returns None before the
/// first import or when every account is OAuth-only (empty hash), in which
/// case no password-bearing email exists to enumerate anyway.
pub async fn representative_password_hash(pool: &PgPool) -> cja::Result<Option<String>> {
    let row = sqlx::query!(
        "SELECT password_hash FROM imported_accounts WHERE password_hash <> '' LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch a representative password hash")?;

    Ok(row.map(|r| r.password_hash))
}

/// All unclaimed accounts matching an email case-insensitively. Play
/// enforced case-SENSITIVE uniqueness, so rare case-variant duplicates are
/// possible; the caller disambiguates by password verification.
pub async fn find_unclaimed_by_email(
    pool: &PgPool,
    email: &str,
) -> cja::Result<Vec<ImportedAccount>> {
    let accounts = sqlx::query_as::<_, ImportedAccount>(&format!(
        "SELECT {IMPORTED_ACCOUNT_COLUMNS} FROM imported_accounts
         WHERE lower(email) = lower($1) AND claimed_by_user_id IS NULL
         ORDER BY imported_at"
    ))
    .bind(email)
    .fetch_all(pool)
    .await
    .wrap_err("Failed to look up imported account by email")?;

    Ok(accounts)
}

/// What a successful claim materialized.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ClaimSummary {
    pub snakes_created: usize,
    pub grants_created: u64,
    pub username: String,
    /// The play account's email, so callers can send a claim notification
    /// to the original owner.
    pub email: String,
}

/// Claim an imported account for an arena user: copy the display identity,
/// materialize staged snakes and grants, and mark the account claimed.
///
/// Runs in one transaction guarded by a compare-and-set on
/// `claimed_by_user_id IS NULL`, so concurrent claims of the same account
/// cannot double-materialize; the loser gets Ok(None).
pub async fn claim_account(
    pool: &PgPool,
    imported_account_id: Uuid,
    user_id: Uuid,
) -> cja::Result<Option<ClaimSummary>> {
    claim_account_inner(pool, imported_account_id, user_id, None).await
}

async fn claim_account_inner(
    pool: &PgPool,
    imported_account_id: Uuid,
    user_id: Uuid,
    expected_github_uid: Option<i64>,
) -> cja::Result<Option<ClaimSummary>> {
    let mut tx = pool.begin().await.wrap_err("Failed to begin claim tx")?;

    let claimed = sqlx::query!(
        r#"
        UPDATE imported_accounts
        SET claimed_by_user_id = $2, claimed_at = NOW()
        WHERE imported_account_id = $1 AND claimed_by_user_id IS NULL
          AND ($3::bigint IS NULL OR github_uid = $3)
        RETURNING username, display_name, email, pronouns, country, backstory
        "#,
        imported_account_id,
        user_id,
        expected_github_uid,
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to mark imported account claimed")?;

    let Some(claimed) = claimed else {
        return Ok(None);
    };

    // Display identity: play username becomes the arena display name, but
    // never clobber one the user already set.
    sqlx::query!(
        r#"
        UPDATE users
        SET display_name = COALESCE(display_name, NULLIF(left($2, 100), ''), left($3, 100)),
            pronouns = COALESCE(NULLIF(pronouns, ''), left($4, 50), pronouns),
            country = COALESCE(NULLIF(country, ''), left($5, 100), country),
            backstory = COALESCE(NULLIF(backstory, ''), left($6, 2000), backstory)
        WHERE user_id = $1
        "#,
        user_id,
        claimed.display_name,
        claimed.username,
        claimed.pronouns,
        claimed.country,
        claimed.backstory,
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to set display name and profile fields")?;

    // Materialize snakes. Names must be unique per user in arena; play had
    // no such constraint, so collisions get a numeric suffix. Existing
    // names are fetched up front to avoid unique-violation aborts mid-tx.
    let mut existing_names: Vec<String> = sqlx::query!(
        "SELECT name FROM battlesnakes WHERE user_id = $1 AND deleted_at IS NULL",
        user_id
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to fetch existing snake names")?
    .into_iter()
    .map(|r| r.name)
    .collect();

    let staged_snakes = sqlx::query!(
        r#"
        SELECT imported_snake_id, name, url, head, tail, color, is_public, engine_region
        FROM imported_snakes
        WHERE imported_account_id = $1 AND materialized_battlesnake_id IS NULL
        ORDER BY imported_at
        "#,
        imported_account_id,
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to fetch staged snakes")?;

    let mut snakes_created = 0;
    for snake in staged_snakes {
        let mut name = snake.name.clone();
        let mut suffix = 2;
        while existing_names.iter().any(|n| n == &name) {
            name = format!("{}-{}", snake.name, suffix);
            suffix += 1;
        }

        let visibility = if snake.is_public { "public" } else { "private" };
        let battlesnake_id = sqlx::query!(
            r#"
            INSERT INTO battlesnakes (user_id, name, url, visibility, color, head, tail, engine_region)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING battlesnake_id
            "#,
            user_id,
            name,
            snake.url,
            visibility,
            normalize_color(&snake.color),
            snake.head,
            snake.tail,
            snake.engine_region,
        )
        .fetch_one(&mut *tx)
        .await
        .wrap_err("Failed to materialize imported snake")?
        .battlesnake_id;

        sqlx::query!(
            "UPDATE imported_snakes SET materialized_battlesnake_id = $2
             WHERE imported_snake_id = $1",
            snake.imported_snake_id,
            battlesnake_id,
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to link materialized snake")?;

        existing_names.push(name);
        snakes_created += 1;
    }

    // Materialize grants. The catalog is code-defined, so staged (type,
    // slug) pairs are filtered through it here; slugs no longer in the
    // catalog are dropped.
    let staged_grants = sqlx::query!(
        r#"
        SELECT customization_type, slug
        FROM imported_grants
        WHERE imported_account_id = $1
        "#,
        imported_account_id,
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to fetch staged grants")?;

    let mut grants_created = 0u64;
    for grant in staged_grants {
        let in_catalog = match grant.customization_type.as_str() {
            "head" => crate::customizations::Head::from_slug(&grant.slug).is_some(),
            "tail" => crate::customizations::Tail::from_slug(&grant.slug).is_some(),
            _ => false,
        };
        if !in_catalog {
            tracing::warn!(
                customization_type = %grant.customization_type,
                slug = %grant.slug,
                "Imported grant references a slug not in the catalog; dropping"
            );
            continue;
        }

        let inserted = sqlx::query!(
            r#"
            INSERT INTO customization_grants (user_id, customization_type, slug)
            VALUES ($1, $2, $3)
            ON CONFLICT (user_id, customization_type, slug) DO NOTHING
            "#,
            user_id,
            grant.customization_type,
            grant.slug,
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to materialize grant")?
        .rows_affected();
        grants_created += inserted;
    }

    tx.commit().await.wrap_err("Failed to commit claim tx")?;

    Ok(Some(ClaimSummary {
        snakes_created,
        grants_created,
        username: claimed.username,
        email: claimed.email,
    }))
}

/// Auto-claim on GitHub login: if an unclaimed imported account carries
/// this user's GitHub ID, claim it silently. Returns the claim summary if
/// one happened.
pub async fn try_auto_claim(
    pool: &PgPool,
    user_id: Uuid,
    external_github_id: i64,
) -> cja::Result<Option<ClaimSummary>> {
    let Some(account) = find_unclaimed_by_github_uid(pool, external_github_id).await? else {
        return Ok(None);
    };

    claim_account_inner(
        pool,
        account.imported_account_id,
        user_id,
        Some(external_github_id),
    )
    .await
}

/// Whether /me should prompt this user to claim a play account: they
/// haven't claimed one and haven't dismissed the prompt. We can't tell a
/// returning play player from a brand-new one, so everyone unclaimed sees
/// it until they hide it.
pub async fn should_show_claim_prompt(pool: &PgPool, user_id: Uuid) -> cja::Result<bool> {
    let show = sqlx::query_scalar!(
        r#"
        SELECT (
            u.claim_prompt_dismissed_at IS NULL
            AND NOT EXISTS (
                SELECT 1 FROM imported_accounts ia
                WHERE ia.claimed_by_user_id = u.user_id
            )
        ) AS "show!"
        FROM users u
        WHERE u.user_id = $1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to query claim prompt state")?;

    Ok(show.unwrap_or(false))
}

/// Hide the claim prompt for good. Idempotent: re-dismissing keeps the
/// original timestamp.
pub async fn dismiss_claim_prompt(pool: &PgPool, user_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        "UPDATE users SET claim_prompt_dismissed_at = NOW()
         WHERE user_id = $1 AND claim_prompt_dismissed_at IS NULL",
        user_id,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record claim prompt dismissal")?;

    Ok(())
}

/// Counts of claim attempts in the last hour, by arena user and by target
/// email, for two-dimension rate limiting.
#[derive(Debug)]
pub struct ClaimAttemptCounts {
    pub by_user: i64,
    pub by_email: i64,
}

/// Record a claim attempt, then return the last-hour counts for this user
/// and this (case-insensitive) email — including the just-recorded row.
///
/// Recording before the count (rather than after a separate check) makes
/// the rate limit race-safe: concurrent attempts each insert first, so the
/// count every request sees reflects the others instead of all reading
/// zero and sailing past the gate.
pub async fn record_and_count_claim_attempts(
    pool: &PgPool,
    user_id: Uuid,
    email: &str,
) -> cja::Result<ClaimAttemptCounts> {
    sqlx::query!(
        "INSERT INTO claim_attempts (user_id, email) VALUES ($1, $2)",
        user_id,
        email,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record claim attempt")?;

    let row = sqlx::query!(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE user_id = $1) as "by_user!",
            COUNT(*) FILTER (WHERE lower(email) = lower($2)) as "by_email!"
        FROM claim_attempts
        WHERE attempted_at > NOW() - INTERVAL '1 hour'
          AND (user_id = $1 OR lower(email) = lower($2))
        "#,
        user_id,
        email,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count claim attempts")?;

    Ok(ClaimAttemptCounts {
        by_user: row.by_user,
        by_email: row.by_email,
    })
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

    fn play_account(n: u32) -> StageAccount {
        StageAccount {
            play_user_id: format!("usr_{n}"),
            play_account_id: format!("act_{n}"),
            email: format!("player{n}@example.com"),
            password_hash:
                "pbkdf2_sha256$260000$saltysalt$fdr4GEVFxx0kLHYGvrnFQUyTekgaAA8DbWRR6Z+A7/A="
                    .to_string(),
            is_email_verified: true,
            username: format!("player{n}"),
            display_name: format!("Player {n}"),
            pronouns: String::new(),
            country: "CA".to_string(),
            backstory: "hiss".to_string(),
            github_uid: None,
            github_login: None,
            points: 150,
            points_high_score: 400,
            is_staff: false,
            play_created_at: None,
        }
    }

    async fn stage_full_account(pool: &PgPool, n: u32) -> cja::Result<Uuid> {
        let result = stage_play_account(
            pool,
            &StagePlayAccount {
                account: play_account(n),
                snakes: vec![StageSnake {
                    play_snake_id: format!("snk_{n}"),
                    play_account_id: format!("act_{n}"),
                    name: "Hissy".to_string(),
                    url: "https://example.com/snake".to_string(),
                    head: "alligator".to_string(),
                    tail: "default".to_string(),
                    color: "#ff0000".to_string(),
                    is_public: true,
                    engine_region: EngineRegion::UsWest1,
                }],
                grants: vec![
                    StageGrant {
                        customization_type: "head".to_string(),
                        slug: "alligator".to_string(),
                    },
                    StageGrant {
                        customization_type: "tail".to_string(),
                        slug: "no-longer-exists".to_string(),
                    },
                ],
            },
        )
        .await?;
        Ok(result.imported_account_id)
    }

    async fn stage_empty_account(pool: &PgPool, account: &StageAccount) -> cja::Result<Uuid> {
        Ok(stage_play_account(
            pool,
            &StagePlayAccount {
                account: account.clone(),
                snakes: vec![],
                grants: vec![],
            },
        )
        .await?
        .imported_account_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_materializes_snakes_grants_and_display_name(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 1001).await?;
        let account_id = stage_full_account(&pool, 1).await?;
        sqlx::query!(
            "UPDATE imported_snakes SET engine_region = 'us-east4' WHERE play_snake_id = 'snk_1'"
        )
        .execute(&pool)
        .await?;

        let summary = claim_account(&pool, account_id, user_id)
            .await?
            .expect("claim should succeed");
        assert_eq!(summary.snakes_created, 1);
        // The retired slug is dropped; only the alligator head lands.
        assert_eq!(summary.grants_created, 1);
        assert_eq!(summary.username, "player1");

        let snake = sqlx::query!(
            "SELECT name, url, head, tail, color, visibility, engine_region FROM battlesnakes WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(snake.name, "Hissy");
        assert_eq!(snake.head, "alligator");
        assert_eq!(snake.visibility, "public");
        assert_eq!(snake.color, "#ff0000");
        assert_eq!(snake.engine_region, "us-east4");

        // A later Play refresh changes staging, never the owner's live choice.
        sqlx::query!(
            "UPDATE battlesnakes SET engine_region = 'europe-west4' WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;
        stage_play_account(
            &pool,
            &StagePlayAccount {
                account: play_account(1),
                snakes: vec![StageSnake {
                    play_snake_id: "snk_1".to_string(),
                    play_account_id: "act_1".to_string(),
                    name: "Hissy".to_string(),
                    url: "https://example.com/snake".to_string(),
                    head: "alligator".to_string(),
                    tail: "default".to_string(),
                    color: "#ff0000".to_string(),
                    is_public: true,
                    engine_region: EngineRegion::UsWest1,
                }],
                grants: vec![StageGrant {
                    customization_type: "head".to_string(),
                    slug: "alligator".to_string(),
                }],
            },
        )
        .await?;
        let saved = sqlx::query_scalar!(
            "SELECT engine_region FROM battlesnakes WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(saved, "europe-west4");

        let display_name =
            sqlx::query!("SELECT display_name FROM users WHERE user_id = $1", user_id)
                .fetch_one(&pool)
                .await?
                .display_name;
        assert_eq!(display_name.as_deref(), Some("Player 1"));

        // The granted head is now usable in games.
        let resolved = crate::customizations::resolve_head(&pool, user_id, "alligator").await?;
        assert_eq!(resolved, "alligator");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_engine_region_is_idempotent(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 1480).await?;
        let account_id = stage_play_account(
            &pool,
            &StagePlayAccount {
                account: play_account(1480),
                snakes: vec![StageSnake {
                    play_snake_id: "snk_33GPkJxhMmQttTGqcrXFJjSC".to_string(),
                    play_account_id: "act_1480".to_string(),
                    name: "Imported EU".to_string(),
                    url: "https://example.com/eu".to_string(),
                    head: "default".to_string(),
                    tail: "default".to_string(),
                    color: "#ff0000".to_string(),
                    is_public: true,
                    engine_region: EngineRegion::UsWest1,
                }],
                grants: vec![],
            },
        )
        .await?
        .imported_account_id;
        claim_account(&pool, account_id, user_id)
            .await?
            .expect("claim succeeds");
        let native_id = Uuid::parse_str("0c041986-aecb-4dc2-9e1e-3e03fd629e04")?;
        sqlx::query(
            "INSERT INTO battlesnakes (battlesnake_id, user_id, name, url, visibility)
                     VALUES ($1, $2, 'Native EU', 'https://example.com/native', 'public')",
        )
        .bind(native_id)
        .bind(user_id)
        .execute(&pool)
        .await?;
        let migration =
            include_str!("../../../migrations/20260930143001_backfill_engine_region.up.sql");
        sqlx::raw_sql(migration).execute(&pool).await?;
        let first: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT name, url, engine_region FROM battlesnakes WHERE user_id = $1 ORDER BY name",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|(_, _, region)| region == "europe-west4"));
        sqlx::raw_sql(migration).execute(&pool).await?;
        let second: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT name, url, engine_region FROM battlesnakes WHERE user_id = $1 ORDER BY name",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(first, second);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn hostile_import_color_is_neutralized(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 3001).await?;
        let account_id = stage_play_account(
            &pool,
            &StagePlayAccount {
                account: play_account(3),
                snakes: vec![StageSnake {
                    play_snake_id: "snk_hostile".to_string(),
                    play_account_id: "act_3".to_string(),
                    name: "Sneaky".to_string(),
                    url: "https://example.com/snake".to_string(),
                    head: "default".to_string(),
                    tail: "default".to_string(),
                    color: "red;position:fixed;inset:0".to_string(),
                    is_public: true,
                    engine_region: EngineRegion::UsWest1,
                }],
                grants: vec![],
            },
        )
        .await?
        .imported_account_id;

        claim_account(&pool, account_id, user_id)
            .await?
            .expect("claim should succeed");

        // Invalid play colors become '' (the board picks a stable fallback
        // for empty; the UI chip renderer greys it out) — never raw CSS.
        let color = sqlx::query!("SELECT color FROM battlesnakes WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await?
            .color;
        assert_eq!(color, "");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_is_single_shot(pool: PgPool) -> cja::Result<()> {
        let user_a = create_user(&pool, 2001).await?;
        let user_b = create_user(&pool, 2002).await?;
        let account_id = stage_full_account(&pool, 2).await?;

        assert!(claim_account(&pool, account_id, user_a).await?.is_some());
        // Second claim (any user, including the same one) is a no-op.
        assert!(claim_account(&pool, account_id, user_b).await?.is_none());
        assert!(claim_account(&pool, account_id, user_a).await?.is_none());

        let snake_count = sqlx::query!("SELECT COUNT(*) as \"count!\" FROM battlesnakes")
            .fetch_one(&pool)
            .await?
            .count;
        assert_eq!(snake_count, 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_prompt_shows_until_claimed(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 2101).await?;
        let other_user = create_user(&pool, 2102).await?;
        let account_id = stage_full_account(&pool, 21).await?;

        assert!(should_show_claim_prompt(&pool, user_id).await?);

        claim_account(&pool, account_id, user_id)
            .await?
            .expect("claim should succeed");

        assert!(!should_show_claim_prompt(&pool, user_id).await?);
        // Someone else's claim doesn't hide your prompt.
        assert!(should_show_claim_prompt(&pool, other_user).await?);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_prompt_dismissal_sticks_and_is_per_user(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 2201).await?;
        let other_user = create_user(&pool, 2202).await?;

        dismiss_claim_prompt(&pool, user_id).await?;
        assert!(!should_show_claim_prompt(&pool, user_id).await?);
        assert!(should_show_claim_prompt(&pool, other_user).await?);

        let first = sqlx::query_scalar!(
            "SELECT claim_prompt_dismissed_at FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        dismiss_claim_prompt(&pool, user_id).await?;
        let second = sqlx::query_scalar!(
            "SELECT claim_prompt_dismissed_at FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert!(first.is_some());
        assert_eq!(first, second, "re-dismissing keeps the original timestamp");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_prompt_hidden_for_unknown_user(pool: PgPool) -> cja::Result<()> {
        assert!(!should_show_claim_prompt(&pool, Uuid::new_v4()).await?);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_suffixes_colliding_snake_names(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 3001).await?;
        sqlx::query!(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, 'Hissy', 'https://example.com/existing', 'private')",
            user_id
        )
        .execute(&pool)
        .await?;

        let account_id = stage_full_account(&pool, 3).await?;
        let summary = claim_account(&pool, account_id, user_id)
            .await?
            .expect("claim should succeed");
        assert_eq!(summary.snakes_created, 1);

        let names: Vec<String> = sqlx::query!(
            "SELECT name FROM battlesnakes WHERE user_id = $1 ORDER BY name",
            user_id
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|r| r.name)
        .collect();
        assert_eq!(names, vec!["Hissy".to_string(), "Hissy-2".to_string()]);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_does_not_clobber_existing_display_name(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 4001).await?;
        sqlx::query!(
            "UPDATE users SET display_name = 'Chosen Name' WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;

        let account_id = stage_full_account(&pool, 4).await?;
        claim_account(&pool, account_id, user_id).await?;

        let display_name =
            sqlx::query!("SELECT display_name FROM users WHERE user_id = $1", user_id)
                .fetch_one(&pool)
                .await?
                .display_name;
        assert_eq!(display_name.as_deref(), Some("Chosen Name"));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_copies_pronouns_country_backstory(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 4101).await?;
        let account_id = stage_full_account(&pool, 10).await?;

        claim_account(&pool, account_id, user_id).await?;

        let user = sqlx::query!(
            "SELECT pronouns, country, backstory FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        // play_account(10) sets country="CA", backstory="hiss", pronouns=""
        assert_eq!(user.pronouns, "");
        assert_eq!(user.country, "CA");
        assert_eq!(user.backstory, "hiss");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_does_not_clobber_existing_profile_fields(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 4201).await?;
        // User already set their profile fields.
        sqlx::query!(
            "UPDATE users SET pronouns = 'she/her', country = 'US', backstory = 'Existing story' WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;

        let account_id = stage_full_account(&pool, 11).await?;
        claim_account(&pool, account_id, user_id).await?;

        let user = sqlx::query!(
            "SELECT pronouns, country, backstory FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(user.pronouns, "she/her");
        assert_eq!(user.country, "US");
        assert_eq!(user.backstory, "Existing story");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_clamps_over_limit_imported_fields(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 4401).await?;

        // Play never enforced these limits; an oversized import must not
        // land values the profile edit form would then refuse to save.
        let mut account = play_account(13);
        account.backstory = "b".repeat(5000);
        account.pronouns = "p".repeat(200);
        let account_id = stage_empty_account(&pool, &account).await?;

        claim_account(&pool, account_id, user_id).await?;

        let user = sqlx::query!(
            "SELECT pronouns, backstory FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(user.pronouns.chars().count(), 50);
        assert_eq!(user.backstory.chars().count(), 2000);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_empty_imported_fields_leave_user_untouched(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 4301).await?;

        // Stage an account where pronouns, country, backstory are all empty.
        let mut account = play_account(12);
        account.pronouns = String::new();
        account.country = String::new();
        account.backstory = String::new();
        let account_id = stage_empty_account(&pool, &account).await?;

        claim_account(&pool, account_id, user_id).await?;

        let user = sqlx::query!(
            "SELECT pronouns, country, backstory FROM users WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        // User fields remain at their DEFAULT '' — empty imported fields
        // don't write anything.
        assert_eq!(user.pronouns, "");
        assert_eq!(user.country, "");
        assert_eq!(user.backstory, "");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn auto_claim_matches_by_github_uid(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 5001).await?;

        let mut linked = play_account(5);
        linked.github_uid = Some(5001);
        linked.github_login = Some("gh-user-5001".to_string());
        stage_empty_account(&pool, &linked).await?;

        // A different GitHub ID finds nothing.
        assert!(try_auto_claim(&pool, user_id, 9999).await?.is_none());

        let summary = try_auto_claim(&pool, user_id, 5001)
            .await?
            .expect("auto-claim should fire");
        assert_eq!(summary.username, "player5");

        // Idempotent on next login.
        assert!(try_auto_claim(&pool, user_id, 5001).await?.is_none());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn email_lookup_is_case_insensitive_and_skips_claimed(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 6001).await?;
        let account_id = stage_full_account(&pool, 6).await?;

        let found = find_unclaimed_by_email(&pool, "PLAYER6@EXAMPLE.COM").await?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].imported_account_id, account_id);

        claim_account(&pool, account_id, user_id).await?;
        assert!(
            find_unclaimed_by_email(&pool, "player6@example.com")
                .await?
                .is_empty()
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn staging_is_idempotent_and_preserves_claims(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 7001).await?;
        let account_id = stage_full_account(&pool, 7).await?;
        claim_account(&pool, account_id, user_id).await?;

        // Re-import with updated play data: claim state must survive.
        let mut updated = play_account(7);
        updated.display_name = "Renamed".to_string();
        let same_id = stage_empty_account(&pool, &updated).await?;
        assert_eq!(same_id, account_id);

        let account = sqlx::query!(
            "SELECT display_name, claimed_by_user_id FROM imported_accounts
             WHERE imported_account_id = $1",
            account_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(account.display_name, "Player 7");
        assert_eq!(account.claimed_by_user_id, Some(user_id));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_attempts_count_by_user_and_email(pool: PgPool) -> cja::Result<()> {
        let attacker = create_user(&pool, 8001).await?;
        let other = create_user(&pool, 8002).await?;

        // First attempt: one for this user, one for this email (itself).
        let c = record_and_count_claim_attempts(&pool, attacker, "victim@example.com").await?;
        assert_eq!(c.by_user, 1);
        assert_eq!(c.by_email, 1);

        // Same user, different email: user count climbs, email count for
        // the new address starts fresh.
        let c = record_and_count_claim_attempts(&pool, attacker, "other@example.com").await?;
        assert_eq!(c.by_user, 2);
        assert_eq!(c.by_email, 1);

        // A DIFFERENT arena user hammering the SAME victim email: the
        // per-email window keeps climbing across users (the key defense —
        // per-user limits alone wouldn't catch this), case-insensitively.
        let c = record_and_count_claim_attempts(&pool, other, "VICTIM@example.com").await?;
        assert_eq!(c.by_user, 1);
        assert_eq!(c.by_email, 2);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reimport_survives_github_uid_move_between_users(pool: PgPool) -> cja::Result<()> {
        // Import 1: user A owns GitHub uid 100.
        let mut a = play_account(1);
        a.github_uid = Some(100);
        stage_empty_account(&pool, &a).await?;

        // Between imports, in play, A unlinks and B links the same GitHub
        // account. Import 2 processes B first (stale A row still holds 100).
        let mut b = play_account(2);
        b.github_uid = Some(100);
        stage_empty_account(&pool, &b).await?; // must NOT abort on the uid unique index

        // B now owns the uid; A's stale row was released to NULL.
        let a_uid =
            sqlx::query!("SELECT github_uid FROM imported_accounts WHERE play_user_id = 'usr_1'")
                .fetch_one(&pool)
                .await?
                .github_uid;
        assert_eq!(a_uid, None);

        let b_owner = find_unclaimed_by_github_uid(&pool, 100)
            .await?
            .expect("uid 100 should resolve to exactly one account");
        assert_eq!(b_owner.play_user_id, "usr_2");

        // Then A is processed with its real (now unlinked) play data.
        let mut a_now = play_account(1);
        a_now.github_uid = None;
        stage_empty_account(&pool, &a_now).await?;
        assert_eq!(
            find_unclaimed_by_github_uid(&pool, 100)
                .await?
                .unwrap()
                .play_user_id,
            "usr_2"
        );

        Ok(())
    }

    fn complete_payload(n: u32) -> StagePlayAccount {
        StagePlayAccount {
            account: play_account(n),
            snakes: vec![StageSnake {
                play_snake_id: format!("snk_{n}"),
                play_account_id: format!("act_{n}"),
                name: "Hissy".to_string(),
                url: "https://example.com/snake".to_string(),
                head: "alligator".to_string(),
                tail: "default".to_string(),
                color: "#ff0000".to_string(),
                is_public: true,
                engine_region: EngineRegion::UsWest1,
            }],
            grants: vec![StageGrant {
                customization_type: "head".to_string(),
                slug: "alligator".to_string(),
            }],
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stage_rolls_back_new_and_refreshed_payloads_on_foreign_snake(
        pool: PgPool,
    ) -> cja::Result<()> {
        stage_play_account(&pool, &complete_payload(101)).await?;
        let mut new = complete_payload(102);
        new.snakes.push(complete_payload(101).snakes.remove(0));
        assert!(stage_play_account(&pool, &new).await.is_err());
        let new_count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM imported_accounts WHERE play_user_id='usr_102'"
        )
        .fetch_one(&pool)
        .await?
        .unwrap_or_default();
        assert_eq!(new_count, 0);

        let original = complete_payload(103);
        let id = stage_play_account(&pool, &original)
            .await?
            .imported_account_id;
        let before: (String, chrono::DateTime<chrono::Utc>, serde_json::Value, serde_json::Value) = sqlx::query_as(
            "SELECT display_name, updated_at, (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.play_snake_id), '[]'::jsonb) FROM imported_snakes s WHERE s.imported_account_id=a.imported_account_id), (SELECT coalesce(jsonb_agg(to_jsonb(g) ORDER BY g.slug), '[]'::jsonb) FROM imported_grants g WHERE g.imported_account_id=a.imported_account_id) FROM imported_accounts a WHERE imported_account_id=$1"
        ).bind(id).fetch_one(&pool).await?;
        let mut refresh = complete_payload(103);
        refresh.account.display_name = "Changed".to_string();
        refresh.grants.push(StageGrant {
            customization_type: "tail".to_string(),
            slug: "alligator".to_string(),
        });
        refresh.snakes.push(complete_payload(101).snakes.remove(0));
        assert!(stage_play_account(&pool, &refresh).await.is_err());
        let after: (String, chrono::DateTime<chrono::Utc>, serde_json::Value, serde_json::Value) = sqlx::query_as(
            "SELECT display_name, updated_at, (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.play_snake_id), '[]'::jsonb) FROM imported_snakes s WHERE s.imported_account_id=a.imported_account_id), (SELECT coalesce(jsonb_agg(to_jsonb(g) ORDER BY g.slug), '[]'::jsonb) FROM imported_grants g WHERE g.imported_account_id=a.imported_account_id) FROM imported_accounts a WHERE imported_account_id=$1"
        ).bind(id).fetch_one(&pool).await?;
        assert_eq!(before, after);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stage_is_noop_and_claimed_identity_is_frozen(pool: PgPool) -> cja::Result<()> {
        let payload = complete_payload(104);
        let id = stage_play_account(&pool, &payload)
            .await?
            .imported_account_id;
        let before: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, uuid::Uuid, uuid::Uuid) = sqlx::query_as(
            "SELECT a.updated_at, s.updated_at, s.imported_snake_id, g.imported_grant_id FROM imported_accounts a JOIN imported_snakes s USING (imported_account_id) JOIN imported_grants g USING (imported_account_id) WHERE a.imported_account_id=$1"
        ).bind(id).fetch_one(&pool).await?;
        assert_eq!(
            stage_play_account(&pool, &payload).await?.status,
            StageStatus::Unchanged
        );
        let same: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, uuid::Uuid, uuid::Uuid) = sqlx::query_as(
            "SELECT a.updated_at, s.updated_at, s.imported_snake_id, g.imported_grant_id FROM imported_accounts a JOIN imported_snakes s USING (imported_account_id) JOIN imported_grants g USING (imported_account_id) WHERE a.imported_account_id=$1"
        ).bind(id).fetch_one(&pool).await?;
        assert_eq!(before, same);
        let user_id = create_user(&pool, 8104).await?;
        claim_account(&pool, id, user_id).await?;
        let mut changed = complete_payload(104);
        changed.account.display_name = "Changed".to_string();
        changed.snakes.push(StageSnake {
            play_snake_id: "extra".to_string(),
            ..changed.snakes[0].clone()
        });
        changed.grants.push(StageGrant {
            customization_type: "tail".to_string(),
            slug: "alligator".to_string(),
        });
        let result = stage_play_account(&pool, &changed).await?;
        assert_eq!(result.status, StageStatus::SkippedClaimed);
        assert_eq!(result.snakes_processed, 0);
        assert_eq!(result.grants_processed, 2);
        let row = sqlx::query!("SELECT display_name, claimed_by_user_id FROM imported_accounts WHERE imported_account_id=$1", id).fetch_one(&pool).await?;
        assert_eq!(row.display_name, payload.account.display_name);
        assert_eq!(row.claimed_by_user_id, Some(user_id));
        let snakes: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM imported_snakes WHERE imported_account_id=$1",
            id
        )
        .fetch_one(&pool)
        .await?
        .unwrap_or_default();
        assert_eq!(snakes, 1);
        let grants: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM imported_grants WHERE imported_account_id=$1",
            id
        )
        .fetch_one(&pool)
        .await?
        .unwrap_or_default();
        assert_eq!(grants, 2);
        Ok(())
    }

    async fn staging_test_pool(pool: &PgPool, name: &str) -> cja::Result<(PgPool, i32)> {
        let options = pool
            .connect_options()
            .as_ref()
            .clone()
            .application_name(name);
        let dedicated = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&dedicated)
            .await?;
        Ok((dedicated, pid))
    }

    async fn wait_for_blocker(
        pool: &PgPool,
        blocked_pid: i32,
        blocker_pid: i32,
    ) -> cja::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT $1 = ANY(pg_blocking_pids($2))")
                    .bind(blocker_pid)
                    .bind(blocked_pid)
                    .fetch_one(pool)
                    .await?;
                if blocked {
                    return Ok::<(), sqlx::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .wrap_err("Timed out waiting for PostgreSQL blocker")??;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn new_account_is_invisible_until_children_commit(pool: PgPool) -> cja::Result<()> {
        let mut owner = complete_payload(201);
        owner.account.github_uid = Some(90201);
        stage_play_account(&pool, &owner).await?;
        let mut lock = pool.begin().await?;
        let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await?;
        sqlx::query!("SELECT imported_account_id FROM imported_accounts WHERE play_user_id='usr_201' FOR UPDATE")
            .fetch_one(&mut *lock).await?;
        let (stage_pool, stage_pid) = staging_test_pool(&pool, "stage-new-1389").await?;
        let mut newcomer = complete_payload(202);
        newcomer.account.github_uid = Some(90201);
        let stage = tokio::spawn(async move { stage_play_account(&stage_pool, &newcomer).await });
        wait_for_blocker(&pool, stage_pid, blocker_pid).await?;
        assert!(
            find_unclaimed_by_email(&pool, "player202@example.com")
                .await?
                .is_empty()
        );
        assert!(find_unclaimed_by_github_uid(&pool, 90201).await?.is_some());
        lock.commit().await?;
        let result = stage.await??;
        assert_eq!(result.status, StageStatus::Created);
        let player = create_user(&pool, 9202).await?;
        let summary = claim_account(&pool, result.imported_account_id, player)
            .await?
            .expect("new account claims");
        assert_eq!((summary.snakes_created, summary.grants_created), (1, 1));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_waits_for_complete_refresh_even_when_parent_fields_match(
        pool: PgPool,
    ) -> cja::Result<()> {
        for (n, change_parent) in [(203, false), (204, true)] {
            let payload = complete_payload(n);
            let id = stage_play_account(&pool, &payload)
                .await?
                .imported_account_id;
            let user_id = create_user(&pool, 9200 + n as i64).await?;
            let mut lock = pool.begin().await?;
            let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *lock)
                .await?;
            sqlx::query!("SELECT imported_snake_id FROM imported_snakes WHERE imported_account_id=$1 FOR UPDATE", id)
                .fetch_one(&mut *lock).await?;
            let (stage_pool, stage_pid) = staging_test_pool(&pool, "stage-refresh-1389").await?;
            let mut refresh = payload.clone();
            if change_parent {
                refresh.account.display_name = "New name".to_string();
            }
            refresh.snakes.push(StageSnake {
                play_snake_id: format!("snk_{n}_extra"),
                ..refresh.snakes[0].clone()
            });
            refresh.grants.push(StageGrant {
                customization_type: "tail".to_string(),
                slug: "alligator".to_string(),
            });
            let stage =
                tokio::spawn(async move { stage_play_account(&stage_pool, &refresh).await });
            wait_for_blocker(&pool, stage_pid, blocker_pid).await?;
            let (claim_pool, claim_pid) = staging_test_pool(&pool, "claim-refresh-1389").await?;
            let claim = tokio::spawn(async move { claim_account(&claim_pool, id, user_id).await });
            wait_for_blocker(&pool, claim_pid, stage_pid).await?;
            lock.commit().await?;
            assert_eq!(stage.await??.status, StageStatus::Refreshed);
            let summary = claim
                .await??
                .expect("claim succeeds after complete refresh");
            assert_eq!((summary.snakes_created, summary.grants_created), (2, 2));
        }
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn staging_skips_identity_and_snakes_when_claim_wins(pool: PgPool) -> cja::Result<()> {
        let payload = complete_payload(205);
        let id = stage_play_account(&pool, &payload)
            .await?
            .imported_account_id;
        let user_id = create_user(&pool, 9205).await?;
        let mut lock = pool.begin().await?;
        let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await?;
        sqlx::query!(
            "SELECT user_id FROM users WHERE user_id=$1 FOR UPDATE",
            user_id
        )
        .fetch_one(&mut *lock)
        .await?;
        let (claim_pool, claim_pid) = staging_test_pool(&pool, "claim-first-1389").await?;
        let claim = tokio::spawn(async move { claim_account(&claim_pool, id, user_id).await });
        wait_for_blocker(&pool, claim_pid, blocker_pid).await?;
        let (stage_pool, stage_pid) = staging_test_pool(&pool, "stage-after-claim-1389").await?;
        let mut refresh = payload.clone();
        refresh.account.display_name = "Changed".to_string();
        refresh.snakes.push(StageSnake {
            play_snake_id: "new-after-claim".to_string(),
            ..refresh.snakes[0].clone()
        });
        refresh.grants.push(StageGrant {
            customization_type: "tail".to_string(),
            slug: "alligator".to_string(),
        });
        let stage = tokio::spawn(async move { stage_play_account(&stage_pool, &refresh).await });
        wait_for_blocker(&pool, stage_pid, claim_pid).await?;
        lock.commit().await?;
        let summary = claim.await??.expect("claim wins");
        assert_eq!((summary.snakes_created, summary.grants_created), (1, 1));
        assert_eq!(stage.await??.status, StageStatus::SkippedClaimed);
        let account = sqlx::query!(
            "SELECT display_name FROM imported_accounts WHERE imported_account_id=$1",
            id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(account.display_name, payload.account.display_name);
        let snake_count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM imported_snakes WHERE imported_account_id=$1",
            id
        )
        .fetch_one(&pool)
        .await?
        .unwrap_or_default();
        assert_eq!(snake_count, 1);
        let grant_count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM imported_grants WHERE imported_account_id=$1",
            id
        )
        .fetch_one(&pool)
        .await?
        .unwrap_or_default();
        assert_eq!(grant_count, 2);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stale_github_lookup_cannot_claim_former_owner(pool: PgPool) -> cja::Result<()> {
        let mut a = complete_payload(206);
        a.account.github_uid = Some(90206);
        stage_play_account(&pool, &a).await?;
        let stale = find_unclaimed_by_github_uid(&pool, 90206)
            .await?
            .expect("A found");
        let mut b = complete_payload(207);
        b.account.github_uid = Some(90206);
        let b_id = stage_play_account(&pool, &b).await?.imported_account_id;
        let user = create_user(&pool, 9206).await?;
        assert!(
            claim_account_inner(&pool, stale.imported_account_id, user, Some(90206))
                .await?
                .is_none()
        );
        assert_eq!(
            find_unclaimed_by_github_uid(&pool, 90206)
                .await?
                .expect("B found")
                .imported_account_id,
            b_id
        );
        assert!(
            claim_account(&pool, stale.imported_account_id, user)
                .await?
                .is_some()
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claimed_github_owner_blocks_transfer_without_mutation(
        pool: PgPool,
    ) -> cja::Result<()> {
        let mut a = complete_payload(209);
        a.account.github_uid = Some(90209);
        let a_id = stage_play_account(&pool, &a).await?.imported_account_id;
        let user = create_user(&pool, 9209).await?;
        claim_account(&pool, a_id, user).await?;
        let mut b = complete_payload(210);
        b.account.github_uid = Some(90209);
        assert!(stage_play_account(&pool, &b).await.is_err());
        assert_eq!(
            find_unclaimed_by_github_uid(&pool, 90209)
                .await?
                .map(|a| a.imported_account_id),
            None
        );
        let owner = sqlx::query!("SELECT github_uid, claimed_by_user_id FROM imported_accounts WHERE imported_account_id=$1", a_id).fetch_one(&pool).await?;
        assert_eq!(owner.github_uid, Some(90209));
        assert_eq!(owner.claimed_by_user_id, Some(user));
        let b_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM imported_accounts WHERE play_user_id='usr_210'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(b_count, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn refresh_completes_historical_partial_row(pool: PgPool) -> cja::Result<()> {
        let historical = sqlx::query!("INSERT INTO imported_accounts (play_user_id, play_account_id, email, username) VALUES ('usr_208', 'act_208', 'player208@example.com', 'player208') RETURNING imported_account_id")
            .fetch_one(&pool).await?;
        sqlx::query!("INSERT INTO imported_grants (imported_account_id, customization_type, slug) VALUES ($1, $2, $3) ON CONFLICT (imported_account_id, customization_type, slug) DO NOTHING",
            historical.imported_account_id, "tail", "obsolete")
            .execute(&pool).await?;
        let payload = complete_payload(208);
        let result = stage_play_account(&pool, &payload).await?;
        assert_eq!(result.status, StageStatus::Refreshed);
        let staged_grants = sqlx::query!(
            "SELECT customization_type, slug FROM imported_grants WHERE imported_account_id=$1",
            result.imported_account_id
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(staged_grants.len(), 1);
        assert_eq!(staged_grants[0].customization_type, "head");
        assert_eq!(staged_grants[0].slug, "alligator");
        let user = create_user(&pool, 9208).await?;
        let summary = claim_account(&pool, result.imported_account_id, user)
            .await?
            .expect("historical row claims");
        assert_eq!((summary.snakes_created, summary.grants_created), (1, 1));
        Ok(())
    }
}
