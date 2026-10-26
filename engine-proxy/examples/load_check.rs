//! Load-check driver for the engine proxy (DEV-1943 step 10).
//!
//! Issues proxied move POSTs with arena's exact header set at a fixed
//! concurrency for a fixed duration, then reports:
//! - p50/p99 *added* latency (wall − the mock's own delay, read from the
//!   forwarded `X-Mock-Delay-Ms` header),
//! - request count and non-200s,
//! - new-connection share from the mock-side accept counters (read before
//!   and after directly from each snake's stats port — the authoritative
//!   method, identical for both proxies).
//!
//! Usage:
//! ```text
//! cargo run --release -p engine-proxy --example load_check -- \
//!   --proxy http://127.0.0.1:18080 --token sekrit --snake-ports 19100-19149 \
//!   --stats-ports 29001-29050 --concurrency 200 --duration 60s [--https]
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    proxy: String,
    #[arg(long)]
    token: String,
    /// Inclusive range of snake ports, e.g. 19100-19149.
    #[arg(long)]
    snake_ports: String,
    /// Inclusive range of stats ports, e.g. 29001-29050.
    #[arg(long)]
    stats_ports: String,
    #[arg(long, default_value_t = 200)]
    concurrency: usize,
    #[arg(long, default_value = "60s")]
    duration: String,
    #[arg(long)]
    https: bool,
}

fn parse_range(s: &str) -> Vec<u16> {
    let (a, b) = s.split_once('-').expect("range like 19100-19149");
    (a.parse().unwrap()..=b.parse().unwrap()).collect()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

async fn stats_total(client: &reqwest::Client, stats_ports: &[u16]) -> u64 {
    let mut total = 0u64;
    for port in stats_ports {
        let fetched = async {
            let res = client
                .get(format!("http://127.0.0.1:{port}/stats"))
                .timeout(Duration::from_secs(5))
                .send()
                .await?
                .error_for_status()?;
            res.text().await
        };
        if let Ok(text) = fetched.await {
            total += text.trim().parse::<u64>().unwrap_or(0);
        }
    }
    total
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let duration = humantime(&args.duration);
    let snake_ports = parse_range(&args.snake_ports);
    let stats_ports = parse_range(&args.stats_ports);
    assert_eq!(
        snake_ports.len(),
        stats_ports.len(),
        "one stats port per snake"
    );
    let scheme = if args.https { "https" } else { "http" };

    // The driver's own client: pools connections to the proxy like arena's
    // engine does. .no_proxy() keeps ambient env proxies out of the path.
    let client = reqwest::Client::builder()
        .no_proxy()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()?;

    let accepts_before = stats_total(&client, &stats_ports).await;
    eprintln!(
        "driver: {} snakes ({scheme}), concurrency {}, duration {duration:?}, accepts_before={accepts_before}",
        snake_ports.len(),
        args.concurrency
    );

    let requests = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let added_ms: Arc<std::sync::Mutex<Vec<f64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let deadline = Instant::now() + duration;

    let mut workers = Vec::new();
    for worker_id in 0..args.concurrency {
        let client = client.clone();
        let proxy = args.proxy.clone();
        let token = args.token.clone();
        let snake_ports = snake_ports.clone();
        let scheme = scheme.to_string();
        let requests = requests.clone();
        let failures = failures.clone();
        let added_ms = added_ms.clone();
        workers.push(tokio::spawn(async move {
            let worker_start = worker_id as u64;
            let mut i: u64 = 0;
            while Instant::now() < deadline {
                i += 1;
                // spread workers over hosts deterministically-ish
                let port = snake_ports
                    [((worker_start.wrapping_add(i) * 7) % snake_ports.len() as u64) as usize];
                let target = format!("{scheme}://127.0.0.1:{port}/move");
                let started = Instant::now();
                let res = client
                    .post(&proxy)
                    .header("X-Request-URI", &target)
                    .header("X-Battlesnake-Timeout-Ms", "10000")
                    .header("X-Proxy-Authorization", format!("token {token}"))
                    .header("User-Agent", "BattlesnakeArena/loadcheck")
                    .header("Content-Type", "application/json")
                    .body(r#"{"move":"up"}"#)
                    .send()
                    .await;
                let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
                requests.fetch_add(1, Ordering::Relaxed);
                match res {
                    Ok(res) if res.status() == 200 => {
                        let mock_delay = res
                            .headers()
                            .get("x-mock-delay-ms")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<f64>().ok())
                            .unwrap_or(0.0);
                        added_ms.lock().unwrap().push(wall_ms - mock_delay);
                        // read the body to reuse the connection
                        let _ = res.bytes().await;
                    }
                    _ => {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }

    let accepts_after = stats_total(&client, &stats_ports).await;
    let total_requests = requests.load(Ordering::Relaxed);
    let total_failures = failures.load(Ordering::Relaxed);
    let mut added = added_ms.lock().unwrap().clone();
    added.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let new_conns = accepts_after.saturating_sub(accepts_before);
    let new_conn_share = new_conns as f64 / total_requests.max(1) as f64;

    println!("RESULT requests={total_requests} failures={total_failures}");
    println!(
        "RESULT added_ms p50={:.1} p99={:.1} max={:.1}",
        percentile(&added, 0.50),
        percentile(&added, 0.99),
        added.last().copied().unwrap_or(f64::NAN)
    );
    println!(
        "RESULT new_connections={new_conns} share={new_conn_share:.4} (accepts {accepts_before} -> {accepts_after})"
    );
    Ok(())
}

fn humantime(s: &str) -> Duration {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        Duration::from_secs_f64(secs.parse().expect("seconds"))
    } else {
        Duration::from_secs(s.parse().expect("seconds"))
    }
}
