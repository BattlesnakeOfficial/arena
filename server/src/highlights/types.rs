//! Domain types shared by the highlights extract, detector, and render
//! layers (DEV-1993). See `highlights.py` in the byte-scratch prototype for
//! the detector semantics these types carry.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use uuid::Uuid;

/// skillratings-style beta used by the arena rating system.
pub const BETA: f64 = 25.0 / 6.0;

/// Canonical display order for the public ladders (prototype `LADDER_ORDER`).
/// Display ordering only — the Duels ladder is identified by
/// `match_size == 2`, never by name.
pub const LADDER_ORDER: [&str; 4] = [
    "Duels 11x11",
    "Standard 11x11",
    "Royale 11x11",
    "Constrictor 11x11",
];

/// Length of a digest window. Invariant-checked by the schema as well.
const WEEK_SECS: i64 = 7 * 24 * 60 * 60;

/// A stored digest's window: 7 days ending Saturday 12:00:00 UTC.
///
/// Construct only via [`Week::latest_completed`] / [`Week::ending`]; both
/// guarantee alignment, so stored windows always satisfy the table CHECKs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Week {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Week {
    /// The most recent 7-day window ending Saturday 12:00 UTC at or before
    /// `now`.
    pub fn latest_completed(now: DateTime<Utc>) -> Week {
        // Snap to the most recent 12:00 UTC boundary at or before `now`...
        let noon = now
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .expect("noon is a valid time");
        let noon = Utc.from_utc_datetime(&noon);
        let mut end = if now >= noon {
            noon
        } else {
            noon - chrono::Duration::days(1)
        };
        // ...then roll back to Saturday.
        while end.weekday() != chrono::Weekday::Sat {
            end -= chrono::Duration::days(1);
        }
        Week {
            start: end - chrono::Duration::seconds(WEEK_SECS),
            end,
        }
    }

    /// The week ending on `saturday` at 12:00 UTC. `None` when the date is
    /// not a Saturday or the window is not fully in the past.
    pub fn ending(saturday: NaiveDate, now: DateTime<Utc>) -> Option<Week> {
        if saturday.weekday() != chrono::Weekday::Sat {
            return None;
        }
        let noon = saturday
            .and_hms_opt(12, 0, 0)
            .expect("noon is a valid time");
        let end = Utc.from_utc_datetime(&noon);
        if end > now {
            return None;
        }
        Some(Week {
            start: end - chrono::Duration::seconds(WEEK_SECS),
            end,
        })
    }
}

/// GameType display string for a ladder name first word, used only when
/// rendering ladder prefixes; ladders are identified by id, not this.
pub fn ladder_short_name(name: &str) -> &str {
    name.split(' ').next().unwrap_or(name)
}

/// A public ladder in the window.
#[derive(Debug, Clone)]
pub struct Ladder {
    pub leaderboard_id: Uuid,
    pub name: String,
    pub match_size: i32,
}

impl Ladder {
    pub fn is_duels(&self) -> bool {
        self.match_size == 2
    }
}

/// House snakes: latency probes and the stress baseline. Kept in the data,
/// tagged in output, never headlined.
#[derive(Debug, Clone)]
pub struct HouseSnakeConfig {
    /// `users.github_login` values whose snakes are house snakes.
    pub owners: Vec<String>,
    /// `battlesnakes.name` values that are house snakes regardless of owner.
    pub snake_names: Vec<String>,
}

impl Default for HouseSnakeConfig {
    fn default() -> Self {
        Self {
            owners: vec!["byte-the-bot".to_string()],
            snake_names: vec!["Bombastic Bob".to_string()],
        }
    }
}

impl HouseSnakeConfig {
    pub fn is_house(&self, owner_login: Option<&str>, snake_name: &str) -> bool {
        owner_login.is_some_and(|owner| self.owners.iter().any(|o| o == owner))
            || self.snake_names.iter().any(|n| n == snake_name)
    }
}

/// The final frame of one snake in one game (flattened from the engine's
/// PascalCase JSON), plus the snake's faulted (timeout/error) turns.
#[derive(Debug, Clone, Default)]
pub struct FinalFrame {
    pub game_battlesnake_id: Uuid,
    pub length: i32,
    pub health: i32,
    pub death_cause: Option<String>,
    pub death_turn: Option<i32>,
    /// game_battlesnake_id of the killer, when any.
    pub eliminated_by: Option<Uuid>,
    /// Turns this snake timed out or errored (filled in the faults phase).
    pub fault_turns: BTreeSet<i32>,
}

/// One participant in one window game.
#[derive(Debug, Clone)]
pub struct WindowSnake {
    pub entry_id: Uuid,
    pub snake_name: String,
    pub owner_login: Option<String>,
    pub house: bool,
    pub placement: i32,
    pub mu_before: f64,
    pub sigma_before: f64,
    pub mu_after: f64,
    pub sigma_after: f64,
    pub frame: Option<FinalFrame>,
}

impl WindowSnake {
    /// Display name, tagged when the snake is a house snake.
    pub fn label(&self) -> String {
        if self.house {
            format!("{} (house)", self.snake_name)
        } else {
            self.snake_name.clone()
        }
    }

    /// Owner login, `"?"` when unknown (matches the prototype).
    pub fn owner_label(&self) -> &str {
        self.owner_login.as_deref().unwrap_or("?")
    }
}

/// One rated ladder game in the window, with its participants.
#[derive(Debug, Clone)]
pub struct WindowGame {
    pub game_id: Uuid,
    /// Index into [`WindowData::ladders`].
    pub ladder_index: usize,
    pub created_at: DateTime<Utc>,
    pub snakes: Vec<WindowSnake>,
    pub final_turn: Option<i32>,
}

impl WindowGame {
    pub fn winners(&self) -> Vec<&WindowSnake> {
        self.snakes.iter().filter(|s| s.placement == 1).collect()
    }

    /// The single placement-1 snake, or `None` for shared first place.
    pub fn unique_winner(&self) -> Option<&WindowSnake> {
        let winners = self.winners();
        (winners.len() == 1).then(|| winners[0])
    }

    /// Pre-game P(first place) per snake: Plackett-Luce over `mu_before`
    /// with `c = sqrt(Σ(σ² + β²)) / k`, softmax max-shifted. Aligned with
    /// `self.snakes`.
    pub fn win_probs(&self, k: f64) -> Vec<f64> {
        let c = (self
            .snakes
            .iter()
            .map(|s| s.sigma_before * s.sigma_before + BETA * BETA)
            .sum::<f64>())
        .sqrt()
            / k;
        let m = self
            .snakes
            .iter()
            .map(|s| s.mu_before)
            .fold(f64::NEG_INFINITY, f64::max);
        let weights: Vec<f64> = self
            .snakes
            .iter()
            .map(|s| ((s.mu_before - m) / c).exp())
            .collect();
        let z: f64 = weights.iter().sum();
        weights.iter().map(|w| w / z).collect()
    }

    pub fn ladder<'a>(&self, data: &'a WindowData) -> &'a Ladder {
        &data.ladders[self.ladder_index]
    }
}

/// Static info about a leaderboard entry (all public ladders).
#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub leaderboard_id: Uuid,
    pub snake_name: String,
    pub owner_login: Option<String>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub house: bool,
}

/// Everything the detectors need for one window.
#[derive(Debug, Clone, Default)]
pub struct WindowData {
    /// Ordered: `LADDER_ORDER` position first, then name.
    pub ladders: Vec<Ladder>,
    /// Sorted by `(created_at, game_id)`.
    pub games: Vec<WindowGame>,
    /// Key: leaderboard_entry_id.
    pub entries: BTreeMap<Uuid, EntryInfo>,
    /// entry -> (mu_after, sigma_after) of its last result before the window.
    pub start_ratings: BTreeMap<Uuid, (f64, f64)>,
}

impl WindowData {
    pub fn games_on_ladder(&self, ladder_index: usize) -> Vec<&WindowGame> {
        self.games
            .iter()
            .filter(|g| g.ladder_index == ladder_index)
            .collect()
    }

    /// The public ladder with `match_size = 2` (first in display order when
    /// several exist).
    pub fn duels_ladder_index(&self) -> Option<usize> {
        self.ladders.iter().position(Ladder::is_duels)
    }
}

/// The 14 section kinds, in no inherent order; `scope` defines the canonical
/// section set emitted for every digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum SectionKind {
    Throne,
    Movers,
    Newcomers,
    Upsets,
    PhotoFinishes,
    MutualDestruction,
    Chicken,
    Fumes,
    HatTricks,
    Marathons,
    Streaks,
    Rivalries,
    OpsFootnote,
    Calibration,
}

/// Each section kind's ladder scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionScope {
    /// One section per public ladder.
    PerLadder,
    /// One per public ladder with `match_size != 2`.
    PerLadderNonDuel,
    /// One section on the Duels ladder (`match_size == 2`); omitted from the
    /// digest when no such ladder exists.
    DuelsOnly,
    /// One section, `ladder_id` NULL.
    CrossLadder,
}

impl SectionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            SectionKind::Throne => "throne",
            SectionKind::Movers => "movers",
            SectionKind::Newcomers => "newcomers",
            SectionKind::Upsets => "upsets",
            SectionKind::PhotoFinishes => "photo_finishes",
            SectionKind::MutualDestruction => "mutual_destruction",
            SectionKind::Chicken => "chicken",
            SectionKind::Fumes => "fumes",
            SectionKind::HatTricks => "hat_tricks",
            SectionKind::Marathons => "marathons",
            SectionKind::Streaks => "streaks",
            SectionKind::Rivalries => "rivalries",
            SectionKind::OpsFootnote => "ops_footnote",
            SectionKind::Calibration => "calibration",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "throne" => SectionKind::Throne,
            "movers" => SectionKind::Movers,
            "newcomers" => SectionKind::Newcomers,
            "upsets" => SectionKind::Upsets,
            "photo_finishes" => SectionKind::PhotoFinishes,
            "mutual_destruction" => SectionKind::MutualDestruction,
            "chicken" => SectionKind::Chicken,
            "fumes" => SectionKind::Fumes,
            "hat_tricks" => SectionKind::HatTricks,
            "marathons" => SectionKind::Marathons,
            "streaks" => SectionKind::Streaks,
            "rivalries" => SectionKind::Rivalries,
            "ops_footnote" => SectionKind::OpsFootnote,
            "calibration" => SectionKind::Calibration,
            _ => return None,
        })
    }

    /// The kind's ladder scope — defines the canonical section set.
    pub const fn scope(self) -> SectionScope {
        match self {
            SectionKind::Throne
            | SectionKind::Movers
            | SectionKind::Upsets
            | SectionKind::PhotoFinishes
            | SectionKind::MutualDestruction
            | SectionKind::Fumes
            | SectionKind::Marathons
            | SectionKind::Streaks => SectionScope::PerLadder,
            SectionKind::HatTricks => SectionScope::PerLadderNonDuel,
            SectionKind::Chicken | SectionKind::Rivalries | SectionKind::OpsFootnote => {
                SectionScope::DuelsOnly
            }
            SectionKind::Newcomers | SectionKind::Calibration => SectionScope::CrossLadder,
        }
    }

    /// Human-readable heading (matches the prototype digest headings).
    pub const fn heading(self) -> &'static str {
        match self {
            SectionKind::Throne => "The throne",
            SectionKind::Movers => "Ladder movers",
            SectionKind::Newcomers => "New on the ladders",
            SectionKind::Upsets => "Upsets",
            SectionKind::PhotoFinishes => "Photo finishes",
            SectionKind::MutualDestruction => "Mutual destruction",
            SectionKind::Chicken => "Chicken",
            SectionKind::Fumes => "Won on fumes",
            SectionKind::HatTricks => "Hat tricks",
            SectionKind::Marathons => "Marathons",
            SectionKind::Streaks => "Hot streaks",
            SectionKind::Rivalries => "Rivalries",
            SectionKind::OpsFootnote => "Ops footnote: opening crashes",
            SectionKind::Calibration => "How rare is a long shot really?",
        }
    }
}

/// One rendered line within a section. `text` carries no links — links
/// derive from `game_id` at render time.
#[derive(Debug, Clone)]
pub struct SectionItem {
    pub game_id: Option<Uuid>,
    pub subject: Option<String>,
    pub value: Option<f64>,
    pub text: String,
}

/// One section of a digest, for one kind and (usually) one ladder.
#[derive(Debug, Clone)]
pub struct Section {
    pub kind: SectionKind,
    pub ladder: Option<Ladder>,
    pub summary: String,
    pub items: Vec<SectionItem>,
}

impl Section {
    pub fn empty(kind: SectionKind, ladder: Option<Ladder>) -> Self {
        Self {
            kind,
            ladder,
            summary: "none this week".to_string(),
            items: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.summary == "none this week"
    }
}

/// A pickable game fact for the Discord message: a rendered line without
/// links plus the game id.
#[derive(Debug, Clone)]
pub struct Pick {
    pub game_id: Option<Uuid>,
    pub text: String,
}

/// Throne facts for the Discord message (ladders with changes only).
#[derive(Debug, Clone)]
pub struct ThroneChange {
    pub ladder_name: String,
    pub changes: i32,
    pub crowning: Option<Pick>,
}

/// Typed facts the Discord message needs, produced by the detectors
/// alongside the sections and consumed at store time. Not persisted.
#[derive(Debug, Clone, Default)]
pub struct DiscordPicks {
    pub throne_changes: Vec<ThroneChange>,
    pub duel_photo_finish: Option<Pick>,
    pub top_upset: Option<Pick>,
}

/// A fully computed digest for a window: the canonical section set in
/// display order plus the Discord picks.
#[derive(Debug, Clone)]
pub struct ComputedDigest {
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub games_count: i64,
    /// Games whose final frame is missing (retention) — drives the banner.
    pub games_without_frames: i64,
    pub per_ladder_counts: Vec<(String, i64)>,
    pub duel_k: f64,
    pub four_snake_k: f64,
    pub sections: Vec<Section>,
    pub picks: DiscordPicks,
}

/// The display order of section kinds within a digest (Calibration sits
/// right after Upsets, mirroring the prototype digest).
pub const SECTION_DISPLAY_ORDER: [SectionKind; 14] = [
    SectionKind::Throne,
    SectionKind::Movers,
    SectionKind::Newcomers,
    SectionKind::Upsets,
    SectionKind::Calibration,
    SectionKind::PhotoFinishes,
    SectionKind::MutualDestruction,
    SectionKind::Chicken,
    SectionKind::Fumes,
    SectionKind::HatTricks,
    SectionKind::Marathons,
    SectionKind::Streaks,
    SectionKind::Rivalries,
    SectionKind::OpsFootnote,
];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn latest_completed_at_saturday_1300z_is_that_week() {
        let week = Week::latest_completed(at(2026, 10, 31, 13, 0));
        assert_eq!(week.start, at(2026, 10, 24, 12, 0));
        assert_eq!(week.end, at(2026, 10, 31, 12, 0));
    }

    #[test]
    fn latest_completed_before_noon_is_previous_week() {
        let week = Week::latest_completed(at(2026, 10, 31, 11, 59));
        assert_eq!(week.end, at(2026, 10, 24, 12, 0));
        assert_eq!(week.start, at(2026, 10, 17, 12, 0));
    }

    #[test]
    fn latest_completed_exactly_at_boundary_is_that_week() {
        let week = Week::latest_completed(at(2026, 10, 31, 12, 0));
        assert_eq!(week.end, at(2026, 10, 31, 12, 0));
    }

    #[test]
    fn latest_completed_midweek_rolls_back_to_saturday() {
        // Wednesday Oct 28, 15:00Z → week ending Sat Oct 24 12:00Z.
        let week = Week::latest_completed(at(2026, 10, 28, 15, 0));
        assert_eq!(week.end, at(2026, 10, 24, 12, 0));
    }

    #[test]
    fn ending_on_past_saturday_is_some() {
        let saturday = chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        let week = Week::ending(saturday, at(2026, 11, 1, 0, 0)).unwrap();
        assert_eq!(week.end, at(2026, 10, 31, 12, 0));
        assert_eq!(week.start, at(2026, 10, 24, 12, 0));
    }

    #[test]
    fn ending_on_friday_is_none() {
        let friday = chrono::NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        assert!(Week::ending(friday, at(2026, 11, 1, 0, 0)).is_none());
    }

    #[test]
    fn ending_on_future_saturday_is_none() {
        let saturday = chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        assert!(Week::ending(saturday, at(2026, 10, 31, 12, 0)).is_none());
    }

    #[test]
    fn ending_at_exact_window_end_is_some() {
        // The window ending exactly now is complete.
        let saturday = chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        assert!(Week::ending(saturday, at(2026, 10, 31, 12, 0)).is_some());
    }

    #[test]
    fn section_kind_round_trips() {
        for kind in SECTION_DISPLAY_ORDER {
            assert_eq!(SectionKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SectionKind::parse("nonsense"), None);
    }

    #[test]
    fn house_config_matches_owner_and_name() {
        let house = HouseSnakeConfig::default();
        assert!(house.is_house(Some("byte-the-bot"), "anything"));
        assert!(house.is_house(None, "Bombastic Bob"));
        assert!(!house.is_house(Some("coreyja"), "Odiodin"));
        assert!(!house.is_house(None, "Odiodin"));
    }
}
