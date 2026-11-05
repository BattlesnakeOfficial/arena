//! Final standings of a finished game.
//!
//! Snakes are ranked by how long they lasted: survivors first, then by the
//! turn they were eliminated, latest first. Snakes that lasted equally long
//! share a placement (competition ranking: 1, 2, 2, 4), the way Play ranked
//! games. So two snakes that die in the same head-to-head tie rather than one
//! of them winning on roster order.
//!
//! A game's winner is the one snake placed first on its own. A shared first
//! place is a draw: nobody won.

/// Each snake's placement, keyed by snake ID, in board order.
pub fn from_final_snakes(snakes: &[rules::Snake]) -> Vec<(String, i32)> {
    let lasted = |snake: &rules::Snake| {
        if snake.eliminated_cause.is_eliminated() {
            snake.eliminated_on_turn
        } else {
            i32::MAX
        }
    };

    snakes
        .iter()
        .map(|snake| {
            let ahead = snakes
                .iter()
                .filter(|other| lasted(other) > lasted(snake))
                .count();
            (snake.id.clone(), ahead as i32 + 1)
        })
        .collect()
}

/// The item placed first on its own. `None` when first place is shared (a
/// draw) or nothing has a placement yet.
pub fn outright_winner<T>(
    items: impl IntoIterator<Item = T>,
    placement: impl Fn(&T) -> Option<i32>,
) -> Option<T> {
    let mut first = items.into_iter().filter(|item| placement(item) == Some(1));
    let winner = first.next()?;
    first.next().is_none().then_some(winner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
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

    fn placements(snakes: &[Snake]) -> Vec<(&str, i32)> {
        let placed = from_final_snakes(snakes);
        snakes
            .iter()
            .zip(placed)
            .map(|(snake, (id, placement))| {
                assert_eq!(snake.id, id, "placements stay in board order");
                (snake.id.as_str(), placement)
            })
            .collect()
    }

    fn winner(snakes: &[Snake]) -> Option<String> {
        outright_winner(from_final_snakes(snakes), |(_, placement)| Some(*placement))
            .map(|(id, _)| id)
    }

    #[test]
    fn survivor_places_first() {
        let snakes = vec![
            snake("loser", EliminationCause::OutOfHealth, 40),
            snake("winner", EliminationCause::NotEliminated, 0),
        ];
        assert_eq!(placements(&snakes), vec![("loser", 2), ("winner", 1)]);
        assert_eq!(winner(&snakes), Some("winner".to_string()));
    }

    #[test]
    fn later_elimination_places_higher() {
        let snakes = vec![
            snake("early", EliminationCause::OutOfBounds, 10),
            snake("late", EliminationCause::OutOfHealth, 42),
        ];
        assert_eq!(placements(&snakes), vec![("early", 2), ("late", 1)]);
        assert_eq!(winner(&snakes), Some("late".to_string()));
    }

    /// The reported bug: equal-length snakes that kill each other head-to-head
    /// on the last turn used to split 1st/2nd by roster order.
    #[test]
    fn simultaneous_final_elimination_is_a_draw() {
        let snakes = vec![
            snake("first-joined", EliminationCause::HeadToHeadCollision, 241),
            snake("last-joined", EliminationCause::HeadToHeadCollision, 241),
            snake("early", EliminationCause::HeadToHeadCollision, 36),
            snake("starved", EliminationCause::OutOfHealth, 84),
        ];
        assert_eq!(
            placements(&snakes),
            vec![
                ("first-joined", 1),
                ("last-joined", 1),
                ("early", 4),
                ("starved", 3),
            ]
        );
        assert_eq!(winner(&snakes), None);
    }

    #[test]
    fn tie_below_first_skips_the_next_placement() {
        let snakes = vec![
            snake("winner", EliminationCause::NotEliminated, 0),
            snake("a", EliminationCause::HeadToHeadCollision, 30),
            snake("b", EliminationCause::HeadToHeadCollision, 30),
            snake("last", EliminationCause::OutOfBounds, 5),
        ];
        assert_eq!(
            placements(&snakes),
            vec![("winner", 1), ("a", 2), ("b", 2), ("last", 4)]
        );
        assert_eq!(winner(&snakes), Some("winner".to_string()));
    }

    #[test]
    fn multiple_survivors_share_first() {
        let snakes = vec![
            snake("a", EliminationCause::NotEliminated, 0),
            snake("b", EliminationCause::NotEliminated, 0),
            snake("c", EliminationCause::OutOfHealth, 100),
        ];
        assert_eq!(placements(&snakes), vec![("a", 1), ("b", 1), ("c", 3)]);
        assert_eq!(winner(&snakes), None);
    }

    #[test]
    fn outright_winner_needs_exactly_one_first_place() {
        let first = |p: &Option<i32>| *p;
        assert_eq!(outright_winner([Some(2), Some(1)], first), Some(Some(1)));
        assert_eq!(outright_winner([Some(1), Some(1)], first), None);
        assert_eq!(outright_winner([None, None], first), None);
        assert_eq!(outright_winner(Vec::<Option<i32>>::new(), first), None);
    }

    fn arb_snakes() -> impl Strategy<Value = Vec<Snake>> {
        // `None` survives; `Some(turn)` was eliminated on that turn. A small
        // turn range makes same-turn eliminations common.
        prop::collection::vec(prop::option::of(1..6i32), 1..=8).prop_map(|fates| {
            fates
                .into_iter()
                .enumerate()
                .map(|(i, fate)| match fate {
                    None => snake(&i.to_string(), EliminationCause::NotEliminated, 0),
                    Some(turn) => snake(&i.to_string(), EliminationCause::OutOfHealth, turn),
                })
                .collect()
        })
    }

    proptest! {
        /// Placement follows survival alone: a snake that lasted longer places
        /// strictly higher, and snakes that lasted equally long tie. So
        /// reordering the roster can never change anyone's placement.
        #[test]
        fn placement_depends_only_on_survival(snakes in arb_snakes()) {
            let placed = from_final_snakes(&snakes);
            let lasted = |s: &Snake| (!s.eliminated_cause.is_eliminated(), s.eliminated_on_turn);
            for (a, (_, pa)) in snakes.iter().zip(&placed) {
                prop_assert!(*pa >= 1 && *pa as usize <= snakes.len());
                for (b, (_, pb)) in snakes.iter().zip(&placed) {
                    prop_assert_eq!(lasted(a).cmp(&lasted(b)), pb.cmp(pa));
                }
            }

            let mut reversed = snakes.clone();
            reversed.reverse();
            let mut again = from_final_snakes(&reversed);
            again.reverse();
            prop_assert_eq!(&placed, &again);
        }
    }
}
