//! Code-defined achievements and permanent cosmetic awards.
use std::collections::{BTreeMap, HashSet};

use arena::design_kit::AssetKind;
use color_eyre::eyre::{Context as _, eyre};
use sqlx::{Connection as _, PgConnection, PgPool};
use uuid::Uuid;

use super::{Head, Tail};
use crate::email::messages::{self, AchievementUnlock, UnlockedItem};
use crate::models::game::GameType;
use crate::notifications::{self, EmailRecipient, NewEmail, NewNotification, NotificationKind};

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
    pub const GETTING_STARTED: [Self; 5] = [
        Self::FirstWin,
        Self::CrowdPleaser,
        Self::ConstrictorChamp,
        Self::SlowAndSteady,
        Self::LadderRegular,
    ];

    pub const ALL: [Self; 5] = [
        Self::FirstWin,
        Self::SlowAndSteady,
        Self::ConstrictorChamp,
        Self::CrowdPleaser,
        Self::LadderRegular,
    ];

    /// Stable machine identifier: `achievement_earns.achievement` value and
    /// the `achievement_earned` event's `slug` field. Never rename — it is
    /// the analytics contract.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::FirstWin => "first_win",
            Self::SlowAndSteady => "slow_and_steady",
            Self::ConstrictorChamp => "constrictor_champ",
            Self::CrowdPleaser => "crowd_pleaser",
            Self::LadderRegular => "ladder_regular",
        }
    }

    /// Resolve the achievement a share URL names, or `None` for an
    /// unknown slug.
    pub fn from_slug(slug: &str) -> Option<Self> {
        Achievement::ALL.into_iter().find(|a| a.slug() == slug)
    }

    /// Achievement family for analytics. All current achievements are
    /// Getting Started.
    pub const fn kind(self) -> &'static str {
        match self {
            Self::FirstWin
            | Self::SlowAndSteady
            | Self::ConstrictorChamp
            | Self::CrowdPleaser
            | Self::LadderRegular => "getting_started",
        }
    }

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

/// The achievements this user has earn rows for. Share links key off these,
/// not customization grants: a token or code holder can own the items
/// without ever earning them.
pub async fn earned_slugs(pool: &PgPool, user_id: Uuid) -> cja::Result<HashSet<String>> {
    let rows = sqlx::query!(
        "SELECT achievement FROM achievement_earns WHERE user_id = $1",
        user_id,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to read earned achievements")?;
    Ok(rows.into_iter().map(|row| row.achievement).collect())
}

/// The user's earned achievements for their public profile: newest first,
/// with a stable slug tie-break because grant-dated rows can share an
/// earned_at. Rows whose slug no longer resolves to a defined achievement
/// are skipped, so an unknown value can never break a public page. This is
/// the section's only query — names, art, and descriptions come from the
/// in-process Achievement definitions.
pub async fn earned_for_profile(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<Vec<(Achievement, chrono::DateTime<chrono::Utc>)>> {
    let rows = sqlx::query!(
        "SELECT achievement, earned_at FROM achievement_earns
         WHERE user_id = $1
         ORDER BY earned_at DESC, achievement",
        user_id,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to read earned achievements")?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Achievement::from_slug(&row.achievement).map(|achievement| (achievement, row.earned_at))
        })
        .collect())
}

/// The game and snake that produced a rule fire, recorded on live earns.
/// `battlesnake_id` is the winning/linked snake's id; the participants
/// query inner-joins `battlesnakes` on the coalesced id, so it is never
/// NULL at runtime.
#[derive(Debug, Clone, Copy)]
struct Provenance {
    game_id: Uuid,
    battlesnake_id: Uuid,
}

#[derive(Default, Clone, Copy)]
struct WinFlags {
    first: bool,
    constrictor: bool,
    crowd: bool,
    ladder: bool,
    ladder_win: bool,
    /// Filled by the first outright-winning row (the row that flips `first`).
    first_win_provenance: Option<Provenance>,
    /// Filled by the first winning Constrictor row.
    constrictor_provenance: Option<Provenance>,
    /// Filled by the first winning row with >= 4 participants.
    crowd_provenance: Option<Provenance>,
    /// Filled by the first winning row that is ladder-linked.
    ladder_win_provenance: Option<Provenance>,
    /// Filled by the first ladder-linked row, win or loss.
    ladder_provenance: Option<Provenance>,
}

impl WinFlags {
    fn include(
        &mut self,
        placement: Option<i32>,
        first_places: i64,
        participants: i64,
        game_type: &str,
        ladder: bool,
        provenance: Provenance,
    ) {
        self.ladder |= ladder;
        if ladder && self.ladder_provenance.is_none() {
            self.ladder_provenance = Some(provenance);
        }
        // A shared first place is a draw, not a win.
        if placement == Some(1) && first_places == 1 && participants >= 2 {
            if self.first_win_provenance.is_none() {
                self.first_win_provenance = Some(provenance);
            }
            if game_type == GameType::Constrictor.as_str() {
                self.constrictor = true;
                if self.constrictor_provenance.is_none() {
                    self.constrictor_provenance = Some(provenance);
                }
            }
            if participants >= 4 {
                self.crowd = true;
                if self.crowd_provenance.is_none() {
                    self.crowd_provenance = Some(provenance);
                }
            }
            if ladder {
                self.ladder_win = true;
                if self.ladder_win_provenance.is_none() {
                    self.ladder_win_provenance = Some(provenance);
                }
            }
            self.first = true;
        }
    }

    /// Which game/snake earned a given achievement, for live earn records.
    /// A slot is Some exactly when the matching flag is true. For the
    /// count-based rules the attribution is the gate row: the ladder-linked
    /// (or ladder-winning) row, earliest-first under the reconcile ORDER BY.
    fn provenance_for(&self, achievement: Achievement) -> Option<Provenance> {
        match achievement {
            Achievement::FirstWin => self.first_win_provenance,
            Achievement::ConstrictorChamp => self.constrictor_provenance,
            Achievement::CrowdPleaser => self.crowd_provenance,
            Achievement::SlowAndSteady => self.ladder_provenance,
            Achievement::LadderRegular => self.ladder_win_provenance,
        }
    }
}

#[derive(Default)]
struct AwardResult {
    inserted: u64,
    newly_earned: Vec<EarnedAchievement>,
    /// Live earns recorded in this call, for post-commit Eyes events.
    /// Stays empty when earn recording failed (the failure was logged;
    /// the grants stand).
    earned_events: Vec<EarnedEvent>,
}

/// A live earn recorded with provenance; drives the Eyes event.
#[derive(Debug, Clone, Copy)]
struct EarnedEvent {
    achievement: Achievement,
    first: bool,
    provenance: Provenance,
}

struct EarnedAchievement {
    achievement: Achievement,
    head_granted: bool,
    tail_granted: bool,
}

pub(crate) fn achievement_unlock(
    achievement: Achievement,
    head_granted: bool,
    tail_granted: bool,
) -> cja::Result<AchievementUnlock> {
    let def = achievement.def();
    let head = if head_granted {
        let slug = def.head.slug();
        Some(UnlockedItem {
            display_name: def.head.def().display_name.to_string(),
            slug: slug.to_string(),
            png_url: crate::routes::og::customization_png_url(AssetKind::Head, slug)
                .ok_or_else(|| eyre!("Missing head PNG for {}: {slug}", def.name))
                .wrap_err_with(|| format!("Failed to render {} head", def.name))?,
        })
    } else {
        None
    };
    let tail = if tail_granted {
        let item = def
            .tail
            .ok_or_else(|| eyre!("Missing tail for {}", def.name))?;
        let slug = item.slug();
        Some(UnlockedItem {
            display_name: item.def().display_name.to_string(),
            slug: slug.to_string(),
            png_url: crate::routes::og::customization_png_url(AssetKind::Tail, slug)
                .ok_or_else(|| eyre!("Missing tail PNG for {}: {slug}", def.name))
                .wrap_err_with(|| format!("Failed to render {} tail", def.name))?,
        })
    } else {
        None
    };
    Ok(AchievementUnlock {
        name: def.name.to_string(),
        head,
        tail,
    })
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
              -- A shared first place is a draw, not a win.
              AND (SELECT COUNT(*) >= $2 AND COUNT(*) FILTER (WHERE peer.placement = 1) = 1
                   FROM game_battlesnakes peer WHERE peer.game_id = owned.game_id)
            LIMIT 1
        ) AS "exists!"
        "#,
        user_id,
        min_participants,
        constrictor,
        GameType::Constrictor.as_str(),
    )
    .fetch_one(conn)
    .await
    .wrap_err("Failed to check historical achievement win")?;
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
            -- A shared first place is a draw, not a win.
            WHERE NOT $3::bool OR (SELECT COUNT(*) >= 2 AND COUNT(*) FILTER (WHERE peer.placement = 1) = 1
                FROM game_battlesnakes peer WHERE peer.game_id = fg.game_id)
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

/// One row of the pre-call grant snapshot: what the user already owns,
/// how each item was obtained, and when.
struct ExistingGrant {
    customization_type: String,
    slug: String,
    source: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a live earn record with game/snake/turn provenance. `earned_at`
/// is the game's finished_at (COALESCE covers fixtures with NULL
/// finished_at). Returns `(achievement_earn_id, first)` when this call
/// created the row.
async fn record_live_earn(
    conn: &mut PgConnection,
    user_id: Uuid,
    achievement: Achievement,
    provenance: Provenance,
) -> cja::Result<Option<(Uuid, bool)>> {
    let earn_id = sqlx::query!(
        r#"INSERT INTO achievement_earns
               (user_id, achievement, battlesnake_id, game_id, turn_number, live, earned_at)
           VALUES ($1, $2, $3, $4,
                   (SELECT MAX(turn_number) FROM turns WHERE game_id = $4),
                   TRUE,
                   COALESCE((SELECT finished_at FROM games WHERE game_id = $4), NOW()))
           ON CONFLICT (user_id, achievement) DO NOTHING
           RETURNING achievement_earn_id"#,
        user_id,
        achievement.slug(),
        provenance.battlesnake_id,
        provenance.game_id,
    )
    .fetch_optional(&mut *conn)
    .await
    .wrap_err("Failed to insert live achievement earn")?
    .map(|row| row.achievement_earn_id);
    let Some(earn_id) = earn_id else {
        return Ok(None);
    };
    let first = sqlx::query_scalar!(
        r#"SELECT NOT EXISTS (
               SELECT 1 FROM achievement_earns
               WHERE achievement = $1 AND achievement_earn_id <> $2
           ) AS "first!""#,
        achievement.slug(),
        earn_id,
    )
    .fetch_one(&mut *conn)
    .await
    .wrap_err("Failed to check first achievement earner")?;
    Ok(Some((earn_id, first)))
}

/// Insert a non-live earn record dated from a grant (or from now for a
/// fresh historical award). Never emits an event.
async fn record_grant_earn(
    conn: &mut PgConnection,
    user_id: Uuid,
    achievement: Achievement,
    earned_at: chrono::DateTime<chrono::Utc>,
) -> cja::Result<()> {
    sqlx::query!(
        r#"INSERT INTO achievement_earns (user_id, achievement, earned_at, live)
           VALUES ($1, $2, $3, FALSE)
           ON CONFLICT (user_id, achievement) DO NOTHING"#,
        user_id,
        achievement.slug(),
        earned_at,
    )
    .execute(&mut *conn)
    .await
    .wrap_err("Failed to insert grant-dated achievement earn")?;
    Ok(())
}

/// The distinct-game thresholds for the two count-based rules, shared by
/// the normal evaluation and the live re-verification for both-item owners.
async fn count_rule_satisfied(
    conn: &mut PgConnection,
    user_id: Uuid,
    achievement: Achievement,
) -> cja::Result<bool> {
    let (cap, wins) = match achievement {
        Achievement::SlowAndSteady => (3_000, false),
        Achievement::LadderRegular => (1_000, true),
        _ => return Ok(false),
    };
    Ok(
        distinct_candidate_count(&mut *conn, user_id, cap, wins).await? >= cap
            && distinct_finished_count(&mut *conn, user_id, cap, wins).await? >= cap,
    )
}

/// What a live call can verify for a user who already owns both items and
/// has no achievement-source grant. Win-family rules are proven by the
/// provenance slots themselves (each slot fills exactly when its flag
/// flips true). Count-based rules re-run the distinct-count queries under
/// the same flag gates the normal evaluation uses. Backfill callers
/// (live = None) verify nothing: a fully-held other-source set records
/// nothing from backfill.
async fn verified_live_provenance(
    conn: &mut PgConnection,
    user_id: Uuid,
    achievement: Achievement,
    live: Option<WinFlags>,
) -> cja::Result<Option<Provenance>> {
    let Some(flags) = live else {
        return Ok(None);
    };
    match achievement {
        Achievement::FirstWin | Achievement::ConstrictorChamp | Achievement::CrowdPleaser => {
            Ok(flags.provenance_for(achievement))
        }
        Achievement::SlowAndSteady => Ok(if !flags.ladder {
            None
        } else {
            count_rule_satisfied(&mut *conn, user_id, achievement)
                .await?
                .then_some(flags.ladder_provenance)
                .flatten()
        }),
        Achievement::LadderRegular => Ok(if !flags.ladder_win {
            None
        } else {
            count_rule_satisfied(&mut *conn, user_id, achievement)
                .await?
                .then_some(flags.ladder_win_provenance)
                .flatten()
        }),
    }
}

async fn award_owner(
    conn: &mut PgConnection,
    user_id: Uuid,
    live: Option<WinFlags>,
) -> cja::Result<AwardResult> {
    let existing = sqlx::query_as!(
        ExistingGrant,
        "SELECT customization_type, slug, source, created_at FROM customization_grants WHERE user_id = $1",
        user_id,
    )
    .fetch_all(&mut *conn)
    .await
    .wrap_err("Failed to read achievement ownership")?;
    let owned: HashSet<(String, String)> = existing
        .iter()
        .map(|grant| (grant.customization_type.clone(), grant.slug.clone()))
        .collect();
    let mut result = AwardResult::default();
    let mut fired: Vec<Achievement> = Vec::new();
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
                    count_rule_satisfied(&mut *conn, user_id, achievement).await?
                }
            }
            Achievement::LadderRegular => {
                if !live.is_none_or(|flags| flags.ladder_win) {
                    false
                } else {
                    count_rule_satisfied(&mut *conn, user_id, achievement).await?
                }
            }
        };
        if !earned {
            continue;
        }
        fired.push(achievement);
        let mut head_granted = false;
        let mut tail_granted = false;
        for (kind, slug, held) in [
            (Head::KIND, def.head.slug(), head_owned),
            (Tail::KIND, def.tail.map_or("", Tail::slug), tail_owned),
        ] {
            if held || slug.is_empty() {
                continue;
            }
            let rows = sqlx::query!(
                "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, $2, $3, 'achievement') ON CONFLICT (user_id, customization_type, slug) DO NOTHING",
                user_id, kind, slug,
            ).execute(&mut *conn).await.wrap_err("Failed to insert achievement grant")?.rows_affected();
            result.inserted += rows;
            if rows > 0 {
                if kind == Head::KIND {
                    head_granted = true;
                } else {
                    tail_granted = true;
                }
            }
        }
        if head_granted || tail_granted {
            result.newly_earned.push(EarnedAchievement {
                achievement,
                head_granted,
                tail_granted,
            });
        }
    }
    // Recording must never cost a grant: a failed statement would abort the
    // shared award transaction, so all earn work runs in its own savepoint.
    // sqlx::Connection::begin on a connection already inside a transaction
    // issues SAVEPOINT; commit releases it, rollback restores it.
    let mut savepoint = conn
        .begin()
        .await
        .wrap_err("Failed to open earn savepoint")?;
    match record_earns(&mut savepoint, user_id, &existing, live, &fired).await {
        Ok(events) => {
            savepoint
                .commit()
                .await
                .wrap_err("Failed to release earn savepoint")?;
            result.earned_events = events;
        }
        Err(error) => {
            savepoint
                .rollback()
                .await
                .wrap_err("Failed to roll back earn savepoint")?;
            tracing::error!(
                user_id = %user_id,
                error = %format!("{error:#}"),
                "Failed to record achievement earns"
            );
        }
    }
    Ok(result)
}

/// Record earns for one owner inside the caller's savepoint. Exactly two
/// facts create a row: a pre-call achievement-source grant (grant-dated,
/// non-live) or a rule fire in this call (live with provenance on a live
/// call, now-dated non-live on the backfill path). A both-items owner
/// with no achievement-source grant gets a row only when a live call can
/// verify the rule right now. Returns the live earns for post-commit
/// events.
async fn record_earns(
    conn: &mut PgConnection,
    user_id: Uuid,
    existing: &[ExistingGrant],
    live: Option<WinFlags>,
    fired: &[Achievement],
) -> cja::Result<Vec<EarnedEvent>> {
    let mut events = Vec::new();
    let already_earned: HashSet<String> = sqlx::query!(
        "SELECT achievement FROM achievement_earns WHERE user_id = $1",
        user_id,
    )
    .fetch_all(&mut *conn)
    .await
    .wrap_err("Failed to read earned achievements")?
    .into_iter()
    .map(|row| row.achievement)
    .collect();
    for achievement in Achievement::ALL {
        if already_earned.contains(achievement.slug()) {
            continue;
        }
        let def = achievement.def();
        // An achievement-source grant proves a past rule fire — for
        // partial holders (head-only) as well as full ones. Date the
        // record from the earliest such grant without re-running the rule.
        let ach_source_earned_at = existing
            .iter()
            .filter(|grant| {
                grant.source == "achievement"
                    && ((grant.customization_type == Head::KIND && grant.slug == def.head.slug())
                        || def.tail.is_some_and(|tail| {
                            grant.customization_type == Tail::KIND && grant.slug == tail.slug()
                        }))
            })
            .map(|grant| grant.created_at)
            .min();
        if let Some(earned_at) = ach_source_earned_at {
            record_grant_earn(&mut *conn, user_id, achievement, earned_at).await?;
            continue;
        }
        if fired.contains(&achievement) {
            match live
                .as_ref()
                .and_then(|flags| flags.provenance_for(achievement))
            {
                Some(provenance) => {
                    if let Some((_earn_id, first)) =
                        record_live_earn(&mut *conn, user_id, achievement, provenance).await?
                    {
                        events.push(EarnedEvent {
                            achievement,
                            first,
                            provenance,
                        });
                    }
                }
                None => {
                    // Fresh historical award (backfill path; a live fire
                    // always has provenance). The grants were created in
                    // this transaction, so NOW() is their created_at —
                    // including a partial other-source holder whose rule
                    // fired over history: the fresh achievement-source
                    // grant is the earn proof, and the job-top statement
                    // would write this same row on the next tick anyway.
                    record_grant_earn(&mut *conn, user_id, achievement, chrono::Utc::now()).await?;
                }
            }
            continue;
        }
        // Both items held from other sources: ownership alone must not
        // create a record, but a live call may verify the rule right now.
        // Backfill verifies nothing here.
        let head_owned = existing
            .iter()
            .any(|grant| grant.customization_type == Head::KIND && grant.slug == def.head.slug());
        let tail_owned = def.tail.is_none_or(|tail| {
            existing
                .iter()
                .any(|grant| grant.customization_type == Tail::KIND && grant.slug == tail.slug())
        });
        if head_owned
            && tail_owned
            && let Some(provenance) =
                verified_live_provenance(&mut *conn, user_id, achievement, live).await?
            && let Some((_earn_id, first)) =
                record_live_earn(&mut *conn, user_id, achievement, provenance).await?
        {
            events.push(EarnedEvent {
                achievement,
                first,
                provenance,
            });
        }
    }
    Ok(events)
}

/// Emit the product event for a live earn. Called only after the award
/// transaction commits, so a rolled-back award never emits. Non-live
/// (backfill/grant-dated) records never emit.
fn emit_earned_event(user_id: Uuid, event: &EarnedEvent) {
    tracing::info!(
        event_type = "achievement_earned",
        user_id = %user_id,
        slug = event.achievement.slug(),
        kind = event.achievement.kind(),
        live = true,
        first = event.first,
        game_id = %event.provenance.game_id,
        battlesnake_id = %event.provenance.battlesnake_id,
        "Achievement earned"
    );
}

pub async fn award_for_game(pool: &PgPool, game_id: Uuid, base_url: &str) -> cja::Result<u64> {
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
        r#"SELECT bs.user_id, gb.placement, gb.leaderboard_entry_id,
                  COALESCE(gb.battlesnake_id, le.battlesnake_id) AS "snake_id!"
           FROM game_battlesnakes gb
           LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
           JOIN battlesnakes bs ON bs.battlesnake_id = COALESCE(gb.battlesnake_id, le.battlesnake_id)
           WHERE gb.game_id = $1"#,
        game_id,
    )
    .fetch_all(&mut *tx)
    .await
    .wrap_err("Failed to read achievement participants")?;
    let participants = rows.len() as i64;
    let first_places = rows.iter().filter(|row| row.placement == Some(1)).count() as i64;
    let mut owners = BTreeMap::<Uuid, WinFlags>::new();
    for row in rows {
        owners.entry(row.user_id).or_default().include(
            row.placement,
            first_places,
            participants,
            &game.game_type,
            row.leaderboard_entry_id.is_some(),
            Provenance {
                game_id,
                battlesnake_id: row.snake_id,
            },
        );
    }
    let mut inserted = 0;
    let mut receipts = Vec::new();
    let mut earned_events: Vec<(Uuid, EarnedEvent)> = Vec::new();
    for (owner, flags) in owners {
        let awarded = award_owner(&mut tx, owner, Some(flags)).await?;
        inserted += awarded.inserted;
        earned_events.extend(
            awarded
                .earned_events
                .into_iter()
                .map(|event| (owner, event)),
        );
        if awarded.newly_earned.is_empty() {
            continue;
        }
        let unlocks = awarded
            .newly_earned
            .iter()
            .map(|item| achievement_unlock(item.achievement, item.head_granted, item.tail_granted))
            .collect::<cja::Result<Vec<_>>>()?;
        let body = unlocks
            .iter()
            .map(|unlock| {
                let items = [("head", &unlock.head), ("tail", &unlock.tail)]
                    .into_iter()
                    .filter_map(|(kind, item)| {
                        item.as_ref().map(|item| format!("{kind} {}", item.slug))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}: {items}", unlock.name)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let achievements_url = format!(
            "{}/customizations/achievements",
            base_url.trim_end_matches('/')
        );
        let content = messages::achievement_earned(&unlocks, &achievements_url);
        receipts.push(
            notifications::record_in_tx(
                &mut tx,
                NewNotification {
                    user_id: owner,
                    kind: NotificationKind::AchievementEarned,
                    title: content.subject.clone(),
                    body,
                    link: Some("/customizations/achievements".to_string()),
                    email: Some(NewEmail {
                        subject: Some(content.subject),
                        text: Some(content.text),
                        html_body: Some(content.html_body),
                        recipient: EmailRecipient::User,
                        claim_imported_account_id: None,
                        notification_email_request_id: None,
                    }),
                },
            )
            .await?,
        );
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit achievement award")?;
    for receipt in receipts {
        receipt.log_enqueued();
    }
    for (owner, event) in earned_events {
        emit_earned_event(owner, &event);
    }
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
        inserted += award_owner(&mut tx, user.user_id, None).await?.inserted;
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

/// Grant-dated earn records for every user holding an achievement-source
/// grant — full or partial holders alike, whenever and however that grant
/// was created (including ones the backfill page path granted earlier in
/// the same job to a partial other-source holder). Runs at the top of
/// BackfillAchievementsJob every tick: idempotent (NOT EXISTS + unique
/// conflict), silent (never notifies, never emits), and near-free once
/// every holder has a row.
pub async fn backfill_earns_from_grants(pool: &PgPool) -> cja::Result<u64> {
    let mut kinds = Vec::with_capacity(9);
    let mut slugs = Vec::with_capacity(9);
    let mut achievements = Vec::with_capacity(9);
    for achievement in Achievement::ALL {
        let def = achievement.def();
        kinds.push(Head::KIND.to_string());
        slugs.push(def.head.slug().to_string());
        achievements.push(achievement.slug().to_string());
        if let Some(tail) = def.tail {
            kinds.push(Tail::KIND.to_string());
            slugs.push(tail.slug().to_string());
            achievements.push(achievement.slug().to_string());
        }
    }
    let inserted = sqlx::query!(
        r#"INSERT INTO achievement_earns (user_id, achievement, earned_at, live)
           SELECT g.user_id, m.achievement, MIN(g.created_at), FALSE
           FROM customization_grants g
           JOIN UNNEST($1::text[], $2::text[], $3::text[])
                AS m(customization_type, slug, achievement)
             ON m.customization_type = g.customization_type AND m.slug = g.slug
           WHERE g.source = 'achievement'
             AND NOT EXISTS (
                 SELECT 1 FROM achievement_earns ae
                 WHERE ae.user_id = g.user_id AND ae.achievement = m.achievement
             )
           GROUP BY g.user_id, m.achievement
           ON CONFLICT (user_id, achievement) DO NOTHING"#,
        &kinds,
        &slugs,
        &achievements,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to backfill achievement earns from grants")?
    .rows_affected();
    Ok(inserted)
}

pub async fn reconcile_recent_achievements(pool: &PgPool) -> cja::Result<u64> {
    let mut tx = pool
        .begin()
        .await
        .wrap_err("Failed to begin achievement reconciliation")?;
    let rows = sqlx::query!(
        r#"SELECT g.game_id, g.game_type, bs.user_id, gb.placement, gb.leaderboard_entry_id,
            COALESCE(gb.battlesnake_id, le.battlesnake_id) AS "snake_id!",
            (SELECT COUNT(*) FROM game_battlesnakes peer WHERE peer.game_id = g.game_id) AS "participants!",
            (SELECT COUNT(*) FROM game_battlesnakes peer WHERE peer.game_id = g.game_id AND peer.placement = 1) AS "first_places!"
        FROM games g
        JOIN game_battlesnakes gb ON gb.game_id = g.game_id
        LEFT JOIN leaderboard_entries le ON le.leaderboard_entry_id = gb.leaderboard_entry_id
        JOIN battlesnakes bs ON bs.battlesnake_id = COALESCE(gb.battlesnake_id, le.battlesnake_id)
        WHERE g.status = 'finished' AND g.finished_at IS NOT NULL
          AND g.finished_at > NOW() - INTERVAL '2 hours'
        ORDER BY g.finished_at, g.game_id"#,
    ).fetch_all(&mut *tx).await.wrap_err("Failed to read recent achievement games")?;
    let candidate_games: HashSet<_> = rows.iter().map(|row| row.game_id).collect();
    let mut owners = BTreeMap::<Uuid, WinFlags>::new();
    // Attribution under the window's ORDER BY: each earn records the
    // earliest row in the window satisfying its rule gate (win slots: the
    // earliest outright win of that rule's shape; ladder/ladder_win
    // slots: the earliest ladder-linked or ladder-winning row).
    // Count-based attribution here is approximate by construction, because
    // a threshold-crossing game can't be identified cheaply in an
    // aggregate window. The exact game is recorded when the earn flows
    // through `award_for_game` (the normal path); reconcile is an hourly
    // repair path.
    for row in rows {
        owners.entry(row.user_id).or_default().include(
            row.placement,
            row.first_places,
            row.participants,
            &row.game_type,
            row.leaderboard_entry_id.is_some(),
            Provenance {
                game_id: row.game_id,
                battlesnake_id: row.snake_id,
            },
        );
    }
    let owner_count = owners.len();
    let mut inserted = 0;
    let mut earned_events: Vec<(Uuid, EarnedEvent)> = Vec::new();
    for (owner, flags) in owners {
        let awarded = award_owner(&mut tx, owner, Some(flags)).await?;
        inserted += awarded.inserted;
        earned_events.extend(
            awarded
                .earned_events
                .into_iter()
                .map(|event| (owner, event)),
        );
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit achievement reconciliation")?;
    for (owner, event) in earned_events {
        emit_earned_event(owner, &event);
    }
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
            "INSERT INTO users (external_github_id, github_login) VALUES ($1, $2) RETURNING user_id",
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
    fn every_getting_started_item_has_exactly_one_rule() {
        for head in Head::ALL
            .iter()
            .filter(|head| head.def().group == super::super::Group::GettingStarted)
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
            .filter(|tail| tail.def().group == super::super::Group::GettingStarted)
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
    async fn live_awards_group_new_unlocks_into_one_notification(pool: PgPool) {
        let owner = user(&pool, 1669001).await;
        let rival = user(&pool, 1669002).await;
        let winner = snake(&pool, owner, "winner").await;
        let opponents = [
            snake(&pool, rival, "one").await,
            snake(&pool, rival, "two").await,
            snake(&pool, rival, "three").await,
        ];
        let first = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, first, winner, 1, None).await;
        participant(&pool, first, opponents[0], 2, None).await;
        assert_eq!(
            award_for_game(&pool, first, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            award_for_game(&pool, first, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let row = sqlx::query!(
            "SELECT n.title, n.body, n.link, e.recipient_user, e.recipient_override, e.battlesnake_id, e.text, e.html_body FROM notifications n JOIN notification_emails e USING (notification_id) WHERE n.user_id = $1 AND n.kind = 'achievement_earned'",
            owner,
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(row.title, "Achievement unlocked: First Win");
        assert_eq!(row.body, "First Win: head frog");
        assert_eq!(row.link.as_deref(), Some("/customizations/achievements"));
        assert!(row.recipient_user);
        assert!(row.recipient_override.is_none() && row.battlesnake_id.is_none());
        assert!(row.text.unwrap().contains("set head to \"frog\""));
        assert!(row.html_body.unwrap().contains("frog.png"));

        let next_owner = user(&pool, 1669003).await;
        let next_winner = snake(&pool, next_owner, "next winner").await;
        let four = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, four, next_winner, 1, None).await;
        for (index, opponent) in opponents.into_iter().enumerate() {
            participant(&pool, four, opponent, index as i32 + 2, None).await;
        }
        assert_eq!(
            award_for_game(&pool, four, "https://arena.example")
                .await
                .unwrap(),
            3
        );
        let row = sqlx::query!(
            "SELECT title, body FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'",
            next_owner,
        ).fetch_one(&pool).await.unwrap();
        assert_eq!(row.title, "2 achievements unlocked");
        assert_eq!(
            row.body,
            "First Win: head frog\nCrowd Pleaser: head monkey, tail monkey"
        );
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM jobs WHERE name = 'SendNotificationEmailJob'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2);
        let delayed = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM jobs WHERE name = 'SendNotificationEmailJob' AND run_at > now()"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(delayed, 0);
        assert_eq!(
            award_for_game(&pool, four, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let count_after = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM jobs WHERE name = 'SendNotificationEmailJob'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count_after, count);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn preowned_head_only_notifies_for_new_tail(pool: PgPool) {
        let owner = user(&pool, 1669004).await;
        let rival = user(&pool, 1669005).await;
        let winner = snake(&pool, owner, "winner").await;
        let game_id = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, game_id, winner, 1, None).await;
        for index in 0..3 {
            let opponent = snake(&pool, rival, &format!("rival {index}")).await;
            participant(&pool, game_id, opponent, index + 2, None).await;
        }
        sqlx::query!(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'monkey', 'achievement')",
            owner,
        ).execute(&pool).await.unwrap();
        assert_eq!(
            award_for_game(&pool, game_id, "https://arena.example")
                .await
                .unwrap(),
            2
        );
        let row = sqlx::query!(
            "SELECT body FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'",
            owner,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.body, "First Win: head frog\nCrowd Pleaser: tail monkey");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn achievement_visibility_follows_in_app_preference(pool: PgPool) {
        use crate::models::notification;
        use crate::notifications::NotificationChannel;
        let owner = user(&pool, 1669006).await;
        let rival = user(&pool, 1669007).await;
        notification::set_preference(
            &mut pool.acquire().await.unwrap(),
            owner,
            NotificationKind::AchievementEarned,
            NotificationChannel::InApp,
            false,
        )
        .await
        .unwrap();
        let winner = snake(&pool, owner, "winner").await;
        let opponent = snake(&pool, rival, "opponent").await;
        let game_id = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, game_id, winner, 1, None).await;
        participant(&pool, game_id, opponent, 2, None).await;
        assert_eq!(
            award_for_game(&pool, game_id, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        let visible = sqlx::query_scalar!(
            "SELECT in_app_visible FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'", owner,
        ).fetch_one(&pool).await.unwrap();
        assert!(!visible);
        assert!(
            notification::list_visible(&pool, owner, None)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(
            notification::unread_badge_count(&pool, owner)
                .await
                .unwrap(),
            0
        );
        notification::set_preference(
            &mut pool.acquire().await.unwrap(),
            owner,
            NotificationKind::AchievementEarned,
            NotificationChannel::Email,
            false,
        )
        .await
        .unwrap();
        assert!(
            !notification::effective_enabled(
                &mut pool.acquire().await.unwrap(),
                owner,
                NotificationKind::AchievementEarned,
                NotificationChannel::Email,
            )
            .await
            .unwrap()
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_evaluations_do_not_double_notify(pool: PgPool) {
        let owner = user(&pool, 1669009).await;
        let rival = user(&pool, 1669010).await;
        let winner = snake(&pool, owner, "winner").await;
        let opponent = snake(&pool, rival, "opponent").await;
        let game_id = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, game_id, winner, 1, None).await;
        participant(&pool, game_id, opponent, 2, None).await;
        let (one, two) = tokio::join!(
            award_for_game(&pool, game_id, "https://arena.example"),
            award_for_game(&pool, game_id, "https://arena.example"),
        );
        assert_eq!(one.unwrap() + two.unwrap(), 1);
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notifications WHERE kind = 'achievement_earned'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
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
        assert_eq!(
            award_for_game(&pool, solo, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let waiting = game(&pool, "Standard", "waiting", None).await;
        participant(&pool, waiting, winner, 1, None).await;
        participant(&pool, waiting, rival_snake, 2, None).await;
        assert_eq!(
            award_for_game(&pool, waiting, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let failed = game(&pool, "Standard", "failed", None).await;
        participant(&pool, failed, winner, 1, None).await;
        participant(&pool, failed, rival_snake, 2, None).await;
        assert_eq!(
            award_for_game(&pool, failed, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let two = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, two, winner, 1, None).await;
        participant(&pool, two, rival_snake, 2, None).await;
        assert_eq!(
            award_for_game(&pool, two, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            award_for_game(&pool, two, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let lower = game(&pool, "constrictor", "finished", Some(0)).await;
        participant(&pool, lower, winner, 1, None).await;
        participant(&pool, lower, second, 2, None).await;
        assert_eq!(
            award_for_game(&pool, lower, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let three = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, three, winner, 1, None).await;
        participant(&pool, three, second, 2, None).await;
        participant(&pool, three, rival_snake, 3, None).await;
        assert_eq!(
            award_for_game(&pool, three, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let four = game(&pool, GameType::Constrictor.as_str(), "finished", Some(0)).await;
        participant(&pool, four, winner, 1, None).await;
        participant(&pool, four, second, 2, None).await;
        participant(&pool, four, rival_snake, 3, None).await;
        participant(&pool, four, fourth, 4, None).await;
        assert_eq!(
            award_for_game(&pool, four, "https://arena.example")
                .await
                .unwrap(),
            4
        );
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
        assert_eq!(
            award_for_game(&pool, almost_judge, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        assert!(!has(&grants(&pool, owner).await, Tail::KIND, "judge"));
        let thousandth_win = game(&pool, "Standard", "finished", None).await;
        participant(&pool, thousandth_win, first, 1, Some(first_entry)).await;
        participant(&pool, thousandth_win, rival, 2, None).await;
        assert_eq!(
            award_for_game(&pool, thousandth_win, "https://arena.example")
                .await
                .unwrap(),
            2
        );
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
        assert_eq!(
            award_for_game(&pool, last, "https://arena.example")
                .await
                .unwrap(),
            1
        );
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
        assert_eq!(
            award_for_game(&pool, last, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        assert_eq!(super::super::token_balance(&pool, owner).await.unwrap(), 0);
        // The fallback owner link is still usable when the direct snake ID is null.
        let fallback = game(&pool, "Standard", "finished", Some(0)).await;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement) VALUES ($1, NULL, $2, 1)")
            .bind(fallback).bind(first_entry).execute(&pool).await.unwrap();
        assert_eq!(
            award_for_game(&pool, fallback, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let fallback_owner = user(&pool, 1644012).await;
        let fallback_snake = snake(&pool, fallback_owner, "fallback").await;
        let fallback_entry = entry(&pool, fallback_snake).await;
        let fallback_game = game(&pool, "Standard", "finished", Some(0)).await;
        sqlx::query("INSERT INTO game_battlesnakes (game_id, battlesnake_id, leaderboard_entry_id, placement) VALUES ($1, NULL, $2, 1)")
            .bind(fallback_game).bind(fallback_entry).execute(&pool).await.unwrap();
        participant(&pool, fallback_game, first, 2, None).await;
        assert_eq!(
            award_for_game(&pool, fallback_game, "https://arena.example")
                .await
                .unwrap(),
            1
        );
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
        assert_eq!(
            award_for_game(&pool, non_ladder, "https://arena.example")
                .await
                .unwrap(),
            1
        );
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
        assert_eq!(
            award_owner(&mut conn, owner, None).await.unwrap().inserted,
            0
        );
        drop(conn);
        let ladder = game(&pool, "Standard", "finished", None).await;
        participant(&pool, ladder, a, 2, Some(a_entry)).await;
        participant(&pool, ladder, b, 1, None).await;
        assert_eq!(
            award_for_game(&pool, ladder, "https://arena.example")
                .await
                .unwrap(),
            2
        );
        assert!(has(&grants(&pool, owner).await, Head::KIND, "turtle"));
        assert!(has(&grants(&pool, owner).await, Tail::KIND, "turtle"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn draws_never_count_as_wins(pool: PgPool) {
        let owner = user(&pool, 2035011).await;
        let opponent = user(&pool, 2035012).await;
        let a = snake(&pool, owner, "drawer").await;
        let b = snake(&pool, opponent, "opponent").await;

        let draw = game(&pool, "Constrictor", "finished", Some(0)).await;
        participant(&pool, draw, a, 1, None).await;
        participant(&pool, draw, b, 1, None).await;

        // Live award and the reconciler both read the recent draw.
        assert_eq!(
            award_for_game(&pool, draw, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        reconcile_recent_achievements(&pool).await.unwrap();
        assert!(grants(&pool, owner).await.is_empty());

        // Backfill reads history, including the ladder-win count.
        let mut conn = pool.acquire().await.unwrap();
        assert!(!historical_win(&mut conn, owner, 2, false).await.unwrap());
        assert!(!historical_win(&mut conn, owner, 2, true).await.unwrap());
        let a_entry = entry(&pool, a).await;
        let ladder_draw = game(&pool, "Standard", "finished", None).await;
        participant(&pool, ladder_draw, a, 1, Some(a_entry)).await;
        participant(&pool, ladder_draw, b, 1, None).await;
        assert_eq!(
            distinct_finished_count(&mut conn, owner, 10, true)
                .await
                .unwrap(),
            0
        );

        // An outright win still counts everywhere.
        let win = game(&pool, "Constrictor", "finished", None).await;
        participant(&pool, win, a, 1, Some(a_entry)).await;
        participant(&pool, win, b, 2, None).await;
        assert!(historical_win(&mut conn, owner, 2, true).await.unwrap());
        assert_eq!(
            distinct_finished_count(&mut conn, owner, 10, true)
                .await
                .unwrap(),
            1
        );
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
        assert_eq!(
            award_for_game(&pool, win_999, "https://arena.example")
                .await
                .unwrap(),
            1
        );
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
        assert_eq!(
            award_for_game(&pool, mixed, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "judge"));
        let win = game(&pool, "Standard", "finished", None).await;
        participant(&pool, win, a, 1, Some(a_entry)).await;
        participant(&pool, win, b, 2, None).await;
        assert_eq!(
            award_for_game(&pool, win, "https://arena.example")
                .await
                .unwrap(),
            2
        );
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
        assert_eq!(
            award_for_game(&pool, nonqualifying, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        let before = grants(&pool, owner).await;
        assert!(!has(&before, Head::KIND, "turtle"));
        assert!(!has(&before, Head::KIND, "judge"));
        // The live evaluation above may notify the opponent. Reconciliation must
        // add no notifications, email outbox rows, or send jobs for either owner.
        let side_effects_before: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(reconcile_recent_achievements(&pool).await.unwrap(), 5);
        let after = grants(&pool, owner).await;
        assert!(has(&after, Head::KIND, "turtle"));
        assert!(has(&after, Tail::KIND, "turtle"));
        assert!(has(&after, Head::KIND, "judge"));
        assert!(has(&after, Tail::KIND, "judge"));
        let side_effects_after: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(side_effects_after, side_effects_before);
        let owner_notifications: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(owner_notifications, 0);
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
        let full_win = game(&pool, "Standard", "finished", Some(3)).await;
        participant(&pool, full_win, full_snake, 1, Some(full_entry)).await;
        participant(&pool, full_win, partial_snake, 2, Some(partial_entry)).await;
        let partial_win = game(&pool, "Standard", "finished", Some(3)).await;
        participant(&pool, partial_win, partial_snake, 1, Some(partial_entry)).await;
        participant(&pool, partial_win, full_snake, 2, Some(full_entry)).await;
        let mut inserted = 0;
        for id in [full_win, partial_win] {
            inserted += award_for_game(&pool, id, "https://arena.example")
                .await
                .unwrap();
        }
        assert_eq!(inserted, 2);
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
            assert_eq!(
                award_owner(&mut conn, owner, Some(flags))
                    .await
                    .unwrap()
                    .inserted,
                0
            );
            let cold = start.elapsed();
            let mut warmed = Vec::new();
            for _ in 0..20 {
                let start = Instant::now();
                assert_eq!(
                    award_owner(&mut conn, owner, Some(flags))
                        .await
                        .unwrap()
                        .inserted,
                    0
                );
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
        let silent_count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!\" FROM notifications WHERE kind = 'achievement_earned'"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(silent_count, 0);
        let email_count =
            sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notification_emails")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(email_count, 0);
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
        assert_eq!(
            award_for_game(&pool, solo, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        assert!(backfill_achievement_page(&pool).await.unwrap().complete);
        assert!(grants(&pool, owner).await.is_empty());

        let three = game(&pool, "Standard", "finished", None).await;
        participant(&pool, three, winner, 1, None).await;
        participant(&pool, three, own_peer, 2, None).await;
        participant(&pool, three, rival, 3, None).await;
        assert_eq!(
            award_for_game(&pool, three, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "monkey"));

        let lower = game(&pool, "constrictor", "finished", None).await;
        participant(&pool, lower, winner, 1, None).await;
        participant(&pool, lower, rival, 2, None).await;
        assert_eq!(
            award_for_game(&pool, lower, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        assert!(!has(&grants(&pool, owner).await, Head::KIND, "subway"));

        let waiting = game(&pool, GameType::Constrictor.as_str(), "waiting", None).await;
        participant(&pool, waiting, winner, 1, None).await;
        participant(&pool, waiting, own_peer, 2, None).await;
        participant(&pool, waiting, rival, 3, None).await;
        participant(&pool, waiting, fourth, 4, None).await;
        assert_eq!(
            award_for_game(&pool, waiting, "https://arena.example")
                .await
                .unwrap(),
            0
        );

        let opponent_ladder = game(&pool, "Standard", "finished", None).await;
        participant(&pool, opponent_ladder, winner, 1, None).await;
        participant(&pool, opponent_ladder, rival, 2, Some(rival_entry)).await;
        assert_eq!(
            award_for_game(&pool, opponent_ladder, "https://arena.example")
                .await
                .unwrap(),
            0
        );
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
        assert_eq!(
            award_for_game(&pool, four, "https://arena.example")
                .await
                .unwrap(),
            4
        );

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
        assert_eq!(
            award_for_game(&pool, almost, "https://arena.example")
                .await
                .unwrap(),
            0
        );
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
        assert_eq!(
            award_for_game(&pool, three_thousandth, "https://arena.example")
                .await
                .unwrap(),
            2
        );
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
        let (award, ()) = tokio::join!(
            award_for_game(&pool, id, "https://arena.example"),
            release_holder
        );
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

    #[sqlx::test(migrations = "../migrations")]
    async fn live_earn_records_provenance_and_emits_event(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let owner = user(&pool, 1669104).await;
        let rival = user(&pool, 1669105).await;
        let winner = snake(&pool, owner, "provenance winner").await;
        let opponent = snake(&pool, rival, "provenance opponent").await;
        let game_id = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, game_id, winner, 1, None).await;
        participant(&pool, game_id, opponent, 2, None).await;
        sqlx::query("INSERT INTO turns (game_id, turn_number) VALUES ($1, 7)")
            .bind(game_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            award_for_game(&pool, game_id, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        let finished_at: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT finished_at FROM games WHERE game_id = $1")
                .bind(game_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let row = sqlx::query!(
            "SELECT achievement, battlesnake_id, game_id, turn_number, live, earned_at FROM achievement_earns WHERE user_id = $1",
            owner,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.achievement, "first_win");
        assert_eq!(row.battlesnake_id, Some(winner));
        assert_eq!(row.game_id, Some(game_id));
        assert_eq!(row.turn_number, Some(7));
        assert!(row.live);
        assert_eq!(row.earned_at, finished_at);
        let events = crate::test_capture::Capture::shared().events("achievement_earned", owner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["slug"], "first_win");
        assert_eq!(events[0]["kind"], "getting_started");
        assert_eq!(events[0]["live"], "true");
        assert_eq!(events[0]["first"], "true");
        assert_eq!(events[0]["game_id"], game_id.to_string());
        assert_eq!(events[0]["battlesnake_id"], winner.to_string());
        assert_eq!(
            award_for_game(&pool, game_id, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM achievement_earns WHERE user_id = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1);
        let events = crate::test_capture::Capture::shared().events("achievement_earned", owner);
        assert_eq!(events.len(), 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn second_live_earner_is_not_first(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let first_owner = user(&pool, 1669106).await;
        let second_owner = user(&pool, 1669107).await;
        let rival = user(&pool, 1669108).await;
        let first_snake = snake(&pool, first_owner, "first earner").await;
        let second_snake = snake(&pool, second_owner, "second earner").await;
        let rival_snake = snake(&pool, rival, "not first rival").await;
        let one = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, one, first_snake, 1, None).await;
        participant(&pool, one, rival_snake, 2, None).await;
        assert_eq!(
            award_for_game(&pool, one, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        let two = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, two, second_snake, 1, None).await;
        participant(&pool, two, rival_snake, 2, None).await;
        assert_eq!(
            award_for_game(&pool, two, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        let first_events =
            crate::test_capture::Capture::shared().events("achievement_earned", first_owner);
        assert_eq!(first_events.len(), 1);
        assert_eq!(first_events[0]["first"], "true");
        let second_events =
            crate::test_capture::Capture::shared().events("achievement_earned", second_owner);
        assert_eq!(second_events.len(), 1);
        assert_eq!(second_events[0]["first"], "false");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfilled_holder_means_next_live_earn_is_not_first(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let holder = user(&pool, 1669109).await;
        let earner = user(&pool, 1669110).await;
        let rival = user(&pool, 1669111).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source, created_at) VALUES ($1, 'head', 'frog', 'achievement', NOW() - INTERVAL '10 days')",
        )
        .bind(holder)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(backfill_earns_from_grants(&pool).await.unwrap(), 1);
        let holder_row = sqlx::query!(
            "SELECT live, game_id, earned_at FROM achievement_earns WHERE user_id = $1",
            holder,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!holder_row.live);
        assert!(holder_row.game_id.is_none());
        let winner = snake(&pool, earner, "late earner").await;
        let opponent = snake(&pool, rival, "late rival").await;
        let game_id = game(&pool, GameType::Standard.as_str(), "finished", Some(0)).await;
        participant(&pool, game_id, winner, 1, None).await;
        participant(&pool, game_id, opponent, 2, None).await;
        assert_eq!(
            award_for_game(&pool, game_id, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        let events = crate::test_capture::Capture::shared().events("achievement_earned", earner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["first"], "false");
        let holder_after = sqlx::query!(
            "SELECT live, game_id, earned_at FROM achievement_earns WHERE user_id = $1",
            holder,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(holder_after.earned_at, holder_row.earned_at);
        assert!(!holder_after.live);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_attributes_each_achievement_to_its_own_game(pool: PgPool) {
        let owner = user(&pool, 1669112).await;
        let rival = user(&pool, 1669113).await;
        let winner = snake(&pool, owner, "attributed winner").await;
        let second = snake(&pool, rival, "attributed second").await;
        let third = snake(&pool, rival, "attributed third").await;
        let fourth = snake(&pool, rival, "attributed fourth").await;
        let two = game(&pool, "Standard", "finished", Some(1)).await;
        participant(&pool, two, winner, 1, None).await;
        participant(&pool, two, second, 2, None).await;
        let four = game(&pool, GameType::Constrictor.as_str(), "finished", Some(0)).await;
        participant(&pool, four, winner, 1, None).await;
        participant(&pool, four, second, 2, None).await;
        participant(&pool, four, third, 3, None).await;
        participant(&pool, four, fourth, 4, None).await;
        assert_eq!(reconcile_recent_achievements(&pool).await.unwrap(), 5);
        let rows: Vec<(String, bool, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT achievement, live, game_id, battlesnake_id FROM achievement_earns WHERE user_id = $1",
        )
        .bind(owner)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        for (achievement, live, game_id, battlesnake_id) in rows {
            assert!(live, "{achievement} should be live");
            assert_eq!(battlesnake_id, Some(winner));
            match achievement.as_str() {
                "first_win" => assert_eq!(game_id, Some(two)),
                "constrictor_champ" | "crowd_pleaser" => assert_eq!(game_id, Some(four)),
                other => panic!("unexpected achievement {other}"),
            }
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_earns_from_grants_records_holders_once_and_silently(pool: PgPool) {
        let full = user(&pool, 1669114).await;
        let partial = user(&pool, 1669115).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source, created_at) VALUES ($1, 'head', 'turtle', 'achievement', NOW() - INTERVAL '9 days'), ($1, 'tail', 'turtle', 'achievement', NOW() - INTERVAL '9 days')",
        )
        .bind(full)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source, created_at) VALUES ($1, 'head', 'judge', 'achievement', NOW() - INTERVAL '5 days')",
        )
        .bind(partial)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(backfill_earns_from_grants(&pool).await.unwrap(), 2);
        let full_grant_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT MIN(created_at) FROM customization_grants WHERE user_id = $1 AND slug = 'turtle'",
        )
        .bind(full)
        .fetch_one(&pool)
        .await
        .unwrap();
        let full_row = sqlx::query!(
            "SELECT achievement, battlesnake_id, game_id, turn_number, live, earned_at FROM achievement_earns WHERE user_id = $1",
            full,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(full_row.achievement, "slow_and_steady");
        assert!(!full_row.live);
        assert!(full_row.battlesnake_id.is_none());
        assert!(full_row.game_id.is_none());
        assert!(full_row.turn_number.is_none());
        assert_eq!(full_row.earned_at, full_grant_at);
        let partial_grant_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT created_at FROM customization_grants WHERE user_id = $1 AND slug = 'judge'",
        )
        .bind(partial)
        .fetch_one(&pool)
        .await
        .unwrap();
        let partial_row = sqlx::query!(
            "SELECT achievement, earned_at FROM achievement_earns WHERE user_id = $1",
            partial,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(partial_row.achievement, "ladder_regular");
        assert_eq!(partial_row.earned_at, partial_grant_at);
        let partial_tails: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM customization_grants WHERE user_id = $1 AND customization_type = 'tail'",
        )
        .bind(partial)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(partial_tails, 0);
        let side_effects: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(side_effects, (0, 0, 0));
        assert_eq!(backfill_earns_from_grants(&pool).await.unwrap(), 0);
        let full_row_again = sqlx::query!(
            "SELECT live, game_id, earned_at FROM achievement_earns WHERE user_id = $1",
            full,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(full_row_again.earned_at, full_row.earned_at);
        assert_eq!(full_row_again.live, full_row.live);
        assert_eq!(full_row_again.game_id, full_row.game_id);
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_earns")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_achievements_job_records_earns_without_cursor_reset(pool: PgPool) {
        let holder = user(&pool, 1669116).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source, created_at) VALUES ($1, 'head', 'turtle', 'achievement', NOW() - INTERVAL '9 days'), ($1, 'tail', 'turtle', 'achievement', NOW() - INTERVAL '9 days')",
        )
        .bind(holder)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE achievement_backfill_cursor SET completed_at = NOW() WHERE singleton = TRUE",
        )
        .execute(&pool)
        .await
        .unwrap();
        let state = crate::state::AppState::test_from_pool(pool.clone());
        use cja::jobs::Job as _;
        crate::jobs::BackfillAchievementsJob
            .run(state)
            .await
            .unwrap();
        let row = sqlx::query!(
            "SELECT achievement, live FROM achievement_earns WHERE user_id = $1",
            holder,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.achievement, "slow_and_steady");
        assert!(!row.live);
        let completed_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT completed_at FROM achievement_backfill_cursor WHERE singleton = TRUE",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(completed_at.is_some());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn other_source_items_never_count_as_earned(pool: PgPool) {
        let token_holder = user(&pool, 1669117).await;
        let code_holder = user(&pool, 1669118).await;
        let rival = user(&pool, 1669119).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'frog', 'token')",
        )
        .bind(token_holder)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'monkey', 'code'), ($1, 'tail', 'monkey', 'code')",
        )
        .bind(code_holder)
        .execute(&pool)
        .await
        .unwrap();
        let token_snake = snake(&pool, token_holder, "token holder snake").await;
        let code_snake = snake(&pool, code_holder, "code holder snake").await;
        let rival_one = snake(&pool, rival, "other source rival one").await;
        let rival_two = snake(&pool, rival, "other source rival two").await;
        let rival_three = snake(&pool, rival, "other source rival three").await;
        let rival_four = snake(&pool, rival, "other source rival four").await;
        let token_win = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, token_win, token_snake, 1, None).await;
        participant(&pool, token_win, rival_one, 2, None).await;
        let code_win = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, code_win, code_snake, 1, None).await;
        participant(&pool, code_win, rival_two, 2, None).await;
        participant(&pool, code_win, rival_three, 3, None).await;
        participant(&pool, code_win, rival_four, 4, None).await;

        assert_eq!(backfill_earns_from_grants(&pool).await.unwrap(), 0);
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool)
            .await
            .unwrap();
        loop {
            let page = backfill_achievement_page(&pool).await.unwrap();
            if page.complete {
                break;
            }
        }
        let token_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM achievement_earns WHERE user_id = $1")
                .bind(token_holder)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(token_rows, 0);
        let code_crowd: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM achievement_earns WHERE user_id = $1 AND achievement = 'crowd_pleaser'",
        )
        .bind(code_holder)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(code_crowd, 0);
        let code_first = sqlx::query!(
            "SELECT live, game_id, turn_number FROM achievement_earns WHERE user_id = $1 AND achievement = 'first_win'",
            code_holder,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!code_first.live);
        assert!(code_first.game_id.is_none());
        assert!(code_first.turn_number.is_none());
        assert!(has(&grants(&pool, code_holder).await, Head::KIND, "frog"));
        let side_effects: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(side_effects, (0, 0, 0));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn backfill_rule_fire_records_partial_other_source_holder(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let partial = user(&pool, 1669120).await;
        let rival = user(&pool, 1669121).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'monkey', 'code')",
        )
        .bind(partial)
        .execute(&pool)
        .await
        .unwrap();
        let own = snake(&pool, partial, "partial holder snake").await;
        let rival_two = snake(&pool, rival, "partial rival two").await;
        let rival_three = snake(&pool, rival, "partial rival three").await;
        let rival_four = snake(&pool, rival, "partial rival four").await;
        let win = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, win, own, 1, None).await;
        participant(&pool, win, rival_two, 2, None).await;
        participant(&pool, win, rival_three, 3, None).await;
        participant(&pool, win, rival_four, 4, None).await;
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool)
            .await
            .unwrap();
        loop {
            let page = backfill_achievement_page(&pool).await.unwrap();
            if page.complete {
                break;
            }
        }
        assert!(grants(&pool, partial).await.contains(&(
            Tail::KIND.to_string(),
            "monkey".to_string(),
            "achievement".to_string()
        )));
        let row = sqlx::query!(
            "SELECT battlesnake_id, game_id, turn_number, live, earned_at FROM achievement_earns WHERE user_id = $1 AND achievement = 'crowd_pleaser'",
            partial,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!row.live);
        assert!(row.battlesnake_id.is_none());
        assert!(row.game_id.is_none());
        assert!(row.turn_number.is_none());
        let tail_grant_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT created_at FROM customization_grants WHERE user_id = $1 AND customization_type = 'tail' AND slug = 'monkey'",
        )
        .bind(partial)
        .fetch_one(&pool)
        .await
        .unwrap();
        let delta = (row.earned_at - tail_grant_at).num_milliseconds().abs();
        assert!(
            delta < 5_000,
            "crowd_pleaser earned_at {delta}ms from tail grant"
        );
        let first_win = sqlx::query!(
            "SELECT live FROM achievement_earns WHERE user_id = $1 AND achievement = 'first_win'",
            partial,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!first_win.live);
        let side_effects: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(side_effects, (0, 0, 0));
        assert!(
            crate::test_capture::Capture::shared()
                .events("achievement_earned", partial)
                .is_empty()
        );
        // Re-running both backfill paths leaves counts and earned_at unchanged.
        loop {
            let page = backfill_achievement_page(&pool).await.unwrap();
            if page.complete {
                break;
            }
        }
        assert_eq!(backfill_earns_from_grants(&pool).await.unwrap(), 0);
        let rows_after: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT achievement, earned_at FROM achievement_earns WHERE user_id = $1 ORDER BY achievement",
        )
        .bind(partial)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows_after.len(), 2);
        let crowd_again = sqlx::query!(
            "SELECT earned_at FROM achievement_earns WHERE user_id = $1 AND achievement = 'crowd_pleaser'",
            partial,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(crowd_again.earned_at, row.earned_at);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn live_win_earns_recorded_for_other_source_holders(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let owner = user(&pool, 1669122).await;
        let rival = user(&pool, 1669123).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'frog', 'code')",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        let winner = snake(&pool, owner, "code frog winner").await;
        let opponent = snake(&pool, rival, "code frog opponent").await;
        let game_id = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, game_id, winner, 1, None).await;
        participant(&pool, game_id, opponent, 2, None).await;
        assert_eq!(
            award_for_game(&pool, game_id, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let notified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notified, 0);
        let row = sqlx::query!(
            "SELECT battlesnake_id, game_id, turn_number, live FROM achievement_earns WHERE user_id = $1",
            owner,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.battlesnake_id, Some(winner));
        assert_eq!(row.game_id, Some(game_id));
        assert_eq!(row.turn_number, None);
        assert!(row.live);
        let events = crate::test_capture::Capture::shared().events("achievement_earned", owner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["slug"], "first_win");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn count_based_live_earn_recorded_for_other_source_both_owner(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let owner = user(&pool, 1669124).await;
        let rival = user(&pool, 1669125).await;
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'turtle', 'play_import'), ($1, 'tail', 'turtle', 'play_import')",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        let own = snake(&pool, owner, "imported turtle").await;
        let own_entry = entry(&pool, own).await;
        let peer = snake(&pool, rival, "turtle peer").await;
        seed_games(
            &pool,
            own,
            Some(own_entry),
            Some(peer),
            2_999,
            2,
            "finished",
        )
        .await;
        // The peer wins the milestone game; pre-owned frog keeps its award a
        // no-op so the owner's counts stay the only side effects.
        sqlx::query(
            "INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, 'head', 'frog', 'admin')",
        )
        .bind(rival)
        .execute(&pool)
        .await
        .unwrap();
        let milestone = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, milestone, own, 2, Some(own_entry)).await;
        participant(&pool, milestone, peer, 1, None).await;
        assert_eq!(
            award_for_game(&pool, milestone, "https://arena.example")
                .await
                .unwrap(),
            0
        );
        let row = sqlx::query!(
            "SELECT battlesnake_id, game_id, turn_number, live FROM achievement_earns WHERE user_id = $1 AND achievement = 'slow_and_steady'",
            owner,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.battlesnake_id, Some(own));
        assert_eq!(row.game_id, Some(milestone));
        assert_eq!(row.turn_number, None);
        assert!(row.live);
        let owner_grants = grants(&pool, owner).await;
        assert!(
            owner_grants
                .iter()
                .all(|(_, _, source)| source != "achievement")
        );
        let notified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notified, 0);
        let events = crate::test_capture::Capture::shared().events("achievement_earned", owner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["slug"], "slow_and_steady");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reconcile_records_live_earns_without_notifying(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let owner = user(&pool, 1669126).await;
        let rival = user(&pool, 1669127).await;
        let own = snake(&pool, owner, "reconcile turtle").await;
        let own_entry = entry(&pool, own).await;
        let peer = snake(&pool, rival, "reconcile peer").await;
        seed_games(
            &pool,
            own,
            Some(own_entry),
            Some(peer),
            2_999,
            2,
            "finished",
        )
        .await;
        let window = game(&pool, "Standard", "finished", Some(1)).await;
        participant(&pool, window, own, 2, Some(own_entry)).await;
        participant(&pool, window, peer, 1, None).await;
        let before: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(reconcile_recent_achievements(&pool).await.unwrap(), 3);
        let row = sqlx::query!(
            "SELECT battlesnake_id, game_id, live FROM achievement_earns WHERE user_id = $1 AND achievement = 'slow_and_steady'",
            owner,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.battlesnake_id, Some(own));
        assert_eq!(row.game_id, Some(window));
        assert!(row.live);
        let events = crate::test_capture::Capture::shared().events("achievement_earned", owner);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["slug"], "slow_and_steady");
        let after: (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM notifications), \
                    (SELECT COUNT(*) FROM notification_emails), \
                    (SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn achievement_slugs_and_kinds_are_stable() {
        let slugs: Vec<_> = Achievement::ALL.iter().map(|a| a.slug()).collect();
        assert_eq!(
            slugs,
            vec![
                "first_win",
                "slow_and_steady",
                "constrictor_champ",
                "crowd_pleaser",
                "ladder_regular"
            ]
        );
        let unique: HashSet<_> = slugs.iter().copied().collect();
        assert_eq!(unique.len(), Achievement::ALL.len());
        for achievement in Achievement::ALL {
            assert_eq!(achievement.kind(), "getting_started");
        }
        // Share URLs name achievements by slug; both directions are stable.
        for achievement in Achievement::ALL {
            assert_eq!(
                Achievement::from_slug(achievement.slug()),
                Some(achievement)
            );
        }
        assert_eq!(Achievement::from_slug("nope"), None);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn earn_recording_failure_never_costs_a_grant(pool: PgPool) {
        crate::test_capture::Capture::shared();
        let a = user(&pool, 1669101).await;
        let b = user(&pool, 1669102).await;
        let c = user(&pool, 1669103).await;
        let rival = user(&pool, 1669128).await;
        let a_snake = snake(&pool, a, "savepoint a").await;
        let b_snake = snake(&pool, b, "savepoint b").await;
        let c_snake = snake(&pool, c, "savepoint c").await;
        let rival_a = snake(&pool, rival, "savepoint rival a").await;
        let rival_b = snake(&pool, rival, "savepoint rival b").await;
        let rival_c = snake(&pool, rival, "savepoint rival c").await;
        sqlx::query(
            "CREATE FUNCTION reject_achievement_earn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected earn failure'; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_achievement_earn BEFORE INSERT ON achievement_earns FOR EACH ROW EXECUTE FUNCTION reject_achievement_earn()",
        )
        .execute(&pool)
        .await
        .unwrap();

        // A live award survives an earn-recording failure.
        let a_game = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, a_game, a_snake, 1, None).await;
        participant(&pool, a_game, rival_a, 2, None).await;
        assert_eq!(
            award_for_game(&pool, a_game, "https://arena.example")
                .await
                .unwrap(),
            1
        );
        assert!(has(&grants(&pool, a).await, Head::KIND, "frog"));
        let a_notified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND kind = 'achievement_earned'",
        )
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(a_notified, 1);
        let a_emails: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notification_emails e JOIN notifications n USING (notification_id) WHERE n.user_id = $1",
        )
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(a_emails, 1);
        let email_jobs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE name = 'SendNotificationEmailJob'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(email_jobs, 1);
        let a_earns: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM achievement_earns WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(a_earns, 0);
        assert!(
            crate::test_capture::Capture::shared()
                .events("achievement_earned", a)
                .is_empty()
        );

        // Reconciliation completes for another owner with the trigger armed.
        let b_game = game(&pool, "Standard", "finished", Some(1)).await;
        participant(&pool, b_game, b_snake, 1, None).await;
        participant(&pool, b_game, rival_b, 2, None).await;
        reconcile_recent_achievements(&pool).await.unwrap();
        assert!(has(&grants(&pool, b).await, Head::KIND, "frog"));
        assert!(has(&grants(&pool, a).await, Head::KIND, "frog"));
        let earns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_earns")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(earns, 0);

        // The backfill job survives too: its earn statement and its pages log
        // and continue, and the cursor still completes.
        let c_game = game(&pool, "Standard", "finished", Some(0)).await;
        participant(&pool, c_game, c_snake, 1, None).await;
        participant(&pool, c_game, rival_c, 2, None).await;
        sqlx::query("UPDATE achievement_backfill_cursor SET after_user_id = NULL, completed_at = NULL WHERE singleton = TRUE")
            .execute(&pool)
            .await
            .unwrap();
        let state = crate::state::AppState::test_from_pool(pool.clone());
        use cja::jobs::Job as _;
        crate::jobs::BackfillAchievementsJob
            .run(state)
            .await
            .unwrap();
        let completed_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT completed_at FROM achievement_backfill_cursor WHERE singleton = TRUE",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(completed_at.is_some());
        assert!(has(&grants(&pool, c).await, Head::KIND, "frog"));
        let earns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM achievement_earns")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(earns, 0);

        // Once recording works again, the job-top statement backfills every
        // holder with a grant-dated, non-live row.
        sqlx::query("DROP TRIGGER reject_achievement_earn ON achievement_earns")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP FUNCTION reject_achievement_earn()")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(backfill_earns_from_grants(&pool).await.unwrap(), 3);
        for owner in [a, b, c] {
            let row = sqlx::query!(
                "SELECT game_id, turn_number, live, earned_at FROM achievement_earns WHERE user_id = $1 AND achievement = 'first_win'",
                owner,
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!row.live);
            assert!(row.game_id.is_none());
            assert!(row.turn_number.is_none());
            let grant_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
                "SELECT created_at FROM customization_grants WHERE user_id = $1 AND slug = 'frog'",
            )
            .bind(owner)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(row.earned_at, grant_at);
        }
    }
}
