//! Shared telemetry capture for the bin test binary.
//!
//! Exactly ONE `set_global_default` lives in this binary, in
//! [`Capture::shared`] — a second would panic depending on test-thread
//! ordering, and a scoped `with_subscriber` races callsite interest caching
//! and can record nothing. Tests share the global capture and filter by
//! their own URI-keyed request close events or unique UUIDs; buffers are
//! never cleared.

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
    /// capture sequence -> full request URI (path + query, the span's
    /// `uri` field), one entry per `server.request` span. Entries live
    /// forever: sequences are never reused. Tests probe with unique
    /// `?probe=<uuid>` URIs so a group is exactly one test's own
    /// request, immune to sibling tests' traffic on shared paths.
    request_uris: Arc<Mutex<HashMap<u64, String>>>,
    /// `finished processing request` events, one (capture seq, fields)
    /// entry per event, tied to the emitting `server.request` span's
    /// capture sequence. Never cleared.
    request_close_events: Arc<Mutex<Vec<RequestCloseEvent>>>,
    /// Monotonic capture sequence for `server.request` spans. Do NOT key
    /// these buffers on tracing span ids: a registry id is a slab slot plus
    /// a generation, so the exact id repeats once a slot has closed 8191
    /// spans (measured on tracing-subscriber 0.3.23). These buffers outlive
    /// the span, so a repeated id would stitch a closed span's URI onto
    /// another span's close events.
    request_seq: Arc<AtomicU64>,
    /// Fields of `battlesnake_created` / `user_authenticated` events — the
    /// product-event buffer for the creation-path and sign-in tests.
    /// Never cleared.
    events: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

/// Extension payload tying a `server.request` span to its capture sequence.
#[derive(Debug)]
struct RequestSeq(u64);

type Record = (bool, BTreeMap<String, String>);
type RequestCloseEvent = (u64, BTreeMap<String, String>);

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
            // cja's make_span records `uri` (the full Display form, path
            // + query) at span creation
            "server.request" => {
                let seq = self.request_seq.fetch_add(1, Ordering::Relaxed);
                if let Some(span) = ctx.span(id) {
                    span.extensions_mut().insert(RequestSeq(seq));
                }
                if let Some(uri) = fields.0.get("uri") {
                    self.request_uris.lock().unwrap().insert(seq, uri.clone());
                }
            }
            _ => {}
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        match fields.0.get("event_type").map(String::as_str) {
            Some("game_phase") => self.phases.lock().unwrap().push((false, fields.0)),
            Some("battlesnake_created") | Some("user_authenticated") => {
                self.events.lock().unwrap().push(fields.0);
            }
            _ => {
                // The message is a `format_args!` value, so it captures
                // through `record_debug` as the bare unquoted string.
                if fields.0.get("message").map(String::as_str)
                    == Some("finished processing request")
                    && let Some(seq) = ctx.event_span(event).and_then(|span| {
                        span.extensions()
                            .get::<RequestSeq>()
                            .map(|RequestSeq(s)| *s)
                    })
                {
                    self.request_close_events
                        .lock()
                        .unwrap()
                        .push((seq, fields.0));
                }
            }
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

    /// `finished processing request` events from requests whose full URI
    /// == `uri`, one Vec<fields> per request span (exactly one event per
    /// request is the contract). Grouping keys on the capture sequence,
    /// never the (reused) tracing span id. Never clears the buffers.
    pub(crate) fn request_close_event_groups(
        &self,
        uri: &str,
    ) -> Vec<Vec<BTreeMap<String, String>>> {
        let seqs: Vec<u64> = {
            let uris = self.request_uris.lock().unwrap();
            uris.iter()
                .filter(|(_, u)| u.as_str() == uri)
                .map(|(s, _)| *s)
                .collect()
        }; // uris lock released before the events lock — never hold two at once
        let events = self.request_close_events.lock().unwrap();
        seqs.into_iter()
            .map(|seq| {
                events
                    .iter()
                    .filter(|(s, _)| *s == seq)
                    .map(|(_, f)| f.clone())
                    .collect()
            })
            .collect()
    }

    /// Fields of `event_type` events whose `user_id` field equals `user_id`,
    /// in emission order. Never clear the buffer — each test filters on its
    /// own unique user id instead.
    pub(crate) fn events(&self, event_type: &str, user_id: Uuid) -> Vec<BTreeMap<String, String>> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|fields| {
                fields.get("event_type").map(String::as_str) == Some(event_type)
                    && fields.get("user_id").map(String::as_str) == Some(&user_id.to_string())
            })
            .cloned()
            .collect()
    }
}
