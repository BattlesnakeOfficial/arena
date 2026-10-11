//! Code-defined achievements and permanent cosmetic awards.
use std::collections::{BTreeMap, HashSet};

use color_eyre::eyre::Context as _;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{Head, Tail};
use crate::models::game::GameType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Achievement {
    FirstWin,
    SlowAndSteady,
    ConstrictorChamp,
    CrowdPleaser,
    LadderRegular,
}

#[derive(Debug, Clone, Copy)]
pub struct AchievementDef {
    pub name: &'static str,
    pub description: &'static str,
    pub head: Head,
    pub tail: Option<Tail>,
}

impl Achievement {
    pub const ALL: [Self; 5] = [
        Self::FirstWin,
        Self::SlowAndSteady,
        Self::ConstrictorChamp,
        Self::CrowdPleaser,
        Self::LadderRegular,
    ];

    pub const fn def(self) -> AchievementDef {
        match self {
            Self::FirstWin => AchievementDef {
                name: "First Win",
                description: "Win a finished game with at least 2 snakes.",
                head: Head::Frog,
                tail: None,
            },
            Self::SlowAndSteady => AchievementDef {
                name: "Slow and Steady",
                description: "Play 3,000 ladder games across your snakes (about a week with a snake on every ladder).",
                head: Head::Turtle,
                tail: Some(Tail::Turtle),
            },
            Self::ConstrictorChamp => AchievementDef {
                name: "Constrictor Champ",
                description: "Win a finished Constrictor game with at least 2 snakes.",
                head: Head::Subway,
                tail: Some(Tail::Subway),
            },
            Self::CrowdPleaser => AchievementDef {
                name: "Crowd Pleaser",
                description: "Win a finished game with at least 4 snakes.",
                head: Head::Monkey,
                tail: Some(Tail::Monkey),
            },
            Self::LadderRegular => AchievementDef {
                name: "Ladder Regular",
                description: "Win 1,000 ladder games across your snakes (about a week for a snake that wins a third of its games).",
                head: Head::Judge,
                tail: Some(Tail::Judge),
            },
        }
    }
}

pub fn for_item(kind: &str, slug: &str) -> Option<AchievementDef> {
    Achievement::ALL
        .into_iter()
        .map(Achievement::def)
        .find(|def| {
            (kind == Head::KIND && slug == def.head.slug())
                || (kind == Tail::KIND && def.tail.is_some_and(|tail| slug == tail.slug()))
        })
}

#[derive(Default, Clone, Copy)]
struct WinFlags {
    first: bool,
    constrictor: bool,
    crowd: bool,
    ladder: bool,
    ladder_win: bool,
}

impl WinFlags {
    fn include(
        &mut self,
        placement: Option<i32>,
        participants: i64,
        game_type: &str,
        ladder: bool,
    ) {
        self.ladder |= ladder;
        if placement == Some(1) && participants >= 2 {
            self.first = true;
            self.constrictor |= game_type == GameType::Constrictor.as_str();
            self.crowd |= participants >= 4;
            self.ladder_win |= ladder;
        }
    }
}

async fn historical_win(
    conn: &mut PgConnection,
    user_id: Uuid,
    min_participants: i64,
    constrictor: bool,
) -> cja::Result<bool> {
    let value = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM (
                SELECT gb.game_id FROM battlesnakes bs
                JOIN game_battlesnakes gb ON gb.battlesnake_id = bs.battlesnake_id
                WHERE bs.user_id = $1 AND gb.placement = 1
                UNION ALL
                SELECT gb.game_id FROM battlesnakes bs
                JOIN leaderboard_entries le ON le.battlesnake_id = bs.battlesnake_id
                JOIN game_battlesnakes gb ON gb.leaderboard_entry_id = le.leaderboard_entry_id
                WHERE bs.user_id = $1 AND gb.battlesnake_id IS NULL AND gb.placement = 1
            ) owned
            JOIN games g ON g.game_id = owned.game_id
            WHERE g.status = 'finished'
              AND (NOT $3::bool OR g.game_type = $4)
              AND (SELECT COUNT(*) FROM game_battlesnakes peer WHERE peer.game_id = owned.game_id) >= $2
            LIMIT 1
        ) AS "exists!"
        "#,
        user_id,
        min_participants,
        constrictor,
        GameType::Constrictor.as_str(),
    )
    .fetch_one(conn).await.wrap_err("Failed to check historical achievement win")?;
    Ok(value)
}

async fn distinct_candidate_count(
    conn: &mut PgConnection,
    user_id: Uuid,
    cap: i64,
    ladder_wins: bool,
) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*)::bigint AS "count!" FROM (
            SELECT game_id FROM (
                SELECT DISTINCT owned.game_id FROM (
                    SELECT gb.game_id FROM battlesnakes bs
                    JOIN game_battlesnakes gb ON gb.battlesnake_id = bs.battlesnake_id
                    WHERE bs.user_id = $1 AND gb.leaderboard_entry_id IS NOT NULL
                      AND (NOT $3::bool OR gb.placement = 1)
                    UNION ALL
                    SELECT gb.game_id FROM battlesnakes bs
                    JOIN leaderboard_entries le ON le.battlesnake_id = bs.battlesnake_id
                    JOIN game_battlesnakes gb ON gb.leaderboard_entry_id = le.leaderboard_entry_id
                    WHERE bs.user_id = $1 AND gb.battlesnake_id IS NULL
                      AND (NOT $3::bool OR gb.placement = 1)
                ) owned
            ) distinct_games
            LIMIT $2
        ) candidates
        "#,
        user_id,
        cap,
        ladder_wins,
    )
    .fetch_one(conn)
    .await
    .wrap_err("Failed to count achievement ladder candidates")?;
    Ok(count)
}

async fn distinct_finished_count(
    conn: &mut PgConnection,
    user_id: Uuid,
    cap: i64,
    ladder_wins: bool,
) -> cja::Result<i64> {
    // Deduplicate via the per-snake/per-entry indexes. The indexed set of
    // non-finished games is small; subtracting it avoids thousands of cold
    // games_pkey lookups for an owner near the threshold. Both achievements
    // require the owner's own ladder participant row.
    let count = sqlx::query_scalar!(
        r#"
        WITH distinct_games AS MATERIALIZED (
            SELECT DISTINCT owned.game_id FROM (
                SELECT gb.game_id FROM battlesnakes bs
                JOIN game_battlesnakes gb ON gb.battlesnake_id = bs.battlesnake_id
                WHERE bs.user_id = $1 AND gb.leaderboard_entry_id IS NOT NULL
                  AND (NOT $3::bool OR gb.placement = 1)
                UNION ALL
                SELECT gb.game_id FROM battlesnakes bs
                JOIN leaderboard_entries le ON le.battlesnake_id = bs.battlesnake_id
                JOIN game_battlesnakes gb ON gb.leaderboard_entry_id = le.leaderboard_entry_id
                WHERE bs.user_id = $1 AND gb.battlesnake_id IS NULL
                  AND (NOT $3::bool OR gb.placement = 1)
            ) owned
        ), nonfinished AS MATERIALIZED (
            -- status is NOT NULL; two ranges use games_status_idx instead of
            -- scanning the 5M-row games table for status <> 'finished'.
            SELECT game_id FROM games
            WHERE status < 'finished' OR status > 'finished'
        ), finished_games AS MATERIALIZED (
            SELECT d.game_id FROM distinct_games d
            LEFT JOIN nonfinished nf ON nf.game_id = d.game_id
            WHERE nf.game_id IS NULL
        )
        SELECT COUNT(*)::bigint AS "count!" FROM (
            SELECT 1 FROM finished_games fg
            -- The materialized finished set keeps Judge's peer probe after
            -- the status check; see the DEV-1663 production EXPLAIN.
            WHERE NOT $3::bool OR (SELECT COUNT(*) FROM game_battlesnakes peer
                WHERE peer.game_id = fg.game_id) >= 2
            LIMIT $2
        ) qualifying
        "#,
        user_id,
        cap,
        ladder_wins,
    )
    .fetch_one(conn)
    .await
    .wrap_err("Failed to count finished achievement games")?;
    Ok(count)
}

async fn award_owner(
    conn: &mut PgConnection,
    user_id: Uuid,
    live: Option<WinFlags>,
) -> cja::Result<u64> {
    let existing = sqlx::query!(
        "SELECT customization_type, slug FROM customization_grants WHERE user_id = $1",
        user_id,
    )
    .fetch_all(&mut *conn)
    .await
    .wrap_err("Failed to read achievement ownership")?;
    let owned: HashSet<_> = existing
        .into_iter()
        .map(|r| (r.customization_type, r.slug))
        .collect();
    let mut inserted = 0;
    for achievement in Achievement::ALL {
        let def = achievement.def();
        let head_owned = owned.contains(&(Head::KIND.to_string(), def.head.slug().to_string()));
        let tail_owned = def
            .tail
            .is_none_or(|tail| owned.contains(&(Tail::KIND.to_string(), tail.slug().to_string())));
        if head_owned && tail_owned {
            continue;
        }
        let earned = match achievement {
            Achievement::FirstWin => match live {
                Some(flags) => flags.first,
                None => historical_win(&mut *conn, user_id, 2, false).await?,
            },
            Achievement::ConstrictorChamp => match live {
                Some(flags) => flags.constrictor,
                None => historical_win(&mut *conn, user_id, 2, true).await?,
            },
            Achievement::CrowdPleaser => match live {
                Some(flags) => flags.crowd,
                None => historical_win(&mut *conn, user_id, 4, false).await?,
            },
            Achievement::SlowAndSteady => {
                if !live.is_none_or(|flags| flags.ladder) {
                    false
                } else {
                    distinct_candidate_count(&mut *conn, user_id, 3_000, false).await? >= 3_000
                        && distinct_finished_count(&mut *conn, user_id, 3_000, false).await?
                            >= 3_000
                }
            }
            Achievement::LadderRegular => {
                if !live.is_none_or(|flags| flags.ladder_win) {
                    false
                } else {
                    distinct_candidate_count(&mut *conn, user_id, 1_000, true).await? >= 1_000
                        && distinct_finished_count(&mut *conn, user_id, 1_000, true).await? >= 1_000
                }
            }
        };
        if !earned {
            continue;
        }
        for (kind, slug, held) in [
            (Head::KIND, def.head.slug(), head_owned),
            (Tail::KIND, def.tail.map_or("", Tail::slug), tail_owned),
        ] {
            if held || slug.is_empty() {
                continue;
            }
            inserted += sqlx::query!(
                "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, $2, $3, 'achievement') ON CONFLICT (user_id, customization_type, slug) DO NOTHING",
                user_id, kind, slug,
            ).execute(&mut *conn).await.wrap_err("Failed to insert achievement grant")?.rows_affected();
        }
    }
    Ok(inserted)
}

pub async fn award_for_game(pool: &PgPool, game_id: Uuid) -> cja::Result<u64> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin achievement award")?;
    let game = sqlx::query!(
        "SELECT status, game_type FROM games WHERE game_id = $1",
        game_id
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to read achievement game")?;
    let Some(game) = game.filter(|game| game.status == "finished") else {
        return Ok(0);
    };
    let rows = sqlx::query!(
        r#"SELECT bs.user_id, gb.placement, gb.leaderboard_entry_id FROM game_battlesnakes gb
        LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
        JOIN battlesnakes bs ON bs.battlesnake_id = COALESCE(gb.battlesnake_id, le.battlesnake_id)
        WHERE gb.game_id = $1"#,
        game_id,
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to read achievement participants")?;
    let participants = rows.len() as i64;
    let mut owners = BTreeMap::<Uuid, WinFlags>::new();
    for row in rows {
        owners.entry(row.user_id).or_default().include(
            row.placement,
            participants,
            &game.game_type,
            row.leaderboard_entry_id.is_some(),
        );
    }
    let mut inserted = 0;
    for (owner, flags) in owners {
        inserted += award_owner(&mut tx, owner, Some(flags)).await?;
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit achievement award")?;
    Ok(inserted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillPage {
    pub inserted: u64,
    pub processed: usize,
    pub complete: bool,
}

pub async fn backfill_achievement_page(pool: &PgPool) -> cja::Result<BackfillPage> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin achievement backfill")?;
    sqlx::query!("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to set achievement backfill lock timeout")?;
    sqlx::query!("SET LOCAL statement_timeout = '60s'")
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to set achievement backfill statement timeout")?;
    let cursor = sqlx::query!(
        "SELECT after_user_id, completed_at FROM achievement_backfill_cursor WHERE singleton = TRUE FOR UPDATE SKIP LOCKED"
    ).fetch_optional(&mut *tx).await.wrap_err("Failed to lock achievement cursor")?;
    let Some(cursor) = cursor else {
        let exists = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM achievement_backfill_cursor WHERE singleton = TRUE) AS \"exists!\"")
            .fetch_one(&mut *tx).await.wrap_err("Failed to check achievement cursor existence")?;
        if !exists {
            color_eyre::eyre::bail!("Achievement backfill cursor is missing");
        }
        return Ok(BackfillPage {
            inserted: 0,
            processed: 0,
            complete: false,
        });
    };
    if cursor.completed_at.is_some() {
        tx.commit()
            .await
            .wrap_err("Failed to commit completed achievement backfill")?;
        return Ok(BackfillPage {
            inserted: 0,
            processed: 0,
            complete: true,
        });
    }
    let users = sqlx::query!(
        "SELECT user_id FROM users WHERE ($1::uuid IS NULL OR user_id > $1) ORDER BY user_id LIMIT 26",
        cursor.after_user_id,
    ).fetch_all(&mut *tx).await.wrap_err("Failed to page achievement users")?;
    let complete = users.len() <= 25;
    let processed = users.len().min(25);
    let mut inserted = 0;
    for user in users.iter().take(25) {
        inserted += award_owner(&mut tx, user.user_id, None).await?;
    }
    let last = users
        .get(processed.saturating_sub(1))
        .map(|user| user.user_id)
        .or(cursor.after_user_id);
    sqlx::query!(
        "UPDATE achievement_backfill_cursor SET after_user_id = $1, completed_at = CASE WHEN $2 THEN NOW() ELSE NULL END WHERE singleton = TRUE",
        last, complete,
    ).execute(&mut *tx).await.wrap_err("Failed to advance achievement cursor")?;
    tx.commit()
        .await
        .wrap_err("Failed to commit achievement backfill page")?;
    Ok(BackfillPage {
        inserted,
        processed,
        complete,
    })
}

pub async fn reconcile_recent_achievements(pool: &PgPool) -> cja::Result<u64> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin achievement reconciliation")?;
    let rows = sqlx::query!(
        r#"SELECT g.game_id, g.game_type, bs.user_id, gb.placement, gb.leaderboard_entry_id,
            (SELECT COUNT(*) FROM game_battlesnakes peer WHERE peer.game_id = g.game_id) AS "participants!"
        FROM games g
        JOIN game_battlesnakes gb ON gb.game_id = g.game_id
        LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
        JOIN battlesnakes bs ON bs.battlesnake_id = COALESCE(gb.battlesnake_id, le.battlesnake_id)
        WHERE g.status = 'finished' AND g.finished_at IS NOT NULL
          AND g.finished_at > NOW() - INTERVAL '2 hours'"#,
    ).fetch_all(&mut *tx).await.wrap_err("Failed to read recent achievement games")?;
    let candidate_games: HashSet<_> = rows.iter().map(|row| row.game_id).collect();
    let mut owners = BTreeMap::<Uuid, WinFlags>::new();
    for row in rows {
        owners.entry(row.user_id).or_default().include(
            row.placement,
            row.participants,
            &row.game_type,
            row.leaderboard_entry_id.is_some(),
        );
    }
    let owner_count = owners.len();
    let mut inserted = 0;
    for (owner, flags) in owners {
        inserted += award_owner(&mut tx, owner, Some(flags)).await?;
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit achievement reconciliation")?;
    tracing::info!(
        candidate_games = candidate_games.len(),
        owners = owner_count,
        inserted,
        "Reconciled recent achievements"
    );
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn user(pool: &PgPool, number: i64) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users (external_github_id, github_login, github_access_token) VALUES ($1, $2, '') RETURNING user_id",
        )
        .bind(number)
        .bind(format!("achievement-{number}"))
        .fetch_one(pool).await.unwrap()
    }

    async fn snake(pool: &PgPool, owner: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO battlesnakes (user_id, name, url) VALUES ($1, $2, 'https://example.com') RETURNING battlesnake_id",
        ).bind(owner).bind(name).fetch_one(pool).await.unwrap()
    }

    async fn game(pool: &PgPool, kind: &str, status: &str, age_hours: Option<i32>) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO games (board_size, game_type, status, finished_at) VALUES ('11x11', $1, $2, CASE WHEN $3::int IS NULL THEN NULL ELSE NOW() - $3 * INTERVAL '1 hour' END) RETURNING game_id",
        ).bind(kind).bind(status).bind(age_hours).fetch_one(pool).await.unwrap()
    }

    async fn participant(
        pool: &PgPool,
        game_id: Uuid,
        snake_id: Uuid,
        placement: i32,
        entry: Option<Uuid>,
    ) {
        sqlx::query(
            "INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement, leaderboard_entry_id) VALUES ($1, $2, $3, $4)",
        ).bind(game_id).bind(snake_id).bind(placement).bind(entry).execute(pool).await.unwrap();
    }

    async fn entry(pool: &PgPool, snake_id: Uuid) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO leaderboard_entries (leaderboard_id, battlesnake_id) VALUES ((SELECT leaderboard_id FROM leaderboards LIMIT 1), $1) RETURNING leaderboard_entry_id",
        ).bind(snake_id).fetch_one(pool).await.unwrap()
    }

    async fn seed_games(
        pool: &PgPool,
        snake_id: Uuid,
        ladder_entry: Option<Uuid>,
        opponent: Option<Uuid>,
        count: i32,
        placement: i32,
        status: &str,
    ) {
        let inserted: i64 = sqlx::query_scalar(
            "WITH inserted_games AS (\
                INSERT INTO games (board_size, game_type, status, finished_at) \
                SELECT '11x11', 'Standard', $1, NOW() - INTERVAL '3 hours' \
                FROM generate_series(1, $2) RETURNING game_id\
            ), owned AS (\
                INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement) \
                SELECT game_id, $3, $4, $5 FROM inserted_games RETURNING game_id\
            ), peer AS (\
                INSERT INTO game_battlesnakes (game_id, battlesnake_id, placement) \
                SELECT game_id, $6, CASE WHEN $5 = 1 THEN 2 ELSE 1 END \
                FROM inserted_games WHERE $6::uuid IS NOT NULL RETURNING game_id\
            ) SELECT COUNT(*) FROM owned",
        )
        .bind(status)
        .bind(count)
        .bind(snake_id)
        .bind(ladder_entry)
        .bind(placement)
        .bind(opponent)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(inserted, i64::from(count));
    }

    async fn grants(pool: &PgPool, owner: Uuid) -> HashSet<(String, String, String)> {
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT customization_type, slug, source FROM customization_grants WHERE user_id = $1",
        )
        .bind(owner)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
    }

    fn has(grants: &HashSet<(String, String, String)>, kind: &str, slug: &str) -> bool {
        grants.iter().any(|(k, s, _)| k == kind && s == slug)
    }

    #[test]
    fn every_2024_item_has_exactly_one_rule() {
        for head in Head::ALL
            .iter()
            .filter(|head| head.def().group == super::super::Group::Collection2024)
        {
            assert_eq!(
                Achievement::ALL
                    .iter()
                    .filter(|rule| rule.def().head == *head)
                    .count(),
                1
            );
            assert!(for_item(Head::KIND, head.slug()).is_some());
        }
        for tail in Tail::ALL
            .iter()
            .filter(|tail| tail.def().group == super::super::Group::Collection2024)
        {
            assert_eq!(
                Achievement::ALL
                    .iter()
                    .filter(|rule| rule.def().tail == Some(*tail))
                    .count(),
                1
            );
            assert!(for_item(Tail::KIND, tail.slug()).is_some());
        }
        assert!(for_item(Head::KIND, "alligator").is_none());
        assert!(for_item(Tail::KIND, "alligator").is_none());
    }

    #[test]
    fn week_scale_achievement_copy() {
        assert_eq!(
            Achievement::SlowAndSteady.def().description,
            "Play 3,000 ladder games across your snakes (about a week with a snake on every ladder)."
        );
        assert_eq!(
            Achievement::LadderRegular.def().description,
            "Win 1,000 ladder games across your snakes (about a week for a snake that wins a third of its games)."
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn live_win_boundaries_and_provenance(pool: PgPool) {
        let owner = user(&pool, 1644001).await;
        sqlx::query("INSERT INTO customization_active_weeks (user_id, week_start) VALUES ($1, '2026-10-05')")
            .bind(owner).execute(&pool).await.unwrap();
        let rival = user(&pool, 1644002).await;
        let winner = snake(&pool, owner, "winner").await;
        let second = snake(&pool, owner, "second").await;
        let rival_snake = snake(&pool, rival, "rival").await;
        let fourth = snake(&pool, rival, "fourth").await;
        let solo = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, solo, winner, 1, None).await;
        assert_eq!(award_for_game(&pool, solo).await.unwrap(), 0);
        let waiting = game(&pool, "Standard", "waiting", None).await;
        participant(&pool, waiting, winner, 1, None).await;
        participant(&pool, waiting, rival_snake, 2, None).await;
        assert_eq!(award_for_game(&pool, waiting).await.unwrap(), 0);
        let failed = game(&pool, "Standard", "failed", None).await;
        participant(&pool, failed, winner, 1, None).await;
        participant(&pool, failed, rival_snake, 2, None).await;
        assert_eq!(award_for_game(&pool, failed).await.unwrap(), 0);
        let two = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, two, winner, 1, None).await;
        participant(&pool, two, rival_snake, 2, None).await;
        assert_eq!(award_for_game(&pool, two).await.unwrap(), 1);
        assert_eq!(award_for_game(&pool, two).await.unwrap(), 0);
        let lower = game(&pool, "constrictor", "finished", Some(0)).await;
        participant(&pool, lower, winner, 1, None).await;
        participant(&pool, lower, second, 2, None).await;
        assert_eq!(award_for_game(&pool, lower).await.unwrap(), 0);
        let three = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, three, winner, 1, None).await;
        participant(&pool, three, second, 2, None).await;
        participant(&pool, three, rival_snake, 3, None).await;
        assert_eq!(award_for_game(&pool, three).await.unwrap(), 0);
        let four = game(&pool, GameType::Constrictor.as_str(), "finished", Some(0)).await;
        participant(&pool, four, winner, 1, None).await;
        participant(&pool, four, second, 2, None).await;
        participant(&pool, four, rival_snake, 3, None).await;
        participant(&pool, four, fourth, 4, None).await;
        assert_eq!(award_for_game(&pool, four).await.unwrap(), 4);
        let owned = grants(&pool, owner).await;
        assert!(has(&owned, Head::KIND, "frog"));
        assert!(has(&owned, Head::KIND, "subway"));
        assert!(has(&owned, Tail::KIND, "subway"));
        assert!(has(&owned, Head::KIND, "monkey"));
        assert!(has(&owned, Tail::KIND, "monkey"));
        assert_eq!(super::super::token_balance(&pool, owner).await.unwrap(), 1);
        sqlx::query("DELETE FROM games WHERE game_id IN ($1, $2, $3, $4)")
            .bind(two)
            .bind(lower)
            .bind(three)
            .bind(four)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1")
            .bind(winner)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(grants(&pool, owner).await, owned);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn distinct_thresholds_fallback_soft_delete_and_partial_grant(pool: PgPool) {
        let owner = user(&pool, 1644011).await;
        let first = snake(&pool, owner, "first").await;
        let second = snake(&pool, owner, "second").await;
        let first_entry = entry(&pool, first).await;
        let second_entry = entry(&pool, second).await;
        sqlx::query("UPDATE battlesnakes SET deleted_at = NOW() WHERE battlesnake_id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'turtle', 'admin'), ($1, 'head', 'judge', 'play_import')")
            .bind(owner).execute(&pool).await.unwrap();
        let rival_owner = user(&pool, 1644013).await;
        let rival = snake(&pool, rival_owner, "rival").await;
        seed_games(
            &pool,
            first,
            Some(first_entry),
            Some(rival),
            998,
            1,
            "finished",
        )
        .await;
        let fallback_win = game(&pool, "Standard", "finished", Some(3)).await;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement) VALUES ($1, NULL, $2, 1)")
            .bind(fallback_win).bind(first_entry).execute(&pool).await.unwrap();
        participant(&pool, fallback_win, rival, 2, None).await;
        let almost_judge = game(&pool, "Standard", "finished", None).await;
        participant(&pool, almost_judge, first, 2, Some(first_entry)).await;
        participant(&pool, almost_judge, second, 2, Some(second_entry)).await;
        assert_eq!(award_for_game(&pool, almost_judge).await.unwrap(), 0);
        assert!(!has(&grants(&pool, owner).await, Tail::KIND, "judge"));
        let thousandth_win = game(&pool, "Standard", "finished", None).await;
        participant(&pool, thousandth_win, first, 1, Some(first_entry)).await;
        participant(&pool, thousandth_win, rival, 2, None).await;
        assert_eq!(award_for_game(&pool, thousandth_win).await.unwrap(), 2);
        seed_games(
            &pool,
            second,
            Some(second_entry),
            Some(rival),
            1998,
            2,
            "finished",
        )
        .await;
        // Two owned rows in almost_judge count as one distinct game.
        assert_eq!(
            distinct_finished_count(&mut pool.acquire().await.unwrap(), owner, 3_000, false)
                .await
                .unwrap(),
            2_999
        );
        assert!(!has(&grants(&pool, owner).await, Tail::KIND, "turtle"));
        let last = game(&pool, "Standard", "finished", None).await;
        participant(&pool, last, first, 2, Some(first_entry)).await;
        participant(&pool, last, second, 1, Some(second_entry)).await;
        assert_eq!(award_for_game(&pool, last).await.unwrap(), 1);
        let owned = grants(&pool, owner).await;
        assert!(owned.contains(&(
            Head::KIND.to_string(),
            "turtle".to_string(),
            "admin".to_string()
        )));
        assert!(owned.contains(&(
            Head::KIND.to_string(),
            "judge".to_string(),
            "play_import".to_string()
        )));
        assert!(owned.contains(&(
            Tail::KIND.to_string(),
            "turtle".to_string(),
            "achievement".to_string()
        )));
        assert!(owned.contains(&(
            Tail::KIND.to_string(),
            "judge".to_string(),
            "achievement".to_string()
        )));
        assert_eq!(award_for_game(&pool, last).await.unwrap(), 0);
        assert_eq!(super::super::token_balance(&pool, owner).await.unwrap(), 0);
        // The fallback owner link is still usable when the direct snake ID is null.
        let fallback = game(&pool, "Standard", "finished", Some(0)).await;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement) VALUES ($1, NULL, $2, 1)")
            .bind(fallback).bind(first_entry).execute(&pool).await.unwrap();
        assert_eq!(award_for_game(&pool, fallback).await.unwrap(), 0);
        let fallback_owner = user(&pool, 1644012).await;
        let fallback_snake = snake(&pool, fallback_owner, "fallback").await;
        let fallback_entry = entry(&pool, fallback_snake).await;
        let fallback_game = game(&pool, "Standard", "finished", Some(0)).await;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement) VALUES ($1, NULL, $2, 1)")
            .bind(fallback_game).bind(fallback_entry).execute(&pool).await.unwrap();
        participant(&pool, fallback_game, first, 2, None).await;
        assert_eq!(award_for_game(&pool, fallback_game).await.unwrap(), 1);
        assert!(has(
            &grants(&pool, fallback_owner).await,
            Head::KIND,
            "frog"
        ));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn ladder_counts_exclude_non_ladder_and_unfinished_candidates(pool: PgPool) {
        let owner = user(&pool, 1663001).await;
        let opponent = user(&pool, 1663002).await;
        let a = snake(&pool, owner, "counted").await;
        let b = snake(&pool, opponent, "opponent").await;
        let a_entry = entry(&pool, a).await;
        seed_games(&pool, a, Some(a_entry), Some(b), 2_999, 2, "finished").await;
        seed_games(&pool, a, None, Some(b), 10, 2, "finished").await;
        let non_ladder = game(&pool, "Standard", "finished", None).await;
        participant(&pool, non_ladder, a, 2, None).await;
        participant(&pool, non_ladder, b, 1, None).await;
        assert_eq!(award_for_game(&pool, non_ladder).await.unwrap(), 1);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "turtle"));
        let unfinished = game(&pool, "Standard", "waiting", None).await;
        participant(&pool, unfinished, a, 2, Some(a_entry)).await;
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            distinct_candidate_count(&mut conn, owner, 3_000, false)
                .await
                .unwrap(),
            3_000
        );
        assert_eq!(
            distinct_finished_count(&mut conn, owner, 3_000, false)
                .await
                .unwrap(),
            2_999
        );
        assert_eq!(award_owner(&mut conn, owner, None).await.unwrap(), 0);
        drop(conn);
        let ladder = game(&pool, "Standard", "finished", None).await;
        participant(&pool, ladder, a, 2, Some(a_entry)).await;
        participant(&pool, ladder, b, 1, None).await;
        assert_eq!(award_for_game(&pool, ladder).await.unwrap(), 2);
        assert!(has(&grants(&pool, owner).await, Head::KIND, "turtle"));
        assert!(has(&grants(&pool, owner).await, Tail::KIND, "turtle"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn ladder_wins_require_owned_winning_row_and_peers(pool: PgPool) {
        let owner = user(&pool, 1663011).await;
        let opponent = user(&pool, 1663012).await;
        let a = snake(&pool, owner, "winner").await;
        let b = snake(&pool, opponent, "opponent").await;
        let a_entry = entry(&pool, a).await;
        seed_games(&pool, a, Some(a_entry), Some(b), 998, 1, "finished").await;
        let win_999 = game(&pool, "Standard", "finished", None).await;
        participant(&pool, win_999, a, 1, Some(a_entry)).await;
        participant(&pool, win_999, b, 2, None).await;
        assert_eq!(award_for_game(&pool, win_999).await.unwrap(), 1);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "judge"));
        assert!(!has(&grants(&pool, owner).await, Tail::KIND, "judge"));
        seed_games(&pool, a, Some(a_entry), None, 1, 1, "finished").await;
        seed_games(&pool, a, None, Some(b), 1, 1, "finished").await;
        let dirty = game(&pool, "Standard", "waiting", None).await;
        participant(&pool, dirty, a, 1, Some(a_entry)).await;
        participant(&pool, dirty, b, 2, None).await;
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            distinct_candidate_count(&mut conn, owner, 1_000, true)
                .await
                .unwrap(),
            1_000
        );
        assert_eq!(
            distinct_finished_count(&mut conn, owner, 1_000, true)
                .await
                .unwrap(),
            999
        );
        drop(conn);
        let mixed = game(&pool, "Standard", "finished", None).await;
        participant(&pool, mixed, a, 1, None).await;
        let other_owned = snake(&pool, owner, "ladder loss").await;
        let other_entry = entry(&pool, other_owned).await;
        participant(&pool, mixed, other_owned, 2, Some(other_entry)).await;
        assert_eq!(award_for_game(&pool, mixed).await.unwrap(), 0);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "judge"));
        let win = game(&pool, "Standard", "finished", None).await;
        participant(&pool, win, a, 1, Some(a_entry)).await;
        participant(&pool, win, b, 2, None).await;
        assert_eq!(award_for_game(&pool, win).await.unwrap(), 2);
        assert!(has(&grants(&pool, owner).await, Head::KIND, "judge"));
        assert!(has(&grants(&pool, owner).await, Tail::KIND, "judge"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn recent_reconciliation_ors_qualifying_rows_but_live_gate_stays_local(pool: PgPool) {
        let owner = user(&pool, 1663031).await;
        let opponent = user(&pool, 1663032).await;
        let a = snake(&pool, owner, "reconcile").await;
        let b = snake(&pool, opponent, "peer").await;
        let a_entry = entry(&pool, a).await;
        seed_games(&pool, a, Some(a_entry), Some(b), 999, 1, "finished").await;
        seed_games(&pool, a, Some(a_entry), Some(b), 2_000, 2, "finished").await;
        let missed = game(&pool, "Standard", "finished", Some(1)).await;
        participant(&pool, missed, a, 1, Some(a_entry)).await;
        participant(&pool, missed, b, 2, None).await;
        let nonqualifying = game(&pool, "Standard", "finished", Some(1)).await;
        participant(&pool, nonqualifying, a, 2, None).await;
        participant(&pool, nonqualifying, b, 1, None).await;
        assert_eq!(award_for_game(&pool, nonqualifying).await.unwrap(), 1);
        let before = grants(&pool, owner).await;
        assert!(!has(&before, Head::KIND, "turtle"));
        assert!(!has(&before, Head::KIND, "judge"));
        assert_eq!(reconcile_recent_achievements(&pool).await.unwrap(), 5);
        let after = grants(&pool, owner).await;
        assert!(has(&after, Head::KIND, "turtle"));
        assert!(has(&after, Tail::KIND, "turtle"));
        assert!(has(&after, Head::KIND, "judge"));
        assert!(has(&after, Tail::KIND, "judge"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn existing_achievement_grants_survive_live_and_backfill(pool: PgPool) {
        let full = user(&pool, 1663021).await;
        let partial = user(&pool, 1663022).await;
        let full_snake = snake(&pool, full, "full").await;
        let partial_snake = snake(&pool, partial, "partial").await;
        let full_entry = entry(&pool, full_snake).await;
        let partial_entry = entry(&pool, partial_snake).await;
        sqlx::query("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'turtle', 'achievement'), ($1, 'tail', 'turtle', 'achievement'), ($1, 'head', 'judge', 'achievement'), ($1, 'tail', 'judge', 'achievement')")
            .bind(full).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'turtle', 'achievement'), ($1, 'head', 'judge', 'achievement')")
            .bind(partial).execute(&pool).await.unwrap();
        let before: Vec<(Uuid, Uuid, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT customization_grant_id, user_id, customization_type, slug, source, created_at FROM customization_grants WHERE user_id IN ($1, $2) ORDER BY user_id, customization_type, slug"
        ).bind(full).bind(partial).fetch_all(&pool).await.unwrap();
        let id = game(&pool, "Standard", "finished", Some(3)).await;
        participant(&pool, id, full_snake, 1, Some(full_entry)).await;
        participant(&pool, id, partial_snake, 1, Some(partial_entry)).await;
        assert_eq!(award_for_game(&pool, id).await.unwrap(), 2);
        let after_live: Vec<(Uuid, Uuid, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT customization_grant_id, user_id, customization_type, slug, source, created_at FROM customization_grants WHERE user_id IN ($1, $2) AND slug IN ('turtle', 'judge') ORDER BY user_id, customization_type, slug"
        ).bind(full).bind(partial).fetch_all(&pool).await.unwrap();
        assert_eq!(after_live, before);
        assert!(!has(&grants(&pool, partial).await, Tail::KIND, "turtle"));
        assert!(!has(&grants(&pool, partial).await, Tail::KIND, "judge"));
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        let after_backfill: Vec<(Uuid, Uuid, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT customization_grant_id, user_id, customization_type, slug, source, created_at FROM customization_grants WHERE user_id IN ($1, $2) AND slug IN ('turtle', 'judge') ORDER BY user_id, customization_type, slug"
        ).bind(full).bind(partial).fetch_all(&pool).await.unwrap();
        assert_eq!(after_backfill, before);
        assert!(!has(&grants(&pool, partial).await, Tail::KIND, "turtle"));
        assert!(!has(&grants(&pool, partial).await, Tail::KIND, "judge"));
    }

    #[tokio::test]
    #[ignore = "requires a disposable 5.16M-game database and ARENA_ACHIEVEMENT_PERF_DATABASE_URL"]
    async fn benchmark_achievement_counts_on_large_games_table() {
        use std::time::{Duration, Instant};

        let url = std::env::var("ARENA_ACHIEVEMENT_PERF_DATABASE_URL").unwrap();
        let pool = PgPool::connect(&url).await.unwrap();
        let background: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM games")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(background >= 5_160_000, "large-table fixture is incomplete");
        let rival_owner = user(&pool, 1663900).await;
        let rival = snake(&pool, rival_owner, "benchmark peer").await;
        let mut cases = Vec::new();
        for (index, name) in [
            "turtle-2999",
            "judge-999",
            "zero",
            "turtle-dirty",
            "judge-dirty",
        ]
        .iter()
        .enumerate()
        {
            let owner = user(&pool, 1663901 + index as i64).await;
            let snake_id = snake(&pool, owner, name).await;
            let ladder_entry = entry(&pool, snake_id).await;
            match *name {
                "turtle-2999" | "turtle-dirty" => {
                    seed_games(
                        &pool,
                        snake_id,
                        Some(ladder_entry),
                        Some(rival),
                        2_999,
                        2,
                        "finished",
                    )
                    .await;
                }
                "judge-999" => {
                    seed_games(
                        &pool,
                        snake_id,
                        Some(ladder_entry),
                        Some(rival),
                        999,
                        1,
                        "finished",
                    )
                    .await;
                    seed_games(
                        &pool,
                        snake_id,
                        Some(ladder_entry),
                        Some(rival),
                        1_998,
                        2,
                        "finished",
                    )
                    .await;
                }
                "judge-dirty" => {
                    seed_games(
                        &pool,
                        snake_id,
                        Some(ladder_entry),
                        Some(rival),
                        999,
                        1,
                        "finished",
                    )
                    .await;
                }
                _ => {}
            }
            if name.ends_with("dirty") {
                seed_games(
                    &pool,
                    snake_id,
                    Some(ladder_entry),
                    Some(rival),
                    1,
                    if *name == "judge-dirty" { 1 } else { 2 },
                    "waiting",
                )
                .await;
            }
            cases.push((*name, owner));
        }
        sqlx::query("ANALYZE games").execute(&pool).await.unwrap();
        sqlx::query("ANALYZE game_battlesnakes")
            .execute(&pool)
            .await
            .unwrap();
        let flags = WinFlags {
            ladder: true,
            ladder_win: true,
            ..WinFlags::default()
        };
        for (name, owner) in cases {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut conn = pool.acquire().await.unwrap();
            let start = Instant::now();
            assert_eq!(award_owner(&mut conn, owner, Some(flags)).await.unwrap(), 0);
            let cold = start.elapsed();
            let mut warmed = Vec::new();
            for _ in 0..20 {
                let start = Instant::now();
                assert_eq!(award_owner(&mut conn, owner, Some(flags)).await.unwrap(), 0);
                warmed.push(start.elapsed());
            }
            warmed.sort_unstable();
            println!(
                "{name}: first_after_idle={cold:?} warm_p95={:?}",
                warmed[18]
            );
        }
        for index in 0..10 {
            let batch_owner = user(&pool, 1663950 + index).await;
            let batch_snake = snake(&pool, batch_owner, "batch seed").await;
            let batch_entry = entry(&pool, batch_snake).await;
            seed_games(
                &pool,
                batch_snake,
                Some(batch_entry),
                Some(rival),
                2_999,
                2,
                "finished",
            )
            .await;
            let recent = game(&pool, "Standard", "finished", Some(1)).await;
            participant(&pool, recent, batch_snake, 2, Some(batch_entry)).await;
            participant(&pool, recent, rival, 1, None).await;
        }
        let start = Instant::now();
        reconcile_recent_achievements(&pool).await.unwrap();
        println!("reconciliation_transaction={:?}", start.elapsed());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_pagination_contention_and_recent_window(pool: PgPool) {
        let mut users = Vec::new();
        for number in 0..27 {
            users.push(user(&pool, 1644100 + number).await);
        }
        users.sort_unstable();
        let owner = users[0];
        let a = snake(&pool, owner, "a").await;
        let b = snake(&pool, owner, "b").await;
        let recent = game(&pool, "Standard", "finished", Some(1)).await;
        participant(&pool, recent, a, 1, None).await;
        participant(&pool, recent, b, 2, None).await;
        let old = game(&pool, GameType::Constrictor.as_str(), "finished", Some(3)).await;
        participant(&pool, old, a, 1, None).await;
        participant(&pool, old, b, 2, None).await;
        let legacy = game(&pool, "Standard", "finished", None).await;
        participant(&pool, legacy, a, 1, None).await;
        participant(&pool, legacy, b, 2, None).await;
        assert_eq!(reconcile_recent_achievements(&pool).await.unwrap(), 1);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "subway"));
        let mut locked = pool.begin().await.unwrap();
        sqlx::query(
            "SELECT singleton FROM achievement_backfill_cursor WHERE singleton = TRUE FOR UPDATE",
        )
        .fetch_one(&mut *locked)
        .await
        .unwrap();
        assert_eq!(
            backfill_achievement_page(&pool).await.unwrap(),
            BackfillPage {
                inserted: 0,
                processed: 0,
                complete: false
            }
        );
        locked.rollback().await.unwrap();
        let first = backfill_achievement_page(&pool).await.unwrap();
        assert_eq!(first.processed, 25);
        assert!(!first.complete);
        let cursor: Option<Uuid> = sqlx::query_scalar(
            "SELECT after_user_id FROM achievement_backfill_cursor WHERE singleton = TRUE",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor, Some(users[24]));
        assert!(has(&grants(&pool, owner).await, Head::KIND, "subway"));
        let second = backfill_achievement_page(&pool).await.unwrap();
        assert_eq!(second.processed, 2);
        assert!(second.complete);
        assert_eq!(
            backfill_achievement_page(&pool).await.unwrap(),
            BackfillPage {
                inserted: 0,
                processed: 0,
                complete: true
            }
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_matches_live_at_win_and_distinct_game_boundaries(pool: PgPool) {
        let owner = user(&pool, 1644300).await;
        let opponent = user(&pool, 1644301).await;
        let winner = snake(&pool, owner, "winner").await;
        let own_peer = snake(&pool, owner, "own peer").await;
        let rival = snake(&pool, opponent, "rival").await;
        let fourth = snake(&pool, opponent, "fourth").await;
        let rival_entry = entry(&pool, rival).await;
        let winner_entry = entry(&pool, winner).await;

        let solo = game(&pool, "Standard", "finished", None).await;
        participant(&pool, solo, winner, 1, None).await;
        assert_eq!(award_for_game(&pool, solo).await.unwrap(), 0);
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert!(grants(&pool, owner).await.is_empty());

        let three = game(&pool, "Standard", "finished", None).await;
        participant(&pool, three, winner, 1, None).await;
        participant(&pool, three, own_peer, 2, None).await;
        participant(&pool, three, rival, 3, None).await;
        assert_eq!(award_for_game(&pool, three).await.unwrap(), 1);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "monkey"));

        let lower = game(&pool, "constrictor", "finished", None).await;
        participant(&pool, lower, winner, 1, None).await;
        participant(&pool, lower, rival, 2, None).await;
        assert_eq!(award_for_game(&pool, lower).await.unwrap(), 0);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "subway"));

        let waiting = game(&pool, GameType::Constrictor.as_str(), "waiting", None).await;
        participant(&pool, waiting, winner, 1, None).await;
        participant(&pool, waiting, own_peer, 2, None).await;
        participant(&pool, waiting, rival, 3, None).await;
        participant(&pool, waiting, fourth, 4, None).await;
        assert_eq!(award_for_game(&pool, waiting).await.unwrap(), 0);

        let opponent_ladder = game(&pool, "Standard", "finished", None).await;
        participant(&pool, opponent_ladder, winner, 1, None).await;
        participant(&pool, opponent_ladder, rival, 2, Some(rival_entry)).await;
        assert_eq!(award_for_game(&pool, opponent_ladder).await.unwrap(), 0);
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "judge"));
        let live_before_four = grants(&pool, owner).await;
        sqlx::query("DELETE FROM customization_grants WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert_eq!(grants(&pool, owner).await, live_before_four);

        let four = game(&pool, GameType::Constrictor.as_str(), "finished", None).await;
        participant(&pool, four, winner, 1, None).await;
        participant(&pool, four, own_peer, 2, None).await;
        participant(&pool, four, rival, 3, None).await;
        participant(&pool, four, fourth, 4, None).await;
        assert_eq!(award_for_game(&pool, four).await.unwrap(), 4);

        seed_games(
            &pool,
            winner,
            Some(winner_entry),
            Some(rival),
            2_998,
            2,
            "finished",
        )
        .await;
        let almost = game(&pool, "Standard", "finished", None).await;
        participant(&pool, almost, winner, 2, Some(winner_entry)).await;
        participant(&pool, almost, own_peer, 1, None).await;
        assert_eq!(award_for_game(&pool, almost).await.unwrap(), 0);
        let live_2999 = grants(&pool, owner).await;
        assert!(!has(&live_2999, Head::KIND, "turtle"));
        assert!(!has(&live_2999, Head::KIND, "judge"));
        sqlx::query("DELETE FROM customization_grants WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert_eq!(grants(&pool, owner).await, live_2999);

        let three_thousandth = game(&pool, "Standard", "finished", None).await;
        participant(&pool, three_thousandth, winner, 2, Some(winner_entry)).await;
        participant(&pool, three_thousandth, own_peer, 2, None).await;
        assert_eq!(award_for_game(&pool, three_thousandth).await.unwrap(), 2);
        let live_3000 = grants(&pool, owner).await;
        assert!(has(&live_3000, Head::KIND, "turtle"));
        assert!(has(&live_3000, Tail::KIND, "turtle"));
        sqlx::query("DELETE FROM customization_grants WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert_eq!(grants(&pool, owner).await, live_3000);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_awards_and_backfill_rollback(pool: PgPool) {
        let owner = user(&pool, 1644200).await;
        let a = snake(&pool, owner, "a").await;
        let b = snake(&pool, owner, "b").await;
        let id = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, id, a, 1, None).await;
        participant(&pool, id, b, 2, None).await;
        // Hold an uncommitted conflicting grant so the award must reach the
        // unique-index conflict path rather than seeing an already owned item.
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'frog', 'admin')")
            .bind(owner).execute(&mut *holder).await.unwrap();
        let release_holder = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let blocked: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE 'INSERT INTO customization_grants%')",
                    )
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if blocked {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("award never blocked on the uncommitted grant");
            holder.commit().await.unwrap();
        };
        let (award, ()) = tokio::join!(award_for_game(&pool, id), release_holder);
        assert_eq!(award.unwrap(), 0);
        assert_eq!(grants(&pool, owner).await.len(), 1);

        sqlx::query("DELETE FROM customization_grants WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE FUNCTION reject_achievement_grant() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected grant failure'; END $$")
            .execute(&pool).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_achievement_grant BEFORE INSERT ON customization_grants FOR EACH ROW EXECUTE FUNCTION reject_achievement_grant()")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.is_err());
        let cursor: Option<Uuid> = sqlx::query_scalar(
            "SELECT after_user_id FROM achievement_backfill_cursor WHERE singleton = TRUE",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cursor, None);
        assert!(grants(&pool, owner).await.is_empty());
        sqlx::query("DROP TRIGGER reject_achievement_grant ON customization_grants")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP FUNCTION reject_achievement_grant()")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(backfill_achievement_page(&pool).await.unwrap().inserted, 1);
        assert_eq!(backfill_achievement_page(&pool).await.unwrap().inserted, 0);
    }
}
