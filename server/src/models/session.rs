use color_eyre::eyre::{Context as _, eyre};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::user::User;

/// Session model for the application
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub session_id: Uuid,
    pub user_id: Option<Uuid>,
    pub github_oauth_state: Option<String>,
    pub flash_message: Option<String>,
    pub flash_type: Option<String>,
    pub is_cli_auth: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

// Constant for session cookie name
pub const SESSION_COOKIE_NAME: &str = "arena_session_id";
// Default session expiration in seconds (30 days)
pub const SESSION_EXPIRATION_SECONDS: i64 = 60 * 60 * 24 * 30;

// Flash message types as constants
pub const FLASH_TYPE_SUCCESS: &str = "success";
pub const FLASH_TYPE_ERROR: &str = "error";
pub const FLASH_TYPE_INFO: &str = "info";
pub const FLASH_TYPE_WARNING: &str = "warning";
pub const FLASH_TYPE_PRIMARY: &str = "primary";

impl Session {
    /// Get the CSS class for the flash message
    pub fn flash_class(&self) -> &'static str {
        match self.flash_type.as_deref() {
            Some(FLASH_TYPE_SUCCESS) => "alert alert-success",
            Some(FLASH_TYPE_ERROR) => "alert alert-danger",
            Some(FLASH_TYPE_INFO) => "alert alert-info",
            Some(FLASH_TYPE_WARNING) => "alert alert-warning",
            Some(FLASH_TYPE_PRIMARY) | Some(_) => "alert alert-primary",
            None => "",
        }
    }
}

/// Create a new session
///
/// Creates a new anonymous session with no user attached.
pub async fn create_session(pool: &PgPool) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        INSERT INTO sessions (github_oauth_state, flash_message, flash_type)
        VALUES (NULL, NULL, NULL)
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to create new session")?;

    Ok(session)
}

/// Get a session by ID
#[allow(dead_code)]
pub async fn get_active_session_by_id(
    pool: &PgPool,
    session_id: Uuid,
) -> cja::Result<Option<Session>> {
    let session = sqlx::query_as!(
        Session,
        r#"
        SELECT
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        FROM sessions
        WHERE
            session_id = $1
            AND expires_at > NOW()
        "#,
        session_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch session from database")?;

    Ok(session)
}

/// Set a flash message on a session
pub async fn set_flash_message(
    pool: &PgPool,
    session_id: Uuid,
    message: String,
    flash_type: &str,
) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            flash_message = $2,
            flash_type = $3
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id,
        message,
        flash_type
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to set flash message for session")?;

    Ok(session)
}

/// Clear a flash message from a session
pub async fn clear_flash_message(pool: &PgPool, session_id: Uuid) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            flash_message = NULL,
            flash_type = NULL
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to clear flash message for session")?;

    Ok(session)
}

/// Get a session with the user data
pub async fn get_session_with_user(
    pool: &PgPool,
    session_id: Uuid,
) -> cja::Result<Option<(Session, Option<User>)>> {
    let result = sqlx::query!(
        r#"
        SELECT
            s.session_id,
            s.user_id,
            s.github_oauth_state,
            s.flash_message,
            s.flash_type,
            s.is_cli_auth,
            s.created_at,
            s.updated_at,
            s.expires_at,
            u.user_id as "user_user_id?",
            u.external_github_id as "external_github_id?",
            u.github_login as "github_login?",
            u.github_avatar_url as "github_avatar_url?",
            u.github_name as "github_name?",
            u.github_email as "github_email?",
            u.is_admin as "is_admin?",
            u.created_at as "user_created_at?",
            u.updated_at as "user_updated_at?"
        FROM sessions s
        LEFT JOIN users u ON s.user_id = u.user_id
        WHERE
            s.session_id = $1
            AND s.expires_at > NOW()
        "#,
        session_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch session with user from database")?;

    match result {
        Some(row) => {
            let session = Session {
                session_id: row.session_id,
                user_id: row.user_id,
                github_oauth_state: row.github_oauth_state,
                flash_message: row.flash_message,
                flash_type: row.flash_type,
                is_cli_auth: row.is_cli_auth,
                created_at: row.created_at,
                updated_at: row.updated_at,
                expires_at: row.expires_at,
            };

            let user = if let Some(user_id) = row.user_user_id {
                // Check that we have the required fields to construct a user
                let github_id = row
                    .external_github_id
                    .ok_or_else(|| eyre!("External GitHub ID is missing for user"))?;
                let github_login = row
                    .github_login
                    .ok_or_else(|| eyre!("GitHub login is missing for user"))?;
                let user_created_at = row
                    .user_created_at
                    .ok_or_else(|| eyre!("User created_at timestamp is missing"))?;
                let user_updated_at = row
                    .user_updated_at
                    .ok_or_else(|| eyre!("User updated_at timestamp is missing"))?;

                Some(User {
                    user_id,
                    external_github_id: github_id,
                    github_login,
                    github_avatar_url: row.github_avatar_url,
                    github_name: row.github_name,
                    github_email: row.github_email,
                    is_admin: row.is_admin.unwrap_or(false),
                    created_at: user_created_at,
                    updated_at: user_updated_at,
                })
            } else {
                None
            };

            Ok(Some((session, user)))
        }
        None => Ok(None),
    }
}

/// Set GitHub OAuth state for a session
pub async fn set_github_oauth_state(
    pool: &PgPool,
    session_id: Uuid,
    oauth_state: String,
) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            github_oauth_state = $2
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id,
        oauth_state
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to set GitHub OAuth state for session")?;

    Ok(session)
}

/// Set GitHub OAuth state and CLI auth flag for a session
pub async fn set_github_oauth_state_with_cli(
    pool: &PgPool,
    session_id: Uuid,
    oauth_state: String,
    is_cli_auth: bool,
) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            github_oauth_state = $2,
            is_cli_auth = $3
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id,
        oauth_state,
        is_cli_auth
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to set GitHub OAuth state for session")?;

    Ok(session)
}

/// Clear GitHub OAuth state for a session (also resets is_cli_auth)
pub async fn clear_github_oauth_state(pool: &PgPool, session_id: Uuid) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            github_oauth_state = NULL,
            is_cli_auth = FALSE
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to clear GitHub OAuth state for session")?;

    Ok(session)
}

/// Associate a user with a session
pub async fn associate_user_with_session(
    pool: &PgPool,
    session_id: Uuid,
    user_id: Uuid,
) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            user_id = $2,
            github_oauth_state = NULL,
            expires_at = NOW() + INTERVAL '30 days'
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id,
        user_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to associate user with session")?;

    Ok(session)
}

/// Disassociate a user from a session (logout)
pub async fn disassociate_user_from_session(
    pool: &PgPool,
    session_id: Uuid,
) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            user_id = NULL,
            github_oauth_state = NULL,
            is_cli_auth = FALSE,
            expires_at = NOW() + INTERVAL '1 hour'
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to disassociate user from session")?;

    Ok(session)
}

/// Refresh a session's expiration time
pub async fn refresh_session(pool: &PgPool, session_id: Uuid) -> cja::Result<Session> {
    let session = sqlx::query_as!(
        Session,
        r#"
        UPDATE sessions
        SET
            expires_at = GREATEST(expires_at, NOW() + INTERVAL '30 days')
        WHERE session_id = $1
        RETURNING
            session_id,
            user_id,
            github_oauth_state,
            flash_message,
            flash_type,
            is_cli_auth,
            created_at,
            updated_at,
            expires_at
        "#,
        session_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to refresh session")?;

    Ok(session)
}

/// Delete a session
pub async fn delete_session(pool: &PgPool, session_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        r#"
        DELETE FROM sessions
        WHERE session_id = $1
        "#,
        session_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to delete session")?;

    Ok(())
}

/// Clean expired sessions
pub async fn clean_expired_sessions(pool: &PgPool) -> cja::Result<u64> {
    let result = sqlx::query!(
        r#"
        DELETE FROM sessions
        WHERE expires_at < NOW()
        "#
    )
    .execute(pool)
    .await
    .wrap_err("Failed to clean expired sessions")?;

    Ok(result.rows_affected())
}
