use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Type};
use std::collections::HashMap;
use uuid::Uuid;

/// Hard cap on tags per snake, enforced on save.
pub const MAX_TAGS_PER_SNAKE: usize = 5;

// Category enum for the curated tag catalog. Backed by a TEXT column with a
// CHECK constraint mirroring these variants.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum TagCategory {
    Language,
    Platform,
}

// A curated tag from the moderated catalog. Users can only pick from this
// set — new tags are added by request (and a migration), not free text.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Tag {
    pub tag_id: Uuid,
    pub name: String,
    pub category: TagCategory,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// The full catalog, grouped by category for form rendering
#[derive(Debug)]
pub struct TagCatalog {
    pub languages: Vec<Tag>,
    pub platforms: Vec<Tag>,
}

// Get the full tag catalog, grouped by category (each group sorted by name)
pub async fn get_tag_catalog(pool: &PgPool) -> cja::Result<TagCatalog> {
    let tags = sqlx::query_as!(
        Tag,
        r#"
        SELECT
            tag_id,
            name,
            category as "category: TagCategory",
            created_at,
            updated_at
        FROM tags
        ORDER BY name ASC
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch tag catalog from database")?;

    let (languages, platforms) = tags
        .into_iter()
        .partition(|tag| tag.category == TagCategory::Language);

    Ok(TagCatalog {
        languages,
        platforms,
    })
}

// Get the tags attached to a battlesnake (languages first, then platforms,
// each sorted by name)
pub async fn get_tags_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
) -> cja::Result<Vec<Tag>> {
    let tags = sqlx::query_as!(
        Tag,
        r#"
        SELECT
            t.tag_id,
            t.name,
            t.category as "category: TagCategory",
            t.created_at,
            t.updated_at
        FROM tags t
        JOIN battlesnake_tags bt ON bt.tag_id = t.tag_id
        WHERE bt.battlesnake_id = $1
        ORDER BY t.category ASC, t.name ASC
        "#,
        battlesnake_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch battlesnake tags from database")?;

    Ok(tags)
}

// Batched variant for list pages: fetch the tags for many snakes in one
// query, grouped by battlesnake ID. Snakes with no tags are simply absent
// from the map — callers should treat a missing key as an empty set.
pub async fn get_tags_for_battlesnakes(
    pool: &PgPool,
    battlesnake_ids: &[Uuid],
) -> cja::Result<HashMap<Uuid, Vec<Tag>>> {
    if battlesnake_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query!(
        r#"
        SELECT
            bt.battlesnake_id as "battlesnake_id!",
            t.tag_id,
            t.name,
            t.category as "category: TagCategory",
            t.created_at,
            t.updated_at
        FROM battlesnake_tags bt
        JOIN tags t ON t.tag_id = bt.tag_id
        WHERE bt.battlesnake_id = ANY($1)
        ORDER BY bt.battlesnake_id, t.category ASC, t.name ASC
        "#,
        battlesnake_ids as &[Uuid],
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch battlesnake tags from database")?;

    let mut map: HashMap<Uuid, Vec<Tag>> = HashMap::new();
    for r in rows {
        map.entry(r.battlesnake_id).or_default().push(Tag {
            tag_id: r.tag_id,
            name: r.name,
            category: r.category,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });
    }
    Ok(map)
}

// Replace a battlesnake's tags with the given set (deduplicated), inside a
// transaction. Enforces the MAX_TAGS_PER_SNAKE cap; unknown tag ids are
// rejected by the foreign key.
pub async fn set_tags_for_battlesnake(
    pool: &PgPool,
    battlesnake_id: Uuid,
    tag_ids: &[Uuid],
) -> cja::Result<()> {
    let mut deduped = tag_ids.to_vec();
    deduped.sort_unstable();
    deduped.dedup();

    if deduped.len() > MAX_TAGS_PER_SNAKE {
        return Err(cja::color_eyre::eyre::eyre!(
            "A battlesnake can have at most {MAX_TAGS_PER_SNAKE} tags"
        ));
    }

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin transaction for tag update")?;

    sqlx::query!(
        r#"
        DELETE FROM battlesnake_tags
        WHERE battlesnake_id = $1
        "#,
        battlesnake_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to clear existing battlesnake tags")?;

    if !deduped.is_empty() {
        sqlx::query!(
            r#"
            INSERT INTO battlesnake_tags (battlesnake_id, tag_id)
            SELECT $1, tag_id
            FROM UNNEST($2::uuid[]) AS t(tag_id)
            "#,
            battlesnake_id,
            &deduped
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to insert battlesnake tags")?;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit tag update transaction")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_snake(pool: &PgPool) -> cja::Result<Uuid> {
        create_snake_as(pool, 4242, "tag-test-user").await
    }

    async fn create_snake_as(pool: &PgPool, github_id: i64, login: &str) -> cja::Result<Uuid> {
        let user = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
             RETURNING user_id",
            github_id,
            login
        )
        .fetch_one(pool)
        .await?;

        let row = sqlx::query!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'Tag Test Snake', 'http://localhost:8000')
             RETURNING battlesnake_id",
            user.user_id
        )
        .fetch_one(pool)
        .await?;

        Ok(row.battlesnake_id)
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn catalog_is_seeded_and_grouped(pool: PgPool) -> cja::Result<()> {
        let catalog = get_tag_catalog(&pool).await?;

        assert_eq!(catalog.languages.len(), 10);
        assert_eq!(catalog.platforms.len(), 10);
        assert!(
            catalog
                .languages
                .iter()
                .all(|t| t.category == TagCategory::Language)
        );
        assert!(
            catalog
                .platforms
                .iter()
                .all(|t| t.category == TagCategory::Platform)
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn batch_fetch_groups_by_snake_and_matches_single(pool: PgPool) -> cja::Result<()> {
        let snake_a = create_snake(&pool).await?;
        let snake_b = create_snake_as(&pool, 4243, "tag-test-user-b").await?;
        let catalog = get_tag_catalog(&pool).await?;

        // Snake A gets two tags, snake B gets one different tag.
        let a_tags: Vec<Uuid> = vec![catalog.languages[0].tag_id, catalog.platforms[0].tag_id];
        let b_tags: Vec<Uuid> = vec![catalog.languages[1].tag_id];
        set_tags_for_battlesnake(&pool, snake_a, &a_tags).await?;
        set_tags_for_battlesnake(&pool, snake_b, &b_tags).await?;

        let map = get_tags_for_battlesnakes(&pool, &[snake_a, snake_b]).await?;
        assert_eq!(map[&snake_a].len(), 2);
        assert_eq!(map[&snake_b].len(), 1);
        assert_eq!(map[&snake_b][0].tag_id, catalog.languages[1].tag_id);

        // Batch ordering matches the single-snake query (category, then name)
        let single = get_tags_for_battlesnake(&pool, snake_a).await?;
        let batch: Vec<Uuid> = map[&snake_a].iter().map(|t| t.tag_id).collect();
        let single_ids: Vec<Uuid> = single.iter().map(|t| t.tag_id).collect();
        assert_eq!(batch, single_ids);

        // Empty input list short-circuits to an empty map
        assert!(get_tags_for_battlesnakes(&pool, &[]).await?.is_empty());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn set_tags_enforces_max(pool: PgPool) -> cja::Result<()> {
        let snake_id = create_snake(&pool).await?;
        let catalog = get_tag_catalog(&pool).await?;

        // Six tags is over the cap — rejected, and nothing is written
        let six: Vec<Uuid> = catalog.languages.iter().take(6).map(|t| t.tag_id).collect();
        assert!(
            set_tags_for_battlesnake(&pool, snake_id, &six)
                .await
                .is_err()
        );
        assert!(get_tags_for_battlesnake(&pool, snake_id).await?.is_empty());

        // Five is fine, even mixing categories
        let five: Vec<Uuid> = catalog
            .languages
            .iter()
            .take(3)
            .chain(catalog.platforms.iter().take(2))
            .map(|t| t.tag_id)
            .collect();
        set_tags_for_battlesnake(&pool, snake_id, &five).await?;
        assert_eq!(get_tags_for_battlesnake(&pool, snake_id).await?.len(), 5);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn set_tags_replaces_existing(pool: PgPool) -> cja::Result<()> {
        let snake_id = create_snake(&pool).await?;
        let catalog = get_tag_catalog(&pool).await?;

        // Two languages in the same category is legit
        let first: Vec<Uuid> = catalog.languages.iter().take(2).map(|t| t.tag_id).collect();
        set_tags_for_battlesnake(&pool, snake_id, &first).await?;
        assert_eq!(get_tags_for_battlesnake(&pool, snake_id).await?.len(), 2);

        // A second save replaces the whole set, not appends
        let second: Vec<Uuid> = catalog.platforms.iter().take(3).map(|t| t.tag_id).collect();
        set_tags_for_battlesnake(&pool, snake_id, &second).await?;

        let tags = get_tags_for_battlesnake(&pool, snake_id).await?;
        assert_eq!(tags.len(), 3);
        let mut expected: Vec<Uuid> = second.clone();
        expected.sort_unstable();
        let mut actual: Vec<Uuid> = tags.iter().map(|t| t.tag_id).collect();
        actual.sort_unstable();
        assert_eq!(actual, expected);

        // An empty save clears everything
        set_tags_for_battlesnake(&pool, snake_id, &[]).await?;
        assert!(get_tags_for_battlesnake(&pool, snake_id).await?.is_empty());

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn set_tags_deduplicates_input(pool: PgPool) -> cja::Result<()> {
        let snake_id = create_snake(&pool).await?;
        let catalog = get_tag_catalog(&pool).await?;

        let tag_id = catalog.languages[0].tag_id;
        set_tags_for_battlesnake(&pool, snake_id, &[tag_id, tag_id, tag_id]).await?;
        assert_eq!(get_tags_for_battlesnake(&pool, snake_id).await?.len(), 1);

        Ok(())
    }
}
