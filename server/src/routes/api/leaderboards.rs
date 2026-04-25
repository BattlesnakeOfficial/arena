use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
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
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
    pub first_place_rate: f64,
    pub scores: HashMap<String, f64>,
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

#[derive(Debug, Deserialize)]
pub struct RankingsQuery {
    #[serde(default)]
    pub sort: leaderboard::LeaderboardSort,
}

#[derive(Debug, Serialize)]
pub struct EntryResponse {
    pub leaderboard_entry_id: Uuid,
    pub battlesnake_id: Uuid,
    pub display_score: f64,
    pub games_played: i32,
    pub first_place_finishes: i32,
    pub non_first_finishes: i32,
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
    Query(query): Query<RankingsQuery>,
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

    let ranked = leaderboard::get_ranked_entries(&state.db, leaderboard_id, query.sort)
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

    // Collect entry IDs from both ranked and placement entries for scoring lookups
    let entry_ids: Vec<Uuid> = ranked
        .iter()
        .chain(placement.iter())
        .map(|e| e.leaderboard_entry_id)
        .collect();

    // Fetch per-algorithm scores for only the relevant entries
    let mut algo_maps: Vec<(String, HashMap<Uuid, f64>)> = vec![];
    for algo in state.scoring.algorithms() {
        let scores = algo.get_scores(&state.db, &entry_ids).await.map_err(|e| {
            tracing::error!("Failed to fetch {} scores: {}", algo.key(), e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        })?;
        let map: HashMap<Uuid, f64> = scores
            .into_iter()
            .map(|s| (s.leaderboard_entry_id, s.score))
            .collect();
        algo_maps.push((algo.key().to_string(), map));
    }

    fn to_ranking_entries(
        entries: Vec<leaderboard::RankedEntry>,
        start_rank: usize,
        algo_maps: &[(String, HashMap<Uuid, f64>)],
    ) -> Vec<RankingEntry> {
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                let first_place_rate = if e.games_played > 0 {
                    e.first_place_finishes as f64 / e.games_played as f64
                } else {
                    0.0
                };
                let mut scores = HashMap::new();
                for (key, map) in algo_maps {
                    if let Some(&score) = map.get(&e.leaderboard_entry_id) {
                        scores.insert(key.clone(), score);
                    }
                }
                RankingEntry {
                    rank: start_rank + i,
                    battlesnake_id: e.battlesnake_id,
                    snake_name: e.snake_name,
                    owner: e.owner_login,
                    display_score: e.display_score,
                    games_played: e.games_played,
                    first_place_finishes: e.first_place_finishes,
                    non_first_finishes: e.non_first_finishes,
                    first_place_rate,
                    scores,
                }
            })
            .collect()
    }

    let ranked_entries = to_ranking_entries(ranked, 1, &algo_maps);
    let placement_entries = to_ranking_entries(placement, 0, &algo_maps);

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

    // Initialize scoring algorithm entries
    for algo in state.scoring.algorithms() {
        algo.initialize_entry(&state.db, entry.leaderboard_entry_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to initialize scoring: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            })?;
    }

    Ok((
        StatusCode::CREATED,
        Json(EntryResponse {
            leaderboard_entry_id: entry.leaderboard_entry_id,
            battlesnake_id: entry.battlesnake_id,
            display_score: entry.display_score,
            games_played: entry.games_played,
            first_place_finishes: entry.first_place_finishes,
            non_first_finishes: entry.non_first_finishes,
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
