//! Stress test binary for Arena - generates load via the Create Game API.
//!
//! Supports configurable load patterns (steady stream, batch), periodic stats output,
//! structured tracing events for Eyes integration, and game completion tracking via SQLite.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use clap::Parser;
use color_eyre::eyre::{Context as _, eyre};
use reqwest::StatusCode;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ============================================================================
// CLI Arguments
// ============================================================================

#[derive(Parser)]
#[command(name = "stress-test")]
#[command(about = "Stress test Arena by generating game creation load")]
struct Cli {
    /// Arena API base URL
    #[arg(long, default_value = "http://localhost:3000")]
    url: String,

    /// Comma-separated snake UUIDs to use for games
    #[arg(long)]
    snakes: String,

    /// API token for authentication
    #[arg(long, env = "ARENA_TOKEN")]
    token: String,

    /// Steady stream rate: N/s (e.g., "10/s" for 10 games per second)
    #[arg(long)]
    steady: Option<String>,

    /// Batch pattern: games,interval (e.g., "100,30s" for 100 games every 30 seconds)
    #[arg(long)]
    batch: Option<String>,

    /// Test duration (e.g., "5m", "1h", "30s")
    #[arg(long, default_value = "1m")]
    duration: String,

    /// Stats output interval in seconds
    #[arg(long, default_value = "10")]
    stats_interval: u64,

    /// Board size for games
    #[arg(long, default_value = "11x11")]
    board: String,

    /// Game type
    #[arg(long = "type", default_value = "standard")]
    game_type: String,

    /// SQLite database path for completion tracking
    #[arg(long, default_value = "stress_test_results.db")]
    db: String,

    /// Poll interval in seconds for checking game completion
    #[arg(long, default_value = "5")]
    poll_interval: u64,

    /// Seconds to wait after load generation before starting completion polling
    #[arg(long, default_value = "0")]
    poll_after: u64,

    /// Maximum seconds to poll for game completion before giving up
    #[arg(long, default_value = "300")]
    poll_timeout: u64,

    /// Disable admin stats collection
    #[arg(long, default_value = "false")]
    no_admin_stats: bool,
}

// ============================================================================
// Duration Parsing
// ============================================================================

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Some(stripped) = s.strip_suffix('s') {
        let secs: u64 = stripped
            .parse()
            .map_err(|_| "Invalid seconds".to_string())?;
        Ok(Duration::from_secs(secs))
    } else if let Some(stripped) = s.strip_suffix('m') {
        let mins: u64 = stripped
            .parse()
            .map_err(|_| "Invalid minutes".to_string())?;
        Ok(Duration::from_secs(mins * 60))
    } else if let Some(stripped) = s.strip_suffix('h') {
        let hours: u64 = stripped.parse().map_err(|_| "Invalid hours".to_string())?;
        Ok(Duration::from_secs(hours * 3600))
    } else {
        Err("Duration must end with 's', 'm', or 'h'".to_string())
    }
}

// ============================================================================
// HTTP Client
// ============================================================================

fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(100)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create HTTP client")
}

#[derive(Debug)]
struct CreateGameResult {
    game_id: Uuid,
    latency: Duration,
}

#[derive(Debug)]
enum GameCreationError {
    Request(reqwest::Error),
    Api { status: StatusCode, body: String },
    Parse(String),
}

impl std::fmt::Display for GameCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(e) => write!(f, "Request error: {}", e),
            Self::Api { status, body } => write!(f, "API error {}: {}", status, body),
            Self::Parse(msg) => write!(f, "Parse error: {}", msg),
        }
    }
}

impl std::error::Error for GameCreationError {}

async fn create_game(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    snakes: &[Uuid],
    board: &str,
    game_type: &str,
) -> Result<CreateGameResult, GameCreationError> {
    let start = Instant::now();

    let response = client
        .post(format!("{}/api/games", base_url))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "snakes": snakes,
            "board": board,
            "game_type": game_type,
        }))
        .send()
        .await;

    let latency = start.elapsed();

    match response {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| GameCreationError::Parse(e.to_string()))?;

            let game_id = body["id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| GameCreationError::Parse("Missing game id".to_string()))?;

            Ok(CreateGameResult { game_id, latency })
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(GameCreationError::Api { status, body })
        }
        Err(e) => Err(GameCreationError::Request(e)),
    }
}

// ============================================================================
// Stats Tracking
// ============================================================================

struct Stats {
    total_games: AtomicU64,
    successful: AtomicU64,
    failed: AtomicU64,
    start_time: Instant,
    latencies: Mutex<Vec<u64>>, // Latencies in microseconds
}

impl Stats {
    fn new() -> Self {
        Self {
            total_games: AtomicU64::new(0),
            successful: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            start_time: Instant::now(),
            latencies: Mutex::new(Vec::with_capacity(10000)),
        }
    }

    fn record_success(&self, latency: Duration) {
        self.total_games.fetch_add(1, Ordering::Relaxed);
        self.successful.fetch_add(1, Ordering::Relaxed);
        let latency_us = latency.as_micros() as u64;
        self.latencies.lock().unwrap().push(latency_us);
    }

    fn record_failure(&self) {
        self.total_games.fetch_add(1, Ordering::Relaxed);
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> StatsSnapshot {
        let total = self.total_games.load(Ordering::Relaxed);
        let successful = self.successful.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let elapsed = self.start_time.elapsed();

        let latencies = self.latencies.lock().unwrap();
        let (avg_latency, p50, p95, p99) = calculate_percentiles(&latencies);

        StatsSnapshot {
            total_games: total,
            successful,
            failed,
            elapsed,
            rate: if elapsed.as_secs_f64() > 0.0 {
                total as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            },
            success_rate: if total > 0 {
                successful as f64 / total as f64 * 100.0
            } else {
                0.0
            },
            avg_latency_ms: avg_latency,
            p50_latency_ms: p50,
            p95_latency_ms: p95,
            p99_latency_ms: p99,
        }
    }
}

struct StatsSnapshot {
    total_games: u64,
    successful: u64,
    failed: u64,
    elapsed: Duration,
    rate: f64,
    success_rate: f64,
    avg_latency_ms: f64,
    p50_latency_ms: f64,
    p95_latency_ms: f64,
    p99_latency_ms: f64,
}

fn calculate_percentiles(latencies: &[u64]) -> (f64, f64, f64, f64) {
    if latencies.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();

    let len = sorted.len();
    let avg = sorted.iter().sum::<u64>() as f64 / len as f64 / 1000.0; // us to ms
    let p50 = sorted[len * 50 / 100] as f64 / 1000.0;
    let p95 = sorted[len * 95 / 100] as f64 / 1000.0;
    let p99_idx = (len * 99 / 100).min(len.saturating_sub(1));
    let p99 = sorted[p99_idx] as f64 / 1000.0;

    (avg, p50, p95, p99)
}

// ============================================================================
// Load Patterns
// ============================================================================

#[derive(Clone)]
struct LoadConfig {
    base_url: String,
    token: String,
    snakes: Vec<Uuid>,
    board: String,
    game_type: String,
    completion_db: Option<CompletionDb>,
}

#[async_trait]
trait LoadPattern: Send + Sync {
    async fn run(
        &self,
        client: &reqwest::Client,
        config: &LoadConfig,
        stats: &Arc<Stats>,
        cancel: CancellationToken,
    );
}

// Steady stream pattern
struct SteadyStreamPattern {
    rate_per_second: f64,
}

impl SteadyStreamPattern {
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if !s.ends_with("/s") {
            return Err("Steady rate must end with '/s' (e.g., '10/s')".to_string());
        }
        let rate: f64 = s[..s.len() - 2]
            .parse()
            .map_err(|_| "Invalid rate number".to_string())?;
        if rate <= 0.0 {
            return Err("Rate must be positive".to_string());
        }
        Ok(Self {
            rate_per_second: rate,
        })
    }
}

#[async_trait]
impl LoadPattern for SteadyStreamPattern {
    async fn run(
        &self,
        client: &reqwest::Client,
        config: &LoadConfig,
        stats: &Arc<Stats>,
        cancel: CancellationToken,
    ) {
        let interval_duration = Duration::from_secs_f64(1.0 / self.rate_per_second);
        let mut interval = tokio::time::interval(interval_duration);
        interval.set_missed_tick_behavior(MissedTickBehavior::Burst);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let client = client.clone();
                    let config = config.clone();
                    let stats = stats.clone();

                    tokio::spawn(async move {
                        match create_game(
                            &client,
                            &config.base_url,
                            &config.token,
                            &config.snakes,
                            &config.board,
                            &config.game_type,
                        )
                        .await
                        {
                            Ok(result) => {
                                stats.record_success(result.latency);
                                if let Some(ref db) = config.completion_db
                                    && let Err(e) =
                                        db.record_game_created(result.game_id).await
                                {
                                    tracing::warn!(error = %e, "failed to record game in completion db");
                                }
                                tracing::info!(
                                    game_id = %result.game_id,
                                    latency_ms = result.latency.as_millis() as u64,
                                    "game_created"
                                );
                            }
                            Err(e) => {
                                stats.record_failure();
                                tracing::warn!(error = %e, "game_creation_failed");
                            }
                        }
                    });
                }
            }
        }
    }
}

// Batch pattern
struct BatchPattern {
    batch_size: u32,
    interval: Duration,
}

impl BatchPattern {
    fn from_str(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() != 2 {
            return Err("Batch format: 'count,interval' (e.g., '100,30s')".to_string());
        }
        let batch_size: u32 = parts[0]
            .trim()
            .parse()
            .map_err(|_| "Invalid batch size".to_string())?;
        let interval = parse_duration(parts[1].trim())?;
        if batch_size == 0 {
            return Err("Batch size must be positive".to_string());
        }
        Ok(Self {
            batch_size,
            interval,
        })
    }
}

#[async_trait]
impl LoadPattern for BatchPattern {
    async fn run(
        &self,
        client: &reqwest::Client,
        config: &LoadConfig,
        stats: &Arc<Stats>,
        cancel: CancellationToken,
    ) {
        let mut interval = tokio::time::interval(self.interval);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    // Spawn batch_size concurrent requests
                    let futures: Vec<_> = (0..self.batch_size)
                        .map(|_| {
                            let client = client.clone();
                            let config = config.clone();
                            let stats = stats.clone();
                            async move {
                                match create_game(
                                    &client,
                                    &config.base_url,
                                    &config.token,
                                    &config.snakes,
                                    &config.board,
                                    &config.game_type,
                                )
                                .await
                                {
                                    Ok(result) => {
                                        stats.record_success(result.latency);
                                        if let Some(ref db) = config.completion_db
                                            && let Err(e) =
                                                db.record_game_created(result.game_id).await
                                        {
                                            tracing::warn!(error = %e, "failed to record game in completion db");
                                        }
                                        tracing::info!(
                                            game_id = %result.game_id,
                                            latency_ms = result.latency.as_millis() as u64,
                                            "game_created"
                                        );
                                    }
                                    Err(e) => {
                                        stats.record_failure();
                                        tracing::warn!(error = %e, "game_creation_failed");
                                    }
                                }
                            }
                        })
                        .collect();

                    futures::future::join_all(futures).await;
                }
            }
        }
    }
}

// ============================================================================
// Stats Output
// ============================================================================

async fn stats_output_task(stats: Arc<Stats>, interval_secs: u64, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                let snapshot = stats.snapshot();

                // Terminal output
                let elapsed = format_duration(snapshot.elapsed);
                println!(
                    "[{}] Games: {} | Rate: {:.1}/s | Success: {:.1}% | Avg: {:.0}ms | p50: {:.0}ms | p95: {:.0}ms | p99: {:.0}ms",
                    elapsed,
                    snapshot.total_games,
                    snapshot.rate,
                    snapshot.success_rate,
                    snapshot.avg_latency_ms,
                    snapshot.p50_latency_ms,
                    snapshot.p95_latency_ms,
                    snapshot.p99_latency_ms,
                );

                // Structured tracing event for Eyes
                tracing::info!(
                    total_games = snapshot.total_games,
                    successful = snapshot.successful,
                    failed = snapshot.failed,
                    rate = snapshot.rate,
                    success_rate = snapshot.success_rate,
                    avg_latency_ms = snapshot.avg_latency_ms,
                    p50_latency_ms = snapshot.p50_latency_ms,
                    p95_latency_ms = snapshot.p95_latency_ms,
                    p99_latency_ms = snapshot.p99_latency_ms,
                    "stress_test_stats"
                );
            }
        }
    }
}

fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, secs)
}

// ============================================================================
// Admin Stats Types (client-side deserialization of /api/admin/stats)
// ============================================================================

#[derive(Debug, Deserialize, Serialize)]
struct AdminStatsResponse {
    job_queue: AdminJobQueue,
    game_counts: AdminGameCounts,
    games_created: AdminTimeWindow,
    games_finished: AdminTimeWindow,
    avg_game_duration_secs: Option<f64>,
    #[serde(default)]
    recent_errors: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AdminJobQueue {
    ready: i64,
    running: i64,
    scheduled: i64,
    total: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct AdminGameCounts {
    waiting: i64,
    running: i64,
    finished: i64,
    total: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct AdminTimeWindow {
    last_hour: i64,
    last_24h: i64,
    last_7d: i64,
}

// ============================================================================
// Completion Database (SQLite)
// ============================================================================

#[derive(Clone)]
struct CompletionDb {
    conn: Arc<Mutex<rusqlite::Connection>>,
    run_id: Uuid,
}

impl CompletionDb {
    fn init_schema_sync(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                base_url TEXT NOT NULL,
                pattern TEXT NOT NULL,
                duration_secs INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS games (
                game_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL REFERENCES runs(run_id),
                created_at TEXT NOT NULL,
                enqueued_at TEXT,
                server_created_at TEXT,
                status TEXT NOT NULL DEFAULT 'created',
                server_updated_at TEXT,
                first_seen_finished_at TEXT,
                poll_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_games_run_id ON games(run_id);
            CREATE INDEX IF NOT EXISTS idx_games_status ON games(status);

            CREATE TABLE IF NOT EXISTS admin_stats_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES runs(run_id),
                captured_at TEXT NOT NULL,
                phase TEXT NOT NULL,
                jobs_ready INTEGER NOT NULL,
                jobs_running INTEGER NOT NULL,
                jobs_scheduled INTEGER NOT NULL,
                jobs_total INTEGER NOT NULL,
                games_waiting INTEGER NOT NULL,
                games_running INTEGER NOT NULL,
                games_finished INTEGER NOT NULL,
                games_total INTEGER NOT NULL,
                games_created_last_hour INTEGER NOT NULL,
                games_finished_last_hour INTEGER NOT NULL,
                avg_game_duration_secs REAL,
                raw_json TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_admin_stats_run_id ON admin_stats_snapshots(run_id);
            ",
        )?;
        Ok(())
    }

    fn insert_run_sync(
        conn: &rusqlite::Connection,
        run_id: Uuid,
        base_url: &str,
        pattern: &str,
        duration_secs: u64,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT INTO runs (run_id, started_at, base_url, pattern, duration_secs) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id.to_string(),
                Utc::now().to_rfc3339(),
                base_url,
                pattern,
                duration_secs as i64,
            ],
        )?;
        Ok(())
    }

    fn new(
        path: &str,
        base_url: &str,
        pattern: &str,
        duration_secs: u64,
    ) -> Result<Self, rusqlite::Error> {
        let conn = rusqlite::Connection::open(path)?;
        Self::init_schema_sync(&conn)?;
        let run_id = Uuid::new_v4();
        Self::insert_run_sync(&conn, run_id, base_url, pattern, duration_secs)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            run_id,
        })
    }

    fn record_game_created_sync(
        conn: &rusqlite::Connection,
        run_id: Uuid,
        game_id: Uuid,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT OR IGNORE INTO games (game_id, run_id, created_at) VALUES (?1, ?2, ?3)",
            params![
                game_id.to_string(),
                run_id.to_string(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    async fn record_game_created(&self, game_id: Uuid) -> Result<(), rusqlite::Error> {
        let conn = self.conn.clone();
        let run_id = self.run_id;
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Self::record_game_created_sync(&conn, run_id, game_id)
        })
        .await
        .unwrap()
    }

    fn get_unfinished_game_ids_sync(
        conn: &rusqlite::Connection,
        run_id: Uuid,
    ) -> Result<Vec<Uuid>, Box<dyn std::error::Error + Send + Sync>> {
        let mut stmt =
            conn.prepare("SELECT game_id FROM games WHERE run_id = ?1 AND status != 'finished'")?;
        let ids = stmt
            .query_map(params![run_id.to_string()], |row| {
                let id_str: String = row.get(0)?;
                Ok(id_str)
            })?
            .filter_map(|r| r.ok())
            .filter_map(|s| Uuid::parse_str(&s).ok())
            .collect();
        Ok(ids)
    }

    async fn get_unfinished_game_ids(
        &self,
    ) -> Result<Vec<Uuid>, Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.conn.clone();
        let run_id = self.run_id;
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Self::get_unfinished_game_ids_sync(&conn, run_id)
        })
        .await
        .unwrap()
    }

    fn update_game_statuses_sync(
        conn: &rusqlite::Connection,
        statuses: &[PollGameStatus],
    ) -> Result<(), rusqlite::Error> {
        let now = Utc::now().to_rfc3339();
        for status in statuses {
            let server_updated_at = status.updated_at.map(|dt| dt.to_rfc3339());
            let enqueued_at = status.enqueued_at.map(|dt| dt.to_rfc3339());
            let server_created_at = status.created_at.map(|dt| dt.to_rfc3339());

            // Set first_seen_finished_at only on first transition to finished
            if status.status == "finished" {
                conn.execute(
                    "UPDATE games SET status = ?1, server_updated_at = ?2, enqueued_at = ?3, server_created_at = ?4, poll_count = poll_count + 1, first_seen_finished_at = COALESCE(first_seen_finished_at, ?5) WHERE game_id = ?6",
                    params![
                        status.status,
                        server_updated_at,
                        enqueued_at,
                        server_created_at,
                        now,
                        status.id.to_string(),
                    ],
                )?;
            } else {
                conn.execute(
                    "UPDATE games SET status = ?1, server_updated_at = ?2, enqueued_at = ?3, server_created_at = ?4, poll_count = poll_count + 1 WHERE game_id = ?5",
                    params![
                        status.status,
                        server_updated_at,
                        enqueued_at,
                        server_created_at,
                        status.id.to_string(),
                    ],
                )?;
            }
        }
        Ok(())
    }

    async fn update_game_statuses(
        &self,
        statuses: Vec<PollGameStatus>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Self::update_game_statuses_sync(&conn, &statuses)
        })
        .await
        .unwrap()
    }

    fn record_admin_stats_sync(
        conn: &rusqlite::Connection,
        run_id: Uuid,
        phase: &str,
        stats: &AdminStatsResponse,
        raw_json: &str,
    ) -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT INTO admin_stats_snapshots (run_id, captured_at, phase, jobs_ready, jobs_running, jobs_scheduled, jobs_total, games_waiting, games_running, games_finished, games_total, games_created_last_hour, games_finished_last_hour, avg_game_duration_secs, raw_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                run_id.to_string(),
                Utc::now().to_rfc3339(),
                phase,
                stats.job_queue.ready,
                stats.job_queue.running,
                stats.job_queue.scheduled,
                stats.job_queue.total,
                stats.game_counts.waiting,
                stats.game_counts.running,
                stats.game_counts.finished,
                stats.game_counts.total,
                stats.games_created.last_hour,
                stats.games_finished.last_hour,
                stats.avg_game_duration_secs,
                raw_json,
            ],
        )?;
        Ok(())
    }

    async fn record_admin_stats(
        &self,
        phase: &str,
        stats: &AdminStatsResponse,
        raw_json: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.clone();
        let run_id = self.run_id;
        let phase = phase.to_string();
        let raw_json = raw_json.to_string();
        // AdminStatsResponse fields are all Copy, so capture them before the move
        let job_queue = AdminJobQueue {
            ready: stats.job_queue.ready,
            running: stats.job_queue.running,
            scheduled: stats.job_queue.scheduled,
            total: stats.job_queue.total,
        };
        let game_counts = AdminGameCounts {
            waiting: stats.game_counts.waiting,
            running: stats.game_counts.running,
            finished: stats.game_counts.finished,
            total: stats.game_counts.total,
        };
        let games_created = AdminTimeWindow {
            last_hour: stats.games_created.last_hour,
            last_24h: stats.games_created.last_24h,
            last_7d: stats.games_created.last_7d,
        };
        let games_finished = AdminTimeWindow {
            last_hour: stats.games_finished.last_hour,
            last_24h: stats.games_finished.last_24h,
            last_7d: stats.games_finished.last_7d,
        };
        let avg_game_duration_secs = stats.avg_game_duration_secs;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let stats_copy = AdminStatsResponse {
                job_queue,
                game_counts,
                games_created,
                games_finished,
                avg_game_duration_secs,
                recent_errors: vec![],
            };
            Self::record_admin_stats_sync(&conn, run_id, &phase, &stats_copy, &raw_json)
        })
        .await
        .unwrap()
    }

    fn total_count_sync(conn: &rusqlite::Connection, run_id: Uuid) -> Result<u64, rusqlite::Error> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM games WHERE run_id = ?1",
            params![run_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    fn generate_report_sync(
        conn: &rusqlite::Connection,
        run_id: Uuid,
        poll_interval_secs: u64,
    ) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
        let total_games = Self::total_count_sync(conn, run_id)?;
        let run_id_str = run_id.to_string();

        let finished: u64 = conn.query_row(
            "SELECT COUNT(*) FROM games WHERE run_id = ?1 AND status = 'finished'",
            params![run_id_str],
            |row| row.get::<_, i64>(0),
        )? as u64;

        let stuck_running: u64 = conn.query_row(
            "SELECT COUNT(*) FROM games WHERE run_id = ?1 AND status = 'running'",
            params![run_id_str],
            |row| row.get::<_, i64>(0),
        )? as u64;

        let not_started = total_games
            .saturating_sub(finished)
            .saturating_sub(stuck_running);

        // Try server-side timing first (server_updated_at - enqueued_at)
        let server_timing_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM games WHERE run_id = ?1 AND status = 'finished' AND server_updated_at IS NOT NULL AND enqueued_at IS NOT NULL",
            params![run_id_str],
            |row| row.get(0),
        )?;

        let (timing_source, completion_times_ms) = if server_timing_count > 0 {
            // Use server-side timing
            let mut stmt = conn.prepare(
                "SELECT server_updated_at, enqueued_at FROM games WHERE run_id = ?1 AND status = 'finished' AND server_updated_at IS NOT NULL AND enqueued_at IS NOT NULL",
            )?;
            let times: Vec<f64> = stmt
                .query_map(params![run_id_str], |row| {
                    let updated: String = row.get(0)?;
                    let enqueued: String = row.get(1)?;
                    Ok((updated, enqueued))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(updated, enqueued)| {
                    let updated = chrono::DateTime::parse_from_rfc3339(&updated).ok()?;
                    let enqueued = chrono::DateTime::parse_from_rfc3339(&enqueued).ok()?;
                    let diff = updated.signed_duration_since(enqueued);
                    Some(diff.num_milliseconds() as f64)
                })
                .collect();
            (TimingSource::ServerSide, times)
        } else {
            // Fall back to client-observed timing
            let mut stmt = conn.prepare(
                "SELECT first_seen_finished_at, created_at FROM games WHERE run_id = ?1 AND status = 'finished' AND first_seen_finished_at IS NOT NULL",
            )?;
            let times: Vec<f64> = stmt
                .query_map(params![run_id_str], |row| {
                    let finished: String = row.get(0)?;
                    let created: String = row.get(1)?;
                    Ok((finished, created))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(finished, created)| {
                    let finished = chrono::DateTime::parse_from_rfc3339(&finished).ok()?;
                    let created = chrono::DateTime::parse_from_rfc3339(&created).ok()?;
                    let diff = finished.signed_duration_since(created);
                    Some(diff.num_milliseconds() as f64)
                })
                .collect();
            (TimingSource::ClientObserved { poll_interval_secs }, times)
        };

        // Calculate percentile stats from completion times
        let (avg_completion_ms, p50_completion_ms, p95_completion_ms, p99_completion_ms) =
            if completion_times_ms.is_empty() {
                (None, None, None, None)
            } else {
                let mut sorted = completion_times_ms.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let len = sorted.len();
                let avg = sorted.iter().sum::<f64>() / len as f64;
                let p50 = sorted[len * 50 / 100];
                let p95 = sorted[len * 95 / 100];
                let p99_idx = (len * 99 / 100).min(len.saturating_sub(1));
                let p99 = sorted[p99_idx];
                (Some(avg), Some(p50), Some(p95), Some(p99))
            };

        // Overall duration and throughput
        let first_game_created: Option<String> = conn
            .query_row(
                "SELECT MIN(created_at) FROM games WHERE run_id = ?1",
                params![run_id_str],
                |row| row.get(0),
            )
            .ok();

        let last_game_finished: Option<String> = conn
            .query_row(
                "SELECT MAX(COALESCE(server_updated_at, first_seen_finished_at)) FROM games WHERE run_id = ?1 AND status = 'finished'",
                params![run_id_str],
                |row| row.get(0),
            )
            .ok();

        let overall_duration_secs = match (&first_game_created, &last_game_finished) {
            (Some(first), Some(last)) => {
                let first = chrono::DateTime::parse_from_rfc3339(first).ok();
                let last = chrono::DateTime::parse_from_rfc3339(last).ok();
                match (first, last) {
                    (Some(f), Some(l)) => {
                        Some(l.signed_duration_since(f).num_milliseconds() as f64 / 1000.0)
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        let throughput_per_min = match (finished, overall_duration_secs) {
            (f, Some(d)) if f > 0 && d > 0.0 => Some(f as f64 / (d / 60.0)),
            _ => None,
        };

        // Admin stats summary
        let admin_stats_summary = Self::generate_admin_stats_summary_sync(conn, run_id)?;

        Ok(Report {
            run_id,
            total_games,
            finished,
            stuck_running,
            not_started,
            timing_source,
            avg_completion_ms,
            p50_completion_ms,
            p95_completion_ms,
            p99_completion_ms,
            first_game_created,
            last_game_finished,
            overall_duration_secs,
            throughput_per_min,
            admin_stats_summary,
        })
    }

    fn generate_admin_stats_summary_sync(
        conn: &rusqlite::Connection,
        run_id: Uuid,
    ) -> Result<Option<AdminStatsSummary>, Box<dyn std::error::Error + Send + Sync>> {
        let run_id_str = run_id.to_string();

        let snapshot_count: u64 = conn.query_row(
            "SELECT COUNT(*) FROM admin_stats_snapshots WHERE run_id = ?1",
            params![run_id_str],
            |row| row.get::<_, i64>(0),
        )? as u64;

        if snapshot_count == 0 {
            return Ok(None);
        }

        let peak_jobs_ready: i64 = conn.query_row(
            "SELECT MAX(jobs_ready) FROM admin_stats_snapshots WHERE run_id = ?1",
            params![run_id_str],
            |row| row.get(0),
        )?;
        let peak_jobs_running: i64 = conn.query_row(
            "SELECT MAX(jobs_running) FROM admin_stats_snapshots WHERE run_id = ?1",
            params![run_id_str],
            |row| row.get(0),
        )?;
        let peak_games_waiting: i64 = conn.query_row(
            "SELECT MAX(games_waiting) FROM admin_stats_snapshots WHERE run_id = ?1",
            params![run_id_str],
            |row| row.get(0),
        )?;
        let peak_games_running: i64 = conn.query_row(
            "SELECT MAX(games_running) FROM admin_stats_snapshots WHERE run_id = ?1",
            params![run_id_str],
            |row| row.get(0),
        )?;

        // Final state from latest snapshot
        let (
            final_jobs_ready,
            final_jobs_total,
            final_games_finished,
            final_games_total,
            final_avg_game_duration_secs,
            final_raw_json,
        ): (i64, i64, i64, i64, Option<f64>, String) = conn.query_row(
            "SELECT jobs_ready, jobs_total, games_finished, games_total, avg_game_duration_secs, raw_json FROM admin_stats_snapshots WHERE run_id = ?1 ORDER BY captured_at DESC LIMIT 1",
            params![run_id_str],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )?;

        // Check for errors in final snapshot raw_json
        let has_errors = serde_json::from_str::<serde_json::Value>(&final_raw_json)
            .ok()
            .and_then(|v| v.get("recent_errors").cloned())
            .and_then(|v| v.as_array().cloned())
            .is_some_and(|arr| !arr.is_empty());

        Ok(Some(AdminStatsSummary {
            snapshot_count,
            peak_jobs_ready,
            peak_jobs_running,
            peak_games_waiting,
            peak_games_running,
            final_jobs_ready,
            final_jobs_total,
            final_games_finished,
            final_games_total,
            final_avg_game_duration_secs,
            has_errors,
        }))
    }

    async fn generate_report(
        &self,
        poll_interval_secs: u64,
    ) -> Result<Report, Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.conn.clone();
        let run_id = self.run_id;
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Self::generate_report_sync(&conn, run_id, poll_interval_secs)
        })
        .await
        .unwrap()
    }
}

// ============================================================================
// Poll Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct PollGameStatus {
    id: Uuid,
    status: String,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ============================================================================
// Report Types
// ============================================================================

struct Report {
    run_id: Uuid,
    total_games: u64,
    finished: u64,
    stuck_running: u64,
    not_started: u64,
    timing_source: TimingSource,
    avg_completion_ms: Option<f64>,
    p50_completion_ms: Option<f64>,
    p95_completion_ms: Option<f64>,
    p99_completion_ms: Option<f64>,
    first_game_created: Option<String>,
    last_game_finished: Option<String>,
    overall_duration_secs: Option<f64>,
    throughput_per_min: Option<f64>,
    admin_stats_summary: Option<AdminStatsSummary>,
}

enum TimingSource {
    ServerSide,
    ClientObserved { poll_interval_secs: u64 },
}

struct AdminStatsSummary {
    snapshot_count: u64,
    peak_jobs_ready: i64,
    peak_jobs_running: i64,
    peak_games_waiting: i64,
    peak_games_running: i64,
    final_jobs_ready: i64,
    final_jobs_total: i64,
    final_games_finished: i64,
    final_games_total: i64,
    final_avg_game_duration_secs: Option<f64>,
    has_errors: bool,
}

impl Report {
    fn print(&self) {
        println!();
        println!("=== Completion Report (run {}) ===", self.run_id);
        println!();
        println!("Games:");
        println!("  Total:          {}", self.total_games);
        println!("  Finished:       {}", self.finished);
        println!("  Stuck (running): {}", self.stuck_running);
        println!("  Not started:    {}", self.not_started);

        if self.stuck_running > 0 {
            println!();
            println!("  NOTE: Games stuck at 'running' may indicate server memory pressure.");
        }

        println!();
        match &self.timing_source {
            TimingSource::ServerSide => println!("Timing: server-side (enqueued_at -> updated_at)"),
            TimingSource::ClientObserved { poll_interval_secs } => println!(
                "Timing: client-observed (poll interval: {}s)",
                poll_interval_secs
            ),
        }

        if let Some(avg) = self.avg_completion_ms {
            println!("  Avg completion:  {:.0}ms", avg);
        }
        if let Some(p50) = self.p50_completion_ms {
            println!("  p50 completion:  {:.0}ms", p50);
        }
        if let Some(p95) = self.p95_completion_ms {
            println!("  p95 completion:  {:.0}ms", p95);
        }
        if let Some(p99) = self.p99_completion_ms {
            println!("  p99 completion:  {:.0}ms", p99);
        }

        println!();
        if let Some(ref first) = self.first_game_created {
            println!("First created:     {}", first);
        }
        if let Some(ref last) = self.last_game_finished {
            println!("Last finished:     {}", last);
        }
        if let Some(duration) = self.overall_duration_secs {
            println!("Overall duration:  {:.1}s", duration);
        }
        if let Some(throughput) = self.throughput_per_min {
            println!("Throughput:        {:.1} games/min", throughput);
        }

        if let Some(ref summary) = self.admin_stats_summary {
            println!();
            println!(
                "=== Server Metrics ({} snapshots) ===",
                summary.snapshot_count
            );
            println!("  Peak jobs ready:    {}", summary.peak_jobs_ready);
            println!("  Peak jobs running:  {}", summary.peak_jobs_running);
            println!("  Peak games waiting: {}", summary.peak_games_waiting);
            println!("  Peak games running: {}", summary.peak_games_running);
            println!();
            println!("  Final jobs ready:   {}", summary.final_jobs_ready);
            println!("  Final jobs total:   {}", summary.final_jobs_total);
            println!("  Final games finished: {}", summary.final_games_finished);
            println!("  Final games total:  {}", summary.final_games_total);
            if let Some(avg_dur) = summary.final_avg_game_duration_secs {
                println!("  Server avg game duration: {:.1}s", avg_dur);
            }
            if summary.has_errors {
                println!("  WARNING: Recent job errors detected on server");
            }
        }
    }
}

// ============================================================================
// Completion Poller
// ============================================================================

#[allow(clippy::too_many_arguments)]
async fn completion_poller(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    db: &CompletionDb,
    poll_interval: Duration,
    poll_timeout: Duration,
    collect_admin_stats: bool,
    cancel: CancellationToken,
) {
    let start = Instant::now();
    let mut admin_stats_available = collect_admin_stats;
    let mut admin_stats_warned = false;
    let mut interval = tokio::time::interval(poll_interval);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                // Check timeout
                if start.elapsed() > poll_timeout {
                    tracing::info!("Poll timeout reached after {:?}", poll_timeout);
                    break;
                }

                // Get unfinished game IDs
                let unfinished = match db.get_unfinished_game_ids().await {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to get unfinished game IDs");
                        continue;
                    }
                };

                if unfinished.is_empty() {
                    tracing::info!("All games finished");
                    break;
                }

                // Batch into chunks of 500
                let total_unfinished = unfinished.len();
                for chunk in unfinished.chunks(500) {
                    let body = serde_json::json!({ "game_ids": chunk });
                    match client
                        .post(format!("{}/api/games/status", base_url))
                        .bearer_auth(token)
                        .json(&body)
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.json::<Vec<PollGameStatus>>().await {
                                Ok(statuses) => {
                                    if let Err(e) = db.update_game_statuses(statuses).await {
                                        tracing::warn!(error = %e, "failed to update game statuses");
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "failed to parse poll response");
                                }
                            }
                        }
                        Ok(resp) => {
                            tracing::warn!(status = %resp.status(), "poll request returned error status");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "poll request failed");
                        }
                    }
                }

                println!(
                    "[polling] {}/{} games still unfinished ({:.0}s elapsed)",
                    total_unfinished,
                    total_unfinished, // will show what we started with this tick
                    start.elapsed().as_secs_f64()
                );

                // Collect admin stats if available
                if admin_stats_available {
                    match fetch_admin_stats(client, base_url, token).await {
                        Ok((stats, raw_json)) => {
                            if let Err(e) = db.record_admin_stats("polling", &stats, &raw_json).await {
                                tracing::warn!(error = %e, "failed to record admin stats");
                            }
                        }
                        Err(AdminStatsError::Forbidden) => {
                            if !admin_stats_warned {
                                tracing::warn!("Admin stats unavailable (403) - disabling collection");
                                admin_stats_warned = true;
                            }
                            admin_stats_available = false;
                        }
                        Err(AdminStatsError::Other(e)) => {
                            tracing::warn!(error = %e, "failed to fetch admin stats");
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Admin Stats Fetching
// ============================================================================

enum AdminStatsError {
    Forbidden,
    Other(String),
}

async fn fetch_admin_stats(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<(AdminStatsResponse, String), AdminStatsError> {
    let resp = client
        .get(format!("{}/api/admin/stats", base_url))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| AdminStatsError::Other(e.to_string()))?;

    if resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND {
        return Err(AdminStatsError::Forbidden);
    }

    if !resp.status().is_success() {
        return Err(AdminStatsError::Other(format!("status {}", resp.status())));
    }

    let raw_json = resp
        .text()
        .await
        .map_err(|e| AdminStatsError::Other(e.to_string()))?;

    let stats: AdminStatsResponse = serde_json::from_str(&raw_json)
        .map_err(|e| AdminStatsError::Other(format!("parse error: {}", e)))?;

    Ok((stats, raw_json))
}

async fn admin_stats_load_phase(
    client: reqwest::Client,
    base_url: String,
    token: String,
    db: CompletionDb,
    poll_interval: Duration,
) {
    let mut interval = tokio::time::interval(poll_interval);

    loop {
        interval.tick().await;
        match fetch_admin_stats(&client, &base_url, &token).await {
            Ok((stats, raw_json)) => {
                if let Err(e) = db.record_admin_stats("load", &stats, &raw_json).await {
                    tracing::warn!(error = %e, "failed to record load-phase admin stats");
                }
            }
            Err(AdminStatsError::Forbidden) => {
                tracing::warn!(
                    "Admin stats unavailable (403) during load phase - stopping collection"
                );
                return;
            }
            Err(AdminStatsError::Other(e)) => {
                tracing::warn!(error = %e, "failed to fetch load-phase admin stats");
            }
        }
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    // Setup tracing (JSON for Eyes compatibility)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("stress_test=info".parse().unwrap()),
        )
        .json()
        .init();

    let cli = Cli::parse();

    // Parse and validate snake UUIDs
    let snakes: Vec<Uuid> = cli
        .snakes
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(Uuid::parse_str)
        .collect::<Result<Vec<_>, _>>()
        .wrap_err("Invalid snake UUID format")?;

    if snakes.is_empty() {
        return Err(eyre!("At least one snake UUID is required"));
    }

    // Parse duration
    let duration = parse_duration(&cli.duration).map_err(|e| eyre!("Invalid duration: {}", e))?;

    // Build load patterns
    let mut patterns: Vec<Box<dyn LoadPattern>> = Vec::new();
    let mut pattern_desc = String::new();

    if let Some(ref steady) = cli.steady {
        let pattern = SteadyStreamPattern::from_str(steady)
            .map_err(|e| eyre!("Invalid steady pattern: {}", e))?;
        pattern_desc.push_str(&format!("steady:{}", steady));
        patterns.push(Box::new(pattern));
    }

    if let Some(ref batch) = cli.batch {
        let pattern =
            BatchPattern::from_str(batch).map_err(|e| eyre!("Invalid batch pattern: {}", e))?;
        if !pattern_desc.is_empty() {
            pattern_desc.push('+');
        }
        pattern_desc.push_str(&format!("batch:{}", batch));
        patterns.push(Box::new(pattern));
    }

    if patterns.is_empty() {
        return Err(eyre!(
            "At least one load pattern (--steady or --batch) is required"
        ));
    }

    // Create completion database
    let completion_db = CompletionDb::new(&cli.db, &cli.url, &pattern_desc, duration.as_secs())
        .map_err(|e| eyre!("Failed to create completion database: {}", e))?;

    // Create shared state
    let client = create_http_client();
    let stats = Arc::new(Stats::new());
    let cancel = CancellationToken::new();

    let config = LoadConfig {
        base_url: cli.url.clone(),
        token: cli.token.clone(),
        snakes,
        board: cli.board.clone(),
        game_type: cli.game_type.clone(),
        completion_db: Some(completion_db.clone()),
    };

    println!("Starting stress test against {}", cli.url);
    println!("Duration: {}", cli.duration);
    println!("Patterns: {}", patterns.len());
    println!("Snakes: {:?}", config.snakes);
    println!("Results DB: {}", cli.db);
    println!("Run ID: {}", completion_db.run_id);
    println!();

    // Spawn load-phase admin stats collector
    let admin_stats_handle = if !cli.no_admin_stats {
        let client = client.clone();
        let base_url = cli.url.clone();
        let token = cli.token.clone();
        let db = completion_db.clone();
        let poll_interval = Duration::from_secs(cli.poll_interval);
        Some(tokio::spawn(async move {
            admin_stats_load_phase(client, base_url, token, db, poll_interval).await;
        }))
    } else {
        None
    };

    // Spawn load pattern tasks
    let mut handles = Vec::new();
    for pattern in patterns {
        let client = client.clone();
        let config = config.clone();
        let stats = stats.clone();
        let cancel = cancel.clone();

        handles.push(tokio::spawn(async move {
            pattern.run(&client, &config, &stats, cancel).await;
        }));
    }

    // Spawn stats output task
    let stats_handle = {
        let stats = stats.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            stats_output_task(stats, cli.stats_interval, cancel).await;
        })
    };

    // Wait for duration then cancel
    tokio::time::sleep(duration).await;
    cancel.cancel();

    // Wait for load tasks to finish
    for handle in handles {
        let _ = handle.await;
    }
    let _ = stats_handle.await;

    // Abort load-phase admin stats collector
    if let Some(handle) = admin_stats_handle {
        handle.abort();
        let _ = handle.await;
    }

    // Final creation stats output
    let final_snapshot = stats.snapshot();
    println!();
    println!("=== Creation Results ===");
    println!("Total games: {}", final_snapshot.total_games);
    println!("Successful: {}", final_snapshot.successful);
    println!("Failed: {}", final_snapshot.failed);
    println!("Success rate: {:.1}%", final_snapshot.success_rate);
    println!("Average rate: {:.1} games/sec", final_snapshot.rate);
    println!("Avg latency: {:.0}ms", final_snapshot.avg_latency_ms);
    println!("p50 latency: {:.0}ms", final_snapshot.p50_latency_ms);
    println!("p95 latency: {:.0}ms", final_snapshot.p95_latency_ms);
    println!("p99 latency: {:.0}ms", final_snapshot.p99_latency_ms);

    // Wait before polling if configured
    if cli.poll_after > 0 {
        println!();
        println!("Waiting {}s before polling...", cli.poll_after);
        tokio::time::sleep(Duration::from_secs(cli.poll_after)).await;
    }

    // Start completion polling
    println!();
    println!(
        "Starting completion polling (interval: {}s, timeout: {}s)...",
        cli.poll_interval, cli.poll_timeout
    );

    let poll_cancel = CancellationToken::new();
    completion_poller(
        &client,
        &cli.url,
        &cli.token,
        &completion_db,
        Duration::from_secs(cli.poll_interval),
        Duration::from_secs(cli.poll_timeout),
        !cli.no_admin_stats,
        poll_cancel,
    )
    .await;

    // Generate and print completion report
    match completion_db.generate_report(cli.poll_interval).await {
        Ok(report) => report.print(),
        Err(e) => {
            tracing::error!(error = %e, "failed to generate completion report");
            println!();
            println!("ERROR: Failed to generate completion report: {}", e);
        }
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("0s").unwrap(), Duration::from_secs(0));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("30x").is_err());
    }

    #[test]
    fn test_steady_stream_pattern_parsing() {
        let pattern = SteadyStreamPattern::from_str("10/s").unwrap();
        assert!((pattern.rate_per_second - 10.0).abs() < f64::EPSILON);

        let pattern = SteadyStreamPattern::from_str("0.5/s").unwrap();
        assert!((pattern.rate_per_second - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_steady_stream_pattern_invalid() {
        assert!(SteadyStreamPattern::from_str("10").is_err());
        assert!(SteadyStreamPattern::from_str("abc/s").is_err());
        assert!(SteadyStreamPattern::from_str("0/s").is_err());
        assert!(SteadyStreamPattern::from_str("-1/s").is_err());
    }

    #[test]
    fn test_batch_pattern_parsing() {
        let pattern = BatchPattern::from_str("100,30s").unwrap();
        assert_eq!(pattern.batch_size, 100);
        assert_eq!(pattern.interval, Duration::from_secs(30));

        let pattern = BatchPattern::from_str("50, 1m").unwrap();
        assert_eq!(pattern.batch_size, 50);
        assert_eq!(pattern.interval, Duration::from_secs(60));
    }

    #[test]
    fn test_batch_pattern_invalid() {
        assert!(BatchPattern::from_str("100").is_err());
        assert!(BatchPattern::from_str("100,30s,extra").is_err());
        assert!(BatchPattern::from_str("abc,30s").is_err());
        assert!(BatchPattern::from_str("0,30s").is_err());
    }

    #[test]
    fn test_calculate_percentiles_empty() {
        let (avg, p50, p95, p99) = calculate_percentiles(&[]);
        assert_eq!(avg, 0.0);
        assert_eq!(p50, 0.0);
        assert_eq!(p95, 0.0);
        assert_eq!(p99, 0.0);
    }

    #[test]
    fn test_calculate_percentiles() {
        // 100 values from 1000 to 100000 microseconds (1ms to 100ms)
        let latencies: Vec<u64> = (1..=100).map(|i| i * 1000).collect();
        let (avg, p50, p95, p99) = calculate_percentiles(&latencies);

        // Average of 1..=100 is 50.5, so in ms: 50.5
        assert!((avg - 50.5).abs() < 0.1);

        // For 100 elements: len*50/100 = 50, sorted[50] = 51ms
        // So p50 is around 51ms (integer division floors)
        assert!((p50 - 51.0).abs() < 1.0);

        // p95: len*95/100 = 95, sorted[95] = 96ms
        assert!((p95 - 96.0).abs() < 1.0);

        // p99: min(len*99/100, len-1) = min(99, 99) = 99, sorted[99] = 100ms
        assert!((p99 - 100.0).abs() < 1.0);
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_duration(Duration::from_secs(61)), "00:01:01");
        assert_eq!(format_duration(Duration::from_secs(3661)), "01:01:01");
        assert_eq!(format_duration(Duration::from_secs(90)), "00:01:30");
    }

    // ========================================================================
    // Completion DB Tests
    // ========================================================================

    fn new_in_memory_db() -> (rusqlite::Connection, Uuid) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        CompletionDb::init_schema_sync(&conn).unwrap();
        let run_id = Uuid::new_v4();
        CompletionDb::insert_run_sync(&conn, run_id, "http://test", "test", 30).unwrap();
        (conn, run_id)
    }

    #[test]
    fn test_completion_db_creation() {
        let (conn, _run_id) = new_in_memory_db();
        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"runs".to_string()));
        assert!(tables.contains(&"games".to_string()));
        assert!(tables.contains(&"admin_stats_snapshots".to_string()));
    }

    #[test]
    fn test_record_and_query_games() {
        let (conn, run_id) = new_in_memory_db();

        let game1 = Uuid::new_v4();
        let game2 = Uuid::new_v4();
        CompletionDb::record_game_created_sync(&conn, run_id, game1).unwrap();
        CompletionDb::record_game_created_sync(&conn, run_id, game2).unwrap();

        let unfinished = CompletionDb::get_unfinished_game_ids_sync(&conn, run_id).unwrap();
        assert_eq!(unfinished.len(), 2);
        assert!(unfinished.contains(&game1));
        assert!(unfinished.contains(&game2));

        // Verify UUID TEXT roundtrip
        let stored: String = conn
            .query_row(
                "SELECT game_id FROM games WHERE game_id = ?1",
                params![game1.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(Uuid::parse_str(&stored).unwrap(), game1);
    }

    #[test]
    fn test_update_game_statuses() {
        let (conn, run_id) = new_in_memory_db();

        let game1 = Uuid::new_v4();
        CompletionDb::record_game_created_sync(&conn, run_id, game1).unwrap();

        // First update: still running
        let statuses = vec![PollGameStatus {
            id: game1,
            status: "running".to_string(),
            updated_at: Some(Utc::now()),
            enqueued_at: Some(Utc::now()),
            created_at: Some(Utc::now()),
        }];
        CompletionDb::update_game_statuses_sync(&conn, &statuses).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM games WHERE game_id = ?1",
                params![game1.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");

        let first_seen: Option<String> = conn
            .query_row(
                "SELECT first_seen_finished_at FROM games WHERE game_id = ?1",
                params![game1.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(first_seen.is_none());

        // Second update: finished
        let statuses = vec![PollGameStatus {
            id: game1,
            status: "finished".to_string(),
            updated_at: Some(Utc::now()),
            enqueued_at: Some(Utc::now()),
            created_at: Some(Utc::now()),
        }];
        CompletionDb::update_game_statuses_sync(&conn, &statuses).unwrap();

        let first_seen: Option<String> = conn
            .query_row(
                "SELECT first_seen_finished_at FROM games WHERE game_id = ?1",
                params![game1.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(first_seen.is_some());
        let first_seen_val = first_seen.unwrap();

        // Third update: should not change first_seen_finished_at
        let statuses = vec![PollGameStatus {
            id: game1,
            status: "finished".to_string(),
            updated_at: Some(Utc::now()),
            enqueued_at: Some(Utc::now()),
            created_at: Some(Utc::now()),
        }];
        CompletionDb::update_game_statuses_sync(&conn, &statuses).unwrap();

        let first_seen_after: String = conn
            .query_row(
                "SELECT first_seen_finished_at FROM games WHERE game_id = ?1",
                params![game1.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_seen_val, first_seen_after);
    }

    #[test]
    fn test_report_generation() {
        let (conn, run_id) = new_in_memory_db();

        let game1 = Uuid::new_v4();
        let game2 = Uuid::new_v4();
        CompletionDb::record_game_created_sync(&conn, run_id, game1).unwrap();
        CompletionDb::record_game_created_sync(&conn, run_id, game2).unwrap();

        // Finish game1
        let statuses = vec![PollGameStatus {
            id: game1,
            status: "finished".to_string(),
            updated_at: Some(Utc::now()),
            enqueued_at: Some(Utc::now() - chrono::Duration::seconds(5)),
            created_at: Some(Utc::now() - chrono::Duration::seconds(10)),
        }];
        CompletionDb::update_game_statuses_sync(&conn, &statuses).unwrap();

        let report = CompletionDb::generate_report_sync(&conn, run_id, 5).unwrap();
        assert_eq!(report.total_games, 2);
        assert_eq!(report.finished, 1);
        // game2 still at 'created' status
        assert_eq!(report.not_started, 1);
        assert_eq!(report.stuck_running, 0);
    }

    #[test]
    fn test_report_all_games_accounted_for() {
        let (conn, run_id) = new_in_memory_db();

        let game1 = Uuid::new_v4();
        let game2 = Uuid::new_v4();
        let game3 = Uuid::new_v4();
        CompletionDb::record_game_created_sync(&conn, run_id, game1).unwrap();
        CompletionDb::record_game_created_sync(&conn, run_id, game2).unwrap();
        CompletionDb::record_game_created_sync(&conn, run_id, game3).unwrap();

        // game1 finished, game2 running (stuck), game3 still created
        CompletionDb::update_game_statuses_sync(
            &conn,
            &[PollGameStatus {
                id: game1,
                status: "finished".to_string(),
                updated_at: Some(Utc::now()),
                enqueued_at: None,
                created_at: None,
            }],
        )
        .unwrap();
        CompletionDb::update_game_statuses_sync(
            &conn,
            &[PollGameStatus {
                id: game2,
                status: "running".to_string(),
                updated_at: None,
                enqueued_at: None,
                created_at: None,
            }],
        )
        .unwrap();

        let report = CompletionDb::generate_report_sync(&conn, run_id, 5).unwrap();
        assert_eq!(
            report.total_games,
            report.finished + report.stuck_running + report.not_started
        );
        assert_eq!(report.finished, 1);
        assert_eq!(report.stuck_running, 1);
        assert_eq!(report.not_started, 1);
    }

    #[test]
    fn test_record_admin_stats() {
        let (conn, run_id) = new_in_memory_db();

        let stats = AdminStatsResponse {
            job_queue: AdminJobQueue {
                ready: 10,
                running: 5,
                scheduled: 3,
                total: 18,
            },
            game_counts: AdminGameCounts {
                waiting: 8,
                running: 4,
                finished: 100,
                total: 112,
            },
            games_created: AdminTimeWindow {
                last_hour: 50,
                last_24h: 200,
                last_7d: 500,
            },
            games_finished: AdminTimeWindow {
                last_hour: 45,
                last_24h: 190,
                last_7d: 490,
            },
            avg_game_duration_secs: Some(12.5),
            recent_errors: vec![],
        };
        let raw_json = serde_json::to_string(&stats).unwrap();

        CompletionDb::record_admin_stats_sync(&conn, run_id, "load", &stats, &raw_json).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM admin_stats_snapshots WHERE run_id = ?1",
                params![run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let phase: String = conn
            .query_row(
                "SELECT phase FROM admin_stats_snapshots WHERE run_id = ?1",
                params![run_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "load");
    }

    #[test]
    fn test_admin_stats_summary_in_report() {
        let (conn, run_id) = new_in_memory_db();

        let stats = AdminStatsResponse {
            job_queue: AdminJobQueue {
                ready: 10,
                running: 5,
                scheduled: 3,
                total: 18,
            },
            game_counts: AdminGameCounts {
                waiting: 8,
                running: 4,
                finished: 100,
                total: 112,
            },
            games_created: AdminTimeWindow {
                last_hour: 50,
                last_24h: 200,
                last_7d: 500,
            },
            games_finished: AdminTimeWindow {
                last_hour: 45,
                last_24h: 190,
                last_7d: 490,
            },
            avg_game_duration_secs: Some(12.5),
            recent_errors: vec![],
        };
        let raw_json = serde_json::to_string(&stats).unwrap();
        CompletionDb::record_admin_stats_sync(&conn, run_id, "load", &stats, &raw_json).unwrap();

        // Add a second snapshot with higher peaks
        let stats2 = AdminStatsResponse {
            job_queue: AdminJobQueue {
                ready: 20,
                running: 8,
                scheduled: 1,
                total: 29,
            },
            game_counts: AdminGameCounts {
                waiting: 15,
                running: 6,
                finished: 110,
                total: 131,
            },
            games_created: AdminTimeWindow {
                last_hour: 60,
                last_24h: 210,
                last_7d: 510,
            },
            games_finished: AdminTimeWindow {
                last_hour: 55,
                last_24h: 200,
                last_7d: 500,
            },
            avg_game_duration_secs: Some(11.0),
            recent_errors: vec![],
        };
        let raw_json2 = serde_json::to_string(&stats2).unwrap();
        CompletionDb::record_admin_stats_sync(&conn, run_id, "polling", &stats2, &raw_json2)
            .unwrap();

        let report = CompletionDb::generate_report_sync(&conn, run_id, 5).unwrap();
        let summary = report.admin_stats_summary.unwrap();

        assert_eq!(summary.snapshot_count, 2);
        assert_eq!(summary.peak_jobs_ready, 20);
        assert_eq!(summary.peak_jobs_running, 8);
        assert_eq!(summary.peak_games_waiting, 15);
        assert_eq!(summary.peak_games_running, 6);
        // Final state from latest snapshot
        assert_eq!(summary.final_games_finished, 110);
        assert_eq!(summary.final_games_total, 131);
        assert!(!summary.has_errors);
    }

    #[test]
    fn test_report_without_admin_stats() {
        let (conn, run_id) = new_in_memory_db();
        let report = CompletionDb::generate_report_sync(&conn, run_id, 5).unwrap();
        assert!(report.admin_stats_summary.is_none());
    }
}
