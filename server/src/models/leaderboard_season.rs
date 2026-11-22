//! Leaderboard seasons: dated windows a league's ratings are scoped to.
//!
//! The active season is derived, never stored: `starts_at <= now < ends_at`
//! (half-open; an open-ended season has `ends_at IS NULL`). The
//! `leaderboard_seasons_no_overlap` exclusion constraint guarantees at most one
//! active season per league. Predicates use `clock_timestamp()` rather than
//! `now()` so a long transaction (or one that waited on a lock) still sees
//! the wall clock, not its start time.

use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LeaderboardSeason {
    pub season_id: Uuid,
    pub leaderboard_id: Uuid,
    pub name: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The league's active season, or `None` during a gap between seasons.
pub async fn get_active_season<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
) -> cja::Result<Option<LeaderboardSeason>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as!(
        LeaderboardSeason,
        r#"SELECT season_id, leaderboard_id, name, starts_at, ends_at
         FROM leaderboard_seasons
         WHERE leaderboard_id = $1
           AND starts_at <= clock_timestamp()
           AND (ends_at IS NULL OR clock_timestamp() < ends_at)"#,
        leaderboard_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch active leaderboard season")
}

/// [`get_active_season`], locking the season row `FOR SHARE` so an
/// admin can't end (or move) it until the caller's transaction commits.
///
/// When the lock had to wait, Postgres re-checks only this row's predicates
/// against the committed version, and `clock_timestamp()` is re-evaluated,
/// so a season that ended while we waited comes back as `None`.
pub async fn lock_active_season<'e, E>(
    executor: E,
    leaderboard_id: Uuid,
) -> cja::Result<Option<LeaderboardSeason>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    sqlx::query_as!(
        LeaderboardSeason,
        r#"SELECT season_id, leaderboard_id, name, starts_at, ends_at
         FROM leaderboard_seasons
         WHERE leaderboard_id = $1
           AND starts_at <= clock_timestamp()
           AND (ends_at IS NULL OR clock_timestamp() < ends_at)
         FOR SHARE"#,
        leaderboard_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to lock active leaderboard season")
}

// --- Season selection (league page, rankings API, cards, admin) ---

/// Where a season sits relative to the DB clock. Derived on every read,
/// never stored; independent of the league's admin pause (`disabled_at`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeasonPhase {
    Upcoming,
    Active,
    Ended,
}

impl SeasonPhase {
    fn at(
        starts_at: chrono::DateTime<chrono::Utc>,
        ends_at: Option<chrono::DateTime<chrono::Utc>>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        if now < starts_at {
            SeasonPhase::Upcoming
        } else if ends_at.is_some_and(|end| now >= end) {
            SeasonPhase::Ended
        } else {
            SeasonPhase::Active
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SeasonPhase::Upcoming => "Upcoming",
            SeasonPhase::Active => "Active",
            SeasonPhase::Ended => "Ended",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SeasonInfo {
    pub season: LeaderboardSeason,
    pub phase: SeasonPhase,
}

/// All seasons of one league, ordered `starts_at ASC`, plus which one this
/// request should show and when the next one starts.
#[derive(Debug, Clone)]
pub struct SeasonSelection {
    pub seasons: Vec<SeasonInfo>,
    pub selected: Option<SeasonInfo>,
    pub next_season_starts_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl SeasonSelection {
    /// The league's currently active season (at most one: seasons can't
    /// overlap). Drives league-live facts, not the selected standings.
    pub fn league_active(&self) -> Option<&SeasonInfo> {
        self.seasons
            .iter()
            .find(|info| info.phase == SeasonPhase::Active)
    }
}

#[derive(Debug, Clone)]
pub enum SeasonLookup {
    Found(SeasonSelection),
    /// The requested `?season=` isn't one of this league's seasons.
    Foreign,
}

struct SeasonRow {
    season_id: Uuid,
    leaderboard_id: Uuid,
    name: String,
    starts_at: chrono::DateTime<chrono::Utc>,
    ends_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
}

/// Pure selection rule: requested (must be one of `seasons`, else
/// `Foreign`) -> the active season -> the most recently ended -> none.
fn build_selection(
    rows: Vec<(LeaderboardSeason, SeasonPhase)>,
    requested: Option<Uuid>,
) -> SeasonLookup {
    let seasons: Vec<SeasonInfo> = rows
        .into_iter()
        .map(|(season, phase)| SeasonInfo { season, phase })
        .collect();
    let next_season_starts_at = seasons
        .iter()
        .filter(|info| info.phase == SeasonPhase::Upcoming)
        .map(|info| info.season.starts_at)
        .min();
    let selected = match requested {
        Some(id) => match seasons.iter().find(|info| info.season.season_id == id) {
            Some(info) => Some(info.clone()),
            None => return SeasonLookup::Foreign,
        },
        None => seasons
            .iter()
            .find(|info| info.phase == SeasonPhase::Active)
            .or_else(|| {
                seasons
                    .iter()
                    .filter(|info| info.phase == SeasonPhase::Ended)
                    .max_by_key(|info| info.season.ends_at)
            })
            .cloned(),
    };
    SeasonLookup::Found(SeasonSelection {
        seasons,
        selected,
        next_season_starts_at,
    })
}

/// Every season of a league with its phase, all judged against one
/// `clock_timestamp()` read in the same statement, and the season this
/// request should show (see [`build_selection`]).
pub async fn select_season(
    pool: &sqlx::PgPool,
    leaderboard_id: Uuid,
    requested: Option<Uuid>,
) -> cja::Result<SeasonLookup> {
    let rows = sqlx::query_as!(
        SeasonRow,
        r#"SELECT s.season_id, s.leaderboard_id, s.name, s.starts_at, s.ends_at,
                clock_timestamp() AS "now!"
         FROM leaderboard_seasons s
         WHERE s.leaderboard_id = $1
         ORDER BY s.starts_at ASC, s.season_id ASC"#,
        leaderboard_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch leaderboard seasons")?;

    let rows = rows
        .into_iter()
        .map(|row| {
            let phase = SeasonPhase::at(row.starts_at, row.ends_at, row.now);
            (
                LeaderboardSeason {
                    season_id: row.season_id,
                    leaderboard_id: row.leaderboard_id,
                    name: row.name,
                    starts_at: row.starts_at,
                    ends_at: row.ends_at,
                },
                phase,
            )
        })
        .collect();
    Ok(build_selection(rows, requested))
}

/// Ids of every league currently inside a season, in one query.
pub async fn leaderboards_with_active_season(
    pool: &sqlx::PgPool,
) -> cja::Result<std::collections::HashSet<Uuid>> {
    let ids = sqlx::query_scalar!(
        r#"SELECT DISTINCT leaderboard_id FROM leaderboard_seasons
         WHERE starts_at <= clock_timestamp()
           AND (ends_at IS NULL OR clock_timestamp() < ends_at)"#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch leaderboards with an active season")?;
    Ok(ids.into_iter().collect())
}

// --- Admin season management ---

/// A user-facing reason a season mutation was refused. Handlers downcast to
/// this and flash it; any other error is a 500.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub &'static str);

pub const MAX_SEASON_NAME_CHARS: usize = 100;

const OVERLAP_MESSAGE: &str = "That season overlaps another season of this leaderboard. Seasons can't overlap — to schedule the next one, first set the current season's end date.";
const END_BEFORE_START_MESSAGE: &str = "The end must be after the start.";

pub struct NewSeason {
    pub name: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct SeasonEdit {
    pub name: String,
    /// `None` = not submitted.
    pub starts_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `None` = not submitted; `Some(None)` = clear to open-ended.
    pub ends_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
}

fn validate_name(name: &str) -> Result<String, ValidationError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ValidationError("Season name can't be blank."));
    }
    if name.chars().count() > MAX_SEASON_NAME_CHARS {
        return Err(ValidationError(
            "Season name must be 100 characters or fewer.",
        ));
    }
    Ok(name.to_string())
}

/// True if any cause in the chain is a Postgres error with `sqlstate`.
fn has_sqlstate(err: &color_eyre::eyre::Report, sqlstate: &str) -> bool {
    err.chain().any(|cause| {
        let Some(sqlx::Error::Database(db_err)) = cause.downcast_ref::<sqlx::Error>() else {
            return false;
        };
        db_err.code().as_deref() == Some(sqlstate)
    })
}

/// True if any cause in the chain is an exclusion violation (SQLSTATE
/// 23P01) — for seasons, `leaderboard_seasons_no_overlap`.
pub(crate) fn is_exclusion_violation(err: &color_eyre::eyre::Report) -> bool {
    has_sqlstate(err, "23P01")
}

/// Map the DB's season constraint violations to form errors.
fn map_constraint_error(err: color_eyre::eyre::Report) -> color_eyre::eyre::Report {
    if is_exclusion_violation(&err) {
        ValidationError(OVERLAP_MESSAGE).into()
    } else if has_sqlstate(&err, "23514") {
        ValidationError(END_BEFORE_START_MESSAGE).into()
    } else {
        err
    }
}

/// Schedule a new season. `Ok(None)` = `starts_at` is in the past by the DB
/// clock (checked inside the INSERT, so there's no window between check and
/// write). Overlap is arbitrated by the exclusion constraint alone.
pub async fn create_season(
    pool: &sqlx::PgPool,
    leaderboard_id: Uuid,
    input: NewSeason,
) -> cja::Result<Option<LeaderboardSeason>> {
    let name = validate_name(&input.name)?;
    if input.ends_at.is_some_and(|end| end <= input.starts_at) {
        return Err(ValidationError(END_BEFORE_START_MESSAGE).into());
    }

    let mut tx = pool.begin().await.wrap_err("Failed to begin transaction")?;
    sqlx::query!("SET LOCAL lock_timeout = '5s'")
        .execute(&mut *tx)
        .await
        .wrap_err("Failed to set lock_timeout")?;
    let season = sqlx::query_as!(
        LeaderboardSeason,
        r#"INSERT INTO leaderboard_seasons (leaderboard_id, season_id, name, starts_at, ends_at)
         SELECT $1::uuid, gen_random_uuid(), $2::text, $3::timestamptz, $4::timestamptz
         WHERE $3::timestamptz >= clock_timestamp()
         RETURNING season_id, leaderboard_id, name, starts_at, ends_at"#,
        leaderboard_id,
        name,
        input.starts_at,
        input.ends_at
    )
    .fetch_optional(&mut *tx)
    .await
    .wrap_err("Failed to create leaderboard season")
    .map_err(map_constraint_error)?;
    tx.commit().await.wrap_err("Failed to commit season")?;
    Ok(season)
}

struct LockedSeason {
    starts_at: chrono::DateTime<chrono::Utc>,
    ends_at: Option<chrono::DateTime<chrono::Utc>>,
    is_future: bool,
    is_active: bool,
    now: chrono::DateTime<chrono::Utc>,
}

/// Lock one season row of `leaderboard_id` `FOR UPDATE`, with its phase
/// judged by the clock at lock time. Own columns and the clock only — no
/// other tables in the locking statement.
async fn lock_season(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    leaderboard_id: Uuid,
    season_id: Uuid,
) -> cja::Result<Option<LockedSeason>> {
    sqlx::query!("SET LOCAL lock_timeout = '5s'")
        .execute(&mut **tx)
        .await
        .wrap_err("Failed to set lock_timeout")?;
    sqlx::query_as!(
        LockedSeason,
        r#"SELECT starts_at, ends_at,
                (starts_at > clock_timestamp()) AS "is_future!",
                (starts_at <= clock_timestamp()
                    AND (ends_at IS NULL OR clock_timestamp() < ends_at)) AS "is_active!",
                clock_timestamp() AS "now!"
         FROM leaderboard_seasons
         WHERE season_id = $1 AND leaderboard_id = $2
         FOR UPDATE"#,
        season_id,
        leaderboard_id
    )
    .fetch_optional(&mut **tx)
    .await
    .wrap_err("Failed to lock leaderboard season")
}

/// The resolved `(starts_at, ends_at)` an edit may write, given the
/// season's phase at lock time. Pure, so each rule is unit-testable.
fn apply_edit(
    locked: &LockedSeason,
    edit: &SeasonEdit,
) -> Result<
    (
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    ),
    ValidationError,
> {
    if locked.is_future {
        let starts_at = match edit.starts_at {
            Some(start) if start < locked.now => {
                return Err(ValidationError("A season can't start in the past."));
            }
            Some(start) => start,
            None => locked.starts_at,
        };
        let ends_at = edit.ends_at.unwrap_or(locked.ends_at);
        if ends_at.is_some_and(|end| end <= starts_at) {
            return Err(ValidationError(END_BEFORE_START_MESSAGE));
        }
        Ok((starts_at, ends_at))
    } else if locked.is_active {
        if edit.starts_at.is_some() {
            return Err(ValidationError(
                "This season has started — its start can't change.",
            ));
        }
        let ends_at = match edit.ends_at {
            Some(Some(end)) if end < locked.now => {
                return Err(ValidationError(
                    "An active season's end can't be set in the past.",
                ));
            }
            Some(ends_at) => ends_at,
            None => locked.ends_at,
        };
        Ok((locked.starts_at, ends_at))
    } else {
        if edit.starts_at.is_some() || edit.ends_at.is_some() {
            return Err(ValidationError(
                "This season has ended — only its name can change.",
            ));
        }
        Ok((locked.starts_at, locked.ends_at))
    }
}

/// Edit a season under its row lock. `Ok(None)` = no such season in this
/// league. The name always applies; dates only where the phase allows
/// (see [`apply_edit`]).
pub async fn update_season(
    pool: &sqlx::PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
    edit: SeasonEdit,
) -> cja::Result<Option<LeaderboardSeason>> {
    let name = validate_name(&edit.name)?;
    let mut tx = pool.begin().await.wrap_err("Failed to begin transaction")?;
    let Some(locked) = lock_season(&mut tx, leaderboard_id, season_id).await? else {
        return Ok(None);
    };
    let (starts_at, ends_at) = apply_edit(&locked, &edit)?;
    let season = sqlx::query_as!(
        LeaderboardSeason,
        r#"UPDATE leaderboard_seasons
         SET name = $2, starts_at = $3, ends_at = $4
         WHERE season_id = $1
         RETURNING season_id, leaderboard_id, name, starts_at, ends_at"#,
        season_id,
        name,
        starts_at,
        ends_at
    )
    .fetch_one(&mut *tx)
    .await
    .wrap_err("Failed to update leaderboard season")
    .map_err(map_constraint_error)?;
    tx.commit().await.wrap_err("Failed to commit season edit")?;
    Ok(Some(season))
}

/// Delete a season that hasn't started. `Ok(false)` = no such season in
/// this league. A season with entries or games can't be deleted (the FKs
/// raise 23503, mapped to a form error).
pub async fn delete_season(
    pool: &sqlx::PgPool,
    leaderboard_id: Uuid,
    season_id: Uuid,
) -> cja::Result<bool> {
    let mut tx = pool.begin().await.wrap_err("Failed to begin transaction")?;
    let Some(locked) = lock_season(&mut tx, leaderboard_id, season_id).await? else {
        return Ok(false);
    };
    if !locked.is_future {
        return Err(ValidationError("Only a season that hasn't started can be deleted.").into());
    }
    sqlx::query!(
        "DELETE FROM leaderboard_seasons WHERE season_id = $1",
        season_id
    )
    .execute(&mut *tx)
    .await
    .wrap_err("Failed to delete leaderboard season")
    .map_err(|err| {
        if has_sqlstate(&err, "23503") {
            ValidationError("That season already has entries or games, so it can't be deleted.")
                .into()
        } else {
            err
        }
    })?;
    tx.commit()
        .await
        .wrap_err("Failed to commit season delete")?;
    Ok(true)
}

// --- Timezone / DST helpers for the admin form ---

/// Parse an IANA timezone name (e.g. `America/New_York`).
pub fn parse_timezone(s: &str) -> Result<chrono_tz::Tz, String> {
    s.trim().parse::<chrono_tz::Tz>().map_err(|_| {
        format!(
            "Unknown timezone \"{}\" — use an IANA name like America/New_York.",
            s.trim()
        )
    })
}

/// Parse a `datetime-local` value (`YYYY-MM-DDTHH:MM`) as a wall-clock time
/// in `tz`. `""` is `Ok(None)`. A time in the repeated DST hour is refused
/// unless one of its two instants is `current` (the stored value the form
/// was prefilled with), which passes through unchanged. A time skipped by
/// DST is refused.
pub fn parse_local_in_zone(
    local: &str,
    tz: &chrono_tz::Tz,
    current: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    use chrono::TimeZone as _;

    let local = local.trim();
    if local.is_empty() {
        return Ok(None);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M")
        .map_err(|_| format!("\"{local}\" isn't a valid date and time."))?;
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => Ok(Some(t.with_timezone(&chrono::Utc))),
        chrono::LocalResult::Ambiguous(a, b) => {
            let (a, b) = (a.with_timezone(&chrono::Utc), b.with_timezone(&chrono::Utc));
            match current {
                Some(current) if current == a || current == b => Ok(Some(current)),
                _ => Err(format!(
                    "{local} in {tz} falls in the repeated hour when daylight-saving ends — pick a time outside it."
                )),
            }
        }
        chrono::LocalResult::None => Err(format!(
            "{local} in {tz} doesn't exist — daylight-saving starts that night."
        )),
    }
}

/// Prefill for a `datetime-local` input: `t` as wall-clock time in `tz`.
pub fn render_in_zone(t: chrono::DateTime<chrono::Utc>, tz: &chrono_tz::Tz) -> String {
    t.with_timezone(tz).format("%Y-%m-%dT%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone as _, Utc};

    fn ny() -> chrono_tz::Tz {
        parse_timezone("America/New_York").unwrap()
    }

    fn season(name: &str) -> LeaderboardSeason {
        LeaderboardSeason {
            season_id: Uuid::new_v4(),
            leaderboard_id: Uuid::nil(),
            name: name.to_string(),
            starts_at: Utc::now(),
            ends_at: None,
        }
    }

    fn found(lookup: SeasonLookup) -> SeasonSelection {
        match lookup {
            SeasonLookup::Found(selection) => selection,
            SeasonLookup::Foreign => panic!("expected Found"),
        }
    }

    #[test]
    fn timezone_parsing() {
        assert_eq!(
            parse_timezone("America/New_York"),
            Ok(chrono_tz::America::New_York)
        );
        assert!(parse_timezone("Not/AZone").is_err());
    }

    #[test]
    fn local_time_parsing_handles_dst() {
        let tz = ny();
        assert_eq!(parse_local_in_zone("", &tz, None), Ok(None));
        assert_eq!(
            parse_local_in_zone("2027-01-01T00:00", &tz, None),
            Ok(Some(Utc.with_ymd_and_hms(2027, 1, 1, 5, 0, 0).unwrap()))
        );
        let skipped = parse_local_in_zone("2027-03-14T02:30", &tz, None).unwrap_err();
        assert!(skipped.contains("doesn't exist"), "{skipped}");

        let repeated = parse_local_in_zone("2027-11-07T01:30", &tz, None).unwrap_err();
        assert!(repeated.contains("repeated hour"), "{repeated}");
        for current in [
            Utc.with_ymd_and_hms(2027, 11, 7, 5, 30, 0).unwrap(),
            Utc.with_ymd_and_hms(2027, 11, 7, 6, 30, 0).unwrap(),
        ] {
            assert_eq!(
                parse_local_in_zone("2027-11-07T01:30", &tz, Some(current)),
                Ok(Some(current))
            );
        }
        // A current that matches neither instant doesn't unlock it.
        let other = Utc.with_ymd_and_hms(2027, 11, 7, 7, 30, 0).unwrap();
        assert!(parse_local_in_zone("2027-11-07T01:30", &tz, Some(other)).is_err());
        // A malformed hidden current is treated as absent.
        let malformed = chrono::DateTime::parse_from_rfc3339("garbage")
            .ok()
            .map(|t| t.with_timezone(&Utc));
        assert!(parse_local_in_zone("2027-11-07T01:30", &tz, malformed).is_err());

        assert!(parse_local_in_zone("not a date", &tz, None).is_err());
    }

    #[test]
    fn render_in_zone_round_trips() {
        let tz = ny();
        let t = Utc.with_ymd_and_hms(2027, 1, 1, 5, 0, 0).unwrap();
        assert_eq!(render_in_zone(t, &tz), "2027-01-01T00:00");
        assert_eq!(render_in_zone(t, &chrono_tz::UTC), "2027-01-01T05:00");
    }

    #[test]
    fn phase_boundaries_are_half_open() {
        let start = Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2027, 4, 1, 0, 0, 0).unwrap();
        let before = start - chrono::Duration::seconds(1);
        assert_eq!(
            SeasonPhase::at(start, Some(end), before),
            SeasonPhase::Upcoming
        );
        assert_eq!(
            SeasonPhase::at(start, Some(end), start),
            SeasonPhase::Active
        );
        let last = end - chrono::Duration::seconds(1);
        assert_eq!(SeasonPhase::at(start, Some(end), last), SeasonPhase::Active);
        assert_eq!(SeasonPhase::at(start, Some(end), end), SeasonPhase::Ended);
        assert_eq!(SeasonPhase::at(start, None, end), SeasonPhase::Active);
    }

    #[test]
    fn selection_rule() {
        let ended_a = season("Ended A");
        let mut ended_b = season("Ended B");
        ended_b.ends_at = Some(Utc::now());
        let active = season("Active");
        let mut upcoming_late = season("Upcoming late");
        upcoming_late.starts_at = Utc::now() + chrono::Duration::days(20);
        let mut upcoming_soon = season("Upcoming soon");
        upcoming_soon.starts_at = Utc::now() + chrono::Duration::days(10);
        let mut ended_a = ended_a;
        ended_a.ends_at = Some(Utc::now() - chrono::Duration::days(30));

        let all = vec![
            (ended_a.clone(), SeasonPhase::Ended),
            (ended_b.clone(), SeasonPhase::Ended),
            (active.clone(), SeasonPhase::Active),
            (upcoming_soon.clone(), SeasonPhase::Upcoming),
            (upcoming_late.clone(), SeasonPhase::Upcoming),
        ];

        // Default: the active season.
        let sel = found(build_selection(all.clone(), None));
        assert_eq!(sel.selected.unwrap().season.season_id, active.season_id);
        assert_eq!(sel.seasons.len(), 5);
        assert_eq!(sel.next_season_starts_at, Some(upcoming_soon.starts_at));

        // No active: the most recently ended.
        let gap: Vec<_> = all
            .iter()
            .filter(|(_, p)| *p != SeasonPhase::Active)
            .cloned()
            .collect();
        let sel = found(build_selection(gap, None));
        assert!(sel.league_active().is_none());
        assert_eq!(sel.selected.unwrap().season.season_id, ended_b.season_id);

        // Only future seasons: nothing selected.
        let future: Vec<_> = all
            .iter()
            .filter(|(_, p)| *p == SeasonPhase::Upcoming)
            .cloned()
            .collect();
        let sel = found(build_selection(future, None));
        assert!(sel.selected.is_none());

        // Requested season wins; a foreign one is Foreign.
        let sel = found(build_selection(all.clone(), Some(ended_a.season_id)));
        assert_eq!(sel.selected.unwrap().season.season_id, ended_a.season_id);
        assert!(matches!(
            build_selection(all, Some(Uuid::new_v4())),
            SeasonLookup::Foreign
        ));
    }

    fn locked(
        is_future: bool,
        is_active: bool,
        ends_at: Option<chrono::DateTime<Utc>>,
    ) -> LockedSeason {
        let now = Utc.with_ymd_and_hms(2027, 1, 15, 0, 0, 0).unwrap();
        let starts_at = if is_future {
            now + chrono::Duration::days(10)
        } else {
            now - chrono::Duration::days(10)
        };
        LockedSeason {
            starts_at,
            ends_at,
            is_future,
            is_active,
            now,
        }
    }

    #[test]
    fn edit_rules_by_phase() {
        let hour = chrono::Duration::hours(1);
        // Ended: name only.
        let ended = locked(
            false,
            false,
            Some(Utc.with_ymd_and_hms(2027, 1, 10, 0, 0, 0).unwrap()),
        );
        let name_only = SeasonEdit {
            name: "x".into(),
            starts_at: None,
            ends_at: None,
        };
        assert_eq!(
            apply_edit(&ended, &name_only).unwrap(),
            (ended.starts_at, ended.ends_at)
        );
        let with_start = SeasonEdit {
            name: "x".into(),
            starts_at: Some(ended.now + hour),
            ends_at: None,
        };
        assert!(apply_edit(&ended, &with_start).is_err());
        let with_end = SeasonEdit {
            name: "x".into(),
            starts_at: None,
            ends_at: Some(None),
        };
        assert!(apply_edit(&ended, &with_end).is_err());

        // Active: ends_at at/after now, or cleared; never starts_at.
        let active = locked(false, true, None);
        let at_now = SeasonEdit {
            name: "x".into(),
            starts_at: None,
            ends_at: Some(Some(active.now)),
        };
        assert_eq!(apply_edit(&active, &at_now).unwrap().1, Some(active.now));
        let past = SeasonEdit {
            name: "x".into(),
            starts_at: None,
            ends_at: Some(Some(active.now - chrono::Duration::seconds(1))),
        };
        assert!(apply_edit(&active, &past).is_err());
        let active_with_end = locked(false, true, Some(active.now + hour));
        let clear = SeasonEdit {
            name: "x".into(),
            starts_at: None,
            ends_at: Some(None),
        };
        assert_eq!(apply_edit(&active_with_end, &clear).unwrap().1, None);
        let start = SeasonEdit {
            name: "x".into(),
            starts_at: Some(active.now + hour),
            ends_at: None,
        };
        assert!(apply_edit(&active, &start).is_err());

        // Upcoming: starts_at at/after now; end after start.
        let upcoming = locked(true, false, None);
        let at_now = SeasonEdit {
            name: "x".into(),
            starts_at: Some(upcoming.now),
            ends_at: Some(Some(upcoming.now + hour)),
        };
        assert_eq!(
            apply_edit(&upcoming, &at_now).unwrap(),
            (upcoming.now, Some(upcoming.now + hour))
        );
        let past_start = SeasonEdit {
            name: "x".into(),
            starts_at: Some(upcoming.now - chrono::Duration::seconds(1)),
            ends_at: None,
        };
        assert!(apply_edit(&upcoming, &past_start).is_err());
        let inverted = SeasonEdit {
            name: "x".into(),
            starts_at: Some(upcoming.now + hour),
            ends_at: Some(Some(upcoming.now + hour)),
        };
        assert!(apply_edit(&upcoming, &inverted).is_err());
    }

    #[test]
    fn name_validation_limits() {
        assert!(validate_name("   ").is_err());
        assert_eq!(validate_name("  Season 1 ").unwrap(), "Season 1");
        assert!(validate_name(&"é".repeat(MAX_SEASON_NAME_CHARS)).is_ok());
        assert!(validate_name(&"é".repeat(MAX_SEASON_NAME_CHARS + 1)).is_err());
    }
}
