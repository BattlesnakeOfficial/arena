use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Type};
use std::collections::HashSet;
use std::str::FromStr;
use uuid::Uuid;

// Which kind of cosmetic a catalog entry is. Colors are free-form hex on the
// battlesnake row, not catalog entries.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum CustomizationType {
    Head,
    Tail,
}

impl CustomizationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CustomizationType::Head => "head",
            CustomizationType::Tail => "tail",
        }
    }
}

impl FromStr for CustomizationType {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "head" => Ok(CustomizationType::Head),
            "tail" => Ok(CustomizationType::Tail),
            _ => Err(color_eyre::eyre::eyre!("Invalid customization type: {}", s)),
        }
    }
}

// Group availability controls whether a grant is required:
// - Everyone: free for all, no grant needed
// - Restricted: needs a grant unless cost = 0
// - Hidden: not shown in UI, grant-only
// - Preview: shown in UI, grant-only (not purchasable)
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    Everyone,
    Restricted,
    Hidden,
    Preview,
}

impl FromStr for Availability {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "everyone" => Ok(Availability::Everyone),
            "restricted" => Ok(Availability::Restricted),
            "hidden" => Ok(Availability::Hidden),
            "preview" => Ok(Availability::Preview),
            _ => Err(color_eyre::eyre::eyre!("Invalid availability: {}", s)),
        }
    }
}

/// The slug snakes fall back to when they declare a customization they can't
/// use (unknown, unreleased, or not granted). Matches play's behavior.
pub const DEFAULT_SLUG: &str = "default";

/// One row of the browsable catalog, joined with its group.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CatalogEntry {
    pub customization_id: Uuid,
    pub customization_type: String,
    pub slug: String,
    pub display_name: String,
    pub description: String,
    pub image_url: String,
    pub cost: i32,
    pub group_slug: String,
    pub group_title: String,
    pub group_availability: String,
    pub group_ordinal: i32,
}

impl CatalogEntry {
    /// Usable by anyone without a grant.
    pub fn is_free(&self) -> bool {
        self.group_availability == "everyone"
            || (self.group_availability == "restricted" && self.cost == 0)
    }
}

/// Fetch the released, non-hidden catalog for the browse page, ordered by
/// group ordinal then name.
pub async fn get_visible_catalog(pool: &PgPool) -> cja::Result<Vec<CatalogEntry>> {
    let entries = sqlx::query_as!(
        CatalogEntry,
        r#"
        SELECT
            c.customization_id,
            c.customization_type,
            c.slug,
            c.display_name,
            c.description,
            c.image_url,
            c.cost,
            g.slug as group_slug,
            g.title as group_title,
            g.availability as group_availability,
            g.ordinal as group_ordinal
        FROM customizations c
        JOIN customization_groups g ON g.customization_group_id = c.customization_group_id
        WHERE g.availability IN ('everyone', 'restricted', 'preview')
          AND (c.release_date IS NULL OR c.release_date <= NOW())
        ORDER BY g.ordinal, g.title, c.customization_type, c.display_name
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch customization catalog")?;

    Ok(entries)
}

/// IDs of every customization the user holds a grant for.
pub async fn get_granted_customization_ids(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<HashSet<Uuid>> {
    let rows = sqlx::query!(
        r#"
        SELECT customization_id
        FROM customization_grants
        WHERE user_id = $1
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch customization grants")?;

    Ok(rows.into_iter().map(|r| r.customization_id).collect())
}

/// Grant a customization to a user. Idempotent — used by the account
/// importer and admin tooling.
pub async fn create_grant(pool: &PgPool, user_id: Uuid, customization_id: Uuid) -> cja::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO customization_grants (user_id, customization_id)
        VALUES ($1, $2)
        ON CONFLICT (user_id, customization_id) DO NOTHING
        "#,
        user_id,
        customization_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to create customization grant")?;

    Ok(())
}

/// Whether `user_id` may use the given head/tail slug: it must exist in the
/// catalog, be released, and be free (everyone group, or restricted with
/// cost 0) or granted to the user.
pub async fn is_customization_allowed(
    pool: &PgPool,
    user_id: Uuid,
    customization_type: CustomizationType,
    slug: &str,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM customizations c
            JOIN customization_groups g
                ON g.customization_group_id = c.customization_group_id
            WHERE c.customization_type = $1
              AND c.slug = $2
              AND (c.release_date IS NULL OR c.release_date <= NOW())
              AND (
                  g.availability = 'everyone'
                  OR (g.availability = 'restricted' AND c.cost = 0)
                  OR EXISTS (
                      SELECT 1
                      FROM customization_grants cg
                      WHERE cg.customization_id = c.customization_id
                        AND cg.user_id = $3
                  )
              )
        ) as "allowed!"
        "#,
        customization_type.as_str(),
        slug,
        user_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check customization access")?;

    Ok(result.allowed)
}

/// Resolve a declared head or tail slug to what the snake is actually allowed
/// to wear. Empty declarations stay empty (the board falls back to its own
/// default rendering); disallowed or unknown slugs become `DEFAULT_SLUG`.
pub async fn resolve_customization(
    pool: &PgPool,
    user_id: Uuid,
    customization_type: CustomizationType,
    declared_slug: &str,
) -> cja::Result<String> {
    if declared_slug.is_empty() || declared_slug == DEFAULT_SLUG {
        return Ok(declared_slug.to_string());
    }

    if is_customization_allowed(pool, user_id, customization_type, declared_slug).await? {
        Ok(declared_slug.to_string())
    } else {
        Ok(DEFAULT_SLUG.to_string())
    }
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

    async fn customization_id_by_slug(
        pool: &PgPool,
        customization_type: CustomizationType,
        slug: &str,
    ) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "SELECT customization_id FROM customizations
             WHERE customization_type = $1 AND slug = $2",
            customization_type.as_str(),
            slug
        )
        .fetch_one(pool)
        .await?;
        Ok(row.customization_id)
    }

    // The seed migration loads play's real catalog, so these tests exercise
    // the actual data games will run against: 'beluga' is in the everyone
    // 'standard' group, 'alligator' is restricted at cost 100, and 'fish' is
    // in the hidden 'special-edition' group.

    #[sqlx::test(migrations = "../migrations")]
    async fn free_slugs_allowed_without_grant(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        let head = resolve_customization(&pool, user_id, CustomizationType::Head, "beluga").await?;
        assert_eq!(head, "beluga");

        // Empty declarations stay empty; explicit default stays default.
        let empty = resolve_customization(&pool, user_id, CustomizationType::Head, "").await?;
        assert_eq!(empty, "");
        let default =
            resolve_customization(&pool, user_id, CustomizationType::Tail, DEFAULT_SLUG).await?;
        assert_eq!(default, DEFAULT_SLUG);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn paid_slug_requires_grant(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        let without_grant =
            resolve_customization(&pool, user_id, CustomizationType::Head, "alligator").await?;
        assert_eq!(without_grant, DEFAULT_SLUG);

        let alligator =
            customization_id_by_slug(&pool, CustomizationType::Head, "alligator").await?;
        create_grant(&pool, user_id, alligator).await?;
        // Granting twice is a no-op, not an error.
        create_grant(&pool, user_id, alligator).await?;

        let with_grant =
            resolve_customization(&pool, user_id, CustomizationType::Head, "alligator").await?;
        assert_eq!(with_grant, "alligator");

        // The grant is per-type: the alligator TAIL is still locked.
        let tail =
            resolve_customization(&pool, user_id, CustomizationType::Tail, "alligator").await?;
        assert_eq!(tail, DEFAULT_SLUG);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn hidden_slug_usable_only_with_grant(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        let without_grant =
            resolve_customization(&pool, user_id, CustomizationType::Head, "fish").await?;
        assert_eq!(without_grant, DEFAULT_SLUG);

        let fish = customization_id_by_slug(&pool, CustomizationType::Head, "fish").await?;
        create_grant(&pool, user_id, fish).await?;

        let with_grant =
            resolve_customization(&pool, user_id, CustomizationType::Head, "fish").await?;
        assert_eq!(with_grant, "fish");

        // Hidden groups never show up in the browse catalog, granted or not.
        let catalog = get_visible_catalog(&pool).await?;
        assert!(catalog.iter().all(|e| e.group_slug != "special-edition"));

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unknown_slug_falls_back_to_default(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;

        let resolved =
            resolve_customization(&pool, user_id, CustomizationType::Head, "not-a-real-head")
                .await?;
        assert_eq!(resolved, DEFAULT_SLUG);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn granted_ids_round_trip(pool: PgPool) -> cja::Result<()> {
        let user_id = create_test_user(&pool).await?;
        assert!(
            get_granted_customization_ids(&pool, user_id)
                .await?
                .is_empty()
        );

        let alligator =
            customization_id_by_slug(&pool, CustomizationType::Head, "alligator").await?;
        create_grant(&pool, user_id, alligator).await?;

        let granted = get_granted_customization_ids(&pool, user_id).await?;
        assert_eq!(granted.len(), 1);
        assert!(granted.contains(&alligator));

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
}
