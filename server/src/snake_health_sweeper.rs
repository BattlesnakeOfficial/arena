//! Periodic health sweep of leaderboard snakes (BS-3534).
//!
//! Arena's port of play's ArenaDeactivator: every snake with an active
//! leaderboard entry gets the same four-call probe as the on-demand "Test
//! Snake" button ([`crate::snake_health`]). A snake that fails
//! [`crate::config::AppConfig::snake_health_failure_threshold`] consecutive
//! sweeps is pulled from matchmaking — its entries are disabled with
//! `disabled_reason = 'health'` — and the owner is emailed once, with a link
//! back to the profile page where they can resume.
//!
//! Deactivated snakes aren't abandoned: each sweep also probes them, and a
//! snake that stays healthy for
//! [`crate::config::AppConfig::snake_health_recovery_threshold`] consecutive
//! sweeps is put back into matchmaking automatically (the owner's Resume
//! button still works for an immediate manual return).
//!
//! Re-entrancy (cja jobs retry, and duplicate enqueues are routine): every
//! step is an idempotent upsert/update, and the notification emails are
//! gated by the compare-and-sets inside [`snake_health_status::deactivate`]
//! and [`snake_health_status::reactivate`], so a retried sweep can never
//! double-send.

use reqwest::Client;

use crate::models::battlesnake::{Battlesnake, Visibility};
use crate::models::snake_health_status;
use crate::snake_health::{self, HEALTH_CHECK_TIMEOUT, HealthCheckReport};
use crate::state::AppState;

/// Everything in one probe result the sweep needs to act on.
struct ProbeOutcome {
    healthy: bool,
    /// Human-readable description of the failed calls, e.g.
    /// `"POST /move: request timed out"`. Empty when healthy.
    failure_summary: String,
}

fn summarize(report: &HealthCheckReport) -> ProbeOutcome {
    let failures: Vec<String> = report
        .calls
        .iter()
        .filter(|c| !c.ok)
        .map(|c| format!("{}: {}", c.name, c.summary))
        .collect();

    ProbeOutcome {
        healthy: failures.is_empty(),
        failure_summary: failures.join("; "),
    }
}

/// All snakes currently in matchmaking rotation: distinct snakes with at
/// least one enabled entry on an enabled leaderboard. The leaderboard
/// filter matters: the matchmaker only draws from active leaderboards
/// (`get_active_leaderboards`), so a snake whose only entry sits on a
/// retired board is matched in zero games — probing (let alone
/// deactivating and emailing about) it would be pure noise. Snakes the
/// sweeper already pulled have no enabled entries, so they naturally drop
/// out of this population and into [`snakes_health_disabled`].
async fn snakes_in_matchmaking(pool: &sqlx::PgPool) -> cja::Result<Vec<Battlesnake>> {
    use color_eyre::eyre::Context as _;

    let snakes = sqlx::query_as!(
        Battlesnake,
        r#"SELECT DISTINCT
            b.battlesnake_id,
            b.user_id,
            b.name,
            b.url,
            b.visibility as "visibility: Visibility",
            b.color,
            b.head,
            b.tail,
            b.created_at,
            b.updated_at
         FROM battlesnakes b
         JOIN leaderboard_entries le ON le.battlesnake_id = b.battlesnake_id
         JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
         WHERE le.disabled_at IS NULL
           AND l.disabled_at IS NULL
         ORDER BY b.battlesnake_id"#,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch snakes in matchmaking")?;

    Ok(snakes)
}

/// Snakes the sweeper pulled from matchmaking: distinct snakes whose only
/// presence on enabled leaderboards is entries disabled with
/// `disabled_reason = 'health'`. These get recovery probes so a fixed
/// server finds its own way back. Manual pauses (NULL reason) are the
/// owner's business and are never probed.
async fn snakes_health_disabled(pool: &sqlx::PgPool) -> cja::Result<Vec<Battlesnake>> {
    use color_eyre::eyre::Context as _;

    let snakes = sqlx::query_as!(
        Battlesnake,
        r#"SELECT DISTINCT
            b.battlesnake_id,
            b.user_id,
            b.name,
            b.url,
            b.visibility as "visibility: Visibility",
            b.color,
            b.head,
            b.tail,
            b.created_at,
            b.updated_at
         FROM battlesnakes b
         JOIN leaderboard_entries le ON le.battlesnake_id = b.battlesnake_id
         JOIN leaderboards l ON l.leaderboard_id = le.leaderboard_id
         WHERE le.disabled_at IS NOT NULL
           AND le.disabled_reason = 'health'
           AND l.disabled_at IS NULL
         ORDER BY b.battlesnake_id"#,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch health-disabled snakes")?;

    Ok(snakes)
}

/// Run one full sweep. Called from the cron-scheduled
/// [`crate::jobs::SnakeHealthSweeperJob`].
pub async fn run_sweep(app_state: &AppState) -> cja::Result<()> {
    let active = snakes_in_matchmaking(&app_state.db).await?;
    let recovering = snakes_health_disabled(&app_state.db).await?;
    if active.is_empty() && recovering.is_empty() {
        return Ok(());
    }

    tracing::info!(
        snake_count = active.len(),
        recovering_count = recovering.len(),
        "Starting snake health sweep"
    );

    // Same generous per-call budget as the on-demand test; sequential probes
    // keep the sweep from hammering shared snake hosts, and the population
    // (active leaderboard snakes) is small.
    let client = Client::builder()
        .timeout(HEALTH_CHECK_TIMEOUT)
        .build()
        .map_err(|e| cja::color_eyre::eyre::eyre!("Failed to build health check client: {e}"))?;

    for snake in &active {
        let outcome = probe(&client, snake).await;
        if let Err(e) = apply_probe_outcome(app_state, snake, &outcome).await {
            // One snake's bookkeeping failing shouldn't abort the sweep for
            // the rest; the next run retries it.
            tracing::error!(
                battlesnake_id = %snake.battlesnake_id,
                error = %e,
                "Failed to record health sweep outcome"
            );
        }
    }

    for snake in &recovering {
        let outcome = probe(&client, snake).await;
        if let Err(e) = apply_recovery_probe_outcome(app_state, snake, &outcome).await {
            tracing::error!(
                battlesnake_id = %snake.battlesnake_id,
                error = %e,
                "Failed to record recovery probe outcome"
            );
        }
    }

    Ok(())
}

/// One four-call health probe, summarized.
async fn probe(client: &Client, snake: &Battlesnake) -> ProbeOutcome {
    let (engine_game, snake_id) = snake_health::build_test_game(snake);
    // AbortOnFailure keeps a dead snake at one timeout (~5s) instead of
    // four, bounding how far a sweep full of dead snakes can stretch.
    let report = snake_health::run_health_check(
        client,
        &snake.url,
        &engine_game,
        &snake_id,
        snake_health::FailureMode::AbortOnFailure,
    )
    .await;
    summarize(&report)
}

/// Record a probe outcome and deactivate + notify when the failure streak
/// crosses the threshold. Split from the HTTP probing so the decision logic
/// is testable against a plain database.
async fn apply_probe_outcome(
    app_state: &AppState,
    snake: &Battlesnake,
    outcome: &ProbeOutcome,
) -> cja::Result<()> {
    if outcome.healthy {
        snake_health_status::record_success(&app_state.db, snake.battlesnake_id).await?;
        return Ok(());
    }

    let failures = snake_health_status::record_failure(
        &app_state.db,
        snake.battlesnake_id,
        &outcome.failure_summary,
    )
    .await?;

    let threshold = app_state.config.snake_health_failure_threshold;
    tracing::info!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        consecutive_failures = failures,
        threshold,
        failure = %outcome.failure_summary,
        "Snake failed health probe"
    );

    if failures < threshold {
        return Ok(());
    }

    let newly_deactivated =
        snake_health_status::deactivate(&app_state.db, snake.battlesnake_id).await?;

    if !newly_deactivated {
        return Ok(());
    }

    tracing::warn!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        consecutive_failures = failures,
        "Deactivated snake from leaderboard matchmaking"
    );

    let profile_url = format!(
        "{}/battlesnakes/{}/profile",
        app_state.config.base_url, snake.battlesnake_id
    );

    match snake_health_status::owner_notification_email(&app_state.db, snake.battlesnake_id).await?
    {
        Some(email) => {
            app_state.mailer.notify_matchmaking_deactivated(
                &app_state.db,
                app_state.config.email_per_recipient_hourly_limit,
                &email,
                &snake.name,
                &outcome.failure_summary,
                &profile_url,
            );
        }
        None => {
            tracing::warn!(
                battlesnake_id = %snake.battlesnake_id,
                "Snake deactivated but owner has no known email; skipping notification"
            );
        }
    }

    Ok(())
}

/// Record a recovery probe of a health-disabled snake and reactivate + notify
/// when the healthy streak crosses the recovery threshold.
async fn apply_recovery_probe_outcome(
    app_state: &AppState,
    snake: &Battlesnake,
    outcome: &ProbeOutcome,
) -> cja::Result<()> {
    if !outcome.healthy {
        snake_health_status::record_recovery_failure(
            &app_state.db,
            snake.battlesnake_id,
            &outcome.failure_summary,
        )
        .await?;
        return Ok(());
    }

    let successes =
        snake_health_status::record_recovery_success(&app_state.db, snake.battlesnake_id).await?;

    let threshold = app_state.config.snake_health_recovery_threshold;
    tracing::info!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        consecutive_successes = successes,
        threshold,
        "Deactivated snake passed recovery probe"
    );

    if successes < threshold {
        return Ok(());
    }

    let newly_reactivated =
        snake_health_status::reactivate(&app_state.db, snake.battlesnake_id).await?;

    if !newly_reactivated {
        return Ok(());
    }

    tracing::info!(
        battlesnake_id = %snake.battlesnake_id,
        snake_name = %snake.name,
        "Reactivated recovered snake into leaderboard matchmaking"
    );

    let profile_url = format!(
        "{}/battlesnakes/{}/profile",
        app_state.config.base_url, snake.battlesnake_id
    );

    if let Some(email) =
        snake_health_status::owner_notification_email(&app_state.db, snake.battlesnake_id).await?
    {
        app_state.mailer.notify_matchmaking_reactivated(
            &app_state.db,
            app_state.config.email_per_recipient_hourly_limit,
            &email,
            &snake.name,
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
    use uuid::Uuid;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn create_snake_on_leaderboard(pool: &PgPool, url: &str) -> cja::Result<(Uuid, Uuid)> {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES (77001, 'sweep-owner', 'test-token')
             RETURNING user_id",
        )
        .fetch_one(pool)
        .await?;
        let battlesnake_id = sqlx::query_scalar!(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'sweepy', $2)
             RETURNING battlesnake_id",
            user_id,
            url,
        )
        .fetch_one(pool)
        .await?;
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards (name) VALUES ('sweep-board') RETURNING leaderboard_id",
        )
        .fetch_one(pool)
        .await?;
        let entry =
            crate::models::leaderboard::get_or_create_entry(pool, leaderboard_id, battlesnake_id)
                .await?;
        Ok((battlesnake_id, entry.leaderboard_entry_id))
    }

    /// Simulate a real sweep interval passing: push the snake's last check
    /// past the failure-count spacing gate.
    async fn age_last_check(pool: &PgPool, battlesnake_id: Uuid) -> cja::Result<()> {
        sqlx::query!(
            "UPDATE snake_health_status
             SET last_checked_at = last_checked_at - INTERVAL '16 minutes'
             WHERE battlesnake_id = $1",
            battlesnake_id,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn entry_disabled(pool: &PgPool, entry_id: Uuid) -> cja::Result<Option<String>> {
        let row = sqlx::query!(
            "SELECT disabled_at, disabled_reason FROM leaderboard_entries
             WHERE leaderboard_entry_id = $1",
            entry_id,
        )
        .fetch_one(pool)
        .await?;
        Ok(row
            .disabled_at
            .map(|_| row.disabled_reason.unwrap_or_default()))
    }

    /// A snake server that errors on everything: every probe call fails.
    async fn broken_snake_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        server
    }

    /// A snake server that answers every probe call successfully.
    async fn healthy_snake_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"apiversion":"1"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&server)
            .await;
        server
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_deactivates_after_threshold_consecutive_failures(
        pool: PgPool,
    ) -> cja::Result<()> {
        let server = broken_snake_server().await;
        let (battlesnake_id, entry_id) = create_snake_on_leaderboard(&pool, &server.uri()).await?;
        let app_state = AppState::test_from_pool(pool.clone());
        let threshold = app_state.config.snake_health_failure_threshold;
        assert!(threshold >= 2, "test assumes a multi-sweep threshold");

        // Every sweep below the threshold leaves the snake in matchmaking.
        // (Failure counting is gated to one per spacing window, so age the
        // last check between sweeps to simulate real 30-minute intervals.)
        for expected_failures in 1..threshold {
            run_sweep(&app_state).await?;
            let status = snake_health_status::get(&pool, battlesnake_id)
                .await?
                .expect("sweeper recorded a row");
            assert_eq!(status.consecutive_failures, expected_failures);
            assert_eq!(entry_disabled(&pool, entry_id).await?, None);
            age_last_check(&pool, battlesnake_id).await?;
        }

        // The sweep that reaches the threshold pulls it.
        run_sweep(&app_state).await?;
        assert_eq!(
            entry_disabled(&pool, entry_id).await?.as_deref(),
            Some(snake_health_status::DISABLED_REASON_HEALTH)
        );
        let status = snake_health_status::get(&pool, battlesnake_id)
            .await?
            .expect("row exists");
        assert!(status.deactivated_at.is_some());
        assert!(status.last_failure.is_some());

        // Further sweeps probe it for recovery, but the server is still
        // broken: failure streak stays frozen, recovery streak stays zero,
        // and the snake stays out of matchmaking.
        run_sweep(&app_state).await?;
        let status = snake_health_status::get(&pool, battlesnake_id)
            .await?
            .expect("row exists");
        assert_eq!(status.consecutive_failures, threshold);
        assert_eq!(status.consecutive_successes, 0);
        assert!(status.deactivated_at.is_some());

        Ok(())
    }

    /// A deactivated snake whose server comes back is reactivated on its
    /// own after the recovery threshold — no owner action needed — and a
    /// pre-existing manual pause is left alone.
    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_reactivates_recovered_snake_after_threshold(pool: PgPool) -> cja::Result<()> {
        let server = healthy_snake_server().await;
        let (battlesnake_id, entry_id) = create_snake_on_leaderboard(&pool, &server.uri()).await?;
        let app_state = AppState::test_from_pool(pool.clone());
        let threshold = app_state.config.snake_health_recovery_threshold;
        assert!(threshold >= 2, "test assumes a multi-sweep threshold");

        // The sweeper pulled this snake while its server was down.
        snake_health_status::record_failure(&pool, battlesnake_id, "was down").await?;
        assert!(snake_health_status::deactivate(&pool, battlesnake_id).await?);
        assert_eq!(
            entry_disabled(&pool, entry_id).await?.as_deref(),
            Some(snake_health_status::DISABLED_REASON_HEALTH)
        );

        // Healthy sweeps below the threshold build the streak but don't
        // reactivate. (Age the last check between sweeps: recovery counting
        // is spacing-gated exactly like failure counting.)
        for expected_successes in 1..threshold {
            age_last_check(&pool, battlesnake_id).await?;
            run_sweep(&app_state).await?;
            let status = snake_health_status::get(&pool, battlesnake_id)
                .await?
                .expect("row exists");
            assert_eq!(status.consecutive_successes, expected_successes);
            assert!(status.deactivated_at.is_some());
            assert_eq!(
                entry_disabled(&pool, entry_id).await?.as_deref(),
                Some(snake_health_status::DISABLED_REASON_HEALTH)
            );
        }

        // A piled-up sweep inside the spacing window must not fake the last
        // step of the streak.
        run_sweep(&app_state).await?;
        let status = snake_health_status::get(&pool, battlesnake_id)
            .await?
            .expect("row exists");
        assert_eq!(status.consecutive_successes, threshold - 1);
        assert!(status.deactivated_at.is_some());

        // The spaced sweep that reaches the threshold puts it back.
        age_last_check(&pool, battlesnake_id).await?;
        run_sweep(&app_state).await?;
        assert_eq!(entry_disabled(&pool, entry_id).await?, None);
        let status = snake_health_status::get(&pool, battlesnake_id)
            .await?
            .expect("row exists");
        assert!(status.deactivated_at.is_none());
        assert_eq!(status.consecutive_successes, 0);
        assert_eq!(status.consecutive_failures, 0);

        Ok(())
    }

    /// A failed recovery probe restarts the healthy streak from zero.
    #[sqlx::test(migrations = "../migrations")]
    async fn failed_recovery_probe_resets_the_streak(pool: PgPool) -> cja::Result<()> {
        let server = broken_snake_server().await;
        let (battlesnake_id, entry_id) = create_snake_on_leaderboard(&pool, &server.uri()).await?;
        let app_state = AppState::test_from_pool(pool.clone());

        snake_health_status::record_failure(&pool, battlesnake_id, "was down").await?;
        assert!(snake_health_status::deactivate(&pool, battlesnake_id).await?);

        // One healthy-looking streak step recorded earlier…
        age_last_check(&pool, battlesnake_id).await?;
        assert_eq!(
            snake_health_status::record_recovery_success(&pool, battlesnake_id).await?,
            1
        );

        // …then a sweep against the still-broken server wipes it.
        age_last_check(&pool, battlesnake_id).await?;
        run_sweep(&app_state).await?;
        let status = snake_health_status::get(&pool, battlesnake_id)
            .await?
            .expect("row exists");
        assert_eq!(status.consecutive_successes, 0);
        assert!(status.deactivated_at.is_some());
        assert_eq!(
            entry_disabled(&pool, entry_id).await?.as_deref(),
            Some(snake_health_status::DISABLED_REASON_HEALTH)
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_resets_streak_when_snake_recovers(pool: PgPool) -> cja::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"apiversion":"1"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"move":"up"}"#))
            .mount(&server)
            .await;

        let (battlesnake_id, entry_id) = create_snake_on_leaderboard(&pool, &server.uri()).await?;
        let app_state = AppState::test_from_pool(pool.clone());

        // The snake was flaky earlier but never crossed the threshold…
        snake_health_status::record_failure(&pool, battlesnake_id, "was down").await?;

        // …and a healthy sweep wipes the streak.
        run_sweep(&app_state).await?;
        let status = snake_health_status::get(&pool, battlesnake_id)
            .await?
            .expect("row exists");
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.last_failure.is_none());
        assert_eq!(entry_disabled(&pool, entry_id).await?, None);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn sweep_skips_snakes_whose_only_board_is_disabled(pool: PgPool) -> cja::Result<()> {
        let server = broken_snake_server().await;
        let (battlesnake_id, _entry_id) = create_snake_on_leaderboard(&pool, &server.uri()).await?;

        // Retire the leaderboard: the matchmaker no longer draws from it,
        // so the sweeper must not probe (or ever deactivate) this snake.
        sqlx::query!("UPDATE leaderboards SET disabled_at = NOW()")
            .execute(&pool)
            .await?;

        let app_state = AppState::test_from_pool(pool.clone());
        run_sweep(&app_state).await?;

        assert!(
            snake_health_status::get(&pool, battlesnake_id)
                .await?
                .is_none(),
            "snake on a disabled leaderboard must not be probed"
        );

        Ok(())
    }
}
