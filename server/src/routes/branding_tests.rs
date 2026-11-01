//! DEV-1751: the product is branded Battlesnake. No user-visible "Arena"
//! (case-sensitive, word-boundary) may appear on any rendered page, and the
//! wordmark + `og:site_name` must read "Battlesnake".

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::models::session::SESSION_COOKIE_NAME;
use crate::routes::test_support::{create_user_session, signed_session_cookie};
use crate::state::AppState;

/// Mirrors `git grep -w Arena` (and JS `\b`): case-sensitive, word-boundary.
/// `_` counts as a word character, so "ArenaStatus" is allowed but a
/// standalone "Arena" fails.
fn assert_no_arena(html: &str, path: &str) {
    for (idx, _) in html.match_indices("Arena") {
        let is_word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let before = html[..idx].chars().next_back();
        let after = html[idx + "Arena".len()..].chars().next();
        if !(is_word(before) || is_word(after)) {
            // Back off to a char boundary: the page HTML is full of
            // multi-byte em dashes, and a mid-char slice would panic with
            // "byte index is not a char boundary" instead of this message.
            let mut from = idx.saturating_sub(60);
            while !html.is_char_boundary(from) {
                from -= 1;
            }
            let start = html[from..idx].lines().last().unwrap_or("");
            panic!("{path} renders user-visible \"Arena\": …{start}Arena…");
        }
    }
}

fn assert_brand(html: &str) {
    assert!(html.contains(r#"<meta property="og:site_name" content="Battlesnake">"#));
    assert!(html.contains(r#"aria-label="Battlesnake — home""#));
    // The header carries the official wordmark artwork, not the old glyph
    // + text wordmark.
    assert!(html.contains(r#"class="wordmark-word""#));
    assert!(html.contains(r#"viewBox="0 0 710 260""#));
    assert!(!html.contains(r#"class="glyph""#));
    assert!(!html.contains("wordmark-text"));
}

async fn request(
    state: &AppState,
    session: Option<Uuid>,
    method: &str,
    path: &str,
    body: &str,
) -> Response {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(session) = session {
        builder = builder.header(
            header::COOKIE,
            format!(
                "{}={}",
                SESSION_COOKIE_NAME,
                signed_session_cookie(state, session)
            ),
        );
    }
    if method == "POST" {
        builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    }
    crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn get_ok(state: &AppState, session: Option<Uuid>, path: &str) -> String {
    let response = request(state, session, "GET", path, "").await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn anonymous_public_pages_carry_no_arena(db: PgPool) {
    let state = AppState::test_from_pool(db);
    let paths = [
        "/",
        "/leaderboards",
        "/tournaments",
        "/snakes",
        "/players",
        "/rankings",
        "/customizations",
        "/customizations/achievements",
        "/customizations/studio",
        "/customizations/studio/guide",
        "/conduct",
        "/privacy",
        "/terms",
        "/_/version",
        "/settings/notification-email/confirm?token=sample",
    ];
    for path in paths {
        let html = get_ok(&state, None, path).await;
        assert_no_arena(&html, path);
    }

    let home = get_ok(&state, None, "/").await;
    assert_brand(&home);
    assert!(home.contains("Battlesnake. Your code is the"));
    assert!(home.contains("<title>Home — Battlesnake</title>"));
}

#[sqlx::test(migrations = "../migrations")]
async fn seeded_detail_pages_carry_no_arena(db: PgPool) {
    let state = AppState::test_from_pool(db);

    // Seed with no "Arena" in any user-chosen name.
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (external_github_id, github_login, github_access_token)
         VALUES ((SELECT COALESCE(MAX(external_github_id), 777000) + 1 FROM users), 'branding-owner', '')
         RETURNING user_id",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    let snake_id: Uuid = sqlx::query_scalar(
        "INSERT INTO battlesnakes (user_id, name, url)
         VALUES ($1, 'Branding Test Snake', 'https://example.com')
         RETURNING battlesnake_id",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .unwrap();
    let game_id: Uuid = sqlx::query_scalar(
        "INSERT INTO games (board_size, game_type, status)
         VALUES ('7x7', 'Standard', 'waiting') RETURNING game_id",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id) VALUES ($1, $2)")
        .bind(game_id)
        .bind(snake_id)
        .execute(&state.db)
        .await
        .unwrap();
    let leaderboard_id: Uuid =
        sqlx::query_scalar("SELECT leaderboard_id FROM leaderboards WHERE name = 'Standard 11x11'")
            .fetch_one(&state.db)
            .await
            .unwrap();

    let paths = [
        format!("/leaderboards/{leaderboard_id}"),
        format!("/games/{game_id}"),
        "/users/branding-owner".to_string(),
        format!("/battlesnakes/{snake_id}/profile"),
    ];
    for path in &paths {
        let html = get_ok(&state, None, path).await;
        assert_no_arena(&html, path);
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn signed_in_pages_carry_no_arena(db: PgPool) {
    let state = AppState::test_from_pool(db);
    let session = create_user_session(&state.db, 9960001, false).await;

    let paths = [
        "/me",
        "/battlesnakes",
        "/battlesnakes/new",
        "/claim",
        "/claim/email",
        "/claim/email/verify?token=sample",
        "/settings/notification-email/confirm?token=sample",
    ];
    for path in paths {
        let html = get_ok(&state, Some(session), path).await;
        assert_no_arena(&html, path);
    }

    // Prove the session took effect (these pages 401 anonymously).
    let me = get_ok(&state, Some(session), "/me").await;
    assert!(me.contains("test-user-9960001"));

    // POST flows that render copy directly (200, no redirect).
    let sent_notice = request(
        &state,
        Some(session),
        "POST",
        "/claim/email",
        "email=branding-check@example.com",
    )
    .await;
    assert_eq!(sent_notice.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(sent_notice.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_no_arena(&html, "/claim/email POST");

    let verify_failure = request(
        &state,
        Some(session),
        "POST",
        "/claim/email/verify",
        "token=not-a-real-token",
    )
    .await;
    assert_eq!(verify_failure.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(verify_failure.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_no_arena(&html, "/claim/email/verify POST");

    // Game-builder flow: /games/new redirects to a flow URL; the gc-sub
    // branch (fresh user has no snakes) renders there.
    let redirect = request(&state, Some(session), "GET", "/games/new", "").await;
    assert_eq!(redirect.status(), StatusCode::SEE_OTHER);
    let location = redirect
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .expect("location header")
        .to_string();
    let html = get_ok(&state, Some(session), &location).await;
    assert_no_arena(&html, &location);
}

#[sqlx::test(migrations = "../migrations")]
async fn not_found_page_carries_no_arena(db: PgPool) {
    let state = AppState::test_from_pool(db);
    let response = request(&state, None, "GET", "/definitely-not-a-page", "").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let html = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_no_arena(&html, "/definitely-not-a-page");
}
