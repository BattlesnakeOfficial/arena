use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Form, Json, extract::Path, http::header, response::Redirect};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::components::page_factory::PageFactory;
use crate::customizations::{Head, Tail};
use crate::email::{SendOutcome, messages};
use crate::errors::{ServerError, ServerResult, WithStatus};
use crate::models::customization_unlock_code::{self as codes, CreateCode, ValidationError};
use crate::models::moderation_flag::{self, ModerationFlagListing};
use crate::models::{email_log, notification_email};
use crate::notifications::{NotificationChannel, NotificationKind};
use crate::routes::auth::{AdminApiUser, AdminUser};
use crate::state::AppState;

#[derive(Serialize)]
pub struct AdminMetrics {
    pub job_queue: JobQueueMetrics,
    pub jobs_by_name: Vec<JobNameCount>,
    pub game_counts: GameCountMetrics,
    pub games_created: TimeWindowMetrics,
    pub games_finished: TimeWindowMetrics,
    pub avg_game_duration_secs: Option<f64>,
    pub recent_errors: Vec<JobError>,
}

#[derive(Serialize)]
pub struct JobQueueMetrics {
    pub ready: i64,
    pub running: i64,
    pub scheduled: i64,
    pub total: i64,
}

#[derive(Serialize)]
pub struct JobNameCount {
    pub name: String,
    pub count: i64,
}

#[derive(Serialize)]
pub struct GameCountMetrics {
    pub waiting: i64,
    pub running: i64,
    pub finished: i64,
    pub total: i64,
}

#[derive(Serialize)]
pub struct TimeWindowMetrics {
    pub last_hour: i64,
    pub last_24h: i64,
    pub last_7d: i64,
}

#[derive(Serialize)]
pub struct JobError {
    pub name: String,
    pub error_count: i32,
    pub last_error_message: Option<String>,
    pub last_failed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl AdminMetrics {
    async fn fetch(db: &PgPool) -> cja::Result<Self> {
        let job_queue = sqlx::query!(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE locked_at IS NULL AND run_at <= NOW()) as "ready!: i64",
                COUNT(*) FILTER (WHERE locked_at IS NOT NULL) as "running!: i64",
                COUNT(*) FILTER (WHERE locked_at IS NULL AND run_at > NOW()) as "scheduled!: i64",
                COUNT(*) as "total!: i64"
            FROM jobs
            "#
        )
        .fetch_one(db)
        .await?;

        let job_queue = JobQueueMetrics {
            ready: job_queue.ready,
            running: job_queue.running,
            scheduled: job_queue.scheduled,
            total: job_queue.total,
        };

        let jobs_by_name_rows = sqlx::query!(
            r#"
            SELECT name, COUNT(*) as "count!: i64"
            FROM jobs GROUP BY name ORDER BY COUNT(*) DESC
            "#
        )
        .fetch_all(db)
        .await?;

        let jobs_by_name = jobs_by_name_rows
            .into_iter()
            .map(|r| JobNameCount {
                name: r.name,
                count: r.count,
            })
            .collect();

        let game_counts = sqlx::query!(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE status = 'waiting') as "waiting!: i64",
                COUNT(*) FILTER (WHERE status = 'running') as "running!: i64",
                COUNT(*) FILTER (WHERE status = 'finished') as "finished!: i64",
                COUNT(*) as "total!: i64"
            FROM games
            "#
        )
        .fetch_one(db)
        .await?;

        let game_counts = GameCountMetrics {
            waiting: game_counts.waiting,
            running: game_counts.running,
            finished: game_counts.finished,
            total: game_counts.total,
        };

        let games_created = sqlx::query!(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '1 hour') as "last_hour!: i64",
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '24 hours') as "last_24h!: i64",
                COUNT(*) FILTER (WHERE created_at > NOW() - INTERVAL '7 days') as "last_7d!: i64"
            FROM games
            "#
        )
        .fetch_one(db)
        .await?;

        let games_created = TimeWindowMetrics {
            last_hour: games_created.last_hour,
            last_24h: games_created.last_24h,
            last_7d: games_created.last_7d,
        };

        let games_finished = sqlx::query!(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE updated_at > NOW() - INTERVAL '1 hour' AND status = 'finished') as "last_hour!: i64",
                COUNT(*) FILTER (WHERE updated_at > NOW() - INTERVAL '24 hours' AND status = 'finished') as "last_24h!: i64",
                COUNT(*) FILTER (WHERE updated_at > NOW() - INTERVAL '7 days' AND status = 'finished') as "last_7d!: i64"
            FROM games
            "#
        )
        .fetch_one(db)
        .await?;

        let games_finished = TimeWindowMetrics {
            last_hour: games_finished.last_hour,
            last_24h: games_finished.last_24h,
            last_7d: games_finished.last_7d,
        };

        let avg_duration = sqlx::query!(
            r#"
            SELECT AVG(EXTRACT(EPOCH FROM (updated_at - created_at))) as "avg_duration_secs: f64"
            FROM games WHERE status = 'finished' AND updated_at > NOW() - INTERVAL '24 hours'
            "#
        )
        .fetch_one(db)
        .await?;

        let recent_errors_rows = sqlx::query!(
            r#"
            SELECT name, error_count, last_error_message, last_failed_at
            FROM jobs WHERE error_count > 0
            ORDER BY last_failed_at DESC NULLS LAST LIMIT 10
            "#
        )
        .fetch_all(db)
        .await?;

        let recent_errors = recent_errors_rows
            .into_iter()
            .map(|r| JobError {
                name: r.name,
                error_count: r.error_count,
                last_error_message: r.last_error_message,
                last_failed_at: r.last_failed_at,
            })
            .collect();

        Ok(AdminMetrics {
            job_queue,
            jobs_by_name,
            game_counts,
            games_created,
            games_finished,
            avg_game_duration_secs: avg_duration.avg_duration_secs,
            recent_errors,
        })
    }
}

fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else if secs < 3600.0 {
        format!("{:.1}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

pub async fn dashboard(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let metrics = AdminMetrics::fetch(&state.db).await?;

    Ok(page_factory.create_page(
        "Admin Dashboard".to_string(),
        Box::new(html! {
            div {
                h1 { "Admin Dashboard" }

                div style="display:flex;flex-wrap:wrap;gap:8px;margin-bottom:20px;" {
                    a href="/admin" style="display:inline-flex;align-items:center;min-height:44px;box-sizing:border-box;padding: 8px 16px; background: #0066cc; color: white; text-decoration: none; border-radius: 4px;" { "Refresh" }
                    a href="/admin/codes" style="display:inline-flex;align-items:center;min-height:44px;box-sizing:border-box;padding: 8px 16px; background: #666; color: white; text-decoration: none; border-radius: 4px;" { "Unlock codes" }
                    a href="/admin/moderation" style="display:inline-flex;align-items:center;min-height:44px;box-sizing:border-box;padding: 8px 16px; background: #666; color: white; text-decoration: none; border-radius: 4px;" { "Moderation queue" }
                    a href="/admin/emails" style="display:inline-flex;align-items:center;min-height:44px;padding:8px 16px;background:#666;color:white;text-decoration:none;border-radius:4px;box-sizing:border-box;" { "Emails" }
                    a href="/stats" style="display:inline-flex;align-items:center;min-height:44px;box-sizing:border-box;padding: 8px 16px; background: #666; color: white; text-decoration: none; border-radius: 4px;" { "Stats" }
                }

                h2 { "Job Queue" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Status" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Ready" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.job_queue.ready) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Running" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.job_queue.running) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Scheduled" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.job_queue.scheduled) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline); font-weight: bold;" { "Total" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); font-weight: bold;" { (metrics.job_queue.total) }
                    }
                }

                @if !metrics.jobs_by_name.is_empty() {
                    h3 { "Jobs by Type" }
                    table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                        tr {
                            th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Job Name" }
                            th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Count" }
                        }
                        @for job in &metrics.jobs_by_name {
                            tr {
                                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (job.name) }
                                td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (job.count) }
                            }
                        }
                    }
                }

                h2 { "Game Stats" }

                h3 { "By Status" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Status" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Waiting" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.game_counts.waiting) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Running" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.game_counts.running) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Finished" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.game_counts.finished) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline); font-weight: bold;" { "Total" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); font-weight: bold;" { (metrics.game_counts.total) }
                    }
                }

                h3 { "Games Created" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Window" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Last Hour" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.games_created.last_hour) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Last 24 Hours" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.games_created.last_24h) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Last 7 Days" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.games_created.last_7d) }
                    }
                }

                h3 { "Games Finished" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Window" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Last Hour" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.games_finished.last_hour) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Last 24 Hours" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.games_finished.last_24h) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { "Last 7 Days" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (metrics.games_finished.last_7d) }
                    }
                }

                h3 { "Average Game Duration (last 24h)" }
                p {
                    @if let Some(secs) = metrics.avg_game_duration_secs {
                        (format_duration(secs))
                    } @else {
                        "N/A"
                    }
                }

                @if !metrics.recent_errors.is_empty() {
                    h2 { "Recent Job Errors" }
                    div class="table-scroll" {
                        table style="border-collapse: collapse; width: 100%; margin-bottom: 20px;" {
                            tr {
                                th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Job Name" }
                                th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Error Count" }
                                th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Last Error" }
                                th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Last Failed" }
                            }
                            @for err in &metrics.recent_errors {
                                tr {
                                    td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (err.name) }
                                    td style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline);" { (err.error_count) }
                                    td style="padding: 8px; border-bottom: 1px solid var(--hairline); max-width: 400px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;" {
                                        @if let Some(msg) = &err.last_error_message {
                                            (msg)
                                        } @else {
                                            "-"
                                        }
                                    }
                                    td style="padding: 8px; border-bottom: 1px solid var(--hairline);" {
                                        @if let Some(ts) = err.last_failed_at {
                                            (ts.format("%Y-%m-%d %H:%M:%S"))
                                        } @else {
                                            "-"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div style="margin-top: 20px;" {
                    a href="/" { "Back to Home" }
                }
            }
        }),
    ))
}

fn email_kind(key: &str) -> ServerResult<NotificationKind, StatusCode> {
    NotificationKind::parse(key)
        .filter(|kind| kind.supports(NotificationChannel::Email))
        .ok_or_else(|| {
            ServerError(
                cja::color_eyre::eyre::eyre!("Unknown email kind"),
                StatusCode::NOT_FOUND,
            )
        })
}

fn sample_message(
    kind: NotificationKind,
    base_url: &str,
    to: &str,
) -> cja::Result<messages::EmailMessage> {
    let content = messages::sample_notification(kind, base_url)
        .ok_or_else(|| cja::color_eyre::eyre::eyre!("Email kind lacks a sample"))?;
    let unsubscribe = kind.toggleable().then(|| {
        format!(
            "{}/notifications/unsubscribe/sample-invalid-token",
            base_url.trim_end_matches('/')
        )
    });
    messages::finish(
        content,
        to.to_owned(),
        kind,
        base_url,
        unsubscribe.as_deref(),
    )
}

fn preview_content(
    kind: NotificationKind,
    message: &messages::EmailMessage,
    destination: Option<&str>,
    outcome: Option<&str>,
) -> Markup {
    let headers: Vec<_> = message
        .form_fields("")
        .into_iter()
        .filter(|(name, _)| name.starts_with("h:"))
        .collect();
    let path = format!("/admin/emails/{}", kind.key());
    html! {
        div style="min-width:0;max-width:100%;font-size:16px;overflow-wrap:anywhere;" {
            nav { a href="/admin/emails" { "← All emails" } }
            h1 { (kind.name()) }
            p { (kind.description()) }
            @if let Some(outcome) = outcome {
                p role="status" { (outcome) }
            }
            h2 { "Send test" }
            @if let Some(address) = destination {
                p { "Your resolved notification address: " (address) }
            } @else {
                p { "No notification address is available for your account. Add one before sending a test." }
            }
            form method="post" action=(format!("{path}/send-test")) {
                button type="submit" disabled[destination.is_none()] style="min-height:44px;padding:10px 18px;font-size:16px;" { "Send test to me" }
            }
            h2 { "Subject" }
            p { (message.subject) }
            h2 { "Sample recipient" }
            p { (message.to) }
            h2 { "Extra headers" }
            div style="max-width:100%;overflow-wrap:anywhere;" {
                @if headers.is_empty() { "None" }
                @for (name, value) in headers { p { (name) ": " (value) } }
            }
            h2 { "HTML" }
            iframe title="Email HTML preview" sandbox="" srcdoc=(message.html) style="display:block;width:100%;max-width:100%;height:600px;border:1px solid var(--hairline);box-sizing:border-box;" {}
            h2 { "Plain text" }
            pre style="white-space:pre-wrap;overflow-wrap:anywhere;max-width:100%;box-sizing:border-box;overflow-x:auto;font-size:16px;" { (message.text) }
        }
    }
}

pub async fn email_catalog(
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page("Emails".into(), Box::new(html! {
        div style="min-width:0;max-width:100%;font-size:16px;overflow-wrap:anywhere;" {
            h1 { "Emails" }
            p { "Every email Arena sends, with an inert sample preview." }
            @for kind in NotificationKind::ALL.into_iter().filter(|kind| kind.supports(NotificationChannel::Email)) {
                article style="padding:12px 0;border-bottom:1px solid var(--hairline);" {
                    a href=(format!("/admin/emails/{}", kind.key())) style="display:inline-flex;align-items:center;min-height:44px;" { (kind.name()) }
                    p style="margin:0 0 8px;" { (kind.description()) }
                }
            }
        }
    })))
}

pub async fn email_preview(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(key): Path<String>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let kind = email_kind(&key)?;
    let mut connection = state.db.acquire().await?;
    let address = notification_email::resolved_recipient(&mut connection, user.user_id).await?;
    let message = sample_message(kind, &state.config.base_url, "sample@example.com")?;
    Ok(page_factory.create_page(
        kind.name().into(),
        Box::new(preview_content(kind, &message, address.as_deref(), None)),
    ))
}

pub async fn send_test_email(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(key): Path<String>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let kind = email_kind(&key)?;
    let mut tx = state.db.begin().await?;
    let address = notification_email::resolved_recipient(&mut tx, user.user_id).await?;
    let mut outcome = "No notification address is available; no test was sent.".to_string();
    if let Some(to) = &address {
        let message = sample_message(kind, &state.config.base_url, to)?;
        outcome = match state
            .mailer
            .send_limited(
                &mut tx,
                state.config.email_per_recipient_hourly_limit,
                email_log::ADMIN_TEST_PURPOSE,
                &message,
            )
            .await
        {
            Ok(SendOutcome::Sent) => "Sent".into(),
            Ok(SendOutcome::Disabled) => "Disabled (no Mailgun config)".into(),
            Ok(SendOutcome::RateLimited) => "RateLimited".into(),
            Err(err) => {
                tracing::error!(error = %format!("{err:#}"), "Admin test email failed");
                format!("{err:#}")
            }
        };
    }
    let status = match tx.commit().await {
        Ok(()) => StatusCode::OK,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "Admin test email attempt commit failed");
            outcome = if outcome == "Sent" {
                format!("Mailgun accepted the email, but recording the attempt failed: {err:#}")
            } else {
                format!("{outcome}; recording the attempt failed: {err:#}")
            };
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    let message = sample_message(kind, &state.config.base_url, "sample@example.com")?;
    let page = page_factory.create_page(
        kind.name().into(),
        Box::new(preview_content(
            kind,
            &message,
            address.as_deref(),
            Some(&outcome),
        )),
    );
    Ok((status, [(header::CACHE_CONTROL, "no-store")], page))
}

/// GET /admin/moderation — unreviewed moderation flags, newest first.
/// Admin-gated via [`AdminUser`] like every other /admin route. This task
/// ships the list only; review actions (dismiss/confirm) are future work.
pub async fn moderation_queue(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let flags = moderation_flag::list_unreviewed(&state.db, 200).await?;

    let fmt = |v: Option<f64>| match v {
        Some(v) => format!("{v:.2}"),
        None => "—".to_string(),
    };
    let cell = |value: String| {
        maud::html! {
            td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (value) }
        }
    };
    let prob_cell = |v: Option<f64>| cell(fmt(v));

    let rows = flags.iter().map(|flag: &ModerationFlagListing| {
        maud::html! {
            tr {
                td style="padding: 8px; border-bottom: 1px solid var(--hairline); white-space: nowrap;" { (flag.created_at.format("%Y-%m-%d %H:%M")) }
                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (flag.field_kind) }
                // maud escapes the text — the submitted string renders
                // inert even though it's attacker-controlled.
                td style="padding: 8px; border-bottom: 1px solid var(--hairline); max-width: 320px;" { (flag.text) }
                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (flag.user_login) }
                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (flag.decision) }
                (prob_cell(flag.action_confidence))
                (prob_cell(flag.action_block_mass))
                (prob_cell(flag.hate_or_slur))
                (prob_cell(flag.sexual_or_graphic))
                (prob_cell(flag.harassment_or_threat))
                (prob_cell(flag.impersonates_staff_or_platform))
                (prob_cell(flag.disguised_evasion))
                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (flag.action_choice.as_deref().unwrap_or("—")) }
                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (flag.model.as_deref().unwrap_or("—")) }
                td style="padding: 8px; border-bottom: 1px solid var(--hairline);" { (flag.subject_id.map(|id| id.to_string()).unwrap_or_else(|| "—".to_string())) }
            }
        }
    });

    Ok(page_factory
        .create_page(
            "Moderation Queue".to_string(),
            Box::new(maud::html! {
                div {
                    h1 { "Moderation Queue" }
                    div style="margin-bottom: 20px;" {
                        a href="/admin" style="padding: 8px 16px; background: #666; color: white; text-decoration: none; border-radius: 4px;" { "Back to Admin" }
                    }
                    @if flags.is_empty() {
                        p { "No unreviewed moderation flags." }
                    } @else {
                        div class="table-scroll" {
                            table style="border-collapse: collapse; width: 100%; font-size: 13px;" {
                                tr {
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Created" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Field" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Text" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Owner" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Decision" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Conf" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Block mass" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Hate" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Sexual" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Harass" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Impersonate" }
                                    th style="text-align: right; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Evasion" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Action" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Model" }
                                    th style="text-align: left; padding: 8px; border-bottom: 1px solid var(--hairline); background-color: var(--pill);" { "Subject" }
                                }
                                @for row in rows {
                                    (row)
                                }
                            }
                        }
                    }
                }
            }),
        )
        .into_response())
}

pub async fn stats_json(
    State(state): State<AppState>,
    AdminApiUser(_user): AdminApiUser,
) -> Result<impl IntoResponse, StatusCode> {
    let metrics = AdminMetrics::fetch(&state.db).await.map_err(|e| {
        tracing::error!("Failed to fetch admin metrics: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(metrics))
}

#[derive(Deserialize)]
pub struct CreateCodeForm {
    item: String,
    max_redemptions: String,
    expires_at: Option<String>,
    note: Option<String>,
}

fn code_error(page_factory: PageFactory, message: &str) -> axum::response::Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        page_factory.create_page(
            "Invalid unlock code".to_string(),
            Box::new(html! {
                h1 { "Invalid unlock code" }
                p role="alert" { (message) }
                a href="/admin/codes" { "Back to codes" }
            }),
        ),
    )
        .into_response()
}

pub async fn list_codes(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let listings = codes::list(&state.db).await?;
    Ok(page_factory.create_page("Unlock codes".to_string(), Box::new(html! {
        h1 { "Unlock codes" }
        form class="form-stack admin-code-form" method="post" action="/admin/codes" {
            div class="field" {
                label for="code-item" { "Item" }
                select id="code-item" name="item" required {
                @for head in Head::ALL.iter().filter(|h| !h.def().is_free()) {
                    option value=(format!("head:{}", head.slug())) {
                        (head.def().display_name) " (head, " (head.def().group.title()) ")"
                    }
                }
                @for tail in Tail::ALL.iter().filter(|t| !t.def().is_free()) {
                    option value=(format!("tail:{}", tail.slug())) {
                        (tail.def().display_name) " (tail, " (tail.def().group.title()) ")"
                    }
                }
                }
            }
            div class="field" {
                label for="code-max" { "Maximum redemptions" }
                input id="code-max" type="number" min="1" name="max_redemptions" required;
            }
            div class="field" {
                label for="code-expiry" { "Expiry (RFC3339 with timezone, optional)" }
                input id="code-expiry" type="text" name="expires_at" placeholder="2026-10-31T23:59:00-04:00";
            }
            div class="field" {
                label for="code-note" { "Internal note (optional)" }
                input id="code-note" type="text" name="note";
            }
            button type="submit" class="btn solid" { "Create code" }
        }
        div style="overflow-x:auto" { table {
            thead { tr { th { "Item" } th { "Note" } th { "Used / max" } th { "Expiry" } th { "Created" } th { "Status" } } }
            tbody {
                @for code in &listings {
                    tr {
                        td { (code.customization_type) ": " (code.slug) }
                        td { (code.note.as_deref().unwrap_or("—")) }
                        td { (code.redemptions_used) " / " (code.max_redemptions) }
                        td { (code.expires_at.map_or_else(|| "—".to_string(), |date| date.to_rfc3339())) }
                        td { (code.created_at.to_rfc3339()) }
                        td {
                            @if code.disabled_at.is_some() { "Disabled" }
                            @else {
                                form method="post" action=(format!("/admin/codes/{}/disable", code.code_id)) {
                                    button type="submit" { "Disable" }
                                }
                            }
                        }
                    }
                }
            }
        } }
    })))
}

pub async fn create_code(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    page_factory: PageFactory,
    Form(form): Form<CreateCodeForm>,
) -> ServerResult<axum::response::Response, StatusCode> {
    let Some((kind, slug)) = form.item.split_once(':') else {
        return Ok(code_error(page_factory, "Choose a valid item"));
    };
    if slug.contains(':') {
        return Ok(code_error(page_factory, "Choose a valid item"));
    }
    let Ok(max_redemptions) = form.max_redemptions.parse::<i32>() else {
        return Ok(code_error(
            page_factory,
            "Maximum redemptions must be at least 1",
        ));
    };
    let expiry_text = form
        .expires_at
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let expires_at = match expiry_text.map(DateTime::parse_from_rfc3339).transpose() {
        Ok(value) => value.map(|date| date.with_timezone(&Utc)),
        Err(_) => {
            return Ok(code_error(
                page_factory,
                "Expiry must be RFC3339 with timezone",
            ));
        }
    };
    let input = CreateCode {
        customization_type: kind.to_string(),
        slug: slug.to_string(),
        max_redemptions,
        expires_at,
        note: form.note,
    };
    let code = match codes::create(&state.db, user.user_id, &input).await {
        Ok(code) => code,
        Err(error) => {
            if let Some(validation) = error.downcast_ref::<ValidationError>() {
                return Ok(code_error(page_factory, validation.0));
            }
            return Err(error.into());
        }
    };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        page_factory.create_page(
            "Unlock code created".to_string(),
            Box::new(html! {
                h1 { "Unlock code created" }
                p { "Copy this code now. It will not appear again." }
                code { (code) }
                a href="/admin/codes" { "Back to codes" }
            }),
        ),
    )
        .into_response())
}

pub async fn disable_code(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    Path(code_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    if !codes::disable(&state.db, code_id).await? {
        return Err("Unlock code not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }
    Ok(Redirect::to("/admin/codes"))
}

#[cfg(test)]
mod email_tests {
    use super::*;
    use crate::{
        email::{Mailer, MailgunConfig},
        routes::test_support::{create_user_session, session_user_id, signed_session_cookie},
    };
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use sqlx::PgPool;
    use std::collections::BTreeSet;
    use tower::ServiceExt as _;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn request(
        state: &AppState,
        session: Option<Uuid>,
        method_name: &str,
        uri: &str,
        body: &str,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method_name).uri(uri);
        if let Some(session) = session {
            builder = builder.header(
                header::COOKIE,
                format!(
                    "{}={}",
                    crate::models::session::SESSION_COOKIE_NAME,
                    signed_session_cookie(state, session)
                ),
            );
        }
        crate::routes::routes(state.clone())
            .expect("asset router")
            .layer(tower_cookies::CookieManagerLayer::new())
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: axum::response::Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    async fn domain_counts(pool: &PgPool) -> cja::Result<[i64; 8]> {
        let r = sqlx::query!(
            r#"SELECT
            (SELECT COUNT(*) FROM users) AS "users!",
            (SELECT COUNT(*) FROM games) AS "games!",
            (SELECT COUNT(*) FROM customization_grants) AS "grants!",
            (SELECT COUNT(*) FROM notifications) AS "notifications!",
            (SELECT COUNT(*) FROM notification_emails) AS "emails!",
            (SELECT COUNT(*) FROM notification_email_requests) AS "requests!",
            (SELECT COUNT(*) FROM claim_email_tokens) AS "tokens!",
            (SELECT COUNT(*) FROM jobs) AS "jobs!""#
        )
        .fetch_one(pool)
        .await?;
        Ok([
            r.users,
            r.games,
            r.grants,
            r.notifications,
            r.emails,
            r.requests,
            r.tokens,
            r.jobs,
        ])
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn catalog_and_previews_are_admin_only_and_escaped(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let admin = create_user_session(&pool, 1668001, true).await;
        let regular = create_user_session(&pool, 1668002, false).await;
        let before = domain_counts(&pool).await?;
        for path in [
            "/admin/emails",
            "/admin/emails/snake_matchmaking_paused",
            "/admin/emails/snake_matchmaking_paused/send-test",
        ] {
            let method_name = if path.ends_with("send-test") {
                "POST"
            } else {
                "GET"
            };
            assert_eq!(
                request(&state, None, method_name, path, "").await.status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                request(&state, Some(regular), method_name, path, "")
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let catalog = body(request(&state, Some(admin), "GET", "/admin/emails", "").await).await;
        let catalog_keys: BTreeSet<_> = catalog
            .split("href=\"/admin/emails/")
            .skip(1)
            .filter_map(|part| part.split('"').next())
            .filter(|path| !path.contains('/'))
            .collect();
        let expected_keys: BTreeSet<_> = NotificationKind::ALL
            .into_iter()
            .filter(|kind| kind.supports(NotificationChannel::Email))
            .map(NotificationKind::key)
            .collect();
        assert_eq!(catalog_keys, expected_keys);
        for kind in NotificationKind::ALL
            .into_iter()
            .filter(|kind| kind.supports(NotificationChannel::Email))
        {
            assert!(catalog.contains(&format!("/admin/emails/{}", kind.key())));
            let page = body(
                request(
                    &state,
                    Some(admin),
                    "GET",
                    &format!("/admin/emails/{}", kind.key()),
                    "",
                )
                .await,
            )
            .await;
            let expected = sample_message(kind, &state.config.base_url, "sample@example.com")?;
            assert!(page.contains(&expected.subject));
            assert!(page.contains("sandbox=\"\""));
            assert!(page.contains("srcdoc=\""));
            let encoded_html = html! { iframe srcdoc=(expected.html) {} }.into_string();
            let srcdoc = encoded_html
                .split("srcdoc=")
                .nth(1)
                .unwrap()
                .split('>')
                .next()
                .unwrap();
            assert!(page.contains(&format!("srcdoc={srcdoc}")));
            assert!(page.contains(&html! { (expected.text) }.into_string()));
            for (name, value) in expected
                .form_fields("")
                .into_iter()
                .filter(|(name, _)| name.starts_with("h:"))
            {
                assert!(page.contains(&html! { p { (name) ": " (value) } }.into_string()));
            }
            if kind.toggleable() {
                assert!(page.contains("h:List-Unsubscribe"));
                assert!(page.contains("h:List-Unsubscribe-Post"));
            } else {
                assert!(!page.contains("h:List-Unsubscribe"));
            }
            assert!(page.contains("&lt;table role=&quot;presentation&quot;"));
            assert!(!page.contains("<table role=\"presentation\""));
            assert!(!page.contains("class=\"email-cta\""));
            assert!(!page.contains("class=\"email-link\""));
            assert!(page.contains("No notification address is available"));
            assert!(page.contains("disabled"));
        }
        assert_eq!(
            request(&state, Some(admin), "GET", "/admin/emails/unknown", "")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                &state,
                Some(admin),
                "POST",
                "/admin/emails/unknown/send-test",
                ""
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let outcome = request(
            &state,
            Some(admin),
            "POST",
            "/admin/emails/play_account_claimed/send-test",
            "to=other@example.com",
        )
        .await;
        assert_eq!(
            outcome.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert!(body(outcome).await.contains("no test was sent"));
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM email_log")
                .fetch_one(&pool)
                .await?,
            0
        );
        assert_eq!(domain_counts(&pool).await?, before);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_send_uses_only_admin_address_and_test_lane(pool: PgPool) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        let admin = create_user_session(&pool, 1668003, true).await;
        let user_id = session_user_id(&pool, admin).await;
        sqlx::query!("UPDATE users SET github_email = 'github@example.com', notification_email = 'override@example.com' WHERE user_id = $1", user_id).execute(&pool).await?;
        let before = domain_counts(&pool).await?;
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/example.test/messages"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;
        state.mailer = Mailer::new(
            Some(MailgunConfig {
                api_key: "test".into(),
                domain: "example.test".into(),
                from: "Arena <mail@example.test>".into(),
                base_url: mock.uri(),
            }),
            reqwest::Client::new(),
        );
        std::sync::Arc::make_mut(&mut state.config).email_per_recipient_hourly_limit = 1;
        let path = "/admin/emails/snake_matchmaking_paused/send-test?to=attacker@example.com";
        let first = request(
            &state,
            Some(admin),
            "POST",
            path,
            "to=attacker@example.com&email=other@example.com&recipient=bad@example.com",
        )
        .await;
        assert!(body(first).await.contains(">Sent<"));
        let second = request(&state, Some(admin), "POST", path, "").await;
        assert!(body(second).await.contains("RateLimited"));
        let sent = mock.received_requests().await.unwrap();
        assert_eq!(sent.len(), 1);
        let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&sent[0].body)
            .into_owned()
            .collect();
        let expected = sample_message(
            NotificationKind::SnakeMatchmakingPaused,
            &state.config.base_url,
            "override@example.com",
        )?;
        let expected_form: std::collections::HashMap<_, _> = expected
            .form_fields("Arena <mail@example.test>")
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(form, expected_form);
        let rows = sqlx::query!("SELECT recipient, purpose FROM email_log ORDER BY sent_at")
            .fetch_all(&pool)
            .await?;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|r| r.recipient == "override@example.com"
                && r.purpose == email_log::ADMIN_TEST_PURPOSE)
        );
        assert_eq!(domain_counts(&pool).await?, before);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_send_uses_github_then_claimed_play_when_override_is_pending(
        pool: PgPool,
    ) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        let github_admin = create_user_session(&pool, 1668007, true).await;
        let github_user = session_user_id(&pool, github_admin).await;
        sqlx::query!(
            "UPDATE users SET github_email = 'admin@example.com' WHERE user_id = $1",
            github_user
        )
        .execute(&pool)
        .await?;
        let request_id = sqlx::query_scalar!("INSERT INTO notification_email_requests (user_id, email) VALUES ($1, $2) RETURNING request_id", github_user, "pending@example.com").fetch_one(&pool).await?;
        sqlx::query!(
            "UPDATE users SET pending_notification_email_request_id = $2 WHERE user_id = $1",
            github_user,
            request_id
        )
        .execute(&pool)
        .await?;

        let play_admin = create_user_session(&pool, 1668008, true).await;
        let play_user = session_user_id(&pool, play_admin).await;
        sqlx::query!("INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, claimed_by_user_id, is_email_verified) VALUES ('p1','a1',' ','one',$1,true), ('p2','a2','play@example.com','two',$1,false)", play_user).execute(&pool).await?;

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/example.test/messages"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&mock)
            .await;
        state.mailer = Mailer::new(
            Some(MailgunConfig {
                api_key: "test".into(),
                domain: "example.test".into(),
                from: "Arena <mail@example.test>".into(),
                base_url: mock.uri(),
            }),
            reqwest::Client::new(),
        );
        for (index, (admin, expected)) in [
            (github_admin, "admin@example.com"),
            (play_admin, "play@example.com"),
        ]
        .into_iter()
        .enumerate()
        {
            let response = request(
                &state,
                Some(admin),
                "POST",
                "/admin/emails/play_account_claimed/send-test",
                "",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(body(response).await.contains(">Sent<"));
            let requests = mock.received_requests().await.unwrap();
            assert_eq!(requests.len(), index + 1);
            let to = url::form_urlencoded::parse(&requests[index].body)
                .find(|(key, _)| key == "to")
                .unwrap()
                .1;
            assert_eq!(to, expected);
            let rows = sqlx::query!("SELECT recipient, purpose FROM email_log ORDER BY sent_at")
                .fetch_all(&pool)
                .await?;
            assert_eq!(rows.len(), index + 1);
            assert_eq!(rows[index].recipient, expected);
            assert_eq!(rows[index].purpose, email_log::ADMIN_TEST_PURPOSE);
        }
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_send_does_not_log(pool: PgPool) -> cja::Result<()> {
        let state = AppState::test_from_pool(pool.clone());
        let admin = create_user_session(&pool, 1668004, true).await;
        let user_id = session_user_id(&pool, admin).await;
        sqlx::query!(
            "UPDATE users SET github_email = 'admin@example.com' WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;
        let response = request(
            &state,
            Some(admin),
            "POST",
            "/admin/emails/play_account_claimed/send-test",
            "",
        )
        .await;
        assert!(
            body(response)
                .await
                .contains("Disabled (no Mailgun config)")
        );
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM email_log")
                .fetch_one(&pool)
                .await?,
            0
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn mailgun_error_is_shown_and_attempt_is_committed(pool: PgPool) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        let admin = create_user_session(&pool, 1668005, true).await;
        let user_id = session_user_id(&pool, admin).await;
        sqlx::query!(
            "UPDATE users SET github_email = 'admin@example.com' WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/example.test/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("<failure>"))
            .expect(1)
            .mount(&mock)
            .await;
        state.mailer = Mailer::new(
            Some(MailgunConfig {
                api_key: "test".into(),
                domain: "example.test".into(),
                from: "Arena <mail@example.test>".into(),
                base_url: mock.uri(),
            }),
            reqwest::Client::new(),
        );
        let response = request(
            &state,
            Some(admin),
            "POST",
            "/admin/emails/play_account_claimed/send-test",
            "",
        )
        .await;
        let html = body(response).await;
        assert!(html.contains("Mailgun returned 500"));
        assert!(html.contains("&lt;failure&gt;"));
        assert!(!html.contains("<failure>"));
        let count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM email_log WHERE purpose = 'admin_test' AND recipient = 'admin@example.com'").fetch_one(&pool).await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn commit_failure_cannot_claim_sent(pool: PgPool) -> cja::Result<()> {
        let mut state = AppState::test_from_pool(pool.clone());
        let admin = create_user_session(&pool, 1668006, true).await;
        let user_id = session_user_id(&pool, admin).await;
        sqlx::query!(
            "UPDATE users SET github_email = 'admin@example.com' WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"CREATE FUNCTION reject_admin_test_commit() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN RAISE EXCEPTION 'commit <blocked>'; END; $$"#,
        )
        .execute(&pool)
        .await?;
        sqlx::query!("CREATE CONSTRAINT TRIGGER reject_admin_test_commit AFTER INSERT ON email_log DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_admin_test_commit()")
            .execute(&pool).await?;
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/example.test/messages"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;
        state.mailer = Mailer::new(
            Some(MailgunConfig {
                api_key: "test".into(),
                domain: "example.test".into(),
                from: "Arena <mail@example.test>".into(),
                base_url: mock.uri(),
            }),
            reqwest::Client::new(),
        );
        let response = request(
            &state,
            Some(admin),
            "POST",
            "/admin/emails/play_account_claimed/send-test",
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let html = body(response).await;
        assert!(html.contains("Mailgun accepted the email, but recording the attempt failed"));
        assert!(html.contains("commit &lt;blocked&gt;"));
        assert!(!html.contains("commit <blocked>"));
        assert!(!html.contains(">Sent<"));
        assert_eq!(
            sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM email_log")
                .fetch_one(&pool)
                .await?,
            0
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_support::{
        create_user_session, session_user_id, signed_session_cookie,
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use sha2::Digest as _;
    use tower::ServiceExt as _;

    async fn code_request(
        app: &axum::Router,
        method: Method,
        uri: &str,
        cookie: Option<&str>,
        body: &str,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(
                header::COOKIE,
                format!("{}={cookie}", crate::models::session::SESSION_COOKIE_NAME),
            );
        }
        if !body.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: axum::response::Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn code_admin_guard_creation_and_disable(db: PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .layer(tower_cookies::CookieManagerLayer::new());
        let admin_session = create_user_session(&db, 164571, true).await;
        let admin = session_user_id(&db, admin_session).await;
        let admin_cookie = signed_session_cookie(&state, admin_session);
        let regular_session = create_user_session(&db, 164572, false).await;
        let regular_cookie = signed_session_cookie(&state, regular_session);
        let create_body = "item=head%3Ahydra&max_redemptions=1&note=Oct+stream";
        for (cookie, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(regular_cookie.as_str()), StatusCode::FORBIDDEN),
        ] {
            for (method, uri, body) in [
                (Method::GET, "/admin/codes", ""),
                (Method::POST, "/admin/codes", create_body),
                (
                    Method::POST,
                    "/admin/codes/00000000-0000-0000-0000-000000000001/disable",
                    "x=1",
                ),
            ] {
                assert_eq!(
                    code_request(&app, method, uri, cookie, body).await.status(),
                    expected
                );
            }
        }
        let list =
            body(code_request(&app, Method::GET, "/admin/codes", Some(&admin_cookie), "").await)
                .await;
        assert!(list.contains("head:hydra"));
        assert!(list.contains("head:turtle"));
        assert!(list.contains("Special Edition"));
        assert!(list.contains("Getting Started"));
        for invalid in [
            "item=head%3Adefault&max_redemptions=1",
            "item=foo%3Ahydra&max_redemptions=1",
            "item=head%3Ahydra&max_redemptions=0",
            "item=head%3Ahydra&max_redemptions=1&expires_at=bad",
        ] {
            assert_eq!(
                code_request(
                    &app,
                    Method::POST,
                    "/admin/codes",
                    Some(&admin_cookie),
                    invalid
                )
                .await
                .status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        let past_expiry = code_request(
            &app,
            Method::POST,
            "/admin/codes",
            Some(&admin_cookie),
            "item=head%3Ahydra&max_redemptions=1&expires_at=2000-01-01T00%3A00%3A00Z",
        )
        .await;
        assert_eq!(past_expiry.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body(past_expiry)
                .await
                .contains("Expiry must be in the future")
        );
        let created = code_request(
            &app,
            Method::POST,
            "/admin/codes",
            Some(&admin_cookie),
            create_body,
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        assert_eq!(created.headers()[header::CACHE_CONTROL], "no-store");
        let created_html = body(created).await;
        let code = created_html
            .split("<code>")
            .nth(1)
            .unwrap()
            .split("</code>")
            .next()
            .unwrap();
        assert_eq!(code.len(), 12);
        assert_eq!(created_html.matches(code).count(), 1);
        let list =
            body(code_request(&app, Method::GET, "/admin/codes", Some(&admin_cookie), "").await)
                .await;
        assert!(!list.contains(code));
        assert!(!list.contains(&hex::encode(sha2::Sha256::digest(code.as_bytes()))));
        assert!(list.contains("Oct stream"));
        let code_id: Uuid = sqlx::query_scalar("SELECT code_id FROM customization_unlock_codes")
            .fetch_one(&db)
            .await
            .unwrap();
        let player = session_user_id(&db, regular_session).await;
        assert!(matches!(
            codes::redeem(&db, player, code).await.unwrap(),
            codes::RedeemOutcome::Granted { .. }
        ));
        let disable = code_request(
            &app,
            Method::POST,
            &format!("/admin/codes/{code_id}/disable"),
            Some(&admin_cookie),
            "x=1",
        )
        .await;
        assert_eq!(disable.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            code_request(
                &app,
                Method::POST,
                &format!("/admin/codes/{code_id}/disable"),
                Some(&admin_cookie),
                "x=1",
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let third = create_user_session(&db, 164573, false).await;
        assert_eq!(
            codes::redeem(&db, session_user_id(&db, third).await, code)
                .await
                .unwrap(),
            codes::RedeemOutcome::Disabled
        );
        let grants: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM customization_grants WHERE user_id = $1 AND source = 'code'",
        )
        .bind(player)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(grants, 1);
        let _ = admin;
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(0.0), "0.0s");
        assert_eq!(format_duration(1.5), "1.5s");
        assert_eq!(format_duration(59.9), "59.9s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(60.0), "1.0m");
        assert_eq!(format_duration(90.0), "1.5m");
        assert_eq!(format_duration(3599.0), "60.0m");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3600.0), "1.0h");
        assert_eq!(format_duration(7200.0), "2.0h");
    }

    #[test]
    fn test_admin_metrics_serialization() {
        let metrics = AdminMetrics {
            job_queue: JobQueueMetrics {
                ready: 5,
                running: 2,
                scheduled: 10,
                total: 17,
            },
            jobs_by_name: vec![
                JobNameCount {
                    name: "GameRunnerJob".to_string(),
                    count: 12,
                },
                JobNameCount {
                    name: "BackupJob".to_string(),
                    count: 5,
                },
            ],
            game_counts: GameCountMetrics {
                waiting: 3,
                running: 1,
                finished: 100,
                total: 104,
            },
            games_created: TimeWindowMetrics {
                last_hour: 10,
                last_24h: 50,
                last_7d: 200,
            },
            games_finished: TimeWindowMetrics {
                last_hour: 8,
                last_24h: 45,
                last_7d: 190,
            },
            avg_game_duration_secs: Some(12.5),
            recent_errors: vec![],
        };

        let json = serde_json::to_value(&metrics).unwrap();

        assert_eq!(json["job_queue"]["ready"], 5);
        assert_eq!(json["job_queue"]["running"], 2);
        assert_eq!(json["job_queue"]["scheduled"], 10);
        assert_eq!(json["job_queue"]["total"], 17);

        assert_eq!(json["jobs_by_name"][0]["name"], "GameRunnerJob");
        assert_eq!(json["jobs_by_name"][0]["count"], 12);
        assert_eq!(json["jobs_by_name"][1]["name"], "BackupJob");

        assert_eq!(json["game_counts"]["waiting"], 3);
        assert_eq!(json["game_counts"]["running"], 1);
        assert_eq!(json["game_counts"]["finished"], 100);
        assert_eq!(json["game_counts"]["total"], 104);

        assert_eq!(json["games_created"]["last_hour"], 10);
        assert_eq!(json["games_created"]["last_24h"], 50);
        assert_eq!(json["games_created"]["last_7d"], 200);

        assert_eq!(json["games_finished"]["last_hour"], 8);
        assert_eq!(json["games_finished"]["last_24h"], 45);
        assert_eq!(json["games_finished"]["last_7d"], 190);

        assert_eq!(json["avg_game_duration_secs"], 12.5);

        assert!(json["recent_errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_admin_metrics_serialization_null_duration() {
        let metrics = AdminMetrics {
            job_queue: JobQueueMetrics {
                ready: 0,
                running: 0,
                scheduled: 0,
                total: 0,
            },
            jobs_by_name: vec![],
            game_counts: GameCountMetrics {
                waiting: 0,
                running: 0,
                finished: 0,
                total: 0,
            },
            games_created: TimeWindowMetrics {
                last_hour: 0,
                last_24h: 0,
                last_7d: 0,
            },
            games_finished: TimeWindowMetrics {
                last_hour: 0,
                last_24h: 0,
                last_7d: 0,
            },
            avg_game_duration_secs: None,
            recent_errors: vec![],
        };

        let json = serde_json::to_value(&metrics).unwrap();
        assert!(json["avg_game_duration_secs"].is_null());
        assert!(json["jobs_by_name"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_job_error_serialization() {
        let error = JobError {
            name: "GameRunnerJob".to_string(),
            error_count: 3,
            last_error_message: Some("connection timeout".to_string()),
            last_failed_at: Some(
                chrono::DateTime::parse_from_rfc3339("2026-02-08T12:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
        };

        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["name"], "GameRunnerJob");
        assert_eq!(json["error_count"], 3);
        assert_eq!(json["last_error_message"], "connection timeout");
        assert!(json["last_failed_at"].is_string());
    }

    #[test]
    fn test_job_error_serialization_nulls() {
        let error = JobError {
            name: "SomeJob".to_string(),
            error_count: 1,
            last_error_message: None,
            last_failed_at: None,
        };

        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["name"], "SomeJob");
        assert_eq!(json["error_count"], 1);
        assert!(json["last_error_message"].is_null());
        assert!(json["last_failed_at"].is_null());
    }
}
