//! A mock snake for the engine-proxy load check.
//!
//! Serves any path with a fixed-status JSON move response after a random
//! delay, echoing the chosen delay in `X-Mock-Delay-Ms` so the driver can
//! compute *added* latency (wall − snake time) per request.
//!
//! Connections are accepted manually (each `accept()` bumps a counter) and
//! served with hyper-util's auto builder, so accepted-TCP-connection
//! counting is mock-side and identical for both proxies. `--stats-port`
//! serves the accept count over plain HTTP even in TLS mode.
//!
//! Usage:
//! ```text
//! cargo run --release -p engine-proxy --example mock_snake -- \
//!   --port 19100 --stats-port 29001 --delay-min-ms 20 --delay-max-ms 300 \
//!   [--response-bytes 128] [--tls cert.pem key.pem]
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use clap::Parser;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rand::Rng as _;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    port: u16,
    /// Plain-HTTP port that reports the accepted-connection count.
    #[arg(long)]
    stats_port: u16,
    #[arg(long, default_value_t = 20)]
    delay_min_ms: u64,
    #[arg(long, default_value_t = 300)]
    delay_max_ms: u64,
    #[arg(long, default_value_t = 128)]
    response_bytes: usize,
    /// Enable TLS with the given cert/key PEM paths.
    #[arg(long, num_args = 2, value_names = ["CERT", "KEY"])]
    tls: Option<Vec<String>>,
}

fn move_body(response_bytes: usize) -> String {
    let mut body = String::from(r#"{"move":"up""#);
    if response_bytes > body.len() + 2 {
        body.push_str(&format!(
            ",\"pad\":\"{}\"",
            "x".repeat(response_bytes - body.len() - 10)
        ));
    }
    body.push('}');
    body
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let accepts = Arc::new(AtomicU64::new(0));

    // stats listener (always plain HTTP)
    let stats_accepts = accepts.clone();
    let stats_listener = tokio::net::TcpListener::bind(("127.0.0.1", args.stats_port)).await?;
    eprintln!("stats on http://{}", stats_listener.local_addr()?);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = stats_listener.accept().await else {
                return;
            };
            let accepts = stats_accepts.clone();
            let io = TokioIo::new(stream);
            let service = service_fn(move |_| {
                let accepts = accepts.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(200)
                            .header("content-type", "text/plain")
                            .body(Full::new(Bytes::from(
                                accepts.load(Ordering::Relaxed).to_string(),
                            )))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await;
        }
    });

    let tls = match &args.tls {
        None => None,
        Some(paths) => {
            use rustls_pki_types::pem::PemObject;
            let (cert_path, key_path) = (paths[0].clone(), paths[1].clone());
            let certs: Vec<_> = rustls_pki_types::CertificateDer::pem_file_iter(cert_path.clone())
                .unwrap_or_else(|e| panic!("cannot read {cert_path:?}: {e:?}"))
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap_or_else(|e| panic!("bad certs in {cert_path:?}: {e:?}"));
            let key = rustls_pki_types::PrivateKeyDer::from_pem_file(key_path.clone())
                .unwrap_or_else(|e| panic!("no key in {key_path:?}: {e:?}"));
            let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS protocol versions: {e:?}"))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| format!("bad TLS material: {e:?}"))?;
            Some(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
        }
    };

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port)).await?;
    eprintln!(
        "mock snake on {} ({}), delays {}-{}ms",
        listener.local_addr()?,
        if tls.is_some() { "https" } else { "http" },
        args.delay_min_ms,
        args.delay_max_ms
    );
    let body = move_body(args.response_bytes);
    loop {
        let (stream, _) = listener.accept().await?;
        accepts.fetch_add(1, Ordering::Relaxed);
        let tls = tls.clone();
        let body = body.clone();
        let (min, max) = (args.delay_min_ms, args.delay_max_ms);
        tokio::spawn(async move {
            match tls {
                None => serve_conn(TokioIo::new(stream), body, min, max).await,
                Some(acceptor) => {
                    if let Ok(tls_stream) = acceptor.accept(stream).await {
                        serve_conn(TokioIo::new(tls_stream), body, min, max).await;
                    }
                }
            }
        });
    }
}

async fn serve_conn(
    io: TokioIo<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>,
    body: String,
    min_ms: u64,
    max_ms: u64,
) {
    let service = service_fn(move |_req| {
        let body = body.clone();
        async move {
            let delay = if max_ms > min_ms {
                rand::thread_rng().gen_range(min_ms..=max_ms)
            } else {
                min_ms
            };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            Ok::<_, std::convert::Infallible>(
                hyper::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .header("x-mock-delay-ms", delay.to_string())
                    .body(Full::new(Bytes::from(body)))
                    .unwrap(),
            )
        }
    });
    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await;
}
