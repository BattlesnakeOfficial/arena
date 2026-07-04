//! Transactional email via Mailgun.
//!
//! [`Mailer`] is a no-op when Mailgun isn't configured (no `MAILGUN_API_KEY`),
//! mirroring play's convention: the code compiles and runs everywhere, and
//! sending only becomes real once credentials are set. Callers treat a send
//! as best-effort — a failed email must never break the operation that
//! triggered it.

pub mod messages;

use color_eyre::eyre::Context as _;

pub use messages::EmailMessage;

use crate::models::imported_account::ClaimSummary;

impl Mailer {
    /// Notify a play account's owner that their account was claimed on
    /// arena. Fire-and-forget: the send runs in a spawned task so it never
    /// adds latency to the claim/login response, and a failure is logged,
    /// never propagated, so it can't fail the claim that triggered it.
    pub fn notify_account_claimed(&self, summary: &ClaimSummary) {
        let message = messages::account_claimed(
            &summary.email,
            &summary.username,
            summary.snakes_created,
            summary.grants_created,
        );
        let mailer = self.clone();
        tokio::spawn(async move {
            if let Err(e) = mailer.send(&message).await {
                tracing::warn!(
                    to = %message.to,
                    error = %e,
                    "Failed to send account-claimed email"
                );
            }
        });
    }
}

/// Resolved Mailgun settings. Built from env; absence of `MAILGUN_API_KEY`
/// leaves the [`Mailer`] disabled rather than erroring.
#[derive(Clone, Debug)]
pub struct MailgunConfig {
    pub api_key: String,
    /// Sending domain, e.g. `mg.battlesnake.com`.
    pub domain: String,
    /// `From` header, e.g. `Battlesnake Arena <reply@battlesnake.com>`.
    pub from: String,
    /// API base, overridable so tests can point at a mock server.
    pub base_url: String,
}

impl MailgunConfig {
    /// Read config from env. Returns `None` (disabled) when `MAILGUN_API_KEY`
    /// is unset or empty — the expected state until creds land. The other
    /// settings fall back to defaults, so this never fails.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("MAILGUN_API_KEY").ok()?;
        if api_key.is_empty() {
            return None;
        }

        let domain =
            std::env::var("MAILGUN_DOMAIN").unwrap_or_else(|_| "mg.battlesnake.com".to_string());
        let from = std::env::var("MAILGUN_FROM")
            .unwrap_or_else(|_| "Battlesnake Arena <reply@battlesnake.com>".to_string());
        let base_url = std::env::var("MAILGUN_BASE_URL")
            .unwrap_or_else(|_| "https://api.mailgun.net".to_string());

        Some(Self {
            api_key,
            domain,
            from,
            base_url,
        })
    }
}

/// Sends transactional email, or silently drops it when Mailgun isn't
/// configured. Cloneable (the client is `Arc`-backed; the small config is
/// copied) so it can live in `AppState` and be moved into spawned sends.
#[derive(Clone)]
pub struct Mailer {
    config: Option<MailgunConfig>,
    http: reqwest::Client,
}

impl Mailer {
    pub fn new(config: Option<MailgunConfig>, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    /// A disabled mailer that drops every send. Used in tests and anywhere
    /// email must be inert.
    pub fn disabled() -> Self {
        Self {
            config: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    /// Send a message. When disabled, logs and returns `Ok(())` without any
    /// network call — so callers get uniform behavior whether or not creds
    /// are present. When enabled, POSTs the Mailgun form and errors on a
    /// non-2xx response; callers log-and-continue rather than propagate.
    pub async fn send(&self, message: &EmailMessage) -> cja::Result<()> {
        let Some(config) = &self.config else {
            tracing::info!(
                to = %message.to,
                subject = %message.subject,
                "Email disabled (no Mailgun config); not sending"
            );
            return Ok(());
        };

        if message.to.trim().is_empty() {
            tracing::warn!(subject = %message.subject, "Skipping email with empty recipient");
            return Ok(());
        }

        let url = format!("{}/v3/{}/messages", config.base_url, config.domain);
        let response = self
            .http
            .post(&url)
            // Mailgun authenticates as `api:<key>` via HTTP basic auth.
            .basic_auth("api", Some(&config.api_key))
            .form(&[
                ("from", config.from.as_str()),
                ("to", message.to.as_str()),
                ("subject", message.subject.as_str()),
                ("text", message.text.as_str()),
            ])
            .send()
            .await
            .wrap_err("Failed to send request to Mailgun")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(cja::color_eyre::eyre::eyre!(
                "Mailgun returned {status}: {body}"
            ));
        }

        tracing::info!(to = %message.to, subject = %message.subject, "Email sent");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_message() -> EmailMessage {
        EmailMessage {
            to: "someone@example.com".to_string(),
            subject: "hi".to_string(),
            text: "body".to_string(),
        }
    }

    fn configured_mailer(base_url: String) -> Mailer {
        Mailer::new(
            Some(MailgunConfig {
                api_key: "key-test".to_string(),
                domain: "mg.example.com".to_string(),
                from: "Arena <reply@example.com>".to_string(),
                base_url,
            }),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn disabled_mailer_is_a_noop_ok() {
        let mailer = Mailer::disabled();
        assert!(!mailer.is_enabled());
        // No network, no panic, just Ok.
        mailer.send(&test_message()).await.unwrap();
    }

    #[tokio::test]
    async fn configured_mailer_posts_form_to_mailgun() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/mg.example.com/messages"))
            // Basic auth is exactly base64("api:key-test") — asserting the
            // value (not just presence) catches a wrong-key regression.
            .and(header("authorization", "Basic YXBpOmtleS10ZXN0"))
            .and(body_string_contains("someone%40example.com"))
            .and(body_string_contains("subject=hi"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"id\":\"ok\"}"))
            .expect(1)
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri());
        mailer.send(&test_message()).await.unwrap();
        // Mock's .expect(1) verifies exactly one matching request on drop.
    }

    #[tokio::test]
    async fn empty_recipient_is_skipped_not_sent() {
        let server = MockServer::start().await;
        // Any request to the mock is a failure — an empty recipient must not
        // reach Mailgun.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri());
        let msg = EmailMessage {
            to: "   ".to_string(),
            subject: "hi".to_string(),
            text: "body".to_string(),
        };
        mailer.send(&msg).await.unwrap();
    }

    #[tokio::test]
    async fn non_2xx_from_mailgun_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri());
        let err = mailer.send(&test_message()).await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }
}
