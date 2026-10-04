//! Importer that copies play's Postgres into arena's staging tables
//! (`imported_accounts` / `imported_snakes` / `imported_grants`).
//!
//! Run via `arena import-play` with PLAY_DATABASE_URL set (read-only play
//! credentials) alongside the usual DATABASE_URL. Idempotent: re-running
//! refreshes play-side data without touching claim state, so it can run
//! repeatedly during the transition window.
//!
//! The hourly grant reconcile derives a play connection from arena credentials,
//! which can write to play. All source reads use a verified READ ONLY transaction.
//!
//! Play reads use runtime queries (not sqlx macros) — the play schema
//! isn't part of arena's compile-time database.

use color_eyre::eyre::Context as _;
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use sqlx::{
    PgPool, Postgres, Row as _, Transaction,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::customizations::catalog::{Head, Tail};
use crate::models::battlesnake::EngineRegion;

use crate::models::imported_account::{
    self, StageAccount, StageGrant, StagePlayAccount, StageSnake, StageStatus,
};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub accounts: u64,
    pub snakes: u64,
    pub snakes_orphaned: u64,
    pub snakes_region_unmapped: u64,
    pub grants: u64,
    pub grants_orphaned: u64,
    pub skipped_claimed: u64,
    pub snakes_skipped_claimed: u64,
    pub accounts_failed: u64,
    pub accountless_users: i64,
    /// Wall-clock staging durations; exclude these from equality assertions on import data.
    pub stage_max_us: u64,
    /// Wall-clock staging durations; exclude these from equality assertions on import data.
    pub stage_p99_us: u64,
}

/// Arena credentials can write to play. This SQL transaction guard is the only
/// protection; verify it on the same connection before any source query.
/// Pooler startup options cannot provide this guard.
async fn begin_read_only_play_transaction(play: &PgPool) -> cja::Result<Transaction<'_, Postgres>> {
    let mut tx = play
        .begin()
        .await
        .wrap_err("Failed to begin Play read transaction")?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to enforce Play read-only snapshot")?;
    let (read_only, isolation): (String, String) = sqlx::query_as(
        "SELECT current_setting('transaction_read_only'), current_setting('transaction_isolation')",
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to verify Play read-only transaction settings")?;
    if read_only != "on" || isolation != "repeatable read" {
        return Err(color_eyre::eyre::eyre!(
            "Play snapshot is not read-only repeatable-read: {read_only}, {isolation}"
        ));
    }
    Ok(tx)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PlayGrant {
    play_account_id: String,
    customization_type: String,
    slug: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct GrantReconcileCounts {
    pub play_grants_read: u64,
    pub newly_staged: u64,
    pub newly_materialized: u64,
    pub skipped_off_catalog: u64,
    pub newly_staged_accounts: u64,
    pub skipped_identity_conflict: u64,
}

/// The Play region tuple is retained even when unknown so import can warn
/// without dropping its snake.
fn map_play_region(id: &str, platform: Option<&str>, region: Option<&str>) -> (EngineRegion, bool) {
    match (platform, region) {
        (Some("GCP"), Some("US-WEST1")) => (EngineRegion::UsWest1, false),
        (Some("GCP"), Some("US-EAST4")) => (EngineRegion::UsEast4, false),
        (Some("GCP"), Some("EUROPE-WEST4")) => (EngineRegion::EuropeWest4, false),
        _ => {
            tracing::warn!(play_snake_id = %id, ?platform, ?region, "Unknown Play engine region; using US-West");
            (EngineRegion::UsWest1, true)
        }
    }
}

async fn read_play_snakes<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
) -> cja::Result<(Vec<StageSnake>, u64)> {
    let rows = sqlx::query(
        r#"SELECT s.id AS play_snake_id, s.account_id, s.name, s.url,
                  s.head, s.tail, s.color, s.is_public, r.platform, r.region
           FROM core_snake s
           LEFT JOIN core_engineregion r ON r.id = s.engine_region_id
           WHERE s.is_archived = false AND s.account_id IS NOT NULL
           ORDER BY s.id"#,
    )
    .fetch_all(executor)
    .await
    .wrap_err("Failed to read Play snakes")?;
    let mut snakes = Vec::with_capacity(rows.len());
    let mut unmapped = 0;
    for row in rows {
        let id: String = row.try_get("play_snake_id")?;
        let platform: Option<String> = row.try_get("platform")?;
        let region: Option<String> = row.try_get("region")?;
        let (engine_region, unknown) = map_play_region(&id, platform.as_deref(), region.as_deref());
        unmapped += u64::from(unknown);
        snakes.push(StageSnake {
            play_snake_id: id,
            play_account_id: row.try_get("account_id")?,
            name: row.try_get("name")?,
            url: row.try_get("url")?,
            head: row.try_get("head")?,
            tail: row.try_get("tail")?,
            color: row.try_get("color")?,
            is_public: row.try_get("is_public")?,
            engine_region,
        });
    }
    Ok((snakes, unmapped))
}

async fn read_play_grants<'e, E: sqlx::PgExecutor<'e>>(executor: E) -> cja::Result<Vec<PlayGrant>> {
    let rows = sqlx::query(
        r#"SELECT g.account_id, c.customization_type, c.slug
           FROM core_snakecustomizationgrant g
           JOIN core_snakecustomization c ON c.id = g.snake_customization_id
           WHERE c.customization_type IN ('head', 'tail')
           ORDER BY g.id"#,
    )
    .fetch_all(executor)
    .await
    .wrap_err("Failed to read Play grants")?;
    rows.into_iter()
        .map(|row| {
            Ok(PlayGrant {
                play_account_id: row.try_get("account_id")?,
                customization_type: row.try_get("customization_type")?,
                slug: row.try_get("slug")?,
            })
        })
        .collect()
}

async fn read_play_accounts<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
) -> cja::Result<Vec<StageAccount>> {
    // One row per play user: identity + profile + optional GitHub link.
    // uid/extra_data are cast to text so this works against both older
    // (text) and newer (jsonb) social-auth schemas.
    //
    // A handful of play users have more than one github social-auth row
    // (e.g. a main + alt GitHub account linked to the same play account).
    // DISTINCT ON collapses them to a single, deterministic link — the most
    // recently created one (highest social-auth id) wins — so the account
    // count is exact and a user's auto-link target can't flip between the
    // repeated imports run during the transition window.
    let account_rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (u.id)
            u.id AS play_user_id,
            u.email,
            u.password,
            u.is_email_verified,
            (u.is_staff OR u.is_superuser) AS is_staff,
            a.id AS play_account_id,
            a.username,
            a.display_name,
            COALESCE(a.pronouns, '') AS pronouns,
            a.country,
            a.backstory,
            a.github_username,
            a.points,
            a.points_high_score,
            a.created AS play_created_at,
            s.uid::text AS github_uid_text,
            s.extra_data::text AS github_extra_data
        FROM authentication_user u
        JOIN core_account a ON a.user_id = u.id
        LEFT JOIN social_auth_usersocialauth s
            ON s.user_id = u.id AND s.provider = 'github'
        ORDER BY u.id, s.id DESC NULLS LAST
        "#,
    )
    .fetch_all(executor)
    .await
    .wrap_err("Failed to read play accounts")?;

    let accounts: Vec<StageAccount> = account_rows
        .into_iter()
        .map(|row| -> cja::Result<StageAccount> {
            let play_user_id: String = row.try_get("play_user_id")?;
            let github_uid = row
                .try_get::<Option<String>, _>("github_uid_text")?
                .and_then(|uid| match uid.parse::<i64>() {
                    Ok(uid) => Some(uid),
                    Err(_) => {
                        tracing::warn!(
                            play_user_id = %play_user_id,
                            uid = %uid,
                            "Non-numeric GitHub uid in play social auth; treating as unlinked"
                        );
                        None
                    }
                });

            // Prefer the OAuth link's login, then the denormalized account field.
            let social_login = row
                .try_get::<Option<String>, _>("github_extra_data")?
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|v| v.get("login").and_then(|l| l.as_str()).map(String::from));
            let denormalized_login: String = row.try_get("github_username")?;
            let github_login = social_login
                .or_else(|| (!denormalized_login.is_empty()).then_some(denormalized_login))
                .filter(|login| !login.is_empty());

            Ok(StageAccount {
                play_user_id,
                play_account_id: row.try_get("play_account_id")?,
                email: row.try_get("email")?,
                password_hash: row.try_get("password")?,
                is_email_verified: row.try_get("is_email_verified")?,
                username: row.try_get("username")?,
                display_name: row.try_get("display_name")?,
                pronouns: row.try_get("pronouns")?,
                country: row.try_get("country")?,
                backstory: row.try_get("backstory")?,
                github_uid,
                github_login,
                points: row.try_get("points")?,
                points_high_score: row.try_get("points_high_score")?,
                is_staff: row.try_get("is_staff")?,
                play_created_at: row.try_get("play_created_at")?,
            })
        })
        .collect::<cja::Result<_>>()
        .wrap_err("Failed to decode Play account snapshot")?;

    Ok(accounts)
}

pub async fn import_from_play(play: &PgPool, arena: &PgPool) -> cja::Result<ImportCounts> {
    let mut counts = ImportCounts::default();
    let mut stage_times_us = Vec::new();
    let mut play_tx = begin_read_only_play_transaction(play).await?;

    // Play users without a core_account (e.g. createsuperuser, which makes
    // only the User) have nothing to migrate and are dropped by the inner
    // JOIN below. Surface the count so the drop isn't silent.
    let accountless: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM authentication_user u
        LEFT JOIN core_account a ON a.user_id = u.id
        WHERE a.id IS NULL
        "#,
    )
    .fetch_one(&mut *play_tx)
    .await
    .wrap_err("Failed to count accountless play users")?;
    counts.accountless_users = accountless;
    if accountless > 0 {
        tracing::warn!(
            accountless_users = accountless,
            "Play users without a core_account are excluded from the import"
        );
    }

    let accounts = read_play_accounts(&mut *play_tx).await?;
    let (snakes, unmapped) = read_play_snakes(&mut *play_tx).await?;
    counts.snakes_region_unmapped = unmapped;
    let grants = read_play_grants(&mut *play_tx).await?;
    play_tx
        .commit()
        .await
        .wrap_err("Failed to finish Play read-only snapshot")?;

    let mut snakes_by_account: HashMap<String, Vec<StageSnake>> = HashMap::new();
    for snake in snakes {
        snakes_by_account
            .entry(snake.play_account_id.clone())
            .or_default()
            .push(snake);
    }
    let mut grants_by_account: HashMap<String, Vec<StageGrant>> = HashMap::new();
    for grant in grants {
        grants_by_account
            .entry(grant.play_account_id)
            .or_default()
            .push(StageGrant {
                customization_type: grant.customization_type,
                slug: grant.slug,
            });
    }
    let account_ids: HashSet<String> = accounts
        .iter()
        .map(|account| account.play_account_id.clone())
        .collect();
    for (owner, children) in &snakes_by_account {
        if !account_ids.contains(owner) {
            counts.snakes_orphaned += children.len() as u64;
            tracing::warn!(play_account_id = %owner, count = children.len(), "Snake owner not staged; skipping");
        }
    }
    for (owner, children) in &grants_by_account {
        if !account_ids.contains(owner) {
            counts.grants_orphaned += children.len() as u64;
            tracing::warn!(play_account_id = %owner, count = children.len(), "Grant owner not staged; skipping");
        }
    }

    for account in accounts {
        let payload = StagePlayAccount {
            snakes: snakes_by_account
                .remove(&account.play_account_id)
                .unwrap_or_default(),
            grants: grants_by_account
                .remove(&account.play_account_id)
                .unwrap_or_default(),
            account,
        };
        let started = std::time::Instant::now();
        let result = imported_account::stage_play_account(arena, &payload).await;
        stage_times_us.push(started.elapsed().as_micros() as u64);
        match result {
            Ok(result) => {
                counts.accounts += 1;
                counts.snakes += result.snakes_processed;
                counts.grants += result.grants_processed;
                match result.status {
                    StageStatus::SkippedClaimed => {
                        counts.skipped_claimed += 1;
                        counts.snakes_skipped_claimed += payload.snakes.len() as u64;
                    }
                    StageStatus::SkippedIdentityConflict(ref conflict) => {
                        counts.skipped_claimed += 1;
                        tracing::warn!(play_account_id = %payload.account.play_account_id, ?conflict, "Play account skipped due to claimed identity conflict");
                    }
                    _ => {}
                }
            }
            Err(error) => {
                counts.accounts_failed += 1;
                tracing::warn!(play_user_id = %payload.account.play_user_id, error = %format!("{error:#}"), "Failed to stage Play account");
            }
        }
    }

    stage_times_us.sort_unstable();
    counts.stage_max_us = stage_times_us.last().copied().unwrap_or_default();
    if !stage_times_us.is_empty() {
        counts.stage_p99_us =
            stage_times_us[(stage_times_us.len() * 99 / 100).min(stage_times_us.len() - 1)];
    }

    Ok(counts)
}

async fn stage_reconcile_grants(
    tx: &mut Transaction<'_, Postgres>,
    grants: &[PlayGrant],
) -> cja::Result<u64> {
    let account_ids: Vec<String> = grants.iter().map(|g| g.play_account_id.clone()).collect();
    let types: Vec<String> = grants
        .iter()
        .map(|g| g.customization_type.clone())
        .collect();
    let slugs: Vec<String> = grants.iter().map(|g| g.slug.clone()).collect();
    let result = sqlx::query!(
        r#"INSERT INTO imported_grants (imported_account_id, customization_type, slug)
           SELECT ia.imported_account_id, g.customization_type, g.slug
           FROM UNNEST($1::text[], $2::text[], $3::text[])
                AS g(play_account_id, customization_type, slug)
           JOIN imported_accounts ia ON ia.play_account_id = g.play_account_id
           ORDER BY ia.imported_account_id, g.customization_type, g.slug
           ON CONFLICT (imported_account_id, customization_type, slug) DO NOTHING"#,
        &account_ids,
        &types,
        &slugs,
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to stage reconciled play grants")?;
    Ok(result.rows_affected())
}

async fn materialize_reconcile_grants(tx: &mut Transaction<'_, Postgres>) -> cja::Result<u64> {
    let catalog: Vec<(&str, &str)> = Head::ALL
        .iter()
        .map(|head| (Head::KIND, head.slug()))
        .chain(Tail::ALL.iter().map(|tail| (Tail::KIND, tail.slug())))
        .collect();
    let types: Vec<String> = catalog
        .iter()
        .map(|(kind, _)| (*kind).to_string())
        .collect();
    let slugs: Vec<String> = catalog
        .iter()
        .map(|(_, slug)| (*slug).to_string())
        .collect();
    let result = sqlx::query!(
        r#"INSERT INTO customization_grants (user_id, customization_type, slug)
           SELECT ia.claimed_by_user_id, ig.customization_type, ig.slug
           FROM imported_grants ig
           JOIN imported_accounts ia ON ia.imported_account_id = ig.imported_account_id
           JOIN UNNEST($1::text[], $2::text[])
                AS catalog(customization_type, slug)
             ON (catalog.customization_type, catalog.slug) = (ig.customization_type, ig.slug)
           WHERE ia.claimed_by_user_id IS NOT NULL
           ORDER BY ia.claimed_by_user_id, ig.customization_type, ig.slug
           ON CONFLICT (user_id, customization_type, slug) DO NOTHING"#,
        &types,
        &slugs,
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to materialize reconciled play grants")?;
    Ok(result.rows_affected())
}

pub async fn reconcile_grants(play: &PgPool, arena: &PgPool) -> cja::Result<GrantReconcileCounts> {
    let mut play_tx = begin_read_only_play_transaction(play).await?;
    let accounts = read_play_accounts(&mut *play_tx).await?;
    let (snakes, _) = read_play_snakes(&mut *play_tx).await?;
    let mut grants = read_play_grants(&mut *play_tx).await?;
    play_tx
        .commit()
        .await
        .wrap_err("Failed to commit play read-only transaction")?;

    let mut counts = GrantReconcileCounts {
        play_grants_read: grants.len() as u64,
        ..Default::default()
    };
    grants.sort_unstable();
    grants.dedup();
    let mut valid = Vec::with_capacity(grants.len());
    for grant in grants {
        let in_catalog = match grant.customization_type.as_str() {
            "head" => Head::from_slug(&grant.slug).is_some(),
            "tail" => Tail::from_slug(&grant.slug).is_some(),
            _ => false,
        };
        if in_catalog {
            valid.push(grant);
        } else {
            counts.skipped_off_catalog += 1;
        }
    }

    // Existing accounts are grant-only. The lookup is one statement independent
    // of the number of accounts and grants in the source snapshot.
    let staged: HashSet<String> = sqlx::query_scalar!("SELECT play_user_id FROM imported_accounts")
        .fetch_all(arena)
        .await
        .wrap_err("Failed to find staged play users")?
        .into_iter()
        .collect();
    let mut snakes_by_account: HashMap<String, Vec<StageSnake>> = HashMap::new();
    for snake in snakes {
        snakes_by_account
            .entry(snake.play_account_id.clone())
            .or_default()
            .push(snake);
    }
    let mut grants_by_account: HashMap<String, Vec<StageGrant>> = HashMap::new();
    for grant in &valid {
        grants_by_account
            .entry(grant.play_account_id.clone())
            .or_default()
            .push(StageGrant {
                customization_type: grant.customization_type.clone(),
                slug: grant.slug.clone(),
            });
    }
    let mut existing_grants = Vec::new();
    for account in accounts {
        if staged.contains(&account.play_user_id) {
            if let Some(grants) = grants_by_account.remove(&account.play_account_id) {
                existing_grants.extend(grants.into_iter().map(|grant| PlayGrant {
                    play_account_id: account.play_account_id.clone(),
                    customization_type: grant.customization_type,
                    slug: grant.slug,
                }));
            }
            continue;
        }
        let payload = StagePlayAccount {
            snakes: snakes_by_account
                .remove(&account.play_account_id)
                .unwrap_or_default(),
            grants: grants_by_account
                .remove(&account.play_account_id)
                .unwrap_or_default(),
            account,
        };
        let result = imported_account::stage_play_account(arena, &payload)
            .await
            .wrap_err("Failed to atomically stage new play account")?;
        match result.status {
            StageStatus::Created => {
                counts.newly_staged_accounts += 1;
                counts.newly_staged += payload.grants.len() as u64;
            }
            StageStatus::SkippedIdentityConflict(conflict) => {
                counts.skipped_identity_conflict += 1;
                match conflict {
                    imported_account::IdentityConflict::GithubUid(uid) => tracing::warn!(
                        play_account_id = %payload.account.play_account_id,
                        github_uid = uid,
                        "Play account skipped due to claimed identity conflict"
                    ),
                    imported_account::IdentityConflict::SnakeId(snake_id) => tracing::warn!(
                        play_account_id = %payload.account.play_account_id,
                        play_snake_id = %snake_id,
                        "Play account skipped due to claimed identity conflict"
                    ),
                }
            }
            StageStatus::Refreshed | StageStatus::Unchanged | StageStatus::SkippedClaimed => {
                // Another run published this account after the lookup.
                existing_grants.extend(payload.grants.into_iter().map(|grant| PlayGrant {
                    play_account_id: payload.account.play_account_id.clone(),
                    customization_type: grant.customization_type,
                    slug: grant.slug,
                }));
            }
        }
    }
    let mut arena_tx = arena
        .begin()
        .await
        .wrap_err("Failed to begin arena grant transaction")?;
    counts.newly_staged += stage_reconcile_grants(&mut arena_tx, &existing_grants).await?;
    counts.newly_materialized = materialize_reconcile_grants(&mut arena_tx).await?;
    arena_tx
        .commit()
        .await
        .wrap_err("Failed to commit arena grant transaction")?;
    Ok(counts)
}

pub async fn run_grant_reconcile(
    database_url: &str,
    database_name: &str,
    arena: &PgPool,
) -> cja::Result<Option<GrantReconcileCounts>> {
    let options = PgConnectOptions::from_str(database_url)
        .wrap_err("Failed to parse arena database options for play grant reconcile")?
        .database(database_name);
    let play = match PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
    {
        Ok(play) => play,
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("3D000") => {
            return Ok(None);
        }
        Err(error) => return Err(error).wrap_err("Failed to connect to play for grant reconcile"),
    };
    let result = reconcile_grants(&play, arena).await;
    play.close().await;
    result.map(Some)
}

/// Print deterministic literal SQL inputs without modifying either database.
pub async fn export_play_regions() -> cja::Result<()> {
    use std::collections::{HashMap, HashSet};
    let play_url = std::env::var("PLAY_DATABASE_URL")?;
    let arena_url = std::env::var("DATABASE_URL")?;
    async fn readonly_pool(url: &str) -> cja::Result<PgPool> {
        Ok(sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET default_transaction_read_only = on")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await?)
    }
    let play = readonly_pool(&play_url).await?;
    let arena = readonly_pool(&arena_url).await?;
    let mut play_tx = begin_read_only_play_transaction(&play).await?;
    let (snakes, unmapped) = read_play_snakes(&mut *play_tx).await?;
    play_tx
        .commit()
        .await
        .wrap_err("Failed to finish Play region read-only snapshot")?;
    let imported: HashSet<String> = sqlx::query_scalar("SELECT play_snake_id FROM imported_snakes")
        .fetch_all(&arena)
        .await?
        .into_iter()
        .collect();
    let materialized: HashSet<uuid::Uuid> = sqlx::query_scalar(
        "SELECT materialized_battlesnake_id FROM imported_snakes WHERE materialized_battlesnake_id IS NOT NULL"
    ).fetch_all(&arena).await?.into_iter().collect();
    let mut by_url: HashMap<String, HashSet<String>> = HashMap::new();
    for snake in &snakes {
        by_url
            .entry(snake.url.to_lowercase().trim_end_matches('/').to_string())
            .or_default()
            .insert(snake.engine_region.as_str().to_string());
    }
    for region in [EngineRegion::UsEast4, EngineRegion::EuropeWest4] {
        let mut ids: Vec<_> = snakes
            .iter()
            .filter(|s| s.engine_region == region && imported.contains(&s.play_snake_id))
            .map(|s| s.play_snake_id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.iter().any(|id| {
            !id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }) {
            return Err(color_eyre::eyre::eyre!(
                "Play snake ID cannot be represented safely in a SQL array literal"
            ));
        }
        println!("-- {} imported: {}", region.as_str(), ids.len());
        println!(
            "UPDATE imported_snakes SET engine_region = '{}' WHERE play_snake_id = ANY('{{{}}}'::text[]);",
            region.as_str(),
            ids.join(",")
        );
        let native: Vec<(uuid::Uuid, String)> =
            sqlx::query_as("SELECT battlesnake_id, url FROM battlesnakes")
                .fetch_all(&arena)
                .await?;
        let mut native_ids: Vec<_> = native
            .iter()
            .filter(|(id, url)| {
                !materialized.contains(id)
                    && by_url
                        .get(url.to_lowercase().trim_end_matches('/'))
                        .is_some_and(|regions| {
                            regions.len() == 1 && regions.contains(region.as_str())
                        })
            })
            .map(|(id, _)| id.to_string())
            .collect();
        native_ids.sort();
        println!("-- {} native: {}", region.as_str(), native_ids.len());
        println!(
            "UPDATE battlesnakes SET engine_region = '{}' WHERE battlesnake_id = ANY('{{{}}}'::uuid[]);",
            region.as_str(),
            native_ids.join(",")
        );
    }
    eprintln!(
        "Play snakes: {}, staged: {}, unmapped: {}",
        snakes.len(),
        imported.len(),
        unmapped
    );
    Ok(())
}

/// Entry point for the `arena import-play` subcommand.
pub async fn run_import() -> cja::Result<()> {
    let play_url = std::env::var("PLAY_DATABASE_URL")
        .wrap_err("PLAY_DATABASE_URL must be set (read-only play Postgres credentials)")?;
    let arena_url =
        std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set (arena Postgres)")?;

    let play = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&play_url)
        .await
        .wrap_err("Failed to connect to play database")?;
    let arena = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&arena_url)
        .await
        .wrap_err("Failed to connect to arena database")?;

    sqlx::migrate!("../migrations")
        .run(&arena)
        .await
        .wrap_err("Failed to run arena migrations")?;

    let counts = import_from_play(&play, &arena).await?;
    finish_import(&counts)
}

fn format_import_counts(counts: &ImportCounts) -> String {
    format!(
        "Imported {} accounts ({} staged/verified, {} skipped claimed, {} failed), {} snakes ({} skipped claimed, {} orphaned, {} unmapped regions), {} grants ({} orphaned), {} accountless users; staging max {} us, p99 {} us",
        counts.accounts,
        counts.accounts - counts.skipped_claimed,
        counts.skipped_claimed,
        counts.accounts_failed,
        counts.snakes,
        counts.snakes_skipped_claimed,
        counts.snakes_orphaned,
        counts.snakes_region_unmapped,
        counts.grants,
        counts.grants_orphaned,
        counts.accountless_users,
        counts.stage_max_us,
        counts.stage_p99_us
    )
}

fn finish_import(counts: &ImportCounts) -> cja::Result<()> {
    println!("{}", format_import_counts(counts));

    if counts.accounts_failed > 0 {
        return Err(color_eyre::eyre::eyre!(
            "{} Play accounts failed staging",
            counts.accounts_failed
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::imported_account::{StageAccount, claim_account};
    use uuid::Uuid;

    async fn test_play_account(
        pool: &PgPool,
        number: u32,
        github_uid: Option<i64>,
    ) -> cja::Result<()> {
        let user = format!("reconcile_user_{number}");
        let account = format!("reconcile_account_{number}");
        sqlx::query(
            "INSERT INTO authentication_user (id, email, password) VALUES ($1, $2, 'test-hash')",
        )
        .bind(&user)
        .bind(format!("reconcile-{number}@example.com"))
        .execute(pool)
        .await?;
        sqlx::query("INSERT INTO core_account (id, user_id, username, display_name) VALUES ($1, $2, $3, $4)")
            .bind(&account).bind(&user).bind(format!("reconcile_{number}"))
            .bind(format!("Reconcile {number}")).execute(pool).await?;
        if let Some(uid) = github_uid {
            sqlx::query("INSERT INTO social_auth_usersocialauth (user_id, provider, uid) VALUES ($1, 'github', $2)")
                .bind(&user).bind(uid.to_string()).execute(pool).await?;
        }
        Ok(())
    }

    async fn test_imported_account(pool: &PgPool, number: u32) -> cja::Result<Uuid> {
        test_play_account(pool, number, None).await?;
        Ok(imported_account::stage_play_account(
            pool,
            &StagePlayAccount {
                account: StageAccount {
                    play_user_id: format!("reconcile_user_{number}"),
                    play_account_id: format!("reconcile_account_{number}"),
                    email: format!("reconcile-{number}@example.com"),
                    password_hash: "test-hash".to_string(),
                    is_email_verified: true,
                    username: format!("reconcile_{number}"),
                    display_name: format!("Reconcile {number}"),
                    pronouns: String::new(),
                    country: String::new(),
                    backstory: String::new(),
                    github_uid: None,
                    github_login: None,
                    points: 0,
                    points_high_score: 0,
                    is_staff: false,
                    play_created_at: None,
                },
                snakes: vec![],
                grants: vec![],
            },
        )
        .await?
        .imported_account_id)
    }

    async fn test_user(pool: &PgPool, number: i64) -> cja::Result<Uuid> {
        Ok(sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token') RETURNING user_id",
            number,
            format!("reconcile-gh-{number}")
        )
        .fetch_one(pool)
        .await?
        .user_id)
    }

    async fn test_play_grant(
        pool: &PgPool,
        id: &str,
        account: &str,
        kind: &str,
        slug: &str,
    ) -> cja::Result<()> {
        sqlx::query("INSERT INTO core_snakecustomization (id, customization_type, slug) VALUES ($1, $2, $3)")
            .bind(id).bind(kind).bind(slug).execute(pool).await?;
        sqlx::query("INSERT INTO core_snakecustomizationgrant (id, account_id, snake_customization_id) VALUES ($1, $2, $3)")
            .bind(format!("grant_{id}")).bind(account).bind(id).execute(pool).await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_grant_bought_after_claim(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        let account = test_imported_account(&pool, 1).await?;
        let user = test_user(&pool, 15281).await?;
        let claim = claim_account(&pool, account, user)
            .await?
            .expect("claim succeeds");
        assert_eq!(claim.grants_created, 0);
        assert_eq!(
            crate::customizations::resolve_head(&pool, user, "alligator").await?,
            "default"
        );

        test_play_grant(
            &pool,
            "late_head",
            "reconcile_account_1",
            "head",
            "alligator",
        )
        .await?;
        let counts = reconcile_grants(&pool, &pool).await?;
        assert_eq!(counts.play_grants_read, 1);
        assert_eq!(counts.newly_staged, 1);
        assert_eq!(counts.newly_materialized, 1);
        assert!(
            crate::customizations::get_granted_slugs(&pool, user)
                .await?
                .contains(&("head".to_string(), "alligator".to_string()))
        );
        assert_eq!(
            crate::customizations::resolve_head(&pool, user, "alligator").await?,
            "alligator"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_grant_bought_before_claim(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        let account = test_imported_account(&pool, 2).await?;
        test_play_grant(
            &pool,
            "early_head",
            "reconcile_account_2",
            "head",
            "alligator",
        )
        .await?;
        let counts = reconcile_grants(&pool, &pool).await?;
        assert_eq!((counts.newly_staged, counts.newly_materialized), (1, 0));
        let staged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM imported_grants WHERE imported_account_id = $1",
        )
        .bind(account)
        .fetch_one(&pool)
        .await?;
        assert_eq!(staged, 1);

        let user = test_user(&pool, 15282).await?;
        let claim = claim_account(&pool, account, user)
            .await?
            .expect("claim succeeds");
        assert_eq!(claim.grants_created, 1);
        assert!(
            crate::customizations::get_granted_slugs(&pool, user)
                .await?
                .contains(&("head".to_string(), "alligator".to_string()))
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_filters_and_is_idempotent(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        let account = test_imported_account(&pool, 3).await?;
        let user = test_user(&pool, 15283).await?;
        claim_account(&pool, account, user)
            .await?
            .expect("claim succeeds");
        crate::customizations::create_grant(&pool, user, "tail", "alligator").await?;
        test_play_grant(&pool, "valid", "reconcile_account_3", "head", "alligator").await?;
        sqlx::query("INSERT INTO core_snakecustomizationgrant (id, account_id, snake_customization_id) VALUES ('duplicate', 'reconcile_account_3', 'valid')")
            .execute(&pool).await?;
        test_play_grant(
            &pool,
            "invalid",
            "reconcile_account_3",
            "head",
            "off-catalog",
        )
        .await?;
        test_play_grant(&pool, "unknown", "missing_account", "head", "beluga").await?;
        test_play_grant(&pool, "color", "reconcile_account_3", "color", "blue").await?;
        let first = reconcile_grants(&pool, &pool).await?;
        assert_eq!(first.play_grants_read, 4);
        assert_eq!(first.newly_staged, 1);
        assert_eq!(first.newly_materialized, 1);
        assert_eq!(first.skipped_off_catalog, 1);
        assert_eq!(first.newly_staged_accounts, 0);
        assert_eq!(first.skipped_identity_conflict, 0);
        let second = reconcile_grants(&pool, &pool).await?;
        assert_eq!((second.newly_staged, second.newly_materialized), (0, 0));
        let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imported_accounts")
            .fetch_one(&pool)
            .await?;
        let snakes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imported_snakes")
            .fetch_one(&pool)
            .await?;
        let staged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imported_grants")
            .fetch_one(&pool)
            .await?;
        assert_eq!((accounts, snakes, staged), (1, 0, 1));
        assert!(
            crate::customizations::get_granted_slugs(&pool, user)
                .await?
                .contains(&("tail".to_string(), "alligator".to_string()))
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_stages_new_account_with_active_snakes_and_grants(
        pool: PgPool,
    ) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        test_play_account(&pool, 10, Some(152810)).await?;
        sqlx::query("INSERT INTO core_snake (id, account_id, name, url, is_archived) VALUES ('new_active', 'reconcile_account_10', 'Active', 'https://example.com/a', false), ('new_archived', 'reconcile_account_10', 'Archived', 'https://example.com/b', true)")
            .execute(&pool).await?;
        test_play_grant(
            &pool,
            "new_head",
            "reconcile_account_10",
            "head",
            "alligator",
        )
        .await?;
        let first = reconcile_grants(&pool, &pool).await?;
        assert_eq!(
            (
                first.newly_staged_accounts,
                first.newly_staged,
                first.skipped_identity_conflict
            ),
            (1, 1, 0)
        );
        let snake_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM imported_snakes s JOIN imported_accounts a USING (imported_account_id) WHERE a.play_account_id = 'reconcile_account_10'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(snake_count, 1);
        let user = test_user(&pool, 152810).await?;
        let claimed = imported_account::try_auto_claim(&pool, user, 152810)
            .await?
            .expect("auto-claim succeeds");
        assert_eq!((claimed.snakes_created, claimed.grants_created), (1, 1));
        let second = reconcile_grants(&pool, &pool).await?;
        assert_eq!(
            (
                second.newly_staged_accounts,
                second.newly_staged,
                second.newly_materialized
            ),
            (0, 0, 0)
        );
        Ok(())
    }

    async fn assert_conflict_skips_account(pool: PgPool, snake_conflict: bool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        test_play_account(&pool, 20, None).await?;
        let mut original = StageAccount {
            play_user_id: "reconcile_user_20".into(),
            play_account_id: "reconcile_account_20".into(),
            email: "reconcile-20@example.com".into(),
            password_hash: "test-hash".into(),
            is_email_verified: false,
            username: "reconcile_20".into(),
            display_name: "Reconcile 20".into(),
            pronouns: String::new(),
            country: String::new(),
            backstory: String::new(),
            github_uid: Some(152820),
            github_login: None,
            points: 0,
            points_high_score: 0,
            is_staff: false,
            play_created_at: None,
        };
        let snake = StageSnake {
            play_snake_id: "claimed_snake".into(),
            play_account_id: "reconcile_account_20".into(),
            name: "Claimed".into(),
            url: "https://example.com/c".into(),
            head: "default".into(),
            tail: "default".into(),
            color: "#888888".into(),
            is_public: false,
            engine_region: EngineRegion::UsWest1,
        };
        let original_id = imported_account::stage_play_account(
            &pool,
            &StagePlayAccount {
                account: original.clone(),
                snakes: vec![snake],
                grants: vec![],
            },
        )
        .await?
        .imported_account_id;
        let user = test_user(&pool, 152820).await?;
        claim_account(&pool, original_id, user)
            .await?
            .expect("claim succeeds");
        original.github_uid = if snake_conflict {
            Some(152822)
        } else {
            Some(152820)
        };
        let newcomer = 21;
        test_play_account(&pool, newcomer, original.github_uid).await?;
        if snake_conflict {
            sqlx::query("INSERT INTO core_snake (id, account_id, name, url) VALUES ('claimed_snake', 'reconcile_account_21', 'Conflict', 'https://example.com/d')")
                .execute(&pool).await?;
        }
        test_play_grant(
            &pool,
            "conflict_head",
            "reconcile_account_21",
            "head",
            "alligator",
        )
        .await?;
        let counts = reconcile_grants(&pool, &pool).await?;
        assert_eq!(counts.skipped_identity_conflict, 1);
        assert_eq!(counts.newly_staged_accounts, 0);
        let rows: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM imported_accounts WHERE play_account_id = 'reconcile_account_21'),
                    (SELECT COUNT(*) FROM imported_snakes WHERE play_snake_id = 'claimed_snake' AND imported_account_id NOT IN (SELECT imported_account_id FROM imported_accounts WHERE play_account_id = 'reconcile_account_20')),
                    (SELECT COUNT(*) FROM imported_grants ig JOIN imported_accounts ia USING (imported_account_id) WHERE ia.play_account_id = 'reconcile_account_21')"
        ).fetch_one(&pool).await?;
        assert_eq!(rows, (0, 0, 0));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_skips_claimed_github_uid_conflict(pool: PgPool) -> cja::Result<()> {
        assert_conflict_skips_account(pool, false).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_skips_claimed_snake_id_conflict(pool: PgPool) -> cja::Result<()> {
        assert_conflict_skips_account(pool, true).await
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_repairs_claim_race(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        let account = test_imported_account(&pool, 4).await?;
        let user = test_user(&pool, 15284).await?;
        let grant = PlayGrant {
            play_account_id: "reconcile_account_4".to_string(),
            customization_type: "head".to_string(),
            slug: "alligator".to_string(),
        };
        let mut tx = pool.begin().await?;
        assert_eq!(stage_reconcile_grants(&mut tx, &[grant]).await?, 1);
        assert_eq!(materialize_reconcile_grants(&mut tx).await?, 0);
        let claim = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            claim_account(&pool, account, user),
        )
        .await??
        .expect("claim succeeds");
        assert_eq!(claim.grants_created, 0);
        tx.commit().await?;
        let claimed: bool = sqlx::query_scalar(
            "SELECT claimed_by_user_id = $1 FROM imported_accounts WHERE imported_account_id = $2",
        )
        .bind(user)
        .bind(account)
        .fetch_one(&pool)
        .await?;
        let staged: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM imported_grants WHERE imported_account_id = $1",
        )
        .bind(account)
        .fetch_one(&pool)
        .await?;
        assert!(claimed);
        assert_eq!(staged, 1);
        let absent: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM customization_grants WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await?;
        assert_eq!(absent, 0);
        let counts = reconcile_grants(&pool, &pool).await?;
        assert_eq!(counts.newly_staged, 0);
        assert_eq!(counts.newly_materialized, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_one_unstageable_new_account_does_not_block_other_grants(
        pool: PgPool,
    ) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        let claimed_account = test_imported_account(&pool, 40).await?;
        let user = test_user(&pool, 15340).await?;
        claim_account(&pool, claimed_account, user)
            .await?
            .expect("claim succeeds");
        // An unclaimed staged account owns this snake id in arena's staging.
        test_play_account(&pool, 41, None).await?;
        imported_account::stage_play_account(
            &pool,
            &StagePlayAccount {
                account: StageAccount {
                    play_user_id: "reconcile_user_41".into(),
                    play_account_id: "reconcile_account_41".into(),
                    email: "reconcile-41@example.com".into(),
                    password_hash: "test-hash".into(),
                    is_email_verified: true,
                    username: "reconcile_41".into(),
                    display_name: "Reconcile 41".into(),
                    pronouns: String::new(),
                    country: String::new(),
                    backstory: String::new(),
                    github_uid: None,
                    github_login: None,
                    points: 0,
                    points_high_score: 0,
                    is_staff: false,
                    play_created_at: None,
                },
                snakes: vec![StageSnake {
                    play_snake_id: "moved_snake".into(),
                    play_account_id: "reconcile_account_41".into(),
                    name: "Moved".into(),
                    url: "https://example.com/m".into(),
                    head: "default".into(),
                    tail: "default".into(),
                    color: "#888888".into(),
                    is_public: false,
                    engine_region: EngineRegion::UsWest1,
                }],
                grants: vec![],
            },
        )
        .await?;
        // On play the snake now belongs to a brand-new account.
        test_play_account(&pool, 42, None).await?;
        sqlx::query("INSERT INTO core_snake (id, account_id, name, url) VALUES ('moved_snake', 'reconcile_account_42', 'Moved', 'https://example.com/m')")
            .execute(&pool).await?;
        test_play_grant(
            &pool,
            "poison_head",
            "reconcile_account_40",
            "head",
            "alligator",
        )
        .await?;

        let counts = reconcile_grants(&pool, &pool).await;
        assert!(
            counts.is_ok(),
            "one unstageable new account must not fail the run: {:#}",
            counts.as_ref().err().unwrap()
        );
        assert!(
            crate::customizations::get_granted_slugs(&pool, user)
                .await?
                .contains(&("head".to_string(), "alligator".to_string())),
            "claimed account's new grant must still be materialized"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn play_transaction_is_read_only(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        let mut tx = begin_read_only_play_transaction(&pool).await?;
        let error = sqlx::query("INSERT INTO core_snakecustomization (id, customization_type, slug) VALUES ('forbidden', 'head', 'alligator')")
            .execute(&mut *tx).await.expect_err("read-only transaction rejects writes");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|db| db.code())
                .as_deref(),
            Some("25006")
        );
        tx.rollback().await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn missing_play_database_is_skipped(pool: PgPool) -> cja::Result<()> {
        let database_url = std::env::var("DATABASE_URL")?;
        let missing = format!("missing_play_{}", Uuid::new_v4().simple());
        assert_eq!(
            run_grant_reconcile(&database_url, &missing, &pool).await?,
            None
        );
        assert!(
            run_grant_reconcile(
                "postgres://invalid:invalid@127.0.0.1:5432/arena",
                "play",
                &pool
            )
            .await
            .is_err()
        );
        let staged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imported_grants")
            .fetch_one(&pool)
            .await?;
        assert_eq!(staged, 0);
        Ok(())
    }

    #[test]
    fn failed_import_reports_count_then_returns_error() {
        let counts = ImportCounts {
            accounts: 2,
            accounts_failed: 1,
            ..ImportCounts::default()
        };
        assert!(format_import_counts(&counts).contains("1 failed"));
        assert!(finish_import(&counts).is_err());
    }

    #[test]
    fn play_region_mapping_keeps_unknown_snakes_at_west() {
        assert_eq!(
            map_play_region("west", Some("GCP"), Some("US-WEST1")),
            (EngineRegion::UsWest1, false)
        );
        assert_eq!(
            map_play_region("east", Some("GCP"), Some("US-EAST4")),
            (EngineRegion::UsEast4, false)
        );
        assert_eq!(
            map_play_region("eu", Some("GCP"), Some("EUROPE-WEST4")),
            (EngineRegion::EuropeWest4, false)
        );
        assert_eq!(
            map_play_region("null", None, None),
            (EngineRegion::UsWest1, true)
        );
        assert_eq!(
            map_play_region("unknown", Some("GCP"), Some("MOON")),
            (EngineRegion::UsWest1, true)
        );
    }

    /// Minimal play-shaped tables (just the columns the importer reads),
    /// created in the arena test database so play-pool == arena-pool in
    /// tests. Column types match play's real schema.
    async fn create_play_tables(pool: &PgPool) -> cja::Result<()> {
        sqlx::raw_sql(
            r#"
            CREATE TABLE authentication_user (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL,
                password TEXT NOT NULL,
                is_email_verified BOOLEAN NOT NULL DEFAULT false,
                is_staff BOOLEAN NOT NULL DEFAULT false,
                is_superuser BOOLEAN NOT NULL DEFAULT false
            );
            CREATE TABLE core_account (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                username TEXT NOT NULL,
                display_name TEXT NOT NULL DEFAULT '',
                pronouns TEXT,
                country TEXT NOT NULL DEFAULT '',
                backstory TEXT NOT NULL DEFAULT '',
                github_username TEXT NOT NULL DEFAULT '',
                points INTEGER NOT NULL DEFAULT 0,
                points_high_score INTEGER NOT NULL DEFAULT 0,
                created TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            CREATE TABLE social_auth_usersocialauth (
                id SERIAL PRIMARY KEY,
                user_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                uid TEXT NOT NULL,
                extra_data JSONB
            );
            CREATE TABLE core_engineregion (
                id TEXT PRIMARY KEY,
                platform TEXT NOT NULL,
                region TEXT NOT NULL
            );
            CREATE TABLE core_snake (
                id TEXT PRIMARY KEY,
                account_id TEXT,
                name TEXT NOT NULL,
                url TEXT NOT NULL DEFAULT '',
                head TEXT NOT NULL DEFAULT 'default',
                tail TEXT NOT NULL DEFAULT 'default',
                color TEXT NOT NULL DEFAULT '#888888',
                is_public BOOLEAN NOT NULL DEFAULT false,
                is_archived BOOLEAN NOT NULL DEFAULT false,
                engine_region_id TEXT
            );
            CREATE TABLE core_snakecustomization (
                id TEXT PRIMARY KEY,
                customization_type TEXT NOT NULL,
                slug TEXT NOT NULL
            );
            CREATE TABLE core_snakecustomizationgrant (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                snake_customization_id TEXT NOT NULL
            );
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn seed_play_data(pool: &PgPool) -> cja::Result<()> {
        sqlx::raw_sql(
            r#"
            INSERT INTO authentication_user (id, email, password, is_email_verified, is_staff, is_superuser) VALUES
                ('usr_linked', 'linked@example.com', 'pbkdf2_sha256$260000$s$h', true, false, false),
                ('usr_legacy', 'legacy@example.com', 'pbkdf2_sha256$260000$s$h', true, false, true),
                ('usr_badsocial', 'bad@example.com', '!unusablepasswordsentinel', false, false, false),
                ('usr_multigh', 'multi@example.com', 'pbkdf2_sha256$260000$s$h', true, false, false);

            INSERT INTO core_account (id, user_id, username, display_name, pronouns, country, backstory, github_username, points, points_high_score) VALUES
                ('act_linked', 'usr_linked', 'linkedplayer', 'Linked Player', 'they/them', 'CA', 'story', 'fallback-login', 120, 300),
                ('act_legacy', 'usr_legacy', 'legacyplayer', '', NULL, '', '', '', 0, 50),
                ('act_badsocial', 'usr_badsocial', 'badsocial', 'Bad Social', NULL, '', '', '', 0, 0),
                ('act_multigh', 'usr_multigh', 'multigh', 'Multi GH', NULL, '', '', '', 0, 0);

            -- usr_multigh has two github links (a main + alt account). The
            -- one inserted later gets the higher serial id and must win, so
            -- the account is staged once against a deterministic uid.
            INSERT INTO social_auth_usersocialauth (user_id, provider, uid, extra_data) VALUES
                ('usr_linked', 'github', '777001', '{"login": "linked-gh"}'),
                ('usr_linked', 'twitter', '999', '{}'),
                ('usr_badsocial', 'github', 'not-a-number', '{}'),
                ('usr_multigh', 'github', '111000', '{"login": "old-alt"}'),
                ('usr_multigh', 'github', '222000', '{"login": "new-main"}');

            INSERT INTO core_snake (id, account_id, name, url, head, tail, color, is_public, is_archived) VALUES
                ('snk_1', 'act_linked', 'Alpha', 'https://example.com/a', 'beluga', 'default', '#ff0000', true, false),
                ('snk_2', 'act_linked', 'Archived', 'https://example.com/b', 'default', 'default', '#00ff00', false, true),
                ('snk_3', 'act_legacy', 'Legacy Snake', 'https://example.com/c', 'default', 'default', '#0000ff', false, false),
                ('snk_4', NULL, 'Orphan', 'https://example.com/d', 'default', 'default', '#888888', false, false),
                ('snk_5', 'act_gone', 'GoneOwner', 'https://example.com/e', 'default', 'default', '#888888', false, false);

            INSERT INTO core_snakecustomization (id, customization_type, slug) VALUES
                ('scst_head', 'head', 'alligator'),
                ('scst_tail', 'tail', 'alligator'),
                ('scst_color', 'color', 'some-color');

            INSERT INTO core_snakecustomizationgrant (id, account_id, snake_customization_id) VALUES
                ('scg_1', 'act_linked', 'scst_head'),
                ('scg_2', 'act_linked', 'scst_tail'),
                ('scg_3', 'act_linked', 'scst_color'),
                ('scg_4', 'act_gone', 'scst_head');
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn import_stages_accounts_snakes_and_grants(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;
        sqlx::raw_sql(
            "INSERT INTO core_engineregion (id, platform, region)
                       VALUES ('east', 'GCP', 'US-EAST4');
                       UPDATE core_snake SET engine_region_id = 'east' WHERE id = 'snk_1';",
        )
        .execute(&pool)
        .await?;

        let counts = import_from_play(&pool, &pool).await?;
        // usr_multigh is staged exactly once despite its two github links,
        // so the count reflects distinct accounts, not joined rows.
        assert_eq!(counts.accounts, 4);
        // Archived and NULL-owner snakes are filtered in SQL; the snake
        // whose owner isn't staged is counted as orphaned.
        assert_eq!(counts.snakes, 2);
        assert_eq!(counts.snakes_orphaned, 1);
        let region: String = sqlx::query_scalar(
            "SELECT engine_region FROM imported_snakes WHERE play_snake_id = 'snk_1'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(region, "us-east4");
        // Color grants are filtered in SQL; the grant with a missing owner
        // is orphaned.
        assert_eq!(counts.grants, 2);
        assert_eq!(counts.grants_orphaned, 1);

        let linked = sqlx::query!(
            r#"SELECT email, username, display_name, pronouns, github_uid,
                      github_login, points, is_staff, is_email_verified
               FROM imported_accounts WHERE play_user_id = 'usr_linked'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(linked.email, "linked@example.com");
        assert_eq!(linked.username, "linkedplayer");
        assert_eq!(linked.pronouns, "they/them");
        assert_eq!(linked.github_uid, Some(777001));
        // extra_data login wins over the denormalized github_username.
        assert_eq!(linked.github_login.as_deref(), Some("linked-gh"));
        assert_eq!(linked.points, 120);
        assert!(!linked.is_staff);
        assert!(linked.is_email_verified);

        // Superuser flag folds into is_staff; NULL pronouns coalesce.
        let legacy = sqlx::query!(
            "SELECT is_staff, pronouns, github_uid FROM imported_accounts
             WHERE play_user_id = 'usr_legacy'"
        )
        .fetch_one(&pool)
        .await?;
        assert!(legacy.is_staff);
        assert_eq!(legacy.pronouns, "");
        assert_eq!(legacy.github_uid, None);

        // Non-numeric social uid degrades to unlinked, import continues.
        let bad = sqlx::query!(
            "SELECT github_uid, github_login FROM imported_accounts
             WHERE play_user_id = 'usr_badsocial'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(bad.github_uid, None);

        // Two github links collapse to the most recently created (highest
        // social-auth id), deterministically — not whichever the join
        // happened to yield last.
        let multi = sqlx::query!(
            "SELECT github_uid, github_login FROM imported_accounts
             WHERE play_user_id = 'usr_multigh'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(multi.github_uid, Some(222000));
        assert_eq!(multi.github_login.as_deref(), Some("new-main"));

        let snake_names: Vec<String> =
            sqlx::query!("SELECT name FROM imported_snakes ORDER BY name")
                .fetch_all(&pool)
                .await?
                .into_iter()
                .map(|r| r.name)
                .collect();
        assert_eq!(
            snake_names,
            vec!["Alpha".to_string(), "Legacy Snake".to_string()]
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn import_is_idempotent(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;

        import_from_play(&pool, &pool).await?;
        // Simulate play-side changes between sync runs.
        sqlx::raw_sql(
            "UPDATE core_account SET display_name = 'Renamed Player' WHERE id = 'act_linked'",
        )
        .execute(&pool)
        .await?;

        let counts = import_from_play(&pool, &pool).await?;
        assert_eq!(counts.accounts, 4);
        // Re-staged grants must count as staged, not orphaned (regression:
        // ON CONFLICT DO NOTHING reported 0 rows_affected on the 2nd run).
        assert_eq!(counts.grants, 2);
        assert_eq!(counts.grants_orphaned, 1);
        // Snakes likewise stay counted on re-import.
        assert_eq!(counts.snakes, 2);

        let total = sqlx::query!(r#"SELECT COUNT(*) as "count!" FROM imported_accounts"#)
            .fetch_one(&pool)
            .await?
            .count;
        assert_eq!(total, 4);

        let renamed = sqlx::query!(
            "SELECT display_name FROM imported_accounts WHERE play_user_id = 'usr_linked'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(renamed.display_name, "Renamed Player");

        let grant_total = sqlx::query!(r#"SELECT COUNT(*) as "count!" FROM imported_grants"#)
            .fetch_one(&pool)
            .await?
            .count;
        assert_eq!(grant_total, 2);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn play_snapshot_is_read_only_and_repeatable(pool: PgPool) -> cja::Result<()> {
        let mut tx = begin_read_only_play_transaction(&pool).await?;
        let (read_only, isolation): (String, String) = sqlx::query_as(
            "SELECT current_setting('transaction_read_only'), current_setting('transaction_isolation')"
        ).fetch_one(&mut *tx).await?;
        assert_eq!(read_only, "on");
        assert_eq!(isolation, "repeatable read");
        assert!(
            sqlx::query("CREATE TABLE must_not_write (id integer)")
                .execute(&mut *tx)
                .await
                .is_err()
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn importer_counts_failed_account_and_continues(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;
        // Existing staging owns snk_1 under a different account. The linked
        // Play account fails atomically; later source accounts still stage.
        let bad_owner = imported_account::StagePlayAccount {
            account: imported_account::StageAccount {
                play_user_id: "other-user".to_string(),
                play_account_id: "other-account".to_string(),
                email: "other@example.com".to_string(),
                password_hash: String::new(),
                is_email_verified: false,
                username: "other".to_string(),
                display_name: String::new(),
                pronouns: String::new(),
                country: String::new(),
                backstory: String::new(),
                github_uid: None,
                github_login: None,
                points: 0,
                points_high_score: 0,
                is_staff: false,
                play_created_at: None,
            },
            snakes: vec![StageSnake {
                play_snake_id: "snk_1".to_string(),
                play_account_id: "other-account".to_string(),
                name: "Owner".to_string(),
                url: "https://example.com/owner".to_string(),
                head: String::new(),
                tail: String::new(),
                color: String::new(),
                is_public: false,
                engine_region: EngineRegion::UsWest1,
            }],
            grants: vec![],
        };
        imported_account::stage_play_account(&pool, &bad_owner).await?;
        let counts = import_from_play(&pool, &pool).await?;
        assert_eq!(counts.accounts_failed, 1);
        assert_eq!(counts.accounts, 3);
        assert_eq!(counts.snakes, 1);
        assert_eq!(counts.snakes_orphaned, 1);
        assert_eq!(counts.grants, 0);
        assert_eq!(counts.grants_orphaned, 1);
        let later: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM imported_accounts WHERE play_user_id='usr_multigh'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(later, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn importer_replaces_unclaimed_children_and_preserves_no_change_rows(
        pool: PgPool,
    ) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;
        import_from_play(&pool, &pool).await?;
        sqlx::raw_sql(
            "UPDATE core_snake SET is_archived=true WHERE id='snk_1'; DELETE FROM core_snakecustomizationgrant WHERE id='scg_1';"
        ).execute(&pool).await?;
        import_from_play(&pool, &pool).await?;
        let after: (serde_json::Value, serde_json::Value, serde_json::Value) = sqlx::query_as(
            "SELECT (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.play_user_id), '[]'::jsonb) FROM imported_accounts a), (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.play_snake_id), '[]'::jsonb) FROM imported_snakes s), (SELECT coalesce(jsonb_agg(to_jsonb(g) ORDER BY g.imported_grant_id), '[]'::jsonb) FROM imported_grants g)"
        ).fetch_one(&pool).await?;
        let third = import_from_play(&pool, &pool).await?;
        assert_eq!(third.accounts_failed, 0);
        let same: (serde_json::Value, serde_json::Value, serde_json::Value) = sqlx::query_as(
            "SELECT (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.play_user_id), '[]'::jsonb) FROM imported_accounts a), (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.play_snake_id), '[]'::jsonb) FROM imported_snakes s), (SELECT coalesce(jsonb_agg(to_jsonb(g) ORDER BY g.imported_grant_id), '[]'::jsonb) FROM imported_grants g)"
        ).fetch_one(&pool).await?;
        assert_eq!(after, same);
        let archived: i64 =
            sqlx::query_scalar("SELECT count(*) FROM imported_snakes WHERE play_snake_id='snk_1'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(archived, 0);
        let removed: i64 = sqlx::query_scalar("SELECT count(*) FROM imported_grants WHERE slug='alligator' AND customization_type='head'").fetch_one(&pool).await?;
        assert_eq!(removed, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn importer_counts_claimed_skips_and_keeps_imported_snakes(
        pool: PgPool,
    ) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;
        import_from_play(&pool, &pool).await?;
        let account_id: uuid::Uuid = sqlx::query_scalar(
            "SELECT imported_account_id FROM imported_accounts WHERE play_user_id='usr_linked'",
        )
        .fetch_one(&pool)
        .await?;
        let user_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (999888, 'test-gh', 'test-token') RETURNING user_id"
        ).fetch_one(&pool).await?;
        imported_account::claim_account(&pool, account_id, user_id).await?;
        let before: serde_json::Value = sqlx::query_scalar(
            "SELECT jsonb_agg(to_jsonb(s) ORDER BY s.play_snake_id) FROM imported_snakes s WHERE imported_account_id=$1"
        ).bind(account_id).fetch_one(&pool).await?;
        sqlx::raw_sql("UPDATE core_account SET display_name='Changed' WHERE id='act_linked'; UPDATE core_snake SET name='Changed' WHERE id='snk_1';")
            .execute(&pool).await?;
        let counts = import_from_play(&pool, &pool).await?;
        assert_eq!(counts.accounts, 4);
        assert_eq!(counts.skipped_claimed, 1);
        assert_eq!(counts.snakes, 1);
        assert_eq!(counts.snakes_skipped_claimed, 1);
        assert_eq!(counts.grants, 2);
        let after: serde_json::Value = sqlx::query_scalar(
            "SELECT jsonb_agg(to_jsonb(s) ORDER BY s.play_snake_id) FROM imported_snakes s WHERE imported_account_id=$1"
        ).bind(account_id).fetch_one(&pool).await?;
        assert_eq!(before, after);
        Ok(())
    }
}
