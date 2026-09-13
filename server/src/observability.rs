//! Arena's operational contract: one process identity, meaningful named
//! metrics, conservative thresholds, and a dashboard declared at boot.
use crate::{
    config::{AppConfig, FeatureFlags},
    state::AppState,
};
use cja::eyes_manifest::{AppManifest, ExpectedProcessRole, HttpMonitor};
use eyes_subscriber::{
    AggregateFunction as Agg, DashboardItem as Item, DashboardSection as Section,
    MetricThresholdBuilder, NamedDashboard, NamedMetric, NamedMetricBuilder as Metric,
    ProcessHeartbeat, ProcessHeartbeatConfig, ProcessHeartbeatHandle, ProcessIdentity,
    ThresholdComparison::Above,
};

pub const ROLE: &str = "arena";

pub fn manifest(
    registry: &cja::cron::CronRegistry<AppState>,
    identity: ProcessIdentity,
    features: FeatureFlags,
) -> Result<AppManifest, String> {
    let mut manifest = cja::eyes_manifest::build_boot_manifest::<crate::jobs::Jobs, AppState>(
        Some(env!("CARGO_PKG_VERSION")),
        option_env!("VERGEN_GIT_SHA"),
        features.cron.then_some(registry),
    )
    .base_url(crate::config::ARENA_PUBLIC_BASE_URL)
    .process(identity)
    .expected_process_roles(vec![ExpectedProcessRole::new(ROLE).min_instances(1)])
    .monitors(if features.server {
        vec![HttpMonitor::new("health", "/health")]
    } else {
        vec![]
    });
    if features.jobs {
        manifest = manifest.critical_jobs(
            vec![
                "GameRunnerJob",
                "RunMatchJob",
                "RunTournamentRoundJob",
                "UpdateTournamentStatusJob",
                "LeaderboardRatingUpdateJob",
                "BackupSingleGameJob",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        );
    } else {
        manifest.jobs.clear();
    }
    let metrics = metrics()?;
    let thresholds = [
        (
            "game-queue-delay",
            "game.queue_wait.p95",
            "Game queue delay",
            10_000.0,
            30_000.0,
        ),
        (
            "turn-storage-latency",
            "game.db_write.p95",
            "Turn persistence latency",
            1_000.0,
            3_000.0,
        ),
    ]
    .into_iter()
    .map(|(id, metric, description, warning, critical)| {
        MetricThresholdBuilder::new(id, metric)
            .description(description)
            .window_seconds(300)
            .evaluation_interval_seconds(60)
            .evaluation_delay_seconds(30)
            .failure_threshold(3)
            .recovery_threshold(2)
            .warning(Above, warning)
            .critical(Above, critical)
            .build()
    })
    .collect::<Result<Vec<_>, _>>()?;
    Ok(manifest
        .metrics(metrics)
        .metric_thresholds(thresholds)
        .dashboards(vec![dashboard()?]))
}

fn count(id: &str, title: &str, path: &str, value: &str) -> Result<Metric, String> {
    Metric::new(id, Agg::Count, None)?
        .display_name(title)
        .filter_eq(path, value)
}

fn timing(id: &str, title: &str, kind: &str) -> Result<NamedMetric, String> {
    Metric::new(id, Agg::P95, Some("fields.duration_ms"))?
        .filter_eq("fields.metric_type", kind)?
        .filter_numeric("fields.duration_ms")?
        .display_name(title)
        .unit("ms")
        .build()
}

fn metrics() -> Result<Vec<NamedMetric>, String> {
    Ok(vec![
        count(
            "http.requests",
            "HTTP requests",
            "semantic_kind",
            "http.request",
        )?
        .unit("requests")
        .build()?,
        count(
            "http.requests.series",
            "Request volume",
            "semantic_kind",
            "http.request",
        )?
        .unit("requests")
        .time_bucket(300)
        .build()?,
        Metric::new("http.latency.p95", Agg::P95, Some("duration"))?
            .filter_eq("semantic_kind", "http.request")?
            .display_name("HTTP latency · p95")
            .unit("µs")
            .build()?,
        Metric::new("http.latency.series", Agg::P95, Some("duration"))?
            .filter_eq("semantic_kind", "http.request")?
            .display_name("HTTP latency · p95")
            .unit("µs")
            .time_bucket(300)
            .build()?,
        count(
            "http.routes",
            "Requests by route",
            "semantic_kind",
            "http.request",
        )?
        .group_by("fields[\"http.route\"]")?
        .unit("requests")
        .build()?,
        count("errors", "Error events", "level", "ERROR")?
            .unit("events")
            .build()?,
        count("errors.series", "Error events", "level", "ERROR")?
            .unit("events")
            .time_bucket(300)
            .build()?,
        count(
            "jobs.runs",
            "Job attempts by type",
            "semantic_kind",
            "job.run",
        )?
        .group_by("fields[\"job.name\"]")?
        .unit("attempts")
        .build()?,
        count(
            "jobs.failures",
            "Job failure events",
            "target",
            "cja::jobs::worker",
        )?
        .filter_eq("level", "ERROR")?
        .unit("events")
        .build()?,
        count(
            "games.completed",
            "Games completed",
            "fields.event_type",
            "game_completed",
        )?
        .unit("games")
        .build()?,
        count(
            "games.series",
            "Games completed",
            "fields.event_type",
            "game_completed",
        )?
        .unit("games")
        .time_bucket(300)
        .build()?,
        timing("game.queue_wait.p95", "Game queue wait · p95", "queue_wait")?,
        timing(
            "game.db_write.p95",
            "Turn persistence · p95",
            "db_write_latency",
        )?,
        Metric::new("game.overhead.p95", Agg::P95, Some("fields.duration_ms"))?
            .filter_eq("fields.metric_type", "processing_overhead")?
            .filter_eq("fields.timing_basis", "elapsed_move_wait")?
            .filter_numeric("fields.duration_ms")?
            .display_name("Game processing overhead · p95")
            .unit("ms")
            .build()?,
    ])
}

fn dashboard() -> Result<NamedDashboard, String> {
    Ok(NamedDashboard::new("arena-operations", "Arena operations")?
        .description("Requests, games, workers, and the database work between turns.")
        .default_range_seconds(3600)
        .section(
            Section::new()
                .title("At a glance")
                .item(Item::stat("http.requests"))
                .item(Item::stat("games.completed"))
                .item(Item::stat("http.latency.p95"))
                .item(Item::stat("errors")),
        )
        .section(
            Section::new()
                .title("Traffic & errors")
                .item(Item::time_series("http.requests.series"))
                .item(Item::time_series("http.latency.series"))
                .item(Item::time_series("errors.series"))
                .item(Item::table("http.routes")),
        )
        .section(
            Section::new()
                .title("Game pipeline")
                .item(Item::time_series("games.series"))
                .item(Item::stat("game.queue_wait.p95"))
                .item(Item::stat("game.db_write.p95"))
                .item(Item::stat("game.overhead.p95")),
        )
        .section(
            Section::new()
                .title("Background work")
                .item(Item::health_summary())
                .item(Item::table("jobs.runs"))
                .item(Item::stat("jobs.failures")),
        ))
}

/// Publishing and heartbeats are best effort: Eyes must not prevent Arena
/// from serving traffic. Wait for manifest acceptance before the first beat.
pub async fn start(config: &AppConfig, manifest: &AppManifest) -> Option<ProcessHeartbeatHandle> {
    let eyes = config.eyes.as_ref()?;
    let result = async {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            eyes_subscriber::send_manifest_from_env(manifest),
        )
        .await??;
        let base = std::env::var("EYES_URL")
            .unwrap_or_else(|_| "https://eyes.coreyja.com".to_owned())
            .parse()?;
        let config =
            ProcessHeartbeatConfig::from_manifest(base, eyes.org_id, eyes.app_id, manifest)?;
        Ok::<_, color_eyre::Report>(ProcessHeartbeat::spawn(config)?)
    }
    .await;
    match result {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Eyes process registration failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyes_subscriber::MetricResultShape;
    use std::collections::HashMap;

    #[test]
    fn dashboard_items_resolve_to_metrics_of_the_right_shape() {
        let manifest = manifest(
            &crate::cron::cron_registry(),
            ProcessIdentity::new(ROLE),
            FeatureFlags {
                server: true,
                jobs: true,
                cron: true,
            },
        )
        .unwrap();
        let metrics: HashMap<_, _> = manifest
            .metrics
            .as_ref()
            .unwrap()
            .iter()
            .map(|metric| (metric.id.as_str(), metric))
            .collect();
        for dashboard in manifest.dashboards.as_ref().unwrap() {
            for section in &dashboard.sections {
                for item in &section.items {
                    let (id, expected) = match item {
                        Item::Stat { query_id, .. } => (query_id, MetricResultShape::Scalar),
                        Item::TimeSeries { query_id, .. } => {
                            (query_id, MetricResultShape::TimeSeries)
                        }
                        Item::Table { query_id, .. } => (query_id, MetricResultShape::Table),
                        Item::HealthSummary { .. } | Item::Links { .. } => continue,
                    };
                    assert_eq!(metrics[id.as_str()].result_shape, expected, "{id}");
                }
            }
        }
        for threshold in manifest.metric_thresholds.as_ref().unwrap() {
            assert_eq!(
                metrics[threshold.metric_id.as_str()].unit.as_deref(),
                Some("ms")
            );
            assert!(
                threshold.critical.as_ref().unwrap().threshold
                    > threshold.warning.as_ref().unwrap().threshold
            );
        }
        assert!(!manifest.crons.is_empty());
        for name in &manifest.critical_jobs {
            assert!(
                manifest.jobs.contains(name),
                "{name} must be a registered job"
            );
        }
    }

    #[test]
    fn disabled_workers_do_not_declare_crons_or_critical_jobs() {
        let identity = ProcessIdentity::new(ROLE);
        let id = identity.instance_id();
        let manifest = manifest(
            &crate::cron::cron_registry(),
            identity,
            FeatureFlags {
                server: true,
                jobs: false,
                cron: false,
            },
        )
        .unwrap();
        assert!(manifest.jobs.is_empty());
        assert!(manifest.crons.is_empty());
        assert!(manifest.critical_jobs.is_empty());
        assert_eq!(manifest.process_instance_id, Some(id));
        assert_eq!(manifest.process_role.as_deref(), Some(ROLE));
        let config = ProcessHeartbeatConfig::from_manifest(
            "http://127.0.0.1".parse().unwrap(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            &manifest,
        )
        .unwrap();
        assert_eq!(config.identity.instance_id(), id);
    }

    #[tokio::test]
    async fn manifest_wire_contract_keeps_critical_jobs_and_process_identity() {
        let manifest = manifest(
            &crate::cron::cron_registry(),
            ProcessIdentity::new(ROLE),
            FeatureFlags {
                server: true,
                jobs: true,
                cron: true,
            },
        )
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let router = axum::Router::new().route(
            "/api/orgs/{org}/apps/{app}/manifest",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let sender = sender.clone();
                async move {
                    sender.send(body).await.unwrap();
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        eyes_subscriber::send_manifest(
            &format!("http://{addr}"),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            &manifest,
            None,
        )
        .await
        .unwrap();
        let body = receiver.recv().await.unwrap();
        server.abort();
        assert_eq!(body["manifest_version"], 2);
        assert_eq!(
            body["process_instance_id"],
            manifest.process_instance_id.unwrap().to_string()
        );
        assert_eq!(
            body["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|job| job["critical"] == true)
                .count(),
            manifest.critical_jobs.len()
        );
        assert_eq!(
            body["metrics"].as_array().unwrap().len(),
            manifest.metrics.as_ref().unwrap().len()
        );
        // Optional artifact for a local server acceptance check, using the
        // actual subscriber wire format rather than a second serializer.
        if let Ok(path) = std::env::var("EYES_MANIFEST_FIXTURE_PATH") {
            std::fs::write(path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        }
    }
}
