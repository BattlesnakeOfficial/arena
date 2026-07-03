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
    for planned in plan.matches.iter().rev() {
        let next_match_id = planned
            .next_position
            .map(|next_pos| match_ids[&(planned.round + 1, next_pos)]);
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

    // Round-1 seeded participants.
    for planned in plan.matches.iter().filter(|m| m.round == 1) {
        let match_id = match_ids[&(planned.round, planned.position)];
        for seed in planned.seed_slots.iter().flatten() {
            create_match_participant(
                &mut **tx,
                CreateMatchParticipant {
                    match_id,
                    battlesnake_id: Some(by_seed[seed].battlesnake_id),
                    source_match_id: None,
                    participant_type: ParticipantType::Seed,
                    seed_position: Some(*seed),
                },
            )
            .await?;
        }
    }

    // Later rounds: one empty winner-fed slot per feeder match.
    for planned in plan.matches.iter().filter(|m| m.round > 1) {
        let match_id = match_ids[&(planned.round, planned.position)];
        for feeder_position in [planned.position * 2, planned.position * 2 + 1] {
            let source_match_id = match_ids[&(planned.round - 1, feeder_position)];
            create_match_participant(
                &mut **tx,
                CreateMatchParticipant {
                    match_id,
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
        let match_id = match_ids[&(planned.round, planned.position)];
        let seed = planned
            .seed_slots
            .iter()
            .flatten()
            .next()
            .expect("bye match has exactly one seed");
        let winner_battlesnake_id = by_seed[seed].battlesnake_id;

        complete_match_with_winner(tx, match_id, winner_battlesnake_id).await?;

        if let Some(next_pos) = planned.next_position {
            let next_match_id = match_ids[&(planned.round + 1, next_pos)];
            fill_participant_from_source(tx, next_match_id, match_id, winner_battlesnake_id)
                .await?;
        }
    }

    Ok(())
}

/// Mark a match completed with the given winner.
pub async fn complete_match_with_winner(
    tx: &mut Transaction<'_, Postgres>,
    match_id: Uuid,
    winner_battlesnake_id: Uuid,
) -> cja::Result<()> {
    sqlx::query!(
        "UPDATE tournament_matches SET status = $2, winner_id = $3 WHERE match_id = $1",
        match_id,
        MatchStatus::Completed.as_str(),
        winner_battlesnake_id,
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to complete match")?;

    Ok(())
}

/// Fill the participant slot in `match_id` that is fed by `source_match_id`.
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
        "#,
        match_id,
        source_match_id,
        battlesnake_id,
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to advance participant into next match")?;

    if updated.rows_affected() != 1 {
        return Err(color_eyre::eyre::eyre!(
            "Expected exactly one participant slot in match {match_id} fed by {source_match_id}, updated {}",
            updated.rows_affected()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

                // Positions are 0..count, visual rows strictly increase, and
                // every non-final match feeds position / 2 in the next round.
                for (i, m) in in_round.iter().enumerate() {
                    prop_assert_eq!(m.position, i as i32);
                    prop_assert_eq!(m.visual_column, round - 1);
                    if round < plan.total_rounds {
                        prop_assert_eq!(m.next_position, Some(m.position / 2));
                    } else {
                        prop_assert_eq!(m.next_position, None);
                    }
                    if i > 0 {
                        prop_assert!(m.visual_row > in_round[i - 1].visual_row);
                    }
                }
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
}
