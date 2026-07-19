use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use axum_macros::debug_handler;
use color_eyre::eyre::Context as _;
use maud::html;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::game::GameStatus,
    models::game_battlesnake,
    models::saved_game,
    routes::auth::OptionalUser,
    state::AppState,
};

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
    Path(game_id): Path<Uuid>,
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

    let iframe_src = board_iframe_src(&state.config.base_url, game_id, &board_params);
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
                    "This game is waiting to start. "
                    a href="" onclick="location.reload(); return false;" class="refresh-link" { "Refresh" }
                    " to check for updates."
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
                        // Board viewer iframe - always show, it handles waiting/empty games
                        // gracefully. Default aspect-ratio is 16/9; the board sends a RESIZE
                        // postMessage with its actual dimensions.
                        div #board-viewer-container style="width: 100%; aspect-ratio: 16 / 9;" {
                            iframe
                                id="board-viewer"
                                src=(iframe_src)
                                title="Battlesnake Board Viewer"
                                allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture"
                                allowfullscreen {}
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
                    }
                }

                aside {
                    h2 class="theater-rail-head" {
                        "Game Results"
                        span class="sub-count" { (battlesnakes.len()) " snakes" }
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
                        }
                    }

                    div class="gmeta" {
                        h3 { "Share" }
                        div class="share-row" {
                            input #share-url type="text" readonly value=(share_url) aria-label="Shareable game link";
                            button #share-copy class="btn sm" type="button" { "Copy Link" }
                        }
                    }

                    script {
                        "(function() {"
                            "var btn = document.getElementById('share-copy');"
                            "var input = document.getElementById('share-url');"
                            "function done() {"
                                "btn.textContent = 'Copied!';"
                                "setTimeout(function() { btn.textContent = 'Copy Link'; }, 1500);"
                            "}"
                            "function fallback() {"
                                "input.focus();"
                                "input.select();"
                                "try { document.execCommand('copy'); done(); } catch (e) {}"
                            "}"
                            "btn.addEventListener('click', function() {"
                                "if (navigator.clipboard && navigator.clipboard.writeText) {"
                                    "navigator.clipboard.writeText(input.value).then(done, fallback);"
                                "} else {"
                                    "fallback();"
                                "}"
                            "});"
                        "})();"
                    }

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
fn board_iframe_src(base_url: &str, game_id: Uuid, params: &BoardParams) -> String {
    format!(
        "https://board.battlesnake.com/?engine={base_url}/api&game={game_id}{}",
        board_query_suffix(params)
    )
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
        );
        assert_eq!(
            src,
            "https://board.battlesnake.com/?engine=https://arena.example.com/api&game=6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1"
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
        let src = board_iframe_src("https://arena.example.com", game_id(), &params);
        assert_eq!(
            src,
            "https://board.battlesnake.com/?engine=https://arena.example.com/api&game=6f9422eb-cd95-4a17-b0a2-a3fefe4f47b1&turn=143&autoplay=true&fps=10&title=Grand%20Final%20%233"
        );
    }

    #[test]
    fn iframe_src_omits_missing_params() {
        let params = BoardParams {
            turn: Some(7),
            ..Default::default()
        };
        let src = board_iframe_src("https://arena.example.com", game_id(), &params);
        assert!(src.ends_with("&turn=7"));
        assert!(!src.contains("autoplay"));
        assert!(!src.contains("fps"));
        assert!(!src.contains("title"));
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
}
