use sqlx::{PgPool, Row, migrate::Migrator};
use std::path::Path;
use uuid::Uuid;

async fn migrate_before(pool: &PgPool, cutoff: &str) -> Result<(), Box<dyn std::error::Error>> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../migrations");
    let scratch = tempfile::tempdir()?;
    for file in std::fs::read_dir(source)? {
        let file = file?;
        let name = file.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".sql") && &name_str[..14] < cutoff {
            std::fs::copy(file.path(), scratch.path().join(name))?;
        }
    }
    Migrator::new(scratch.path()).await?.run(pool).await?;
    Ok(())
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    let statement = match table {
        "leaderboards" => "SELECT count(*) FROM leaderboards",
        "leaderboard_entries" => "SELECT count(*) FROM leaderboard_entries",
        "leaderboard_games" => "SELECT count(*) FROM leaderboard_games",
        "leaderboard_game_results" => "SELECT count(*) FROM leaderboard_game_results",
        "leaderboard_entries WHERE season_id IS NULL" => {
            "SELECT count(*) FROM leaderboard_entries WHERE season_id IS NULL"
        }
        "leaderboard_games WHERE season_id IS NULL" => {
            "SELECT count(*) FROM leaderboard_games WHERE season_id IS NULL"
        }
        "leaderboard_seasons" => "SELECT count(*) FROM leaderboard_seasons",
        _ => panic!("unhandled count target: {table}"),
    };
    sqlx::query_scalar::<_, i64>(statement)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = false)]
async fn historical_backfill_preserves_rows_and_values(pool: PgPool) {
    migrate_before(&pool, "20261121120000").await.unwrap();
    let before_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns WHERE table_name IN ('leaderboard_entries', 'leaderboard_games') AND column_name = 'season_id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before_columns, 0);

    let league: Uuid = sqlx::query_scalar("INSERT INTO leaderboards (name, created_at) VALUES ('Historical', '2025-01-01') RETURNING leaderboard_id")
        .fetch_one(&pool).await.unwrap();
    let empty: Uuid = sqlx::query_scalar(
        "INSERT INTO leaderboards (name) VALUES ('Empty') RETURNING leaderboard_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar("INSERT INTO users (external_github_id, github_login) VALUES (2636, 'season-test') RETURNING user_id")
        .fetch_one(&pool).await.unwrap();
    let snake: Uuid = sqlx::query_scalar("INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Fixture Snake', 'https://example.com') RETURNING battlesnake_id")
        .bind(user).fetch_one(&pool).await.unwrap();
    let entry: Uuid = sqlx::query_scalar(
        "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, created_at, mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes) VALUES ($1, $2, '2024-01-01', 31.5, 7.25, 12.75, 4, 3, 1) RETURNING leaderboard_entry_id",
    ).bind(league).bind(snake).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO games (board_size, game_type, status, created_at) SELECT '11x11', 'Standard', 'finished', '2023-01-01'::timestamptz FROM generate_series(1, 10001)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id, created_at) SELECT $1, game_id, '2022-01-01' FROM games")
        .bind(league).execute(&pool).await.unwrap();
    let game: Uuid =
        sqlx::query_scalar("SELECT leaderboard_game_id FROM leaderboard_games LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("INSERT INTO leaderboard_game_results (leaderboard_game_id, leaderboard_entry_id, placement, mu_before, mu_after, sigma_before, sigma_after, display_score_change) VALUES ($1, $2, 1, 25.0, 31.5, 8.333, 7.25, 12.75)")
        .bind(game).bind(entry).execute(&pool).await.unwrap();

    let tables = [
        "leaderboards",
        "leaderboard_entries",
        "leaderboard_games",
        "leaderboard_game_results",
    ];
    let before: Vec<i64> = futures::future::join_all(tables.iter().map(|t| count(&pool, t))).await;
    migrate_before(&pool, "20261121120500").await.unwrap();
    // Lock the newest game by UUID: with 10,001 games it lands in the second
    // and final game batch, so the entry batch and the first 10,000-game
    // batch commit before the procedure aborts on this row. That exercises
    // resume-after-partial-commits, not just a first-batch abort.
    let late_game: Uuid = sqlx::query_scalar(
        "SELECT leaderboard_game_id FROM leaderboard_games ORDER BY leaderboard_game_id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT leaderboard_game_id FROM leaderboard_games WHERE leaderboard_game_id = $1 FOR UPDATE")
        .bind(late_game).execute(&mut *blocker).await.unwrap();
    let failed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        sqlx::query("CALL backfill_leaderboard_seasons()")
            .execute(&pool)
            .await
    })
    .await
    .expect("blocked backfill call must finish within its lock timeout")
    .unwrap_err();
    blocker.rollback().await.unwrap();
    let failure = failed.to_string();
    assert!(failure.contains("lock timeout"), "{failure}");
    // Earlier batches committed before the abort: the entry is linked and
    // exactly the locked row is still missing its season.
    assert_eq!(
        count(&pool, "leaderboard_entries WHERE season_id IS NULL").await,
        0
    );
    assert_eq!(
        count(&pool, "leaderboard_games WHERE season_id IS NULL").await,
        1
    );
    let still_null: Option<Uuid> = sqlx::query_scalar(
        "SELECT season_id FROM leaderboard_games WHERE leaderboard_game_id = $1",
    )
    .bind(late_game)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(still_null.is_none());
    // Retrying the unapplied migration resumes past the committed batches.
    sqlx::migrate!("../migrations").run(&pool).await.unwrap();
    let after: Vec<i64> = futures::future::join_all(tables.iter().map(|t| count(&pool, t))).await;
    assert_eq!(after, before);
    let seasons =
        sqlx::query("SELECT leaderboard_id, name, starts_at, ends_at FROM leaderboard_seasons")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(seasons.len() as i64, count(&pool, "leaderboards").await);
    assert!(seasons.iter().all(|s| {
        s.get::<String, _>("name") == "Preseason"
            && s.get::<Option<chrono::DateTime<chrono::Utc>>, _>("ends_at")
                .is_none()
    }));
    let start: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT starts_at FROM leaderboard_seasons WHERE leaderboard_id = $1")
            .bind(league)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        start
            <= "2022-01-01T00:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
    );
    assert_eq!(
        count(&pool, "leaderboard_entries WHERE season_id IS NULL").await,
        0
    );
    assert_eq!(
        count(&pool, "leaderboard_games WHERE season_id IS NULL").await,
        0
    );
    let row = sqlx::query("SELECT mu, sigma, display_score, games_played, first_place_finishes, non_first_finishes, leaderboard_entry_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1")
        .bind(entry).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<Uuid, _>("leaderboard_entry_id"), entry);
    assert_eq!(
        (
            row.get::<f64, _>("mu"),
            row.get::<f64, _>("sigma"),
            row.get::<f64, _>("display_score")
        ),
        (31.5, 7.25, 12.75)
    );
    assert_eq!(
        (
            row.get::<i32, _>("games_played"),
            row.get::<i32, _>("first_place_finishes"),
            row.get::<i32, _>("non_first_finishes")
        ),
        (4, 3, 1)
    );
    let result = sqlx::query("SELECT r.placement, r.mu_before, r.mu_after, r.sigma_before, r.sigma_after, r.display_score_change, g.season_id = e.season_id AS same_season FROM leaderboard_game_results r JOIN leaderboard_games g USING (leaderboard_game_id) JOIN leaderboard_entries e USING (leaderboard_entry_id)")
        .fetch_one(&pool).await.unwrap();
    assert!(result.get::<bool, _>("same_season"));
    assert_eq!(result.get::<i32, _>("placement"), 1);
    assert_eq!(
        (
            result.get::<f64, _>("mu_before"),
            result.get::<f64, _>("mu_after"),
            result.get::<f64, _>("sigma_before"),
            result.get::<f64, _>("sigma_after"),
            result.get::<f64, _>("display_score_change")
        ),
        (25.0, 31.5, 8.333, 7.25, 12.75)
    );
    let season: Uuid =
        sqlx::query_scalar("SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1")
            .bind(league)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        sqlx::query("DELETE FROM leaderboard_seasons WHERE season_id = $1")
            .bind(season)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM leaderboards WHERE leaderboard_id = $1")
        .bind(league)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count(&pool, "leaderboard_entries").await, 0);
    assert_eq!(count(&pool, "leaderboard_games").await, 0);
    assert_eq!(count(&pool, "leaderboard_game_results").await, 0);
    assert_eq!(count(&pool, "leaderboard_seasons").await, before[0] - 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM leaderboard_seasons WHERE leaderboard_id = $1"
        )
        .bind(empty)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "../migrations")]
async fn seasons_enforce_ranges_and_keep_old_insert_shape(pool: PgPool) {
    let league: Uuid = sqlx::query_scalar(
        "INSERT INTO leaderboards (name) VALUES ('Season Test') RETURNING leaderboard_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let preseason: Uuid = sqlx::query_scalar("SELECT season_id FROM leaderboard_seasons WHERE leaderboard_id = $1 AND name = 'Preseason'")
        .bind(league).fetch_one(&pool).await.unwrap();
    let user: Uuid = sqlx::query_scalar("INSERT INTO users (external_github_id, github_login) VALUES (2637, 'season-test') RETURNING user_id")
        .fetch_one(&pool).await.unwrap();
    let snake: Uuid = sqlx::query_scalar("INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Fixture Snake', 'https://example.com') RETURNING battlesnake_id")
        .bind(user).fetch_one(&pool).await.unwrap();
    let entry: Uuid = sqlx::query_scalar("INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) VALUES ($1, $2) RETURNING leaderboard_entry_id")
        .bind(league).bind(snake).fetch_one(&pool).await.unwrap();
    let game: Uuid = sqlx::query_scalar("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'finished') RETURNING game_id")
        .fetch_one(&pool).await.unwrap();
    let link: Uuid = sqlx::query_scalar("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2) RETURNING leaderboard_game_id")
        .bind(league).bind(game).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO leaderboard_game_results (leaderboard_game_id, leaderboard_entry_id, placement, mu_before, mu_after, sigma_before, sigma_after, display_score_change) VALUES ($1, $2, 1, 25, 26, 8.333, 8, 1)")
        .bind(link).bind(entry).execute(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT season_id FROM leaderboard_entries WHERE leaderboard_entry_id = $1"
        )
        .bind(entry)
        .fetch_one(&pool)
        .await
        .unwrap(),
        preseason
    );
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT season_id FROM leaderboard_games WHERE leaderboard_game_id = $1"
        )
        .bind(link)
        .fetch_one(&pool)
        .await
        .unwrap(),
        preseason
    );
    assert!(
        sqlx::query(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) VALUES ($1, $2)"
        )
        .bind(league)
        .bind(snake)
        .execute(&pool)
        .await
        .is_err()
    );
    sqlx::query("UPDATE leaderboard_seasons SET ends_at = '2030-01-01' WHERE season_id = $1")
        .bind(preseason)
        .execute(&pool)
        .await
        .unwrap();
    let adjacent: Uuid = sqlx::query_scalar("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at) VALUES ($1, 'Next', '2030-01-01', '2031-01-01') RETURNING season_id")
        .bind(league).fetch_one(&pool).await.unwrap();
    assert!(sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at) VALUES ($1, 'Overlap', '2029-12-31', '2030-02-01')").bind(league).execute(&pool).await.is_err());
    assert!(sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at) VALUES ($1, 'Open', '2030-01-01', NULL)").bind(league).execute(&pool).await.is_err());
    assert!(sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at) VALUES ($1, 'Empty', '2032-01-01', '2032-01-01')").bind(league).execute(&pool).await.is_err());
    assert!(sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at) VALUES ($1, 'Reverse', '2033-01-01', '2032-01-01')").bind(league).execute(&pool).await.is_err());
    assert!(
        sqlx::query("UPDATE leaderboard_seasons SET starts_at = '2029-12-01' WHERE season_id = $1")
            .bind(adjacent)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at) VALUES ($1, 'Open ended', '2031-01-01')")
        .bind(league).execute(&pool).await.unwrap();
    assert!(sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at) VALUES ($1, 'Second open ended', '2032-01-01')")
        .bind(league).execute(&pool).await.is_err());
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO leaderboards (name) VALUES ('Other') RETURNING leaderboard_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE leaderboard_seasons SET ends_at = '2030-01-01' WHERE leaderboard_id = $1")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO leaderboard_seasons (leaderboard_id, name, starts_at, ends_at) VALUES ($1, 'Same range', '2030-01-01', '2031-01-01')")
        .bind(other).execute(&pool).await.unwrap();
    // A gap and a future-only league are valid even though inserts there must fail.
    sqlx::query(
        "UPDATE leaderboard_seasons SET starts_at = now() - interval '2 days', ends_at = now() - interval '1 day' WHERE season_id = $1",
    )
    .bind(preseason)
    .execute(&pool)
    .await
    .unwrap();
    let gap_snake: Uuid = sqlx::query_scalar("INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, 'Gap Snake', 'https://example.com') RETURNING battlesnake_id")
        .bind(user).fetch_one(&pool).await.unwrap();
    let gap_error = sqlx::query(
        "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) VALUES ($1, $2)",
    )
    .bind(league)
    .bind(gap_snake)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        gap_error.to_string().contains("no active season"),
        "{gap_error}"
    );
    let gap_game: Uuid = sqlx::query_scalar("INSERT INTO games (board_size, game_type, status) VALUES ('11x11', 'Standard', 'waiting') RETURNING game_id")
        .fetch_one(&pool).await.unwrap();
    let game_error =
        sqlx::query("INSERT INTO leaderboard_games (leaderboard_id, game_id) VALUES ($1, $2)")
            .bind(league)
            .bind(gap_game)
            .execute(&pool)
            .await
            .unwrap_err();
    assert!(
        game_error.to_string().contains("no active season"),
        "{game_error}"
    );
    let future: Uuid = sqlx::query_scalar("INSERT INTO leaderboards (name, created_at) VALUES ('Future only', '2035-01-01') RETURNING leaderboard_id")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM leaderboard_seasons WHERE leaderboard_id = $1 AND starts_at > now()")
        .bind(future).fetch_one(&pool).await.unwrap(), 1);
    let future_error = sqlx::query(
        "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) VALUES ($1, $2)",
    )
    .bind(future)
    .bind(gap_snake)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        future_error.to_string().contains("no active season"),
        "{future_error}"
    );
    let valid: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_index WHERE indexrelid IN ('leaderboard_entries_season_snake_idx'::regclass, 'leaderboard_games_season_created_idx'::regclass) AND indisvalid")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(valid, 2);
    let validated: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conname IN ('leaderboard_entries_season_required', 'leaderboard_games_season_required', 'leaderboard_entries_season_league_fk', 'leaderboard_games_season_league_fk') AND convalidated")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(validated, 4);
}

/// `20261121130100` (registration backfill + sync trigger), re-applied on its
/// own over legacy entries: every entry gets exactly one registration carrying
/// its membership and health state, and nothing is lost or rewritten.
#[sqlx::test(migrations = false)]
async fn registration_backfill_carries_entry_state(pool: PgPool) {
    migrate_before(&pool, "99999999999999").await.unwrap();

    let user: Uuid = sqlx::query_scalar("INSERT INTO users (external_github_id, github_login) VALUES (2660, 'registration-backfill') RETURNING user_id")
        .fetch_one(&pool).await.unwrap();
    let mut leagues = Vec::new();
    for name in ["Backfill A", "Backfill B"] {
        leagues.push(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO leaderboards (name) VALUES ($1) RETURNING leaderboard_id",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap(),
        );
    }
    let mut snakes = Vec::new();
    for name in ["Backfill Active", "Backfill Health", "Backfill Retired"] {
        snakes.push(
            sqlx::query_scalar::<_, Uuid>("INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id")
                .bind(user)
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap(),
        );
    }
    // Legacy-shaped writes: raw entry inserts, then membership/health updates.
    for league in &leagues {
        for snake in &snakes {
            sqlx::query("INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id, mu, display_score, games_played) VALUES ($1, $2, 27.5, 3.25, 5)")
                .bind(league)
                .bind(snake)
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    sqlx::query("UPDATE leaderboard_entries SET disabled_at = '2026-11-01T00:00:00Z', disabled_reason = 'health', health_consecutive_failures = 3, health_consecutive_successes = 1, health_last_checked_at = '2026-11-02T00:00:00Z', health_last_failure = 'POST /move: boom' WHERE battlesnake_id = $1 AND leaderboard_id = $2")
        .bind(snakes[1])
        .bind(leagues[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE leaderboard_entries SET disabled_at = '2026-11-03T00:00:00Z', disabled_reason = 'retired' WHERE battlesnake_id = $1")
        .bind(snakes[2])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE leaderboard_entries SET health_consecutive_failures = 2, health_last_failure = 'slow' WHERE battlesnake_id = $1 AND leaderboard_id = $2")
        .bind(snakes[0])
        .bind(leagues[1])
        .execute(&pool)
        .await
        .unwrap();

    // Rewind to just before the backfill migration (its table stays).
    sqlx::raw_sql(
        "DROP TRIGGER sync_entry_state_to_registration ON leaderboard_entries;
         DROP FUNCTION sync_entry_state_to_registration();
         DELETE FROM leaderboard_registrations;
         DELETE FROM _sqlx_migrations WHERE version = 20261121130100;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let entries_before: Vec<(Uuid, f64, f64, i32)> = sqlx::query_as(
        "SELECT leaderboard_entry_id, mu, display_score, games_played FROM leaderboard_entries ORDER BY leaderboard_entry_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(entries_before.len(), 6);

    migrate_before(&pool, "99999999999999").await.unwrap();

    let reapplied: bool =
        sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version = 20261121130100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(reapplied);
    // The 20261121130000 table objects persisted through the rewind.
    let objects: Vec<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes WHERE tablename = 'leaderboard_registrations' AND indexname = 'leaderboard_registrations_battlesnake_idx'
         UNION ALL
         SELECT tgname::text FROM pg_trigger WHERE tgname IN ('update_leaderboard_registrations_updated_at', 'sync_entry_state_to_registration')
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        objects,
        vec![
            "leaderboard_registrations_battlesnake_idx",
            "sync_entry_state_to_registration",
            "update_leaderboard_registrations_updated_at",
        ]
    );

    // Nothing lost or rewritten on the entries.
    let entries_after: Vec<(Uuid, f64, f64, i32)> = sqlx::query_as(
        "SELECT leaderboard_entry_id, mu, display_score, games_played FROM leaderboard_entries ORDER BY leaderboard_entry_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(entries_after, entries_before);

    // Exactly one registration per entry, carrying its state.
    let unmatched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM leaderboard_entries le
         WHERE (SELECT count(*) FROM leaderboard_registrations r
                WHERE r.leaderboard_id = le.leaderboard_id AND r.battlesnake_id = le.battlesnake_id) <> 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unmatched, 0, "every entry has exactly one registration");
    let registrations: i64 = sqlx::query_scalar("SELECT count(*) FROM leaderboard_registrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(registrations, 6);
    let differing: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM leaderboard_entries le
         JOIN leaderboard_registrations r
           ON r.leaderboard_id = le.leaderboard_id AND r.battlesnake_id = le.battlesnake_id
         WHERE (le.disabled_at, le.disabled_reason, le.health_consecutive_failures,
                le.health_consecutive_successes, le.health_last_checked_at, le.health_last_failure)
               IS DISTINCT FROM
               (r.disabled_at, r.disabled_reason, r.health_consecutive_failures,
                r.health_consecutive_successes, r.health_last_checked_at, r.health_last_failure)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(differing, 0, "state carried verbatim");
    let health = sqlx::query(
        "SELECT disabled_reason, health_consecutive_failures, health_consecutive_successes, health_last_failure
         FROM leaderboard_registrations WHERE leaderboard_id = $1 AND battlesnake_id = $2",
    )
    .bind(leagues[0])
    .bind(snakes[1])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        health.get::<Option<String>, _>(0).as_deref(),
        Some("health")
    );
    assert_eq!(health.get::<i32, _>(1), 3);
    assert_eq!(health.get::<i32, _>(2), 1);
    assert_eq!(
        health.get::<Option<String>, _>(3).as_deref(),
        Some("POST /move: boom")
    );
    let retired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM leaderboard_registrations WHERE battlesnake_id = $1 AND disabled_reason = 'retired'",
    )
    .bind(snakes[2])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retired, 2);
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM leaderboard_registrations WHERE disabled_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 3, "the other three stay in rotation");
}
