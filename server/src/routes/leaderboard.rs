use std::collections::HashMap;

use axum::{
    Form,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use chrono_humanize::HumanTime;
use color_eyre::eyre::Context as _;
use maud::html;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    cron::MATCHMAKER_INTERVAL_SECS,
    customizations::chip_color,
    errors::{ServerResult, WithRedirect},
    models::snake_health_status,
    models::{
        battlesnake::{self, Visibility},
        leaderboard::{self, MIN_GAMES_FOR_RANKING},
        user,
    },
    routes::auth::{CurrentUser, OptionalUser},
    scoring::EntryScore,
    state::AppState,
};

#[derive(serde::Deserialize)]
pub struct PaginationParams {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub sort: leaderboard::LeaderboardSort,
}

/// GET /leaderboards — list all leaderboards
pub async fn list_leaderboards(
    State(state): State<AppState>,
    OptionalUser(_user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let leaderboards = leaderboard::get_all_leaderboards(&state.db)
        .await
        .wrap_err("Failed to fetch leaderboards")?;

    Ok(page_factory.create_page(
        "Leaderboards".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "Leaderboards" }
                div class="sub" {
                    "Ranked ladders, one per game mode. Join with a public snake and the matchmaker takes it from there."
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
    Path(leaderboard_id): Path<Uuid>,
    Query(pagination): Query<PaginationParams>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard not found"),
                StatusCode::NOT_FOUND,
            )
        })?;

    let all_leaderboards = leaderboard::get_all_leaderboards(&state.db)
        .await
        .wrap_err("Failed to fetch leaderboards")?;

    let per_page: i64 = 50;

    let total_ranked = leaderboard::count_ranked_entries(&state.db, leaderboard_id)
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
        page,
        per_page,
        pagination.sort,
    )
    .await
    .wrap_err("Failed to fetch ranked entries")?;

    let placement = leaderboard::get_placement_entries(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch placement entries")?;

    let status = leaderboard::get_leaderboard_status(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard status")?;

    let activity = leaderboard::get_activity_feed(&state.db, leaderboard_id, 20)
        .await
        .wrap_err("Failed to fetch activity feed")?;

    let top_eaters = leaderboard::get_top_eaters(&state.db, leaderboard_id, 3)
        .await
        .wrap_err("Failed to fetch top eaters")?;

    // Get user's snakes for the join form
    let user_snakes = if let Some(ref u) = user {
        battlesnake::get_battlesnakes_by_user_id(&state.db, u.user_id)
            .await
            .wrap_err("Failed to fetch user's battlesnakes")?
    } else {
        vec![]
    };

    // Get user's entries in this leaderboard
    let user_entries = if let Some(ref u) = user {
        leaderboard::get_user_entries(&state.db, leaderboard_id, u.user_id)
            .await
            .wrap_err("Failed to fetch user's leaderboard entries")?
    } else {
        vec![]
    };

    // Compute next matchmaker run time. When the ladder is starved the
    // matchmaker no-ops, so "last game + interval" would show an ever-staler
    // past timestamp — say why matchmaking is paused instead.
    let enabled_count = leaderboard::get_active_entries(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch active entries")?
        .len();
    let next_run_str = if enabled_count < leaderboard::MIN_MATCH_SIZE {
        None
    } else {
        status.last_game_created_at.map(|last| {
            let next_run = last + chrono::Duration::seconds(MATCHMAKER_INTERVAL_SECS as i64);
            HumanTime::from(next_run).to_string()
        })
    };

    let rank_start = page * per_page;
    let sort_param = pagination.sort.as_str();
    // The active sort's score column stays visible on narrow screens.
    let active_algo_key = match pagination.sort {
        leaderboard::LeaderboardSort::Rating => "weng_lin",
        leaderboard::LeaderboardSort::FoodEaten => "food_eaten",
    };

    // Collect entry IDs from the current page for scoring lookups
    let entry_ids: Vec<Uuid> = ranked
        .iter()
        .chain(placement.iter())
        .map(|e| e.leaderboard_entry_id)
        .collect();

    // Fetch per-algorithm scores for only the visible entries
    let mut algo_scores: Vec<(&str, &str, HashMap<Uuid, EntryScore>)> = vec![];
    for algo in state.scoring.algorithms() {
        let scores = algo
            .get_scores(&state.db, &entry_ids)
            .await
            .wrap_err_with(|| format!("Failed to fetch {} scores", algo.key()))?;
        let map: HashMap<Uuid, EntryScore> = scores
            .into_iter()
            .map(|s| (s.leaderboard_entry_id, s))
            .collect();
        algo_scores.push((algo.key(), algo.score_column_name(), map));
    }

    let description = format!(
        "{} leaderboard on Battlesnake Arena — {} ranked snakes, {} games played.",
        lb.name, total_ranked, status.total_games
    );

    Ok(page_factory.create_page(
        lb.name.clone(),
        Box::new(html! {
            div class="crumb" {
                a href="/leaderboards" { "Leaderboards" }
                " / " (lb.name)
            }
            div class="page-head" {
                h1 { (lb.name) }
                div class="sub" {
                    "Ranked play — register a public snake, join the ladder, and the "
                    "matchmaker starts new games every few minutes."
                }
            }

            @if all_leaderboards.len() > 1 {
                nav class="modes" aria-label="Game modes" {
                    @for other in &all_leaderboards {
                        @if other.leaderboard_id == leaderboard_id {
                            span class="mode on" aria-current="page" { (other.name) }
                        } @else {
                            a class="mode" href={"/leaderboards/"(other.leaderboard_id)} { (other.name) }
                        }
                    }
                }
            }

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
                        @if enabled_count < leaderboard::MIN_MATCH_SIZE {
                            "paused — needs at least "
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

            div class="grid" {
                div {
                    div class="sortbar" {
                        span { "sort" }
                        @if pagination.sort == leaderboard::LeaderboardSort::Rating {
                            span class="on" aria-current="true" { "Rating" }
                        } @else {
                            a href={"/leaderboards/"(leaderboard_id)"?sort=rating"} { "Rating" }
                        }
                        @if pagination.sort == leaderboard::LeaderboardSort::FoodEaten {
                            span class="on" aria-current="true" { "Food eaten" }
                        } @else {
                            a href={"/leaderboards/"(leaderboard_id)"?sort=food_eaten"} { "Food eaten" }
                        }
                    }

                    @if ranked.is_empty() {
                        p class="empty" {
                            "No snakes have completed enough games to be ranked yet. "
                            "(Minimum: " (MIN_GAMES_FOR_RANKING) " games)"
                        }
                    } @else {
                        table class="data" {
                            thead {
                                tr {
                                    th { "#" }
                                    th { "Battlesnake" }
                                    @for (key, col_name, _map) in &algo_scores {
                                        th .r .hide-sm[*key != active_algo_key] { (col_name) }
                                    }
                                    th class="r hide-md" { "Games" }
                                    th class="r hide-sm" { "1st place %" }
                                }
                            }
                            tbody {
                                @for (i, entry) in ranked.iter().enumerate() {
                                    @let rank = rank_start + i as i64 + 1;
                                    @let is_you = user.as_ref().is_some_and(|u| u.user_id == entry.user_id);
                                    tr .top[rank <= 3] .you[is_you] {
                                        td class="rank" { (format!("{rank:02}")) }
                                        td {
                                            div class="snake-cell" {
                                                span class="chip" style={"background:"(chip_color(&entry.snake_color))} {}
                                                span {
                                                    a class="name" href={"/leaderboards/"(leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} {
                                                        (entry.snake_name)
                                                    }
                                                    span class="owner" {
                                                        "by "
                                                        a href={"/users/"(entry.owner_login)} { (entry.owner_login) }
                                                        @if is_you { " — you" }
                                                    }
                                                }
                                            }
                                        }
                                        @for (key, _col_name, map) in &algo_scores {
                                            td .r .rating .hide-sm[*key != active_algo_key] {
                                                @if let Some(score) = map.get(&entry.leaderboard_entry_id) {
                                                    (format!("{:.1}", score.score))
                                                } @else {
                                                    "—"
                                                }
                                            }
                                        }
                                        td class="r num hide-md" { (entry.games_played) }
                                        td class="r num hide-sm" {
                                            @if entry.games_played > 0 {
                                                (format!("{:.0}%", (entry.first_place_finishes as f64 / entry.games_played as f64) * 100.0))
                                            } @else {
                                                "N/A"
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div class="pager" {
                            @if page > 0 {
                                a href={"/leaderboards/"(leaderboard_id)"?sort="(sort_param)"&page="(page - 1)} { "‹ Prev" }
                            }
                            @if total_pages > 1 {
                                span class="cur" { "Page " (page + 1) " of " (total_pages) }
                            }
                            @if page < total_pages - 1 {
                                a href={"/leaderboards/"(leaderboard_id)"?sort="(sort_param)"&page="(page + 1)} { "Next ›" }
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
                                                    span class="chip" style={"background:"(chip_color(&entry.snake_color))} {}
                                                    span {
                                                        a class="name" href={"/leaderboards/"(leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} {
                                                            (entry.snake_name)
                                                        }
                                                        span class="owner" {
                                                            "by "
                                                            a href={"/users/"(entry.owner_login)} { (entry.owner_login) }
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
                                            a href={"/leaderboards/"(leaderboard_id)"/entries/"(event.leaderboard_entry_id)} {
                                                b { (event.snake_name) }
                                            }
                                            " "
                                            span class={"place p"(event.placement)} { (ordinal(event.placement)) }
                                            " "
                                            @if event.display_score_change >= 0.0 {
                                                span class="delta up" { (format!("{:+.1}", event.display_score_change)) }
                                            } @else {
                                                span class="delta down" { (format!("{:+.1}", event.display_score_change)) }
                                            }
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
                                        span class="chip" style={"background:"(chip_color(&snake.color))} {}
                                        span class="mname" { (snake.name) }
                                        @if entry.disabled_at.is_some() {
                                            @if entry.disabled_reason.as_deref() == Some(snake_health_status::DISABLED_REASON_HEALTH) {
                                                a class="badge warn" href={"/battlesnakes/"(snake.battlesnake_id)"/profile"}
                                                    title="Automatically paused: this snake is failing health checks. Resume re-tests it — details on its profile." {
                                                    "Auto-paused"
                                                }
                                            } @else {
                                                span class="badge" { "Paused" }
                                            }
                                            form action={"/leaderboards/"(leaderboard_id)"/join"} method="post" {
                                                input type="hidden" name="battlesnake_id" value=(snake.battlesnake_id);
                                                button type="submit" class="btn sm" aria-label={"Resume " (snake.name)} { "Resume" }
                                            }
                                        } @else {
                                            span class="badge ok" { "Active" }
                                            form action={"/leaderboards/"(leaderboard_id)"/leave"} method="post" {
                                                input type="hidden" name="leaderboard_entry_id" value=(entry.leaderboard_entry_id);
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

                            @let joinable: Vec<_> = user_snakes.iter()
                                .filter(|s| s.visibility == Visibility::Public)
                                .collect();
                            @if !joinable.is_empty() {
                                form class="join-form" action={"/leaderboards/"(leaderboard_id)"/join"} method="post" {
                                    select name="battlesnake_id" aria-label="Snake to join with" {
                                        @for snake in joinable {
                                            option value=(snake.battlesnake_id) { (snake.name) }
                                        }
                                    }
                                    button type="submit" class="btn solid sm" { "Join" }
                                }
                            } @else if user_entries.is_empty() {
                                p class="railp" {
                                    "You need a public snake to join. "
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
                                            a href={"/leaderboards/"(leaderboard_id)"/entries/"(eater.leaderboard_entry_id)} {
                                                b { (eater.snake_name) }
                                            }
                                            span class="owner-inline" {
                                                " by "
                                                a href={"/users/"(eater.owner_login)} { (eater.owner_login) }
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
    .with_description(description))
}

/// GET /leaderboards/:id/entries/:entry_id — snake detail on leaderboard
#[allow(clippy::too_many_lines)]
pub async fn show_leaderboard_entry(
    State(state): State<AppState>,
    OptionalUser(_user): OptionalUser,
    Path((leaderboard_id, entry_id)): Path<(Uuid, Uuid)>,
    Query(pagination): Query<PaginationParams>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard not found"),
                StatusCode::NOT_FOUND,
            )
        })?;

    let entry = leaderboard::get_entry_by_id(&state.db, entry_id)
        .await
        .wrap_err("Failed to fetch leaderboard entry")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard entry not found"),
                StatusCode::NOT_FOUND,
            )
        })?;

    if entry.leaderboard_id != leaderboard_id {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Entry does not belong to this leaderboard"),
            StatusCode::NOT_FOUND,
        ));
    }

    let snake = battlesnake::get_battlesnake_by_id(&state.db, entry.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Snake no longer exists"),
                StatusCode::NOT_FOUND,
            )
        })?;

    let owner = user::get_user_by_id(&state.db, snake.user_id)
        .await
        .wrap_err("Failed to fetch owner")?;

    let owner_login = owner
        .as_ref()
        .map(|o| o.github_login.clone())
        .unwrap_or_else(|| "Unknown".to_string());
    let owner_avatar = owner.as_ref().and_then(|o| o.github_avatar_url.clone());

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

    let rank = leaderboard::get_rank_for_entry(
        &state.db,
        leaderboard_id,
        entry.display_score,
        entry.games_played,
    )
    .await
    .wrap_err("Failed to get rank")?;

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

        let labels: Vec<(String, String)> = (0..=grid_count)
            .map(|i| {
                let score = y_max - (i as f64 / grid_count as f64) * (y_max - y_min);
                let y_pos = format!("{:.0}", 10.0 + (i as f64 / grid_count as f64) * 200.0 + 4.0);
                (format!("{score:.0}"), y_pos)
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
    let recent_form: Vec<i32> = recent_games.iter().map(|h| h.placement).collect();

    // Fetch per-algorithm scores for this entry
    let mut algo_entry_scores: Vec<(&str, &str, Option<EntryScore>)> = vec![];
    for algo in state.scoring.algorithms() {
        let score = algo
            .get_entry_score(&state.db, entry_id)
            .await
            .wrap_err_with(|| format!("Failed to fetch {} entry score", algo.key()))?;
        algo_entry_scores.push((algo.key(), algo.display_name(), score));
    }

    let description = format!(
        "{} by {} on the {} leaderboard — rating {:.1}, {} games played.",
        snake.name, owner_login, lb.name, entry.display_score, entry.games_played
    );

    Ok(page_factory.create_page(
        format!("{} - {}", snake.name, lb.name),
        Box::new(html! {
            div class="container" {
                // Header
                div class="card mb-4" {
                    div class="card-body" {
                        div class="d-flex align-items-center" style="gap: 16px;" {
                            @if let Some(ref avatar_url) = owner_avatar {
                                img src=(avatar_url) alt="Avatar" style="width: 48px; height: 48px; border-radius: 50%;" {}
                            }
                            div {
                                h1 class="mb-1" {
                                    a href={"/battlesnakes/"(snake.battlesnake_id)"/profile"} { (snake.name) }
                                }
                                @if owner.is_some() {
                                    a href={"/users/"(owner_login)} style="color: #666;" { (owner_login) }
                                } @else {
                                    span style="color: #666;" { (owner_login) }
                                }
                                span { " on " }
                                a href={"/leaderboards/"(leaderboard_id)} { (lb.name) }
                            }
                        }
                        div style="margin-top: 12px; display: flex; gap: 16px; align-items: center;" {
                            span style="font-size: 1.5em; font-weight: bold;" {
                                "Rating: " (format!("{:.1}", entry.display_score))
                            }
                            span {
                                "Rank: "
                                @if let Some(r) = rank {
                                    "#" (r)
                                } @else {
                                    "In Placement"
                                }
                            }
                            a href={"/battlesnakes/"(entry.battlesnake_id)"/profile"} class="btn btn-sm btn-secondary" { "Snake Profile" }
                        }
                    }
                }

                // Summary Stats
                h2 { "Summary" }
                div class="d-flex" style="gap: 16px; flex-wrap: wrap; margin-bottom: 20px;" {
                    div class="card" style="flex: 1; min-width: 120px;" {
                        div class="card-body" {
                            h5 { "Games" }
                            p style="font-size: 2em; margin: 0;" { (entry.games_played) }
                        }
                    }
                    div class="card" style="flex: 1; min-width: 120px;" {
                        div class="card-body" {
                            h5 { "1st Place" }
                            p style="font-size: 2em; margin: 0;" { (entry.first_place_finishes) }
                        }
                    }
                    div class="card" style="flex: 1; min-width: 120px;" {
                        div class="card-body" {
                            h5 { "Other" }
                            p style="font-size: 2em; margin: 0;" { (entry.non_first_finishes) }
                        }
                    }
                    div class="card" style="flex: 1; min-width: 120px;" {
                        div class="card-body" {
                            h5 { "Win Rate" }
                            p style="font-size: 2em; margin: 0;" {
                                @if entry.games_played > 0 {
                                    (format!("{:.0}%", entry.first_place_finishes as f64 / entry.games_played as f64 * 100.0))
                                } @else {
                                    "N/A"
                                }
                            }
                        }
                    }
                    div class="card" style="flex: 1; min-width: 120px;" {
                        div class="card-body" {
                            h5 { "Rating" }
                            p style="font-size: 2em; margin: 0;" { (format!("{:.1}", entry.display_score)) }
                        }
                    }
                    div class="card" style="flex: 1; min-width: 120px;" {
                        div class="card-body" {
                            h5 { "Recent Form" }
                            p style="font-size: 1.5em; margin: 0;" {
                                @for p in &recent_form {
                                    @match *p {
                                        1 => span { "🥇" },
                                        2 => span { "🥈" },
                                        3 => span { "🥉" },
                                        _ => span { "📍" },
                                    }
                                }
                                @if recent_form.is_empty() {
                                    span style="color: #999;" { "-" }
                                }
                            }
                        }
                    }
                }

                // Scores by Algorithm
                h2 { "Scores by Algorithm" }
                div class="d-flex" style="gap: 16px; flex-wrap: wrap; margin-bottom: 20px;" {
                    @for (_key, display_name, score) in &algo_entry_scores {
                        div class="card" style="flex: 1; min-width: 200px;" {
                            div class="card-body" {
                                h5 { (display_name) }
                                @if let Some(s) = score {
                                    p style="font-size: 2em; margin: 0;" { (format!("{:.1}", s.score)) }
                                    @for (detail_name, detail_value) in &s.details {
                                        p style="margin: 2px 0; color: #666; font-size: 0.9em;" {
                                            (detail_name) ": " (detail_value)
                                        }
                                    }
                                } @else {
                                    p style="color: #999;" { "No data" }
                                }
                            }
                        }
                    }
                }

                // Rating Chart
                h2 { "Rating Trajectory" }
                div class="rating-chart-container" {
                    @if rating_points.len() >= 2 {
                        svg width="100%" viewBox="0 0 620 220" style="border: 1px solid #ddd; border-radius: 8px;" {
                            rect x="0" y="0" width="620" height="220" fill="#fafafa" {}
                            @for y_line in &grid_y_positions {
                                line x1="40" y1=(y_line) x2="600" y2=(y_line)
                                     stroke="#eee" stroke-width="1" {}
                            }
                            polyline
                                points=(points_str)
                                fill="none" stroke="#4a90d9" stroke-width="2" {}
                            @for (label, y_pos) in &y_labels {
                                text x="35" y=(y_pos) text-anchor="end" font-size="11" fill="#666" { (label) }
                            }
                        }
                    } @else {
                        p { "Not enough data for chart" }
                    }
                }

                // Game History
                h2 { "Game History" }
                @if history.is_empty() {
                    p { "No games played yet." }
                } @else {
                    table class="table" {
                        thead {
                            tr {
                                th { "Date" }
                                th { "Opponents" }
                                th { "Placement" }
                                th { "Rating Change" }
                                th { "Food" }
                                th { "Replay" }
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
                                            span style="color: #999;" { "-" }
                                        }
                                    }
                                    td {
                                        @match game.placement {
                                            1 => span class="badge bg-warning text-dark" { "🥇 1st" },
                                            2 => span class="badge bg-secondary text-white" { "🥈 2nd" },
                                            3 => span class="badge bg-danger text-white" { "🥉 3rd" },
                                            _ => span class="badge bg-dark text-white" { (game.placement) "th" },
                                        }
                                    }
                                    td {
                                        @if game.display_score_change >= 0.0 {
                                            span class="rating-positive" { (format!("{:+.1}", game.display_score_change)) }
                                        } @else {
                                            span class="rating-negative" { (format!("{:+.1}", game.display_score_change)) }
                                        }
                                    }
                                    td { (game.food_eaten) }
                                    td {
                                        a href={"/games/"(game.game_id)} class="btn btn-sm btn-primary" { "Watch" }
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

                div class="nav" style="margin-top: 20px;" {
                    a href={"/leaderboards/"(leaderboard_id)} { "Back to Leaderboard" }
                    span { " | " }
                    a href="/leaderboards" { "All Leaderboards" }
                }
            }
        }),
    )
    .with_description(description))
}

#[derive(serde::Deserialize)]
pub struct JoinLeaveForm {
    pub battlesnake_id: Uuid,
}

#[derive(serde::Deserialize)]
pub struct LeaveForm {
    pub leaderboard_entry_id: Uuid,
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

    // Verify snake belongs to user and is public
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

    if snake.visibility != Visibility::Public {
        flasher
            .error(format!(
                "{} is private — only public snakes can join leaderboards. \
                 Make it public on its edit page first.",
                snake.name
            ))
            .await
            .wrap_err("Failed to flash")
            .with_redirect(redirect.clone())?;
        return Ok(redirect);
    }

    // Opt-in (or resume if paused)
    let entry = leaderboard::get_or_create_entry(&state.db, leaderboard_id, form.battlesnake_id)
        .await
        .wrap_err("Failed to join leaderboard")
        .with_redirect(redirect.clone())?;

    // A snake the health sweeper pulled keeps its failure streak until it's
    // reactivated, so resuming only the entry would get re-paused on the
    // very next sweep. Resume means "put my snake back in rotation": clear
    // the streak too, exactly like the profile page's Resume Matchmaking.
    snake_health_status::reactivate(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to reset snake health status")
        .with_redirect(redirect.clone())?;

    // Initialize scoring algorithm entries
    for algo in state.scoring.algorithms() {
        algo.initialize_entry(&state.db, entry.leaderboard_entry_id)
            .await
            .wrap_err("Failed to initialize scoring")
            .with_redirect(redirect.clone())?;
    }

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

/// POST /leaderboards/:id/leave — pause a snake
pub async fn leave_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<LeaveForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}"));

    // Find the specific entry by ID
    let entry = leaderboard::get_entry_by_id(&state.db, form.leaderboard_entry_id)
        .await
        .wrap_err("Failed to fetch entry")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard entry not found"),
                redirect.clone(),
            )
        })?;

    // Security: verify this entry belongs to the requested leaderboard
    if entry.leaderboard_id != leaderboard_id {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Entry does not belong to this leaderboard"),
            redirect,
        ));
    }

    // Verify snake belongs to user
    let snake = battlesnake::get_battlesnake_by_id(&state.db, entry.battlesnake_id)
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

    leaderboard::set_disabled(
        &state.db,
        entry.leaderboard_entry_id,
        Some(chrono::Utc::now()),
    )
    .await
    .wrap_err("Failed to pause entry")
    .with_redirect(redirect.clone())?;

    Ok(redirect)
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
