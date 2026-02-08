use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use maud::html;
use serde::Serialize;
use sqlx::PgPool;

use crate::components::page_factory::PageFactory;
use crate::errors::ServerResult;
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

                div style="margin-bottom: 20px;" {
                    a href="/admin" style="padding: 8px 16px; background: #0066cc; color: white; text-decoration: none; border-radius: 4px;" { "Refresh" }
                }

                h2 { "Job Queue" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Status" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Ready" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.job_queue.ready) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Running" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.job_queue.running) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Scheduled" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.job_queue.scheduled) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd; font-weight: bold;" { "Total" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; font-weight: bold;" { (metrics.job_queue.total) }
                    }
                }

                @if !metrics.jobs_by_name.is_empty() {
                    h3 { "Jobs by Type" }
                    table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                        tr {
                            th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Job Name" }
                            th style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Count" }
                        }
                        @for job in &metrics.jobs_by_name {
                            tr {
                                td style="padding: 8px; border-bottom: 1px solid #ddd;" { (job.name) }
                                td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (job.count) }
                            }
                        }
                    }
                }

                h2 { "Game Stats" }

                h3 { "By Status" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Status" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Waiting" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.game_counts.waiting) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Running" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.game_counts.running) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Finished" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.game_counts.finished) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd; font-weight: bold;" { "Total" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; font-weight: bold;" { (metrics.game_counts.total) }
                    }
                }

                h3 { "Games Created" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Window" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Last Hour" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.games_created.last_hour) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Last 24 Hours" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.games_created.last_24h) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Last 7 Days" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.games_created.last_7d) }
                    }
                }

                h3 { "Games Finished" }
                table style="border-collapse: collapse; width: 100%; max-width: 600px; margin-bottom: 20px;" {
                    tr {
                        th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Window" }
                        th style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Count" }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Last Hour" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.games_finished.last_hour) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Last 24 Hours" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.games_finished.last_24h) }
                    }
                    tr {
                        td style="padding: 8px; border-bottom: 1px solid #ddd;" { "Last 7 Days" }
                        td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (metrics.games_finished.last_7d) }
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
                    table style="border-collapse: collapse; width: 100%; margin-bottom: 20px;" {
                        tr {
                            th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Job Name" }
                            th style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Error Count" }
                            th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Last Error" }
                            th style="text-align: left; padding: 8px; border-bottom: 1px solid #ddd; background-color: #f5f5f5;" { "Last Failed" }
                        }
                        @for err in &metrics.recent_errors {
                            tr {
                                td style="padding: 8px; border-bottom: 1px solid #ddd;" { (err.name) }
                                td style="text-align: right; padding: 8px; border-bottom: 1px solid #ddd;" { (err.error_count) }
                                td style="padding: 8px; border-bottom: 1px solid #ddd; max-width: 400px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;" {
                                    @if let Some(msg) = &err.last_error_message {
                                        (msg)
                                    } @else {
                                        "-"
                                    }
                                }
                                td style="padding: 8px; border-bottom: 1px solid #ddd;" {
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

                div style="margin-top: 20px;" {
                    a href="/" { "Back to Home" }
                }
            }
        }),
    ))
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
