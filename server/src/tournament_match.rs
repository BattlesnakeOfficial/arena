//! Tournament match execution (BS-020) and round orchestration (BS-021).
//!
//! A match is driven by re-entrant evaluation: [`run_match`] looks at the
//! games played so far and either declares a winner, waits for an in-flight
//! game, or creates the next game. Game completion re-enqueues the match's
//! evaluation via the hook in `game_runner`, so a best-of-N match plays out
//! one game at a time without anything blocking.
//!
//! # Tie cap (product-visible rule)
//!
//! Ties count for nobody, so a match can need more games than
//! [`MatchStyle::max_games_without_ties`]. To keep a pathological matchup
//! (e.g. two unreachable snakes that draw every game) from scheduling games
//! forever, a match is force-resolved once it has played
//! `max_games_without_ties() + TIE_ALLOWANCE` games without anyone reaching
//! the win threshold: the participant with more game wins takes the match,
//! and if game wins are level the slot-1 participant advances (slot 1
//! deterministically descends from the higher seed). For example, a
//! best-of-3 (cap 3 + 5 = 8) that stands at 1 win, 0 wins, and 7 ties awards
//! the match to the 1-win snake instead of playing a 9th game.
//!
//! # Failure recovery
//!
//! Every job in the pipeline can die permanently (cja deletes jobs that
//! exhaust their retries), so nothing may depend on a single enqueue
//! succeeding. The stuck-match sweeper cron ([`sweep_stuck_matches`])
//! re-enqueues evaluation for in-progress matches that have gone quiet, and
//! [`run_match`] re-enqueues the runner for a stalled game (`run_game` is
//! re-entrant). Together these make the sweep converge a match no matter
//! where the pipeline died.

use std::collections::HashMap;

use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::game::{CreateGame, GameStatus};
use crate::models::game_battlesnake::AddBattlesnakeToGame;
use crate::models::tournament::{
    MatchParticipant, MatchStatus, MatchStyle, TournamentMatch, TournamentStatus,
    count_unfinished_matches_in_round, create_match_game, find_match_game_by_game_id,
    find_stale_in_progress_matches, get_match_by_id, get_match_games_for_match,
    get_matches_for_round, get_participants_for_match, get_tournament_by_id, round_exists,
    set_match_status, set_tournament_current_round, try_set_tournament_status,
};
use crate::state::AppState;
use crate::tournament_bracket::{complete_match_with_winner, fill_participant_from_source};

/// Extra tied games allowed beyond [`MatchStyle::max_games_without_ties`]
/// before a match is force-resolved instead of scheduling yet another game.
/// See the module docs for the forced-resolution policy.
pub const TIE_ALLOWANCE: i32 = 5;

/// How long a match game may go without any sign of life (a status change or
/// a newly persisted turn) before [`run_match`] assumes its `GameRunnerJob`
/// died and re-enqueues it. Generous on purpose: games are bounded by
/// `MAX_TURNS` and normally finish in minutes.
const GAME_STALL_THRESHOLD_MINUTES: i64 = 15;

/// How long a tournament match may sit `in_progress` without updates before
/// the stuck-match sweeper re-enqueues its evaluation.
const MATCH_STALE_MINUTES: i64 = 5;

/// Decide the match winner from per-game winners (`None` = tie).
///
/// Ties count for nobody, so with enough tied games a match can exceed
/// `max_games_without_ties` — `run_match` keeps scheduling games until
/// someone reaches the threshold or the tie cap forces a resolution (see
/// the module docs).
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
        // The match can vanish between enqueue and execution when the owner
        // resets the tournament (matches are deleted). Treat it as a no-op
        // rather than an error so stale jobs don't retry forever.
        tracing::warn!(
            match_id = %match_id,
            "Match not found (deleted by a tournament reset?); skipping"
        );
        return Ok(());
    };
    if tournament_match.status == MatchStatus::Canceled {
        return Ok(());
    }
    if tournament_match.status == MatchStatus::Completed {
        // Already done — but a retry can land here after the completion
        // transaction committed and the follow-up enqueue failed. Re-enqueue
        // the (cheap, re-runnable) progression job so every retry converges
        // instead of stranding the round.
        cja::jobs::Job::enqueue(
            crate::jobs::UpdateTournamentStatusJob {
                tournament_id: tournament_match.tournament_id,
            },
            app_state.clone(),
            format!("Match {match_id} already completed; ensuring round progression"),
            None,
        )
        .await
        .wrap_err("Failed to enqueue tournament status update")?;
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

    // If a game is still running, its completion hook re-enqueues us — unless
    // the game has stalled (its GameRunnerJob died and was eventually deleted
    // by the job system), in which case we re-enqueue the runner ourselves.
    // `run_game` is re-entrant, so re-running a crashed game is safe.
    for match_game in &match_games {
        let Some(game) = crate::models::game::get_game_by_id(pool, match_game.game_id).await?
        else {
            return Err(color_eyre::eyre::eyre!(
                "Game {} missing for match game {}",
                match_game.game_id,
                match_game.match_game_id
            ));
        };
        if game.status == GameStatus::Finished {
            continue;
        }

        let last_activity = crate::models::game::get_game_last_activity(pool, match_game.game_id)
            .await?
            .unwrap_or(game.updated_at);
        let stalled = chrono::Utc::now() - last_activity
            > chrono::Duration::minutes(GAME_STALL_THRESHOLD_MINUTES);

        if stalled {
            // Touch the game before enqueueing so overlapping evaluations
            // (the sweeper fires every couple of minutes) don't enqueue
            // duplicate runners while this one waits in the queue.
            crate::models::game::touch_game_updated_at(pool, match_game.game_id).await?;
            tracing::warn!(
                match_id = %match_id,
                game_id = %match_game.game_id,
                last_activity = %last_activity,
                "Match game stalled; re-enqueueing its runner"
            );
            cja::jobs::Job::enqueue(
                crate::jobs::GameRunnerJob {
                    game_id: match_game.game_id,
                },
                app_state.clone(),
                format!("Stalled game {} for match {match_id}", match_game.game_id),
                None,
            )
            .await
            .wrap_err("Failed to re-enqueue stalled game runner")?;
        } else {
            tracing::info!(
                match_id = %match_id,
                game_id = %match_game.game_id,
                "Match has a game in flight; waiting"
            );
        }
        return Ok(());
    }

    // Every game here is Finished, and `run_game` records the match_games
    // winner in the same transaction that marks a game Finished — so each
    // result below is authoritative: `winner_id` NULL unambiguously means a
    // tie, never "not recorded yet".
    let game_winners: Vec<Option<Uuid>> = match_games.iter().map(|mg| mg.winner_id).collect();

    if let Some(winner_battlesnake_id) = match_winner(tournament.match_style, &game_winners) {
        complete_and_advance(app_state, &tournament_match, winner_battlesnake_id).await?;

        tracing::info!(
            match_id = %match_id,
            winner_battlesnake_id = %winner_battlesnake_id,
            games_played = match_games.len(),
            "Match completed"
        );

        return Ok(());
    }

    let games_played =
        i32::try_from(match_games.len()).wrap_err("Match game count does not fit in an i32")?;

    // Tie cap: force a deterministic resolution instead of scheduling games
    // forever. See the module docs — this is a product-visible rule.
    let tie_cap = tournament.match_style.max_games_without_ties() + TIE_ALLOWANCE;
    if games_played >= tie_cap {
        let winner_battlesnake_id = forced_tie_resolution(&participants, &game_winners)?;
        tracing::warn!(
            match_id = %match_id,
            games_played = games_played,
            tie_cap = tie_cap,
            winner_battlesnake_id = %winner_battlesnake_id,
            "Match hit the tie cap without a winner; forcing deterministic resolution"
        );
        complete_and_advance(app_state, &tournament_match, winner_battlesnake_id).await?;
        return Ok(());
    }

    // No winner yet and nothing in flight: play the next game.
    let game_number = games_played
        .checked_add(1)
        .ok_or_else(|| color_eyre::eyre::eyre!("Match game number overflow"))?;

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
    // evaluations of the same match safe: the loser's whole transaction
    // (including its orphan game) rolls back. Losing that race is expected
    // contention, not a failure — swallow it and let the winner's game drive
    // progress instead of burning job retries.
    if let Err(err) = create_match_game(&mut *tx, match_id, game.game_id, game_number).await {
        if is_unique_violation(&err, "match_games_match_id_game_number_key") {
            tracing::info!(
                match_id = %match_id,
                game_number = game_number,
                "Concurrent evaluation already created this match game; skipping"
            );
            tx.rollback()
                .await
                .wrap_err("Failed to roll back losing match game transaction")?;
            return Ok(());
        }
        return Err(err);
    }

    if tournament_match.status == MatchStatus::Scheduled {
        set_match_status(&mut *tx, match_id, MatchStatus::InProgress).await?;
    }

    // Tournament games spend the owner's game-creation budget (same table
    // the web/API limits check) so the start → run-round → reset loop can't
    // mint unmetered games. Recorded here, not rejected: a job must never
    // fail a match mid-flight — enforcement lives in the run-round handler.
    // Same transaction as the game, so a losing concurrent evaluation rolls
    // its charge back with its orphan game.
    crate::models::rate_limit::record_game_creation_attempt(
        &mut *tx,
        tournament.user_id,
        "tournament",
    )
    .await?;

    tx.commit()
        .await
        .wrap_err("Failed to commit match game creation")?;

    cja::jobs::Job::enqueue(
        crate::jobs::GameRunnerJob {
            game_id: game.game_id,
        },
        app_state.clone(),
        format!("Tournament match {match_id} game {game_number}"),
        None,
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
///
/// `round` is the round the owner was shown when they clicked "Run Round".
/// The job queue can delay execution past a round transition (or past a
/// reset-then-restart that rebuilt the bracket), so a payload that no longer
/// matches `current_round` is stale and must no-op — auto-running a round the
/// owner never asked for would break the caster flow.
pub async fn run_round(app_state: &AppState, tournament_id: Uuid, round: i32) -> cja::Result<()> {
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
    if round != tournament.current_round {
        tracing::warn!(
            tournament_id = %tournament_id,
            payload_round = round,
            current_round = tournament.current_round,
            "Run round payload is stale (round changed since enqueue); skipping"
        );
        return Ok(());
    }

    let matches = get_matches_for_round(pool, tournament_id, round).await?;
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
            None,
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
        // No next round: the final just finished. Compare-and-swap on the
        // InProgress status we observed above so a concurrent status change
        // wins the race — in particular, a completion racing a cancel must
        // NOT resurrect the tournament (canceled -> completed). Losing the
        // CAS is benign, not a job failure: whoever changed the status owns
        // it now.
        let completed = try_set_tournament_status(
            pool,
            tournament_id,
            TournamentStatus::Completed,
            TournamentStatus::InProgress,
        )
        .await?;
        if completed {
            tracing::info!(
                tournament_id = %tournament_id,
                "All rounds complete; tournament finished"
            );
        } else {
            tracing::info!(
                tournament_id = %tournament_id,
                "Tournament status changed concurrently (e.g. canceled); not marking completed"
            );
        }
    }

    Ok(())
}

/// Complete a match, advance the winner into the next match, and enqueue
/// round progression. Safe to retry: completion and advancement are
/// idempotent for the same winner, and if the post-commit enqueue fails the
/// retry hits `run_match`'s completed-match path, which re-enqueues it.
async fn complete_and_advance(
    app_state: &AppState,
    tournament_match: &TournamentMatch,
    winner_battlesnake_id: Uuid,
) -> cja::Result<()> {
    let pool = &app_state.db;
    let match_id = tournament_match.match_id;

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

    // Round/tournament progression happens off the hot path. cja's enqueue
    // can't join the transaction above, so this can fail after the commit —
    // that's fine, retries converge via the completed-match path.
    cja::jobs::Job::enqueue(
        crate::jobs::UpdateTournamentStatusJob {
            tournament_id: tournament_match.tournament_id,
        },
        app_state.clone(),
        format!("Match {match_id} completed"),
        None,
    )
    .await
    .wrap_err("Failed to enqueue tournament status update")?;

    Ok(())
}

/// Decide the winner of a match that hit the tie cap (see the module docs):
/// most game wins takes it; if wins are level, the slot-1 participant
/// advances (slot 1 deterministically descends from the higher seed).
///
/// `participants` must be in slot order with both battlesnakes filled — the
/// caller has already verified the match is ready.
fn forced_tie_resolution(
    participants: &[MatchParticipant],
    game_winners: &[Option<Uuid>],
) -> cja::Result<Uuid> {
    let mut slot_snakes = participants.iter().filter_map(|p| p.battlesnake_id);
    let (Some(slot1), Some(slot2)) = (slot_snakes.next(), slot_snakes.next()) else {
        return Err(color_eyre::eyre::eyre!(
            "Forced tie resolution needs two filled participants"
        ));
    };

    let wins = |snake: Uuid| {
        game_winners
            .iter()
            .flatten()
            .filter(|w| **w == snake)
            .count()
    };
    if wins(slot2) > wins(slot1) {
        Ok(slot2)
    } else {
        Ok(slot1)
    }
}

/// True if `err`'s chain contains a Postgres unique violation (SQLSTATE
/// 23505) on the named constraint.
pub(crate) fn is_unique_violation(err: &color_eyre::eyre::Report, constraint: &str) -> bool {
    err.chain().any(|cause| {
        let Some(sqlx::Error::Database(db_err)) = cause.downcast_ref::<sqlx::Error>() else {
            return false;
        };
        db_err.code().as_deref() == Some("23505") && db_err.constraint() == Some(constraint)
    })
}

/// Re-enqueue evaluation for stuck matches.
///
/// Nothing else re-evaluates a match once its driving job dies (cja deletes
/// jobs that exhaust their retries): game completion hooks only fire when a
/// game finishes, so a match whose RunMatchJob or GameRunnerJob vanished
/// would wait forever. This cron sweep re-enqueues [`run_match`] for every
/// in-progress match that has gone quiet for `MATCH_STALE_MINUTES`;
/// `run_match` is idempotent and knows how to re-enqueue a stalled game's
/// runner, so the sweep converges the match no matter where the pipeline
/// died.
pub async fn sweep_stuck_matches(app_state: &AppState) -> cja::Result<()> {
    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(MATCH_STALE_MINUTES);
    let match_ids = find_stale_in_progress_matches(&app_state.db, cutoff).await?;

    for match_id in match_ids {
        tracing::info!(
            match_id = %match_id,
            "Stuck-match sweeper re-enqueueing match evaluation"
        );
        cja::jobs::Job::enqueue(
            crate::jobs::RunMatchJob { match_id },
            app_state.clone(),
            format!("Stuck-match sweep for match {match_id}"),
            None,
        )
        .await
        .wrap_err("Failed to enqueue swept match evaluation")?;
    }

    Ok(())
}

/// The tournament-side result of a finished game, resolved from the engine
/// snake id to a battlesnake id. `run_game` writes this via
/// [`crate::models::tournament::set_match_game_winner`] inside its finish
/// transaction, so a Finished game ALWAYS has its match result recorded.
pub struct ResolvedMatchGame {
    pub match_game_id: Uuid,
    pub match_id: Uuid,
    /// `None` records a tie.
    pub winner_battlesnake_id: Option<Uuid>,
}

/// Resolve which match_games row a finished game belongs to (`None` for
/// non-tournament games) and map the engine-level winner (a stringified
/// `game_battlesnake_id`, `None` for a tie) to a battlesnake id.
///
/// Read-only: the caller writes the result inside its own transaction.
pub async fn resolve_finished_match_game(
    pool: &PgPool,
    game_id: Uuid,
    winner_game_battlesnake_id: Option<&str>,
) -> cja::Result<Option<ResolvedMatchGame>> {
    let Some(match_game) = find_match_game_by_game_id(pool, game_id).await? else {
        return Ok(None);
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

    Ok(Some(ResolvedMatchGame {
        match_game_id: match_game.match_game_id,
        match_id: match_game.match_id,
        winner_battlesnake_id,
    }))
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

    // --- forced tie resolution (pure) ---

    fn participant(slot: i16, battlesnake_id: Uuid) -> MatchParticipant {
        MatchParticipant {
            match_participant_id: Uuid::new_v4(),
            match_id: Uuid::new_v4(),
            slot,
            battlesnake_id: Some(battlesnake_id),
            source_match_id: None,
            participant_type: crate::models::tournament::ParticipantType::Seed,
            seed_position: Some(i32::from(slot)),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn forced_tie_resolution_picks_most_game_wins() {
        let v = ids(2);
        let (slot1, slot2) = (v[0], v[1]);
        let participants = [participant(1, slot1), participant(2, slot2)];

        // Slot 2 has strictly more wins: it advances despite the lower slot.
        let winners = [Some(slot2), None, None, Some(slot2), Some(slot1)];
        assert_eq!(
            forced_tie_resolution(&participants, &winners).unwrap(),
            slot2
        );

        // Slot 1 has strictly more wins.
        let winners = [Some(slot1), None, Some(slot1), Some(slot2)];
        assert_eq!(
            forced_tie_resolution(&participants, &winners).unwrap(),
            slot1
        );
    }

    #[test]
    fn forced_tie_resolution_falls_back_to_slot_one() {
        let v = ids(2);
        let (slot1, slot2) = (v[0], v[1]);
        let participants = [participant(1, slot1), participant(2, slot2)];

        // All ties: slot 1 advances.
        assert_eq!(
            forced_tie_resolution(&participants, &[None, None, None]).unwrap(),
            slot1
        );
        // Level nonzero wins: slot 1 advances.
        let winners = [Some(slot2), Some(slot1), None, None];
        assert_eq!(
            forced_tie_resolution(&participants, &winners).unwrap(),
            slot1
        );
    }

    #[test]
    fn forced_tie_resolution_requires_two_filled_participants() {
        let v = ids(1);
        let participants = [participant(1, v[0])];
        assert!(forced_tie_resolution(&participants, &[None]).is_err());
    }

    // --- DB tests for the failure-recovery paths ---

    use crate::models::game::{GameBoardSize, GameType, create_game};
    use crate::models::tournament::{
        CreateTournament, RegistrationStatus, Tournament, TournamentVisibility,
        create_registration, create_tournament, get_matches_for_tournament, set_match_game_winner,
        set_tournament_status,
    };
    use sqlx::PgPool;

    /// Create a two-snake tournament with its (single-match) bracket
    /// persisted and the tournament moved to in_progress. Returns the
    /// tournament, its only match, and the battlesnake ids by seed.
    async fn fixture_two_snake_match(
        pool: &PgPool,
        match_style: MatchStyle,
    ) -> cja::Result<(Tournament, TournamentMatch, Vec<Uuid>)> {
        // Random identifiers so the fixture can be used more than once per test.
        let github_id = i64::from(Uuid::new_v4().as_fields().0);
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, $3) RETURNING user_id",
        )
        .bind(github_id)
        .bind(format!("test-user-{github_id}"))
        .bind("test-token")
        .fetch_one(pool)
        .await?;

        let tournament = create_tournament(
            pool,
            user_id,
            CreateTournament {
                name: "Match Test".to_string(),
                description: None,
                game_type: GameType::Standard,
                board_size: GameBoardSize::Medium,
                registration_status: RegistrationStatus::Open,
                visibility: TournamentVisibility::Public,
                match_style,
                max_snakes_per_user: 2,
                required_participants: 2,
            },
        )
        .await?;

        let mut registrations = Vec::new();
        let mut snakes = Vec::new();
        for seed in 1..=2i32 {
            let battlesnake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, $3) RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(format!("snake-{github_id}-{seed}"))
            .bind("http://example.com")
            .fetch_one(pool)
            .await?;
            snakes.push(battlesnake_id);
            registrations.push(
                create_registration(
                    pool,
                    tournament.tournament_id,
                    battlesnake_id,
                    user_id,
                    seed,
                )
                .await?,
            );
        }

        let mut tx = pool.begin().await?;
        crate::tournament_bracket::persist_bracket(
            &mut tx,
            tournament.tournament_id,
            &registrations,
        )
        .await?;
        tx.commit().await?;

        set_tournament_status(
            pool,
            tournament.tournament_id,
            TournamentStatus::Registration,
            TournamentStatus::Created,
        )
        .await?;
        set_tournament_status(
            pool,
            tournament.tournament_id,
            TournamentStatus::InProgress,
            TournamentStatus::Registration,
        )
        .await?;

        let matches = get_matches_for_tournament(pool, tournament.tournament_id).await?;
        assert_eq!(matches.len(), 1, "two participants means a single final");
        let tournament_match = matches.into_iter().next().unwrap();
        Ok((tournament, tournament_match, snakes))
    }

    /// Insert a finished game recorded on the match with the given winner
    /// (`None` = tie), mirroring what run_game's finish transaction writes.
    async fn add_finished_game(
        pool: &PgPool,
        match_id: Uuid,
        game_number: i32,
        winner_id: Option<Uuid>,
    ) -> cja::Result<Uuid> {
        let game = create_game(
            pool,
            CreateGame {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
            },
        )
        .await?;
        let match_game = create_match_game(pool, match_id, game.game_id, game_number).await?;
        sqlx::query("UPDATE games SET status = 'finished' WHERE game_id = $1")
            .bind(game.game_id)
            .execute(pool)
            .await?;
        set_match_game_winner(pool, match_game.match_game_id, winner_id).await?;
        Ok(game.game_id)
    }

    async fn count_jobs(pool: &PgPool, name: &str) -> cja::Result<i64> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE name = $1")
                .bind(name)
                .fetch_one(pool)
                .await?,
        )
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn tie_cap_forces_resolution_to_most_wins(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (_, tournament_match, snakes) =
            fixture_two_snake_match(&pool, MatchStyle::BestOf3).await?;

        // Cap = max_games_without_ties (3) + TIE_ALLOWANCE (5) = 8 games.
        // The slot-2 snake has the only win; everything else tied.
        add_finished_game(&pool, tournament_match.match_id, 1, Some(snakes[1])).await?;
        for game_number in 2..=8 {
            add_finished_game(&pool, tournament_match.match_id, game_number, None).await?;
        }

        run_match(&app_state, tournament_match.match_id).await?;

        let reloaded = get_match_by_id(&pool, tournament_match.match_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, MatchStatus::Completed);
        assert_eq!(reloaded.winner_id, Some(snakes[1]), "most wins advances");
        // No ninth game was scheduled.
        let games = get_match_games_for_match(&pool, tournament_match.match_id).await?;
        assert_eq!(games.len(), 8);
        // Round progression was enqueued.
        assert_eq!(count_jobs(&pool, "UpdateTournamentStatusJob").await?, 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn tie_cap_falls_back_to_slot_one_when_wins_are_level(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (_, tournament_match, snakes) =
            fixture_two_snake_match(&pool, MatchStyle::BestOf3).await?;

        // One win each, six ties: level on wins at the cap, so slot 1 (the
        // higher seed's descendant) advances.
        add_finished_game(&pool, tournament_match.match_id, 1, Some(snakes[1])).await?;
        add_finished_game(&pool, tournament_match.match_id, 2, Some(snakes[0])).await?;
        for game_number in 3..=8 {
            add_finished_game(&pool, tournament_match.match_id, game_number, None).await?;
        }

        run_match(&app_state, tournament_match.match_id).await?;

        let reloaded = get_match_by_id(&pool, tournament_match.match_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, MatchStatus::Completed);
        assert_eq!(reloaded.winner_id, Some(snakes[0]), "slot 1 breaks the tie");
        let games = get_match_games_for_match(&pool, tournament_match.match_id).await?;
        assert_eq!(games.len(), 8);

        Ok(())
    }

    /// A retry that lands on an already-completed match must still enqueue
    /// round progression, otherwise a failed post-commit enqueue would
    /// strand the round forever.
    #[sqlx::test(migrations = "../migrations")]
    async fn completed_match_retry_reenqueues_progression(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (_, tournament_match, snakes) =
            fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;

        add_finished_game(&pool, tournament_match.match_id, 1, Some(snakes[0])).await?;

        // First evaluation completes the match and enqueues progression.
        run_match(&app_state, tournament_match.match_id).await?;
        let reloaded = get_match_by_id(&pool, tournament_match.match_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, MatchStatus::Completed);
        assert_eq!(count_jobs(&pool, "UpdateTournamentStatusJob").await?, 1);

        // A retry converges: it re-enqueues progression instead of no-oping.
        run_match(&app_state, tournament_match.match_id).await?;
        assert_eq!(count_jobs(&pool, "UpdateTournamentStatusJob").await?, 2);

        Ok(())
    }

    /// Two evaluations racing to create the same game number collide on the
    /// (match_id, game_number) unique constraint; the loser must be able to
    /// recognize the collision so it can no-op instead of failing the job.
    #[sqlx::test(migrations = "../migrations")]
    async fn duplicate_match_game_is_a_recognizable_unique_violation(
        pool: PgPool,
    ) -> cja::Result<()> {
        let (_, tournament_match, _) =
            fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;

        add_finished_game(&pool, tournament_match.match_id, 1, None).await?;

        let other_game = create_game(
            &pool,
            CreateGame {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
            },
        )
        .await?;
        let err = create_match_game(&pool, tournament_match.match_id, other_game.game_id, 1)
            .await
            .unwrap_err();

        assert!(is_unique_violation(
            &err,
            "match_games_match_id_game_number_key"
        ));
        assert!(!is_unique_violation(&err, "some_other_constraint"));

        Ok(())
    }

    /// A RunTournamentRoundJob whose payload round no longer matches
    /// `current_round` (the round advanced, or a reset-then-restart rebuilt
    /// the bracket) must no-op instead of firing matches the owner never
    /// clicked.
    #[sqlx::test(migrations = "../migrations")]
    async fn stale_round_payload_no_ops(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (tournament, _, _) = fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;
        set_tournament_current_round(&pool, tournament.tournament_id, 1).await?;

        // Stale payload: no match jobs are enqueued.
        run_round(&app_state, tournament.tournament_id, 2).await?;
        assert_eq!(count_jobs(&pool, "RunMatchJob").await?, 0);

        // Matching payload: the round's (single) match is kicked off.
        run_round(&app_state, tournament.tournament_id, 1).await?;
        assert_eq!(count_jobs(&pool, "RunMatchJob").await?, 1);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sweeper_finds_only_stale_in_progress_matches(pool: PgPool) -> cja::Result<()> {
        async fn backdate_match(pool: &PgPool, match_id: Uuid) -> cja::Result<()> {
            // The updated_at trigger stamps NOW() on every UPDATE, so it has
            // to be disabled to plant a stale timestamp.
            sqlx::query(
                "ALTER TABLE tournament_matches DISABLE TRIGGER update_tournament_matches_updated_at",
            )
            .execute(pool)
            .await?;
            sqlx::query(
                "UPDATE tournament_matches SET updated_at = NOW() - INTERVAL '10 minutes'
                 WHERE match_id = $1",
            )
            .bind(match_id)
            .execute(pool)
            .await?;
            sqlx::query(
                "ALTER TABLE tournament_matches ENABLE TRIGGER update_tournament_matches_updated_at",
            )
            .execute(pool)
            .await?;
            Ok(())
        }

        // Stale in-progress match in an in-progress tournament: swept.
        let (_, stale_match, _) = fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;
        set_match_status(&pool, stale_match.match_id, MatchStatus::InProgress).await?;
        backdate_match(&pool, stale_match.match_id).await?;

        // Fresh in-progress match: not swept.
        let (_, fresh_match, _) = fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;
        set_match_status(&pool, fresh_match.match_id, MatchStatus::InProgress).await?;

        // Stale but still scheduled: not swept (nothing started it yet).
        let (_, scheduled_match, _) =
            fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;
        backdate_match(&pool, scheduled_match.match_id).await?;

        // Stale in-progress match in a canceled tournament: not swept.
        let (canceled_tournament, canceled_match, _) =
            fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;
        set_match_status(&pool, canceled_match.match_id, MatchStatus::InProgress).await?;
        backdate_match(&pool, canceled_match.match_id).await?;
        set_tournament_status(
            &pool,
            canceled_tournament.tournament_id,
            TournamentStatus::Canceled,
            TournamentStatus::InProgress,
        )
        .await?;

        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(MATCH_STALE_MINUTES);
        let stale = find_stale_in_progress_matches(&pool, cutoff).await?;
        assert_eq!(stale, vec![stale_match.match_id]);

        Ok(())
    }

    /// A match whose game runner died must converge: run_match re-enqueues
    /// the runner for a stalled game (and only once per stall, thanks to the
    /// updated_at touch), while a healthy in-flight game just waits.
    #[sqlx::test(migrations = "../migrations")]
    async fn stalled_in_flight_game_gets_its_runner_reenqueued(pool: PgPool) -> cja::Result<()> {
        let app_state = AppState::test_from_pool(pool.clone());
        let (_, tournament_match, _) =
            fixture_two_snake_match(&pool, MatchStyle::SingleGame).await?;

        // An in-flight (running, unfinished) game.
        let game = create_game(
            &pool,
            CreateGame {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
            },
        )
        .await?;
        create_match_game(&pool, tournament_match.match_id, game.game_id, 1).await?;
        set_match_status(&pool, tournament_match.match_id, MatchStatus::InProgress).await?;
        sqlx::query("UPDATE games SET status = 'running' WHERE game_id = $1")
            .bind(game.game_id)
            .execute(&pool)
            .await?;

        // Healthy in-flight game: wait, don't enqueue anything.
        run_match(&app_state, tournament_match.match_id).await?;
        assert_eq!(count_jobs(&pool, "GameRunnerJob").await?, 0);
        let games = get_match_games_for_match(&pool, tournament_match.match_id).await?;
        assert_eq!(games.len(), 1, "no extra game while one is in flight");

        // Backdate the game past the stall threshold (trigger must be off).
        sqlx::query("ALTER TABLE games DISABLE TRIGGER update_games_updated_at")
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE games SET updated_at = NOW() - INTERVAL '20 minutes' WHERE game_id = $1",
        )
        .bind(game.game_id)
        .execute(&pool)
        .await?;
        sqlx::query("ALTER TABLE games ENABLE TRIGGER update_games_updated_at")
            .execute(&pool)
            .await?;

        // Stalled: re-enqueue the runner and touch the game.
        run_match(&app_state, tournament_match.match_id).await?;
        assert_eq!(count_jobs(&pool, "GameRunnerJob").await?, 1);

        // The touch makes an immediate re-evaluation wait instead of
        // enqueueing a duplicate runner.
        run_match(&app_state, tournament_match.match_id).await?;
        assert_eq!(count_jobs(&pool, "GameRunnerJob").await?, 1);

        Ok(())
    }
}
