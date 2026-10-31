//! Emails telling play.battlesnake.com players that Battlesnake has a new
//! home (DEV-1983).
//!
//! Three dated waves go to every imported play account that hasn't been
//! claimed and either signed in to play within the last year or still has a
//! snake on an active play leaderboard. Each email lists those leaderboard
//! snakes and links to `/welcome-back`.
//!
//! [`PlayShutdownNoticeJob`] runs from cron every few minutes and sends one
//! small batch for the wave in progress, so a wave drips out over a few
//! hours. Each send is at-most-once per (account, wave): the
//! `play_shutdown_notices` row is written before the email goes out. Nothing
//! sends unless `PLAY_SHUTDOWN_NOTICES_ENABLED=true`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use cja::jobs::Job;
use color_eyre::eyre::Context as _;
use maud::html;
use ring::hmac;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    email::{
        SendOutcome,
        messages::{EmailMessage, action_button, render_layout},
    },
    state::AppState,
};

/// `email_log` purpose for these sends.
pub const PURPOSE: &str = "play_shutdown_notice";
/// Emails per cron run. At one run every [`CRON_INTERVAL_SECS`] this is
/// ~900 an hour, so a wave of a few thousand finishes the same day.
pub const BATCH_SIZE: i64 = 75;
pub const CRON_INTERVAL_SECS: u64 = 5 * 60;
/// Snakes listed by name before "…and N more".
const MAX_LISTED_SNAKES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wave {
    First,
    Second,
    Final,
}

impl Wave {
    pub const ALL: [Wave; 3] = [Wave::First, Wave::Second, Wave::Final];

    pub fn number(self) -> i16 {
        match self {
            Wave::First => 1,
            Wave::Second => 2,
            Wave::Final => 3,
        }
    }

    pub fn from_number(number: i16) -> Option<Wave> {
        Wave::ALL.into_iter().find(|wave| wave.number() == number)
    }

    /// 10:00 ET on Mon Nov 2, Mon Nov 16 and Sun Nov 29, 2026 (15:00 UTC; ET is UTC-5 after
    /// Nov 1).
    pub fn starts_at(self) -> DateTime<Utc> {
        at_unix(match self {
            Wave::First => 1_793_631_600,  // 2026-11-02T15:00:00Z
            Wave::Second => 1_794_841_200, // 2026-11-16T15:00:00Z
            Wave::Final => 1_795_964_400,  // 2026-11-29T15:00:00Z
        })
    }

    /// Each wave sends until the next one starts; the final wave stops when
    /// play.battlesnake.com starts redirecting.
    pub fn ends_at(self) -> DateTime<Utc> {
        match self {
            Wave::First => Wave::Second.starts_at(),
            Wave::Second => Wave::Final.starts_at(),
            Wave::Final => redirect_at(),
        }
    }

    /// The wave being sent at `now`, if any.
    pub fn current(now: DateTime<Utc>) -> Option<Wave> {
        Wave::ALL
            .into_iter()
            .find(|wave| wave.starts_at() <= now && now < wave.ends_at())
    }

    fn subject(self) -> &'static str {
        match self {
            Wave::First => "Battlesnake has a new home",
            Wave::Second => "Your Battlesnake account is waiting on the new site",
            Wave::Final => "play.battlesnake.com redirects to the new site on November 30",
        }
    }

    fn intro(self) -> &'static str {
        match self {
            Wave::First => "Battlesnake has a new home!",
            Wave::Second => "A quick reminder that Battlesnake has a new home.",
            Wave::Final => {
                "play.battlesnake.com redirects to Battlesnake's new home on November 30."
            }
        }
    }
}

/// Midnight ET on Nov 30, 2026, when play.battlesnake.com starts
/// redirecting.
pub fn redirect_at() -> DateTime<Utc> {
    at_unix(1_796_014_800) // 2026-11-30T05:00:00Z
}

fn at_unix(seconds: i64) -> DateTime<Utc> {
    DateTime::UNIX_EPOCH + chrono::TimeDelta::seconds(seconds)
}

/// One of a player's snakes that is on at least one active play leaderboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardSnake {
    pub name: String,
    pub leaderboards: Vec<String>,
}

// Play reads are runtime queries, like the importer's: play's schema isn't
// part of arena's compile-time database.

/// Play accounts the notices go to: signed in within the last year, or own
/// a snake that is active on an active leaderboard.
pub async fn eligible_play_account_ids(play: &PgPool) -> cja::Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT id FROM core_account WHERE last_visit > NOW() - INTERVAL '1 year'
        UNION
        SELECT s.account_id
        FROM arena_leaderboardsnake ls
        JOIN arena_arena a ON a.id = ls.arena_id AND a.is_active
        JOIN core_snake s ON s.id = ls.snake_id
        WHERE ls.is_active AND s.account_id IS NOT NULL
        "#,
    )
    .fetch_all(play)
    .await
    .wrap_err("Failed to read eligible play accounts")
}

/// Each account's snakes on active play leaderboards, keyed by play account
/// id, snakes and leaderboards in name order. Accounts with none are absent.
pub async fn leaderboard_snakes(
    play: &PgPool,
    play_account_ids: &[String],
) -> cja::Result<HashMap<String, Vec<LeaderboardSnake>>> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        r#"
        SELECT s.account_id, s.name, a.name
        FROM arena_leaderboardsnake ls
        JOIN arena_arena a ON a.id = ls.arena_id AND a.is_active
        JOIN core_snake s ON s.id = ls.snake_id
        WHERE ls.is_active AND s.account_id = ANY($1)
        ORDER BY s.account_id, lower(s.name), s.id, lower(a.name)
        "#,
    )
    .bind(play_account_ids)
    .fetch_all(play)
    .await
    .wrap_err("Failed to read play leaderboard snakes")?;

    let mut by_account: HashMap<String, Vec<LeaderboardSnake>> = HashMap::new();
    for (account_id, snake, leaderboard) in rows {
        let snakes = by_account.entry(account_id).or_default();
        match snakes.last_mut() {
            Some(last) if last.name == snake => last.leaderboards.push(leaderboard),
            _ => snakes.push(LeaderboardSnake {
                name: snake,
                leaderboards: vec![leaderboard],
            }),
        }
    }
    Ok(by_account)
}

fn unsubscribe_key(cookie_key: &cja::server::cookies::CookieKey) -> hmac::Key {
    let root = hmac::Key::new(hmac::HMAC_SHA256, cookie_key.0.signing());
    let digest = hmac::sign(&root, b"arena.play-shutdown-notice-unsubscribe.v1");
    hmac::Key::new(hmac::HMAC_SHA256, digest.as_ref())
}

/// Signed, stateless unsubscribe token for an imported play account. These
/// accounts may have no arena user, so the notification tokens don't fit.
pub fn unsubscribe_token(
    cookie_key: &cja::server::cookies::CookieKey,
    imported_account_id: Uuid,
) -> String {
    let message = format!("v1.{imported_account_id}");
    let mac = hmac::sign(&unsubscribe_key(cookie_key), message.as_bytes());
    format!("{message}.{}", hex::encode(mac.as_ref()))
}

pub fn verify_unsubscribe_token(
    cookie_key: &cja::server::cookies::CookieKey,
    token: &str,
) -> Option<Uuid> {
    let mut parts = token.split('.');
    if parts.next()? != "v1" {
        return None;
    }
    let id_part = parts.next()?;
    let imported_account_id = Uuid::parse_str(id_part).ok()?;
    if id_part != imported_account_id.to_string() {
        return None;
    }
    let mac = hex::decode(parts.next()?).ok()?;
    if parts.next().is_some() {
        return None;
    }
    hmac::verify(
        &unsubscribe_key(cookie_key),
        format!("v1.{imported_account_id}").as_bytes(),
        &mac,
    )
    .ok()?;
    Some(imported_account_id)
}

pub fn unsubscribe_url(base_url: &str, token: &str) -> String {
    format!(
        "{}/play-notices/unsubscribe/{token}",
        base_url.trim_end_matches('/')
    )
}

/// Stop all future notices to this account. Idempotent; returns whether the
/// account exists.
pub async fn unsubscribe(db: &PgPool, imported_account_id: Uuid) -> cja::Result<bool> {
    let result = sqlx::query!(
        r#"
        UPDATE imported_accounts
        SET shutdown_notices_unsubscribed_at = COALESCE(shutdown_notices_unsubscribed_at, NOW())
        WHERE imported_account_id = $1
        "#,
        imported_account_id,
    )
    .execute(db)
    .await
    .wrap_err("Failed to record shutdown notice unsubscribe")?;
    Ok(result.rows_affected() == 1)
}

/// Text lines for the leaderboard snake list, capped at [`MAX_LISTED_SNAKES`].
fn snake_lines(snakes: &[LeaderboardSnake]) -> Vec<String> {
    let mut lines: Vec<String> = snakes
        .iter()
        .take(MAX_LISTED_SNAKES)
        .map(|snake| format!("{} ({})", snake.name, snake.leaderboards.join(", ")))
        .collect();
    if snakes.len() > MAX_LISTED_SNAKES {
        lines.push(format!("…and {} more", snakes.len() - MAX_LISTED_SNAKES));
    }
    lines
}

/// The notice for one play account.
pub fn notice_message(
    wave: Wave,
    to: &str,
    username: &str,
    snakes: &[LeaderboardSnake],
    base_url: &str,
    unsubscribe_url: &str,
) -> EmailMessage {
    let welcome_url = format!("{}/welcome-back", base_url.trim_end_matches('/'));
    let carry_over = "Your snakes, profile, and customization unlocks from play.battlesnake.com \
                      can come with you whenever you're ready. Sign in on the new site with \
                      GitHub and it walks you through it.";
    let lines = snake_lines(snakes);
    let leaderboard_note = "Those leaderboards stop when play.battlesnake.com redirects on \
                            November 30. Move your account over and you can enter your snakes \
                            on the new site's leaderboards.";
    let redirect_note = "play.battlesnake.com will redirect to the new site on November 30.";
    let reason = "You're getting this because you have an account on play.battlesnake.com.";

    let mut text = format!("Hi {username},\n\n{}\n\n{carry_over}\n\n", wave.intro());
    if lines.is_empty() {
        text.push_str(&format!("{redirect_note}\n\n"));
    } else {
        text.push_str(
            "These snakes of yours are still on the play.battlesnake.com leaderboards:\n",
        );
        for line in &lines {
            text.push_str(&format!("- {line}\n"));
        }
        text.push_str(&format!("\n{leaderboard_note}\n\n"));
    }
    text.push_str(&format!(
        "Move my account: {welcome_url}\n\n— Battlesnake\n\n{reason}\nUnsubscribe from these emails: {unsubscribe_url}\n"
    ));

    let body = html! {
        p { "Hi " (username) "," }
        p { (wave.intro()) }
        p { (carry_over) }
        @if lines.is_empty() {
            p { (redirect_note) }
        } @else {
            p { "These snakes of yours are still on the play.battlesnake.com leaderboards:" }
            ul { @for line in &lines { li { (line) } } }
            p { (leaderboard_note) }
        }
        (action_button("Move my account", &welcome_url))
        p { "— Battlesnake" }
    };
    let footer = html! {
        p style="margin:0 0 12px" { (reason) }
        p style="margin:0" { a class="email-link" href=(unsubscribe_url) style="color:#C11655;text-decoration:underline" { "Unsubscribe from these emails" } }
    };
    let html = render_layout(
        wave.subject(),
        "Bring your snakes, profile, and unlocks along whenever you're ready.",
        &body.into_string(),
        footer,
    );

    EmailMessage {
        to: to.to_string(),
        subject: wave.subject().to_string(),
        text,
        html,
        list_unsubscribe: Some(format!("<{unsubscribe_url}>")),
    }
}

/// How many accounts a wave would reach right now: eligible on play,
/// unclaimed, subscribed, with an email.
pub async fn eligible_recipient_count(db: &PgPool, eligible: &[String]) -> cja::Result<i64> {
    sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM imported_accounts
        WHERE play_account_id = ANY($1)
          AND claimed_by_user_id IS NULL
          AND shutdown_notices_unsubscribed_at IS NULL
          AND email <> ''
        "#,
        eligible,
    )
    .fetch_one(db)
    .await
    .wrap_err("Failed to count shutdown notice recipients")
}

/// Rows per (wave, status), for the admin page.
pub async fn wave_status_counts(db: &PgPool) -> cja::Result<Vec<(i16, String, i64)>> {
    let rows = sqlx::query!(
        r#"
        SELECT wave, status, COUNT(*) AS "count!"
        FROM play_shutdown_notices
        GROUP BY wave, status
        ORDER BY wave, status
        "#
    )
    .fetch_all(db)
    .await
    .wrap_err("Failed to count shutdown notices")?;
    Ok(rows
        .into_iter()
        .map(|row| (row.wave, row.status, row.count))
        .collect())
}

pub async fn unsubscribed_count(db: &PgPool) -> cja::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM imported_accounts WHERE shutdown_notices_unsubscribed_at IS NOT NULL"#
    )
    .fetch_one(db)
    .await
    .wrap_err("Failed to count shutdown notice unsubscribes")
}

/// Sample snakes for previews and admin test sends.
pub fn sample_snakes() -> Vec<LeaderboardSnake> {
    vec![
        LeaderboardSnake {
            name: "Example Snake".to_string(),
            leaderboards: vec!["Duels".to_string(), "Standard".to_string()],
        },
        LeaderboardSnake {
            name: "Another Snake".to_string(),
            leaderboards: vec!["Royale".to_string()],
        },
    ]
}

struct Recipient {
    imported_account_id: Uuid,
    play_account_id: String,
    email: String,
    username: String,
}

/// The next unclaimed, subscribed, eligible accounts this wave hasn't
/// reached yet.
async fn next_recipients(
    db: &PgPool,
    wave: Wave,
    eligible: &[String],
    limit: i64,
) -> cja::Result<Vec<Recipient>> {
    sqlx::query_as!(
        Recipient,
        r#"
        SELECT ia.imported_account_id, ia.play_account_id, ia.email, ia.username
        FROM imported_accounts ia
        WHERE ia.play_account_id = ANY($1)
          AND ia.claimed_by_user_id IS NULL
          AND ia.shutdown_notices_unsubscribed_at IS NULL
          AND ia.email <> ''
          AND NOT EXISTS (
              SELECT 1 FROM play_shutdown_notices n
              WHERE n.imported_account_id = ia.imported_account_id AND n.wave = $2
          )
        ORDER BY ia.imported_account_id
        LIMIT $3
        "#,
        eligible,
        wave.number(),
        limit,
    )
    .fetch_all(db)
    .await
    .wrap_err("Failed to select shutdown notice recipients")
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchCounts {
    pub sent: usize,
    pub failed: usize,
    /// Claimed or unsubscribed between selection and send.
    pub skipped: usize,
    /// Over the per-recipient hourly limit; retried by a later batch.
    pub deferred: usize,
}

enum NoticeOutcome {
    Sent,
    Failed,
    Skipped,
    Deferred,
    /// No Mailgun config: stop the batch.
    Disabled,
}

async fn send_notice(
    state: &AppState,
    wave: Wave,
    recipient: &Recipient,
    snakes: &[LeaderboardSnake],
) -> cja::Result<NoticeOutcome> {
    // Reserve the (account, wave) row before sending, re-checking the claim
    // and unsubscribe in the same statement. Losing the reservation means
    // another run got here first or the account just moved over.
    let reserved = sqlx::query!(
        r#"
        INSERT INTO play_shutdown_notices (imported_account_id, wave, status)
        SELECT imported_account_id, $2, 'sending'
        FROM imported_accounts
        WHERE imported_account_id = $1
          AND claimed_by_user_id IS NULL
          AND shutdown_notices_unsubscribed_at IS NULL
        ON CONFLICT DO NOTHING
        "#,
        recipient.imported_account_id,
        wave.number(),
    )
    .execute(&state.db)
    .await
    .wrap_err("Failed to reserve shutdown notice")?
    .rows_affected()
        == 1;
    if !reserved {
        return Ok(NoticeOutcome::Skipped);
    }

    let token = unsubscribe_token(&state.cookie_key, recipient.imported_account_id);
    let message = notice_message(
        wave,
        &recipient.email,
        &recipient.username,
        snakes,
        &state.config.base_url,
        &unsubscribe_url(&state.config.base_url, &token),
    );

    let _permit = state.mailer.acquire_delivery_permit().await?;
    let mut tx = state.db.begin().await?;
    let sent = state
        .mailer
        .send_limited(
            &mut tx,
            state.config.email_per_recipient_hourly_limit,
            PURPOSE,
            &message,
        )
        .await;
    tx.commit()
        .await
        .wrap_err("Failed to commit shutdown notice email log")?;

    let (status, outcome) = match sent {
        Ok(SendOutcome::Sent) => (Some("sent"), NoticeOutcome::Sent),
        // Release the reservation so a later batch retries.
        Ok(SendOutcome::RateLimited) => (None, NoticeOutcome::Deferred),
        Ok(SendOutcome::Disabled) => (None, NoticeOutcome::Disabled),
        Err(error) => {
            tracing::warn!(
                imported_account_id = %recipient.imported_account_id,
                wave = wave.number(),
                error = format!("{error:#}"),
                "Shutdown notice failed to send"
            );
            (Some("failed"), NoticeOutcome::Failed)
        }
    };
    match status {
        Some(status) => {
            sqlx::query!(
                "UPDATE play_shutdown_notices SET status = $3, updated_at = NOW()
                 WHERE imported_account_id = $1 AND wave = $2",
                recipient.imported_account_id,
                wave.number(),
                status,
            )
            .execute(&state.db)
            .await
            .wrap_err("Failed to record shutdown notice status")?;
        }
        None => {
            sqlx::query!(
                "DELETE FROM play_shutdown_notices WHERE imported_account_id = $1 AND wave = $2",
                recipient.imported_account_id,
                wave.number(),
            )
            .execute(&state.db)
            .await
            .wrap_err("Failed to release shutdown notice reservation")?;
        }
    }
    Ok(outcome)
}

/// Send up to `limit` notices for `wave`, reading eligibility from `play`.
pub async fn send_batch(
    state: &AppState,
    play: &PgPool,
    wave: Wave,
    limit: i64,
) -> cja::Result<BatchCounts> {
    let eligible = eligible_play_account_ids(play).await?;
    let recipients = next_recipients(&state.db, wave, &eligible, limit).await?;
    let ids: Vec<String> = recipients
        .iter()
        .map(|r| r.play_account_id.clone())
        .collect();
    let snakes = leaderboard_snakes(play, &ids).await?;

    let mut counts = BatchCounts::default();
    for recipient in &recipients {
        let account_snakes = snakes
            .get(&recipient.play_account_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        match send_notice(state, wave, recipient, account_snakes).await? {
            NoticeOutcome::Sent => counts.sent += 1,
            NoticeOutcome::Failed => counts.failed += 1,
            NoticeOutcome::Skipped => counts.skipped += 1,
            NoticeOutcome::Deferred => counts.deferred += 1,
            NoticeOutcome::Disabled => break,
        }
    }
    Ok(counts)
}

/// Cron-driven: sends one batch for the wave in progress, if armed.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlayShutdownNoticeJob;

#[async_trait::async_trait]
impl Job<AppState> for PlayShutdownNoticeJob {
    const NAME: &'static str = "PlayShutdownNoticeJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        if !app_state.config.play_shutdown_notices_enabled {
            return Ok(());
        }
        let Some(wave) = Wave::current(Utc::now()) else {
            return Ok(());
        };
        let Some(play) =
            crate::play_import::connect_play(&app_state.config.database_url, "play").await?
        else {
            tracing::info!("Shutdown notices skipped: play database does not exist");
            return Ok(());
        };
        let result = send_batch(&app_state, &play, wave, BATCH_SIZE).await;
        play.close().await;
        let counts = result?;
        tracing::info!(
            event_type = "play_shutdown_notice_batch",
            wave = wave.number(),
            sent = counts.sent,
            failed = counts.failed,
            skipped = counts.skipped,
            deferred = counts.deferred,
            "Play shutdown notice batch completed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::*;
    use crate::email::{Mailer, MailgunConfig};

    fn snake(name: &str, leaderboards: &[&str]) -> LeaderboardSnake {
        LeaderboardSnake {
            name: name.to_string(),
            leaderboards: leaderboards.iter().map(|l| l.to_string()).collect(),
        }
    }

    #[test]
    fn waves_run_from_nov_2_until_the_redirect() {
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(Wave::current(at("2026-11-02T14:59:59Z")), None);
        assert_eq!(Wave::current(at("2026-11-02T15:00:00Z")), Some(Wave::First));
        assert_eq!(Wave::current(at("2026-11-16T14:59:59Z")), Some(Wave::First));
        assert_eq!(
            Wave::current(at("2026-11-16T15:00:00Z")),
            Some(Wave::Second)
        );
        assert_eq!(Wave::current(at("2026-11-29T15:00:00Z")), Some(Wave::Final));
        assert_eq!(Wave::current(at("2026-11-30T04:59:59Z")), Some(Wave::Final));
        assert_eq!(Wave::current(at("2026-11-30T05:00:00Z")), None);

        // Every wave starts at 10:00 Eastern, and the redirect is midnight.
        let eastern = |t: DateTime<Utc>| t.with_timezone(&cja::chrono_tz::America::New_York);
        for wave in Wave::ALL {
            assert_eq!(
                eastern(wave.starts_at()).format("%H:%M").to_string(),
                "10:00"
            );
            assert_eq!(Wave::from_number(wave.number()), Some(wave));
        }
        assert_eq!(
            eastern(redirect_at()).format("%Y-%m-%d %H:%M").to_string(),
            "2026-11-30 00:00"
        );
    }

    #[test]
    fn unsubscribe_tokens_round_trip_and_reject_tampering() {
        let key = cja::server::cookies::CookieKey::generate();
        let id = Uuid::new_v4();
        let token = unsubscribe_token(&key, id);
        assert_eq!(verify_unsubscribe_token(&key, &token), Some(id));

        let other = unsubscribe_token(&key, Uuid::new_v4());
        let forged = format!(
            "v1.{id}.{}",
            other.rsplit('.').next().expect("token has a mac")
        );
        for bad in [
            forged.as_str(),
            "v1.not-a-uuid.00",
            "v2.x.y",
            "",
            &format!("{token}.extra"),
        ] {
            assert_eq!(verify_unsubscribe_token(&key, bad), None, "{bad}");
        }
        let other_key = cja::server::cookies::CookieKey::generate();
        assert_eq!(verify_unsubscribe_token(&other_key, &token), None);
    }

    #[test]
    fn notice_lists_leaderboard_snakes_and_caps_the_list() {
        let snakes: Vec<_> = (1..=12)
            .map(|n| snake(&format!("Snake {n:02}"), &["Duels", "Standard"]))
            .collect();
        let message = notice_message(
            Wave::First,
            "player@example.com",
            "old-player",
            &snakes,
            "https://arena.example/",
            "https://arena.example/play-notices/unsubscribe/tok",
        );
        assert_eq!(message.to, "player@example.com");
        assert_eq!(message.subject, "Battlesnake has a new home");
        for body in [&message.text, &message.html] {
            assert!(body.contains("old-player"));
            assert!(body.contains("Snake 01 (Duels, Standard)"));
            assert!(body.contains("Snake 10 (Duels, Standard)"));
            assert!(!body.contains("Snake 11"));
            assert!(body.contains("…and 2 more"));
            assert!(body.contains("https://arena.example/welcome-back"));
            assert!(body.contains("https://arena.example/play-notices/unsubscribe/tok"));
            assert!(
                body.contains("whenever you&#39;re ready")
                    || body.contains("whenever you're ready")
            );
        }
        assert_eq!(
            message.list_unsubscribe.as_deref(),
            Some("<https://arena.example/play-notices/unsubscribe/tok>")
        );
    }

    #[test]
    fn notice_without_leaderboard_snakes_states_the_redirect() {
        let message = notice_message(
            Wave::Final,
            "player@example.com",
            "old-player",
            &[],
            "https://arena.example",
            "https://arena.example/play-notices/unsubscribe/tok",
        );
        assert!(!message.text.contains("leaderboards:"));
        assert!(
            message
                .text
                .contains("play.battlesnake.com will redirect to the new site on November 30.")
        );
        let subjects: std::collections::HashSet<_> =
            Wave::ALL.into_iter().map(Wave::subject).collect();
        assert_eq!(subjects.len(), 3, "each wave has its own subject");
    }

    async fn create_play_tables(pool: &PgPool) {
        sqlx::raw_sql(
            r#"
            CREATE TABLE core_account (id TEXT PRIMARY KEY, last_visit TIMESTAMPTZ);
            CREATE TABLE core_snake (id TEXT PRIMARY KEY, account_id TEXT, name TEXT NOT NULL);
            CREATE TABLE arena_arena (id TEXT PRIMARY KEY, name TEXT NOT NULL, is_active BOOLEAN NOT NULL);
            CREATE TABLE arena_leaderboardsnake (
                id TEXT PRIMARY KEY, snake_id TEXT, arena_id TEXT NOT NULL, is_active BOOLEAN NOT NULL
            );
            INSERT INTO arena_arena VALUES ('std', 'Standard', true), ('duel', 'Duels', true), ('old', 'Retired', false);
            "#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn play_account(pool: &PgPool, id: &str, last_visit_days_ago: Option<i32>) {
        sqlx::query(
            "INSERT INTO core_account (id, last_visit) VALUES ($1, NOW() - make_interval(days => $2))",
        )
        .bind(id)
        .bind(last_visit_days_ago)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn on_leaderboard(pool: &PgPool, account: &str, snake: &str, arena: &str, active: bool) {
        sqlx::query("INSERT INTO core_snake (id, account_id, name) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(format!("{account}-{snake}"))
            .bind(account)
            .bind(snake)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO arena_leaderboardsnake (id, snake_id, arena_id, is_active) VALUES ($1, $2, $3, $4)")
            .bind(format!("{account}-{snake}-{arena}"))
            .bind(format!("{account}-{snake}"))
            .bind(arena)
            .bind(active)
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn eligibility_is_a_recent_login_or_an_active_leaderboard_snake(pool: PgPool) {
        create_play_tables(&pool).await;
        play_account(&pool, "recent", Some(30)).await;
        play_account(&pool, "stale", Some(800)).await;
        play_account(&pool, "stale-ladder", Some(800)).await;
        on_leaderboard(&pool, "stale-ladder", "Bot", "std", true).await;
        play_account(&pool, "never-ladder", None).await;
        on_leaderboard(&pool, "never-ladder", "Bot", "duel", true).await;
        play_account(&pool, "stale-retired", Some(800)).await;
        on_leaderboard(&pool, "stale-retired", "Bot", "old", true).await;
        play_account(&pool, "stale-paused", Some(800)).await;
        on_leaderboard(&pool, "stale-paused", "Bot", "std", false).await;

        let mut eligible = eligible_play_account_ids(&pool).await.unwrap();
        eligible.sort();
        assert_eq!(eligible, ["never-ladder", "recent", "stale-ladder"]);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_snakes_group_by_snake_and_skip_inactive(pool: PgPool) {
        create_play_tables(&pool).await;
        play_account(&pool, "a", Some(1)).await;
        on_leaderboard(&pool, "a", "Zed", "std", true).await;
        on_leaderboard(&pool, "a", "Alpha", "std", true).await;
        on_leaderboard(&pool, "a", "Alpha", "duel", true).await;
        on_leaderboard(&pool, "a", "Alpha", "old", true).await;
        on_leaderboard(&pool, "a", "Paused", "std", false).await;

        let snakes = leaderboard_snakes(&pool, &["a".to_string(), "nobody".to_string()])
            .await
            .unwrap();
        assert_eq!(
            snakes.get("a").unwrap(),
            &vec![
                snake("Alpha", &["Duels", "Standard"]),
                snake("Zed", &["Standard"])
            ]
        );
        assert!(!snakes.contains_key("nobody"));
    }

    /// An imported play account; returns its id.
    async fn imported(pool: &PgPool, play_account_id: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, password_hash, is_email_verified)
             VALUES ($1, $1, $2, $1, 'unused', true) RETURNING imported_account_id",
        )
        .bind(play_account_id)
        .bind(format!("{play_account_id}@example.com"))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn status(pool: &PgPool, id: Uuid, wave: Wave) -> Option<String> {
        sqlx::query_scalar(
            "SELECT status FROM play_shutdown_notices WHERE imported_account_id = $1 AND wave = $2",
        )
        .bind(id)
        .bind(wave.number())
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    async fn mailgun_state(pool: &PgPool, server: &MockServer, hourly_limit: i64) -> AppState {
        let mut state = AppState::test_from_pool(pool.clone());
        let config = Arc::get_mut(&mut state.config).expect("unique config Arc");
        config.email_per_recipient_hourly_limit = hourly_limit;
        state.mailer = Mailer::new(
            Some(MailgunConfig {
                api_key: "test".into(),
                domain: "mg.example.com".into(),
                from: "Battlesnake <reply@example.com>".into(),
                base_url: server.uri(),
            }),
            reqwest::Client::new(),
        );
        state
    }

    async fn mailgun() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        server
    }

    async fn sent_bodies(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|request| {
                url::form_urlencoded::parse(&request.body)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn batches_send_once_per_wave_and_skip_claimed_and_unsubscribed(pool: PgPool) {
        create_play_tables(&pool).await;
        for account in ["fresh", "claimed", "unsubscribed", "stale"] {
            play_account(
                &pool,
                account,
                Some(if account == "stale" { 800 } else { 10 }),
            )
            .await;
        }
        on_leaderboard(&pool, "fresh", "Bot", "std", true).await;
        let fresh = imported(&pool, "fresh").await;
        let claimed = imported(&pool, "claimed").await;
        let unsubscribed = imported(&pool, "unsubscribed").await;
        let stale = imported(&pool, "stale").await;
        let user_id = crate::routes::test_support::create_user(&pool, 77001, false).await;
        sqlx::query("UPDATE imported_accounts SET claimed_by_user_id = $2, claimed_at = NOW() WHERE imported_account_id = $1")
            .bind(claimed)
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(unsubscribe(&pool, unsubscribed).await.unwrap());

        let server = mailgun().await;
        let state = mailgun_state(&pool, &server, 5).await;

        let counts = send_batch(&state, &pool, Wave::First, 50).await.unwrap();
        assert_eq!(counts.sent, 1);
        assert_eq!(
            status(&pool, fresh, Wave::First).await.as_deref(),
            Some("sent")
        );
        for skipped in [claimed, unsubscribed, stale] {
            assert_eq!(status(&pool, skipped, Wave::First).await, None);
        }
        let bodies = sent_bodies(&server).await;
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("to=fresh@example.com"));
        assert!(bodies[0].contains("Bot (Standard)"));
        assert!(bodies[0].contains("h:List-Unsubscribe-Post=List-Unsubscribe=One-Click"));

        // The same wave never sends twice.
        let again = send_batch(&state, &pool, Wave::First, 50).await.unwrap();
        assert_eq!(again, BatchCounts::default());
        assert_eq!(sent_bodies(&server).await.len(), 1);

        // The next wave is a separate send.
        let second = send_batch(&state, &pool, Wave::Second, 50).await.unwrap();
        assert_eq!(second.sent, 1);
        assert_eq!(sent_bodies(&server).await.len(), 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rate_limited_sends_are_released_for_a_later_batch(pool: PgPool) {
        create_play_tables(&pool).await;
        play_account(&pool, "fresh", Some(10)).await;
        let fresh = imported(&pool, "fresh").await;
        let server = mailgun().await;

        let limited = mailgun_state(&pool, &server, 0).await;
        let counts = send_batch(&limited, &pool, Wave::First, 50).await.unwrap();
        assert_eq!(counts.deferred, 1);
        assert_eq!(status(&pool, fresh, Wave::First).await, None);
        assert!(sent_bodies(&server).await.is_empty());

        let state = mailgun_state(&pool, &server, 5).await;
        let counts = send_batch(&state, &pool, Wave::First, 50).await.unwrap();
        assert_eq!(counts.sent, 1);
        assert_eq!(
            status(&pool, fresh, Wave::First).await.as_deref(),
            Some("sent")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unsubscribe_link_stops_later_notices(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .layer(tower_cookies::CookieManagerLayer::new());
        let id = imported(&pool, "leaving").await;
        let token = unsubscribe_token(&state.cookie_key, id);

        let send = |method: Method, token: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/play-notices/unsubscribe/{token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };
        assert_eq!(
            send(Method::GET, "v1.garbage.00".into()).await,
            StatusCode::BAD_REQUEST
        );
        let unknown = unsubscribe_token(&state.cookie_key, Uuid::new_v4());
        assert_eq!(send(Method::POST, unknown).await, StatusCode::BAD_REQUEST);

        assert_eq!(send(Method::POST, token.clone()).await, StatusCode::OK);
        assert_eq!(send(Method::GET, token).await, StatusCode::OK);
        let unsubscribed: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT shutdown_notices_unsubscribed_at FROM imported_accounts WHERE imported_account_id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(unsubscribed.is_some());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn job_does_nothing_unless_armed(pool: PgPool) {
        create_play_tables(&pool).await;
        play_account(&pool, "fresh", Some(10)).await;
        imported(&pool, "fresh").await;
        let state = AppState::test_from_pool(pool.clone());
        assert!(!state.config.play_shutdown_notices_enabled);
        PlayShutdownNoticeJob.run(state).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM play_shutdown_notices")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
}
