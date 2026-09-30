use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Type};
use std::collections::HashSet;
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

/// Region where the engine should measure snake requests. West calls directly.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[sqlx(type_name = "text")]
pub enum EngineRegion {
    #[default]
    #[serde(rename = "us-west1")]
    #[sqlx(rename = "us-west1")]
    UsWest1,
    #[serde(rename = "us-east4")]
    #[sqlx(rename = "us-east4")]
    UsEast4,
    #[serde(rename = "europe-west4")]
    #[sqlx(rename = "europe-west4")]
    EuropeWest4,
}

impl EngineRegion {
    pub const ALL: [Self; 3] = [Self::UsWest1, Self::UsEast4, Self::EuropeWest4];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UsWest1 => "us-west1",
            Self::UsEast4 => "us-east4",
            Self::EuropeWest4 => "europe-west4",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::UsWest1 => "US-West (Oregon)",
            Self::UsEast4 => "US-East (Virginia)",
            Self::EuropeWest4 => "Europe (Netherlands)",
        }
    }
}

impl FromStr for EngineRegion {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|region| region.as_str() == s)
            .ok_or_else(|| color_eyre::eyre::eyre!("Invalid engine region: {s}"))
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
    pub engine_region: EngineRegion,
    pub color: String,
    pub head: String,
    pub tail: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Return the requested snakes that are selectable by this user. Missing,
/// deleted, and another user's private snakes are intentionally indistinguishable.
pub async fn eligible_battlesnake_ids(
    pool: &PgPool,
    requester: Uuid,
    requested: &[Uuid],
) -> cja::Result<HashSet<Uuid>> {
    let rows = sqlx::query!(
        r#"SELECT battlesnake_id FROM battlesnakes
           WHERE battlesnake_id = ANY($1)
             AND (user_id = $2 OR visibility = 'public')
             AND deleted_at IS NULL
           ORDER BY battlesnake_id"#,
        requested,
        requester
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to validate battlesnake eligibility")?;
    Ok(rows.into_iter().map(|row| row.battlesnake_id).collect())
}

/// Transactional eligibility check. The deterministic shared locks prevent a
/// visibility change or deletion racing game creation.
pub async fn lock_eligible_battlesnake_ids(
    conn: &mut PgConnection,
    requester: Uuid,
    requested: &[Uuid],
) -> cja::Result<HashSet<Uuid>> {
    let rows = sqlx::query!(
        r#"SELECT battlesnake_id FROM battlesnakes
           WHERE battlesnake_id = ANY($1)
             AND (user_id = $2 OR visibility = 'public')
             AND deleted_at IS NULL
           ORDER BY battlesnake_id
           FOR SHARE"#,
        requested,
        requester
    )
    .fetch_all(conn)
    .await
    .wrap_err("Failed to lock battlesnake eligibility")?;
    Ok(rows.into_iter().map(|row| row.battlesnake_id).collect())
}

/// Validate that a snake URL parses and uses http or https. Shared by the
/// web form (after hostname normalization) and the JSON API.
pub fn validate_url(url: &str) -> Result<(), &'static str> {
    match url::Url::parse(url) {
        Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => Ok(()),
        Ok(_) => Err("URL must use HTTP or HTTPS scheme"),
        Err(_) => Err("Invalid URL format"),
    }
}

/// Maximum length of a battlesnake name, in characters.
pub const MAX_NAME_LEN: usize = 64;

/// Validate and normalize a user-supplied battlesnake name.
///
/// Trims surrounding whitespace, rejects empty names, and enforces
/// [`MAX_NAME_LEN`]. Shared by the web form and the JSON API so both
/// surfaces accept exactly the same names.
pub fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Name is required".to_string());
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(format!("Name must be {MAX_NAME_LEN} characters or fewer"));
    }
    Ok(trimmed.to_string())
}

// For creating a new battlesnake
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateBattlesnake {
    pub name: String,
    pub url: String,
    pub visibility: Visibility,
    #[serde(default)]
    pub engine_region: EngineRegion,
}

// For updating an existing battlesnake
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpdateBattlesnake {
    pub name: String,
    pub url: String,
    pub visibility: Visibility,
    #[serde(default)]
    pub engine_region: Option<EngineRegion>,
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
            engine_region as "engine_region: EngineRegion",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE user_id = $1
          AND deleted_at IS NULL
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
            engine_region as "engine_region: EngineRegion",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE battlesnake_id = $1
          AND deleted_at IS NULL
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
            visibility,
            engine_region
        )
        VALUES ($1, $2, $3, $4, $5)
        RETURNING
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            engine_region as "engine_region: EngineRegion",
            color,
            head,
            tail,
            created_at,
            updated_at
        "#,
        user_id,
        data.name,
        data.url,
        visibility_str,
        data.engine_region.as_str()
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
            visibility = $5,
            engine_region = COALESCE($6, engine_region)
        WHERE
            battlesnake_id = $1
            AND user_id = $2
            AND deleted_at IS NULL
        RETURNING
            battlesnake_id,
            user_id,
            name,
            url,
            visibility as "visibility: Visibility",
            engine_region as "engine_region: EngineRegion",
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
        visibility_str,
        data.engine_region.map(EngineRegion::as_str)
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

/// `leaderboard_entries.disabled_reason` for entries of a deleted snake.
/// Nothing re-enables these: health reactivation only touches `health`
/// entries, and owner pause/resume refuses them.
pub const DISABLED_REASON_DELETED: &str = "deleted";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBattlesnakeOutcome {
    Deleted,
    /// No live snake with this ID belongs to the user.
    NotFound,
    /// Registered in a tournament that is open for registration or running;
    /// the owner has to withdraw it first.
    InActiveTournament,
}

/// Soft-delete a battlesnake so the games, placements and tournament results
/// it took part in stay intact for everyone else.
///
/// In one transaction: refuses if the snake is in an active tournament,
/// withdraws it from not-yet-opened (`created`) tournaments, stamps
/// `deleted_at`, and disables its leaderboard entries so matchmaking and the
/// rankings drop it.
pub async fn delete_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    user_id: Uuid,
) -> cja::Result<DeleteBattlesnakeOutcome> {
    use crate::models::tournament::{self, TournamentStatus};

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin delete transaction")?;

    // Row lock serializes against tournament registration, which takes a
    // FOR SHARE lock on the snake before inserting.
    let locked = sqlx::query_scalar!(
        r#"
        SELECT battlesnake_id
        FROM battlesnakes
        WHERE battlesnake_id = $1
          AND user_id = $2
          AND deleted_at IS NULL
        FOR UPDATE
        "#,
        battlesnake_id,
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to lock battlesnake for deletion")?;
    if locked.is_none() {
        return Ok(DeleteBattlesnakeOutcome::NotFound);
    }

    if tournament::count_active_tournament_registrations(&mut *tx, battlesnake_id).await? > 0 {
        return Ok(DeleteBattlesnakeOutcome::InActiveTournament);
    }

    let pending = sqlx::query!(
        r#"
        SELECT tr.registration_id, tr.tournament_id
        FROM tournament_registrations tr
        JOIN tournaments t ON t.tournament_id = tr.tournament_id
        WHERE tr.battlesnake_id = $1
          AND t.status = 'created'
        ORDER BY tr.tournament_id
        "#,
        battlesnake_id
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to find registrations in unopened tournaments")?;

    for registration in pending {
        // The status may have moved on since the read above; re-check it on
        // the locked row.
        let status = tournament::get_tournament_for_update(&mut tx, registration.tournament_id)
            .await?
            .map(|t| t.status);
        match status {
            Some(TournamentStatus::Created) => {
                tournament::delete_registration_and_renumber(
                    &mut tx,
                    registration.tournament_id,
                    registration.registration_id,
                )
                .await?;
            }
            Some(TournamentStatus::Registration | TournamentStatus::InProgress) => {
                return Ok(DeleteBattlesnakeOutcome::InActiveTournament);
            }
            Some(TournamentStatus::Completed | TournamentStatus::Canceled) | None => {}
        }
    }

    sqlx::query!(
        "UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1",
        battlesnake_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to mark battlesnake deleted")?;

    sqlx::query!(
        r#"
        UPDATE leaderboard_entries
        SET disabled_at = COALESCE(disabled_at, NOW()),
            disabled_reason = $2,
            updated_at = NOW()
        WHERE battlesnake_id = $1
        "#,
        battlesnake_id,
        DISABLED_REASON_DELETED
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to disable leaderboard entries of deleted battlesnake")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit battlesnake deletion")?;

    Ok(DeleteBattlesnakeOutcome::Deleted)
}

/// Name of a soft-deleted battlesnake, for rendering its "deleted" page.
/// `None` when the snake is live or never existed.
pub async fn get_deleted_battlesnake_name(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Option<String>> {
    sqlx::query_scalar!(
        r#"
        SELECT name
        FROM battlesnakes
        WHERE battlesnake_id = $1
          AND deleted_at IS NOT NULL
        "#,
        battlesnake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch deleted battlesnake")
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
                AND deleted_at IS NULL
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
            engine_region as "engine_region: EngineRegion",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE visibility = 'public'
          AND deleted_at IS NULL
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

#[derive(Debug, Clone)]
pub struct PublicBattlesnakeQuery<'a> {
    pub search: &'a str,
    pub excluded_owner_id: Option<Uuid>,
    pub page: i64,
    pub per_page: i64,
}

// Count public snakes matching a literal, case-insensitive name or owner
// substring. An empty search matches every public snake.
pub async fn count_public_battlesnakes(
    pool: &PgPool,
    query: &PublicBattlesnakeQuery<'_>,
) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM battlesnakes b
        JOIN users u ON b.user_id = u.user_id
        WHERE b.visibility = 'public'
          AND b.deleted_at IS NULL
          AND ($2::uuid IS NULL OR b.user_id != $2)
          AND (strpos(lower(b.name), lower($1)) > 0
               OR strpos(lower(u.github_login), lower($1)) > 0)
        "#,
        query.search,
        query.excluded_owner_id
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
    query: &PublicBattlesnakeQuery<'_>,
) -> cja::Result<Vec<PublicBattlesnakeListItem>> {
    let offset = query.page * query.per_page;

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
          AND b.deleted_at IS NULL
          AND ($4::uuid IS NULL OR b.user_id != $4)
          AND (strpos(lower(b.name), lower($3)) > 0
               OR strpos(lower(u.github_login), lower($3)) > 0)
        ORDER BY b.name ASC, b.battlesnake_id ASC
        LIMIT $1 OFFSET $2
        "#,
        query.per_page,
        offset,
        query.search,
        query.excluded_owner_id
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
            engine_region as "engine_region: EngineRegion",
            color,
            head,
            tail,
            created_at,
            updated_at
        FROM battlesnakes
        WHERE (user_id = $1 OR visibility = 'public')
          AND deleted_at IS NULL
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
          AND deleted_at IS NULL
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

    #[test]
    fn validate_url_accepts_http_and_https() {
        assert!(validate_url("https://snake.example.com/v1").is_ok());
        assert!(validate_url("http://localhost:8000").is_ok());
    }

    #[test]
    fn validate_url_rejects_garbage_and_other_schemes() {
        assert_eq!(
            validate_url("https://not a url").unwrap_err(),
            "Invalid URL format"
        );
        assert_eq!(validate_url("not a url").unwrap_err(), "Invalid URL format");
        assert_eq!(
            validate_url("ftp://snake.example.com").unwrap_err(),
            "URL must use HTTP or HTTPS scheme"
        );
    }

    #[test]
    fn validate_name_trims_and_accepts() {
        assert_eq!(validate_name("  Bob  ").unwrap(), "Bob");
        assert_eq!(
            validate_name(&"x".repeat(MAX_NAME_LEN)).unwrap().len(),
            MAX_NAME_LEN
        );
    }

    #[test]
    fn validate_name_rejects_empty_and_whitespace() {
        assert_eq!(validate_name("").unwrap_err(), "Name is required");
        assert_eq!(validate_name("   ").unwrap_err(), "Name is required");
    }

    #[test]
    fn validate_name_rejects_overlong() {
        let err = validate_name(&"x".repeat(MAX_NAME_LEN + 1)).unwrap_err();
        assert!(err.contains("64 characters"), "{err}");
        // Multi-byte characters count as one character each.
        assert!(validate_name(&"é".repeat(MAX_NAME_LEN)).is_ok());
    }

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

        let query = PublicBattlesnakeQuery {
            search: "",
            excluded_owner_id: None,
            page: 0,
            per_page: 50,
        };
        assert_eq!(count_public_battlesnakes(&pool, &query).await?, 2);

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

        let query = PublicBattlesnakeQuery {
            search: "",
            excluded_owner_id: None,
            page: 0,
            per_page: 50,
        };
        let snakes = get_public_battlesnakes_paginated(&pool, &query).await?;

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

        let page = |page| PublicBattlesnakeQuery {
            search: "",
            excluded_owner_id: None,
            page,
            per_page: 2,
        };
        let first = get_public_battlesnakes_paginated(&pool, &page(0)).await?;
        let second = get_public_battlesnakes_paginated(&pool, &page(1)).await?;
        let third = get_public_battlesnakes_paginated(&pool, &page(2)).await?;

        let names = |snakes: &[PublicBattlesnakeListItem]| {
            snakes.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(&first), vec!["Snake 00", "Snake 01"]);
        assert_eq!(names(&second), vec!["Snake 02", "Snake 03"]);
        assert_eq!(names(&third), vec!["Snake 04"]);

        // Past the final page there is simply nothing left.
        assert!(
            get_public_battlesnakes_paginated(&pool, &page(3))
                .await?
                .is_empty()
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn public_query_searches_name_and_owner_and_excludes_owner(
        pool: PgPool,
    ) -> cja::Result<()> {
        let excluded = create_user(&pool, 8301, "excluded-owner").await?;
        let included = create_user(&pool, 8302, "NeedleOwner").await?;
        create_snake(&pool, excluded, "Needle Snake", Visibility::Public).await?;
        create_snake(&pool, included, "Ordinary", Visibility::Public).await?;
        create_snake(&pool, included, "Needle Hidden", Visibility::Private).await?;

        let query = PublicBattlesnakeQuery {
            search: "needle",
            excluded_owner_id: Some(excluded),
            page: 0,
            per_page: 10,
        };
        assert_eq!(count_public_battlesnakes(&pool, &query).await?, 1);
        let snakes = get_public_battlesnakes_paginated(&pool, &query).await?;
        assert_eq!(snakes.len(), 1);
        assert_eq!(snakes[0].name, "Ordinary");
        assert_eq!(snakes[0].owner_login, "NeedleOwner");

        let literal = PublicBattlesnakeQuery {
            search: "%_",
            excluded_owner_id: None,
            page: 0,
            per_page: 10,
        };
        assert_eq!(count_public_battlesnakes(&pool, &literal).await?, 0);
        Ok(())
    }

    async fn create_leaderboard_entry(
        pool: &PgPool,
        leaderboard_id: Uuid,
        battlesnake_id: Uuid,
    ) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar!(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
             VALUES ($1, $2)
             RETURNING leaderboard_entry_id",
            leaderboard_id,
            battlesnake_id
        )
        .fetch_one(pool)
        .await?)
    }

    async fn create_finished_game(pool: &PgPool) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar!(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'Standard', 'finished')
             RETURNING game_id"
        )
        .fetch_one(pool)
        .await?)
    }

    async fn create_tournament_in(pool: &PgPool, owner: Uuid, status: &str) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar!(
            "INSERT INTO tournaments (name, user_id, status)
             VALUES ('Delete Test', $1, $2)
             RETURNING tournament_id",
            owner,
            status
        )
        .fetch_one(pool)
        .await?)
    }

    async fn register(
        pool: &PgPool,
        tournament_id: Uuid,
        battlesnake_id: Uuid,
        user_id: Uuid,
        seed: i32,
    ) -> cja::Result<()> {
        crate::models::tournament::create_registration(
            pool,
            tournament_id,
            battlesnake_id,
            user_id,
            seed,
        )
        .await?;
        Ok(())
    }

    async fn entry_state(pool: &PgPool, entry_id: Uuid) -> cja::Result<(bool, Option<String>)> {
        let row = sqlx::query!(
            "SELECT disabled_at IS NOT NULL AS \"disabled!\", disabled_reason
             FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
            entry_id
        )
        .fetch_one(pool)
        .await?;
        Ok((row.disabled, row.disabled_reason))
    }

    // Regression: deleting a snake that had played a leaderboard game used to
    // 500 on game_battlesnakes_snake_or_entry_required. Soft delete keeps every
    // game row, so other players' results (including who won) are untouched.
    #[sqlx::test(migrations = "../migrations")]
    async fn delete_battlesnake_keeps_leaderboard_and_casual_game_history(
        pool: PgPool,
    ) -> cja::Result<()> {
        use crate::models::game::{add_battlesnake_to_game, add_leaderboard_entry_to_game};
        use crate::models::game_battlesnake::{AddBattlesnakeToGame, get_battlesnakes_by_game_id};

        let owner = create_user(&pool, 8401, "delete-owner").await?;
        let rival_owner = create_user(&pool, 8402, "rival-owner").await?;
        let doomed = create_snake(&pool, owner, "Doomed", Visibility::Public).await?;
        let rival = create_snake(&pool, rival_owner, "Rival", Visibility::Public).await?;

        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('Delete Test') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let doomed_entry = create_leaderboard_entry(&pool, leaderboard_id, doomed).await?;
        let rival_entry = create_leaderboard_entry(&pool, leaderboard_id, rival).await?;

        let leaderboard_game = create_finished_game(&pool).await?;
        let casual_game = create_finished_game(&pool).await?;
        for entry_id in [doomed_entry, rival_entry] {
            add_leaderboard_entry_to_game(&pool, leaderboard_game, entry_id).await?;
        }
        for battlesnake_id in [doomed, rival] {
            add_battlesnake_to_game(&pool, casual_game, AddBattlesnakeToGame { battlesnake_id })
                .await?;
        }
        // The soon-to-be-deleted snake won both games.
        sqlx::query!(
            "UPDATE game_battlesnakes gb
             SET placement = CASE WHEN COALESCE(
                 gb.battlesnake_id,
                 (SELECT le.battlesnake_id FROM leaderboard_entries le
                  WHERE le.leaderboard_entry_id = gb.leaderboard_entry_id)
             ) = $1 THEN 1 ELSE 2 END",
            doomed
        )
        .execute(&pool)
        .await?;

        assert_eq!(
            delete_battlesnake(&pool, doomed, owner).await?,
            DeleteBattlesnakeOutcome::Deleted
        );

        for game_id in [leaderboard_game, casual_game] {
            let results: Vec<(Uuid, Option<i32>)> = get_battlesnakes_by_game_id(&pool, game_id)
                .await?
                .into_iter()
                .map(|gb| (gb.battlesnake_id, gb.placement))
                .collect();
            assert_eq!(results, vec![(doomed, Some(1)), (rival, Some(2))]);
        }

        assert!(get_battlesnake_by_id(&pool, doomed).await?.is_none());
        assert_eq!(
            get_deleted_battlesnake_name(&pool, doomed)
                .await?
                .as_deref(),
            Some("Doomed")
        );
        assert_eq!(get_deleted_battlesnake_name(&pool, rival).await?, None);

        // Out of matchmaking and the rankings; the rival is unaffected.
        assert_eq!(
            entry_state(&pool, doomed_entry).await?,
            (true, Some(DISABLED_REASON_DELETED.to_string()))
        );
        assert_eq!(entry_state(&pool, rival_entry).await?, (false, None));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_battlesnake_is_hidden_from_active_queries(pool: PgPool) -> cja::Result<()> {
        let owner = create_user(&pool, 8501, "hidden-owner").await?;
        let other = create_user(&pool, 8502, "hidden-other").await?;
        let live = create_snake(&pool, owner, "Live", Visibility::Public).await?;
        let gone = create_snake(&pool, owner, "Gone", Visibility::Public).await?;
        assert_eq!(
            delete_battlesnake(&pool, gone, owner).await?,
            DeleteBattlesnakeOutcome::Deleted
        );

        let ids = |snakes: Vec<Battlesnake>| {
            snakes
                .into_iter()
                .map(|s| s.battlesnake_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(get_battlesnakes_by_user_id(&pool, owner).await?),
            vec![live]
        );
        assert_eq!(ids(get_public_battlesnakes(&pool).await?), vec![live]);
        assert_eq!(
            ids(get_available_battlesnakes(&pool, other).await?),
            vec![live]
        );

        let query = PublicBattlesnakeQuery {
            search: "",
            excluded_owner_id: None,
            page: 0,
            per_page: 50,
        };
        assert_eq!(count_public_battlesnakes(&pool, &query).await?, 1);
        assert_eq!(
            get_public_battlesnakes_paginated(&pool, &query)
                .await?
                .len(),
            1
        );

        assert_eq!(
            eligible_battlesnake_ids(&pool, owner, &[live, gone]).await?,
            HashSet::from([live])
        );
        let mut conn = pool.acquire().await?;
        assert_eq!(
            lock_eligible_battlesnake_ids(&mut conn, owner, &[live, gone]).await?,
            HashSet::from([live])
        );
        drop(conn);

        assert!(!belongs_to_user(&pool, gone, owner).await?);
        assert!(
            update_battlesnake(
                &pool,
                gone,
                owner,
                UpdateBattlesnake {
                    engine_region: None,
                    name: "Revived".to_string(),
                    url: "http://localhost:8000".to_string(),
                    visibility: Visibility::Public,
                },
            )
            .await
            .is_err()
        );

        // Deleting twice is a not-found, not a second delete.
        assert_eq!(
            delete_battlesnake(&pool, gone, owner).await?,
            DeleteBattlesnakeOutcome::NotFound
        );
        // Only the owner can delete.
        assert_eq!(
            delete_battlesnake(&pool, live, other).await?,
            DeleteBattlesnakeOutcome::NotFound
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_battlesnake_name_can_be_reused(pool: PgPool) -> cja::Result<()> {
        let owner = create_user(&pool, 8601, "reuse-owner").await?;
        let first = create_snake(&pool, owner, "Reuse", Visibility::Public).await?;
        delete_battlesnake(&pool, first, owner).await?;

        let data = CreateBattlesnake {
            name: "Reuse".to_string(),
            url: "http://localhost:8000".to_string(),
            visibility: Visibility::Public,
            engine_region: EngineRegion::UsWest1,
        };
        let second = create_battlesnake(&pool, owner, data.clone()).await?;
        assert_ne!(second.battlesnake_id, first);

        // Live snakes still can't share a name.
        let err = create_battlesnake(&pool, owner, data).await.unwrap_err();
        assert!(
            err.to_string().contains("already have a battlesnake named"),
            "{err}"
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn delete_refuses_active_tournaments_and_withdraws_from_unopened_ones(
        pool: PgPool,
    ) -> cja::Result<()> {
        let owner = create_user(&pool, 8701, "tourney-owner").await?;
        let other = create_user(&pool, 8702, "tourney-other").await?;
        let doomed = create_snake(&pool, owner, "Doomed", Visibility::Public).await?;
        let a = create_snake(&pool, other, "A", Visibility::Public).await?;
        let b = create_snake(&pool, other, "B", Visibility::Public).await?;

        // Open for registration: refused, nothing changes.
        let open = create_tournament_in(&pool, other, "registration").await?;
        register(&pool, open, doomed, owner, 1).await?;
        assert_eq!(
            delete_battlesnake(&pool, doomed, owner).await?,
            DeleteBattlesnakeOutcome::InActiveTournament
        );
        assert!(get_battlesnake_by_id(&pool, doomed).await?.is_some());

        sqlx::query!(
            "UPDATE tournaments SET status = 'completed' WHERE tournament_id = $1",
            open
        )
        .execute(&pool)
        .await?;

        // Not opened yet: withdrawn, and the remaining seeds close the gap.
        let draft = create_tournament_in(&pool, other, "created").await?;
        register(&pool, draft, a, other, 1).await?;
        register(&pool, draft, doomed, owner, 2).await?;
        register(&pool, draft, b, other, 3).await?;
        assert_eq!(
            delete_battlesnake(&pool, doomed, owner).await?,
            DeleteBattlesnakeOutcome::Deleted
        );

        let seeds = sqlx::query!(
            "SELECT battlesnake_id, seed FROM tournament_registrations
             WHERE tournament_id = $1 ORDER BY seed",
            draft
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|r| (r.battlesnake_id, r.seed))
        .collect::<Vec<_>>();
        assert_eq!(seeds, vec![(a, 1), (b, 2)]);

        // The finished tournament keeps its history.
        let kept = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM tournament_registrations
             WHERE tournament_id = $1 AND battlesnake_id = $2",
            open,
            doomed
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(kept, 1);

        // And a deleted snake can't be registered anywhere.
        let later = create_tournament_in(&pool, other, "registration").await?;
        assert!(register(&pool, later, doomed, owner, 1).await.is_err());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_snake_leaderboard_entry_cannot_be_resumed(pool: PgPool) -> cja::Result<()> {
        let owner = create_user(&pool, 8801, "resume-owner").await?;
        let doomed = create_snake(&pool, owner, "Doomed", Visibility::Public).await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('Resume Test') RETURNING leaderboard_id"
        )
        .fetch_one(&pool)
        .await?;
        let entry = create_leaderboard_entry(&pool, leaderboard_id, doomed).await?;
        delete_battlesnake(&pool, doomed, owner).await?;

        crate::models::leaderboard::set_disabled(&pool, entry, None).await?;

        assert_eq!(
            entry_state(&pool, entry).await?,
            (true, Some(DISABLED_REASON_DELETED.to_string()))
        );

        Ok(())
    }
}
