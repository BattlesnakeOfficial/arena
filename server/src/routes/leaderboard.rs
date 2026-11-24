use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use chrono_humanize::HumanTime;
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use uuid::Uuid;

use crate::{
    components::{
        avatar::user_avatar,
        live_refresh::{LiveRefreshConfig, live_page_refresh},
        page_factory::PageFactory,
        snake_avatar::snake_avatar,
        snake_tags::snake_tag_chips,
    },
    cron::MATCHMAKER_INTERVAL_SECS,
    errors::{ServerResult, WithRedirect},
    models::leaderboard_registration,
    models::{
        battlesnake::{self, Visibility},
        game::GameType,
        leaderboard::{self, MIN_GAMES_FOR_RANKING},
        leaderboard_season::{self, SeasonLookup, SeasonPhase},
        private_leaderboard, tag, user,
    },
    routes::auth::{CurrentUser, OptionalUser},
    routes::{UuidPath, UuidPath2},
    scoring::EntryScore,
    state::AppState,
};

#[derive(serde::Deserialize)]
pub struct PaginationParams {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub sort: leaderboard::LeaderboardSort,
    /// `?season=<season_id>`: show that season (must belong to the league).
    /// Absent = the league's default season (see `select_season`).
    #[serde(default)]
    pub season: Option<Uuid>,
}

/// GET /leaderboards — list all leaderboards
pub async fn list_leaderboards(
    State(state): State<AppState>,
    OptionalUser(_user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let leaderboards = leaderboard::get_public_leaderboards(&state.db)
        .await
        .wrap_err("Failed to fetch leaderboards")?;

    Ok(page_factory.create_page(
        "Leaderboards".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "Leaderboards" }
                div class="sub" {
                    "Ranked ladders, one per game mode. Join with one of your snakes and the matchmaker takes it from there."
                }
            }

            @if leaderboards.is_empty() {
                p class="empty" { "No leaderboards available yet." }
            } @else {
                div class="section" {
                    table class="data" {
                        thead {
                            tr {
                                th { "Leaderboard" }
                                th class="r" { "Status" }
                            }
                        }
                        tbody {
                            @for lb in &leaderboards {
                                tr {
                                    td {
                                        div class="snake-cell" {
                                            span {
                                                a class="name" href={"/leaderboards/"(lb.leaderboard_id)} { (lb.name) }
                                            }
                                        }
                                    }
                                    td class="r" {
                                        @if lb.disabled_at.is_some() {
                                            span class="badge" { "Inactive" }
                                        } @else {
                                            span class="badge ok" { "Active" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
    ))
}

/// GET /leaderboards/:id — leaderboard detail with rankings
#[allow(clippy::too_many_lines)]
pub async fn show_leaderboard(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    UuidPath(leaderboard_id): UuidPath,
    Query(pagination): Query<PaginationParams>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let Some(lb) = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")?
    else {
        return Ok(crate::routes::render_not_found(page_factory));
    };

    // Mode nav lists public ladders only; a private ladder is unlisted, so it
    // neither shows up in the nav nor shows it.
    let is_private = lb.visibility == Visibility::Private;
    let mode_nav = if is_private {
        vec![]
    } else {
        leaderboard::get_public_leaderboards(&state.db)
            .await
            .wrap_err("Failed to fetch leaderboards")?
    };

    let game_type: GameType = lb
        .game_type
        .parse()
        .wrap_err("Failed to parse leaderboard game type")?;
    let has_food = game_type.has_food();
    // Food-less modes have no food sort; an old ?sort=food_eaten link falls
    // back to rating instead of ranking a column of zeros.
    let sort = if has_food {
        pagination.sort
    } else {
        leaderboard::LeaderboardSort::Rating
    };

    let SeasonLookup::Found(selection) =
        leaderboard_season::select_season(&state.db, leaderboard_id, pagination.season)
            .await
            .wrap_err("Failed to select leaderboard season")?
    else {
        return Ok(crate::routes::render_not_found(page_factory));
    };
    let selected = selection.selected.clone();
    let league_active = selection.league_active().cloned();
    let next_season_starts_at = selection.next_season_starts_at;
    // Sort and pager links stay sticky to an explicit ?season= only, so the
    // canonical (default-season) URLs are unchanged.
    let season_param = pagination
        .season
        .map(|id| format!("&season={id}"))
        .unwrap_or_default();

    let per_page: i64 = 50;

    let (total_ranked, page, ranked, placement, status, activity, top_eaters) = if let Some(info) =
        &selected
    {
        let season_id = info.season.season_id;
        let total_ranked = leaderboard::count_ranked_entries(&state.db, leaderboard_id, season_id)
            .await
            .wrap_err("Failed to count ranked entries")?;

        let total_pages = if total_ranked > 0 {
            (total_ranked + per_page - 1) / per_page
        } else {
            1
        };
        let page = pagination.page.unwrap_or(0).clamp(0, total_pages - 1);

        let ranked = leaderboard::get_ranked_entries_paginated(
            &state.db,
            leaderboard_id,
            season_id,
            page,
            per_page,
            sort,
        )
        .await
        .wrap_err("Failed to fetch ranked entries")?;

        let placement = leaderboard::get_placement_entries(&state.db, leaderboard_id, season_id)
            .await
            .wrap_err("Failed to fetch placement entries")?;

        let status = leaderboard::get_leaderboard_status(&state.db, leaderboard_id, season_id)
            .await
            .wrap_err("Failed to fetch leaderboard status")?;

        let activity = leaderboard::get_activity_feed(&state.db, leaderboard_id, season_id, 8)
            .await
            .wrap_err("Failed to fetch activity feed")?;

        let top_eaters = if has_food {
            leaderboard::get_top_eaters(&state.db, leaderboard_id, season_id, 3)
                .await
                .wrap_err("Failed to fetch top eaters")?
        } else {
            vec![]
        };
        (
            total_ranked,
            page,
            ranked,
            placement,
            status,
            activity,
            top_eaters,
        )
    } else {
        // Only future seasons: the empty-league shape.
        (
            0,
            0,
            vec![],
            vec![],
            leaderboard::LeaderboardStatus {
                last_game_created_at: None,
                games_in_progress: 0,
                total_games: 0,
            },
            vec![],
            vec![],
        )
    };
    let total_pages = if total_ranked > 0 {
        (total_ranked + per_page - 1) / per_page
    } else {
        1
    };

    // Get user's snakes for the join form
    let user_snakes = if let Some(ref u) = user {
        battlesnake::get_battlesnakes_by_user_id(&state.db, u.user_id)
            .await
            .wrap_err("Failed to fetch user's battlesnakes")?
    } else {
        vec![]
    };

    // The user's snakes registered in this league
    let user_entries = if let Some(ref u) = user {
        leaderboard::get_user_memberships(&state.db, leaderboard_id, u.user_id)
            .await
            .wrap_err("Failed to fetch user's league memberships")?
    } else {
        vec![]
    };

    // Non-members of a private ladder must enter the invite code to join.
    let needs_invite_code = match &user {
        Some(u) if is_private => {
            !private_leaderboard::is_member(&state.db, leaderboard_id, u.user_id)
                .await
                .wrap_err("Failed to check leaderboard membership")?
        }
        _ => false,
    };

    // Compute next matchmaker run time — a league-live fact, keyed to the
    // league's active season, never to the selected (maybe past) one. When
    // the ladder is starved the matchmaker no-ops, so "last game + interval"
    // would show an ever-staler past timestamp — say why matchmaking is
    // paused instead.
    let enabled_count = leaderboard::count_active_entries(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to count active entries")?;
    let active_last_game_created_at = match &league_active {
        None => None,
        Some(active)
            if selected
                .as_ref()
                .is_some_and(|info| info.season.season_id == active.season.season_id) =>
        {
            status.last_game_created_at
        }
        Some(active) => {
            leaderboard::get_leaderboard_status(&state.db, leaderboard_id, active.season.season_id)
                .await
                .wrap_err("Failed to fetch active season status")?
                .last_game_created_at
        }
    };
    let next_run_str = if enabled_count < leaderboard::MIN_MATCH_SIZE as i64 {
        None
    } else {
        active_last_game_created_at.map(|last| {
            let next_run = last + chrono::Duration::seconds(MATCHMAKER_INTERVAL_SECS as i64);
            HumanTime::from(next_run).to_string()
        })
    };

    let rank_start = page * per_page;
    let sort_param = sort.as_str();
    // Collect entry IDs from the current page for scoring lookups
    let entry_ids: Vec<Uuid> = ranked
        .iter()
        .chain(placement.iter())
        .map(|e| e.leaderboard_entry_id)
        .collect();

    // Batch-load tags for the snakes visible on this page (ranked +
    // placement only). Untagged snakes are absent from the map.
    let visible_snake_ids: Vec<Uuid> = ranked
        .iter()
        .chain(placement.iter())
        .map(|e| e.battlesnake_id)
        .collect();
    let snake_tags = tag::get_tags_for_battlesnakes(&state.db, &visible_snake_ids)
        .await
        .wrap_err("Failed to fetch battlesnake tags")?;

    // Fetch per-algorithm scores for only the visible entries
    let mut algo_scores: Vec<(&str, HashMap<Uuid, EntryScore>)> = vec![];
    for algo in state
        .scoring
        .algorithms()
        .iter()
        .filter(|algo| algo.applies_to(&game_type))
    {
        let scores = algo
            .get_scores(&state.db, &entry_ids)
            .await
            .wrap_err_with(|| format!("Failed to fetch {} scores", algo.key()))?;
        let map: HashMap<Uuid, EntryScore> = scores
            .into_iter()
            .map(|s| (s.leaderboard_entry_id, s))
            .collect();
        algo_scores.push((algo.score_column_name(), map));
    }

    let description = format!(
        "{} leaderboard on Battlesnake — {} ranked snakes, {} games played.",
        lb.name, total_ranked, status.total_games
    );
    let social_alt = format!("{}: the top of the leaderboard", lb.name);

    Ok(page_factory.create_page(
        lb.name.clone(),
        Box::new(html! {
            div class="crumb" {
                a href="/leaderboards" { "Leaderboards" }
                " / " (lb.name)
            }
            div class="page-head" {
                h1 {
                    (lb.name)
                    @if is_private {
                        " "
                        span class="badge" { "Private" }
                    }
                }
                div class="sub" {
                    @if is_private {
                        "Private ladder — only people with this link can find it, and "
                        "joining takes an invite code. "
                    }
                    "Ranked play — register a snake, join the ladder, and the "
                    "matchmaker starts new games every few minutes."
                }
            }

            @if mode_nav.len() > 1 {
                nav class="modes" aria-label="Game modes" {
                    @for other in &mode_nav {
                        @if other.leaderboard_id == leaderboard_id {
                            span class="mode on" aria-current="page" { (other.name) }
                        } @else {
                            a class="mode" href={"/leaderboards/"(other.leaderboard_id)} { (other.name) }
                        }
                    }
                }
            }

            (season_picker(leaderboard_id, &selection.seasons, selected.as_ref().map(|info| info.season.season_id)))
            (season_notice(leaderboard_id, selected.as_ref(), league_active.as_ref(), next_season_starts_at))

            div class="stats" {
                div class="stat" {
                    div class="label" { "Ranked snakes" }
                    div class="value" { (total_ranked) }
                }
                div class="stat" {
                    div class="label" { "Games played" }
                    div class="value" { (status.total_games) }
                }
                div class="stat" {
                    div class="label" { "In progress" }
                    div class="value" {
                        span class="live" { (status.games_in_progress) }
                        small { "live now" }
                    }
                }
                div class="stat" {
                    div class="label" { "Next matchmaker run" }
                    div class="value sm" {
                        @if league_active.is_none() {
                            // Never "paused" here either (see below).
                            @if let Some(next) = next_season_starts_at {
                                "between seasons — next starts " (HumanTime::from(next).to_string())
                            } @else {
                                "between seasons"
                            }
                        } @else if enabled_count < leaderboard::MIN_MATCH_SIZE as i64 {
                            // "waiting", not "paused": entry status badges say
                            // "Paused" and e2e locators match text loosely.
                            "waiting for at least "
                            (leaderboard::MIN_MATCH_SIZE)
                            " healthy snakes ("
                            (enabled_count)
                            " enabled)"
                        } @else if let Some(ref next) = next_run_str {
                            (next)
                        } @else {
                            "waiting for first games"
                        }
                    }
                }
            }

            @if status.games_in_progress > 0 {
                (live_page_refresh(&LiveRefreshConfig {
                    interval_ms: 30_000,
                    max_ticks: 120,
                    state_key: &format!(
                        "leaderboard:{}:{}:{}",
                        status.total_games,
                        status.games_in_progress,
                        status.last_game_created_at.map_or_else(
                            || "none".to_string(),
                            |timestamp| timestamp.timestamp().to_string(),
                        ),
                    ),
                }))
            }

            div class="grid" {
                div {
                    @if has_food {
                        div class="sortbar" {
                            span { "sort" }
                            @if sort == leaderboard::LeaderboardSort::Rating {
                                span class="on" aria-current="true" { "Rating" }
                            } @else {
                                a href={"/leaderboards/"(leaderboard_id)"?sort=rating"(season_param)} { "Rating" }
                            }
                            @if sort == leaderboard::LeaderboardSort::FoodEaten {
                                span class="on" aria-current="true" { "Food eaten" }
                            } @else {
                                a href={"/leaderboards/"(leaderboard_id)"?sort=food_eaten"(season_param)} { "Food eaten" }
                            }
                        }
                    }

                    @if ranked.is_empty() {
                        p class="empty" {
                            "No snakes have completed enough games to be ranked yet. "
                            "(Minimum: " (MIN_GAMES_FOR_RANKING) " games)"
                        }
                    } @else {
                        div class="table-scroll" {
                            table class="data" {
                                thead {
                                    tr {
                                        th { "#" }
                                        th { "Battlesnake" }
                                        @for (col_name, _map) in &algo_scores {
                                            th .r { (col_name) }
                                        }
                                        th class="r" { "Games" }
                                    }
                                }
                                tbody {
                                    @for (i, entry) in ranked.iter().enumerate() {
                                        @let rank = rank_start + i as i64 + 1;
                                        // Tombstone rows are not "yours": no row highlight,
                                        // no "— you" (the user id is not nulled).
                                        @let is_you = !entry.deleted && user.as_ref().is_some_and(|u| u.user_id == entry.user_id);
                                        tr .top[rank <= 3] .you[is_you] {
                                            td class="rank" { (format!("{rank:02}")) }
                                            td {
                                                div class="snake-cell" {
                                                    (snake_avatar(entry.snake_head.as_deref().unwrap_or(""), entry.snake_tail.as_deref().unwrap_or(""), entry.snake_color.as_deref().unwrap_or("")))
                                                    div class="snake-details" {
                                                        @if entry.deleted {
                                                            span class="name" { "Deleted snake" }
                                                        } @else {
                                                            a class="name" href={"/leaderboards/"(leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} {
                                                                (entry.snake_name.as_deref().unwrap_or_default())
                                                            }
                                                            span class="owner" {
                                                                "by "
                                                                (crate::components::flag::country_flag(entry.owner_country.as_deref().unwrap_or("")))
                                                                a href={"/users/"(entry.owner_login.as_deref().unwrap_or_default())} { (entry.owner_name.as_deref().unwrap_or_default()) }
                                                                @if is_you { " — you" }
                                                            }
                                                            (snake_tag_chips(snake_tags.get(&entry.battlesnake_id).map(Vec::as_slice).unwrap_or(&[])))
                                                        }
                                                    }
                                                }
                                            }
                                            @for (_col_name, map) in &algo_scores {
                                                td .r .rating {
                                                    @if let Some(score) = map.get(&entry.leaderboard_entry_id) {
                                                        (format!("{:.1}", score.score))
                                                    } @else {
                                                        "—"
                                                    }
                                                }
                                            }
                                            td class="r num" { (entry.games_played) }
                                        }
                                    }
                                }
                            }
                        }

                        div class="pager" {
                            @if page > 0 {
                                a href={"/leaderboards/"(leaderboard_id)"?sort="(sort_param)"&page="(page - 1)(season_param)} { "‹ Prev" }
                            }
                            @if total_pages > 1 {
                                span class="cur" { "Page " (page + 1) " of " (total_pages) }
                            }
                            @if page < total_pages - 1 {
                                a href={"/leaderboards/"(leaderboard_id)"?sort="(sort_param)"&page="(page + 1)(season_param)} { "Next ›" }
                            }
                            span class="spacer" {}
                            span {
                                "Showing " (rank_start + 1) "–" (rank_start + ranked.len() as i64)
                                " of " (total_ranked) " ranked snakes"
                            }
                        }
                    }

                    @if !placement.is_empty() {
                        div class="section" {
                            h2 { "In Placement" }
                            p class="empty" { "These snakes need more games before appearing in rankings." }
                            table class="data" {
                                thead {
                                    tr {
                                        th { "Battlesnake" }
                                        th class="r" { "Games played" }
                                        th class="r" { "Games remaining" }
                                    }
                                }
                                tbody {
                                    @for entry in &placement {
                                        tr {
                                            td {
                                                div class="snake-cell" {
                                                    (snake_avatar(entry.snake_head.as_deref().unwrap_or(""), entry.snake_tail.as_deref().unwrap_or(""), entry.snake_color.as_deref().unwrap_or("")))
                                                    div class="snake-details" {
                                                        @if entry.deleted {
                                                            span class="name" { "Deleted snake" }
                                                        } @else {
                                                            a class="name" href={"/leaderboards/"(leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} {
                                                                (entry.snake_name.as_deref().unwrap_or_default())
                                                            }
                                                            span class="owner" {
                                                                "by "
                                                                (crate::components::flag::country_flag(entry.owner_country.as_deref().unwrap_or("")))
                                                                a href={"/users/"(entry.owner_login.as_deref().unwrap_or_default())} { (entry.owner_name.as_deref().unwrap_or_default()) }
                                                            }
                                                            (snake_tag_chips(snake_tags.get(&entry.battlesnake_id).map(Vec::as_slice).unwrap_or(&[])))
                                                        }
                                                    }
                                                }
                                            }
                                            td class="r num" { (entry.games_played) }
                                            td class="r num" { (MIN_GAMES_FOR_RANKING - entry.games_played) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                aside class="rail" {
                    @if !activity.is_empty() {
                        div class="block" {
                            h3 { span class="live-dot" {} "Recent games" }
                            ul class="feed" {
                                @for event in &activity {
                                    li {
                                        span class="t" { (fmt_ago(event.created_at)) }
                                        span {
                                            @if event.deleted {
                                                b { "Deleted snake" }
                                            } @else {
                                                a href={"/leaderboards/"(leaderboard_id)"/entries/"(event.leaderboard_entry_id)} {
                                                    b { (event.snake_name.as_deref().unwrap_or_default()) }
                                                }
                                            }
                                            " "
                                            (render_feed_place(event.placement, event.is_draw))
                                            " "
                                            (render_score_delta(event.display_score_change, "delta up", "delta down"))
                                        }
                                    }
                                }
                            }
                        }
                    }

                    @if user.is_some() {
                        div class="block" {
                            h3 { "Your Snakes" }
                            @for entry in &user_entries {
                                @if let Some(snake) = user_snakes.iter().find(|s| s.battlesnake_id == entry.battlesnake_id) {
                                    div class="mine" {
                                        (snake_avatar(&snake.head, &snake.tail, &snake.color))
                                        span class="mname" { (snake.name) }
                                        @if snake.retired_at.is_some() {
                                            span class="badge" { "Retired" }
                                        }
                                        @if entry.disabled_at.is_some() {
                                            @if entry.disabled_reason.as_deref() == Some(leaderboard_registration::DISABLED_REASON_HEALTH) {
                                                a class="badge warn" href={"/battlesnakes/"(snake.battlesnake_id)"/profile"}
                                                    title="Automatically paused: this snake is failing health checks. Resume re-tests it — details on its profile." {
                                                    "Auto-paused"
                                                }
                                            } @else if entry.disabled_reason.as_deref() != Some(battlesnake::DISABLED_REASON_RETIRED) {
                                                span class="badge" { "Paused" }
                                            }
                                            @if snake.retired_at.is_none() {
                                                form action={"/leaderboards/"(leaderboard_id)"/join"} method="post" {
                                                    input type="hidden" name="battlesnake_id" value=(snake.battlesnake_id);
                                                    button type="submit" class="btn sm" aria-label={"Resume " (snake.name)} { "Resume" }
                                                }
                                            }
                                        } @else {
                                            span class="badge ok" { "Active" }
                                            form action={"/leaderboards/"(leaderboard_id)"/leave"} method="post" {
                                                input type="hidden" name="battlesnake_id" value=(entry.battlesnake_id);
                                                button type="submit" class="btn sm" aria-label={"Pause " (snake.name)} { "Pause" }
                                            }
                                        }
                                    }
                                    div class="mstats" {
                                        "score " (format!("{:.1}", entry.display_score))
                                        " · games " (entry.games_played)
                                    }
                                }
                            }

                            @let joinable: Vec<_> = user_snakes.iter().filter(|snake| snake.retired_at.is_none()).collect();
                            @if !joinable.is_empty() {
                                form class="join-form" action={"/leaderboards/"(leaderboard_id)"/join"} method="post" {
                                    @if needs_invite_code {
                                        input type="text" name="invite_code" required
                                            aria-label="Invite code" placeholder="Invite code"
                                            autocomplete="off" autocapitalize="characters" spellcheck="false";
                                    }
                                    select name="battlesnake_id" aria-label="Snake to join with" {
                                        @for snake in joinable {
                                            option value=(snake.battlesnake_id) { (snake.name) }
                                        }
                                    }
                                    button type="submit" class="btn solid sm" { "Join" }
                                }
                            } @else if user_entries.is_empty() {
                                p class="railp" {
                                    "Register a snake to join. "
                                    a href="/battlesnakes/new" style="color:var(--pink)" { "Register one" }
                                }
                            }
                        }
                    }

                    @if !top_eaters.is_empty() {
                        div class="block" {
                            h3 { "Top eaters" }
                            ul class="feed" {
                                @for (i, eater) in top_eaters.iter().enumerate() {
                                    li {
                                        span class="t" { "#" (i + 1) }
                                        span {
                                            @if eater.deleted {
                                                b { "Deleted snake" }
                                            } @else {
                                                a href={"/leaderboards/"(leaderboard_id)"/entries/"(eater.leaderboard_entry_id)} {
                                                    b { (eater.snake_name.as_deref().unwrap_or_default()) }
                                                }
                                                span class="owner-inline" {
                                                    " by "
                                                    (crate::components::flag::country_flag(eater.owner_country.as_deref().unwrap_or("")))
                                                    a href={"/users/"(eater.owner_login.as_deref().unwrap_or_default())} { (eater.owner_name.as_deref().unwrap_or_default()) }
                                                }
                                            }
                                            " · "
                                            span class="place" { (eater.food_score) " food" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    @if user.is_none() {
                        div class="block" {
                            h3 { "Your snake here" }
                            p class="railp" {
                                "Deploy a server, register your snake, and the matchmaker "
                                "takes it from there — new ranked games every few minutes."
                            }
                            a class="btn solid sm" style="margin-top:14px; display:inline-block" href="/auth/github" { "Sign in to join" }
                        }
                    }
                }
            }
        }),
    )
    .with_description(description)
    .with_social_image(crate::routes::og::leaderboard_card_path(leaderboard_id), social_alt)
    .with_noindex(is_private)
    .into_response())
}

/// GET /leaderboards/:id/entries/:entry_id — snake detail on leaderboard
#[allow(clippy::too_many_lines)]
pub async fn show_leaderboard_entry(
    State(state): State<AppState>,
    OptionalUser(_user): OptionalUser,
    UuidPath2(leaderboard_id, entry_id): UuidPath2,
    Query(pagination): Query<PaginationParams>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let Some(lb) = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")?
    else {
        return Ok(crate::routes::render_not_found(page_factory));
    };

    let Some(entry) = leaderboard::get_entry_by_id(&state.db, entry_id)
        .await
        .wrap_err("Failed to fetch leaderboard entry")?
    else {
        return Ok(crate::routes::render_not_found(page_factory));
    };

    if entry.leaderboard_id != leaderboard_id {
        return Ok(crate::routes::render_not_found(page_factory));
    }

    let Some(snake) = battlesnake::get_battlesnake_by_id(&state.db, entry.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")?
    else {
        return Ok(crate::routes::render_missing_snake(
            &state.db,
            entry.battlesnake_id,
            page_factory,
        )
        .await?);
    };

    let owner = user::get_user_by_id(&state.db, snake.user_id)
        .await
        .wrap_err("Failed to fetch owner")?;

    // `owner_login` is the `/users/{login}` URL key; `owner_name` is the
    // public name shown as text.
    let (owner_login, owner_name) = owner.as_ref().map_or_else(
        || ("Unknown".to_string(), "Unknown".to_string()),
        |o| (o.github_login.clone(), o.public_name().to_string()),
    );
    let owner_avatar = owner.as_ref().and_then(|o| o.github_avatar_url.clone());
    let owner_country = owner
        .as_ref()
        .map_or_else(String::new, |o| o.country.clone());

    let per_page: i64 = 20;

    let total_games = leaderboard::count_game_results_for_entry(&state.db, entry_id)
        .await
        .wrap_err("Failed to count game results")?;

    let total_pages = if total_games > 0 {
        (total_games + per_page - 1) / per_page
    } else {
        1
    };
    let page = pagination.page.unwrap_or(0).max(0).min(total_pages - 1);

    let history = leaderboard::get_game_history_for_entry(&state.db, entry_id, page, per_page)
        .await
        .wrap_err("Failed to fetch game history")?;

    let game_ids: Vec<Uuid> = history.iter().map(|h| h.game_id).collect();
    let opponents_list = if !game_ids.is_empty() {
        leaderboard::get_opponents_for_games(&state.db, &game_ids, entry_id)
            .await
            .wrap_err("Failed to fetch opponents")?
    } else {
        vec![]
    };

    let mut opponents_map: HashMap<Uuid, Vec<leaderboard::GameOpponent>> = HashMap::new();
    for opp in opponents_list {
        opponents_map.entry(opp.game_id).or_default().push(opp);
    }

    let rating_points = leaderboard::get_rating_history_for_entry(&state.db, entry_id)
        .await
        .wrap_err("Failed to fetch rating history")?;

    // Rank within the entry's own season, so an old entry link keeps its
    // standing after a rollover.
    let rank = leaderboard::get_rank_for_entry(
        &state.db,
        entry.season_id,
        entry.display_score,
        entry.games_played,
    )
    .await
    .wrap_err("Failed to get rank")?;

    // Name the entry's season only once the league has more than one.
    let season_badge = match leaderboard_season::select_season(&state.db, leaderboard_id, None)
        .await
        .wrap_err("Failed to fetch leaderboard seasons")?
    {
        SeasonLookup::Found(selection) if selection.seasons.len() > 1 => selection
            .seasons
            .into_iter()
            .find(|info| info.season.season_id == entry.season_id)
            .map(|info| info.season.name),
        _ => None,
    };

    // Compute SVG chart data
    let (points_str, grid_y_positions, y_labels) = if rating_points.len() >= 2 {
        let min_score = rating_points
            .iter()
            .map(|p| p.display_score_after)
            .fold(f64::INFINITY, f64::min);
        let max_score = rating_points
            .iter()
            .map(|p| p.display_score_after)
            .fold(f64::NEG_INFINITY, f64::max);
        let score_range = max_score - min_score;
        let padding = if score_range < 0.01 {
            1.0
        } else {
            score_range * 0.1
        };
        let y_min = min_score - padding;
        let y_max = max_score + padding;

        let first_ts = rating_points.first().unwrap().game_created_at.timestamp() as f64;
        let last_ts = rating_points.last().unwrap().game_created_at.timestamp() as f64;
        let ts_range = if (last_ts - first_ts).abs() < 1.0 {
            1.0
        } else {
            last_ts - first_ts
        };

        let points: Vec<String> = rating_points
            .iter()
            .map(|p| {
                let x = 40.0 + (p.game_created_at.timestamp() as f64 - first_ts) / ts_range * 560.0;
                let y = 210.0 - (p.display_score_after - y_min) / (y_max - y_min) * 200.0;
                format!("{x:.0},{y:.0}")
            })
            .collect();
        let points_str = points.join(" ");

        let grid_count = 4;
        let grid_y: Vec<String> = (0..=grid_count)
            .map(|i| format!("{:.0}", 10.0 + (i as f64 / grid_count as f64) * 200.0))
            .collect();

        // Tick precision follows the visible span: a ladder that only moved
        // half a point would otherwise render every gridline as the same
        // integer (five ticks all reading "49").
        // Integer ticks need span >= grid_count so adjacent gridlines can't
        // round to the same value (step = span/4 must be >= 1.0).
        let span = y_max - y_min;
        let tick_decimals: usize = if span < 0.5 {
            2
        } else if span < 4.0 {
            1
        } else {
            0
        };
        let labels: Vec<(String, String)> = (0..=grid_count)
            .map(|i| {
                let score = y_max - (i as f64 / grid_count as f64) * (y_max - y_min);
                let y_pos = format!("{:.0}", 10.0 + (i as f64 / grid_count as f64) * 200.0 + 4.0);
                (format!("{score:.tick_decimals$}"), y_pos)
            })
            .collect();

        (points_str, grid_y, labels)
    } else {
        (String::new(), vec![], vec![])
    };

    // Recent form: always the 5 most recent games (independent of current page)
    let recent_games = leaderboard::get_game_history_for_entry(&state.db, entry_id, 0, 5)
        .await
        .wrap_err("Failed to fetch recent form")?;

    let game_type: GameType = lb
        .game_type
        .parse()
        .wrap_err("Failed to parse leaderboard game type")?;
    let has_food = game_type.has_food();

    // Fetch per-algorithm scores for this entry
    let mut algo_entry_scores: Vec<(&str, &str, Option<EntryScore>)> = vec![];
    for algo in state
        .scoring
        .algorithms()
        .iter()
        .filter(|algo| algo.applies_to(&game_type))
    {
        let score = algo
            .get_entry_score(&state.db, entry_id)
            .await
            .wrap_err_with(|| format!("Failed to fetch {} entry score", algo.key()))?;
        algo_entry_scores.push((algo.key(), algo.display_name(), score));
    }

    let description = format!(
        "{} by {} on the {} leaderboard — rating {:.1}, {} games played.",
        snake.name, owner_name, lb.name, entry.display_score, entry.games_played
    );
    let social_alt = format!(
        "{} by {} on the {} leaderboard",
        snake.name, owner_name, lb.name
    );

    Ok(page_factory.create_page(
        format!("{} - {}", snake.name, lb.name),
        Box::new(html! {
            div class="crumb" {
                a href="/leaderboards" { "Leaderboards" }
                " / "
                a href={"/leaderboards/"(leaderboard_id)} { (lb.name) }
                " / " span { (snake.name) }
            }

            div class="page-head" {
                (user_avatar(owner_avatar.as_deref(), &owner_name, "entry-avatar"))
                div {
                    h1 {
                        a href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} { (snake.name) }
                        @if snake.retired_at.is_some() {
                            span class="badge" { "Retired" }
                        }
                        @if let Some(season_name) = &season_badge {
                            " "
                            span class="badge season" { (season_name) }
                        }
                    }
                    div class="sub" {
                        "by "
                        (crate::components::flag::country_flag(&owner_country))
                        @if owner.is_some() {
                            a href={"/users/"(owner_login)} { (owner_name) }
                        } @else {
                            (owner_name)
                        }
                        " on "
                        a href={"/leaderboards/"(leaderboard_id)} { (lb.name) }
                    }
                }
                div class="spacer" {}
                a href={"/battlesnakes/"(entry.battlesnake_id)"/profile"} class="btn head-cta" { "Snake Profile" }
            }

            div class="stats" {
                div class="stat" {
                    div class="label" { "Rating" }
                    div class="value" { (format!("{:.1}", entry.display_score)) }
                }
                div class="stat" {
                    div class="label" { "Rank" }
                    @if let Some(r) = rank {
                        div class="value" { "#" (r) }
                    } @else {
                        div class="value sm" { "In Placement" }
                    }
                }
                div class="stat" {
                    div class="label" { "Games" }
                    div class="value" {
                        (entry.games_played)
                        small { (entry.first_place_finishes) " won" }
                    }
                    @if !recent_games.is_empty() {
                        div class="formrow" title="Recent form, newest first" {
                            @for game in &recent_games {
                                @match (game.is_draw, game.placement) {
                                    (true, _) => span { "🤝" },
                                    (_, 1) => span { "🥇" },
                                    (_, 2) => span { "🥈" },
                                    (_, 3) => span { "🥉" },
                                    _ => span { "▪️" },
                                }
                            }
                        }
                    }
                }
                div class="stat" {
                    div class="label" { "Win Rate" }
                    div class="value" {
                        @if entry.games_played > 0 {
                            (format!("{:.0}%", entry.first_place_finishes as f64 / entry.games_played as f64 * 100.0))
                        } @else {
                            "—"
                        }
                    }
                }
            }

            div class="section" {
                h2 { "Rating Trajectory" }
                div class="rating-chart-container" {
                    @if rating_points.len() >= 2 {
                        svg class="rating-chart" width="100%" viewBox="0 0 620 220" {
                            @for y_line in &grid_y_positions {
                                line class="grid" x1="40" y1=(y_line) x2="600" y2=(y_line) stroke-width="1" {}
                            }
                            polyline class="line" points=(points_str) fill="none" stroke-width="2" {}
                            @for (label, y_pos) in &y_labels {
                                text class="lbl" x="35" y=(y_pos) text-anchor="end" { (label) }
                            }
                        }
                    } @else {
                        p class="empty" { "Not enough games yet to chart a trajectory." }
                    }
                }
            }

            div class="section" {
                h2 { "Scores by Algorithm" }
                div class="table-scroll" {
                    table class="data" {
                        thead {
                            tr {
                                th { "Algorithm" }
                                th class="r" { "Score" }
                                th { "Details" }
                            }
                        }
                        tbody {
                            @for (_key, display_name, score) in &algo_entry_scores {
                                tr {
                                    td { (display_name) }
                                    @if let Some(s) = score {
                                        td class="r num" { (format!("{:.1}", s.score)) }
                                        td {
                                            span class="sub" {
                                                @for (j, (detail_name, detail_value)) in s.details.iter().enumerate() {
                                                    @if j > 0 { " · " }
                                                    (detail_name) " " (detail_value)
                                                }
                                            }
                                        }
                                    } @else {
                                        td class="r num" { "—" }
                                        td { span class="sub" { "No data" } }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div class="section" {
                h2 { "Game History" }
                @if history.is_empty() {
                    p class="empty" { "No games played yet." }
                } @else {
                    div class="table-scroll" {
                        table class="data" {
                            thead {
                                tr {
                                    th { "Date" }
                                    th { "Opponents" }
                                    th class="r" { "Placement" }
                                    th class="r" { "Rating" }
                                    @if has_food {
                                        th class="r" { "Food" }
                                    }
                                    th class="r" { "Replay" }
                                }
                            }
                            tbody {
                                @for game in &history {
                                    tr {
                                        td { (game.game_created_at.format("%Y-%m-%d %H:%M")) }
                                        td {
                                            @if let Some(opps) = opponents_map.get(&game.game_id) {
                                                @for (j, opp) in opps.iter().enumerate() {
                                                    @if j > 0 { ", " }
                                                    @if let Some(opp_entry_id) = opp.leaderboard_entry_id {
                                                        a href={"/leaderboards/"(leaderboard_id)"/entries/"(opp_entry_id)} { (opp.snake_name) }
                                                    } @else {
                                                        (opp.snake_name)
                                                    }
                                                }
                                            } @else {
                                                span class="sub" { "—" }
                                            }
                                        }
                                        td class="r" {
                                            (placement_badge(game.placement, game.is_draw))
                                        }
                                        td class="r num" {
                                            (render_score_delta(game.display_score_change, "rating-positive", "rating-negative"))
                                        }
                                        @if has_food {
                                            td class="r num" { (game.food_eaten) }
                                        }
                                        td class="r" {
                                            a href={"/games/"(game.game_id)} class="btn sm" { "Watch" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Pagination
                    @if total_pages > 1 {
                        div class="pagination" {
                            @if page > 0 {
                                a href={"/leaderboards/"(leaderboard_id)"/entries/"(entry_id)"?page="(page - 1)} { "Previous" }
                            } @else {
                                span class="disabled" { "Previous" }
                            }
                            span class="current" { "Page " (page + 1) " of " (total_pages) }
                            @if page < total_pages - 1 {
                                a href={"/leaderboards/"(leaderboard_id)"/entries/"(entry_id)"?page="(page + 1)} { "Next" }
                            } @else {
                                span class="disabled" { "Next" }
                            }
                        }
                    }
                }
            }
        }),
    )
    .with_description(description)
    .with_social_image(
        crate::routes::og::entry_card_path(leaderboard_id, entry_id),
        social_alt,
    )
    .with_noindex(lb.visibility == Visibility::Private)
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct JoinLeaveForm {
    pub battlesnake_id: Uuid,
    /// Required to join a private leaderboard you aren't a member of yet.
    #[serde(default)]
    pub invite_code: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct LeaveForm {
    pub battlesnake_id: Uuid,
}

/// POST /leaderboards/:id/join — opt-in a snake
pub async fn join_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    flasher: crate::flasher::Flasher,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<JoinLeaveForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}"));

    // Verify leaderboard exists and is active
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")
        .with_redirect(redirect.clone())?;

    let lb = lb.ok_or_else(|| {
        crate::errors::ServerError(
            color_eyre::eyre::eyre!("Leaderboard not found"),
            redirect.clone(),
        )
    })?;

    if lb.disabled_at.is_some() {
        flasher
            .error("That leaderboard is no longer active.")
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    }

    // Verify snake belongs to user. `get_battlesnake_by_id` filters
    // deleted snakes, so a missing/deleted snake is a flash, not a 500.
    let Some(snake) = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")
        .with_redirect(redirect.clone())?
    else {
        flasher
            .error("That battlesnake doesn't exist or has been deleted.")
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    };

    if snake.user_id != user.user_id {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You don't own this battlesnake"),
            redirect,
        ));
    }

    if snake.retired_at.is_some() {
        flasher
            .error("This battlesnake is retired — unretire it before joining.")
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    }

    if !private_leaderboard::join_allowed(&state.db, &lb, user.user_id, form.invite_code.as_deref())
        .await
        .wrap_err("Failed to check invite code")
        .with_redirect(redirect.clone())?
    {
        flasher
            .error("That invite code isn't right. Ask the leaderboard's organizer for the current one.")
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    }

    // Opt-in (or resume if paused). Resuming also clears the registration's
    // health streak, so a snake the sweeper pulled isn't re-paused on the very
    // next sweep (see `get_or_create_registration`).
    let Some(_registration) = leaderboard_registration::get_or_create_registration(
        &state.db,
        leaderboard_id,
        form.battlesnake_id,
    )
    .await
    .wrap_err("Failed to join leaderboard")
    .with_redirect(redirect.clone())?
    else {
        // Raced a delete: the snake was live at the pre-check but is gone (or
        // its registration is 'deleted'-marked) by the time the upsert ran.
        let retired = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
            .await
            .wrap_err("Failed to recheck battlesnake")
            .with_redirect(redirect.clone())?
            .is_some_and(|snake| snake.retired_at.is_some());
        flasher
            .error(if retired {
                "This battlesnake is retired — unretire it before joining."
            } else {
                "That battlesnake doesn't exist or has been deleted."
            })
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    };

    // The active season's entry, created lazily. `None` (a gap between
    // seasons) still means the join succeeded: the snake plays once the next
    // season starts.
    leaderboard::ensure_snake_season_entry(
        &state.db,
        &state.scoring,
        leaderboard_id,
        form.battlesnake_id,
    )
    .await
    .wrap_err("Failed to create season entry")
    .with_redirect(redirect.clone())?;

    flasher
        .success(format!(
            "{} is in the matchmaking rotation on {} — it'll be picked up in upcoming games.",
            snake.name, lb.name
        ))
        .await
        .wrap_err("Failed to flash")
        .with_redirect(redirect.clone())?;

    Ok(redirect)
}

/// POST /leaderboards/:id/leave — pause a snake's league registration
pub async fn leave_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    flasher: crate::flasher::Flasher,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<LeaveForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}"));

    // Verify snake belongs to user
    let snake = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Battlesnake not found"),
                redirect.clone(),
            )
        })?;

    if snake.user_id != user.user_id {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You don't own this battlesnake"),
            redirect,
        ));
    }

    // A registration may have no entry yet (joined during a gap), so leave
    // is keyed on the snake, not an entry.
    let registration =
        leaderboard_registration::get_registration(&state.db, leaderboard_id, snake.battlesnake_id)
            .await
            .wrap_err("Failed to fetch registration")
            .with_redirect(redirect.clone())?;
    if registration.is_none() {
        flasher
            .error("That battlesnake is not in this leaderboard.")
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    }

    leaderboard_registration::set_registration_disabled(
        &state.db,
        leaderboard_id,
        snake.battlesnake_id,
        Some(chrono::Utc::now()),
    )
    .await
    .wrap_err("Failed to pause registration")
    .with_redirect(redirect.clone())?;

    Ok(redirect)
}

/// Season picker: every season of the league, the shown one marked current.
/// Always rendered — even a lone Preseason gets its chip.
fn season_picker(
    leaderboard_id: Uuid,
    seasons: &[leaderboard_season::SeasonInfo],
    selected: Option<Uuid>,
) -> Markup {
    html! {
        nav class="modes seasons" aria-label="Seasons" {
            @for info in seasons {
                @if selected == Some(info.season.season_id) {
                    span class="mode on" aria-current="page" { (info.season.name) }
                } @else {
                    a class="mode" href={"/leaderboards/"(leaderboard_id)"?season="(info.season.season_id)} {
                        (info.season.name)
                    }
                }
            }
        }
    }
}

/// The one-line note above the stats when the shown season isn't the live
/// one. Nothing for the active season.
fn season_notice(
    leaderboard_id: Uuid,
    selected: Option<&leaderboard_season::SeasonInfo>,
    league_active: Option<&leaderboard_season::SeasonInfo>,
    next_season_starts_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Markup {
    html! {
        @match selected.map(|info| info.phase) {
            Some(SeasonPhase::Active) => {}
            Some(SeasonPhase::Ended) => {
                p class="empty season-notice" {
                    @if let Some(active) = league_active {
                        "This season has ended — see the live standings in "
                        a href={"/leaderboards/"(leaderboard_id)} { (active.season.name) }
                        "."
                    } @else if let Some(next) = next_season_starts_at {
                        "This season has ended. The next one starts "
                        (HumanTime::from(next).to_string()) "."
                    } @else {
                        "This season has ended."
                    }
                }
            }
            Some(SeasonPhase::Upcoming) => {
                @if let Some(info) = selected {
                    p class="empty season-notice" {
                        "This season starts " (HumanTime::from(info.season.starts_at).to_string()) "."
                    }
                }
            }
            None => {
                p class="empty season-notice" {
                    @if let Some(next) = next_season_starts_at {
                        "The next season starts " (HumanTime::from(next).to_string()) "."
                    } @else {
                        "The next season hasn't started yet."
                    }
                }
            }
        }
    }
}

/// Compact relative time for rail feeds ("2m ago"), where HumanTime is too wordy.
fn fmt_ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - t).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn ordinal(n: i32) -> String {
    match n {
        1 => "1st".to_string(),
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        _ => format!("{n}th"),
    }
}

/// Rating delta chip. Veteran snakes move by hundredths or thousandths of a
/// point per game, so the number keeps enough decimals to stay readable: two
/// from 0.01 up, three from 0.001, four below that. A change that would still
/// print as zero renders nothing, like an exact zero. Class names are
/// caller-supplied because each surface styles deltas differently
/// ("delta up"/"delta down" vs "rating-positive"/"rating-negative").
pub(crate) fn render_score_delta(delta: f64, up_class: &str, down_class: &str) -> Markup {
    let magnitude = delta.abs();
    if magnitude < 0.00005 {
        return html! {};
    }
    let class = if delta > 0.0 { up_class } else { down_class };
    let decimals = if magnitude >= 0.01 {
        2
    } else if magnitude >= 0.001 {
        3
    } else {
        4
    };
    html! {
        span class=(class) { (format!("{delta:+.decimals$}")) }
    }
}

/// A game's placement as a badge. Sharing first place is a draw, not a win, so
/// it never gets the gold medal.
pub(crate) fn placement_badge(placement: i32, is_draw: bool) -> Markup {
    html! {
        @if is_draw {
            span class="badge" { "🤝 Draw" }
        } @else {
            @match placement {
                1 => span class="badge ok" { "🥇 1st" },
                2 => span class="badge" { "🥈 2nd" },
                3 => span class="badge" { "🥉 3rd" },
                // 4+ (max 4 snakes/game) — "th" is always right
                p => span class="badge" { (p) "th" },
            }
        }
    }
}

/// A game's placement in an activity feed.
pub(crate) fn render_feed_place(placement: i32, is_draw: bool) -> Markup {
    html! {
        @if is_draw {
            span class="place" { "Draw" }
        } @else {
            span class={"place p"(placement)} { (ordinal(placement)) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{placement_badge, render_feed_place, render_score_delta};

    #[test]
    fn deltas_from_a_hundredth_up_show_two_decimals() {
        assert_eq!(
            render_score_delta(1.26, "delta up", "delta down").into_string(),
            r#"<span class="delta up">+1.26</span>"#
        );
        assert_eq!(
            render_score_delta(-0.033, "rating-positive", "rating-negative").into_string(),
            r#"<span class="rating-negative">-0.03</span>"#
        );
        assert_eq!(
            render_score_delta(0.01, "delta up", "delta down").into_string(),
            r#"<span class="delta up">+0.01</span>"#
        );
    }

    #[test]
    fn deltas_under_a_hundredth_show_three_decimals() {
        assert_eq!(
            render_score_delta(-0.002, "rating-positive", "rating-negative").into_string(),
            r#"<span class="rating-negative">-0.002</span>"#
        );
        assert_eq!(
            render_score_delta(0.001, "delta up", "delta down").into_string(),
            r#"<span class="delta up">+0.001</span>"#
        );
    }

    #[test]
    fn deltas_under_a_thousandth_show_four_decimals() {
        assert_eq!(
            render_score_delta(0.0004, "delta up", "delta down").into_string(),
            r#"<span class="delta up">+0.0004</span>"#
        );
        assert_eq!(
            render_score_delta(-0.00005, "rating-positive", "rating-negative").into_string(),
            r#"<span class="rating-negative">-0.0001</span>"#
        );
    }

    #[test]
    fn deltas_that_would_print_as_zero_render_nothing() {
        for delta in [0.0, -0.0, 0.00004, -0.00004] {
            assert_eq!(
                render_score_delta(delta, "delta up", "delta down").into_string(),
                "",
                "{delta}"
            );
        }
    }

    #[test]
    fn draws_never_show_a_medal() {
        assert_eq!(
            placement_badge(1, true).into_string(),
            r#"<span class="badge">🤝 Draw</span>"#
        );
        assert_eq!(
            render_feed_place(1, true).into_string(),
            r#"<span class="place">Draw</span>"#
        );
    }

    #[test]
    fn outright_placements_keep_their_labels() {
        assert_eq!(
            placement_badge(1, false).into_string(),
            r#"<span class="badge ok">🥇 1st</span>"#
        );
        assert_eq!(
            placement_badge(3, false).into_string(),
            r#"<span class="badge">🥉 3rd</span>"#
        );
        assert_eq!(
            render_feed_place(1, false).into_string(),
            r#"<span class="place p1">1st</span>"#
        );
    }
}

#[cfg(test)]
mod owner_name_route_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::models::leaderboard::{self, MIN_GAMES_FOR_RANKING};
    use crate::state::AppState;

    async fn get(pool: &PgPool, path: &str) -> (StatusCode, String) {
        let app = crate::routes::routes(AppState::test_from_pool(pool.clone()))
            .expect("asset router")
            .into_router();
        let mut builder = Request::builder().uri(path);
        if path.starts_with("/api/") {
            builder = builder.header(header::ORIGIN, "https://example.com");
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn owner(pool: &PgPool, github_id: i64, login: &str, display_name: Option<&str>) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, display_name)
             VALUES ($1, $2, $3) RETURNING user_id",
            github_id,
            login,
            display_name
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Adds a snake to the leaderboard with the given games played and food
    /// score, returning its leaderboard entry id.
    async fn entry(
        pool: &PgPool,
        leaderboard_id: Uuid,
        user_id: Uuid,
        name: &str,
        games_played: i32,
        food_score: i64,
    ) -> Uuid {
        let battlesnake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'http://snake') RETURNING battlesnake_id",
            user_id,
            name
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let entry_id = leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id)
            .await
            .unwrap()
            .expect("live snake joins")
            .leaderboard_entry_id;
        sqlx::query!(
            "UPDATE leaderboard_entries SET games_played = $2 WHERE leaderboard_entry_id = $1",
            entry_id,
            games_played
        )
        .execute(pool)
        .await
        .unwrap();
        // get_or_create_entry initializes every algorithm's row, so overwrite it.
        sqlx::query(
            "INSERT INTO food_eaten_stats (leaderboard_entry_id, food_score) VALUES ($1, $2)
             ON CONFLICT (leaderboard_entry_id) DO UPDATE SET food_score = EXCLUDED.food_score",
        )
        .bind(entry_id)
        .bind(food_score)
        .execute(pool)
        .await
        .unwrap();
        entry_id
    }

    /// Records one finished two-snake leaderboard game for both entries.
    async fn finished_duel(
        pool: &PgPool,
        leaderboard_id: Uuid,
        results: [(Uuid, i32, f64); 2],
    ) -> Uuid {
        let game = crate::models::game::create_game(
            pool,
            crate::models::game::CreateGame {
                board_size: crate::models::game::GameBoardSize::Medium,
                game_type: crate::models::game::GameType::Standard,
            },
        )
        .await
        .unwrap();
        let lb_game = leaderboard::create_leaderboard_game(pool, leaderboard_id, game.game_id)
            .await
            .unwrap();
        for (entry_id, placement, display_score_change) in results {
            sqlx::query(
                "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id, placement)
                 VALUES ($1, $2, $3)",
            )
            .bind(game.game_id)
            .bind(entry_id)
            .bind(placement)
            .execute(pool)
            .await
            .unwrap();
            leaderboard::create_game_result(
                pool,
                leaderboard::CreateGameResult {
                    leaderboard_game_id: lb_game.leaderboard_game_id,
                    leaderboard_entry_id: entry_id,
                    placement,
                    mu_before: 50.0,
                    mu_after: 50.0,
                    sigma_before: 0.6,
                    sigma_after: 0.6,
                    display_score_change,
                },
            )
            .await
            .unwrap();
        }
        game.game_id
    }

    /// A veteran's history shows real numbers for tiny rating changes, and a
    /// shared first place reads as a draw while an outright win keeps the medal.
    #[sqlx::test(migrations = "../migrations")]
    async fn entry_history_shows_small_deltas_and_draws(pool: PgPool) {
        let leaderboard_id = leaderboard::get_active_leaderboards(&pool)
            .await
            .unwrap()
            .first()
            .expect("migrations seed an active leaderboard")
            .leaderboard_id;
        let user = owner(&pool, 9930101, "gh-veteran", None).await;
        let veteran = entry(&pool, leaderboard_id, user, "Veteran", 3000, 0).await;
        let rival = entry(&pool, leaderboard_id, user, "Rival", 3000, 0).await;

        let draw_game = finished_duel(
            &pool,
            leaderboard_id,
            [(veteran, 1, -0.013), (rival, 1, 0.013)],
        )
        .await;
        let path = format!("/leaderboards/{leaderboard_id}/entries/{veteran}");

        let (status, body) = get(&pool, &path).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("🤝 Draw"), "shared first place is a draw");
        assert!(!body.contains("🥇 1st"), "a draw never gets the medal");
        assert!(
            body.contains("<span>🤝</span>"),
            "recent form shows the draw"
        );
        assert!(
            body.contains(r#"<span class="rating-negative">-0.01</span>"#),
            "tiny loss shows its number"
        );

        for feed_path in ["/".to_string(), format!("/leaderboards/{leaderboard_id}")] {
            let (status, body) = get(&pool, &feed_path).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                body.contains(r#"<span class="place">Draw</span>"#),
                "{feed_path} feed shows the draw"
            );
            assert!(!body.contains(r#"class="place p1""#), "{feed_path} no 1st");
        }

        let battlesnake_id: Uuid = sqlx::query_scalar(
            "SELECT battlesnake_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
        )
        .bind(veteran)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (status, body) = get(&pool, &format!("/battlesnakes/{battlesnake_id}/profile")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("🤝 Draw"), "profile history shows the draw");
        assert!(!body.contains("🥇 1st"), "profile history has no medal");

        sqlx::query("UPDATE games SET status = 'finished' WHERE game_id = $1")
            .bind(draw_game)
            .execute(&pool)
            .await
            .unwrap();
        let (status, body) = get(&pool, &format!("/games/{draw_game}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"<div class="place">Draw</div>"#),
            "game page shows the draw"
        );
        assert!(!body.contains("1st Place"), "game page has no first place");
        assert!(!body.contains("scard p1"), "game page highlights no winner");

        finished_duel(
            &pool,
            leaderboard_id,
            [(veteran, 1, 0.033), (rival, 2, -0.033)],
        )
        .await;

        let (_, body) = get(&pool, &path).await;
        assert!(body.contains("🥇 1st"), "an outright win keeps the medal");
        assert!(body.contains(r#"<span class="rating-positive">+0.03</span>"#));

        let (_, body) = get(&pool, &format!("/leaderboards/{leaderboard_id}")).await;
        assert!(body.contains(r#"<span class="place p1">1st</span>"#));
    }

    /// Leaderboard surfaces show the owner's display name as link text while
    /// keeping the GitHub login as the `/users/{login}` URL key; owners
    /// without a display name fall back to their login.
    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_pages_show_owner_display_name(pool: PgPool) {
        // The homepage features the first active leaderboard, so use it.
        let leaderboard_id = leaderboard::get_active_leaderboards(&pool)
            .await
            .unwrap()
            .first()
            .expect("migrations seed an active leaderboard")
            .leaderboard_id;

        let named = owner(&pool, 9930001, "gh-display", Some("Display Person")).await;
        let plain = owner(&pool, 9930002, "gh-plain", None).await;
        let named_entry = entry(
            &pool,
            leaderboard_id,
            named,
            "named-ranked",
            MIN_GAMES_FOR_RANKING + 5,
            7,
        )
        .await;
        entry(&pool, leaderboard_id, named, "named-placement", 1, 0).await;
        entry(
            &pool,
            leaderboard_id,
            plain,
            "plain-ranked",
            MIN_GAMES_FOR_RANKING,
            2,
        )
        .await;

        let named_link = r#"<a href="/users/gh-display">Display Person</a>"#;
        let plain_link = r#"<a href="/users/gh-plain">gh-plain</a>"#;

        // Ranked table, placement table, and top-eaters rail.
        let (status, body) = get(&pool, &format!("/leaderboards/{leaderboard_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.matches(named_link).count(), 3, "{body}");
        assert_eq!(body.matches(plain_link).count(), 2, "{body}");
        assert!(!body.contains(">gh-display<"), "login shown as text");

        let (status, body) = get(
            &pool,
            &format!("/leaderboards/{leaderboard_id}/entries/{named_entry}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(named_link), "{body}");
        assert!(body.contains("named-ranked by Display Person on the"));
        assert!(!body.contains(">gh-display<"), "login shown as text");

        let (status, body) = get(&pool, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("by Display Person"), "{body}");
        assert!(!body.contains("by gh-display"));

        let (status, body) = get(
            &pool,
            &format!("/api/leaderboards/{leaderboard_id}/rankings"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let owners = |key: &str| -> Vec<(String, String)> {
            let mut rows: Vec<(String, String)> = json[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e["owner"].as_str().unwrap().to_string(),
                        e["owner_name"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            rows.sort();
            rows
        };
        let pair = |login: &str, name: &str| (login.to_string(), name.to_string());
        assert_eq!(
            owners("ranked"),
            vec![
                pair("gh-display", "Display Person"),
                pair("gh-plain", "gh-plain")
            ]
        );
        assert_eq!(
            owners("placement"),
            vec![pair("gh-display", "Display Person")]
        );
    }
}

#[cfg(test)]
mod tombstone_route_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::models::leaderboard::{self, MIN_GAMES_FOR_RANKING};
    use crate::scoring::{
        ScoringRegistry, food_eaten::FoodEatenScoring, weng_lin::WengLinScoring,
        win_rate::WinRateScoring,
    };
    use crate::state::AppState;

    /// GET `path` with the production scoring algorithms registered, so the
    /// rankings API returns real per-algorithm scores.
    async fn get(pool: &PgPool, path: &str) -> (StatusCode, String) {
        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        scoring.register(Box::new(FoodEatenScoring));
        state.scoring = std::sync::Arc::new(scoring);
        let mut builder = Request::builder().uri(path);
        if path.starts_with("/api/") {
            builder = builder.header(header::ORIGIN, "https://example.com");
        }
        let app = crate::routes::routes(state)
            .expect("asset router")
            .into_router();
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn owner(pool: &PgPool, github_id: i64, login: &str) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES ($1, $2) RETURNING user_id",
            github_id,
            login
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Adds a snake to the leaderboard with the given games played and food
    /// score, returning its leaderboard entry id.
    async fn entry(
        pool: &PgPool,
        leaderboard_id: Uuid,
        user_id: Uuid,
        name: &str,
        games_played: i32,
        food_score: i64,
    ) -> Uuid {
        let battlesnake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, 'http://snake') RETURNING battlesnake_id",
            user_id,
            name
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let entry_id = leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id)
            .await
            .unwrap()
            .expect("live snake joins")
            .leaderboard_entry_id;
        sqlx::query!(
            "UPDATE leaderboard_entries SET games_played = $2 WHERE leaderboard_entry_id = $1",
            entry_id,
            games_played
        )
        .execute(pool)
        .await
        .unwrap();
        // get_or_create_entry initializes every algorithm's row, so overwrite it.
        sqlx::query(
            "INSERT INTO food_eaten_stats (leaderboard_entry_id, food_score) VALUES ($1, $2)
             ON CONFLICT (leaderboard_entry_id) DO UPDATE SET food_score = EXCLUDED.food_score",
        )
        .bind(entry_id)
        .bind(food_score)
        .execute(pool)
        .await
        .unwrap();
        entry_id
    }

    /// Ended-season standings tombstone soft-deleted snakes on the league
    /// page and the rankings API: the row and its rank stay, identity is
    /// blanked, no links to the snake or its owner.
    #[sqlx::test(migrations = "../migrations")]
    async fn ended_season_standings_tombstone_deleted_snakes(pool: PgPool) {
        let leaderboard_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboards (name) VALUES ('tombstone-league') RETURNING leaderboard_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let live_owner = owner(&pool, 9932101, "gh-live").await;
        let ghost_owner = owner(&pool, 9932102, "gh-ghost").await;
        let live = entry(
            &pool,
            leaderboard_id,
            live_owner,
            "Live Lass",
            MIN_GAMES_FOR_RANKING + 2,
            7,
        )
        .await;
        let ghost_ranked = entry(
            &pool,
            leaderboard_id,
            ghost_owner,
            "Ghost Racer",
            MIN_GAMES_FOR_RANKING + 1,
            0,
        )
        .await;
        let ghost_placement =
            entry(&pool, leaderboard_id, ghost_owner, "Ghost Placement", 1, 0).await;
        // Rank order: the live snake leads the frozen standings.
        for (entry_id, score) in [(live, 50.0), (ghost_ranked, 40.0)] {
            sqlx::query(
                "UPDATE leaderboard_entries SET display_score = $2
                 WHERE leaderboard_entry_id = $1",
            )
            .bind(entry_id)
            .bind(score)
            .execute(&pool)
            .await
            .unwrap();
        }

        // The Preseason ends, then the ghosts are deleted through the real
        // flow (registrations disabled exactly as in prod).
        sqlx::query(
            "UPDATE leaderboard_seasons
             SET starts_at = clock_timestamp() - interval '30 days',
                 ends_at = clock_timestamp() - interval '1 day'
             WHERE leaderboard_id = $1",
        )
        .bind(leaderboard_id)
        .execute(&pool)
        .await
        .unwrap();
        for entry_id in [ghost_ranked, ghost_placement] {
            let snake_id: Uuid = sqlx::query_scalar(
                "SELECT battlesnake_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1",
            )
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                crate::models::battlesnake::delete_battlesnake(&pool, snake_id, ghost_owner)
                    .await
                    .unwrap(),
                crate::models::battlesnake::DeleteBattlesnakeOutcome::Deleted
            );
        }
        let season: Uuid = sqlx::query_scalar(
            "SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1",
        )
        .bind(leaderboard_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // League page: tombstones render in the ranked and placement rows
        // with their ranks; nothing links the ghosts' names or owner.
        let (status, body) = get(
            &pool,
            &format!("/leaderboards/{leaderboard_id}?season={season}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body.matches("Deleted snake").count(),
            2,
            "ranked + placement rows: {body}"
        );
        assert!(body.contains("Live Lass"), "{body}");
        assert!(!body.contains("Ghost Racer"), "{body}");
        assert!(!body.contains("Ghost Placement"), "{body}");
        assert!(!body.contains("/users/gh-ghost"), "{body}");
        assert!(body.contains(r#"<td class="rank">01</td>"#), "{body}");
        assert!(body.contains(r#"<td class="rank">02</td>"#), "{body}");

        // Rankings API: tombstone rows keep id, rank, and stats; identity
        // fields are null. Both `ranked` and `placement` share the mapper.
        let (status, body) = get(
            &pool,
            &format!("/api/leaderboards/{leaderboard_id}/rankings?season={season}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let ranked = json["ranked"].as_array().unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0]["snake_name"], "Live Lass");
        assert_eq!(ranked[0]["owner"], "gh-live");
        assert_eq!(ranked[0]["deleted"], false);
        assert_eq!(ranked[1]["rank"], 2);
        assert_eq!(ranked[1]["snake_name"], serde_json::Value::Null);
        assert_eq!(ranked[1]["owner"], serde_json::Value::Null);
        assert_eq!(ranked[1]["owner_name"], serde_json::Value::Null);
        assert_eq!(ranked[1]["deleted"], true);
        assert_eq!(ranked[1]["games_played"], MIN_GAMES_FOR_RANKING + 1);
        assert_eq!(ranked[1]["display_score"], 40.0);
        let live_score_keys = {
            let mut keys: Vec<&str> = ranked[0]["scores"]
                .as_object()
                .unwrap()
                .keys()
                .map(|k| k.as_str())
                .collect();
            keys.sort_unstable();
            keys
        };
        let mut ghost_score_keys: Vec<&str> = ranked[1]["scores"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        ghost_score_keys.sort_unstable();
        assert_eq!(
            ghost_score_keys, live_score_keys,
            "tombstone keeps its per-algorithm scores"
        );

        let placement = json["placement"].as_array().unwrap();
        assert_eq!(placement.len(), 1);
        assert_eq!(placement[0]["snake_name"], serde_json::Value::Null);
        assert_eq!(placement[0]["owner"], serde_json::Value::Null);
        assert_eq!(placement[0]["owner_name"], serde_json::Value::Null);
        assert_eq!(placement[0]["deleted"], true);
        assert_eq!(placement[0]["games_played"], 1);
    }
}

#[cfg(test)]
mod column_route_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::models::leaderboard::{self, MIN_GAMES_FOR_RANKING};
    use crate::scoring::{
        ScoringRegistry, food_eaten::FoodEatenScoring, weng_lin::WengLinScoring,
        win_rate::WinRateScoring,
    };
    use crate::state::AppState;

    /// GET `path` with the production scoring algorithms registered.
    async fn get(pool: &PgPool, path: &str) -> String {
        let mut state = AppState::test_from_pool(pool.clone());
        let mut scoring = ScoringRegistry::new();
        scoring.register(Box::new(WengLinScoring));
        scoring.register(Box::new(WinRateScoring));
        scoring.register(Box::new(FoodEatenScoring));
        state.scoring = std::sync::Arc::new(scoring);
        let app = crate::routes::routes(state)
            .expect("asset router")
            .into_router();
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    /// A ranked entry with a nonzero food score on the named seeded ladder.
    async fn ranked_entry(pool: &PgPool, github_id: i64, leaderboard_name: &str) -> (Uuid, Uuid) {
        let leaderboard_id = sqlx::query_scalar!(
            "SELECT leaderboard_id FROM leaderboards WHERE name = $1",
            leaderboard_name
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES ($1, $2) RETURNING user_id",
            github_id,
            format!("gh-{github_id}")
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let battlesnake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'ranked', 'http://snake') RETURNING battlesnake_id",
            user_id
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let entry_id = leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id)
            .await
            .unwrap()
            .expect("live snake joins")
            .leaderboard_entry_id;
        sqlx::query!(
            "UPDATE leaderboard_entries SET games_played = $2 WHERE leaderboard_entry_id = $1",
            entry_id,
            MIN_GAMES_FOR_RANKING
        )
        .execute(pool)
        .await
        .unwrap();
        // get_or_create_entry initializes every algorithm's row, so overwrite it.
        sqlx::query(
            "INSERT INTO food_eaten_stats (leaderboard_entry_id, food_score) VALUES ($1, 5)
             ON CONFLICT (leaderboard_entry_id) DO UPDATE SET food_score = EXCLUDED.food_score",
        )
        .bind(entry_id)
        .execute(pool)
        .await
        .unwrap();
        (leaderboard_id, entry_id)
    }

    /// Win % is the only win-rate column (#232: "1st place %" was the same
    /// number twice). Constrictor never has food, so its pages drop every
    /// food stat, even if stale scores exist and even when the URL asks to
    /// sort by food (#233).
    #[sqlx::test(migrations = "../migrations")]
    async fn leaderboard_columns_by_mode(pool: PgPool) {
        for (github_id, name, has_food) in [
            (2331, "Standard 11x11", true),
            (2332, "Constrictor 11x11", false),
        ] {
            let (leaderboard_id, entry_id) = ranked_entry(&pool, github_id, name).await;
            for path in [
                format!("/leaderboards/{leaderboard_id}"),
                format!("/leaderboards/{leaderboard_id}?sort=food_eaten"),
            ] {
                let body = get(&pool, &path).await;
                assert!(body.contains(">Win %<"), "{path}");
                assert!(!body.contains("1st place"), "{path}");
                assert_eq!(body.contains(">Food<"), has_food, "{path} food column");
                assert_eq!(body.contains("Food eaten"), has_food, "{path} food sort");
                assert_eq!(body.contains("Top eaters"), has_food, "{path} top eaters");
            }

            let body = get(
                &pool,
                &format!("/leaderboards/{leaderboard_id}/entries/{entry_id}"),
            )
            .await;
            assert_eq!(body.contains("Food Eaten"), has_food, "{name} entry scores");
            assert!(body.contains("Win Rate"), "{name} entry scores");
        }
    }
}

#[cfg(test)]
mod cosmetics_route_tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::models::leaderboard::{self, MIN_GAMES_FOR_RANKING};
    use crate::state::AppState;

    async fn get(pool: &PgPool, path: &str) -> (StatusCode, String) {
        let app = crate::routes::routes(AppState::test_from_pool(pool.clone()))
            .expect("asset router")
            .into_router();
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn owner(pool: &PgPool, github_id: i64, login: &str) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login)
             VALUES ($1, $2) RETURNING user_id",
            github_id,
            login
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Adds a snake with the given cosmetic look (color, head, tail) to the
    /// leaderboard at the given games played, returning its entry id.
    async fn entry(
        pool: &PgPool,
        leaderboard_id: Uuid,
        user_id: Uuid,
        name: &str,
        games_played: i32,
        look: (&str, &str, &str),
    ) -> Uuid {
        let battlesnake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url, color, head, tail)
             VALUES ($1, $2, 'http://snake', $3, $4, $5) RETURNING battlesnake_id",
            user_id,
            name,
            look.0,
            look.1,
            look.2
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let entry_id = leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id)
            .await
            .unwrap()
            .expect("live snake joins")
            .leaderboard_entry_id;
        sqlx::query!(
            "UPDATE leaderboard_entries SET games_played = $2 WHERE leaderboard_entry_id = $1",
            entry_id,
            games_played
        )
        .execute(pool)
        .await
        .unwrap();
        entry_id
    }

    /// Ranked and placement rows each show their own snake's head and tail
    /// art (the mini avatar that replaced the plain colour chip), and the
    /// declared colour flows through `--snake`.
    #[sqlx::test(migrations = "../migrations")]
    async fn ranked_and_placement_rows_show_their_own_head_and_tail(pool: PgPool) {
        let leaderboard_id = leaderboard::get_active_leaderboards(&pool)
            .await
            .unwrap()
            .first()
            .expect("migrations seed an active leaderboard")
            .leaderboard_id;

        let user = owner(&pool, 9931001, "gh-cosmetic").await;
        entry(
            &pool,
            leaderboard_id,
            user,
            "cosmetic-ranked",
            MIN_GAMES_FOR_RANKING,
            ("#FF4F86", "alligator", "bolt"),
        )
        .await;
        entry(
            &pool,
            leaderboard_id,
            user,
            "cosmetic-placement",
            MIN_GAMES_FOR_RANKING - 1,
            ("", "", ""),
        )
        .await;
        entry(
            &pool,
            leaderboard_id,
            user,
            "cosmetic-puffin",
            MIN_GAMES_FOR_RANKING,
            ("#da8a1a", "beach-puffin-special", "beach-puffin-special"),
        )
        .await;

        let (status, body) = get(&pool, &format!("/leaderboards/{leaderboard_id}")).await;
        assert_eq!(status, StatusCode::OK);

        let head_d = crate::og::board::shape(arena::design_kit::AssetKind::Head, "alligator")
            .expect("alligator in catalog")
            .d;
        let tail_d = crate::og::board::shape(arena::design_kit::AssetKind::Tail, "bolt")
            .expect("bolt in catalog")
            .d;
        let default_d = crate::og::board::shape(arena::design_kit::AssetKind::Head, "default")
            .expect("default in catalog")
            .d;

        // Exactly once each: a regression that gives every row the same art
        // trips these, and the default shape doubling as the fallback means
        // only an exact count distinguishes "default snake rendered" from
        // "custom slug ignored".
        assert_eq!(body.matches(head_d).count(), 1, "alligator head once");
        assert_eq!(body.matches(tail_d).count(), 1, "bolt tail once");
        assert_eq!(body.matches(default_d).count(), 1, "default head once");

        assert!(
            body.contains(r#"style="--snake: #ff4f86""#),
            "declared colour not lowered into --snake"
        );
        assert!(body.contains(r#"class="snake-avatar""#), "{body}");

        // Multi-colour special art keeps its fixed accents end to end
        // (DB slug -> avatar markup): the puffin's orange beak.
        assert!(
            body.contains(r#"style="fill: #e84600""#),
            "puffin beak layer missing from the leaderboard page"
        );
    }

    /// The home page's "Top of the ladder" preview shows the same avatar.
    #[sqlx::test(migrations = "../migrations")]
    async fn home_ladder_preview_shows_head_and_tail(pool: PgPool) {
        let leaderboard_id = leaderboard::get_active_leaderboards(&pool)
            .await
            .unwrap()
            .first()
            .expect("migrations seed an active leaderboard")
            .leaderboard_id;

        let user = owner(&pool, 9931002, "gh-home-cosmetic").await;
        entry(
            &pool,
            leaderboard_id,
            user,
            "home-cosmetic-ranked",
            MIN_GAMES_FOR_RANKING,
            ("#FF4F86", "alligator", "bolt"),
        )
        .await;

        let (status, body) = get(&pool, "/").await;
        assert_eq!(status, StatusCode::OK);
        let head_d = crate::og::board::shape(arena::design_kit::AssetKind::Head, "alligator")
            .expect("alligator in catalog")
            .d;
        assert!(body.contains(head_d), "home ladder avatar missing");
    }
}
