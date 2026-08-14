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

// A public battlesnake as shown in the public /snakes directory. Joined with
// the owner's login so the listing doesn't need a per-row user lookup.
// Deliberately omits `url` — a snake's server URL is only shown to its owner.
#[derive(Debug)]
pub struct PublicBattlesnakeListItem {
    pub battlesnake_id: Uuid,
    pub name: String,
    pub color: String,
    pub owner_login: String,
}

// Count of all public battlesnakes, for paginating the public directory
pub async fn count_public_battlesnakes(pool: &PgPool) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM battlesnakes
        WHERE visibility = 'public'
        "#
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count public battlesnakes in database")?;

    Ok(count)
}

// One page of public battlesnakes, ordered by name. `battlesnake_id` breaks
// ties so duplicate names can't shuffle rows between pages.
pub async fn get_public_battlesnakes_paginated(
    pool: &PgPool,
    page: i64,
    per_page: i64,
) -> cja::Result<Vec<PublicBattlesnakeListItem>> {
    let offset = page * per_page;

    let battlesnakes = sqlx::query_as!(
        PublicBattlesnakeListItem,
        r#"
        SELECT
            b.battlesnake_id,
            b.name,
            b.color,
            u.github_login AS owner_login
        FROM battlesnakes b
        JOIN users u ON b.user_id = u.user_id
        WHERE b.visibility = 'public'
        ORDER BY b.name ASC, b.battlesnake_id ASC
        LIMIT $1 OFFSET $2
        "#,
        per_page,
        offset
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch paginated public battlesnakes from database")?;

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

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_user(pool: &PgPool, github_id: i64, login: &str) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
             RETURNING user_id",
            github_id,
            login
        )
        .fetch_one(pool)
        .await?;

        Ok(row.user_id)
    }

    async fn create_snake(
        pool: &PgPool,
        user_id: Uuid,
        name: &str,
        visibility: Visibility,
    ) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, $2, 'http://localhost:8000', $3)
             RETURNING battlesnake_id",
            user_id,
            name,
            visibility.as_str()
        )
        .fetch_one(pool)
        .await?;

        Ok(row.battlesnake_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn count_public_battlesnakes_excludes_private(pool: PgPool) -> cja::Result<()> {
        let owner = create_user(&pool, 8001, "count-owner").await?;
        create_snake(&pool, owner, "Alpha", Visibility::Public).await?;
        create_snake(&pool, owner, "Beta", Visibility::Public).await?;
        create_snake(&pool, owner, "Hidden", Visibility::Private).await?;

        assert_eq!(count_public_battlesnakes(&pool).await?, 2);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn paginated_public_battlesnakes_exclude_private_and_join_owner(
        pool: PgPool,
    ) -> cja::Result<()> {
        let first_owner = create_user(&pool, 8101, "first-owner").await?;
        let second_owner = create_user(&pool, 8102, "second-owner").await?;

        create_snake(&pool, first_owner, "Anaconda", Visibility::Public).await?;
        create_snake(&pool, second_owner, "Boa", Visibility::Public).await?;
        create_snake(&pool, second_owner, "Secret", Visibility::Private).await?;

        let snakes = get_public_battlesnakes_paginated(&pool, 0, 50).await?;

        let listed: Vec<(&str, &str)> = snakes
            .iter()
            .map(|s| (s.name.as_str(), s.owner_login.as_str()))
            .collect();
        assert_eq!(
            listed,
            vec![("Anaconda", "first-owner"), ("Boa", "second-owner")]
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn paginated_public_battlesnakes_honor_page_boundaries(pool: PgPool) -> cja::Result<()> {
        let owner = create_user(&pool, 8201, "page-owner").await?;
        for i in 0..5 {
            create_snake(&pool, owner, &format!("Snake {i:02}"), Visibility::Public).await?;
        }

        let first = get_public_battlesnakes_paginated(&pool, 0, 2).await?;
        let second = get_public_battlesnakes_paginated(&pool, 1, 2).await?;
        let third = get_public_battlesnakes_paginated(&pool, 2, 2).await?;

        let names = |snakes: &[PublicBattlesnakeListItem]| {
            snakes.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(&first), vec!["Snake 00", "Snake 01"]);
        assert_eq!(names(&second), vec!["Snake 02", "Snake 03"]);
        assert_eq!(names(&third), vec!["Snake 04"]);

        // Past the final page there is simply nothing left.
        assert!(
            get_public_battlesnakes_paginated(&pool, 3, 2)
                .await?
                .is_empty()
        );

        Ok(())
    }
}
