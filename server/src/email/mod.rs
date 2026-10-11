//! Transactional email via Mailgun.
//!
//! [`Mailer`] is a no-op when Mailgun isn't configured (no `MAILGUN_API_KEY`),
//! mirroring play's convention: the code compiles and runs everywhere, and
//! sending only becomes real once credentials are set. Delivery jobs retry
//! transport errors without failing the operation that raised the event.

pub mod messages;

use color_eyre::eyre::Context as _;

pub use messages::EmailMessage;

use crate::models::email_log;

/// What became of a rate-limited send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The recipient hit the per-address hourly cap; the message was
    /// dropped (the attempt still spent budget).
    RateLimited,
    /// Mailgun isn't configured; nothing was sent or logged.
    Disabled,
}

impl Mailer {
    /// Send through the per-recipient hourly rate limit (play's safety net:
    /// no address gets more than `hourly_limit` emails/hour, no matter what
    /// triggers them). The attempt is logged before the check — record-
    /// before-check, so concurrent sends see each other and a suppressed
    /// send still spends budget. Every production send should come through
    /// here; raw [`Mailer::send`] is the transport underneath.
    pub async fn send_limited(
        &self,
        connection: &mut sqlx::PgConnection,
        hourly_limit: i64,
        purpose: &str,
        message: &EmailMessage,
    ) -> cja::Result<SendOutcome> {
        if !self.is_enabled() {
            // Keep the disabled path a true no-op (matching `send`): no log
            // rows for messages that never had a transport.
            tracing::info!(
                subject = %message.subject,
                "Email disabled (no Mailgun config); not sending"
            );
            return Ok(SendOutcome::Disabled);
        }

        let recent =
            email_log::record_and_count_recent_sends(connection, &message.to, purpose).await?;
        if recent > hourly_limit {
            tracing::warn!(
                event_type = "email_rate_limited",
                purpose = purpose,
                recent = recent,
                hourly_limit,
                "Suppressing email: recipient over hourly limit"
            );
            return Ok(SendOutcome::RateLimited);
        }

        self.send(message).await?;
        Ok(SendOutcome::Sent)
    }
}

/// Resolved Mailgun settings. Built from env in `crate::config` (the single
/// env-reading boundary); a `None` here leaves the [`Mailer`] disabled.
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

/// Sends transactional email, or silently drops it when Mailgun isn't
/// configured. Cloneable (the client is `Arc`-backed; the small config is
/// copied) so it can live in `AppState` and be shared by delivery jobs.
#[derive(Clone)]
pub struct Mailer {
    config: Option<MailgunConfig>,
    http: reqwest::Client,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
}

impl Mailer {
    pub fn new(config: Option<MailgunConfig>, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// A disabled mailer that drops every send. Used in tests and anywhere
    /// email must be inert.
    pub fn disabled() -> Self {
        Self {
            config: None,
            http: reqwest::Client::new(),
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    pub async fn acquire_delivery_permit(&self) -> cja::Result<tokio::sync::OwnedSemaphorePermit> {
        Ok(self.semaphore.clone().acquire_owned().await?)
    }

    /// Send a message. When disabled, logs and returns `Ok(())` without any
    /// network call — so callers get uniform behavior whether or not creds
    /// are present. When enabled, POSTs the Mailgun form and errors on a
    /// non-2xx response; delivery jobs propagate that error for retry.
    pub async fn send(&self, message: &EmailMessage) -> cja::Result<()> {
        let Some(config) = &self.config else {
            tracing::info!(subject = %message.subject, "Email disabled (no Mailgun config); not sending");
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
            .timeout(std::time::Duration::from_secs(10))
            .form(&message.form_fields(&config.from))
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

        tracing::info!(subject = %message.subject, "Email sent");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use sqlx::PgPool;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_API_KEY: &str = "key-test";

    fn test_message() -> EmailMessage {
        EmailMessage {
            to: "someone@example.com".to_string(),
            subject: "hi".to_string(),
            text: "body".to_string(),
            html: "<p>body</p>".to_string(),
            list_unsubscribe: None,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn send_limited_caps_per_recipient_per_hour(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        // The cap is the contract: with a limit of 2, exactly 2 requests may
        // ever reach Mailgun no matter how many sends are attempted.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"id\":\"ok\"}"))
            .expect(2)
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri());
        let msg = test_message();
        let mut tx = pool.begin().await?;

        assert_eq!(
            mailer.send_limited(&mut tx, 2, "test", &msg).await?,
            SendOutcome::Sent
        );
        assert_eq!(
            mailer.send_limited(&mut tx, 2, "test", &msg).await?,
            SendOutcome::Sent
        );
        assert_eq!(
            mailer.send_limited(&mut tx, 2, "test", &msg).await?,
            SendOutcome::RateLimited
        );
        // Retrying doesn't help — the suppressed attempts spent budget too.
        assert_eq!(
            mailer.send_limited(&mut tx, 2, "test", &msg).await?,
            SendOutcome::RateLimited
        );
        // (Per-recipient isolation of the window is covered in email_log's
        // own tests; the wiremock .expect(2) enforces the transport cap.)

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn send_limited_disabled_is_a_noop_without_log_rows(pool: PgPool) -> cja::Result<()> {
        let mailer = Mailer::disabled();
        let mut tx = pool.begin().await?;
        assert_eq!(
            mailer
                .send_limited(&mut tx, 5, "test", &test_message())
                .await?,
            SendOutcome::Disabled
        );
        tx.commit().await?;

        let logged: i64 = sqlx::query_scalar!("SELECT COUNT(*) as \"c!\" FROM email_log")
            .fetch_one(&pool)
            .await?;
        assert_eq!(logged, 0);

        Ok(())
    }

    fn configured_mailer(base_url: String) -> Mailer {
        Mailer::new(
            Some(MailgunConfig {
                api_key: TEST_API_KEY.to_string(),
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
        // Asserting the exact value (not just presence) catches a wrong-key
        // regression.
        let expected_auth = format!("Basic {}", BASE64.encode(format!("api:{TEST_API_KEY}")));
        Mock::given(method("POST"))
            .and(path("/v3/mg.example.com/messages"))
            .and(header("authorization", expected_auth.as_str()))
            .and(body_string_contains("someone%40example.com"))
            .and(body_string_contains("subject=hi"))
            .and(body_string_contains("text=body"))
            .and(body_string_contains("html=%3Cp%3Ebody%3C%2Fp%3E"))
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
            html: "<p>body</p>".to_string(),
            list_unsubscribe: None,
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
