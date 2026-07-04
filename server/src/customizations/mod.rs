//! Snake customizations: a code-defined catalog (see [`catalog`]) plus
//! per-user ownership grants in the database.
//!
//! The catalog is static data that changes a few times a year, so it lives
//! in the type system — exhaustive matches, compile-time slug uniqueness,
//! no seed migrations. Grants are runtime state and stay in Postgres.

pub mod catalog;

use color_eyre::eyre::Context as _;
use sqlx::PgPool;
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
            Availability::Restricted => self.cost == 0,
            Availability::Hidden | Availability::Preview => false,
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

/// Grant a customization to a user. Idempotent — used by the play account
/// importer and admin tooling. The (kind, slug) pair is validated against
/// the catalog by the callers that accept external input.
pub async fn create_grant(
    pool: &PgPool,
    user_id: Uuid,
    customization_type: &str,
    slug: &str,
) -> cja::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO customization_grants (user_id, customization_type, slug)
        VALUES ($1, $2, $3)
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
        assert_eq!(Head::Alligator.def().cost, 100);
        assert!(!Head::Alligator.def().is_free());
        assert_eq!(
            Head::Beluga.image_url(),
            "https://media.battlesnake.com/snakes/heads/beluga.svg"
        );
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
}
