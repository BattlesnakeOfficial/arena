//! Periodic health sweep of leaderboard entries (BS-3534, DEV-1515).
//!
//! Arena's port of play's ArenaDeactivator, driven by evidence: healthy
//! snakes never see a probe. Each sweep probes only
//!
//! - **suspect** entries in matchmaking: at least
//!   [`SUSPECT_BAD_MOVE_PERCENT`]% of the entry's moves in its recent games
//!   (since its last probe, within [`EVIDENCE_WINDOW_HOURS`]) timed out or
//!   errored, or it's partway through a failure streak; and
//! - entries the sweeper **paused** earlier, so they can recover on their own.
//!
//! An entry is probed with a full test game shaped like its leaderboard
//! ([`snake_health::play_test_game`]: same mode, board size and snake count),
//! so a snake that only breaks on Royale fails only its Royale entry. An entry
//! that fails [`crate::config::AppConfig::snake_health_failure_threshold`]
//! consecutive sweeps is pulled from that leaderboard's matchmaking
//! (`disabled_reason = 'health'`); a paused entry that passes
//! [`crate::config::AppConfig::snake_health_recovery_threshold`] consecutive
//! sweeps is put back. The owner gets one email per snake per sweep listing
//! what changed, with a link to the profile page where they can resume.
//!
//! Re-entrancy (cja jobs retry, and duplicate enqueues are routine): every
//! step is a guarded update, and the emails are gated by the transitions in
//! [`leaderboard_entry_health::deactivate`] and
//! [`leaderboard_entry_health::reactivate`], which only one call can win, so
//! a retried sweep can never double-send.

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use futures::future::join_all;
use reqwest::Client;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::models::battlesnake::{Battlesnake, EngineRegion, Visibility};
use crate::models::leaderboard_entry_health::{self, DISABLED_REASON_HEALTH};
use crate::snake_client::ProxyClients;
use crate::snake_health::{
    self, FailureMode, HEALTH_CHECK_TIMEOUT, HealthCallStatus, HealthCheckCall, TestGameSpec,
};
use crate::state::AppState;

/// Share of an entry's recent moves that must have timed out or errored
/// before it's probed. The snakes the probe can actually catch sit near 100%:
/// a dead server, a crash on this leaderboard's payloads, or a snake that's
/// always over the move budget. Healthy snakes sit far below: arena-side
/// stalls time out roughly 1 in 1,000 moves of every snake (DEV-1498), and
/// snakes that search right up to the deadline overshoot 8-10% of moves (in
/// prod on 2026-10-02, none of the busiest one's 198 games reached 50%) while
/// passing most probes. Snakes that only break late in a game fall in
/// between, but the probe only plays turns 0-1 and couldn't confirm them.
pub const SUSPECT_BAD_MOVE_PERCENT: i32 = 50;

/// How far back to look for an entry's games when weighing its moves.
/// Comfortably more than the 30-minute sweep interval, so a missed sweep
/// (deploy, backlog) doesn't lose the evidence; already-probed moves are
/// excluded by the entry's last-check cursor.
pub const EVIDENCE_WINDOW_HOURS: i32 = 2;

/// An entry the sweep will probe.
struct Candidate {
    leaderboard_entry_id: Uuid,
    /// Paused by the sweeper earlier: this is a recovery probe.
    paused: bool,
    spec: TestGameSpec,
}

/// What one probe concluded.
struct ProbeOutcome {
    status: HealthCallStatus,
    /// The calls that would break a real game, e.g.
    /// `"POST /move (turn 0): Snake timed out at engine proxy"`. Empty when
    /// healthy.
    failure_summary: String,
}

fn summarize(calls: &[HealthCheckCall]) -> ProbeOutcome {
    let has = |status| calls.iter().any(|c| c.status == status);
    let failures: Vec<String> = calls
        .iter()
        .filter(|c| c.status == HealthCallStatus::SnakeFailure)
        .map(|c| format!("{}: {}", c.name, c.summary))
        .collect();
    ProbeOutcome {
        status: if has(HealthCallStatus::ProxyFault) {
            HealthCallStatus::ProxyFault
        } else if failures.is_empty() {
            // Warnings (spec problems real games shrug off) are healthy here.
            HealthCallStatus::Healthy
        } else {
            HealthCallStatus::SnakeFailure
        },
        failure_summary: failures.join("; "),
    }
}

/// Every entry this sweep should probe, grouped by snake.
///
/// Evidence is weighed per entry, from its own leaderboard games: the
/// `(leaderboard_entry_id, created_at)` index bounds each lookup to the
/// entry's games in the window, and only moves after the entry's last probe
/// (and before this sweep began, so the next sweep sees the rest) count.
/// Only enabled leaderboards matter: the matchmaker never draws from retired
/// ones. Manual pauses (NULL reason) are the owner's business and are never
/// probed.
async fn candidates(
    pool: &sqlx::PgPool,
    sweep_started: DateTime<Utc>,
) -> cja::Result<BTreeMap<Uuid, (Battlesnake, Vec<Candidate>)>> {
    let rows = sqlx::query!(
        r#"SELECT
            le.leaderboard_entry_id,
            le.disabled_at IS NOT NULL AS "paused!",
            l.name AS leaderboard_name,
            l.game_type,
            l.board_size,
            l.match_size,
            b.battlesnake_id,
            b.user_id,
            b.name,
            b.url,
            b.visibility AS "visibility: Visibility",
            b.engine_region AS "engine_region: EngineRegion",
            b.color,
            b.head,
            b.tail,
            b.created_at,
            b.updated_at
         FROM leaderboard_entries le
         JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
         JOIN battlesnakes b ON b.battlesnake_id = le.battlesnake_id
         CROSS JOIN LATERAL (
            SELECT COUNT(*) AS moves,
                   COUNT(*) FILTER (WHERE st.timed_out OR st.errored) AS bad
            FROM game_battlesnakes gb
            JOIN snake_turns st ON st.game_battlesnake_id = gb.game_battlesnake_id
            WHERE gb.leaderboard_entry_id = le.leaderboard_entry_id
              AND gb.created_at > $1::timestamptz - make_interval(hours => $2)
              AND st.created_at <= $1::timestamptz
              AND st.created_at > COALESCE(le.health_last_checked_at, '-infinity')
         ) evidence
         WHERE l.disabled_at IS NULL
           AND b.deleted_at IS NULL
           AND (
             (le.disabled_at IS NOT NULL AND le.disabled_reason = $3)
             OR (
               le.disabled_at IS NULL
               AND (
                 le.health_consecutive_failures > 0
                 OR (evidence.bad > 0 AND evidence.bad * 100 >= evidence.moves * $4)
               )
             )
           )
         ORDER BY b.battlesnake_id, l.name"#,
        sweep_started,
        EVIDENCE_WINDOW_HOURS,
        DISABLED_REASON_HEALTH,
        i64::from(SUSPECT_BAD_MOVE_PERCENT),
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch health sweep candidates")?;

    let mut by_snake: BTreeMap<Uuid, (Battlesnake, Vec<Candidate>)> = BTreeMap::new();
    for row in rows {
        let spec = TestGameSpec::for_leaderboard(
            &row.leaderboard_name,
            &row.game_type,
            &row.board_size,
            row.match_size,
        )?;
        let candidate = Candidate {
            leaderboard_entry_id: row.leaderboard_entry_id,
            paused: row.paused,
            spec,
        };
        by_snake
            .entry(row.battlesnake_id)
            .or_insert_with(|| {
                (
                    Battlesnake {
                        battlesnake_id: row.battlesnake_id,
                        user_id: row.user_id,
                        name: row.name,
                        url: row.url,
                        visibility: row.visibility,
                        engine_region: row.engine_region,
                        color: row.color,
                        head: row.head,
                        tail: row.tail,
                        created_at: row.created_at,
                        updated_at: row.updated_at,
                    },
                    Vec::new(),
                )
            })
            .1
            .push(candidate);
    }
    Ok(by_snake)
}

/// What a probe changed for one entry, for the owner's email.
enum Transition {
    /// Pulled from matchmaking, with the most recent problem.
    Paused(String),
    /// Put back into matchmaking.
    Resumed,
}

/// Run one full sweep. Called from the cron-scheduled
/// [`crate::jobs::SnakeHealthSweeperJob`].
pub async fn run_sweep(app_state: &AppState) -> cja::Result<()> {
    let sweep_started = Utc::now();
    let by_snake = candidates(&app_state.db, sweep_started).await?;
    if by_snake.is_empty() {
        return Ok(());
    }

    let (paused, suspect): (Vec<&Candidate>, Vec<&Candidate>) = by_snake
        .values()
        .flat_map(|(_, candidates)| candidates)
        .partition(|c| c.paused);
    tracing::info!(
        snakes = by_snake.len(),
        suspect_probe_count = suspect.len(),
        recovery_probe_count = paused.len(),
        "Starting snake health sweep"
    );

    // Same generous per-call budget as the on-demand test. Snakes are probed
    // one at a time to keep the sweep from hammering shared snake hosts; a
    // snake's own entries play concurrently, like the real games it serves.
    let client = Client::builder()
        .timeout(HEALTH_CHECK_TIMEOUT)
        .build()
        .wrap_err("Failed to build health check client")?;
    let clients = ProxyClients {
        direct: &client,
        east: &app_state.proxy_east_health_client,
        europe: &app_state.proxy_europe_health_client,
        config: &app_state.config.engine_proxy,
    };

    for (snake, candidates) in by_snake.values() {
        let games = join_all(candidates.iter().map(|c| {
            snake_health::play_test_game(&clients, snake, &c.spec, FailureMode::AbortOnFailure)
        }))
        .await;

        let mut pulled = Vec::new();
        let mut resumed = Vec::new();
        for (candidate, calls) in candidates.iter().zip(games) {
            let transition = match calls {
                Ok(calls) => record(app_state, snake, candidate, &calls, sweep_started).await,
                Err(e) => Err(e),
            };
            match transition {
                Ok(Some(Transition::Paused(problem))) => {
                    pulled.push((candidate.spec.label.clone(), problem));
                }
                Ok(Some(Transition::Resumed)) => resumed.push(candidate.spec.label.clone()),
                Ok(None) => {}
                // One entry's bookkeeping failing shouldn't abort the sweep
                // for the rest; the next run retries it.
                Err(e) => tracing::error!(
                    battlesnake_id = %snake.battlesnake_id,
                    leaderboard_entry_id = %candidate.leaderboard_entry_id,
                    error = format!("{e:#}"),
                    "Failed to record health sweep outcome"
                ),
            }
        }
        if let Err(e) = notify_owner(app_state, snake, &pulled, &resumed).await {
            tracing::error!(
                battlesnake_id = %snake.battlesnake_id,
                error = format!("{e:#}"),
                "Failed to notify owner of health sweep changes"
            );
        }
    }

    Ok(())
}

/// Record one entry's probe and apply any transition it earns.
async fn record(
    app_state: &AppState,
    snake: &Battlesnake,
    candidate: &Candidate,
    calls: &[HealthCheckCall],
    sweep_started: DateTime<Utc>,
) -> cja::Result<Option<Transition>> {
    let outcome = summarize(calls);
    if outcome.status == HealthCallStatus::ProxyFault {
        tracing::error!(
            battlesnake_id = %snake.battlesnake_id,
            leaderboard_entry_id = %candidate.leaderboard_entry_id,
            region = snake.engine_region.as_str(),
            "Engine proxy fault during health sweep; preserving entry state"
        );
        return Ok(None);
    }
    if candidate.paused {
        apply_recovery_probe(app_state, snake, candidate, &outcome, sweep_started).await
    } else {
        apply_probe(app_state, snake, candidate, &outcome, sweep_started).await
    }
}

/// Record a probe of an entry in matchmaking; pull it once the failure streak
/// crosses the threshold.
async fn apply_probe(
    app_state: &AppState,
    snake: &Battlesnake,
    candidate: &Candidate,
    outcome: &ProbeOutcome,
    sweep_started: DateTime<Utc>,
) -> cja::Result<Option<Transition>> {
    let entry_id = candidate.leaderboard_entry_id;
    if outcome.status == HealthCallStatus::Healthy {
        leaderboard_entry_health::record_success(&app_state.db, entry_id, sweep_started).await?;
        return Ok(None);
    }

    let Some(failures) = leaderboard_entry_health::record_failure(
        &app_state.db,
        entry_id,
        &outcome.failure_summary,
        sweep_started,
    )
    .await?
    else {
        // The owner paused it meanwhile.
        return Ok(None);
    };
    let threshold = app_state.config.snake_health_failure_threshold;
    tracing::info!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        leaderboard = %candidate.spec.label,
        consecutive_failures = failures,
        threshold,
        failure = %outcome.failure_summary,
        "Snake failed health probe"
    );
    if failures < threshold
        || !leaderboard_entry_health::deactivate(&app_state.db, entry_id).await?
    {
        return Ok(None);
    }

    tracing::warn!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        leaderboard = %candidate.spec.label,
        consecutive_failures = failures,
        "Pulled snake from leaderboard matchmaking"
    );
    Ok(Some(Transition::Paused(outcome.failure_summary.clone())))
}

/// Record a recovery probe of a paused entry; put it back once the healthy
/// streak crosses the recovery threshold.
async fn apply_recovery_probe(
    app_state: &AppState,
    snake: &Battlesnake,
    candidate: &Candidate,
    outcome: &ProbeOutcome,
    sweep_started: DateTime<Utc>,
) -> cja::Result<Option<Transition>> {
    let entry_id = candidate.leaderboard_entry_id;
    if outcome.status != HealthCallStatus::Healthy {
        leaderboard_entry_health::record_recovery_failure(
            &app_state.db,
            entry_id,
            &outcome.failure_summary,
            sweep_started,
        )
        .await?;
        return Ok(None);
    }

    let Some(successes) =
        leaderboard_entry_health::record_recovery_success(&app_state.db, entry_id, sweep_started)
            .await?
    else {
        // The owner resumed it meanwhile.
        return Ok(None);
    };
    let threshold = app_state.config.snake_health_recovery_threshold;
    tracing::info!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        leaderboard = %candidate.spec.label,
        consecutive_successes = successes,
        threshold,
        "Paused snake passed recovery probe"
    );
    if successes < threshold
        || !leaderboard_entry_health::reactivate(&app_state.db, entry_id).await?
    {
        return Ok(None);
    }

    tracing::info!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        leaderboard = %candidate.spec.label,
        "Put recovered snake back into leaderboard matchmaking"
    );
    Ok(Some(Transition::Resumed))
}

/// One email per snake per sweep, covering every leaderboard that changed.
async fn notify_owner(
    app_state: &AppState,
    snake: &Battlesnake,
    pulled: &[(String, String)],
    resumed: &[String],
) -> cja::Result<()> {
    if pulled.is_empty() && resumed.is_empty() {
        return Ok(());
    }
    let Some(email) =
        leaderboard_entry_health::owner_notification_email(&app_state.db, snake.battlesnake_id)
            .await?
    else {
        tracing::warn!(
            battlesnake_id = %snake.battlesnake_id,
            "Snake's matchmaking changed but owner has no known email; skipping notification"
        );
        return Ok(());
    };
    let profile_url = format!(
        "{}/battlesnakes/{}/profile",
        app_state.config.base_url, snake.battlesnake_id
    );
    let hourly_limit = app_state.config.email_per_recipient_hourly_limit;
    if !pulled.is_empty() {
        app_state.mailer.notify_matchmaking_deactivated(
            &app_state.db,
            hourly_limit,
            &email,
            &snake.name,
            pulled,
            &profile_url,
        );
    }
    if !resumed.is_empty() {
        app_state.mailer.notify_matchmaking_reactivated(
            &app_state.db,
            hourly_limit,
            &email,
            &snake.name,
            resumed,
            &profile_url,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use sqlx::PgPool;
    use wiremock::matchers::{any, body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Clone, Copy)]
    enum Bad {
        TimedOut,
        Errored,
    }

    struct Seed {
        pool: PgPool,
        snake_id: Uuid,
    }

    impl Seed {
        async fn new(pool: &PgPool, url: &str) -> cja::Result<Self> {
            let user_id = sqlx::query_scalar!(
                "INSERT INTO users (external_github_id, github_login, github_access_token)
                 VALUES (77001, 'sweep-owner', 'test-token')
                 RETURNING user_id",
            )
            .fetch_one(pool)
            .await?;
            let snake_id = sqlx::query_scalar!(
                "INSERT INTO battlesnakes (user_id, name, url)
                 VALUES ($1, 'sweepy', $2)
                 RETURNING battlesnake_id",
                user_id,
                url,
            )
            .fetch_one(pool)
            .await?;
            Ok(Self {
                pool: pool.clone(),
                snake_id,
            })
        }

        /// Enter the snake in a fresh 11x11 leaderboard.
        async fn join(&self, name: &str, game_type: &str, match_size: i32) -> cja::Result<Uuid> {
            let leaderboard_id = sqlx::query_scalar!(
                "INSERT INTO leaderboards (name, game_type, board_size, match_size)
                 VALUES ($1, $2, '11x11', $3)
                 RETURNING leaderboard_id",
                name,
                game_type,
                match_size,
            )
            .fetch_one(&self.pool)
            .await?;
            Ok(crate::models::leaderboard::get_or_create_entry(
                &self.pool,
                leaderboard_id,
                self.snake_id,
            )
            .await?
            .leaderboard_entry_id)
        }

        /// One leaderboard game `minutes_ago` in which the entry made `bad`
        /// unusable moves and `good` fine ones.
        async fn game(
            &self,
            entry_id: Uuid,
            minutes_ago: i32,
            bad: usize,
            good: usize,
            kind: Bad,
        ) -> cja::Result<()> {
            let game_id = sqlx::query_scalar!(
                "INSERT INTO games (board_size, game_type, status, created_at)
                 VALUES ('11x11', 'Standard', 'finished', NOW() - make_interval(mins => $1))
                 RETURNING game_id",
                minutes_ago,
            )
            .fetch_one(&self.pool)
            .await?;
            let game_battlesnake_id = sqlx::query_scalar!(
                "INSERT INTO game_battlesnakes (game_id, leaderboard_entry_id, created_at)
                 SELECT $1, $2, created_at FROM games WHERE game_id = $1
                 RETURNING game_battlesnake_id",
                game_id,
                entry_id,
            )
            .fetch_one(&self.pool)
            .await?;
            for turn in 0..bad + good {
                let is_bad = turn < bad;
                let turn_id = sqlx::query_scalar!(
                    "INSERT INTO turns (game_id, turn_number) VALUES ($1, $2) RETURNING turn_id",
                    game_id,
                    turn as i32,
                )
                .fetch_one(&self.pool)
                .await?;
                sqlx::query!(
                    "INSERT INTO snake_turns
                         (turn_id, game_battlesnake_id, direction, latency_ms, timed_out, errored, created_at)
                     VALUES ($1, $2, 'up', $3, $4, $5, NOW() - make_interval(mins => $6))",
                    turn_id,
                    game_battlesnake_id,
                    (!is_bad || matches!(kind, Bad::Errored)).then_some(40),
                    is_bad && matches!(kind, Bad::TimedOut),
                    is_bad && matches!(kind, Bad::Errored),
                    minutes_ago,
                )
                .execute(&self.pool)
                .await?;
            }
            Ok(())
        }

        /// Make the snake's server fail everything its real games ask of it.
        async fn broken_games(&self, entry_id: Uuid) -> cja::Result<()> {
            self.game(entry_id, 20, 20, 0, Bad::TimedOut).await
        }
    }

    struct Health {
        disabled: bool,
        reason: Option<String>,
        failures: i32,
        successes: i32,
        last_failure: Option<String>,
        checked: bool,
    }

    async fn health(pool: &PgPool, entry_id: Uuid) -> cja::Result<Health> {
        let row = sqlx::query!(
            r#"SELECT disabled_at IS NOT NULL AS "disabled!", disabled_reason,
                      health_consecutive_failures, health_consecutive_successes,
                      health_last_failure, health_last_checked_at
             FROM leaderboard_entries WHERE leaderboard_entry_id = $1"#,
            entry_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(Health {
            disabled: row.disabled,
            reason: row.disabled_reason,
            failures: row.health_consecutive_failures,
            successes: row.health_consecutive_successes,
            last_failure: row.health_last_failure,
            checked: row.health_last_checked_at.is_some(),
        })
    }

    /// Simulate a real sweep interval passing: push every entry's last check
    /// past the probe-count spacing gate.
    async fn age_checks(pool: &PgPool) -> cja::Result<()> {
        sqlx::query!(
            "UPDATE leaderboard_entries
             SET health_last_checked_at = health_last_checked_at - INTERVAL '31 minutes'"
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn pause(pool: &PgPool, entry_id: Uuid) -> cja::Result<()> {
        let failed =
            leaderboard_entry_health::record_failure(pool, entry_id, "was down", Utc::now())
                .await?;
        assert_eq!(failed, Some(1));
        assert!(leaderboard_entry_health::deactivate(pool, entry_id).await?);
        Ok(())
    }

    async fn requests(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    async fn healthy_snake_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&server)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        server
    }

    async fn broken_snake_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        server
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn healthy_snakes_get_no_probe_traffic(pool: PgPool) -> cja::Result<()> {
        let server = healthy_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let playing = seed.join("Standard", "Standard", 4).await?;
        let idle = seed.join("Duels", "Standard", 2).await?;
        seed.game(playing, 10, 0, 150, Bad::TimedOut).await?;
        // One arena-side stall is noise, not evidence.
        seed.game(playing, 5, 1, 149, Bad::TimedOut).await?;
        // Neither is a snake that searches right up to the deadline and
        // often overshoots it: just under the threshold, it plays.
        let deadline_pusher = seed.join("Constrictor", "Constrictor", 4).await?;
        seed.game(deadline_pusher, 10, 49, 51, Bad::TimedOut)
            .await?;

        run_sweep(&AppState::test_from_pool(pool.clone())).await?;

        assert_eq!(requests(&server).await, 0);
        assert!(!health(&pool, playing).await?.checked);
        assert!(!health(&pool, idle).await?.checked);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn timeouts_trigger_a_probe_shaped_like_the_leaderboard(pool: PgPool) -> cja::Result<()> {
        let server = healthy_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let royale = seed.join("Royale", "Royale", 4).await?;
        seed.game(royale, 10, 30, 10, Bad::TimedOut).await?;
        let app_state = AppState::test_from_pool(pool.clone());

        run_sweep(&app_state).await?;

        let received = server.received_requests().await.unwrap();
        let paths: Vec<&str> = received.iter().map(|r| r.url.path()).collect();
        assert_eq!(
            paths,
            ["/start", "/move", "/move", "/end"],
            "no GET / in sweeps"
        );
        let start: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(start["game"]["map"], "royale");
        assert_eq!(start["game"]["source"], "arena");
        assert_eq!(start["board"]["snakes"].as_array().unwrap().len(), 4);
        let state = health(&pool, royale).await?;
        assert!(state.checked);
        assert_eq!(state.failures, 0);

        // The evidence was spent on that probe: the next sweep leaves it be.
        run_sweep(&app_state).await?;
        assert_eq!(requests(&server).await, 4);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn errored_moves_are_evidence_too(pool: PgPool) -> cja::Result<()> {
        let server = healthy_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let entry = seed.join("Standard", "Standard", 4).await?;
        // Exactly at the threshold counts.
        seed.game(entry, 10, 25, 25, Bad::Errored).await?;

        run_sweep(&AppState::test_from_pool(pool.clone())).await?;

        assert_eq!(requests(&server).await, 4);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn evidence_from_before_the_last_probe_does_not_count(pool: PgPool) -> cja::Result<()> {
        let server = healthy_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let entry = seed.join("Standard", "Standard", 4).await?;
        seed.game(entry, 30, 20, 0, Bad::TimedOut).await?;
        sqlx::query!(
            "UPDATE leaderboard_entries SET health_last_checked_at = NOW() - INTERVAL '10 minutes'
             WHERE leaderboard_entry_id = $1",
            entry
        )
        .execute(&pool)
        .await?;

        run_sweep(&AppState::test_from_pool(pool.clone())).await?;

        assert_eq!(requests(&server).await, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failing_entry_is_pulled_after_threshold_sweeps(pool: PgPool) -> cja::Result<()> {
        let server = broken_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let entry = seed.join("Standard", "Standard", 4).await?;
        seed.broken_games(entry).await?;
        let app_state = AppState::test_from_pool(pool.clone());
        let threshold = app_state.config.snake_health_failure_threshold;
        assert!(threshold >= 2, "test assumes a multi-sweep threshold");

        // Once failing, it keeps being probed with no new evidence, and
        // stays in matchmaking until the threshold.
        for expected in 1..threshold {
            run_sweep(&app_state).await?;
            let state = health(&pool, entry).await?;
            assert_eq!(state.failures, expected);
            assert!(!state.disabled);
            age_checks(&pool).await?;
        }
        run_sweep(&app_state).await?;

        let state = health(&pool, entry).await?;
        assert!(state.disabled);
        assert_eq!(state.reason.as_deref(), Some(DISABLED_REASON_HEALTH));
        assert_eq!(state.failures, threshold);
        // The /start 500 is only a warning; the /move 500 is what pulled it.
        let failure = state.last_failure.unwrap();
        assert!(failure.starts_with("POST /move (turn 0):"), "{failure}");
        assert!(!failure.contains("POST /start"), "{failure}");
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn a_royale_only_crash_pulls_only_the_royale_entry(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .and(body_partial_json(
                serde_json::json!({"game": {"map": "royale"}}),
            ))
            .respond_with(ResponseTemplate::new(500).set_body_string("hazards?!"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/move"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&server)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let royale = seed.join("Royale", "Royale", 4).await?;
        let duels = seed.join("Duels", "Standard", 2).await?;
        seed.broken_games(royale).await?;
        seed.broken_games(duels).await?;
        let app_state = AppState::test_from_pool(pool.clone());

        for _ in 0..app_state.config.snake_health_failure_threshold {
            run_sweep(&app_state).await?;
            age_checks(&pool).await?;
        }

        let royale = health(&pool, royale).await?;
        assert!(royale.disabled);
        assert_eq!(royale.reason.as_deref(), Some(DISABLED_REASON_HEALTH));
        let duels = health(&pool, duels).await?;
        assert!(!duels.disabled, "Duels games work fine");
        assert_eq!(duels.failures, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn paused_entry_comes_back_after_recovery_threshold(pool: PgPool) -> cja::Result<()> {
        let server = healthy_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let entry = seed.join("Standard", "Standard", 4).await?;
        pause(&pool, entry).await?;
        let app_state = AppState::test_from_pool(pool.clone());
        let threshold = app_state.config.snake_health_recovery_threshold;
        assert!(threshold >= 2, "test assumes a multi-sweep threshold");

        for expected in 1..threshold {
            age_checks(&pool).await?;
            run_sweep(&app_state).await?;
            let state = health(&pool, entry).await?;
            assert_eq!(state.successes, expected);
            assert!(state.disabled);
        }
        // A piled-up sweep inside the spacing window must not fake the last
        // step of the streak.
        run_sweep(&app_state).await?;
        assert_eq!(health(&pool, entry).await?.successes, threshold - 1);

        age_checks(&pool).await?;
        run_sweep(&app_state).await?;
        let state = health(&pool, entry).await?;
        assert!(!state.disabled);
        assert_eq!(state.reason, None);
        assert_eq!((state.failures, state.successes), (0, 0));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn failed_recovery_probe_resets_the_streak(pool: PgPool) -> cja::Result<()> {
        let server = broken_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let entry = seed.join("Standard", "Standard", 4).await?;
        pause(&pool, entry).await?;
        age_checks(&pool).await?;
        assert_eq!(
            leaderboard_entry_health::record_recovery_success(&pool, entry, Utc::now()).await?,
            Some(1)
        );

        age_checks(&pool).await?;
        run_sweep(&AppState::test_from_pool(pool.clone())).await?;

        let state = health(&pool, entry).await?;
        assert_eq!(state.successes, 0);
        assert!(state.disabled);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn retired_boards_and_manual_pauses_are_never_probed(pool: PgPool) -> cja::Result<()> {
        let server = broken_snake_server().await;
        let seed = Seed::new(&pool, &server.uri()).await?;
        let retired = seed.join("Retired", "Standard", 4).await?;
        let paused = seed.join("Paused", "Standard", 4).await?;
        seed.broken_games(retired).await?;
        seed.broken_games(paused).await?;
        sqlx::query!("UPDATE leaderboards SET disabled_at = NOW() WHERE name = 'Retired'")
            .execute(&pool)
            .await?;
        crate::models::leaderboard::set_disabled(&pool, paused, Some(Utc::now())).await?;

        run_sweep(&AppState::test_from_pool(pool.clone())).await?;

        assert_eq!(requests(&server).await, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn proxy_fault_preserves_entry_state(pool: PgPool) -> cja::Result<()> {
        let proxy = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(502))
            .mount(&proxy)
            .await;
        let seed = Seed::new(&pool, "https://example.com/eu").await?;
        sqlx::query!(
            "UPDATE battlesnakes SET engine_region = 'europe-west4' WHERE battlesnake_id = $1",
            seed.snake_id
        )
        .execute(&pool)
        .await?;
        let suspect = seed.join("Standard", "Standard", 4).await?;
        let paused = seed.join("Duels", "Standard", 2).await?;
        seed.broken_games(suspect).await?;
        pause(&pool, paused).await?;
        let mut state = AppState::test_from_pool(pool.clone());
        let config = std::sync::Arc::get_mut(&mut state.config).unwrap();
        config.engine_proxy.token = Some("test-token".to_string());
        config.engine_proxy.europe_west4_url = proxy.uri();

        for _ in 0..=state.config.snake_health_failure_threshold {
            run_sweep(&state).await?;
            age_checks(&pool).await?;
        }

        assert!(requests(&proxy).await > 0, "the sweep did try");
        let suspect = health(&pool, suspect).await?;
        assert!(!suspect.disabled);
        assert_eq!(suspect.failures, 0);
        let paused = health(&pool, paused).await?;
        assert!(paused.disabled);
        assert_eq!(paused.successes, 0);
        Ok(())
    }
}
