//! Every page that shows a person shows their public name (display name,
//! falling back to the GitHub login) while profile links stay keyed on the
//! login. Leaderboard surfaces are covered in `routes::leaderboard`.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::models::{flow::GameCreationFlow, session, tournament};
use crate::routes::test_support::{create_user_session, session_user_id, signed_session_cookie};
use crate::state::AppState;

const NAMED_LINK: &str = r#"<a href="/users/gh-display">Display Person</a>"#;

struct Fixture {
    state: AppState,
    owner_id: Uuid,
    snake_id: Uuid,
    viewer_session: Uuid,
    viewer_id: Uuid,
}

impl Fixture {
    /// An owner with a display name and one public snake, plus a signed-in
    /// viewer who also has a display name.
    async fn new(pool: PgPool) -> Self {
        let owner_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token, display_name)
             VALUES (9940001, 'gh-display', 'token', 'Display Person') RETURNING user_id"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let snake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url, visibility)
             VALUES ($1, 'Display Snake', 'http://snake', 'public') RETURNING battlesnake_id",
            owner_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let viewer_session = create_user_session(&pool, 9940002, false).await;
        let viewer_id = session_user_id(&pool, viewer_session).await;
        sqlx::query!(
            "UPDATE users SET display_name = 'Viewer Person' WHERE user_id = $1",
            viewer_id
        )
        .execute(&pool)
        .await
        .unwrap();
        Self {
            state: AppState::test_from_pool(pool),
            owner_id,
            snake_id,
            viewer_session,
            viewer_id,
        }
    }

    fn db(&self) -> &PgPool {
        &self.state.db
    }

    async fn get(&self, path: &str, session: Option<Uuid>) -> (StatusCode, String) {
        let app = crate::routes::routes(self.state.clone())
            .layer(tower_cookies::CookieManagerLayer::new());
        let mut builder = Request::builder().uri(path);
        if path.starts_with("/api/") {
            builder = builder.header(header::ORIGIN, "https://example.com");
        }
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
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// GET `path` and assert it shows the owner by public name, never by login.
    async fn assert_owner_named(&self, path: &str, session: Option<Uuid>) -> String {
        let (status, body) = self.get(path, session).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains("Display Person"), "{path}: {body}");
        assert!(!body.contains(">gh-display<"), "{path}: login as text");
        assert!(!body.contains("by gh-display"), "{path}: login as text");
        body
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn snake_pages_show_owner_public_name(pool: PgPool) {
    let fx = Fixture::new(pool).await;

    let body = fx.assert_owner_named("/snakes", None).await;
    assert!(body.contains(NAMED_LINK), "{body}");

    // The directory search matches the public name, not just the login.
    let (_, body) = fx.get("/snakes?q=display%20person", None).await;
    assert!(body.contains("Display Snake"), "{body}");

    let body = fx
        .assert_owner_named(&format!("/battlesnakes/{}/profile", fx.snake_id), None)
        .await;
    assert!(body.contains(NAMED_LINK), "{body}");

    // Game-builder public opponents list.
    let flow = GameCreationFlow::create_for_user(fx.db(), fx.viewer_id)
        .await
        .unwrap();
    let body = fx
        .assert_owner_named(
            &format!("/games/flow/{}", flow.flow_id),
            Some(fx.viewer_session),
        )
        .await;
    assert!(body.contains(NAMED_LINK), "{body}");
}

#[sqlx::test(migrations = "../migrations")]
async fn game_page_and_frames_show_owner_public_name(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let game_id = sqlx::query_scalar!(
        "INSERT INTO games (board_size, game_type, status)
         VALUES ('11x11', 'Standard', 'finished') RETURNING game_id"
    )
    .fetch_one(fx.db())
    .await
    .unwrap();
    let game_battlesnake_id = sqlx::query_scalar!(
        "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement)
         VALUES ($1, $2, 1) RETURNING game_battlesnake_id",
        game_id,
        fx.snake_id
    )
    .fetch_one(fx.db())
    .await
    .unwrap();
    // A frame persisted without an author gets one filled in at serve time.
    let frame = serde_json::json!({
        "Turn": 0,
        "Snakes": [{"ID": game_battlesnake_id.to_string(), "Author": ""}],
        "Food": [],
        "Hazards": [],
    });
    sqlx::query!(
        "INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, 0, $2)",
        game_id,
        frame
    )
    .execute(fx.db())
    .await
    .unwrap();

    let body = fx
        .assert_owner_named(&format!("/games/{game_id}"), None)
        .await;
    assert!(body.contains(NAMED_LINK), "{body}");

    let (status, body) = fx.get(&format!("/api/games/{game_id}/frames"), None).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["frames"][0]["Snakes"][0]["Author"], "Display Person");
}

#[sqlx::test(migrations = "../migrations")]
async fn tournament_pages_show_owner_public_name(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let tournament_id = sqlx::query_scalar!(
        "INSERT INTO tournaments (name, user_id, status)
         VALUES ('Name Cup', $1, 'registration') RETURNING tournament_id",
        fx.owner_id
    )
    .fetch_one(fx.db())
    .await
    .unwrap();
    tournament::create_registration(fx.db(), tournament_id, fx.snake_id, fx.owner_id, 1)
        .await
        .unwrap();

    let body = fx.assert_owner_named("/tournaments", None).await;
    assert!(body.contains("by Display Person"), "{body}");

    // Page byline (tournament owner) and the registration row (snake owner).
    let body = fx
        .assert_owner_named(&format!("/tournaments/{tournament_id}"), None)
        .await;
    assert_eq!(body.matches("by Display Person").count(), 2, "{body}");
}

#[sqlx::test(migrations = "../migrations")]
async fn signed_in_pages_greet_viewer_by_public_name(pool: PgPool) {
    let fx = Fixture::new(pool).await;

    let (status, body) = fx.get("/", Some(fx.viewer_session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Welcome, Viewer Person!"), "{body}");
    assert!(
        body.contains(r#"<span class="nav-user-name">Viewer Person</span>"#),
        "{body}"
    );

    let (status, body) = fx.get("/me", Some(fx.viewer_session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h2>Viewer Person</h2>"), "{body}");
    // The handle line keeps the login.
    assert!(body.contains("@test-user-9940002"), "{body}");

    // An empty display name falls back to the login everywhere.
    sqlx::query!(
        "UPDATE users SET display_name = '' WHERE user_id = $1",
        fx.viewer_id
    )
    .execute(fx.db())
    .await
    .unwrap();
    let (_, body) = fx.get("/", Some(fx.viewer_session)).await;
    assert!(body.contains("Welcome, test-user-9940002!"), "{body}");
    assert!(
        body.contains(r#"<span class="nav-user-name">test-user-9940002</span>"#),
        "{body}"
    );
}
