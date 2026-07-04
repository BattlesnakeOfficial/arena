use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

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
}

/// Upsert a play account into staging. Re-runnable: refreshes play-side
/// data on conflict but never touches claim state.
pub async fn stage_account(pool: &PgPool, account: &StageAccount) -> cja::Result<Uuid> {
    // A GitHub link can move between play users across imports (user A
    // unlinks, user B links the same account — legal, since social_django's
    // (provider, uid) uniqueness holds at every instant). On re-import, A's
    // stale staging row still holds the uid, so B's upsert would trip the
    // `github_uid` unique index and abort the whole run. Release the uid
    // from any other staging row first; A's own row gets refreshed (to NULL
    // or its new uid) when A is processed later in this same import.
    if let Some(github_uid) = account.github_uid {
        sqlx::query!(
            r#"
            UPDATE imported_accounts
            SET github_uid = NULL
            WHERE github_uid = $1 AND play_user_id <> $2
            "#,
            github_uid,
            account.play_user_id,
        )
        .execute(pool)
        .await
        .wrap_err("Failed to release GitHub uid from stale staging row")?;
    }

    let row = sqlx::query!(
        r#"
        INSERT INTO imported_accounts (
            play_user_id, play_account_id, email, password_hash,
            is_email_verified, username, display_name, pronouns, country,
            backstory, github_uid, github_login, points, points_high_score,
            is_staff, play_created_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (play_user_id) DO UPDATE SET
            email = $3,
            password_hash = $4,
            is_email_verified = $5,
            username = $6,
            display_name = $7,
            pronouns = $8,
            country = $9,
            backstory = $10,
            github_uid = $11,
            github_login = $12,
            points = $13,
            points_high_score = $14,
            is_staff = $15,
            play_created_at = $16
        RETURNING imported_account_id
        "#,
        account.play_user_id,
        account.play_account_id,
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
        account.play_created_at,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to stage imported account")?;

    Ok(row.imported_account_id)
}

/// Upsert a play snake into staging, resolving its owner by play account
/// ID. Returns false (and stages nothing) if the owner isn't staged.
pub async fn stage_snake(pool: &PgPool, snake: &StageSnake) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"
        INSERT INTO imported_snakes (
            imported_account_id, play_snake_id, name, url, head, tail, color, is_public
        )
        SELECT ia.imported_account_id, $2, $3, $4, $5, $6, $7, $8
        FROM imported_accounts ia
        WHERE ia.play_account_id = $1
        ON CONFLICT (play_snake_id) DO UPDATE SET
            name = $3,
            url = $4,
            head = $5,
            tail = $6,
            color = $7,
            is_public = $8
        "#,
        snake.play_account_id,
        snake.play_snake_id,
        snake.name,
        snake.url,
        snake.head,
        snake.tail,
        snake.color,
        snake.is_public,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to stage imported snake")?;

    Ok(result.rows_affected() > 0)
}

/// Upsert a play customization grant into staging, keyed by (type, slug).
/// Returns false only when the owner isn't staged (a real orphan). On a
/// re-import of an already-staged grant, the no-op `DO UPDATE` keeps
/// `rows_affected` at 1 so it counts as staged, not orphaned — mirroring
/// `stage_snake`. (Plain `DO NOTHING` reports 0 affected on conflict, which
/// would misreport every grant as orphaned on the second run.)
pub async fn stage_grant(
    pool: &PgPool,
    play_account_id: &str,
    customization_type: &str,
    slug: &str,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"
        INSERT INTO imported_grants (imported_account_id, customization_type, slug)
        SELECT ia.imported_account_id, $2, $3
        FROM imported_accounts ia
        WHERE ia.play_account_id = $1
        ON CONFLICT (imported_account_id, customization_type, slug)
            DO UPDATE SET imported_account_id = EXCLUDED.imported_account_id
        "#,
        play_account_id,
        customization_type,
        slug,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to stage imported grant")?;

    Ok(result.rows_affected() > 0)
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
    let mut tx = pool.begin().await.wrap_err("Failed to begin claim tx")?;

    let claimed = sqlx::query!(
        r#"
        UPDATE imported_accounts
        SET claimed_by_user_id = $2, claimed_at = NOW()
        WHERE imported_account_id = $1 AND claimed_by_user_id IS NULL
        RETURNING username, display_name, email
        "#,
        imported_account_id,
        user_id,
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
        SET display_name = COALESCE(display_name, NULLIF($2, ''), $3)
        WHERE user_id = $1
        "#,
        user_id,
        claimed.display_name,
        claimed.username,
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to set display name")?;

    // Materialize snakes. Names must be unique per user in arena; play had
    // no such constraint, so collisions get a numeric suffix. Existing
    // names are fetched up front to avoid unique-violation aborts mid-tx.
    let mut existing_names: Vec<String> =
        sqlx::query!("SELECT name FROM battlesnakes WHERE user_id = $1", user_id)
            .fetch_all(&mut *tx)
            .await
            .wrap_err("Failed to fetch existing snake names")?
            .into_iter()
            .map(|r| r.name)
            .collect();

    let staged_snakes = sqlx::query!(
        r#"
        SELECT imported_snake_id, name, url, head, tail, color, is_public
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
            INSERT INTO battlesnakes (user_id, name, url, visibility, color, head, tail)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING battlesnake_id
            "#,
            user_id,
            name,
            snake.url,
            visibility,
            snake.color,
            snake.head,
            snake.tail,
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

    claim_account(pool, account.imported_account_id, user_id).await
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
        let id = stage_account(pool, &play_account(n)).await?;
        stage_snake(
            pool,
            &StageSnake {
                play_snake_id: format!("snk_{n}"),
                play_account_id: format!("act_{n}"),
                name: "Hissy".to_string(),
                url: "https://example.com/snake".to_string(),
                head: "alligator".to_string(),
                tail: "default".to_string(),
                color: "#ff0000".to_string(),
                is_public: true,
            },
        )
        .await?;
        stage_grant(pool, &format!("act_{n}"), "head", "alligator").await?;
        stage_grant(pool, &format!("act_{n}"), "tail", "no-longer-exists").await?;
        Ok(id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_materializes_snakes_grants_and_display_name(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 1001).await?;
        let account_id = stage_full_account(&pool, 1).await?;

        let summary = claim_account(&pool, account_id, user_id)
            .await?
            .expect("claim should succeed");
        assert_eq!(summary.snakes_created, 1);
        // The retired slug is dropped; only the alligator head lands.
        assert_eq!(summary.grants_created, 1);
        assert_eq!(summary.username, "player1");

        let snake = sqlx::query!(
            "SELECT name, url, head, tail, color, visibility FROM battlesnakes WHERE user_id = $1",
            user_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(snake.name, "Hissy");
        assert_eq!(snake.head, "alligator");
        assert_eq!(snake.visibility, "public");

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
    async fn auto_claim_matches_by_github_uid(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 5001).await?;

        let mut linked = play_account(5);
        linked.github_uid = Some(5001);
        linked.github_login = Some("gh-user-5001".to_string());
        stage_account(&pool, &linked).await?;

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
        let same_id = stage_account(&pool, &updated).await?;
        assert_eq!(same_id, account_id);

        let account = sqlx::query!(
            "SELECT display_name, claimed_by_user_id FROM imported_accounts
             WHERE imported_account_id = $1",
            account_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(account.display_name, "Renamed");
        assert_eq!(account.claimed_by_user_id, Some(user_id));

        // Orphaned staging rows report false.
        assert!(!stage_grant(&pool, "act_missing", "head", "alligator").await?);

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
        stage_account(&pool, &a).await?;

        // Between imports, in play, A unlinks and B links the same GitHub
        // account. Import 2 processes B first (stale A row still holds 100).
        let mut b = play_account(2);
        b.github_uid = Some(100);
        stage_account(&pool, &b).await?; // must NOT abort on the uid unique index

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
        stage_account(&pool, &a_now).await?;
        assert_eq!(
            find_unclaimed_by_github_uid(&pool, 100)
                .await?
                .unwrap()
                .play_user_id,
            "usr_2"
        );

        Ok(())
    }
}
