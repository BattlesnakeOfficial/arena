//! The axum application: routes, state, and the health/version/metrics
//! endpoints. The data path is [`crate::proxy::proxy_handler`] mounted as the
//! fallback so it sees every method and path (arena posts to the proxy root).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use color_eyre::eyre::WrapErr as _;
use http_body_util::BodyExt as _;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use tokio_util::sync::CancellationToken;
use tower_http::catch_panic::ResponseForPanic;

use crate::config::ProxyConfig;
use crate::connector::{OutboundBody, TimedConnector};
use crate::metrics;
use crate::proxy;

/// Shared proxy state. No database anywhere: cja's server module never needs
/// one (we do not implement cja's `AppState`, which requires a `PgPool` —
/// `run_server_until`/`serve_until` take a plain `axum::Router`).
pub struct ProxyState {
    pub(crate) cfg: Arc<ProxyConfig>,
    pub(crate) client: Client<TimedConnector, OutboundBody>,
    pub(crate) boot: Instant,
    pub(crate) shutdown: CancellationToken,
}

impl ProxyState {
    /// Read config from the environment and build the client pool. Missing
    /// `BATTLESNAKE_PROXY_AUTH_TOKEN` returns `Err` so `main` exits non-zero,
    /// exactly like Go.
    pub fn from_env(shutdown: CancellationToken) -> color_eyre::Result<Self> {
        let cfg = ProxyConfig::from_env()?;
        Self::new(cfg, shutdown)
    }

    /// A bad `ENGINE_PROXY_EXTRA_CA` (unreadable file, non-PEM contents)
    /// surfaces as `Err` at boot — never a panic (PR #277 round 1).
    pub fn new(cfg: ProxyConfig, shutdown: CancellationToken) -> color_eyre::Result<Self> {
        let connector =
            TimedConnector::new(&cfg).wrap_err("building the engine-proxy TLS configuration")?;
        // Pool sizing (DD 3): 8 idle per host (Go's MaxIdleConnsPerHost: 2 was
        // the 24-37% fresh-handshake bug from DEV-1499 item 2), 90s idle
        // timeout matching Go's IdleConnTimeout, pool_timer REQUIRED —
        // without it idle sockets are never reaped (FD/memory leak).
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Some(cfg.pool_idle_timeout))
            .pool_max_idle_per_host(cfg.pool_idle_per_host)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(connector);
        Ok(Self {
            cfg: Arc::new(cfg),
            client,
            boot: Instant::now(),
            shutdown,
        })
    }
}

pub fn router(state: ProxyState) -> axum::Router {
    let state = Arc::new(state);
    axum::Router::new()
        .route("/healthz/alive", axum::routing::get(healthz_alive))
        .route("/healthz/ready", axum::routing::get(healthz_ready))
        .route("/version", axum::routing::get(version))
        .route("/metrics", axum::routing::get(metrics_endpoint))
        .fallback(proxy::proxy_handler)
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            PanicResponse,
        ))
        .with_state(state)
}

async fn healthz_ready() -> impl IntoResponse {
    (StatusCode::OK, "ready")
}

async fn version(State(state): State<Arc<ProxyState>>) -> impl IntoResponse {
    (StatusCode::OK, state.cfg.app_version.clone())
}

async fn healthz_alive(State(state): State<Arc<ProxyState>>) -> Response {
    if state.cfg.health_check_urls.is_empty() {
        return (StatusCode::OK, "alive").into_response();
    }
    // Mirror Go's HealthHandler: per-check timeout 2s, overall window 3s,
    // first success wins. Unlike Go (whose select `break` is a no-op bug), we
    // answer as soon as every check has actually failed.
    const CONNECTIVITY_URL_TIMEOUT: Duration = Duration::from_secs(2);
    const HEALTH_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(3);

    let checks = state.cfg.health_check_urls.iter().map(|url| async {
        match url.parse::<hyper::Uri>() {
            Ok(uri) => {
                let (body, _) = OutboundBody::empty(Instant::now());
                match hyper::Request::builder().method("GET").uri(uri).body(body) {
                    Ok(req) => {
                        match tokio::time::timeout(CONNECTIVITY_URL_TIMEOUT, async {
                            let res = state.client.request(req).await?;
                            // drain the body
                            let _ = res.into_body().collect().await;
                            Ok::<(), hyper_util::client::legacy::Error>(())
                        })
                        .await
                        {
                            Ok(Ok(())) => true,
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    target: "engine_proxy",
                                    url = %crate::logging::redact_host_or_url(url),
                                    error = %crate::logging::redact_error_string(&e.to_string()),
                                    "health connectivity check failed"
                                );
                                false
                            }
                            Err(_) => false,
                        }
                    }
                    // A URI that parses but cannot build a GET counts as a
                    // failed check, not a panic (PR #277 round 1).
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    });
    let all: Vec<bool> = match tokio::time::timeout(
        HEALTH_ENDPOINT_TIMEOUT,
        futures::future::join_all(checks),
    )
    .await
    {
        Ok(results) => results,
        Err(_) => vec![false],
    };
    if all.iter().any(|&ok| ok) {
        (StatusCode::OK, "alive").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
    }
}

async fn metrics_endpoint(
    State(state): State<Arc<ProxyState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Same `Authorization: token <secret>` scheme as Go's TokenAuthHandler;
    // deliberately a different header/status than the data path.
    let header = headers.get(axum::http::header::AUTHORIZATION);
    let ok = match header.and_then(|v| v.to_str().ok()) {
        Some(s) => match s.split_once(' ') {
            Some((scheme, secret)) => {
                scheme == "token"
                    && subtle::ConstantTimeEq::ct_eq(
                        secret.as_bytes(),
                        state.cfg.auth_token.as_bytes(),
                    )
                    .into()
            }
            None => false,
        },
        None => false,
    };
    if !ok {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", "token")
            .body(Body::empty())
            .unwrap();
    }
    (StatusCode::OK, metrics::render()).into_response()
}

/// Panics inside the proxy handler become 500 + `X-Battlesnake-Server-Error:
/// 0:An unknown error occurred` instead of a dropped connection (Go's
/// net/http recovers per-connection and just closes it).
///
/// The layer logs the panic payload at ERROR, target `tower_http::catch_panic`
/// — that line is the panic evidence; `RUST_LOG` keeps it alive.
///
/// Deviation from the plan's DD 12: the custom hook receives no request
/// context, so `X-Battlesnake-Latency-Ms` cannot be measured here and is
/// omitted — arena classifies the response as a proxy fault (fallback),
/// the same outcome class as Go's connection close.
#[derive(Clone, Copy)]
struct PanicResponse;

impl ResponseForPanic for PanicResponse {
    type ResponseBody = Body;

    fn response_for_panic(&mut self, _err: Box<dyn std::any::Any + Send + 'static>) -> Response {
        if let Some(s) = _err.downcast_ref::<String>() {
            tracing::error!(target: "tower_http::catch_panic", "Service panicked: {s}");
        } else if let Some(s) = _err.downcast_ref::<&str>() {
            tracing::error!(target: "tower_http::catch_panic", "Service panicked: {s}");
        } else {
            tracing::error!(
                target: "tower_http::catch_panic",
                "Service panicked but unable to downcast the panic info"
            );
        }
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(
                "x-battlesnake-server-error",
                proxy::SnakeErrorKind::Unknown.display(),
            )
            .body(Body::empty())
            .unwrap()
    }
}
