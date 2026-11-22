//! Arena seasons phase 4 (DEV-2644): the league page's season picker and
//! default-season rule, season-scoped rankings API, and admin season
//! management — all through the real router.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use chrono::{DateTime, TimeZone as _, Utc};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::models::{game, leaderboard, leaderboard_registration, session};
use crate::routes::test_support::{create_user_session, session_user_id, signed_session_cookie};
use crate::state::AppState;

struct Response {
    status: StatusCode,
    location: Option<String>,
    body: String,
}

async fn send(
    state: &AppState,
    method: &str,
    path: &str,
    session_id: Option<Uuid>,
    form: Option<String>,
) -> Response {
    let app = crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router();
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ORIGIN, "https://example.com")
        .header(header::ACCEPT, "text/html");
    if let Some(session_id) = session_id {
        builder = builder.header(
            header::COOKIE,
            format!(
                "{}={}",
                session::SESSION_COOKIE_NAME,
                signed_session_cookie(state, session_id)
            ),
        );
    }
    let request = match form {
        Some(form) => builder
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form)),
        None => builder.body(Body::empty()),
    };
    let response = app.oneshot(request.unwrap()).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Response {
        status,
        location,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

async fn get(state: &AppState, path: &str) -> Response {
    send(state, "GET", path, None, None).await
}

/// POST a form as `session`, follow the redirect, and return the page it
/// lands on (which renders the flash).
async fn post_and_follow(state: &AppState, path: &str, session: Uuid, form: &str) -> String {
    let res = send(state, "POST", path, Some(session), Some(form.to_string())).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{path}: {}", res.body);
    let location = res.location.expect("redirect location");
    let page = send(state, "GET", &location, Some(session), None).await;
    assert_eq!(page.status, StatusCode::OK, "{location}");
    page.body
}

async fn league(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO leaderboards (name, game_type, board_size, match_size)
         VALUES ($1, $2, '11x11', 2) RETURNING leaderboard_id",
    )
    .bind(name)
    .bind(game::GameType::Standard.as_str())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn preseason(pool: &PgPool, leaderboard_id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1")
        .bind(leaderboard_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Rewrite a season's dates with offsets (in days) from the DB clock;
/// `None` = open-ended.
async fn set_season_days(pool: &PgPool, season_id: Uuid, start: i32, end: Option<i32>) {
    sqlx::query(
        "UPDATE leaderboard_seasons
         SET starts_at = clock_timestamp() + make_interval(days => $2),
             ends_at = clock_timestamp() + make_interval(days => $3)
         WHERE season_id = $1",
    )
    .bind(season_id)
    .bind(start)
    .bind(end)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_season(
    pool: &PgPool,
    leaderboard_id: Uuid,
    name: &str,
    start: i32,
    end: Option<i32>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at)
         VALUES ($1, $2, clock_timestamp() + make_interval(days => $3),
                 clock_timestamp() + make_interval(days => $4))
         RETURNING season_id",
    )
    .bind(leaderboard_id)
    .bind(name)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn season_row(
    pool: &PgPool,
    season_id: Uuid,
) -> (String, DateTime<Utc>, Option<DateTime<Utc>>) {
    sqlx::query_as("SELECT name, starts_at, ends_at FROM leaderboard_seasons WHERE season_id = $1")
        .bind(season_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn season_count(pool: &PgPool, leaderboard_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM leaderboard_seasons WHERE leaderboard_id = $1")
        .bind(leaderboard_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn owner(pool: &PgPool, github_id: i64) -> Uuid {
    crate::routes::test_support::create_user(pool, github_id, false).await
}

async fn snake(pool: &PgPool, user_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO battlesnakes (user_id, name, url, visibility)
         VALUES ($1, $2, 'http://snake', 'public') RETURNING battlesnake_id",
    )
    .bind(user_id)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn register(pool: &PgPool, leaderboard_id: Uuid, battlesnake_id: Uuid) {
    leaderboard_registration::get_or_create_registration(pool, leaderboard_id, battlesnake_id)
        .await
        .unwrap()
        .expect("live snake registers");
}

/// A raw entry in one season with the given stats.
async fn entry(
    pool: &PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
    battlesnake_id: Uuid,
    games_played: i32,
    display_score: f64,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO leaderboard_entries
             (leaderboard_id, season_id, battlesnake_id, games_played, display_score)
         VALUES ($1, $2, $3, $4, $5) RETURNING leaderboard_entry_id",
    )
    .bind(leaderboard_id)
    .bind(season_id)
    .bind(battlesnake_id)
    .bind(games_played)
    .bind(display_score)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn leave(pool: &PgPool, leaderboard_id: Uuid, battlesnake_id: Uuid) {
    leaderboard_registration::set_registration_disabled(
        pool,
        leaderboard_id,
        battlesnake_id,
        Some(Utc::now()),
    )
    .await
    .unwrap();
}

fn ranked_names(json: &serde_json::Value, key: &str) -> Vec<String> {
    json[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["snake_name"].as_str().unwrap().to_string())
        .collect()
}

/// The text of the "Next matchmaker run" stat cell.
fn matchmaker_cell(body: &str) -> &str {
    let start = body.find("Next matchmaker run").expect("matchmaker cell");
    let rest = &body[start..];
    let end = rest.find("</div></div>").unwrap_or(rest.len());
    &rest[..end]
}

/// A league in rollover: Preseason ended a day ago (Ada + Bea ranked; Bea
/// has since left the league), Season 1 live (Cy ranked, and Bea — whose
/// left registration hides her while the season is live).
struct Rollover {
    lb: Uuid,
    preseason: Uuid,
    season1: Uuid,
    ada_pre: Uuid,
}

async fn rollover(pool: &PgPool) -> Rollover {
    let lb = league(pool, "Rollover").await;
    let preseason = preseason(pool, lb).await;
    set_season_days(pool, preseason, -30, Some(-1)).await;
    let season1 = insert_season(pool, lb, "Season 1", -1, None).await;
    let user = owner(pool, 4_400_001).await;
    let ada = snake(pool, user, "Ada").await;
    let bea = snake(pool, user, "Bea").await;
    let cy = snake(pool, user, "Cy").await;
    for s in [ada, bea, cy] {
        register(pool, lb, s).await;
    }
    let ada_pre = entry(pool, lb, preseason, ada, 12, 50.0).await;
    entry(pool, lb, preseason, bea, 11, 40.0).await;
    entry(pool, lb, season1, cy, 10, 45.0).await;
    entry(pool, lb, season1, bea, 10, 90.0).await;
    leave(pool, lb, bea).await;
    Rollover {
        lb,
        preseason,
        season1,
        ada_pre,
    }
}

// --- League page ---

#[sqlx::test(migrations = "../migrations")]
async fn preseason_only_league_renders_as_today_plus_one_chip(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let lb = league(&pool, "Deploy Safety").await;
    let pre = preseason(&pool, lb).await;
    let user = owner(&pool, 4_400_010).await;
    let ranked = snake(&pool, user, "Ranked One").await;
    let placing = snake(&pool, user, "Placing One").await;
    register(&pool, lb, ranked).await;
    register(&pool, lb, placing).await;
    entry(&pool, lb, pre, ranked, 10, 30.0).await;
    entry(&pool, lb, pre, placing, 3, 5.0).await;

    let res = get(&state, &format!("/leaderboards/{lb}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(r#"aria-label="Seasons""#), "{}", res.body);
    assert_eq!(
        res.body
            .matches(r#"class="mode on" aria-current="page""#)
            .count(),
        1
    );
    assert!(res.body.contains(r#"aria-current="page">Preseason</span>"#));
    // One chip: no link to any other season.
    assert!(!res.body.contains("?season="));
    assert!(res.body.contains("Ranked One"));
    assert!(res.body.contains("Placing One"));
    assert!(!res.body.contains("season-notice"));
    assert!(!res.body.contains("between seasons"));
    let cell = matchmaker_cell(&res.body);
    assert!(cell.contains("waiting for"), "{cell}");

    // The API sees the same default season, and the league is live.
    let (status, body) = crate::routes::test_support::send_api(
        &crate::routes::test_support::api_app(&state),
        "GET",
        &format!("/api/leaderboards/{lb}/rankings"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ranked_names(&json, "ranked"), vec!["Ranked One"]);
    assert_eq!(ranked_names(&json, "placement"), vec!["Placing One"]);
    let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "leaderboard_id",
            "leaderboard_name",
            "min_games",
            "placement",
            "ranked"
        ]
    );

    let (_, body) = crate::routes::test_support::send_api(
        &crate::routes::test_support::api_app(&state),
        "GET",
        "/api/leaderboards",
        None,
        None,
    )
    .await;
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mine = list
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == lb.to_string())
        .unwrap();
    assert_eq!(mine["active"], true);
}

#[sqlx::test(migrations = "../migrations")]
async fn between_seasons_shows_frozen_standings_and_next_start(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let lb = league(&pool, "Gap League").await;
    let pre = preseason(&pool, lb).await;
    set_season_days(&pool, pre, -30, Some(-1)).await;
    let future = insert_season(&pool, lb, "Season 1", 7, None).await;
    let user = owner(&pool, 4_400_020).await;
    let s = snake(&pool, user, "Frozen Star").await;
    register(&pool, lb, s).await;
    entry(&pool, lb, pre, s, 12, 30.0).await;

    let res = get(&state, &format!("/leaderboards/{lb}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Frozen Star"));
    assert!(
        res.body
            .contains("This season has ended. The next one starts"),
        "{}",
        res.body
    );
    assert!(!res.body.contains("see the live standings"));
    assert!(res.body.contains(r#"aria-current="page">Preseason</span>"#));
    assert!(res.body.contains(&format!(
        r#"href="/leaderboards/{lb}?season={future}">Season 1</a>"#
    )));
    let cell = matchmaker_cell(&res.body);
    assert!(cell.contains("between seasons — next starts"), "{cell}");
    assert!(!cell.to_lowercase().contains("paused"), "{cell}");
    assert!(!cell.contains("waiting for"), "{cell}");

    // The future season: empty standings and its own start.
    let res = get(&state, &format!("/leaderboards/{lb}?season={future}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("This season starts"));
    assert!(res.body.contains("No snakes have completed enough games"));
    assert!(!res.body.contains("Frozen Star</a>"));
    assert!(res.body.contains(r#"aria-current="page">Season 1</span>"#));

    // API: gap = not active; the default rankings are the frozen ones.
    let app = crate::routes::test_support::api_app(&state);
    let (_, body) =
        crate::routes::test_support::send_api(&app, "GET", "/api/leaderboards", None, None).await;
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mine = list
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == lb.to_string())
        .unwrap();
    assert_eq!(mine["active"], false);
    let (_, body) = crate::routes::test_support::send_api(
        &app,
        "GET",
        &format!("/api/leaderboards/{lb}/rankings"),
        None,
        None,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ranked_names(&json, "ranked"), vec!["Frozen Star"]);

    // Join and leave still work during the gap.
    let session = create_user_session(&pool, 4_400_021, false).await;
    let joiner = snake(&pool, session_user_id(&pool, session).await, "Gap Joiner").await;
    let cookie = signed_session_cookie(&state, session);
    let (status, body) = crate::routes::test_support::send_api_with_cookie(
        &app,
        "POST",
        &format!("/api/leaderboards/{lb}/entries"),
        &cookie,
        Some(serde_json::json!({ "battlesnake_id": joiner })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = crate::routes::test_support::send_api_with_cookie(
        &app,
        "DELETE",
        &format!("/api/leaderboards/{lb}/entries/{joiner}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[sqlx::test(migrations = "../migrations")]
async fn only_future_seasons_render_the_empty_state(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let lb = league(&pool, "Not Yet").await;
    let pre = preseason(&pool, lb).await;
    sqlx::query("DELETE FROM leaderboard_seasons WHERE season_id = $1")
        .bind(pre)
        .execute(&pool)
        .await
        .unwrap();
    insert_season(&pool, lb, "Season 1", 7, None).await;

    let res = get(&state, &format!("/leaderboards/{lb}")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("The next season starts"), "{}", res.body);
    assert!(res.body.contains("No snakes have completed enough games"));
    // Nothing selected: the lone chip is a link, not the current one.
    assert!(!res.body.contains(r#"aria-current="page">Season 1"#));
    assert!(res.body.contains(r#"<div class="value">0</div>"#));
    let cell = matchmaker_cell(&res.body);
    assert!(cell.contains("between seasons — next starts"), "{cell}");
    assert!(!cell.to_lowercase().contains("paused"), "{cell}");

    let app = crate::routes::test_support::api_app(&state);
    let (status, body) = crate::routes::test_support::send_api(
        &app,
        "GET",
        &format!("/api/leaderboards/{lb}/rankings"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["ranked"], serde_json::json!([]));
    assert_eq!(json["placement"], serde_json::json!([]));
}

#[sqlx::test(migrations = "../migrations")]
async fn past_season_shows_frozen_standings_with_leavers(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let r = rollover(&pool).await;

    // Default: the live Season 1. Bea left, so she's hidden while it's live.
    let res = get(&state, &format!("/leaderboards/{}", r.lb)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(">Cy</a>"));
    assert!(!res.body.contains(">Bea</a>"));
    assert!(!res.body.contains(">Ada</a>"));
    assert!(!res.body.contains("season-notice"));
    assert!(res.body.contains(r#"aria-current="page">Season 1</span>"#));
    // Canonical sort links carry no season.
    assert!(
        res.body
            .contains(&format!(r#"href="/leaderboards/{}?sort=food_eaten""#, r.lb))
    );

    // The ended Preseason: frozen, leaver included.
    let res = get(
        &state,
        &format!("/leaderboards/{}?season={}", r.lb, r.preseason),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains(">Ada</a>"));
    assert!(res.body.contains(">Bea</a>"));
    assert!(!res.body.contains(">Cy</a>"));
    assert!(res.body.contains(&format!(
        r#"This season has ended — see the live standings in <a href="/leaderboards/{}">Season 1</a>."#,
        r.lb
    )), "{}", res.body);
    assert!(!res.body.contains("The next one starts"));
    let cell = matchmaker_cell(&res.body);
    assert!(!cell.contains("between seasons"), "{cell}");
    // Sort links stay on the selected season.
    assert!(res.body.contains(&format!(
        r#"href="/leaderboards/{}?sort=food_eaten&amp;season={}""#,
        r.lb, r.preseason
    )));

    // The API agrees, by default and by ?season=.
    let app = crate::routes::test_support::api_app(&state);
    for (query, expected) in [
        (String::new(), vec!["Cy"]),
        (format!("?season={}", r.preseason), vec!["Ada", "Bea"]),
        (format!("?season={}", r.season1), vec!["Cy"]),
    ] {
        let (status, body) = crate::routes::test_support::send_api(
            &app,
            "GET",
            &format!("/api/leaderboards/{}/rankings{query}", r.lb),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(ranked_names(&json, "ranked"), expected, "{query}");
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn season_selector_must_belong_to_the_league(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let lb = league(&pool, "Mine").await;
    let other = league(&pool, "Theirs").await;
    let foreign = preseason(&pool, other).await;
    let own = preseason(&pool, lb).await;

    assert_eq!(
        get(&state, &format!("/leaderboards/{lb}?season={own}"))
            .await
            .status,
        StatusCode::OK
    );
    for season in [foreign, Uuid::new_v4()] {
        let res = get(&state, &format!("/leaderboards/{lb}?season={season}")).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND);
        assert!(!res.body.contains(r#"aria-label="Seasons""#));
    }
    let res = get(&state, &format!("/leaderboards/{lb}?season=abc")).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let app = crate::routes::test_support::api_app(&state);
    let (status, body) = crate::routes::test_support::send_api(
        &app,
        "GET",
        &format!("/api/leaderboards/{lb}/rankings?season={foreign}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "Season not found");
}

#[sqlx::test(migrations = "../migrations")]
async fn legacy_entry_url_shows_the_entry_ranked_in_its_own_season(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let r = rollover(&pool).await;
    // A Season 1 entry scoring above Ada must not outrank her Preseason entry.
    let user = owner(&pool, 4_400_030).await;
    let dee = snake(&pool, user, "Dee").await;
    register(&pool, r.lb, dee).await;
    entry(&pool, r.lb, r.season1, dee, 10, 99.0).await;

    let res = get(
        &state,
        &format!("/leaderboards/{}/entries/{}", r.lb, r.ada_pre),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.contains("Ada"));
    assert!(
        res.body.contains(r##"<div class="value">#1</div>"##),
        "{}",
        res.body
    );
    assert!(
        res.body
            .contains(r#"<span class="badge season">Preseason</span>"#)
    );

    // Ranks don't interleave across seasons.
    assert_eq!(
        leaderboard::get_rank_for_entry(&pool, r.preseason, 45.0, 10)
            .await
            .unwrap(),
        Some(2),
        "only Ada (50) is above 45 in Preseason"
    );
    assert_eq!(
        leaderboard::get_rank_for_entry(&pool, r.season1, 45.0, 10)
            .await
            .unwrap(),
        Some(2),
        "only Dee (99) is above 45 live in Season 1 — Bea left"
    );
}

// --- Admin ---

#[sqlx::test(migrations = "../migrations")]
async fn admin_season_endpoints_require_an_admin(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let lb = league(&pool, "Guarded").await;
    let season = preseason(&pool, lb).await;
    let regular = create_user_session(&pool, 4_400_040, false).await;
    let form = "name=X&starts_at=2030-01-01T00%3A00&timezone=UTC";
    for (session, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(regular), StatusCode::FORBIDDEN),
    ] {
        for (method, path, body) in [
            ("GET", format!("/admin/leaderboards/{lb}"), None),
            (
                "POST",
                format!("/admin/leaderboards/{lb}/seasons"),
                Some(form.to_string()),
            ),
            (
                "POST",
                format!("/admin/leaderboards/{lb}/seasons/{season}/edit"),
                Some(form.to_string()),
            ),
            (
                "POST",
                format!("/admin/leaderboards/{lb}/seasons/{season}/delete"),
                Some("x=1".to_string()),
            ),
        ] {
            let res = send(&state, method, &path, session, body).await;
            assert_eq!(res.status, expected, "{method} {path}");
        }
    }
    assert_eq!(season_row(&pool, season).await.0, "Preseason");
    assert_eq!(season_count(&pool, lb).await, 1);

    // Unknown league / season: 404 for an admin.
    let admin = create_user_session(&pool, 4_400_041, true).await;
    let res = send(
        &state,
        "GET",
        &format!("/admin/leaderboards/{}", Uuid::new_v4()),
        Some(admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let other = league(&pool, "Other").await;
    let res = send(
        &state,
        "POST",
        &format!("/admin/leaderboards/{other}/seasons/{season}/edit"),
        Some(admin),
        Some("name=Hijack&timezone=UTC".to_string()),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert_eq!(season_row(&pool, season).await.0, "Preseason");
}

#[sqlx::test(migrations = "../migrations")]
async fn admin_create_season_rules(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let admin = create_user_session(&pool, 4_400_050, true).await;
    let lb = league(&pool, "Create Rules").await;
    let pre = preseason(&pool, lb).await;
    let path = format!("/admin/leaderboards/{lb}/seasons");

    // Overlapping the open-ended Preseason: the constraint's error, as a flash.
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=Season+1&starts_at=2030-01-01T00%3A00&ends_at=&timezone=America%2FNew_York",
    )
    .await;
    assert!(
        page.contains("first set the current season&#39;s end date")
            || page.contains("first set the current season's end date"),
        "{page}"
    );
    assert!(page.contains("overlaps another season"));
    assert_eq!(season_count(&pool, lb).await, 1);

    // A past start is refused before any row is written.
    set_season_days(&pool, pre, -30, Some(-1)).await;
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=Old&starts_at=2020-01-01T00%3A00&timezone=America%2FNew_York",
    )
    .await;
    assert!(
        page.contains("can&#39;t start in the past") || page.contains("can't start in the past"),
        "{page}"
    );
    assert_eq!(season_count(&pool, lb).await, 1);

    // A future season in New York time is stored as its UTC instant.
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=Season+1&starts_at=2030-01-01T00%3A00&ends_at=2030-04-01T00%3A00&timezone=America%2FNew_York",
    )
    .await;
    assert!(page.contains("Scheduled Season 1."), "{page}");
    assert!(!page.contains("overlaps"));
    assert_eq!(season_count(&pool, lb).await, 2);
    let (starts_at, ends_at): (DateTime<Utc>, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT starts_at, ends_at FROM leaderboard_seasons WHERE leaderboard_id = $1 AND name = 'Season 1'",
    )
    .bind(lb)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        starts_at,
        Utc.with_ymd_and_hms(2030, 1, 1, 5, 0, 0).unwrap()
    );
    assert_eq!(
        ends_at,
        Some(Utc.with_ymd_and_hms(2030, 4, 1, 4, 0, 0).unwrap())
    );

    // Bad timezone and DST-skipped times are form errors.
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=S2&starts_at=2031-01-01T00%3A00&timezone=Not%2FAZone",
    )
    .await;
    assert!(page.contains("Unknown timezone"), "{page}");
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=S2&starts_at=2031-03-09T02%3A30&timezone=America%2FNew_York",
    )
    .await;
    assert!(page.contains("daylight-saving starts that night"), "{page}");
    assert_eq!(season_count(&pool, lb).await, 2);
}

#[sqlx::test(migrations = "../migrations")]
async fn admin_edit_rules_by_phase(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let admin = create_user_session(&pool, 4_400_060, true).await;
    let lb = league(&pool, "Edit Rules").await;
    let ended = preseason(&pool, lb).await;
    set_season_days(&pool, ended, -30, Some(-10)).await;
    let active = insert_season(&pool, lb, "Season 1", -10, Some(10)).await;
    let future = insert_season(&pool, lb, "Season 2", 10, Some(40)).await;
    let edit = |id: Uuid| format!("/admin/leaderboards/{lb}/seasons/{id}/edit");
    let tz = "timezone=UTC";

    // Ended: rename yes, dates no.
    let before = season_row(&pool, ended).await;
    let page = post_and_follow(&state, &edit(ended), admin, &format!("name=Old+Times&{tz}")).await;
    assert!(page.contains("Saved Old Times."), "{page}");
    assert_eq!(season_row(&pool, ended).await.0, "Old Times");
    let page = post_and_follow(
        &state,
        &edit(ended),
        admin,
        &format!("name=Old+Times&starts_at=2020-01-01T00%3A00&{tz}"),
    )
    .await;
    assert!(page.contains("only its name can change"), "{page}");
    assert_eq!(season_row(&pool, ended).await.1, before.1);

    // Active: ends_at forward ok, past refused, cleared ok, starts_at refused.
    let (_, active_start, _) = season_row(&pool, active).await;
    let soon = Utc::now() + chrono::Duration::hours(1);
    let soon_local = soon.format("%Y-%m-%dT%H%%3A%M").to_string();
    let page = post_and_follow(
        &state,
        &edit(active),
        admin,
        &format!("name=Season+1&ends_at={soon_local}&{tz}"),
    )
    .await;
    assert!(page.contains("Saved Season 1."), "{page}");
    let stored_end = season_row(&pool, active).await.2.unwrap();
    assert_eq!(
        stored_end.format("%Y-%m-%dT%H:%M").to_string(),
        soon.format("%Y-%m-%dT%H:%M").to_string()
    );
    let past = (Utc::now() - chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H%%3A%M")
        .to_string();
    let page = post_and_follow(
        &state,
        &edit(active),
        admin,
        &format!("name=Season+1&ends_at={past}&{tz}"),
    )
    .await;
    assert!(
        page.contains("can&#39;t be set in the past") || page.contains("can't be set in the past"),
        "{page}"
    );
    assert_eq!(season_row(&pool, active).await.2, Some(stored_end));
    // Clearing would overlap Season 2, so move Season 2 out of the way first
    // by deleting it, then clear.
    let page = post_and_follow(
        &state,
        &format!("/admin/leaderboards/{lb}/seasons/{future}/delete"),
        admin,
        "x=1",
    )
    .await;
    assert!(page.contains("Season deleted."), "{page}");
    assert_eq!(season_count(&pool, lb).await, 2);
    let page = post_and_follow(
        &state,
        &edit(active),
        admin,
        &format!("name=Season+1&ends_at=&{tz}"),
    )
    .await;
    assert!(page.contains("Saved Season 1."), "{page}");
    assert_eq!(season_row(&pool, active).await.2, None);
    let page = post_and_follow(
        &state,
        &edit(active),
        admin,
        &format!("name=Season+1&starts_at=2040-01-01T00%3A00&{tz}"),
    )
    .await;
    assert!(
        page.contains("its start can&#39;t change") || page.contains("its start can't change"),
        "{page}"
    );
    assert_eq!(season_row(&pool, active).await.1, active_start);
}

#[sqlx::test(migrations = "../migrations")]
async fn admin_future_season_edit_and_delete(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let admin = create_user_session(&pool, 4_400_070, true).await;
    let lb = league(&pool, "Future Rules").await;
    let pre = preseason(&pool, lb).await;
    // Predecessor ended 2 days ago; the next season is 10 days out.
    set_season_days(&pool, pre, -30, Some(-2)).await;
    let future = insert_season(&pool, lb, "Season 1", 10, Some(40)).await;
    let path = format!("/admin/leaderboards/{lb}/seasons/{future}/edit");

    // Both dates move (UTC form).
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=Season+One&starts_at=2030-01-01T00%3A00&ends_at=2030-02-01T00%3A00&timezone=UTC",
    )
    .await;
    assert!(page.contains("Saved Season One."), "{page}");
    let row = season_row(&pool, future).await;
    assert_eq!(row.1, Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap());
    assert_eq!(
        row.2,
        Some(Utc.with_ymd_and_hms(2030, 2, 1, 0, 0, 0).unwrap())
    );

    // A start after the predecessor's end but before now: refused, unchanged.
    let yesterday = (Utc::now() - chrono::Duration::days(1))
        .format("%Y-%m-%dT%H%%3A%M")
        .to_string();
    let page = post_and_follow(
        &state,
        &path,
        admin,
        &format!("name=Season+One&starts_at={yesterday}&ends_at=2030-02-01T00%3A00&timezone=UTC"),
    )
    .await;
    assert!(
        page.contains("can&#39;t start in the past") || page.contains("can't start in the past"),
        "{page}"
    );
    assert_eq!(season_row(&pool, future).await, row);

    // End before start: refused.
    let page = post_and_follow(
        &state,
        &path,
        admin,
        "name=Season+One&starts_at=2030-03-01T00%3A00&ends_at=2030-02-01T00%3A00&timezone=UTC",
    )
    .await;
    assert!(page.contains("The end must be after the start."), "{page}");
    assert_eq!(season_row(&pool, future).await, row);

    // Delete guards: ended season refused; season with history refused.
    let page = post_and_follow(
        &state,
        &format!("/admin/leaderboards/{lb}/seasons/{pre}/delete"),
        admin,
        "x=1",
    )
    .await;
    assert!(
        page.contains("Only a season that hasn&#39;t started")
            || page.contains("Only a season that hasn't started"),
        "{page}"
    );
    let user = owner(&pool, 4_400_071).await;
    let s = snake(&pool, user, "Early Bird").await;
    let early = entry(&pool, lb, future, s, 0, 0.0).await;
    let page = post_and_follow(
        &state,
        &format!("/admin/leaderboards/{lb}/seasons/{future}/delete"),
        admin,
        "x=1",
    )
    .await;
    assert!(page.contains("already has entries or games"), "{page}");
    assert_eq!(season_count(&pool, lb).await, 2);
    sqlx::query("DELETE FROM leaderboard_entries WHERE leaderboard_entry_id = $1")
        .bind(early)
        .execute(&pool)
        .await
        .unwrap();
    let page = post_and_follow(
        &state,
        &format!("/admin/leaderboards/{lb}/seasons/{future}/delete"),
        admin,
        "x=1",
    )
    .await;
    assert!(page.contains("Season deleted."), "{page}");
    assert_eq!(season_count(&pool, lb).await, 1);

    // Deleting the active season is refused too.
    let lb2 = league(&pool, "Live One").await;
    let live = preseason(&pool, lb2).await;
    let page = post_and_follow(
        &state,
        &format!("/admin/leaderboards/{lb2}/seasons/{live}/delete"),
        admin,
        "x=1",
    )
    .await;
    assert!(
        page.contains("Only a season that hasn&#39;t started")
            || page.contains("Only a season that hasn't started"),
        "{page}"
    );
    assert_eq!(season_count(&pool, lb2).await, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn admin_page_renders_fields_by_phase(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let admin = create_user_session(&pool, 4_400_080, true).await;
    let lb = league(&pool, "Phase Page").await;
    let ended = preseason(&pool, lb).await;
    set_season_days(&pool, ended, -30, Some(-10)).await;
    let active = insert_season(&pool, lb, "Season 1", -10, Some(10)).await;
    let future = insert_season(&pool, lb, "Season 2", 10, None).await;

    let res = send(
        &state,
        "GET",
        &format!("/admin/leaderboards/{lb}"),
        Some(admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let card = |id: Uuid| {
        let marker = format!("/seasons/{id}/edit");
        let start = res.body.find(&marker).expect("season form");
        let end = res.body[start..].find("</article>").unwrap() + start;
        res.body[start..end].to_string()
    };
    let ended_card = card(ended);
    assert!(ended_card.contains(r#"name="name""#));
    assert!(!ended_card.contains(r#"name="starts_at""#));
    assert!(!ended_card.contains(r#"name="ends_at""#));
    assert!(!ended_card.contains("Delete"));
    let active_card = card(active);
    assert!(active_card.contains(r#"name="ends_at""#));
    assert!(active_card.contains(r#"name="ends_at_current""#));
    assert!(!active_card.contains(r#"name="starts_at""#));
    assert!(!active_card.contains("Delete"));
    let future_card = card(future);
    assert!(future_card.contains(r#"name="starts_at""#));
    assert!(future_card.contains(r#"name="starts_at_current""#));
    assert!(future_card.contains(r#"name="ends_at""#));
    assert!(future_card.contains("Delete season"));
    // Default timezone, and the create form's overlap hint.
    assert!(res.body.contains(r#"value="America/New_York""#));
    assert!(res.body.contains("Times are shown in America/New_York."));

    // The leaderboards list links each league to its seasons.
    let list = send(&state, "GET", "/admin/leaderboards", Some(admin), None).await;
    assert!(
        list.body
            .contains(&format!(r#"href="/admin/leaderboards/{lb}""#))
    );
}

/// A season created in UTC whose start reads as an ambiguous New York time
/// can still be renamed from a New York form: the hidden current instant
/// passes through. Without it, the same local value is refused.
#[sqlx::test(migrations = "../migrations")]
async fn admin_rename_passes_ambiguous_prefill_through(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let admin = create_user_session(&pool, 4_400_090, true).await;
    let lb = league(&pool, "DST League").await;
    let pre = preseason(&pool, lb).await;
    sqlx::query("UPDATE leaderboard_seasons SET ends_at = clock_timestamp() + interval '1 hour' WHERE season_id = $1")
        .bind(pre)
        .execute(&pool)
        .await
        .unwrap();
    // 2031-11-02 01:30 New York happens twice (05:30Z and 06:30Z).
    let page = post_and_follow(
        &state,
        &format!("/admin/leaderboards/{lb}/seasons"),
        admin,
        "name=Fall&starts_at=2031-11-02T06%3A30&timezone=UTC",
    )
    .await;
    assert!(page.contains("Scheduled Fall."), "{page}");
    let season: Uuid =
        sqlx::query_scalar("SELECT season_id FROM leaderboard_seasons WHERE name = 'Fall'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let stored = Utc.with_ymd_and_hms(2031, 11, 2, 6, 30, 0).unwrap();
    assert_eq!(season_row(&pool, season).await.1, stored);

    // The New York page prefills the ambiguous wall time.
    let res = send(
        &state,
        "GET",
        &format!("/admin/leaderboards/{lb}?timezone=America%2FNew_York"),
        Some(admin),
        None,
    )
    .await;
    assert!(
        res.body.contains(r#"value="2031-11-02T01:30""#),
        "{}",
        res.body
    );

    let path = format!("/admin/leaderboards/{lb}/seasons/{season}/edit");
    let current = urlencoding::encode(&stored.to_rfc3339()).into_owned();
    let page = post_and_follow(
        &state,
        &path,
        admin,
        &format!("name=Fall+Classic&starts_at=2031-11-02T01%3A30&starts_at_current={current}&ends_at=&timezone=America%2FNew_York"),
    )
    .await;
    assert!(page.contains("Saved Fall Classic."), "{page}");
    assert_eq!(
        season_row(&pool, season).await,
        ("Fall Classic".to_string(), stored, None)
    );

    // Without (or with a malformed) hidden current: refused, unchanged.
    for hidden in ["", "&starts_at_current=garbage"] {
        let page = post_and_follow(
            &state,
            &path,
            admin,
            &format!(
                "name=Renamed&starts_at=2031-11-02T01%3A30{hidden}&timezone=America%2FNew_York"
            ),
        )
        .await;
        assert!(page.contains("repeated hour"), "{page}");
        assert_eq!(season_row(&pool, season).await.0, "Fall Classic");
    }
}

/// A finished ladder game in `season_id`, created `minutes_ago` minutes ago.
async fn ladder_game(pool: &PgPool, leaderboard_id: Uuid, season_id: Uuid, minutes_ago: i32) {
    let game_id: Uuid = sqlx::query_scalar(
        "INSERT INTO games (board_size, game_type, status)
         VALUES ('11x11', 'Standard', 'finished') RETURNING game_id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO leaderboard_games (leaderboard_id, game_id, season_id, created_at)
         VALUES ($1, $2, $3, clock_timestamp() - make_interval(mins => $4))",
    )
    .bind(leaderboard_id)
    .bind(game_id)
    .bind(season_id)
    .bind(minutes_ago)
    .execute(pool)
    .await
    .unwrap();
}

/// Viewing a past season while one is live: the matchmaker countdown comes
/// from the *active* season's last game, never the viewed season's
/// (ever-staler) one.
#[sqlx::test(migrations = "../migrations")]
async fn past_season_view_counts_down_from_the_active_seasons_last_game(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let r = rollover(&pool).await;
    // Ada + Cy are live members: enough for the countdown branch.
    assert!(
        leaderboard::count_active_entries(&pool, r.lb)
            .await
            .unwrap()
            >= leaderboard::MIN_MATCH_SIZE as i64
    );
    // Preseason's last game is days old; Season 1's is a minute old.
    ladder_game(&pool, r.lb, r.preseason, 3 * 24 * 60).await;
    ladder_game(&pool, r.lb, r.season1, 1).await;

    for query in [String::new(), format!("?season={}", r.preseason)] {
        let res = get(&state, &format!("/leaderboards/{}{query}", r.lb)).await;
        assert_eq!(res.status, StatusCode::OK);
        let cell = matchmaker_cell(&res.body);
        // last Season 1 game + interval is in the future; the Preseason
        // game + interval would read "… days ago".
        assert!(cell.contains("in "), "{query}: {cell}");
        assert!(!cell.contains("ago"), "{query}: {cell}");
        assert!(!cell.contains("waiting"), "{query}: {cell}");
    }
}

/// Tournament import during a gap draws on the last ended season's frozen
/// standings, but only from current live members: a leaver and a retired
/// snake are skipped (not a 500), and the remaining member still imports.
#[sqlx::test(migrations = "../migrations")]
async fn import_in_a_gap_skips_leavers_and_retired_snakes(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let lb = league(&pool, "Gap Ladder").await;
    let pre = preseason(&pool, lb).await;
    set_season_days(&pool, pre, -30, Some(-1)).await;
    insert_season(&pool, lb, "Season 1", 7, None).await;

    let user = owner(&pool, 4_400_101).await;
    let retired = snake(&pool, user, "Retired").await;
    let leaver = snake(&pool, user, "Leaver").await;
    let member = snake(&pool, user, "Member").await;
    for s in [retired, leaver, member] {
        register(&pool, lb, s).await;
    }
    entry(&pool, lb, pre, retired, 12, 90.0).await;
    entry(&pool, lb, pre, leaver, 12, 80.0).await;
    entry(&pool, lb, pre, member, 12, 70.0).await;
    leave(&pool, lb, leaver).await;
    crate::models::battlesnake::retire_battlesnake(&pool, retired, user)
        .await
        .unwrap();

    // The frozen standings still show the leaver.
    let res = get(&state, &format!("/leaderboards/{lb}")).await;
    for name in ["Leaver", "Member"] {
        assert!(res.body.contains(&format!(">{name}</a>")), "{name}");
    }

    let organizer = create_user_session(&pool, 4_400_102, false).await;
    let organizer_id = session_user_id(&pool, organizer).await;
    let tournament_id: Uuid = sqlx::query_scalar(
        "INSERT INTO tournaments (name, user_id, status)
         VALUES ('Gap Cup', $1, 'registration') RETURNING tournament_id",
    )
    .bind(organizer_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let res = send(
        &state,
        "POST",
        &format!("/tournaments/{tournament_id}/import-leaderboard"),
        Some(organizer),
        Some(format!("leaderboard_id={lb}&count=8")),
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.body);

    let imported: Vec<Uuid> = sqlx::query_scalar(
        "SELECT battlesnake_id FROM tournament_registrations
         WHERE tournament_id = $1 ORDER BY seed",
    )
    .bind(tournament_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(imported, vec![member]);
}
