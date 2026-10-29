//! Secrets read from setec (DEV-1919).
//!
//! In production arena joins the tailnet in-process (`SETEC_TRANSPORT=tailnet`,
//! keyless via workload identity federation) and loads [`AppSecrets`] at boot,
//! failing fast if setec is unreachable or a grant is missing. The refresher
//! then picks up newly activated versions, so rotating a secret in setec needs
//! no redeploy.
//!
//! Without `SETEC_SERVER` (local dev, tests, e2e) arena boots with no secrets
//! and every secret-backed feature stays disabled, the same as an unset env var
//! used to.

use cja_secret::{Refresher, Secret, SecretStore, Secrets, SetecConfig};
use color_eyre::eyre::Context as _;

#[derive(Clone, Secrets)]
pub struct AppSecrets {
    /// Discord webhook for community notifications. Absent means disabled.
    #[secret("arena/discord/webhook-url")]
    pub discord_webhook_url: Option<Secret<String>>,

    /// Eyes ingest token (`arena/eyes/token`). Absent means fall back to the
    /// `EYES_TOKEN` env var, then boot tokenless. Read once at boot; rotating
    /// it in setec needs an arena restart (DEV-1966).
    #[secret("arena/eyes/token")]
    pub eyes_token: Option<Secret<String>>,
}

/// Loads [`AppSecrets`] from setec when `SETEC_SERVER` is set; `None` otherwise.
pub async fn load_from_env() -> cja::Result<Option<(AppSecrets, Refresher)>> {
    if std::env::var_os("SETEC_SERVER").is_none() {
        tracing::info!("SETEC_SERVER not set, secret-backed features disabled");
        return Ok(None);
    }
    let config = SetecConfig::from_env().wrap_err("Invalid setec config")?;
    let (secrets, refresher) = SecretStore::connect(config)
        .await
        .wrap_err("Failed to connect to setec")?
        .load::<AppSecrets>()
        .await
        .wrap_err("Failed to load secrets from setec")?;
    tracing::info!(
        discord_webhook_url = presence(secrets.discord_webhook_url.as_ref()),
        eyes_token = presence(secrets.eyes_token.as_ref()),
        "Arena secrets loaded from setec"
    );
    Ok(Some((secrets, refresher)))
}

/// `present`/`missing` for an optional secret, for logs. Never the value.
fn presence<T>(secret: Option<&Secret<T>>) -> &'static str {
    if secret.is_some() {
        "present"
    } else {
        "missing"
    }
}

/// Resolve the Eyes ingest token: the setec value wins over the `EYES_TOKEN`
/// env value; whitespace-only values count as unset. Pure so tests decide
/// precedence without ambient env. Never log the resolved value.
pub fn resolve_eyes_token(setec: Option<&Secret<String>>, env: Option<String>) -> Option<String> {
    fn clean(value: String) -> Option<String> {
        let trimmed = value.trim().to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    }
    setec
        .map(|secret| secret.current().as_str().to_owned())
        .and_then(clean)
        .or_else(|| env.and_then(clean))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EYES_TOKEN_KEY: &str = "arena/eyes/token";

    async fn setec_token(value: &str) -> Secret<String> {
        cja_secret::FakeSetec::new([(EYES_TOKEN_KEY, value.as_bytes())])
            .store()
            .declare::<String>(EYES_TOKEN_KEY)
            .await
            .expect("fake eyes token secret")
    }

    #[tokio::test]
    async fn setec_token_wins_over_env() {
        let secret = setec_token("eyes_setec").await;
        let resolved = resolve_eyes_token(Some(&secret), Some("eyes_env".into()));
        assert_eq!(resolved.as_deref(), Some("eyes_setec"));
    }

    #[tokio::test]
    async fn env_used_when_setec_absent() {
        let resolved = resolve_eyes_token(None, Some("eyes_env".into()));
        assert_eq!(resolved.as_deref(), Some("eyes_env"));
    }

    #[tokio::test]
    async fn none_when_both_absent() {
        let resolved = resolve_eyes_token(None, None);
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn whitespace_only_is_absent() {
        let secret = setec_token("  ").await;
        let resolved = resolve_eyes_token(Some(&secret), Some("  ".into()));
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn appsecrets_loads_the_eyes_token_key() {
        let (secrets, _refresher) =
            cja_secret::FakeSetec::new([(EYES_TOKEN_KEY, "eyes_setec".as_bytes())])
                .store()
                .load::<AppSecrets>()
                .await
                .unwrap();
        assert!(secrets.eyes_token.is_some());

        // Safe to merge before setec has the key: absent boots tokenless.
        let (secrets, _refresher) = cja_secret::FakeSetec::default()
            .store()
            .load::<AppSecrets>()
            .await
            .unwrap();
        assert!(secrets.eyes_token.is_none());
    }
}
