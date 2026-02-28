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
    OptionalUser(_user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let leaderboards = leaderboard::get_all_leaderboards(&state.db)
        .await
        .wrap_err("Failed to fetch leaderboards")?;

    Ok(page_factory.create_page(
        "Leaderboards".to_string(),
        Box::new(html! {
            div class="container" {
                h1 { "Leaderboards" }

                @if leaderboards.is_empty() {
                    p { "No leaderboards available yet." }
                } @else {
                    div class="leaderboards-list" {
                        @for lb in &leaderboards {
                            div class="card" style="border: 1px solid #ddd; border-radius: 8px; padding: 20px; margin-bottom: 16px;" {
                                h2 {
                                    a href={"/leaderboards/"(lb.leaderboard_id)} { (lb.name) }
                                }
                                @if lb.disabled_at.is_some() {
                                    span class="badge bg-secondary text-white" { "Inactive" }
                                } @else {
                                    span class="badge bg-success text-white" { "Active" }
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

    Ok(page_factory.create_page(
        format!("Leaderboard: {}", lb.name),
        Box::new(html! {
            div class="container" {
                h1 { "Leaderboard: " (lb.name) }

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
                @if user.is_some() {
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
                                th { "Score" }
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
                                    td { (format!("{:.1}", entry.display_score)) }
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
    let owner_avatar = owner
        .as_ref()
        .and_then(|o| o.github_avatar_url.clone())
        .unwrap_or_default();

    let page = pagination.page.unwrap_or(0).max(0);
    let per_page: i64 = 20;

    let history = leaderboard::get_game_history_for_entry(&state.db, entry_id, page, per_page)
        .await
        .wrap_err("Failed to fetch game history")?;

    let total_games = leaderboard::count_game_results_for_entry(&state.db, entry_id)
        .await
        .wrap_err("Failed to count game results")?;

    let total_pages = if total_games > 0 {
        (total_games + per_page - 1) / per_page
    } else {
        1
    };
    let page = page.min(total_pages - 1);

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

    // Recent form: last 5 games
    let recent_form: Vec<i32> = history.iter().take(5).map(|h| h.placement).collect();

    Ok(page_factory.create_page(
        format!("{} - {}", snake.name, lb.name),
        Box::new(html! {
            div class="container" {
                // Header
                div class="card mb-4" {
                    div class="card-body" {
                        div class="d-flex align-items-center" style="gap: 16px;" {
                            img src=(owner_avatar) alt="Avatar" style="width: 48px; height: 48px; border-radius: 50%;" {}
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

    // Opt-in (or resume if paused)
    leaderboard::get_or_create_entry(&state.db, leaderboard_id, form.battlesnake_id)
        .await
        .wrap_err("Failed to join leaderboard")
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
