use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::{
    models::session,
    notifications::{self, EmailRecipient, NewEmail, NewNotification, NotificationKind},
    routes::test_support::{create_user_session, session_user_id, signed_session_cookie},
    state::AppState,
};

struct Fixture {
    state: AppState,
    session: Uuid,
    user_id: Uuid,
    other_id: Uuid,
}

impl Fixture {
    async fn new(pool: PgPool) -> Self {
        let session = create_user_session(&pool, 1676001, false).await;
        let other_session = create_user_session(&pool, 1676002, false).await;
        let user_id = session_user_id(&pool, session).await;
        let other_id = session_user_id(&pool, other_session).await;
        Self {
            state: AppState::test_from_pool(pool),
            session,
            user_id,
            other_id,
        }
    }

    async fn request(&self, method: &str, path: &str, signed: bool, body: &str) -> Response {
        let mut builder = Request::builder().method(method).uri(path);
        if signed {
            builder = builder.header(
                header::COOKIE,
                format!(
                    "{}={}",
                    session::SESSION_COOKIE_NAME,
                    signed_session_cookie(&self.state, self.session)
                ),
            );
        }
        if method == "POST" {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        crate::routes::routes(self.state.clone())
            .layer(tower_cookies::CookieManagerLayer::new())
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn get_html(&self, path: &str) -> String {
        let response = self.request("GET", path, true, "").await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    async fn seed(
        &self,
        owner: Uuid,
        title: &str,
        kind: &str,
        visible: bool,
        link: Option<&str>,
        created_at: DateTime<Utc>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO notifications (user_id, kind, title, body, link, in_app_visible, created_at) VALUES ($1, $2, $3, 'A test body', $4, $5, $6) RETURNING notification_id"
        )
        .bind(owner).bind(kind).bind(title).bind(link).bind(visible).bind(created_at)
        .fetch_one(&self.state.db).await.unwrap()
    }

    async fn badge(&self) -> i64 {
        let response = self.request("GET", "/notifications/count", true, "").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["count"]
            .as_i64()
            .unwrap()
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

#[sqlx::test(migrations = "../migrations")]
async fn empty_and_item_accessible_names(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    for path in ["/notifications", "/notifications/drawer"] {
        let html = fx.get_html(path).await;
        assert!(html.contains("No notifications yet."));
        assert!(!html.contains("/notifications/read-all"));
        assert!(!html.contains("class=\"notification-items\""));
    }
    let unread = fx
        .seed(
            fx.user_id,
            "Unread title",
            "snake_matchmaking_paused",
            true,
            None,
            now(),
        )
        .await;
    let read = fx
        .seed(
            fx.user_id,
            "Read title",
            "snake_matchmaking_paused",
            true,
            None,
            now(),
        )
        .await;
    sqlx::query("UPDATE notifications SET read_at = now() WHERE notification_id = $1")
        .bind(read)
        .execute(&fx.state.db)
        .await
        .unwrap();
    let html = fx.get_html("/notifications/drawer").await;
    let unread_button = html
        .split(&format!("/notifications/{unread}/open"))
        .nth(1)
        .unwrap()
        .split("</button>")
        .next()
        .unwrap();
    let read_button = html
        .split(&format!("/notifications/{read}/open"))
        .nth(1)
        .unwrap()
        .split("</button>")
        .next()
        .unwrap();
    assert!(!unread_button.contains("aria-label="));
    assert!(!read_button.contains("aria-label="));
    assert!(unread_button.contains("Open notification: "));
    assert!(unread_button.contains("Unread. "));
    assert!(!read_button.contains("Unread. "));
    assert!(unread_button.contains("aria-hidden=\"true\">New"));
}

#[sqlx::test(migrations = "../migrations")]
async fn opening_one_decrements_badge(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let id = fx
        .seed(
            fx.user_id,
            "One",
            "snake_matchmaking_paused",
            true,
            Some("/me"),
            now(),
        )
        .await;
    assert_eq!(fx.badge().await, 1);
    assert_eq!(
        fx.request("POST", &format!("/notifications/{id}/open"), true, "")
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(fx.badge().await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn bell_count_and_guest_access(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let guest = fx.request("GET", "/", false, "").await;
    let body = to_bytes(guest.into_body(), usize::MAX).await.unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("id=\"notification-bell\""));
    for path in [
        "/notifications",
        "/notifications/drawer",
        "/notifications/count",
        "/notifications/more",
    ] {
        assert_eq!(
            fx.request("GET", path, false, "").await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(fx.badge().await, 0);
    let page = fx.get_html("/notifications").await;
    assert_eq!(page.matches("id=\"notification-bell\"").count(), 1);
    assert!(page.contains("0 unread notifications"));
    assert!(page.contains("class=\"notification-badge\" hidden"));
    for count in 1..=10 {
        fx.seed(
            fx.user_id,
            &format!("Notice {count}"),
            "snake_matchmaking_paused",
            true,
            Some("/me"),
            now(),
        )
        .await;
        assert_eq!(fx.badge().await, count.min(10));
        if count == 1 || count == 9 {
            let page = fx.get_html("/notifications").await;
            let label = if count == 1 {
                "1 unread notification".to_string()
            } else {
                format!("{count} unread notifications")
            };
            assert!(page.contains(&format!("aria-label=\"{label}\"")));
            assert!(page.contains(&format!(">{count}</span>")));
        }
    }
    let real_id: Uuid =
        sqlx::query_scalar("SELECT notification_id FROM notifications WHERE user_id = $1 LIMIT 1")
            .bind(fx.user_id)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    for path in [
        format!("/notifications/{real_id}/open"),
        "/notifications/read-all".to_string(),
    ] {
        assert_eq!(
            fx.request("POST", &path, false, "").await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let unchanged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND read_at IS NOT NULL",
    )
    .bind(fx.user_id)
    .fetch_one(&fx.state.db)
    .await
    .unwrap();
    assert_eq!(unchanged, 0);
    let page = fx.get_html("/notifications").await;
    assert!(page.contains("9 or more unread notifications"));
    assert!(page.contains(">9+</span>"));
}

#[sqlx::test(migrations = "../migrations")]
async fn pagination_and_no_js_actions(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let base = now();
    let mut ids = Vec::new();
    for index in 0..22 {
        let created_at = base - chrono::Duration::seconds(i64::from(index / 2));
        ids.push(
            fx.seed(
                fx.user_id,
                &format!("Notice {index:02}"),
                "snake_matchmaking_paused",
                true,
                Some("/me"),
                created_at,
            )
            .await,
        );
    }
    let hidden = fx
        .seed(
            fx.user_id,
            "Snapshot hidden",
            "snake_matchmaking_paused",
            false,
            None,
            base,
        )
        .await;
    sqlx::query("INSERT INTO notification_preferences (user_id, kind, channel, enabled) VALUES ($1, 'snake_matchmaking_resumed', 'in_app', false)")
        .bind(fx.user_id).execute(&fx.state.db).await.unwrap();
    let disabled = fx
        .seed(
            fx.user_id,
            "Preference hidden",
            "snake_matchmaking_resumed",
            true,
            None,
            base,
        )
        .await;
    let foreign = fx
        .seed(
            fx.other_id,
            "Foreign",
            "snake_matchmaking_paused",
            true,
            None,
            base,
        )
        .await;
    let page = fx.get_html("/notifications").await;
    assert_eq!(
        page.matches("class=\"notification-item unread\"").count(),
        20
    );
    assert!(!page.contains("Preference hidden"));
    assert!(!page.contains(&format!("/notifications/{disabled}/open")));
    let mut expected: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (index / 2, *id))
        .collect();
    expected.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let positions: Vec<_> = expected[..20]
        .iter()
        .map(|(_, id)| page.find(&format!("/notifications/{id}/open")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(page.contains("method=\"post\""));
    assert!(page.contains("/notifications/read-all"));
    let href = page
        .split("class=\"notification-next\" href=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&amp;", "&");
    assert!(href.contains("before_id="));
    let drawer = fx.get_html("/notifications/drawer").await;
    assert!(!drawer.contains("Preference hidden"));
    assert!(drawer.contains("data-before-id="));
    let before = drawer
        .split("data-before=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let before_id = drawer
        .split("data-before-id=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("before", before)
        .append_pair("before_id", before_id)
        .finish();
    let more = fx.get_html(&format!("/notifications/more?{query}")).await;
    assert_eq!(
        more.matches("class=\"notification-item unread\"").count(),
        2
    );
    assert!(!more.contains("Preference hidden"));
    fx.seed(
        fx.user_id,
        "Newer",
        "snake_matchmaking_paused",
        true,
        None,
        base + chrono::Duration::seconds(1),
    )
    .await;
    let next = fx.get_html(&href).await;
    assert_eq!(
        next.matches("class=\"notification-item unread\"").count(),
        2
    );
    assert!(!next.contains("Preference hidden"));
    assert!(!next.contains("Newer"));
    for (_, id) in &expected[..20] {
        assert!(!next.contains(&format!("/notifications/{id}/open")));
    }
    for (_, id) in &expected[20..] {
        assert!(next.contains(&format!("/notifications/{id}/open")));
    }
    let oldest = expected.last().unwrap().1;
    let oldest_at: DateTime<Utc> =
        sqlx::query_scalar("SELECT created_at FROM notifications WHERE notification_id = $1")
            .bind(oldest)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    let exhausted_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "before",
            &oldest_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        )
        .append_pair("before_id", &oldest.to_string())
        .finish();
    let exhausted_page = fx
        .get_html(&format!("/notifications?{exhausted_query}"))
        .await;
    assert!(exhausted_page.contains("No older notifications."));
    assert!(exhausted_page.contains("href=\"/notifications\""));
    let exhausted_more = fx
        .get_html(&format!("/notifications/more?{exhausted_query}"))
        .await;
    assert!(exhausted_more.contains("class=\"notification-batch\""));
    assert!(!exhausted_more.contains("<li class=\"notification-item"));
    assert!(!exhausted_more.contains("notification-load-more"));
    for path in [
        "/notifications?before=2026-01-01T00%3A00%3A00Z",
        "/notifications/more?before_id=bad",
        "/notifications/more?before=x&before_id=y",
    ] {
        assert_eq!(
            fx.request("GET", path, true, "").await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let unread: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND read_at IS NULL",
    )
    .bind(fx.user_id)
    .fetch_one(&fx.state.db)
    .await
    .unwrap();
    assert_eq!(unread, 25);
    let before_open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND in_app_visible AND read_at IS NULL AND kind = 'snake_matchmaking_paused'")
        .bind(fx.user_id).fetch_one(&fx.state.db).await.unwrap();
    let response = fx
        .request("POST", &format!("/notifications/{}/open", ids[0]), true, "")
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/me");
    let first_read: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(ids[0])
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert!(first_read.is_some());
    let after_open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND in_app_visible AND read_at IS NULL AND kind = 'snake_matchmaking_paused'")
        .bind(fx.user_id).fetch_one(&fx.state.db).await.unwrap();
    assert_eq!(after_open, before_open - 1);
    fx.request("POST", &format!("/notifications/{}/open", ids[0]), true, "")
        .await;
    let second_read: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(ids[0])
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert_eq!(first_read, second_read);
    for id in [foreign, hidden, disabled] {
        assert_eq!(
            fx.request("POST", &format!("/notifications/{id}/open"), true, "")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    for id in [hidden, disabled] {
        let read_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
                .bind(id)
                .fetch_one(&fx.state.db)
                .await
                .unwrap();
        assert!(read_at.is_none());
    }
    assert_eq!(
        fx.request("POST", "/notifications/not-a-uuid/open", true, "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = fx
        .request("POST", "/notifications/read-all", true, "")
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/notifications");
    assert_eq!(fx.badge().await, 0);
    let foreign_read: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(foreign)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert!(foreign_read.is_none());
    let hidden_read: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(hidden)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert!(hidden_read.is_some());
    let disabled_read: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(disabled)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert!(disabled_read.is_some());
    fx.request(
        "POST",
        "/settings/notifications",
        true,
        "kind=snake_matchmaking_resumed&channel=in_app&enabled=true",
    )
    .await;
    assert_eq!(fx.badge().await, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn safe_targets_and_email_only_recorder(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let no_link = fx
        .seed(
            fx.user_id,
            "No link",
            "snake_matchmaking_resumed",
            true,
            None,
            now(),
        )
        .await;
    let unsafe_link = fx
        .seed(
            fx.user_id,
            "Unsafe legacy link",
            "snake_matchmaking_resumed",
            true,
            Some("//outside.example"),
            now(),
        )
        .await;
    for id in [no_link, unsafe_link] {
        let response = fx
            .request("POST", &format!("/notifications/{id}/open"), true, "")
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/notifications");
    }
    let receipt = notifications::record(
        &fx.state.db,
        NewNotification {
            user_id: fx.user_id,
            kind: NotificationKind::ClaimVerification,
            title: "Email only".into(),
            body: "Delivered by email".into(),
            link: Some("/me".into()),
            email: Some(NewEmail {
                subject: None,
                text: None,
                html_body: None,
                recipient: EmailRecipient::Override("fake@example.test".into()),
                claim_imported_account_id: Some(Uuid::new_v4()),
                notification_email_request_id: None,
            }),
        },
    )
    .await
    .unwrap();
    let visible: bool =
        sqlx::query_scalar("SELECT in_app_visible FROM notifications WHERE notification_id = $1")
            .bind(receipt.notification_id)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert!(!visible);
    assert!(!fx.get_html("/notifications").await.contains("Email only"));
    assert!(
        !fx.get_html("/notifications/drawer")
            .await
            .contains("Email only")
    );
    assert_eq!(fx.badge().await, 0);
    assert_eq!(
        fx.request(
            "POST",
            &format!("/notifications/{}/open", receipt.notification_id),
            true,
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn current_preference_filters_every_surface(pool: PgPool) {
    let fx = Fixture::new(pool).await;
    let id = fx
        .seed(
            fx.user_id,
            "Toggleable notice",
            "snake_matchmaking_paused",
            true,
            Some("/me"),
            now(),
        )
        .await;
    fx.seed(
        fx.user_id,
        "Still unread",
        "snake_matchmaking_paused",
        true,
        None,
        now(),
    )
    .await;
    assert_eq!(fx.badge().await, 2);
    assert_eq!(
        fx.request("POST", &format!("/notifications/{id}/open"), true, "")
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let first_read: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(id)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert!(first_read.is_some());
    assert_eq!(fx.badge().await, 1);
    let response = fx
        .request(
            "POST",
            "/settings/notifications",
            true,
            "kind=snake_matchmaking_paused&channel=in_app&enabled=false",
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(fx.badge().await, 0);
    assert!(
        !fx.get_html("/notifications")
            .await
            .contains("Toggleable notice")
    );
    assert!(
        !fx.get_html("/notifications/drawer")
            .await
            .contains("Toggleable notice")
    );
    assert!(!fx.get_html("/notifications").await.contains("Still unread"));
    assert_eq!(
        fx.request("POST", &format!("/notifications/{id}/open"), true, "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let while_disabled: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(id)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert_eq!(while_disabled, first_read);
    let snapshot_hidden = notifications::record(
        &fx.state.db,
        NewNotification {
            user_id: fx.user_id,
            kind: NotificationKind::SnakeMatchmakingPaused,
            title: "Recorded while off".into(),
            body: "Off snapshot".into(),
            link: None,
            email: Some(NewEmail {
                subject: Some("Paused".into()),
                text: Some("Paused".into()),
                html_body: Some("<p>Paused</p>".into()),
                recipient: EmailRecipient::User,
                claim_imported_account_id: None,
                notification_email_request_id: None,
            }),
        },
    )
    .await
    .unwrap()
    .notification_id;
    fx.request(
        "POST",
        "/settings/notifications",
        true,
        "kind=snake_matchmaking_paused&channel=in_app&enabled=true",
    )
    .await;
    assert_eq!(fx.badge().await, 1);
    let page = fx.get_html("/notifications").await;
    assert!(page.contains("Toggleable notice"));
    assert!(!page.contains("Recorded while off"));
    let after_enable: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE notification_id = $1")
            .bind(id)
            .fetch_one(&fx.state.db)
            .await
            .unwrap();
    assert_eq!(after_enable, first_read);
    assert_eq!(
        fx.request(
            "POST",
            &format!("/notifications/{snapshot_hidden}/open"),
            true,
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    fx.request(
        "POST",
        "/settings/notifications",
        true,
        "kind=snake_matchmaking_paused&channel=email&enabled=false",
    )
    .await;
    assert_eq!(fx.badge().await, 1);
}
