//! The proxy forward core: auth, budget, buffering, header shaping, error
//! mapping, and the one-log-line-per-request guarantee.
//!
//! Wire contract (matches Go `src/proxy/server.go`, pinned by the contract
//! suite in arena): see `docs/engine-proxy.md`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::StreamExt as _;
use hyper::body::{Frame, Incoming};
use tokio_util::sync::CancellationToken;

use crate::app::ProxyState;
use crate::connector::{ConnTimings, ConnectError, OutboundBody};
use crate::logging;
use crate::metrics;
use crate::timing::{Timings, format_go_timing};

pub const LATENCY_HEADER: &str = "x-battlesnake-latency-ms";
pub const TIMING_HEADER: &str = "x-battlesnake-timing-ms";
pub const ERROR_HEADER: &str = "x-battlesnake-server-error";
pub const REQUEST_URI_HEADER: &str = "x-request-uri";
pub const TIMEOUT_MS_HEADER: &str = "x-battlesnake-timeout-ms";
pub const PROXY_AUTH_ALT_HEADER: &str = "x-proxy-authorization";
pub const PROXY_AUTH_HEADER: &str = "proxy-authorization";

/// Go's `DEFAULT_TIMEOUT` when `X-Battlesnake-Timeout-Ms` is absent.
pub const DEFAULT_TIMEOUT_MS: i64 = 1000;

// Header names erased before forwarding (request side): control headers,
// hop-by-hop headers, plus `Host`/framing which hyper re-derives from the
// target URI and the buffered body.
const REQUEST_STRIP_HEADERS: &[&str] = &[
    PROXY_AUTH_ALT_HEADER,
    PROXY_AUTH_HEADER,
    REQUEST_URI_HEADER,
    TIMEOUT_MS_HEADER,
    "x-forwarded-for", // Go's director nulls it explicitly
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

// Response side: hop-by-hop only; the snake's end-to-end headers pass
// through unchanged, and Go's `proxyModifyResponse` overwrites the latency
// and timing headers the same way we do.
const RESPONSE_STRIP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Error codes and messages, byte-for-byte from Go `snake_errors.go`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnakeErrorKind {
    Timeout,
    Dns,
    Connect,
    Tls,
    ReadBody,
    Unknown,
}

impl SnakeErrorKind {
    pub fn code(self) -> u8 {
        match self {
            Self::Timeout => 1,
            Self::Dns => 2,
            Self::Connect => 3,
            Self::Tls => 4,
            Self::ReadBody => 5,
            Self::Unknown => 0,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Timeout => "A response was not received before the timeout",
            Self::Dns => "Failed to resolve Battlesnake hostname",
            Self::Connect => "Failed to connect to Battlesnake host",
            Self::Tls => "Failed to set up a TLS connection",
            Self::ReadBody => "Failed to read response body",
            Self::Unknown => "An unknown error occurred",
        }
    }

    /// The exact `X-Battlesnake-Server-Error` value Go sends.
    pub fn display(self) -> String {
        format!("{}:{}", self.code(), self.message())
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::ReadBody => "read_body",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    RequestBody,
    Headers,
    Body,
}

impl TimeoutPhase {
    fn label(self) -> &'static str {
        match self {
            Self::RequestBody => "request_body",
            Self::Headers => "headers",
            Self::Body => "body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    BodyCap,
    BodyError,
}

impl AbortReason {
    fn label(self) -> &'static str {
        match self {
            Self::BodyCap => "body_cap",
            Self::BodyError => "body_error",
        }
    }
}

/// Terminal outcome of one proxied request. Everything the drop guard needs
/// to emit exactly one log line and metric.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Success { status: u16 },
    Timeout(TimeoutPhase),
    Transport(SnakeErrorKind),
    Auth,
    Malformed,
    RequestBodyTooLarge,
    Aborted(AbortReason),
    ClientCancelled,
    ShutdownAborted,
}

impl Outcome {
    pub fn label(&self) -> String {
        match self {
            Self::Success { .. } => "success".to_string(),
            Self::Timeout(p) => format!("timeout:{}", p.label()),
            Self::Transport(k) => format!("transport:{}", k.label()),
            Self::Auth => "auth".to_string(),
            Self::Malformed => "malformed".to_string(),
            Self::RequestBodyTooLarge => "request_body_too_large".to_string(),
            Self::Aborted(r) => format!("aborted:{}", r.label()),
            Self::ClientCancelled => "client_cancelled".to_string(),
            Self::ShutdownAborted => "shutdown_aborted".to_string(),
        }
    }

    /// Response status this outcome produces, when it produces one on this
    /// path (body-phase outcomes kill the connection instead).
    fn status_code(&self) -> Option<u16> {
        match self {
            Self::Success { status } => Some(*status),
            Self::Timeout(_) => Some(504),
            Self::Transport(k) => match k {
                SnakeErrorKind::Timeout => Some(504),
                _ => Some(502),
            },
            Self::Auth => Some(407),
            Self::Malformed => Some(400),
            Self::RequestBodyTooLarge => Some(413),
            _ => None,
        }
    }
}

/// The per-request structured log record (one INFO line, target
/// `engine_proxy`). The guard guarantees emission exactly once.
#[derive(Debug, Clone)]
pub struct RequestLog {
    host: String,
    scheme: String,
    method: String,
    outcome: Option<Outcome>,
    status: Option<u16>,
    timeout_ms: u64,
    /// Redacted failure detail (transport error strings etc.), folded into
    /// the single request line so failed requests stay one-line events
    /// (PR #277 round 1) — the standalone WARNs were demoted to DEBUG.
    error: Option<String>,
    dns_ms: f32,
    connect_ms: f32,
    tls_ms: f32,
    client_headers_ms: f32,
    client_body_ms: f32,
    first_byte_ms: f32,
    total_ms: f32,
    fresh_connection: bool,
    process_age_secs: u64,
    start: Instant,
}

impl RequestLog {
    fn new(method: &Method, start: Instant, boot: Instant) -> Self {
        Self {
            host: "-".to_string(),
            scheme: "-".to_string(),
            method: method.to_string(),
            outcome: None,
            status: None,
            timeout_ms: 0,
            error: None,
            dns_ms: 0.0,
            connect_ms: 0.0,
            tls_ms: 0.0,
            client_headers_ms: 0.0,
            client_body_ms: 0.0,
            first_byte_ms: 0.0,
            total_ms: 0.0,
            fresh_connection: false,
            process_age_secs: boot.elapsed().as_secs(),
            start,
        }
    }

    fn set_target(&mut self, host: &str, scheme: &str) {
        self.host = logging::redact_host_or_url(host);
        self.scheme = scheme.to_string();
    }

    fn emit(&self) {
        let outcome = self.outcome.clone().unwrap_or(Outcome::Malformed);
        let error = self.error.as_deref().unwrap_or("");
        tracing::info!(
            target: "engine_proxy",
            host = %self.host,
            scheme = %self.scheme,
            method = %self.method,
            outcome = %outcome.label(),
            status = self.status.unwrap_or(0),
            timeout_ms = self.timeout_ms,
            error = %error,
            dns_ms = self.dns_ms,
            connect_ms = self.connect_ms,
            tls_ms = self.tls_ms,
            client_headers_ms = self.client_headers_ms,
            client_body_ms = self.client_body_ms,
            first_byte_ms = self.first_byte_ms,
            total_ms = self.total_ms,
            fresh_connection = self.fresh_connection,
            process_age_secs = self.process_age_secs,
            "proxied request"
        );
        metrics::get()
            .requests_total
            .with_label_values(&[&outcome.label()])
            .inc();
        metrics::get()
            .request_duration_ms
            .observe(f64::from(self.total_ms));
    }
}

/// Guarantees exactly one log line per request, including the cases no
/// normal code path reaches: the client disconnecting before headers,
/// mid-body or mid-upload, and shutdown aborting stragglers.
pub struct RequestLogGuard {
    log: Option<RequestLog>,
    shutdown: CancellationToken,
}

impl RequestLogGuard {
    fn new(method: &Method, start: Instant, boot: Instant, shutdown: CancellationToken) -> Self {
        Self {
            log: Some(RequestLog::new(method, start, boot)),
            shutdown,
        }
    }

    /// Record the (redacted) target host and scheme. No-op once the log has
    /// been emitted.
    fn set_target(&mut self, host: &str, scheme: &str) {
        if let Some(log) = self.log.as_mut() {
            log.set_target(host, scheme);
        }
    }

    /// Record the clamped budget. No-op once the log has been emitted.
    fn set_timeout_ms(&mut self, ms: u64) {
        if let Some(log) = self.log.as_mut() {
            log.timeout_ms = ms;
        }
    }

    /// Record the dispatch-phase measurements (headers received). No-op
    /// once the log has been emitted.
    fn record_dispatch(&mut self, timings: DispatchTimings) {
        if let Some(log) = self.log.as_mut() {
            log.dns_ms = timings.dns_ms;
            log.connect_ms = timings.connect_ms;
            log.tls_ms = timings.tls_ms;
            log.first_byte_ms = timings.first_byte_ms;
            log.total_ms = timings.first_byte_ms;
            log.fresh_connection = timings.fresh_connection;
            log.client_headers_ms = timings.client_headers_ms;
            log.client_body_ms = timings.client_body_ms;
        }
    }

    /// Record redacted failure detail for the request line. No-op once the
    /// log has been emitted.
    fn record_error(&mut self, error: String) {
        if let Some(log) = self.log.as_mut()
            && log.error.is_none()
        {
            log.error = Some(error);
        }
    }

    /// Record the terminal outcome and emit. Idempotent.
    fn finish(&mut self, outcome: Outcome) {
        if let Some(mut log) = self.log.take() {
            log.status = log.status.or(outcome.status_code());
            log.outcome = Some(outcome);
            log.total_ms = if log.total_ms > 0.0 {
                log.total_ms
            } else {
                log.start.elapsed().as_secs_f32() * 1000.0
            };
            log.emit();
        }
    }
}

impl Drop for RequestLogGuard {
    fn drop(&mut self) {
        if self.log.is_some() {
            // The Supervisor's token: cancelled once at signal time, never
            // reset, so a client that cancels during the drain window is
            // labelled `shutdown_aborted`; the reverse mislabel is impossible.
            let fallback = if self.shutdown.is_cancelled() {
                Outcome::ShutdownAborted
            } else {
                Outcome::ClientCancelled
            };
            self.finish(fallback);
        }
    }
}

enum BufferError {
    TooLarge,
    Timeout,
    Read,
}

/// Dispatch-phase measurements handed to [`RequestLogGuard::record_dispatch`]
/// once response headers arrive.
struct DispatchTimings {
    dns_ms: f32,
    connect_ms: f32,
    tls_ms: f32,
    client_headers_ms: f32,
    client_body_ms: f32,
    first_byte_ms: f32,
    fresh_connection: bool,
}

pub(crate) async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    req: Request<Body>,
) -> Response<Body> {
    let start = Instant::now();
    let mut guard = RequestLogGuard::new(req.method(), start, state.boot, state.shutdown.clone());

    // -- auth, before any URI/timeout validation (Go's order) --
    let auth_header = req
        .headers()
        .get(PROXY_AUTH_ALT_HEADER)
        .or_else(|| req.headers().get(PROXY_AUTH_HEADER));
    if !auth_ok(auth_header, &state.cfg.auth_token) {
        // Go logs the header value (the secret!) here; we do not. DEBUG so
        // the emitted request line stays the only per-request stdout record
        // (the baked RUST_LOG filter drops DEBUG in production).
        tracing::debug!(target: "engine_proxy", "bad or missing proxy auth");
        guard.finish(Outcome::Auth);
        return StatusCode::PROXY_AUTHENTICATION_REQUIRED.into_response();
    }

    // -- target URI --
    let target_str = req
        .headers()
        .get(REQUEST_URI_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if target_str.is_empty() {
        guard.finish(Outcome::Malformed);
        return StatusCode::BAD_REQUEST.into_response();
    }
    let target: hyper::Uri = match target_str.parse() {
        Ok(uri) => uri,
        Err(_) => {
            guard.finish(Outcome::Malformed);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let scheme = target.scheme_str().unwrap_or_default().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        guard.finish(Outcome::Malformed);
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Log `host[:port]` only — `authority().as_str()` would carry userinfo
    // (`user:pass@host`), which must never reach a log line (PR #277 round 1
    // pin: snake URLs are user-supplied). `Uri::host()` keeps IPv6 brackets
    // and never carries credentials; the port is how tests and operators
    // tell mock snakes apart.
    let host_for_log = {
        let host = target.host().unwrap_or_default();
        match target.port_u16() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        }
    };
    guard.set_target(&host_for_log, &scheme);

    // -- timeout budget --
    let timeout_raw = req
        .headers()
        .get(TIMEOUT_MS_HEADER)
        .and_then(|v| v.to_str().ok());
    let timeout_ms: i64 = match timeout_raw {
        None | Some("") => DEFAULT_TIMEOUT_MS,
        Some(s) => match s.parse::<i64>() {
            Ok(v) => v,
            Err(_) => {
                guard.finish(Outcome::Malformed);
                return StatusCode::BAD_REQUEST.into_response();
            }
        },
    };
    let timeout_ms = timeout_ms.clamp(0, state.cfg.max_timeout_ms);
    guard.set_timeout_ms(timeout_ms.unsigned_abs());
    let budget = Duration::from_millis(timeout_ms.unsigned_abs());
    // The clamp makes overflow impossible; `checked_add` regardless.
    let deadline = tokio::time::Instant::now()
        .checked_add(budget)
        .unwrap_or_else(tokio::time::Instant::now);

    // -- phase 0: buffer the inbound request body under the same deadline --
    let (in_parts, in_body) = req.into_parts();
    let body_bytes = match buffer_request_body(in_body, deadline, state.cfg.body_cap_bytes).await {
        Ok(bytes) => bytes,
        Err(BufferError::TooLarge) => {
            guard.finish(Outcome::RequestBodyTooLarge);
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(BufferError::Timeout) => {
            guard.finish(Outcome::Timeout(TimeoutPhase::RequestBody));
            return error_response(StatusCode::GATEWAY_TIMEOUT, SnakeErrorKind::Timeout, &start);
        }
        Err(BufferError::Read) => {
            guard.finish(Outcome::Malformed);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // -- build the outbound request --
    let (out_body, body_tracker) = OutboundBody::new(body_bytes, start);
    let mut builder = hyper::Request::builder()
        .method(in_parts.method.clone())
        .uri(target.clone());
    for (name, value) in in_parts.headers.iter() {
        if REQUEST_STRIP_HEADERS.contains(&name.as_str())
            || connection_named(name.as_str(), &in_parts.headers)
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    let out_req = match builder.body(out_body) {
        Ok(r) => r,
        Err(_) => {
            guard.finish(Outcome::Malformed);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // -- phase a: send, await headers under the deadline --
    let response = match tokio::time::timeout_at(deadline, state.client.request(out_req)).await {
        Err(_elapsed) => {
            guard.finish(Outcome::Timeout(TimeoutPhase::Headers));
            return error_response(StatusCode::GATEWAY_TIMEOUT, SnakeErrorKind::Timeout, &start);
        }
        Ok(Err(e)) => {
            let kind = classify_transport_error(&e);
            let redacted = logging::redact_error_string(&e.to_string());
            // The detail rides the request line (one line per request, even
            // on failure); the standalone WARN was demoted to DEBUG so a
            // snake-outage burst does not double per-request log volume.
            tracing::debug!(
                target: "engine_proxy",
                host = %logging::redact_host_or_url(&host_for_log),
                kind = kind.label(),
                error = %redacted,
                "snake transport failure"
            );
            guard.record_error(redacted);
            guard.finish(Outcome::Transport(kind));
            let status = match kind {
                SnakeErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            return error_response(status, kind, &start);
        }
        Ok(Ok(res)) => res,
    };

    // -- headers received: timings, freshness, pre-checks --
    let (mut parts, body) = response.into_parts();
    let status = parts.status;
    let conn_timings = parts.extensions.get::<ConnTimings>().cloned();
    let fresh = conn_timings.as_ref().is_some_and(|t| t.mark_first_use());
    let (dns_ms, connect_ms, tls_ms) = match (&conn_timings, fresh) {
        (Some(t), true) => (t.dns_ms(), t.connect_ms(), t.tls_ms()),
        _ => (0.0, 0.0, 0.0),
    };
    let first_byte_ms = elapsed_ms(start);

    if declared_content_length(&parts.headers)
        .is_some_and(|cl| cl > state.cfg.body_cap_bytes as u64)
    {
        // Deliberate improvement over Go: arena gets a classifiable
        // 502+5: instead of a 200 that truncates mid-stream.
        guard.finish(Outcome::Transport(SnakeErrorKind::ReadBody));
        return error_response(StatusCode::BAD_GATEWAY, SnakeErrorKind::ReadBody, &start);
    }

    {
        let client_headers_ms = body_tracker.client_headers_ms().unwrap_or(0.0);
        let client_body_ms = body_tracker.client_body_ms().unwrap_or(0.0);
        guard.record_dispatch(DispatchTimings {
            dns_ms,
            connect_ms,
            tls_ms,
            client_headers_ms,
            client_body_ms,
            first_byte_ms,
            fresh_connection: fresh,
        });
    }
    metrics::get()
        .snake_status_total
        .with_label_values(&[status_class(status).as_str()])
        .inc();

    // -- build the passthrough response --
    let mut out_headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if RESPONSE_STRIP_HEADERS.contains(&name.as_str())
            || connection_named(name.as_str(), &parts.headers)
        {
            continue;
        }
        out_headers.append(name, value.clone());
    }
    out_headers.insert(
        HeaderName::from_static(LATENCY_HEADER),
        HeaderValue::from_str(&start.elapsed().as_millis().to_string())
            .unwrap_or(HeaderValue::from_static("0")),
    );
    let timings = Timings {
        dns_ms,
        connect_ms,
        tls_ms,
        client_headers_ms: body_tracker.client_headers_ms().unwrap_or(0.0),
        client_body_ms: body_tracker.client_body_ms().unwrap_or(0.0),
        first_byte_ms,
        total_ms: first_byte_ms,
    };
    if let Ok(hv) = HeaderValue::from_str(&format_go_timing(&timings)) {
        out_headers.insert(HeaderName::from_static(TIMING_HEADER), hv);
    }
    parts.headers = out_headers;

    // A body that is already complete (e.g. `Content-Length: 0`) is never
    // polled by hyper, so `DeadlineBody` would never observe end-of-stream
    // and the drop guard would mislabel the request `client_cancelled`.
    // Finish the guard eagerly and return an empty body.
    if <Incoming as http_body::Body>::is_end_stream(&body) {
        guard.finish(Outcome::Success {
            status: status.as_u16(),
        });
        parts.status = status;
        return Response::from_parts(parts, Body::new(http_body_util::Empty::<Bytes>::new()));
    }
    let deadline_body = DeadlineBody::new(body, deadline, state.cfg.body_cap_bytes, guard, status);
    parts.status = status;
    Response::from_parts(parts, Body::new(deadline_body))
}

fn auth_ok(header: Option<&HeaderValue>, token: &str) -> bool {
    let Some(value) = header else { return false };
    let Ok(s) = value.to_str() else {
        return false;
    };
    // Go: strings.SplitN(header, " ", 2); scheme must equal "token"
    // case-sensitively (parity beats RFC 7235 here).
    let Some((scheme, secret)) = s.split_once(' ') else {
        return false;
    };
    scheme == "token" && subtle::ConstantTimeEq::ct_eq(secret.as_bytes(), token.as_bytes()).into()
}

fn connection_named(name: &str, headers: &HeaderMap) -> bool {
    if let Some(connection) = headers.get("connection").and_then(|v| v.to_str().ok()) {
        let name = name.to_ascii_lowercase();
        return connection
            .split(',')
            .map(|tok| tok.trim().to_ascii_lowercase())
            .any(|tok| tok == name);
    }
    false
}

fn declared_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

fn status_class(status: StatusCode) -> String {
    let code = status.as_u16();
    match code {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "unknown",
    }
    .to_string()
}

/// Walk the hyper-util error chain for a `ConnectError` to pick the
/// `X-Battlesnake-Server-Error` code, mirroring Go's
/// `HTTPErrorToSnakeResponseError`.
fn classify_transport_error(e: &(dyn std::error::Error + 'static)) -> SnakeErrorKind {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = src {
        if let Some(ce) = err.downcast_ref::<ConnectError>() {
            return match ce {
                ConnectError::Dns(_) => SnakeErrorKind::Dns,
                ConnectError::Connect(_) => SnakeErrorKind::Connect,
                ConnectError::Tls(_) => SnakeErrorKind::Tls,
            };
        }
        src = err.source();
    }
    SnakeErrorKind::Unknown
}

fn error_response(status: StatusCode, kind: SnakeErrorKind, start: &Instant) -> Response<Body> {
    let mut builder = Response::builder()
        .status(status)
        .header(ERROR_HEADER, kind.display());
    builder = builder.header(LATENCY_HEADER, start.elapsed().as_millis().to_string());
    builder
        .body(Body::empty())
        .unwrap_or_else(|_| (status, format!("{ERROR_HEADER}: {}", kind.display())).into_response())
}

fn elapsed_ms(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
}

async fn buffer_request_body(
    body: Body,
    deadline: tokio::time::Instant,
    cap: usize,
) -> Result<Bytes, BufferError> {
    let buffered = tokio::time::timeout_at(deadline, async {
        let mut stream = body.into_data_stream();
        let mut buf = bytes::BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| BufferError::Read)?;
            if buf.len() + chunk.len() > cap {
                return Err(BufferError::TooLarge);
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    })
    .await;
    match buffered {
        Ok(inner) => inner,
        Err(_) => Err(BufferError::Timeout),
    }
}

/// Response body wrapper enforcing the single request deadline and byte cap
/// after headers have been forwarded. Owns the request log guard; finishing
/// cleanly on end-of-stream, timing out or aborting the connection otherwise.
///
/// Phase-split consequence: once a 200 status line has been forwarded, no
/// 504 can follow — a body-phase breach kills the connection, which arena
/// sees as a transport fault (`ProxyCall::Fault { kind: "transport" }` →
/// fallback, `timed_out: false`), not a 504.
pub struct DeadlineBody {
    inner: Incoming,
    sleep: Pin<Box<tokio::time::Sleep>>,
    cap: usize,
    sent: usize,
    guard: RequestLogGuard,
    status: u16,
    closed: bool,
}

impl DeadlineBody {
    fn new(
        inner: Incoming,
        deadline: tokio::time::Instant,
        cap: usize,
        guard: RequestLogGuard,
        status: StatusCode,
    ) -> Self {
        Self {
            inner,
            sleep: Box::pin(tokio::time::sleep_until(deadline)),
            cap,
            sent: 0,
            guard,
            status: status.as_u16(),
            closed: false,
        }
    }

    fn abort(&mut self, reason: AbortReason) -> Box<dyn std::error::Error + Send + Sync> {
        metrics::get()
            .body_aborts_total
            .with_label_values(&[reason.label()])
            .inc();
        self.guard.finish(Outcome::Aborted(reason));
        io_error(reason)
    }
}

fn io_error(reason: AbortReason) -> Box<dyn std::error::Error + Send + Sync> {
    let kind = match reason {
        AbortReason::BodyCap => std::io::ErrorKind::InvalidData,
        AbortReason::BodyError => std::io::ErrorKind::ConnectionAborted,
    };
    Box::new(std::io::Error::new(
        kind,
        match reason {
            AbortReason::BodyCap => "response body exceeded proxy cap",
            AbortReason::BodyError => "error reading snake response body",
        },
    ))
}

impl http_body::Body for DeadlineBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.closed {
            return Poll::Ready(None);
        }
        // Inner first: ready data always wins over the deadline.
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.sent += data.len();
                    if this.sent > this.cap {
                        let err = this.abort(AbortReason::BodyCap);
                        this.closed = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                // Content-Length-delimited bodies never yield a trailing
                // `Ready(None)` poll from hyper once every expected byte was
                // delivered, so `is_end_stream` (or trailers, or None) is the
                // completion signal.
                if frame.is_trailers() || this.inner.is_end_stream() {
                    this.closed = true;
                    let status = this.status;
                    this.guard.finish(Outcome::Success { status });
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                this.closed = true;
                let status = this.status;
                this.guard.finish(Outcome::Success { status });
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                tracing::debug!(
                    target: "engine_proxy",
                    error = %logging::redact_error_string(&e.to_string()),
                    "snake response body read error"
                );
                let err = this.abort(AbortReason::BodyError);
                this.closed = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Pending => {
                // Stalled: enforce the deadline while waiting.
                if this.sleep.as_mut().poll(cx).is_ready() {
                    metrics::get()
                        .body_aborts_total
                        .with_label_values(&["deadline"])
                        .inc();
                    this.guard.finish(Outcome::Timeout(TimeoutPhase::Body));
                    this.closed = true;
                    return Poll::Ready(Some(Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "response body deadline exceeded",
                    )))));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        // Never claim early completion as a hint: hyper may skip polling on a
        // true hint while frames are still buffered. Completion (and the log
        // emission) is handled inside `poll_frame`.
        false
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

// A tiny alias so `futures::StreamExt` resolves without naming the crate in
// public signatures.

#[cfg(test)]
mod tests {
    use super::*;

    /// The response-body budget is absolute: a buffered frame must not be
    /// forwarded when the consumer first polls it after the deadline.
    #[tokio::test]
    async fn buffered_response_frame_after_deadline_is_rejected() {
        use http_body_util::BodyExt as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap();
        let driver = tokio::spawn(connection);
        let request = hyper::Request::builder()
            .uri("/")
            .body(http_body_util::Empty::<Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let now = Instant::now();
        let guard = RequestLogGuard::new(&Method::GET, now, now, CancellationToken::new());
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        let mut body =
            DeadlineBody::new(response.into_body(), deadline, 1024, guard, StatusCode::OK);
        // The server has already sent its byte. Let both the byte and the
        // deadline become ready before the first downstream body poll.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let frame = body.frame().await.expect("buffered response frame");
        assert!(
            frame.is_err(),
            "body frame was forwarded after the hard deadline"
        );

        server.abort();
        driver.abort();
    }

    #[test]
    fn snake_error_display_matches_go() {
        assert_eq!(
            SnakeErrorKind::Timeout.display(),
            "1:A response was not received before the timeout"
        );
        assert_eq!(
            SnakeErrorKind::Dns.display(),
            "2:Failed to resolve Battlesnake hostname"
        );
        assert_eq!(
            SnakeErrorKind::Connect.display(),
            "3:Failed to connect to Battlesnake host"
        );
        assert_eq!(
            SnakeErrorKind::Tls.display(),
            "4:Failed to set up a TLS connection"
        );
        assert_eq!(
            SnakeErrorKind::ReadBody.display(),
            "5:Failed to read response body"
        );
        assert_eq!(
            SnakeErrorKind::Unknown.display(),
            "0:An unknown error occurred"
        );
    }

    #[test]
    fn auth_parsing_matches_go_splitn() {
        let token = "sekret";
        let hv = |s: &str| Some(HeaderValue::from_str(s).unwrap());
        assert!(auth_ok(hv("token sekret").as_ref(), token));
        assert!(!auth_ok(hv("token wrong").as_ref(), token));
        assert!(
            !auth_ok(hv("Token sekret").as_ref(), token),
            "scheme is case-sensitive"
        );
        assert!(!auth_ok(hv("sekret").as_ref(), token), "no scheme => fail");
        assert!(!auth_ok(hv("").as_ref(), token));
        assert!(!auth_ok(None, token));
        // Go SplitN(2): "token a b" compares against "a b"
        assert!(!auth_ok(hv("token a b").as_ref(), token));
        let spaced = "two words";
        assert!(auth_ok(hv("token two words").as_ref(), spaced));
    }

    #[test]
    fn outcome_labels() {
        assert_eq!(Outcome::Success { status: 200 }.label(), "success");
        assert_eq!(Outcome::Timeout(TimeoutPhase::Body).label(), "timeout:body");
        assert_eq!(
            Outcome::Transport(SnakeErrorKind::Dns).label(),
            "transport:dns"
        );
        assert_eq!(Outcome::ClientCancelled.label(), "client_cancelled");
        assert_eq!(Outcome::ShutdownAborted.label(), "shutdown_aborted");
    }

    #[derive(thiserror::Error, Debug)]
    #[error("wrapped")]
    struct Wrapped(#[source] ConnectError);

    #[test]
    fn classify_walks_source_chain() {
        // hyper-util surfaces connector errors as `Error.source = ConnectError`
        // (its `e!` macro boxes the source directly), so a two-level chain is
        // the real shape.
        let wrapped = Wrapped(ConnectError::Dns(std::io::Error::other("no such host")));
        assert_eq!(classify_transport_error(&wrapped), SnakeErrorKind::Dns);
        let direct = ConnectError::Tls(std::io::Error::other("bad cert"));
        assert_eq!(classify_transport_error(&direct), SnakeErrorKind::Tls);
        let plain = std::io::Error::other("connection reset");
        assert_eq!(classify_transport_error(&plain), SnakeErrorKind::Unknown);
    }
}
