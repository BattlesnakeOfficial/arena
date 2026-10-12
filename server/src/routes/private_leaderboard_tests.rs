//! Private leaderboards end to end through the real router: admins create
//! them and rotate their invite codes, they stay off every listing and
//! public rollup, and joining one takes the invite code unless you're
//! already a member.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::models::{
    leaderboard::{self, MIN_GAMES_FOR_RANKING},
    private_leaderboard::{self, CreatePrivateLeaderboard},
    session,
};
use crate::routes::test_support::{create_user_session, session_user_id, signed_session_cookie};
use crate::state::AppState;

const NOINDEX: &str = r#"<meta name="robots" content="noindex">"#;

struct Fixture {
    state: AppState,
    admin_session: Uuid,
    member_session: Uuid,
    member_id: Uuid,
    member_snake: Uuid,
    outsider_session: Uuid,
    outsider_snake: Uuid,
}

impl Fixture {
    async fn new(pool: PgPool) -> Self {
        let admin_session = create_user_session(&pool, 9950001, true).await;
        let member_session = create_user_session(&pool, 9950002, false).await;
        let member_id = session_user_id(&pool, member_session).await;
        let outsider_session = create_user_session(&pool, 9950003, false).await;
        let outsider_id = session_user_id(&pool, outsider_session).await;
        let member_snake = snake(&pool, member_id, "Member Snake").await;
        let outsider_snake = snake(&pool, outsider_id, "Outsider Snake").await;
        Self {
            state: AppState::test_from_pool(pool),
            admin_session,
            member_session,
            member_id,
            member_snake,
            outsider_session,
            outsider_snake,
        }
    }

    fn db(&self) -> &PgPool {
        &self.state.db
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        session: Option<Uuid>,
        body: Option<(&str, String)>,
    ) -> (StatusCode, String) {
        let app = crate::routes::routes(self.state.clone())
            .expect("asset router")
            .layer(tower_cookies::CookieManagerLayer::new());
        let mut builder = Request::builder().method(method).uri(path);
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
        let request = match body {
            Some((content_type, body)) => builder
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body)),
            None => builder.body(Body::empty()),
        };
        let response = app.oneshot(request.unwrap()).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn get(&self, path: &str, session: Option<Uuid>) -> (StatusCode, String) {
        self.request("GET", path, session, None).await
    }

    async fn post_form(&self, path: &str, session: Uuid, form: &str) -> StatusCode {
        let body = Some(("application/x-www-form-urlencoded", form.to_string()));
        self.request("POST", path, Some(session), body).await.0
    }

    async fn post_json(&self, path: &str, session: Uuid, json: serde_json::Value) -> StatusCode {
        let body = Some(("application/json", json.to_string()));
        self.request("POST", path, Some(session), body).await.0
    }

    async fn create_private(&self, name: &str) -> Uuid {
        private_leaderboard::create(
            self.db(),
            &CreatePrivateLeaderboard {
                name: name.to_string(),
                game_type: "Standard".to_string(),
                board_size: "11x11".to_string(),
                match_size: 4,
            },
        )
        .await
        .unwrap()
    }

    async fn invite_code(&self, leaderboard_id: Uuid) -> String {
        sqlx::query_scalar!(
            r#"SELECT invite_code AS "invite_code!" FROM leaderboards WHERE leaderboard_id = $1"#,
            leaderboard_id
        )
        .fetch_one(self.db())
        .await
        .unwrap()
    }

    /// Whether the snake is on the leaderboard and in rotation.
    async fn active_entry(&self, leaderboard_id: Uuid, battlesnake_id: Uuid) -> bool {
        sqlx::query_scalar!(
            r#"SELECT EXISTS (
                SELECT 1 FROM leaderboard_entries
                WHERE leaderboard_id = $1 AND battlesnake_id = $2 AND disabled_at IS NULL
            ) AS "exists!""#,
            leaderboard_id,
            battlesnake_id
        )
        .fetch_one(self.db())
        .await
        .unwrap()
    }

    async fn join(&self, leaderboard_id: Uuid, session: Uuid, snake: Uuid, code: &str) -> bool {
        let form = format!("battlesnake_id={snake}&invite_code={code}");
        let status = self
            .post_form(
                &format!("/leaderboards/{leaderboard_id}/join"),
                session,
                &form,
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        self.active_entry(leaderboard_id, snake).await
    }

    async fn first_public_leaderboard(&self) -> Uuid {
        leaderboard::get_active_public_leaderboards(self.db())
            .await
            .unwrap()
            .first()
            .expect("migrations seed public leaderboards")
            .leaderboard_id
    }
}

async fn snake(pool: &PgPool, user_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar!(
        "INSERT INTO battlesnakes (user_id, name, url, visibility)
         VALUES ($1, $2, 'http://snake', 'public') RETURNING battlesnake_id",
        user_id,
        name
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn only_admins_create_private_leaderboards(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let form = "name=Friday+League&game_type=Royale&board_size=19x19&match_size=2";

    let (status, _) = fx.get("/admin/leaderboards", Some(fx.member_session)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let status = fx
        .post_form("/admin/leaderboards", fx.member_session, form)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let status = fx
        .post_form("/admin/leaderboards", fx.admin_session, form)
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let created = sqlx::query!(
        r#"SELECT leaderboard_id, game_type, board_size, match_size, visibility,
                  invite_code AS "invite_code!"
           FROM leaderboards WHERE name = 'Friday League'"#
    )
    .fetch_all(fx.db())
    .await
    .unwrap();
    assert_eq!(created.len(), 1, "only the admin's create landed");
    let lb = &created[0];
    assert_eq!(
        (
            lb.game_type.as_str(),
            lb.board_size.as_str(),
            lb.match_size,
            lb.visibility.as_str()
        ),
        ("Royale", "19x19", 2, "private")
    );
    assert_eq!(lb.invite_code.len(), 12);

    // The admin list shows the flash, the shareable link, and the code.
    let (status, body) = fx.get("/admin/leaderboards", Some(fx.admin_session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Created Friday League."), "{body}");
    assert!(
        body.contains(&format!("/leaderboards/{}", lb.leaderboard_id)),
        "{body}"
    );
    assert!(body.contains(&lb.invite_code), "{body}");
    // Public ladders are listed too, without a code.
    assert!(body.contains("Standard 11x11"), "{body}");
}

#[sqlx::test(migrations = "../migrations")]
async fn create_rejects_invalid_settings(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    for (form, message) in [
        (
            "name=+++&game_type=Standard&board_size=11x11&match_size=4",
            "Name is required",
        ),
        (
            "name=Bad&game_type=Solo&board_size=11x11&match_size=4",
            "Choose a valid game type",
        ),
        (
            "name=Bad&game_type=Standard&board_size=25x25&match_size=4",
            "Choose a valid board size",
        ),
        (
            "name=Bad&game_type=Standard&board_size=11x11&match_size=5",
            "Snakes per game must be between 2 and 4",
        ),
    ] {
        let status = fx
            .post_form("/admin/leaderboards", fx.admin_session, form)
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{form}");
        let (_, body) = fx.get("/admin/leaderboards", Some(fx.admin_session)).await;
        assert!(body.contains(message), "{form}: {body}");
    }
    let private_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM leaderboards WHERE visibility = 'private'"#
    )
    .fetch_one(fx.db())
    .await
    .unwrap();
    assert_eq!(private_count, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn regenerating_the_invite_code_retires_the_old_one(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let lb = fx.create_private("Rotating League").await;
    let old_code = fx.invite_code(lb).await;

    let path = format!("/admin/leaderboards/{lb}/invite-code");
    assert_eq!(
        fx.post_form(&path, fx.member_session, "").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(fx.invite_code(lb).await, old_code);

    assert_eq!(
        fx.post_form(&path, fx.admin_session, "").await,
        StatusCode::SEE_OTHER
    );
    let new_code = fx.invite_code(lb).await;
    assert_ne!(new_code, old_code);

    assert!(
        !fx.join(lb, fx.outsider_session, fx.outsider_snake, &old_code)
            .await
    );
    assert!(
        fx.join(lb, fx.outsider_session, fx.outsider_snake, &new_code)
            .await
    );

    // Public ladders have no code to rotate.
    let public = fx.first_public_leaderboard().await;
    let status = fx
        .post_form(
            &format!("/admin/leaderboards/{public}/invite-code"),
            fx.admin_session,
            "",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../migrations")]
async fn joining_takes_the_invite_code_unless_already_a_member(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let lb = fx.create_private("Invite League").await;
    let code = fx.invite_code(lb).await;
    let page = format!("/leaderboards/{lb}");

    // Non-members see the code field.
    let (status, body) = fx.get(&page, Some(fx.member_session)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"name="invite_code""#), "{body}");

    assert!(!fx.join(lb, fx.member_session, fx.member_snake, "").await);
    assert!(
        !fx.join(lb, fx.member_session, fx.member_snake, "WRONGCODE123")
            .await
    );
    let (_, body) = fx.get(&page, Some(fx.member_session)).await;
    assert!(body.contains("That invite code isn"), "{body}");

    // Codes are typed by hand: whitespace and case don't matter.
    let typed = format!("+{}+", code.to_lowercase());
    assert!(
        fx.join(lb, fx.member_session, fx.member_snake, &typed)
            .await
    );

    // Members don't see the field again...
    let (_, body) = fx.get(&page, Some(fx.member_session)).await;
    assert!(!body.contains(r#"name="invite_code""#), "{body}");

    // ...and pause/resume and further snakes need no code.
    let entry_id = leaderboard::get_entry(fx.db(), lb, fx.member_snake)
        .await
        .unwrap()
        .unwrap()
        .leaderboard_entry_id;
    let status = fx
        .post_form(
            &format!("/leaderboards/{lb}/leave"),
            fx.member_session,
            &format!("leaderboard_entry_id={entry_id}"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(!fx.active_entry(lb, fx.member_snake).await);
    assert!(fx.join(lb, fx.member_session, fx.member_snake, "").await);
    let second = snake(fx.db(), fx.member_id, "Second Snake").await;
    assert!(fx.join(lb, fx.member_session, second, "").await);

    // Another user's membership doesn't carry over.
    assert!(
        !fx.join(lb, fx.outsider_session, fx.outsider_snake, "")
            .await
    );

    // Public ladders still take no code.
    let public = fx.first_public_leaderboard().await;
    assert!(
        fx.join(public, fx.outsider_session, fx.outsider_snake, "")
            .await
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn api_join_takes_the_invite_code_too(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let lb = fx.create_private("API League").await;
    let path = format!("/api/leaderboards/{lb}/entries");

    let status = fx
        .post_json(
            &path,
            fx.member_session,
            serde_json::json!({ "battlesnake_id": fx.member_snake }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let status = fx
        .post_json(
            &path,
            fx.member_session,
            serde_json::json!({ "battlesnake_id": fx.member_snake, "invite_code": "NOPE" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!fx.active_entry(lb, fx.member_snake).await);

    let status = fx
        .post_json(
            &path,
            fx.member_session,
            serde_json::json!({
                "battlesnake_id": fx.member_snake,
                "invite_code": fx.invite_code(lb).await,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(fx.active_entry(lb, fx.member_snake).await);

    // Public ladders still join without one.
    let public = fx.first_public_leaderboard().await;
    let status = fx
        .post_json(
            &format!("/api/leaderboards/{public}/entries"),
            fx.member_session,
            serde_json::json!({ "battlesnake_id": fx.member_snake }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "../migrations")]
async fn private_leaderboards_are_unlisted(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let lb = fx.create_private("Hidden League").await;
    // Oldest active ladder, so the homepage would feature it if it could.
    sqlx::query!(
        "UPDATE leaderboards SET created_at = '2000-01-01' WHERE leaderboard_id = $1",
        lb
    )
    .execute(fx.db())
    .await
    .unwrap();
    // The member's snake is ranked there and nowhere else.
    let entry_id = leaderboard::get_or_create_entry(fx.db(), lb, fx.member_snake)
        .await
        .unwrap()
        .leaderboard_entry_id;
    sqlx::query!(
        "UPDATE leaderboard_entries SET games_played = $2, display_score = 40
         WHERE leaderboard_entry_id = $1",
        entry_id,
        MIN_GAMES_FOR_RANKING
    )
    .execute(fx.db())
    .await
    .unwrap();
    let lb_id = lb.to_string();
    let public = fx.first_public_leaderboard().await;
    let member_login = sqlx::query_scalar!(
        "SELECT github_login FROM users WHERE user_id = $1",
        fx.member_id
    )
    .fetch_one(fx.db())
    .await
    .unwrap();

    // Nothing that lists ladders, or a snake's/user's ladders to anyone not
    // on it, mentions it.
    for (path, session) in [
        ("/leaderboards".to_string(), None),
        (format!("/leaderboards/{public}"), None),
        ("/api/leaderboards".to_string(), None),
        ("/".to_string(), None),
        (format!("/battlesnakes/{}/profile", fx.member_snake), None),
        (
            format!("/battlesnakes/{}/profile", fx.member_snake),
            Some(fx.outsider_session),
        ),
        (format!("/users/{member_login}"), None),
        (format!("/users/{member_login}"), Some(fx.outsider_session)),
    ] {
        let (status, body) = fx.get(&path, session).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(!body.contains(&lb_id), "{path} leaks the private ladder");
        assert!(!body.contains("Hidden League"), "{path} leaks its name");
    }

    // Anyone on the ladder sees it on its players' profiles, badged: the
    // snake's owner, a fellow member, and admins.
    let fellow_session = create_user_session(fx.db(), 9950004, false).await;
    let fellow_id = session_user_id(fx.db(), fellow_session).await;
    let fellow_snake = snake(fx.db(), fellow_id, "Fellow Snake").await;
    leaderboard::get_or_create_entry(fx.db(), lb, fellow_snake)
        .await
        .unwrap();
    let profiles = [
        format!("/battlesnakes/{}/profile", fx.member_snake),
        format!("/users/{member_login}"),
    ];
    for session in [fx.member_session, fellow_session, fx.admin_session] {
        for path in &profiles {
            let (_, body) = fx.get(path, Some(session)).await;
            assert!(body.contains(&lb_id), "{path}: {body}");
            assert!(
                body.contains(r#"<span class="live-pill quiet">Private</span>"#),
                "{path}: {body}"
            );
        }
    }

    // Membership lasts only as long as a snake on the ladder does.
    sqlx::query!(
        "UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1",
        fellow_snake
    )
    .execute(fx.db())
    .await
    .unwrap();
    for path in &profiles {
        let (_, body) = fx.get(path, Some(fellow_session)).await;
        assert!(!body.contains(&lb_id), "{path} after leaving: {body}");
    }

    // The link itself works for anyone, kept out of search engines and
    // without the public mode nav.
    let (status, body) = fx.get(&format!("/leaderboards/{lb}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Hidden League"), "{body}");
    assert!(body.contains(NOINDEX), "{body}");
    assert!(!body.contains(r#"aria-label="Game modes""#), "{body}");
    let (status, body) = fx
        .get(&format!("/leaderboards/{lb}/entries/{entry_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(NOINDEX), "{body}");
    let (status, body) = fx
        .get(&format!("/api/leaderboards/{lb}/rankings"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Member Snake"), "{body}");
    // Public ladders stay indexable.
    let (_, body) = fx.get(&format!("/leaderboards/{public}"), None).await;
    assert!(!body.contains(NOINDEX));

    // The global rating ignores private ladders, including the board count.
    let scores = sqlx::query!(
        r#"SELECT user_id AS "user_id!", enabled_leaderboards AS "enabled!"
           FROM global_player_scores(0)"#
    )
    .fetch_all(fx.db())
    .await
    .unwrap();
    assert!(scores.iter().all(|s| s.user_id != fx.member_id));
    let public_count = leaderboard::get_active_public_leaderboards(fx.db())
        .await
        .unwrap()
        .len();
    let other = fx.first_public_leaderboard().await;
    leaderboard::get_or_create_entry(fx.db(), other, fx.outsider_snake)
        .await
        .unwrap();
    let enabled = sqlx::query_scalar!(
        r#"SELECT enabled_leaderboards AS "enabled!" FROM global_player_scores(0) LIMIT 1"#
    )
    .fetch_one(fx.db())
    .await
    .unwrap();
    assert_eq!(enabled, i64::try_from(public_count).unwrap());

    // Background work still treats it like any other ladder.
    let active = leaderboard::get_active_leaderboards(fx.db()).await.unwrap();
    assert!(active.iter().any(|l| l.leaderboard_id == lb));
}

#[sqlx::test(migrations = "../migrations")]
async fn tournaments_cannot_import_from_private_leaderboards(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let lb = fx.create_private("Closed League").await;
    let entry_id = leaderboard::get_or_create_entry(fx.db(), lb, fx.member_snake)
        .await
        .unwrap()
        .leaderboard_entry_id;
    sqlx::query!(
        "UPDATE leaderboard_entries SET games_played = $2 WHERE leaderboard_entry_id = $1",
        entry_id,
        MIN_GAMES_FOR_RANKING
    )
    .execute(fx.db())
    .await
    .unwrap();
    let outsider_id = session_user_id(fx.db(), fx.outsider_session).await;
    let tournament_id = sqlx::query_scalar!(
        "INSERT INTO tournaments (name, user_id, status)
         VALUES ('Import Cup', $1, 'registration') RETURNING tournament_id",
        outsider_id
    )
    .fetch_one(fx.db())
    .await
    .unwrap();

    // The owner's import dropdown lists public ladders only.
    let (status, body) = fx
        .get(
            &format!("/tournaments/{tournament_id}"),
            Some(fx.outsider_session),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Standard 11x11"), "{body}");
    assert!(!body.contains(&lb.to_string()), "{body}");

    // A forged import of the private ladder imports nothing.
    let status = fx
        .post_form(
            &format!("/tournaments/{tournament_id}/import-leaderboard"),
            fx.outsider_session,
            &format!("leaderboard_id={lb}&count=8"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let registrations = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM tournament_registrations WHERE tournament_id = $1"#,
        tournament_id
    )
    .fetch_one(fx.db())
    .await
    .unwrap();
    assert_eq!(registrations, 0);
}
