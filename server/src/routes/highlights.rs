//! `/admin/highlights` (DEV-1993): list stored weeks, view one digest with
//! inline spoiler cards, preview an arbitrary window (never stored), and
//! backfill an aligned past week.

use axum::{
    Form,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect},
};
use chrono::{DateTime, NaiveDate, Utc};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    errors::{ServerError, ServerResult},
    flasher::Flasher,
    highlights::{self, Week},
    routes::auth::AdminUser,
    state::AppState,
};

const BODY_STYLE: &str = "font-size:16px";
const BUTTON_STYLE: &str = "min-height:44px;padding:8px 16px;background:#666;color:white;text-decoration:none;\
     border-radius:4px;box-sizing:border-box;display:inline-flex;align-items:center;font-size:16px";

fn fmt_window(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "{} – {}",
        start.format("%a %b %-d"),
        end.format("%a %b %-d, %Y")
    )
}

fn status_badge(status: &str) -> Markup {
    let color = match status {
        "posted" => "#2e7d32",
        "pending" => "#b26a00",
        "skipped" => "#666",
        _ => "#b00020",
    };
    html! {
        span style=(format!("color:{color}")) { (status) }
    }
}

/// GET /admin/highlights — stored weeks, newest first, plus the backfill
/// form and a pointer to the preview form.
pub async fn list(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let digests = highlights::list_digests(&state.db).await?;
    let rows: Vec<Markup> = digests
        .iter()
        .map(|digest| {
            html! {
                tr style="white-space:nowrap" {
                    td {
                        a href=(format!("/admin/highlights/{}", digest.digest_id))
                          style="min-height:44px;display:inline-flex;align-items:center;font-size:16px" {
                            (fmt_window(digest.window_start, digest.window_end))
                        }
                    }
                    td { (digest.games_count) }
                    td { (status_badge(&digest.discord_status)) }
                    td { (digest.computed_at.format("%Y-%m-%d %H:%MZ")) }
                }
            }
        })
        .collect();
    let page = page_factory.create_page(
        "Arena highlights".to_string(),
        Box::new(html! {
            div style=(BODY_STYLE) {
                h1 { "Arena highlights" }
                p {
                    "Weekly digest of postable ladder games. Weeks end Saturday 12:00 UTC; \
                     each listed game links its replay, spoiler card, and GIF export."
                }
                div style="overflow-x:auto" {
                    table style="border-collapse:collapse;min-width:100%;font-size:16px" {
                        thead {
                            tr {
                                th style="text-align:left;padding:0.5rem" { "Week" }
                                th style="text-align:right;padding:0.5rem" { "Games" }
                                th style="text-align:left;padding:0.5rem" { "Discord" }
                                th style="text-align:left;padding:0.5rem" { "Computed" }
                            }
                        }
                        tbody {
                            @for row in rows {
                                (row)
                            }
                        }
                    }
                }
                h2 { "Backfill a week" }
                form method="post" action="/admin/highlights/backfill" {
                    p {
                        label { "Saturday the week ends (YYYY-MM-DD): " }
                        input type="date" name="week" required
                            style="min-height:44px;font-size:16px;padding:4px 8px";
                        " "
                        button type="submit" style=(BUTTON_STYLE) { "Compute & store" }
                    }
                    p style="color:#666" {
                        "Non-Saturdays and future weeks are rejected. A week past its \
                         announcement deadline is stored without a Discord post."
                    }
                }
                h2 { "Preview a window" }
                form method="get" action="/admin/highlights/preview" {
                    p {
                        input type="text" name="start" placeholder="2026-10-24T12:00:00Z"
                            style="min-height:44px;font-size:16px;padding:4px 8px;width:15rem";
                        " → "
                        input type="text" name="end" placeholder="2026-10-31T12:00:00Z"
                            style="min-height:44px;font-size:16px;padding:4px 8px;width:15rem";
                        " "
                        button type="submit" style=(BUTTON_STYLE) { "Preview" }
                    }
                    p style="color:#666" {
                        "RFC 3339 timestamps. Computes any window read-only — nothing is \
                         stored or posted. On prod this can take about a minute."
                    }
                }
            }
        }),
    );
    Ok(([(header::CACHE_CONTROL, "no-store")], page))
}

/// GET /admin/highlights/{id} — one week's full digest.
pub async fn detail(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    Path(digest_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let Some((digest, sections)) = highlights::load_digest(&state.db, digest_id).await? else {
        return Err(ServerError(
            color_eyre::eyre::eyre!("Highlights digest {digest_id} not found"),
            StatusCode::NOT_FOUND,
        ));
    };
    let page = page_factory.create_page(
        format!(
            "Highlights {}",
            fmt_window(digest.window_start, digest.window_end)
        ),
        Box::new(html! {
            div style=(BODY_STYLE) {
                h1 { "Arena highlights " (fmt_window(digest.window_start, digest.window_end)) }
                p {
                    (digest.games_count) " rated ladder games · "
                    "k " (format!("{:.2}", digest.duel_k)) " (duels) / "
                    (format!("{:.2}", digest.four_snake_k)) " (4-snake) · "
                    "Discord: " (status_badge(&digest.discord_status))
                }
                p { a href="/admin/highlights" style=(BUTTON_STYLE) { "← All weeks" } }
                (highlights::render::sections_html(&sections, &state.config.base_url))
            }
        }),
    );
    Ok(([(header::CACHE_CONTROL, "no-store")], page))
}

#[derive(Deserialize)]
pub struct WindowParams {
    pub start: String,
    pub end: String,
}

#[derive(Deserialize)]
pub struct WeekForm {
    pub week: String,
}

/// GET /admin/highlights/preview — compute any window read-only. May take
/// ~1 min on prod; never stores or posts.
pub async fn preview(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    page_factory: PageFactory,
    Query(params): Query<WindowParams>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let parse = |value: String| -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&value)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    };
    let Some(start) = parse(params.start) else {
        return Err(preview_error(
            "start must be RFC 3339, e.g. 2026-10-24T12:00:00Z",
        ));
    };
    let Some(end) = parse(params.end) else {
        return Err(preview_error(
            "end must be RFC 3339, e.g. 2026-10-31T12:00:00Z",
        ));
    };
    if end <= start {
        return Err(preview_error("end must be after start"));
    }

    let house = state.config.highlights_house();
    let digest = highlights::compute_digest(&state.db, &house, start, end).await?;

    let banner = highlights::render::frames_missing_banner(&digest);
    let sections = digest
        .sections
        .iter()
        .map(|section| highlights::StoredSection {
            kind: section.kind,
            ladder_name: section.ladder.as_ref().map(|l| l.name.clone()),
            position: 0,
            summary: section.summary.clone(),
            items: section
                .items
                .iter()
                .map(|item| highlights::StoredItem {
                    position: 0,
                    game_id: item.game_id,
                    subject: item.subject.clone(),
                    value: item.value,
                    text: item.text.clone(),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let page = page_factory.create_page(
        format!("Highlights preview {}", fmt_window(start, end)),
        Box::new(html! {
            div style=(BODY_STYLE) {
                h1 { "Highlights preview (not stored)" }
                p {
                    (fmt_window(digest.window_start, digest.window_end)) ": "
                    (digest.games_count) " rated ladder games · "
                    "k " (format!("{:.2}", digest.duel_k)) " (duels) / "
                    (format!("{:.2}", digest.four_snake_k)) " (4-snake)"
                }
                @if let Some(banner) = banner {
                    div style="background:#fff3cd;border:1px solid #ffca2c;border-radius:6px;padding:0.75rem;margin-bottom:1rem" {
                        (banner)
                    }
                }
                p { a href="/admin/highlights" style=(BUTTON_STYLE) { "← All weeks" } }
                (highlights::render::sections_html(&sections, &state.config.base_url))
            }
        }),
    );
    Ok(([(header::CACHE_CONTROL, "no-store")], page))
}

/// POST /admin/highlights/backfill — compute and store one aligned week.
/// Invalid dates flash an error and redirect (form UX); compute failures
/// propagate as errors.
pub async fn backfill(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    flasher: Flasher,
    Form(form): Form<WeekForm>,
) -> ServerResult<Redirect, StatusCode> {
    let now = Utc::now();
    let week = match NaiveDate::parse_from_str(&form.week, "%Y-%m-%d")
        .ok()
        .and_then(|date| Week::ending(date, now))
    {
        Some(week) => week,
        None => {
            flasher
                .error(format!(
                    "{:?} is not a past Saturday (YYYY-MM-DD); nothing stored.",
                    form.week
                ))
                .await
                .wrap_err("Failed to flash")?;
            return Ok(Redirect::to("/admin/highlights"));
        }
    };

    if highlights::digest_for_week(&state.db, &week)
        .await?
        .is_some()
    {
        flasher
            .info(format!("Week ending {} was already stored.", form.week))
            .await
            .wrap_err("Failed to flash")?;
        return Ok(Redirect::to("/admin/highlights"));
    }

    let house = state.config.highlights_house();
    let digest = highlights::compute_digest(&state.db, &house, week.start, week.end).await?;
    let past_deadline = now >= week.end + chrono::Duration::days(7);
    let banner = highlights::render::frames_missing_banner(&digest);
    let stored = highlights::store_digest(&state.db, &digest, &state.config.base_url, now).await?;
    let date = form.week.clone();
    let message = match stored {
        Some(_) if past_deadline => format!(
            "Week ending {date} stored. Its announcement deadline had passed, so no Discord post."
        ),
        Some(_) => format!("Week ending {date} stored."),
        None => format!("Week ending {date} was already stored."),
    };
    flasher.success(message).await.wrap_err("Failed to flash")?;
    if let Some(banner) = banner {
        flasher.warning(banner).await.wrap_err("Failed to flash")?;
    }
    Ok(Redirect::to("/admin/highlights"))
}

/// A 400-status error for invalid preview input (logged by
/// `ServerError`, surfaced to the client as a bare status).
fn preview_error(message: &str) -> ServerError<StatusCode> {
    ServerError(
        color_eyre::eyre::eyre!("{message}"),
        StatusCode::BAD_REQUEST,
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    use crate::highlights::{self, Week, types::HouseSnakeConfig};
    use crate::models::session;
    use crate::routes::test_support::{create_user_session, signed_session_cookie};
    use crate::state::AppState;

    struct Fixture {
        state: AppState,
        admin_session: uuid::Uuid,
        user_session: uuid::Uuid,
        pool: PgPool,
    }

    impl Fixture {
        async fn new(pool: PgPool) -> Self {
            let state = AppState::test_from_pool(pool.clone());
            Self {
                admin_session: create_user_session(&pool, 9954001, true).await,
                user_session: create_user_session(&pool, 9954002, false).await,
                state,
                pool,
            }
        }

        /// The flash message recorded on a session (the Flasher is
        /// DB-backed, not cookie-based).
        async fn flash(&self) -> Option<String> {
            sqlx::query_scalar("SELECT flash_message FROM sessions WHERE session_id = $1")
                .bind(self.admin_session)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn request(
            &self,
            method: &str,
            path: &str,
            session: Option<uuid::Uuid>,
            body: Option<(&str, String)>,
        ) -> (StatusCode, String, Option<String>) {
            let app = crate::routes::routes(self.state.clone())
                .expect("asset router")
                .layer(tower_cookies::CookieManagerLayer::new());
            let mut builder = Request::builder().method(method).uri(path);
            if let Some(session_id) = session {
                builder = builder.header(
                    header::COOKIE,
                    format!(
                        "{}={}",
                        session::SESSION_COOKIE_NAME,
                        signed_session_cookie(&self.state, session_id)
                    ),
                );
            }
            let request = match body {
                Some((content_type, body)) => builder
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body)),
                None => builder.body(Body::empty()),
            };
            let response = app.oneshot(request.unwrap()).await.unwrap();
            let status = response.status();
            let set_cookies: Vec<String> = response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(str::to_string)
                .collect();
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec();
            (
                status,
                String::from_utf8_lossy(&body).into_owned(),
                (!set_cookies.is_empty()).then_some(set_cookies.join("\n")),
            )
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn admin_pages_guarded(pool: PgPool) {
        let f = Fixture::new(pool).await;
        let (status, _, _) = f
            .request("GET", "/admin/highlights", Some(f.admin_session), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body, _) = f
            .request("GET", "/admin/highlights", Some(f.user_session), None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("Admin access required"));
        let (status, _, _) = f.request("GET", "/admin/highlights", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn detail_page_shows_links_and_cards(pool: PgPool) {
        let f = Fixture::new(pool).await;
        let week = Week::latest_completed(chrono::Utc::now());
        let digest = highlights::compute_digest(
            &f.state.db,
            &HouseSnakeConfig::default(),
            week.start,
            week.end,
        )
        .await
        .unwrap();
        let digest_id = highlights::store_digest(
            &f.state.db,
            &digest,
            &f.state.config.base_url,
            chrono::Utc::now(),
        )
        .await
        .unwrap()
        .expect("stored");

        let (status, body, _) = f
            .request(
                "GET",
                &format!("/admin/highlights/{digest_id}"),
                Some(f.admin_session),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Arena highlights"));
        // Sections render even for an empty window.
        assert!(body.contains("none this week"));

        // A seeded game's item carries the card image and exporter link.
        // (The game row must exist — the FK is real.)
        let game_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status)
             VALUES ('11x11', 'standard', 'finished') RETURNING game_id",
        )
        .fetch_one(&f.state.db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO highlight_sections
             VALUES (DEFAULT, $1, 'marathons', NULL, 999, 'the longest game this week')",
            digest_id
        )
        .execute(&f.state.db)
        .await
        .unwrap();
        let section_id: uuid::Uuid = sqlx::query_scalar!(
            "SELECT section_id FROM highlight_sections WHERE digest_id = $1 AND kind = 'marathons' AND position = 999",
            digest_id
        )
        .fetch_one(&f.state.db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO highlight_items
             VALUES (DEFAULT, $1, 0, $2, 'subject', 1.0, '10 turns')",
            section_id,
            game_id
        )
        .execute(&f.state.db)
        .await
        .unwrap();
        let (status, body, _) = f
            .request(
                "GET",
                &format!("/admin/highlights/{digest_id}"),
                Some(f.admin_session),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&format!("/og/games/{game_id}.png?showSpoilers=true")));
        assert!(body.contains(&format!(
            "https://exporter.battlesnake.com/games/{game_id}/gif"
        )));

        // Unknown digest 404s.
        let (status, _, _) = f
            .request(
                "GET",
                &format!("/admin/highlights/{}", uuid::Uuid::new_v4()),
                Some(f.admin_session),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn preview_computes_without_storing(pool: PgPool) {
        let f = Fixture::new(pool).await;
        let week = Week::latest_completed(chrono::Utc::now());
        let query = format!(
            "?start={}&end={}",
            week.start
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            week.end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        let (status, body, _) = f
            .request(
                "GET",
                &format!("/admin/highlights/preview{query}"),
                Some(f.admin_session),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Highlights preview"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM highlight_digests")
            .fetch_one(&f.state.db)
            .await
            .unwrap();
        assert_eq!(count, 0, "preview must never store");

        // Invalid input → 400.
        let (status, _, _) = f
            .request(
                "GET",
                "/admin/highlights/preview?start=notadate&end=alsonot",
                Some(f.admin_session),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // end <= start → 400.
        let (status, _, _) = f
            .request(
                "GET",
                &format!(
                    "/admin/highlights/preview?start={}&end={}",
                    week.end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    week.start
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
                Some(f.admin_session),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_stores_once_and_rejects_non_saturdays(pool: PgPool) {
        let f = Fixture::new(pool).await;
        let week = Week::latest_completed(chrono::Utc::now());
        let saturday = week.end.date_naive().format("%Y-%m-%d").to_string();

        let (status, _, _) = f
            .request(
                "POST",
                "/admin/highlights/backfill",
                Some(f.admin_session),
                Some((
                    "application/x-www-form-urlencoded",
                    format!("week={saturday}"),
                )),
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let flash = f.flash().await;
        assert!(
            flash.as_deref().is_some_and(|m| m.contains("stored")),
            "flash must record the store: {flash:?}"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM highlight_digests")
            .fetch_one(&f.state.db)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Repeat: flash "already stored", still one row.
        let (status, _, _) = f
            .request(
                "POST",
                "/admin/highlights/backfill",
                Some(f.admin_session),
                Some((
                    "application/x-www-form-urlencoded",
                    format!("week={saturday}"),
                )),
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            f.flash()
                .await
                .is_some_and(|m| m.contains("already stored"))
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM highlight_digests")
            .fetch_one(&f.state.db)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Non-Saturday: flashed error, nothing stored (the same latest week
        // would collide, so use the Friday before it).
        let friday = (week.end - chrono::Duration::days(1))
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let (status, _, _) = f
            .request(
                "POST",
                "/admin/highlights/backfill",
                Some(f.admin_session),
                Some((
                    "application/x-www-form-urlencoded",
                    format!("week={friday}"),
                )),
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(
            f.flash()
                .await
                .is_some_and(|m| m.contains("not a past Saturday"))
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM highlight_digests")
            .fetch_one(&f.state.db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
