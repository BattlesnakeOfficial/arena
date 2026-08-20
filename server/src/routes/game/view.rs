use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use axum_macros::debug_handler;
use color_eyre::eyre::Context as _;
use maud::{PreEscaped, html};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::game::{GameStatus, GameType},
    models::game_battlesnake,
    models::saved_game,
    models::turn::{SoloGameStats, get_solo_game_stats},
    routes::UuidPath,
    routes::auth::OptionalUser,
    state::AppState,
};

/// "Copy Link" button behavior for the share panel. Rendered via
/// [`PreEscaped`] because maud HTML-escapes string content inside `script {}`
/// blocks — without it, the `&&` below serializes as `&amp;&amp;`, which is a
/// JavaScript syntax error that silently kills the whole handler (the button
/// then does nothing). Keep any JS with `&`, `<`, `>` or `"` in a constant
/// like this, not inline maud strings.
const SHARE_COPY_JS: &str = r#"(function() {
    var btn = document.getElementById('share-copy');
    var input = document.getElementById('share-url');
    function done() {
        btn.textContent = 'Copied!';
        setTimeout(function() { btn.textContent = 'Copy Link'; }, 1500);
    }
    function fallback() {
        input.focus();
        input.select();
        try { document.execCommand('copy'); done(); } catch (e) {}
    }
    btn.addEventListener('click', function() {
        if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(input.value).then(done, fallback);
        } else {
            fallback();
        }
    });
})();"#;

/// Syncs the board iframe's `theme` param with the theme the pre-paint
/// bootstrap actually resolved (`data-app-theme` on <html>). The server can
/// only see signed-in preferences; an anonymous visitor who toggled the
/// theater to light lives in localStorage, which only the bootstrap sees.
/// Runs synchronously right after the iframe tag, so a rewrite lands before
/// the board app boots; when server and client agree (the common case) the
/// src is untouched and no reload happens. PreEscaped: contains `&&`.
const BOARD_THEME_SYNC_JS: &str = r#"(function() {
    var frame = document.getElementById('board-viewer');
    var theme = document.documentElement.getAttribute('data-app-theme');
    if (!frame || !theme) return;
    if (frame.src.indexOf('theme=' + theme) === -1) {
        frame.src = frame.src.replace(/theme=[a-z]+/, 'theme=' + theme);
    }
})();"#;

/// Optional viewer params forwarded to the board.battlesnake.com iframe so
/// shared links can jump to a turn, autoplay, etc. Only params that were
/// actually provided are passed through.
#[derive(Debug, Default, Deserialize)]
pub struct BoardParams {
    turn: Option<u32>,
    // Bool-ish: play accepted autoplay=true/1/etc. Kept as a string so
    // `?autoplay` variants don't 400 the whole page; normalized on output.
    autoplay: Option<String>,
    fps: Option<u32>,
    title: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ViewGameParams {
    /// Opt-in winner reveal for social embeds: a bare `?showSpoilers` or any
    /// value other than false/0/no/off counts as on. String (not bool) so a
    /// bare param doesn't 400 the whole page.
    #[serde(rename = "showSpoilers")]
    show_spoilers: Option<String>,
}

impl ViewGameParams {
    fn show_spoilers(&self) -> bool {
        match self.show_spoilers.as_deref() {
            None => false,
            Some(v) => !matches!(
                v.to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            ),
        }
    }
}

// Display game details in the game theater (themed by the theater axis)
#[debug_handler]
pub async fn view_game(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    UuidPath(game_id): UuidPath,
    Query(board_params): Query<BoardParams>,
    Query(params): Query<ViewGameParams>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Get the game with its battlesnakes
    let (game, battlesnakes) = game_battlesnake::get_game_with_battlesnakes(&state.db, game_id)
        .await
        .wrap_err("Failed to get game details")
        .with_status(StatusCode::NOT_FOUND)?;

    let finished = game.status == GameStatus::Finished;

    // Survival stats for finished Solo games, read from the final persisted
    // frame. Waiting/running games may lack a final frame and other modes
    // have no survival semantics, so only finished Solo games query. A
    // finished Solo game with no frames (archived/imported) yields None and
    // the stats rows are simply omitted.
    let solo_stats: Option<SoloGameStats> = if finished && game.game_type == GameType::Solo {
        get_solo_game_stats(&state.db, game_id)
            .await
            .wrap_err("Failed to get Solo game stats")?
    } else {
        None
    };
    // Bind the borrowed cause before the markup block: the value must
    // outlive the whole `html!` expression.
    let solo_cause: Option<&str> = solo_stats
        .as_ref()
        .and_then(|s| s.cause_of_death.as_deref());

    let iframe_src = board_iframe_src(
        &state.config.base_url,
        game_id,
        &board_params,
        board_theme(user.as_ref()),
    );
    // A copied ?showSpoilers link keeps the reveal: sharing the spoiler
    // version is an explicit choice, so the share URL preserves it.
    let share_url = append_show_spoilers(
        share_url(&state.config.base_url, game_id, &board_params),
        params.show_spoilers(),
    );

    // Social-embed description. No winner by default — half the fun of a
    // shared replay is finding out who won by watching it — but sharers can
    // opt into the reveal with ?showSpoilers.
    let winner = battlesnakes.iter().find(|b| b.placement == Some(1));
    let description = match winner {
        Some(winner) if finished && params.show_spoilers() => format!(
            "{} game on a {} board — {} won. Watch the replay on Battlesnake Arena.",
            game.game_type.as_str(),
            game.board_size.as_str(),
            winner.name,
        ),
        _ => format!(
            "{} game on a {} board with {} snakes — watch the replay on Battlesnake Arena.",
            game.game_type.as_str(),
            game.board_size.as_str(),
            battlesnakes.len(),
        ),
    };

    // The viewer's existing saved-game row, if any: pre-fills the save form
    // in the aside so re-saving updates the title.
    let saved = match &user {
        Some(u) => saved_game::get_saved_game_for_user_and_game(&state.db, u.user_id, game_id)
            .await
            .wrap_err("Failed to fetch saved game")?,
        None => None,
    };

    Ok(page_factory.create_theater_page(
        format!("Game {game_id}"),
        Box::new(html! {
            h1 class="vh" { "Game Details" }
            div class="crumb" {
                a href="/leaderboards" { "Leaderboards" }
                " / " span { "Game " (game_id) }
                @match game.status {
                    GameStatus::Waiting => span class="live-pill quiet" { "Waiting" },
                    GameStatus::Running => span class="live-pill" { span class="live-dot" {} "Live" },
                    GameStatus::Finished => span class="live-pill quiet" { "Replay" },
                    GameStatus::Failed => span class="live-pill quiet" { "Incomplete" },
                }
            }

            @if game.status == GameStatus::Waiting {
                p class="empty" {
                    "This game is queued and will start shortly — the page refreshes "
                    "automatically. "
                    a href="" onclick="location.reload(); return false;" class="refresh-link" { "Refresh" }
                    " to check manually."
                }
                // Poll until the runner picks the game up, then reload to show
                // the live board (no-JS fallback: the manual refresh link above).
                script {
                    "(function() {"
                        "var timer = setInterval(function() {"
                            "fetch('/api/games/" (game_id) "')"
                                ".then(function(r) { return r.json(); })"
                                ".then(function(body) {"
                                    // Nested ifs: maud escapes '&&' inside script text
                                    "if (body.Game) { if (body.Game.Status !== 'pending') {"
                                        "clearInterval(timer);"
                                        "location.reload();"
                                    "} }"
                                "})"
                                ".catch(function() {});"
                        "}, 2000);"
                    "})();"
                }
            }

            @if game.status == GameStatus::Failed {
                p class="empty" {
                    "This game never finished — its runner died partway "
                    "through. It has no results and didn't affect any ratings."
                }
            }

            div class="theater" {
                div {
                    div class="board-wrap" {
                        @if game.status == GameStatus::Waiting {
                            // No frames exist yet, so the board viewer would just show a
                            // fetch error — hold its place until the game starts.
                            div class="board-placeholder" style="width: 100%; aspect-ratio: 16 / 9;" {
                                span class="live-dot" {}
                                "Waiting for the game to start"
                            }
                        } @else {
                            // Default aspect-ratio is 16/9; the board sends a RESIZE
                            // postMessage with its actual dimensions.
                            div #board-viewer-container style="width: 100%; aspect-ratio: 16 / 9;" {
                                iframe
                                    id="board-viewer"
                                    src=(iframe_src)
                                    title="Battlesnake Board Viewer"
                                    allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
                                    allowfullscreen {}
                                script { (PreEscaped(BOARD_THEME_SYNC_JS)) }
                            }
                        }
                    }

                    script {
                        "window.addEventListener('message', function(e) {"
                            "if (e.origin !== 'https://board.battlesnake.com') return;"
                            "var evt = e.data;"
                            "if (evt.event === 'RESIZE') {"
                                "document.getElementById('board-viewer-container').style"
                                    ".setProperty('aspect-ratio', evt.data.width + ' / ' + evt.data.height);"
                            "}"
                        "});"
                    }

                    div class="theater-actions" {
                        @if user.is_some() {
                            @if finished {
                                form action={"/games/"(game_id)"/rematch"} method="post" style="display: inline;" {
                                    button type="submit" class="btn" { "Rematch" }
                                }
                            }
                            a href="/games/new" class="btn" { "Create Another Game" }
                            a href="/me" class="btn" { "Back to Profile" }
                        } @else {
                            a href="/leaderboards" class="btn" { "View Leaderboards" }
                            a href="/" class="btn" { "Back to Home" }
                        }
                        @if finished {
                            a
                                href=(export_gif_url(&state.config.base_url, game_id))
                                class="btn"
                                target="_blank"
                                rel="noopener" { "Export GIF" }
                        }
                    }
                }

                aside {
                    h2 class="theater-rail-head" {
                        "Game Results"
                        span class="sub-count" {
                            (battlesnakes.len())
                            @if battlesnakes.len() == 1 { " snake" } @else { " snakes" }
                        }
                    }
                    div class="snakes" {
                        @for battlesnake in &battlesnakes {
                            div .scard .p1[battlesnake.placement == Some(1)] {
                                div class="top" {
                                    span class="chip" style={"background:"(chip_color(&battlesnake.color))} {}
                                    div {
                                        div class="name" {
                                            a href={"/battlesnakes/"(battlesnake.battlesnake_id)"/profile"} {
                                                (battlesnake.name)
                                            }
                                        }
                                        div class="owner" {
                                            "by "
                                            a href={"/users/"(battlesnake.owner_login)} { (battlesnake.owner_login) }
                                        }
                                    }
                                    div class="place" {
                                        @if let Some(placement) = battlesnake.placement {
                                            (ordinal_place(placement))
                                        } @else if finished {
                                            "—"
                                        } @else {
                                            "In Progress"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div class="gmeta" {
                        h3 { "Details" }
                        dl class="meta-list" {
                            div { dt { "Board" } dd { (game.board_size.as_str()) } }
                            div { dt { "Mode" } dd { (game.game_type.as_str()) } }
                            div { dt { "Status" } dd { (capitalize(game.status.as_str())) } }
                            div { dt { "Created" } dd { (game.created_at.format("%Y-%m-%d %H:%M UTC")) } }
                            @if let Some(stats) = &solo_stats {
                                // "Turns Survived" is the final frame's turn
                                // number verbatim: the same number the board
                                // viewer's turn counter shows on the last frame.
                                div { dt { "Turns Survived" } dd { (stats.turns_survived) } }
                                @if let Some(cause) = solo_cause {
                                    div { dt { "Cause of Death" } dd { (death_cause_copy(cause)) } }
                                } @else if stats.turns_survived >= crate::engine::MAX_TURNS {
                                    div {
                                        dt { "Outcome" }
                                        dd {
                                            "Survived to the "
                                            (comma_separate(crate::engine::MAX_TURNS))
                                            "-turn limit"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div class="gmeta" {
                        h3 { "Share" }
                        div class="share-row" {
                            input #share-url type="text" readonly value=(share_url) aria-label="Shareable game link";
                            button #share-copy class="btn sm" type="button" { "Copy Link" }
                        }
                    }

                    script { (PreEscaped(SHARE_COPY_JS)) }

                    @if user.is_some() {
                        div class="gmeta" {
                            @if saved.is_some() {
                                h3 { "Saved to Your Profile" }
                            } @else {
                                h3 { "Save Game" }
                            }
                            form action={"/games/"(game_id)"/save"} method="post" {
                                input
                                    type="text"
                                    name="title"
                                    maxlength="100"
                                    placeholder="Title (optional)"
                                    value=[saved.as_ref().map(|s| s.title.as_str())];
                                " "
                                button type="submit" class="btn" {
                                    @if saved.is_some() { "Update" } @else { "Save Game" }
                                }
                            }
                        }
                    }
                }
            }
        }),
    )
    .with_description(description))
}

/// Query-string suffix (each param prefixed with `&`) for the optional board
/// viewer params. Shared by the iframe src and the share URL so both stay in
/// sync. Values are URL-encoded; only provided params are emitted.
fn board_query_suffix(params: &BoardParams) -> String {
    let mut suffix = String::new();
    if let Some(turn) = params.turn {
        suffix.push_str(&format!("&turn={turn}"));
    }
    if let Some(autoplay) = &params.autoplay
        && autoplay_is_truthy(autoplay)
    {
        suffix.push_str("&autoplay=true");
    }
    if let Some(fps) = params.fps {
        suffix.push_str(&format!("&fps={fps}"));
    }
    if let Some(title) = &params.title {
        suffix.push_str(&format!("&title={}", urlencoding::encode(title)));
    }
    suffix
}

/// Bool-ish parse matching how play treated autoplay: present and not an
/// explicit "off" value means on.
fn autoplay_is_truthy(value: &str) -> bool {
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "false" | "0" | "no" | "off"
    )
}

/// Build the board.battlesnake.com iframe src, forwarding any provided
/// viewer params onto the board.
fn board_iframe_src(base_url: &str, game_id: Uuid, params: &BoardParams, theme: &str) -> String {
    format!(
        "https://board.battlesnake.com/?engine={base_url}/api&game={game_id}&theme={theme}{}",
        board_query_suffix(params)
    )
}

/// Theme param for the board iframe, mirroring the theater-axis resolution in
/// `Page::initial_theme` / the theme bootstrap script. Without it the board
/// defaults to `system`, so a light-OS visitor gets a light board inside
/// arena's dark-default theater. Anonymous visitors get the theater default
/// ("dark"); a signed-in "match"+"system" user gets "system" so the board
/// resolves prefers-color-scheme exactly like the surrounding page.
fn board_theme(user: Option<&crate::models::user::User>) -> &'static str {
    let Some(u) = user else { return "dark" };
    let axis = match u.theater_theme.as_str() {
        "match" => u.site_theme.as_str(),
        explicit => explicit,
    };
    match axis {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    }
}

/// Canonical shareable URL for this game page, including any viewer params
/// that were provided on the current request.
fn share_url(base_url: &str, game_id: Uuid, params: &BoardParams) -> String {
    let suffix = board_query_suffix(params);
    if suffix.is_empty() {
        format!("{base_url}/games/{game_id}")
    } else {
        // suffix starts with '&'; swap the first separator for '?'
        format!("{base_url}/games/{game_id}?{}", &suffix[1..])
    }
}

/// Append the opt-in spoiler flag to a share URL, normalized to `=true`.
fn append_show_spoilers(url: String, show_spoilers: bool) -> String {
    if !show_spoilers {
        return url;
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}showSpoilers=true")
}

/// Build the exporter.battlesnake.com GIF URL for a game. The exporter
/// fetches frame history from `{base_url}/api/games/{game_id}/frames`, so the
/// `/api` suffix is load-bearing: dropping it silently sends the exporter to
/// its default engine, which contains no Arena games.
fn export_gif_url(base_url: &str, game_id: Uuid) -> String {
    format!("https://exporter.battlesnake.com/games/{game_id}/gif?engine_url={base_url}/api")
}

fn ordinal_place(n: i32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix} Place")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Wire-protocol elimination slugs (see `game_runner::elimination_cause_label`)
/// rendered as display copy. Total by construction: unknown slugs pass through
/// unchanged so imported/legacy rows never panic.
fn death_cause_copy(slug: &str) -> &str {
    match slug {
        "out-of-health" => "Starved",
        "wall-collision" => "Hit a wall",
        "self-collision" => "Collided with itself",
        other => other,
    }
}

/// Thousands-separate a non-negative number (e.g. `5000` -> `"5,000"`),
/// for displaying the turn cap in the Solo Outcome row.
fn comma_separate(n: i32) -> String {
    debug_assert!(n >= 0);
    let digits = n.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game_id() -> Uuid {
        Uuid::parse_str("6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1").unwrap()
    }

    #[test]
    fn iframe_src_without_params_matches_original_shape() {
        let src = board_iframe_src(
            "https://arena.example.com",
            game_id(),
            &BoardParams::default(),
            "dark",
        );
        assert_eq!(
            src,
            "https://board.battlesnake.com/?engine=https://arena.example.com/api&game=6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1&theme=dark"
        );
    }

    #[test]
    fn iframe_src_forwards_provided_params() {
        let params = BoardParams {
            turn: Some(143),
            autoplay: Some("true".to_string()),
            fps: Some(10),
            title: Some("Grand Final #3".to_string()),
        };
        let src = board_iframe_src("https://arena.example.com", game_id(), &params, "dark");
        assert_eq!(
            src,
            "https://board.battlesnake.com/?engine=https://arena.example.com/api&game=6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1&theme=dark&turn=143&autoplay=true&fps=10&title=Grand%20Final%20%233"
        );
    }

    #[test]
    fn iframe_src_omits_missing_params() {
        let params = BoardParams {
            turn: Some(7),
            ..Default::default()
        };
        let src = board_iframe_src("https://arena.example.com", game_id(), &params, "light");
        assert!(src.contains("&theme=light"));
        assert!(src.ends_with("&turn=7"));
        assert!(!src.contains("autoplay"));
        assert!(!src.contains("fps"));
        assert!(!src.contains("title"));
    }

    fn user_with_themes(theater: &str, site: &str) -> crate::models::user::User {
        crate::models::user::User {
            user_id: game_id(),
            external_github_id: 1,
            github_login: "tester".to_string(),
            github_avatar_url: None,
            github_name: None,
            github_email: None,
            display_name: None,
            pronouns: String::new(),
            country: String::new(),
            backstory: String::new(),
            is_admin: false,
            site_theme: site.to_string(),
            theater_theme: theater.to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn board_theme_defaults_dark_for_anonymous() {
        // Matches the theater axis default in the theme bootstrap script.
        assert_eq!(board_theme(None), "dark");
    }

    #[test]
    fn board_theme_uses_explicit_theater_theme() {
        assert_eq!(
            board_theme(Some(&user_with_themes("light", "dark"))),
            "light"
        );
        assert_eq!(
            board_theme(Some(&user_with_themes("dark", "light"))),
            "dark"
        );
    }

    #[test]
    fn board_theme_match_follows_site_axis() {
        assert_eq!(
            board_theme(Some(&user_with_themes("match", "light"))),
            "light"
        );
        assert_eq!(
            board_theme(Some(&user_with_themes("match", "dark"))),
            "dark"
        );
        // "match" + "system": the board resolves prefers-color-scheme itself,
        // exactly like the surrounding page.
        assert_eq!(
            board_theme(Some(&user_with_themes("match", "system"))),
            "system"
        );
    }

    #[test]
    fn title_is_url_encoded() {
        let params = BoardParams {
            title: Some("a&b=c?d".to_string()),
            ..Default::default()
        };
        let suffix = board_query_suffix(&params);
        assert_eq!(suffix, "&title=a%26b%3Dc%3Fd");
    }

    #[test]
    fn autoplay_boolish_values() {
        for on in ["true", "1", "yes", "TRUE", ""] {
            assert!(autoplay_is_truthy(on), "{on:?} should be truthy");
        }
        for off in ["false", "0", "no", "off", "FALSE"] {
            assert!(!autoplay_is_truthy(off), "{off:?} should be falsy");
        }
    }

    #[test]
    fn share_url_without_params_is_plain_game_url() {
        let url = share_url(
            "https://arena.example.com",
            game_id(),
            &BoardParams::default(),
        );
        assert_eq!(
            url,
            "https://arena.example.com/games/6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1"
        );
    }

    #[test]
    fn share_url_includes_provided_params() {
        let params = BoardParams {
            turn: Some(143),
            autoplay: Some("1".to_string()),
            fps: None,
            title: None,
        };
        let url = share_url("https://arena.example.com", game_id(), &params);
        assert_eq!(
            url,
            "https://arena.example.com/games/6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1?turn=143&autoplay=true"
        );
    }

    #[test]
    fn export_gif_url_is_full_exporter_url_with_api_engine() {
        let url = export_gif_url("https://arena.example.com", game_id());
        assert_eq!(
            url,
            "https://exporter.battlesnake.com/games/6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1/gif?engine_url=https://arena.example.com/api"
        );
    }

    fn params(value: Option<&str>) -> ViewGameParams {
        ViewGameParams {
            show_spoilers: value.map(str::to_string),
        }
    }

    #[test]
    fn spoilers_off_by_default() {
        assert!(!params(None).show_spoilers());
    }

    #[test]
    fn bare_or_truthy_param_enables_spoilers() {
        // A bare ?showSpoilers deserializes as an empty string
        assert!(params(Some("")).show_spoilers());
        assert!(params(Some("true")).show_spoilers());
        assert!(params(Some("1")).show_spoilers());
    }

    #[test]
    fn explicit_falsy_values_disable_spoilers() {
        for v in ["false", "0", "no", "off", "FALSE", "No"] {
            assert!(!params(Some(v)).show_spoilers(), "{v} should be falsy");
        }
    }

    #[test]
    fn share_url_preserves_show_spoilers() {
        let plain = append_show_spoilers("https://a.example/games/x".to_string(), false);
        assert_eq!(plain, "https://a.example/games/x");
        let bare = append_show_spoilers("https://a.example/games/x".to_string(), true);
        assert_eq!(bare, "https://a.example/games/x?showSpoilers=true");
        let with_params =
            append_show_spoilers("https://a.example/games/x?turn=3".to_string(), true);
        assert_eq!(
            with_params,
            "https://a.example/games/x?turn=3&showSpoilers=true"
        );
    }

    #[test]
    fn death_cause_copy_maps_known_slugs_and_passes_others_through() {
        assert_eq!(death_cause_copy("out-of-health"), "Starved");
        assert_eq!(death_cause_copy("wall-collision"), "Hit a wall");
        assert_eq!(death_cause_copy("self-collision"), "Collided with itself");
        // Total function: unknown/imported slugs pass through unchanged.
        assert_eq!(death_cause_copy("mystery-mode"), "mystery-mode");
        assert_eq!(death_cause_copy(""), "");
    }

    #[test]
    fn comma_separate_groups_thousands() {
        assert_eq!(comma_separate(5000), "5,000");
        assert_eq!(comma_separate(1000), "1,000");
        assert_eq!(comma_separate(999), "999");
        assert_eq!(comma_separate(1234567), "1,234,567");
        assert_eq!(comma_separate(0), "0");
    }
}
