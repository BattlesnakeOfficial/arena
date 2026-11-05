//! Arena's operational contract: one process identity, meaningful named
//! metrics, conservative thresholds, and a dashboard declared at boot.
use crate::{
    config::{AppConfig, FeatureFlags},
    state::AppState,
};
use cja::eyes_manifest::{AppManifest, ExpectedProcessRole, HttpMonitor};
use eyes_subscriber::{
    AggregateFunction as Agg, DashboardItem as Item, DashboardSection as Section,
    MetricResultShape, MetricThresholdBuilder, MetricValueType, NamedDashboard, NamedMetric,
    NamedMetricBuilder as Metric, ProcessHeartbeat, ProcessHeartbeatConfig, ProcessHeartbeatHandle,
    ProcessIdentity, RatioMetricBuilder, RetentionPolicy, ThresholdComparison::Above,
};

pub const ROLE: &str = "arena";

/// Days Eyes keeps arena's DEBUG telemetry in Postgres; its lake keeps it after
/// that. Every other level keeps Eyes' server default.
pub const DEBUG_RETENTION_DAYS: u32 = 2;

/// Trailing-window constants for the product dashboards' fixed tiles and
/// charts (DEV-2062). All within the server's 1..=31_622_400 s item bound.
const DAY: u64 = 86_400;
const WEEK: u64 = 604_800;
const FOUR_WEEKS: u64 = 2_419_200;
const NINETY_DAYS: u64 = 7_776_000; // 90 days
const THREE_HUNDRED_SIXTY_FOUR_DAYS: u64 = 31_449_600; // 364 days

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
    .retention(RetentionPolicy::new().level(tracing::Level::DEBUG, DEBUG_RETENTION_DAYS))
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
        .dashboards(vec![
            dashboard()?,
            product_dashboard()?,
            product_history_dashboard()?,
        ]))
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

/// Distinct users with an authenticated request, counted on cja's
/// `finished processing request` event. Target AND message, in that order:
/// the target alone also matches `server.request` span rows, and
/// `fields.user_id` alone also matches `snake.lifecycle` spans
/// (DEV-2076 fallback ruling). Stage order is filter call order.
fn active_request_users(id: &str, title: &str) -> Result<Metric, String> {
    Ok(Metric::new(id, Agg::CountDistinct, Some("fields.user_id"))?
        .display_name(title)
        .filter_eq("target", "cja::server::trace")?
        .filter_eq("message", "finished processing request")?
        .unit("users"))
}

/// Distinct owners/snakes behind a real `/end` call. `endpoint` is a
/// span-creation field on `snake.lifecycle` (DEV-2061) — open row, no
/// close-row merge needed. Name first, then the endpoint field.
fn active_lifecycle(id: &str, title: &str, value: &str, unit: &str) -> Result<Metric, String> {
    Ok(Metric::new(id, Agg::CountDistinct, Some(value))?
        .display_name(title)
        .filter_eq("name", "snake.lifecycle")?
        .filter_eq("fields.endpoint", "end")?
        .unit(unit))
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
        count(
            "moderation.decisions",
            "Moderation decisions",
            "fields.event_type",
            "moderation_decision",
        )?
        .unit("decisions")
        .build()?,
        count(
            "moderation.decisions.by_field",
            "Moderation decisions by field",
            "fields.event_type",
            "moderation_decision",
        )?
        .group_by("fields[\"field_kind\"]")?
        .unit("decisions")
        .build()?,
        Metric::new(
            "moderation.latency.p95",
            Agg::P95,
            Some("fields.latency_ms"),
        )?
        .filter_eq("fields.event_type", "moderation_decision")?
        .filter_numeric("fields.latency_ms")?
        .display_name("Moderation latency · p95")
        .unit("ms")
        .build()?,
        count(
            "moderation.shouts.screened",
            "Games screened for shouts",
            "fields.event_type",
            "shout_screening",
        )?
        .unit("games")
        .build()?,
        Metric::new(
            "moderation.shouts.suppressed",
            Agg::Sum,
            Some("fields.suppressed_count"),
        )?
        .filter_eq("fields.event_type", "shout_screening")?
        .filter_numeric("fields.suppressed_count")?
        .display_name("Shouts suppressed")
        .unit("shouts")
        .build()?,
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
        count(
            "product.games.played",
            "Games played",
            "fields.event_type",
            "game_completed",
        )?
        .unit("games")
        .build()?,
        count(
            "product.games.daily",
            "Games played",
            "fields.event_type",
            "game_completed",
        )?
        .unit("games")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.games.ladder.daily",
            "Games played (ladder)",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "ladder")?
        .unit("games")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.games.custom.daily",
            "Games played (custom)",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "custom")?
        .unit("games")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.games.tournament.daily",
            "Games played (tournament)",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "tournament")?
        .unit("games")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.games.user_created",
            "User-created games",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "custom")?
        .unit("games")
        .build()?,
        count(
            "product.play_claims.daily",
            "Play claims",
            "fields.event_type",
            "play_claim_succeeded",
        )?
        .unit("claims")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.new_users.daily",
            "New users",
            "fields.event_type",
            "user_authenticated",
        )?
        .filter_eq("fields.new_user", true)?
        .unit("users")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.new_snakes.daily",
            "New snakes",
            "fields.event_type",
            "battlesnake_created",
        )?
        .unit("snakes")
        .time_bucket(86_400)
        .build()?,
        // DEV-2062: active users/owners/snakes, per-source games, weekly
        // variants, and the ratios over them. Every aggregate is
        // count_distinct or count over events that exist today; Arena
        // computes nothing itself.
        active_request_users("product.users.active", "Users with an authenticated request")?
            .description(
                "Distinct users who made an authenticated request (session sign-in or API token), counted on the request-completion event.",
            )
            .build()?,
        active_request_users(
            "product.users.active.daily",
            "Users with an authenticated request, per day",
        )?
        .time_bucket(DAY)
        .build()?,
        active_request_users(
            "product.users.active.weekly",
            "Users with an authenticated request, per week",
        )?
        .time_bucket_weeks(1)
        .build()?,
        active_request_users(
            "product.users.active.month",
            "Users with an authenticated request (28-day side)",
        )?
        .build()?,
        active_lifecycle(
            "product.owners.active",
            "Owners whose snake played",
            "fields.user_id",
            "users",
        )?
        .description(
            "Distinct owners whose snake played a game, counted on the snake /end call (snake.lifecycle span, endpoint end).",
        )
        .build()?,
        active_lifecycle(
            "product.owners.active.daily",
            "Owners whose snake played, per day",
            "fields.user_id",
            "users",
        )?
        .time_bucket(DAY)
        .build()?,
        active_lifecycle(
            "product.owners.active.weekly",
            "Owners whose snake played, per week",
            "fields.user_id",
            "users",
        )?
        .time_bucket_weeks(1)
        .build()?,
        active_lifecycle(
            "product.owners.active.month",
            "Owners whose snake played (28-day side)",
            "fields.user_id",
            "users",
        )?
        .build()?,
        active_lifecycle(
            "product.snakes.active",
            "Snakes that played a game",
            "fields.battlesnake_id",
            "snakes",
        )?
        .description(
            "Distinct battlesnakes that played a game, counted on the snake /end call (snake.lifecycle span, endpoint end).",
        )
        .build()?,
        active_lifecycle(
            "product.snakes.active.weekly",
            "Snakes that played a game, per week",
            "fields.battlesnake_id",
            "snakes",
        )?
        .time_bucket_weeks(1)
        .build()?,
        count(
            "product.games.ladder",
            "Games played (ladder)",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "ladder")?
        .unit("games")
        .build()?,
        count(
            "product.games.tournament",
            "Games played (tournament)",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "tournament")?
        .unit("games")
        .build()?,
        count(
            "product.games.ladder.weekly",
            "Games played (ladder), per week",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "ladder")?
        .unit("games")
        .time_bucket_weeks(1)
        .build()?,
        count(
            "product.games.custom.weekly",
            "Games played (custom), per week",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "custom")?
        .unit("games")
        .time_bucket_weeks(1)
        .build()?,
        count(
            "product.games.tournament.weekly",
            "Games played (tournament), per week",
            "fields.event_type",
            "game_completed",
        )?
        .filter_eq("fields.source", "tournament")?
        .unit("games")
        .time_bucket_weeks(1)
        .build()?,
        count(
            "product.new_users.weekly",
            "New users, per week",
            "fields.event_type",
            "user_authenticated",
        )?
        .filter_eq("fields.new_user", true)?
        .unit("users")
        .time_bucket_weeks(1)
        .build()?,
        count(
            "product.new_snakes.weekly",
            "New snakes, per week",
            "fields.event_type",
            "battlesnake_created",
        )?
        .unit("snakes")
        .time_bucket_weeks(1)
        .build()?,
        RatioMetricBuilder::new(
            "product.users.active.stickiness.daily",
            "product.users.active",
            "product.users.active.month",
        )
        .numerator_window_seconds(DAY)
        .denominator_window_seconds(FOUR_WEEKS)
        .as_percentage()
        .display_name("Users with an authenticated request: last 1 day as % of last 28 days")
        .description(
            "Distinct users with an authenticated request in the last 1 day as a percentage of the last 28 days.",
        )
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        RatioMetricBuilder::new(
            "product.users.active.stickiness.weekly",
            "product.users.active",
            "product.users.active.month",
        )
        .numerator_window_seconds(WEEK)
        .denominator_window_seconds(FOUR_WEEKS)
        .as_percentage()
        .display_name("Users with an authenticated request: last 7 days as % of last 28 days")
        .description(
            "Distinct users with an authenticated request in the last 7 days as a percentage of the last 28 days.",
        )
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        RatioMetricBuilder::new(
            "product.owners.active.stickiness.daily",
            "product.owners.active",
            "product.owners.active.month",
        )
        .numerator_window_seconds(DAY)
        .denominator_window_seconds(FOUR_WEEKS)
        .as_percentage()
        .display_name("Owners whose snake played: last 1 day as % of last 28 days")
        .description(
            "Distinct owners whose snake played in the last 1 day as a percentage of the last 28 days.",
        )
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        RatioMetricBuilder::new(
            "product.owners.active.stickiness.weekly",
            "product.owners.active",
            "product.owners.active.month",
        )
        .numerator_window_seconds(WEEK)
        .denominator_window_seconds(FOUR_WEEKS)
        .as_percentage()
        .display_name("Owners whose snake played: last 7 days as % of last 28 days")
        .description(
            "Distinct owners whose snake played in the last 7 days as a percentage of the last 28 days.",
        )
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        RatioMetricBuilder::new(
            "product.games.ladder.share",
            "product.games.ladder",
            "product.games.played",
        )
        .as_percentage()
        .display_name("Ladder share of games played")
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        RatioMetricBuilder::new(
            "product.games.user_created.share",
            "product.games.user_created",
            "product.games.played",
        )
        .as_percentage()
        .display_name("User-created share of games played")
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        RatioMetricBuilder::new(
            "product.games.tournament.share",
            "product.games.tournament",
            "product.games.played",
        )
        .as_percentage()
        .display_name("Tournament share of games played")
        .build(
            MetricResultShape::Scalar,
            MetricValueType::Int,
            MetricResultShape::Scalar,
            MetricValueType::Int,
        )?,
        count(
            "product.achievements.earned",
            "Achievements earned",
            "fields.event_type",
            "achievement_earned",
        )?
        .unit("earns")
        .build()?,
        count(
            "product.achievements.earned.daily",
            "Achievements earned",
            "fields.event_type",
            "achievement_earned",
        )?
        .unit("earns")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.achievements.firsts.daily",
            "First achievement earns",
            "fields.event_type",
            "achievement_earned",
        )?
        .filter_eq("fields.first", true)?
        .unit("earns")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.achievements.by_slug",
            "Achievements earned by achievement",
            "fields.event_type",
            "achievement_earned",
        )?
        .group_by("fields[\"slug\"]")?
        .unit("earns")
        .build()?,
        count(
            "product.shares.page_views",
            "Achievement share page views",
            "fields.event_type",
            "share_page_view",
        )?
        .unit("views")
        .build()?,
        count(
            "product.shares.page_views.daily",
            "Achievement share page views",
            "fields.event_type",
            "share_page_view",
        )?
        .unit("views")
        .time_bucket(86_400)
        .build()?,
        count(
            "product.shares.page_views.by_referrer",
            "Achievement share page views by referrer",
            "fields.event_type",
            "share_page_view",
        )?
        .group_by("fields[\"referrer\"]")?
        .unit("views")
        .build()?,
        count(
            "product.shares.actions",
            "Achievement share actions",
            "fields.event_type",
            "share_action",
        )?
        .group_by("fields[\"channel\"]")?
        .unit("actions")
        .build()?,
        count(
            "product.shares.cta_clicks",
            "Achievement share CTA clicks",
            "fields.event_type",
            "share_cta_click",
        )?
        .group_by("fields[\"cta\"]")?
        .unit("clicks")
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
                .title("Moderation")
                .item(Item::stat("moderation.decisions"))
                .item(Item::table("moderation.decisions.by_field"))
                .item(Item::stat("moderation.latency.p95")),
        )
        .section(
            Section::new()
                .title("Background work")
                .item(Item::health_summary())
                .item(Item::table("jobs.runs"))
                .item(Item::stat("jobs.failures")),
        ))
}

fn product_dashboard() -> Result<NamedDashboard, String> {
    Ok(NamedDashboard::new("arena-product", "Arena product")?
        .description("Games played, active users and snakes, play claims, signups, new snakes, achievements earned, and achievement shares. Long-window charts: arena-product-history.")
        .default_range_seconds(30 * 24 * 60 * 60)
        .section(
            Section::new()
                .title("At a glance")
                .item(Item::stat("product.games.played"))
                .item(Item::stat("product.games.user_created")),
        )
        .section(
            Section::new()
                .title("Active users, owners, and snakes")
                .item(
                    Item::stat("product.users.active")
                        .window_seconds(DAY)
                        .label("Users with an authenticated request · last 1 day"),
                )
                .item(
                    Item::stat("product.users.active")
                        .window_seconds(WEEK)
                        .label("Users with an authenticated request · last 7 days"),
                )
                .item(
                    Item::stat("product.users.active")
                        .window_seconds(FOUR_WEEKS)
                        .label("Users with an authenticated request · last 28 days"),
                )
                .item(
                    Item::stat("product.owners.active")
                        .window_seconds(DAY)
                        .label("Owners whose snake played · last 1 day"),
                )
                .item(
                    Item::stat("product.owners.active")
                        .window_seconds(WEEK)
                        .label("Owners whose snake played · last 7 days"),
                )
                .item(
                    Item::stat("product.owners.active")
                        .window_seconds(FOUR_WEEKS)
                        .label("Owners whose snake played · last 28 days"),
                )
                .item(
                    Item::stat("product.snakes.active")
                        .window_seconds(WEEK)
                        .label("Snakes that played a game · last 7 days"),
                )
                .item(
                    Item::stat("product.games.played")
                        .window_seconds(WEEK)
                        .label("Games played · last 7 days"),
                ),
        )
        .section(
            Section::new()
                .title("Stickiness (share of 28-day actives)")
                .item(Item::stat("product.users.active.stickiness.daily"))
                .item(Item::stat("product.users.active.stickiness.weekly"))
                .item(Item::stat("product.owners.active.stickiness.daily"))
                .item(Item::stat("product.owners.active.stickiness.weekly")),
        )
        .section(
            Section::new()
                .title("Games played")
                .item(Item::time_series("product.games.daily"))
                .item(Item::time_series("product.games.ladder.daily"))
                .item(Item::time_series("product.games.custom.daily"))
                .item(Item::time_series("product.games.tournament.daily")),
        )
        .section(
            Section::new()
                .title("Source share of games (last 7 days)")
                .item(
                    Item::stat("product.games.ladder.share")
                        .window_seconds(WEEK)
                        .label("Ladder share of games · last 7 days"),
                )
                .item(
                    Item::stat("product.games.user_created.share")
                        .window_seconds(WEEK)
                        .label("User-created share of games · last 7 days"),
                )
                .item(
                    Item::stat("product.games.tournament.share")
                        .window_seconds(WEEK)
                        .label("Tournament share of games · last 7 days"),
                ),
        )
        .section(
            Section::new()
                .title("Growth")
                .item(Item::time_series("product.play_claims.daily"))
                .item(Item::time_series("product.new_users.daily"))
                .item(Item::time_series("product.new_snakes.daily")),
        )
        .section(
            Section::new()
                .title("Achievements")
                .item(Item::stat("product.achievements.earned"))
                .item(Item::time_series("product.achievements.earned.daily"))
                .item(Item::time_series("product.achievements.firsts.daily"))
                .item(Item::table("product.achievements.by_slug")),
        )
        .section(
            Section::new()
                .title("Sharing")
                .item(Item::stat("product.shares.page_views"))
                .item(Item::time_series("product.shares.page_views.daily"))
                .item(Item::table("product.shares.page_views.by_referrer"))
                .item(Item::table("product.shares.actions"))
                .item(Item::table("product.shares.cta_clicks")),
        ))
}

/// The long-window charts (DEV-2062): 90-day daily series and 364-day
/// Monday-week series, on their own page so `arena-product` never pays for
/// year-long lake scans. Every window clamps to the lake's history start
/// until coverage grows (DEV-2059).
fn product_history_dashboard() -> Result<NamedDashboard, String> {
    Ok(NamedDashboard::new("arena-product-history", "Arena product history")?
        .description("Long-window engagement, games, and growth charts. Each card's window starts at the lake's history until coverage grows; every clamped card shows the date it starts from.")
        .default_range_seconds(31_622_400) // 366 days, the post-DEV-2059 bound
        .section(
            Section::new()
                .title("Active users, owners, and snakes over time")
                .item(
                    Item::time_series("product.users.active.daily")
                        .window_seconds(NINETY_DAYS),
                )
                .item(
                    Item::time_series("product.owners.active.daily")
                        .window_seconds(NINETY_DAYS),
                )
                .item(
                    Item::time_series("product.users.active.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                )
                .item(
                    Item::time_series("product.owners.active.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                )
                .item(
                    Item::time_series("product.snakes.active.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                ),
        )
        .section(
            Section::new()
                .title("Games played by source")
                .item(
                    Item::time_series("product.games.ladder.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                )
                .item(
                    Item::time_series("product.games.custom.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                )
                .item(
                    Item::time_series("product.games.tournament.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                ),
        )
        .section(
            Section::new()
                .title("Growth")
                .item(
                    Item::time_series("product.new_users.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                )
                .item(
                    Item::time_series("product.new_snakes.weekly")
                        .window_seconds(THREE_HUNDRED_SIXTY_FOUR_DAYS),
                ),
        ))
}

/// Publishing and heartbeats are best effort: Eyes must not prevent Arena
/// from serving traffic. Wait for manifest acceptance before the first beat.
pub async fn start(
    config: &AppConfig,
    manifest: &AppManifest,
    token: Option<&str>,
) -> Option<ProcessHeartbeatHandle> {
    let eyes = config.eyes.as_ref()?;
    let base_url =
        std::env::var("EYES_URL").unwrap_or_else(|_| "https://eyes.coreyja.com".to_owned());
    match register(&base_url, eyes.org_id, eyes.app_id, manifest, token).await {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "Eyes process registration failed");
            None
        }
    }
}

/// Publish the boot manifest and start the process heartbeat against one Eyes
/// server. `token` is the resolved ingest token (setec first, then
/// `EYES_TOKEN`); `None` sends no Authorization header. The token is
/// force-assigned onto the heartbeat config — `from_manifest` seeds it from
/// ambient `EYES_TOKEN`, which must not resurface a resolved `None`.
async fn register(
    base_url: &str,
    org_id: uuid::Uuid,
    app_id: uuid::Uuid,
    manifest: &AppManifest,
    token: Option<&str>,
) -> color_eyre::Result<ProcessHeartbeatHandle> {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        eyes_subscriber::send_manifest(base_url, org_id, app_id, manifest, token),
    )
    .await??;
    let mut heartbeat =
        ProcessHeartbeatConfig::from_manifest(base_url.parse()?, org_id, app_id, manifest)?;
    heartbeat.token = token.map(str::to_owned);
    Ok(ProcessHeartbeat::spawn(heartbeat)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eyes_subscriber::MetricResultShape;
    use std::collections::HashMap;

    #[test]
    fn dashboard_items_resolve_to_metrics_of_the_right_shape() {
        let manifest = manifest(
            &crate::cron::cron_registry(&AppConfig::test_default()),
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
        let ids: Vec<_> = manifest
            .dashboards
            .as_ref()
            .unwrap()
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["arena-operations", "arena-product", "arena-product-history"]
        );
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
    fn product_metrics_declare_windows_ratios_and_labels() {
        let manifest = manifest(
            &crate::cron::cron_registry(&AppConfig::test_default()),
            ProcessIdentity::new(ROLE),
            FeatureFlags {
                server: true,
                jobs: true,
                cron: true,
            },
        )
        .unwrap();
        // Assert against the serialized wire body, not the in-memory types:
        // this pins exactly what ships to Eyes at boot.
        let wire = serde_json::to_value(&manifest).unwrap();
        let metrics: HashMap<_, _> = manifest
            .metrics
            .as_ref()
            .unwrap()
            .iter()
            .map(|metric| (metric.id.as_str(), metric))
            .collect();
        let metric_wire: HashMap<_, _> = wire["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|metric| (metric["id"].as_str().unwrap().to_owned(), metric))
            .collect();

        // The 24 new ids exist, enumerated explicitly (other tasks add
        // metrics to this file, so a total count would false-fail there).
        const NEW_IDS: &[&str] = &[
            "product.users.active",
            "product.users.active.daily",
            "product.users.active.weekly",
            "product.users.active.month",
            "product.owners.active",
            "product.owners.active.daily",
            "product.owners.active.weekly",
            "product.owners.active.month",
            "product.snakes.active",
            "product.snakes.active.weekly",
            "product.games.ladder",
            "product.games.tournament",
            "product.games.ladder.weekly",
            "product.games.custom.weekly",
            "product.games.tournament.weekly",
            "product.new_users.weekly",
            "product.new_snakes.weekly",
            "product.users.active.stickiness.daily",
            "product.users.active.stickiness.weekly",
            "product.owners.active.stickiness.daily",
            "product.owners.active.stickiness.weekly",
            "product.games.ladder.share",
            "product.games.user_created.share",
            "product.games.tournament.share",
        ];
        for id in NEW_IDS {
            assert!(metrics.contains_key(id), "missing new metric {id}");
        }

        // Monday weeks pinned by origin, not by width: `.time_bucket(604_800)`
        // produces the same preferred_bucket_seconds with epoch
        // (Thursday) buckets and no third argument (DEV-2060).
        let weekly_ids = [
            "product.users.active.weekly",
            "product.owners.active.weekly",
            "product.snakes.active.weekly",
            "product.games.ladder.weekly",
            "product.games.custom.weekly",
            "product.games.tournament.weekly",
            "product.new_users.weekly",
            "product.new_snakes.weekly",
        ];
        for id in weekly_ids {
            assert_eq!(metrics[id].preferred_bucket_seconds, Some(604_800), "{id}");
            let stages = metric_wire[id]["query"]["query"]["stages"]
                .as_array()
                .unwrap_or_else(|| panic!("{id} has no stages"));
            let aggregate = stages
                .last()
                .unwrap_or_else(|| panic!("{id} has no aggregate stage"));
            assert_eq!(aggregate["stage"], "aggregate", "{id}");
            let groups = aggregate["groups"].as_array().unwrap();
            let bucket = groups
                .first()
                .unwrap_or_else(|| panic!("{id} aggregate has no bucket group"));
            assert_eq!(bucket["alias"], "bucket", "{id}");
            let expected_call = serde_json::json!({
                "type": "call",
                "function": "time_bucket",
                "args": [
                    {"type": "path", "path": {"root": "timestamp", "segments": []}},
                    {"type": "literal", "value": {"type": "duration", "value": 604_800_000_000u64}},
                    {"type": "literal", "value": {
                        "type": "duration",
                        "value": eyes_subscriber::eyes_query::MONDAY_ORIGIN_US
                    }},
                ],
            });
            assert_eq!(
                bucket["expr"], expected_call,
                "{id} Monday-origin time_bucket"
            );
        }

        // Filters pinned by stage order (stage order is evaluation order,
        // and the population each filter carves is load-bearing).
        let eq_stage = |path: &str, value: &str| {
            let (root, key) = match path.split_once('.') {
                Some((root, key)) => (root, Some(key)),
                None => (path, None),
            };
            let segments = key
                .map(|key| vec![serde_json::json!({"type": "key", "value": key})])
                .unwrap_or_default();
            serde_json::json!({
                "stage": "where",
                "predicate": {
                    "type": "binary",
                    "op": "eq",
                    "left": {"type": "path", "path": {"root": root, "segments": segments}},
                    "right": {"type": "literal", "value": {"type": "string", "value": value}},
                },
            })
        };
        let aggregate_stage = |argument_root: &str, argument_key: &str| {
            serde_json::json!({
                "stage": "aggregate",
                "aggregates": [{
                    "function": "count_distinct",
                    "argument": {
                        "type": "path",
                        "path": {"root": argument_root, "segments": [{"type": "key", "value": argument_key}]},
                    },
                    "alias": "value",
                }],
                "groups": [],
            })
        };
        let expected_stages = |filters: Vec<serde_json::Value>, aggregate: serde_json::Value| {
            let mut stages = filters;
            stages.push(aggregate);
            stages
        };
        let users_stages = expected_stages(
            vec![
                eq_stage("target", "cja::server::trace"),
                eq_stage("message", "finished processing request"),
            ],
            aggregate_stage("fields", "user_id"),
        );
        assert_eq!(
            metric_wire["product.users.active"]["query"]["query"]["stages"],
            serde_json::json!(users_stages),
            "users base definition stages"
        );
        let owners_stages = expected_stages(
            vec![
                eq_stage("name", "snake.lifecycle"),
                eq_stage("fields.endpoint", "end"),
            ],
            aggregate_stage("fields", "user_id"),
        );
        assert_eq!(
            metric_wire["product.owners.active"]["query"]["query"]["stages"],
            serde_json::json!(owners_stages),
            "owners base definition stages"
        );
        let snakes_stages = expected_stages(
            vec![
                eq_stage("name", "snake.lifecycle"),
                eq_stage("fields.endpoint", "end"),
            ],
            aggregate_stage("fields", "battlesnake_id"),
        );
        assert_eq!(
            metric_wire["product.snakes.active"]["query"]["query"]["stages"],
            serde_json::json!(snakes_stages),
            "snakes base definition stages"
        );

        // Ratios: stickiness declare per-side windows over the 28-day side;
        // source shares declare none (the item's window sizes both sides).
        let stickiness = [
            (
                "product.users.active.stickiness.daily",
                "product.users.active",
                DAY,
            ),
            (
                "product.users.active.stickiness.weekly",
                "product.users.active",
                WEEK,
            ),
            (
                "product.owners.active.stickiness.daily",
                "product.owners.active",
                DAY,
            ),
            (
                "product.owners.active.stickiness.weekly",
                "product.owners.active",
                WEEK,
            ),
        ];
        let month_sides = [
            (
                "product.users.active.stickiness.daily",
                "product.users.active.month",
            ),
            (
                "product.users.active.stickiness.weekly",
                "product.users.active.month",
            ),
            (
                "product.owners.active.stickiness.daily",
                "product.owners.active.month",
            ),
            (
                "product.owners.active.stickiness.weekly",
                "product.owners.active.month",
            ),
        ];
        let month_sides: HashMap<_, _> = month_sides.into_iter().collect();
        for (id, numerator, numerator_window) in stickiness {
            let ratio = metrics[id].ratio.as_ref().unwrap();
            assert_eq!(ratio.numerator, numerator, "{id}");
            assert_eq!(ratio.denominator, month_sides[id], "{id}");
            assert_eq!(
                ratio.numerator_window_seconds,
                Some(numerator_window),
                "{id}"
            );
            assert_eq!(ratio.denominator_window_seconds, Some(FOUR_WEEKS), "{id}");
            assert!(ratio.display_as_percentage, "{id}");
            assert!(metrics[id].query.is_none(), "{id}");
            assert_eq!(metric_wire[id]["value_type"], "float", "{id}");
        }
        let shares = [
            ("product.games.ladder.share", "product.games.ladder"),
            (
                "product.games.user_created.share",
                "product.games.user_created",
            ),
            ("product.games.tournament.share", "product.games.tournament"),
        ];
        for (id, numerator) in shares {
            let ratio = metrics[id].ratio.as_ref().unwrap();
            assert_eq!(ratio.numerator, numerator, "{id}");
            assert_eq!(ratio.denominator, "product.games.played", "{id}");
            assert_eq!(ratio.numerator_window_seconds, None, "{id}");
            assert_eq!(ratio.denominator_window_seconds, None, "{id}");
            assert!(ratio.display_as_percentage, "{id}");
            assert!(metrics[id].query.is_none(), "{id}");
            assert_eq!(metric_wire[id]["value_type"], "float", "{id}");
        }
        // Every ratio side is a declared plain metric, never a ratio itself.
        for id in stickiness
            .iter()
            .map(|(id, _, _)| *id)
            .chain(shares.iter().map(|(id, _)| *id))
        {
            for side in [
                &metrics[id].ratio.as_ref().unwrap().numerator,
                &metrics[id].ratio.as_ref().unwrap().denominator,
            ] {
                let side = metrics
                    .get(side.as_str())
                    .unwrap_or_else(|| panic!("{id} side {side} not declared"));
                assert!(
                    side.ratio.is_none(),
                    "{id} side {} is itself a ratio",
                    side.id
                );
            }
        }

        // Dashboards: fixed tiles carry exact windows and plain-word labels.
        let dashboards: HashMap<_, _> = manifest
            .dashboards
            .as_ref()
            .unwrap()
            .iter()
            .map(|d| (d.id.as_str(), d))
            .collect();
        let product = dashboards["arena-product"];
        fn find_section<'a>(dashboard: &'a NamedDashboard, title: &str) -> &'a Section {
            dashboard
                .sections
                .iter()
                .find(|s| s.title.as_deref() == Some(title))
                .unwrap_or_else(|| panic!("dashboard has no {title} section"))
        }
        fn item_windows(section: &Section) -> Vec<(&str, Option<u64>)> {
            section
                .items
                .iter()
                .map(|item| match item {
                    Item::Stat {
                        query_id,
                        window_seconds,
                        ..
                    }
                    | Item::TimeSeries {
                        query_id,
                        window_seconds,
                        ..
                    }
                    | Item::Table {
                        query_id,
                        window_seconds,
                        ..
                    } => (query_id.as_str(), *window_seconds),
                    _ => panic!("section item is not a query item"),
                })
                .collect()
        }
        let active = find_section(product, "Active users, owners, and snakes");
        assert_eq!(
            item_windows(active),
            vec![
                ("product.users.active", Some(DAY)),
                ("product.users.active", Some(WEEK)),
                ("product.users.active", Some(FOUR_WEEKS)),
                ("product.owners.active", Some(DAY)),
                ("product.owners.active", Some(WEEK)),
                ("product.owners.active", Some(FOUR_WEEKS)),
                ("product.snakes.active", Some(WEEK)),
                ("product.games.played", Some(WEEK)),
            ],
            "active tile windows"
        );
        let source_share = find_section(product, "Source share of games (last 7 days)");
        assert_eq!(
            item_windows(source_share),
            vec![
                ("product.games.ladder.share", Some(WEEK)),
                ("product.games.user_created.share", Some(WEEK)),
                ("product.games.tournament.share", Some(WEEK)),
            ],
            "source-share tile windows"
        );
        for item in active.items.iter().chain(source_share.items.iter()) {
            let label = match item {
                Item::Stat { label, .. } => label,
                _ => panic!("expected a stat item"),
            };
            assert!(
                label.as_deref().is_some_and(|l| !l.is_empty()),
                "tile label empty"
            );
        }
        let history = dashboards["arena-product-history"];
        let history_windows: Vec<(&str, Option<u64>)> = history
            .sections
            .iter()
            .flat_map(|s| item_windows(s))
            .collect();
        assert_eq!(
            history_windows,
            vec![
                ("product.users.active.daily", Some(NINETY_DAYS)),
                ("product.owners.active.daily", Some(NINETY_DAYS)),
                (
                    "product.users.active.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.owners.active.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.snakes.active.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.games.ladder.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.games.custom.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.games.tournament.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.new_users.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
                (
                    "product.new_snakes.weekly",
                    Some(THREE_HUNDRED_SIXTY_FOUR_DAYS)
                ),
            ],
            "history chart windows"
        );

        // Plain-words criterion: each definition says what it counts.
        assert!(
            metrics["product.users.active"]
                .display_name
                .as_deref()
                .is_some_and(|n| n.contains("authenticated request"))
        );
        assert!(
            metrics["product.owners.active"]
                .display_name
                .as_deref()
                .is_some_and(|n| n.contains("Owners whose snake played"))
        );
        assert!(
            metrics["product.snakes.active"]
                .display_name
                .as_deref()
                .is_some_and(|n| n.contains("Snakes that played"))
        );

        // Sections other tasks own keep main's item ids verbatim.
        let achievements = find_section(product, "Achievements");
        assert_eq!(
            item_windows(achievements)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec![
                "product.achievements.earned",
                "product.achievements.earned.daily",
                "product.achievements.firsts.daily",
                "product.achievements.by_slug",
            ]
        );
        let sharing = find_section(product, "Sharing");
        assert_eq!(
            item_windows(sharing)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec![
                "product.shares.page_views",
                "product.shares.page_views.daily",
                "product.shares.page_views.by_referrer",
                "product.shares.actions",
                "product.shares.cta_clicks",
            ]
        );

        // Manifest caps: a future addition trips a deliberate test edit,
        // not an ingest rejection at boot.
        assert!(manifest.metrics.as_ref().unwrap().len() <= 100);
        assert!(manifest.dashboards.as_ref().unwrap().len() <= 20);
        for dashboard in manifest.dashboards.as_ref().unwrap() {
            assert!(dashboard.sections.len() <= 20);
            for s in &dashboard.sections {
                assert!(s.items.len() <= 24);
            }
        }
    }

    #[test]
    fn disabled_workers_do_not_declare_crons_or_critical_jobs() {
        let identity = ProcessIdentity::new(ROLE);
        let id = identity.instance_id();
        let manifest = manifest(
            &crate::cron::cron_registry(&AppConfig::test_default()),
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
            &crate::cron::cron_registry(&AppConfig::test_default()),
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
        // Only DEBUG is shortened; no app-wide days, so other levels keep
        // Eyes' server default.
        assert_eq!(
            body["retention"],
            serde_json::json!({ "levels": { "DEBUG": DEBUG_RETENTION_DAYS } })
        );
        // Optional artifact for a local server acceptance check, using the
        // actual subscriber wire format rather than a second serializer.
        if let Ok(path) = std::env::var("EYES_MANIFEST_FIXTURE_PATH") {
            std::fs::write(path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        }
    }

    /// `(method, path, Authorization)` for one recorded request.
    type Recorded = (String, String, Option<String>);

    /// Shared log of recorded requests.
    type RecordedLog = std::sync::Arc<std::sync::Mutex<Vec<Recorded>>>;

    /// A fake Eyes server recording `(method, path, Authorization)` for every
    /// request. Hermetic: tokens are always passed explicitly to `register`,
    /// never resolved from ambient `EYES_TOKEN` (which every dev shell on
    /// byte-vm carries).
    async fn recording_server() -> (String, RecordedLog, tokio::task::JoinHandle<()>) {
        use std::sync::{Arc, Mutex};

        let recorded: RecordedLog = Arc::new(Mutex::new(Vec::new()));
        let router = {
            let recorded = recorded.clone();
            axum::Router::new().fallback(
                move |method: axum::http::Method,
                      uri: axum::http::Uri,
                      headers: axum::http::HeaderMap| {
                    let recorded = recorded.clone();
                    async move {
                        let authorization = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        recorded.lock().unwrap().push((
                            method.to_string(),
                            uri.path().to_owned(),
                            authorization,
                        ));
                        axum::http::StatusCode::OK
                    }
                },
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{addr}"), recorded, server)
    }

    async fn test_manifest() -> AppManifest {
        manifest(
            &crate::cron::cron_registry(&AppConfig::test_default()),
            ProcessIdentity::new(ROLE),
            FeatureFlags {
                server: true,
                jobs: true,
                cron: true,
            },
        )
        .unwrap()
    }

    /// Wait for the first heartbeat POST before shutting down: on a
    /// current-thread runtime the spawned heartbeat task must get polled
    /// first, or `shutdown`'s `abort` wins and the POST never happens.
    async fn await_first_heartbeat(recorded: &std::sync::Mutex<Vec<Recorded>>) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if recorded
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(method, path, _)| method == "POST" && path.ends_with("/heartbeat"))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first heartbeat POST");
    }

    #[tokio::test]
    async fn register_sends_bearer_on_every_request_when_a_token_is_set() {
        let manifest = test_manifest().await;
        let (base_url, recorded, server) = recording_server().await;
        let (org_id, app_id) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

        let handle = register(
            &base_url,
            org_id,
            app_id,
            &manifest,
            Some("eyes_test_token"),
        )
        .await
        .expect("registration succeeds");
        await_first_heartbeat(&recorded).await;
        handle.shutdown().await.unwrap();
        server.abort();

        let recorded = recorded.lock().unwrap();
        assert!(
            recorded.len() >= 2,
            "expected manifest + heartbeat POSTs, saw paths {:?}",
            recorded.iter().map(|(_, path, _)| path).collect::<Vec<_>>()
        );
        for (_, path, authorization) in recorded.iter() {
            // Presence-only failure message: an ambient `EYES_TOKEN` leaking in
            // is exactly the regression this test guards against, so the
            // header value must never be printed.
            assert!(
                authorization.as_deref() == Some("Bearer eyes_test_token"),
                "request to {path} did not carry the expected test bearer (header present: {})",
                authorization.is_some()
            );
        }
    }

    #[tokio::test]
    async fn register_boots_tokenless_when_no_token_is_set() {
        let manifest = test_manifest().await;
        let (base_url, recorded, server) = recording_server().await;
        let (org_id, app_id) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

        // `None` must stay `None` even with an ambient `EYES_TOKEN` (set on
        // every byte-vm dev shell): `register` force-assigns the heartbeat
        // config's token, and `send_manifest` takes it as an explicit argument.
        let handle = register(&base_url, org_id, app_id, &manifest, None)
            .await
            .expect("unset token still boots");
        await_first_heartbeat(&recorded).await;
        handle.shutdown().await.unwrap();
        server.abort();

        let recorded = recorded.lock().unwrap();
        assert!(
            recorded.len() >= 2,
            "expected manifest + heartbeat POSTs, saw paths {:?}",
            recorded.iter().map(|(_, path, _)| path).collect::<Vec<_>>()
        );
        for (_, path, authorization) in recorded.iter() {
            // Presence-only failure message: never print a captured header
            // value (ambient `EYES_TOKEN` could have leaked in).
            assert!(
                authorization.is_none(),
                "request to {path} carried an Authorization header"
            );
        }
    }
}
