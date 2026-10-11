//! Snake customizations: a code-defined catalog (see [`catalog`]) plus
//! per-user ownership grants in the database.
//!
//! The catalog is static data that changes a few times a year, so it lives
//! in the type system — exhaustive matches, compile-time slug uniqueness,
//! no seed migrations. Grants are runtime state and stay in Postgres.

pub mod achievements;
pub mod catalog;

use color_eyre::eyre::Context as _;
use sqlx::{PgConnection, PgPool};
use std::collections::HashSet;
use uuid::Uuid;

pub use catalog::{Availability, CustomizationDef, Group, Head, Tail};

/// The slug snakes fall back to when they declare a customization they
/// can't use (unknown or not granted). Play's allow/deny rules are the
/// same, but play kept the snake's previously-stored value on a denied
/// declare; arena re-resolves from the declaration every game.
pub const DEFAULT_SLUG: &str = "default";

impl CustomizationDef {
    /// Usable by anyone without a grant.
    pub fn is_free(&self) -> bool {
        match self.group.availability() {
            Availability::Everyone => true,
            Availability::Restricted => !self.requires_grant,
            Availability::Hidden | Availability::Preview => false,
        }
    }

    pub fn is_token_unlockable(&self) -> bool {
        self.group.availability() == Availability::Restricted && self.requires_grant
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockOutcome {
    Unlocked(&'static str),
    UnknownItem,
    NotTokenUnlockable,
    AlreadyOwned,
    InsufficientTokens,
}

/// Record each participant's owner once for the game's UTC Monday. Call inside
/// the transaction that marks the game finished.
pub async fn record_active_week_for_game(
    conn: &mut PgConnection,
    game_id: Uuid,
    finished_at: chrono::DateTime<chrono::Utc>,
) -> cja::Result<u64> {
    let result = sqlx::query!(
        r#"
        INSERT INTO customization_active_weeks (user_id, week_start)
        SELECT DISTINCT bs.user_id,
            date_trunc('week', $2::timestamptz AT TIME ZONE 'UTC')::date
        FROM game_battlesnakes gb
        LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
        JOIN battlesnakes bs ON bs.battlesnake_id = COALESCE(gb.battlesnake_id, le.battlesnake_id)
        WHERE gb.game_id = $1
        ORDER BY 1, 2
        ON CONFLICT (user_id, week_start) DO NOTHING
        "#,
        game_id,
        finished_at,
    )
    .execute(conn)
    .await
    .wrap_err("Failed to record customization active week")?;
    Ok(result.rows_affected())
}

pub async fn token_balance(pool: &PgPool, user_id: Uuid) -> cja::Result<i64> {
    let row = sqlx::query!(
        r#"
        SELECT
            (SELECT COUNT(*)::bigint FROM customization_active_weeks WHERE user_id = $1)
          - (SELECT COUNT(*)::bigint FROM customization_grants WHERE user_id = $1 AND source = 'token')
          AS "balance!"
        "#,
        user_id,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to read customization token balance")?;
    Ok(row.balance)
}

pub async fn unlock_with_token(
    pool: &PgPool,
    user_id: Uuid,
    kind: &str,
    slug: &str,
) -> cja::Result<UnlockOutcome> {
    let def = match kind {
        Head::KIND => Head::from_slug(slug).map(Head::def),
        Tail::KIND => Tail::from_slug(slug).map(Tail::def),
        _ => None,
    };
    let Some(def) = def else {
        return Ok(UnlockOutcome::UnknownItem);
    };
    if !def.is_token_unlockable() {
        return Ok(UnlockOutcome::NotTokenUnlockable);
    }

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin customization unlock")?;
    let locked = sqlx::query!(
        "SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE",
        user_id,
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to lock customization owner")?;
    if locked.is_none() {
        return Ok(UnlockOutcome::UnknownItem);
    }
    let existing = sqlx::query!(
        "SELECT customization_grant_id FROM customization_grants WHERE user_id = $1 AND customization_type = $2 AND slug = $3",
        user_id,
        kind,
        slug,
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to check customization grant")?;
    if existing.is_some() {
        return Ok(UnlockOutcome::AlreadyOwned);
    }
    let balance = sqlx::query!(
        r#"SELECT
            (SELECT COUNT(*)::bigint FROM customization_active_weeks WHERE user_id = $1)
          - (SELECT COUNT(*)::bigint FROM customization_grants WHERE user_id = $1 AND source = 'token')
          AS "balance!""#,
        user_id,
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to check customization token balance")?
    .balance;
    if balance < 1 {
        return Ok(UnlockOutcome::InsufficientTokens);
    }
    let inserted = sqlx::query!(
        r#"INSERT INTO customization_grants (user_id, customization_type, slug, source)
           VALUES ($1, $2, $3, 'token')
           ON CONFLICT (user_id, customization_type, slug) DO NOTHING"#,
        user_id,
        kind,
        slug,
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to insert token customization grant")?
    .rows_affected();
    if inserted == 0 {
        return Ok(UnlockOutcome::AlreadyOwned);
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit customization unlock")?;
    Ok(UnlockOutcome::Unlocked(def.display_name))
}

/// Scan historical links in bounded timestamp windows. Credits and cursor
/// advance share one transaction, so a failed batch is retried intact.
pub async fn backfill_active_weeks(pool: &PgPool) -> cja::Result<u64> {
    let mut total = 0;
    loop {
        let mut tx = pool
            .begin()
            .await
            .wrap_err("Failed to begin active-week backfill")?;
        sqlx::query!("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *tx)
            .await
            .wrap_err("Failed to set active-week backfill lock timeout")?;
        sqlx::query!("SET LOCAL statement_timeout = '60s'")
            .execute(&mut *tx)
            .await
            .wrap_err("Failed to set active-week backfill statement timeout")?;
        let cursor = sqlx::query!(
            "SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE FOR UPDATE"
        )
        .fetch_one(&mut *tx).await
        .wrap_err("Failed to lock active-week backfill cursor")?
        .scanned_through;
        let run_now = chrono::Utc::now();
        let advancing = cursor < run_now;
        let batch_end = if advancing {
            (cursor + chrono::Duration::days(7)).min(run_now)
        } else {
            run_now
        };
        let window_start = if advancing { cursor } else { run_now } - chrono::Duration::days(1);
        let inserted = sqlx::query!(
            r#"
            WITH window_links AS MATERIALIZED (
                SELECT gb.game_id, COALESCE(gb.battlesnake_id, le.battlesnake_id) AS battlesnake_id
                FROM game_battlesnakes gb
                LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
                WHERE gb.created_at >= $1 AND gb.created_at < $2
            ), window_games AS MATERIALIZED (
                SELECT DISTINCT game_id FROM window_links
            ), legacy_finishes AS MATERIALIZED (
                SELECT g.game_id,
                    COALESCE((SELECT t.created_at FROM turns t
                              WHERE t.game_id = g.game_id
                              ORDER BY t.turn_number DESC LIMIT 1), g.updated_at) AS finished_at
                FROM window_games wg
                JOIN games g ON g.game_id = wg.game_id
                WHERE g.status = 'finished' AND g.finished_at IS NULL
            )
            INSERT INTO customization_active_weeks (user_id, week_start)
            SELECT DISTINCT bs.user_id,
                date_trunc('week', lf.finished_at AT TIME ZONE 'UTC')::date
            FROM window_links wl
            JOIN legacy_finishes lf ON lf.game_id = wl.game_id
            JOIN battlesnakes bs ON bs.battlesnake_id = wl.battlesnake_id
            ORDER BY 1, 2
            ON CONFLICT (user_id, week_start) DO NOTHING
            "#,
            window_start,
            batch_end,
        )
        .execute(&mut *tx).await
        .wrap_err("Failed to insert historical active weeks")?
        .rows_affected();
        if advancing {
            sqlx::query!(
                "UPDATE customization_active_week_backfill_cursor SET scanned_through = $1 WHERE singleton = TRUE",
                batch_end,
            ).execute(&mut *tx).await
            .wrap_err("Failed to advance active-week backfill cursor")?;
        }
        tx.commit()
            .await
            .wrap_err("Failed to commit active-week backfill")?;
        total += inserted;
        if !advancing || batch_end == run_now {
            return Ok(total);
        }
    }
}

impl Head {
    pub fn image_url(self) -> String {
        self.def().image_override.map_or_else(
            || {
                format!(
                    "https://media.battlesnake.com/snakes/heads/{}.svg",
                    self.slug()
                )
            },
            String::from,
        )
    }
}

impl Tail {
    pub fn image_url(self) -> String {
        self.def().image_override.map_or_else(
            || {
                format!(
                    "https://media.battlesnake.com/snakes/tails/{}.svg",
                    self.slug()
                )
            },
            String::from,
        )
    }
}

/// Grant a customization to a user as an admin. The play importer writes its
/// own provenance. The (kind, slug) pair is validated against
/// the catalog by the callers that accept external input.
pub async fn create_grant(
    pool: &PgPool,
    user_id: Uuid,
    customization_type: &str,
    slug: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO customization_grants (user_id, customization_type, slug, source)
        VALUES ($1, $2, $3, 'admin')
        ON CONFLICT (user_id, customization_type, slug) DO NOTHING
        "#,
        user_id,
        customization_type,
        slug,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to create customization grant")?;

    Ok(())
}

/// Every (type, slug) grant the user holds.
pub async fn get_granted_slugs(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<HashSet<(String, String)>> {
    let rows = sqlx::query!(
        "SELECT customization_type, slug FROM customization_grants WHERE user_id = $1",
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch customization grants")?;

    Ok(rows
        .into_iter()
        .map(|r| (r.customization_type, r.slug))
        .collect())
}

async fn has_grant(
    pool: &PgPool,
    user_id: Uuid,
    customization_type: &str,
    slug: &str,
) -> cja::Result<bool> {
    let row = sqlx::query!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM customization_grants
            WHERE user_id = $1 AND customization_type = $2 AND slug = $3
        ) as "exists!"
        "#,
        user_id,
        customization_type,
        slug,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check customization grant")?;

    Ok(row.exists)
}

/// Shared resolution logic once the declared slug has been looked up in
/// the catalog.
async fn resolve(
    pool: &PgPool,
    user_id: Uuid,
    customization_type: &str,
    declared_slug: &str,
    def: Option<CustomizationDef>,
) -> cja::Result<String> {
    if declared_slug.is_empty() || declared_slug == DEFAULT_SLUG {
        return Ok(declared_slug.to_string());
    }

    let allowed = match def {
        None => false,
        Some(def) if def.is_free() => true,
        Some(_) => has_grant(pool, user_id, customization_type, declared_slug).await?,
    };

    Ok(if allowed {
        declared_slug.to_string()
    } else {
        DEFAULT_SLUG.to_string()
    })
}

/// Resolve a declared head slug to what the snake may actually wear:
/// unknown or ungranted slugs become [`DEFAULT_SLUG`], empty stays empty
/// (the board renders its own default for empty).
pub async fn resolve_head(pool: &PgPool, user_id: Uuid, declared: &str) -> cja::Result<String> {
    let def = Head::from_slug(declared).map(Head::def);
    resolve(pool, user_id, Head::KIND, declared, def).await
}

/// Tail counterpart of [`resolve_head`].
pub async fn resolve_tail(pool: &PgPool, user_id: Uuid, declared: &str) -> cja::Result<String> {
    let def = Tail::from_slug(declared).map(Tail::def);
    resolve(pool, user_id, Tail::KIND, declared, def).await
}

/// Normalize a declared color to a valid `#rrggbb` hex string, or empty if
/// invalid (the board generates a stable per-snake fallback color for empty).
pub fn normalize_color(declared: &str) -> String {
    let Some(hex) = declared.strip_prefix('#') else {
        return String::new();
    };
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        declared.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// Sanitize a stored snake color for direct interpolation into an inline
/// `style` attribute. Imported play-era rows can hold arbitrary strings (and
/// the column defaults to ''), so anything that is not strict `#rrggbb` falls
/// back to a neutral gray rather than injecting CSS onto public pages.
pub fn chip_color(declared: &str) -> String {
    let normalized = normalize_color(declared);
    if normalized.is_empty() {
        "#888888".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_user(pool: &PgPool) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (42424242, 'customization-tester', 'test-token')
             RETURNING user_id"
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    #[test]
    fn catalog_matches_play_extraction() {
        assert_eq!(Head::ALL.len(), 102);
        assert_eq!(Tail::ALL.len(), 84);

        // Round-trip: every variant's slug parses back to itself.
        for head in Head::ALL {
            assert_eq!(Head::from_slug(head.slug()), Some(*head));
        }
        for tail in Tail::ALL {
            assert_eq!(Tail::from_slug(tail.slug()), Some(*tail));
        }
        assert_eq!(Head::from_slug("not-a-real-head"), None);

        // Spot-checks against play's data.
        assert!(Head::Default.def().is_free());
        assert_eq!(Head::Beluga.def().group, Group::Standard);
        assert!(Head::Beluga.def().is_free());
        assert!(Head::Alligator.def().requires_grant);
        assert!(!Head::Alligator.def().is_free());
        assert_eq!(
            Head::Beluga.image_url(),
            "https://media.battlesnake.com/snakes/heads/beluga.svg"
        );
    }

    #[test]
    fn catalog_access_is_exact() {
        let free_heads: HashSet<_> = "default beluga bendr dead evil fang pixel safe sand-worm shades silly smile tongue do-sammy rbc-bowler replit-mark mlh-gene nr-rocket all-seeing smart-caterpillar trans-rights-scarf caffeine gamer tiger-king workout bonhomme earmuffs rudolph scarf ski snowman snow-worm".split_whitespace().collect();
        let free_tails: HashSet<_> = "default block-bum bolt curled fat-rattle freckled hook pixel round-bum sharp skinny small-rattle do-sammy rbc-necktie replit-notmark mlh-gene nr-booster mystic-moon coffee mouse tiger-tail weight bonhomme flake ice-skate present".split_whitespace().collect();
        assert_eq!(free_heads.len() + free_tails.len(), 58);
        assert_eq!(
            Head::ALL
                .iter()
                .filter(|item| item.def().is_free())
                .map(|item| item.slug())
                .collect::<HashSet<_>>(),
            free_heads
        );
        assert_eq!(
            Tail::ALL
                .iter()
                .filter(|item| item.def().is_free())
                .map(|item| item.slug())
                .collect::<HashSet<_>>(),
            free_tails
        );
        assert_eq!(
            Head::ALL
                .iter()
                .filter(|item| item.def().is_token_unlockable())
                .count(),
            59
        );
        assert_eq!(
            Tail::ALL
                .iter()
                .filter(|item| item.def().is_token_unlockable())
                .count(),
            49
        );
        let special: Vec<_> = Head::ALL
            .iter()
            .map(|item| item.def())
            .chain(Tail::ALL.iter().map(|item| item.def()))
            .filter(|def| def.group == Group::SpecialEdition)
            .collect();
        assert_eq!(special.len(), 11);
        assert!(
            special
                .iter()
                .all(|def| def.group.availability() == Availability::Hidden)
        );
        let achievements: Vec<_> = Head::ALL
            .iter()
            .map(|item| item.def())
            .chain(Tail::ALL.iter().map(|item| item.def()))
            .filter(|def| def.group == Group::Collection2024)
            .collect();
        assert_eq!(achievements.len(), 9);
        assert!(
            achievements
                .iter()
                .all(|def| def.group.availability() == Availability::Preview
                    && def.requires_grant
                    && !def.is_free()
                    && !def.is_token_unlockable())
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn week_credit_spend_and_permanence(pool: PgPool) -> cja::Result<()> {
        use chrono::{TimeZone as _, Utc};
        let owner = create_test_user(&pool).await?;
        let other = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (42424243, 'other-customization-tester', 'token') RETURNING user_id"
        ).fetch_one(&pool).await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Week Snake', 'https://example.com') RETURNING battlesnake_id", owner
        ).fetch_one(&pool).await?;
        let snake2 = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Second Snake', 'https://example.com') RETURNING battlesnake_id", owner
        ).fetch_one(&pool).await?;
        let other_snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Other Snake', 'https://example.com') RETURNING battlesnake_id", other
        ).fetch_one(&pool).await?;
        let game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        ).fetch_one(&pool).await?;
        for id in [snake, snake2, other_snake] {
            sqlx::query!(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)",
                game,
                id
            )
            .execute(&pool)
            .await?;
        }
        let sunday = Utc
            .with_ymd_and_hms(2026, 10, 11, 23, 59, 59)
            .single()
            .unwrap();
        let monday = Utc
            .with_ymd_and_hms(2026, 10, 12, 0, 0, 0)
            .single()
            .unwrap();
        let mut conn = pool.acquire().await?;
        assert_eq!(
            record_active_week_for_game(&mut conn, game, sunday).await?,
            2
        );
        assert_eq!(
            record_active_week_for_game(&mut conn, game, sunday).await?,
            0
        );
        assert_eq!(
            record_active_week_for_game(&mut conn, game, monday).await?,
            2
        );
        assert_eq!(token_balance(&pool, owner).await?, 2);
        assert_eq!(token_balance(&pool, other).await?, 2);
        assert_eq!(
            unlock_with_token(&pool, owner, "head", "alligator").await?,
            UnlockOutcome::Unlocked(Head::Alligator.def().display_name)
        );
        assert_eq!(
            unlock_with_token(&pool, owner, "head", "alligator").await?,
            UnlockOutcome::AlreadyOwned
        );
        assert_eq!(token_balance(&pool, owner).await?, 1);
        assert_eq!(
            unlock_with_token(&pool, owner, "head", "default").await?,
            UnlockOutcome::NotTokenUnlockable
        );
        assert_eq!(
            unlock_with_token(&pool, owner, "head", "fish").await?,
            UnlockOutcome::NotTokenUnlockable
        );
        assert_eq!(
            unlock_with_token(&pool, owner, "head", "not-real").await?,
            UnlockOutcome::UnknownItem
        );
        assert_eq!(
            unlock_with_token(&pool, owner, "invalid", "alligator").await?,
            UnlockOutcome::UnknownItem
        );
        crate::models::game::delete_game(&pool, game).await?;
        assert_eq!(token_balance(&pool, owner).await?, 1);
        crate::models::battlesnake::delete_battlesnake(&pool, snake, owner).await?;
        sqlx::query!(
            "DELETE FROM battlesnakes WHERE battlesnake_id = $1",
            other_snake
        )
        .execute(&pool)
        .await?;
        assert_eq!(token_balance(&pool, owner).await?, 1);
        assert_eq!(token_balance(&pool, other).await?, 2);
        let empty_game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        ).fetch_one(&pool).await?;
        assert_eq!(
            record_active_week_for_game(&mut conn, empty_game, monday).await?,
            0
        );
        assert_eq!(token_balance(&pool, owner).await?, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_is_idempotent_and_skips_new_finishes(pool: PgPool) -> cja::Result<()> {
        use chrono::{TimeZone as _, Utc};
        let user = create_test_user(&pool).await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Backfill Snake', 'https://example.com') RETURNING battlesnake_id", user
        ).fetch_one(&pool).await?;
        let legacy = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        ).fetch_one(&pool).await?;
        let sunday = Utc
            .with_ymd_and_hms(2026, 10, 4, 23, 59, 59)
            .single()
            .unwrap();
        let monday = Utc.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).single().unwrap();
        let earliest = Utc.with_ymd_and_hms(2026, 9, 21, 0, 0, 0).single().unwrap();
        let zero_turn_finish = Utc
            .with_ymd_and_hms(2026, 9, 22, 12, 0, 0)
            .single()
            .unwrap();
        sqlx::query!("UPDATE customization_active_week_backfill_cursor SET scanned_through = $1 WHERE singleton = TRUE", earliest).execute(&pool).await?;
        let zero_turn_game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status, updated_at) VALUES ('11x11', 'Standard', 'finished', $1) RETURNING game_id",
            zero_turn_finish,
        ).fetch_one(&pool).await?;
        sqlx::query!("INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)", zero_turn_game, snake, zero_turn_finish).execute(&pool).await?;
        sqlx::query!(
            "INSERT INTO games (board_size, game_type, status, created_at) VALUES ('11x11', 'Standard', 'finished', '1970-01-01')"
        ).execute(&pool).await?;
        sqlx::query!("INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)", legacy, snake, sunday).execute(&pool).await?;
        sqlx::query!(
            "INSERT INTO turns (game_id, turn_number, created_at) VALUES ($1, 2, $2)",
            legacy,
            sunday
        )
        .execute(&pool)
        .await?;
        sqlx::query!(
            "INSERT INTO turns (game_id, turn_number, created_at) VALUES ($1, 1, $2)",
            legacy,
            monday
        )
        .execute(&pool)
        .await?;
        let new_game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status, finished_at) VALUES ('11x11', 'Standard', 'finished', $1) RETURNING game_id", monday
        ).fetch_one(&pool).await?;
        sqlx::query!("INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)", new_game, snake, sunday).execute(&pool).await?;
        sqlx::query!(
            "INSERT INTO turns (game_id, turn_number, created_at) VALUES ($1, 1, $2)",
            new_game,
            sunday
        )
        .execute(&pool)
        .await?;
        let mut conn = pool.acquire().await?;
        assert_eq!(
            record_active_week_for_game(&mut conn, new_game, monday).await?,
            1
        );
        assert_eq!(backfill_active_weeks(&pool).await?, 2);
        assert_eq!(backfill_active_weeks(&pool).await?, 0);
        let weeks = sqlx::query_scalar!("SELECT week_start FROM customization_active_weeks WHERE user_id = $1 ORDER BY week_start", user).fetch_all(&pool).await?;
        assert_eq!(weeks.len(), 3);
        assert_eq!(token_balance(&pool, user).await?, 3);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_catches_late_finish_in_lookback(pool: PgPool) -> cja::Result<()> {
        let user = create_test_user(&pool).await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Late Snake', 'https://example.com') RETURNING battlesnake_id", user
        ).fetch_one(&pool).await?;
        let game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'running') RETURNING game_id"
        ).fetch_one(&pool).await?;
        let linked_at = chrono::Utc::now() - chrono::Duration::hours(2);
        sqlx::query!(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)",
            game, snake, linked_at
        ).execute(&pool).await?;
        sqlx::query!(
            "UPDATE customization_active_week_backfill_cursor SET scanned_through = $1 WHERE singleton = TRUE",
            linked_at - chrono::Duration::hours(1)
        ).execute(&pool).await?;
        assert_eq!(backfill_active_weeks(&pool).await?, 0);
        sqlx::query!(
            "UPDATE games SET status = 'finished' WHERE game_id = $1",
            game
        )
        .execute(&pool)
        .await?;
        let finished_at = chrono::Utc::now();
        sqlx::query!(
            "INSERT INTO turns (game_id, turn_number, created_at) VALUES ($1, 1, $2)",
            game,
            finished_at
        )
        .execute(&pool)
        .await?;
        assert_eq!(backfill_active_weeks(&pool).await?, 1);
        let week = sqlx::query_scalar!(
            "SELECT week_start FROM customization_active_weeks WHERE user_id = $1",
            user
        )
        .fetch_one(&pool)
        .await?;
        use chrono::Datelike as _;
        assert_eq!(
            week,
            finished_at.date_naive()
                - chrono::Duration::days(i64::from(finished_at.weekday().num_days_from_monday()))
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failed_backfill_batch_rolls_back_credit_and_cursor(pool: PgPool) -> cja::Result<()> {
        let a = create_test_user(&pool).await?;
        let b = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (42424243, 'poison-tester', 'token') RETURNING user_id"
        ).fetch_one(&pool).await?;
        let (normal, poisoned) = if a < b { (a, b) } else { (b, a) };
        let start = chrono::Utc::now() - chrono::Duration::hours(4);
        sqlx::query!(
            "UPDATE customization_active_week_backfill_cursor SET scanned_through = $1 WHERE singleton = TRUE", start
        ).execute(&pool).await?;
        let start = sqlx::query_scalar!(
            "SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE"
        ).fetch_one(&pool).await?;
        for (index, user) in [normal, poisoned].into_iter().enumerate() {
            let snake = sqlx::query_scalar!(
                "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
                user, format!("Batch Snake {index}")
            ).fetch_one(&pool).await?;
            let game = sqlx::query_scalar!(
                "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
            ).fetch_one(&pool).await?;
            sqlx::query!(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)",
                game, snake, start + chrono::Duration::hours(1)
            ).execute(&pool).await?;
        }
        sqlx::query(&format!(
            "CREATE FUNCTION reject_poisoned_week() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.user_id = '{poisoned}'::uuid THEN RAISE EXCEPTION 'poisoned week'; END IF; RETURN NEW; END $$"
        )).execute(&pool).await?;
        sqlx::query("CREATE TRIGGER reject_poisoned_week BEFORE INSERT ON customization_active_weeks FOR EACH ROW EXECUTE FUNCTION reject_poisoned_week()")
            .execute(&pool).await?;
        assert!(backfill_active_weeks(&pool).await.is_err());
        let cursor = sqlx::query_scalar!(
            "SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE"
        ).fetch_one(&pool).await?;
        assert_eq!(cursor, start);
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*)::bigint AS \"count!\" FROM customization_active_weeks"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER reject_poisoned_week ON customization_active_weeks")
            .execute(&pool)
            .await?;
        sqlx::query("DROP FUNCTION reject_poisoned_week()")
            .execute(&pool)
            .await?;
        assert_eq!(backfill_active_weeks(&pool).await?, 2);
        let cursor = sqlx::query_scalar!(
            "SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE"
        ).fetch_one(&pool).await?;
        assert!(cursor > start);
        assert_eq!(token_balance(&pool, normal).await?, 1);
        assert_eq!(token_balance(&pool, poisoned).await?, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn overlapping_backfills_wait_for_cursor_lock(pool: PgPool) -> cja::Result<()> {
        let user = create_test_user(&pool).await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Cursor Snake', 'https://example.com') RETURNING battlesnake_id", user
        ).fetch_one(&pool).await?;
        let game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        ).fetch_one(&pool).await?;
        let start = chrono::Utc::now() - chrono::Duration::hours(4);
        sqlx::query!("UPDATE customization_active_week_backfill_cursor SET scanned_through = $1 WHERE singleton = TRUE", start).execute(&pool).await?;
        sqlx::query!("INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)", game, snake, start + chrono::Duration::hours(1)).execute(&pool).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query!("SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE FOR UPDATE")
            .fetch_one(&mut *blocker).await?;
        let task = {
            let pool = pool.clone();
            tokio::spawn(async move { backfill_active_weeks(&pool).await })
        };
        let wait = async {
            loop {
                let waiting = sqlx::query_scalar!(
                    "SELECT COUNT(*)::bigint AS \"count!\" FROM pg_stat_activity WHERE query LIKE 'SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE FOR UPDATE%' AND wait_event_type = 'Lock' AND datname = current_database()"
                ).fetch_one(&pool).await?;
                if waiting == 1 {
                    break Ok::<(), cja::color_eyre::Report>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        let barrier = tokio::time::timeout(std::time::Duration::from_secs(4), wait).await;
        blocker.commit().await?;
        barrier??;
        assert_eq!(task.await??, 1);
        assert_eq!(token_balance(&pool, user).await?, 1);
        let cursor = sqlx::query_scalar!("SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE")
            .fetch_one(&pool).await?;
        assert!(cursor >= start);
        assert!((chrono::Utc::now() - cursor).num_seconds() < 5);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn seven_day_boundary_is_in_next_batch_and_catches_up(pool: PgPool) -> cja::Result<()> {
        let user = create_test_user(&pool).await?;
        let snake = sqlx::query_scalar!("INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Boundary Snake', 'https://example.com') RETURNING battlesnake_id", user)
            .fetch_one(&pool).await?;
        let game = sqlx::query_scalar!("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id")
            .fetch_one(&pool).await?;
        let start = chrono::Utc::now() - chrono::Duration::days(15);
        let edge = start + chrono::Duration::days(7);
        sqlx::query!("UPDATE customization_active_week_backfill_cursor SET scanned_through = $1 WHERE singleton = TRUE", start)
            .execute(&pool).await?;
        sqlx::query!("INSERT INTO game_battlesnakes (game_id, battlesnake_id, created_at) VALUES ($1, $2, $3)", game, snake, edge)
            .execute(&pool).await?;
        assert_eq!(backfill_active_weeks(&pool).await?, 1);
        assert_eq!(token_balance(&pool, user).await?, 1);
        let caught_up = sqlx::query_scalar!("SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE")
            .fetch_one(&pool).await?;
        assert!((chrono::Utc::now() - caught_up).num_seconds() < 5);
        assert_eq!(backfill_active_weeks(&pool).await?, 0);
        let again = sqlx::query_scalar!("SELECT scanned_through FROM customization_active_week_backfill_cursor WHERE singleton = TRUE")
            .fetch_one(&pool).await?;
        assert!(again >= caught_up);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_spends_serialize_on_user_lock(pool: PgPool) -> cja::Result<()> {
        let user = create_test_user(&pool).await?;
        sqlx::query!("INSERT INTO customization_active_weeks (user_id, week_start) VALUES ($1, '2026-10-05')", user).execute(&pool).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query!(
            "SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE",
            user
        )
        .fetch_one(&mut *blocker)
        .await?;
        let a = {
            let pool = pool.clone();
            tokio::spawn(async move { unlock_with_token(&pool, user, "head", "alligator").await })
        };
        let b = {
            let pool = pool.clone();
            tokio::spawn(async move { unlock_with_token(&pool, user, "tail", "alligator").await })
        };
        let wait = async {
            loop {
                let waiting = sqlx::query_scalar!(
                    "SELECT COUNT(*)::bigint AS \"count!\" FROM pg_stat_activity WHERE query LIKE 'SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE%' AND wait_event_type = 'Lock' AND datname = current_database()"
                ).fetch_one(&pool).await?;
                if waiting == 2 {
                    break Ok::<(), cja::color_eyre::Report>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), wait).await??;
        blocker.commit().await?;
        let (a, b) = (a.await??, b.await??);
        assert!(matches!(
            (a, b),
            (
                UnlockOutcome::Unlocked(_),
                UnlockOutcome::InsufficientTokens
            ) | (
                UnlockOutcome::InsufficientTokens,
                UnlockOutcome::Unlocked(_)
            )
        ));
        assert_eq!(token_balance(&pool, user).await?, 0);
        let grant_count = sqlx::query_scalar!("SELECT COUNT(*)::bigint AS \"count!\" FROM customization_grants WHERE user_id = $1 AND source = 'token'", user).fetch_one(&pool).await?;
        assert_eq!(grant_count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_double_submit_spends_once(pool: PgPool) -> cja::Result<()> {
        let user = create_test_user(&pool).await?;
        sqlx::query!("INSERT INTO customization_active_weeks (user_id, week_start) VALUES ($1, '2026-10-05')", user).execute(&pool).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query!(
            "SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE",
            user
        )
        .fetch_one(&mut *blocker)
        .await?;
        let a = {
            let pool = pool.clone();
            tokio::spawn(async move { unlock_with_token(&pool, user, "head", "alligator").await })
        };
        let b = {
            let pool = pool.clone();
            tokio::spawn(async move { unlock_with_token(&pool, user, "head", "alligator").await })
        };
        let wait = async {
            loop {
                let waiting = sqlx::query_scalar!(
                    "SELECT COUNT(*)::bigint AS \"count!\" FROM pg_stat_activity WHERE query LIKE 'SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE%' AND wait_event_type = 'Lock' AND datname = current_database()"
                ).fetch_one(&pool).await?;
                if waiting == 2 {
                    break Ok::<(), cja::color_eyre::Report>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(15), wait).await??;
        blocker.commit().await?;
        let (a, b) = (a.await??, b.await??);
        assert!(matches!(
            (a, b),
            (UnlockOutcome::Unlocked(_), UnlockOutcome::AlreadyOwned)
                | (UnlockOutcome::AlreadyOwned, UnlockOutcome::Unlocked(_))
        ));
        assert_eq!(token_balance(&pool, user).await?, 0);
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*)::bigint AS \"count!\" FROM customization_grants WHERE user_id = $1",
            user
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_entry_link_credits_owner(pool: PgPool) -> cja::Result<()> {
        use chrono::{TimeZone as _, Utc};
        let user = create_test_user(&pool).await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Ladder Snake', 'https://example.com') RETURNING battlesnake_id", user
        ).fetch_one(&pool).await?;
        let entry = sqlx::query_scalar!(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) SELECT leaderboard_id, $1 FROM leaderboards LIMIT 1 RETURNING leaderboard_entry_id", snake
        ).fetch_one(&pool).await?;
        let game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        ).fetch_one(&pool).await?;
        sqlx::query!(
            "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id) VALUES ($1, $2)",
            game,
            entry
        )
        .execute(&pool)
        .await?;
        let monday = Utc
            .with_ymd_and_hms(2026, 10, 12, 0, 0, 0)
            .single()
            .unwrap();
        let mut conn = pool.acquire().await?;
        assert_eq!(
            record_active_week_for_game(&mut conn, game, monday).await?,
            1
        );
        assert_eq!(token_balance(&pool, user).await?, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn active_week_insert_does_not_wait_for_spend_lock(pool: PgPool) -> cja::Result<()> {
        use chrono::{TimeZone as _, Utc};
        let user = create_test_user(&pool).await?;
        let snake = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Unlocked Snake', 'https://example.com') RETURNING battlesnake_id", user
        ).fetch_one(&pool).await?;
        let game = sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
        ).fetch_one(&pool).await?;
        sqlx::query!(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)",
            game,
            snake
        )
        .execute(&pool)
        .await?;
        let mut spending = pool.begin().await?;
        sqlx::query!(
            "SELECT user_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE",
            user
        )
        .fetch_one(&mut *spending)
        .await?;
        let finish = Utc
            .with_ymd_and_hms(2026, 10, 12, 0, 0, 0)
            .single()
            .unwrap();
        let mut recording = pool.acquire().await?;
        let inserted = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            record_active_week_for_game(&mut recording, game, finish),
        )
        .await??;
        assert_eq!(inserted, 1);
        spending.rollback().await?;
        assert_eq!(token_balance(&pool, user).await?, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn randomized_finish_and_unlock_model(pool: PgPool) -> cja::Result<()> {
        use chrono::{TimeZone as _, Utc};
        use proptest::{prelude::*, strategy::ValueTree as _};
        let mut runner = proptest::test_runner::TestRunner::default();
        let strategy = proptest::collection::vec((any::<u8>(), any::<u8>()), 1..41);
        let anchors = [
            (
                Utc.with_ymd_and_hms(2026, 10, 11, 23, 59, 59)
                    .single()
                    .unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(),
            ),
            (
                Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0)
                    .single()
                    .unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
            ),
            (
                Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0)
                    .single()
                    .unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
            ),
        ];
        // Eligibility is deliberately declared independently of the catalog predicate.
        let candidates = [
            ("head", "alligator", true),
            ("tail", "alligator", true),
            ("head", "crystal-power", true),
            ("head", "pirate", true),
            ("head", "default", false),
            ("head", "fish", false),
            ("head", "turtle", false),
            ("head", "no-such-slug", false),
            ("invalid", "alligator", false),
        ];
        for case in 0..24 {
            let events = strategy
                .new_tree(&mut runner)
                .map_err(|error| color_eyre::eyre::eyre!("case {case} generation: {error}"))?
                .current();
            let mut users = Vec::new();
            for owner in 0..2 {
                users.push(sqlx::query_scalar!(
                    "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES ($1, $2, 'token') RETURNING user_id",
                    4_000_000_i64 + i64::from(case) * 2 + owner as i64,
                    format!("property-{case}-{owner}"),
                ).fetch_one(&pool).await?);
            }
            let mut snakes = Vec::new();
            for (index, owner) in [0, 0, 1].into_iter().enumerate() {
                snakes.push(sqlx::query_scalar!(
                    "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
                    users[owner], format!("Property Snake {case}-{index}"),
                ).fetch_one(&pool).await?);
            }
            let leaderboard =
                sqlx::query_scalar!("SELECT leaderboard_id FROM leaderboards LIMIT 1")
                    .fetch_one(&pool)
                    .await?;
            let ladder_entry = sqlx::query_scalar!(
                "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) VALUES ($1, $2) RETURNING leaderboard_entry_id",
                leaderboard, snakes[2],
            ).fetch_one(&pool).await?;
            create_grant(&pool, users[0], "head", "pirate").await?;
            let mut earned = [HashSet::new(), HashSet::new()];
            let mut owned = [
                HashSet::from([("head".to_string(), "pirate".to_string())]),
                HashSet::new(),
            ];
            let mut token_owned = [HashSet::new(), HashSet::new()];
            // Fixed game identities and participant masks make replay a true replay.
            let mut games: Vec<(Uuid, usize, u8)> = Vec::new();
            // Every case starts with both owners, two snakes for the first owner,
            // and a ladder link, independent of the random event sequence.
            let seeded_game = sqlx::query_scalar!(
                "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
            ).fetch_one(&pool).await?;
            for snake in &snakes[..2] {
                sqlx::query!(
                    "INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)",
                    seeded_game,
                    snake
                )
                .execute(&pool)
                .await?;
            }
            sqlx::query!(
                "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id) VALUES ($1, $2)",
                seeded_game,
                ladder_entry
            )
            .execute(&pool)
            .await?;
            let mut seeded_tx = pool.begin().await?;
            record_active_week_for_game(&mut seeded_tx, seeded_game, anchors[0].0).await?;
            seeded_tx.commit().await?;
            earned[0].insert(anchors[0].1);
            earned[1].insert(anchors[0].1);
            games.push((seeded_game, 0, 0b111));
            for (step, (a, b)) in events.iter().copied().enumerate() {
                if a < 100 || (a < 130 && games.is_empty()) {
                    let bucket = usize::from(b % 3);
                    let mask = (b / 3) % 8;
                    let game = sqlx::query_scalar!(
                        "INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
                    ).fetch_one(&pool).await?;
                    for (index, snake) in snakes.iter().enumerate() {
                        if mask & (1 << index) == 0 {
                            continue;
                        }
                        if index == 2 {
                            sqlx::query!("INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id) VALUES ($1, $2)", game, ladder_entry)
                                .execute(&pool).await?;
                        } else {
                            sqlx::query!("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)", game, snake)
                                .execute(&pool).await?;
                        }
                    }
                    if mask & 0b011 != 0 {
                        earned[0].insert(anchors[bucket].1);
                    }
                    if mask & 0b100 != 0 {
                        earned[1].insert(anchors[bucket].1);
                    }
                    let mut tx = pool.begin().await?;
                    record_active_week_for_game(&mut tx, game, anchors[bucket].0).await?;
                    tx.commit().await?;
                    games.push((game, bucket, mask));
                } else if a < 130 {
                    let (game, bucket, _mask) = games[usize::from(b) % games.len()];
                    let mut tx = pool.begin().await?;
                    record_active_week_for_game(&mut tx, game, anchors[bucket].0).await?;
                    tx.commit().await?;
                } else {
                    let owner = usize::from(a % 2);
                    let (kind, slug, eligible) = candidates[usize::from(b) % candidates.len()];
                    let key = (kind.to_string(), slug.to_string());
                    let expected = if kind == "invalid" || slug == "no-such-slug" {
                        UnlockOutcome::UnknownItem
                    } else if !eligible {
                        UnlockOutcome::NotTokenUnlockable
                    } else if owned[owner].contains(&key) {
                        UnlockOutcome::AlreadyOwned
                    } else if earned[owner].len() <= token_owned[owner].len() {
                        UnlockOutcome::InsufficientTokens
                    } else {
                        let name = match (kind, slug) {
                            ("head", "alligator") => Head::Alligator.def().display_name,
                            ("tail", "alligator") => Tail::Alligator.def().display_name,
                            ("head", "crystal-power") => Head::CrystalPower.def().display_name,
                            ("head", "pirate") => Head::Pirate.def().display_name,
                            _ => unreachable!(),
                        };
                        UnlockOutcome::Unlocked(name)
                    };
                    let actual = unlock_with_token(&pool, users[owner], kind, slug).await?;
                    assert_eq!(
                        actual, expected,
                        "case {case}, step {step}, events={events:?}"
                    );
                    if matches!(expected, UnlockOutcome::Unlocked(_)) {
                        owned[owner].insert(key.clone());
                        token_owned[owner].insert(key);
                    }
                }
                for owner in 0..2 {
                    let db_weeks: HashSet<_> = sqlx::query_scalar!(
                        "SELECT week_start FROM customization_active_weeks WHERE user_id = $1",
                        users[owner]
                    )
                    .fetch_all(&pool)
                    .await?
                    .into_iter()
                    .collect();
                    let db_grants = sqlx::query!(
                        "SELECT customization_type, slug, source FROM customization_grants WHERE user_id = $1", users[owner]
                    ).fetch_all(&pool).await?;
                    let db_owned: HashSet<_> = db_grants
                        .iter()
                        .map(|row| (row.customization_type.clone(), row.slug.clone()))
                        .collect();
                    let db_token_owned: HashSet<_> = db_grants
                        .iter()
                        .filter(|row| row.source == "token")
                        .map(|row| (row.customization_type.clone(), row.slug.clone()))
                        .collect();
                    let balance = token_balance(&pool, users[owner]).await?;
                    assert_eq!(
                        db_weeks, earned[owner],
                        "case {case}, step {step}, owner {owner}, events={events:?}"
                    );
                    assert_eq!(
                        db_owned, owned[owner],
                        "case {case}, step {step}, owner {owner}, events={events:?}"
                    );
                    assert_eq!(
                        db_token_owned, token_owned[owner],
                        "case {case}, step {step}, owner {owner}, events={events:?}"
                    );
                    assert_eq!(
                        balance,
                        (earned[owner].len() - token_owned[owner].len()) as i64,
                        "case {case}, step {step}, owner {owner}, events={events:?}"
                    );
                    assert!(
                        balance >= 0,
                        "case {case}, step {step}, owner {owner}, events={events:?}"
                    );
                }
            }
        }
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn free_slugs_allowed_without_grant(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        assert_eq!(resolve_head(&pool, user_id, "beluga").await?, "beluga");
        // Empty declarations stay empty; explicit default stays default.
        assert_eq!(resolve_head(&pool, user_id, "").await?, "");
        assert_eq!(
            resolve_tail(&pool, user_id, DEFAULT_SLUG).await?,
            DEFAULT_SLUG
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn paid_slug_requires_grant(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        assert_eq!(
            resolve_head(&pool, user_id, "alligator").await?,
            DEFAULT_SLUG
        );

        create_grant(&pool, user_id, Head::KIND, "alligator").await?;
        // Granting twice is a no-op, not an error.
        create_grant(&pool, user_id, Head::KIND, "alligator").await?;

        assert_eq!(
            resolve_head(&pool, user_id, "alligator").await?,
            "alligator"
        );
        // The grant is per-type: the alligator TAIL is still locked.
        assert_eq!(
            resolve_tail(&pool, user_id, "alligator").await?,
            DEFAULT_SLUG
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn hidden_slug_usable_only_with_grant(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        // 'fish' is in the hidden special-edition group.
        assert_eq!(Head::Fish.def().group.availability(), Availability::Hidden);
        assert_eq!(resolve_head(&pool, user_id, "fish").await?, DEFAULT_SLUG);

        create_grant(&pool, user_id, Head::KIND, "fish").await?;
        assert_eq!(resolve_head(&pool, user_id, "fish").await?, "fish");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unknown_slug_falls_back_to_default(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;
        assert_eq!(
            resolve_head(&pool, user_id, "not-a-real-head").await?,
            DEFAULT_SLUG
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn granted_slugs_round_trip(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;
        assert!(get_granted_slugs(&pool, user_id).await?.is_empty());

        create_grant(&pool, user_id, Head::KIND, "alligator").await?;

        let granted = get_granted_slugs(&pool, user_id).await?;
        assert_eq!(granted.len(), 1);
        assert!(granted.contains(&("head".to_string(), "alligator".to_string())));

        Ok(())
    }

    #[test]
    fn normalize_color_accepts_valid_hex() {
        assert_eq!(normalize_color("#FF8800"), "#ff8800");
        assert_eq!(normalize_color("#00aa33"), "#00aa33");
    }

    #[test]
    fn normalize_color_rejects_invalid() {
        assert_eq!(normalize_color(""), "");
        assert_eq!(normalize_color("ff8800"), "");
        assert_eq!(normalize_color("#ff880"), "");
        assert_eq!(normalize_color("#ff88001"), "");
        assert_eq!(normalize_color("#gg8800"), "");
        assert_eq!(normalize_color("red"), "");
    }

    #[test]
    fn chip_color_falls_back_on_invalid() {
        assert_eq!(chip_color("#FF8800"), "#ff8800");
        assert_eq!(chip_color(""), "#888888");
        assert_eq!(chip_color("red;position:fixed"), "#888888");
    }
}
