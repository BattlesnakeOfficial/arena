//! The play-host routing surface: old play.battlesnake.com URLs, redirected
//! to their arena equivalents for play hostnames
//! (`AppConfig::play_redirect_hosts`, default `play.battlesnake.com`).
//!
//! play.battlesnake.com's DNS moves to arena on 2026-11-30 (DEV-2353). Its
//! links are pinned in the docs blog, Discord, emails and READMEs, so every
//! play URL answers with a 307 to the matching arena page, or a bare 410 for
//! machine webhooks. Anything arena doesn't know lands on arena's branded 404,
//! on the canonical host.
//!
//! Contract:
//! * Never renders a page and never touches sessions: nothing here uses
//!   `PageFactory`, `OptionalUser` or `CurrentSession`. A page served on the
//!   play host would be broken anyway: the session cookie is host-only
//!   (`__Host-session_id`), so everyone would look logged out; the GitHub
//!   callback is fixed to the arena host, so a login started here would fail
//!   its state check; and each anonymous render would create a `sessions` row.
//! * Every Location is absolute (`{base_url}{path}`), so even `//evil.example`
//!   stays on arena. Redirects are 307, temporary like `redirects.rs`, so arena
//!   can still move its own URLs. 307 keeps the method, so stray POSTs land on
//!   arena's 404/405.
//! * Path segments are read raw (still percent-encoded) from the URI, never
//!   through axum's decoding extractors: a bad escape can't become a 400, and
//!   a decoded control character can't produce an invalid Location (a 500).
//! * Every route answers any method, with and without one trailing slash
//!   (play had `APPEND_SLASH=False`; arena normalises nothing).
//! * `/u/{username}` and `/profile/{username}` are not registered here: the
//!   fallback sends them to the site router's play_links handlers (#319).
//! * Host dispatch lives in `cja::server::App` (see `routes()`), before route
//!   matching, so the arena host never reaches this router.

use std::borrow::Cow;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, Uri};
use axum::response::Redirect;
use axum::routing::{MethodRouter, any};
use uuid::Uuid;

use crate::errors::{ServerResult, WithRedirect as _};
use crate::models::{imported_account, leaderboard, user};
use crate::routes::redirects::{LOCAL_REDIRECTS, REDIRECTS};
use crate::routes::welcome_back;
use crate::state::AppState;

/// Play ladder slugs and the arena leaderboard each became, by name: the
/// seeded ladders have random ids per database, so ids can't be hardcoded.
const PLAY_LADDERS: &[(&str, &str)] = &[
    ("standard", "Standard 11x11"),
    ("standard-duels", "Duels 11x11"),
    ("royale", "Royale 11x11"),
    ("constrictor", "Constrictor 11x11"),
];

/// Play pages that moved to one fixed place. The query is dropped: it held
/// play-only state (settings tabs, HTMX params) or secrets (password-reset
/// and email-verification tokens, OAuth `code`/`state`) that must not be
/// forwarded.
const MOVED: &[(&str, &str)] = &[
    // Accounts are GitHub sign-in plus a claim on arena; /welcome-back
    // explains moving a play account over.
    ("/login", welcome_back::PATH),
    ("/register", welcome_back::PATH),
    ("/recover", welcome_back::PATH),
    ("/recover/{uidb64}/{token}", welcome_back::PATH),
    ("/account/verify/{token}", welcome_back::PATH),
    ("/oauth/login/{backend}", welcome_back::PATH),
    ("/oauth/complete/{backend}", welcome_back::PATH),
    // There is no session on the play host to end.
    ("/logout", "/"),
    ("/account/battlesnakes", "/battlesnakes"),
    ("/account/games/create", "/games/new"),
    ("/account/settings", "/me"),
    ("/account/redeem", "/customizations"),
    ("/account/tournaments/create", "/tournaments/new"),
    (
        "/account/sponsor",
        "https://github.com/sponsors/BattlesnakeOfficial",
    ),
    // Older play pages, still linked from Discord history and READMEs.
    ("/account/dashboard", "/battlesnakes"),
    ("/account/snakes/create", "/battlesnakes/new"),
    ("/game/create", "/games/new"),
    ("/about/conduct", "/conduct"),
];

/// Where old play GIF links go: the exporter renders any game it can load.
const EXPORTER_GAMES_URL: &str = "https://exporter.battlesnake.com/games";

/// Build the play-host router (state applied). The fallback is registered
/// before the trace layer so it covers every path.
pub fn play_router(app_state: AppState) -> Router {
    let mut router = Router::new()
        .route("/", any(home))
        .route("/webhook/{*rest}", any(webhook_gone));
    let routes: [(&str, MethodRouter<AppState>); 12] = [
        ("/game/{id}", any(game)),
        ("/g/{id}", any(game)),
        ("/game/{id}/gif", any(game_gif)),
        ("/g/{id}/gif", any(game_gif)),
        ("/leaderboard/{slug}", any(ladder)),
        ("/leaderboard/{slug}/{username}/stats", any(ladder_stats)),
        ("/arena/{slug}", any(old_ladder)),
        ("/ladder/{slug}", any(old_ladder)),
        ("/league/{event}/arenas/{slug}", any(old_ladder)),
        ("/rankings", any(rankings)),
        ("/u/{username}/{snake_slug}", any(snake_by_author)),
        ("/s/{snake_id}", any(snake_by_id)),
    ];
    for (path, handler) in routes {
        router = route_both(router, path, handler);
    }
    // The catch-all above never matches an empty tail.
    router = route_both(router, "/webhook", any(webhook_gone));
    for (path, target) in MOVED {
        router = route_both(
            router,
            path,
            any(move |State(state): State<AppState>| async move { location(&state, target) }),
        );
    }
    router
        .fallback(same_path_on_arena)
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::routes::inject_trace_context,
        ))
        .with_state(app_state)
}

/// Register `path` and `path/`: play matched only the exact form it listed,
/// and links carry both.
fn route_both(
    router: Router<AppState>,
    path: &str,
    handler: MethodRouter<AppState>,
) -> Router<AppState> {
    router
        .route(path, handler.clone())
        .route(&format!("{path}/"), handler)
}

/// `/`: arena's welcome-back page is the landing page for play visitors (its
/// copy says play.battlesnake.com redirects there).
async fn home(State(state): State<AppState>, RawQuery(query): RawQuery) -> Redirect {
    to_arena(&state, welcome_back::PATH, query.as_deref())
}

/// `/game/{id}`, `/g/{id}`: arena's game page already resolves play engine
/// ids and reads play's `turn`/`autoplay`/`fps`/`title` params.
async fn game(State(state): State<AppState>, uri: Uri) -> Redirect {
    to_arena(&state, &format!("/games/{}", segment(&uri, 1)), uri.query())
}

/// `/game/{id}/gif`, `/g/{id}/gif`: old GIF links, often embedded as images.
async fn game_gif(uri: Uri) -> Redirect {
    Redirect::temporary(&format!("{EXPORTER_GAMES_URL}/{}/gif", segment(&uri, 1)))
}

/// `/leaderboard/{slug}`: a play ladder's arena leaderboard. Slugs play never
/// had take the fallback (arena's 404).
async fn ladder(State(state): State<AppState>, uri: Uri) -> ServerResult<Redirect, Redirect> {
    let Some(name) = ladder_name(segment(&uri, 1)) else {
        return Ok(fallback(&state, &uri));
    };
    let id = leaderboard::public_leaderboard_id_by_name(&state.db, name)
        .await
        .with_redirect(to_arena(&state, "/leaderboards", None))?;
    Ok(to_arena(&state, &leaderboard_path(id), None))
}

/// `/leaderboard/{slug}/{username}/stats`: the player's best active-season
/// entry on that ladder, else the ladder itself.
async fn ladder_stats(State(state): State<AppState>, uri: Uri) -> ServerResult<Redirect, Redirect> {
    let Some(name) = ladder_name(segment(&uri, 1)) else {
        return Ok(fallback(&state, &uri));
    };
    let Some(leaderboard_id) = leaderboard::public_leaderboard_id_by_name(&state.db, name)
        .await
        .with_redirect(to_arena(&state, "/leaderboards", None))?
    else {
        return Ok(to_arena(&state, "/leaderboards", None));
    };
    let ladder = leaderboard_path(Some(leaderboard_id));
    let entry = best_entry(&state, leaderboard_id, segment(&uri, 2))
        .await
        .with_redirect(to_arena(&state, &ladder, None))?;
    Ok(match entry {
        Some(entry_id) => to_arena(&state, &format!("{ladder}/entries/{entry_id}"), None),
        None => to_arena(&state, &ladder, None),
    })
}

/// The active-season entry of the play user's best snake on a leaderboard.
/// The user is whoever claimed that play username, else the arena user with
/// that GitHub login (as on `/u/{username}`).
async fn best_entry(
    state: &AppState,
    leaderboard_id: Uuid,
    raw_username: &str,
) -> cja::Result<Option<Uuid>> {
    let Some(username) = decoded(raw_username) else {
        return Ok(None);
    };
    let user_id = match imported_account::claimant_of_play_username(&state.db, &username).await? {
        Some((_, user_id)) => Some(user_id),
        None => user::get_user_by_github_login(&state.db, &username)
            .await?
            .map(|user| user.user_id),
    };
    let Some(user_id) = user_id else {
        return Ok(None);
    };
    let memberships = leaderboard::get_user_memberships(&state.db, leaderboard_id, user_id).await?;
    Ok(memberships
        .into_iter()
        .find_map(|membership| membership.leaderboard_entry_id))
}

/// `/arena/{slug}`, `/ladder/{slug}`, `/league/{event}/arenas/{slug}`: older
/// ladder pages. The slug is the last segment; unknown ones get the list.
async fn old_ladder(State(state): State<AppState>, uri: Uri) -> ServerResult<Redirect, Redirect> {
    let slug = uri.path().split('/').rfind(|s| !s.is_empty());
    let Some(name) = slug.and_then(ladder_name) else {
        return Ok(to_arena(&state, "/leaderboards", None));
    };
    let id = leaderboard::public_leaderboard_id_by_name(&state.db, name)
        .await
        .with_redirect(to_arena(&state, "/leaderboards", None))?;
    Ok(to_arena(&state, &leaderboard_path(id), None))
}

/// `/rankings`: play's `?page=N` counted from 1, arena's counts from 0.
async fn rankings(State(state): State<AppState>, RawQuery(query): RawQuery) -> Redirect {
    let page = query
        .as_deref()
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(key, _)| key == "page"))
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .filter(|page| *page >= 2);
    match page {
        Some(page) => to_arena(&state, "/rankings", Some(&format!("page={}", page - 1))),
        None => to_arena(&state, "/rankings", None),
    }
}

/// `/u/{username}/{snake_slug}`: play never stored snake slugs anywhere arena
/// can see, so send the author's profile (`/u/` resolves it, #319).
async fn snake_by_author(State(state): State<AppState>, uri: Uri) -> Redirect {
    to_arena(&state, &format!("/u/{}", segment(&uri, 1)), None)
}

/// `/s/{play_snake_id}`: the imported snake's arena page once claimed, else
/// its owner's profile. Ids that were never imported take the fallback.
async fn snake_by_id(State(state): State<AppState>, uri: Uri) -> ServerResult<Redirect, Redirect> {
    let Some(snake_id) = decoded(segment(&uri, 1)) else {
        return Ok(fallback(&state, &uri));
    };
    let Some(link) = imported_account::play_snake_link(&state.db, &snake_id)
        .await
        .with_redirect(fallback(&state, &uri))?
    else {
        return Ok(fallback(&state, &uri));
    };
    let path = match (link.materialized_battlesnake_id, link.claimant) {
        (Some(battlesnake_id), _) => format!("/battlesnakes/{battlesnake_id}/profile"),
        (None, Some((login, user_id))) => {
            format!("/users/{}/{user_id}", urlencoding::encode(&login))
        }
        (None, None) => format!("/u/{}", urlencoding::encode(&link.play_username)),
    };
    Ok(to_arena(&state, &path, None))
}

/// `/webhook/...` (any method): GitHub Sponsors, Mailgun, GCP alerts and the
/// legacy engine posted here. Arena handles none of them, and a redirect
/// would only make senders retry elsewhere, so: 410, no body, no Location.
async fn webhook_gone() -> StatusCode {
    StatusCode::GONE
}

async fn same_path_on_arena(State(state): State<AppState>, uri: Uri) -> Redirect {
    fallback(&state, &uri)
}

/// Everything else: the same path on arena, with one trailing `/` trimmed
/// (arena has no `/leaderboards/` or `/discord/`) and the query kept. If arena
/// serves that path the visitor lands on the right page; if not, they see
/// arena's branded 404 on the canonical host. Short links (`/discord`,
/// `/mission`, ...) skip the extra hop and go straight to their target.
fn fallback(state: &AppState, uri: &Uri) -> Redirect {
    let path = uri.path();
    let path = match path.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed,
        _ => path,
    };
    // Only origin-form paths start with '/'; anything else (`*`) goes home.
    let path = if path.starts_with('/') { path } else { "/" };
    if let Some((_, target)) = REDIRECTS.iter().find(|(link, _)| *link == path) {
        return with_query(target, uri.query());
    }
    if let Some((_, target)) = LOCAL_REDIRECTS.iter().find(|(link, _)| *link == path) {
        return to_arena(state, target, uri.query());
    }
    to_arena(state, path, uri.query())
}

/// A 307 to `path` on arena (absolute, on `base_url`).
fn to_arena(state: &AppState, path: &str, query: Option<&str>) -> Redirect {
    let base = state.config.base_url.trim_end_matches('/');
    with_query(&format!("{base}{path}"), query)
}

/// A 307 to a `MOVED` target (an arena path or an absolute external URL).
fn location(state: &AppState, target: &str) -> Redirect {
    if target.starts_with("https://") {
        Redirect::temporary(target)
    } else {
        to_arena(state, target, None)
    }
}

fn with_query(url: &str, query: Option<&str>) -> Redirect {
    match query.filter(|q| !q.is_empty()) {
        Some(q) => Redirect::temporary(&format!("{url}?{q}")),
        None => Redirect::temporary(url),
    }
}

fn ladder_name(slug: &str) -> Option<&'static str> {
    PLAY_LADDERS
        .iter()
        .find(|(play_slug, _)| *play_slug == slug)
        .map(|(_, name)| *name)
}

/// A ladder's page, or the leaderboard list when this database doesn't have
/// that ladder.
fn leaderboard_path(leaderboard_id: Option<Uuid>) -> String {
    leaderboard_id.map_or_else(
        || "/leaderboards".to_string(),
        |id| format!("/leaderboards/{id}"),
    )
}

/// The `index`th segment of the request path, still percent-encoded, so it is
/// always safe to put back into a Location.
fn segment(uri: &Uri, index: usize) -> &str {
    uri.path()
        .split('/')
        .filter(|s| !s.is_empty())
        .nth(index)
        .unwrap_or_default()
}

/// A raw segment percent-decoded for a DB lookup; `None` if it isn't UTF-8
/// or holds a NUL, which Postgres text can't store (the query would only
/// error).
fn decoded(raw: &str) -> Option<Cow<'_, str>> {
    urlencoding::decode(raw)
        .ok()
        .filter(|decoded| !decoded.contains('\0'))
}
