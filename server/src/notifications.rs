use cja::jobs::Job;
use ring::hmac;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{jobs::SendNotificationEmailJob, models::notification};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationChannel {
    InApp,
    Email,
}

impl NotificationChannel {
    pub const ALL: [Self; 2] = [Self::InApp, Self::Email];
    pub fn key(self) -> &'static str {
        match self {
            Self::InApp => "in_app",
            Self::Email => "email",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::InApp => "In app",
            Self::Email => "Email",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "in_app" => Some(Self::InApp),
            "email" => Some(Self::Email),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    SnakeMatchmakingPaused,
    SnakeMatchmakingResumed,
    PlayAccountClaimed,
    ClaimVerification,
    NotificationEmailConfirmation,
}

impl NotificationKind {
    pub const ALL: [Self; 5] = [
        Self::SnakeMatchmakingPaused,
        Self::SnakeMatchmakingResumed,
        Self::PlayAccountClaimed,
        Self::ClaimVerification,
        Self::NotificationEmailConfirmation,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::SnakeMatchmakingPaused => "snake_matchmaking_paused",
            Self::SnakeMatchmakingResumed => "snake_matchmaking_resumed",
            Self::PlayAccountClaimed => "play_account_claimed",
            Self::ClaimVerification => "claim_verification",
            Self::NotificationEmailConfirmation => "notification_email_confirmation",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == value)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::SnakeMatchmakingPaused => "Snake matchmaking paused",
            Self::SnakeMatchmakingResumed => "Snake matchmaking resumed",
            Self::PlayAccountClaimed => "Play account claimed",
            Self::ClaimVerification => "Claim verification",
            Self::NotificationEmailConfirmation => "Notification email confirmation",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::SnakeMatchmakingPaused => {
                "A snake is removed from matchmaking after failed health checks."
            }
            Self::SnakeMatchmakingResumed => "A snake is restored to matchmaking after recovery.",
            Self::PlayAccountClaimed => "Security notice when a play account is claimed.",
            Self::ClaimVerification => "A link to verify a play account claim.",
            Self::NotificationEmailConfirmation => {
                "A link to confirm an address for optional notification emails."
            }
        }
    }
    pub fn email_reason(self) -> &'static str {
        match self {
            Self::SnakeMatchmakingPaused | Self::SnakeMatchmakingResumed => {
                "A snake associated with your Arena account changed matchmaking state."
            }
            Self::PlayAccountClaimed => "A play account registered to this address was claimed.",
            Self::ClaimVerification => {
                "Someone requested a claim for a play account registered to this address."
            }
            Self::NotificationEmailConfirmation => {
                "Someone entered this address for Arena notification emails."
            }
        }
    }
    pub fn toggleable(self) -> bool {
        matches!(
            self,
            Self::SnakeMatchmakingPaused | Self::SnakeMatchmakingResumed
        )
    }
    pub fn supports(self, channel: NotificationChannel) -> bool {
        self.toggleable() || channel == NotificationChannel::Email
    }
    pub fn default_enabled(self, channel: NotificationChannel) -> bool {
        self.supports(channel)
    }
}

pub enum EmailRecipient {
    Override(String),
    Snake(Uuid),
    User,
}
pub struct NewEmail {
    pub subject: Option<String>,
    pub text: Option<String>,
    pub html_body: Option<String>,
    pub recipient: EmailRecipient,
    pub claim_imported_account_id: Option<Uuid>,
    pub notification_email_request_id: Option<Uuid>,
}
pub struct NewNotification {
    pub user_id: Uuid,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub email: Option<NewEmail>,
}
pub struct RecordResult {
    pub notification_id: Uuid,
    pub job_id: Option<Uuid>,
    pub kind: NotificationKind,
}

impl RecordResult {
    pub fn log_enqueued(&self) {
        if let Some(job_id) = self.job_id {
            tracing::info!(event_type = "job_enqueued", %job_id, name = SendNotificationEmailJob::NAME, context = %format!("notification:{}", self.kind.key()), "Notification email job enqueued");
        }
    }
}

/// Accept only paths within this site as notification targets.
pub fn valid_local_link(link: &str) -> bool {
    link.starts_with('/')
        && !link.starts_with("//")
        && !link
            .chars()
            .any(|character| character == '\\' || character.is_ascii_control())
}

pub async fn record_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    event: NewNotification,
) -> cja::Result<RecordResult> {
    use color_eyre::eyre::eyre;
    if event.email.is_some() != event.kind.supports(NotificationChannel::Email) {
        return Err(eyre!("Email payload does not match notification kind"));
    }
    if let Some(email) = &event.email {
        if event.kind.toggleable()
            != matches!(
                email.recipient,
                EmailRecipient::Snake(_) | EmailRecipient::User
            )
        {
            return Err(eyre!(
                "Email recipient source does not match notification kind"
            ));
        }
        if (event.kind == NotificationKind::ClaimVerification)
            != email.claim_imported_account_id.is_some()
            || (event.kind == NotificationKind::NotificationEmailConfirmation)
                != email.notification_email_request_id.is_some()
        {
            return Err(eyre!("Email payload does not match notification kind"));
        }
        if !(email.subject.is_some() == email.text.is_some()
            && email.text.is_some() == email.html_body.is_some())
        {
            return Err(eyre!(
                "Email subject, text, and HTML must be all present or all absent"
            ));
        }
        if event.kind == NotificationKind::NotificationEmailConfirmation
            && (email.subject.is_some() || email.text.is_some())
        {
            return Err(eyre!("Confirmation content must be rendered at delivery"));
        }
    }
    if event
        .link
        .as_ref()
        .is_some_and(|link| !valid_local_link(link))
    {
        return Err(eyre!("Notification link must be a local path"));
    }
    let visible =
        notification::effective_enabled(tx, event.user_id, event.kind, NotificationChannel::InApp)
            .await?;
    let notification_id = sqlx::query_scalar!("INSERT INTO notifications (user_id, kind, title, body, link, in_app_visible) VALUES ($1, $2, $3, $4, $5, $6) RETURNING notification_id", event.user_id, event.kind.key(), event.title, event.body, event.link, visible).fetch_one(&mut **tx).await?;
    let mut job_id = None;
    if let Some(email) = event.email {
        let (recipient_override, battlesnake_id, recipient_user) = match email.recipient {
            EmailRecipient::Override(address) => (Some(address), None, false),
            EmailRecipient::Snake(id) => (None, Some(id), false),
            EmailRecipient::User => (None, None, true),
        };
        sqlx::query!("INSERT INTO notification_emails (notification_id, subject, text, html_body, recipient_override, battlesnake_id, recipient_user, claim_imported_account_id, notification_email_request_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)", notification_id, email.subject, email.text, email.html_body, recipient_override, battlesnake_id, recipient_user, email.claim_imported_account_id, email.notification_email_request_id).execute(&mut **tx).await?;
        let id = Uuid::new_v4();
        let payload = serde_json::to_value(SendNotificationEmailJob { notification_id })?;
        sqlx::query!("INSERT INTO jobs (job_id, name, payload, priority, run_at, created_at, context) VALUES ($1, $2, $3, 0, NOW(), NOW(), $4)", id, SendNotificationEmailJob::NAME, payload, format!("notification:{}", event.kind.key())).execute(&mut **tx).await?;
        job_id = Some(id);
    }
    Ok(RecordResult {
        notification_id,
        job_id,
        kind: event.kind,
    })
}

pub async fn record(pool: &PgPool, event: NewNotification) -> cja::Result<RecordResult> {
    let mut tx = pool.begin().await?;
    let receipt = record_in_tx(&mut tx, event).await?;
    tx.commit().await?;
    receipt.log_enqueued();
    Ok(receipt)
}

fn unsubscribe_key(cookie_key: &cja::server::cookies::CookieKey) -> hmac::Key {
    let root = hmac::Key::new(hmac::HMAC_SHA256, cookie_key.0.signing());
    let digest = hmac::sign(&root, b"arena.notification-unsubscribe.v1");
    hmac::Key::new(hmac::HMAC_SHA256, digest.as_ref())
}

pub fn unsubscribe_token(
    cookie_key: &cja::server::cookies::CookieKey,
    user_id: Uuid,
    kind: NotificationKind,
) -> String {
    let message = format!("v1.{user_id}.{}", kind.key());
    let mac = hmac::sign(&unsubscribe_key(cookie_key), message.as_bytes());
    format!("{message}.{}", hex::encode(mac.as_ref()))
}

pub fn verify_unsubscribe_token(
    cookie_key: &cja::server::cookies::CookieKey,
    token: &str,
) -> Option<(Uuid, NotificationKind)> {
    let mut parts = token.split('.');
    if parts.next()? != "v1" {
        return None;
    }
    let user_part = parts.next()?;
    let user_id = Uuid::parse_str(user_part).ok()?;
    if user_part != user_id.to_string() {
        return None;
    }
    let kind = NotificationKind::parse(parts.next()?)?;
    if !kind.toggleable() {
        return None;
    }
    let mac = hex::decode(parts.next()?).ok()?;
    if parts.next().is_some() {
        return None;
    }
    let canonical = format!("v1.{user_id}.{}", kind.key());
    hmac::verify(&unsubscribe_key(cookie_key), canonical.as_bytes(), &mac).ok()?;
    if token != unsubscribe_token(cookie_key, user_id, kind) {
        return None;
    }
    Some((user_id, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn user(pool: &PgPool) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES (9001673, 'notification-test', '') RETURNING user_id").fetch_one(pool).await?)
    }

    #[test]
    fn catalog_and_tokens() {
        for kind in NotificationKind::ALL {
            assert_eq!(NotificationKind::parse(kind.key()), Some(kind));
            assert!(!kind.name().is_empty());
            assert!(!kind.description().is_empty());
            assert!(kind.supports(NotificationChannel::Email));
            assert!(kind.default_enabled(NotificationChannel::Email));
        }
        assert!(NotificationKind::parse("unknown").is_none());
        assert!(!NotificationKind::ClaimVerification.supports(NotificationChannel::InApp));
        assert_eq!(
            NotificationKind::NotificationEmailConfirmation.key(),
            "notification_email_confirmation"
        );
        assert!(!NotificationKind::NotificationEmailConfirmation.toggleable());
        assert!(
            !NotificationKind::NotificationEmailConfirmation.supports(NotificationChannel::InApp)
        );
        let cookie_key = cja::server::cookies::CookieKey::generate();
        let id = Uuid::new_v4();
        let token = unsubscribe_token(&cookie_key, id, NotificationKind::SnakeMatchmakingPaused);
        assert_eq!(
            verify_unsubscribe_token(&cookie_key, &token),
            Some((id, NotificationKind::SnakeMatchmakingPaused))
        );
        assert!(
            verify_unsubscribe_token(&cookie_key, &token.replace("paused", "resumed")).is_none()
        );
        let message = format!("v1.{id}.snake_matchmaking_paused");
        let raw_key = hmac::Key::new(hmac::HMAC_SHA256, cookie_key.0.signing());
        let raw_token = format!(
            "{message}.{}",
            hex::encode(hmac::sign(&raw_key, message.as_bytes()).as_ref())
        );
        assert!(verify_unsubscribe_token(&cookie_key, &raw_token).is_none());
        assert_ne!(
            hmac::sign(&unsubscribe_key(&cookie_key), message.as_bytes()).as_ref(),
            hmac::sign(&raw_key, message.as_bytes()).as_ref()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn records_snapshot_visibility_and_email_outbox(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool).await?;
        let mut connection = pool.acquire().await?;
        assert!(
            notification::effective_enabled(
                &mut connection,
                user_id,
                NotificationKind::SnakeMatchmakingPaused,
                NotificationChannel::InApp
            )
            .await?
        );
        notification::set_preference(
            &mut connection,
            user_id,
            NotificationKind::SnakeMatchmakingPaused,
            NotificationChannel::InApp,
            false,
        )
        .await?;
        drop(connection);
        let receipt = record(
            &pool,
            NewNotification {
                user_id,
                kind: NotificationKind::SnakeMatchmakingPaused,
                title: "Paused".into(),
                body: "Health checks failed".into(),
                link: Some("/battlesnakes/one/profile".into()),
                email: Some(NewEmail {
                    subject: Some("Paused".into()),
                    text: Some("body".into()),
                    html_body: Some("<p>body</p>".into()),
                    recipient: EmailRecipient::User,
                    claim_imported_account_id: None,
                    notification_email_request_id: None,
                }),
            },
        )
        .await?;
        assert!(receipt.job_id.is_some());
        let row = sqlx::query!("SELECT kind, title, body, link, in_app_visible FROM notifications WHERE notification_id = $1", receipt.notification_id).fetch_one(&pool).await?;
        assert_eq!(row.kind, NotificationKind::SnakeMatchmakingPaused.key());
        assert!(!row.in_app_visible);
        assert_eq!(row.title, "Paused");
        assert_eq!(row.body, "Health checks failed");
        let email_count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notification_emails WHERE notification_id = $1",
            receipt.notification_id
        )
        .fetch_one(&pool)
        .await?;
        let job_count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM jobs WHERE job_id = $1",
            receipt.job_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!((email_count, job_count), (1, 1));
        let in_app_id = sqlx::query_scalar!("INSERT INTO notifications (user_id, kind, title, body, in_app_visible) VALUES ($1, 'future_in_app_only', 'Update', 'body', true) RETURNING notification_id", user_id).fetch_one(&pool).await?;
        let email_count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notification_emails WHERE notification_id = $1",
            in_app_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(email_count, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn invalid_payloads_do_not_record(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool).await?;
        let bad = NewNotification {
            user_id,
            kind: NotificationKind::ClaimVerification,
            title: "Nope".into(),
            body: "Nope".into(),
            link: Some("//evil.example".into()),
            email: None,
        };
        assert!(record(&pool, bad).await.is_err());
        let count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notifications")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        let toggleable_override = NewNotification {
            user_id,
            kind: NotificationKind::SnakeMatchmakingPaused,
            title: "Paused".into(),
            body: "Body".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Paused".into()),
                text: Some("Body".into()),
                html_body: Some("<p>body</p>".into()),
                recipient: EmailRecipient::Override("unconfirmed@example.com".into()),
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        };
        assert!(record(&pool, toggleable_override).await.is_err());
        let partial = NewNotification {
            user_id,
            kind: NotificationKind::SnakeMatchmakingPaused,
            title: "Paused".into(),
            body: "Body".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Paused".into()),
                text: Some("Body".into()),
                html_body: None,
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        };
        assert!(record(&pool, partial).await.is_err());
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notifications")
                .fetch_one(&pool)
                .await?,
            0
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn enqueue_failure_rolls_back_event_and_outbox(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool).await?;
        sqlx::query!("ALTER TABLE jobs ADD CONSTRAINT reject_notification_job CHECK (name <> 'SendNotificationEmailJob')")
            .execute(&pool).await?;
        let result = record(
            &pool,
            NewNotification {
                user_id,
                kind: NotificationKind::SnakeMatchmakingPaused,
                title: "Paused".into(),
                body: "Unhealthy".into(),
                link: None,
                email: Some(NewEmail {
                    subject: Some("Paused".into()),
                    text: Some("body".into()),
                    html_body: Some("<p>body</p>".into()),
                    recipient: EmailRecipient::User,
                    claim_imported_account_id: None,
                    notification_email_request_id: None,
                }),
            },
        )
        .await;
        assert!(result.is_err());
        let events = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notifications")
            .fetch_one(&pool)
            .await?;
        let emails = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notification_emails")
            .fetch_one(&pool)
            .await?;
        assert_eq!((events, emails), (0, 0));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn outbox_checks_rendered_and_deferred_content(pool: PgPool) -> cja::Result<()> {
        let user_id = user(&pool).await?;
        let first = sqlx::query_scalar!("INSERT INTO notifications (user_id, kind, title, body, in_app_visible) VALUES ($1, 'snake_matchmaking_paused', 'Paused', 'body', true) RETURNING notification_id", user_id).fetch_one(&pool).await?;
        assert!(sqlx::query!("INSERT INTO notification_emails (notification_id, subject, text, recipient_override) VALUES ($1, 'Subject', NULL, 'owner@example.com')", first).execute(&pool).await.is_err());
        sqlx::query!("INSERT INTO notification_emails (notification_id, subject, text, recipient_override) VALUES ($1, 'Subject', 'Body', 'owner@example.com')", first).execute(&pool).await?;
        let second = sqlx::query_scalar!("INSERT INTO notifications (user_id, kind, title, body, in_app_visible) VALUES ($1, 'claim_verification', 'Verify', 'body', false) RETURNING notification_id", user_id).fetch_one(&pool).await?;
        let imported_id = Uuid::new_v4();
        sqlx::query!("INSERT INTO notification_emails (notification_id, recipient_override, claim_imported_account_id) VALUES ($1, 'owner@example.com', $2)", second, imported_id).execute(&pool).await?;
        Ok(())
    }
}
