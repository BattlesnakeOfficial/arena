//! OAuth provider route tests: the full authorize → approve → code → token
//! → API flow, plus every negative path from the task's success criteria.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use sqlx::PgPool;
use tower::ServiceExt as _;
use uuid::Uuid;

use crate::models::api_token::Permission;
use crate::models::oauth::{self, OauthClient};
use crate::routes::oauth::{consent_csrf_key, consent_csrf_message, mint_consent_csrf};
use crate::routes::test_support::{
    api_app, create_user_session, send_api, session_user_id, signed_session_cookie,
};
use crate::state::AppState;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

const REDIRECT_URI: &str = "https://client.example/cb";

async fn make_client(db: &PgPool, name: &str, uris: &[&str]) -> oauth::NewOauthClient {
    oauth::create_client(
        db,
        oauth::CreateOauthClientInput {
            name: name.to_string(),
            logo_url: Some("https://client.example/logo.png".to_string()),
            homepage_url: Some("https://client.example".to_string()),
            kind: oauth::OauthClientKind::Standard,
            redirect_uris: uris.iter().map(|u| u.to_string()).collect(),
        },
    )
    .await
    .unwrap()
}

/// A PKCE pair: verifier (43+ chars) and its S256 challenge.
fn make_verifier_pair() -> (String, String) {
    let verifier = "a".repeat(64);
    let challenge = oauth::pkce_s256(&verifier);
    (verifier, challenge)
}

fn authorize_uri(
    client: &OauthClient,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    challenge: &str,
) -> String {
    format!(
        "/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        client.public_client_id,
        urlencoding::encode(redirect_uri),
        urlencoding::encode(scope),
        urlencoding::encode(state),
        challenge,
    )
}

async fn make_snake(db: &PgPool, user: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        r#"INSERT INTO battlesnakes (user_id, name, url)
           VALUES ($1, $2, 'https://snake.example') RETURNING battlesnake_id"#,
    )
    .bind(user)
    .bind(name)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

struct Fixture {
    state: AppState,
    app: axum::Router,
    cookie: String,
    session_id: Uuid,
    user_id: Uuid,
    client: OauthClient,
    secret: String,
    snakes: [Uuid; 3],
}

impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        let state = AppState::test_from_pool(pool.clone());
        let app = api_app(&state);
        let session_id = create_user_session(pool, 9922001, false).await;
        let user_id = session_user_id(pool, session_id).await;
        let new_client = make_client(pool, "Test Client", &[REDIRECT_URI]).await;
        let snakes = [
            make_snake(pool, user_id, "oauth-a").await,
            make_snake(pool, user_id, "oauth-b").await,
            make_snake(pool, user_id, "oauth-c").await,
        ];
        Self {
            cookie: signed_session_cookie(&state, session_id),
            state,
            app,
            session_id,
            user_id,
            client: new_client.client,
            secret: new_client.secret,
            snakes,
        }
    }

    fn cookie_header(&self) -> (&'static str, String) {
        (
            "cookie",
            format!(
                "{}={}",
                crate::models::session::SESSION_COOKIE_NAME,
                self.cookie
            ),
        )
    }

    fn consent_csrf(&self, challenge: &str, scope: &str) -> String {
        self.consent_csrf_for(REDIRECT_URI, challenge, scope, "xyz")
    }

    fn consent_csrf_for(
        &self,
        redirect_uri: &str,
        challenge: &str,
        scope: &str,
        state: &str,
    ) -> String {
        mint_consent_csrf(
            &consent_csrf_key(self.state.cookie_key.master()),
            &consent_csrf_message(
                self.session_id,
                &self.client.public_client_id,
                redirect_uri,
                challenge,
                scope,
                Some(state),
            ),
        )
    }

    /// GET the authorize screen: (status, Location, body, headers).
    async fn get_authorize(
        &self,
        uri: &str,
    ) -> (StatusCode, Option<String>, String, axum::http::HeaderMap) {
        let (name, value) = self.cookie_header();
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get(uri)
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let location = headers
            .get(header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .map(str::to_string);
        let body = body_string(response).await;
        (status, location, body, headers)
    }

    /// POST the consent decision: (status, Location, body).
    async fn post_decision(&self, fields: &[(&str, &str)]) -> (StatusCode, Option<String>, String) {
        let body = form_encode(fields);
        let (name, value) = self.cookie_header();
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post("/oauth/authorize/decision")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(name, value)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let location = headers
            .get(header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .map(str::to_string);
        let body = body_string(response).await;
        (status, location, body)
    }

    /// Approve with a valid CSRF and the given scope/snakes; returns the raw
    /// code from the redirect.
    async fn approve(
        &self,
        scope: &str,
        challenge: &str,
        snake_access: &str,
        snake_ids: &[Uuid],
        check_email: bool,
    ) -> String {
        let mut fields: Vec<(&str, String)> = vec![
            ("client_id", self.client.public_client_id.clone()),
            ("redirect_uri", REDIRECT_URI.to_string()),
            ("state", "xyz".to_string()),
            ("code_challenge", challenge.to_string()),
            ("scope", scope.to_string()),
            ("csrf", self.consent_csrf(challenge, scope)),
            ("decision", "approve".to_string()),
            ("snake_access", snake_access.to_string()),
        ];
        if check_email {
            fields.push(("email_read", "on".to_string()));
        }
        let mut body = fields
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        for id in snake_ids {
            body.push_str(&format!(
                "&snake_ids={}",
                urlencoding::encode(&id.to_string())
            ));
        }

        let (name, value) = self.cookie_header();
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post("/oauth/authorize/decision")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(name, value)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let headers = response.headers().clone();
        let location = headers
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with(REDIRECT_URI), "{location}");
        let code = query_param(&location, "code").expect("code in redirect");
        assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));
        code
    }

    /// POST the token endpoint: (status, JSON, headers).
    async fn post_token(
        &self,
        fields: &[(&str, &str)],
        basic: Option<(&str, &str)>,
    ) -> (StatusCode, serde_json::Value, axum::http::HeaderMap) {
        let body = form_encode(fields);
        let mut builder = Request::post("/oauth/token")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some((id, secret)) = basic {
            let credentials = BASE64.encode(format!("{id}:{secret}"));
            builder = builder.header(header::AUTHORIZATION, format!("Basic {credentials}"));
        }
        let response = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = body_string(response).await;
        let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body));
        (status, json, headers)
    }

    /// authorize → approve → exchange; returns the access token secret.
    async fn full_flow(&self, scope: &str, snake_access: &str, snake_ids: &[Uuid]) -> String {
        let (verifier, challenge) = make_verifier_pair();
        let code = self
            .approve(scope, &challenge, snake_access, snake_ids, false)
            .await;
        self.exchange(&code, &verifier).await
    }

    /// Exchange a code with the fixture client's credentials.
    async fn exchange(&self, code: &str, verifier: &str) -> String {
        let (status, json, _headers) = self
            .post_token(
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", REDIRECT_URI),
                    ("code_verifier", verifier),
                ],
                Some((&self.client.public_client_id, &self.secret)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json["access_token"].as_str().unwrap().to_string()
    }

    async fn active_grant(&self, pool: &PgPool) -> Uuid {
        sqlx::query_scalar!(
            "SELECT id FROM oauth_grants WHERE user_id = $1 AND oauth_client_id = $2 AND revoked_at IS NULL",
            self.user_id,
            self.client.id
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// POST the authorized-apps snake edit; returns the response status.
    async fn post_grant_snake_edit(
        &self,
        grant_id: Uuid,
        snake_access: &str,
        snake_ids: &[Uuid],
    ) -> StatusCode {
        let mut body = form_encode(&[("snake_access", snake_access)]);
        for id in snake_ids {
            body.push_str(&format!(
                "&snake_ids={}",
                urlencoding::encode(&id.to_string())
            ));
        }
        let (name, value) = self.cookie_header();
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(format!("/settings/authorized-apps/{grant_id}/snakes"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(name, value)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        response.status()
    }
}

fn form_encode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn full_flow_authorize_to_api(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let scope = "profile.read email.read snakes.read snake.url.read snake.url.write";
    let uri = authorize_uri(&fx.client, REDIRECT_URI, scope, "xyz", &challenge);

    // 1. The approval screen.
    let (status, location, body, headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(location.is_none());
    // Rendered through the full site layout (viewport meta, styles, header)
    // — a bare fragment would lay out at ~980px on phones.
    assert!(
        body.contains("viewport"),
        "consent screen must use the site layout: {body}"
    );
    assert!(body.contains("Test Client"), "{body}");
    for text in [
        Permission::ProfileRead.description(),
        Permission::EmailRead.description(),
        Permission::SnakesRead.description(),
        Permission::SnakeUrlRead.description(),
        Permission::SnakeUrlWrite.description(),
    ] {
        assert!(
            body.contains(text),
            "missing permission line {text:?}: {body}"
        );
    }
    // Exactly one permission checkbox (email.read), default checked; every
    // other permission line is static text.
    assert_eq!(body.matches("name=\"email_read\"").count(), 1, "{body}");
    assert!(
        body.contains("checked"),
        "email.read defaults checked: {body}"
    );
    assert!(
        body.contains("take control of where your snake runs"),
        "{body}"
    );
    assert!(
        body.contains("It does not limit seeing your snakes or starting games"),
        "{body}"
    );
    assert!(body.contains("min-height: 44px"), "{body}");
    assert_eq!(body.matches("name=\"snake_ids\"").count(), 3, "{body}");
    assert!(body.contains("name=\"csrf\""), "{body}");
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );

    // 2. Approve WITHOUT the email_read key (box unchecked), selected [A, B]
    //    — two snake_ids values prove the RawForm parse.
    let code = fx
        .approve(
            scope,
            &challenge,
            "selected",
            &[fx.snakes[0], fx.snakes[1]],
            false,
        )
        .await;

    // 3. Exchange.
    let (status, json, headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["token_type"], "Bearer");
    assert_eq!(
        json["scope"],
        "profile.read snakes.read snake.url.read snake.url.write"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::PRAGMA], "no-cache");
    let access_token = json["access_token"].as_str().unwrap().to_string();

    // 4. /api/me: 200, email absent (no email.read).
    let (status, body) = send_api(&fx.app, "GET", "/api/me", Some(&access_token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let me: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        me.get("email")
            .map(serde_json::Value::is_null)
            .unwrap_or(true),
        "email must be absent without email.read: {me}"
    );

    // 5. /api/snakes: A and B have url, C (unselected) doesn't.
    let (status, body) = send_api(&fx.app, "GET", "/api/snakes", Some(&access_token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let snakes: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let by_name = |name: &str| {
        snakes
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {snakes:?}"))
            .clone()
    };
    let a = by_name("oauth-a");
    let b = by_name("oauth-b");
    let c = by_name("oauth-c");
    assert!(a.get("url").is_some(), "{a}");
    assert!(b.get("url").is_some(), "{b}");
    assert!(
        c.get("url").is_none(),
        "unselected snake url must be redacted: {c}"
    );

    // 6. A forbidden endpoint: 403 (well-formed body so the JSON extractor
    //    passes and the permission check is what fires).
    let (status, body) = send_api(
        &fx.app,
        "POST",
        "/api/games",
        Some(&access_token),
        Some(serde_json::json!({"snakes": []})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, "missing permission: games.create");

    // 7. DB invariants: one active grant; its token bound; one live token;
    //    code permissions = the granted set (no email.read).
    let grant = fx.active_grant(&pool).await;
    let token = sqlx::query!(
        "SELECT id, oauth_grant_id FROM api_tokens WHERE user_id = $1 AND revoked_at IS NULL",
        fx.user_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(token.oauth_grant_id, Some(grant));
    let live: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM api_tokens WHERE oauth_grant_id = $1 AND revoked_at IS NULL"#,
        grant
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 1);
    let code_permissions: Vec<String> = sqlx::query_scalar!(
        r#"SELECT p.permission FROM oauth_authorization_code_permissions p
           JOIN oauth_authorization_codes c ON c.id = p.oauth_authorization_code_id
           ORDER BY p.permission"#
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!code_permissions.contains(&"email.read".to_string()));
    assert!(code_permissions.contains(&"profile.read".to_string()));
}

// ---------------------------------------------------------------------------
// Consent-decision behavior
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn decision_ignores_non_email_permission_subset(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let scope = "profile.read snakes.read";
    let csrf = fx.consent_csrf(&challenge, scope);

    // A stray permissions= key carrying a subset — ignored outright: the
    // code's permissions are the full requested scope.
    let (status, location, _body) = fx
        .post_decision(&[
            ("client_id", &fx.client.public_client_id),
            ("redirect_uri", REDIRECT_URI),
            ("state", "xyz"),
            ("code_challenge", &challenge),
            ("scope", scope),
            ("csrf", &csrf),
            ("decision", "approve"),
            ("snake_access", "all"),
            ("permissions", "profile.read"),
        ])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let code = query_param(&location.unwrap(), "code").unwrap();

    let count: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64"
           FROM oauth_authorization_code_permissions p
           JOIN oauth_authorization_codes c ON c.id = p.oauth_authorization_code_id
           WHERE p.permission = 'snakes.read'"#
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "snakes.read must survive the stray subset key");

    // And after exchange the token's scope is the full requested set.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["scope"], "profile.read snakes.read");
}

#[sqlx::test(migrations = "../migrations")]
async fn decision_rejects_missing_or_forged_csrf(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    let scope = "profile.read";

    let valid_csrf = fx.consent_csrf(&challenge, scope);
    let minted_over_other_challenge = {
        let other = "b".repeat(64);
        fx.consent_csrf(&oauth::pkce_s256(&other), scope)
    };
    let minted_over_other_scope = fx.consent_csrf(&challenge, "profile.read snakes.read");

    let other_session_csrf = mint_consent_csrf(
        &consent_csrf_key(fx.state.cookie_key.master()),
        &consent_csrf_message(
            Uuid::new_v4(),
            &fx.client.public_client_id,
            REDIRECT_URI,
            &challenge,
            scope,
            Some("xyz"),
        ),
    );

    let base = |csrf: &str| {
        vec![
            ("client_id".to_string(), fx.client.public_client_id.clone()),
            ("redirect_uri".to_string(), REDIRECT_URI.to_string()),
            ("state".to_string(), "xyz".to_string()),
            ("code_challenge".to_string(), challenge.clone()),
            ("scope".to_string(), scope.to_string()),
            ("csrf".to_string(), csrf.to_string()),
            ("decision".to_string(), "approve".to_string()),
            ("snake_access".to_string(), "all".to_string()),
        ]
    };

    // (a) csrf absent; (b) different session; (c) different challenge;
    // (d) hidden scope tampered (csrf minted over a wider scope).
    let mut cases: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    let mut no_csrf = base(&valid_csrf);
    no_csrf.retain(|(k, _)| k != "csrf");
    cases.push(("no csrf", no_csrf));
    cases.push(("other session", base(&other_session_csrf)));
    cases.push(("other challenge", base(&minted_over_other_challenge)));
    cases.push(("tampered scope", base(&minted_over_other_scope)));

    for (label, fields) in cases {
        let refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let body = form_encode(&refs);
        let (name, value) = fx.cookie_header();
        let response = fx
            .app
            .clone()
            .oneshot(
                Request::post("/oauth/authorize/decision")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(name, value)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let has_location = headers.contains_key(header::LOCATION);
        let body = body_string(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        assert!(!has_location, "{label} must never redirect");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY", "{label}");
    }

    // No code row was created by any variant.
    let codes: i64 =
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_authorization_codes"#)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(codes, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn only_email_read_may_be_withheld_and_empty_scope_is_valid(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();

    // A request of only email.read, unchecked: a valid zero-permission token.
    let code = fx
        .approve("email.read", &challenge, "all", &[], false)
        .await;
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["scope"], "");

    let token = json["access_token"].as_str().unwrap();
    // The token authenticates (a valid zero-permission token exists) but
    // holds nothing: /api/me names the missing permission.
    let (status, body) = send_api(&fx.app, "GET", "/api/me", Some(token), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, "missing permission: profile.read");
}

#[sqlx::test(migrations = "../migrations")]
async fn email_read_checked_grants_email(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    sqlx::query!(
        "UPDATE users SET github_email = 'dev1766@example.com' WHERE user_id = $1",
        fx.user_id
    )
    .execute(&pool)
    .await
    .unwrap();
    let (verifier, challenge) = make_verifier_pair();

    // email.read left CHECKED: it is granted, and /api/me returns the email.
    let code = fx
        .approve("profile.read email.read", &challenge, "all", &[], true)
        .await;
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["scope"], "profile.read email.read");
    let token = json["access_token"].as_str().unwrap();
    let (status, body) = send_api(&fx.app, "GET", "/api/me", Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let me: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(me["email"], "dev1766@example.com", "{me}");
}

#[sqlx::test(migrations = "../migrations")]
async fn decision_rejects_foreign_or_deleted_snakes(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    let scope = "snakes.read snake.url.read";

    // Another user's snake.
    let other_session = create_user_session(&pool, 9922003, false).await;
    let other_user = session_user_id(&pool, other_session).await;
    let foreign_snake = make_snake(&pool, other_user, "not-yours").await;

    // One of the user's own snakes, soft-deleted.
    sqlx::query!(
        "UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1",
        fx.snakes[2]
    )
    .execute(&pool)
    .await
    .unwrap();

    for (label, snake_id) in [
        ("foreign snake", foreign_snake),
        ("own soft-deleted snake", fx.snakes[2]),
    ] {
        let csrf = fx.consent_csrf(&challenge, scope);
        let body = form_encode(&[
            ("client_id", &fx.client.public_client_id),
            ("redirect_uri", REDIRECT_URI),
            ("state", "xyz"),
            ("code_challenge", &challenge),
            ("scope", scope),
            ("csrf", &csrf),
            ("decision", "approve"),
            ("snake_access", "selected"),
        ]) + "&snake_ids="
            + &urlencoding::encode(&snake_id.to_string());
        let (name, value) = fx.cookie_header();
        let response = fx
            .app
            .clone()
            .oneshot(
                Request::post("/oauth/authorize/decision")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(name, value)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let has_location = response.headers().contains_key(header::LOCATION);
        let body = body_string(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        assert!(!has_location, "{label} must never redirect");

        let codes: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_authorization_codes"#
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(codes, 0, "{label}: no code row");
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn decision_deny_redirects_without_code(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    let scope = "profile.read";
    let csrf = fx.consent_csrf(&challenge, scope);

    let (status, location, _body) = fx
        .post_decision(&[
            ("client_id", &fx.client.public_client_id),
            ("redirect_uri", REDIRECT_URI),
            ("state", "xyz"),
            ("code_challenge", &challenge),
            ("scope", scope),
            ("csrf", &csrf),
            ("decision", "deny"),
            ("snake_access", "all"),
        ])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(
        query_param(&location, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));

    let codes: i64 =
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_authorization_codes"#)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(codes, 0, "deny must not create a code");
}

#[sqlx::test(migrations = "../migrations")]
async fn decision_requires_explicit_approve(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    let scope = "profile.read";

    for (label, decision) in [
        ("missing decision", None),
        ("unknown decision", Some("maybe")),
    ] {
        let csrf = fx.consent_csrf(&challenge, scope);
        let mut fields: Vec<(&str, &str)> = vec![
            ("client_id", &fx.client.public_client_id),
            ("redirect_uri", REDIRECT_URI),
            ("state", "xyz"),
            ("code_challenge", &challenge),
            ("scope", scope),
            ("csrf", &csrf),
            ("snake_access", "all"),
        ];
        if let Some(decision) = decision {
            fields.push(("decision", decision));
        }
        let (status, location, _body) = fx.post_decision(&fields).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
        assert!(
            location.is_none(),
            "{label} must never redirect: {location:?}"
        );
    }

    let codes: i64 =
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_authorization_codes"#)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(codes, 0, "no decision value may approve implicitly");
}

// ---------------------------------------------------------------------------
// Negative tests: authorize
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn unknown_client_no_redirect(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    let uri = authorize_uri(&fx.client, REDIRECT_URI, "profile.read", "xyz", &challenge).replacen(
        &fx.client.public_client_id,
        "deadbeefdeadbeefdeadbeefdeadbeef",
        1,
    );
    let (status, location, body, headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(location.is_none(), "{location:?}");
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    // The error page doesn't leak whether the client exists.
    assert!(!body.contains("Test Client"), "{body}");

    // A missing client_id is the same error page.
    let (status, location, _body, _headers) = fx
        .get_authorize("/oauth/authorize?redirect_uri=https%3A%2F%2Fclient.example%2Fcb&response_type=code&scope=profile.read&code_challenge=whatever&code_challenge_method=S256")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(location.is_none());
}

#[sqlx::test(migrations = "../migrations")]
async fn unregistered_redirect_uri_no_redirect(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    for bad_uri in [
        "https://evil.example/cb",      // different host
        "https://client.example/cb/",   // trailing slash
        "https://client.example/other", // different path
    ] {
        let uri = authorize_uri(&fx.client, bad_uri, "profile.read", "xyz", &challenge);
        let (status, location, body, headers) = fx.get_authorize(&uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad_uri}: {body}");
        assert!(
            location.is_none(),
            "{bad_uri} must never redirect: {location:?}"
        );
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    }

    // Missing redirect_uri entirely: same error page.
    let uri = format!(
        "/oauth/authorize?client_id={}&response_type=code&scope=profile.read&code_challenge={challenge}&code_challenge_method=S256",
        fx.client.public_client_id
    );
    let (status, location, _body, _headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(location.is_none());
}

#[sqlx::test(migrations = "../migrations")]
async fn missing_challenge_and_plain_method_redirect(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();

    // code_challenge absent → invalid_request, state echoed.
    let uri = format!(
        "/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&scope=profile.read&state=st1&code_challenge_method=S256",
        fx.client.public_client_id,
        urlencoding::encode(REDIRECT_URI),
    );
    let (status, location, _body, _headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(
        query_param(&location, "error").as_deref(),
        Some("invalid_request")
    );
    assert_eq!(query_param(&location, "state").as_deref(), Some("st1"));

    // code_challenge_method=plain → invalid_request too.
    let uri = format!(
        "/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&scope=profile.read&state=st2&code_challenge={challenge}&code_challenge_method=plain",
        fx.client.public_client_id,
        urlencoding::encode(REDIRECT_URI),
    );
    let (status, location, _body, _headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = location.unwrap();
    assert_eq!(
        query_param(&location, "error").as_deref(),
        Some("invalid_request")
    );
    assert_eq!(query_param(&location, "state").as_deref(), Some("st2"));
}

#[sqlx::test(migrations = "../migrations")]
async fn banned_scope_is_invalid_scope(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (_verifier, challenge) = make_verifier_pair();
    for scope in ["tokens.manage profile.read", "admin", "profile.read nope"] {
        let uri = authorize_uri(&fx.client, REDIRECT_URI, scope, "s", &challenge);
        let (status, location, _body, _headers) = fx.get_authorize(&uri).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{scope}");
        assert_eq!(
            query_param(&location.unwrap(), "error").as_deref(),
            Some("invalid_scope"),
            "{scope}"
        );
    }
}

// ---------------------------------------------------------------------------
// Negative tests: token exchange
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn wrong_verifier_leaves_code_reusable(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    let (status, json, headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &"c".repeat(64)), // wrong verifier
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    // Verifier failure precedes the consume: the same code still works with
    // the right verifier.
    let token = fx.exchange(&code, &verifier).await;
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../migrations")]
async fn replayed_code_revokes_issued_token(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;
    let token = fx.exchange(&code, &verifier).await;

    // Token works…
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    // …then the same code is replayed.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");

    // The replay revoked the issued token AND its grant.
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let revoked: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar!(
        "SELECT revoked_at FROM oauth_grants WHERE user_id = $1",
        fx.user_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(revoked.is_some(), "no zombie grant after replay");
}

#[sqlx::test(migrations = "../migrations")]
async fn expired_code(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    let code_hash = {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(code.as_bytes());
        hex::encode(digest)
    };
    sqlx::query!(
        "UPDATE oauth_authorization_codes SET expires_at = NOW() - INTERVAL '1 second' WHERE code_hash = $1",
        code_hash
    )
    .execute(&pool)
    .await
    .unwrap();

    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");
}

#[sqlx::test(migrations = "../migrations")]
async fn client_b_cannot_redeem_client_a_code(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let other = make_client(&pool, "Other Client", &["https://other.example/cb"]).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    // B redeems A's unconsumed code: plain invalid_grant (not invalid_client),
    // no side effects.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&other.client.public_client_id, &other.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");

    // A's code is still unconsumed and redeemable.
    let token = fx.exchange(&code, &verifier).await;
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    // Consumed variant: B redeems A's CONSUMED code → invalid_grant with NO
    // revocation of A's token (client check precedes replay).
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&other.client.public_client_id, &other.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "client B must not trigger replay revocation"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn wrong_redirect_uri_at_exchange(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    // Register a second URI on the fixture client.
    let alt_uri = "https://client.example/cb2";
    sqlx::query!(
        "INSERT INTO oauth_client_redirect_uris (oauth_client_id, uri) VALUES ($1, $2)",
        fx.client.id,
        alt_uri
    )
    .execute(&pool)
    .await
    .unwrap();

    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    // The code was issued for REDIRECT_URI; presenting the (also
    // registered) alt URI is invalid_grant — and the code is NOT consumed,
    // so the right URI still works.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", alt_uri),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");

    let token = fx.exchange(&code, &verifier).await;
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../migrations")]
async fn disabled_client_and_revival(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let token = fx.full_flow("profile.read", "all", &[]).await;

    // One successful call stamps last_used_at.
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    let used_before: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar!(
        "SELECT last_used_at FROM api_tokens WHERE user_id = $1 AND revoked_at IS NULL",
        fx.user_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(used_before.is_some());

    // A code approved while the client is live (for the exchange below).
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    oauth::set_client_disabled(&pool, fx.client.id, true)
        .await
        .unwrap();

    // (a) authorize shows the error page.
    let uri = authorize_uri(&fx.client, REDIRECT_URI, "profile.read", "xyz", &challenge);
    let (status, location, _body, _headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(location.is_none());

    // The decision handler re-validates too: a disabled client gets the
    // no-redirect error page, never a code.
    let csrf = fx.consent_csrf(&challenge, "profile.read");
    let fields = [
        ("client_id", fx.client.public_client_id.clone()),
        ("redirect_uri", REDIRECT_URI.to_string()),
        ("state", "xyz".to_string()),
        ("code_challenge", challenge.clone()),
        ("scope", "profile.read".to_string()),
        ("csrf", csrf),
        ("decision", "approve".to_string()),
        ("snake_access", "all".to_string()),
    ];
    let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, location, _body) = fx.post_decision(&refs).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(location.is_none());

    // (b) exchange of the pre-disable code is invalid_client.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["error"], "invalid_client");

    // (c) the previously issued token is rejected with no last_used_at bump.
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let used_after: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar!(
        "SELECT last_used_at FROM api_tokens WHERE user_id = $1 AND revoked_at IS NULL",
        fx.user_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        used_before, used_after,
        "rejected token must not stamp last_used_at"
    );

    // Revival: enabling the client revives the same token; revoked_at stays
    // NULL throughout (disable never revokes rows).
    oauth::set_client_disabled(&pool, fx.client.id, false)
        .await
        .unwrap();
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    let revoked: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar!(
        "SELECT revoked_at FROM api_tokens WHERE user_id = $1",
        fx.user_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(revoked.is_none());
}

#[sqlx::test(migrations = "../migrations")]
async fn revoked_grant_token_dies(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let token = fx.full_flow("profile.read", "all", &[]).await;
    let grant = fx.active_grant(&pool).await;

    let (name, value) = fx.cookie_header();
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/settings/authorized-apps/{grant}/revoke"))
                .header(name, value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let revoked: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar!("SELECT revoked_at FROM oauth_grants WHERE id = $1", grant)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked.is_some());
}

#[sqlx::test(migrations = "../migrations")]
async fn revoke_grant_invalidates_outstanding_codes(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    // A live grant via an earlier exchange (the Authorized apps revoke only
    // reaches an existing grant).
    let _token = fx.full_flow("profile.read", "all", &[]).await;
    let grant = fx.active_grant(&pool).await;

    // A second approval, not yet exchanged — the code a client could be
    // holding when the player revokes.
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    let (name, value) = fx.cookie_header();
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/settings/authorized-apps/{grant}/revoke"))
                .header(name, value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // The outstanding code cannot resurrect the grant.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");

    let active: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_grants WHERE user_id = $1 AND oauth_client_id = $2 AND revoked_at IS NULL"#,
        fx.user_id,
        fx.client.id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 0, "no active grant may survive the revoke");
}

#[sqlx::test(migrations = "../migrations")]
async fn earlier_code_cannot_supersede_newer_exchange(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    // Two approvals: the broader one first, the narrower one second.
    let earlier = fx
        .approve("profile.read snakes.read", &challenge, "all", &[], false)
        .await;
    let later = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    // The NEWER code is exchanged: the grant rotation invalidates the older
    // outstanding code.
    let token = fx.exchange(&later, &verifier).await;

    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &earlier),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");

    // The newer exchange's grant and token are still live.
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    let active: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_grants WHERE user_id = $1 AND oauth_client_id = $2 AND revoked_at IS NULL"#,
        fx.user_id,
        fx.client.id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn grant_snake_edit_invalidates_outstanding_codes(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    // A live grant narrowed to [A], via an earlier exchange.
    let _token = fx
        .full_flow("snakes.read snake.url.read", "selected", &[fx.snakes[0]])
        .await;
    let grant = fx.active_grant(&pool).await;

    // A pre-edit approval selecting [A, B] — the code that could undo the
    // narrowing below.
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve(
            "snakes.read snake.url.read",
            &challenge,
            "selected",
            &[fx.snakes[0], fx.snakes[1]],
            false,
        )
        .await;

    // Re-narrow the live grant to [A].
    let status = fx
        .post_grant_snake_edit(grant, "selected", &[fx.snakes[0]])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // The pre-edit code is dead.
    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_grant");

    // The live grant's selection is still exactly [A].
    let rows: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT battlesnake_id FROM api_token_snakes s\n         JOIN api_tokens t ON t.id = s.api_token_id\n         WHERE t.oauth_grant_id = $1 AND t.revoked_at IS NULL",
        grant
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![fx.snakes[0]]);
}

#[sqlx::test(migrations = "../migrations")]
async fn edit_grant_can_remove_and_widen_selection(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let token = fx
        .full_flow(
            "snakes.read snake.url.read",
            "selected",
            &[fx.snakes[0], fx.snakes[1]],
        )
        .await;
    let grant = fx.active_grant(&pool).await;

    let url_of = |token: &str, name: &str| {
        let app = fx.app.clone();
        let token = token.to_string();
        let name = name.to_string();
        async move {
            let (status, body) = send_api(&app, "GET", "/api/snakes", Some(&token), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let snakes: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
            snakes
                .iter()
                .find(|s| s["name"] == name)
                .unwrap()
                .get("url")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        }
    };

    // Before: both visible.
    assert!(url_of(&token, "oauth-a").await.is_string());
    assert!(url_of(&token, "oauth-b").await.is_string());

    // REMOVING: selected {A, B} → {A} — B's url is redacted on the same
    // token.
    let status = fx
        .post_grant_snake_edit(grant, "selected", &[fx.snakes[0]])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(url_of(&token, "oauth-a").await.is_string());
    assert!(
        url_of(&token, "oauth-b").await.is_null(),
        "removal must take effect on the SAME token"
    );

    // WIDENING: selected → all.
    let status = fx.post_grant_snake_edit(grant, "all", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(url_of(&token, "oauth-a").await.is_string());
    assert!(url_of(&token, "oauth-b").await.is_string());
}

#[sqlx::test(migrations = "../migrations")]
async fn grant_snake_edit_rejects_foreign_snake(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let _token = fx
        .full_flow("snakes.read snake.url.read", "selected", &[fx.snakes[0]])
        .await;
    let grant = fx.active_grant(&pool).await;

    let other_session = create_user_session(&pool, 9922004, false).await;
    let other_user = session_user_id(&pool, other_session).await;
    let foreign_snake = make_snake(&pool, other_user, "still-not-yours").await;

    // The edit tries to add another user's snake: rejected (flash error +
    // redirect), and the selection is unchanged.
    let status = fx
        .post_grant_snake_edit(grant, "selected", &[fx.snakes[0], foreign_snake])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let rows: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT battlesnake_id FROM api_token_snakes s\n         JOIN api_tokens t ON t.id = s.api_token_id\n         WHERE t.oauth_grant_id = $1 AND t.revoked_at IS NULL",
        grant
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![fx.snakes[0]], "selection must be unchanged");
}

#[sqlx::test(migrations = "../migrations")]
async fn grant_snake_edit_rejects_foreign_grant(pool: PgPool) {
    let fx = Fixture::new(&pool).await;

    // Another user's grant, created through the model layer directly.
    let other_session = create_user_session(&pool, 9922005, false).await;
    let other_user = session_user_id(&pool, other_session).await;
    let other_snake = make_snake(&pool, other_user, "their-snake").await;
    let (verifier, challenge) = make_verifier_pair();
    let code = oauth::create_authorization_code(
        &pool,
        oauth::NewAuthorizationCode {
            client_db_id: fx.client.id,
            user_id: other_user,
            redirect_uri: REDIRECT_URI,
            code_challenge: &challenge,
            permissions: &[Permission::SnakesRead, Permission::SnakeUrlRead],
            snake_access: crate::models::api_token::SnakeAccess::Selected,
            snake_ids: &[other_snake],
        },
    )
    .await
    .unwrap();
    oauth::exchange_authorization_code(&pool, fx.client.id, &code, REDIRECT_URI, &verifier)
        .await
        .unwrap()
        .unwrap();
    let other_grant: Uuid = sqlx::query_scalar!(
        "SELECT id FROM oauth_grants WHERE user_id = $1 AND revoked_at IS NULL",
        other_user
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // The fixture user POSTs an edit against the other user's grant
    // (IDOR): rejected, and the other user's grant is untouched.
    let status = fx.post_grant_snake_edit(other_grant, "all", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let rows: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT battlesnake_id FROM api_token_snakes s\n         JOIN api_tokens t ON t.id = s.api_token_id\n         WHERE t.oauth_grant_id = $1 AND t.revoked_at IS NULL",
        other_grant
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![other_snake],
        "the other user's grant must be unchanged"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn oauth_delete_only_own_snakes(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    // OAuth token with snake.delete (all access, so every snake passes the
    // access check and the creator guard is what decides).
    let token = fx.full_flow("snakes.create snake.delete", "all", &[]).await;

    // DELETE on the player-made C is 403: the client didn't create it.
    let (status, body) = send_api(
        &fx.app,
        "DELETE",
        &format!("/api/snakes/{}", fx.snakes[2]),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, "battlesnake was not created by this app");

    // A snake created THROUGH the OAuth token deletes fine, and carries the
    // creator stamp.
    let (status, body) = send_api(
        &fx.app,
        "POST",
        "/api/snakes",
        Some(&token),
        Some(serde_json::json!({
            "name": "app-made",
            "url": "https://app-made.example",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created: Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stamp: Option<Uuid> = sqlx::query_scalar!(
        "SELECT created_by_oauth_client_id FROM battlesnakes WHERE battlesnake_id = $1",
        created
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stamp, Some(fx.client.id));

    let (status, _body) = send_api(
        &fx.app,
        "DELETE",
        &format!("/api/snakes/{created}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A missing snake is a 404, not 403.
    let (status, _body) = send_api(
        &fx.app,
        "DELETE",
        &format!("/api/snakes/{}", Uuid::new_v4()),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Contrast: a personal token with snake.delete deletes the player-made C.
    let personal = crate::models::api_token::create_api_token(
        &pool,
        fx.user_id,
        "personal",
        &[Permission::SnakeDelete],
        crate::models::api_token::SnakeAccess::All,
        &[],
    )
    .await
    .unwrap()
    .secret;
    let (status, _body) = send_api(
        &fx.app,
        "DELETE",
        &format!("/api/snakes/{}", fx.snakes[2]),
        Some(&personal),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Grant lifecycle
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn exchange_is_atomic_code_not_consumed_on_failure(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    // Make the token mint fail mid-transaction.
    sqlx::query!(
        "CREATE FUNCTION fail_token_insert() RETURNS trigger
         LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''injected failure''; END'"
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "CREATE TRIGGER fail_token_insert BEFORE INSERT ON api_token_permissions
         FOR EACH ROW EXECUTE FUNCTION fail_token_insert()"
    )
    .execute(&pool)
    .await
    .unwrap();

    let (status, json, _headers) = fx
        .post_token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json}");

    // The whole transaction rolled back: the code is still unconsumed.
    let consumed: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar!("SELECT consumed_at FROM oauth_authorization_codes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(consumed.is_none(), "consume must roll back with the mint");

    // Drop the fault; the same code exchanges successfully.
    sqlx::query!("DROP TRIGGER fail_token_insert ON api_token_permissions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query!("DROP FUNCTION fail_token_insert")
        .execute(&pool)
        .await
        .unwrap();
    let token = fx.exchange(&code, &verifier).await;
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../migrations")]
async fn exchange_after_selected_snake_deleted(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve(
            "snakes.read snake.url.read",
            &challenge,
            "selected",
            &[fx.snakes[0], fx.snakes[1]],
            false,
        )
        .await;

    // Snake B is soft-deleted between approve and exchange: the exchange
    // filters soft-deleted snakes out when copying the code's selection
    // into the token (snakes are soft-deleted, so the FK cascade never
    // fires).
    sqlx::query!(
        "UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1",
        fx.snakes[1]
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = fx.exchange(&code, &verifier).await;
    let (status, body) = send_api(&fx.app, "GET", "/api/snakes", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let snakes: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let a = snakes.iter().find(|s| s["name"] == "oauth-a").unwrap();
    assert!(a.get("url").is_some(), "{a}");
    let c = snakes.iter().find(|s| s["name"] == "oauth-c").unwrap();
    assert!(c.get("url").is_none(), "{c}");

    // The token's snake selection is EXACTLY {A} — the soft-deleted B was
    // dropped at exchange, not carried into api_token_snakes.
    let rows: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT battlesnake_id FROM api_token_snakes s
         JOIN api_tokens t ON t.id = s.api_token_id
         WHERE t.revoked_at IS NULL"
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![fx.snakes[0]],
        "soft-deleted snake must not be copied into the token"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn reauthorize_replaces_grant_and_token(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let first_code = fx
        .approve("profile.read snakes.read", &challenge, "all", &[], false)
        .await;
    let first_token = fx.exchange(&first_code, &verifier).await;

    // Second authorization, narrower scope, exchanged after the first.
    let second_code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;
    let second_token = fx.exchange(&second_code, &verifier).await;

    // The first token died with the rotation; the second works.
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&first_token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = send_api(&fx.app, "GET", "/api/snakes", Some(&second_token), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}"); // no snakes.read
    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&second_token), None).await;
    assert_eq!(status, StatusCode::OK);

    // Exactly one active grant; the old grant row is revoked.
    let active: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_grants
           WHERE user_id = $1 AND oauth_client_id = $2 AND revoked_at IS NULL"#,
        fx.user_id,
        fx.client.id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 1);
    let total: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_grants WHERE user_id = $1"#,
        fx.user_id
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, 2);
}

#[sqlx::test(migrations = "../migrations")]
async fn edit_grant_snakes_takes_effect_immediately(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let token = fx
        .full_flow("snakes.read snake.url.read", "selected", &[fx.snakes[0]])
        .await;
    let token_row = fx.active_grant(&pool).await;
    let token_id: Uuid = sqlx::query_scalar!(
        "SELECT id FROM api_tokens WHERE oauth_grant_id = $1 AND revoked_at IS NULL",
        token_row
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Before: A visible, B redacted.
    let url_of = |token: &str, name: &str| {
        let app = fx.app.clone();
        let token = token.to_string();
        let name = name.to_string();
        async move {
            let (status, body) = send_api(&app, "GET", "/api/snakes", Some(&token), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let snakes: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
            snakes
                .iter()
                .find(|s| s["name"] == name)
                .unwrap()
                .get("url")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        }
    };
    assert!(url_of(&token, "oauth-a").await.is_string());
    assert!(url_of(&token, "oauth-b").await.is_null());

    // Edit the selection to BOTH A and B (repeated keys prove the parse).
    let (name, value) = fx.cookie_header();
    let body = form_encode(&[
        ("snake_access", "selected"),
        ("snake_ids", &fx.snakes[0].to_string()),
    ]) + "&snake_ids="
        + &urlencoding::encode(&fx.snakes[1].to_string());
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/settings/authorized-apps/{token_row}/snakes"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(name, value)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // Same token secret, same row: now B's url is visible too.
    assert!(url_of(&token, "oauth-a").await.is_string());
    assert!(
        url_of(&token, "oauth-b").await.is_string(),
        "edit must take effect on the SAME token"
    );
    let token_id_after: Uuid = sqlx::query_scalar!(
        "SELECT id FROM api_tokens WHERE oauth_grant_id = $1 AND revoked_at IS NULL",
        token_row
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(token_id, token_id_after, "no re-issue");
}

#[sqlx::test(migrations = "../migrations")]
async fn edit_grant_can_switch_all_to_selected(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let token = fx.full_flow("snakes.read snake.url.read", "all", &[]).await;
    let grant = fx.active_grant(&pool).await;

    // All → selected [A].
    let (name, value) = fx.cookie_header();
    let body = form_encode(&[
        ("snake_access", "selected"),
        ("snake_ids", &fx.snakes[0].to_string()),
    ]);
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/settings/authorized-apps/{grant}/snakes"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(name, value)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let (status, body) = send_api(&fx.app, "GET", "/api/snakes", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let snakes: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let a = snakes.iter().find(|s| s["name"] == "oauth-a").unwrap();
    let b = snakes.iter().find(|s| s["name"] == "oauth-b").unwrap();
    assert!(a.get("url").is_some(), "{a}");
    assert!(b.get("url").is_none(), "{b}");

    let access: String = sqlx::query_scalar!(
        "SELECT snake_access FROM api_tokens WHERE oauth_grant_id = $1 AND revoked_at IS NULL",
        grant
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(access, "selected");
    let rows: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT battlesnake_id FROM api_token_snakes s
         JOIN api_tokens t ON t.id = s.api_token_id
         WHERE t.oauth_grant_id = $1 AND t.revoked_at IS NULL",
        grant
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![fx.snakes[0]]);
}

#[sqlx::test(migrations = "../migrations")]
async fn authorized_apps_page_renders(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    fx.full_flow("profile.read snake.url.read", "selected", &[fx.snakes[0]])
        .await;

    let (status, _location, body, headers) = fx.get_authorize("/settings/authorized-apps").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Test Client"), "{body}");
    assert!(body.contains("oauth-a"), "{body}");
    assert!(
        body.contains(Permission::SnakeUrlRead.description()),
        "{body}"
    );
    // Anti-framing: the page (and its POSTs) refuse framing.
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"),
        "authorized apps page must refuse framing"
    );

    // Another user's session: not listed, and their revoke 404s.
    let other_session = create_user_session(&pool, 9922002, false).await;
    let grant = fx.active_grant(&pool).await;
    let cookie = signed_session_cookie(&fx.state, other_session);
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::get("/settings/authorized-apps")
                .header(
                    header::COOKIE,
                    format!("{}={cookie}", crate::models::session::SESSION_COOKIE_NAME),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(!body.contains("Test Client"), "{body}");

    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/settings/authorized-apps/{grant}/revoke"))
                .header(
                    header::COOKIE,
                    format!("{}={cookie}", crate::models::session::SESSION_COOKIE_NAME),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../migrations")]
async fn token_revoke_via_api_kills_grant(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let token = fx.full_flow("profile.read", "all", &[]).await;
    let grant = fx.active_grant(&pool).await;
    let token_id: Uuid = sqlx::query_scalar!(
        "SELECT id FROM api_tokens WHERE oauth_grant_id = $1 AND revoked_at IS NULL",
        grant
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // DELETE /api/tokens/{id} with a session: the grant dies inside
    // models::api_token::revoke_token.
    let (name, value) = fx.cookie_header();
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::delete(format!("/api/tokens/{token_id}"))
                .header(name, value)
                .header(header::ORIGIN, "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let (status, _body) = send_api(&fx.app, "GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let revoked: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar!("SELECT revoked_at FROM oauth_grants WHERE id = $1", grant)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked.is_some());
}

// ---------------------------------------------------------------------------
// Token-endpoint client auth
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn token_endpoint_basic_auth_and_dual_method(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let (verifier, challenge) = make_verifier_pair();
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;

    let exchange_fields = |code: &str| {
        vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", REDIRECT_URI.to_string()),
            ("code_verifier", verifier.clone()),
        ]
    };

    // Form-body credentials work with no Basic header.
    let fields = exchange_fields(&code);
    let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut with_client = refs.clone();
    with_client.push(("client_id", &fx.client.public_client_id));
    with_client.push(("client_secret", &fx.secret));
    let (status, json, _headers) = fx.post_token(&with_client, None).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // A new code for the Basic-auth variants.
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;
    let fields = exchange_fields(&code);
    let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // Basic with percent-encoded halves authenticates.
    let encoded_id = urlencoding::encode(&fx.client.public_client_id).to_string();
    let encoded_secret = urlencoding::encode(&fx.secret).to_string();
    let basic: Option<(&str, &str)> = Some((encoded_id.as_str(), encoded_secret.as_str()));
    let (status, json, _headers) = fx.post_token(&refs, basic).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // Both Basic header AND body credentials → 400 invalid_request.
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;
    let fields = exchange_fields(&code);
    let mut refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    refs.push(("client_id", &fx.client.public_client_id));
    let (status, json, _headers) = fx.post_token(&refs, basic).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_request");

    // Malformed base64 → 401 invalid_client.
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::AUTHORIZATION, "Basic !!!not-base64!!!")
                .body(Body::from(form_encode(&[])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // No colon in the decoded value → 401 invalid_client.
    let no_colon = BASE64.encode("id-without-colon");
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::AUTHORIZATION, format!("Basic {no_colon}"))
                .body(Body::from(form_encode(&[])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Wrong secret → 401 invalid_client.
    let code = fx
        .approve("profile.read", &challenge, "all", &[], false)
        .await;
    let fields = exchange_fields(&code);
    let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (status, json, headers) = fx
        .post_token(&refs, Some((&fx.client.public_client_id, "wrong-secret")))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["error"], "invalid_client");
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    // Missing grant_type → unsupported_grant_type; missing params →
    // invalid_request. (Order: client auth first.)
    let (status, json, _headers) = fx
        .post_token(&[], Some((&fx.client.public_client_id, &fx.secret)))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "unsupported_grant_type");
    let (status, json, _headers) = fx
        .post_token(
            &[("grant_type", "authorization_code")],
            Some((&fx.client.public_client_id, &fx.secret)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_request");
}

#[sqlx::test(migrations = "../migrations")]
async fn token_endpoint_rejects_malformed_bodies(pool: PgPool) {
    let fx = Fixture::new(&pool).await;

    // A duplicate parameter fails serde_urlencoded parsing → invalid_request,
    // not invalid_client/unsupported_grant_type.
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(
                    "grant_type=authorization_code&grant_type=authorization_code",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = body_string(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_request");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    // A JSON body (with a JSON content type) → invalid_request too.
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    "{\"grant_type\":\"authorization_code\",\"code\":\"x\"}",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = body_string(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["error"], "invalid_request");
}

#[sqlx::test(migrations = "../migrations")]
async fn token_endpoint_invalid_utf8_still_returns_json_error(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(vec![0xff]))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = body_string(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"], "invalid_request");
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = "../migrations")]
async fn oauth_client_admin_lifecycle(pool: PgPool) {
    let fx = Fixture::new(&pool).await;

    // Non-admin session: 403.
    let (status, location, _body, _headers) = fx.get_authorize("/admin/oauth-clients").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(location.is_none());

    // Admin session drives the real forms.
    let admin_session = create_user_session(&pool, 9922010, true).await;
    let admin_cookie = signed_session_cookie(&fx.state, admin_session);
    let cookie = format!(
        "{}={admin_cookie}",
        crate::models::session::SESSION_COOKIE_NAME
    );

    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post("/admin/oauth-clients")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, cookie.clone())
                .body(Body::from(form_encode(&[
                    ("name", "Admin Made"),
                    ("kind", "hosting_provider"),
                    ("logo_url", "https://made.example/logo.png"),
                    ("homepage_url", "https://made.example"),
                    (
                        "redirect_uris",
                        "https://made.example/cb\nhttps://made.example/cb2\n\n",
                    ),
                ])))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = body_string(response).await;
    assert!(body.contains("client_secret"), "{body}");
    assert!(body.contains("shown once"), "{body}");

    // The created client verifies via authenticate_client and carries both
    // redirect URIs (blank lines skipped).
    let created = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, public_client_id FROM oauth_clients WHERE name = 'Admin Made'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let secret = body
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|run| run.len() == 64 && *run != created.1)
        .expect("one-time secret in page")
        .to_string();
    let authenticated = oauth::authenticate_client(&pool, &created.1, &secret)
        .await
        .unwrap();
    assert!(authenticated.is_some());
    assert_eq!(
        authenticated.unwrap().kind,
        oauth::OauthClientKind::HostingProvider
    );
    let uris = oauth::get_client_with_uris(&pool, created.0)
        .await
        .unwrap()
        .unwrap()
        .redirect_uris;
    assert_eq!(
        uris,
        vec![
            "https://made.example/cb".to_string(),
            "https://made.example/cb2".to_string()
        ]
    );

    // Rotate: the old secret stops working, the new one works.
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/admin/oauth-clients/{}/rotate", created.0))
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let rotate_body = body_string(response).await;
    let new_secret = rotate_body
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|run| run.len() == 64 && *run != created.1 && *run != secret)
        .unwrap()
        .to_string();
    assert!(
        oauth::authenticate_client(&pool, &created.1, &secret)
            .await
            .unwrap()
            .is_none(),
        "old secret must stop working"
    );
    assert!(
        oauth::authenticate_client(&pool, &created.1, &new_secret)
            .await
            .unwrap()
            .is_some()
    );

    // Disable: authenticate_client returns None (even with the good secret).
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::post(format!("/admin/oauth-clients/{}/disable", created.0))
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(
        oauth::authenticate_client(&pool, &created.1, &new_secret)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn admin_create_rejects_javascript_urls(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let admin_session = create_user_session(&pool, 9922011, true).await;
    let admin_cookie = signed_session_cookie(&fx.state, admin_session);
    let cookie = format!(
        "{}={admin_cookie}",
        crate::models::session::SESSION_COOKIE_NAME
    );

    for (label, logo_url, homepage_url) in [
        ("javascript homepage", "", "javascript:alert(1)"),
        ("javascript logo", "javascript:alert(1)", ""),
        ("data homepage", "", "data:text/html,alert(1)"),
    ] {
        let response = fx
            .app
            .clone()
            .oneshot(
                Request::post("/admin/oauth-clients")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::from(form_encode(&[
                        ("name", "XSS Client"),
                        ("kind", "standard"),
                        ("logo_url", logo_url),
                        ("homepage_url", homepage_url),
                        ("redirect_uris", "https://xss.example/cb"),
                    ])))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "{label}: rejected with a flash redirect"
        );

        let count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_clients WHERE name = 'XSS Client'"#
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "{label}: no client row may be created");
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn admin_create_validates_redirect_uris(pool: PgPool) {
    let fx = Fixture::new(&pool).await;
    let admin_session = create_user_session(&pool, 9922012, true).await;
    let admin_cookie = signed_session_cookie(&fx.state, admin_session);
    let cookie = format!(
        "{}={admin_cookie}",
        crate::models::session::SESSION_COOKIE_NAME
    );

    let post_create = |uris: &str| {
        let app = fx.app.clone();
        let cookie = cookie.clone();
        let uris = uris.to_string();
        async move {
            app.oneshot(
                Request::post("/admin/oauth-clients")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(form_encode(&[
                        ("name", "URI Test Client"),
                        ("kind", "standard"),
                        ("logo_url", ""),
                        ("homepage_url", ""),
                        ("redirect_uris", &uris),
                    ])))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // Rejected: non-loopback http, fragment, unparseable.
    for uris in [
        "http://example.com/cb",
        "https://x.example/cb#frag",
        "not a uri at all",
    ] {
        let response = post_create(uris).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{uris}");
        let count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM oauth_clients WHERE name = 'URI Test Client'"#
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "{uris}: no client row");
    }

    // Accepted: loopback http (for local development).
    let response = post_create("http://127.0.0.1:8080/cb").await;
    assert_eq!(response.status(), StatusCode::OK);
    let client = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, public_client_id FROM oauth_clients WHERE name = 'URI Test Client'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let uris = oauth::get_client_with_uris(&pool, client.0)
        .await
        .unwrap()
        .unwrap()
        .redirect_uris;
    assert_eq!(uris, vec!["http://127.0.0.1:8080/cb".to_string()]);

    // Duplicate lines collapse to one registration (a duplicate would hit
    // the composite PK and 500).
    let response = post_create("https://dup.example/cb\nhttps://dup.example/cb").await;
    assert_eq!(response.status(), StatusCode::OK);
    let dup = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, public_client_id FROM oauth_clients WHERE name = 'URI Test Client' ORDER BY created_at DESC",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(dup.0, client.0);
    let uris = oauth::get_client_with_uris(&pool, dup.0)
        .await
        .unwrap()
        .unwrap()
        .redirect_uris;
    assert_eq!(
        uris,
        vec!["https://dup.example/cb".to_string()],
        "duplicate lines must collapse"
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn consent_screen_renders_no_javascript_urls(pool: PgPool) {
    let fx = Fixture::new(&pool).await;

    // A row inserted directly with javascript: URLs (bypassing admin
    // validation) must not reach href/src on the consent screen — defense
    // in depth at render.
    let public_client_id = "cafe0000deadbeefcafe0000deadbeef".to_string();
    sqlx::query!(
        r#"
        INSERT INTO oauth_clients (public_client_id, secret_hash, name, logo_url, homepage_url)
        VALUES ($1, 'never-authenticated', 'Evil Client', 'javascript:alert(1)', 'javascript:alert(1)')
        "#,
        public_client_id
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO oauth_client_redirect_uris (oauth_client_id, uri) SELECT id, $2 FROM oauth_clients WHERE public_client_id = $1",
        public_client_id,
        REDIRECT_URI
    )
    .execute(&pool)
    .await
    .unwrap();

    let (_verifier, challenge) = make_verifier_pair();
    let uri = format!(
        "/oauth/authorize?client_id={public_client_id}&redirect_uri={}&response_type=code&scope=profile.read&code_challenge={challenge}&code_challenge_method=S256",
        urlencoding::encode(REDIRECT_URI),
    );
    let (status, _location, body, headers) = fx.get_authorize(&uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Evil Client"), "{body}");
    assert!(
        !body.contains("href=\"javascript:"),
        "javascript: homepage must not render as a link: {body}"
    );
    assert!(
        !body.contains("src=\"javascript:"),
        "javascript: logo must not render as an image: {body}"
    );
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
}

// ---------------------------------------------------------------------------
// Login resume
// ---------------------------------------------------------------------------

mod resume {
    use super::*;
    use crate::github::auth::GitHubOAuthConfig;
    use crate::models::session;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

    async fn github_mocks() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .and(matchers::path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "mock_token_xyz",
                "token_type": "bearer",
                "scope": "",
            })))
            .mount(&server)
            .await;
        Mock::given(matchers::method("GET"))
            .and(matchers::path("/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 424299,
                "login": "resume-login",
                "name": "Resume User",
                "email": null,
                "avatar_url": "https://example.com/a.png",
            })))
            .mount(&server)
            .await;
        server
    }

    struct ResumeFixture {
        app: axum::Router,
        state: AppState,
        session_id: Uuid,
        pool: PgPool,
    }

    impl ResumeFixture {
        async fn new(pool: PgPool, github: &MockServer) -> Self {
            let mut state = AppState::test_from_pool(pool.clone());
            let config = std::sync::Arc::get_mut(&mut state.config).expect("unique config Arc");
            config.github = Some(GitHubOAuthConfig {
                client_id: "client".into(),
                client_secret: "secret".into(),
                redirect_uri: "https://arena.example/auth/github/callback".into(),
                oauth_url: github.uri(),
                token_url: format!("{}/login/oauth/access_token", github.uri()),
                api_url: github.uri(),
            });
            let session = session::create_session(&pool).await.unwrap();
            let app = crate::routes::routes(state.clone())
                .expect("asset router")
                .layer(tower_cookies::CookieManagerLayer::new());
            Self {
                app,
                state,
                session_id: session.session_id,
                pool,
            }
        }

        fn cookie(&self) -> String {
            format!(
                "{}={}",
                crate::models::session::SESSION_COOKIE_NAME,
                signed_session_cookie(&self.state, self.session_id)
            )
        }

        async fn login_round_trip(&self, with_marker: bool) -> Option<String> {
            // The initiator (with or without the resume marker).
            let initiate = if with_marker {
                "/auth/github?resume_oauth=true"
            } else {
                "/auth/github"
            };
            let response = self
                .app
                .clone()
                .oneshot(
                    Request::get(initiate)
                        .header(header::COOKIE, self.cookie())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            // The GitHub authorize redirect carries a state we don't know;
            // set it directly like the callback tests do.
            session::set_github_oauth_state_with_cli(
                &self.pool,
                self.session_id,
                "test-state".to_string(),
                false,
            )
            .await
            .unwrap();

            let response = self
                .app
                .clone()
                .oneshot(
                    Request::get("/auth/github/callback?code=mock_code_1&state=test-state")
                        .header(header::COOKIE, self.cookie())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if response.status() == StatusCode::SEE_OTHER {
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .map(str::to_string)
            } else {
                None
            }
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn login_resume_returns_to_authorize(pool: PgPool) {
        let github = github_mocks().await;
        let fx = ResumeFixture::new(pool.clone(), &github).await;
        let client = make_client(&pool, "Resume Client", &[REDIRECT_URI]).await;
        let (_verifier, challenge) = make_verifier_pair();
        let authorize_uri = authorize_uri(
            &client.client,
            REDIRECT_URI,
            "profile.read",
            "xyz",
            &challenge,
        );

        // Anonymous authorize → 303 to the login initiator; the request is
        // stored on the session.
        let response = fx
            .app
            .clone()
            .oneshot(
                Request::get(authorize_uri.clone())
                    .header(header::COOKIE, fx.cookie())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(location, "/auth/github?resume_oauth=true");
        let stored: Option<String> = sqlx::query_scalar!(
            "SELECT post_login_redirect FROM sessions WHERE session_id = $1",
            fx.session_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored.as_deref(), Some(authorize_uri.as_str()));

        // Login WITH the marker: the callback lands back on the authorize URI.
        let final_location = fx.login_round_trip(true).await.expect("303 callback");
        assert_eq!(final_location, authorize_uri);
        let cleared: Option<String> = sqlx::query_scalar!(
            "SELECT post_login_redirect FROM sessions WHERE session_id = $1",
            fx.session_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(cleared.is_none(), "take clears the stored path");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stale_resume_is_cleared_by_normal_login(pool: PgPool) {
        let github = github_mocks().await;
        let fx = ResumeFixture::new(pool.clone(), &github).await;
        let client = make_client(&pool, "Resume Client", &[REDIRECT_URI]).await;
        let (_verifier, challenge) = make_verifier_pair();
        let authorize_uri = authorize_uri(
            &client.client,
            REDIRECT_URI,
            "profile.read",
            "xyz",
            &challenge,
        );

        // An abandoned consent parks its request…
        session::set_post_login_redirect(&pool, fx.session_id, authorize_uri)
            .await
            .unwrap();

        // …then an unrelated login (no marker) wipes it and lands on /.
        let final_location = fx.login_round_trip(false).await.expect("303 callback");
        assert_eq!(final_location, "/");
        let cleared: Option<String> = sqlx::query_scalar!(
            "SELECT post_login_redirect FROM sessions WHERE session_id = $1",
            fx.session_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(cleared.is_none());
    }
}
