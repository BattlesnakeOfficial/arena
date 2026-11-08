//! Transactional email via Mailgun.
//!
//! [`Mailer`] is a no-op when Mailgun isn't configured (no `MAILGUN_API_KEY`),
//! mirroring play's convention: the code compiles and runs everywhere, and
//! sending only becomes real once credentials are set. Delivery jobs retry
//! transient errors without failing the operation that raised the event.
//!
//! Transport failures are classified at the type level by [`SendError`]: a
//! 4xx other than 429 is a permanent [`SendError::Rejected`] (retrying the
//! identical request cannot succeed). A transport error is definite
//! non-delivery — [`SendError::Transient`] — only when it failed in the
//! connect phase ([`reqwest::Error::is_connect`]: refused, DNS, TLS, connect
//! timeout): no request bytes were written, so Mailgun cannot have the
//! message. Every other transport failure — a response-wait timeout, or the
//! connection dying after the request was on the wire — is
//! [`SendError::NoResponse`]: Mailgun may have accepted it, delivery status
//! unknown. HTTP-level failures that are 5xx, 429, or otherwise unexpected
//! are [`SendError::Transient`] (the server answered, so it did not accept
//! the message); unknown statuses default to transient, safe because the
//! per-recipient hourly cap bounds retries.

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
    /// Mailgun permanently rejected the message (4xx other than 429). The
    /// attempt still spent hourly budget; callers should record a terminal
    /// failure.
    Failed,
}

/// Why a Mailgun send failed, classified by whether a retry can help and
/// whether the message may nonetheless have been delivered. Returned by
/// [`Mailer::send`]; [`Mailer::send_limited`] folds [`SendError::Rejected`]
/// into [`SendOutcome::Failed`] and propagates the other kinds for retry.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// Mailgun permanently rejected the message: a 4xx other than 429
    /// (malformed address, bad credentials, invalid payload). Retrying
    /// the identical request cannot succeed.
    #[error("Mailgun rejected the message: {status}: {body}")]
    Rejected {
        status: reqwest::StatusCode,
        body: String,
    },
    /// No response was received after the request may have been sent: a
    /// response-wait timeout, or the connection lost after Mailgun received
    /// the request (reset, closed mid-response). Mailgun may have accepted
    /// the message: delivery status is unknown. Callers that must not risk
    /// a duplicate (bulk sends to cold addresses) should treat this as if
    /// delivered.
    #[error(transparent)]
    NoResponse(color_eyre::eyre::Report),
    /// The message was definitely not delivered: 5xx, 429, other
    /// non-2xx (the server answered and did not accept it), a connect-phase
    /// failure (connection refused, DNS, TLS, connect timeout — no request
    /// bytes were written), or an internal error before any HTTP attempt.
    /// The caller should retry.
    #[error(transparent)]
    Transient(color_eyre::eyre::Report),
}

impl Mailer {
    /// Send through the per-recipient hourly rate limit (play's safety net:
    /// no address gets more than `hourly_limit` emails/hour, no matter what
    /// triggers them). The attempt is logged and committed **before** the
    /// HTTP call — record-before-check, so concurrent sends see each other,
    /// a suppressed send still spends budget, and a transport error cannot
    /// roll the log row back: every send attempt counts toward the cap. A
    /// permanent Mailgun rejection returns [`SendOutcome::Failed`]
    /// (terminal); no-response and transient failures propagate as
    /// [`SendError`] for retry. Every production send should come through
    /// here; raw [`Mailer::send`] is the transport underneath.
    pub async fn send_limited(
        &self,
        db: &sqlx::PgPool,
        hourly_limit: i64,
        purpose: &str,
        message: &EmailMessage,
    ) -> Result<SendOutcome, SendError> {
        if !self.is_enabled() {
            // Keep the disabled path a true no-op (matching `send`): no log
            // rows for messages that never had a transport.
            tracing::info!(
                subject = %message.subject,
                "Email disabled (no Mailgun config); not sending"
            );
            return Ok(SendOutcome::Disabled);
        }

        // The log row commits before the HTTP call: a send attempt always
        // counts toward the cap, even when the transport errors or the
        // process dies mid-send. The cap — not any caller's transaction —
        // is the retry bound, so a timeout-after-acceptance storm cannot
        // bypass it.
        let recent = {
            let mut tx = db
                .begin()
                .await
                .wrap_err("Failed to begin email log transaction")
                .map_err(SendError::Transient)?;
            let count = email_log::record_and_count_recent_sends(&mut tx, &message.to, purpose)
                .await
                .map_err(SendError::Transient)?;
            tx.commit()
                .await
                .wrap_err("Failed to commit email log transaction")
                .map_err(SendError::Transient)?;
            count
        };
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

        match self.send(message).await {
            Ok(()) => Ok(SendOutcome::Sent),
            Err(SendError::Rejected { status, body }) => {
                tracing::warn!(
                    event_type = "email_rejected",
                    recipient = %message.to,
                    %status,
                    body = %body,
                    "Mailgun permanently rejected the message"
                );
                Ok(SendOutcome::Failed)
            }
            Err(error @ (SendError::NoResponse(_) | SendError::Transient(_))) => Err(error),
        }
    }
}

/// Resolved Mailgun settings. Built from env in `crate::config` (the single
/// env-reading boundary); a `None` here leaves the [`Mailer`] disabled.
#[derive(Clone, Debug)]
pub struct MailgunConfig {
    pub api_key: String,
    /// Sending domain, e.g. `mg.battlesnake.com`.
    pub domain: String,
    /// `From` header, e.g. `Battlesnake <reply@battlesnake.com>`.
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
    /// are present. When enabled, POSTs the Mailgun form and classifies any
    /// failure into [`SendError`]: permanent 4xx rejection, no-response
    /// (delivery unknown), or transient. This method enforces no timeout
    /// itself — the client's timeout governs, so every construction site
    /// must build the client with one (`state.rs` sets a 10s total timeout
    /// and a 5s connect timeout; [`Mailer::disabled`] never sends).
    pub async fn send(&self, message: &EmailMessage) -> Result<(), SendError> {
        let Some(config) = &self.config else {
            tracing::info!(subject = %message.subject, "Email disabled (no Mailgun config); not sending");
            return Ok(());
        };

        if message.to.trim().is_empty() {
            tracing::warn!(subject = %message.subject, "Skipping email with empty recipient");
            return Ok(());
        }

        let url = format!("{}/v3/{}/messages", config.base_url, config.domain);
        let response = match self
            .http
            .post(&url)
            // Mailgun authenticates as `api:<key>` via HTTP basic auth.
            .basic_auth("api", Some(&config.api_key))
            .form(&message.form_fields(&config.from))
            .send()
            .await
        {
            Ok(response) => response,
            // Connect-phase failure (refused, DNS, TLS, connect timeout):
            // no request bytes were written, so the message was definitely
            // not delivered — retry. This is the *only* transport arm that
            // is definite non-delivery: `state.rs` sets `connect_timeout`
            // alongside the client's total timeout, so a connect-phase
            // deadline surfaces as a connect error here. (A total-timeout
            // deadline that fires while still connecting has no connect
            // source and correctly falls through to the ambiguous arm.)
            Err(error) if error.is_connect() => {
                return Err(SendError::Transient(
                    color_eyre::eyre::Report::new(error).wrap_err("Failed to connect to Mailgun"),
                ));
            }
            // Every other transport failure — a response-wait timeout, or
            // the connection lost after the request was on the wire — leaves
            // delivery status unknown: Mailgun may have accepted it.
            Err(error) => {
                return Err(SendError::NoResponse(
                    color_eyre::eyre::Report::new(error).wrap_err("No response from Mailgun"),
                ));
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(if status.is_client_error() && status.as_u16() != 429 {
                SendError::Rejected { status, body }
            } else {
                // 5xx, 429, and any unexpected status: retry, bounded by
                // the per-recipient hourly cap.
                SendError::Transient(color_eyre::eyre::eyre!("Mailgun returned {status}: {body}"))
            });
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
            tags: Vec::new(),
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn send_limited_caps_per_recipient_per_hour(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        // The cap is the contract: with a limit of 2, exactly 2 requests may
        // ever reach Mailgun no matter how many sends are attempted. The log
        // row now commits before the HTTP call, so failed attempts spend
        // budget too — a transport error can no longer hide an attempt.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"id\":\"ok\"}"))
            .expect(2)
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri(), reqwest::Client::new());
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

    fn configured_mailer(base_url: String, http: reqwest::Client) -> Mailer {
        Mailer::new(
            Some(MailgunConfig {
                api_key: TEST_API_KEY.to_string(),
                domain: "mg.example.com".to_string(),
                from: "Arena <reply@example.com>".to_string(),
                base_url,
            }),
            http,
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

        let mailer = configured_mailer(server.uri(), reqwest::Client::new());
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

        let mailer = configured_mailer(server.uri(), reqwest::Client::new());
        let msg = EmailMessage {
            to: "   ".to_string(),
            subject: "hi".to_string(),
            text: "body".to_string(),
            html: "<p>body</p>".to_string(),
            list_unsubscribe: None,
            tags: Vec::new(),
        };
        mailer.send(&msg).await.unwrap();
    }

    #[tokio::test]
    async fn permanent_4xx_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri(), reqwest::Client::new());
        let err = mailer.send(&test_message()).await.unwrap_err();
        assert!(matches!(err, SendError::Rejected { .. }));
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn rate_limit_and_server_errors_are_transient() {
        for status in [429u16, 503] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;

            let mailer = configured_mailer(server.uri(), reqwest::Client::new());
            let err = mailer.send(&test_message()).await.unwrap_err();
            assert!(
                matches!(err, SendError::Transient(_)),
                "{status} should classify as Transient, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn connection_refusal_is_transient_not_no_response() {
        // Bind a listener, note the port, then drop it: connections to the
        // freed port are refused — the request never reached the wire.
        // (A dropped wiremock MockServer won't do: pooled servers keep
        // answering 404.)
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mailer = configured_mailer(format!("http://127.0.0.1:{port}"), reqwest::Client::new());
        let err = mailer.send(&test_message()).await.unwrap_err();
        assert!(matches!(err, SendError::Transient(_)), "got {err:?}");
        assert!(!matches!(err, SendError::NoResponse(_)));
    }

    #[tokio::test]
    async fn connection_lost_after_request_received_is_not_definite_non_delivery() {
        use tokio::io::AsyncReadExt as _;

        // A server that reads the entire request, then drops the connection
        // before responding: Mailgun may have accepted the message, so this
        // cannot be classified as `Transient` ("definitely not delivered").
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..read]);
                let Some(head_end) = received.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&received[..head_end]).to_ascii_lowercase();
                let body_length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                if received.len() >= head_end + 4 + body_length {
                    break;
                }
            }
            String::from_utf8_lossy(&received).to_string()
        });

        let mailer = configured_mailer(format!("http://127.0.0.1:{port}"), reqwest::Client::new());
        let err = mailer.send(&test_message()).await.unwrap_err();

        let received = server.await.unwrap();
        assert!(
            received.contains("someone%40example.com"),
            "the server must have received the full message: {received}"
        );
        assert!(
            !matches!(err, SendError::Transient(_)),
            "request reached the server, so delivery is unknown, got {err:?}"
        );
    }

    #[tokio::test]
    async fn timeout_classified_as_no_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(400)),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(150))
            .build()
            .unwrap();
        let mailer = configured_mailer(server.uri(), client);
        let err = mailer.send(&test_message()).await.unwrap_err();
        assert!(matches!(err, SendError::NoResponse(_)), "got {err:?}");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn permanent_failure_is_a_failed_outcome_and_is_logged(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1)
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri(), reqwest::Client::new());
        let outcome = mailer
            .send_limited(&pool, 5, "test", &test_message())
            .await?;
        assert_eq!(outcome, SendOutcome::Failed);

        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_log")
            .fetch_one(&pool)
            .await?;
        assert_eq!(logged, 1, "a rejected attempt still spent budget");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn transient_failure_still_logs_the_attempt(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let mailer = configured_mailer(server.uri(), reqwest::Client::new());
        let err = mailer
            .send_limited(&pool, 5, "test", &test_message())
            .await
            .unwrap_err();
        assert!(matches!(err, SendError::Transient(_)));

        // The core regression: the log row survives the transport error, so
        // the attempt counts toward the cap even though the send failed.
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_log")
            .fetch_one(&pool)
            .await?;
        assert_eq!(logged, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn timeouts_after_acceptance_count_toward_the_cap(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        // A 200 that always arrives slower than the client timeout: Mailgun
        // "accepted" each request, but the client never learns. Only two
        // requests may ever reach the mock, no matter how many sends are
        // attempted.
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(std::time::Duration::from_millis(400)),
            )
            .expect(2)
            .mount(&server)
            .await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(150))
            .build()?;
        let mailer = configured_mailer(server.uri(), client);
        let msg = test_message();

        // Two no-response failures: wiremock received both requests, so
        // both attempts are logged and spent budget.
        assert!(matches!(
            mailer.send_limited(&pool, 2, "test", &msg).await,
            Err(SendError::NoResponse(_))
        ));
        assert!(matches!(
            mailer.send_limited(&pool, 2, "test", &msg).await,
            Err(SendError::NoResponse(_))
        ));
        // The third attempt is suppressed by the cap before any HTTP call.
        assert_eq!(
            mailer.send_limited(&pool, 2, "test", &msg).await?,
            SendOutcome::RateLimited
        );
        Ok(())
    }
}
