use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::github::auth::{GitHubTokenResponse, GitHubUser};

// User model for our application
#[derive(Debug, Serialize, Deserialize)]
pub struct User {
    pub user_id: Uuid,
    pub external_github_id: i64,
    pub github_login: String,
    pub github_avatar_url: Option<String>,
    pub github_name: Option<String>,
    pub github_email: Option<String>,
    pub display_name: Option<String>,
    pub pronouns: String,
    pub country: String,
    pub backstory: String,
    pub is_admin: bool,
    pub site_theme: String,
    pub theater_theme: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Valid values for `users.site_theme` (mirrors the DB CHECK constraint).
pub const SITE_THEMES: [&str; 3] = ["system", "light", "dark"];
/// Valid values for `users.theater_theme` (mirrors the DB CHECK constraint).
pub const THEATER_THEMES: [&str; 3] = ["match", "dark", "light"];

// Database functions for user management
pub async fn get_user_by_id(pool: &PgPool, user_id: Uuid) -> cja::Result<Option<User>> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT
            user_id,
            external_github_id,
            github_login,
            github_avatar_url,
            github_name,
            github_email,
            display_name,
            pronouns,
            country,
            backstory,
            is_admin,
            site_theme,
            theater_theme,
            created_at,
            updated_at
        FROM users
        WHERE user_id = $1
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch user from database")?;

    Ok(user)
}

/// Look up a user for the public profile page. GitHub logins are
/// case-insensitive, so match accordingly.
pub async fn get_user_by_github_login(pool: &PgPool, login: &str) -> cja::Result<Option<User>> {
    let user = sqlx::query_as!(
        User,
        r#"
        SELECT
            user_id,
            external_github_id,
            github_login,
            github_avatar_url,
            github_name,
            github_email,
            display_name,
            pronouns,
            country,
            backstory,
            is_admin,
            site_theme,
            theater_theme,
            created_at,
            updated_at
        FROM users
        WHERE LOWER(github_login) = LOWER($1)
        "#,
        login
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch user by login from database")?;

    Ok(user)
}

/// One row of the public `/players` directory.
///
/// `user_id` is carried alongside the login because `users.github_login` has
/// no uniqueness constraint — the UUID is what makes a directory link resolve
/// to exactly the player the row describes.
#[derive(Debug)]
pub struct PlayerDirectoryEntry {
    pub user_id: Uuid,
    pub github_login: String,
    pub public_name: String,
}

/// Count players in the directory. `active_only` restricts to players with at
/// least one enabled leaderboard entry — there is no snake-level active flag,
/// so pausing a snake (manually or via the health sweeper) is expressed as
/// `leaderboard_entries.disabled_at`.
///
/// The two modes are separate statements rather than one query gated on a
/// bind parameter. Writing the filter as `NOT $1 OR EXISTS (...)` keeps the
/// sublink inside an `OR`, which stops Postgres pulling it up into a semi-join
/// — it stays a `SubPlan` costed as if it re-ran per user row, and past a few
/// thousand users that estimate crosses `jit_above_cost` and every request
/// pays for LLVM compilation. A bare top-level `EXISTS` plans as a hash
/// semi-join instead.
pub async fn count_players(pool: &PgPool, active_only: bool) -> cja::Result<i64> {
    let count = if active_only {
        sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "count!"
            FROM users u
            WHERE EXISTS (
                SELECT 1
                FROM battlesnakes b
                JOIN leaderboard_entries le
                  ON le.battlesnake_id = b.battlesnake_id
                WHERE b.user_id = u.user_id
                  AND le.disabled_at IS NULL
            )
            "#
        )
        .fetch_one(pool)
        .await
    } else {
        sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "count!"
            FROM users
            "#
        )
        .fetch_one(pool)
        .await
    }
    .wrap_err("Failed to count players in database")?;

    Ok(count)
}

/// One page of the player directory. Ordering is fixed (not user-facing sort):
/// displayed name, then login, both case-insensitive, with `user_id` as the
/// final tie-breaker so equal names can't shuffle rows between pages.
///
/// Split by mode for the same planner reason as [`count_players`].
pub async fn get_players_paginated(
    pool: &PgPool,
    active_only: bool,
    page: i64,
    per_page: i64,
) -> cja::Result<Vec<PlayerDirectoryEntry>> {
    let offset = page * per_page;

    let players = if active_only {
        sqlx::query_as!(
            PlayerDirectoryEntry,
            r#"
            SELECT
                u.user_id,
                u.github_login,
                COALESCE(NULLIF(u.display_name, ''), u.github_login) AS "public_name!"
            FROM users u
            WHERE EXISTS (
                SELECT 1
                FROM battlesnakes b
                JOIN leaderboard_entries le
                  ON le.battlesnake_id = b.battlesnake_id
                WHERE b.user_id = u.user_id
                  AND le.disabled_at IS NULL
            )
            ORDER BY
                LOWER(COALESCE(NULLIF(u.display_name, ''), u.github_login)) ASC,
                LOWER(u.github_login) ASC,
                u.user_id ASC
            LIMIT $1 OFFSET $2
            "#,
            per_page,
            offset
        )
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as!(
            PlayerDirectoryEntry,
            r#"
            SELECT
                u.user_id,
                u.github_login,
                COALESCE(NULLIF(u.display_name, ''), u.github_login) AS "public_name!"
            FROM users u
            ORDER BY
                LOWER(COALESCE(NULLIF(u.display_name, ''), u.github_login)) ASC,
                LOWER(u.github_login) ASC,
                u.user_id ASC
            LIMIT $1 OFFSET $2
            "#,
            per_page,
            offset
        )
        .fetch_all(pool)
        .await
    }
    .wrap_err("Failed to fetch paginated players from database")?;

    Ok(players)
}

pub async fn create_or_update_user(
    pool: &PgPool,
    github_user: GitHubUser,
    token: GitHubTokenResponse,
) -> cja::Result<User> {
    let token_expires_at = token
        .expires_in
        .map(|expires_in| chrono::Utc::now() + chrono::Duration::seconds(expires_in));

    let user = sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (
            external_github_id,
            github_login,
            github_avatar_url,
            github_name,
            github_email,
            github_access_token,
            github_refresh_token,
            github_token_expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (external_github_id) DO UPDATE SET
            github_login = $2,
            github_avatar_url = $3,
            github_name = $4,
            github_email = $5,
            github_access_token = $6,
            github_refresh_token = $7,
            github_token_expires_at = $8
        RETURNING
            user_id,
            external_github_id,
            github_login,
            github_avatar_url,
            github_name,
            github_email,
            display_name,
            pronouns,
            country,
            backstory,
            is_admin,
            site_theme,
            theater_theme,
            created_at,
            updated_at
        "#,
        github_user.id,
        github_user.login,
        github_user.avatar_url,
        github_user.name,
        github_user.email,
        token.access_token,
        token.refresh_token,
        token_expires_at
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to create or update user in database")?;

    Ok(user)
}

pub const MAX_DISPLAY_NAME_LEN: usize = 100;
pub const MAX_PRONOUNS_LEN: usize = 50;
pub const MAX_COUNTRY_LEN: usize = 100;
pub const MAX_BACKSTORY_LEN: usize = 2000;

/// Validate profile field lengths. Call after trimming.
/// Returns `Err(message)` on the first field that exceeds its limit.
pub fn validate_profile_fields(
    display_name: &str,
    pronouns: &str,
    country: &str,
    backstory: &str,
) -> Result<(), String> {
    if display_name.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(format!(
            "Display name must be {MAX_DISPLAY_NAME_LEN} characters or fewer"
        ));
    }
    if pronouns.chars().count() > MAX_PRONOUNS_LEN {
        return Err(format!(
            "Pronouns must be {MAX_PRONOUNS_LEN} characters or fewer"
        ));
    }
    if country.chars().count() > MAX_COUNTRY_LEN {
        return Err(format!(
            "Country must be {MAX_COUNTRY_LEN} characters or fewer"
        ));
    }
    if backstory.chars().count() > MAX_BACKSTORY_LEN {
        return Err(format!(
            "Backstory must be {MAX_BACKSTORY_LEN} characters or fewer"
        ));
    }
    Ok(())
}

/// Update the user's editable profile fields. All values should be
/// trimmed before calling. Empty `display_name` sets the column to NULL
/// (clearing); empty pronouns/country/backstory store empty string
/// (matching the NOT NULL DEFAULT '' convention).
pub async fn update_profile_fields(
    pool: &PgPool,
    user_id: Uuid,
    display_name: &str,
    pronouns: &str,
    country: &str,
    backstory: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        UPDATE users
        SET display_name = NULLIF($2, ''),
            pronouns = $3,
            country = $4,
            backstory = $5
        WHERE user_id = $1
        "#,
        user_id,
        display_name,
        pronouns,
        country,
        backstory,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to update profile fields")?;

    Ok(())
}

/// Persist the two-axis appearance preference. Values must already be
/// validated against `SITE_THEMES` / `THEATER_THEMES` (the DB CHECK
/// constraint is the backstop).
pub async fn update_theme_preferences(
    pool: &PgPool,
    user_id: Uuid,
    site_theme: &str,
    theater_theme: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        UPDATE users
        SET site_theme = $2,
            theater_theme = $3
        WHERE user_id = $1
        "#,
        user_id,
        site_theme,
        theater_theme,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to update theme preferences")?;

    Ok(())
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

    /// Named variant of `create_user` for directory tests, which care about
    /// the login and display name that drive labels and ordering.
    async fn create_named_user(
        pool: &PgPool,
        github_id: i64,
        login: &str,
        display_name: Option<&str>,
    ) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token, display_name)
             VALUES ($1, $2, 'test-token', $3)
             RETURNING user_id",
            github_id,
            login,
            display_name,
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    async fn create_snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'https://example.com/snake')
             RETURNING battlesnake_id",
            user_id,
            name,
        )
        .fetch_one(pool)
        .await?;
        Ok(row.battlesnake_id)
    }

    async fn create_private_snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, $2, 'https://example.com/snake', 'private')
             RETURNING battlesnake_id",
            user_id,
            name,
        )
        .fetch_one(pool)
        .await?;
        Ok(row.battlesnake_id)
    }

    /// Create a leaderboard entry on the migration-seeded `Standard 11x11`
    /// board. `disabled` mirrors a paused/health-disabled snake.
    async fn create_entry(pool: &PgPool, battlesnake_id: Uuid, disabled: bool) -> cja::Result<()> {
        create_entry_on(pool, battlesnake_id, "Standard 11x11", disabled).await
    }

    async fn create_entry_on(
        pool: &PgPool,
        battlesnake_id: Uuid,
        leaderboard_name: &str,
        disabled: bool,
    ) -> cja::Result<()> {
        let disabled_at = disabled.then(chrono::Utc::now);
        sqlx::query!(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, disabled_at)
             SELECT leaderboard_id, $2, $3 FROM leaderboards WHERE name = $1",
            leaderboard_name,
            battlesnake_id,
            disabled_at,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn player_logins(pool: &PgPool, active_only: bool) -> cja::Result<Vec<String>> {
        Ok(get_players_paginated(pool, active_only, 0, 50)
            .await?
            .into_iter()
            .map(|p| p.github_login)
            .collect())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn directory_is_empty_on_a_fresh_database(pool: PgPool) -> cja::Result<()> {
        assert_eq!(count_players(&pool, false).await?, 0);
        assert_eq!(count_players(&pool, true).await?, 0);
        assert!(get_players_paginated(&pool, false, 0, 50).await?.is_empty());
        assert!(get_players_paginated(&pool, true, 0, 50).await?.is_empty());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn active_filter_keys_off_enabled_leaderboard_entries(pool: PgPool) -> cja::Result<()> {
        // No snakes at all.
        create_named_user(&pool, 7001, "no-snakes", None).await?;

        // A snake, but never entered on a leaderboard.
        let unentered = create_named_user(&pool, 7002, "no-entries", None).await?;
        create_snake(&pool, unentered, "Unentered").await?;

        // A snake whose only entry is disabled.
        let paused = create_named_user(&pool, 7003, "only-disabled", None).await?;
        let paused_snake = create_snake(&pool, paused, "Paused").await?;
        create_entry(&pool, paused_snake, true).await?;

        // A snake with an enabled entry.
        let active = create_named_user(&pool, 7004, "has-enabled", None).await?;
        let active_snake = create_snake(&pool, active, "Active").await?;
        create_entry(&pool, active_snake, false).await?;

        assert_eq!(count_players(&pool, false).await?, 4);
        assert_eq!(
            player_logins(&pool, false).await?,
            vec!["has-enabled", "no-entries", "no-snakes", "only-disabled"]
        );

        assert_eq!(count_players(&pool, true).await?, 1);
        assert_eq!(player_logins(&pool, true).await?, vec!["has-enabled"]);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn active_filter_never_duplicates_a_player(pool: PgPool) -> cja::Result<()> {
        let user_id = create_named_user(&pool, 7101, "many-entries", None).await?;

        // Two snakes, and one of them on two boards — four qualifying rows in
        // the join, but the player must appear exactly once.
        sqlx::query!("INSERT INTO leaderboards (name) VALUES ('Duplicated Board')")
            .execute(&pool)
            .await?;
        let first = create_snake(&pool, user_id, "First").await?;
        let second = create_snake(&pool, user_id, "Second").await?;
        create_entry(&pool, first, false).await?;
        create_entry_on(&pool, first, "Duplicated Board", false).await?;
        create_entry(&pool, second, false).await?;
        create_entry_on(&pool, second, "Duplicated Board", false).await?;

        assert_eq!(count_players(&pool, true).await?, 1);
        assert_eq!(player_logins(&pool, true).await?, vec!["many-entries"]);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn active_filter_ignores_visibility_and_parent_leaderboard_state(
        pool: PgPool,
    ) -> cja::Result<()> {
        // A private snake still makes its owner active — visibility only
        // gates matchmaking, not the directory.
        let private_owner = create_named_user(&pool, 7201, "private-owner", None).await?;
        let private_snake = create_private_snake(&pool, private_owner, "Hidden").await?;
        create_entry(&pool, private_snake, false).await?;

        // So does an enabled entry on a disabled leaderboard — only the
        // entry's own `disabled_at` is part of the definition.
        sqlx::query!(
            "INSERT INTO leaderboards (name, disabled_at) VALUES ('Retired Board', NOW())"
        )
        .execute(&pool)
        .await?;
        let retired_owner = create_named_user(&pool, 7202, "retired-board-owner", None).await?;
        let retired_snake = create_snake(&pool, retired_owner, "OnRetiredBoard").await?;
        create_entry_on(&pool, retired_snake, "Retired Board", false).await?;

        assert_eq!(
            player_logins(&pool, true).await?,
            vec!["private-owner", "retired-board-owner"]
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn public_name_falls_back_to_login(pool: PgPool) -> cja::Result<()> {
        create_named_user(&pool, 7301, "zeta-null", None).await?;
        create_named_user(&pool, 7302, "zeta-empty", Some("")).await?;
        create_named_user(&pool, 7303, "zeta-named", Some("Aardvark")).await?;

        let names: Vec<(String, String)> = get_players_paginated(&pool, false, 0, 50)
            .await?
            .into_iter()
            .map(|p| (p.github_login, p.public_name))
            .collect();

        // "Aardvark" sorts ahead of the two logins it beats alphabetically.
        assert_eq!(
            names,
            vec![
                ("zeta-named".to_string(), "Aardvark".to_string()),
                ("zeta-empty".to_string(), "zeta-empty".to_string()),
                ("zeta-null".to_string(), "zeta-null".to_string()),
            ]
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn pagination_partitions_the_directory_deterministically(
        pool: PgPool,
    ) -> cja::Result<()> {
        for i in 0..5 {
            create_named_user(&pool, 7400 + i, &format!("player-{i:02}"), None).await?;
        }

        assert_eq!(count_players(&pool, false).await?, 5);

        let logins = |players: Vec<PlayerDirectoryEntry>| {
            players
                .into_iter()
                .map(|p| p.github_login)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            logins(get_players_paginated(&pool, false, 0, 2).await?),
            vec!["player-00", "player-01"]
        );
        assert_eq!(
            logins(get_players_paginated(&pool, false, 1, 2).await?),
            vec!["player-02", "player-03"]
        );
        assert_eq!(
            logins(get_players_paginated(&pool, false, 2, 2).await?),
            vec!["player-04"]
        );
        // Past the end there is simply nothing left.
        assert!(get_players_paginated(&pool, false, 3, 2).await?.is_empty());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn colliding_logins_keep_distinct_resolvable_identities(pool: PgPool) -> cja::Result<()> {
        // `users.github_login` has no uniqueness constraint, and lookups by
        // login are case-insensitive — so these three are indistinguishable by
        // login alone but must stay distinct rows in the directory.
        let first = create_named_user(&pool, 7501, "twin", Some("First Twin")).await?;
        let second = create_named_user(&pool, 7502, "twin", Some("Second Twin")).await?;
        let cased = create_named_user(&pool, 7503, "TWIN", Some("Third Twin")).await?;

        let players = get_players_paginated(&pool, false, 0, 50).await?;
        let ids: Vec<Uuid> = players.iter().map(|p| p.user_id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&first) && ids.contains(&second) && ids.contains(&cased));

        // Each row's UUID resolves to the user that row describes.
        for player in &players {
            let user = get_user_by_id(&pool, player.user_id).await?.unwrap();
            assert_eq!(
                user.display_name.as_deref(),
                Some(player.public_name.as_str())
            );
        }

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn update_profile_fields_writes_and_clears(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9001).await?;

        // Write values.
        update_profile_fields(
            &pool,
            user_id,
            "My Name",
            "they/them",
            "Canada",
            "Snake fighter",
        )
        .await?;

        let user = get_user_by_id(&pool, user_id).await?.unwrap();
        assert_eq!(user.display_name.as_deref(), Some("My Name"));
        assert_eq!(user.pronouns, "they/them");
        assert_eq!(user.country, "Canada");
        assert_eq!(user.backstory, "Snake fighter");

        // Clear by setting to empty string.
        update_profile_fields(&pool, user_id, "", "", "", "").await?;

        let user = get_user_by_id(&pool, user_id).await?.unwrap();
        assert_eq!(user.display_name, None); // NULLIF('', '') -> NULL
        assert_eq!(user.pronouns, "");
        assert_eq!(user.country, "");
        assert_eq!(user.backstory, "");

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn theme_preferences_default_and_update(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9002).await?;

        let user = get_user_by_id(&pool, user_id).await?.unwrap();
        assert_eq!(user.site_theme, "system");
        assert_eq!(user.theater_theme, "dark");

        update_theme_preferences(&pool, user_id, "dark", "match").await?;

        let user = get_user_by_id(&pool, user_id).await?.unwrap();
        assert_eq!(user.site_theme, "dark");
        assert_eq!(user.theater_theme, "match");

        // The CHECK constraint rejects values outside the allowed sets.
        assert!(
            update_theme_preferences(&pool, user_id, "hotdog", "match")
                .await
                .is_err()
        );

        Ok(())
    }

    #[test]
    fn validation_rejects_over_limit() {
        assert!(validate_profile_fields("ok", "ok", "ok", "ok").is_ok());

        let long_pronouns = "x".repeat(51);
        assert!(validate_profile_fields("ok", &long_pronouns, "ok", "ok").is_err());

        let long_country = "x".repeat(101);
        assert!(validate_profile_fields("ok", "ok", &long_country, "ok").is_err());

        let long_backstory = "x".repeat(2001);
        assert!(validate_profile_fields("ok", "ok", "ok", &long_backstory).is_err());

        let long_display = "x".repeat(101);
        assert!(validate_profile_fields(&long_display, "ok", "ok", "ok").is_err());
    }

    #[test]
    fn validation_counts_characters_not_bytes() {
        // 40 emoji = 160 bytes but only 40 chars — under the 50-char
        // pronouns limit; byte-based validation would wrongly reject it.
        let emoji_pronouns = "\u{1F40D}".repeat(40);
        assert!(validate_profile_fields("ok", &emoji_pronouns, "ok", "ok").is_ok());

        let too_many = "\u{1F40D}".repeat(51);
        assert!(validate_profile_fields("ok", &too_many, "ok", "ok").is_err());
    }

    #[test]
    fn validation_accepts_at_limit() {
        let max_pronouns = "x".repeat(50);
        let max_country = "x".repeat(100);
        let max_backstory = "x".repeat(2000);
        let max_display = "x".repeat(100);
        assert!(
            validate_profile_fields(&max_display, &max_pronouns, &max_country, &max_backstory)
                .is_ok()
        );
    }
}
