use std::fmt;

use cja::setup::EyesShutdownHandle;
use color_eyre::eyre::Context as _;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{self as fmt_layer, FmtContext, FormatEvent, FormatFields, MakeWriter},
    registry::LookupSpan,
};

/// Newtype for storing GCP trace context in span extensions.
pub struct TraceContext(pub String);

/// Custom JSON formatter that produces GCP Cloud Logging-compatible structured JSON.
struct GcpJsonFormatter;

impl<S, N> FormatEvent<S, N> for GcpJsonFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: fmt_layer::format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        use tracing_subscriber::registry::SpanRef;

        let mut map = serde_json::Map::new();

        // Map severity
        let severity = match *event.metadata().level() {
            tracing::Level::TRACE | tracing::Level::DEBUG => "DEBUG",
            tracing::Level::INFO => "INFO",
            tracing::Level::WARN => "WARNING",
            tracing::Level::ERROR => "ERROR",
        };
        map.insert(
            "severity".to_string(),
            serde_json::Value::String(severity.to_string()),
        );

        // Collect event fields via visitor
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        // Extract message and put it at top level
        if let Some(message) = visitor.fields.remove("message") {
            map.insert("message".to_string(), message);
        }

        // Add timestamp
        map.insert(
            "time".to_string(),
            serde_json::Value::String(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        );

        // Walk spans to find TraceContext
        if let Some(scope) = ctx.event_scope() {
            for span in scope {
                let span: SpanRef<'_, S> = span;
                let extensions = span.extensions();
                if let Some(trace_ctx) = extensions.get::<TraceContext>() {
                    map.insert(
                        "logging.googleapis.com/trace".to_string(),
                        serde_json::Value::String(trace_ctx.0.clone()),
                    );
                    break;
                }
            }
        }

        // Flatten all remaining event fields to top level
        for (key, value) in visitor.fields {
            map.insert(key, value);
        }

        let json = serde_json::Value::Object(map);
        write!(writer, "{json}")?;
        writeln!(writer)?;

        Ok(())
    }
}

/// Visitor that collects tracing event fields into a JSON map.
#[derive(Default)]
struct JsonVisitor {
    fields: serde_json::Map<String, serde_json::Value>,
}

impl tracing::field::Visit for JsonVisitor {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::from(value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{:?}", value)),
        );
    }
}

/// The structured JSON stdout layer.
///
/// `stdout_log` (`ARENA_STDOUT_LOG`) narrows what reaches stdout below the
/// global `RUST_LOG` filter without touching the other layers, so Eyes keeps
/// everything `RUST_LOG` admits: `"warn"` keeps warnings and errors, `"off"`
/// silences stdout. `None` passes through everything `RUST_LOG` admits.
fn stdout_layer<S, W>(stdout_log: Option<&str>, make_writer: W) -> color_eyre::Result<impl Layer<S>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let filter = stdout_log
        .map(|directives| {
            EnvFilter::builder()
                .parse(directives)
                .wrap_err_with(|| format!("Couldn't create stdout filter from {directives}"))
        })
        .transpose()?;

    Ok(fmt_layer::Layer::default()
        .event_format(GcpJsonFormatter)
        .with_ansi(false)
        .with_writer(make_writer)
        .with_filter(filter))
}

/// Sets up GCP-compatible structured JSON logging, plus the Eyes telemetry
/// layer when configured (see `AppConfig::eyes`).
///
/// Returns the Eyes shutdown handle when Eyes is enabled, maintaining type
/// compatibility with `cja::setup::setup_tracing` (the non-GCP path, which
/// wires the same layer itself).
pub fn setup_gcp_tracing(
    rust_log: &str,
    stdout_log: Option<&str>,
    eyes: Option<&crate::config::EyesConfig>,
    identity: &eyes_subscriber::ProcessIdentity,
    eyes_token: Option<&str>,
) -> color_eyre::Result<Option<EyesShutdownHandle>> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let env_filter = EnvFilter::builder().parse(rust_log).map_err(|e| {
        color_eyre::eyre::eyre!("Couldn't create env filter from {}: {}", rust_log, e)
    })?;

    let (eyes_layer, eyes_shutdown_handle) = match eyes {
        Some(eyes) => {
            let (builder, transport) = eyes_subscriber_builder(eyes, identity, eyes_token)?;
            let (layer, handle) = builder.build_with_transport(transport);
            println!(
                "Eyes layer configured (org: {}, app: {})",
                eyes.org_id, eyes.app_id
            );
            (Some(layer), Some(handle))
        }
        None => {
            println!("Skipping Eyes layer");
            (None, None)
        }
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer(stdout_log, std::io::stdout)?)
        .with(eyes_layer)
        .try_init()?;

    Ok(eyes_shutdown_handle)
}

/// Env-derived URL/transport (`EYES_URL`/`EYES_TRANSPORT`, read by the
/// library), boot identity, and the resolved ingest token. An explicit token
/// wins over the library's `EYES_TOKEN` fallback; `None` keeps that fallback.
fn eyes_subscriber_builder(
    eyes: &crate::config::EyesConfig,
    identity: &eyes_subscriber::ProcessIdentity,
    token: Option<&str>,
) -> color_eyre::Result<(
    eyes_subscriber::EyesSubscriberBuilder,
    eyes_subscriber::TransportType,
)> {
    let (mut builder, transport) =
        eyes_subscriber::EyesSubscriberBuilder::from_env_with_transport(eyes.org_id, eyes.app_id)
            .map_err(|error| color_eyre::eyre::eyre!("Failed to build Eyes subscriber: {error}"))?;
    builder = builder.with_process_instance_id(identity.instance_id());
    if let Some(token) = token {
        builder = builder.with_auth_token(token);
    }
    Ok((builder, transport))
}

/// Parse the `X-Cloud-Trace-Context` header value and return a GCP trace path.
///
/// Header format: `TRACE_ID/SPAN_ID;o=TRACE_TRUE`
/// Returns: `projects/{project_id}/traces/{trace_id}`
///
/// `project_id` comes from config (see `AppConfig::gcp_project_id`), not the
/// environment — this runs per request, so it must not read env.
pub fn extract_trace_context(header_value: &str, project_id: Option<&str>) -> Option<String> {
    let project_id = project_id?;

    // Extract trace_id (everything before the first '/')
    let trace_id = header_value.split('/').next()?;

    // Validate trace_id is non-empty and looks reasonable
    if trace_id.is_empty() {
        return None;
    }

    Some(format!("projects/{project_id}/traces/{trace_id}"))
}

/// Insert a GCP trace context path into the current span's extensions.
///
/// Uses the `with_subscriber` + downcast pattern since `tracing::Span` does not
/// have `extensions_mut()` directly. Silently no-ops if the subscriber is not a Registry.
pub fn insert_trace_context_into_current_span(trace_path: String) {
    let span = tracing::Span::current();
    span.with_subscriber(|(id, dispatch)| {
        if let Some(registry) = dispatch.downcast_ref::<tracing_subscriber::Registry>()
            && let Some(span_data) = tracing_subscriber::registry::LookupSpan::span(registry, id)
        {
            span_data.extensions_mut().insert(TraceContext(trace_path));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    /// In-memory stand-in for stdout.
    #[derive(Clone, Default)]
    struct CapturedOutput(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedOutput {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Emits one INFO and one WARN event under a global `info` filter (the
    /// `RUST_LOG` role). Returns what reached the stdout layer and what reached
    /// an unfiltered sibling layer (the Eyes role).
    fn emit_info_and_warn(stdout_log: Option<&str>) -> (String, String) {
        let stdout = CapturedOutput::default();
        let sibling = CapturedOutput::default();
        let (stdout_writer, sibling_writer) = (stdout.clone(), sibling.clone());

        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(stdout_layer(stdout_log, move || stdout_writer.clone()).unwrap())
            .with(fmt_layer::layer().with_writer(move || sibling_writer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("info event");
            tracing::warn!("warn event");
            tracing::debug!("debug event");
        });

        (stdout.text(), sibling.text())
    }

    #[test]
    fn stdout_log_unset_passes_everything_rust_log_admits() {
        let (stdout, _) = emit_info_and_warn(None);
        assert!(stdout.contains("info event"), "{stdout}");
        assert!(stdout.contains("warn event"), "{stdout}");
        assert!(!stdout.contains("debug event"), "{stdout}");
    }

    #[test]
    fn stdout_log_warn_keeps_only_warnings_on_stdout() {
        let (stdout, sibling) = emit_info_and_warn(Some("warn"));
        assert!(!stdout.contains("info event"), "{stdout}");
        assert!(stdout.contains("\"severity\":\"WARNING\""), "{stdout}");
        // Narrowing stdout must not narrow Eyes.
        assert!(sibling.contains("info event"), "{sibling}");
        assert!(sibling.contains("warn event"), "{sibling}");
    }

    #[test]
    fn stdout_log_off_silences_stdout_only() {
        let (stdout, sibling) = emit_info_and_warn(Some("off"));
        assert!(stdout.is_empty(), "{stdout}");
        assert!(sibling.contains("info event"), "{sibling}");
    }

    #[test]
    fn stdout_log_rejects_invalid_directives() {
        let result =
            stdout_layer::<tracing_subscriber::Registry, _>(Some("arena=loud"), std::io::sink);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_trace_context_valid() {
        let result = extract_trace_context(
            "105445aa7843bc8bf206b12000100000/1;o=1",
            Some("test-project"),
        );
        assert_eq!(
            result,
            Some("projects/test-project/traces/105445aa7843bc8bf206b12000100000".to_string())
        );
    }

    #[test]
    fn test_extract_trace_context_no_options() {
        let result =
            extract_trace_context("105445aa7843bc8bf206b12000100000/1", Some("test-project"));
        assert_eq!(
            result,
            Some("projects/test-project/traces/105445aa7843bc8bf206b12000100000".to_string())
        );
    }

    #[test]
    fn test_extract_trace_context_empty() {
        let result = extract_trace_context("", Some("test-project"));
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_trace_context_no_project_id() {
        // No configured project id → no trace path, regardless of header.
        let result = extract_trace_context("105445aa7843bc8bf206b12000100000/1;o=1", None);
        assert_eq!(result, None);
    }

    fn test_eyes_config() -> crate::config::EyesConfig {
        crate::config::EyesConfig {
            org_id: uuid::Uuid::new_v4(),
            app_id: uuid::Uuid::new_v4(),
            token: None,
        }
    }

    /// eyes-subscriber's hand-written builder `Debug` redacts the token to
    /// its first 12 chars + `…`, so asserting on that prefix never prints a
    /// full (real) token.
    #[test]
    fn token_is_applied_to_the_builder() {
        let identity = eyes_subscriber::ProcessIdentity::new("arena-test");
        let (builder, _transport) =
            eyes_subscriber_builder(&test_eyes_config(), &identity, Some("eyes_test_token"))
                .unwrap();
        let debug = format!("{builder:?}");
        assert!(debug.contains("auth_token: Some(\"eyes_test_to"), "{debug}");
        // The boot identity round-trips onto the builder.
        assert!(debug.contains(&format!(
            "process_instance_id: Some({}",
            identity.instance_id()
        )));
    }

    #[test]
    fn no_token_leaves_the_builder_explicit() {
        let identity = eyes_subscriber::ProcessIdentity::new("arena-test");
        let (builder, _transport) =
            eyes_subscriber_builder(&test_eyes_config(), &identity, None).unwrap();
        let debug = format!("{builder:?}");
        // The builder field stays `None` regardless of ambient `EYES_TOKEN`:
        // the env read happens later inside `build_with_transport`.
        assert!(debug.contains("auth_token: None"), "{debug}");
    }

    #[tokio::test]
    async fn layer_sends_bearer_to_the_events_endpoint() {
        use std::sync::{Arc, Mutex};

        use eyes_subscriber::TransportType;

        let org_id = uuid::Uuid::new_v4();
        let app_id = uuid::Uuid::new_v4();
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let router = {
            let captured = captured.clone();
            axum::Router::new().route(
                "/api/orgs/{org}/apps/{app}/events",
                axum::routing::post(
                    move |headers: axum::http::HeaderMap,
                          axum::extract::Path((org, app)): axum::extract::Path<(
                        String,
                        String,
                    )>| {
                        let captured = captured.clone();
                        async move {
                            assert_eq!(org, org_id.to_string());
                            assert_eq!(app, app_id.to_string());
                            let value = headers
                                .get(axum::http::header::AUTHORIZATION)
                                .map(|value| value.to_str().unwrap().to_owned());
                            captured.lock().unwrap().push(value.unwrap_or_default());
                            axum::http::StatusCode::OK
                        }
                    },
                ),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let identity = eyes_subscriber::ProcessIdentity::new("arena-test");
        let (layer, handle) =
            eyes_subscriber::EyesSubscriberBuilder::new(format!("http://{addr}"), org_id, app_id)
                .expect("valid base url")
                .with_process_instance_id(identity.instance_id())
                .with_auth_token("eyes_test_wire")
                .build_with_transport(TransportType::Http);

        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!(target: "arena::eyes_token_wire_test", "eyes token wire test");
        });

        handle.shutdown().await.unwrap();
        server.abort();

        let captured = captured.lock().unwrap();
        assert!(!captured.is_empty(), "no events reached the fake server");
        // Count-only failure message: never print captured header values
        // (an ambient `EYES_TOKEN` could have leaked in).
        let mismatches = captured
            .iter()
            .filter(|header| header.as_str() != "Bearer eyes_test_wire")
            .count();
        assert!(
            mismatches == 0,
            "{mismatches} of {} captured Authorization headers were not the test bearer",
            captured.len()
        );
    }
}
