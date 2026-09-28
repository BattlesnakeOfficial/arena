use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use chrono::{Duration, Utc};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};

use crate::{
    components::page_factory::PageFactory, errors::ServerResult, models::stats::StatsSnapshot,
    state::AppState,
};

async fn snapshot(state: &AppState) -> cja::Result<Arc<StatsSnapshot>> {
    snapshot_for_day(state, Utc::now().date_naive()).await
}

async fn snapshot_for_day(
    state: &AppState,
    today_utc: chrono::NaiveDate,
) -> cja::Result<Arc<StatsSnapshot>> {
    let as_of = today_utc - Duration::days(1);
    if let Some(cached) = state.stats_cache.get()
        && cached.as_of_utc_date == as_of
    {
        return Ok(cached);
    }
    let fresh = StatsSnapshot::fetch(&state.db, today_utc).await?;
    Ok(state.stats_cache.put(fresh))
}

pub async fn stats_json(
    State(state): State<AppState>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let stats = snapshot(&state)
        .await
        .wrap_err("Failed to fetch public stats")?;
    Ok(Json(stats.as_ref().clone()))
}

pub async fn stats_page(
    State(state): State<AppState>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let stats = snapshot(&state)
        .await
        .wrap_err("Failed to fetch public stats")?;
    Ok(page_factory.create_page("Stats".to_string(), Box::new(render_stats(&stats))))
}

fn chart(id: &str, title: &str, description: &str, values: &[[i64; 3]]) -> Markup {
    let max = values
        .iter()
        .map(|v| v.iter().sum::<i64>())
        .max()
        .unwrap_or(0)
        .max(1);
    let bar_step = 720.0 / values.len().max(1) as f64;
    let width = (bar_step - 1.0).max(1.0);
    let mut bars = Vec::new();
    for (i, amounts) in values.iter().enumerate() {
        let mut bottom = 200.0;
        for (series, amount) in amounts.iter().enumerate() {
            let height = (*amount as f64 / max as f64) * 180.0;
            bottom -= height;
            if *amount > 0 {
                bars.push((i as f64 * bar_step, bottom, width, height, series));
            }
        }
    }
    let label_id = format!("{id}-title");
    let desc_id = format!("{id}-desc");
    let labelled_by = format!("{label_id} {desc_id}");
    html! {
        svg class="public-stats-chart" viewBox="0 0 720 220" role="img" aria-labelledby=(labelled_by) {
            title id=(label_id) { (title) }
            desc id=(desc_id) { (description) }
            line x1="0" y1="200" x2="720" y2="200" class="chart-axis" {}
            @for (x, y, width, height, series) in bars {
                rect x=(format!("{x:.2}")) y=(format!("{y:.2}"))
                     width=(format!("{width:.2}")) height=(format!("{height:.2}"))
                     class=(format!("chart-series-{series}")) {}
            }
        }
    }
}

fn render_stats(s: &StatsSnapshot) -> Markup {
    let dau: Vec<_> = s
        .daily_active_users
        .iter()
        .map(|r| [r.count, 0, 0])
        .collect();
    let wau: Vec<_> = s
        .weekly_active_users
        .iter()
        .map(|r| [r.count, 0, 0])
        .collect();
    let daily_games: Vec<_> = s
        .daily_games
        .iter()
        .map(|r| [r.custom, r.leaderboard, r.tournament])
        .collect();
    let weekly_games: Vec<_> = s
        .weekly_games
        .iter()
        .map(|r| [r.custom, r.leaderboard, r.tournament])
        .collect();
    let growth: Vec<_> = s
        .weekly_growth
        .iter()
        .map(|r| [r.new_users, r.new_snakes, 0])
        .collect();
    let snakes: Vec<_> = s
        .weekly_active_snakes
        .iter()
        .map(|r| [r.count, 0, 0])
        .collect();
    let h = &s.headlines;
    html! {
        div class="public-stats" {
            div class="page-head" {
                h1 { "Arena stats" }
                p class="sub" { "A public view of the people and snakes building this arena." }
            }
            p class="stats-period" { "UTC; through " (s.as_of_utc_date) }
            div class="stats public-stats-tiles" {
                (tile("DAU", h.dau.to_string(), "Last complete day"))
                (tile("WAU", h.wau.to_string(), "Trailing 7 complete days"))
                (tile("MAU", h.mau.to_string(), "Trailing 28 complete days"))
                (tile("DAU / MAU", format!("{:.1}%", h.dau_mau_percent), "28-day average DAU / MAU"))
                (tile("Games played", h.games_7d.to_string(), "Trailing 7 complete days"))
                (tile("Active snakes", h.active_snakes_7d.to_string(), "Trailing 7 complete days"))
                (tile("Registered users", h.registered_users.to_string(), "Through last complete day"))
                (tile("Total snakes", h.total_snakes.to_string(), "Through last complete day"))
            }
            div class="public-stats-note" {
                p { "Live activity tracking starts on " (s.live_tracking_started_on) " UTC. Earlier account activity is reconstructed from durable records and is a lower bound." }
                @if !s.backfill_complete {
                    p { "Historical reconstruction in progress." }
                }
                p { "An active user is an account making at least one signed-in web request or API-token request during a UTC day. Automated API clients count." }
                p { "A played game finished on Arena, excludes legacy imports, and is dated by game creation. An active snake appeared in a played game during the period." }
                p { "Read the " a href="/privacy" { "privacy policy" } " for how activity dates are stored." }
            }

            section class="public-stats-section" {
                h2 { "Daily active users" }
                (chart("daily-users", "Daily active users", "Last 90 complete UTC days", &dau))
                div class="public-stats-table-wrap" {
                    table class="public-stats-table" {
                        caption { "Daily active users, last 90 complete UTC days" }
                        thead { tr { th scope="col" { "Date (UTC)" } th scope="col" { "Active users" } } }
                        tbody { @for row in &s.daily_active_users { tr { th scope="row" { (row.date) } td { (row.count) } } } }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Weekly active users" }
                (chart("weekly-users", "Weekly active users", "Last 52 complete ISO weeks", &wau))
                div class="public-stats-table-wrap" {
                    table class="public-stats-table" {
                        caption { "Weekly active users, last 52 complete ISO weeks" }
                        thead { tr { th scope="col" { "Week starting Monday (UTC)" } th scope="col" { "Active users" } } }
                        tbody { @for row in &s.weekly_active_users { tr { th scope="row" { (row.week_start) } td { (row.count) } } } }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Games per day" }
                (chart("daily-games", "Games played per day", "Custom, leaderboard, and tournament games over the last 90 complete UTC days", &daily_games))
                (game_legend())
                div class="public-stats-table-wrap" {
                    table class="public-stats-table" {
                        caption { "Games played per day, last 90 complete UTC days" }
                        thead { tr { th scope="col" { "Date (UTC)" } th scope="col" { "Custom" } th scope="col" { "Leaderboard" } th scope="col" { "Tournament" } } }
                        tbody { @for row in &s.daily_games { tr { th scope="row" { (row.date) } td { (row.custom) } td { (row.leaderboard) } td { (row.tournament) } } } }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Games per week" }
                (chart("weekly-games", "Games played per week", "Custom, leaderboard, and tournament games over the last 52 complete ISO weeks", &weekly_games))
                (game_legend())
                div class="public-stats-table-wrap" {
                    table class="public-stats-table" {
                        caption { "Games played per week, last 52 complete ISO weeks" }
                        thead { tr { th scope="col" { "Week starting Monday (UTC)" } th scope="col" { "Custom" } th scope="col" { "Leaderboard" } th scope="col" { "Tournament" } } }
                        tbody { @for row in &s.weekly_games { tr { th scope="row" { (row.week_start) } td { (row.custom) } td { (row.leaderboard) } td { (row.tournament) } } } }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Community growth" }
                (chart("growth", "New users and snakes per week", "New users and snakes over the last 52 complete ISO weeks", &growth))
                p class="public-stats-legend" { span class="key series-0" {} "New users" span class="key series-1" {} "New snakes" }
                div class="public-stats-table-wrap" {
                    table class="public-stats-table" {
                        caption { "New users and snakes per week, last 52 complete ISO weeks" }
                        thead { tr { th scope="col" { "Week starting Monday (UTC)" } th scope="col" { "New users" } th scope="col" { "New snakes" } } }
                        tbody { @for row in &s.weekly_growth { tr { th scope="row" { (row.week_start) } td { (row.new_users) } td { (row.new_snakes) } } } }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Active snakes per week" }
                (chart("weekly-snakes", "Active snakes per week", "Distinct snakes in played games over the last 52 complete ISO weeks", &snakes))
                div class="public-stats-table-wrap" {
                    table class="public-stats-table" {
                        caption { "Active snakes per week, last 52 complete ISO weeks" }
                        thead { tr { th scope="col" { "Week starting Monday (UTC)" } th scope="col" { "Active snakes" } } }
                        tbody { @for row in &s.weekly_active_snakes { tr { th scope="row" { (row.week_start) } td { (row.count) } } } }
                    }
                }
            }
        }
    }
}

fn tile(label: &str, value: String, detail: &str) -> Markup {
    html! { div class="stat" { div class="label" { (label) } div class="value" { (value) } div class="detail" { (detail) } } }
}

fn game_legend() -> Markup {
    html! { p class="public-stats-legend" {
        span class="key series-0" {} "Custom"
        span class="key series-1" {} "Leaderboard"
        span class="key series-2" {} "Tournament"
    } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use tower::ServiceExt as _;

    #[sqlx::test(migrations = "../migrations")]
    async fn stale_day_is_not_served_inside_cache_ttl(db: sqlx::PgPool) {
        let mut state = AppState::test_from_pool(db.clone());
        state.stats_cache = Arc::new(crate::cache::TtlCell::new(std::time::Duration::from_secs(
            900,
        )));
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let stale = StatsSnapshot::fetch(&db, today - Duration::days(1))
            .await
            .unwrap();
        state.stats_cache.put(stale);
        let fresh = snapshot_for_day(&state, today).await.unwrap();
        assert_eq!(fresh.as_of_utc_date, today - Duration::days(1));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn public_router_serves_html_json_and_reports_query_failure(db: sqlx::PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = crate::routes::routes(state).layer(tower_cookies::CookieManagerLayer::new());
        let html_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(html_response.status(), StatusCode::OK);
        let html = String::from_utf8(
            to_bytes(html_response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("href=\"/stats\""));
        assert_eq!(html.matches("class=\"public-stats-chart\"").count(), 6);
        assert_eq!(html.matches("class=\"public-stats-table\"").count(), 6);
        assert_eq!(html.matches("class=\"stat\"").count(), 8);
        assert!(!html.contains("u1-secret"));

        let json_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .header(header::ORIGIN, "https://example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_response.status(), StatusCode::OK);
        assert_eq!(
            json_response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "*"
        );
        let body = to_bytes(json_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["headlines"]["dau"], 0);
        assert_eq!(json["daily_games"].as_array().unwrap().len(), 90);
        assert_eq!(json["weekly_growth"].as_array().unwrap().len(), 52);

        sqlx::query!("DELETE FROM stats_tracking_start")
            .execute(&db)
            .await
            .unwrap();
        let failed = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
