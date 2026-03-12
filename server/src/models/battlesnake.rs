use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Type};
use std::str::FromStr;
use uuid::Uuid;

// Visibility enum for battlesnakes
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Visibility {
    #[default]
    Public,
    Private,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }
}

impl FromStr for Visibility {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "public" => Ok(Visibility::Public),
            "private" => Ok(Visibility::Private),
            _ => Err(color_eyre::eyre::eyre!("Invalid visibility: {}", s)),
        }
    }
}

// Default implementation for Visibility - default to Public

// Battlesnake model for our application
#[derive(Debug, Serialize, Deserialize)]
pub struct Battlesnake {
    pub battlesnake_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub url: String,
    pub visibility: Visibility,
    pub color: String,
    pub head: String,
    pub tail: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// For creating a new battlesnake
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateBattlesnake {
    pub name: String,
    pub url: String,
    pub visibility: Visibility,
}

// For updating an existing battlesnake
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpdateBattlesnake {
    pub name: String,
    pub url: String,
    pub visibility: Visibility,
}

// Database functions for battlesnake management

// Get all battlesnakes for a user
pub async fn get_battlesnakes_by_user_id(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<Vec<Battlesnake>> {
    let battlesnakes = sqlx::query_as!(
        Battlesnake,
        r#"
        SELECT
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE user_id = $1
        ORDER BY name ASC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch battlesnakes from database")?;

    Ok(battlesnakes)
}

// Get a single battlesnake by ID
pub async fn get_battlesnake_by_id(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Option<Battlesnake>> {
    let battlesnake = sqlx::query_as!(
        Battlesnake,
        r#"
        SELECT
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE battlesnake_id = $1
        "#,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch battlesnake from database")?;

    Ok(battlesnake)
}

// Create a new battlesnake
pub async fn create_battlesnake(
    pool: &PgPool,
    user_id: Uuid,
    data: CreateBattlesnake,
) -> cja::Result<Battlesnake> {
    let visibility_str = data.visibility.as_str();

    let result = sqlx::query_as!(
        Battlesnake,
        r#"
        INSERT INTO battlesnakes (
            user_id,
            name,
            url,
            visibility
        )
        VALUES ($1, $2, $3, $4)
        RETURNING
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        "#,
        user_id,
        data.name,
        data.url,
        visibility_str
    )
    .fetch_one(pool)
    .await;

    match result {
        Ok(battlesnake) => Ok(battlesnake),
        Err(err) => {
            // Check if this is a unique violation error
            if let Some(db_err) = err.as_database_error()
                && let Some(constraint) = db_err.constraint()
                && constraint == "unique_battlesnake_name_per_user"
            {
                return Err(cja::color_eyre::eyre::eyre!(
                    "You already have a battlesnake named '{}'. Please choose a different name.",
                    data.name
                ));
            }

            // If it's not a unique constraint violation, wrap with a generic error
            Err(err).wrap_err("Failed to create battlesnake in database")
        }
    }
}

// Update an existing battlesnake
pub async fn update_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    user_id: Uuid,
    data: UpdateBattlesnake,
) -> cja::Result<Battlesnake> {
    let visibility_str = data.visibility.as_str();

    let result = sqlx::query_as!(
        Battlesnake,
        r#"
        UPDATE battlesnakes
        SET
            name = $3,
            url = $4,
            visibility = $5
        WHERE
            battlesnake_id = $1
            AND user_id = $2
        RETURNING
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        "#,
        battlesnake_id,
        user_id,
        data.name,
        data.url,
        visibility_str
    )
    .fetch_one(pool)
    .await;

    match result {
        Ok(battlesnake) => Ok(battlesnake),
        Err(err) => {
            // Check if this is a unique violation error
            if let Some(db_err) = err.as_database_error()
                && let Some(constraint) = db_err.constraint()
                && constraint == "unique_battlesnake_name_per_user"
            {
                return Err(cja::color_eyre::eyre::eyre!(
                    "You already have a battlesnake named '{}'. Please choose a different name.",
                    data.name
                ));
            }

            // If it's not a unique constraint violation, wrap with a generic error
            Err(err).wrap_err("Failed to update battlesnake in database")
        }
    }
}

// Delete a battlesnake
pub async fn delete_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    user_id: Uuid,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        DELETE FROM battlesnakes
        WHERE
            battlesnake_id = $1
            AND user_id = $2
        "#,
        battlesnake_id,
        user_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to delete battlesnake from database")?;

    Ok(())
}

// Check if a battlesnake belongs to a user
pub async fn belongs_to_user(
    pool: &PgPool,
    battlesnake_id: Uuid,
    user_id: Uuid,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM battlesnakes
            WHERE
                battlesnake_id = $1
                AND user_id = $2
        ) as "exists!"
        "#,
        battlesnake_id,
        user_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check if battlesnake belongs to user")?;

    Ok(result.exists)
}

// Get all public battlesnakes (for other users to select)
pub async fn get_public_battlesnakes(pool: &PgPool) -> cja::Result<Vec<Battlesnake>> {
    let battlesnakes = sqlx::query_as!(
        Battlesnake,
        r#"
        SELECT
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE visibility = 'public'
        ORDER BY name ASC
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch public battlesnakes from database")?;

    Ok(battlesnakes)
}

// Get all battlesnakes available to a user (their own + public ones)
pub async fn get_available_battlesnakes(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<Vec<Battlesnake>> {
    let battlesnakes = sqlx::query_as!(
        Battlesnake,
        r#"
        SELECT
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE user_id = $1 OR visibility = 'public'
        ORDER BY name ASC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch available battlesnakes from database")?;

    Ok(battlesnakes)
}

pub async fn search_public_battlesnakes(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> cja::Result<Vec<Battlesnake>> {
    // Escape ILIKE wildcards so literal '%' and '_' in search terms don't match as wildcards
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let battlesnakes = sqlx::query_as!(
        Battlesnake,
        r#"
        SELECT
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE visibility = 'public' AND name ILIKE '%' || $1 || '%' ESCAPE '\'
        ORDER BY name
        LIMIT $2
        "#,
        escaped,
        limit
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to search public battlesnakes")?;

    Ok(battlesnakes)
}

pub async fn update_battlesnake_customizations(
    pool: &PgPool,
    battlesnake_id: Uuid,
    color: &str,
    head: &str,
    tail: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        UPDATE battlesnakes
        SET color = $2, head = $3, tail = $4
        WHERE battlesnake_id = $1
        "#,
        battlesnake_id,
        color,
        head,
        tail,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to update battlesnake customizations")?;
    Ok(())
}
