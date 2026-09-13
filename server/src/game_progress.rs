//! Evidence for a game's last active phase. A closed span alone is not a
//! completion receipt: cancelled futures also close their tracing spans.

use std::{future::Future, time::Instant};

use tracing::Instrument;
use uuid::Uuid;

pub async fn phase<T>(
    game_id: Uuid,
    phase: &'static str,
    turn: Option<i32>,
    work: impl Future<Output = cja::Result<T>>,
) -> cja::Result<T> {
    let span = tracing::info_span!("arena.game.phase", %game_id, phase, turn);
    async move {
        let mut progress = Progress { game_id, phase, turn, started: Instant::now(), finished: false };
        tracing::info!(event_type = "game_phase", %game_id, phase, turn, state = "started", "Game phase started");
        let result = work.await;
        progress.finished = true;
        let duration_ms = progress.started.elapsed().as_millis() as u64;
        match &result {
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

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

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
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            if fields.0.get("event_type").map(String::as_str) == Some("game_phase") {
                self.0.lock().unwrap().push(fields.0);
            }
        }
    }

    #[tokio::test]
    async fn completed_and_failed_phases_preserve_results_and_error_causes() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let game_id = Uuid::new_v4();
        async {
            assert_eq!(
                phase(game_id, "persist_turn", Some(9), async { Ok(42) })
                    .await
                    .unwrap(),
                42
            );
            let result: cja::Result<()> = phase(game_id, "finish_game", Some(9), async {
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
        let records = capture.0.lock().unwrap();
        assert_eq!(
            records
                .iter()
                .map(|r| r["state"].as_str())
                .collect::<Vec<_>>(),
            ["started", "completed", "started", "failed"]
        );
        assert!(
            records
                .iter()
                .all(|r| r["game_id"] == game_id.to_string() && r["turn"] == "9")
        );
        assert_eq!(
            records[3]["error"],
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
                    "request_moves",
                    Some(3),
                    std::future::pending::<cja::Result<()>>(),
                ),
            )
            .await;
            assert!(result.is_err());
        }
        .with_subscriber(subscriber)
        .await;
        let records = capture.0.lock().unwrap();
        assert_eq!(
            records
                .iter()
                .map(|r| r["state"].as_str())
                .collect::<Vec<_>>(),
            ["started", "cancelled"]
        );
        assert!(
            records
                .iter()
                .all(|r| r["game_id"] == game_id.to_string() && r["phase"] == "request_moves")
        );
    }
}
