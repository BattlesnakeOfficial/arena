//! Connection-establishment telemetry for the engine-proxy HTTP clients.
//!
//! A proxied snake call that trips the local guard leaves no trace on the
//! proxy, so arena's logs alone can't say whether the time went to opening a
//! connection (DNS + TCP + TLS) or to a request that was already on the wire.
//! This layer wraps reqwest's connector for the proxy clients and records
//! every new connection: how long it took, how it ended, and how many were
//! still being opened at once. Pooled reuse never reaches the connector, so a
//! request that fires no `proxy_connect` event rode an existing connection.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use tower::{Layer, Service};

use crate::models::battlesnake::EngineRegion;

static IN_FLIGHT_US_WEST1: AtomicUsize = AtomicUsize::new(0);
static IN_FLIGHT_US_EAST4: AtomicUsize = AtomicUsize::new(0);
static IN_FLIGHT_EUROPE_WEST4: AtomicUsize = AtomicUsize::new(0);

fn in_flight_counter(region: EngineRegion) -> &'static AtomicUsize {
    match region {
        EngineRegion::UsWest1 => &IN_FLIGHT_US_WEST1,
        EngineRegion::UsEast4 => &IN_FLIGHT_US_EAST4,
        EngineRegion::EuropeWest4 => &IN_FLIGHT_EUROPE_WEST4,
    }
}

/// New connections to `region`'s proxy currently being established.
pub(crate) fn connects_in_flight(region: EngineRegion) -> usize {
    in_flight_counter(region).load(Ordering::Relaxed)
}

/// Wraps a reqwest connector so every new proxy connection is timed and
/// logged with `metric_type = "proxy_connect"`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProxyConnectTelemetryLayer {
    region: EngineRegion,
}

impl ProxyConnectTelemetryLayer {
    pub(crate) fn new(region: EngineRegion) -> Self {
        Self { region }
    }
}

impl<S> Layer<S> for ProxyConnectTelemetryLayer {
    type Service = ProxyConnectTelemetry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ProxyConnectTelemetry {
            inner,
            region: self.region,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProxyConnectTelemetry<S> {
    inner: S,
    region: EngineRegion,
}

impl<S, Req> Service<Req> for ProxyConnectTelemetry<S>
where
    S: Service<Req>,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    S::Error: std::fmt::Display + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let attempt = ConnectAttempt::start(self.region);
        let fut = self.inner.call(req);
        Box::pin(async move {
            let result = fut.await;
            match &result {
                Ok(_) => attempt.finish("ok", None),
                Err(e) => attempt.finish("error", Some(e.to_string())),
            }
            result
        })
    }
}

/// One connection attempt. Counted in-flight from `start` until it finishes;
/// dropped without finishing (the request gave up mid-connect) it logs as
/// `abandoned`, which is the case a local-guard fault would leave behind.
struct ConnectAttempt {
    region: EngineRegion,
    started: Instant,
    concurrent: usize,
    finished: bool,
}

impl ConnectAttempt {
    fn start(region: EngineRegion) -> Self {
        let concurrent = in_flight_counter(region).fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            region,
            started: Instant::now(),
            concurrent,
            finished: false,
        }
    }

    fn finish(mut self, outcome: &'static str, error: Option<String>) {
        self.finished = true;
        self.record(outcome, error.as_deref());
    }

    fn record(&self, outcome: &'static str, error: Option<&str>) {
        in_flight_counter(self.region).fetch_sub(1, Ordering::Relaxed);
        let connect_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let region = self.region.as_str();
        if outcome == "ok" {
            tracing::info!(
                region,
                connect_ms,
                outcome,
                concurrent_connects = self.concurrent,
                metric_type = "proxy_connect",
                "Engine proxy connection opened"
            );
        } else {
            tracing::warn!(
                region,
                connect_ms,
                outcome,
                concurrent_connects = self.concurrent,
                error,
                metric_type = "proxy_connect",
                "Engine proxy connection not opened"
            );
        }
    }
}

impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        if !self.finished {
            self.record("abandoned", None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tower::ServiceExt as _;

    #[derive(Clone)]
    struct Ready;
    impl Service<()> for Ready {
        type Response = ();
        type Error = Infallible;
        type Future = std::future::Ready<Result<(), Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, (): ()) -> Self::Future {
            std::future::ready(Ok(()))
        }
    }

    #[derive(Clone)]
    struct Never;
    impl Service<()> for Never {
        type Response = ();
        type Error = Infallible;
        type Future = std::future::Pending<Result<(), Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, (): ()) -> Self::Future {
            std::future::pending()
        }
    }

    // Each test uses its own region so parallel tests don't share a counter.

    #[tokio::test]
    async fn completed_connect_leaves_no_in_flight_count() {
        let region = EngineRegion::UsWest1;
        let svc = ProxyConnectTelemetryLayer::new(region).layer(Ready);
        svc.oneshot(()).await.unwrap();
        assert_eq!(connects_in_flight(region), 0);
    }

    #[tokio::test]
    async fn pending_connect_counts_until_abandoned() {
        let region = EngineRegion::EuropeWest4;
        let mut svc = ProxyConnectTelemetryLayer::new(region).layer(Never);
        let fut = svc.ready().await.unwrap().call(());
        assert_eq!(connects_in_flight(region), 1);
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(10), fut).await;
        assert!(timed_out.is_err(), "Never must not resolve");
        assert_eq!(connects_in_flight(region), 0, "drop must release the count");
    }
}
