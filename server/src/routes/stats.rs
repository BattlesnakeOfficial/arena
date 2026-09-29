use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use chrono::{Duration, Utc};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};

use crate::{
    components::page_factory::PageFactory,
    errors::ServerResult,
    models::stats::StatsSnapshot,
    routes::auth::{AdminApiUser, AdminUser},
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
        && cached.as_of_utc_date >= as_of
    {
        return Ok(cached);
    }
    let _refresh = state.stats_refresh.lock().await;
    if let Some(cached) = state.stats_cache.get()
        && cached.as_of_utc_date >= as_of
    {
        return Ok(cached);
    }
    let fresh = StatsSnapshot::fetch(&state.db, today_utc).await?;
    Ok(state.stats_cache.put(fresh))
}

pub async fn stats_json(
    State(state): State<AppState>,
    AdminApiUser(_user): AdminApiUser,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let stats = snapshot(&state).await.wrap_err("Failed to fetch stats")?;
    Ok(Json(stats.as_ref().clone()))
}

pub async fn stats_page(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let stats = snapshot(&state).await.wrap_err("Failed to fetch stats")?;
    Ok(page_factory.create_page("Stats".to_string(), Box::new(render_stats(&stats))))
}

fn chart(
    id: &str,
    title: &str,
    description: &str,
    values: &[[i64; 3]],
    first_period: Option<String>,
    last_period: Option<String>,
) -> Markup {
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
        svg class="public-stats-chart" viewBox="0 0 720 240" role="img" aria-labelledby=(labelled_by) {
            title id=(label_id) { (title) }
            desc id=(desc_id) { (description) }
            text x="0" y="16" class="chart-label" { "Max " (max) }
            line x1="0" y1="200" x2="720" y2="200" class="chart-axis" {}
            @for (x, y, width, height, series) in bars {
                rect x=(format!("{x:.2}")) y=(format!("{y:.2}"))
                     width=(format!("{width:.2}")) height=(format!("{height:.2}"))
                     class=(format!("chart-series-{series}")) {}
            }
            @if let Some(first) = first_period { text x="0" y="224" class="chart-label" { (first) } }
            @if let Some(last) = last_period { text x="720" y="224" text-anchor="end" class="chart-label" { (last) } }
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
                p class="sub" { "Aggregate usage for admins. No per-user data." }
            }
            p class="stats-period" { "UTC; through " (s.as_of_utc_date) }
            div class="stats public-stats-tiles" {
                (tile("DAU", active_tile(h.dau.map(|value| value.to_string()), h.dau_available_on), "Last complete day"))
                (tile("WAU", active_tile(h.wau.map(|value| value.to_string()), h.wau_available_on), "Trailing 7 complete days"))
                (tile("MAU", active_tile(h.mau.map(|value| value.to_string()), h.mau_available_on), "Trailing 28 complete days"))
                (tile("DAU / MAU", active_tile(h.dau_mau_percent.map(|value| format!("{value:.1}%")), h.dau_mau_percent_available_on), "28-day average DAU / MAU"))
                (tile("Games played", h.games_7d.to_string(), "Trailing 7 complete days"))
                (tile("Active snakes", h.active_snakes_7d.to_string(), "Trailing 7 complete days"))
                (tile("Registered users", h.registered_users.to_string(), "Through last complete day"))
                (tile("Total snakes", h.total_snakes.to_string(), "Through last complete day"))
            }
            div class="public-stats-note" {
                p { "Active-user tracking began " (s.tracking_started_on) " UTC. The first complete tracked day is " (s.live_tracking_started_on) "." }
                p { "An active user is an account making at least one signed-in web request or API-token request during a UTC day. Automated API clients count." }
                p { "A played game finished on Arena, excludes legacy imports, and is dated by game creation. An active snake appeared in a played game during the period." }
                p { "Read the " a href="/privacy" { "privacy policy" } " for how activity dates are stored." }
            }

            section class="public-stats-section" {
                h2 { "Daily active users" }
                @if s.daily_active_users.is_empty() {
                    p class="public-stats-collecting" { (active_tile(None, h.dau_available_on)) }
                } @else {
                    (chart("daily-users", "Daily active users", "Up to 90 complete UTC days since tracking began", &dau, s.daily_active_users.first().map(|r| r.date.to_string()), s.daily_active_users.last().map(|r| r.date.to_string())))
                    div class="public-stats-table-wrap" {
                        table class="public-stats-table" {
                            caption { "Daily active users, up to 90 complete UTC days since tracking began" }
                            thead { tr { th scope="col" { "Date (UTC)" } th scope="col" { "Active users" } } }
                            tbody { @for row in &s.daily_active_users { tr { th scope="row" { (row.date) } td { (row.count) } } } }
                        }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Weekly active users" }
                @if s.weekly_active_users.is_empty() {
                    p class="public-stats-collecting" { (active_tile(None, h.wau_available_on)) }
                } @else {
                    (chart("weekly-users", "Weekly active users", "Up to 52 complete ISO weeks since tracking began", &wau, s.weekly_active_users.first().map(|r| r.week_start.to_string()), s.weekly_active_users.last().map(|r| r.week_start.to_string())))
                    div class="public-stats-table-wrap" {
                        table class="public-stats-table" {
                            caption { "Weekly active users, up to 52 complete ISO weeks since tracking began" }
                            thead { tr { th scope="col" { "Week starting Monday (UTC)" } th scope="col" { "Active users" } } }
                            tbody { @for row in &s.weekly_active_users { tr { th scope="row" { (row.week_start) } td { (row.count) } } } }
                        }
                    }
                }
            }
            section class="public-stats-section" {
                h2 { "Games per day" }
                (chart("daily-games", "Games played per day", "Custom, leaderboard, and tournament games over the last 90 complete UTC days", &daily_games, s.daily_games.first().map(|r| r.date.to_string()), s.daily_games.last().map(|r| r.date.to_string())))
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
                (chart("weekly-games", "Games played per week", "Custom, leaderboard, and tournament games over the last 52 complete ISO weeks", &weekly_games, s.weekly_games.first().map(|r| r.week_start.to_string()), s.weekly_games.last().map(|r| r.week_start.to_string())))
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
                (chart("growth", "New users and snakes per week", "New users and snakes over the last 52 complete ISO weeks", &growth, s.weekly_growth.first().map(|r| r.week_start.to_string()), s.weekly_growth.last().map(|r| r.week_start.to_string())))
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
                (chart("weekly-snakes", "Active snakes per week", "Distinct snakes in played games over the last 52 complete ISO weeks", &snakes, s.weekly_active_snakes.first().map(|r| r.week_start.to_string()), s.weekly_active_snakes.last().map(|r| r.week_start.to_string())))
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

fn active_tile(value: Option<String>, available_on: chrono::NaiveDate) -> String {
    value.unwrap_or_else(|| format!("Collecting data — available {available_on}"))
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

    use crate::routes::test_support::{
        create_user_session, session_user_id, signed_session_cookie,
    };

    async fn route_response(
        app: &axum::Router,
        path: &str,
        session_cookie: Option<&str>,
        token: Option<&str>,
    ) -> axum::response::Response {
        let mut builder = Request::builder().uri(path);
        if path.starts_with("/api/") {
            builder = builder.header(header::ORIGIN, "https://example.com");
        }
        if let Some(session_cookie) = session_cookie {
            builder = builder.header(
                header::COOKIE,
                format!(
                    "{}={session_cookie}",
                    crate::models::session::SESSION_COOKIE_NAME
                ),
            );
        }
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

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
    async fn admin_router_serves_html_json_and_reports_query_failure(db: sqlx::PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app =
            crate::routes::routes(state.clone()).layer(tower_cookies::CookieManagerLayer::new());
        for (stats_path, admin_path) in [("/stats", "/admin"), ("/api/stats", "/api/admin/stats")] {
            let stats = route_response(&app, stats_path, None, None).await;
            let admin = route_response(&app, admin_path, None, None).await;
            assert_eq!(stats.status(), admin.status(), "anonymous {stats_path}");
            assert_eq!(stats.status(), StatusCode::UNAUTHORIZED);
        }

        let non_admin_session = create_user_session(&db, 14621, false).await;
        let non_admin_cookie = signed_session_cookie(&state, non_admin_session);
        for (stats_path, admin_path) in [("/stats", "/admin"), ("/api/stats", "/api/admin/stats")] {
            let stats = route_response(&app, stats_path, Some(&non_admin_cookie), None).await;
            let admin = route_response(&app, admin_path, Some(&non_admin_cookie), None).await;
            assert_eq!(stats.status(), admin.status(), "non-admin {stats_path}");
            assert_eq!(stats.status(), StatusCode::FORBIDDEN);
        }
        let non_admin_id = session_user_id(&db, non_admin_session).await;
        let non_admin_token =
            crate::models::api_token::create_api_token(&db, non_admin_id, "stats-test")
                .await
                .unwrap();
        let stats = route_response(&app, "/api/stats", None, Some(&non_admin_token.secret)).await;
        let admin = route_response(
            &app,
            "/api/admin/stats",
            None,
            Some(&non_admin_token.secret),
        )
        .await;
        assert_eq!(stats.status(), admin.status());
        assert_eq!(stats.status(), StatusCode::FORBIDDEN);

        let root = route_response(&app, "/", None, None).await;
        let root_html = String::from_utf8(
            to_bytes(root.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(!root_html.contains("href=\"/stats\""));

        let admin_session = create_user_session(&db, 14622, true).await;
        let admin_cookie = signed_session_cookie(&state, admin_session);
        let admin_page = route_response(&app, "/admin", Some(&admin_cookie), None).await;
        assert_eq!(admin_page.status(), StatusCode::OK);
        let admin_html = String::from_utf8(
            to_bytes(admin_page.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(admin_html.contains("href=\"/stats\""));

        let html_response = route_response(&app, "/stats", Some(&admin_cookie), None).await;
        assert_eq!(html_response.status(), StatusCode::OK);
        let html = String::from_utf8(
            to_bytes(html_response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert_eq!(html.matches("class=\"public-stats-chart\"").count(), 4);
        assert_eq!(html.matches("class=\"public-stats-table\"").count(), 4);
        assert_eq!(html.matches("class=\"stat\"").count(), 8);
        assert_eq!(html.matches("class=\"public-stats-collecting\"").count(), 2);
        assert!(!html.contains("u1-secret"));

        let admin_id = session_user_id(&db, admin_session).await;
        let admin_token = crate::models::api_token::create_api_token(&db, admin_id, "stats-test")
            .await
            .unwrap();
        let json_response =
            route_response(&app, "/api/stats", None, Some(&admin_token.secret)).await;
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
        assert!(json["headlines"]["dau"].is_null());
        assert!(json["headlines"]["dau_available_on"].is_string());
        assert_eq!(json["daily_games"].as_array().unwrap().len(), 90);
        assert_eq!(json["weekly_growth"].as_array().unwrap().len(), 52);

        sqlx::query!("DELETE FROM stats_tracking_start")
            .execute(&db)
            .await
            .unwrap();
        let failed = route_response(&app, "/api/stats", None, Some(&admin_token.secret)).await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
