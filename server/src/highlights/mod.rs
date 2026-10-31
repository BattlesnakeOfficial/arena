//! Weekly arena highlights digest (DEV-1993): computed weekly inside arena,
//! stored in `highlight_digests`/`highlight_sections`/`highlight_items`,
//! viewable by admins at `/admin/highlights`, and announced once to a
//! private Discord webhook.
//!
//! Invariants:
//! - The stored unit is the **aligned week** (7 days ending Saturday
//!   12:00:00 UTC), enforced by [`types::Week`] in code and CHECK
//!   constraints in the schema. Arbitrary windows exist only in the
//!   never-storing surfaces (admin preview, `highlights-preview` CLI).
//! - The Discord announcement is a **snapshot string rendered at store
//!   time**, sent verbatim on retry, at most once per week.

pub mod detectors;
pub mod extract;
pub mod render;
pub mod types;

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use crate::state::AppState;
pub use types::Week;
use types::{ComputedDigest, SectionKind};

pub use extract::compute_digest;

/// A digest row as loaded for the admin pages.
#[derive(Debug, Clone)]
pub struct DigestSummary {
    pub digest_id: Uuid,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub games_count: i32,
    pub duel_k: f64,
    pub four_snake_k: f64,
    pub discord_status: String,
    pub discord_posted_at: Option<DateTime<Utc>>,
    pub computed_at: DateTime<Utc>,
}

/// A stored section with its items.
#[derive(Debug, Clone)]
pub struct StoredSection {
    pub kind: SectionKind,
    pub ladder_name: Option<String>,
    pub position: i32,
    pub summary: String,
    pub items: Vec<StoredItem>,
}

#[derive(Debug, Clone)]
pub struct StoredItem {
    pub position: i32,
    pub game_id: Option<Uuid>,
    pub subject: Option<String>,
    pub value: Option<f64>,
    pub text: String,
}

/// The hourly cron tick: ensure the latest completed week's digest exists,
/// then run the Discord post pass. A stored digest is never recomputed.
pub async fn run_weekly(app_state: &AppState) -> cja::Result<()> {
    let week = Week::latest_completed(Utc::now());
    if digest_for_week(&app_state.db, &week).await?.is_none() {
        let started = std::time::Instant::now();
        let house = app_state.config.highlights_house();
        let digest = compute_digest(&app_state.db, &house, week.start, week.end).await?;
        let stored = store_digest(
            &app_state.db,
            &digest,
            &app_state.config.base_url,
            Utc::now(),
        )
        .await?;
        tracing::info!(
            window_end = %week.end,
            games = digest.games_count,
            duel_k = digest.duel_k,
            four_snake_k = digest.four_snake_k,
            stored = stored.is_some(),
            elapsed_secs = started.elapsed().as_secs(),
            "Weekly highlights digest computed"
        );
    }
    process_discord_posts(app_state).await?;
    Ok(())
}

/// Store a computed digest (exactly once per `window_end`). `None` when the
/// week is already stored. Past-deadline weeks (window end + 7d ago or
/// older) land `abandoned` at insert — backfilled weeks are never
/// announceable.
pub async fn store_digest(
    db: &PgPool,
    digest: &ComputedDigest,
    base_url: &str,
    now: DateTime<Utc>,
) -> cja::Result<Option<Uuid>> {
    let digest_id = Uuid::new_v4();
    let message = render::discord_message(digest, &digest.picks, digest_id, base_url);
    let announcement_deadline = digest.window_end + chrono::Duration::days(7);
    let initial_status = if now >= announcement_deadline {
        "abandoned"
    } else {
        "pending"
    };

    let mut tx = db.begin().await?;
    let inserted = sqlx::query_scalar!(
        r#"
        INSERT INTO highlight_digests
            (digest_id, window_start, window_end, games_count,
             duel_calibration_k, four_snake_calibration_k,
             discord_status, discord_message)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (window_end) DO NOTHING
        RETURNING digest_id
        "#,
        digest_id,
        digest.window_start,
        digest.window_end,
        digest.games_count as i32,
        digest.duel_k,
        digest.four_snake_k,
        initial_status,
        message
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("highlights: failed to insert digest")?;
    let Some(digest_id) = inserted else {
        tx.commit().await?;
        return Ok(None);
    };

    for (position, section) in digest.sections.iter().enumerate() {
        let section_id = Uuid::new_v4();
        sqlx::query!(
            r#"
            INSERT INTO highlight_sections
                (section_id, digest_id, kind, ladder_id, position, summary)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            section_id,
            digest_id,
            section.kind.as_str(),
            section.ladder.as_ref().map(|l| l.leaderboard_id),
            position as i32,
            section.summary
        )
        .execute(&mut *tx)
        .await
        .wrap_err("highlights: failed to insert section")?;
        for (item_position, item) in section.items.iter().enumerate() {
            sqlx::query!(
                r#"
                INSERT INTO highlight_items
                    (item_id, section_id, position, game_id, subject, value, text)
                VALUES ($1, $2, $3, $4, $5, $6, $7)
                "#,
                Uuid::new_v4(),
                section_id,
                item_position as i32,
                item.game_id,
                item.subject,
                item.value,
                item.text
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(Some(digest_id))
}

fn row_to_summary(row: DigestRow) -> DigestSummary {
    DigestSummary {
        digest_id: row.digest_id,
        window_start: row.window_start,
        window_end: row.window_end,
        games_count: row.games_count,
        duel_k: row.duel_calibration_k,
        four_snake_k: row.four_snake_calibration_k,
        discord_status: row.discord_status,
        discord_posted_at: row.discord_posted_at,
        computed_at: row.computed_at,
    }
}

struct DigestRow {
    digest_id: Uuid,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    games_count: i32,
    duel_calibration_k: f64,
    four_snake_calibration_k: f64,
    discord_status: String,
    discord_posted_at: Option<DateTime<Utc>>,
    computed_at: DateTime<Utc>,
}

pub async fn digest_for_week(db: &PgPool, week: &Week) -> cja::Result<Option<DigestSummary>> {
    let row = sqlx::query_as!(
        DigestRow,
        r#"
        SELECT digest_id, window_start, window_end, games_count,
               duel_calibration_k, four_snake_calibration_k, discord_status,
               discord_posted_at, computed_at
        FROM highlight_digests
        WHERE window_end = $1
        "#,
        week.end
    )
    .fetch_optional(db)
    .await
    .wrap_err("highlights: failed to load digest for week")?;
    Ok(row.map(row_to_summary))
}

pub async fn list_digests(db: &PgPool) -> cja::Result<Vec<DigestSummary>> {
    let rows = sqlx::query_as!(
        DigestRow,
        r#"
        SELECT digest_id, window_start, window_end, games_count,
               duel_calibration_k, four_snake_calibration_k, discord_status,
               discord_posted_at, computed_at
        FROM highlight_digests
        ORDER BY window_end DESC
        "#
    )
    .fetch_all(db)
    .await
    .wrap_err("highlights: failed to list digests")?;
    Ok(rows.into_iter().map(row_to_summary).collect())
}

/// Load one digest with all its sections and items (two round trips — no
/// per-section queries).
pub async fn load_digest(
    db: &PgPool,
    digest_id: Uuid,
) -> cja::Result<Option<(DigestSummary, Vec<StoredSection>)>> {
    let row = sqlx::query_as!(
        DigestRow,
        r#"
        SELECT digest_id, window_start, window_end, games_count,
               duel_calibration_k, four_snake_calibration_k, discord_status,
               discord_posted_at, computed_at
        FROM highlight_digests
        WHERE digest_id = $1
        "#,
        digest_id
    )
    .fetch_optional(db)
    .await
    .wrap_err("highlights: failed to load digest")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let summary = row_to_summary(row);

    let section_rows = sqlx::query!(
        r#"
        SELECT s.kind, l.name AS "ladder_name?", s.position, s.summary,
               i.position AS "item_position?", i.game_id AS "item_game_id?",
               i.subject AS "item_subject?", i.value AS "item_value?", i.text AS "item_text?"
        FROM highlight_sections s
        LEFT JOIN leaderboards l ON l.leaderboard_id = s.ladder_id
        LEFT JOIN highlight_items i USING (section_id)
        WHERE s.digest_id = $1
        ORDER BY s.position, i.position
        "#,
        digest_id
    )
    .fetch_all(db)
    .await
    .wrap_err("highlights: failed to load digest sections")?;

    let mut sections: Vec<StoredSection> = Vec::new();
    for row in section_rows {
        let Some(kind) = SectionKind::parse(&row.kind) else {
            tracing::warn!(kind = %row.kind, "highlights: unknown section kind in storage");
            continue;
        };
        let section = match sections
            .iter_mut()
            .find(|s| s.kind == kind && s.ladder_name == row.ladder_name)
        {
            Some(section) => section,
            None => {
                sections.push(StoredSection {
                    kind,
                    ladder_name: row.ladder_name.clone(),
                    position: row.position,
                    summary: row.summary.clone(),
                    items: Vec::new(),
                });
                match sections.last_mut() {
                    Some(section) => section,
                    // Unreachable: pushed directly above.
                    None => continue,
                }
            }
        };
        if let (Some(position), Some(text)) = (row.item_position, row.item_text) {
            section.items.push(StoredItem {
                position,
                game_id: row.item_game_id,
                subject: row.item_subject,
                value: row.item_value,
                text,
            });
        }
    }
    Ok(Some((summary, sections)))
}

/// Send pending Discord announcements. Each pending digest is claimed
/// with `FOR UPDATE SKIP LOCKED` inside its own transaction **before** the
/// webhook call, so two overlapping passes (duplicate cron enqueues are
/// routine with cja) never both send the same week's announcement: the
/// second pass skips the locked row instead of posting it. The lock is
/// held across the send, bounded by the webhook client's 10 s timeout.
/// Claimed rows: past the announcement deadline → abandoned; no webhook
/// → skipped (terminal at first evaluation); otherwise send the stored
/// snapshot — success posts, any failure rolls back and stays pending for
/// the hourly retry.
///
/// Residual double-send window, inherent to a webhook with no idempotency
/// key: if the send succeeds but the commit fails, the row stays pending
/// and the next tick re-sends. This is the only remaining path and it
/// requires the Neon commit to fail after a successful HTTP POST.
pub async fn process_discord_posts(app_state: &AppState) -> cja::Result<()> {
    let pending_ids: Vec<Uuid> = sqlx::query_scalar!(
        "SELECT digest_id FROM highlight_digests WHERE discord_status = 'pending' ORDER BY window_end"
    )
    .fetch_all(&app_state.db)
    .await
    .wrap_err("highlights: failed to load pending digests")?;

    for digest_id in pending_ids {
        // Claim the row: a concurrent pass holding it sees SKIP LOCKED
        // return nothing here and moves on. Both passes process digests
        // in the same `window_end` order, so lock acquisition is
        // deadlock-free.
        let mut tx = app_state
            .db
            .begin()
            .await
            .wrap_err("highlights: failed to begin post claim")?;
        let Some(row) = sqlx::query!(
            "SELECT window_end, discord_message FROM highlight_digests \
             WHERE digest_id = $1 AND discord_status = 'pending' \
             FOR UPDATE SKIP LOCKED",
            digest_id
        )
        .fetch_optional(&mut *tx)
        .await
        .wrap_err("highlights: failed to claim pending digest")?
        else {
            // Claimed by a concurrent pass, or no longer pending.
            continue;
        };

        // The row is locked: the plain UPDATE below cannot race another
        // status flip, so no predicate guard is needed.
        let deadline = row.window_end + chrono::Duration::days(7);
        if Utc::now() >= deadline {
            sqlx::query!(
                "UPDATE highlight_digests SET discord_status = 'abandoned' WHERE digest_id = $1",
                digest_id
            )
            .execute(&mut *tx)
            .await
            .wrap_err("highlights: failed to abandon digest post")?;
            tx.commit()
                .await
                .wrap_err("highlights: failed to commit digest abandonment")?;
            tracing::error!(
                digest_id = %digest_id,
                window_end = %row.window_end,
                "Highlights Discord post abandoned: announcement deadline passed"
            );
            continue;
        }
        if !app_state.discord_highlights.is_enabled() {
            sqlx::query!(
                "UPDATE highlight_digests SET discord_status = 'skipped' WHERE digest_id = $1",
                digest_id
            )
            .execute(&mut *tx)
            .await
            .wrap_err("highlights: failed to skip digest post")?;
            tx.commit()
                .await
                .wrap_err("highlights: failed to commit digest skip")?;
            tracing::info!(
                digest_id = %digest_id,
                window_end = %row.window_end,
                "No highlights webhook configured; weekly post skipped"
            );
            continue;
        }
        match app_state
            .discord_highlights
            .send(&row.discord_message)
            .await
        {
            Ok(()) => {
                sqlx::query!(
                    "UPDATE highlight_digests SET discord_status = 'posted', \
                     discord_posted_at = NOW() WHERE digest_id = $1",
                    digest_id
                )
                .execute(&mut *tx)
                .await
                .wrap_err("highlights: failed to mark digest posted")?;
                tx.commit()
                    .await
                    .wrap_err("highlights: failed to commit digest post")?;
                tracing::info!(digest_id = %digest_id, "Highlights Discord post sent");
            }
            Err(error) => {
                tx.rollback().await.ok();
                tracing::warn!(
                    digest_id = %digest_id,
                    error = %format!("{error:#}"),
                    "Highlights Discord post failed; will retry next tick"
                );
            }
        }
    }
    Ok(())
}

/// Hourly cron job: ensure the latest completed week's digest is stored,
/// then run the Discord post pass.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct WeeklyHighlightsJob;

#[async_trait::async_trait]
impl cja::jobs::Job<AppState> for WeeklyHighlightsJob {
    const NAME: &'static str = "WeeklyHighlightsJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        run_weekly(&app_state).await
    }
}

/// `arena highlights-preview <start> <end>` — compute an arbitrary window
/// against DATABASE_URL, print the markdown digest to stdout and the
/// frames-missing banner (if any) to stderr. Never stores, never posts.
pub async fn preview_main() -> cja::Result<()> {
    let mut args = std::env::args().skip(2);
    let (Some(start), Some(end)) = (args.next(), args.next()) else {
        return Err(color_eyre::eyre::eyre!(
            "usage: arena highlights-preview <start> <end>   (RFC 3339, e.g. 2026-10-24T12:00:00Z)"
        ));
    };
    let parse = |value: String, label: &str| -> cja::Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&value)
            .map(|dt| dt.with_timezone(&Utc))
            .wrap_err_with(|| format!("invalid {label} timestamp {value:?}"))
    };
    let start = parse(start, "start")?;
    let end = parse(end, "end")?;
    if end <= start {
        return Err(color_eyre::eyre::eyre!("end must be after start"));
    }

    let config = crate::config::AppConfig::from_env()?;
    let database_url = std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set")?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                // The arena-neon-readonly role is not enforced read-only and
                // Neon's pooler rejects PGOPTIONS, so guard here.
                sqlx::query("SET default_transaction_read_only = on")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET statement_timeout = '120s'")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .wrap_err("highlights-preview: failed to connect to database")?;

    let house = config.highlights_house();
    let digest = compute_digest(&pool, &house, start, end).await?;
    if let Some(banner) = render::frames_missing_banner(&digest) {
        eprintln!("{banner}");
    }
    print!("{}", render::render_markdown(&digest, &config.base_url));
    pool.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlights::types::{DiscordPicks, HouseSnakeConfig, SectionKind};
    use sqlx::PgPool;
    use uuid::Uuid;

    // ------------------------------------------------------------ helpers

    fn w0() -> DateTime<Utc> {
        Week::latest_completed(Utc::now()).start
    }

    fn w1() -> DateTime<Utc> {
        Week::latest_completed(Utc::now()).end
    }

    async fn ladder_id(pool: &PgPool, name: &str) -> Uuid {
        sqlx::query_scalar("SELECT leaderboard_id FROM leaderboards WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn seed_user(pool: &PgPool, github_login: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ((SELECT COALESCE(MAX(external_github_id), 991000) + 1 FROM users), $1, '')
             RETURNING user_id",
        )
        .bind(github_login)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Seed one rated duel: user, two snakes, entries, game, results, and a
    /// final frame where `winner` (0 or 1) is one segment longer and the
    /// loser died head-to-head on the final turn.
    #[allow(clippy::too_many_arguments)]
    async fn seed_duel(
        pool: &PgPool,
        ladder_name: &str,
        owner_login: &str,
        snake_names: [&str; 2],
        created_at: DateTime<Utc>,
        winner: usize,
        mus: [f64; 2],
        final_turn: i32,
    ) -> (Uuid, [Uuid; 2]) {
        let user_id = seed_user(pool, owner_login).await;
        let ladder = ladder_id(pool, ladder_name).await;
        let game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO games (board_size, game_type, status, created_at)
             VALUES ('11x11', 'standard', 'finished', $1) RETURNING game_id",
        )
        .bind(created_at)
        .fetch_one(pool)
        .await
        .unwrap();
        let leaderboard_game_id: Uuid = sqlx::query_scalar(
            "INSERT INTO leaderboard_games (leaderboard_id, game_id, created_at)
             VALUES ($1, $2, $3) RETURNING leaderboard_game_id",
        )
        .bind(ladder)
        .bind(game_id)
        .bind(created_at)
        .fetch_one(pool)
        .await
        .unwrap();
        let mut gb_ids = [Uuid::nil(); 2];
        for (i, snake_name) in snake_names.iter().enumerate() {
            let snake_id: Uuid = sqlx::query_scalar(
                "INSERT INTO battlesnakes (user_id, name, url, color, head, tail)
                 VALUES ($1, $2, 'https://example.com', '#ff3d8a', 'bendr', 'bolt')
                 RETURNING battlesnake_id",
            )
            .bind(user_id)
            .bind(snake_name)
            .fetch_one(pool)
            .await
            .unwrap();
            let entry_id: Uuid = sqlx::query_scalar(
                "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id)
                 VALUES ($1, $2) RETURNING leaderboard_entry_id",
            )
            .bind(ladder)
            .bind(snake_id)
            .fetch_one(pool)
            .await
            .unwrap();
            let placement = if i == winner { 1 } else { 2 };
            gb_ids[i] = sqlx::query_scalar(
                "INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement)
                 VALUES ($1, $2, $3, $4) RETURNING game_battlesnake_id",
            )
            .bind(game_id)
            .bind(snake_id)
            .bind(entry_id)
            .bind(placement)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO leaderboard_game_results
                 (leaderboard_game_id, leaderboard_entry_id, placement,
                  mu_before, mu_after, sigma_before, sigma_after, display_score_change, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8)",
            )
            .bind(leaderboard_game_id)
            .bind(entry_id)
            .bind(placement)
            .bind(mus[i])
            .bind(mus[i] + 0.5)
            .bind(25.0 / 6.0)
            .bind(25.0 / 6.0)
            .bind(created_at)
            .execute(pool)
            .await
            .unwrap();
        }
        // Final frame: winner (length 2) vs loser (length 1, head-collision
        // on the final turn, eliminated by the winner).
        let loser = 1 - winner;
        let frame = serde_json::json!({
            "Turn": final_turn,
            "Snakes": [
                {"ID": gb_ids[0].to_string(), "Name": snake_names[0],
                 "Body": [{"X": 1, "Y": 1}, {"X": 1, "Y": 0}],
                 "Health": 40, "Death": null},
                {"ID": gb_ids[1].to_string(), "Name": snake_names[1],
                 "Body": [{"X": 9, "Y": 9}],
                 "Health": 20,
                 "Death": {"Cause": "head-collision", "Turn": final_turn,
                           "EliminatedBy": gb_ids[loser ^ 1].to_string()}},
            ],
            "Food": [], "Hazards": []
        });
        sqlx::query("INSERT INTO turns (game_id, turn_number, frame_data) VALUES ($1, $2, $3)")
            .bind(game_id)
            .bind(final_turn)
            .bind(frame)
            .execute(pool)
            .await
            .unwrap();
        (game_id, gb_ids)
    }

    async fn count_digests(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM highlight_digests")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // ------------------------------------------------- canonical section set

    #[sqlx::test(migrations = "../migrations")]
    async fn canonical_section_set_zero_game_window(pool: PgPool) {
        let digest = compute_digest(&pool, &HouseSnakeConfig::default(), w0(), w1())
            .await
            .unwrap();
        assert_eq!(digest.games_count, 0);
        // Fitted k over zero games: every ll equal → highest grid k.
        assert!((digest.duel_k - 4.0).abs() < 1e-9);

        let id = store_digest(&pool, &digest, "https://arena.example.com", Utc::now())
            .await
            .unwrap()
            .expect("stored");
        let (summary, sections) = load_digest(&pool, id).await.unwrap().unwrap();
        assert_eq!(summary.games_count, 0);
        assert_eq!(summary.discord_status, "pending");
        // 8 PerLadder kinds x 4 public ladders + hat_tricks x 3 non-duel
        // ladders + chicken/rivalries/ops_footnote on Duels + newcomers +
        // calibration (ladder NULL) = 40 sections.
        assert_eq!(
            sections.len(),
            40,
            "{:?}",
            sections.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>()
        );
        let per_ladder = sections
            .iter()
            .filter(|s| s.kind.scope() == types::SectionScope::PerLadder)
            .count();
        assert_eq!(per_ladder, 32);
        let ladderless = sections.iter().filter(|s| s.ladder_name.is_none()).count();
        assert_eq!(ladderless, 2); // newcomers + calibration
        for section in &sections {
            if section.kind == SectionKind::Calibration {
                assert!(!section.summary.starts_with("none this week"));
            } else {
                assert_eq!(section.summary, "none this week", "{:?}", section.kind);
            }
        }
    }

    // ------------------------------------------------- window edges

    #[sqlx::test(migrations = "../migrations")]
    async fn window_edge_inclusion_is_half_open(pool: PgPool) {
        let (at_start, _) = seed_duel(
            &pool,
            "Duels 11x11",
            "edge-owner",
            ["Edge In", "Edge Opponent"],
            w0(),
            0,
            [25.0, 25.0],
            10,
        )
        .await;
        let (at_end, _) = seed_duel(
            &pool,
            "Duels 11x11",
            "edge-owner-2",
            ["Edge Out", "Edge Opponent 2"],
            w1(),
            0,
            [25.0, 25.0],
            10,
        )
        .await;
        let digest = compute_digest(&pool, &HouseSnakeConfig::default(), w0(), w1())
            .await
            .unwrap();
        assert_eq!(digest.games_count, 1);
        let marathon_games: Vec<Uuid> = digest
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Marathons)
            .flat_map(|s| s.items.iter().filter_map(|i| i.game_id))
            .collect();
        assert!(marathon_games.contains(&at_start));
        assert!(!marathon_games.contains(&at_end));
    }

    // ------------------------------------------------- exactly-once

    #[sqlx::test(migrations = "../migrations")]
    async fn store_digest_is_exactly_once_and_misaligned_fails(pool: PgPool) {
        let digest = compute_digest(&pool, &HouseSnakeConfig::default(), w0(), w1())
            .await
            .unwrap();
        let first = store_digest(&pool, &digest, "https://arena.example.com", Utc::now())
            .await
            .unwrap();
        assert!(first.is_some());
        let second = store_digest(&pool, &digest, "https://arena.example.com", Utc::now())
            .await
            .unwrap();
        assert!(second.is_none());
        assert_eq!(count_digests(&pool).await, 1);

        // A misaligned window trips the schema backstop.
        let misaligned = types::ComputedDigest {
            window_start: w0() + chrono::Duration::hours(1),
            window_end: w1() + chrono::Duration::hours(1),
            ..digest.clone()
        };
        assert!(
            store_digest(&pool, &misaligned, "https://x", Utc::now())
                .await
                .is_err()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn run_weekly_twice_stores_one_digest(pool: PgPool) {
        let state = crate::state::AppState::test_from_pool(pool.clone());
        run_weekly(&state).await.unwrap();
        run_weekly(&state).await.unwrap();
        assert_eq!(count_digests(&pool).await, 1);
        // No webhook configured (test state): the pending post is skipped,
        // terminally, on the first pass.
        let status: String = sqlx::query_scalar("SELECT discord_status FROM highlight_digests")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "skipped");
    }

    // ------------------------------------------------- snapshot integrity

    #[sqlx::test(migrations = "../migrations")]
    async fn discord_message_snapshot_has_link_week_and_photo_finish(pool: PgPool) {
        let mid = w0() + chrono::Duration::hours(12);
        let (game_id, _) = seed_duel(
            &pool,
            "Duels 11x11",
            "photo-owner",
            ["Photo Winner", "Photo Loser"],
            mid,
            0,
            [25.0, 25.0],
            10,
        )
        .await;
        let digest = compute_digest(&pool, &HouseSnakeConfig::default(), w0(), w1())
            .await
            .unwrap();
        // The seeded duel is a photo finish (2 over 1 on the final turn).
        assert!(
            digest.picks.duel_photo_finish.is_some(),
            "seeded duel must be a photo finish"
        );
        let id = store_digest(&pool, &digest, "https://arena.example.com", Utc::now())
            .await
            .unwrap()
            .expect("stored");
        let message: String = sqlx::query_scalar(
            "SELECT discord_message FROM highlight_digests WHERE digest_id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(message.contains(&format!("/admin/highlights/{id}")));
        assert!(message.contains("Photo finish:"));
        assert!(message.contains("https://arena.example.com/games/"));
        // The pick's game is the seeded photo finish.
        assert!(
            digest
                .picks
                .duel_photo_finish
                .as_ref()
                .is_some_and(|pick| pick.game_id == Some(game_id))
        );
    }

    // ------------------------------------------------- post behavior

    async fn insert_pending_digest(
        pool: &PgPool,
        window_end: DateTime<Utc>,
        message: &str,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO highlight_digests
             (window_start, window_end, games_count, duel_calibration_k,
              four_snake_calibration_k, discord_status, discord_message)
             VALUES ($1 - INTERVAL '7 days', $1, 0, 1.0, 1.0, 'pending', $2)
             RETURNING digest_id",
        )
        .bind(window_end)
        .bind(message)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn status_of(pool: &PgPool, digest_id: Uuid) -> String {
        sqlx::query_scalar("SELECT discord_status FROM highlight_digests WHERE digest_id = $1")
            .bind(digest_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn post_success_marks_posted(pool: PgPool) {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .respond_with(ResponseTemplate::new(204))
                    .expect(1),
            )
            .await;
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.discord_highlights = crate::discord::DiscordNotifier::new(
            Some(crate::discord::test_webhook_url(&server.uri()).await),
            reqwest::Client::new(),
        );

        let digest_id = insert_pending_digest(&pool, w1(), "snapshot text").await;
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "posted");
        let posted_at: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT discord_posted_at FROM highlight_digests WHERE digest_id = $1",
        )
        .bind(digest_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(posted_at.is_some());
        // A second pass sends nothing more.
        process_discord_posts(&state).await.unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn post_failure_stays_pending_then_posts_once(pool: PgPool) {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        let fail = Mock::given(method("POST")).respond_with(ResponseTemplate::new(500));
        server.register(fail.expect(1)).await;
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.discord_highlights = crate::discord::DiscordNotifier::new(
            Some(crate::discord::test_webhook_url(&server.uri()).await),
            reqwest::Client::new(),
        );

        let digest_id = insert_pending_digest(&pool, w1(), "snapshot text").await;
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "pending");

        // Reset the server and mount 204 with exactly one expectation; the
        // retry posts once.
        server.reset().await;
        server
            .register(
                Mock::given(method("POST"))
                    .respond_with(ResponseTemplate::new(204))
                    .expect(1),
            )
            .await;
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "posted");
        server.verify().await;
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn post_unauthorized_stays_pending(pool: PgPool) {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .respond_with(ResponseTemplate::new(401))
                    .expect(1),
            )
            .await;
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.discord_highlights = crate::discord::DiscordNotifier::new(
            Some(crate::discord::test_webhook_url(&server.uri()).await),
            reqwest::Client::new(),
        );

        let digest_id = insert_pending_digest(&pool, w1(), "snapshot text").await;
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "pending");
        server.verify().await;
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn disabled_notifier_marks_skipped_not_posted(pool: PgPool) {
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        let digest_id = insert_pending_digest(&pool, w1(), "snapshot text").await;
        process_discord_posts(&state).await.unwrap();
        // The is_enabled() gate: explicitly skipped, never marked posted
        // (send() would silently Ok on a disabled notifier).
        assert_eq!(status_of(&pool, digest_id).await, "skipped");
        // Terminal: a later pass with a webhook still does not post it.
        let server = wiremock::MockServer::start().await;
        state.discord_highlights = crate::discord::DiscordNotifier::new(
            Some(crate::discord::test_webhook_url(&server.uri()).await),
            reqwest::Client::new(),
        );
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "skipped");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn past_deadline_pending_abandons_without_http(pool: PgPool) {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        // Any POST fails the mock's zero-expectation check.
        server
            .register(
                Mock::given(method("POST"))
                    .respond_with(ResponseTemplate::new(500))
                    .expect(0),
            )
            .await;
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.discord_highlights = crate::discord::DiscordNotifier::new(
            Some(crate::discord::test_webhook_url(&server.uri()).await),
            reqwest::Client::new(),
        );

        // Window ended 8 days ago: past the 7-day announcement deadline.
        let old_end = Utc::now() - chrono::Duration::days(8);
        // Align to a Saturday noon so the CHECK passes.
        let old_week = Week::ending(old_week_saturday(old_end), Utc::now()).unwrap();
        let digest_id = insert_pending_digest(&pool, old_week.end, "old snapshot").await;
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "abandoned");
        // Later passes: no further HTTP, still abandoned.
        process_discord_posts(&state).await.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "abandoned");
        server.verify().await;
    }

    /// The most recent Saturday-noon boundary at or before `at`.
    fn old_week_saturday(at: DateTime<Utc>) -> chrono::NaiveDate {
        Week::latest_completed(at).end.date_naive()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn past_deadline_backfill_lands_abandoned_at_insert(pool: PgPool) {
        // A week whose announcement deadline already passed stores directly
        // as abandoned — no error, no post attempt.
        let old = Week::latest_completed(Utc::now() - chrono::Duration::days(14));
        let digest = types::ComputedDigest {
            window_start: old.start,
            window_end: old.end,
            games_count: 0,
            games_without_frames: 0,
            per_ladder_counts: vec![],
            duel_k: 1.0,
            four_snake_k: 1.0,
            sections: vec![],
            picks: DiscordPicks::default(),
        };
        let id = store_digest(&pool, &digest, "https://x", Utc::now())
            .await
            .unwrap()
            .expect("stored");
        assert_eq!(status_of(&pool, id).await, "abandoned");
    }

    /// PR review round 1 pin: two concurrent post passes (duplicate cron
    /// enqueue with ARENA_JOB_WORKERS > 1) must not both send the same
    /// week's announcement — the task requires "at most once, ever".
    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_post_passes_send_at_most_once(pool: PgPool) {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        server
            .register(Mock::given(method("POST")).respond_with(
                ResponseTemplate::new(204).set_delay(std::time::Duration::from_millis(500)),
            ))
            .await;
        let mut state = crate::state::AppState::test_from_pool(pool.clone());
        state.discord_highlights = crate::discord::DiscordNotifier::new(
            Some(crate::discord::test_webhook_url(&server.uri()).await),
            reqwest::Client::new(),
        );

        let digest_id = insert_pending_digest(&pool, w1(), "snapshot text").await;
        let (a, b) = tokio::join!(process_discord_posts(&state), process_discord_posts(&state));
        a.unwrap();
        b.unwrap();
        assert_eq!(status_of(&pool, digest_id).await, "posted");
        let sent = server.received_requests().await.unwrap_or_default().len();
        assert_eq!(sent, 1, "the week's announcement was POSTed {sent} times");
    }

    /// A corrupt final-frame field must not prevent the entire weekly digest.
    /// The extraction contract says malformed snake frames are skipped.
    #[sqlx::test(migrations = "../migrations")]
    async fn malformed_final_frame_death_turn_does_not_abort_digest(pool: PgPool) {
        let (game_id, _) = seed_duel(
            &pool,
            "Duels 11x11",
            "malformed-frame-owner",
            ["Malformed Winner", "Malformed Loser"],
            w0() + chrono::Duration::hours(1),
            0,
            [25.0, 25.0],
            10,
        )
        .await;
        sqlx::query(
            "UPDATE turns SET frame_data = jsonb_set(frame_data, \
             '{Snakes,1,Death,Turn}', to_jsonb('not-an-integer'::text)) \
             WHERE game_id = $1",
        )
        .bind(game_id)
        .execute(&pool)
        .await
        .unwrap();

        let digest = compute_digest(&pool, &HouseSnakeConfig::default(), w0(), w1()).await;
        assert!(
            digest.is_ok(),
            "one malformed snake frame aborted the digest: {digest:?}"
        );
        assert_eq!(digest.unwrap().games_count, 1);
    }
}
