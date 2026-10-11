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
                description: "Play in 100 finished games across your snakes.",
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
                description: "Win 10 finished ladder games across your snakes.",
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
}

impl WinFlags {
    fn include(&mut self, placement: Option<i32>, participants: i64, game_type: &str) {
        if placement == Some(1) && participants >= 2 {
            self.first = true;
            self.constrictor |= game_type == GameType::Constrictor.as_str();
            self.crowd |= participants >= 4;
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

async fn distinct_finished_count(
    conn: &mut PgConnection,
    user_id: Uuid,
    cap: i64,
    ladder_wins: bool,
) -> cja::Result<i64> {
    // Deduplicate via the per-snake/per-entry indexes before probing games; the
    // indexed participant rows are cheap, while each games lookup is costly.
    // For Ladder Regular the owner's winning participant row must have the entry.
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*)::bigint AS "count!" FROM (
            SELECT 1 FROM (
                SELECT DISTINCT owned.game_id FROM (
                SELECT gb.game_id FROM battlesnakes bs
                JOIN game_battlesnakes gb ON gb.battlesnake_id = bs.battlesnake_id
                WHERE bs.user_id = $1
                  AND (NOT $3::bool OR (gb.placement = 1 AND gb.leaderboard_entry_id IS NOT NULL))
                UNION ALL
                SELECT gb.game_id FROM battlesnakes bs
                JOIN leaderboard_entries le ON le.battlesnake_id = bs.battlesnake_id
                JOIN game_battlesnakes gb ON gb.leaderboard_entry_id = le.leaderboard_entry_id
                WHERE bs.user_id = $1 AND gb.battlesnake_id IS NULL
                  AND (NOT $3::bool OR gb.placement = 1)
                ) owned
            ) distinct_games
            JOIN games g ON g.game_id = distinct_games.game_id
            WHERE g.status = 'finished'
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
                distinct_finished_count(&mut *conn, user_id, 100, false).await? >= 100
            }
            Achievement::LadderRegular => {
                distinct_finished_count(&mut *conn, user_id, 10, true).await? >= 10
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
        r#"SELECT bs.user_id, gb.placement FROM game_battlesnakes gb
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
        r#"SELECT g.game_id, g.game_type, bs.user_id, gb.placement,
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
        let mut last = Uuid::nil();
        for number in 0..100 {
            let id = game(&pool, "Standard", "finished", Some(0)).await;
            last = id;
            // Two owner rows in each game still count as one distinct game.
            participant(&pool, id, first, 1, Some(first_entry)).await;
            participant(&pool, id, second, 2, Some(second_entry)).await;
            let inserted = award_for_game(&pool, id).await.unwrap();
            if number == 8 {
                assert_eq!(inserted, 0);
                assert!(!has(&grants(&pool, owner).await, Tail::KIND, "judge"));
            }
            if number == 9 {
                assert_eq!(inserted, 1);
            }
            if number == 98 {
                assert_eq!(inserted, 0);
                let owned = grants(&pool, owner).await;
                assert!(!has(&owned, Tail::KIND, "turtle"));
            }
            if number == 99 {
                assert_eq!(inserted, 1);
            }
        }
        assert_eq!(award_for_game(&pool, last).await.unwrap(), 0);
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

        // Five finished games above plus 94 distinct games reaches 99.
        for _ in 0..94 {
            let id = game(&pool, "Standard", "finished", None).await;
            participant(&pool, id, winner, 2, None).await;
            participant(&pool, id, own_peer, 2, None).await;
            assert_eq!(award_for_game(&pool, id).await.unwrap(), 0);
        }
        let live_99 = grants(&pool, owner).await;
        assert!(!has(&live_99, Head::KIND, "turtle"));
        assert!(!has(&live_99, Head::KIND, "judge"));
        sqlx::query("DELETE FROM customization_grants WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert_eq!(grants(&pool, owner).await, live_99);

        let hundredth = game(&pool, "Standard", "finished", None).await;
        participant(&pool, hundredth, winner, 2, None).await;
        participant(&pool, hundredth, own_peer, 2, None).await;
        assert_eq!(award_for_game(&pool, hundredth).await.unwrap(), 2);
        let live_100 = grants(&pool, owner).await;
        assert!(has(&live_100, Head::KIND, "turtle"));
        assert!(has(&live_100, Tail::KIND, "turtle"));
        sqlx::query("DELETE FROM customization_grants WHERE user_id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool).await.unwrap();
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert_eq!(grants(&pool, owner).await, live_100);
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
