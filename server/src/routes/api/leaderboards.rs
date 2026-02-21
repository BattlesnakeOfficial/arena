use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    models::{
        battlesnake::{self, Visibility},
        leaderboard::{self, MIN_GAMES_FOR_RANKING},
    },
    routes::auth::ApiUser,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct LeaderboardResponse {
    pub id: Uuid,
    pub name: String,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct RankingEntry {
    pub rank: usize,
    pub battlesnake_id: Uuid,
    pub snake_name: String,
    pub owner: String,
    pub display_score: f64,
    pub games_played: i32,
    pub wins: i32,
    pub losses: i32,
    pub win_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct RankingsResponse {
    pub leaderboard_id: Uuid,
    pub leaderboard_name: String,
    pub min_games: i32,
    pub ranked: Vec<RankingEntry>,
    pub placement: Vec<RankingEntry>,
}

#[derive(Debug, Deserialize)]
pub struct OptInRequest {
    pub battlesnake_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct EntryResponse {
    pub leaderboard_entry_id: Uuid,
    pub battlesnake_id: Uuid,
    pub display_score: f64,
    pub games_played: i32,
    pub wins: i32,
    pub losses: i32,
    pub active: bool,
}

/// GET /api/leaderboards
pub async fn list_leaderboards(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let leaderboards = leaderboard::get_all_leaderboards(&state.db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list leaderboards: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

    let response: Vec<LeaderboardResponse> = leaderboards
        .into_iter()
        .map(|lb| LeaderboardResponse {
            id: lb.leaderboard_id,
            name: lb.name,
            active: lb.disabled_at.is_none(),
            created_at: lb.created_at,
        })
        .collect();

    Ok(Json(response))
}

/// GET /api/leaderboards/:id/rankings
pub async fn get_rankings(
    State(state): State<AppState>,
    Path(leaderboard_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch leaderboard: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Leaderboard not found".to_string()))?;

    let ranked = leaderboard::get_ranked_entries(&state.db, leaderboard_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch ranked entries: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

    let placement = leaderboard::get_placement_entries(&state.db, leaderboard_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch placement entries: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

    fn to_ranking_entries(
        entries: Vec<leaderboard::RankedEntry>,
        start_rank: usize,
    ) -> Vec<RankingEntry> {
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let win_rate = if e.games_played > 0 {
                    e.wins as f64 / e.games_played as f64
                } else {
                    0.0
                };
                RankingEntry {
                    rank: start_rank + i,
                    battlesnake_id: e.battlesnake_id,
                    snake_name: e.snake_name,
                    owner: e.owner_login,
                    display_score: e.display_score,
                    games_played: e.games_played,
                    wins: e.wins,
                    losses: e.losses,
                    win_rate,
                }
            })
            .collect()
    }

    let ranked_entries = to_ranking_entries(ranked, 1);
    let placement_entries = to_ranking_entries(placement, 0);

    Ok(Json(RankingsResponse {
        leaderboard_id: lb.leaderboard_id,
        leaderboard_name: lb.name,
        min_games: MIN_GAMES_FOR_RANKING,
        ranked: ranked_entries,
        placement: placement_entries,
    }))
}

/// POST /api/leaderboards/:id/entries — opt-in a snake
pub async fn create_entry(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path(leaderboard_id): Path<Uuid>,
    Json(request): Json<OptInRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Verify leaderboard exists and is active
    let lb = leaderboard::get_leaderboard_by_id(&state.db, leaderboard_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch leaderboard: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Leaderboard not found".to_string()))?;

    if lb.disabled_at.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Leaderboard is not active".to_string(),
        ));
    }

    // Verify snake belongs to user and is public
    let snake = battlesnake::get_battlesnake_by_id(&state.db, request.battlesnake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch battlesnake: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Battlesnake not found".to_string()))?;

    if snake.user_id != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't own this battlesnake".to_string(),
        ));
    }

    if snake.visibility != Visibility::Public {
        return Err((
            StatusCode::BAD_REQUEST,
            "Only public snakes can join leaderboards".to_string(),
        ));
    }

    let entry = leaderboard::get_or_create_entry(&state.db, leaderboard_id, request.battlesnake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to create entry: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(EntryResponse {
            leaderboard_entry_id: entry.leaderboard_entry_id,
            battlesnake_id: entry.battlesnake_id,
            display_score: entry.display_score,
            games_played: entry.games_played,
            wins: entry.wins,
            losses: entry.losses,
            active: entry.disabled_at.is_none(),
        }),
    ))
}

/// DELETE /api/leaderboards/:id/entries/:battlesnake_id — opt-out (pause)
pub async fn delete_entry(
    State(state): State<AppState>,
    ApiUser(user): ApiUser,
    Path((leaderboard_id, battlesnake_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Verify snake belongs to user
    let snake = battlesnake::get_battlesnake_by_id(&state.db, battlesnake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch battlesnake: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Battlesnake not found".to_string()))?;

    if snake.user_id != user.user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't own this battlesnake".to_string(),
        ));
    }

    let entry = leaderboard::get_entry(&state.db, leaderboard_id, battlesnake_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch entry: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Snake is not in this leaderboard".to_string(),
        ))?;

    leaderboard::set_disabled(
        &state.db,
        entry.leaderboard_entry_id,
        Some(chrono::Utc::now()),
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to disable entry: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal server error".to_string(),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}
