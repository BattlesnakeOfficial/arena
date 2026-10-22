use std::collections::{BTreeSet, HashSet};

use color_eyre::eyre::Context as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Executor, FromRow, PgPool, Postgres};
use uuid::Uuid;

pub use arena::api_scope::{Permission, SnakeAccess};

/// API token stored in the database (hashed)
#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct ApiToken {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String,
    pub name: String,
    pub snake_access: SnakeAccess,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A token's scope, loaded at auth time: permission set + snake access.
/// Doubles as the model's decoded scope; the OAuth client reference marks
/// which special rules apply (OAuth `snake.delete` scope, creator stamping).
#[derive(Debug, Clone)]
pub struct TokenAuth {
    pub token_id: Uuid,
    pub user_id: Uuid,
    pub permissions: BTreeSet<Permission>,
    pub snake_access: SnakeAccess,
    pub snake_ids: HashSet<Uuid>,
    /// The OAuth client this token acts for (its DB `id`); `None` for
    /// session/personal tokens.
    pub oauth_client_id: Option<Uuid>,
}

impl TokenAuth {
    /// Is a token with this scope no wider than `self`? Permissions must be a
    /// subset, and snake access no wider: `all` only from `all`; `selected`
    /// from `all`, or from `selected` when every snake is in this list.
    ///
    /// Gates minting a token (`POST /api/tokens`) and revoking one
    /// (`DELETE /api/tokens/{id}`): a token never acts on a wider one.
    pub fn covers(
        &self,
        permissions: &BTreeSet<Permission>,
        snake_access: SnakeAccess,
        snake_ids: &BTreeSet<Uuid>,
    ) -> bool {
        let access_ok = match (self.snake_access, snake_access) {
            (SnakeAccess::All, _) => true,
            (SnakeAccess::Selected, SnakeAccess::All) => false,
            (SnakeAccess::Selected, SnakeAccess::Selected) => {
                snake_ids.iter().all(|id| self.snake_ids.contains(id))
            }
        };
        access_ok && permissions.is_subset(&self.permissions)
    }
}

/// Result of creating a new token - includes the raw secret (only shown once)
pub struct NewApiToken {
    pub token: ApiToken,
    /// The raw secret token to give to the user. Only shown once!
    pub secret: String,
}

/// A token plus its scope, as returned by [`list_user_tokens`] and
/// [`get_user_token`].
pub struct TokenSummary {
    pub token: ApiToken,
    pub permissions: BTreeSet<Permission>,
    /// Empty when access is `All`.
    pub snake_ids: BTreeSet<Uuid>,
}

/// Decode stored permission names, skipping any this binary doesn't know.
///
/// Fails closed: during a rolling deploy that adds a permission, an older
/// instance treats the new one as not held rather than failing the request.
pub(crate) fn decode_permissions(token_id: Uuid, names: Vec<String>) -> BTreeSet<Permission> {
    names
        .into_iter()
        .filter_map(|name| match name.parse() {
            Ok(permission) => Some(permission),
            Err(_) => {
                tracing::warn!(%token_id, permission = %name, "ignoring unknown token permission");
                None
            }
        })
        .collect()
}

/// Generate a random 32-byte token and return it as a hex string (64 chars)
fn generate_token_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Hash a token secret using SHA-256, returning the hex-encoded hash
fn hash_token(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Create a new API token for a user with the given scope.
///
/// `snake_ids` must be empty when `snake_access` is `All`, and both lists must
/// be de-duplicated — the handler layer guarantees both.
pub async fn create_api_token(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    permissions: &[Permission],
    snake_access: SnakeAccess,
    snake_ids: &[Uuid],
) -> cja::Result<NewApiToken> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin token creation transaction")?;
    let new_token = create_api_token_on(
        &mut tx,
        user_id,
        name,
        permissions,
        snake_access,
        snake_ids,
        None,
    )
    .await?;
    tx.commit()
        .await
        .wrap_err("Failed to commit token creation")?;
    Ok(new_token)
}

/// Connection-taking core of [`create_api_token`]: inserts the token and its
/// scope rows on the caller's connection (usually inside their transaction)
/// and commits nothing. The OAuth exchange calls this to mint inside the
/// grant-rotation transaction.
pub async fn create_api_token_on(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    name: &str,
    permissions: &[Permission],
    snake_access: SnakeAccess,
    snake_ids: &[Uuid],
    oauth_grant_id: Option<Uuid>,
) -> cja::Result<NewApiToken> {
    let secret = generate_token_secret();
    let token_hash = hash_token(&secret);

    let token = sqlx::query_as!(
        ApiToken,
        r#"
        INSERT INTO api_tokens (user_id, token_hash, name, snake_access, oauth_grant_id)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, user_id, token_hash, name,
            snake_access as "snake_access: SnakeAccess",
            last_used_at, created_at, revoked_at
        "#,
        user_id,
        token_hash,
        name,
        snake_access as SnakeAccess,
        oauth_grant_id
    )
    .fetch_one(&mut *conn)
    .await
    .wrap_err("Failed to create API token")?;

    if !permissions.is_empty() {
        let permission_names: Vec<String> =
            permissions.iter().map(|p| p.as_str().to_string()).collect();
        sqlx::query!(
            r#"
            INSERT INTO api_token_permissions (api_token_id, permission)
            SELECT $1, p.permission
            FROM UNNEST($2::text[]) AS p(permission)
            "#,
            token.id,
            &permission_names
        )
        .execute(&mut *conn)
        .await
        .wrap_err("Failed to insert token permissions")?;
    }

    if snake_access == SnakeAccess::Selected && !snake_ids.is_empty() {
        sqlx::query!(
            r#"
            INSERT INTO api_token_snakes (api_token_id, battlesnake_id)
            SELECT $1, s.battlesnake_id
            FROM UNNEST($2::uuid[]) AS s(battlesnake_id)
            "#,
            token.id,
            snake_ids
        )
        .execute(&mut *conn)
        .await
        .wrap_err("Failed to insert token snake access")?;
    }

    Ok(NewApiToken { token, secret })
}

/// Add a snake to a token's selected-access list (idempotent). Used when a
/// `selected` token creates a snake: the new snake joins that token's list.
pub async fn add_snake_to_token<'e, E>(
    executor: E,
    token_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        r#"
        INSERT INTO api_token_snakes (api_token_id, battlesnake_id)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        "#,
        token_id,
        battlesnake_id
    )
    .execute(executor)
    .await
    .wrap_err("Failed to add snake to token access")?;
    Ok(())
}

/// Active (non-revoked) tokens for a user with their scope, newest first.
/// `token_id` narrows the result to that one token.
async fn load_token_summaries(
    pool: &PgPool,
    user_id: Uuid,
    token_id: Option<Uuid>,
) -> cja::Result<Vec<TokenSummary>> {
    let rows = sqlx::query!(
        r#"
        SELECT t.id, t.user_id, t.token_hash, t.name,
            t.snake_access AS "snake_access: SnakeAccess",
            t.last_used_at, t.created_at, t.revoked_at,
            ARRAY(SELECT p.permission FROM api_token_permissions p WHERE p.api_token_id = t.id) AS "permissions!: Vec<String>",
            ARRAY(SELECT s.battlesnake_id FROM api_token_snakes s WHERE s.api_token_id = t.id) AS "snake_ids!: Vec<Uuid>"
        FROM api_tokens t
        WHERE t.user_id = $1 AND t.revoked_at IS NULL
          AND ($2::uuid IS NULL OR t.id = $2)
        ORDER BY t.created_at DESC
        "#,
        user_id,
        token_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to load API tokens")?;

    Ok(rows
        .into_iter()
        .map(|row| TokenSummary {
            permissions: decode_permissions(row.id, row.permissions),
            snake_ids: row.snake_ids.into_iter().collect(),
            token: ApiToken {
                id: row.id,
                user_id: row.user_id,
                token_hash: row.token_hash,
                name: row.name,
                snake_access: row.snake_access,
                last_used_at: row.last_used_at,
                created_at: row.created_at,
                revoked_at: row.revoked_at,
            },
        })
        .collect())
}

/// Get all active (non-revoked) tokens for a user, with their scope.
pub async fn list_user_tokens(pool: &PgPool, user_id: Uuid) -> cja::Result<Vec<TokenSummary>> {
    load_token_summaries(pool, user_id, None).await
}

/// Get one of the user's active tokens, with its scope. `None` when the token
/// is unknown, revoked, or belongs to someone else.
pub async fn get_user_token(
    pool: &PgPool,
    user_id: Uuid,
    token_id: Uuid,
) -> cja::Result<Option<TokenSummary>> {
    Ok(load_token_summaries(pool, user_id, Some(token_id))
        .await?
        .into_iter()
        .next())
}

/// Authenticate a raw token secret: stamps `last_used_at` and loads the
/// token's full scope in one statement. Returns `None` for unknown/revoked
/// tokens — and for OAuth tokens whose grant is revoked or whose client is
/// disabled. A rejected token never matches the UPDATE, so it also gets no
/// `last_used_at` bump.
pub async fn authenticate_token(
    pool: &PgPool,
    token_secret: &str,
) -> cja::Result<Option<TokenAuth>> {
    let token_hash = hash_token(token_secret);

    let row = sqlx::query!(
        r#"
        WITH tok AS (
            UPDATE api_tokens SET last_used_at = NOW()
            WHERE token_hash = $1 AND revoked_at IS NULL
              AND (
                  oauth_grant_id IS NULL
                  OR EXISTS (
                      SELECT 1 FROM oauth_grants g
                      JOIN oauth_clients c ON c.id = g.oauth_client_id
                      WHERE g.id = api_tokens.oauth_grant_id
                        AND g.revoked_at IS NULL AND c.disabled_at IS NULL
                  )
              )
            RETURNING id, user_id, snake_access, oauth_grant_id
        )
        SELECT tok.id AS "token_id!: Uuid", tok.user_id AS "user_id!: Uuid",
               tok.snake_access AS "snake_access!: SnakeAccess",
               c.id AS "oauth_client_id?: Uuid",
               ARRAY(SELECT p.permission FROM api_token_permissions p WHERE p.api_token_id = tok.id) AS "permissions!: Vec<String>",
               ARRAY(SELECT s.battlesnake_id FROM api_token_snakes s WHERE s.api_token_id = tok.id) AS "snake_ids!: Vec<Uuid>"
        FROM tok
        LEFT JOIN oauth_grants g ON g.id = tok.oauth_grant_id
        LEFT JOIN oauth_clients c ON c.id = g.oauth_client_id
        "#,
        token_hash
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to authenticate API token")?;

    Ok(row.map(|row| TokenAuth {
        token_id: row.token_id,
        user_id: row.user_id,
        permissions: decode_permissions(row.token_id, row.permissions),
        snake_access: row.snake_access,
        snake_ids: row.snake_ids.into_iter().collect(),
        oauth_client_id: row.oauth_client_id,
    }))
}

/// Revoke a token by ID (must belong to the user). When the token carries an
/// OAuth grant, the grant is revoked in the same transaction — revoking any
/// of a grant's tokens removes the whole consent.
pub async fn revoke_token(pool: &PgPool, token_id: Uuid, user_id: Uuid) -> cja::Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin token revocation transaction")?;

    let result = sqlx::query!(
        r#"
        UPDATE api_tokens
        SET revoked_at = NOW()
        WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
        "#,
        token_id,
        user_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to revoke API token")?;

    if result.rows_affected() > 0 {
        // The subquery still reads the row: revoking sets a column, it does
        // not delete. Tokens without a grant (the common case) leave the
        // subquery NULL and this UPDATE a no-op.
        sqlx::query!(
            r#"
            UPDATE oauth_grants
            SET revoked_at = NOW()
            WHERE revoked_at IS NULL
              AND id = (SELECT oauth_grant_id FROM api_tokens WHERE id = $1)
            "#,
            token_id
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to revoke token's OAuth grant")?;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit token revocation")?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_token_secret_length() {
        let secret = generate_token_secret();
        assert_eq!(secret.len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn test_generate_token_secret_is_random() {
        let secret1 = generate_token_secret();
        let secret2 = generate_token_secret();
        assert_ne!(secret1, secret2);
    }

    #[test]
    fn test_hash_token_consistency() {
        let secret = "test_secret_value";
        let hash1 = hash_token(secret);
        let hash2 = hash_token(secret);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_token_length() {
        let hash = hash_token("test");
        assert_eq!(hash.len(), 64); // SHA-256 = 64 hex chars
    }

    fn scope(
        permissions: &[Permission],
        snake_access: SnakeAccess,
        snake_ids: &[Uuid],
    ) -> TokenAuth {
        TokenAuth {
            token_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            permissions: permissions.iter().copied().collect(),
            snake_access,
            snake_ids: snake_ids.iter().copied().collect(),
            oauth_client_id: None,
        }
    }

    #[test]
    fn covers_requires_permission_subset_and_no_wider_access() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let perms = |ps: &[Permission]| ps.iter().copied().collect::<BTreeSet<_>>();
        let ids = |ids: &[Uuid]| ids.iter().copied().collect::<BTreeSet<_>>();

        let all = scope(
            &[Permission::SnakesRead, Permission::TokensManage],
            SnakeAccess::All,
            &[],
        );
        // Itself, and anything narrower.
        assert!(all.covers(&all.permissions, SnakeAccess::All, &ids(&[])));
        assert!(all.covers(&perms(&[]), SnakeAccess::Selected, &ids(&[a, b])));
        // A permission it lacks.
        assert!(!all.covers(
            &perms(&[Permission::ProfileRead]),
            SnakeAccess::All,
            &ids(&[])
        ));

        let selected = scope(&[Permission::SnakesRead], SnakeAccess::Selected, &[a]);
        assert!(selected.covers(
            &perms(&[Permission::SnakesRead]),
            SnakeAccess::Selected,
            &ids(&[a])
        ));
        assert!(selected.covers(&perms(&[]), SnakeAccess::Selected, &ids(&[])));
        // Wider selection, or `all`.
        assert!(!selected.covers(&perms(&[]), SnakeAccess::Selected, &ids(&[a, b])));
        assert!(!selected.covers(&perms(&[]), SnakeAccess::All, &ids(&[])));
    }

    #[test]
    fn decode_permissions_skips_unknown_names() {
        let decoded = decode_permissions(
            Uuid::new_v4(),
            vec![
                "snakes.read".to_string(),
                "from.the.future".to_string(),
                "profile.read".to_string(),
            ],
        );
        assert_eq!(
            decoded,
            [Permission::ProfileRead, Permission::SnakesRead]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn authenticate_token_loads_scope(pool: sqlx::PgPool) {
        let user_id: Uuid = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (9917001, 'token-scope', '') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let snake_id: Uuid = sqlx::query_scalar!(
            r#"INSERT INTO battlesnakes (user_id, name, url)
               VALUES ($1, 'scope-snake', 'https://scope.example') RETURNING battlesnake_id"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let created = create_api_token(
            &pool,
            user_id,
            "scoped",
            &[Permission::ProfileRead, Permission::SnakesRead],
            SnakeAccess::Selected,
            &[snake_id],
        )
        .await
        .unwrap();

        let auth = authenticate_token(&pool, &created.secret)
            .await
            .unwrap()
            .expect("fresh token authenticates");
        assert_eq!(auth.token_id, created.token.id);
        assert_eq!(auth.user_id, user_id);
        assert_eq!(
            auth.permissions,
            [Permission::ProfileRead, Permission::SnakesRead]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(auth.snake_access, SnakeAccess::Selected);
        assert!(auth.snake_ids.contains(&snake_id));

        // Revoked tokens do not authenticate.
        revoke_token(&pool, created.token.id, user_id)
            .await
            .unwrap();
        assert!(
            authenticate_token(&pool, &created.secret)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn add_snake_to_token_is_idempotent_and_scoped(pool: sqlx::PgPool) {
        let user_id: Uuid = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (9917002, 'token-add', '') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let snake_ids: Vec<Uuid> = sqlx::query_scalar!(
            r#"INSERT INTO battlesnakes (user_id, name, url)
               VALUES ($1, 'a-snake', 'https://a.example'), ($1, 'b-snake', 'https://b.example')
               RETURNING battlesnake_id"#,
            user_id
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let created = create_api_token(
            &pool,
            user_id,
            "empty-selection",
            &[Permission::SnakesRead],
            SnakeAccess::Selected,
            &[],
        )
        .await
        .unwrap();

        // Join twice: one row, no error.
        add_snake_to_token(&pool, created.token.id, snake_ids[0])
            .await
            .unwrap();
        add_snake_to_token(&pool, created.token.id, snake_ids[0])
            .await
            .unwrap();

        let auth = authenticate_token(&pool, &created.secret)
            .await
            .unwrap()
            .unwrap();
        assert!(auth.snake_ids.contains(&snake_ids[0]));
        assert!(!auth.snake_ids.contains(&snake_ids[1]));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn list_user_tokens_returns_scope(pool: sqlx::PgPool) {
        let user_id: Uuid = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (9917003, 'token-list', '') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let snake_id: Uuid = sqlx::query_scalar!(
            r#"INSERT INTO battlesnakes (user_id, name, url)
               VALUES ($1, 'list-snake', 'https://list.example') RETURNING battlesnake_id"#,
            user_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        create_api_token(
            &pool,
            user_id,
            "all-access",
            Permission::ALL.as_slice(),
            SnakeAccess::All,
            &[],
        )
        .await
        .unwrap();
        create_api_token(
            &pool,
            user_id,
            "selected",
            &[Permission::SnakesRead],
            SnakeAccess::Selected,
            &[snake_id],
        )
        .await
        .unwrap();

        let summaries = list_user_tokens(&pool, user_id).await.unwrap();
        assert_eq!(summaries.len(), 2);
        // Ordered by created_at DESC; same-transaction timestamps can tie, so
        // look up by name.
        let all = summaries
            .iter()
            .find(|s| s.token.name == "all-access")
            .unwrap();
        assert_eq!(all.token.snake_access, SnakeAccess::All);
        assert_eq!(all.permissions.len(), 13);
        assert!(all.snake_ids.is_empty());
        let selected = summaries
            .iter()
            .find(|s| s.token.name == "selected")
            .unwrap();
        assert_eq!(selected.token.snake_access, SnakeAccess::Selected);
        assert_eq!(selected.snake_ids, BTreeSet::from([snake_id]));

        // get_user_token: the user's own active token, else None.
        let fetched = get_user_token(&pool, user_id, selected.token.id)
            .await
            .unwrap()
            .expect("own token");
        assert_eq!(fetched.token.id, selected.token.id);
        assert_eq!(fetched.permissions, selected.permissions);
        assert_eq!(fetched.snake_ids, selected.snake_ids);
        assert!(
            get_user_token(&pool, Uuid::new_v4(), selected.token.id)
                .await
                .unwrap()
                .is_none()
        );
        revoke_token(&pool, selected.token.id, user_id)
            .await
            .unwrap();
        assert!(
            get_user_token(&pool, user_id, selected.token.id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
