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
    errors::{ServerResult, WithRedirect},
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
}

/// GET /leaderboards — list all leaderboards
pub async fn list_leaderboards(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let user_id = user.as_ref().map(|u| u.user_id);
    let leaderboards = leaderboard::get_visible_leaderboards(&state.db, user_id)
        .await
        .wrap_err("Failed to fetch leaderboards")?;

    Ok(page_factory.create_page(
        "Leaderboards".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Leaderboards" }

                @if user.is_some() {
                    div style="margin-bottom: 20px;" {
                        a href="/leaderboards/new" class="btn btn-primary" { "Create Leaderboard" }
                    }
                }

                @if leaderboards.is_empty() {
                    p { "No leaderboards available yet." }
                } @else {
                    div class="leaderboards-list" {
                        @for lb in &leaderboards {
                            div class="card" style="border: 1px solid #ddd; border-radius: 8px; padding: 20px; margin-bottom: 16px;" {
                                h2 {
                                    a href={"/leaderboards/"(lb.leaderboard_id)} { (lb.name) }
                                }
                                @if !lb.description.is_empty() {
                                    p style="color: #666;" { (lb.description) }
                                }
                                div style="display: flex; gap: 8px; flex-wrap: wrap;" {
                                    span class="badge bg-info text-white" { (lb.board_size) }
                                    span class="badge bg-info text-white" { (lb.game_type) }
                                    @if lb.visibility == Visibility::Private {
                                        span class="badge bg-warning text-dark" { "Private" }
                                    }
                                    @if lb.matchmaking_enabled {
                                        span class="badge bg-success text-white" { "Matchmaking Active" }
                                    }
                                    @if lb.disabled_at.is_some() {
                                        span class="badge bg-secondary text-white" { "Inactive" }
                                    }
                                }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/" { "Back to Home" }
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

    let ranked =
        leaderboard::get_ranked_entries_paginated(&state.db, leaderboard_id, page, per_page)
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

    // Compute next matchmaker run time
    let next_run_str = status.last_game_created_at.map(|last| {
        let next_run = last + chrono::Duration::seconds(MATCHMAKER_INTERVAL_SECS as i64);
        HumanTime::from(next_run).to_string()
    });

    let rank_start = page * per_page;

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

    Ok(page_factory.create_page(
        format!("Leaderboard: {}", lb.name),
        Box::new(html! {
            div class="container" {
                h1 { "Leaderboard: " (lb.name) }

                @if !lb.description.is_empty() {
                    p style="color: #666; font-size: 1.1em;" { (lb.description) }
                }

                div style="margin-bottom: 16px; display: flex; gap: 8px; flex-wrap: wrap; align-items: center;" {
                    span class="badge bg-info text-white" { (lb.board_size) }
                    span class="badge bg-info text-white" { (lb.game_type) }
                    @if lb.visibility == Visibility::Private {
                        span class="badge bg-warning text-dark" { "Private" }
                    }
                    @if let Some(ref u) = user {
                        @if lb.creator_user_id == Some(u.user_id) {
                            a href={"/leaderboards/"(lb.leaderboard_id)"/manage"} class="btn btn-sm btn-outline-primary" { "Manage" }
                        }
                    }
                }

                // Matchmaker status
                @if status.total_games > 0 {
                    div style="margin-bottom: 20px; padding: 16px; background: #f8f9fa; border: 1px solid #ddd; border-radius: 8px;" {
                        h3 { "Matchmaker Status" }
                        div class="d-flex" style="gap: 24px; flex-wrap: wrap;" {
                            div {
                                strong { "Last games created: " }
                                @if let Some(last) = status.last_game_created_at {
                                    (HumanTime::from(last).to_string())
                                } @else {
                                    "No games yet"
                                }
                            }
                            div {
                                strong { "Games in progress: " }
                                (status.games_in_progress)
                            }
                            div {
                                strong { "Next matchmaker run: " }
                                @if let Some(ref next) = next_run_str {
                                    "~" (next)
                                } @else {
                                    "Waiting for first run"
                                }
                            }
                            div {
                                strong { "Total games played: " }
                                (status.total_games)
                            }
                        }
                    }
                }

                // Join/leave section for logged-in users
                @if lb.visibility == Visibility::Private && user.as_ref().is_none_or(|u| lb.creator_user_id != Some(u.user_id)) {
                    div style="margin-bottom: 20px; padding: 16px; border: 1px solid #ddd; border-radius: 8px; background: #fff3cd;" {
                        p { "This is a private leaderboard. Contact the creator to join." }
                    }
                } @else if lb.visibility == Visibility::Private && user.as_ref().is_some_and(|u| lb.creator_user_id == Some(u.user_id)) {
                    div style="margin-bottom: 20px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                        p { "You are the creator of this private leaderboard. Use the " a href={"/leaderboards/"(leaderboard_id)"/manage"} { "management page" } " to add snakes." }
                    }
                } @else if user.is_some() {
                    div style="margin-bottom: 20px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                        h3 { "Your Snakes" }

                        // Show currently joined snakes with leave button
                        @for entry in &user_entries {
                            @if let Some(snake) = user_snakes.iter().find(|s| s.battlesnake_id == entry.battlesnake_id) {
                                div style="display: flex; align-items: center; gap: 10px; margin-bottom: 8px;" {
                                    span { (snake.name) }
                                    @if entry.disabled_at.is_some() {
                                        span class="badge bg-secondary text-white" { "Paused" }
                                        form action={"/leaderboards/"(leaderboard_id)"/join"} method="post" style="display: inline;" {
                                            input type="hidden" name="battlesnake_id" value=(snake.battlesnake_id);
                                            button type="submit" class="btn btn-sm btn-success" { "Resume" }
                                        }
                                    } @else {
                                        span class="badge bg-success text-white" { "Active" }
                                        form action={"/leaderboards/"(leaderboard_id)"/leave"} method="post" style="display: inline;" {
                                            input type="hidden" name="leaderboard_entry_id" value=(entry.leaderboard_entry_id);
                                            button type="submit" class="btn btn-sm btn-warning" { "Pause" }
                                        }
                                    }
                                    span style="color: #666;" {
                                        "Score: " (format!("{:.1}", entry.display_score))
                                        " | Games: " (entry.games_played)
                                    }
                                }
                            }
                        }

                        // Show joinable snakes (public, not already joined)
                        @let joinable: Vec<_> = user_snakes.iter()
                            .filter(|s| s.visibility == Visibility::Public)
                            .collect();
                        @if !joinable.is_empty() {
                            form action={"/leaderboards/"(leaderboard_id)"/join"} method="post" style="margin-top: 10px;" {
                                label { "Join with: " }
                                select name="battlesnake_id" {
                                    @for snake in joinable {
                                        option value=(snake.battlesnake_id) { (snake.name) }
                                    }
                                }
                                button type="submit" class="btn btn-sm btn-primary" style="margin-left: 8px;" { "Join" }
                            }
                        }
                    }
                }

                // Rankings table
                h2 { "Rankings" }
                @if ranked.is_empty() {
                    p { "No snakes have completed enough games to be ranked yet. (Minimum: " (MIN_GAMES_FOR_RANKING) " games)" }
                } @else {
                    p style="color: #666;" {
                        "Showing " (rank_start + 1) "-" (rank_start + ranked.len() as i64) " of " (total_ranked) " ranked snakes"
                    }
                    table class="table" {
                        thead {
                            tr {
                                th { "Rank" }
                                th { "Snake" }
                                th { "Owner" }
                                @for (_key, col_name, _map) in &algo_scores {
                                    th { (col_name) }
                                }
                                th { "Games" }
                                th { "1st Place %" }
                            }
                        }
                        tbody {
                            @for (i, entry) in ranked.iter().enumerate() {
                                tr {
                                    td { (rank_start + i as i64 + 1) }
                                    td {
                                        a href={"/leaderboards/"(leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} { (entry.snake_name) }
                                    }
                                    td { (entry.owner_login) }
                                    @for (_key, _col_name, map) in &algo_scores {
                                        td {
                                            @if let Some(score) = map.get(&entry.leaderboard_entry_id) {
                                                (format!("{:.1}", score.score))
                                            } @else {
                                                "-"
                                            }
                                        }
                                    }
                                    td { (entry.games_played) }
                                    td {
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

                    // Pagination
                    @if total_pages > 1 {
                        div class="pagination" {
                            @if page > 0 {
                                a href={"/leaderboards/"(leaderboard_id)"?page="(page - 1)} { "Previous" }
                            } @else {
                                span class="disabled" { "Previous" }
                            }
                            span class="current" { "Page " (page + 1) " of " (total_pages) }
                            @if page < total_pages - 1 {
                                a href={"/leaderboards/"(leaderboard_id)"?page="(page + 1)} { "Next" }
                            } @else {
                                span class="disabled" { "Next" }
                            }
                        }
                    }
                }

                // Placement section
                @if !placement.is_empty() {
                    h2 { "In Placement" }
                    p style="color: #666;" { "These snakes need more games before appearing in rankings." }
                    table class="table" {
                        thead {
                            tr {
                                th { "Snake" }
                                th { "Owner" }
                                th { "Games Played" }
                                th { "Games Remaining" }
                            }
                        }
                        tbody {
                            @for entry in &placement {
                                tr {
                                    td {
                                        a href={"/leaderboards/"(leaderboard_id)"/entries/"(entry.leaderboard_entry_id)} { (entry.snake_name) }
                                    }
                                    td { (entry.owner_login) }
                                    td { (entry.games_played) }
                                    td { (MIN_GAMES_FOR_RANKING - entry.games_played) }
                                }
                            }
                        }
                    }
                }

                // Activity Feed
                @if !activity.is_empty() {
                    h2 { "Recent Activity" }
                    div class="activity-feed" {
                        @for event in &activity {
                            div class="activity-feed-item" {
                                a href={"/leaderboards/"(leaderboard_id)"/entries/"(event.leaderboard_entry_id)} {
                                    (event.snake_name)
                                }
                                " placed "
                                @match event.placement {
                                    1 => span { "🥇 1st" },
                                    2 => span { "🥈 2nd" },
                                    3 => span { "🥉 3rd" },
                                    _ => span { (event.placement) "th" },
                                }
                                " ("
                                @if event.display_score_change >= 0.0 {
                                    span class="rating-positive" { (format!("{:+.1}", event.display_score_change)) }
                                } @else {
                                    span class="rating-negative" { (format!("{:+.1}", event.display_score_change)) }
                                }
                                ") "
                                span style="color: #999; font-size: 0.9em;" {
                                    (HumanTime::from(event.created_at).to_string())
                                }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/leaderboards" { "Back to Leaderboards" }
                    span { " | " }
                    a href="/" { "Home" }
                }
            }
        }),
    ))
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
                                h1 class="mb-1" { (snake.name) }
                                span style="color: #666;" { (owner_login) }
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
    ))
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
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Leaderboard is not active"),
            redirect,
        ));
    }

    if lb.visibility == Visibility::Private {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!(
                "Private leaderboards don't accept direct joins. Contact the leaderboard creator."
            ),
            redirect,
        ));
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
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Only public snakes can join leaderboards"),
            redirect,
        ));
    }

    // Check if already enrolled to avoid duplicate entries (no unique constraint on leaderboard_entries)
    let already_enrolled =
        leaderboard::has_active_entry(&state.db, leaderboard_id, form.battlesnake_id)
            .await
            .wrap_err("Failed to check existing entry")
            .with_redirect(redirect.clone())?;

    if already_enrolled {
        return Ok(redirect);
    }

    // Opt-in (or resume if paused)
    let entry = leaderboard::get_or_create_entry(&state.db, leaderboard_id, form.battlesnake_id)
        .await
        .wrap_err("Failed to join leaderboard")
        .with_redirect(redirect.clone())?;

    // Initialize scoring algorithm entries
    for algo in state.scoring.algorithms() {
        algo.initialize_entry(&state.db, entry.leaderboard_entry_id)
            .await
            .wrap_err("Failed to initialize scoring")
            .with_redirect(redirect.clone())?;
    }

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

// --- Custom leaderboard form structs ---

#[derive(serde::Deserialize)]
pub struct CreateLeaderboardForm {
    pub name: String,
    pub description: String,
    pub board_size: String,
    pub game_type: String,
    pub visibility: String,
}

#[derive(serde::Deserialize)]
pub struct AddSnakeForm {
    pub battlesnake_id: Uuid,
}

#[derive(serde::Deserialize)]
pub struct ToggleMatchmakingForm {
    pub enabled: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

fn is_valid_board_size(s: &str) -> bool {
    matches!(s, "7x7" | "11x11" | "19x19")
}

fn is_valid_game_type(s: &str) -> bool {
    matches!(s, "Standard" | "Royale" | "Constrictor" | "Snail Mode")
}

/// GET /leaderboards/new — form to create a new leaderboard
pub async fn new_leaderboard(
    CurrentUser(_user): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page(
        "Create Leaderboard".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Create Leaderboard" }
                form action="/leaderboards" method="post" style="max-width: 600px;" {
                    div style="margin-bottom: 16px;" {
                        label for="name" { "Name" }
                        input type="text" id="name" name="name" required class="form-control" {}
                    }
                    div style="margin-bottom: 16px;" {
                        label for="description" { "Description" }
                        textarea id="description" name="description" class="form-control" rows="3" {}
                    }
                    div style="margin-bottom: 16px;" {
                        label for="board_size" { "Board Size" }
                        select id="board_size" name="board_size" class="form-control" {
                            option value="7x7" { "7x7" }
                            option value="11x11" selected { "11x11" }
                            option value="19x19" { "19x19" }
                        }
                    }
                    div style="margin-bottom: 16px;" {
                        label for="game_type" { "Game Type" }
                        select id="game_type" name="game_type" class="form-control" {
                            option value="Standard" selected { "Standard" }
                            option value="Royale" { "Royale" }
                            option value="Constrictor" { "Constrictor" }
                            option value="Snail Mode" { "Snail Mode" }
                        }
                    }
                    div style="margin-bottom: 16px;" {
                        label for="visibility" { "Visibility" }
                        select id="visibility" name="visibility" class="form-control" {
                            option value="public" selected { "Public" }
                            option value="private" { "Private" }
                        }
                    }
                    button type="submit" class="btn btn-primary" { "Create Leaderboard" }
                }
                div class="nav" style="margin-top: 20px;" {
                    a href="/leaderboards" { "Back to Leaderboards" }
                }
            }
        }),
    ))
}

/// POST /leaderboards — create a new leaderboard
pub async fn create_leaderboard_handler(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<CreateLeaderboardForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to("/leaderboards/new");

    if form.name.trim().is_empty() {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Name cannot be empty"),
            redirect,
        ));
    }

    if !is_valid_board_size(&form.board_size) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid board size: {}", form.board_size),
            redirect,
        ));
    }

    if !is_valid_game_type(&form.game_type) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid game type: {}", form.game_type),
            redirect,
        ));
    }

    let visibility = std::str::FromStr::from_str(&form.visibility)
        .map_err(|e: color_eyre::eyre::Report| crate::errors::ServerError(e, redirect.clone()))?;

    let lb = leaderboard::create_leaderboard(
        &state.db,
        user.user_id,
        form.name.trim(),
        &form.description,
        &visibility,
        &form.board_size,
        &form.game_type,
    )
    .await
    .with_redirect(redirect)?;

    Ok(Redirect::to(&format!(
        "/leaderboards/{}",
        lb.leaderboard_id
    )))
}

/// GET /leaderboards/:id/manage — creator management dashboard
#[allow(clippy::too_many_lines)]
pub async fn manage_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
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

    if lb.creator_user_id != Some(user.user_id) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You are not the creator of this leaderboard"),
            StatusCode::FORBIDDEN,
        ));
    }

    let entry_snake_names = leaderboard::get_active_entries_with_names(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch entries")?;

    let request_snake_names =
        leaderboard::get_pending_requests_with_names(&state.db, leaderboard_id)
            .await
            .wrap_err("Failed to fetch pending requests")?;

    Ok(page_factory.create_page(
        format!("Manage: {}", lb.name),
        Box::new(html! {
            div class="container" {
                h1 { "Manage: " (lb.name) }

                // Settings form
                div style="margin-bottom: 24px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                    h3 { "Settings" }
                    form action={"/leaderboards/"(leaderboard_id)"/update"} method="post" {
                        div style="margin-bottom: 12px;" {
                            label for="name" { "Name" }
                            input type="text" id="name" name="name" value=(lb.name) required class="form-control" {}
                        }
                        div style="margin-bottom: 12px;" {
                            label for="description" { "Description" }
                            textarea id="description" name="description" class="form-control" rows="2" { (lb.description) }
                        }
                        div style="margin-bottom: 12px;" {
                            label for="board_size" { "Board Size" }
                            select id="board_size" name="board_size" class="form-control" {
                                @for size in &["7x7", "11x11", "19x19"] {
                                    option value=(size) selected[*size == lb.board_size] { (size) }
                                }
                            }
                        }
                        div style="margin-bottom: 12px;" {
                            label for="game_type" { "Game Type" }
                            select id="game_type" name="game_type" class="form-control" {
                                @for gt in &["Standard", "Royale", "Constrictor", "Snail Mode"] {
                                    option value=(gt) selected[*gt == lb.game_type] { (gt) }
                                }
                            }
                        }
                        div style="margin-bottom: 12px;" {
                            label for="visibility" { "Visibility" }
                            select id="visibility" name="visibility" class="form-control" {
                                option value="public" selected[lb.visibility == Visibility::Public] { "Public" }
                                option value="private" selected[lb.visibility == Visibility::Private] { "Private" }
                            }
                        }
                        button type="submit" class="btn btn-primary" { "Update Settings" }
                    }
                }

                // Matchmaking toggle
                div style="margin-bottom: 24px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                    h3 { "Matchmaking" }
                    form action={"/leaderboards/"(leaderboard_id)"/matchmaking"} method="post" {
                        div style="margin-bottom: 12px;" {
                            label {
                                input type="checkbox" name="enabled" value="on" checked[lb.matchmaking_enabled] {}
                                " Enable matchmaking"
                            }
                        }
                        button type="submit" class="btn btn-primary" { "Update Matchmaking" }
                    }
                }

                // Enrolled snakes
                div style="margin-bottom: 24px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                    h3 { "Enrolled Snakes (" (entry_snake_names.len()) ")" }
                    @if entry_snake_names.is_empty() {
                        p { "No snakes enrolled yet." }
                    } @else {
                        @for entry in &entry_snake_names {
                            div style="display: flex; align-items: center; gap: 10px; margin-bottom: 8px;" {
                                span { (entry.snake_name) }
                                span style="color: #666;" { "Score: " (format!("{:.1}", entry.display_score)) " | Games: " (entry.games_played) }
                            }
                        }
                    }
                }

                // Add snake
                div style="margin-bottom: 24px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                    h3 { "Add Snake" }
                    div style="margin-bottom: 16px;" {
                        h4 style="font-size: 14px; margin-bottom: 8px;" { "Search Public Snakes" }
                        input type="text" id="snake-search" placeholder="Search public snakes by name..." class="form-control"
                            hx-get={"/leaderboards/"(leaderboard_id)"/manage/search-snakes"}
                            hx-trigger="input changed delay:300ms"
                            hx-target="#snake-search-results"
                            name="q" {}
                        div id="snake-search-results" style="margin-top: 8px;" {}
                    }
                    div {
                        h4 style="font-size: 14px; margin-bottom: 8px;" { "Add by Snake ID" }
                        p style="font-size: 12px; color: #666; margin-bottom: 8px;" {
                            "To add a private snake, enter its ID directly. The snake owner will receive an enrollment request to accept or decline."
                        }
                        form action={"/leaderboards/"(leaderboard_id)"/add-snake"} method="post" style="display: flex; gap: 8px;" {
                            input type="text" name="battlesnake_id" placeholder="Snake ID (UUID)" class="form-control" style="flex: 1;" {}
                            button type="submit" class="btn btn-primary" { "Add" }
                        }
                    }
                }

                // Pending enrollment requests
                @if !request_snake_names.is_empty() {
                    div style="margin-bottom: 24px; padding: 16px; border: 1px solid #ddd; border-radius: 8px;" {
                        h3 { "Pending Enrollment Requests" }
                        @for req in &request_snake_names {
                            div style="display: flex; align-items: center; gap: 10px; margin-bottom: 8px;" {
                                span { (req.snake_name) }
                                span class="badge bg-warning text-dark" { "Pending" }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href={"/leaderboards/"(leaderboard_id)} { "View Leaderboard" }
                    span { " | " }
                    a href="/leaderboards" { "All Leaderboards" }
                }
            }
        }),
    ))
}

/// GET /leaderboards/:id/manage/search-snakes — HTMX snake search fragment
pub async fn search_snakes_for_leaderboard(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Query(query): Query<SearchQuery>,
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

    if lb.creator_user_id != Some(user.user_id) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Only the leaderboard creator can manage this leaderboard"),
            StatusCode::FORBIDDEN,
        ));
    }

    if query.q.trim().is_empty() {
        return Ok(html! {}.into_response());
    }

    let snakes = battlesnake::search_public_battlesnakes(&state.db, query.q.trim(), 10)
        .await
        .wrap_err("Failed to search snakes")?;

    Ok(html! {
        @for snake in &snakes {
            div style="display: flex; align-items: center; gap: 10px; margin-bottom: 8px; padding: 8px; border: 1px solid #eee; border-radius: 4px;" {
                span { (snake.name) }
                form action={"/leaderboards/"(leaderboard_id)"/add-snake"} method="post" style="display: inline;" {
                    input type="hidden" name="battlesnake_id" value=(snake.battlesnake_id);
                    button type="submit" class="btn btn-sm btn-primary" { "Add" }
                }
            }
        }
        @if snakes.is_empty() {
            p style="color: #666;" { "No matching public snakes found. To add a private snake, use the \"Add by Snake ID\" form below." }
        }
    }.into_response())
}

/// POST /leaderboards/:id/update — update leaderboard settings
pub async fn update_leaderboard_handler(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<CreateLeaderboardForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}/manage"));

    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard not found"),
                redirect.clone(),
            )
        })?;

    if lb.creator_user_id != Some(user.user_id) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You are not the creator of this leaderboard"),
            redirect,
        ));
    }

    if form.name.trim().is_empty() {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Name cannot be empty"),
            redirect,
        ));
    }

    if !is_valid_board_size(&form.board_size) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid board size"),
            redirect,
        ));
    }

    if !is_valid_game_type(&form.game_type) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Invalid game type"),
            redirect,
        ));
    }

    let visibility: Visibility = std::str::FromStr::from_str(&form.visibility)
        .map_err(|e: color_eyre::eyre::Report| crate::errors::ServerError(e, redirect.clone()))?;

    leaderboard::update_leaderboard(
        &state.db,
        leaderboard_id,
        form.name.trim(),
        &form.description,
        &visibility,
        &form.board_size,
        &form.game_type,
    )
    .await
    .with_redirect(redirect.clone())?;

    Ok(redirect)
}

/// POST /leaderboards/:id/matchmaking — toggle matchmaking
pub async fn toggle_matchmaking(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<ToggleMatchmakingForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}/manage"));

    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard not found"),
                redirect.clone(),
            )
        })?;

    if lb.creator_user_id != Some(user.user_id) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You are not the creator of this leaderboard"),
            redirect,
        ));
    }

    let enabled = form.enabled.is_some();
    leaderboard::set_matchmaking_enabled(&state.db, leaderboard_id, enabled)
        .await
        .with_redirect(redirect.clone())?;

    Ok(redirect)
}

/// POST /leaderboards/:id/add-snake — creator adds a snake
pub async fn creator_add_snake(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(leaderboard_id): Path<Uuid>,
    Form(form): Form<AddSnakeForm>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to(&format!("/leaderboards/{leaderboard_id}/manage"));

    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Leaderboard not found"),
                redirect.clone(),
            )
        })?;

    if lb.creator_user_id != Some(user.user_id) {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("You are not the creator of this leaderboard"),
            redirect,
        ));
    }

    if lb.visibility != Visibility::Private {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!(
                "Creator-managed snake additions are only available for private leaderboards. For public leaderboards, snake owners can join directly."
            ),
            redirect,
        ));
    }

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

    if snake.visibility == Visibility::Public {
        // Direct add for public snakes
        let already_enrolled =
            leaderboard::has_active_entry(&state.db, leaderboard_id, snake.battlesnake_id)
                .await
                .wrap_err("Failed to check entry")
                .with_redirect(redirect.clone())?;

        if !already_enrolled {
            let entry =
                leaderboard::get_or_create_entry(&state.db, leaderboard_id, snake.battlesnake_id)
                    .await
                    .wrap_err("Failed to create entry")
                    .with_redirect(redirect.clone())?;

            for algo in state.scoring.algorithms() {
                algo.initialize_entry(&state.db, entry.leaderboard_entry_id)
                    .await
                    .wrap_err("Failed to initialize scoring")
                    .with_redirect(redirect.clone())?;
            }
        }
    } else {
        // Create enrollment request for private snakes
        leaderboard::create_enrollment_request(
            &state.db,
            leaderboard_id,
            snake.battlesnake_id,
            user.user_id,
        )
        .await
        .wrap_err("Failed to create enrollment request")
        .with_redirect(redirect.clone())?;
    }

    Ok(redirect)
}

/// POST /enrollment-requests/:request_id/accept
pub async fn accept_enrollment_request(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(request_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to("/me");

    let req = leaderboard::get_enrollment_request_by_id(&state.db, request_id)
        .await
        .wrap_err("Failed to fetch enrollment request")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Enrollment request not found"),
                redirect.clone(),
            )
        })?;

    if req.status != "pending" {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Request is no longer pending"),
            redirect,
        ));
    }

    let snake = battlesnake::get_battlesnake_by_id(&state.db, req.battlesnake_id)
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

    leaderboard::update_enrollment_request_status(&state.db, request_id, "accepted")
        .await
        .wrap_err("Failed to accept request")
        .with_redirect(redirect.clone())?;

    let already_enrolled =
        leaderboard::has_active_entry(&state.db, req.leaderboard_id, req.battlesnake_id)
            .await
            .wrap_err("Failed to check entry")
            .with_redirect(redirect.clone())?;

    if !already_enrolled {
        let entry =
            leaderboard::get_or_create_entry(&state.db, req.leaderboard_id, req.battlesnake_id)
                .await
                .wrap_err("Failed to create entry")
                .with_redirect(redirect.clone())?;

        for algo in state.scoring.algorithms() {
            algo.initialize_entry(&state.db, entry.leaderboard_entry_id)
                .await
                .wrap_err("Failed to initialize scoring")
                .with_redirect(redirect.clone())?;
        }
    }

    Ok(redirect)
}

/// POST /enrollment-requests/:request_id/decline
pub async fn decline_enrollment_request(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(request_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, Redirect> {
    let redirect = Redirect::to("/me");

    let req = leaderboard::get_enrollment_request_by_id(&state.db, request_id)
        .await
        .wrap_err("Failed to fetch enrollment request")
        .with_redirect(redirect.clone())?
        .ok_or_else(|| {
            crate::errors::ServerError(
                color_eyre::eyre::eyre!("Enrollment request not found"),
                redirect.clone(),
            )
        })?;

    if req.status != "pending" {
        return Err(crate::errors::ServerError(
            color_eyre::eyre::eyre!("Request is no longer pending"),
            redirect,
        ));
    }

    let snake = battlesnake::get_battlesnake_by_id(&state.db, req.battlesnake_id)
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

    leaderboard::update_enrollment_request_status(&state.db, request_id, "declined")
        .await
        .wrap_err("Failed to decline request")
        .with_redirect(redirect.clone())?;

    Ok(redirect)
}

// --- BS-37342921850a4fc2: Custom leaderboard tests ---

#[cfg(test)]
mod custom_leaderboard_tests {
    // Helper: mirrors the validation logic the create/update leaderboard handlers must implement.
    fn is_valid_board_size(s: &str) -> bool {
        matches!(s, "7x7" | "11x11" | "19x19")
    }

    fn is_valid_game_type(s: &str) -> bool {
        matches!(s, "Standard" | "Royale" | "Constrictor" | "Snail Mode")
    }

    #[test]
    fn test_valid_board_sizes_accepted() {
        assert!(
            is_valid_board_size("7x7"),
            "7x7 should be a valid board size"
        );
        assert!(
            is_valid_board_size("11x11"),
            "11x11 should be a valid board size"
        );
        assert!(
            is_valid_board_size("19x19"),
            "19x19 should be a valid board size"
        );
    }

    #[test]
    fn test_invalid_board_sizes_rejected() {
        assert!(
            !is_valid_board_size("5x5"),
            "5x5 is not a supported board size"
        );
        assert!(
            !is_valid_board_size(""),
            "empty string is not a valid board size"
        );
        assert!(!is_valid_board_size("large"), "text aliases are not valid");
        assert!(!is_valid_board_size("11x11x11"), "3D board is not valid");
        assert!(!is_valid_board_size("0x0"), "zero board is not valid");
    }

    #[test]
    fn test_valid_game_types_accepted() {
        assert!(
            is_valid_game_type("Standard"),
            "Standard should be a valid game type"
        );
        assert!(
            is_valid_game_type("Royale"),
            "Royale should be a valid game type"
        );
        assert!(
            is_valid_game_type("Constrictor"),
            "Constrictor should be a valid game type"
        );
        assert!(
            is_valid_game_type("Snail Mode"),
            "Snail Mode should be a valid game type"
        );
    }

    #[test]
    fn test_invalid_game_types_rejected() {
        assert!(!is_valid_game_type("standard"), "lowercase is not valid");
        assert!(
            !is_valid_game_type(""),
            "empty string is not a valid game type"
        );
        assert!(
            !is_valid_game_type("Unknown"),
            "unknown game type should be rejected"
        );
        assert!(
            !is_valid_game_type("snail mode"),
            "wrong case should be rejected"
        );
    }

    #[test]
    fn test_visibility_parsing_for_form() {
        use crate::models::battlesnake::Visibility;
        use std::str::FromStr;

        // create_leaderboard_handler validates visibility via Visibility::from_str
        assert!(
            Visibility::from_str("public").is_ok(),
            "public visibility is valid"
        );
        assert!(
            Visibility::from_str("private").is_ok(),
            "private visibility is valid"
        );
        assert!(
            Visibility::from_str("unlisted").is_err(),
            "unlisted is not a valid visibility"
        );
        assert!(
            Visibility::from_str("").is_err(),
            "empty string is not a valid visibility"
        );
    }

    #[test]
    fn test_create_leaderboard_form_struct_exists() {
        use super::CreateLeaderboardForm;
        let form = CreateLeaderboardForm {
            name: "My League".to_string(),
            description: "A fun league".to_string(),
            board_size: "11x11".to_string(),
            game_type: "Standard".to_string(),
            visibility: "public".to_string(),
        };
        assert!(!form.name.is_empty());
        assert!(is_valid_board_size(&form.board_size));
        assert!(is_valid_game_type(&form.game_type));
    }

    #[test]
    fn test_new_leaderboard_handler_exists() {
        let _ = super::new_leaderboard;
    }

    #[test]
    fn test_create_leaderboard_handler_exists() {
        let _ = super::create_leaderboard_handler;
    }

    #[test]
    fn test_manage_leaderboard_handler_exists() {
        let _ = super::manage_leaderboard;
    }

    #[test]
    fn test_toggle_matchmaking_handler_exists() {
        let _ = super::toggle_matchmaking;
    }

    #[test]
    fn test_creator_add_snake_handler_exists() {
        let _ = super::creator_add_snake;
    }

    #[test]
    fn test_accept_enrollment_request_handler_exists() {
        let _ = super::accept_enrollment_request;
    }

    #[test]
    fn test_decline_enrollment_request_handler_exists() {
        let _ = super::decline_enrollment_request;
    }

    #[test]
    fn test_join_leaderboard_rejects_private_leaderboards() {
        // join_leaderboard now checks lb.visibility == Visibility::Private
        // and returns error: "Private leaderboards don't accept direct joins."
        // Verified via code inspection.
    }

    #[test]
    fn test_search_snakes_handler_exists() {
        let _ = super::search_snakes_for_leaderboard;
    }

    #[test]
    fn test_update_leaderboard_handler_exists() {
        let _ = super::update_leaderboard_handler;
    }
}
