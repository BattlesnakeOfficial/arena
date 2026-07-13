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
