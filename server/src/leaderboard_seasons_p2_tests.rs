//! Arena seasons phase 2 (DEV-2660, DEV-2661): league registrations and
//! season-aware matchmaking, dispatch, ratings and membership, end to end.
//!
//! PR 1's tests used the gap-join arrangement (end the Preseason, join during
//! the gap, then start the season) because the (league, snake) entry
//! constraint allowed one entry across seasons. PR 2 dropped that
//! constraint, so a (league, snake) now holds one entry per season.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::models::{
    battlesnake, game,
    leaderboard::{self, LeaderboardEntry},
    leaderboard_registration, leaderboard_season, private_leaderboard, session,
};
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
    session_id: Uuid,
    body: Option<(&str, String)>,
) -> Response {
    let app = crate::routes::routes(state.clone())
        .expect("asset router")
        .into_router();
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ORIGIN, "https://example.com")
        .header(
            header::COOKIE,
            format!(
                "{}={}",
                session::SESSION_COOKIE_NAME,
                signed_session_cookie(state, session_id)
            ),
        );
    let request = match body {
        Some((content_type, body)) => builder
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body)),
        None => {
            builder = builder.header(header::ACCEPT, "text/html");
            builder.body(Body::empty())
        }
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

async fn post_form(state: &AppState, path: &str, session_id: Uuid, form: String) -> Response {
    send(
        state,
        "POST",
        path,
        session_id,
        Some(("application/x-www-form-urlencoded", form)),
    )
    .await
}

async fn post_json(
    state: &AppState,
    path: &str,
    session_id: Uuid,
    json: serde_json::Value,
) -> Response {
    send(
        state,
        "POST",
        path,
        session_id,
        Some(("application/json", json.to_string())),
    )
    .await
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

/// A fresh public league (its Preseason is created by trigger, active now).
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

/// End every open season of the league now: the league is in a gap.
async fn end_open_seasons(pool: &PgPool, leaderboard_id: Uuid) {
    sqlx::query(
        "UPDATE leaderboard_seasons SET ends_at = clock_timestamp()
         WHERE leaderboard_id = $1 AND (ends_at IS NULL OR ends_at > clock_timestamp())",
    )
    .bind(leaderboard_id)
    .execute(pool)
    .await
    .unwrap();
}

/// Start an open-ended season now (the league must be in a gap).
async fn start_season(pool: &PgPool, leaderboard_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
         VALUES ($1, $2, clock_timestamp()) RETURNING season_id",
    )
    .bind(leaderboard_id)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn league_games(pool: &PgPool, leaderboard_id: Uuid) -> Vec<(Uuid, Option<Uuid>)> {
    sqlx::query_as("SELECT game_id, season_id FROM leaderboard_games WHERE leaderboard_id = $1")
        .bind(leaderboard_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn runner_jobs_for(pool: &PgPool, game_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE name = 'GameRunnerJob' AND payload->>'game_id' = $1",
    )
    .bind(game_id.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Registration only (no entry): the snake joins while the league is in a gap.
async fn register(pool: &PgPool, leaderboard_id: Uuid, battlesnake_id: Uuid) -> Uuid {
    leaderboard_registration::get_or_create_registration(pool, leaderboard_id, battlesnake_id)
        .await
        .unwrap()
        .expect("live snake registers")
        .leaderboard_registration_id
}

struct Player {
    session: Uuid,
    user_id: Uuid,
    snakes: [Uuid; 2],
}

async fn player(pool: &PgPool, github_id: i64) -> Player {
    let session = create_user_session(pool, github_id, false).await;
    let user_id = session_user_id(pool, session).await;
    let snakes = [
        snake(pool, user_id, "Season Snake A").await,
        snake(pool, user_id, "Season Snake B").await,
    ];
    Player {
        session,
        user_id,
        snakes,
    }
}

/// A league between seasons: the snakes can join (registration, no entry),
/// nothing is scheduled, and both the web and API surfaces say so cleanly.
#[sqlx::test(migrations = "../migrations")]
async fn gap_join_registers_without_entry_and_schedules_nothing(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let p = player(&pool, 9_266_001).await;

    // League A: Preseason ended, no successor. League B: only a future season.
    let ended = league(&pool, "Gap Ended").await;
    end_open_seasons(&pool, ended).await;
    let future = league(&pool, "Gap Future").await;
    sqlx::query("DELETE FROM leaderboard_seasons WHERE leaderboard_id = $1")
        .bind(future)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
         VALUES ($1, 'Season 1', clock_timestamp() + interval '1 day')",
    )
    .bind(future)
    .execute(&pool)
    .await
    .unwrap();
    // Control: an identical league with an active season does schedule.
    let live = league(&pool, "Gap Control").await;

    for lb in [ended, future] {
        assert!(
            leaderboard_season::get_active_season(&pool, lb)
                .await
                .unwrap()
                .is_none()
        );
        // API join: 201 with no entry and zeroed stats.
        let res = post_json(
            &state,
            &format!("/api/leaderboards/{lb}/entries"),
            p.session,
            serde_json::json!({ "battlesnake_id": p.snakes[0] }),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        let json: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(json["leaderboard_entry_id"], serde_json::Value::Null);
        assert_eq!(json["battlesnake_id"], p.snakes[0].to_string());
        assert_eq!(json["games_played"], 0);
        assert_eq!(json["display_score"], 0.0);
        assert_eq!(json["active"], true);

        // Web join flashes success.
        let res = post_form(
            &state,
            &format!("/leaderboards/{lb}/join"),
            p.session,
            format!("battlesnake_id={}", p.snakes[1]),
        )
        .await;
        assert_eq!(res.status, StatusCode::SEE_OTHER);
        assert_eq!(
            res.location.as_deref(),
            Some(&*format!("/leaderboards/{lb}"))
        );
        let page = send(
            &state,
            "GET",
            &format!("/leaderboards/{lb}"),
            p.session,
            None,
        )
        .await;
        assert!(
            page.body.contains("is in the matchmaking rotation"),
            "{}",
            page.body
        );
        assert!(!page.body.contains("doesn&#39;t exist or has been deleted"));

        let entries: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM leaderboard_entries WHERE leaderboard_id = $1",
        )
        .bind(lb)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(entries, 0, "no season, no entry");
        let registrations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM leaderboard_registrations WHERE leaderboard_id = $1 AND disabled_at IS NULL",
        )
        .bind(lb)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(registrations, 2);

        // The rail's membership read: present, no entry, zeroed stats.
        let memberships = leaderboard::get_user_memberships(&pool, lb, p.user_id)
            .await
            .unwrap();
        assert_eq!(memberships.len(), 2);
        for m in &memberships {
            assert_eq!(m.leaderboard_entry_id, None);
            assert_eq!(m.display_score, 0.0);
            assert_eq!(m.games_played, 0);
            assert!(m.disabled_at.is_none());
        }
    }
    for snake_id in p.snakes {
        leaderboard::get_or_create_entry(&pool, live, snake_id)
            .await
            .unwrap()
            .expect("active season entry");
    }

    crate::leaderboard_matchmaker::run_matchmaker(&state)
        .await
        .unwrap();
    assert!(league_games(&pool, ended).await.is_empty());
    assert!(league_games(&pool, future).await.is_empty());
    assert_eq!(
        league_games(&pool, live).await.len(),
        1,
        "control league plays"
    );

    // Web leave of a gap-joined snake works by battlesnake_id.
    let res = post_form(
        &state,
        &format!("/leaderboards/{ended}/leave"),
        p.session,
        format!("battlesnake_id={}", p.snakes[0]),
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(
        res.location.as_deref(),
        Some(&*format!("/leaderboards/{ended}"))
    );
    let left = leaderboard_registration::get_registration(&pool, ended, p.snakes[0])
        .await
        .unwrap()
        .unwrap();
    assert!(left.disabled_at.is_some());
    assert_eq!(left.disabled_reason, None);
    let other = leaderboard_registration::get_registration(&pool, ended, p.snakes[1])
        .await
        .unwrap()
        .unwrap();
    assert!(other.disabled_at.is_none(), "only the named snake pauses");

    // Leaving a league the snake never joined flashes instead of pausing.
    let never_joined = snake(&pool, p.user_id, "Never Joined").await;
    let res = post_form(
        &state,
        &format!("/leaderboards/{live}/leave"),
        p.session,
        format!("battlesnake_id={never_joined}"),
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(
        leaderboard_registration::get_registration(&pool, live, never_joined)
            .await
            .unwrap()
            .is_none(),
        "leaving an unjoined league must not create a registration"
    );
    let page = send(
        &state,
        "GET",
        &format!("/leaderboards/{live}"),
        p.session,
        None,
    )
    .await;
    assert!(
        page.body.contains("is not in this leaderboard"),
        "{}",
        page.body
    );
}

/// Seasons are half-open: active from `starts_at` (inclusive) until
/// `ends_at` (exclusive), and every game is stamped with its season.
#[sqlx::test(migrations = "../migrations")]
async fn season_boundaries_are_half_open_and_games_carry_their_season(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let p = player(&pool, 9_266_002).await;
    let lb = league(&pool, "Boundaries").await;
    end_open_seasons(&pool, lb).await;
    for snake_id in p.snakes {
        register(&pool, lb, snake_id).await;
    }

    // Start inclusive: a season starting exactly at a past clock reading.
    let starts_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let season_id: Uuid = sqlx::query_scalar(
        "INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at)
         VALUES ($1, 'Season 1', $2) RETURNING season_id",
    )
    .bind(lb)
    .bind(starts_at)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let locked = leaderboard_season::lock_active_season(&mut *tx, lb)
        .await
        .unwrap()
        .expect("season active from its start");
    assert_eq!(locked.season_id, season_id);
    assert_eq!(locked.starts_at, starts_at);
    tx.rollback().await.unwrap();

    crate::leaderboard_matchmaker::run_matchmaker(&state)
        .await
        .unwrap();
    let games = league_games(&pool, lb).await;
    assert_eq!(games.len(), 1);
    assert_eq!(games[0].1, Some(season_id), "persisted season");
    let found = leaderboard::find_leaderboard_game_by_game_id(&pool, games[0].0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.season_id, season_id);
    // The lazily created entries belong to the season too.
    let entry_seasons: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT season_id FROM leaderboard_entries WHERE leaderboard_id = $1")
            .bind(lb)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(entry_seasons, vec![Some(season_id); 2]);

    // End exclusive: at ends_at the season is over.
    sqlx::query("UPDATE leaderboard_seasons SET ends_at = clock_timestamp() WHERE season_id = $1")
        .bind(season_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        leaderboard_season::get_active_season(&pool, lb)
            .await
            .unwrap()
            .is_none()
    );
    let game_id: Uuid = sqlx::query_scalar(
        "INSERT INTO games (board_size, game_type) VALUES ('11x11', $1) RETURNING game_id",
    )
    .bind(game::GameType::Standard.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    let err = leaderboard::create_leaderboard_game(&pool, lb, game_id)
        .await
        .expect_err("no active season");
    assert!(
        format!("{err:#}").contains(&format!("no active season for leaderboard {lb}")),
        "{err:#}"
    );
    assert_eq!(league_games(&pool, lb).await.len(), 1, "nothing inserted");
}

/// A waiting game whose season ended is held (never dispatched or claimed);
/// a game already running when its season ends still gets rated.
#[sqlx::test(migrations = "../migrations")]
async fn waiting_game_is_held_and_running_game_is_rated_across_season_end(pool: PgPool) {
    let mut state = AppState::test_from_pool(pool.clone());
    // update_ratings is a rolled-back no-op with the empty test registry.
    state.scoring = std::sync::Arc::new(crate::scoring::default_registry());
    // Separate players per league: a shared snake would make the older held
    // waiter outrank the rated game for it, and the claim would be Busy.
    let held_player = player(&pool, 9_266_003).await;
    let rated_player = player(&pool, 9_266_013).await;
    let held_lb = league(&pool, "Held").await;
    let rated_lb = league(&pool, "Rated").await;
    for (lb, p) in [(held_lb, &held_player), (rated_lb, &rated_player)] {
        for snake_id in p.snakes {
            leaderboard::get_or_create_entry(&pool, lb, snake_id)
                .await
                .unwrap()
                .expect("entry");
        }
    }
    crate::leaderboard_matchmaker::run_matchmaker(&state)
        .await
        .unwrap();
    let held = league_games(&pool, held_lb).await[0].0;
    let rated = league_games(&pool, rated_lb).await[0].0;

    // The rated league's game starts before its season ends.
    assert!(matches!(
        game::claim_game_start(&pool, rated, 480).await.unwrap(),
        game::StartClaim::Started { .. }
    ));
    end_open_seasons(&pool, held_lb).await;
    end_open_seasons(&pool, rated_lb).await;

    crate::leaderboard_matchmaker::dispatch_pending_ladder_games(&state)
        .await
        .unwrap();
    assert_eq!(
        runner_jobs_for(&pool, held).await,
        0,
        "held game not dispatched"
    );
    assert_eq!(
        game::claim_game_start(&pool, held, 480).await.unwrap(),
        game::StartClaim::Busy
    );
    let status: String = sqlx::query_scalar("SELECT status FROM games WHERE game_id = $1")
        .bind(held)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "waiting");

    // Finish the running game and rate it after its season ended.
    sqlx::query("UPDATE games SET status = 'finished' WHERE game_id = $1")
        .bind(rated)
        .execute(&pool)
        .await
        .unwrap();
    let participants: Vec<Uuid> = sqlx::query_scalar(
        "SELECT game_battlesnake_id FROM game_battlesnakes WHERE game_id = $1 ORDER BY game_battlesnake_id",
    )
    .bind(rated)
    .fetch_all(&pool)
    .await
    .unwrap();
    for (i, gb) in participants.iter().enumerate() {
        sqlx::query("UPDATE game_battlesnakes SET placement = $2 WHERE game_battlesnake_id = $1")
            .bind(gb)
            .bind(i as i32 + 1)
            .execute(&pool)
            .await
            .unwrap();
    }
    let lb_game = leaderboard::find_leaderboard_game_by_game_id(&pool, rated)
        .await
        .unwrap()
        .unwrap();
    crate::leaderboard_ratings::update_ratings(&state, lb_game.leaderboard_game_id)
        .await
        .unwrap();
    let results: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM leaderboard_game_results WHERE leaderboard_game_id = $1",
    )
    .bind(lb_game.leaderboard_game_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(results, 2);
    let played: Vec<i32> = sqlx::query_scalar(
        "SELECT games_played FROM leaderboard_entries WHERE leaderboard_id = $1",
    )
    .bind(rated_lb)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(played, vec![1, 1]);
}

/// Leaving takes the snake out of matchmaking without touching its history;
/// rejoining the same season resumes the same entry and rating.
#[sqlx::test(migrations = "../migrations")]
async fn leave_and_rejoin_keep_the_season_entry(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let p = player(&pool, 9_266_004).await;
    let lb = league(&pool, "Leave Rejoin").await;
    let mut entries: Vec<LeaderboardEntry> = Vec::new();
    for snake_id in p.snakes {
        entries.push(
            leaderboard::get_or_create_entry(&pool, lb, snake_id)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let leaver = &entries[0];
    sqlx::query(
        "UPDATE leaderboard_entries SET mu = 31.5, display_score = 12.25, games_played = 7 WHERE leaderboard_entry_id = $1",
    )
    .bind(leaver.leaderboard_entry_id)
    .execute(&pool)
    .await
    .unwrap();

    let api_path = format!("/api/leaderboards/{lb}/entries/{}", p.snakes[0]);
    let res = send(&state, "DELETE", &api_path, p.session, None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    let active: Vec<Uuid> = leaderboard::get_active_entries(&pool, lb)
        .await
        .unwrap()
        .iter()
        .map(|e| e.battlesnake_id)
        .collect();
    assert_eq!(active, vec![p.snakes[1]]);
    crate::leaderboard_matchmaker::run_matchmaker(&state)
        .await
        .unwrap();
    assert!(
        league_games(&pool, lb).await.is_empty(),
        "one snake left: starved"
    );
    let kept = leaderboard::get_entry_by_id(&pool, leaver.leaderboard_entry_id)
        .await
        .unwrap()
        .expect("entry survives leave");
    assert_eq!(
        (kept.mu, kept.display_score, kept.games_played),
        (31.5, 12.25, 7)
    );
    assert!(
        leaderboard_registration::get_registration(&pool, lb, p.snakes[0])
            .await
            .unwrap()
            .unwrap()
            .disabled_at
            .is_some(),
        "the leave lands on the registration"
    );

    let res = post_json(
        &state,
        &format!("/api/leaderboards/{lb}/entries"),
        p.session,
        serde_json::json!({ "battlesnake_id": p.snakes[0] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let json: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        json["leaderboard_entry_id"],
        leaver.leaderboard_entry_id.to_string()
    );
    assert_eq!(json["display_score"], 12.25);
    assert_eq!(json["games_played"], 7);
    assert_eq!(json["active"], true);
    assert_eq!(
        leaderboard::get_active_entries(&pool, lb)
            .await
            .unwrap()
            .len(),
        2
    );

    // Leaving a league the snake was never in is a 404.
    let other = league(&pool, "Never Joined").await;
    let res = send(
        &state,
        "DELETE",
        &format!("/api/leaderboards/{other}/entries/{}", p.snakes[0]),
        p.session,
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(res.body.contains("Snake is not in this leaderboard"));
}

/// Leaving in the Preseason and rejoining in Season 1 starts a fresh entry
/// at default ratings; the Preseason entry keeps its history.
#[sqlx::test(migrations = "../migrations")]
async fn later_season_rejoin_starts_a_fresh_entry(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let p = player(&pool, 9_266_101).await;
    let lb = league(&pool, "Later Season Rejoin").await;
    let old = leaderboard::get_or_create_entry(&pool, lb, p.snakes[0])
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "UPDATE leaderboard_entries SET mu = 31.5, sigma = 6.5, display_score = 12.25, games_played = 7 WHERE leaderboard_entry_id = $1",
    )
    .bind(old.leaderboard_entry_id)
    .execute(&pool)
    .await
    .unwrap();
    let api_path = format!("/api/leaderboards/{lb}/entries/{}", p.snakes[0]);
    let res = send(&state, "DELETE", &api_path, p.session, None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    end_open_seasons(&pool, lb).await;
    let season_one = start_season(&pool, lb, "Season 1").await;

    let res = post_json(
        &state,
        &format!("/api/leaderboards/{lb}/entries"),
        p.session,
        serde_json::json!({ "battlesnake_id": p.snakes[0] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let json: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    let fresh_id: Uuid = json["leaderboard_entry_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(fresh_id, old.leaderboard_entry_id);
    assert_eq!(json["display_score"], 0.0);
    assert_eq!(json["games_played"], 0);
    assert_eq!(json["active"], true);
    let fresh = leaderboard::get_entry_by_id(&pool, fresh_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((fresh.mu, fresh.games_played), (25.0, 0));
    let fresh_season: Uuid = sqlx::query_scalar(
        "SELECT season_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
    )
    .bind(fresh_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fresh_season, season_one);
    assert_eq!(
        leaderboard::get_entry(&pool, lb, p.snakes[0])
            .await
            .unwrap()
            .unwrap()
            .leaderboard_entry_id,
        fresh_id,
        "get_entry resolves the active season's entry"
    );
    let kept = leaderboard::get_entry_by_id(&pool, old.leaderboard_entry_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (kept.mu, kept.sigma, kept.display_score, kept.games_played),
        (31.5, 6.5, 12.25, 7)
    );
    assert!(
        leaderboard_registration::get_registration(&pool, lb, p.snakes[0])
            .await
            .unwrap()
            .unwrap()
            .disabled_at
            .is_none(),
        "the rejoin re-enables the one registration"
    );

    // Rejoining again in the same season returns the same Season 1 entry.
    let res = post_json(
        &state,
        &format!("/api/leaderboards/{lb}/entries"),
        p.session,
        serde_json::json!({ "battlesnake_id": p.snakes[0] }),
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(json["leaderboard_entry_id"], fresh_id.to_string());
}

/// A disabled league refuses joins and isn't matched, even with an active
/// season.
#[sqlx::test(migrations = "../migrations")]
async fn disabled_league_refuses_joins_and_is_not_matched(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let p = player(&pool, 9_266_005).await;
    let lb = league(&pool, "Disabled").await;
    for snake_id in p.snakes {
        leaderboard::get_or_create_entry(&pool, lb, snake_id)
            .await
            .unwrap()
            .unwrap();
    }
    sqlx::query("UPDATE leaderboards SET disabled_at = NOW() WHERE leaderboard_id = $1")
        .bind(lb)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        leaderboard_season::get_active_season(&pool, lb)
            .await
            .unwrap()
            .is_some()
    );
    let newcomer = snake(&pool, p.user_id, "Newcomer").await;

    let res = post_json(
        &state,
        &format!("/api/leaderboards/{lb}/entries"),
        p.session,
        serde_json::json!({ "battlesnake_id": newcomer }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(res.body.contains("Leaderboard is not active"));
    let res = post_form(
        &state,
        &format!("/leaderboards/{lb}/join"),
        p.session,
        format!("battlesnake_id={newcomer}"),
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(
        leaderboard_registration::get_registration(&pool, lb, newcomer)
            .await
            .unwrap()
            .is_none()
    );

    crate::leaderboard_matchmaker::run_matchmaker(&state)
        .await
        .unwrap();
    assert!(league_games(&pool, lb).await.is_empty());
}

/// Private-league membership is league-level: a snake that joined during a
/// gap (no entry) still makes its owner a member.
#[sqlx::test(migrations = "../migrations")]
async fn private_membership_holds_during_a_gap(pool: PgPool) {
    let state = AppState::test_from_pool(pool.clone());
    let p = player(&pool, 9_266_006).await;
    let lb = private_leaderboard::create(
        &pool,
        &private_leaderboard::CreatePrivateLeaderboard {
            name: "Private Gap".to_string(),
            game_type: game::GameType::Standard.as_str().to_string(),
            board_size: "11x11".to_string(),
            match_size: 2,
        },
    )
    .await
    .unwrap();
    end_open_seasons(&pool, lb).await;
    let code: String =
        sqlx::query_scalar("SELECT invite_code FROM leaderboards WHERE leaderboard_id = $1")
            .bind(lb)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !private_leaderboard::is_member(&pool, lb, p.user_id)
            .await
            .unwrap()
    );

    let res = post_json(
        &state,
        &format!("/api/leaderboards/{lb}/entries"),
        p.session,
        serde_json::json!({ "battlesnake_id": p.snakes[0], "invite_code": code }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let member: bool = sqlx::query_scalar("SELECT leaderboard_member($1, $2)")
        .bind(lb)
        .bind(p.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(member);
    assert!(
        private_leaderboard::is_member(&pool, lb, p.user_id)
            .await
            .unwrap()
    );
    let viewer = crate::models::user::get_user_by_id(&pool, p.user_id)
        .await
        .unwrap()
        .unwrap();
    let listed = leaderboard::get_entries_for_battlesnake(&pool, p.snakes[0], Some(&viewer))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].leaderboard_id, lb);
    assert_eq!(listed[0].leaderboard_entry_id, None);
    assert_eq!(listed[0].display_score, None);
    assert!(listed[0].disabled_at.is_none());
    // Public surfaces still don't list the private league.
    assert!(
        leaderboard::get_entries_for_battlesnake(&pool, p.snakes[0], None)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A lazily created first entry starts at defaults without touching the
/// registration's health streak.
#[sqlx::test(migrations = "../migrations")]
async fn lazy_entry_leaves_registration_health_untouched(pool: PgPool) {
    let p = player(&pool, 9_266_007).await;
    let lb = league(&pool, "Streaky").await;
    end_open_seasons(&pool, lb).await;
    let registration_id = register(&pool, lb, p.snakes[0]).await;
    sqlx::query(
        "UPDATE leaderboard_registrations
         SET health_consecutive_failures = 2, health_last_failure = 'timeout'
         WHERE leaderboard_registration_id = $1",
    )
    .bind(registration_id)
    .execute(&pool)
    .await
    .unwrap();
    let season_id = start_season(&pool, lb, "Season 1").await;

    let scoring = crate::scoring::default_registry();
    leaderboard::ensure_active_season_entries(&pool, &scoring, lb)
        .await
        .unwrap();
    let entry = leaderboard::get_entry(&pool, lb, p.snakes[0])
        .await
        .unwrap()
        .expect("lazy first entry");
    let entry_season: Option<Uuid> = sqlx::query_scalar(
        "SELECT season_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
    )
    .bind(entry.leaderboard_entry_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(entry_season, Some(season_id));
    assert_eq!(entry.games_played, 0);
    let registration = leaderboard_registration::get_registration(&pool, lb, p.snakes[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(registration.health_consecutive_failures, 2);
    assert_eq!(registration.health_last_failure.as_deref(), Some("timeout"));
    // Every algorithm's row exists for the new entry.
    for table in ["weng_lin_ratings", "win_rate_stats", "food_eaten_stats"] {
        let rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE leaderboard_entry_id = $1"
        )))
        .bind(entry.leaderboard_entry_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1, "{table}");
    }
    // Idempotent: a second ensure creates nothing new.
    leaderboard::ensure_active_season_entries(&pool, &scoring, lb)
        .await
        .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM leaderboard_entries WHERE leaderboard_id = $1")
            .bind(lb)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

/// Un-retiring restores only the registrations retirement paused; manual and
/// health pauses stay, and no re-enable path revives a terminal registration.
#[sqlx::test(migrations = "../migrations")]
async fn unretire_restores_only_retired_registrations(pool: PgPool) {
    let p = player(&pool, 9_266_008).await;
    let snake_id = p.snakes[0];
    let active = league(&pool, "Unretire Active").await;
    let manual = league(&pool, "Unretire Manual").await;
    let health = league(&pool, "Unretire Health").await;
    for lb in [active, manual, health] {
        register(&pool, lb, snake_id).await;
    }
    leaderboard_registration::set_registration_disabled(
        &pool,
        manual,
        snake_id,
        Some(chrono::Utc::now()),
    )
    .await
    .unwrap();
    let health_reg = leaderboard_registration::get_registration(&pool, health, snake_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        leaderboard_registration::deactivate(
            &mut pool.acquire().await.unwrap(),
            health_reg.leaderboard_registration_id
        )
        .await
        .unwrap()
    );

    assert!(matches!(
        battlesnake::retire_battlesnake(&pool, snake_id, p.user_id)
            .await
            .unwrap(),
        battlesnake::RetireBattlesnakeOutcome::Retired { .. }
    ));
    let reason = |lb| {
        let pool = pool.clone();
        async move {
            let r = leaderboard_registration::get_registration(&pool, lb, snake_id)
                .await
                .unwrap()
                .unwrap();
            (r.disabled_at.is_some(), r.disabled_reason)
        }
    };
    assert_eq!(
        reason(active).await,
        (true, Some(battlesnake::DISABLED_REASON_RETIRED.to_string()))
    );
    // While retired, no re-enable path revives it.
    assert!(
        leaderboard_registration::get_or_create_registration(&pool, active, snake_id)
            .await
            .unwrap()
            .is_none()
    );
    leaderboard_registration::set_registration_disabled(&pool, active, snake_id, None)
        .await
        .unwrap();
    assert_eq!(
        leaderboard_registration::reactivate_snake(&pool, snake_id)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        reason(active).await,
        (true, Some(battlesnake::DISABLED_REASON_RETIRED.to_string()))
    );

    assert_eq!(
        battlesnake::unretire_battlesnake(&pool, snake_id, p.user_id)
            .await
            .unwrap(),
        battlesnake::UnretireBattlesnakeOutcome::Unretired
    );
    assert_eq!(reason(active).await, (false, None));
    assert_eq!(reason(manual).await, (true, None));
    assert_eq!(
        reason(health).await,
        (
            true,
            Some(leaderboard_registration::DISABLED_REASON_HEALTH.to_string())
        )
    );

    // Deletion is terminal for every re-enable path.
    let doomed = p.snakes[1];
    register(&pool, active, doomed).await;
    battlesnake::delete_battlesnake(&pool, doomed, p.user_id)
        .await
        .unwrap();
    assert!(
        leaderboard_registration::get_or_create_registration(&pool, active, doomed)
            .await
            .unwrap()
            .is_none()
    );
    let deleted = leaderboard_registration::get_registration(&pool, active, doomed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        deleted.disabled_reason.as_deref(),
        Some(battlesnake::DISABLED_REASON_DELETED)
    );
    assert!(
        !leaderboard_registration::reactivate(
            &mut pool.acquire().await.unwrap(),
            deleted.leaderboard_registration_id
        )
        .await
        .unwrap()
    );
}

/// The sync trigger's INSERT branch must never revert a concurrent pause: a
/// lazy ensure whose entry INSERT races an uncommitted pause leaves the pause
/// in place (DO NOTHING, not DO UPDATE).
#[sqlx::test(migrations = "../migrations")]
async fn lazy_ensure_never_reverts_a_concurrent_pause(pool: PgPool) {
    let p = player(&pool, 9_266_009).await;
    let lb = league(&pool, "Clobber").await;
    end_open_seasons(&pool, lb).await;
    let registration_id = register(&pool, lb, p.snakes[0]).await;
    start_season(&pool, lb, "Season 1").await;

    let mut pauser = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE leaderboard_registrations SET disabled_at = NOW() WHERE leaderboard_registration_id = $1",
    )
    .bind(registration_id)
    .execute(&mut *pauser)
    .await
    .unwrap();

    let ensure_pool = pool.clone();
    let ensure = tokio::spawn(async move {
        leaderboard::ensure_active_season_entries(
            &ensure_pool,
            &crate::scoring::ScoringRegistry::new(),
            lb,
        )
        .await
    });
    wait_for_lock_waiters(&pool, 1).await;
    pauser.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), ensure)
        .await
        .expect("ensure finishes once the pause commits")
        .unwrap()
        .unwrap();

    let registration = leaderboard_registration::get_registration(&pool, lb, p.snakes[0])
        .await
        .unwrap()
        .unwrap();
    assert!(
        registration.disabled_at.is_some(),
        "the pause survived the lazy entry insert"
    );
    assert!(
        leaderboard::get_active_entries(&pool, lb)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn wait_for_lock_waiters(pool: &PgPool, n: i64) {
    for _ in 0..3000 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {n} lock waiters");
}
