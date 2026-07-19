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

use crate::models::email_log;
use crate::models::imported_account::ClaimSummary;

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
        pool: &sqlx::PgPool,
        hourly_limit: i64,
        purpose: &str,
        message: &EmailMessage,
    ) -> cja::Result<SendOutcome> {
        if !self.is_enabled() {
            // Keep the disabled path a true no-op (matching `send`): no log
            // rows for messages that never had a transport.
            tracing::info!(
                to = %message.to,
                subject = %message.subject,
                "Email disabled (no Mailgun config); not sending"
            );
            return Ok(SendOutcome::Disabled);
        }

        let recent = email_log::record_and_count_recent_sends(pool, &message.to, purpose).await?;
        if recent > hourly_limit {
            tracing::warn!(
                event_type = "email_rate_limited",
                to = %message.to,
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

    /// Notify a play account's owner that their account was claimed on
    /// arena. Fire-and-forget: the send runs in a spawned task so it never
    /// adds latency to the claim/login response, and a failure is logged,
    /// never propagated, so it can't fail the claim that triggered it.
    pub fn notify_account_claimed(
        &self,
        pool: &sqlx::PgPool,
        hourly_limit: i64,
        summary: &ClaimSummary,
    ) {
        let message = messages::account_claimed(
            &summary.email,
            &summary.username,
            summary.snakes_created,
            summary.grants_created,
        );
        self.spawn_limited_send(pool.clone(), hourly_limit, "account_claimed", message);
    }

    /// Notify a snake's owner that the health sweeper pulled their snake
    /// from leaderboard matchmaking. Same fire-and-forget contract as
    /// [`Mailer::notify_account_claimed`]: the sweep never waits on or fails
    /// because of an email.
    pub fn notify_matchmaking_deactivated(
        &self,
        pool: &sqlx::PgPool,
        hourly_limit: i64,
        to_email: &str,
        snake_name: &str,
        failure_summary: &str,
        profile_url: &str,
    ) {
        let message =
            messages::matchmaking_deactivated(to_email, snake_name, failure_summary, profile_url);
        self.spawn_limited_send(
            pool.clone(),
            hourly_limit,
            "matchmaking_deactivated",
            message,
        );
    }

    /// Notify a snake's owner that the sweeper put their recovered snake
    /// back into matchmaking automatically. Fire-and-forget like
    /// [`Mailer::notify_matchmaking_deactivated`].
    pub fn notify_matchmaking_reactivated(
        &self,
        pool: &sqlx::PgPool,
        hourly_limit: i64,
        to_email: &str,
        snake_name: &str,
        profile_url: &str,
    ) {
        let message = messages::matchmaking_reactivated(to_email, snake_name, profile_url);
        self.spawn_limited_send(
            pool.clone(),
            hourly_limit,
            "matchmaking_reactivated",
            message,
        );
    }

    /// Send the email-recovery magic link. Fire-and-forget on purpose: the
    /// requesting handler must respond identically whether or not the email
    /// matched an account, so it can never wait on (or fail with) the
    /// transport.
    pub fn notify_claim_verification(
        &self,
        pool: &sqlx::PgPool,
        hourly_limit: i64,
        to_email: &str,
        play_username: &str,
        verify_url: &str,
    ) {
        let message = messages::claim_verification(to_email, play_username, verify_url);
        self.spawn_limited_send(pool.clone(), hourly_limit, "claim_verification", message);
    }

    /// The shared fire-and-forget tail: run `send_limited` in a spawned
    /// task, log any failure, never propagate.
    fn spawn_limited_send(
        &self,
        pool: sqlx::PgPool,
        hourly_limit: i64,
        purpose: &'static str,
        message: EmailMessage,
    ) {
        let mailer = self.clone();
        tokio::spawn(async move {
            if let Err(e) = mailer
                .send_limited(&pool, hourly_limit, purpose, &message)
                .await
            {
                tracing::warn!(
                    to = %message.to,
                    purpose = purpose,
                    error = %e,
                    "Failed to send email"
                );
            }
        });
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
    use sqlx::PgPool;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_message() -> EmailMessage {
        EmailMessage {
            to: "someone@example.com".to_string(),
            subject: "hi".to_string(),
            text: "body".to_string(),
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

        assert_eq!(
            mailer.send_limited(&pool, 2, "test", &msg).await?,
            SendOutcome::Sent
        );
        assert_eq!(
            mailer.send_limited(&pool, 2, "test", &msg).await?,
            SendOutcome::Sent
        );
        assert_eq!(
            mailer.send_limited(&pool, 2, "test", &msg).await?,
            SendOutcome::RateLimited
        );
        // Retrying doesn't help — the suppressed attempts spent budget too.
        assert_eq!(
            mailer.send_limited(&pool, 2, "test", &msg).await?,
            SendOutcome::RateLimited
        );
        // (Per-recipient isolation of the window is covered in email_log's
        // own tests; the wiremock .expect(2) enforces the transport cap.)

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn send_limited_disabled_is_a_noop_without_log_rows(pool: PgPool) -> cja::Result<()> {
        let mailer = Mailer::disabled();
        assert_eq!(
            mailer
                .send_limited(&pool, 5, "test", &test_message())
                .await?,
            SendOutcome::Disabled
        );

        let logged: i64 = sqlx::query_scalar!("SELECT COUNT(*) as \"c!\" FROM email_log")
            .fetch_one(&pool)
            .await?;
        assert_eq!(logged, 0);

        Ok(())
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
