//! OAuth 2 provider routes: the authorization endpoint (approval screen),
//! the consent decision POST, and the token endpoint.
//!
//! Authorization code + PKCE (RFC 6749 + RFC 7636); S256 required. These
//! live on the main router, NOT inside the `/api` nest — OAuth tokens call
//! the existing `/api/*` routes and get enforcement for free there.

use std::collections::BTreeSet;

use axum::{
    body::Bytes,
    extract::{Query, RawForm, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    errors::ServerResult,
    models::{
        api_token::{Permission, SnakeAccess},
        battlesnake, oauth, session,
    },
    routes::auth::{CurrentSession, CurrentUserWithSession},
    state::AppState,
};

#[derive(Debug, Default, Deserialize)]
pub struct AuthorizeQuery {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub response_type: Option<String>,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

/// A request that passed full validation.
struct ValidatedAuthorize {
    client: oauth::OauthClient,
    /// The exact redirect URI that was validated (a registered one).
    redirect_uri: String,
    /// Requested scope, canonical order, de-duplicated.
    permissions: BTreeSet<Permission>,
    /// The space-joined request the user saw (what the consent token binds).
    scope_str: String,
    code_challenge: String,
    state: Option<String>,
}

/// Why validation failed. `ErrorPage` failures never redirect (the
/// `redirect_uri` cannot be trusted, or must not be trusted with the
/// failure); `Redirect` failures bounce back with the RFC 6749 `error=`
/// parameter and the echoed `state`.
enum AuthorizeFailure {
    ErrorPage,
    Redirect { error: &'static str },
}

/// The wire parameters of an authorize request, from the query string or
/// the consent form — the inputs to [`validate_authorize_request`].
struct AuthorizeParams<'a> {
    client_id: Option<&'a str>,
    redirect_uri: Option<&'a str>,
    response_type: Option<&'a str>,
    scope: Option<&'a str>,
    code_challenge: Option<&'a str>,
    code_challenge_method: Option<&'a str>,
    state: Option<&'a str>,
}

/// Validate an authorize request (GET or decision form). Order matters and
/// is pinned by tests: client → redirect URI → response type → PKCE →
/// scope.
async fn validate_authorize_request(
    state: &AppState,
    params: AuthorizeParams<'_>,
) -> Result<ValidatedAuthorize, AuthorizeFailure> {
    let AuthorizeParams {
        client_id,
        redirect_uri,
        response_type,
        scope,
        code_challenge,
        code_challenge_method,
        state: state_param,
    } = params;
    // 1. Unknown/missing client id or a disabled client: error page, never a
    //    redirect (an unknown application must not learn anything about
    //    redirect targets, and a disabled client has no standing).
    let Some(client_id) = client_id.filter(|id| !id.is_empty()) else {
        return Err(AuthorizeFailure::ErrorPage);
    };
    let client = match oauth::get_client_by_public_id(&state.db, client_id).await {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("Failed to load OAuth client: {e:#}");
            return Err(AuthorizeFailure::ErrorPage);
        }
    };
    let Some(client) = client else {
        return Err(AuthorizeFailure::ErrorPage);
    };
    if client.disabled_at.is_some() {
        return Err(AuthorizeFailure::ErrorPage);
    }

    // 2. redirect_uri must be a byte-exact member of the registered set:
    //    error page, never a redirect.
    let Some(redirect_uri) = redirect_uri.filter(|uri| !uri.is_empty()) else {
        return Err(AuthorizeFailure::ErrorPage);
    };
    let redirect_uris = match oauth::get_client_with_uris(&state.db, client.id).await {
        Ok(with_uris) => with_uris.map(|w| w.redirect_uris).unwrap_or_default(),
        Err(e) => {
            tracing::error!("Failed to load OAuth client redirect URIs: {e:#}");
            return Err(AuthorizeFailure::ErrorPage);
        }
    };
    if !redirect_uris
        .iter()
        .any(|registered| registered == redirect_uri)
    {
        return Err(AuthorizeFailure::ErrorPage);
    }

    // 3. Everything else redirects back with the standard error.
    if response_type != Some("code") {
        return Err(AuthorizeFailure::Redirect {
            error: "unsupported_response_type",
        });
    }
    let Some(code_challenge) = code_challenge.filter(|c| (43..=128).contains(&c.len())) else {
        return Err(AuthorizeFailure::Redirect {
            error: "invalid_request",
        });
    };
    if code_challenge_method != Some("S256") {
        return Err(AuthorizeFailure::Redirect {
            error: "invalid_request",
        });
    }
    let Some(scope_raw) = scope.filter(|s| !s.trim().is_empty()) else {
        return Err(AuthorizeFailure::Redirect {
            error: "invalid_scope",
        });
    };
    // Parse strictly; dedupe silently; every permission must be one an OAuth
    // client may request (this covers tokens.manage and admin).
    let mut permissions = BTreeSet::new();
    let mut names: Vec<&str> = Vec::new();
    for token in scope_raw.split_whitespace() {
        if !names.contains(&token) {
            names.push(token);
        }
    }
    for name in &names {
        let Ok(permission) = name.parse::<Permission>() else {
            return Err(AuthorizeFailure::Redirect {
                error: "invalid_scope",
            });
        };
        if !Permission::OAUTH_REQUESTABLE.contains(&permission) {
            return Err(AuthorizeFailure::Redirect {
                error: "invalid_scope",
            });
        }
        permissions.insert(permission);
    }
    let scope_str = names.join(" ");

    Ok(ValidatedAuthorize {
        client,
        redirect_uri: redirect_uri.to_string(),
        permissions,
        scope_str,
        code_challenge: code_challenge.to_string(),
        state: state_param.map(str::to_string),
    })
}

/// Anti-framing headers on every authorize response that renders an arena
/// page (the consent screen and the no-redirect error pages).
fn anti_framing_headers() -> [(header::HeaderName, &'static str); 2] {
    [
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
    ]
}

/// The branded error page for failures that must never redirect to the
/// client. Generic message: does not leak whether a client exists.
fn authorization_error_page(page_factory: PageFactory) -> Response {
    let page = page_factory.create_page(
        "Authorization error".to_string(),
        Box::new(html! {
            div class="home" {
                section class="section" {
                    h1 { "Authorization error" }
                    p class="empty" {
                        "This authorization request can't be completed: the "
                        "application or its redirect target isn't registered. "
                        "Go back to the application and try again, or contact "
                        "the Battlesnake team if you believe this is a mistake."
                    }
                    div class="cta-row" {
                        a class="btn" href="/" { "Back to Home" }
                    }
                }
            }
        }),
    );
    (StatusCode::BAD_REQUEST, anti_framing_headers(), page).into_response()
}

/// Build `redirect_uri?error=...(&state=...)` — appending to whatever query
/// the registered URI already carries.
fn error_redirect(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    match append_query_params(redirect_uri, &[("error", error)]) {
        Some(url) => {
            let url = match state.filter(|s| !s.is_empty()) {
                Some(state) => append_query_params(&url, &[("state", state)]).unwrap_or(url),
                None => url,
            };
            Redirect::to(&url).into_response()
        }
        None => StatusCode::BAD_REQUEST.into_response(),
    }
}

/// Append query parameters to a URI, keeping any existing query. `None`
/// when the URI doesn't parse (registered URIs are admin-validated absolute
/// http(s) URLs, so this is defense in depth).
fn append_query_params(base: &str, params: &[(&str, &str)]) -> Option<String> {
    let mut url = url::Url::parse(base).ok()?;
    {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in params {
            pairs.append_pair(key, value);
        }
    }
    Some(url.into())
}

// ---------------------------------------------------------------------------
// Consent token (stateless HMAC over the exact displayed request)
// ---------------------------------------------------------------------------

/// The consent-token message: six fields, each percent-encoded (no
/// delimiter can survive inside a field), joined with `\n`. `state` absent
/// and empty encode identically — harmless, and the form round-trips
/// uniformly.
pub(crate) fn consent_csrf_message(
    session_id: Uuid,
    client_public_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    scope: &str,
    state: Option<&str>,
) -> String {
    [
        urlencoding::encode(&session_id.to_string()).to_string(),
        urlencoding::encode(client_public_id).to_string(),
        urlencoding::encode(redirect_uri).to_string(),
        urlencoding::encode(code_challenge).to_string(),
        urlencoding::encode(scope).to_string(),
        urlencoding::encode(state.unwrap_or("")).to_string(),
    ]
    .join("\n")
}

/// Domain-separated HMAC key for consent tokens: a labeled subkey derived
/// from the server cookie key's master bytes (the `unsubscribe_key`
/// pattern in notifications.rs) — never the raw master key. Rotating
/// COOKIE_KEY invalidates in-flight consents at the same moment it
/// invalidates all sessions.
pub(crate) fn consent_csrf_key(cookie_key_master: &[u8]) -> ring::hmac::Key {
    let root = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, cookie_key_master);
    let digest = ring::hmac::sign(&root, b"arena.oauth-consent.v1");
    ring::hmac::Key::new(ring::hmac::HMAC_SHA256, digest.as_ref())
}

/// hex(HMAC-SHA256(key, message)).
pub(crate) fn mint_consent_csrf(key: &ring::hmac::Key, message: &str) -> String {
    hex::encode(ring::hmac::sign(key, message.as_bytes()).as_ref())
}

/// Constant-time verification (`ring::hmac::verify`); non-hex or
/// wrong-length tokens are rejected before any comparison.
pub(crate) fn verify_consent_csrf(key: &ring::hmac::Key, message: &str, token: &str) -> bool {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let Ok(tag) = hex::decode(token) else {
        return false;
    };
    ring::hmac::verify(key, message.as_bytes(), &tag).is_ok()
}

// ---------------------------------------------------------------------------
// GET /oauth/authorize
// ---------------------------------------------------------------------------

/// GET /oauth/authorize — validate the request, then either the error page,
/// an error redirect, a login-and-return bounce, or the approval screen.
pub async fn authorize(
    State(state): State<AppState>,
    CurrentSession { session, user }: CurrentSession,
    page_factory: PageFactory,
    Query(query): Query<AuthorizeQuery>,
) -> ServerResult<Response, StatusCode> {
    let validated = match validate_authorize_request(
        &state,
        AuthorizeParams {
            client_id: query.client_id.as_deref(),
            redirect_uri: query.redirect_uri.as_deref(),
            response_type: query.response_type.as_deref(),
            scope: query.scope.as_deref(),
            code_challenge: query.code_challenge.as_deref(),
            code_challenge_method: query.code_challenge_method.as_deref(),
            state: query.state.as_deref(),
        },
    )
    .await
    {
        Ok(validated) => validated,
        Err(AuthorizeFailure::ErrorPage) => {
            return Ok(authorization_error_page(page_factory));
        }
        Err(AuthorizeFailure::Redirect { error }) => {
            // The redirect_uri is registered (checked second), so this
            // redirect is safe.
            let redirect_uri = query.redirect_uri.as_deref().unwrap_or_default();
            let redirect = if redirect_uri.is_empty() {
                // Unreachable: validation redirects only after the
                // redirect_uri passed the exact-match check.
                return Ok(authorization_error_page(page_factory));
            } else {
                error_redirect(redirect_uri, error, query.state.as_deref())
            };
            return Ok(redirect);
        }
    };

    let redirect_uri = validated.redirect_uri.clone();

    // Not logged in: remember this exact authorize request, then run the
    // existing GitHub login and come back to it.
    let Some(user) = user else {
        let path_and_query = format!("/oauth/authorize?{}", query_string_of(&query));
        if !path_and_query.starts_with("/oauth/authorize") {
            return Ok(authorization_error_page(page_factory));
        }
        session::set_post_login_redirect(&state.db, session.session_id, path_and_query)
            .await
            .wrap_err("Failed to store post-login redirect")?;
        return Ok(Redirect::to("/auth/github?resume_oauth=true").into_response());
    };

    let snakes = battlesnake::get_battlesnakes_by_user_id(&state.db, user.user_id)
        .await
        .wrap_err("Failed to load user's snakes for consent screen")?;

    let key = consent_csrf_key(state.cookie_key.master());
    let csrf = mint_consent_csrf(
        &key,
        &consent_csrf_message(
            session.session_id,
            &validated.client.public_client_id,
            &redirect_uri,
            &validated.code_challenge,
            &validated.scope_str,
            validated.state.as_deref(),
        ),
    );

    // The full site layout (viewport meta, styles, the logged-in header) —
    // a bare fragment would lay out at ~980px on phones.
    let page = page_factory.create_page(
        format!("Authorize {}", validated.client.name),
        Box::new(consent_screen(&validated, &redirect_uri, &snakes, &csrf)),
    );
    Ok((anti_framing_headers(), page).into_response())
}

/// Rebuild the request's query string (the value stored in
/// `post_login_redirect`). Only the original params, nothing added.
fn query_string_of(query: &AuthorizeQuery) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in [
        ("client_id", query.client_id.as_deref()),
        ("redirect_uri", query.redirect_uri.as_deref()),
        ("response_type", query.response_type.as_deref()),
        ("scope", query.scope.as_deref()),
        ("state", query.state.as_deref()),
        ("code_challenge", query.code_challenge.as_deref()),
        (
            "code_challenge_method",
            query.code_challenge_method.as_deref(),
        ),
    ] {
        if let Some(value) = value {
            serializer.append_pair(key, value);
        }
    }
    serializer.finish()
}

/// The mobile-first approval screen: one plain-words line per requested
/// permission (only `email.read` gets a checkbox), the snake-access radios
/// with an honest explainer, and approve/deny at 44px tap targets.
fn consent_screen(
    validated: &ValidatedAuthorize,
    redirect_uri: &str,
    snakes: &[battlesnake::Battlesnake],
    csrf: &str,
) -> Markup {
    let requestable = &validated.permissions;
    let logo = oauth::renderable_https_url(&validated.client.logo_url);
    let homepage = oauth::renderable_https_url(&validated.client.homepage_url);
    html! {
        div class="home" style="max-width: 480px; margin: 0 auto;" {
            section class="section" {
                div style="display: flex; align-items: center; gap: 14px; flex-wrap: wrap;" {
                    @if let Some(logo) = logo {
                        img src=(logo) alt="" width="44" height="44"
                            style="width: 44px; height: 44px; border-radius: 8px; object-fit: cover;";
                    }
                    h1 style="font-size: 24px; overflow-wrap: anywhere;" {
                        (validated.client.name)
                    }
                }
                p style="font-size: 16px; color: var(--muted); margin-top: 6px;" {
                    @if let Some(homepage) = homepage {
                        a href=(homepage) rel="noopener" { (validated.client.name) }
                        " wants to access your Battlesnake account."
                    } @else {
                        (validated.client.name) " wants to access your Battlesnake account."
                    }
                }

                form action="/oauth/authorize/decision" method="post" {
                    input type="hidden" name="client_id" value=(validated.client.public_client_id);
                    input type="hidden" name="redirect_uri" value=(redirect_uri);
                    input type="hidden" name="state" value=(validated.state.as_deref().unwrap_or(""));
                    input type="hidden" name="code_challenge" value=(validated.code_challenge);
                    input type="hidden" name="scope" value=(validated.scope_str);
                    input type="hidden" name="csrf" value=(csrf);

                    h2 style="margin-top: 28px;" { "The app will be able to:" }
                    ul style="list-style: none; padding: 0; margin: 12px 0 0; display: flex; flex-direction: column; gap: 10px;" {
                        @for permission in Permission::ALL {
                            @if requestable.contains(&permission) {
                                @if permission == Permission::EmailRead {
                                    li {
                                        label style="display: flex; gap: 12px; align-items: flex-start; font-size: 16px; min-height: 44px; padding: 6px 0; cursor: pointer;" {
                                            input type="checkbox" name="email_read" checked[true]
                                                style="width: 22px; height: 22px; margin-top: 2px; flex: none;";
                                            span { (permission.description()) }
                                        }
                                    }
                                } @else if permission == Permission::SnakeUrlWrite {
                                    li style="font-size: 16px; padding: 6px 0;" {
                                        div style="color: var(--down); font-weight: 600;" {
                                            (permission.description())
                                        }
                                    }
                                } @else {
                                    li style="font-size: 16px; padding: 6px 0;" {
                                        (permission.description())
                                    }
                                }
                            }
                        }
                    }
                    @if requestable.is_empty() {
                        p style="font-size: 16px;" { "No permissions requested." }
                    }
                    @if requestable.contains(&Permission::EmailRead) {
                        p style="font-size: 16px; color: var(--muted); margin-top: 4px;" {
                            "You can uncheck your email address above."
                        }
                    }

                    h2 style="margin-top: 28px;" { "Which of your snakes?" }
                    div style="display: flex; flex-direction: column; gap: 8px; margin-top: 12px;" {
                        label style="display: flex; gap: 12px; align-items: center; font-size: 16px; min-height: 44px; cursor: pointer;" {
                            input type="radio" name="snake_access" value="all" checked[true]
                                style="width: 22px; height: 22px; flex: none;";
                            span { "All of my snakes" }
                        }
                        label style="display: flex; gap: 12px; align-items: center; font-size: 16px; min-height: 44px; cursor: pointer;" {
                            input type="radio" name="snake_access" value="selected"
                                style="width: 22px; height: 22px; flex: none;";
                            span { "Only the snakes I pick" }
                        }
                    }
                    p style="font-size: 16px; color: var(--muted); margin-top: 10px;" {
                        "This choice limits which snakes the app can see the URL of, edit, "
                        "delete (app-created only), and enter into leaderboards. It does not "
                        "limit seeing your snakes or starting games — those permissions "
                        "always apply to all your snakes."
                    }

                    @if snakes.is_empty() {
                        p style="font-size: 16px; color: var(--muted);" {
                            "You have no battlesnakes yet."
                        }
                    } @else {
                        ul style="list-style: none; padding: 0; margin: 12px 0 0; display: flex; flex-direction: column; gap: 4px;" {
                            @for snake in snakes {
                                li {
                                    label style="display: flex; gap: 12px; align-items: center; font-size: 16px; min-height: 44px; cursor: pointer;" {
                                        input type="checkbox" name="snake_ids" value=(snake.battlesnake_id)
                                            style="width: 22px; height: 22px; flex: none;";
                                        span { (snake.name) }
                                    }
                                }
                            }
                        }
                    }

                    div class="cta-row" style="margin-top: 28px; display: flex; gap: 14px; flex-wrap: wrap;" {
                        button type="submit" name="decision" value="deny" class="btn"
                            style="min-height: 44px; font-size: 16px;" {
                            "Deny"
                        }
                        button type="submit" name="decision" value="approve" class="btn solid"
                            style="min-height: 44px; font-size: 16px;" {
                            "Approve"
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// POST /oauth/authorize/decision
// ---------------------------------------------------------------------------

/// POST /oauth/authorize/decision — the consent form's approve/deny. The
/// form is client-controlled: the stateless consent token is verified
/// FIRST (binding the decision to the exact displayed request), then
/// everything is re-validated from scratch.
pub async fn authorize_decision(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    page_factory: PageFactory,
    RawForm(form_bytes): RawForm,
) -> ServerResult<Response, StatusCode> {
    let form: Vec<(String, String)> = url::form_urlencoded::parse(&form_bytes)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    // First check: the consent token, rebuilt from the form's own bound
    // fields plus this session. A missing or forged token (including a
    // tampered hidden `scope`, or a token minted for another session) means
    // the no-redirect error page — no code, no redirect.
    let message = consent_csrf_message(
        session.session_id,
        field("client_id").unwrap_or_default(),
        field("redirect_uri").unwrap_or_default(),
        field("code_challenge").unwrap_or_default(),
        field("scope").unwrap_or_default(),
        field("state").filter(|s| !s.is_empty()),
    );
    let key = consent_csrf_key(state.cookie_key.master());
    let csrf_ok = field("csrf").is_some_and(|token| verify_consent_csrf(&key, &message, token));
    if !csrf_ok {
        return Ok(authorization_error_page(page_factory));
    }

    // Re-validate from scratch (client, redirect URI, response type —
    // implied by reaching this handler — PKCE, scope).
    let validated = match validate_authorize_request(
        &state,
        AuthorizeParams {
            client_id: field("client_id"),
            redirect_uri: field("redirect_uri"),
            response_type: Some("code"),
            scope: field("scope"),
            code_challenge: field("code_challenge"),
            code_challenge_method: Some("S256"),
            state: field("state"),
        },
    )
    .await
    {
        Ok(validated) => validated,
        Err(AuthorizeFailure::ErrorPage) => {
            return Ok(authorization_error_page(page_factory));
        }
        Err(AuthorizeFailure::Redirect { error }) => {
            let redirect_uri = field("redirect_uri").unwrap_or_default();
            return Ok(error_redirect(redirect_uri, error, field("state")));
        }
    };
    let redirect_uri = field("redirect_uri").unwrap_or_default().to_string();

    // The decision must be explicit: `deny` → access_denied, `approve` →
    // grant, anything else (or missing) → the error page. Never treat an
    // unknown submit as approval.
    match field("decision") {
        Some("deny") => {
            return Ok(error_redirect(
                &redirect_uri,
                "access_denied",
                validated.state.as_deref(),
            ));
        }
        Some("approve") => {}
        _ => {
            return Ok(authorization_error_page(page_factory));
        }
    }

    // Approve. Granted = the re-validated requested scope, minus email.read
    // when its box is unchecked — the only subset input the screen offers
    // (stray `permissions=` keys are ignored outright).
    let mut permissions = validated.permissions.clone();
    if field("email_read").is_none() {
        permissions.remove(&Permission::EmailRead);
    }

    // Snake access: `selected` requires every checked id to parse as a UUID
    // and belong to the user.
    let snake_access = match field("snake_access") {
        Some("all") => SnakeAccess::All,
        Some("selected") => SnakeAccess::Selected,
        _ => {
            return Ok(authorization_error_page(page_factory));
        }
    };
    let mut snake_ids: BTreeSet<Uuid> = BTreeSet::new();
    for (_, value) in form.iter().filter(|(key, _)| key == "snake_ids") {
        let Ok(id) = Uuid::parse_str(value) else {
            return Ok(authorization_error_page(page_factory));
        };
        snake_ids.insert(id);
    }
    if snake_access == SnakeAccess::Selected && !snake_ids.is_empty() {
        let id_list: Vec<Uuid> = snake_ids.iter().copied().collect();
        let owned: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64"
               FROM battlesnakes
               WHERE battlesnake_id = ANY($1) AND user_id = $2 AND deleted_at IS NULL"#,
            &id_list,
            user.user_id
        )
        .fetch_one(&state.db)
        .await
        .wrap_err("Failed to check snake ownership for consent")?;
        if owned != snake_ids.len() as i64 {
            return Ok(authorization_error_page(page_factory));
        }
    }
    let snake_ids: Vec<Uuid> = if snake_access == SnakeAccess::All {
        Vec::new()
    } else {
        snake_ids.into_iter().collect()
    };

    let permissions: Vec<Permission> = permissions.into_iter().collect();
    let code = oauth::create_authorization_code(
        &state.db,
        oauth::NewAuthorizationCode {
            client_db_id: validated.client.id,
            user_id: user.user_id,
            redirect_uri: &redirect_uri,
            code_challenge: &validated.code_challenge,
            permissions: &permissions,
            snake_access,
            snake_ids: &snake_ids,
        },
    )
    .await
    .wrap_err("Failed to create authorization code")?;

    let success = append_query_params(&redirect_uri, &[("code", &code)]);
    let location = match (success, validated.state.as_deref()) {
        (Some(url), Some(state_param)) if !state_param.is_empty() => {
            append_query_params(&url, &[("state", state_param)]).unwrap_or(url)
        }
        (Some(url), _) => url,
        (None, _) => return Ok(authorization_error_page(page_factory)),
    };
    Ok(Redirect::to(&location).into_response())
}

// ---------------------------------------------------------------------------
// POST /oauth/token
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct TokenRequest {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

/// Every token-endpoint failure: RFC 6749 §5.2 JSON with the no-store
/// headers (§5.1).
fn oauth_error(status: StatusCode, error: &str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        axum::Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// POST /oauth/token — the authorization-code exchange. Always answers
/// JSON, whatever the body's content type, encoding, or parse failures (the
/// body is taken as raw bytes and parsed manually for exactly that reason:
/// the `String` extractor would reject invalid UTF-8 before this handler
/// runs, with a plain-text error).
pub async fn token(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // The request body MUST be application/x-www-form-urlencoded (RFC 6749
    // §4.1.3). A body that names another media type, or one that fails to
    // parse as form encoding at all, is a malformed request — not an auth
    // or grant-type failure.
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        let is_form = content_type
            .to_str()
            .map(|value| {
                value.split(';').next().is_some_and(|media_type| {
                    media_type
                        .trim()
                        .eq_ignore_ascii_case("application/x-www-form-urlencoded")
                })
            })
            .unwrap_or(false);
        if !is_form {
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
        }
    }
    // Form-encoded bodies are UTF-8 by definition (RFC 6749 §4.1.3 via the
    // urlencoded spec); anything else is malformed the same way.
    let body = match std::str::from_utf8(&body) {
        Ok(body) => body,
        Err(e) => {
            tracing::debug!("Non-UTF-8 token endpoint body: {e}");
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
        }
    };
    let request: TokenRequest = match serde_urlencoded::from_str(body) {
        Ok(request) => request,
        Err(e) => {
            tracing::debug!("Malformed token endpoint body: {e}");
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
        }
    };

    // Client authentication — exactly one method per request (RFC 6749
    // §2.3). Basic auth: base64, split at the first ':', then
    // form-urldecode each half (§2.3.1).
    enum ClientCredentials {
        Basic { id: String, secret: String },
        Form,
    }
    let credentials = if let Some(basic) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
    {
        if request.client_id.is_some() || request.client_secret.is_some() {
            return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
        }
        let decoded = match BASE64_STANDARD.decode(basic) {
            Ok(bytes) => String::from_utf8(bytes).ok(),
            Err(_) => None,
        };
        let Some(decoded) = decoded else {
            return oauth_error_client_basic();
        };
        let Some((id_raw, secret_raw)) = decoded.split_once(':') else {
            return oauth_error_client_basic();
        };
        let (Ok(id), Ok(secret)) = (urlencoding::decode(id_raw), urlencoding::decode(secret_raw))
        else {
            return oauth_error_client_basic();
        };
        ClientCredentials::Basic {
            id: id.into_owned(),
            secret: secret.into_owned(),
        }
    } else {
        ClientCredentials::Form
    };

    let (client_id, client_secret) = match &credentials {
        ClientCredentials::Basic { id, secret } => (id.clone(), secret.clone()),
        ClientCredentials::Form => {
            match (request.client_id.clone(), request.client_secret.clone()) {
                (Some(id), Some(secret)) => (id, secret),
                _ => return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client"),
            }
        }
    };

    let client = match oauth::authenticate_client(&state.db, &client_id, &client_secret).await {
        Ok(Some(client)) => client,
        Ok(None) => {
            return match credentials {
                ClientCredentials::Basic { .. } => oauth_error_client_basic(),
                ClientCredentials::Form => oauth_error(StatusCode::UNAUTHORIZED, "invalid_client"),
            };
        }
        Err(e) => {
            // A DB failure is arena's problem, not a bad client: 500 with
            // temporarily_unavailable (RFC 6749 §5.2), logged.
            tracing::error!("Failed to authenticate OAuth client: {e:#}");
            return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "temporarily_unavailable");
        }
    };

    if request.grant_type.as_deref() != Some("authorization_code") {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    let (Some(code), Some(redirect_uri), Some(code_verifier)) = (
        request.code.clone(),
        request.redirect_uri.clone(),
        request.code_verifier.clone(),
    ) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request");
    };

    match oauth::exchange_authorization_code(
        &state.db,
        client.id,
        &code,
        &redirect_uri,
        &code_verifier,
    )
    .await
    {
        Ok(Ok(exchanged)) => {
            // The granted scope in canonical `Permission::ALL` order — which
            // may be fewer than requested (a withheld `email.read`).
            let scope = Permission::ALL
                .iter()
                .filter(|p| exchanged.permissions.contains(*p))
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            (
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (header::PRAGMA, "no-cache"),
                ],
                axum::Json(serde_json::json!({
                    "access_token": exchanged.secret,
                    "token_type": "Bearer",
                    "scope": scope,
                })),
            )
                .into_response()
        }
        Ok(Err(oauth::ExchangeError::Replayed)) => {
            // Replay revokes the token this code issued and its grant
            // (RFC 6749 §4.1.2). Best-effort: the response stays
            // invalid_grant even if this errors.
            if let Err(e) = oauth::revoke_token_issued_by_code(&state.db, &code).await {
                tracing::error!("Failed to revoke replayed code's token: {e:#}");
            }
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant")
        }
        Ok(Err(_)) => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant"),
        Err(e) => {
            tracing::error!("Authorization code exchange failed: {e:#}");
            oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "temporarily_unavailable")
        }
    }
}

fn oauth_error_client_basic() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
            (header::WWW_AUTHENTICATE, "Basic"),
        ],
        axum::Json(serde_json::json!({ "error": "invalid_client" })),
    )
        .into_response()
}
