//! Pure metric and dashboard declarations shared by Eyes and PostHog sync.
use cja::observability::{
    AggregateFunction as Agg, AlertBuilder, Comparison, Dashboard as NamedDashboard, Declarations,
    FilterOperator, Item, Metric as NamedMetric, ProductMetricBuilder, RatioMetricBuilder, Section,
    TelemetryMetricBuilder as Metric,
};

pub const DAY: u64 = 86_400;
pub const WEEK: u64 = 604_800;
pub const FOUR_WEEKS: u64 = 2_419_200;
pub const NINETY_DAYS: u64 = 7_776_000;
pub const THREE_HUNDRED_SIXTY_FOUR_DAYS: u64 = 31_449_600;

pub fn declarations() -> Result<Declarations, String> {
    let mut declarations = Declarations::new();
    for metric in metrics()? {
        declarations = declarations.metric(metric);
    }
    for (id, metric, description, warning, critical) in [
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
    ] {
        declarations = declarations.alert(
            AlertBuilder::new(id, metric)
                .description(description)
                .window_seconds(300)
                .evaluation_interval_seconds(60)
                .evaluation_delay_seconds(30)
                .failure_threshold(3)
                .recovery_threshold(2)
                .warning(Comparison::Above, warning)
                .critical(Comparison::Above, critical)
                .build()?,
        );
    }
    declarations = declarations
        .dashboard(dashboard()?)
        .dashboard(product_dashboard()?)
        .dashboard(product_history_dashboard()?);
    declarations.validate()?;
    Ok(declarations)
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
        ProductMetricBuilder::count("product.play_claims.daily", "play_claim_succeeded").display_name("Play claims")
        .unit("claims")
        .bucket_seconds(86_400)
        .build()?,
        ProductMetricBuilder::count("product.new_users.daily", "user_authenticated").display_name("New users")
        .filter("new_user", FilterOperator::Eq, true)
        .unit("users")
        .bucket_seconds(86_400)
        .build()?,
        ProductMetricBuilder::count("product.new_snakes.daily", "battlesnake_created").display_name("New snakes")
        .unit("snakes")
        .bucket_seconds(86_400)
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
        ProductMetricBuilder::count("product.new_users.weekly", "user_authenticated").display_name("New users, per week")
        .filter("new_user", FilterOperator::Eq, true)
        .unit("users")
        .bucket_weeks(1)
        .build()?,
        ProductMetricBuilder::count("product.new_snakes.weekly", "battlesnake_created").display_name("New snakes, per week")
        .unit("snakes")
        .bucket_weeks(1)
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
        .build()?,
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
        .build()?,
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
        .build()?,
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
        .build()?,
        RatioMetricBuilder::new(
            "product.games.ladder.share",
            "product.games.ladder",
            "product.games.played",
        )
        .as_percentage()
        .display_name("Ladder share of games played")
        .build()?,
        RatioMetricBuilder::new(
            "product.games.user_created.share",
            "product.games.user_created",
            "product.games.played",
        )
        .as_percentage()
        .display_name("User-created share of games played")
        .build()?,
        RatioMetricBuilder::new(
            "product.games.tournament.share",
            "product.games.tournament",
            "product.games.played",
        )
        .as_percentage()
        .display_name("Tournament share of games played")
        .build()?,
        ProductMetricBuilder::count("product.achievements.earned", "achievement_earned").display_name("Achievements earned")
        .unit("earns")
        .build()?,
        ProductMetricBuilder::count("product.achievements.earned.daily", "achievement_earned").display_name("Achievements earned")
        .unit("earns")
        .bucket_seconds(86_400)
        .build()?,
        ProductMetricBuilder::count("product.achievements.firsts.daily", "achievement_earned").display_name("First achievement earns")
        .filter("first", FilterOperator::Eq, true)
        .unit("earns")
        .bucket_seconds(86_400)
        .build()?,
        ProductMetricBuilder::count("product.achievements.by_slug", "achievement_earned").display_name("Achievements earned by achievement")
        .breakdown("slug")
        .unit("earns")
        .build()?,
        ProductMetricBuilder::count("product.shares.page_views", "share_page_view").display_name("Achievement share page views")
        .unit("views")
        .build()?,
        ProductMetricBuilder::count("product.shares.page_views.daily", "share_page_view").display_name("Achievement share page views")
        .unit("views")
        .bucket_seconds(86_400)
        .build()?,
        ProductMetricBuilder::count("product.shares.page_views.by_referrer", "share_page_view").display_name("Achievement share page views by referrer")
        .breakdown("referrer")
        .unit("views")
        .build()?,
        ProductMetricBuilder::count("product.shares.actions", "share_action").display_name("Achievement share actions")
        .breakdown("channel")
        .unit("actions")
        .build()?,
        ProductMetricBuilder::count("product.shares.cta_clicks", "share_cta_click").display_name("Achievement share CTA clicks")
        .breakdown("cta")
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
