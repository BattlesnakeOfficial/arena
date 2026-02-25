use std::fmt;

use cja::setup::EyesShutdownHandle;
use tracing::{Event, Subscriber};
use tracing_subscriber::{
    fmt::{self as fmt_layer, FmtContext, FormatEvent, FormatFields},
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

/// Sets up GCP-compatible structured JSON logging.
///
/// Returns `Ok(None)` for the Eyes shutdown handle (not used with GCP logging),
/// maintaining type compatibility with `cja::setup::setup_tracing`.
pub fn setup_gcp_tracing() -> color_eyre::Result<Option<EyesShutdownHandle>> {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    let env_filter = EnvFilter::builder().parse(&rust_log).map_err(|e| {
        color_eyre::eyre::eyre!("Couldn't create env filter from {}: {}", rust_log, e)
    })?;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::Layer::default()
                .event_format(GcpJsonFormatter)
                .with_ansi(false),
        )
        .try_init()?;

    Ok(None)
}

/// Parse the `X-Cloud-Trace-Context` header value and return a GCP trace path.
///
/// Header format: `TRACE_ID/SPAN_ID;o=TRACE_TRUE`
/// Returns: `projects/{project_id}/traces/{trace_id}`
pub fn extract_trace_context(header_value: &str) -> Option<String> {
    let project_id = std::env::var("GCP_PROJECT_ID").ok()?;

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

    #[test]
    fn test_extract_trace_context_valid() {
        // SAFETY: Test-only env var manipulation, tests run serially with serial_test if needed
        unsafe { std::env::set_var("GCP_PROJECT_ID", "test-project") };
        let result = extract_trace_context("105445aa7843bc8bf206b12000100000/1;o=1");
        assert_eq!(
            result,
            Some("projects/test-project/traces/105445aa7843bc8bf206b12000100000".to_string())
        );
        unsafe { std::env::remove_var("GCP_PROJECT_ID") };
    }

    #[test]
    fn test_extract_trace_context_no_options() {
        // SAFETY: Test-only env var manipulation
        unsafe { std::env::set_var("GCP_PROJECT_ID", "test-project") };
        let result = extract_trace_context("105445aa7843bc8bf206b12000100000/1");
        assert_eq!(
            result,
            Some("projects/test-project/traces/105445aa7843bc8bf206b12000100000".to_string())
        );
        unsafe { std::env::remove_var("GCP_PROJECT_ID") };
    }

    #[test]
    fn test_extract_trace_context_empty() {
        // SAFETY: Test-only env var manipulation
        unsafe { std::env::set_var("GCP_PROJECT_ID", "test-project") };
        let result = extract_trace_context("");
        assert_eq!(result, None);
        unsafe { std::env::remove_var("GCP_PROJECT_ID") };
    }

    #[test]
    fn test_extract_trace_context_no_project_id() {
        // SAFETY: Test-only env var manipulation
        unsafe { std::env::remove_var("GCP_PROJECT_ID") };
        let result = extract_trace_context("105445aa7843bc8bf206b12000100000/1;o=1");
        assert_eq!(result, None);
    }
}
