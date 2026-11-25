//! Host-routing tests for the play.battlesnake.com redirect router (DEV-2697).
//!
//! Every test drives `routes(state).into_router()`, the exact cja stack
//! (host dispatch before route matching, TraceLayer and CookieManagerLayer
//! per branch) that production serves. They pin what the play host answers
//! for each old play URL, that it never renders a page, sets a cookie or
//! creates a session, and that nothing changes for any other host.

use std::sync::Arc;

use axum::http::{StatusCode, header};
use sqlx::PgPool;
use uuid::Uuid;

use crate::routes::redirects::REDIRECTS;
use crate::routes::test_support::{create_user, send_with_host};
use crate::state::AppState;

const PLAY: &str = "play.battlesnake.com";
const ARENA: &str = "arena.battlesnake.com";
const BASE: &str = "https://arena.battlesnake.com";
/// A real play game id (it resolves on arena by its engine id).
const PLAY_GAME: &str = "048106f1-56e2-4f9c-8501-3a62712a600f";

/// Production-like state: arena's public base URL and the given play hosts.
fn state(pool: &PgPool, play_hosts: &[&str]) -> AppState {
    let mut state = AppState::test_from_pool(pool.clone());
    let config = Arc::make_mut(&mut state.config);
    config.base_url = BASE.to_string();
    config.play_redirect_hosts = play_hosts.iter().map(|h| (*h).to_string()).collect();
    state
}

fn app(state: &AppState) -> axum::Router {
    crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router()
}

/// The default wiring: play.battlesnake.com is a play host.
fn play_app(pool: &PgPool) -> axum::Router {
    app(&state(pool, &[PLAY]))
}

fn location(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get(header::LOCATION)
        .map(|v| v.to_str().unwrap().to_string())
}

/// Status and Location for `method path` with `host` (`None` = no Host header).
async fn send(
    router: &axum::Router,
    method: &str,
    path: &str,
    host: Option<&str>,
) -> (StatusCode, Option<String>) {
    let response = match host {
        Some(host) => send_with_host(router, method, path, host, &[]).await,
        None => {
            use tower::ServiceExt as _;
            let request = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            router.clone().oneshot(request).await.unwrap()
        }
    };
    (response.status(), location(&response))
}

/// Assert `path` on the play host is a 307 to exactly `expected`.
async fn assert_redirect(router: &axum::Router, path: &str, expected: &str) {
    let (status, location) = send(router, "GET", path, Some(PLAY)).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{path}");
    assert_eq!(location.as_deref(), Some(expected), "{path}");
}

/// Play URLs, for the sweeps: play's own smoke-test list, then one URL for
/// each row of the cutover inventory, then hostile inputs.
const PLAY_URLS: &[&str] = &[
    // play src/apps/ui/tests/test_urls.py TEST_URLS
    "/",
    "/customizations",
    "/game/bad-game-engine-id",
    "/leaderboards",
    "/leaderboard/standard",
    "/leaderboard/standard/bad-username/stats",
    "/profile/bad-username",
    "/rankings",
    "/tournaments",
    "/login",
    "/register",
    "/recover",
    "/recover/one-bad-token/another-bad-token",
    "/account/battlesnakes",
    "/account/redeem",
    "/account/settings",
    "/account/settings?tab=profile",
    "/account/settings?tab=email",
    "/account/settings?tab=auth",
    "/account/settings?tab=advanced",
    "/account/verify/bad-token",
    "/me",
    "/conduct",
    "/diversity",
    "/mission",
    "/privacy",
    "/terms",
    "/iframes/leaderboard-games?leaderboard=bad-slug",
    // Inventory §1.1 public pages
    "/game/048106f1-56e2-4f9c-8501-3a62712a600f?autoplay=true&turn=5&fps=10&title=x",
    "/game/048106f1-56e2-4f9c-8501-3a62712a600f/rematch",
    "/leaderboard/standard-duels?a=join",
    "/leaderboard/royale/",
    "/leaderboard/constrictor/some-user/stats",
    "/profile/SomeUser",
    "/rankings?page=3",
    "/tournament/spring-league",
    // §1.2 account and auth
    "/logout",
    "/account/games/create?battlesnakes=a,b&map=standard",
    "/account/sponsor",
    "/account/tournaments/create",
    "/oauth/login/github/",
    "/oauth/complete/github/?code=secret&state=abc",
    "/oauth/disconnect/github/",
    "/oauth/disconnect/github/12/",
    // §1.4 short links
    "/sponsor",
    "/sponsors",
    "/docs",
    "/faq",
    "/feedback",
    "/blog",
    "/discord",
    "/facebook",
    "/github",
    "/instagram",
    "/reddit",
    "/twitch",
    "/twitter",
    "/youtube",
    "/discord/?utm_source=play_system",
    "/github/",
    "/twitter/",
    // §1.5 staff
    "/admin/",
    "/admin/versions/",
    "/admin/django/core/account/12/",
    "/admin/account/details/12/",
    // §1.6 webhooks
    "/webhook/engine/?id=abc",
    "/webhook/gcp-alert/",
    "/webhook/github/",
    "/webhook/mailgun/",
    "/webhook/",
    // §1.7 outside Django
    "/version",
    "/healthz/alive",
    "/static/css/site.css",
    "/media/x.png",
    "/robots.txt",
    "/favicon.ico",
    "/sitemap.xml",
    // §1.8 legacy
    "/g/048106f1-56e2-4f9c-8501-3a62712a600f/",
    "/g/048106f1-56e2-4f9c-8501-3a62712a600f/gif/",
    "/game/048106f1-56e2-4f9c-8501-3a62712a600f/gif",
    "/u/SomeUser/",
    "/u/SomeUser/some-snake/",
    "/s/snk_abc123/",
    "/arena/standard/",
    "/ladder/royale/",
    "/league/summer-league-2022/arenas/standard/",
    "/account/dashboard",
    "/account/snakes/create/",
    "/game/create/",
    "/about/conduct/",
    "/league/summer-league-2022/",
    "/competitions/",
    "/events/",
    "/schedule/",
    "/challenges/",
    "/battlegrounds/",
    "/live/",
    "/t/team/",
    "/partner/x",
    "/slack",
    "/summer-league",
    "/survey",
    "/api/games/x",
    "/game/048106f1-56e2-4f9c-8501-3a62712a600f/save",
    "/leaderboard/standard/recent-game/json",
    "/account/tournaments/x",
    "/account/shoutcast-game/x",
    "/customizations/unlock",
    // Hostile input: bad escapes, encoded control characters
    "/game/%FF",
    "/game/a%0Ab",
    "/s/%FF",
    "/u/%FF/snake",
    "/leaderboard/standard/%0A/stats",
    "/rankings?page=%FF",
    // A NUL can't reach Postgres (it would only log an error)
    "/s/%00",
    "/u/%00",
    "/leaderboard/standard/%00/stats",
];

/// Locations the play host may send people to besides arena itself.
fn is_listed_external(location: &str) -> bool {
    location.starts_with("https://exporter.battlesnake.com/games/")
        || REDIRECTS
            .iter()
            .any(|(_, target)| location.starts_with(target))
}

#[sqlx::test(migrations = "../migrations")]
async fn fixed_and_rewritten_routes(pool: PgPool) {
    let router = play_app(&pool);
    let welcome = format!("{BASE}/welcome-back");
    let cases: Vec<(String, String)> = vec![
        // R1: home → welcome-back, query kept
        ("/".into(), welcome.clone()),
        ("/?utm_source=x".into(), format!("{welcome}?utm_source=x")),
        // R2 / R10: game pages, query kept verbatim
        (
            format!("/game/{PLAY_GAME}?turn=5&autoplay=true"),
            format!("{BASE}/games/{PLAY_GAME}?turn=5&autoplay=true"),
        ),
        (
            format!("/game/{PLAY_GAME}/"),
            format!("{BASE}/games/{PLAY_GAME}"),
        ),
        (
            format!("/g/{PLAY_GAME}"),
            format!("{BASE}/games/{PLAY_GAME}"),
        ),
        (
            format!("/g/{PLAY_GAME}/?fps=10"),
            format!("{BASE}/games/{PLAY_GAME}?fps=10"),
        ),
        (
            format!("/g/{PLAY_GAME}/gif"),
            format!("https://exporter.battlesnake.com/games/{PLAY_GAME}/gif"),
        ),
        (
            format!("/game/{PLAY_GAME}/gif/"),
            format!("https://exporter.battlesnake.com/games/{PLAY_GAME}/gif"),
        ),
        // R5: play pages count from 1, arena's from 0
        ("/rankings?page=3".into(), format!("{BASE}/rankings?page=2")),
        (
            "/rankings/?page=2".into(),
            format!("{BASE}/rankings?page=1"),
        ),
        ("/rankings".into(), format!("{BASE}/rankings")),
        ("/rankings?page=1".into(), format!("{BASE}/rankings")),
        ("/rankings?page=0".into(), format!("{BASE}/rankings")),
        ("/rankings?page=-4".into(), format!("{BASE}/rankings")),
        ("/rankings?page=abc".into(), format!("{BASE}/rankings")),
        (
            "/rankings?sort=x&page=2".into(),
            format!("{BASE}/rankings?page=1"),
        ),
        // R6: auth pages → welcome-back, query (tokens, OAuth codes) dropped
        ("/login?next=/me".into(), welcome.clone()),
        ("/register/".into(), welcome.clone()),
        ("/recover".into(), welcome.clone()),
        ("/recover/MTI/abc-123/".into(), welcome.clone()),
        ("/account/verify/tok?x=1".into(), welcome.clone()),
        ("/oauth/login/github/".into(), welcome.clone()),
        (
            "/oauth/complete/github/?code=x&state=y".into(),
            welcome.clone(),
        ),
        // R7
        ("/logout".into(), format!("{BASE}/")),
        // R8
        (
            "/account/battlesnakes/".into(),
            format!("{BASE}/battlesnakes"),
        ),
        (
            "/account/games/create?battlesnakes=a".into(),
            format!("{BASE}/games/new"),
        ),
        ("/account/settings?tab=email".into(), format!("{BASE}/me")),
        ("/account/redeem".into(), format!("{BASE}/customizations")),
        (
            "/account/tournaments/create".into(),
            format!("{BASE}/tournaments/new"),
        ),
        (
            "/account/sponsor".into(),
            "https://github.com/sponsors/BattlesnakeOfficial".into(),
        ),
        // R12: case preserved; the site router's /u/ resolves it
        ("/u/SomeUser/my-snake/".into(), format!("{BASE}/u/SomeUser")),
        // R13 fixed pages; unknown ladder slugs get the list
        ("/account/dashboard".into(), format!("{BASE}/battlesnakes")),
        (
            "/account/snakes/create/".into(),
            format!("{BASE}/battlesnakes/new"),
        ),
        ("/game/create".into(), format!("{BASE}/games/new")),
        ("/about/conduct/".into(), format!("{BASE}/conduct")),
        ("/arena/old-ladder".into(), format!("{BASE}/leaderboards")),
        (
            "/league/summer/arenas/bogus/".into(),
            format!("{BASE}/leaderboards"),
        ),
        // F: same path on arena, one trailing slash trimmed, query kept
        ("/leaderboards/".into(), format!("{BASE}/leaderboards")),
        ("/terms".into(), format!("{BASE}/terms")),
        ("/u/Foo/".into(), format!("{BASE}/u/Foo")),
        ("/u/Foo".into(), format!("{BASE}/u/Foo")),
        ("/profile/Foo/".into(), format!("{BASE}/profile/Foo")),
        ("/tournament/x".into(), format!("{BASE}/tournament/x")),
        ("/me/?a=1".into(), format!("{BASE}/me?a=1")),
        ("/admin/".into(), format!("{BASE}/admin")),
        ("/a//".into(), format!("{BASE}/a/")),
        // F: short links go straight to their target, in one hop
        (
            "/discord/?utm_source=play".into(),
            "https://discord.gg/BYubeHQ?utm_source=play".into(),
        ),
        (
            "/mission".into(),
            "https://docs.battlesnake.com/policies/mission".into(),
        ),
        (
            "/diversity/".into(),
            "https://docs.battlesnake.com/policies/diversity".into(),
        ),
        ("/studio/".into(), format!("{BASE}/customizations/studio")),
        // Raw segments go back out still encoded: never a 400 or a 500
        ("/game/%FF".into(), format!("{BASE}/games/%FF")),
        ("/game/a%0Ab".into(), format!("{BASE}/games/a%0Ab")),
        ("/u/%FF/snake".into(), format!("{BASE}/u/%FF")),
    ];
    for (path, expected) in &cases {
        assert_redirect(&router, path, expected).await;
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn protocol_relative_paths_stay_on_arena(pool: PgPool) {
    let router = play_app(&pool);
    let (status, location) = send(&router, "GET", "//evil.example/x", Some(PLAY)).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    let location = location.unwrap();
    assert!(location.starts_with(&format!("{BASE}/")), "{location}");
}

async fn ladder_id(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar("SELECT leaderboard_id FROM leaderboards WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn play_ladders_resolve_by_name(pool: PgPool) {
    let router = play_app(&pool);
    for (slug, name) in [
        ("standard", "Standard 11x11"),
        ("standard-duels", "Duels 11x11"),
        ("royale", "Royale 11x11"),
        ("constrictor", "Constrictor 11x11"),
    ] {
        let expected = format!("{BASE}/leaderboards/{}", ladder_id(&pool, name).await);
        for path in [
            format!("/leaderboard/{slug}"),
            format!("/leaderboard/{slug}/?a=join"),
            format!("/arena/{slug}"),
            format!("/ladder/{slug}/"),
            format!("/league/summer-league/arenas/{slug}/"),
        ] {
            assert_redirect(&router, &path, &expected).await;
        }
    }

    // Slugs play never had take the fallback (arena's 404 page).
    assert_redirect(
        &router,
        "/leaderboard/bogus/",
        &format!("{BASE}/leaderboard/bogus"),
    )
    .await;

    // A known slug whose ladder isn't in this database (or isn't public).
    sqlx::query("UPDATE leaderboards SET name = 'Retired Royale' WHERE name = 'Royale 11x11'")
        .execute(&pool)
        .await
        .unwrap();
    for path in ["/leaderboard/royale", "/arena/royale"] {
        assert_redirect(&router, path, &format!("{BASE}/leaderboards")).await;
    }
}

async fn set_login(pool: &PgPool, user_id: Uuid, login: &str) {
    sqlx::query("UPDATE users SET github_login = $2 WHERE user_id = $1")
        .bind(user_id)
        .bind(login)
        .execute(pool)
        .await
        .unwrap();
}

/// An imported play account named `username`, claimed by `claimed_by` if given.
async fn play_account(pool: &PgPool, username: &str, claimed_by: Option<Uuid>) {
    sqlx::query(
        "INSERT INTO imported_accounts
            (play_user_id, play_account_id, email, username, password_hash, is_email_verified,
             claimed_by_user_id, claimed_at)
         VALUES ($1, $1, $1 || '@example.com', $1, 'unused', true, $2,
                 CASE WHEN $2::uuid IS NULL THEN NULL ELSE NOW() END)",
    )
    .bind(username)
    .bind(claimed_by)
    .execute(pool)
    .await
    .unwrap();
}

async fn create_snake(pool: &PgPool, user_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO battlesnakes (user_id, name, url)
         VALUES ($1, $2, 'http://example.com/snake') RETURNING battlesnake_id",
    )
    .bind(user_id)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Register a snake on a ladder; with a score, also give it an entry in the
/// ladder's active season. Returns the entry id.
async fn join_ladder(
    pool: &PgPool,
    leaderboard_id: Uuid,
    battlesnake_id: Uuid,
    score: Option<f64>,
) -> Option<Uuid> {
    sqlx::query(
        "INSERT INTO leaderboard_registrations (leaderboard_id, battlesnake_id) VALUES ($1, $2)",
    )
    .bind(leaderboard_id)
    .bind(battlesnake_id)
    .execute(pool)
    .await
    .unwrap();
    let score = score?;
    Some(
        sqlx::query_scalar(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, season_id, display_score)
             SELECT $1, $2, season_id, $3 FROM leaderboard_seasons
             WHERE leaderboard_id = $1 AND starts_at <= NOW() AND (ends_at IS NULL OR NOW() < ends_at)
             RETURNING leaderboard_entry_id",
        )
        .bind(leaderboard_id)
        .bind(battlesnake_id)
        .bind(score)
        .fetch_one(pool)
        .await
        .unwrap(),
    )
}

#[sqlx::test(migrations = "../migrations")]
async fn ladder_stats_go_to_the_players_best_entry(pool: PgPool) {
    let router = play_app(&pool);
    let standard = ladder_id(&pool, "Standard 11x11").await;
    let ladder = format!("{BASE}/leaderboards/{standard}");

    // A claimed play account: its best active-season entry wins, whatever
    // the claimant's GitHub login is now.
    let claimant = create_user(&pool, 53001, false).await;
    set_login(&pool, claimant, "new-login").await;
    play_account(&pool, "OldPlayName", Some(claimant)).await;
    let weaker = create_snake(&pool, claimant, "weaker").await;
    let stronger = create_snake(&pool, claimant, "stronger").await;
    join_ladder(&pool, standard, weaker, Some(10.0)).await;
    let best = join_ladder(&pool, standard, stronger, Some(30.0))
        .await
        .unwrap();
    for path in [
        "/leaderboard/standard/OldPlayName/stats",
        "/leaderboard/standard/oldplayname/stats/",
    ] {
        assert_redirect(&router, path, &format!("{ladder}/entries/{best}")).await;
    }

    // No play account, but the GitHub login matches (case-insensitively).
    let github_user = create_user(&pool, 53002, false).await;
    set_login(&pool, github_user, "GhUser").await;
    let snake = create_snake(&pool, github_user, "gh-snake").await;
    let entry = join_ladder(&pool, standard, snake, Some(5.0))
        .await
        .unwrap();
    assert_redirect(
        &router,
        "/leaderboard/standard/ghuser/stats",
        &format!("{ladder}/entries/{entry}"),
    )
    .await;

    // Claimed but never joined this ladder; joined but no active-season
    // entry yet; unknown players: all get the ladder itself.
    let idle = create_user(&pool, 53003, false).await;
    play_account(&pool, "idle-player", Some(idle)).await;
    let waiting = create_user(&pool, 53004, false).await;
    set_login(&pool, waiting, "waiting").await;
    let waiting_snake = create_snake(&pool, waiting, "waiting-snake").await;
    join_ladder(&pool, standard, waiting_snake, None).await;
    play_account(&pool, "unclaimed", None).await;
    for path in [
        "/leaderboard/standard/idle-player/stats",
        "/leaderboard/standard/waiting/stats",
        "/leaderboard/standard/unclaimed/stats",
        "/leaderboard/standard/nobody-at-all/stats",
        "/leaderboard/standard/%0A/stats",
    ] {
        assert_redirect(&router, path, &ladder).await;
    }

    // The entry is on the requested ladder only.
    let duels = ladder_id(&pool, "Duels 11x11").await;
    assert_redirect(
        &router,
        "/leaderboard/standard-duels/OldPlayName/stats",
        &format!("{BASE}/leaderboards/{duels}"),
    )
    .await;

    // Unknown slug → fallback.
    assert_redirect(
        &router,
        "/leaderboard/bogus/OldPlayName/stats/",
        &format!("{BASE}/leaderboard/bogus/OldPlayName/stats"),
    )
    .await;
}

async fn play_snake(pool: &PgPool, owner: &str, play_snake_id: &str, materialized: Option<Uuid>) {
    sqlx::query(
        "INSERT INTO imported_snakes
            (imported_account_id, play_snake_id, name, url, materialized_battlesnake_id)
         SELECT imported_account_id, $2, 'Old Snake', 'http://example.com', $3
         FROM imported_accounts WHERE username = $1",
    )
    .bind(owner)
    .bind(play_snake_id)
    .bind(materialized)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../migrations")]
async fn play_snake_ids_resolve_through_the_import(pool: PgPool) {
    let router = play_app(&pool);

    // Claimed and materialized: the arena snake's page.
    let claimant = create_user(&pool, 54001, false).await;
    set_login(&pool, claimant, "claimant").await;
    play_account(&pool, "mover", Some(claimant)).await;
    let snake = create_snake(&pool, claimant, "moved-snake").await;
    play_snake(&pool, "mover", "snk_moved", Some(snake)).await;
    assert_redirect(
        &router,
        "/s/snk_moved/",
        &format!("{BASE}/battlesnakes/{snake}/profile"),
    )
    .await;

    // Owner claimed, this snake not brought over: the owner's profile.
    play_snake(&pool, "mover", "snk_left_behind", None).await;
    assert_redirect(
        &router,
        "/s/snk_left_behind",
        &format!("{BASE}/users/claimant/{claimant}"),
    )
    .await;

    // Owner never claimed: their play username (`/u/` says not moved yet).
    play_account(&pool, "Stayer", None).await;
    play_snake(&pool, "Stayer", "snk_stayed", None).await;
    assert_redirect(&router, "/s/snk_stayed", &format!("{BASE}/u/Stayer")).await;

    // Claimed, but the arena snake was deleted since: the owner's profile.
    let deleted = create_snake(&pool, claimant, "deleted-snake").await;
    sqlx::query("UPDATE battlesnakes SET deleted_at = now() WHERE battlesnake_id = $1")
        .bind(deleted)
        .execute(&pool)
        .await
        .unwrap();
    play_snake(&pool, "mover", "snk_deleted", Some(deleted)).await;
    assert_redirect(
        &router,
        "/s/snk_deleted",
        &format!("{BASE}/users/claimant/{claimant}"),
    )
    .await;

    // Never imported, or not decodable: the fallback.
    assert_redirect(&router, "/s/snk_missing/", &format!("{BASE}/s/snk_missing")).await;
    assert_redirect(&router, "/s/%FF", &format!("{BASE}/s/%FF")).await;
    assert_redirect(&router, "/s/%00", &format!("{BASE}/s/%00")).await;
}

#[sqlx::test(migrations = "../migrations")]
async fn webhooks_are_gone_for_any_method(pool: PgPool) {
    let router = play_app(&pool);
    for (method, path) in [
        ("POST", "/webhook/github/"),
        ("POST", "/webhook/mailgun/"),
        ("POST", "/webhook/gcp-alert/"),
        ("GET", "/webhook/engine/?id=x"),
        ("POST", "/webhook/engine/?id=x"),
        ("PUT", "/webhook/anything/else"),
        ("POST", "/webhook"),
        ("POST", "/webhook/"),
    ] {
        let response = send_with_host(&router, method, path, PLAY, &[]).await;
        assert_eq!(response.status(), StatusCode::GONE, "{method} {path}");
        assert!(location(&response).is_none(), "{method} {path}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty(), "{method} {path}");
    }
}

/// cja ignores case, port and a trailing dot when matching hosts; only play
/// hosts reach the play router.
#[sqlx::test(migrations = "../migrations")]
async fn host_matching(pool: PgPool) {
    let router = play_app(&pool);
    for host in [
        PLAY,
        "PLAY.battlesnake.com:443",
        "play.battlesnake.com.",
        "Play.Battlesnake.Com.:8080",
    ] {
        let (status, location) = send(&router, "GET", "/", Some(host)).await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{host}");
        assert_eq!(
            location.as_deref(),
            Some(format!("{BASE}/welcome-back").as_str()),
            "{host}"
        );
    }
    for host in [
        Some(ARENA),
        Some("tf-arena-xxxx-uc.a.run.app"),
        Some("battlesnake.com"),
        Some("play.battlesnake.com.evil.example"),
        None,
    ] {
        let (status, location) = send(&router, "GET", "/", host).await;
        assert_eq!(status, StatusCode::OK, "{host:?}");
        assert_eq!(location, None, "{host:?}");
    }
    // Engine hosts keep their own router.
    let (status, _) = send(&router, "GET", "/", Some("engine.battlesnake.com")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Every play URL on the play host: a 307 to arena or a listed external URL,
/// or a bare 410. Never a page, never a 4xx/5xx, never a cookie or a new
/// session row, for any method; HEAD answers exactly like GET.
#[sqlx::test(migrations = "../migrations")]
async fn inventory_sweep_on_the_play_host(pool: PgPool) {
    let router = play_app(&pool);
    let before = session_count(&pool).await;

    for path in PLAY_URLS {
        let mut get = None;
        for method in ["GET", "HEAD", "POST"] {
            let response = send_with_host(&router, method, path, PLAY, &[]).await;
            let status = response.status();
            assert!(
                response.headers().get(header::SET_COOKIE).is_none(),
                "{method} {path} set a cookie"
            );
            let target = location(&response);
            if path.starts_with("/webhook/") {
                assert_eq!(status, StatusCode::GONE, "{method} {path}");
                assert_eq!(target, None, "{method} {path}");
            } else {
                assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{method} {path}");
                let target = target.as_deref().unwrap();
                assert!(
                    target.starts_with(&format!("{BASE}/")) || is_listed_external(target),
                    "{method} {path} → {target}"
                );
            }
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(body.is_empty(), "{method} {path} has a body");
            match method {
                "GET" => get = Some((status, target)),
                "HEAD" => assert_eq!(Some((status, target)), get, "HEAD {path}"),
                _ => {}
            }
        }
    }

    assert_eq!(
        session_count(&pool).await,
        before,
        "the play host created sessions"
    );

    // The counter is live: an arena page render does create a session.
    send(&router, "GET", "/", Some(ARENA)).await;
    assert!(session_count(&pool).await > before);
}

async fn session_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Until DNS moves nothing changes for anyone else: on the arena host, with
/// no Host header, and with the play router switched off, every play URL
/// answers exactly as it does with no play router wired at all.
#[sqlx::test(migrations = "../migrations")]
async fn other_hosts_are_unchanged(pool: PgPool) {
    let baseline = app(&state(&pool, &[]));
    let wired = play_app(&pool);
    let disabled = app(&state(&pool, &["disabled.invalid"]));

    for path in PLAY_URLS {
        let expected = send(&baseline, "GET", path, Some(ARENA)).await;
        assert_eq!(
            send(&wired, "GET", path, Some(ARENA)).await,
            expected,
            "arena host {path}"
        );
        assert_eq!(
            send(&wired, "GET", path, None).await,
            expected,
            "no Host {path}"
        );
        assert_eq!(
            send(&disabled, "GET", path, Some(PLAY)).await,
            expected,
            "disabled.invalid, play host {path}"
        );
    }

    // Spot checks of what "unchanged" means on the arena host.
    let game = format!("/game/{PLAY_GAME}");
    for (path, status) in [
        ("/", StatusCode::OK),
        (game.as_str(), StatusCode::NOT_FOUND),
        ("/leaderboard/standard", StatusCode::NOT_FOUND),
        ("/webhook/github/", StatusCode::NOT_FOUND),
        ("/login", StatusCode::NOT_FOUND),
        ("/rankings?page=2", StatusCode::OK),
    ] {
        let (got, location) = send(&wired, "GET", path, Some(ARENA)).await;
        assert_eq!(got, status, "{path}");
        assert_eq!(location, None, "{path}");
    }
    // The new docs short links are not host-gated.
    assert_eq!(
        send(&wired, "GET", "/mission", Some(ARENA)).await,
        (
            StatusCode::TEMPORARY_REDIRECT,
            Some("https://docs.battlesnake.com/policies/mission".to_string())
        )
    );
}
