//! Shared telemetry capture for the bin test binary.
//!
//! Exactly ONE `set_global_default` lives in this binary, in
//! [`Capture::shared`] — a second would panic depending on test-thread
//! ordering, and a scoped `with_subscriber` races callsite interest caching
//! and can record nothing. Tests share the global capture and filter by
//! their own span groups (path-keyed for `server.request`) or unique UUIDs;
//! buffers are never cleared.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tracing::field::Visit;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

#[derive(Clone, Default)]
pub(crate) struct Capture {
    /// (is_span, fields) rows for `arena.game.phase` spans / `game_phase`
    /// events — the moved storage_tests buffer.
    phases: Arc<Mutex<Vec<Record>>>,
    /// capture sequence -> url.path, one entry per `server.request` span
    /// (entries live forever: sequences are never reused).
    request_paths: Arc<Mutex<HashMap<u64, String>>>,
    /// (capture sequence, field, value) per late `record` on a
    /// `server.request` span.
    request_records: Arc<Mutex<Vec<(u64, String, String)>>>,
    /// Monotonic capture sequence for `server.request` spans. Do NOT key
    /// these buffers on tracing span ids: a registry id is a slab slot plus
    /// a generation, so the exact id repeats once a slot has closed 8191
    /// spans (measured on tracing-subscriber 0.3.23). These buffers outlive
    /// the span, so a repeated id would stitch a closed span's path onto
    /// another span's records.
    request_seq: Arc<AtomicU64>,
}

/// Extension payload tying a `server.request` span to its capture sequence.
#[derive(Debug)]
struct RequestSeq(u64);

type Record = (bool, BTreeMap<String, String>);

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

impl<S> tracing_subscriber::Layer<S> for Capture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let name = attrs.metadata().name();
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        match name {
            "arena.game.phase" => self.phases.lock().unwrap().push((true, fields.0)),
            // cja's make_span records url.path at span creation (trace.rs:30)
            "server.request" => {
                let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
                if let Some(span) = ctx.span(id) {
                    span.extensions_mut().insert(RequestSeq(seq));
                }
                if let Some(path) = fields.0.get("url.path") {
                    self.request_paths.lock().unwrap().insert(seq, path.clone());
                }
            }
            _ => {}
        }
    }

    fn on_record(
        &self,
        id: &tracing::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Only server.request spans carry a RequestSeq extension — the
        // extension check replaces any span-name check. cja's own late
        // records (http.response.status_code, content_type) land here too;
        // harmless, the test assertions look at user_id/auth_type only.
        let Some(span) = ctx.span(id) else { return };
        let Some(seq) = span
            .extensions()
            .get::<RequestSeq>()
            .map(|RequestSeq(s)| *s)
        else {
            return;
        };
        let mut fields = Fields::default();
        values.record(&mut fields);
        let mut buf = self.request_records.lock().unwrap();
        for (field, value) in fields.0 {
            buf.push((seq, field, value));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.get("event_type").map(String::as_str) == Some("game_phase") {
            self.phases.lock().unwrap().push((false, fields.0));
        }
    }
}

impl Capture {
    pub(crate) fn shared() -> &'static Self {
        static CAPTURE: OnceLock<Capture> = OnceLock::new();
        CAPTURE.get_or_init(|| {
            let capture = Capture::default();
            // Keep callsite interest stable for concurrent database tests. Each
            // assertion selects its own game UUID from the shared capture.
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(capture.clone()),
            )
            .unwrap();
            capture
        })
    }

    pub(crate) fn records(&self, game_id: Uuid, spans: bool) -> Vec<BTreeMap<String, String>> {
        self.phases
            .lock()
            .unwrap()
            .iter()
            .filter(|(span, fields)| {
                *span == spans && fields.get("game_id") == Some(&game_id.to_string())
            })
            .map(|(_, fields)| fields.clone())
            .collect()
    }

    /// Lifecycle states in emission order: `started` comes from the phase
    /// span's creation, terminal states from events.
    pub(crate) fn states(&self, game_id: Uuid) -> Vec<(String, String)> {
        self.phases
            .lock()
            .unwrap()
            .iter()
            .map(|(_, fields)| fields)
            .filter(|fields| fields.get("game_id") == Some(&game_id.to_string()))
            .map(|fields| {
                (
                    fields["phase"]
                        .trim_start_matches("persist_turn.")
                        .to_owned(),
                    fields["state"].clone(),
                )
            })
            .collect()
    }

    /// Field records on `server.request` spans whose url.path == `path`,
    /// one Vec<(field, value)> per matching span — including spans that
    /// recorded nothing (empty vec). Grouping keys on the capture sequence,
    /// never the (reused) tracing span id. Never clear the buffers.
    pub(crate) fn server_request_record_groups(&self, path: &str) -> Vec<Vec<(String, String)>> {
        let seqs: Vec<u64> = {
            let paths = self.request_paths.lock().unwrap();
            paths
                .iter()
                .filter(|(_, p)| p.as_str() == path)
                .map(|(s, _)| *s)
                .collect()
        }; // paths lock released before the records lock — never hold two at once
        let records = self.request_records.lock().unwrap();
        seqs.into_iter()
            .map(|seq| {
                records
                    .iter()
                    .filter(|(s, _, _)| *s == seq)
                    .map(|(_, f, v)| (f.clone(), v.clone()))
                    .collect()
            })
            .collect()
    }
}
