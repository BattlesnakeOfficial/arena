use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate, Utc};
use color_eyre::eyre::{Context as _, eyre};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    pub as_of_utc_date: NaiveDate,
    pub live_tracking_started_on: NaiveDate,
    pub backfill_complete: bool,
    pub headlines: StatsHeadlines,
    pub daily_active_users: Vec<DailyCount>,
    pub weekly_active_users: Vec<WeeklyCount>,
    pub daily_games: Vec<DailyGames>,
    pub weekly_games: Vec<WeeklyGames>,
    pub weekly_growth: Vec<WeeklyGrowth>,
    pub weekly_active_snakes: Vec<WeeklySnakes>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsHeadlines {
    pub dau: i64,
    pub wau: i64,
    pub mau: i64,
    pub dau_mau_percent: f64,
    pub games_7d: i64,
    pub active_snakes_7d: i64,
    pub registered_users: i64,
    pub total_snakes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyCount {
    pub date: NaiveDate,
    pub count: i64,
}
#[derive(Debug, Clone, Serialize)]
pub struct WeeklyCount {
    pub week_start: NaiveDate,
    pub count: i64,
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct DailyGames {
    pub date: NaiveDate,
    pub custom: i64,
    pub leaderboard: i64,
    pub tournament: i64,
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct WeeklyGames {
    pub week_start: NaiveDate,
    pub custom: i64,
    pub leaderboard: i64,
    pub tournament: i64,
}
#[derive(Debug, Clone, Serialize)]
pub struct WeeklyGrowth {
    pub week_start: NaiveDate,
    pub new_users: i64,
    pub new_snakes: i64,
}
#[derive(Debug, Clone, Serialize)]
pub struct WeeklySnakes {
    pub week_start: NaiveDate,
    pub count: i64,
}

fn midnight(day: NaiveDate) -> cja::Result<chrono::DateTime<Utc>> {
    Ok(day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| eyre!("Invalid UTC day"))?
        .and_utc())
}

fn days_before(day: NaiveDate, n: i64) -> cja::Result<NaiveDate> {
    day.checked_sub_signed(Duration::days(n))
        .ok_or_else(|| eyre!("Stats date out of range"))
}

fn monday(day: NaiveDate) -> cja::Result<NaiveDate> {
    days_before(day, i64::from(day.weekday().num_days_from_monday()))
}

impl StatsSnapshot {
    pub async fn fetch(db: &PgPool, today_utc: NaiveDate) -> cja::Result<Self> {
        let as_of_utc_date = days_before(today_utc, 1)?;
        let daily_start = days_before(today_utc, 90)?;
        let seven_start = days_before(today_utc, 7)?;
        let mau_start = days_before(today_utc, 28)?;
        let current_monday = monday(today_utc)?;
        let weekly_start = days_before(current_monday, 52 * 7)?;
        let today_ts = midnight(today_utc)?;
        let weekly_start_ts = midnight(weekly_start)?;
        let seven_start_ts = midnight(seven_start)?;

        let tracking = sqlx::query!(
            "SELECT (tracking_started_at AT TIME ZONE 'UTC')::date AS \"start_day!: NaiveDate\", backfill_completed_at FROM stats_tracking_start WHERE singleton = TRUE"
        ).fetch_optional(db).await.wrap_err("Failed to read stats tracking epoch")?
         .ok_or_else(|| eyre!("Missing stats tracking singleton"))?;
        let live_tracking_started_on = tracking
            .start_day
            .succ_opt()
            .ok_or_else(|| eyre!("Invalid tracking epoch"))?;

        let activity_rows = sqlx::query!(
            // The (user_id, day) primary key makes COUNT(*) a distinct-user count for each day.
            "SELECT day, COUNT(*) AS \"count!: i64\" FROM user_activity_days WHERE day >= $1 AND day < $2 GROUP BY day",
            daily_start, today_utc
        ).fetch_all(db).await.wrap_err("Failed to fetch daily account activity")?;
        let activity_map: BTreeMap<_, _> = activity_rows
            .into_iter()
            .map(|row| (row.day, row.count))
            .collect();
        let daily_active_users: Vec<_> = (0..90)
            .map(|offset| {
                let date = daily_start + Duration::days(offset);
                DailyCount {
                    date,
                    count: *activity_map.get(&date).unwrap_or(&0),
                }
            })
            .collect();

        let weekly_activity_rows = sqlx::query!(
            "SELECT date_trunc('week', day::timestamp)::date AS \"week_start!: NaiveDate\", COUNT(DISTINCT user_id) AS \"count!: i64\" FROM user_activity_days WHERE day >= $1 AND day < $2 GROUP BY 1",
            weekly_start, current_monday
        ).fetch_all(db).await.wrap_err("Failed to fetch weekly account activity")?;
        let weekly_activity_map: BTreeMap<_, _> = weekly_activity_rows
            .into_iter()
            .map(|row| (row.week_start, row.count))
            .collect();
        let weekly_active_users: Vec<_> = (0..52)
            .map(|offset| {
                let week_start = weekly_start + Duration::weeks(offset);
                WeeklyCount {
                    week_start,
                    count: *weekly_activity_map.get(&week_start).unwrap_or(&0),
                }
            })
            .collect();

        let active_headlines = sqlx::query!(
            "SELECT COUNT(DISTINCT user_id) FILTER (WHERE day = $1) AS \"dau!: i64\", COUNT(DISTINCT user_id) FILTER (WHERE day >= $2) AS \"wau!: i64\", COUNT(DISTINCT user_id) FILTER (WHERE day >= $3) AS \"mau!: i64\" FROM user_activity_days WHERE day >= $3 AND day < $4",
            as_of_utc_date, seven_start, mau_start, today_utc
        ).fetch_one(db).await.wrap_err("Failed to fetch activity headlines")?;
        let average_dau = daily_active_users
            .iter()
            .rev()
            .take(28)
            .map(|row| row.count)
            .sum::<i64>() as f64
            / 28.0;
        let dau_mau_percent = if active_headlines.mau == 0 {
            0.0
        } else {
            average_dau / active_headlines.mau as f64 * 100.0
        };

        let game_rows = sqlx::query!(
            r#"SELECT (g.created_at AT TIME ZONE 'UTC')::date AS "day!: NaiveDate",
               CASE WHEN mg.game_id IS NOT NULL THEN 'tournament'
                    WHEN lg.game_id IS NOT NULL THEN 'leaderboard'
                    ELSE 'custom' END AS "source!: String",
               COUNT(*) AS "count!: i64"
               FROM games g
               LEFT JOIN leaderboard_games lg ON lg.game_id = g.game_id
               LEFT JOIN match_games mg ON mg.game_id = g.game_id
               WHERE g.status = 'finished' AND g.engine_game_id IS NULL
                 AND g.created_at >= $1 AND g.created_at < $2
               GROUP BY 1, 2"#,
            weekly_start_ts,
            today_ts
        )
        .fetch_all(db)
        .await
        .wrap_err("Failed to fetch played games")?;
        let mut games_by_day: BTreeMap<NaiveDate, DailyGames> = BTreeMap::new();
        for row in game_rows {
            let entry = games_by_day.entry(row.day).or_insert_with(|| DailyGames {
                date: row.day,
                ..DailyGames::default()
            });
            match row.source.as_str() {
                "custom" => entry.custom = row.count,
                "leaderboard" => entry.leaderboard = row.count,
                "tournament" => entry.tournament = row.count,
                other => return Err(eyre!("Unknown game source: {other}")),
            }
        }
        let daily_games: Vec<_> = (0..90)
            .map(|offset| {
                let date = daily_start + Duration::days(offset);
                games_by_day.get(&date).cloned().unwrap_or(DailyGames {
                    date,
                    ..DailyGames::default()
                })
            })
            .collect();
        let weekly_games: Vec<_> = (0..52)
            .map(|offset| {
                let week_start = weekly_start + Duration::weeks(offset);
                let mut total = WeeklyGames {
                    week_start,
                    ..WeeklyGames::default()
                };
                for day_offset in 0..7 {
                    if let Some(row) = games_by_day.get(&(week_start + Duration::days(day_offset)))
                    {
                        total.custom += row.custom;
                        total.leaderboard += row.leaderboard;
                        total.tournament += row.tournament;
                    }
                }
                total
            })
            .collect();
        let games_7d = games_by_day
            .range(seven_start..today_utc)
            .map(|(_, row)| row.custom + row.leaderboard + row.tournament)
            .sum();

        let weekly_snake_rows = sqlx::query!(
            r#"SELECT (date_trunc('week', g.created_at AT TIME ZONE 'UTC'))::date AS "week_start!: NaiveDate",
               COUNT(DISTINCT COALESCE(gb.battlesnake_id, le.battlesnake_id)) AS "count!: i64"
               FROM games g
               JOIN game_battlesnakes gb ON gb.game_id = g.game_id
               LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
               WHERE g.status = 'finished' AND g.engine_game_id IS NULL
                 AND g.created_at >= $1 AND g.created_at < $2
               GROUP BY 1"#,
            weekly_start_ts, midnight(current_monday)?
        ).fetch_all(db).await.wrap_err("Failed to fetch weekly active snakes")?;
        let weekly_snake_map: BTreeMap<_, _> = weekly_snake_rows
            .into_iter()
            .map(|row| (row.week_start, row.count))
            .collect();
        let weekly_active_snakes: Vec<_> = (0..52)
            .map(|offset| {
                let week_start = weekly_start + Duration::weeks(offset);
                WeeklySnakes {
                    week_start,
                    count: *weekly_snake_map.get(&week_start).unwrap_or(&0),
                }
            })
            .collect();
        let active_snakes_7d = sqlx::query_scalar!(
            r#"SELECT COUNT(DISTINCT COALESCE(gb.battlesnake_id, le.battlesnake_id)) AS "count!: i64"
               FROM games g
               JOIN game_battlesnakes gb ON gb.game_id = g.game_id
               LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
               WHERE g.status = 'finished' AND g.engine_game_id IS NULL
                 AND g.created_at >= $1 AND g.created_at < $2"#,
            seven_start_ts, today_ts
        ).fetch_one(db).await.wrap_err("Failed to fetch seven-day active snakes")?;

        let user_growth_rows = sqlx::query!(
            "SELECT (date_trunc('week', created_at AT TIME ZONE 'UTC'))::date AS \"week_start!: NaiveDate\", COUNT(*) AS \"count!: i64\" FROM users WHERE created_at >= $1 AND created_at < $2 GROUP BY 1",
            weekly_start_ts, midnight(current_monday)?
        ).fetch_all(db).await.wrap_err("Failed to fetch user growth")?;
        let snake_growth_rows = sqlx::query!(
            "SELECT (date_trunc('week', created_at AT TIME ZONE 'UTC'))::date AS \"week_start!: NaiveDate\", COUNT(*) AS \"count!: i64\" FROM battlesnakes WHERE created_at >= $1 AND created_at < $2 GROUP BY 1",
            weekly_start_ts, midnight(current_monday)?
        ).fetch_all(db).await.wrap_err("Failed to fetch snake growth")?;
        let user_growth: BTreeMap<_, _> = user_growth_rows
            .into_iter()
            .map(|row| (row.week_start, row.count))
            .collect();
        let snake_growth: BTreeMap<_, _> = snake_growth_rows
            .into_iter()
            .map(|row| (row.week_start, row.count))
            .collect();
        let weekly_growth: Vec<_> = (0..52)
            .map(|offset| {
                let week_start = weekly_start + Duration::weeks(offset);
                WeeklyGrowth {
                    week_start,
                    new_users: *user_growth.get(&week_start).unwrap_or(&0),
                    new_snakes: *snake_growth.get(&week_start).unwrap_or(&0),
                }
            })
            .collect();

        let totals = sqlx::query!(
            "SELECT (SELECT COUNT(*) FROM users WHERE created_at < $1) AS \"users!: i64\", (SELECT COUNT(*) FROM battlesnakes WHERE created_at < $1) AS \"snakes!: i64\"",
            today_ts
        ).fetch_one(db).await.wrap_err("Failed to fetch community totals")?;

        Ok(Self {
            as_of_utc_date,
            live_tracking_started_on,
            backfill_complete: tracking.backfill_completed_at.is_some(),
            headlines: StatsHeadlines {
                dau: active_headlines.dau,
                wau: active_headlines.wau,
                mau: active_headlines.mau,
                dau_mau_percent,
                games_7d,
                active_snakes_7d,
                registered_users: totals.users,
                total_snakes: totals.snakes,
            },
            daily_active_users,
            weekly_active_users,
            daily_games,
            weekly_games,
            weekly_growth,
            weekly_active_snakes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{PgPool, postgres::PgPoolOptions};

    #[sqlx::test(migrations = "../migrations")]
    async fn empty_database_has_complete_zero_filled_calendars(db: PgPool) {
        let today = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let stats = StatsSnapshot::fetch(&db, today).await.unwrap();
        assert_eq!(stats.daily_active_users.len(), 90);
        assert_eq!(stats.weekly_active_users.len(), 52);
        assert_eq!(stats.daily_games.len(), 90);
        assert_eq!(stats.weekly_games.len(), 52);
        assert_eq!(stats.weekly_growth.len(), 52);
        assert_eq!(stats.weekly_active_snakes.len(), 52);
        assert_eq!(
            stats.daily_active_users.last().unwrap().date.to_string(),
            "2026-09-27"
        );
        assert_eq!(
            stats
                .weekly_active_users
                .last()
                .unwrap()
                .week_start
                .to_string(),
            "2026-09-21"
        );
        assert_eq!(stats.headlines.dau, 0);
        assert_eq!(stats.headlines.dau_mau_percent, 0.0);
        assert!(!stats.backfill_complete);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn accounts_games_and_snakes_respect_utc_and_iso_boundaries(db: PgPool) {
        let u1 = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token, created_at) VALUES (1, 'u1-secret', '', '2026-09-20 23:59:59+00') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let u2 = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token, created_at) VALUES (2, 'u2-secret', '', '2026-09-21 00:00:00+00') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let s1 = sqlx::query_scalar!(
            "INSERT INTO battlesnakes(user_id, name, url, created_at) VALUES ($1, 's1-secret', 'https://example.com', '2026-09-20 23:59:59+00') RETURNING battlesnake_id", u1
        ).fetch_one(&db).await.unwrap();
        let s2 = sqlx::query_scalar!(
            "INSERT INTO battlesnakes(user_id, name, url, created_at) VALUES ($1, 's2-secret', 'https://example.com', '2026-09-21 00:00:00+00') RETURNING battlesnake_id", u2
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO user_activity_days(user_id, day) VALUES ($1, '2026-09-27'), ($1, '2026-09-26'), ($2, '2026-09-27'), ($2, '2026-09-21')",
            u1, u2
        ).execute(&db).await.unwrap();
        let leaderboard_id = sqlx::query_scalar!(
            "INSERT INTO leaderboards(name) VALUES ('Test') RETURNING leaderboard_id"
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let entry_id = sqlx::query_scalar!(
            "INSERT INTO leaderboard_entries(leaderboard_id, battlesnake_id) VALUES ($1, $2) RETURNING leaderboard_entry_id",
            leaderboard_id, s2
        ).fetch_one(&db).await.unwrap();
        let tournament_id = sqlx::query_scalar!(
            "INSERT INTO tournaments(name, user_id) VALUES ('Test', $1) RETURNING tournament_id",
            u1
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let match_id = sqlx::query_scalar!(
            "INSERT INTO tournament_matches(tournament_id, round, position, visual_column, visual_row) VALUES ($1, 1, 0, 0, 0) RETURNING match_id",
            tournament_id
        ).fetch_one(&db).await.unwrap();

        let custom = sqlx::query_scalar!(
            "INSERT INTO games(board_size, game_type, status, created_at) VALUES ('11x11', 'Standard', 'finished', '2026-09-20 23:59:59+00') RETURNING game_id"
        ).fetch_one(&db).await.unwrap();
        let leaderboard = sqlx::query_scalar!(
            "INSERT INTO games(board_size, game_type, status, created_at) VALUES ('11x11', 'Standard', 'finished', '2026-09-21 00:00:00+00') RETURNING game_id"
        ).fetch_one(&db).await.unwrap();
        let tournament = sqlx::query_scalar!(
            "INSERT INTO games(board_size, game_type, status, created_at) VALUES ('11x11', 'Standard', 'finished', '2026-09-27 23:59:59+00') RETURNING game_id"
        ).fetch_one(&db).await.unwrap();
        let double_linked = sqlx::query_scalar!(
            "INSERT INTO games(board_size, game_type, status, created_at) VALUES ('11x11', 'Standard', 'finished', '2026-09-27 23:59:59+00') RETURNING game_id"
        ).fetch_one(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO leaderboard_games(leaderboard_id, game_id) VALUES ($1, $2), ($1, $3)",
            leaderboard_id,
            leaderboard,
            double_linked
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO match_games(match_id, game_id, game_number) VALUES ($1, $2, 1), ($1, $3, 2)",
            match_id, tournament, double_linked
        ).execute(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO game_battlesnakes(game_id, battlesnake_id) VALUES ($1, $2), ($3, $2), ($4, $2)",
            custom, s1, tournament, double_linked
        ).execute(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO game_battlesnakes(game_id, leaderboard_entry_id) VALUES ($1, $2), ($3, $2)",
            leaderboard, entry_id, double_linked
        ).execute(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO games(board_size, game_type, status, created_at, engine_game_id) VALUES ('11x11', 'Standard', 'finished', '2026-09-27 12:00+00', 'legacy'), ('11x11', 'Standard', 'failed', '2026-09-27 12:00+00', NULL), ('11x11', 'Standard', 'running', '2026-09-27 12:00+00', NULL), ('11x11', 'Standard', 'finished', '2026-09-28 00:00+00', NULL)"
        ).execute(&db).await.unwrap();

        let today = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let stats = StatsSnapshot::fetch(&db, today).await.unwrap();
        assert_eq!(
            (
                stats.headlines.dau,
                stats.headlines.wau,
                stats.headlines.mau
            ),
            (2, 2, 2)
        );
        assert_eq!(stats.headlines.games_7d, 3);
        assert_eq!(stats.headlines.active_snakes_7d, 2);
        assert_eq!(stats.headlines.registered_users, 2);
        assert_eq!(stats.headlines.total_snakes, 2);
        let sunday = stats.daily_games.last().unwrap();
        assert_eq!(
            (sunday.custom, sunday.leaderboard, sunday.tournament),
            (0, 0, 2)
        );
        let week = stats.weekly_games.last().unwrap();
        assert_eq!((week.custom, week.leaderboard, week.tournament), (0, 1, 2));
        assert_eq!(stats.weekly_games[50].custom, 1);
        assert_eq!(stats.weekly_active_snakes.last().unwrap().count, 2);
        assert_eq!(stats.weekly_growth.last().unwrap().new_users, 1);
        assert_eq!(stats.weekly_growth.last().unwrap().new_snakes, 1);
        assert_eq!(stats.weekly_active_users.last().unwrap().count, 2);
        assert_eq!(stats.daily_active_users.last().unwrap().count, 2);
        assert!((stats.headlines.dau_mau_percent - (4.0 / 28.0 / 2.0 * 100.0)).abs() < 0.001);
    }

    /// Success criteria: "The DAU and WAU charts begin at the first complete
    /// tracked day or week. Periods before tracking began are not drawn at
    /// all, not even as zeros." The 90-day and 52-week active-user series
    /// must therefore never contain a date earlier than
    /// `live_tracking_started_on`, regardless of how far back the fixed
    /// 90-day/52-week window would otherwise reach.
    #[sqlx::test(migrations = "../migrations")]
    async fn active_user_series_excludes_periods_before_tracking_start(db: PgPool) {
        let today = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        // Tracking began 10 days before "today", so most of the fixed 90-day
        // / 52-week windows predate the tracking epoch and must be omitted
        // entirely rather than zero-filled.
        let tracking_started_at = today
            .checked_sub_signed(Duration::days(10))
            .unwrap()
            .and_hms_opt(3, 0, 0)
            .unwrap()
            .and_utc();
        sqlx::query!(
            "UPDATE stats_tracking_start SET tracking_started_at = $1 WHERE singleton = TRUE",
            tracking_started_at
        )
        .execute(&db)
        .await
        .unwrap();

        let stats = StatsSnapshot::fetch(&db, today).await.unwrap();

        assert!(
            stats
                .daily_active_users
                .iter()
                .all(|row| row.date >= stats.live_tracking_started_on),
            "daily active-user series must start no earlier than tracking start \
             ({:?}); got entries as early as {:?}",
            stats.live_tracking_started_on,
            stats.daily_active_users.first().map(|r| r.date)
        );
        assert!(
            stats
                .weekly_active_users
                .iter()
                .all(|row| row.week_start >= stats.live_tracking_started_on),
            "weekly active-user series must start no earlier than tracking start \
             ({:?}); got entries as early as {:?}",
            stats.live_tracking_started_on,
            stats.weekly_active_users.first().map(|r| r.week_start)
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn utc_buckets_do_not_depend_on_database_session_timezone(db: PgPool) {
        sqlx::query!(
            "INSERT INTO games(board_size, game_type, status, created_at) VALUES ('11x11', 'Standard', 'finished', '2026-09-20 23:59:59+00'), ('11x11', 'Standard', 'finished', '2026-09-21 00:00:00+00')"
        ).execute(&db).await.unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let utc = StatsSnapshot::fetch(&db, today).await.unwrap();
        let options = (*db.connect_options()).clone();
        let ny_db = PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query!("SET TIME ZONE 'America/New_York'")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .unwrap();
        let timezone = sqlx::query_scalar!("SHOW TimeZone")
            .fetch_one(&ny_db)
            .await
            .unwrap();
        assert_eq!(timezone.as_deref(), Some("America/New_York"));
        let ny = StatsSnapshot::fetch(&ny_db, today).await.unwrap();
        assert_eq!(utc.daily_games[82].custom, ny.daily_games[82].custom);
        assert_eq!(
            utc.weekly_games.last().unwrap().custom,
            ny.weekly_games.last().unwrap().custom
        );
        assert_eq!(ny.weekly_games.last().unwrap().custom, 1);
        assert_eq!(ny.weekly_games[50].custom, 1);
    }
}
