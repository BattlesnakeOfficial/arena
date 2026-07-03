//! Tournament match execution (BS-020) and round orchestration (BS-021).
//!
//! A match is driven by re-entrant evaluation: [`run_match`] looks at the
//! games played so far and either declares a winner, waits for an in-flight
//! game, or creates the next game. Game completion re-enqueues the match's
//! evaluation via the hook in `game_runner`, so a best-of-N match plays out
//! one game at a time without anything blocking.

use std::collections::HashMap;

use color_eyre::eyre::Context as _;
use uuid::Uuid;

use crate::models::game::{CreateGame, GameStatus};
use crate::models::game_battlesnake::AddBattlesnakeToGame;
use crate::models::tournament::{
    MatchStatus, MatchStyle, TournamentStatus, count_unfinished_matches_in_round,
    create_match_game, find_match_game_by_game_id, get_match_by_id, get_match_games_for_match,
    get_matches_for_round, get_participants_for_match, get_tournament_by_id, round_exists,
    set_match_status, set_tournament_current_round, set_tournament_status,
};
use crate::state::AppState;
use crate::tournament_bracket::{complete_match_with_winner, fill_participant_from_source};

/// Decide the match winner from per-game winners (`None` = tie).
///
/// Ties count for nobody, so with enough tied games a match can exceed
/// `max_games_without_ties` — `run_match` just keeps scheduling games until
/// someone reaches the threshold.
pub fn match_winner(style: MatchStyle, game_winners: &[Option<Uuid>]) -> Option<Uuid> {
    let mut wins: HashMap<Uuid, i32> = HashMap::new();
    for winner in game_winners.iter().flatten() {
        let count = wins.entry(*winner).or_insert(0);
        *count += 1;
        if *count >= style.wins_needed() {
            return Some(*winner);
        }
    }
    None
}

/// Determine the winner of a finished game from the final snake states.
///
/// Returns the engine snake id (a `game_battlesnake_id` string). A snake
/// still alive at the end wins outright. If everyone was eliminated, the
/// snake that survived strictly longest wins; a shared final turn is a tie
/// (`None`).
pub fn game_winner_from_snakes(snakes: &[rules::Snake]) -> Option<String> {
    let alive: Vec<&rules::Snake> = snakes
        .iter()
        .filter(|s| !s.eliminated_cause.is_eliminated())
        .collect();
    match alive.as_slice() {
        [only] => return Some(only.id.clone()),
        [] => {}
        // Multiple survivors (e.g. a turn-capped game) is a tie.
        _ => return None,
    }

    let max_turn = snakes.iter().map(|s| s.eliminated_on_turn).max()?;
    let mut last_standing = snakes.iter().filter(|s| s.eliminated_on_turn == max_turn);
    let candidate = last_standing.next()?;
    if last_standing.next().is_some() {
        None // simultaneous elimination on the final turn: tie
    } else {
        Some(candidate.id.clone())
    }
}

/// Evaluate a match and take the next step: complete it, wait on an
/// in-flight game, or create the next game. Safe to run repeatedly.
pub async fn run_match(app_state: &AppState, match_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;

    let Some(tournament_match) = get_match_by_id(pool, match_id).await? else {
        return Err(color_eyre::eyre::eyre!("Match {match_id} not found"));
    };
    if matches!(
        tournament_match.status,
        MatchStatus::Completed | MatchStatus::Canceled
    ) {
        return Ok(());
    }

    let Some(tournament) = get_tournament_by_id(pool, tournament_match.tournament_id).await? else {
        return Err(color_eyre::eyre::eyre!(
            "Tournament {} not found for match {match_id}",
            tournament_match.tournament_id
        ));
    };

    let participants = get_participants_for_match(pool, match_id).await?;
    let snake_ids: Vec<Uuid> = participants
        .iter()
        .filter_map(|p| p.battlesnake_id)
        .collect();
    if snake_ids.len() < 2 {
        // Feeder matches haven't produced both participants yet. The round
        // orchestration only enqueues ready matches, but a stale or manual
        // enqueue can land here — just wait.
        tracing::warn!(
            match_id = %match_id,
            filled = snake_ids.len(),
            "Match not ready: waiting on participants"
        );
        return Ok(());
    }

    let match_games = get_match_games_for_match(pool, match_id).await?;

    // If a game is still running, its completion hook re-enqueues us.
    for match_game in &match_games {
        let Some(game) = crate::models::game::get_game_by_id(pool, match_game.game_id).await?
        else {
            return Err(color_eyre::eyre::eyre!(
                "Game {} missing for match game {}",
                match_game.game_id,
                match_game.match_game_id
            ));
        };
        if game.status != GameStatus::Finished {
            tracing::info!(
                match_id = %match_id,
                game_id = %match_game.game_id,
                "Match has a game in flight; waiting"
            );
            return Ok(());
        }
    }

    let game_winners: Vec<Option<Uuid>> = match_games.iter().map(|mg| mg.winner_id).collect();

    if let Some(winner_battlesnake_id) = match_winner(tournament.match_style, &game_winners) {
        let mut tx = pool
            .begin()
            .await
            .wrap_err("Failed to start match completion transaction")?;
        complete_match_with_winner(&mut tx, match_id, winner_battlesnake_id).await?;
        if let Some(next_match_id) = tournament_match.next_match_id {
            fill_participant_from_source(&mut tx, next_match_id, match_id, winner_battlesnake_id)
                .await?;
        }
        tx.commit()
            .await
            .wrap_err("Failed to commit match completion")?;

        tracing::info!(
            match_id = %match_id,
            winner_battlesnake_id = %winner_battlesnake_id,
            games_played = match_games.len(),
            "Match completed"
        );

        // Round/tournament progression happens off the hot path.
        cja::jobs::Job::enqueue(
            crate::jobs::UpdateTournamentStatusJob {
                tournament_id: tournament.tournament_id,
            },
            app_state.clone(),
            format!("Match {match_id} completed"),
        )
        .await
        .wrap_err("Failed to enqueue tournament status update")?;

        return Ok(());
    }

    // No winner yet and nothing in flight: play the next game.
    let game_number = i32::try_from(match_games.len() + 1)
        .wrap_err("Match game number does not fit in an i32")?;

    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to start match game transaction")?;
    let game = crate::models::game::create_game(
        &mut *tx,
        CreateGame {
            board_size: tournament.board_size.clone(),
            game_type: tournament.game_type.clone(),
        },
    )
    .await
    .wrap_err("Failed to create match game")?;

    for battlesnake_id in &snake_ids {
        crate::models::game::add_battlesnake_to_game(
            &mut *tx,
            game.game_id,
            AddBattlesnakeToGame {
                battlesnake_id: *battlesnake_id,
            },
        )
        .await
        .wrap_err_with(|| {
            format!(
                "Failed to add battlesnake {battlesnake_id} to game {}",
                game.game_id
            )
        })?;
    }

    crate::models::game::set_game_enqueued_at_tx(&mut tx, game.game_id, chrono::Utc::now())
        .await
        .wrap_err("Failed to set enqueued_at")?;

    // The (match_id, game_number) unique constraint makes concurrent
    // evaluations of the same match safe: the second transaction aborts.
    create_match_game(&mut *tx, match_id, game.game_id, game_number).await?;

    if tournament_match.status == MatchStatus::Scheduled {
        set_match_status(&mut *tx, match_id, MatchStatus::InProgress).await?;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit match game creation")?;

    cja::jobs::Job::enqueue(
        crate::jobs::GameRunnerJob {
            game_id: game.game_id,
        },
        app_state.clone(),
        format!("Tournament match {match_id} game {game_number}"),
    )
    .await
    .wrap_err("Failed to enqueue game runner job")?;

    tracing::info!(
        match_id = %match_id,
        game_id = %game.game_id,
        game_number = game_number,
        "Created next match game"
    );

    Ok(())
}

/// Kick off every ready match in the tournament's current round.
pub async fn run_round(app_state: &AppState, tournament_id: Uuid) -> cja::Result<()> {
    let pool = &app_state.db;

    let Some(tournament) = get_tournament_by_id(pool, tournament_id).await? else {
        return Err(color_eyre::eyre::eyre!(
            "Tournament {tournament_id} not found"
        ));
    };
    if tournament.status != TournamentStatus::InProgress {
        tracing::warn!(
            tournament_id = %tournament_id,
            status = tournament.status.as_str(),
            "Run round requested for a tournament that is not in progress"
        );
        return Ok(());
    }

    let matches = get_matches_for_round(pool, tournament_id, tournament.current_round).await?;
    for tournament_match in matches {
        if matches!(
            tournament_match.status,
            MatchStatus::Completed | MatchStatus::Canceled
        ) {
            continue;
        }
        cja::jobs::Job::enqueue(
            crate::jobs::RunMatchJob {
                match_id: tournament_match.match_id,
            },
            app_state.clone(),
            format!(
                "Tournament {tournament_id} round {} match {}",
                tournament_match.round, tournament_match.position
            ),
        )
        .await
        .wrap_err("Failed to enqueue match job")?;
    }

    Ok(())
}

/// Advance `current_round` when a round finishes; complete the tournament
/// when the final round is done. Safe to run repeatedly.
pub async fn update_tournament_progress(
    app_state: &AppState,
    tournament_id: Uuid,
) -> cja::Result<()> {
    let pool = &app_state.db;

    let Some(tournament) = get_tournament_by_id(pool, tournament_id).await? else {
        return Err(color_eyre::eyre::eyre!(
            "Tournament {tournament_id} not found"
        ));
    };
    if tournament.status != TournamentStatus::InProgress {
        return Ok(());
    }

    let unfinished =
        count_unfinished_matches_in_round(pool, tournament_id, tournament.current_round).await?;
    if unfinished > 0 {
        return Ok(());
    }

    let next_round = tournament.current_round + 1;
    if round_exists(pool, tournament_id, next_round).await? {
        set_tournament_current_round(pool, tournament_id, next_round).await?;
        tracing::info!(
            tournament_id = %tournament_id,
            round = next_round,
            "Round complete; advanced to next round (waiting for Run Round)"
        );
    } else {
        // can_transition_to(InProgress -> Completed) always holds; checked
        // for symmetry with the route-layer transitions.
        if tournament
            .status
            .can_transition_to(TournamentStatus::Completed)
        {
            set_tournament_status(pool, tournament_id, TournamentStatus::Completed).await?;
            tracing::info!(
                tournament_id = %tournament_id,
                "All rounds complete; tournament finished"
            );
        }
    }

    Ok(())
}

/// Hook called by `game_runner` when a game that belongs to a tournament
/// match finishes. Records the game result and re-enqueues match evaluation.
///
/// `winner_game_battlesnake_id` is the engine snake id (a stringified
/// `game_battlesnake_id`), `None` for a tie.
pub async fn record_finished_match_game(
    app_state: &AppState,
    game_id: Uuid,
    winner_game_battlesnake_id: Option<&str>,
) -> cja::Result<()> {
    let pool = &app_state.db;

    let Some(match_game) = find_match_game_by_game_id(pool, game_id).await? else {
        return Ok(());
    };

    let winner_battlesnake_id = match winner_game_battlesnake_id {
        Some(engine_snake_id) => {
            let game_battlesnake_id: Uuid = engine_snake_id
                .parse()
                .wrap_err_with(|| format!("Invalid game_battlesnake id: {engine_snake_id}"))?;
            let details =
                crate::models::game_battlesnake::get_battlesnakes_by_game_id(pool, game_id).await?;
            let winner = details
                .iter()
                .find(|d| d.game_battlesnake_id == game_battlesnake_id)
                .ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "Winner {game_battlesnake_id} not found in game {game_id}"
                    )
                })?;
            Some(winner.battlesnake_id)
        }
        None => None,
    };

    crate::models::tournament::set_match_game_winner(
        pool,
        match_game.match_game_id,
        winner_battlesnake_id,
    )
    .await?;

    cja::jobs::Job::enqueue(
        crate::jobs::RunMatchJob {
            match_id: match_game.match_id,
        },
        app_state.clone(),
        format!("Game {game_id} finished for match {}", match_game.match_id),
    )
    .await
    .wrap_err("Failed to enqueue match evaluation after game completion")?;

    tracing::info!(
        game_id = %game_id,
        match_id = %match_game.match_id,
        winner_battlesnake_id = ?winner_battlesnake_id,
        "Recorded tournament match game result"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rules::{EliminationCause, Point, Snake};

    fn snake(id: &str, eliminated_cause: EliminationCause, eliminated_on_turn: i32) -> Snake {
        Snake {
            id: id.to_string(),
            body: vec![Point { x: 0, y: 0 }],
            health: if eliminated_cause == EliminationCause::NotEliminated {
                100
            } else {
                0
            },
            eliminated_cause,
            eliminated_by: String::new(),
            eliminated_on_turn,
        }
    }

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::new_v4()).collect()
    }

    #[test]
    fn single_game_first_win_takes_the_match() {
        let v = ids(2);
        let (a, b) = (v[0], v[1]);
        assert_eq!(match_winner(MatchStyle::SingleGame, &[Some(a)]), Some(a));
        assert_eq!(match_winner(MatchStyle::SingleGame, &[None]), None);
        assert_eq!(
            match_winner(MatchStyle::SingleGame, &[None, Some(b)]),
            Some(b)
        );
    }

    #[test]
    fn best_of_three_needs_two_wins() {
        let v = ids(2);
        let (a, b) = (v[0], v[1]);
        assert_eq!(match_winner(MatchStyle::BestOf3, &[Some(a)]), None);
        assert_eq!(match_winner(MatchStyle::BestOf3, &[Some(a), Some(b)]), None);
        assert_eq!(
            match_winner(MatchStyle::BestOf3, &[Some(a), Some(b), Some(a)]),
            Some(a)
        );
        assert_eq!(
            match_winner(MatchStyle::BestOf3, &[Some(a), Some(a)]),
            Some(a)
        );
    }

    #[test]
    fn first_to_three_needs_three_wins() {
        let v = ids(2);
        let (a, b) = (v[0], v[1]);
        let games = [Some(a), Some(b), Some(a), Some(b), Some(a)];
        assert_eq!(match_winner(MatchStyle::FirstTo3, &games), Some(a));
        assert_eq!(match_winner(MatchStyle::FirstTo3, &games[..4]), None);
    }

    #[test]
    fn ties_count_for_nobody() {
        let v = ids(2);
        let (a, b) = (v[0], v[1]);
        // All ties: no winner, match keeps going.
        assert_eq!(match_winner(MatchStyle::BestOf3, &[None, None, None]), None);
        // Ties interleaved with wins: only wins count.
        assert_eq!(
            match_winner(
                MatchStyle::BestOf3,
                &[None, Some(a), None, Some(b), Some(a)]
            ),
            Some(a)
        );
    }

    #[test]
    fn surviving_snake_wins_the_game() {
        let snakes = vec![
            snake("winner", EliminationCause::NotEliminated, 0),
            snake("loser", EliminationCause::OutOfHealth, 40),
        ];
        assert_eq!(game_winner_from_snakes(&snakes), Some("winner".to_string()));
    }

    #[test]
    fn longest_survivor_wins_when_all_eliminated() {
        let snakes = vec![
            snake("early", EliminationCause::OutOfBounds, 10),
            snake("late", EliminationCause::OutOfHealth, 42),
        ];
        assert_eq!(game_winner_from_snakes(&snakes), Some("late".to_string()));
    }

    #[test]
    fn simultaneous_elimination_is_a_tie() {
        let snakes = vec![
            snake("a", EliminationCause::HeadToHeadCollision, 30),
            snake("b", EliminationCause::HeadToHeadCollision, 30),
        ];
        assert_eq!(game_winner_from_snakes(&snakes), None);
    }

    #[test]
    fn multiple_survivors_is_a_tie() {
        let snakes = vec![
            snake("a", EliminationCause::NotEliminated, 0),
            snake("b", EliminationCause::NotEliminated, 0),
        ];
        assert_eq!(game_winner_from_snakes(&snakes), None);
    }
}
