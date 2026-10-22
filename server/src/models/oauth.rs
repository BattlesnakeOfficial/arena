//! OAuth provider models: clients, grants, and single-use authorization
//! codes (stored hashed).
//!
//! An OAuth access token is an ordinary `api_tokens` row carrying an
//! `oauth_grant_id`; `api_token_permissions`/`api_token_snakes` stay the
//! single source of truth for what any token can do. Nothing here builds a
//! second authorization system.
//!
//! Housekeeping note: expired/consumed `oauth_authorization_codes` rows (and
//! their join rows) are inert (hashed, expired, single-use) and are not
//! pruned in v1; a periodic prune is accepted follow-up debt.

use std::collections::{BTreeSet, HashMap};

use base64::Engine as _;
use color_eyre::eyre::Context as _;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use subtle::ConstantTimeEq as _;
use uuid::Uuid;

pub use arena::api_scope::{Permission, SnakeAccess};

use crate::models::api_token::{ApiToken, NewApiToken, create_api_token_on, decode_permissions};

#[derive(Debug, sqlx::Type, Clone, Copy, PartialEq, Eq)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum OauthClientKind {
    Standard,
    HostingProvider,
}

impl OauthClientKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            OauthClientKind::Standard => "standard",
            OauthClientKind::HostingProvider => "hosting_provider",
        }
    }
}

impl std::str::FromStr for OauthClientKind {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "standard" => Ok(OauthClientKind::Standard),
            "hosting_provider" => Ok(OauthClientKind::HostingProvider),
            other => Err(color_eyre::eyre::eyre!(
                "unknown OAuth client kind: {other}"
            )),
        }
    }
}

#[derive(Debug, FromRow)]
pub struct OauthClient {
    /// DB primary key; UUID FKs to `oauth_clients` are named `oauth_client_id`.
    pub id: Uuid,
    /// Wire-format `client_id` (32 hex chars), shown to users and clients.
    pub public_client_id: String,
    pub secret_hash: String,
    pub name: String,
    pub logo_url: Option<String>,
    pub homepage_url: Option<String>,
    pub onboarding_url: Option<String>,
    pub description: Option<String>,
    pub kind: OauthClientKind,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A client plus its registered redirect URIs (admin list page + authorize
/// validation).
#[derive(Debug)]
pub struct OauthClientWithUris {
    pub client: OauthClient,
    pub redirect_uris: Vec<String>,
    pub hosting_domains: Vec<String>,
}

/// A freshly created client: `secret` is the raw secret, shown once.
pub struct NewOauthClient {
    pub client: OauthClient,
    pub secret: String,
}

pub struct CreateOauthClientInput {
    pub name: String,
    pub logo_url: Option<String>,
    pub homepage_url: Option<String>,
    pub onboarding_url: Option<String>,
    pub description: Option<String>,
    pub kind: OauthClientKind,
    pub redirect_uris: Vec<String>,
    pub hosting_domains: Vec<String>,
}

/// Generate a random 32-byte secret, hex-encoded (64 chars) — same shape as
/// token secrets.
fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Hash a client secret (SHA-256 hex), like token secrets.
fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Generate the public wire-format client id: 16 random bytes hex (32 chars).
fn generate_public_client_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// PKCE S256 challenge for a verifier: BASE64URL-ENCODE(SHA256(verifier)).
pub fn pkce_s256(verifier: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Create an OAuth client and its redirect URIs in one transaction. The raw
/// secret is returned exactly once.
pub async fn create_client(
    pool: &PgPool,
    input: CreateOauthClientInput,
) -> cja::Result<NewOauthClient> {
    let secret = generate_secret();
    let secret_hash = hash_secret(&secret);
    let public_client_id = generate_public_client_id();

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin OAuth client creation transaction")?;

    let client = sqlx::query_as!(
        OauthClient,
        r#"
        INSERT INTO oauth_clients
            (public_client_id, secret_hash, name, logo_url, homepage_url, onboarding_url, description, kind)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING
            id,
            public_client_id,
            secret_hash,
            name,
            logo_url,
            homepage_url,
            onboarding_url,
            description,
            kind as "kind: OauthClientKind",
            created_at,
            disabled_at
        "#,
        public_client_id,
        secret_hash,
        input.name,
        input.logo_url,
        input.homepage_url,
        input.onboarding_url,
        input.description,
        input.kind as OauthClientKind
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to insert OAuth client")?;

    for uri in input.redirect_uris {
        let uri = uri.trim();
        if uri.is_empty() {
            continue;
        }
        sqlx::query!(
            "INSERT INTO oauth_client_redirect_uris (oauth_client_id, uri) VALUES ($1, $2)",
            client.id,
            uri
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to insert OAuth client redirect URI")?;
    }

    for domain in input.hosting_domains {
        let domain = domain.trim();
        if domain.is_empty() {
            continue;
        }
        sqlx::query!(
            "INSERT INTO oauth_client_hosting_domains (oauth_client_id, domain) VALUES ($1, $2)",
            client.id,
            domain
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to insert OAuth client hosting domain")?;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit OAuth client creation transaction")?;

    Ok(NewOauthClient { client, secret })
}

/// All clients with their redirect URIs, ordered by name.
pub async fn list_clients(pool: &PgPool) -> cja::Result<Vec<OauthClientWithUris>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            c.id,
            c.public_client_id,
            c.secret_hash,
            c.name,
            c.logo_url,
            c.homepage_url,
            c.onboarding_url,
            c.description,
            c.kind as "kind: OauthClientKind",
            c.created_at,
            c.disabled_at,
            ARRAY(
                SELECT uri FROM oauth_client_redirect_uris u
                WHERE u.oauth_client_id = c.id
                ORDER BY uri
            ) AS "redirect_uris!: Vec<String>",
            ARRAY(
                SELECT domain FROM oauth_client_hosting_domains d
                WHERE d.oauth_client_id = c.id
                ORDER BY domain
            ) AS "hosting_domains!: Vec<String>"
        FROM oauth_clients c
        ORDER BY c.name ASC
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list OAuth clients")?;

    Ok(rows
        .into_iter()
        .map(|row| OauthClientWithUris {
            client: OauthClient {
                id: row.id,
                public_client_id: row.public_client_id,
                secret_hash: row.secret_hash,
                name: row.name,
                logo_url: row.logo_url,
                homepage_url: row.homepage_url,
                onboarding_url: row.onboarding_url,
                description: row.description,
                kind: row.kind,
                created_at: row.created_at,
                disabled_at: row.disabled_at,
            },
            redirect_uris: row.redirect_uris,
            hosting_domains: row.hosting_domains,
        })
        .collect())
}

/// Look up a client by its wire-format `client_id`.
pub async fn get_client_by_public_id(
    pool: &PgPool,
    public_client_id: &str,
) -> cja::Result<Option<OauthClient>> {
    let client = sqlx::query_as!(
        OauthClient,
        r#"
        SELECT
            id,
            public_client_id,
            secret_hash,
            name,
            logo_url,
            homepage_url,
            onboarding_url,
            description,
            kind as "kind: OauthClientKind",
            created_at,
            disabled_at
        FROM oauth_clients
        WHERE public_client_id = $1
        "#,
        public_client_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch OAuth client by public id")?;

    Ok(client)
}

/// Look up a client by its DB primary key (marks, kind checks).
pub async fn get_client_by_id(pool: &PgPool, id: Uuid) -> cja::Result<Option<OauthClient>> {
    let client = sqlx::query_as!(
        OauthClient,
        r#"
        SELECT
            id,
            public_client_id,
            secret_hash,
            name,
            logo_url,
            homepage_url,
            onboarding_url,
            description,
            kind as "kind: OauthClientKind",
            created_at,
            disabled_at
        FROM oauth_clients
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch OAuth client by id")?;

    Ok(client)
}

/// Look up a client (by DB id) with its registered redirect URIs.
pub async fn get_client_with_uris(
    pool: &PgPool,
    client_db_id: Uuid,
) -> cja::Result<Option<OauthClientWithUris>> {
    let row = sqlx::query!(
        r#"
        SELECT
            c.id,
            c.public_client_id,
            c.secret_hash,
            c.name,
            c.logo_url,
            c.homepage_url,
            c.onboarding_url,
            c.description,
            c.kind as "kind: OauthClientKind",
            c.created_at,
            c.disabled_at,
            ARRAY(
                SELECT uri FROM oauth_client_redirect_uris u
                WHERE u.oauth_client_id = c.id
                ORDER BY uri
            ) AS "redirect_uris!: Vec<String>",
            ARRAY(
                SELECT domain FROM oauth_client_hosting_domains d
                WHERE d.oauth_client_id = c.id
                ORDER BY domain
            ) AS "hosting_domains!: Vec<String>"
        FROM oauth_clients c
        WHERE c.id = $1
        "#,
        client_db_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch OAuth client with redirect URIs")?;

    Ok(row.map(|row| OauthClientWithUris {
        client: OauthClient {
            id: row.id,
            public_client_id: row.public_client_id,
            secret_hash: row.secret_hash,
            name: row.name,
            logo_url: row.logo_url,
            homepage_url: row.homepage_url,
            onboarding_url: row.onboarding_url,
            description: row.description,
            kind: row.kind,
            created_at: row.created_at,
            disabled_at: row.disabled_at,
        },
        redirect_uris: row.redirect_uris,
        hosting_domains: row.hosting_domains,
    }))
}

/// Authenticate a client's credentials at the token endpoint. `Some(client)`
/// iff found, not disabled, and the secret matches — `None` for
/// unknown/disabled/wrong-secret alike (callers emit `invalid_client`).
pub async fn authenticate_client(
    pool: &PgPool,
    public_client_id: &str,
    secret: &str,
) -> cja::Result<Option<OauthClient>> {
    let Some(client) = get_client_by_public_id(pool, public_client_id).await? else {
        return Ok(None);
    };
    if client.disabled_at.is_some() {
        return Ok(None);
    }
    let candidate = hash_secret(secret);
    let matches: bool = client
        .secret_hash
        .as_bytes()
        .ct_eq(candidate.as_bytes())
        .into();
    Ok(matches.then_some(client))
}

/// Rotate a client's secret; returns the raw secret exactly once. `None`
/// when the client id is unknown.
pub async fn rotate_client_secret(pool: &PgPool, id: Uuid) -> cja::Result<Option<String>> {
    let secret = generate_secret();
    let secret_hash = hash_secret(&secret);
    let result = sqlx::query!(
        "UPDATE oauth_clients SET secret_hash = $2 WHERE id = $1",
        id,
        secret_hash
    )
    .execute(pool)
    .await
    .wrap_err("Failed to rotate OAuth client secret")?;
    Ok((result.rows_affected() > 0).then_some(secret))
}

/// Set or clear a client's `disabled_at`. Does NOT touch tokens: disabling
/// is enforced at auth time, so re-enabling revives its tokens by design.
pub async fn set_client_disabled(pool: &PgPool, id: Uuid, disabled: bool) -> cja::Result<bool> {
    let result = if disabled {
        sqlx::query!(
            "UPDATE oauth_clients SET disabled_at = NOW() WHERE id = $1 AND disabled_at IS NULL",
            id
        )
        .execute(pool)
        .await
        .wrap_err("Failed to disable OAuth client")?
    } else {
        sqlx::query!(
            "UPDATE oauth_clients SET disabled_at = NULL WHERE id = $1 AND disabled_at IS NOT NULL",
            id
        )
        .execute(pool)
        .await
        .wrap_err("Failed to enable OAuth client")?
    };
    Ok(result.rows_affected() > 0)
}

/// A stored client URL that is safe to render into `href`/`src`: the value
/// only when it parses as an absolute `https` URL. Client URLs are
/// admin-validated at create; this is defense in depth against a stale or
/// directly-inserted row carrying a `javascript:` value (arena serves no
/// script-src CSP).
pub fn renderable_https_url(value: &Option<String>) -> Option<&str> {
    let value = value.as_deref()?;
    let parsed = url::Url::parse(value).ok()?;
    (parsed.scheme() == "https").then_some(value)
}

/// Display-only projection of a hosting-provider client for page-render
/// code (fork cards, badge). Deliberately excludes `secret_hash`,
/// `public_client_id`, `kind`, and timestamps: these queries back public
/// pages (the profile badge renders for anonymous visitors) and have no
/// business loading admin/protocol columns. Admin code keeps using
/// `OauthClient`.
#[derive(Debug, FromRow)]
pub struct HostingProviderDisplay {
    pub id: Uuid,
    pub name: String,
    pub logo_url: Option<String>,
    pub homepage_url: Option<String>,
    pub onboarding_url: Option<String>,
    pub description: Option<String>,
}

/// THE single host extractor for snake URLs (Decision: no second inline
/// `Url::parse(..).host_str()` chain anywhere). Lowercased host of a URL
/// (port-free; the url crate also converts IDN to punycode and preserves
/// trailing dots — `softmax.com.` stays `softmax.com.`, which then does
/// NOT match a registration of `softmax.com`), or `None` when
/// unparseable. Called by `client_domain_covers_url` AND
/// `models::battlesnake::update_battlesnake`.
pub fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// A snake URL matches a registered hosting domain when the URL's host
/// equals the domain or is a subdomain of it (label-boundary match).
/// `softmax.com.evil.io` and `evilsoftmax.com` do NOT match `softmax.com`.
pub fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Validate and normalize an admin-entered hosting domain: trimmed;
/// REJECTS non-ASCII input with an error naming the punycode form
/// ("domains must be ASCII — enter the punycode (xn--) form, e.g.
/// xn--…"); then lowercased, >=2 DNS labels, each 1-63 chars of
/// [a-z0-9-] not starting or ending with '-'. Mirrors the table's CHECK
/// constraint (which additionally rejects uppercase).
pub fn validate_hosting_domain(domain: &str) -> Result<String, String> {
    let domain = domain.trim();
    if !domain.is_ascii() {
        let punycode = idna_punycode_hint(domain);
        return Err(format!(
            "hosting domain {domain:?} must be ASCII — enter the punycode (xn--) form, e.g. {punycode}"
        ));
    }
    let domain = domain.to_ascii_lowercase();
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return Err(format!(
            "hosting domain {domain:?} must have at least two dot-separated labels (e.g. softmax.com)"
        ));
    }
    for label in labels {
        let valid = (1..=63).contains(&label.len())
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err(format!(
                "hosting domain {domain:?} is not a valid DNS domain (lowercase letters, digits and hyphens per label)"
            ));
        }
    }
    Ok(domain)
}

/// The punycode form of a non-ASCII host, for the admin error message —
/// exactly what the url crate will turn the host into at match time, so
/// the admin sees the string that would actually have to be registered.
fn idna_punycode_hint(domain: &str) -> String {
    url::Url::parse(&format!("https://{domain}"))
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| "xn--…".to_string())
}

/// Does `url`'s host sit on one of this client's registered hosting
/// domains (equal or subdomain)? Used by the mark-hosting route. Fetches
/// the client's domains and matches in Rust via `url_host` +
/// `host_matches_domain`.
pub async fn client_domain_covers_url(
    pool: &PgPool,
    client_id: Uuid,
    url: &str,
) -> cja::Result<bool> {
    let Some(host) = url_host(url) else {
        return Ok(false);
    };
    let domains = sqlx::query_scalar!(
        "SELECT domain FROM oauth_client_hosting_domains WHERE oauth_client_id = $1",
        client_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch OAuth client hosting domains")?;
    Ok(domains
        .iter()
        .any(|domain| host_matches_domain(&host, domain)))
}

/// Enabled hosting_provider clients, ordered by name — the create-snake
/// fork cards. Display projection; the fork hides disabled providers.
pub async fn list_enabled_hosting_providers(
    pool: &PgPool,
) -> cja::Result<Vec<HostingProviderDisplay>> {
    let providers = sqlx::query_as!(
        HostingProviderDisplay,
        r#"
        SELECT id, name, logo_url, homepage_url, onboarding_url, description
        FROM oauth_clients
        WHERE disabled_at IS NULL AND kind = 'hosting_provider'
        ORDER BY name ASC
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list enabled hosting providers")?;
    Ok(providers)
}

/// The client whose mark this snake carries (badge on the profile page).
/// Display projection. No disabled filter: the mark is truthful while
/// the URL is on-domain. No kind filter: the mark column is only ever
/// set through the kind-checked mark endpoint, and kind never changes
/// after creation.
pub async fn hosting_client_for_snake(
    pool: &PgPool,
    snake_id: Uuid,
) -> cja::Result<Option<HostingProviderDisplay>> {
    let client = sqlx::query_as!(
        HostingProviderDisplay,
        r#"
        SELECT c.id, c.name, c.logo_url, c.homepage_url, c.onboarding_url, c.description
        FROM oauth_clients c
        JOIN battlesnakes b ON b.hosted_by_oauth_client_id = c.id
        WHERE b.battlesnake_id = $1 AND b.deleted_at IS NULL
        "#,
        snake_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch snake's hosting client")?;
    Ok(client)
}

/// Per-snake hosting client for a batch (owner's snake list badge) — one
/// round trip, returns a map keyed by battlesnake_id. Display projection;
/// unmarked and soft-deleted snakes are simply absent.
pub async fn hosting_clients_for_snakes(
    pool: &PgPool,
    snake_ids: &[Uuid],
) -> cja::Result<HashMap<Uuid, HostingProviderDisplay>> {
    /// Row of the single join query: the snake id plus the client's
    /// display columns (one struct because `query_as!` cannot return two
    /// values per row).
    #[derive(FromRow)]
    struct HostedRow {
        battlesnake_id: Uuid,
        id: Uuid,
        name: String,
        logo_url: Option<String>,
        homepage_url: Option<String>,
        onboarding_url: Option<String>,
        description: Option<String>,
    }

    let rows = sqlx::query_as!(
        HostedRow,
        r#"
        SELECT b.battlesnake_id, c.id, c.name, c.logo_url,
               c.homepage_url, c.onboarding_url, c.description
        FROM oauth_clients c
        JOIN battlesnakes b ON b.hosted_by_oauth_client_id = c.id
        WHERE b.battlesnake_id = ANY($1) AND b.deleted_at IS NULL
        "#,
        snake_ids
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch hosting clients for snakes")?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let HostedRow {
                battlesnake_id,
                id,
                name,
                logo_url,
                homepage_url,
                onboarding_url,
                description,
            } = row;
            (
                battlesnake_id,
                HostingProviderDisplay {
                    id,
                    name,
                    logo_url,
                    homepage_url,
                    onboarding_url,
                    description,
                },
            )
        })
        .collect())
}

/// A player's standing consent to one client, as shown on the "Authorized
/// apps" page. The scope shown is the live token's scope.
pub struct GrantSummary {
    pub grant_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub client_name: String,
    pub client_logo_url: Option<String>,
    pub permissions: BTreeSet<Permission>,
    pub snake_access: SnakeAccess,
    pub snake_ids: BTreeSet<Uuid>,
}

/// The player's active grants. INNER JOINs the live token: a grant whose
/// token died by any path simply isn't listed.
pub async fn list_active_grants(pool: &PgPool, user_id: Uuid) -> cja::Result<Vec<GrantSummary>> {
    let rows = sqlx::query!(
        r#"
        SELECT
            g.id AS grant_id,
            g.created_at,
            c.name AS "client_name!",
            c.logo_url AS "client_logo_url?",
            t.snake_access AS "snake_access: SnakeAccess",
            ARRAY(SELECT p.permission FROM api_token_permissions p WHERE p.api_token_id = t.id) AS "permissions!: Vec<String>",
            ARRAY(SELECT s.battlesnake_id FROM api_token_snakes s WHERE s.api_token_id = t.id) AS "snake_ids!: Vec<Uuid>"
        FROM oauth_grants g
        JOIN api_tokens t ON t.oauth_grant_id = g.id AND t.revoked_at IS NULL
        JOIN oauth_clients c ON c.id = g.oauth_client_id
        WHERE g.user_id = $1 AND g.revoked_at IS NULL
        ORDER BY g.created_at DESC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list active OAuth grants")?;

    Ok(rows
        .into_iter()
        .map(|row| GrantSummary {
            grant_id: row.grant_id,
            created_at: row.created_at,
            permissions: decode_permissions(row.grant_id, row.permissions),
            client_name: row.client_name,
            client_logo_url: row.client_logo_url,
            snake_access: row.snake_access,
            snake_ids: row.snake_ids.into_iter().collect(),
        })
        .collect())
}

/// Delete every outstanding (unconsumed) authorization code for a (user,
/// client), join rows cascading — run inside the same transaction as any
/// grant-affecting change (approve, revoke, snake edit, token revoke,
/// exchange rotation) so a client holding an older code cannot resurrect
/// or supersede the new state within the code lifetime. Callers take
/// `lock_user_client` first.
pub(crate) async fn delete_outstanding_codes(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    oauth_client_id: Uuid,
) -> cja::Result<()> {
    sqlx::query!(
        "DELETE FROM oauth_authorization_codes WHERE user_id = $1 AND oauth_client_id = $2 AND consumed_at IS NULL",
        user_id,
        oauth_client_id
    )
    .execute(&mut *conn)
    .await
    .wrap_err("Failed to invalidate outstanding authorization codes")?;
    Ok(())
}

/// Serialize every grant-affecting transaction per (user_id,
/// oauth_client_id) with a transaction-scoped advisory lock, so a
/// snake-selection edit can never race an exchange (or a revoke) into
/// landing on a just-revoked token.
///
/// Callers — each takes the lock AFTER its plain SELECTs and BEFORE its
/// first INSERT/UPDATE/DELETE:
/// - [`create_authorization_code`] (before deleting outstanding codes)
/// - [`exchange_authorization_code`] (after the cheap rejections, before
///   the consume UPDATE)
/// - [`revoke_grant`] (after the plain grant-client SELECT)
/// - [`set_grant_snakes`] (after the plain grant-client SELECT)
/// - [`crate::models::api_token::revoke_token`] (after its plain
///   grant-client SELECT)
/// - [`revoke_token_issued_by_code`] (after the plain code-owner SELECT)
///
/// One lock order everywhere also removes the lock-order deadlocks the
/// cross-table writes could otherwise introduce: exchange locks code →
/// grant/token while the other writers lock grant/token → codes, and the
/// advisory lock serializes them per (user, client) before either order
/// begins.
pub(crate) async fn lock_user_client(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    oauth_client_id: Uuid,
) -> cja::Result<()> {
    let key = user_client_advisory_key(user_id, oauth_client_id);
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))",
        key
    )
    .execute(&mut *conn)
    .await
    .wrap_err("Failed to take (user, client) advisory lock")?;
    Ok(())
}

/// The advisory-lock key text for a (user, client) pair. The
/// `oauth.user_client:` prefix keeps this lock in its own
/// `hashtextextended(_, 0)` keyspace instead of sharing one with the
/// other advisory-lock users (e.g. `models/email_log.rs` hashes bare
/// lowercase text). Built in exactly one place so every taker —
/// `lock_user_client` and the tests that hold the same lock to prove
/// serialization — agrees on the text.
pub(crate) fn user_client_advisory_key(user_id: Uuid, oauth_client_id: Uuid) -> String {
    format!("oauth.user_client:{user_id}:{oauth_client_id}")
}

/// Revoke a grant and its live tokens in one transaction. `false` when the
/// grant isn't found, already revoked, or not the user's.
pub async fn revoke_grant(pool: &PgPool, grant_id: Uuid, user_id: Uuid) -> cja::Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin grant revocation transaction")?;

    // Plain SELECT first (no row lock), then serialize with every other
    // grant-affecting writer for this (user, client) before writing.
    let grant_client = sqlx::query_scalar!(
        "SELECT oauth_client_id FROM oauth_grants WHERE id = $1 AND user_id = $2",
        grant_id,
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to load grant's OAuth client")?;
    let Some(oauth_client_id) = grant_client else {
        tx.rollback()
            .await
            .wrap_err("Failed to roll back grant revocation transaction")?;
        return Ok(false);
    };
    lock_user_client(&mut tx, user_id, oauth_client_id).await?;

    let revoked_client = sqlx::query_scalar!(
        r#"
        UPDATE oauth_grants
        SET revoked_at = NOW()
        WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
        RETURNING oauth_client_id
        "#,
        grant_id,
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to revoke OAuth grant")?;

    if let Some(oauth_client_id) = revoked_client {
        sqlx::query!(
            "UPDATE api_tokens SET revoked_at = NOW() WHERE oauth_grant_id = $1 AND revoked_at IS NULL",
            grant_id
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to revoke grant's tokens")?;
        // Outstanding codes die with the grant: exchanging one would re-mint
        // a grant (and a token) after the player revoked the app.
        delete_outstanding_codes(&mut tx, user_id, oauth_client_id).await?;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit grant revocation transaction")?;

    Ok(revoked_client.is_some())
}

/// Edit a grant's snake selection: rewrites the live token's `snake_access`
/// and `api_token_snakes`. No permission changes; no token re-issue — the
/// new selection takes effect on the token's next use because
/// `authenticate_token` loads scope fresh. `snake_ids` must be empty when
/// `snake_access` is `All`. `false` when the grant isn't found/active, isn't
/// the user's, or has no live token.
pub async fn set_grant_snakes(
    pool: &PgPool,
    grant_id: Uuid,
    user_id: Uuid,
    snake_access: SnakeAccess,
    snake_ids: &[Uuid],
) -> cja::Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin grant snake-edit transaction")?;

    // Plain SELECT first (no row lock), then serialize with every other
    // grant-affecting writer for this (user, client) — a concurrent
    // exchange cannot revoke/rotate the live token between our SELECT and
    // our writes.
    let grant_client = sqlx::query_scalar!(
        "SELECT oauth_client_id FROM oauth_grants WHERE id = $1 AND user_id = $2",
        grant_id,
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to load grant's OAuth client")?;
    let Some(oauth_client_id) = grant_client else {
        tx.rollback()
            .await
            .wrap_err("Failed to roll back grant snake-edit transaction")?;
        return Ok(false);
    };
    lock_user_client(&mut tx, user_id, oauth_client_id).await?;

    let token_id = sqlx::query_scalar!(
        r#"
        SELECT t.id
        FROM oauth_grants g
        JOIN api_tokens t ON t.oauth_grant_id = g.id AND t.revoked_at IS NULL
        WHERE g.id = $1 AND g.user_id = $2 AND g.revoked_at IS NULL
        "#,
        grant_id,
        user_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to find grant's live token")?;

    let Some(token_id) = token_id else {
        tx.rollback()
            .await
            .wrap_err("Failed to roll back grant snake-edit transaction")?;
        return Ok(false);
    };

    // The `revoked_at IS NULL` guard (with the rows_affected check) makes a
    // stale token id a no-op instead of a silent rewrite of a now-revoked
    // token: if the token died, the edit fails loudly for the player
    // rather than pretending to succeed on a dead row.
    let updated = sqlx::query!(
        "UPDATE api_tokens SET snake_access = $2 WHERE id = $1 AND revoked_at IS NULL",
        token_id,
        snake_access as SnakeAccess
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to update grant token snake access")?;
    if updated.rows_affected() == 0 {
        tx.rollback()
            .await
            .wrap_err("Failed to roll back grant snake-edit transaction")?;
        return Ok(false);
    }

    sqlx::query!(
        "DELETE FROM api_token_snakes WHERE api_token_id = $1",
        token_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to clear grant token snake selection")?;

    if snake_access == SnakeAccess::Selected && !snake_ids.is_empty() {
        sqlx::query!(
            r#"
            INSERT INTO api_token_snakes (api_token_id, battlesnake_id)
            SELECT $1, s.battlesnake_id
            FROM UNNEST($2::uuid[]) AS s(battlesnake_id)
            "#,
            token_id,
            snake_ids
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to insert grant token snake selection")?;
    }

    // A pre-approval code still in flight would mint a token with the OLD
    // selection at exchange time, undoing this narrowing — kill it.
    delete_outstanding_codes(&mut tx, user_id, oauth_client_id).await?;

    tx.commit()
        .await
        .wrap_err("Failed to commit grant snake-edit transaction")?;

    Ok(true)
}

/// Create a single-use authorization code, stored hashed, valid 10 minutes,
/// bound to client, user, redirect URI, PKCE challenge, and the approved
/// scope (permissions + snake access). Returns the raw code exactly once.
///
/// Caller guarantees `permissions ⊆ OAUTH_REQUESTABLE` and `snake_ids ⊆
/// user's snakes`; the join tables' CHECK/FK constraints back both.
pub struct NewAuthorizationCode<'a> {
    pub client_db_id: Uuid,
    pub user_id: Uuid,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub permissions: &'a [Permission],
    pub snake_access: SnakeAccess,
    pub snake_ids: &'a [Uuid],
}

pub async fn create_authorization_code(
    pool: &PgPool,
    input: NewAuthorizationCode<'_>,
) -> cja::Result<String> {
    let NewAuthorizationCode {
        client_db_id,
        user_id,
        redirect_uri,
        code_challenge,
        permissions,
        snake_access,
        snake_ids,
    } = input;
    let mut code_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut code_bytes);
    let code = hex::encode(code_bytes);
    let code_hash = hash_secret(&code);

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin authorization code transaction")?;

    // Latest consent wins: serialize with every other grant-affecting
    // writer for this (user, client), then kill every earlier outstanding
    // code — a client holding an older, broader approval must not be able
    // to exchange it after the player approved a narrower consent. (What
    // docs/oauth.md promises for re-authorizing.)
    lock_user_client(&mut tx, user_id, client_db_id).await?;
    delete_outstanding_codes(&mut tx, user_id, client_db_id).await?;

    let code_id = sqlx::query_scalar!(
        r#"
        INSERT INTO oauth_authorization_codes
            (oauth_client_id, user_id, redirect_uri, code_hash, code_challenge, snake_access, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '10 minutes')
        RETURNING id
        "#,
        client_db_id,
        user_id,
        redirect_uri,
        code_hash,
        code_challenge,
        snake_access as SnakeAccess
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to insert authorization code")?;

    if !permissions.is_empty() {
        let permission_names: Vec<String> =
            permissions.iter().map(|p| p.as_str().to_string()).collect();
        sqlx::query!(
            r#"
            INSERT INTO oauth_authorization_code_permissions (oauth_authorization_code_id, permission)
            SELECT $1, p.permission
            FROM UNNEST($2::text[]) AS p(permission)
            "#,
            code_id,
            &permission_names
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to insert authorization code permissions")?;
    }

    if snake_access == SnakeAccess::Selected && !snake_ids.is_empty() {
        sqlx::query!(
            r#"
            INSERT INTO oauth_authorization_code_snakes (oauth_authorization_code_id, battlesnake_id)
            SELECT $1, s.battlesnake_id
            FROM UNNEST($2::uuid[]) AS s(battlesnake_id)
            "#,
            code_id,
            snake_ids
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to insert authorization code snakes")?;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit authorization code transaction")?;

    Ok(code)
}

/// Why an authorization-code exchange failed. Every variant maps to
/// `invalid_grant` (or `invalid_client` upstream) at the route; the split
/// exists so replay can revoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeError {
    UnknownCode,
    /// Presented by a different client than issued the code — checked
    /// FIRST, no side effects.
    ClientMismatch,
    /// Already consumed: callers revoke the token this code issued (and its
    /// grant), per RFC 6749 §4.1.2.
    Replayed,
    RedirectMismatch,
    Expired,
    VerifierMismatch,
}

pub struct ExchangedToken {
    pub token: ApiToken,
    /// Raw token secret — shown exactly once.
    pub secret: String,
    /// The permissions actually granted (the code's approved scope, which
    /// may be narrower than requested, e.g. withheld `email.read`).
    pub permissions: BTreeSet<Permission>,
}

/// Exchange an authorization code for an access token, in one transaction.
///
/// Check order (matters — tests pin it): unknown code → client mismatch →
/// already-consumed (replay) → redirect mismatch → expired → PKCE verify →
/// conditional consume. Only the code's own client can trigger replay
/// revocation; another client presenting the code gets a plain
/// `ClientMismatch` with no side effects.
///
/// Consume, grant rotation, and token mint share the transaction: a failure
/// anywhere after the conditional consume rolls the consume back, so the
/// client may retry the same code.
pub async fn exchange_authorization_code(
    pool: &PgPool,
    client_db_id: Uuid,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> cja::Result<Result<ExchangedToken, ExchangeError>> {
    let code_hash = hash_secret(code);

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin code exchange transaction")?;

    let row = sqlx::query!(
        r#"
        SELECT c.id,
               c.oauth_client_id,
               c.user_id,
               c.redirect_uri,
               c.code_challenge,
               c.snake_access AS "snake_access: SnakeAccess",
               c.expires_at,
               c.consumed_at,
               ARRAY(SELECT p.permission FROM oauth_authorization_code_permissions p
                     WHERE p.oauth_authorization_code_id = c.id) AS "permissions!: Vec<String>",
               ARRAY(SELECT s.battlesnake_id FROM oauth_authorization_code_snakes s
                     JOIN battlesnakes b ON b.battlesnake_id = s.battlesnake_id AND b.deleted_at IS NULL
                     WHERE s.oauth_authorization_code_id = c.id) AS "snake_ids!: Vec<Uuid>"
        FROM oauth_authorization_codes c
        WHERE c.code_hash = $1
        "#,
        code_hash
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to load authorization code")?;

    let Some(row) = row else {
        return Ok(Err(ExchangeError::UnknownCode));
    };

    if row.oauth_client_id != client_db_id {
        return Ok(Err(ExchangeError::ClientMismatch));
    }
    if row.consumed_at.is_some() {
        return Ok(Err(ExchangeError::Replayed));
    }
    if row.redirect_uri != redirect_uri {
        return Ok(Err(ExchangeError::RedirectMismatch));
    }
    if row.expires_at < chrono::Utc::now() {
        return Ok(Err(ExchangeError::Expired));
    }
    // PKCE verify (lengths were checked at authorize; verify here too).
    let verifier_ok = (43..=128).contains(&code_verifier.len())
        && (43..=128).contains(&row.code_challenge.len())
        && pkce_s256(code_verifier) == row.code_challenge;
    if !verifier_ok {
        return Ok(Err(ExchangeError::VerifierMismatch));
    }

    // Serialize with every other grant-affecting writer for this (user,
    // client) BEFORE the first write: a concurrent snake-selection edit or
    // re-approval must not interleave with the rotation below.
    lock_user_client(&mut tx, row.user_id, row.oauth_client_id).await?;

    // Race-safe consume: exactly one concurrent exchange wins. Expiry is
    // checked against the DB clock here too, so app/DB clock skew cannot
    // keep a dead code consumable (the Rust-side check above only picks
    // the error message).
    let consumed = sqlx::query!(
        "UPDATE oauth_authorization_codes SET consumed_at = NOW() WHERE id = $1 AND consumed_at IS NULL AND expires_at > NOW()",
        row.id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to consume authorization code")?;
    if consumed.rows_affected() == 0 {
        return Ok(Err(ExchangeError::Replayed));
    }

    // Every OTHER outstanding code for this (user, client) dies with the
    // rotation: an older, broader code must not supersede the grant this
    // exchange is about to create. (The code being exchanged is already
    // consumed above, so it is not deleted.)
    delete_outstanding_codes(&mut tx, row.user_id, row.oauth_client_id).await?;

    // Grant rotation: revoke the prior active grant (and its tokens) for
    // this (user, client), then insert the new grant. The prior token's
    // revoke happens before the new token's INSERT — required by
    // api_tokens_one_live_per_grant.
    let prior_grant = sqlx::query_scalar!(
        r#"
        SELECT id FROM oauth_grants
        WHERE user_id = $1 AND oauth_client_id = $2 AND revoked_at IS NULL
        "#,
        row.user_id,
        row.oauth_client_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to load prior OAuth grant")?;
    if let Some(prior) = prior_grant {
        sqlx::query!(
            "UPDATE oauth_grants SET revoked_at = NOW() WHERE id = $1",
            prior
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to revoke prior OAuth grant")?;
        sqlx::query!(
            "UPDATE api_tokens SET revoked_at = NOW() WHERE oauth_grant_id = $1 AND revoked_at IS NULL",
            prior
        )
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to revoke prior grant's tokens")?;
    }

    let grant_id = sqlx::query_scalar!(
        r#"
        INSERT INTO oauth_grants (user_id, oauth_client_id)
        VALUES ($1, $2)
        RETURNING id
        "#,
        row.user_id,
        row.oauth_client_id
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to insert OAuth grant")?;

    let client_name = sqlx::query_scalar!(
        "SELECT name FROM oauth_clients WHERE id = $1",
        row.oauth_client_id
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to load OAuth client name")?;

    let permissions: Vec<Permission> = decode_permissions(row.id, row.permissions)
        .into_iter()
        .collect();
    let snake_ids = row.snake_ids;

    // A snake soft-deleted between approve and exchange (snakes are
    // soft-deleted; the FK cascade never fires) was filtered out of
    // `snake_ids` above, so this INSERT can never hit a dead snake.
    let NewApiToken { token, secret } = create_api_token_on(
        &mut tx,
        row.user_id,
        &client_name,
        &permissions,
        row.snake_access,
        &snake_ids,
        Some(grant_id),
    )
    .await?;

    sqlx::query!(
        "UPDATE oauth_authorization_codes SET issued_token_id = $2 WHERE id = $1",
        row.id,
        token.id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to record code's issued token")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit code exchange transaction")?;

    Ok(Ok(ExchangedToken {
        token,
        secret,
        permissions: permissions.into_iter().collect(),
    }))
}

/// Revoke the token a code issued AND its grant, in one transaction — the
/// replay side effect of RFC 6749 §4.1.2. Best-effort from the route (the
/// response stays `invalid_grant` even if this errors); errors are logged
/// by the caller.
pub async fn revoke_token_issued_by_code(pool: &PgPool, code: &str) -> cja::Result<()> {
    let code_hash = hash_secret(code);

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin replay revocation transaction")?;

    // Plain read first (no row lock): resolve the (user, client) the code
    // belongs to so this path serializes with every other grant-affecting
    // writer for that pair. A code row that has vanished (nothing was
    // issued to revoke) is a clean no-op.
    let owner = sqlx::query!(
        "SELECT user_id, oauth_client_id FROM oauth_authorization_codes WHERE code_hash = $1",
        code_hash
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to load replayed code's owner")?;
    let Some(owner) = owner else {
        tx.rollback()
            .await
            .wrap_err("Failed to roll back replay revocation transaction")?;
        return Ok(());
    };

    lock_user_client(&mut tx, owner.user_id, owner.oauth_client_id).await?;

    // Grant first, then token — the same row order every other
    // grant-affecting writer uses, so a replay racing a player's Revoke
    // (or a newer-code exchange) cannot lock-order deadlock (40P01 → 500).
    sqlx::query!(
        r#"
        UPDATE oauth_grants SET revoked_at = NOW()
        WHERE revoked_at IS NULL
          AND id = (SELECT oauth_grant_id FROM api_tokens
                    WHERE id = (SELECT issued_token_id FROM oauth_authorization_codes WHERE code_hash = $1))
        "#,
        code_hash
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to revoke replayed code's grant")?;

    sqlx::query!(
        r#"
        UPDATE api_tokens SET revoked_at = NOW()
        WHERE revoked_at IS NULL
          AND id = (SELECT issued_token_id FROM oauth_authorization_codes WHERE code_hash = $1)
        "#,
        code_hash
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to revoke replayed code's token")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit replay revocation transaction")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc7636_appendix_b_vector() {
        // The RFC 7636 test vector: verifier "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        // has challenge "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn secret_hashing_is_stable_and_hidden() {
        let a = hash_secret("client-secret");
        let b = hash_secret("client-secret");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, "client-secret");
        let secret = generate_secret();
        assert_eq!(secret.len(), 64);
        let public = generate_public_client_id();
        assert_eq!(public.len(), 32);
        assert_ne!(generate_public_client_id(), public);
    }

    /// The shared vector table for host extraction and domain matching,
    /// pinned to `url` 2.5.8 behavior: exact host strings, so a url-crate
    /// upgrade that changes lowercasing, port-stripping, punycoding, or
    /// trailing-dot preservation cannot slip through silently.
    #[test]
    fn url_host_extracts_exact_hosts() {
        let cases = [
            ("https://softmax.com/s", Some("softmax.com")),
            ("https://api.softmax.com/s", Some("api.softmax.com")),
            // userinfo is not the host
            ("https://user@api.softmax.com/s", Some("api.softmax.com")),
            // host_str() drops the port
            ("https://api.softmax.com:8443/s", Some("api.softmax.com")),
            // the crate lowercases hosts
            ("https://API.SoftMax.com/s", Some("api.softmax.com")),
            // the real host is evil.io
            ("https://softmax.com@evil.io/s", Some("evil.io")),
            // trailing dot, deliberately unnormalized
            ("https://softmax.com./s", Some("softmax.com.")),
            // IDN → punycode
            ("https://softm\u{0430}x.com/s", Some("xn--softmx-7nf.com")),
            ("https://softmax.com.evil.io/s", Some("softmax.com.evil.io")),
            ("https://evilsoftmax.com/s", Some("evilsoftmax.com")),
            ("https://notsoftmax.com/s", Some("notsoftmax.com")),
            // IP literal
            ("https://1.2.3.4/s", Some("1.2.3.4")),
            // fails safe
            ("not a url", None),
        ];
        for (url, expected) in cases {
            assert_eq!(url_host(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn host_matches_domain_is_label_boundary() {
        let softmax = "softmax.com";
        for host in ["softmax.com", "api.softmax.com", "deep.sub.softmax.com"] {
            assert!(host_matches_domain(host, softmax), "{host} must match");
        }
        for host in [
            "softmax.com.evil.io",
            "evilsoftmax.com",
            "notsoftmax.com",
            "softmax.com.",
            "xn--softmx-7nf.com",
            "evil.io",
            "1.2.3.4",
        ] {
            assert!(!host_matches_domain(host, softmax), "{host} must NOT match");
        }
        // A host never matches the empty domain or a domain it merely ends with.
        assert!(!host_matches_domain("softmax.com", ""));
        assert!(!host_matches_domain("xsoftmax.com", "oftmax.com"));
    }

    #[test]
    fn validate_hosting_domain_normalizes_and_rejects() {
        // Case-fold + trim
        assert_eq!(
            validate_hosting_domain(" SoftMax.COM ").unwrap(),
            "softmax.com"
        );
        assert_eq!(
            validate_hosting_domain("api.softmax.com").unwrap(),
            "api.softmax.com"
        );

        // Shape rejections
        for bad in [
            "-bad.com",
            "bad-.com",
            "nodot",
            "a..b",
            "softmax.com.",
            "soft max.com",
            "soft_max.com",
        ] {
            assert!(
                validate_hosting_domain(bad).is_err(),
                "{bad} must be rejected"
            );
        }
        // A single label longer than 63 chars (a domain whose only labels are
        // one 64-char blob and a TLD)
        let too_long = format!("{}.com", "a".repeat(64));
        assert!(validate_hosting_domain(&too_long).is_err());

        // Non-ASCII is rejected with the punycode hint (Cyrillic а).
        let err = validate_hosting_domain("softm\u{0430}x.com").unwrap_err();
        assert!(err.contains("ASCII"), "{err}");
        assert!(err.contains("xn--softmx-7nf.com"), "{err}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn create_client_domains_are_transactional(pool: sqlx::PgPool) {
        // Force the hosting-domain insert to fail: the client row must not
        // survive.
        sqlx::query(
            "CREATE OR REPLACE FUNCTION raise_domain_boom() RETURNS trigger AS $fn$ BEGIN RAISE EXCEPTION 'boom'; END $fn$ LANGUAGE plpgsql",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER hosting_domain_boom BEFORE INSERT ON oauth_client_hosting_domains
             FOR EACH ROW EXECUTE FUNCTION raise_domain_boom()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let result = create_client(
            &pool,
            CreateOauthClientInput {
                name: "Boom Host".to_string(),
                logo_url: None,
                homepage_url: None,
                onboarding_url: Some("https://boom.example/start".to_string()),
                description: None,
                kind: OauthClientKind::HostingProvider,
                redirect_uris: vec!["https://boom.example/cb".to_string()],
                hosting_domains: vec!["boom.example".to_string()],
            },
        )
        .await;
        assert!(result.is_err(), "create must fail while the trigger stands");
        let count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_clients WHERE name = 'Boom Host'"#
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "no client row may survive a failed domain insert");

        // Drop the trigger and retry: success, domains stored.
        sqlx::query("DROP TRIGGER hosting_domain_boom ON oauth_client_hosting_domains")
            .execute(&pool)
            .await
            .unwrap();
        let created = create_client(
            &pool,
            CreateOauthClientInput {
                name: "Boom Host".to_string(),
                logo_url: None,
                homepage_url: None,
                onboarding_url: Some("https://boom.example/start".to_string()),
                description: Some("We boom politely".to_string()),
                kind: OauthClientKind::HostingProvider,
                redirect_uris: vec!["https://boom.example/cb".to_string()],
                hosting_domains: vec!["boom.example".to_string(), "alt.boom.example".to_string()],
            },
        )
        .await
        .unwrap();
        let with_uris = get_client_with_uris(&pool, created.client.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            with_uris.hosting_domains,
            vec!["alt.boom.example".to_string(), "boom.example".to_string()]
        );
        assert_eq!(
            with_uris.client.onboarding_url.as_deref(),
            Some("https://boom.example/start")
        );
        assert_eq!(
            with_uris.client.description.as_deref(),
            Some("We boom politely")
        );
    }
}
