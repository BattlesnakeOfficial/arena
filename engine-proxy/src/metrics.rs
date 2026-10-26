//! Prometheus metrics, exposed at `/metrics` (token-gated) in the text
//! exposition format. Registered once into the default registry.

use std::sync::OnceLock;

use prometheus::{Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts};

pub struct Metrics {
    /// One per terminated proxied request, by outcome label (includes
    /// `client_cancelled` / `shutdown_aborted` from the drop guard).
    pub requests_total: IntCounterVec,
    /// The status the snake answered with (2xx/3xx/4xx/5xx/unknown), i.e.
    /// forwarded responses only.
    pub snake_status_total: IntCounterVec,
    /// Wall time from handler entry to response headers, in milliseconds.
    pub request_duration_ms: Histogram,
    /// TLS handshake duration on fresh connections.
    pub tls_handshake_ms: Histogram,
    /// Every TCP dial attempted by the connector. A dial count: it can exceed
    /// the number of requests labelled `fresh_connection` (background dials
    /// land idle in the pool; the load check's mock-side accept counters are
    /// authoritative).
    pub connections_total: IntCounter,
    /// Response-body stream aborts by reason (deadline / byte cap / body
    /// error).
    pub body_aborts_total: IntCounterVec,
    /// Unix timestamp of process start.
    pub process_start_time_seconds: IntGauge,
}

fn request_buckets() -> Vec<f64> {
    vec![
        1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
    ]
}

fn tls_buckets() -> Vec<f64> {
    vec![1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0]
}

fn build() -> Metrics {
    let requests_total = IntCounterVec::new(
        Opts::new(
            "engine_proxy_requests_total",
            "Proxied requests by terminal outcome.",
        ),
        &["outcome"],
    )
    .expect("valid requests_total metric");

    let snake_status_total = IntCounterVec::new(
        Opts::new(
            "engine_proxy_snake_status_total",
            "Forwarded snake response statuses by class.",
        ),
        &["class"],
    )
    .expect("valid snake_status_total metric");

    let request_duration_ms = Histogram::with_opts(
        HistogramOpts::new(
            "engine_proxy_request_duration_ms",
            "Wall time from handler entry to response headers, in milliseconds.",
        )
        .buckets(request_buckets()),
    )
    .expect("valid request_duration metric");

    let tls_handshake_ms = Histogram::with_opts(
        HistogramOpts::new(
            "engine_proxy_tls_handshake_ms",
            "TLS handshake duration on fresh https connections, in milliseconds.",
        )
        .buckets(tls_buckets()),
    )
    .expect("valid tls_handshake metric");

    let connections_total = IntCounter::new(
        "engine_proxy_connections_total",
        "TCP dials attempted by the connector.",
    )
    .expect("valid connections_total metric");

    let body_aborts_total = IntCounterVec::new(
        Opts::new(
            "engine_proxy_body_aborts_total",
            "Response-body stream aborts by reason.",
        ),
        &["reason"],
    )
    .expect("valid body_aborts_total metric");

    let process_start_time_seconds = IntGauge::new(
        "engine_proxy_process_start_time_seconds",
        "Unix timestamp of process start.",
    )
    .expect("valid process_start_time metric");

    let registry = prometheus::default_registry();
    for collector in [
        Box::new(requests_total.clone()) as Box<dyn prometheus::core::Collector>,
        Box::new(snake_status_total.clone()),
        Box::new(request_duration_ms.clone()),
        Box::new(tls_handshake_ms.clone()),
        Box::new(connections_total.clone()),
        Box::new(body_aborts_total.clone()),
        Box::new(process_start_time_seconds.clone()),
    ] {
        registry.register(collector).ok();
    }
    process_start_time_seconds.set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );

    Metrics {
        requests_total,
        snake_status_total,
        request_duration_ms,
        tls_handshake_ms,
        connections_total,
        body_aborts_total,
        process_start_time_seconds,
    }
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn get() -> &'static Metrics {
    METRICS.get_or_init(build)
}

/// Render the whole default registry in the prometheus text format.
pub fn render() -> String {
    prometheus::TextEncoder::new()
        .encode_to_string(&prometheus::gather())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_the_requests_counter() {
        get().requests_total.with_label_values(&["success"]).inc();
        let out = render();
        assert!(out.contains("engine_proxy_requests_total"), "{out}");
        assert!(out.contains("outcome=\"success\""), "{out}");
    }
}
