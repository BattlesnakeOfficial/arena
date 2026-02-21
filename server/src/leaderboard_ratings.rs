use color_eyre::eyre::Context as _;
use skillratings::MultiTeamOutcome;
use skillratings::weng_lin::{WengLinConfig, WengLinRating, weng_lin_multi_team};
use uuid::Uuid;

use crate::{
    models::{
        game_battlesnake,
        leaderboard::{self, LeaderboardEntry, LeaderboardGame},
    },
    state::AppState,
};

/// Update ratings for all snakes in a completed leaderboard game
pub async fn update_ratings(app_state: &AppState, leaderboard_game_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;

    // Fetch the leaderboard game using runtime query
    let lb_game = sqlx::query_as::<_, LeaderboardGame>(
        "SELECT leaderboard_game_id, leaderboard_id, game_id, created_at
         FROM leaderboard_games
         WHERE leaderboard_game_id = $1",
    )
    .bind(leaderboard_game_id)
    .fetch_one(pool)
    .await
    .wrap_err("Failed to fetch leaderboard game")?;

    // Fetch all game_battlesnakes with their placements
    let game_snakes = game_battlesnake::get_battlesnakes_by_game_id(pool, lb_game.game_id).await?;

    if game_snakes.is_empty() {
        tracing::warn!(
            game_id = %lb_game.game_id,
            "No snakes found for leaderboard game"
        );
        return Ok(());
    }

    // Look up each snake's leaderboard entry
    let mut entries_with_placements: Vec<(LeaderboardEntry, i32)> = Vec::new();

    for gs in &game_snakes {
        let placement = gs.placement.unwrap_or(game_snakes.len() as i32);

        let entry = leaderboard::get_entry(pool, lb_game.leaderboard_id, gs.battlesnake_id)
            .await
            .wrap_err_with(|| {
                format!(
                    "Failed to get leaderboard entry for snake {}",
                    gs.battlesnake_id
                )
            })?;

        if let Some(entry) = entry {
            entries_with_placements.push((entry, placement));
        } else {
            tracing::warn!(
                battlesnake_id = %gs.battlesnake_id,
                leaderboard_id = %lb_game.leaderboard_id,
                "Snake has no leaderboard entry, skipping"
            );
        }
    }

    if entries_with_placements.len() < 2 {
        tracing::warn!(
            game_id = %lb_game.game_id,
            "Fewer than 2 snakes with leaderboard entries, skipping rating update"
        );
        return Ok(());
    }

    // Build skillratings input: each snake as a single-player team
    let config = WengLinConfig::new();

    let ratings: Vec<Vec<WengLinRating>> = entries_with_placements
        .iter()
        .map(|(entry, _)| {
            vec![WengLinRating {
                rating: entry.mu,
                uncertainty: entry.sigma,
            }]
        })
        .collect();

    let teams_and_ranks: Vec<(&[WengLinRating], MultiTeamOutcome)> = ratings
        .iter()
        .zip(entries_with_placements.iter())
        .map(|(team, (_, placement))| (team.as_slice(), MultiTeamOutcome::new(*placement as usize)))
        .collect();

    // Calculate new ratings
    let new_ratings = weng_lin_multi_team(&teams_and_ranks, &config);

    // Update each snake's rating and record the result
    for (i, (entry, placement)) in entries_with_placements.iter().enumerate() {
        let new_rating = &new_ratings[i][0]; // single-player team
        let new_mu = new_rating.rating;
        let new_sigma = new_rating.uncertainty;
        let new_display_score = new_mu - 3.0 * new_sigma;

        let old_display_score = entry.mu - 3.0 * entry.sigma;
        let display_score_change = new_display_score - old_display_score;

        let is_win = *placement == 1;

        // Record the result (audit trail)
        leaderboard::create_game_result(
            pool,
            leaderboard::CreateGameResult {
                leaderboard_game_id,
                leaderboard_entry_id: entry.leaderboard_entry_id,
                placement: *placement,
                mu_before: entry.mu,
                mu_after: new_mu,
                sigma_before: entry.sigma,
                sigma_after: new_sigma,
                display_score_change,
            },
        )
        .await
        .wrap_err("Failed to create game result record")?;

        // Update the entry's rating
        leaderboard::update_rating(
            pool,
            entry.leaderboard_entry_id,
            new_mu,
            new_sigma,
            new_display_score,
            is_win,
        )
        .await
        .wrap_err("Failed to update entry rating")?;

        tracing::debug!(
            entry_id = %entry.leaderboard_entry_id,
            battlesnake_id = %entry.battlesnake_id,
            placement = placement,
            mu = format!("{:.2} -> {:.2}", entry.mu, new_mu),
            sigma = format!("{:.2} -> {:.2}", entry.sigma, new_sigma),
            score_change = format!("{:+.2}", display_score_change),
            "Updated rating"
        );
    }

    tracing::info!(
        leaderboard_game_id = %leaderboard_game_id,
        game_id = %lb_game.game_id,
        snakes_updated = entries_with_placements.len(),
        "Ratings updated for leaderboard game"
    );

    Ok(())
}
