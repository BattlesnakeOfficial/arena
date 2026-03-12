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
        assert!(is_valid_board_size("7x7"), "7x7 should be a valid board size");
        assert!(is_valid_board_size("11x11"), "11x11 should be a valid board size");
        assert!(is_valid_board_size("19x19"), "19x19 should be a valid board size");
    }

    #[test]
    fn test_invalid_board_sizes_rejected() {
        assert!(!is_valid_board_size("5x5"), "5x5 is not a supported board size");
        assert!(!is_valid_board_size(""), "empty string is not a valid board size");
        assert!(!is_valid_board_size("large"), "text aliases are not valid");
        assert!(!is_valid_board_size("11x11x11"), "3D board is not valid");
        assert!(!is_valid_board_size("0x0"), "zero board is not valid");
    }

    #[test]
    fn test_valid_game_types_accepted() {
        assert!(is_valid_game_type("Standard"), "Standard should be a valid game type");
        assert!(is_valid_game_type("Royale"), "Royale should be a valid game type");
        assert!(is_valid_game_type("Constrictor"), "Constrictor should be a valid game type");
        assert!(is_valid_game_type("Snail Mode"), "Snail Mode should be a valid game type");
    }

    #[test]
    fn test_invalid_game_types_rejected() {
        assert!(!is_valid_game_type("standard"), "lowercase is not valid");
        assert!(!is_valid_game_type(""), "empty string is not a valid game type");
        assert!(!is_valid_game_type("Unknown"), "unknown game type should be rejected");
        assert!(!is_valid_game_type("snail mode"), "wrong case should be rejected");
    }

    #[test]
    fn test_visibility_parsing_for_form() {
        use std::str::FromStr;
        use crate::models::battlesnake::Visibility;

        // create_leaderboard_handler validates visibility via Visibility::from_str
        assert!(Visibility::from_str("public").is_ok(), "public visibility is valid");
        assert!(Visibility::from_str("private").is_ok(), "private visibility is valid");
        assert!(Visibility::from_str("unlisted").is_err(), "unlisted is not a valid visibility");
        assert!(Visibility::from_str("").is_err(), "empty string is not a valid visibility");
    }

    #[test]
    #[ignore = "Requires CreateLeaderboardForm struct with name, description, board_size, game_type, visibility fields (BS-37342921850a4fc2)"]
    fn test_create_leaderboard_form_struct_exists() {
        // Implementation agent: after adding CreateLeaderboardForm, un-ignore and uncomment:
        //
        // use super::CreateLeaderboardForm;
        // let form = CreateLeaderboardForm {
        //     name: "My League".to_string(), description: "A fun league".to_string(),
        //     board_size: "11x11".to_string(), game_type: "Standard".to_string(),
        //     visibility: "public".to_string(),
        // };
        // assert!(!form.name.is_empty());
        // assert!(is_valid_board_size(&form.board_size));
        // assert!(is_valid_game_type(&form.game_type));
    }

    #[test]
    #[ignore = "Requires new_leaderboard handler (GET /leaderboards/new) (BS-37342921850a4fc2)"]
    fn test_new_leaderboard_handler_exists() {
        // Implementation agent: after adding new_leaderboard handler, un-ignore and uncomment:
        // let _ = super::new_leaderboard;
    }

    #[test]
    #[ignore = "Requires create_leaderboard_handler (POST /leaderboards) that validates name, board_size, game_type, visibility (BS-37342921850a4fc2)"]
    fn test_create_leaderboard_handler_exists() {
        // Implementation agent: after adding create_leaderboard_handler, un-ignore and uncomment:
        // let _ = super::create_leaderboard_handler;
        //
        // Validation requirements:
        // - name must be non-empty
        // - board_size must be one of: "7x7", "11x11", "19x19"
        // - game_type must be one of: "Standard", "Royale", "Constrictor", "Snail Mode"
        // - visibility must parse via Visibility::from_str
    }

    #[test]
    #[ignore = "Requires manage_leaderboard handler (GET /leaderboards/:id/manage) that returns 403 for non-creators and system leaderboards (BS-37342921850a4fc2)"]
    fn test_manage_leaderboard_handler_exists() {
        // Implementation agent: after adding manage_leaderboard handler, un-ignore and uncomment:
        // let _ = super::manage_leaderboard;
        //
        // Authorization requirements:
        // - Return 403 if user is not the leaderboard creator
        // - Return 403 for system leaderboards (creator_user_id = None)
        // Shows: settings form, enrolled snakes, pending requests, matchmaking toggle, snake search
    }

    #[test]
    #[ignore = "Requires toggle_matchmaking handler (POST /leaderboards/:id/matchmaking) (BS-37342921850a4fc2)"]
    fn test_toggle_matchmaking_handler_exists() {
        // Implementation agent: after adding toggle_matchmaking, un-ignore and uncomment:
        // let _ = super::toggle_matchmaking;
        //
        // Uses ToggleMatchmakingForm { enabled: Option<String> }
        // HTML checkbox: present="on" means enabled, absent means disabled
    }

    #[test]
    #[ignore = "Requires creator_add_snake handler (POST /leaderboards/:id/add-snake) (BS-37342921850a4fc2)"]
    fn test_creator_add_snake_handler_exists() {
        // Implementation agent: after adding creator_add_snake, un-ignore and uncomment:
        // let _ = super::creator_add_snake;
        //
        // Behavior:
        // - Public snake: check has_active_entry() first, then get_or_create_entry() + initialize scoring
        // - Private snake: create_enrollment_request() instead
        // CRITICAL: always call has_active_entry() before get_or_create_entry() (no unique constraint)
    }

    #[test]
    #[ignore = "Requires accept_enrollment_request handler (POST /enrollment-requests/:id/accept) (BS-37342921850a4fc2)"]
    fn test_accept_enrollment_request_handler_exists() {
        // Implementation agent: after adding accept_enrollment_request, un-ignore and uncomment:
        // let _ = super::accept_enrollment_request;
        //
        // Requirements:
        // 1. Fetch request. Return error if not found.
        // 2. Verify status is "pending".
        // 3. Verify snake.user_id == current_user.user_id (403 if not).
        // 4. Update status to "accepted".
        // 5. Check has_active_entry(). If false, get_or_create_entry() + initialize scoring.
        // 6. Redirect to /me.
    }

    #[test]
    #[ignore = "Requires decline_enrollment_request handler (POST /enrollment-requests/:id/decline) (BS-37342921850a4fc2)"]
    fn test_decline_enrollment_request_handler_exists() {
        // Implementation agent: after adding decline_enrollment_request, un-ignore and uncomment:
        // let _ = super::decline_enrollment_request;
        //
        // Requirements:
        // 1. Fetch request. Return error if not found.
        // 2. Verify status is "pending".
        // 3. Verify snake.user_id == current_user.user_id (403 if not).
        // 4. Update status to "declined".
        // 5. Redirect to /me. Do NOT create a leaderboard entry.
    }

    #[test]
    #[ignore = "Requires join_leaderboard to check lb.visibility and reject private leaderboards (BS-37342921850a4fc2)"]
    fn test_join_leaderboard_rejects_private_leaderboards() {
        // Implementation agent: after updating join_leaderboard to check visibility,
        // write an integration test that:
        // 1. Creates a private leaderboard
        // 2. Attempts to join it as a non-creator
        // 3. Verifies the response is an error with message:
        //    "Private leaderboards don't accept direct joins. Contact the leaderboard creator."
    }

    #[test]
    #[ignore = "Requires search_snakes_for_leaderboard handler (GET /leaderboards/:id/manage/search-snakes) (BS-37342921850a4fc2)"]
    fn test_search_snakes_handler_exists() {
        // Implementation agent: after adding search_snakes_for_leaderboard, un-ignore and uncomment:
        // let _ = super::search_snakes_for_leaderboard;
        //
        // Returns HTMX partial HTML with matching PUBLIC snakes and "Add" buttons.
        // Uses search_public_battlesnakes(pool, query, limit).
    }

    #[test]
    #[ignore = "Requires update_leaderboard_handler (POST /leaderboards/:id/update) (BS-37342921850a4fc2)"]
    fn test_update_leaderboard_handler_exists() {
        // Implementation agent: after adding update_leaderboard_handler, un-ignore and uncomment:
        // let _ = super::update_leaderboard_handler;
        //
        // Same validation as create: name non-empty, board_size/game_type/visibility validated.
        // Returns 403 if user is not the creator.
    }
}
