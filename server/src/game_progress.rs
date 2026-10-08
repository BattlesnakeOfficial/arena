//! Evidence for a game's last active phase. A closed span alone is not a
//! completion receipt: cancelled futures also close their tracing spans.
//!
//! Per-turn phases are emitted at DEBUG, and Eyes keeps arena's DEBUG
//! telemetry for 7 days. Once-per-game phases stay at INFO (30 days). Failed
//! and cancelled phases are ERROR and WARN whatever their cadence, so a
//! failure never ages out early.

use std::{future::Future, time::Instant};

use tracing::{Instrument, Level};
use uuid::Uuid;

/// A unit of game-runner work with a start receipt and one terminal event.
/// `as_str` is the `phase` field value, a contract with `eyes investigate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    LoadGame,
    ResetGame,
    ClaimStart,
    PrepareSnakes,
    StartSnakes,
    RequestMoves,
    PersistTurn,
    PersistTurnAcquireFrameConnection,
    PersistTurnInsertFrame,
    PersistTurnAcquireSnakeConnection,
    PersistTurnInsertSnake,
    EndSnakes,
    FinishGame,
    PostCompletion,
}

impl Phase {
    pub const ALL: [Phase; 14] = [
        Phase::LoadGame,
        Phase::ResetGame,
        Phase::ClaimStart,
        Phase::PrepareSnakes,
        Phase::StartSnakes,
        Phase::RequestMoves,
        Phase::PersistTurn,
        Phase::PersistTurnAcquireFrameConnection,
        Phase::PersistTurnInsertFrame,
        Phase::PersistTurnAcquireSnakeConnection,
        Phase::PersistTurnInsertSnake,
        Phase::EndSnakes,
        Phase::FinishGame,
        Phase::PostCompletion,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::LoadGame => "load_game",
            Phase::ResetGame => "reset_game",
            Phase::ClaimStart => "claim_start",
            Phase::PrepareSnakes => "prepare_snakes",
            Phase::StartSnakes => "start_snakes",
            Phase::RequestMoves => "request_moves",
            Phase::PersistTurn => "persist_turn",
            Phase::PersistTurnAcquireFrameConnection => "persist_turn.acquire_frame_connection",
            Phase::PersistTurnInsertFrame => "persist_turn.insert_frame",
            Phase::PersistTurnAcquireSnakeConnection => "persist_turn.acquire_snake_connection",
            Phase::PersistTurnInsertSnake => "persist_turn.insert_snake",
            Phase::EndSnakes => "end_snakes",
            Phase::FinishGame => "finish_game",
            Phase::PostCompletion => "post_completion",
        }
    }

    /// Whether the phase runs every turn. `start_snakes` and `finish_game`
    /// carry a turn number but run once per game, so a turn number doesn't
    /// decide this.
    pub fn per_turn(self) -> bool {
        match self {
            Phase::RequestMoves
            | Phase::PersistTurn
            | Phase::PersistTurnAcquireFrameConnection
            | Phase::PersistTurnInsertFrame
            | Phase::PersistTurnAcquireSnakeConnection
            | Phase::PersistTurnInsertSnake => true,
            Phase::LoadGame
            | Phase::ResetGame
            | Phase::ClaimStart
            | Phase::PrepareSnakes
            | Phase::StartSnakes
            | Phase::EndSnakes
            | Phase::FinishGame
            | Phase::PostCompletion => false,
        }
    }

    /// Level of the start span and the `completed` event.
    pub fn level(self) -> Level {
        if self.per_turn() {
            Level::DEBUG
        } else {
            Level::INFO
        }
    }
}

pub async fn phase<T>(
    game_id: Uuid,
    phase: Phase,
    turn: Option<i32>,
    work: impl Future<Output = cja::Result<T>>,
) -> cja::Result<T> {
    let per_turn = phase.per_turn();
    let phase = phase.as_str();
    // The span's creation record is the `started` receipt. A separate started
    // event would add a row per phase, per turn, without adding evidence.
    // tracing needs a constant level per callsite, hence the two arms.
    let span = if per_turn {
        tracing::debug_span!("arena.game.phase", event_type = "game_phase", %game_id, phase, turn, state = "started")
    } else {
        tracing::info_span!("arena.game.phase", event_type = "game_phase", %game_id, phase, turn, state = "started")
    };
    async move {
        let mut progress = Progress { game_id, phase, turn, started: Instant::now(), finished: false };
        let result = work.await;
        progress.finished = true;
        let duration_ms = progress.started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) if per_turn => tracing::debug!(event_type = "game_phase", %game_id, phase, turn, state = "completed", duration_ms, "Game phase completed"),
            Ok(_) => tracing::info!(event_type = "game_phase", %game_id, phase, turn, state = "completed", duration_ms, "Game phase completed"),
            Err(error) => tracing::error!(event_type = "game_phase", %game_id, phase, turn, state = "failed", duration_ms, error = %format!("{error:#}"), "Game phase failed"),
        }
        result
    }
    .instrument(span)
    .await
}

struct Progress {
    game_id: Uuid,
    phase: &'static str,
    turn: Option<i32>,
    started: Instant,
    finished: bool,
}

impl Drop for Progress {
    fn drop(&mut self) {
        if !self.finished {
            tracing::warn!(event_type = "game_phase", game_id = %self.game_id, phase = self.phase, turn = self.turn,
                state = "cancelled", duration_ms = self.started.elapsed().as_millis() as u64,
                "Game phase future cancelled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };
    use tracing::{field::Visit, instrument::WithSubscriber};
    use tracing_subscriber::{Layer, prelude::*};

    /// A game-phase record, tagged with whether it came from a span's
    /// creation (`true`) or an event (`false`).
    type Record = (bool, BTreeMap<String, String>);

    /// Game-phase records in emission order.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Record>>>);

    impl Capture {
        fn push_if_phase(&self, span: bool, fields: Fields) {
            if fields.0.get("event_type").map(String::as_str) == Some("game_phase") {
                self.0.lock().unwrap().push((span, fields.0));
            }
        }

        /// `(is_span, state, level)` per record, in emission order.
        fn levels(&self) -> Vec<(bool, String, String)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(span, fields)| (*span, fields["state"].clone(), fields["level"].clone()))
                .collect()
        }

        fn states(&self) -> Vec<(bool, String)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(span, fields)| (*span, fields["state"].clone()))
                .collect()
        }
    }

    #[derive(Default)]
    struct Fields(BTreeMap<String, String>);

    impl Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            fields
                .0
                .insert("level".into(), attrs.metadata().level().to_string());
            self.push_if_phase(true, fields);
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            fields
                .0
                .insert("level".into(), event.metadata().level().to_string());
            self.push_if_phase(false, fields);
        }
    }

    #[tokio::test]
    async fn completed_and_failed_phases_preserve_results_and_error_causes() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let game_id = Uuid::new_v4();
        async {
            assert_eq!(
                phase(game_id, Phase::PersistTurn, Some(9), async { Ok(42) })
                    .await
                    .unwrap(),
                42
            );
            let result: cja::Result<()> = phase(game_id, Phase::FinishGame, Some(9), async {
                Err(color_eyre::eyre::eyre!("database unavailable").wrap_err("finish transaction"))
            })
            .await;
            assert_eq!(
                format!("{:#}", result.unwrap_err()),
                "finish transaction: database unavailable"
            );
        }
        .with_subscriber(subscriber)
        .await;
        assert_eq!(
            capture.states(),
            [
                (true, "started".into()),
                (false, "completed".into()),
                (true, "started".into()),
                (false, "failed".into()),
            ]
        );
        let records = capture.0.lock().unwrap();
        assert!(
            records
                .iter()
                .all(|(_, r)| r["game_id"] == game_id.to_string() && r["turn"] == "9")
        );
        assert_eq!(
            records[3].1["error"],
            "finish transaction: database unavailable"
        );
    }

    #[tokio::test]
    async fn dropping_an_active_phase_reports_cancellation_without_completion() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let game_id = Uuid::new_v4();
        async {
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(20),
                phase(
                    game_id,
                    Phase::RequestMoves,
                    Some(3),
                    std::future::pending::<cja::Result<()>>(),
                ),
            )
            .await;
            assert!(result.is_err());
        }
        .with_subscriber(subscriber)
        .await;
        assert_eq!(
            capture.states(),
            [(true, "started".into()), (false, "cancelled".into())]
        );
        assert!(
            capture
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|(_, r)| r["game_id"] == game_id.to_string() && r["phase"] == "request_moves")
        );
    }

    #[test]
    fn per_turn_phases_are_exactly_the_turn_loop() {
        let per_turn: Vec<&str> = Phase::ALL
            .into_iter()
            .filter(|phase| phase.per_turn())
            .map(Phase::as_str)
            .collect();
        assert_eq!(
            per_turn,
            [
                "request_moves",
                "persist_turn",
                "persist_turn.acquire_frame_connection",
                "persist_turn.insert_frame",
                "persist_turn.acquire_snake_connection",
                "persist_turn.insert_snake",
            ]
        );
    }

    #[tokio::test]
    async fn completed_phases_emit_debug_per_turn_and_info_per_game() {
        for phase_kind in Phase::ALL {
            let capture = Capture::default();
            let subscriber = tracing_subscriber::registry().with(capture.clone());
            phase(Uuid::new_v4(), phase_kind, Some(1), async { Ok(()) })
                .with_subscriber(subscriber)
                .await
                .unwrap();
            let level = if phase_kind.per_turn() {
                "DEBUG"
            } else {
                "INFO"
            };
            assert_eq!(
                capture.levels(),
                [
                    (true, "started".into(), level.into()),
                    (false, "completed".into(), level.into()),
                ],
                "{}",
                phase_kind.as_str()
            );
        }
    }

    #[tokio::test]
    async fn failures_and_cancellations_keep_their_levels_for_every_cadence() {
        for phase_kind in [Phase::PersistTurnInsertSnake, Phase::FinishGame] {
            let capture = Capture::default();
            let subscriber = tracing_subscriber::registry().with(capture.clone());
            async {
                let failed: cja::Result<()> = phase(Uuid::new_v4(), phase_kind, Some(1), async {
                    Err(color_eyre::eyre::eyre!("boom"))
                })
                .await;
                assert!(failed.is_err());
                let cancelled = tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    phase(
                        Uuid::new_v4(),
                        phase_kind,
                        Some(1),
                        std::future::pending::<cja::Result<()>>(),
                    ),
                )
                .await;
                assert!(cancelled.is_err());
            }
            .with_subscriber(subscriber)
            .await;
            let start = phase_kind.level().to_string();
            assert_eq!(
                capture.levels(),
                [
                    (true, "started".into(), start.clone()),
                    (false, "failed".into(), "ERROR".into()),
                    (true, "started".into(), start),
                    (false, "cancelled".into(), "WARN".into()),
                ],
                "{}",
                phase_kind.as_str()
            );
        }
    }
}
