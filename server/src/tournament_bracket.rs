//! Single-elimination bracket generation (BS-019).
//!
//! `generate_bracket` is pure and fully testable: it produces a
//! [`BracketPlan`] describing every match in every round, seeded so that
//! high seeds meet low seeds first and the top seeds can only meet in late
//! rounds. `persist_bracket` writes a plan to the database for a tournament,
//! resolving byes immediately (top seeds advance without playing).

use std::collections::HashMap;

use color_eyre::eyre::Context as _;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::models::tournament::{
    CreateMatchParticipant, CreateTournamentMatch, MatchStatus, ParticipantType,
    TournamentRegistration, create_match, create_match_participant,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BracketPlan {
    /// Number of rounds; the final is round `total_rounds`.
    pub total_rounds: i32,
    /// Bracket size: participant count rounded up to a power of two.
    pub bracket_size: i32,
    pub matches: Vec<PlannedMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMatch {
    /// 1-indexed round number.
    pub round: i32,
    /// 0-indexed position within the round.
    pub position: i32,
    /// Position of the match in the next round the winner advances to.
    /// `None` only for the final.
    pub next_position: Option<i32>,
    pub visual_column: i32,
    pub visual_row: i32,
    /// Round-1 matches: the two seed slots. `None` in a slot is a bye.
    /// Later rounds have `[None, None]` — participants come from feeder
    /// matches instead.
    pub seed_slots: [Option<i32>; 2],
}

impl PlannedMatch {
    pub fn is_bye(&self) -> bool {
        self.round == 1 && self.seed_slots.iter().filter(|s| s.is_some()).count() == 1
    }
}

/// Seed placement order for a bracket of `size` (power of two).
///
/// Built by the standard halving expansion: `[1, 2]` grows by pairing each
/// seed `s` with `2 * len + 1 - s`, so consecutive pairs are round-1 matches
/// and seeds 1 and 2 land in opposite halves. For size 8 this yields
/// `[1, 8, 4, 5, 2, 7, 3, 6]`, i.e. 1v8, 4v5, 2v7, 3v6.
fn seed_order(size: i32) -> Vec<i32> {
    let mut order = vec![1, 2];
    while (order.len() as i32) < size {
        let doubled = order.len() as i32 * 2;
        order = order.iter().flat_map(|&s| [s, doubled + 1 - s]).collect();
    }
    order
}

/// Generate a full single-elimination bracket for `participant_count` seeds.
///
/// Participants are identified by seed number (1-indexed). Byes are the
/// empty slots left when the count is not a power of two; because byes take
/// the place of the lowest seeds, they always fall opposite the top seeds.
pub fn generate_bracket(participant_count: usize) -> cja::Result<BracketPlan> {
    if participant_count < 2 {
        return Err(color_eyre::eyre::eyre!(
            "A bracket needs at least 2 participants, got {participant_count}"
        ));
    }
    let bracket_size = i32::try_from(participant_count.next_power_of_two())
        .wrap_err("Bracket size does not fit in an i32")?;
    let participant_count = participant_count as i32;
    let total_rounds = bracket_size.trailing_zeros() as i32;
    let order = seed_order(bracket_size);

    let mut matches = Vec::new();
    for round in 1..=total_rounds {
        let matches_in_round = bracket_size >> round;
        for position in 0..matches_in_round {
            let seed_slots = if round == 1 {
                let a = order[(position * 2) as usize];
                let b = order[(position * 2 + 1) as usize];
                // Seeds beyond the participant count don't exist: byes.
                [
                    (a <= participant_count).then_some(a),
                    (b <= participant_count).then_some(b),
                ]
            } else {
                [None, None]
            };

            matches.push(PlannedMatch {
                round,
                position,
                next_position: (round < total_rounds).then_some(position / 2),
                visual_column: round - 1,
                // Center each match between its two feeders: rows spread by
                // 2^round with an offset of 2^(round-1) - 1.
                visual_row: position * (1 << round) + (1 << (round - 1)) - 1,
                seed_slots,
            });
        }
    }

    Ok(BracketPlan {
        total_rounds,
        bracket_size,
        matches,
    })
}

/// Persist a bracket plan for a tournament.
///
/// Creates every match with `next_match_id` links and visual coordinates,
/// round-1 participants from `registrations` (matched by seed), and empty
/// winner-fed slots for later rounds. Round-1 byes are resolved immediately:
/// the lone participant is marked the winner and advanced into round 2.
///
/// Slot assignment is deterministic:
/// - Round 1: the first seed of the pair (the higher seed, e.g. 1 in 1v8)
///   takes slot 1, the second takes slot 2. A bye leaves slot 2 empty.
/// - Later rounds: the feeder match with the lower `position` feeds slot 1,
///   the higher `position` feeds slot 2.
///
/// Note that non-round-1 matches may be fully populated immediately: when two
/// round-1 byes feed the same round-2 match (e.g. 5 participants), that
/// round-2 match ends up with both participants filled while its status stays
/// `scheduled`. This is intentional — matches are started by the owner-
/// triggered round-execution scan, not by an event fired when a match fills.
///
/// `registrations` must contain exactly the seeds `1..=len` (the caller
/// renumbers seeds on unregister, so this holds for any started tournament).
pub async fn persist_bracket(
    tx: &mut Transaction<'_, Postgres>,
    tournament_id: Uuid,
    registrations: &[TournamentRegistration],
) -> cja::Result<()> {
    let plan = generate_bracket(registrations.len())?;

    let by_seed: HashMap<i32, &TournamentRegistration> =
        registrations.iter().map(|r| (r.seed, r)).collect();
    for seed in 1..=registrations.len() as i32 {
        if !by_seed.contains_key(&seed) {
            return Err(color_eyre::eyre::eyre!(
                "Registrations are not seeded 1..={}: missing seed {seed}",
                registrations.len()
            ));
        }
    }

    // Create matches from the final backward so next_match_id targets exist.
    let mut match_ids: HashMap<(i32, i32), Uuid> = HashMap::new();
    let lookup_match_id = |match_ids: &HashMap<(i32, i32), Uuid>, round: i32, position: i32| {
        match_ids.get(&(round, position)).copied().ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "Bracket plan is inconsistent: no match at round {round} position {position}"
            )
        })
    };
    for planned in plan.matches.iter().rev() {
        let next_match_id = match planned.next_position {
            Some(next_pos) => Some(lookup_match_id(&match_ids, planned.round + 1, next_pos)?),
            None => None,
        };
        let created = create_match(
            &mut **tx,
            CreateTournamentMatch {
                tournament_id,
                round: planned.round,
                position: planned.position,
                next_match_id,
                visual_column: planned.visual_column,
                visual_row: planned.visual_row,
            },
        )
        .await?;
        match_ids.insert((planned.round, planned.position), created.match_id);
    }

    let snake_for_seed = |seed: i32| {
        by_seed
            .get(&seed)
            .map(|r| r.battlesnake_id)
            .ok_or_else(|| color_eyre::eyre::eyre!("Bracket plan references unknown seed {seed}"))
    };

    // Round-1 seeded participants: the first seed of the pair takes slot 1,
    // the second takes slot 2 (a bye leaves slot 2 empty).
    for planned in plan.matches.iter().filter(|m| m.round == 1) {
        let match_id = lookup_match_id(&match_ids, planned.round, planned.position)?;
        for (slot_index, seed) in planned.seed_slots.iter().enumerate() {
            let Some(seed) = seed else { continue };
            create_match_participant(
                &mut **tx,
                CreateMatchParticipant {
                    match_id,
                    slot: slot_index as i16 + 1,
                    battlesnake_id: Some(snake_for_seed(*seed)?),
                    source_match_id: None,
                    participant_type: ParticipantType::Seed,
                    seed_position: Some(*seed),
                },
            )
            .await?;
        }
    }

    // Later rounds: one empty winner-fed slot per feeder match. The feeder
    // with the lower position feeds slot 1, the higher position slot 2.
    for planned in plan.matches.iter().filter(|m| m.round > 1) {
        let match_id = lookup_match_id(&match_ids, planned.round, planned.position)?;
        for (slot_index, feeder_position) in [planned.position * 2, planned.position * 2 + 1]
            .iter()
            .enumerate()
        {
            let source_match_id = lookup_match_id(&match_ids, planned.round - 1, *feeder_position)?;
            create_match_participant(
                &mut **tx,
                CreateMatchParticipant {
                    match_id,
                    slot: slot_index as i16 + 1,
                    battlesnake_id: None,
                    source_match_id: Some(source_match_id),
                    participant_type: ParticipantType::Winner,
                    seed_position: None,
                },
            )
            .await?;
        }
    }

    // Resolve byes: the lone participant wins and advances immediately.
    for planned in plan.matches.iter().filter(|m| m.is_bye()) {
        let match_id = lookup_match_id(&match_ids, planned.round, planned.position)?;
        let seed = planned.seed_slots.iter().flatten().next().ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "Bye match at round {} position {} has no seed",
                planned.round,
                planned.position
            )
        })?;
        let winner_battlesnake_id = snake_for_seed(*seed)?;

        complete_match_with_winner(tx, match_id, winner_battlesnake_id).await?;

        if let Some(next_pos) = planned.next_position {
            let next_match_id = lookup_match_id(&match_ids, planned.round + 1, next_pos)?;
            fill_participant_from_source(tx, next_match_id, match_id, winner_battlesnake_id)
                .await?;
        }
    }

    Ok(())
}

/// Mark a match completed with the given winner.
///
/// The winner must be one of the match's participants. Idempotent: completing
/// a match that is already completed with the same winner is a no-op (job
/// retries must be safe); completing with a different winner is an error.
pub async fn complete_match_with_winner(
    tx: &mut Transaction<'_, Postgres>,
    match_id: Uuid,
    winner_battlesnake_id: Uuid,
) -> cja::Result<()> {
    let is_participant = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM match_participants
            WHERE match_id = $1 AND battlesnake_id = $2
        ) as "is_participant!"
        "#,
        match_id,
        winner_battlesnake_id,
    )
    .fetch_one(&mut **tx)
    .await
    .wrap_err("Failed to check match participants")?;

    if !is_participant {
        return Err(color_eyre::eyre::eyre!(
            "Cannot complete match {match_id}: {winner_battlesnake_id} is not a participant"
        ));
    }

    let updated = sqlx::query!(
        "UPDATE tournament_matches SET status = $2, winner_id = $3
         WHERE match_id = $1 AND status <> $2",
        match_id,
        MatchStatus::Completed.as_str(),
        winner_battlesnake_id,
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to complete match")?;

    if updated.rows_affected() == 1 {
        return Ok(());
    }

    // 0 rows: the match is already completed (it must exist — it has
    // participants). Retrying with the same winner is fine; a different
    // winner means two results were recorded for one match.
    let existing_winner = sqlx::query_scalar!(
        "SELECT winner_id FROM tournament_matches WHERE match_id = $1",
        match_id,
    )
    .fetch_one(&mut **tx)
    .await
    .wrap_err("Failed to fetch completed match")?;

    if existing_winner == Some(winner_battlesnake_id) {
        Ok(())
    } else {
        Err(color_eyre::eyre::eyre!(
            "Match {match_id} is already completed with winner {existing_winner:?}; \
             refusing to overwrite with {winner_battlesnake_id}"
        ))
    }
}

/// Fill the participant slot in `match_id` that is fed by `source_match_id`.
///
/// Retry-safe: re-filling a slot with the same battlesnake is a no-op update
/// that still counts as success. Filling a slot that already holds a
/// different battlesnake is an error.
pub async fn fill_participant_from_source(
    tx: &mut Transaction<'_, Postgres>,
    match_id: Uuid,
    source_match_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<()> {
    let updated = sqlx::query!(
        r#"
        UPDATE match_participants
        SET battlesnake_id = $3
        WHERE match_id = $1 AND source_match_id = $2
          AND (battlesnake_id IS NULL OR battlesnake_id = $3)
        "#,
        match_id,
        source_match_id,
        battlesnake_id,
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to advance participant into next match")?;

    if updated.rows_affected() != 1 {
        // 0 rows: either no slot is fed by this match, or the slot is already
        // filled with a different battlesnake — check which.
        let existing = sqlx::query_scalar!(
            "SELECT battlesnake_id FROM match_participants
             WHERE match_id = $1 AND source_match_id = $2",
            match_id,
            source_match_id,
        )
        .fetch_optional(&mut **tx)
        .await
        .wrap_err("Failed to inspect participant slot")?;

        return Err(match existing {
            None => color_eyre::eyre::eyre!(
                "No participant slot in match {match_id} is fed by {source_match_id}"
            ),
            Some(other) => color_eyre::eyre::eyre!(
                "Participant slot in match {match_id} fed by {source_match_id} already \
                 holds {other:?}; refusing to overwrite with {battlesnake_id}"
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sqlx::PgPool;

    use crate::models::game::{GameBoardSize, GameType};
    use crate::models::tournament::{
        CreateTournament, MatchParticipant, MatchStyle, RegistrationStatus, TournamentMatch,
        TournamentVisibility, create_registration, create_tournament, get_matches_for_tournament,
        get_participants_for_match,
    };

    fn round_one_pairs(plan: &BracketPlan) -> Vec<[Option<i32>; 2]> {
        plan.matches
            .iter()
            .filter(|m| m.round == 1)
            .map(|m| m.seed_slots)
            .collect()
    }

    #[test]
    fn two_participants_is_a_single_final() {
        let plan = generate_bracket(2).unwrap();
        assert_eq!(plan.total_rounds, 1);
        assert_eq!(plan.matches.len(), 1);
        let only = &plan.matches[0];
        assert_eq!(only.seed_slots, [Some(1), Some(2)]);
        assert_eq!(only.next_position, None);
    }

    #[test]
    fn eight_participants_match_the_classic_bracket() {
        let plan = generate_bracket(8).unwrap();
        assert_eq!(plan.total_rounds, 3);
        assert_eq!(plan.matches.len(), 7);
        assert_eq!(
            round_one_pairs(&plan),
            vec![
                [Some(1), Some(8)],
                [Some(4), Some(5)],
                [Some(2), Some(7)],
                [Some(3), Some(6)],
            ]
        );
    }

    #[test]
    fn sixteen_participants_match_the_classic_bracket() {
        let plan = generate_bracket(16).unwrap();
        assert_eq!(
            round_one_pairs(&plan),
            vec![
                [Some(1), Some(16)],
                [Some(8), Some(9)],
                [Some(4), Some(13)],
                [Some(5), Some(12)],
                [Some(2), Some(15)],
                [Some(7), Some(10)],
                [Some(3), Some(14)],
                [Some(6), Some(11)],
            ]
        );
    }

    #[test]
    fn six_participants_give_byes_to_the_top_two_seeds() {
        let plan = generate_bracket(6).unwrap();
        assert_eq!(
            round_one_pairs(&plan),
            vec![
                [Some(1), None],
                [Some(4), Some(5)],
                [Some(2), None],
                [Some(3), Some(6)],
            ]
        );
        assert_eq!(plan.matches.iter().filter(|m| m.is_bye()).count(), 2);
    }

    #[test]
    fn fewer_than_two_participants_is_an_error() {
        assert!(generate_bracket(0).is_err());
        assert!(generate_bracket(1).is_err());
    }

    #[test]
    fn visual_rows_center_matches_between_feeders() {
        let plan = generate_bracket(8).unwrap();
        let row = |round: i32, position: i32| {
            plan.matches
                .iter()
                .find(|m| m.round == round && m.position == position)
                .unwrap()
                .visual_row
        };
        // Round 1: 0, 2, 4, 6. Round 2 sits between its feeders: 1, 5. Final: 3.
        assert_eq!(
            (0..4).map(|p| row(1, p)).collect::<Vec<_>>(),
            vec![0, 2, 4, 6]
        );
        assert_eq!((0..2).map(|p| row(2, p)).collect::<Vec<_>>(), vec![1, 5]);
        assert_eq!(row(3, 0), 3);
    }

    proptest! {
        #[test]
        fn bracket_structure_invariants(n in 2usize..=128) {
            let plan = generate_bracket(n).unwrap();
            let size = n.next_power_of_two() as i32;

            // Full bracket: size - 1 matches, log2(size) rounds.
            prop_assert_eq!(plan.bracket_size, size);
            prop_assert_eq!(plan.matches.len() as i32, size - 1);
            prop_assert_eq!(1 << plan.total_rounds, size);

            for round in 1..=plan.total_rounds {
                let in_round: Vec<_> =
                    plan.matches.iter().filter(|m| m.round == round).collect();
                prop_assert_eq!(in_round.len() as i32, size >> round);

                // Positions are 0..count and visual rows strictly increase.
                for (i, m) in in_round.iter().enumerate() {
                    prop_assert_eq!(m.position, i as i32);
                    if i > 0 {
                        prop_assert!(m.visual_row > in_round[i - 1].visual_row);
                    }
                }
            }

            // Only the final has no next match.
            for m in &plan.matches {
                prop_assert_eq!(m.next_position.is_none(), m.round == plan.total_rounds);
            }

            // Feeder pairing: every non-round-1 match at position k is fed by
            // exactly the two previous-round matches at positions 2k and
            // 2k + 1 (the matches whose winners advance into it), it sits one
            // visual column to their right, and its visual row is the mean of
            // its feeders' rows (each match is centered between its feeders).
            for m in plan.matches.iter().filter(|m| m.round > 1) {
                let feeders: Vec<_> = plan
                    .matches
                    .iter()
                    .filter(|f| f.round == m.round - 1 && f.next_position == Some(m.position))
                    .collect();
                prop_assert_eq!(feeders.len(), 2);
                prop_assert_eq!(
                    feeders.iter().map(|f| f.position).collect::<Vec<_>>(),
                    vec![m.position * 2, m.position * 2 + 1]
                );
                prop_assert_eq!(feeders[0].visual_column, feeders[1].visual_column);
                prop_assert_eq!(m.visual_column, feeders[0].visual_column + 1);
                prop_assert_eq!(
                    m.visual_row * 2,
                    feeders[0].visual_row + feeders[1].visual_row,
                    "match r{} p{} is not centered between its feeders",
                    m.round,
                    m.position
                );
            }
        }

        #[test]
        fn seeding_invariants(n in 2usize..=128) {
            let plan = generate_bracket(n).unwrap();
            let size = n.next_power_of_two() as i32;
            let byes = size - n as i32;

            let mut seen = Vec::new();
            let mut bye_seeds = Vec::new();
            for m in plan.matches.iter().filter(|m| m.round == 1) {
                match m.seed_slots {
                    [Some(a), Some(b)] => {
                        // Full pairs always sum to size + 1 (1v8, 4v5, ...).
                        prop_assert_eq!(a + b, size + 1);
                        seen.extend([a, b]);
                    }
                    [Some(a), None] | [None, Some(a)] => {
                        bye_seeds.push(a);
                        seen.push(a);
                    }
                    [None, None] => prop_assert!(false, "round-1 match with no seeds"),
                }
            }

            // Every participant appears exactly once, and only participants.
            seen.sort_unstable();
            prop_assert_eq!(seen, (1..=n as i32).collect::<Vec<_>>());

            // Byes go to exactly the top seeds.
            bye_seeds.sort_unstable();
            prop_assert_eq!(bye_seeds, (1..=byes).collect::<Vec<_>>());
        }

        #[test]
        fn top_seeds_cannot_meet_early(n in 4usize..=128) {
            let plan = generate_bracket(n).unwrap();
            let round_one = round_one_pairs(&plan);
            let half = round_one.len() / 2;

            let position_of = |seed: i32| {
                round_one
                    .iter()
                    .position(|slots| slots.contains(&Some(seed)))
                    .unwrap()
            };

            // Seeds 1 and 2 start in opposite halves of the bracket, so they
            // can only meet in the final.
            prop_assert!((position_of(1) < half) != (position_of(2) < half));

            // Seeds 1-4 all start in different quarters.
            if n >= 4 && round_one.len() >= 4 {
                let quarter = round_one.len() / 4;
                let mut quarters: Vec<_> =
                    (1..=4).map(|s| position_of(s) / quarter.max(1)).collect();
                quarters.sort_unstable();
                quarters.dedup();
                prop_assert_eq!(quarters.len(), 4.min(round_one.len()));
            }
        }
    }

    // --- DB tests for persist_bracket and match progression ---

    /// Create a tournament with `n` registered snakes (seeds 1..=n) and
    /// persist its bracket. Returns the tournament id and the battlesnake ids
    /// indexed by seed (`snakes[0]` is seed 1).
    async fn fixture_persisted_bracket(pool: &PgPool, n: usize) -> cja::Result<(Uuid, Vec<Uuid>)> {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, $3) RETURNING user_id",
        )
        .bind(424_242_i64)
        .bind("test-user")
        .bind("test-token")
        .fetch_one(pool)
        .await?;

        let tournament = create_tournament(
            pool,
            user_id,
            CreateTournament {
                name: "Bracket Test".to_string(),
                description: None,
                game_type: GameType::Standard,
                board_size: GameBoardSize::Medium,
                registration_status: RegistrationStatus::Open,
                visibility: TournamentVisibility::Public,
                match_style: MatchStyle::SingleGame,
                max_snakes_per_user: n as i32,
                required_participants: n as i32,
            },
        )
        .await?;

        let mut registrations = Vec::new();
        for seed in 1..=n as i32 {
            let battlesnake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, $2, $3) RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(format!("snake-{seed}"))
            .bind("http://example.com")
            .fetch_one(pool)
            .await?;
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
        persist_bracket(&mut tx, tournament.tournament_id, &registrations).await?;
        tx.commit().await?;

        let snakes = registrations.iter().map(|r| r.battlesnake_id).collect();
        Ok((tournament.tournament_id, snakes))
    }

    fn match_at(matches: &[TournamentMatch], round: i32, position: i32) -> &TournamentMatch {
        matches
            .iter()
            .find(|m| m.round == round && m.position == position)
            .unwrap_or_else(|| panic!("no match at round {round} position {position}"))
    }

    fn snake_in_slot(participants: &[MatchParticipant], slot: i16) -> Option<Uuid> {
        participants
            .iter()
            .find(|p| p.slot == slot)
            .and_then(|p| p.battlesnake_id)
    }

    /// Five participants: seeds 1, 2, and 3 get round-1 byes. Seeds 2 and 3
    /// both feed round-2 position 1, so that match is fully populated
    /// immediately — but stays `scheduled`, because matches are started by
    /// the round-execution scan, not by an event when they fill.
    #[sqlx::test(migrations = "../migrations")]
    async fn five_participants_double_bye_fills_round_two_but_stays_scheduled(
        pool: PgPool,
    ) -> cja::Result<()> {
        let (tournament_id, snakes) = fixture_persisted_bracket(&pool, 5).await?;
        let matches = get_matches_for_tournament(&pool, tournament_id).await?;
        assert_eq!(matches.len(), 7); // bracket size 8

        // Round 1: 1-bye, 4v5, 2-bye, 3-bye. Byes complete immediately.
        for (position, seed) in [(0, 1), (2, 2), (3, 3)] {
            let bye = match_at(&matches, 1, position);
            assert_eq!(bye.status, MatchStatus::Completed);
            assert_eq!(bye.winner_id, Some(snakes[seed - 1]));
        }

        // 4v5 is a real match: higher seed in slot 1, still scheduled.
        let four_v_five = match_at(&matches, 1, 1);
        assert_eq!(four_v_five.status, MatchStatus::Scheduled);
        assert_eq!(four_v_five.winner_id, None);
        let participants = get_participants_for_match(&pool, four_v_five.match_id).await?;
        assert_eq!(snake_in_slot(&participants, 1), Some(snakes[3]));
        assert_eq!(snake_in_slot(&participants, 2), Some(snakes[4]));

        // Round-2 position 0: seed 1's bye fills slot 1, slot 2 waits on 4v5.
        let semi_top = match_at(&matches, 2, 0);
        assert_eq!(semi_top.status, MatchStatus::Scheduled);
        let participants = get_participants_for_match(&pool, semi_top.match_id).await?;
        assert_eq!(snake_in_slot(&participants, 1), Some(snakes[0]));
        assert_eq!(snake_in_slot(&participants, 2), None);

        // Round-2 position 1: fed by two byes, so both slots are filled —
        // lower feeder position (seed 2's bye) in slot 1 — and the match is
        // still scheduled, waiting for round execution to start it.
        let semi_bottom = match_at(&matches, 2, 1);
        assert_eq!(semi_bottom.status, MatchStatus::Scheduled);
        assert_eq!(semi_bottom.winner_id, None);
        let participants = get_participants_for_match(&pool, semi_bottom.match_id).await?;
        assert_eq!(participants.len(), 2);
        assert_eq!(snake_in_slot(&participants, 1), Some(snakes[1]));
        assert_eq!(snake_in_slot(&participants, 2), Some(snakes[2]));

        // The final is untouched: two empty winner-fed slots.
        let final_match = match_at(&matches, 3, 0);
        assert_eq!(final_match.status, MatchStatus::Scheduled);
        let participants = get_participants_for_match(&pool, final_match.match_id).await?;
        assert_eq!(participants.len(), 2);
        assert!(participants.iter().all(|p| p.battlesnake_id.is_none()));

        Ok(())
    }

    /// Completing a match and advancing its winner must be safe under job
    /// retries: same-winner repeats are no-ops, conflicting results error.
    #[sqlx::test(migrations = "../migrations")]
    async fn completion_and_advancement_are_retry_safe(pool: PgPool) -> cja::Result<()> {
        // Three participants: seed 1 gets a bye into the final (round 2),
        // 2v3 plays round 1.
        let (tournament_id, snakes) = fixture_persisted_bracket(&pool, 3).await?;
        let matches = get_matches_for_tournament(&pool, tournament_id).await?;
        let bye = match_at(&matches, 1, 0);
        let two_v_three = match_at(&matches, 1, 1);
        let final_match = match_at(&matches, 2, 0);

        let mut tx = pool.begin().await?;

        // Re-completing the bye with the same winner is idempotent.
        complete_match_with_winner(&mut tx, bye.match_id, snakes[0]).await?;
        // A non-participant can never be recorded as the winner.
        let result = complete_match_with_winner(&mut tx, bye.match_id, snakes[1]).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not a participant")
        );

        // Complete 2v3, then retry with the same and a different winner.
        complete_match_with_winner(&mut tx, two_v_three.match_id, snakes[1]).await?;
        complete_match_with_winner(&mut tx, two_v_three.match_id, snakes[1]).await?;
        let result = complete_match_with_winner(&mut tx, two_v_three.match_id, snakes[2]).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already completed")
        );

        // Re-filling the bye-fed final slot with the same snake is idempotent.
        fill_participant_from_source(&mut tx, final_match.match_id, bye.match_id, snakes[0])
            .await?;
        // Advance the 2v3 winner; retry with the same snake, then conflict.
        fill_participant_from_source(
            &mut tx,
            final_match.match_id,
            two_v_three.match_id,
            snakes[1],
        )
        .await?;
        fill_participant_from_source(
            &mut tx,
            final_match.match_id,
            two_v_three.match_id,
            snakes[1],
        )
        .await?;
        let result = fill_participant_from_source(
            &mut tx,
            final_match.match_id,
            two_v_three.match_id,
            snakes[2],
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("already"));
        // A source match that doesn't feed this match is a distinct error.
        let result =
            fill_participant_from_source(&mut tx, final_match.match_id, Uuid::new_v4(), snakes[1])
                .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No participant slot")
        );

        tx.commit().await?;

        let final_participants = get_participants_for_match(&pool, final_match.match_id).await?;
        assert_eq!(snake_in_slot(&final_participants, 1), Some(snakes[0]));
        assert_eq!(snake_in_slot(&final_participants, 2), Some(snakes[1]));

        Ok(())
    }
}
