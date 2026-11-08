//! Public share pages for earned achievements: /achievements/{slug}/{user_id}.

use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use color_eyre::eyre::{Context as _, eyre};
use maud::html;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    arena_assets::asset_url,
    components::{avatar::user_avatar, page_factory::PageFactory},
    customizations::{achievements::Achievement, chip_color},
    errors::{ServerError, ServerResult},
    models::{
        battlesnake::{self, Battlesnake},
        game::get_game_by_id,
        game_battlesnake::get_battlesnakes_by_game_id,
        user::{User, get_user_by_id},
    },
    routes::auth::OptionalUser,
    state::AppState,
};

/// The card-visible core of an earn: everything except game context. The
/// social card uses only this, and it has no game id or opponent field, so
/// none can reach a card by construction.
pub(crate) struct EarnView {
    pub achievement: Achievement,
    pub owner: User,
    pub earned_at: chrono::DateTime<chrono::Utc>,
    /// The snake that earned it, when the earn recorded one and it still
    /// exists (soft-deleted snakes resolve to None).
    pub snake: Option<Battlesnake>,
}

/// The earn row's raw references, read only by the page loader to resolve
/// replay context. Kept off `EarnView` so a card can't reach them.
struct EarnRefs {
    battlesnake_id: Option<Uuid>,
    game_id: Option<Uuid>,
}

pub(crate) struct AchievementShareView {
    pub earn: EarnView,
    /// Replay context; only for earns with a game that exists and that no
    /// hiding route (private leaderboard link, private entry, non-public
    /// tournament match) covers.
    pub game: Option<PublicGame>,
    /// A rendered clip exists and its game is public: the page may show
    /// and link it. False for pending/failed/no-clip and every hidden-game
    /// case alike — the page can't tell them apart.
    pub clip_ready: bool,
}

pub(crate) struct PublicGame {
    pub game_id: Uuid,
    /// Display names of the opponents' snakes — never the owner's own — in
    /// placement order.
    pub opponents: Vec<String>,
}

/// The earn's core, or None when there is no such earned achievement.
pub(crate) async fn load_earn(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    achievement: Achievement,
) -> cja::Result<Option<EarnView>> {
    Ok(load_earn_row(pool, user_id, achievement)
        .await?
        .map(|(earn, _)| earn))
}

async fn load_earn_row(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    achievement: Achievement,
) -> cja::Result<Option<(EarnView, EarnRefs)>> {
    let earn = sqlx::query!(
        "SELECT earned_at, battlesnake_id, game_id
         FROM achievement_earns WHERE user_id = $1 AND achievement = $2",
        user_id,
        achievement.slug(),
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to read achievement earn")?;
    let Some(earn) = earn else { return Ok(None) };
    // Owner and snake are independent once the earn row is read — run them
    // together instead of paying two sequential round trips.
    let (owner, snake) = tokio::try_join!(get_user_by_id(pool, user_id), async {
        match earn.battlesnake_id {
            Some(id) => battlesnake::get_battlesnake_by_id(pool, id).await,
            None => Ok(None),
        }
    })
    .wrap_err("Failed to load share page owner and snake")?;
    let Some(owner) = owner else { return Ok(None) };
    Ok(Some((
        EarnView {
            achievement,
            owner,
            earned_at: earn.earned_at,
            snake,
        },
        EarnRefs {
            battlesnake_id: earn.battlesnake_id,
            game_id: earn.game_id,
        },
    )))
}

/// The share page's data, or None when there is no such earned achievement.
pub(crate) async fn load_share_view(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    achievement: Achievement,
) -> cja::Result<Option<AchievementShareView>> {
    let Some((earn, refs)) = load_earn_row(pool, user_id, achievement).await? else {
        return Ok(None);
    };
    let game = match refs.game_id {
        Some(game_id) => public_game(pool, game_id, user_id, refs.battlesnake_id).await?,
        None => None,
    };
    // Separate from load_earn_row on purpose: EarnView is shared with the
    // card path and stays card-safe; this costs one round trip only on
    // public-game pages.
    let clip_ready = match &game {
        Some(_) => {
            sqlx::query!(
                r#"SELECT EXISTS (
                SELECT 1 FROM achievement_clips ac
                WHERE ac.achievement_earn_id = (
                    SELECT achievement_earn_id FROM achievement_earns ae
                    WHERE ae.user_id = $1 AND ae.achievement = $2
                ) AND ac.status = 'rendered'
            ) AS "clip_ready!""#,
                user_id,
                achievement.slug(),
            )
            .fetch_one(pool)
            .await
            .wrap_err("Failed to check achievement clip")?
            .clip_ready
        }
        None => false,
    };
    Ok(Some(AchievementShareView {
        earn,
        game,
        clip_ready,
    }))
}

/// True when any hiding route covers this game: a private leaderboard
/// link, a participant entry, or a non-public tournament match.
/// Over-hiding is the safe direction. Shared by the page loader, the
/// clip render job, and the clip route; the sweep's candidate query
/// carries the same three clauses in SQL (pinned by test).
pub(crate) async fn game_is_hidden(pool: &sqlx::PgPool, game_id: Uuid) -> cja::Result<bool> {
    let hidden = sqlx::query!(
        r#"SELECT (
            EXISTS (
                SELECT 1 FROM leaderboard_games lg
                JOIN leaderboards l ON l.leaderboard_id = lg.leaderboard_id
                WHERE lg.game_id = $1 AND l.visibility = 'private'
            ) OR EXISTS (
                SELECT 1
                FROM game_battlesnakes gb
                JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
                JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
                WHERE gb.game_id = $1 AND l.visibility = 'private'
            ) OR EXISTS (
                SELECT 1
                FROM match_games mg
                JOIN tournament_matches tm ON tm.match_id = mg.match_id
                JOIN tournaments t ON t.tournament_id = tm.tournament_id
                WHERE mg.game_id = $1 AND t.visibility <> 'public'
            )
        ) AS "hidden!""#,
        game_id,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check game visibility")?
    .hidden;
    Ok(hidden)
}

/// The game's replay context, or None when the game is gone or any hiding
/// route covers it: a leaderboard link, a participant entry, or a
/// non-public tournament match. Over-hiding is the safe direction.
async fn public_game(
    pool: &sqlx::PgPool,
    game_id: Uuid,
    owner_id: Uuid,
    earning_snake_id: Option<Uuid>,
) -> cja::Result<Option<PublicGame>> {
    if get_game_by_id(pool, game_id)
        .await
        .wrap_err("Failed to get earn game")?
        .is_none()
    {
        return Ok(None);
    }
    let hidden = game_is_hidden(pool, game_id).await?;
    if hidden {
        return Ok(None);
    }
    let opponents = get_battlesnakes_by_game_id(pool, game_id)
        .await
        .wrap_err("Failed to get game participants")?
        .into_iter()
        // The owner's own snakes are not opponents, whoever earned the
        // achievement; the filter covers the earning snake too.
        .filter(|p| p.user_id != owner_id && Some(p.battlesnake_id) != earning_snake_id)
        .map(|p| p.name)
        .collect();
    Ok(Some(PublicGame { game_id, opponents }))
}

/// Lowercase fragments that mark a crawler or link-preview fetcher. A fetch
/// with no User-Agent at all also counts: real browsers always send one.
const CRAWLER_UA_FRAGMENTS: &[&str] = &[
    "bot",
    "crawl",
    "spider",
    "slurp",
    "facebookexternalhit",
    "whatsapp",
    "preview",
    "headless",
    "lighthouse",
    "pingdom",
    "cardyb",
    "bluesky",
    "mastodon",
    "pleroma",
    "http.rb",
    "iframely",
    "embedly",
    "curl",
    "wget",
    "python-",
    "go-http",
    "node-fetch",
    "axios",
    "okhttp",
];

fn is_crawler(headers: &HeaderMap) -> bool {
    // A prefetch is the same visitor's browser warming the link; the real
    // navigation is what counts, so prefetches are not views either.
    if headers
        .get("sec-purpose")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|purpose| purpose.to_ascii_lowercase().contains("prefetch"))
    {
        return true;
    }
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|ua| {
            let ua = ua.to_ascii_lowercase();
            CRAWLER_UA_FRAGMENTS
                .iter()
                .any(|fragment| ua.contains(fragment))
        })
}

/// "direct" (no Referer), "internal" (same host as the site), "external"
/// (anywhere else), "other" (unparseable).
fn referrer_class(headers: &HeaderMap, base_url: &str) -> &'static str {
    fn host(url: &str) -> Option<&str> {
        url.split_once("://")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .filter(|h| !h.is_empty())
    }
    let Some(referer) = headers
        .get(header::REFERER)
        .and_then(|value| value.to_str().ok())
    else {
        return "direct";
    };
    match (host(referer), host(base_url)) {
        (Some(from), Some(site)) if from.eq_ignore_ascii_case(site) => "internal",
        (Some(_), Some(_)) => "external",
        _ => "other",
    }
}

/// GET /achievements/{slug}/{user_id} — public page for one earned
/// achievement. Unknown achievements, malformed ids, and unearned URLs 404
/// like any other missing page.
pub async fn share_page(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path((slug, user_id)): Path<(String, String)>,
    page_factory: PageFactory,
    headers: HeaderMap,
) -> ServerResult<Response, StatusCode> {
    let Some(achievement) = Achievement::from_slug(&slug) else {
        return Ok(crate::routes::render_not_found(page_factory));
    };
    // Truncated or mangled share links land on the branded 404, not a
    // plain extractor rejection.
    let Ok(user_id) = Uuid::parse_str(&user_id) else {
        return Ok(crate::routes::render_not_found(page_factory));
    };
    let Some(view) = load_share_view(&state.db, user_id, achievement)
        .await
        .wrap_err("Failed to load achievement share view")?
    else {
        return Ok(crate::routes::render_not_found(page_factory));
    };
    let is_owner = viewer.as_ref().is_some_and(|v| v.user_id == user_id);
    // The view metric measures reach beyond the owner: owner fetches,
    // crawler fetchers, and prefetches all get the page but no event.
    if !is_owner && !is_crawler(&headers) {
        emit_page_view(
            achievement,
            user_id,
            referrer_class(&headers, &page_factory.base_url),
        );
    }

    let def = achievement.def();
    let owner_name = view.earn.owner.public_name().to_string();
    let earned = view.earn.earned_at.format("%b %-d, %Y").to_string();
    let card_path = crate::routes::og::achievement_card_path(achievement.slug(), user_id);
    let clip_path = share_clip_path(achievement.slug(), user_id);
    let description = match &view.earn.snake {
        Some(snake) => format!(
            "{owner_name} unlocked {} with {} in Battlesnake.",
            def.name, snake.name
        ),
        None => format!("{owner_name} unlocked {} in Battlesnake.", def.name),
    };
    let page_url = format!(
        "{}/achievements/{}/{}",
        page_factory.base_url.trim_end_matches('/'),
        achievement.slug(),
        user_id
    );

    let content = html! {
        div class="ach-share" data-share-page data-share-url=(page_url)
             data-share-base=(format!("/achievements/{}/{}", achievement.slug(), user_id)) {
            header class="ach-share-head" {
                div class="ach-share-art" {
                    img src=(def.head.image_url()) alt=(format!("{} head", def.head.def().display_name)) width="88" height="88";
                    @if let Some(tail) = def.tail {
                        img src=(tail.image_url()) alt=(format!("{} tail", tail.def().display_name)) width="88" height="88";
                    }
                }
                div {
                    span class="badge ok" { "Unlocked" }
                    h1 { (def.name) }
                    p class="ach-share-description" { (def.description) }
                }
            }
            div class="ach-share-owner" {
                (user_avatar(view.earn.owner.github_avatar_url.as_deref(), &view.earn.owner.github_login, "avatar"))
                div {
                    a class="name" href=(format!("/users/{}/{}", view.earn.owner.github_login, user_id)) { (owner_name) }
                    div class="meta" { "Unlocked " (earned) }
                }
            }
            @if let Some(snake) = &view.earn.snake {
                p class="ach-share-snake" {
                    span class="chip" style={"background:" (chip_color(&snake.color))} {}
                    span class="ach-share-snake-name" { "Earned by " (snake.name) }
                }
            }
            @if let Some(game) = &view.game {
                p class="ach-share-game" {
                    @if game.opponents.is_empty() {
                        "Earned in a public game. "
                    } @else {
                        "Earned in a game against " (opponent_list(&game.opponents)) ". "
                    }
                    a href=(format!("/games/{}", game.game_id)) { "Watch the replay" }
                }
            }
            // The stored clip, streamed as bytes by share_clip — never
            // rendered on request. Shown only when a rendered clip exists
            // AND the game is public (clip_ready folds both together).
            @if view.clip_ready {
                figure class="ach-share-clip" {
                    img src=(clip_path) alt=(format!("Replay clip of the game that earned {}", def.name))
                        width="504" height="504" loading="lazy";
                    @if is_owner {
                        a class="btn" href=(clip_path)
                           download=(format!("{}-clip.gif", achievement.slug()))
                           data-share-download { "Download Clip" }
                    }
                }
            }
            @if is_owner {
                div class="share-actions" {
                    button #share-native class="btn" type="button" hidden { "Share" }
                    input #share-url type="text" readonly value=(page_url) aria-label="Shareable achievement link";
                    button #share-copy class="btn sm" type="button" { "Copy Link" }
                    a #share-download class="btn" href=(card_path) data-share-download
                       download=(format!("{}-{}.png", achievement.slug(), view.earn.owner.github_login)) { "Download Card" }
                }
            } @else {
                div class="share-cta" {
                    a class="btn solid" href="/customizations/achievements" data-share-cta="achievements" { "See all achievements" }
                    @if viewer.is_none() {
                        a class="btn" href="/auth/github" data-share-cta="get_started" { "Get started on Battlesnake" }
                    }
                }
            }
            // Served for owners and visitors alike: the visitor CTA links are
            // wired by the same entry as the share buttons.
            script src=(asset_url("share-card.js")) {}
        }
    };

    Ok(page_factory
        .create_page(format!("{} · {}", def.name, owner_name), Box::new(content))
        .with_description(description)
        .with_social_image(
            card_path,
            format!("{} unlocked by {}", def.name, owner_name),
        )
        .into_response())
}

/// The stored-clip URL for one earned achievement. Earn-keyed: no game id.
pub(crate) fn share_clip_path(slug: &str, user_id: Uuid) -> String {
    format!("/achievements/{slug}/{user_id}/clip.gif")
}

/// Up to three opponent names, then "and N more" for longer rosters.
fn opponent_list(opponents: &[String]) -> String {
    match opponents.len() {
        0 => String::new(),
        1 => opponents[0].clone(),
        2 => format!("{} and {}", opponents[0], opponents[1]),
        3 => format!("{}, {} and {}", opponents[0], opponents[1], opponents[2]),
        _ => format!(
            "{}, {}, {} and {} more",
            opponents[0],
            opponents[1],
            opponents[2],
            opponents.len() - 3
        ),
    }
}

fn emit_page_view(achievement: Achievement, owner_id: Uuid, referrer: &str) {
    tracing::info!(
        event_type = "share_page_view",
        slug = achievement.slug(),
        owner_id = %owner_id,
        referrer,
        "Achievement share page viewed"
    );
}

#[derive(Deserialize)]
pub struct ShareActionForm {
    pub channel: String,
}

/// POST /achievements/{slug}/{user_id}/share-action — record a share
/// action from the page's share buttons. Returns 204; callers are JS.
pub async fn share_action(
    State(state): State<AppState>,
    Path((slug, user_id)): Path<(String, String)>,
    Form(form): Form<ShareActionForm>,
) -> ServerResult<StatusCode, StatusCode> {
    let (achievement, user_id) = valid_share_target(&slug, &user_id, &state.db).await?;
    // An unknown channel is client junk, not a server error: a bare 400
    // without the ERROR-level logging a ServerError would do.
    if !matches!(form.channel.as_str(), "native" | "copy" | "download") {
        return Ok(StatusCode::BAD_REQUEST);
    }
    tracing::info!(
        event_type = "share_action",
        slug = achievement.slug(),
        owner_id = %user_id,
        channel = %form.channel,
        "Achievement share action"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ShareCtaForm {
    pub cta: String,
}

/// POST /achievements/{slug}/{user_id}/cta-click — record a visitor CTA
/// click. Returns 204; callers are JS.
pub async fn share_cta_click(
    State(state): State<AppState>,
    Path((slug, user_id)): Path<(String, String)>,
    Form(form): Form<ShareCtaForm>,
) -> ServerResult<StatusCode, StatusCode> {
    let (achievement, user_id) = valid_share_target(&slug, &user_id, &state.db).await?;
    if !matches!(form.cta.as_str(), "achievements" | "get_started") {
        return Ok(StatusCode::BAD_REQUEST);
    }
    tracing::info!(
        event_type = "share_cta_click",
        slug = achievement.slug(),
        owner_id = %user_id,
        cta = %form.cta,
        "Achievement share CTA click"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The slug names a real achievement this user has earned, parsed from the
/// path (else a branded 404 — a malformed id must not leak a plain 400).
async fn valid_share_target(
    slug: &str,
    user_id: &str,
    pool: &sqlx::PgPool,
) -> ServerResult<(Achievement, Uuid), StatusCode> {
    let Some(achievement) = Achievement::from_slug(slug) else {
        return Err(not_found_error());
    };
    let Ok(user_id) = Uuid::parse_str(user_id) else {
        return Err(not_found_error());
    };
    let exists = sqlx::query!(
        r#"SELECT EXISTS (
            SELECT 1 FROM achievement_earns
            WHERE user_id = $1 AND achievement = $2
        ) AS "exists!""#,
        user_id,
        achievement.slug(),
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check achievement earn")?
    .exists;
    if exists {
        Ok((achievement, user_id))
    } else {
        Err(not_found_error())
    }
}

/// GET /achievements/{slug}/{user_id}/clip.gif — the stored clip for one
/// earned achievement. Streams stored bytes only: no render, no exporter.
/// Unknown slug/user, missing/pending/failed clips, and games that are
/// gone or not public all 404 alike, with no body differences.
pub async fn share_clip(
    State(state): State<AppState>,
    Path((slug, user_id)): Path<(String, String)>,
) -> ServerResult<Response, StatusCode> {
    let Some(achievement) = Achievement::from_slug(&slug) else {
        return Err(not_found_error());
    };
    let Ok(user_id) = Uuid::parse_str(&user_id) else {
        return Err(not_found_error());
    };
    let earn = sqlx::query!(
        "SELECT achievement_earn_id, game_id
         FROM achievement_earns WHERE user_id = $1 AND achievement = $2",
        user_id,
        achievement.slug(),
    )
    .fetch_optional(&state.db)
    .await
    .wrap_err("Failed to read achievement earn for clip")?
    .ok_or_else(not_found_error)?;
    let Some(game_id) = earn.game_id else {
        return Err(not_found_error());
    };
    let clip = sqlx::query!(
        "SELECT 1 AS one FROM achievement_clips
         WHERE achievement_earn_id = $1 AND status = 'rendered'",
        earn.achievement_earn_id,
    )
    .fetch_optional(&state.db)
    .await
    .wrap_err("Failed to read achievement clip row")?;
    if clip.is_none() {
        return Err(not_found_error());
    }
    // Serve-time privacy re-check. No existence check is needed: the FK
    // sets game_id NULL when the game row is deleted, so a non-NULL id
    // always references an existing game.
    if game_is_hidden(&state.db, game_id).await? {
        return Err(not_found_error());
    }
    let path = crate::achievement_clips::clip_object_path(earn.achievement_earn_id);
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.object_store.get(&path),
    )
    .await
    .wrap_err("Clip object fetch timed out")?
    .wrap_err_with(|| format!("Failed to fetch achievement clip object at {path}"))?
    .ok_or_else(|| {
        // ServerError with NOT_FOUND logs at debug only — say it loudly.
        tracing::error!(%path, "Rendered clip object missing from bucket");
        ServerError(
            eyre!("Rendered clip object missing at {path}"),
            StatusCode::NOT_FOUND,
        )
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/gif".to_string()),
            (header::CACHE_CONTROL, "private, max-age=300".to_string()),
        ],
        bytes,
    )
        .into_response())
}

fn not_found_error() -> ServerError<StatusCode> {
    ServerError(
        eyre!("Achievement share page not found"),
        StatusCode::NOT_FOUND,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use tower::ServiceExt as _;

    use crate::routes::test_support::signed_session_cookie;

    /// A real-browser UA: every GET that should record a view sends it.
    const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

    /// The fixed earn date `seed_earn` writes, as the page renders it.
    const EARNED_ON: &str = "Oct 31, 2026";

    async fn user(pool: &PgPool, github_id: i64) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES ($1, $2, '') RETURNING user_id",
        )
        .bind(github_id)
        .bind(format!("share-{github_id}"))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn snake(pool: &PgPool, owner: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
        )
        .bind(owner)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn game(pool: &PgPool, status: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO games (board_size, game_type, status, finished_at) VALUES ('11x11', 'Standard', $1, CASE WHEN $1 = 'finished' THEN NOW() ELSE NULL END) RETURNING game_id",
        )
        .bind(status)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn participant(
        pool: &PgPool,
        game_id: Uuid,
        snake_id: Uuid,
        placement: i32,
        entry: Option<Uuid>,
    ) {
        sqlx::query(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement, leaderboard_entry_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(game_id)
        .bind(snake_id)
        .bind(placement)
        .bind(entry)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A raw earn row at a fixed date, so date assertions are stable.
    async fn seed_earn(
        pool: &PgPool,
        user_id: Uuid,
        slug: &str,
        snake: Option<Uuid>,
        game: Option<Uuid>,
        live: bool,
    ) {
        sqlx::query(
            "INSERT INTO achievement_earns (user_id, achievement, battlesnake_id, game_id, live, earned_at)
             VALUES ($1, $2, $3, $4, $5, TIMESTAMPTZ '2026-10-31 12:00:00+00')",
        )
        .bind(user_id)
        .bind(slug)
        .bind(snake)
        .bind(game)
        .bind(live)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A signed session cookie for an existing user, minted with the same
    /// state the requests will hit.
    async fn session_cookie(state: &AppState, pool: &sqlx::PgPool, user_id: Uuid) -> String {
        let session_id: Uuid =
            sqlx::query_scalar("INSERT INTO sessions (user_id) VALUES ($1) RETURNING session_id")
                .bind(user_id)
                .fetch_one(pool)
                .await
                .unwrap();
        format!(
            "{}={}",
            crate::models::session::SESSION_COOKIE_NAME,
            signed_session_cookie(state, session_id)
        )
    }

    struct Reply {
        status: StatusCode,
        body: String,
    }

    async fn request(
        state: &AppState,
        method: &str,
        uri: &str,
        headers: &[(&'static str, &str)],
        form_body: Option<&str>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match form_body {
            Some(body) => builder
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router()
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(state: &AppState, uri: &str, headers: &[(&'static str, &str)]) -> Reply {
        request(state, "GET", uri, headers, None).await
    }

    /// Share-page views recorded for this owner (the capture buffer is
    /// process-global; ids are unique per test).
    fn page_views(owner_id: Uuid) -> Vec<std::collections::BTreeMap<String, String>> {
        crate::test_capture::Capture::shared().events_with_field(
            "share_page_view",
            "owner_id",
            &owner_id.to_string(),
        )
    }

    fn browser() -> Vec<(&'static str, &'static str)> {
        vec![(axum::http::header::USER_AGENT.as_str(), BROWSER_UA)]
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn opponent_list_shows_three_names_then_counts_the_rest() {
        assert_eq!(opponent_list(&[]), "");
        assert_eq!(opponent_list(&names(&["A"])), "A");
        assert_eq!(opponent_list(&names(&["A", "B"])), "A and B");
        assert_eq!(opponent_list(&names(&["A", "B", "C"])), "A, B and C");
        assert_eq!(
            opponent_list(&names(&["A", "B", "C", "D"])),
            "A, B, C and 1 more"
        );
        assert_eq!(
            opponent_list(&names(&["A", "B", "C", "D", "E"])),
            "A, B, C and 2 more"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unknown_slug_user_or_earn_404s(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120001).await;
        // Holds the First Win head through a token grant without ever
        // earning it: the share page keys off earn rows, not grants.
        let token_holder = user(&pool, 2120002).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'frog', 'token')",
        )
        .bind(token_holder)
        .execute(&pool)
        .await
        .unwrap();

        // Pages render the branded 404, not a plain framework rejection.
        for uri in [
            format!("/achievements/nope/{owner}"),
            format!("/achievements/first_win/{}", Uuid::new_v4()),
            format!("/achievements/first_win/{owner}"),
            format!("/achievements/first_win/{token_holder}"),
            "/achievements/first_win/not-a-uuid".to_string(),
        ] {
            let reply = get(&state, &uri, &[]).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
            assert!(
                reply.body.contains("404"),
                "{uri} should be the branded 404 page"
            );
        }
        // Cards just 404.
        for uri in [
            format!("/og/achievements/nope/{owner}.png"),
            format!("/og/achievements/first_win/{token_holder}.png"),
        ] {
            let reply = get(&state, &uri, &[]).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfilled_earn_page_looks_finished(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120101).await;
        // A backfilled earn: no game, snake, or turn.
        seed_earn(&pool, owner, "first_win", None, None, false).await;

        let reply = get(
            &state,
            &format!("/achievements/first_win/{owner}"),
            &browser(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("First Win"));
        assert!(reply.body.contains("Unlocked"));
        assert!(reply.body.contains("share-2120101"), "owner's public name");
        assert!(reply.body.contains(EARNED_ON));
        assert!(
            reply
                .body
                .contains(&format!("/og/achievements/first_win/{owner}.png"))
        );
        assert!(!reply.body.contains("/games/"), "no game to link");
        // A visitor sees CTAs, not the owner's share actions.
        assert!(reply.body.contains("/customizations/achievements"));
        assert!(reply.body.contains("/auth/github"));
        assert!(!reply.body.contains("share-actions"));

        let views = page_views(owner);
        assert_eq!(views.len(), 1, "one anonymous visitor view");
        assert_eq!(views[0].get("slug").map(String::as_str), Some("first_win"));
        assert_eq!(views[0].get("referrer").map(String::as_str), Some("direct"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn public_game_earn_links_replay(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let winner = user(&pool, 2120201).await;
        let loser = user(&pool, 2120202).await;
        let winner_snake = snake(&pool, winner, "public game winner").await;
        let loser_snake = snake(&pool, loser, "public game loser").await;
        // A second snake belonging to the winner: their own snake is
        // never an opponent, so its name must not appear on the page.
        let winner_second_snake = snake(&pool, winner, "winner second entry").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, winner_snake, 1, None).await;
        participant(&pool, game_id, loser_snake, 2, None).await;
        participant(&pool, game_id, winner_second_snake, 3, None).await;
        seed_earn(
            &pool,
            winner,
            "first_win",
            Some(winner_snake),
            Some(game_id),
            true,
        )
        .await;

        let uri = format!("/achievements/first_win/{winner}");
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains(&format!("href=\"/games/{game_id}\"")));
        assert!(reply.body.contains("Watch the replay"));
        assert!(reply.body.contains("public game loser"));
        assert!(
            !reply.body.contains("winner second entry"),
            "the owner's other snake is not an opponent"
        );

        // Snake colors are untrusted: a hostile value never reaches the
        // inline style, and the chip falls back to neutral gray.
        let hostile = "red;background-image:url(//example.test/x)";
        sqlx::query("UPDATE battlesnakes SET color = $1 WHERE battlesnake_id = $2")
            .bind(hostile)
            .bind(winner_snake)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains(hostile));
        assert!(reply.body.contains("background:#888888"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn hidden_game_earn_leaks_nothing(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120301).await;
        let rival = user(&pool, 2120302).await;
        let owner_snake = snake(&pool, owner, "private lb owner").await;
        let rival_snake = snake(&pool, rival, "Private Opponent").await;
        let game_id = game(&pool, "finished").await;
        let private_lb = crate::models::private_leaderboard::create(
            &pool,
            &crate::models::private_leaderboard::CreatePrivateLeaderboard {
                name: "Hidden".to_string(),
                game_type: "Standard".to_string(),
                board_size: "11x11".to_string(),
                match_size: 2,
            },
        )
        .await
        .unwrap();
        let entry = crate::models::leaderboard::get_or_create_entry(&pool, private_lb, rival_snake)
            .await
            .unwrap()
            .expect("live snake joins")
            .leaderboard_entry_id;
        participant(&pool, game_id, owner_snake, 1, None).await;
        participant(&pool, game_id, rival_snake, 2, Some(entry)).await;
        crate::models::leaderboard::create_leaderboard_game(&pool, private_lb, game_id)
            .await
            .unwrap();
        seed_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            true,
        )
        .await;

        let uri = format!("/achievements/first_win/{owner}");
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains(&game_id.to_string()), "game id leaked");
        assert!(!reply.body.contains("/games/"), "game link leaked");
        assert!(
            !reply.body.contains("Private Opponent"),
            "opponent name leaked"
        );

        // Control 1: with the leaderboard public, the replay link appears —
        // proving the check above can fail.
        sqlx::query("UPDATE leaderboards SET visibility = 'public', invite_code = NULL WHERE leaderboard_id = $1")
            .bind(private_lb)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains(&format!("href=\"/games/{game_id}\"")));

        // Control 2: private again, participants gone — the leaderboard_games
        // link alone still hides the game.
        sqlx::query("UPDATE leaderboards SET visibility = 'private', invite_code = 'back-to-private' WHERE leaderboard_id = $1")
            .bind(private_lb)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM game_battlesnakes WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains("/games/"));

        // Control 3: participants back, the rival's row carrying the private
        // leaderboard entry, and the leaderboard_games link deleted — the
        // entry route alone still hides the game.
        participant(&pool, game_id, owner_snake, 1, None).await;
        participant(&pool, game_id, rival_snake, 2, Some(entry)).await;
        sqlx::query("DELETE FROM leaderboard_games WHERE game_id = $1")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains(&game_id.to_string()), "game id leaked");
        assert!(!reply.body.contains("/games/"), "game link leaked");
        assert!(
            !reply.body.contains("Private Opponent"),
            "opponent name leaked"
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn participants_only_tournament_earn_leaks_nothing(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120401).await;
        let rival = user(&pool, 2120402).await;
        let owner_snake = snake(&pool, owner, "tournament earn owner").await;
        let rival_snake = snake(&pool, rival, "tournament earn rival").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1, None).await;
        participant(&pool, game_id, rival_snake, 2, None).await;
        let tournament_id: Uuid = sqlx::query_scalar(
            "INSERT INTO tournaments (user_id, name, visibility) VALUES ($1, 'Hidden tournament', 'participants_only') RETURNING tournament_id",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        let match_id: Uuid = sqlx::query_scalar(
            "INSERT INTO tournament_matches (tournament_id, round, position, visual_column, visual_row) VALUES ($1, 1, 1, 0, 0) RETURNING match_id",
        )
        .bind(tournament_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO match_games (match_id, game_id, game_number) VALUES ($1, $2, 1)")
            .bind(match_id)
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        seed_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            true,
        )
        .await;

        let uri = format!("/achievements/first_win/{owner}");
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains("/games/"), "game link leaked");
        assert!(
            !reply.body.contains("tournament earn rival"),
            "opponent name leaked"
        );

        // Control: a public tournament match links the replay.
        sqlx::query("UPDATE tournaments SET visibility = 'public' WHERE tournament_id = $1")
            .bind(tournament_id)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains(&format!("href=\"/games/{game_id}\"")));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn deleted_snake_and_game_look_like_backfill(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120501).await;
        let owner_snake = snake(&pool, owner, "soon deleted snake").await;
        let game_id = game(&pool, "finished").await;
        participant(&pool, game_id, owner_snake, 1, None).await;
        seed_earn(
            &pool,
            owner,
            "first_win",
            Some(owner_snake),
            Some(game_id),
            true,
        )
        .await;

        // A soft-deleted snake reads as no snake: the page stays finished.
        sqlx::query("UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1")
            .bind(owner_snake)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(
            &state,
            &format!("/achievements/first_win/{owner}"),
            &browser(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("First Win"));
        assert!(!reply.body.contains("soon deleted snake"));
        let earn = load_earn(&state.db, owner, Achievement::FirstWin)
            .await
            .unwrap()
            .expect("earn survives a soft-deleted snake");
        assert!(earn.snake.is_none());

        // A hard-deleted game rewrites the earn's game_id to NULL: the
        // snake line stays, the game line goes.
        let survivor_snake = snake(&pool, owner, "surviving snake").await;
        let game2 = game(&pool, "finished").await;
        participant(&pool, game2, survivor_snake, 1, None).await;
        seed_earn(
            &pool,
            owner,
            "crowd_pleaser",
            Some(survivor_snake),
            Some(game2),
            true,
        )
        .await;
        sqlx::query("DELETE FROM games WHERE game_id = $1")
            .bind(game2)
            .execute(&pool)
            .await
            .unwrap();
        let reply = get(
            &state,
            &format!("/achievements/crowd_pleaser/{owner}"),
            &browser(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("surviving snake"));
        assert!(!reply.body.contains("/games/"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn owner_sees_share_actions_visitor_sees_cta(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120601).await;
        let other = user(&pool, 2120602).await;
        seed_earn(&pool, owner, "first_win", None, None, false).await;
        let uri = format!("/achievements/first_win/{owner}");

        // Owner: share actions, no CTAs, and no view of their own page.
        let cookie = session_cookie(&state, &pool, owner).await;
        let reply = get(
            &state,
            &uri,
            &[
                (axum::http::header::USER_AGENT.as_str(), BROWSER_UA),
                (axum::http::header::COOKIE.as_str(), cookie.as_str()),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("share-actions"));
        assert!(reply.body.contains("id=\"share-url\""));
        assert!(reply.body.contains(&format!("http://localhost:3000{uri}")));
        assert!(
            reply
                .body
                .contains(&format!("/og/achievements/first_win/{owner}.png"))
        );
        assert!(reply.body.contains("id=\"share-download\""));
        assert!(reply.body.contains("id=\"share-native\""));
        assert!(
            reply
                .body
                .contains(r#"<button id="share-native" class="btn" type="button" hidden>"#),
            "native share starts hidden"
        );
        assert!(!reply.body.contains("share-cta"));
        assert!(!reply.body.contains("Get started"));
        assert_eq!(page_views(owner).len(), 0, "owner self-view is not a view");

        // Anonymous visitor: CTAs including Get started, no share actions.
        let reply = get(&state, &uri, &browser()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("share-cta"));
        assert!(reply.body.contains("Get started on Battlesnake"));
        assert!(!reply.body.contains("share-actions"));
        assert_eq!(page_views(owner).len(), 1);

        // Signed-in other player: achievements CTA only.
        let cookie = session_cookie(&state, &pool, other).await;
        let reply = get(
            &state,
            &uri,
            &[
                (axum::http::header::USER_AGENT.as_str(), BROWSER_UA),
                (axum::http::header::COOKIE.as_str(), cookie.as_str()),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(reply.body.contains("share-cta"));
        assert!(reply.body.contains("See all achievements"));
        assert!(!reply.body.contains("Get started"));
        assert!(!reply.body.contains("share-actions"));
        assert_eq!(page_views(owner).len(), 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn crawler_fetches_do_not_record_views(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());

        // Each fetcher gets the full page but no view event.
        let crawler = user(&pool, 2120701).await;
        seed_earn(&pool, crawler, "first_win", None, None, false).await;
        let crawler_ua = [
            "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
            "facebookexternalhit/1.1",
            "Bluesky Cardyb/1.1",
            "http.rb/4.4.1 (Mastodon/4.2.1; +https://mastodon.social)",
            "Iframely/1.0",
            "curl/8.5.0",
            "python-requests/2.31.0",
            "Go-http-client/2.0",
        ];
        for ua in crawler_ua {
            let reply = get(
                &state,
                &format!("/achievements/first_win/{crawler}"),
                &[(axum::http::header::USER_AGENT.as_str(), ua)],
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK, "{ua}");
            assert!(reply.body.contains("First Win"), "{ua} still gets the page");
        }
        // A missing UA counts as a crawler too.
        let reply = get(&state, &format!("/achievements/first_win/{crawler}"), &[]).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(page_views(crawler).len(), 0, "no crawler view events");

        // Control: a real browser UA does record exactly one view.
        let human = user(&pool, 2120702).await;
        seed_earn(&pool, human, "first_win", None, None, false).await;
        let reply = get(
            &state,
            &format!("/achievements/first_win/{human}"),
            &browser(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(page_views(human).len(), 1);

        // A prefetch warms the same link and is not the real navigation.
        let prefetcher = user(&pool, 2120703).await;
        seed_earn(&pool, prefetcher, "first_win", None, None, false).await;
        let reply = get(
            &state,
            &format!("/achievements/first_win/{prefetcher}"),
            &[
                (axum::http::header::USER_AGENT.as_str(), BROWSER_UA),
                ("sec-purpose", "prefetch"),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(page_views(prefetcher).len(), 0);

        // Referrer classes: internal (same host) vs external.
        let internal = user(&pool, 2120704).await;
        seed_earn(&pool, internal, "first_win", None, None, false).await;
        let reply = get(
            &state,
            &format!("/achievements/first_win/{internal}"),
            &[
                (axum::http::header::USER_AGENT.as_str(), BROWSER_UA),
                (
                    axum::http::header::REFERER.as_str(),
                    "http://localhost:3000/leaderboards",
                ),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let views = page_views(internal);
        assert_eq!(views.len(), 1);
        assert_eq!(
            views[0].get("referrer").map(String::as_str),
            Some("internal")
        );

        let external = user(&pool, 2120705).await;
        seed_earn(&pool, external, "first_win", None, None, false).await;
        let reply = get(
            &state,
            &format!("/achievements/first_win/{external}"),
            &[
                (axum::http::header::USER_AGENT.as_str(), BROWSER_UA),
                (
                    axum::http::header::REFERER.as_str(),
                    "https://elsewhere.example/x",
                ),
            ],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let views = page_views(external);
        assert_eq!(views.len(), 1);
        assert_eq!(
            views[0].get("referrer").map(String::as_str),
            Some("external")
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn share_action_and_cta_click_are_posts(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let state = AppState::test_from_pool(pool.clone());
        let owner = user(&pool, 2120801).await;
        seed_earn(&pool, owner, "first_win", None, None, false).await;
        let base = format!("/achievements/first_win/{owner}");

        for channel in ["native", "copy", "download"] {
            let reply = request(
                &state,
                "POST",
                &format!("{base}/share-action"),
                &[],
                Some(&format!("channel={channel}")),
            )
            .await;
            assert_eq!(reply.status, StatusCode::NO_CONTENT, "{channel}");
            let actions = crate::test_capture::Capture::shared().events_with_field(
                "share_action",
                "owner_id",
                &owner.to_string(),
            );
            assert!(
                actions
                    .iter()
                    .any(|fields| fields.get("channel").map(String::as_str) == Some(channel)),
                "{channel} recorded"
            );
        }
        let reply = request(
            &state,
            "POST",
            &format!("{base}/share-action"),
            &[],
            Some("channel=bogus"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        // Unknown earn, malformed id, and GET all fail.
        let stranger = user(&pool, 2120802).await;
        let reply = request(
            &state,
            "POST",
            &format!("/achievements/first_win/{stranger}/share-action"),
            &[],
            Some("channel=copy"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = request(
            &state,
            "POST",
            "/achievements/first_win/not-a-uuid/share-action",
            &[],
            Some("channel=copy"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = get(&state, &format!("{base}/share-action"), &[]).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);

        for cta in ["achievements", "get_started"] {
            let reply = request(
                &state,
                "POST",
                &format!("{base}/cta-click"),
                &[],
                Some(&format!("cta={cta}")),
            )
            .await;
            assert_eq!(reply.status, StatusCode::NO_CONTENT, "{cta}");
            let clicks = crate::test_capture::Capture::shared().events_with_field(
                "share_cta_click",
                "owner_id",
                &owner.to_string(),
            );
            assert!(
                clicks
                    .iter()
                    .any(|fields| fields.get("cta").map(String::as_str) == Some(cta)),
                "{cta} recorded"
            );
        }
        let reply = request(
            &state,
            "POST",
            &format!("{base}/cta-click"),
            &[],
            Some("cta=bogus"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let reply = get(&state, &format!("{base}/cta-click"), &[]).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    }
}
