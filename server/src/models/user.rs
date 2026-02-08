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
    pub is_admin: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

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
            is_admin,
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
            is_admin,
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
