//! Window extraction for the highlights digest (DEV-1993): the SQL reads
//! that build [`WindowData`], plus the two-phase candidate/faults flow.
//!
//! Prod-DB constraints (Neon, `turns` 27 GB / `snake_turns` 11 GB):
//! - Final frames come from one indexed lateral lookup per game
//!   (`turns (game_id, turn_number)`), one query per ladder, sequential.
//! - `snake_turns` is read only for candidate games by
//!   `game_battlesnake_id`, never as a window-wide scan.
//! - `games` is never joined per result row.
//! - Frame parsing never panics (a panicking cja job kills the live game
//!   runner); malformed rows are skipped with a warn.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use super::detectors;
use super::types::{
    ComputedDigest, EntryInfo, FinalFrame, HouseSnakeConfig, LADDER_ORDER, Ladder, WindowData,
    WindowGame, WindowSnake,
};

/// Compute a digest for an arbitrary window (`w0 <= created_at < w1`).
/// Reads only; never stores or posts.
pub async fn compute_digest(
    pool: &PgPool,
    house: &HouseSnakeConfig,
    w0: DateTime<Utc>,
    w1: DateTime<Utc>,
) -> cja::Result<ComputedDigest> {
    let mut data = extract_window(pool, house, w0, w1).await?;

    // Phase 1 (CPU-bound): fit the calibration k's and pick the candidate
    // games whose faults matter.
    let (duel_k, four_snake_k, candidates) = {
        let data = data.clone();
        tokio::task::spawn_blocking(move || {
            let all: Vec<&WindowGame> = data.games.iter().collect();
            let duel_k = detectors::fit_k(&all, true);
            let four_snake_k = detectors::fit_k(&all, false);
            let candidates = detectors::candidate_game_ids(&data, duel_k, four_snake_k);
            (duel_k, four_snake_k, candidates)
        })
        .await
        .wrap_err("highlights phase-1 task panicked")?
    };

    // Faults for the candidates only.
    let faults = fetch_faults(pool, &candidates).await?;
    for game in &mut data.games {
        for snake in &mut game.snakes {
            if let Some(frame) = snake.frame.as_mut()
                && let Some(turns) = faults.get(&frame.game_battlesnake_id)
            {
                frame.fault_turns = turns.clone();
            }
        }
    }

    // Phase 2 (CPU-bound): all detectors + assembly, reusing the phase-1
    // fitted k's (deterministic, so candidates and output agree).
    tokio::task::spawn_blocking(move || {
        detectors::assemble_digest(&data, w0, w1, duel_k, four_snake_k)
    })
    .await
    .wrap_err("highlights phase-2 task panicked")
}

async fn extract_window(
    pool: &PgPool,
    house: &HouseSnakeConfig,
    w0: DateTime<Utc>,
    w1: DateTime<Utc>,
) -> cja::Result<WindowData> {
    // 1. Ladders, ordered by LADDER_ORDER position then name.
    let ladder_rows = sqlx::query!(
        "SELECT leaderboard_id, name, match_size FROM leaderboards WHERE visibility = 'public'"
    )
    .fetch_all(pool)
    .await
    .wrap_err("highlights: failed to load public ladders")?;
    let mut ladders: Vec<Ladder> = ladder_rows
        .into_iter()
        .map(|row| Ladder {
            leaderboard_id: row.leaderboard_id,
            name: row.name,
            match_size: row.match_size,
        })
        .collect();
    let display_position = |name: &str| -> usize {
        LADDER_ORDER
            .iter()
            .position(|known| *known == name)
            .unwrap_or(LADDER_ORDER.len())
    };
    ladders.sort_by(|a, b| {
        display_position(&a.name)
            .cmp(&display_position(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    let ladder_index: BTreeMap<Uuid, usize> = ladders
        .iter()
        .enumerate()
        .map(|(i, ladder)| (ladder.leaderboard_id, i))
        .collect();

    // 4. Entries (needed to filter start_ratings and for throne walks).
    let entry_rows = sqlx::query!(
        r#"
        SELECT e.leaderboard_entry_id, e.leaderboard_id, e.disabled_at,
               b.name AS snake_name, u.github_login AS "owner_login?"
        FROM leaderboard_entries e
        JOIN leaderboards l USING (leaderboard_id)
        JOIN battlesnakes b USING (battlesnake_id)
        LEFT JOIN users u ON u.user_id = b.user_id
        WHERE l.visibility = 'public'
        "#
    )
    .fetch_all(pool)
    .await
    .wrap_err("highlights: failed to load leaderboard entries")?;
    let entries: BTreeMap<Uuid, EntryInfo> = entry_rows
        .into_iter()
        .map(|row| {
            let house = house.is_house(row.owner_login.as_deref(), &row.snake_name);
            (
                row.leaderboard_entry_id,
                EntryInfo {
                    leaderboard_id: row.leaderboard_id,
                    snake_name: row.snake_name,
                    owner_login: row.owner_login,
                    disabled_at: row.disabled_at,
                    house,
                },
            )
        })
        .collect();

    // 3. Start ratings: each entry's last pre-window result. The query is
    // an unindexed full scan + sort of leaderboard_game_results, once per
    // week — accepted; no index is added in this PR.
    let start_rows = sqlx::query!(
        r#"
        SELECT DISTINCT ON (r.leaderboard_entry_id)
               r.leaderboard_entry_id, r.mu_after, r.sigma_after
        FROM leaderboard_game_results r
        WHERE r.created_at < $1
        ORDER BY r.leaderboard_entry_id, r.created_at DESC
        "#,
        w0
    )
    .fetch_all(pool)
    .await
    .wrap_err("highlights: failed to load start ratings")?;
    // Seed only entries on public ladders (the prototype's entries join).
    let start_ratings: BTreeMap<Uuid, (f64, f64)> = start_rows
        .into_iter()
        .filter_map(|row| {
            entries
                .contains_key(&row.leaderboard_entry_id)
                .then_some((row.leaderboard_entry_id, (row.mu_after, row.sigma_after)))
        })
        .collect();

    // 2. Results: every rated result in the window, one row per
    // (game, snake).
    let result_rows = sqlx::query!(
        r#"
        SELECT lg.game_id, lg.leaderboard_id, lg.created_at,
               r.leaderboard_entry_id, r.placement,
               r.mu_before, r.sigma_before, r.mu_after, r.sigma_after,
               b.name AS snake_name, u.github_login AS "owner_login?"
        FROM leaderboard_games lg
        JOIN leaderboards l USING (leaderboard_id)
        JOIN leaderboard_game_results r USING (leaderboard_game_id)
        JOIN leaderboard_entries e USING (leaderboard_entry_id)
        JOIN battlesnakes b USING (battlesnake_id)
        LEFT JOIN users u ON u.user_id = b.user_id
        WHERE lg.created_at >= $1 AND lg.created_at < $2
          AND l.visibility = 'public'
        ORDER BY lg.created_at, lg.game_id
        "#,
        w0,
        w1
    )
    .fetch_all(pool)
    .await
    .wrap_err("highlights: failed to load window results")?;
    let mut games: Vec<WindowGame> = Vec::new();
    let mut game_positions: BTreeMap<Uuid, usize> = BTreeMap::new();
    for row in result_rows {
        let Some(&ladder_idx) = ladder_index.get(&row.leaderboard_id) else {
            continue;
        };
        let position = match game_positions.get(&row.game_id) {
            Some(&position) => position,
            None => {
                games.push(WindowGame {
                    game_id: row.game_id,
                    ladder_index: ladder_idx,
                    created_at: row.created_at,
                    snakes: Vec::new(),
                    final_turn: None,
                });
                game_positions.insert(row.game_id, games.len() - 1);
                games.len() - 1
            }
        };
        let house = house.is_house(row.owner_login.as_deref(), &row.snake_name);
        games[position].snakes.push(WindowSnake {
            entry_id: row.leaderboard_entry_id,
            snake_name: row.snake_name,
            owner_login: row.owner_login,
            house,
            placement: row.placement,
            mu_before: row.mu_before,
            sigma_before: row.sigma_before,
            mu_after: row.mu_after,
            sigma_after: row.sigma_after,
            frame: None,
        });
    }

    // 6. gb map: game_battlesnake_id -> (game_id, entry_id).
    let gb_rows = sqlx::query!(
        r#"
        SELECT gb.game_battlesnake_id, gb.game_id, gb.leaderboard_entry_id
        FROM game_battlesnakes gb
        JOIN leaderboard_games lg USING (game_id)
        WHERE lg.created_at >= $1 AND lg.created_at < $2
        "#,
        w0,
        w1
    )
    .fetch_all(pool)
    .await
    .wrap_err("highlights: failed to load game_battlesnakes map")?;
    // gbid -> entry_id (window games only).
    let mut gb_entry: BTreeMap<Uuid, Uuid> = BTreeMap::new();
    for row in gb_rows {
        if let (Some(entry_id), true) = (
            row.leaderboard_entry_id,
            game_positions.contains_key(&row.game_id),
        ) {
            gb_entry.insert(row.game_battlesnake_id, entry_id);
        }
    }

    // 5. Final frames: one sequential query per ladder (each statement
    // stays small against Neon; ~9 s each on the week in the prototype).
    for ladder in &ladders {
        // Frame fields are extracted with guards (jsonb_typeof / digit
        // regex) so a malformed field yields NULL instead of failing the
        // statement; the Rust side then warns and skips that field/frame.
        // `s->'Body'` stays server-side via jsonb_array_length (never
        // transferred), and `Snakes` is only unnested when it is an array
        // (jsonb_array_elements errors on scalars/objects).
        let frame_rows = sqlx::query!(
            r#"
            SELECT lg.game_id, t.turn_number AS final_turn,
                   s->>'ID' AS game_battlesnake_id,
                   CASE WHEN jsonb_typeof(s->'Body') = 'array'
                        THEN jsonb_array_length(s->'Body') END AS "length",
                   CASE WHEN (s->>'Health') ~ '^[0-9]{1,9}$'
                        THEN (s->>'Health')::int4 END AS "health",
                   s->'Death'->>'Cause' AS death_cause,
                   CASE WHEN (s->'Death'->>'Turn') ~ '^[0-9]{1,9}$'
                        THEN (s->'Death'->>'Turn')::int4 END AS "death_turn",
                   s->'Death'->>'EliminatedBy' AS eliminated_by
            FROM leaderboard_games lg
            JOIN leaderboards l USING (leaderboard_id)
            CROSS JOIN LATERAL (
                SELECT turn_number, frame_data FROM turns t
                WHERE t.game_id = lg.game_id ORDER BY t.turn_number DESC LIMIT 1
            ) t
            CROSS JOIN LATERAL jsonb_array_elements(
                CASE WHEN jsonb_typeof(t.frame_data->'Snakes') = 'array'
                     THEN t.frame_data->'Snakes' END
            ) s
            WHERE l.leaderboard_id = $1 AND lg.created_at >= $2 AND lg.created_at < $3
            "#,
            ladder.leaderboard_id,
            w0,
            w1
        )
        .fetch_all(pool)
        .await
        .wrap_err_with(|| {
            format!(
                "highlights: failed to load final frames for {}",
                ladder.name
            )
        })?;
        for row in frame_rows {
            let Some(&position) = game_positions.get(&row.game_id) else {
                continue;
            };
            games[position].final_turn = Some(row.final_turn);
            let Some(gbid_text) = row.game_battlesnake_id.as_deref() else {
                tracing::warn!(game_id = %row.game_id, "highlights: frame without ID, skipping");
                continue;
            };
            let Ok(gbid) = Uuid::parse_str(gbid_text) else {
                tracing::warn!(game_id = %row.game_id, "highlights: frame ID is not a UUID, skipping");
                continue;
            };
            let (Some(length), Some(health)) = (row.length, row.health) else {
                tracing::warn!(
                    game_id = %row.game_id,
                    "highlights: frame missing Body/Health, skipping snake frame"
                );
                continue;
            };
            let eliminated_by = row.eliminated_by.as_deref().and_then(|by| {
                Uuid::parse_str(by)
                    .map_err(|_| {
                        tracing::warn!(
                            game_id = %row.game_id,
                            "highlights: EliminatedBy is not a UUID, ignoring"
                        )
                    })
                    .ok()
            });
            let frame = FinalFrame {
                game_battlesnake_id: gbid,
                length,
                health,
                death_cause: row.death_cause,
                death_turn: row.death_turn,
                eliminated_by,
                fault_turns: BTreeSet::new(),
            };
            // Attach to the participant whose (game, entry) matches — first
            // match, like the prototype.
            let Some(&entry_id) = gb_entry.get(&gbid) else {
                continue;
            };
            let snake = games[position]
                .snakes
                .iter_mut()
                .find(|s| s.entry_id == entry_id);
            if let Some(snake) = snake {
                snake.frame = Some(frame);
            }
        }
    }

    games.sort_by_key(|game| (game.created_at, game.game_id));

    Ok(WindowData {
        ladders,
        games,
        entries,
        start_ratings,
    })
}

/// Faulted (timed_out/errored) turns for the candidate games' snakes.
/// Never scans the window — `game_battlesnake_id` lookups only.
async fn fetch_faults(
    pool: &PgPool,
    candidate_game_ids: &BTreeSet<Uuid>,
) -> cja::Result<BTreeMap<Uuid, BTreeSet<i32>>> {
    if candidate_game_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ids: Vec<Uuid> = candidate_game_ids.iter().copied().collect();
    let rows = sqlx::query!(
        r#"
        SELECT gb.game_battlesnake_id, t.turn_number
        FROM game_battlesnakes gb
        JOIN snake_turns st USING (game_battlesnake_id)
        JOIN turns t USING (turn_id)
        WHERE gb.game_id = ANY($1) AND (st.timed_out OR st.errored)
        "#,
        &ids
    )
    .fetch_all(pool)
    .await
    .wrap_err("highlights: failed to load candidate faults")?;
    let mut faults: BTreeMap<Uuid, BTreeSet<i32>> = BTreeMap::new();
    for row in rows {
        faults
            .entry(row.game_battlesnake_id)
            .or_default()
            .insert(row.turn_number);
    }
    Ok(faults)
}
