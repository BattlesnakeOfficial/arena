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
