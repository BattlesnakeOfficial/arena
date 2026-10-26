//! The hand-rolled timed connector.
//!
//! reqwest exposes no DNS/connect/TLS phase timing and `X-Battlesnake-Timing-Ms`
//! requires it, so the outbound client is a `hyper_util::client::legacy::Client`
//! over this connector. It satisfies hyper-util's (sealed) `Connect` contract
//! via the blanket impl for `tower_service::Service<Uri>` whose response is
//! `hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send + 'static`.
//!
//! Phase timings travel from the connector into every response's extensions
//! via `Connected::extra`: hyper-util stores the `Connected` captured at dial
//! time with the pooled connection and re-inserts its extra on *every*
//! dispatch, fresh or reused. `ConnTimings` is a cheap `Arc` handle sharing a
//! first-use flag, so a request can tell whether it is the connection's first
//! user (the honest definition of `fresh_connection` — immune to hyper-util's
//! checkout-vs-dial race, where a background dial lands idle in the pool and
//! the first user of it never waited for the handshake).

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::Uri;
use hyper::body::{Frame, SizeHint};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tower_service::Service;

use crate::config::ProxyConfig;
use crate::metrics;

/// Delay between starting successive connect attempts in the race
/// (Go's `DualStack` fallback delay is 300ms; ours is a touch tighter).
const CONNECT_STAGGER: Duration = Duration::from_millis(250);

/// Error surfaced by the connector. hyper-util preserves the source chain of
/// client errors, so `proxy.rs` walks `Error::source()` and downcasts to this
/// type to pick the `X-Battlesnake-Server-Error` code (2 dns / 3 connect /
/// 4 tls).
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("failed to resolve Battlesnake hostname: {0}")]
    Dns(#[source] io::Error),
    #[error("failed to connect to Battlesnake host: {0}")]
    Connect(#[source] io::Error),
    #[error("failed to set up a TLS connection: {0}")]
    Tls(#[source] io::Error),
}

/// Connector-measured connection phases, shared from the dial into every
/// response dispatched over the connection.
///
/// The phase durations are written once by the connector before the handle is
/// cloned anywhere; only `used` mutates afterwards (atomically).
#[derive(Clone)]
pub struct ConnTimings {
    inner: Arc<ConnTimingsInner>,
}

struct ConnTimingsInner {
    dns_ms: f32,
    connect_ms: f32,
    tls_ms: f32,
    /// When the dial completed (debug aid; also marks that these timings are
    /// dial-time, not dispatch-time).
    dial_end: Instant,
    used: AtomicBool,
}

impl ConnTimings {
    fn new(dns_ms: f32, connect_ms: f32, tls_ms: f32) -> Self {
        Self {
            inner: Arc::new(ConnTimingsInner {
                dns_ms,
                connect_ms,
                tls_ms,
                dial_end: Instant::now(),
                used: AtomicBool::new(false),
            }),
        }
    }

    /// Mark this connection as used by a request. Returns `true` when the
    /// caller is the *first* user, i.e. the connection is fresh for this
    /// request.
    pub fn mark_first_use(&self) -> bool {
        !self.inner.used.swap(true, Ordering::Relaxed)
    }

    pub fn dns_ms(&self) -> f32 {
        self.inner.dns_ms
    }

    pub fn connect_ms(&self) -> f32 {
        self.inner.connect_ms
    }

    pub fn tls_ms(&self) -> f32 {
        self.inner.tls_ms
    }

    #[allow(dead_code)]
    pub fn dial_end(&self) -> Instant {
        self.inner.dial_end
    }
}

/// The stream handed to hyper: plain TCP or TLS-over-TCP.
pub enum ConnStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl tokio::io::AsyncRead for ConnStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            ConnStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for ConnStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            ConnStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_flush(cx),
            ConnStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            ConnStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            ConnStream::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            ConnStream::Plain(s) => s.is_write_vectored(),
            ConnStream::Tls(s) => s.is_write_vectored(),
        }
    }
}

/// hyper connection wrapper: delegates IO to the stream and reports the
/// connection info (timings extra + negotiated h2) hyper-util stores with
/// the pooled connection.
pub struct TimedConn {
    io: TokioIo<ConnStream>,
    timings: ConnTimings,
    is_h2: bool,
}

impl hyper::rt::Read for TimedConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for TimedConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

impl Connection for TimedConn {
    fn connected(&self) -> Connected {
        // `Connected` has no public clone; rebuild from live state each call
        // (the pattern rustls' connector uses).
        let connected = Connected::new().extra(self.timings.clone());
        if self.is_h2 {
            connected.negotiated_h2()
        } else {
            connected
        }
    }
}

#[derive(Clone)]
pub struct TimedConnector {
    tls: TlsConnector,
    dial_timeout: Duration,
}

impl TimedConnector {
    pub fn new(cfg: &ProxyConfig) -> color_eyre::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = &cfg.extra_ca {
            let pem = std::fs::read(path)?;
            let certs: std::result::Result<Vec<_>, _> =
                rustls_pki_types::pem::PemObject::pem_slice_iter(&pem).collect();
            let certs = certs.map_err(|e| {
                color_eyre::eyre::eyre!("bad PEM in ENGINE_PROXY_EXTRA_CA at {path:?}: {e:?}")
            })?;
            if certs.is_empty() {
                return Err(color_eyre::eyre::eyre!(
                    "no certificates found in ENGINE_PROXY_EXTRA_CA at {path:?}"
                ));
            }
            for cert in certs {
                roots
                    .add(cert)
                    .map_err(|e| io::Error::other(format!("bad cert in {path:?}: {e:?}")))?;
            }
            tracing::info!(path = %path.display(), "Loaded extra CA bundle");
        }
        let mut tls_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Self {
            tls: TlsConnector::from(Arc::new(tls_cfg)),
            dial_timeout: cfg.dial_timeout,
        })
    }
}

impl Service<Uri> for TimedConnector {
    type Response = TimedConn;
    type Error = ConnectError;
    type Future = Pin<Box<dyn Future<Output = Result<TimedConn, ConnectError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.connect(uri).await })
    }
}

impl TimedConnector {
    async fn connect(self, uri: Uri) -> Result<TimedConn, ConnectError> {
        let scheme = uri.scheme_str().unwrap_or("http").to_ascii_lowercase();
        let default_port = if scheme == "https" { 443 } else { 80 };
        let port = uri.port_u16().unwrap_or(default_port);
        let raw_host = uri.host().ok_or_else(|| {
            ConnectError::Dns(io::Error::new(io::ErrorKind::InvalidInput, "missing host"))
        })?;
        // `Uri::host()` keeps IPv6 brackets — trim them.
        let host = raw_host.trim_start_matches('[').trim_end_matches(']');

        let dns_start = Instant::now();
        let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port))
                .await
                .map_err(ConnectError::Dns)?
                .collect(),
        };
        let dns_ms = elapsed_ms_f32(dns_start);

        if addrs.is_empty() {
            return Err(ConnectError::Dns(io::Error::new(
                io::ErrorKind::NotFound,
                format!("host resolved to no addresses: {host}"),
            )));
        }

        // IPv4 first (the distant-snake path is overwhelmingly IPv4 today).
        let mut addrs = addrs;
        addrs.sort_by_key(|a| a.is_ipv6());

        let connect_start = Instant::now();
        let counted = |addr: SocketAddr| async move {
            metrics::get().connections_total.inc();
            TcpStream::connect(addr).await
        };
        let stream = connect_race(&addrs, CONNECT_STAGGER, self.dial_timeout, counted)
            .await
            .map_err(ConnectError::Connect)?;
        let connect_ms = elapsed_ms_f32(connect_start);

        let (stream, is_h2, tls_ms) = if scheme == "https" {
            let tls_start = Instant::now();
            let server_name = ServerName::try_from(host.to_string()).map_err(|e| {
                ConnectError::Tls(io::Error::other(format!("invalid server name: {e:?}")))
            })?;
            let tls_stream = Box::new(
                self.tls
                    .connect(server_name, stream)
                    .await
                    .map_err(ConnectError::Tls)?,
            );
            let is_h2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2");
            let tls_ms = elapsed_ms_f32(tls_start);
            metrics::get().tls_handshake_ms.observe(f64::from(tls_ms));
            (ConnStream::Tls(tls_stream), is_h2, tls_ms)
        } else {
            (ConnStream::Plain(stream), false, 0.0)
        };

        Ok(TimedConn {
            io: TokioIo::new(stream),
            timings: ConnTimings::new(dns_ms, connect_ms, tls_ms),
            is_h2,
        })
    }
}

fn elapsed_ms_f32(start: Instant) -> f32 {
    start.elapsed().as_secs_f32() * 1000.0
}

/// Staggered RFC 8305-style connect race, generic over the connect function
/// so tests can drive it with synthetic futures under a paused clock.
///
/// - Attempt `i+1` starts when attempt `i` has failed **or** `stagger` after
///   attempt `i` started; earlier attempts keep running.
/// - No per-attempt timeout: distant IPv4-only snakes (RTT ~200-250ms) and
///   single lost SYNs (kernel retry ~1s) must connect fine, like Go's
///   `DualStack` dialer. Two bounds apply instead: on-path dials are covered
///   by the request deadline, and the whole race is capped by `cap`
///   (`ENGINE_PROXY_DIAL_TIMEOUT_MS`) so hyper-util's *background* dials
///   (spawned when its checkout race loses) cannot hold sockets for the
///   kernel's SYN-retry window.
/// - First success wins; dropping the `JoinSet` aborts losers, which closes
///   their sockets.
pub async fn connect_race<T, F, Fut>(
    addrs: &[SocketAddr],
    stagger: Duration,
    cap: Duration,
    connect_fn: F,
) -> io::Result<T>
where
    T: Send + 'static,
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    debug_assert!(!addrs.is_empty(), "connect_race needs at least one address");
    let race_deadline = tokio::time::Instant::now()
        .checked_add(cap)
        .expect("sane dial cap");

    let mut attempts: JoinSet<io::Result<T>> = JoinSet::new();
    attempts.spawn(connect_fn(addrs[0]));
    let mut spawned = 1usize;
    let mut next_stagger = tokio::time::Instant::now()
        .checked_add(stagger)
        .expect("sane stagger");

    let last_err = loop {
        if attempts.is_empty() {
            if spawned < addrs.len() {
                attempts.spawn(connect_fn(addrs[spawned]));
                spawned += 1;
                next_stagger = tokio::time::Instant::now() + stagger;
                continue;
            }
            break None;
        }
        // The deadline arm is always armed while attempts are in flight — a
        // blackholed host leaves every attempt pending, and without this
        // timer nothing would ever wake the loop to enforce the cap.
        enum Arm<E> {
            Joined(E),
            Stagger,
            Deadline,
        }
        let arm = tokio::select! {
            biased;
            outcome = attempts.join_next() => Arm::Joined(outcome),
            _ = tokio::time::sleep_until(next_stagger), if spawned < addrs.len() => Arm::Stagger,
            _ = tokio::time::sleep_until(race_deadline) => Arm::Deadline,
        };
        match arm {
            Arm::Stagger => {
                attempts.spawn(connect_fn(addrs[spawned]));
                spawned += 1;
                next_stagger += stagger;
            }
            Arm::Deadline => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connect race exceeded dial timeout",
                ));
            }
            Arm::Joined(outcome) => match outcome {
                Some(Ok(Ok(stream))) => return Ok(stream),
                Some(Ok(Err(e))) => {
                    tracing::debug!(error = %crate::logging::redact_error_string(&e.to_string()),
                        "connect attempt failed");
                    if attempts.is_empty() && spawned >= addrs.len() {
                        break Some(e);
                    }
                }
                Some(Err(join_err)) => {
                    let e = io::Error::other(join_err);
                    if attempts.is_empty() && spawned >= addrs.len() {
                        break Some(e);
                    }
                }
                None => unreachable!("join_next pending-guarded by is_empty check"),
            },
        }
    };

    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no addresses")))
}

/// The buffered outbound body, wrapped so the first and final body-frame
/// polls timestamp `client_headers` / `client_body` (see `timing.rs`).
/// Timestamps are measured from the request start instant captured here.
///
/// Implements `http_body::Body` with an exact `SizeHint` so hyper writes a
/// correct `Content-Length` for HTTP/1.1.
pub struct OutboundBody {
    inner: http_body_util::Full<bytes::Bytes>,
    tracker: Arc<BodyTracker>,
    start: Instant,
}

#[derive(Default)]
pub struct BodyTracker {
    first_poll_ms: std::sync::Mutex<Option<f32>>,
    final_poll_ms: std::sync::Mutex<Option<f32>>,
}

impl OutboundBody {
    pub fn new(data: bytes::Bytes, start: Instant) -> (Self, Arc<BodyTracker>) {
        let tracker = Arc::new(BodyTracker::default());
        (
            Self {
                inner: http_body_util::Full::new(data),
                tracker: tracker.clone(),
                start,
            },
            tracker,
        )
    }

    pub fn empty(start: Instant) -> (Self, Arc<BodyTracker>) {
        Self::new(bytes::Bytes::new(), start)
    }
}

impl Clone for OutboundBody {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            tracker: self.tracker.clone(),
            start: self.start,
        }
    }
}

impl BodyTracker {
    pub fn client_headers_ms(&self) -> Option<f32> {
        *self.first_poll_ms.lock().unwrap()
    }

    pub fn client_body_ms(&self) -> Option<f32> {
        *self.final_poll_ms.lock().unwrap()
    }
}
impl http_body::Body for OutboundBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let out = Pin::new(&mut this.inner).poll_frame(cx);
        if out.is_ready() {
            let now = elapsed_ms_f32(this.start);
            if this.tracker.first_poll_ms.lock().unwrap().is_none() {
                *this.tracker.first_poll_ms.lock().unwrap() = Some(now);
            }
            *this.tracker.final_poll_ms.lock().unwrap() = Some(now);
        }
        out
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- connect_race under a paused clock ----

    async fn ok_after<T: Send + 'static>(value: T, delay: Duration) -> io::Result<T> {
        tokio::time::sleep(delay).await;
        Ok(value)
    }

    #[tokio::test(start_paused = true)]
    async fn single_address_completing_at_400ms_succeeds() {
        // No per-attempt cap: a 400ms dial must complete even though the
        // stagger would have fired at 250ms (nothing else to spawn).
        let addrs: Vec<SocketAddr> = vec!["127.0.0.1:1".parse().unwrap()];
        let start = tokio::time::Instant::now();
        let out = connect_race(&addrs, CONNECT_STAGGER, Duration::from_secs(10), |_| {
            ok_after(7u32, Duration::from_millis(400))
        })
        .await
        .unwrap();
        assert_eq!(out, 7);
        assert!(start.elapsed() >= Duration::from_millis(400));
    }

    #[tokio::test(start_paused = true)]
    async fn first_address_never_completes_second_wins_at_stagger() {
        let first_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let addrs = vec![first_addr, "127.0.0.1:2".parse().unwrap()];
        let start = tokio::time::Instant::now();
        let out = connect_race(&addrs, CONNECT_STAGGER, Duration::from_secs(10), |addr| {
            let fut: Pin<Box<dyn Future<Output = io::Result<u32>> + Send>> = if addr == first_addr {
                Box::pin(std::future::pending())
            } else {
                Box::pin(ok_after(21u32, Duration::from_millis(10)))
            };
            fut
        })
        .await
        .unwrap();
        assert_eq!(out, 21);
        // Second attempt starts at the 250ms stagger.
        assert!(start.elapsed() >= Duration::from_millis(250));
        assert!(start.elapsed() < Duration::from_millis(600));
    }

    #[tokio::test(start_paused = true)]
    async fn all_attempts_fail_returns_connect_error() {
        let addrs: Vec<SocketAddr> = vec![
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        ];
        let err = connect_race(
            &addrs,
            CONNECT_STAGGER,
            Duration::from_secs(10),
            |addr| async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Err::<u32, _>(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    addr.to_string(),
                ))
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test(start_paused = true)]
    async fn race_cap_expires_with_timeout_error() {
        let addrs: Vec<SocketAddr> = vec!["127.0.0.1:1".parse().unwrap()];
        let err = connect_race(&addrs, CONNECT_STAGGER, Duration::from_millis(300), |_| {
            std::future::pending::<io::Result<u32>>()
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn conn_timings_first_use_flag() {
        let t = ConnTimings::new(1.0, 2.0, 3.0);
        assert!(t.mark_first_use());
        assert!(!t.mark_first_use());
        let t2 = t.clone();
        assert!(!t2.mark_first_use(), "clone shares the flag");
        assert_eq!(t.dns_ms(), 1.0);
        assert_eq!(t.connect_ms(), 2.0);
        assert_eq!(t.tls_ms(), 3.0);
    }
}
