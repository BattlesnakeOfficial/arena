//! Discord webhook notifications for community events.
//!
//! [`DiscordNotifier`] is a no-op when no webhook URL is configured
//! (`DISCORD_WEBHOOK_URL` unset/empty), mirroring the Mailer convention:
//! the code compiles and runs everywhere, and sending only becomes real
//! once the URL is set. Callers treat a send as best-effort — a failed
//! notification must never break the operation that triggered it.
//!
//! All payloads include `allowed_mentions: {parse: []}` to suppress
//! Discord's default mention parsing. This prevents `@everyone`/`@here`
//! injection from user-controlled identifiers (snake names, play
//! usernames) that appear in message content.

use color_eyre::eyre::Context as _;

/// Posts community events to a Discord webhook, or silently drops them
/// when no webhook URL is configured. Cloneable (the client is cheap to
/// clone) so it can live in `AppState` and be moved into spawned sends.
#[derive(Clone)]
pub struct DiscordNotifier {
    webhook_url: Option<String>,
    http: reqwest::Client,
}

impl DiscordNotifier {
    pub fn new(webhook_url: Option<String>, http: reqwest::Client) -> Self {
        Self { webhook_url, http }
    }

    /// A disabled notifier that drops every send. Used in tests and
    /// anywhere Discord must be inert.
    pub fn disabled() -> Self {
        Self {
            webhook_url: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.webhook_url.is_some()
    }

    /// Send a message to the Discord webhook. When disabled, logs at info
    /// and returns `Ok(())` without any network call. When enabled, POSTs
    /// the message and errors on a non-2xx response; callers log-and-continue
    /// rather than propagate.
    ///
    /// The payload includes `allowed_mentions: { "parse": [] }` to suppress
    /// all mention parsing — prevents `@everyone`/`@here` injection from
    /// user-controlled content like snake names.
    pub async fn send(&self, content: &str) -> cja::Result<()> {
        let Some(url) = &self.webhook_url else {
            tracing::info!(
                content = content,
                "Discord disabled (no webhook URL); not sending"
            );
            return Ok(());
        };

        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "content": content,
                "allowed_mentions": { "parse": [] }
            }))
            .send()
            .await
            .wrap_err("Failed to send request to Discord webhook")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(cja::color_eyre::eyre::eyre!(
                "Discord webhook returned {status}: {body}"
            ));
        }

        tracing::info!(content = content, "Discord notification sent");
        Ok(())
    }

    /// Notify that a new user signed up via GitHub OAuth. Fire-and-forget:
    /// runs in a spawned task, failures are logged, never propagated.
    pub fn notify_user_signup(&self, github_login: &str) {
        let message = user_signup_message(github_login);
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(e) = notifier.send(&message).await {
                tracing::warn!(error = %e, "Failed to send Discord user-signup notification");
            }
        });
    }

    /// Notify that a play.battlesnake.com account was claimed. Fire-and-forget.
    pub fn notify_account_claimed(&self, play_username: &str, snakes_migrated: usize) {
        let message = account_claimed_message(play_username, snakes_migrated);
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(e) = notifier.send(&message).await {
                tracing::warn!(error = %e, "Failed to send Discord account-claimed notification");
            }
        });
    }

    /// Notify that a new public snake was registered. Fire-and-forget.
    pub fn notify_snake_registered(&self, snake_name: &str, github_login: &str) {
        let message = snake_registered_message(snake_name, github_login);
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(e) = notifier.send(&message).await {
                tracing::warn!(error = %e, "Failed to send Discord snake-registered notification");
            }
        });
    }
}

// --- Message builders (private, testable from in-file tests) ---

fn user_signup_message(github_login: &str) -> String {
    format!("👋 {github_login} just joined Battlesnake Arena")
}

fn account_claimed_message(play_username: &str, snakes_migrated: usize) -> String {
    format!(
        "🎉 {play_username} claimed their play.battlesnake.com account ({snakes_migrated} snake(s) migrated)"
    )
}

fn snake_registered_message(snake_name: &str, github_login: &str) -> String {
    format!("🐍 {snake_name} slithered into the arena (by {github_login})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn configured_notifier(base_url: String) -> DiscordNotifier {
        DiscordNotifier::new(Some(base_url), reqwest::Client::new())
    }

    #[tokio::test]
    async fn disabled_notifier_is_a_noop_ok() {
        let notifier = DiscordNotifier::disabled();
        assert!(!notifier.is_enabled());
        // No network, no panic, just Ok.
        notifier.send("test").await.unwrap();
    }

    #[tokio::test]
    async fn configured_notifier_posts_to_webhook() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("test message"))
            .and(body_string_contains("content"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let notifier = configured_notifier(server.uri());
        notifier.send("test message").await.unwrap();
    }

    #[tokio::test]
    async fn send_includes_allowed_mentions_to_suppress_pings() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("allowed_mentions"))
            .and(body_string_contains("\"parse\":[]"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let notifier = configured_notifier(server.uri());
        notifier.send("test message").await.unwrap();
    }

    #[tokio::test]
    async fn non_2xx_from_discord_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let notifier = configured_notifier(server.uri());
        let err = notifier.send("test").await.unwrap_err();
        assert!(err.to_string().contains("429"));
    }

    #[test]
    fn user_signup_message_contains_login() {
        let msg = user_signup_message("octocat");
        assert!(msg.contains("octocat"));
        assert!(msg.contains("joined"));
    }

    #[test]
    fn account_claimed_message_contains_username_and_count() {
        let msg = account_claimed_message("old_snake_fan", 3);
        assert!(msg.contains("old_snake_fan"));
        assert!(msg.contains("3"));
        assert!(msg.contains("claimed"));
    }

    #[test]
    fn snake_registered_message_contains_name_and_login() {
        let msg = snake_registered_message("Slitherbot", "octocat");
        assert!(msg.contains("Slitherbot"));
        assert!(msg.contains("octocat"));
        assert!(msg.contains("slithered"));
    }
}
