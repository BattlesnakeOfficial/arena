//! The highlights detectors: pure functions over [`WindowData`], one per
//! section, ported from the byte-scratch prototype's `highlights.py`.
//!
//! House exclusion sits exactly where the prototype has it: upsets (winner
//! and favourite), hat-trick victims, movers, and streaks *output* exclude
//! house snakes; throne and chicken counters include them, tagged
//! "(house)". Every "win" means a unique placement-1.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::types::{
    BETA, ComputedDigest, DiscordPicks, Ladder, Pick, SECTION_DISPLAY_ORDER, Section, SectionItem,
    SectionKind, SectionScope, ThroneChange, WindowData, WindowGame, WindowSnake,
};

// ---------------------------------------------------------------- fit_k

/// Max-likelihood sharpening factor for P(first) over a window's
/// unique-winner games. Grid `k = 0.50..=4.00 step 0.02` (prototype
/// `range(50, 401, 2)/100`); ties pick the highest k (Python `max` over
/// `(ll, k)` tuples). With no qualifying games every ll is 0, so the
/// highest grid k (4.0) wins — same as the prototype.
pub fn fit_k(games: &[&WindowGame], duels: bool) -> f64 {
    let candidates: Vec<&WindowGame> = games
        .iter()
        .copied()
        .filter(|g| g.unique_winner().is_some() && ((g.snakes.len() == 2) == duels))
        .collect();
    let series: Vec<(f64, f64)> = (50..=400)
        .step_by(2)
        .map(|i| {
            let k = i as f64 / 100.0;
            let ll: f64 = candidates
                .iter()
                .map(|game| {
                    let winner = game.unique_winner().expect("filtered above");
                    let index = game
                        .snakes
                        .iter()
                        .position(|s| s.entry_id == winner.entry_id)
                        .expect("winner is a participant");
                    game.win_probs(k)[index].ln()
                })
                .sum();
            (ll, k)
        })
        .collect();
    argmax_ties_highest(&series)
}

/// Argmax over `(ll, k)` pairs where a strictly greater `(ll, k)` tuple
/// wins — so equal-ll ties resolve to the highest k, matching Python's
/// `max(best, (ll, k))` tuple comparison over the ascending grid.
fn argmax_ties_highest(series: &[(f64, f64)]) -> f64 {
    let mut best = &series[0];
    for item in &series[1..] {
        if item.0 > best.0 || (item.0 == best.0 && item.1 > best.1) {
            best = item;
        }
    }
    best.1
}

// ---------------------------------------------------------------- helpers

/// The snake whose final-frame `EliminatedBy` matches `s`'s killer.
fn killer_of<'a>(game: &'a WindowGame, snake: &WindowSnake) -> Option<&'a WindowSnake> {
    let killer_id = snake.frame.as_ref()?.eliminated_by?;
    game.snakes.iter().find(|other| {
        other
            .frame
            .as_ref()
            .is_some_and(|f| f.game_battlesnake_id == killer_id)
    })
}

/// Timed out or errored within two turns of its own elimination.
fn faulted(snake: &WindowSnake) -> bool {
    let Some(frame) = snake.frame.as_ref() else {
        return false;
    };
    let Some(turn) = frame.death_turn else {
        return false;
    };
    frame.fault_turns.range(turn - 2..=turn).next().is_some()
}

/// Fault exactly on the death turn (photo-finish "(loser timed out that
/// turn)" tag).
fn fault_on_death_turn(snake: &WindowSnake) -> bool {
    let Some(frame) = snake.frame.as_ref() else {
        return false;
    };
    frame
        .death_turn
        .is_some_and(|turn| frame.fault_turns.contains(&turn))
}

/// Death phrase for upset item lines (port of `how_died`).
fn how_died(game: &WindowGame, snake: &WindowSnake) -> String {
    let Some(frame) = snake.frame.as_ref() else {
        return format!("{} survived", snake.label());
    };
    let Some(cause) = frame.death_cause.as_deref() else {
        return format!("{} survived", snake.label());
    };
    let killer = killer_of(game, snake)
        .map(|k| k.label())
        .unwrap_or_else(|| "?".to_string());
    let base = match cause {
        "head-collision" => format!("{} lost a head-to-head with {}", snake.label(), killer),
        "snake-collision" => format!("{} ran into {}", snake.label(), killer),
        "wall-collision" => format!("{} hit a wall", snake.label()),
        "self-collision" => format!("{} ran into itself", snake.label()),
        "out-of-health" => format!("{} starved", snake.label()),
        "hazard" => format!("{} died in the hazard", snake.label()),
        other => format!("{}: {}", snake.label(), other),
    };
    match frame.death_turn {
        Some(turn) => format!("{base} (turn {turn})"),
        None => base,
    }
}

/// `%a %H:%MZ` timestamp, like the prototype.
fn short_time(at: DateTime<Utc>) -> String {
    at.format("%a %H:%MZ").to_string()
}

/// Thousands-grouped integer, like Python's `{:,}` (Rust's format!
/// has no grouping option).
fn thousands(n: i64) -> String {
    let raw = n.abs().to_string();
    let mut grouped = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    if n < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Percentage formatting matching Python's `{:.1%}`.
fn pct(value: f64) -> String {
    format!("{:.1}%", value * 100.0)
}

fn ladder_short(ladder: &Ladder) -> &str {
    ladder.name.split(' ').next().unwrap_or(&ladder.name)
}

fn winner_index(game: &WindowGame) -> Option<usize> {
    game.unique_winner().map(|w| {
        game.snakes
            .iter()
            .position(|s| s.entry_id == w.entry_id)
            .expect("winner is a participant")
    })
}

// ---------------------------------------------------------------- upsets

/// One qualifying upset row (shared by the candidate pass and the section
/// builder).
pub struct UpsetRow {
    pub calibrated_p: f64,
    pub raw_p: f64,
    pub game_index: usize,
    pub winner_index: usize,
}

/// Lowest calibrated P(first) winners, skipping games a loser lost to a
/// timeout or error. Fault turns are only consulted once fetched — empty
/// fault sets filter nothing, matching the prototype's `have_faults=False`
/// candidate pass.
pub fn upset_rows(games: &[&WindowGame], k: f64, top_n: usize) -> Vec<UpsetRow> {
    let mut rows = Vec::new();
    for (game_index, game) in games.iter().enumerate() {
        let Some(winner) = game.unique_winner() else {
            continue;
        };
        if winner.house {
            continue;
        }
        let Some(w_index) = winner_index(game) else {
            continue;
        };
        // Favourite: max mu_before, first on ties (Python `max`).
        let mut favourite = 0;
        for (i, snake) in game.snakes.iter().enumerate() {
            if snake.mu_before > game.snakes[favourite].mu_before {
                favourite = i;
            }
        }
        if favourite == w_index || game.snakes[favourite].house {
            continue;
        }
        if game
            .snakes
            .iter()
            .enumerate()
            .any(|(i, snake)| i != w_index && faulted(snake))
        {
            continue;
        }
        let calibrated = game.win_probs(k);
        let raw = game.win_probs(1.0);
        rows.push(UpsetRow {
            calibrated_p: calibrated[w_index],
            raw_p: raw[w_index],
            game_index,
            winner_index: w_index,
        });
    }
    rows.sort_by(|a, b| a.calibrated_p.partial_cmp(&b.calibrated_p).unwrap());
    rows.truncate(top_n);
    rows
}

fn upsets_section(
    ladder: &Ladder,
    games: &[&WindowGame],
    k: f64,
) -> (Section, Option<(Pick, f64)>) {
    let rows = upset_rows(games, k, 3);
    let mut section = Section::empty(SectionKind::Upsets, Some(ladder.clone()));
    let mut pick = None;
    if rows.is_empty() {
        return (section, pick);
    }
    section.summary = format!(
        "{} upsets (lowest calibrated pre-game win chance that still won)",
        rows.len()
    );
    for row in &rows {
        let game = games[row.game_index];
        let winner = &game.snakes[row.winner_index];
        let deaths: Vec<String> = game
            .snakes
            .iter()
            .filter(|s| s.entry_id != winner.entry_id)
            .map(|s| how_died(game, s))
            .collect();
        let one_in = (1.0 / row.calibrated_p).round() as i64;
        let text = format!(
            "{} turns: **{}** won at {} (1 in {}; raw model {}). {}.",
            game.final_turn
                .map(|t| t.to_string())
                .unwrap_or_else(|| "?".into()),
            winner.label(),
            pct(row.calibrated_p),
            thousands(one_in),
            pct(row.raw_p),
            deaths.join("; ")
        );
        if pick.is_none() {
            pick = Some((
                Pick {
                    game_id: Some(game.game_id),
                    text: format!(
                        "{} upset on {}: **{}** won at {}",
                        ladder_short(ladder),
                        ladder.name,
                        winner.label(),
                        pct(row.calibrated_p)
                    ),
                },
                row.calibrated_p,
            ));
        }
        section.items.push(SectionItem {
            game_id: Some(game.game_id),
            subject: Some(winner.snake_name.clone()),
            value: Some(row.calibrated_p),
            text,
        });
    }
    (section, pick)
}

// ---------------------------------------------------------------- throne

/// Everything the throne detector produces for one ladder (rank maps feed
/// movers, newcomers, and rivalries too).
pub struct ThroneOutcome {
    pub changes: i32,
    /// (entry, name, seconds at #1) sorted by held time desc.
    pub held: Vec<(Uuid, String, f64)>,
    /// (new leader name, old leader name, at, game) for the last change
    /// whose new == final leader.
    pub crowning: Option<(String, Option<String>, DateTime<Utc>, Uuid)>,
    pub rank0: BTreeMap<Uuid, i32>,
    pub rank1: BTreeMap<Uuid, i32>,
    pub names: BTreeMap<Uuid, String>,
    pub house: BTreeMap<Uuid, bool>,
}

/// Per-ladder replay: display-score (#1) reigns and rank maps at window
/// start and end. Port of `replay`. Scores are `mu - 3*sigma`; the map's
/// insertion order (start_ratings uuid order, then game order) provides
/// deterministic first-seen tie-breaks, matching the prototype's
/// entry_id-sorted CSV seeding.
pub fn replay_ladder(
    data: &WindowData,
    ladder_index: usize,
    w0: DateTime<Utc>,
    w1: DateTime<Utc>,
) -> ThroneOutcome {
    let ladder = &data.ladders[ladder_index];
    let games = data.games_on_ladder(ladder_index);

    let mut order: Vec<Uuid> = Vec::new();
    let mut score: BTreeMap<Uuid, f64> = BTreeMap::new();
    let mut names: BTreeMap<Uuid, String> = BTreeMap::new();
    let mut house: BTreeMap<Uuid, bool> = BTreeMap::new();
    for (entry, (mu, sigma)) in &data.start_ratings {
        if let Some(info) = data.entries.get(entry)
            && info.leaderboard_id == ladder.leaderboard_id
        {
            order.push(*entry);
            score.insert(*entry, mu - 3.0 * sigma);
            names.insert(*entry, info.snake_name.clone());
            house.insert(*entry, info.house);
        }
    }

    let active = |entry: Uuid, at: DateTime<Utc>| -> bool {
        data.entries
            .get(&entry)
            .and_then(|info| info.disabled_at)
            .is_none_or(|disabled_at| disabled_at > at)
    };

    let rank_at =
        |order: &[Uuid], score: &BTreeMap<Uuid, f64>, at: DateTime<Utc>| -> BTreeMap<Uuid, i32> {
            let mut alive: Vec<Uuid> = order
                .iter()
                .copied()
                .filter(|entry| score.contains_key(entry) && active(*entry, at))
                .collect();
            alive.sort_by(|a, b| score[b].partial_cmp(&score[a]).unwrap());
            alive
                .into_iter()
                .enumerate()
                .map(|(i, entry)| (entry, (i + 1) as i32))
                .collect()
        };
    let rank0 = rank_at(&order, &score, w0);

    let argmax_leader =
        |order: &[Uuid], score: &BTreeMap<Uuid, f64>, at: DateTime<Utc>| -> Option<Uuid> {
            let mut best: Option<Uuid> = None;
            for entry in order {
                let Some(s) = score.get(entry) else { continue };
                if !active(*entry, at) {
                    continue;
                }
                if best.is_none_or(|b| *s > score[&b]) {
                    best = Some(*entry);
                }
            }
            best
        };

    let mut leader = argmax_leader(&order, &score, w0);
    let mut since = w0;
    let mut held: BTreeMap<Uuid, f64> = BTreeMap::new();
    // (at, old, new, game); old None = the ladder's first-ever leader.
    let mut changes: Vec<(DateTime<Utc>, Option<Uuid>, Uuid, Uuid)> = Vec::new();

    for game in &games {
        for snake in &game.snakes {
            if !score.contains_key(&snake.entry_id) {
                order.push(snake.entry_id);
            }
            names.insert(snake.entry_id, snake.snake_name.clone());
            house.insert(snake.entry_id, snake.house);
            score.insert(snake.entry_id, snake.mu_after - 3.0 * snake.sigma_after);
        }
        let new = argmax_leader(&order, &score, game.created_at);
        if new != leader {
            if let Some(old) = leader {
                *held.entry(old).or_insert(0.0) += (game.created_at - since).num_seconds() as f64;
            }
            if let Some(new_leader) = new {
                changes.push((game.created_at, leader, new_leader, game.game_id));
            }
            leader = new;
            since = game.created_at;
        }
    }
    if let Some(final_leader) = leader {
        *held.entry(final_leader).or_insert(0.0) += (w1 - since).num_seconds() as f64;
    }
    let rank1 = rank_at(&order, &score, w1);

    let held: Vec<(Uuid, String, f64)> = {
        let mut v: Vec<(Uuid, String, f64)> = held
            .into_iter()
            .map(|(entry, secs)| {
                (
                    entry,
                    names.get(&entry).cloned().unwrap_or_else(|| "?".into()),
                    secs,
                )
            })
            .collect();
        v.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
        v
    };
    let name_of = |entry: Uuid| names.get(&entry).cloned().unwrap_or_else(|| "?".into());
    let crowning = leader.and_then(|final_leader| {
        changes
            .iter()
            .rev()
            .find(|change| change.2 == final_leader)
            .map(|&(at, old, new, game)| (name_of(new), old.map(name_of), at, game))
    });

    ThroneOutcome {
        changes: changes.len() as i32,
        held,
        crowning,
        rank0,
        rank1,
        names,
        house,
    }
}

fn throne_section(ladder: &Ladder, outcome: &ThroneOutcome) -> (Section, ThroneChange) {
    let mut section = Section::empty(SectionKind::Throne, Some(ladder.clone()));
    let reigns = outcome
        .held
        .iter()
        .map(|(_, name, secs)| format!("{name} {:.0}h", secs / 3600.0))
        .collect::<Vec<_>>()
        .join(", ");
    // No reigns and no changes means the ladder had nothing at all this
    // week (empty window) — render the canonical empty marker.
    if !reigns.is_empty() || outcome.changes > 0 {
        section.summary = if reigns.is_empty() {
            format!("#1 changed hands {} times.", outcome.changes)
        } else {
            format!(
                "#1 changed hands {} times. Hours at #1: {}.",
                outcome.changes, reigns
            )
        };
    }
    let mut crowning = None;
    if let Some((new_name, old_name, at, game_id)) = &outcome.crowning {
        let text = match old_name {
            Some(old) => format!("{new_name} took it from {old} {}", short_time(*at)),
            None => format!("{new_name} became #1 {}", short_time(*at)),
        };
        section.items.push(SectionItem {
            game_id: Some(*game_id),
            subject: Some(new_name.clone()),
            value: None,
            text,
        });
        crowning = Some(Pick {
            game_id: Some(*game_id),
            text: format!("{new_name} took #1 on {} {}", ladder.name, short_time(*at)),
        });
    }
    (
        section,
        ThroneChange {
            ladder_name: ladder.name.clone(),
            changes: outcome.changes,
            crowning,
        },
    )
}

// ---------------------------------------------------------------- movers

fn movers_section(ladder: &Ladder, games: &[&WindowGame], throne: &ThroneOutcome) -> Section {
    let mut played: BTreeMap<Uuid, i64> = BTreeMap::new();
    for game in games {
        for snake in &game.snakes {
            *played.entry(snake.entry_id).or_insert(0) += 1;
        }
    }
    // (delta, entry, rank0, rank1), delta desc.
    let mut moves: Vec<(i32, Uuid, i32, i32)> = Vec::new();
    for (entry, rank1) in &throne.rank1 {
        if let Some(rank0) = throne.rank0.get(entry)
            && played.get(entry).is_some_and(|p| *p >= 20)
            && !throne.house.get(entry).copied().unwrap_or(false)
        {
            moves.push((rank0 - rank1, *entry, *rank0, *rank1));
        }
    }
    moves.sort_by_key(|m| std::cmp::Reverse(m.0));
    let mut section = Section::empty(SectionKind::Movers, Some(ladder.clone()));
    let up: Vec<&(i32, Uuid, i32, i32)> = moves.iter().filter(|m| m.0 > 0).take(3).collect();
    let down: Vec<&(i32, Uuid, i32, i32)> = {
        let negatives: Vec<&(i32, Uuid, i32, i32)> = moves.iter().filter(|m| m.0 < 0).collect();
        let start = negatives.len().saturating_sub(2);
        negatives[start..].to_vec()
    };
    if up.is_empty() && down.is_empty() {
        return section;
    }
    section.summary = "rank at window start → end, ≥20 games in window".to_string();
    for (delta, entry, rank0, rank1) in up.iter().chain(down.iter()) {
        let name = throne
            .names
            .get(entry)
            .cloned()
            .unwrap_or_else(|| "?".into());
        section.items.push(SectionItem {
            game_id: None,
            subject: Some(name.clone()),
            value: Some(*delta as f64),
            text: format!("{name} {rank0}→{rank1}"),
        });
    }
    section
}

// ---------------------------------------------------------------- newcomers

fn newcomers_section(data: &WindowData, thrones: &[ThroneOutcome]) -> Section {
    let mut section = Section::empty(SectionKind::Newcomers, None);
    // (name, owner) -> per-(ladder, entry) stats.
    struct Stats {
        entry: Uuid,
        ladder_index: usize,
        debut: DateTime<Utc>,
        games: i64,
        wins: i64,
        first_win_at: Option<DateTime<Utc>>,
        first_win_game: Option<Uuid>,
    }
    let mut per: BTreeMap<(String, String), Vec<Stats>> = BTreeMap::new();
    for game in &data.games {
        let winner = game.unique_winner();
        for snake in &game.snakes {
            if data.start_ratings.contains_key(&snake.entry_id) || snake.house {
                continue;
            }
            let stats_list = per
                .entry((snake.snake_name.clone(), snake.owner_label().to_string()))
                .or_default();
            let stats = match stats_list
                .iter_mut()
                .find(|s| s.entry == snake.entry_id && s.ladder_index == game.ladder_index)
            {
                Some(stats) => stats,
                None => {
                    stats_list.push(Stats {
                        entry: snake.entry_id,
                        ladder_index: game.ladder_index,
                        debut: game.created_at,
                        games: 0,
                        wins: 0,
                        first_win_at: None,
                        first_win_game: None,
                    });
                    stats_list.last_mut().expect("just pushed")
                }
            };
            stats.games += 1;
            if let Some(w) = winner
                && w.entry_id == snake.entry_id
            {
                stats.wins += 1;
                if stats.first_win_game.is_none() {
                    stats.first_win_game = Some(game.game_id);
                    stats.first_win_at = Some(game.created_at);
                }
            }
        }
    }
    if per.is_empty() {
        return section;
    }
    type NewcomerRow<'a> = (DateTime<Utc>, &'a (String, String), &'a Vec<Stats>);
    let mut rows: Vec<NewcomerRow> = per
        .iter()
        .map(|(key, stats)| {
            let debut = stats.iter().map(|s| s.debut).min().expect("non-empty");
            (debut, key, stats)
        })
        .collect();
    rows.sort_by_key(|row| row.0);
    section.summary = format!("{} newcomers", rows.len());
    for (debut, (name, owner), stats) in rows {
        let mut parts: Vec<String> = Vec::new();
        for (ladder_index, throne) in thrones.iter().enumerate() {
            if let Some(stat) = stats.iter().find(|s| s.ladder_index == ladder_index) {
                let short = ladder_short(&data.ladders[ladder_index]);
                match throne.rank1.get(&stat.entry) {
                    Some(rank) => parts.push(format!(
                        "{short} {}/{} wins, now #{rank}",
                        stat.wins, stat.games
                    )),
                    None => parts.push(format!("{short} {}/{} wins", stat.wins, stat.games)),
                }
            }
        }
        let first_win = stats
            .iter()
            .filter_map(|s| s.first_win_at.map(|at| (at, s.first_win_game)))
            .min_by(|a, b| a.0.cmp(&b.0))
            .and_then(|(_, game)| game);
        section.items.push(SectionItem {
            game_id: first_win,
            subject: Some(name.clone()),
            value: None,
            text: format!(
                "**{name}** ({owner}), debuted {}: {}.",
                short_time(debut),
                parts.join("; ")
            ),
        });
    }
    section
}

// ---------------------------------------------------------------- photo finishes

/// One qualifying photo finish (candidate pass and section builder share it).
pub struct PhotoRow {
    pub final_turn: i32,
    pub game_index: usize,
    pub winner_index: usize,
    pub loser_index: usize,
}

pub fn photo_finish_rows(games: &[&WindowGame], top_n: usize) -> Vec<PhotoRow> {
    let mut rows = Vec::new();
    for (game_index, game) in games.iter().enumerate() {
        let Some(winner) = game.unique_winner() else {
            continue;
        };
        let Some(winner_frame) = winner.frame.as_ref() else {
            continue;
        };
        let Some(final_turn) = game.final_turn else {
            continue;
        };
        let Some(w_index) = winner_index(game) else {
            continue;
        };
        for (loser_index, loser) in game.snakes.iter().enumerate() {
            if loser_index == w_index {
                continue;
            }
            let Some(frame) = loser.frame.as_ref() else {
                continue;
            };
            if frame.death_cause.as_deref() == Some("head-collision")
                && frame.death_turn == Some(final_turn)
                && frame.eliminated_by == Some(winner_frame.game_battlesnake_id)
                && winner_frame.length - frame.length == 1
            {
                rows.push(PhotoRow {
                    final_turn,
                    game_index,
                    winner_index: w_index,
                    loser_index,
                });
            }
        }
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.final_turn));
    rows.truncate(top_n);
    rows
}

fn photo_finishes_section(ladder: &Ladder, games: &[&WindowGame]) -> (Section, Option<Pick>) {
    let rows = photo_finish_rows(games, 2);
    let mut section = Section::empty(SectionKind::PhotoFinishes, Some(ladder.clone()));
    let mut pick = None;
    if rows.is_empty() {
        return (section, pick);
    }
    section.summary = "last-turn head-to-head, winner longer by exactly 1".to_string();
    for row in &rows {
        let game = games[row.game_index];
        let winner = &game.snakes[row.winner_index];
        let loser = &game.snakes[row.loser_index];
        let note = if fault_on_death_turn(loser) {
            " (loser timed out that turn)"
        } else {
            ""
        };
        let text = format!(
            "**{}** ({}) over {} ({}) on turn {}{note}",
            winner.label(),
            winner.frame.as_ref().map(|f| f.length).unwrap_or(0),
            loser.label(),
            loser.frame.as_ref().map(|f| f.length).unwrap_or(0),
            row.final_turn
        );
        if pick.is_none() {
            pick = Some(Pick {
                game_id: Some(game.game_id),
                text: format!(
                    "{} photo finish on {}: {} over {}, turn {}",
                    ladder_short(ladder),
                    ladder.name,
                    winner.label(),
                    loser.label(),
                    row.final_turn
                ),
            });
        }
        section.items.push(SectionItem {
            game_id: Some(game.game_id),
            subject: Some(winner.snake_name.clone()),
            value: Some(row.final_turn as f64),
            text,
        });
    }
    (section, pick)
}

// ---------------------------------------------------------------- mutual destruction

fn mutual_destruction_section(ladder: &Ladder, games: &[&WindowGame]) -> Section {
    let mut section = Section::empty(SectionKind::MutualDestruction, Some(ladder.clone()));
    let mut best: Option<(i32, usize)> = None; // (final_turn, game_index); strictly greater wins
    for (game_index, game) in games.iter().enumerate() {
        let winners = game.winners();
        if winners.len() < 2 {
            continue;
        }
        let Some(final_turn) = game.final_turn else {
            continue;
        };
        if winners
            .iter()
            .all(|w| w.frame.as_ref().and_then(|f| f.death_turn) == Some(final_turn))
            && best.is_none_or(|(turn, _)| final_turn > turn)
        {
            best = Some((final_turn, game_index));
        }
    }
    if let Some((final_turn, game_index)) = best {
        let game = games[game_index];
        let labels: Vec<String> = game.winners().iter().map(|w| w.label()).collect();
        section.summary = "nobody survived the last turn".to_string();
        section.items.push(SectionItem {
            game_id: Some(game.game_id),
            subject: None,
            value: Some(final_turn as f64),
            text: format!(
                "{} took each other out on turn {}",
                labels.join(" & "),
                final_turn
            ),
        });
    }
    section
}

// ---------------------------------------------------------------- chicken

fn chicken_section(ladder: &Ladder, games: &[&WindowGame]) -> Section {
    let mut section = Section::empty(SectionKind::Chicken, Some(ladder.clone()));
    let duels = games.iter().filter(|g| g.snakes.len() == 2).count();
    let mut count: i64 = 0;
    let mut per: BTreeMap<String, i64> = BTreeMap::new();
    let mut pairs: BTreeMap<(String, String), i64> = BTreeMap::new();
    for game in games.iter().filter(|g| g.snakes.len() == 2) {
        let Some(final_turn) = game.final_turn else {
            continue;
        };
        if final_turn > 5 {
            continue;
        }
        let (a, b) = (&game.snakes[0], &game.snakes[1]);
        if a.placement != b.placement {
            continue;
        }
        // Exactly the prototype's rule: only the first participant's frame
        // is consulted; a missing second frame does not disqualify.
        if a.frame.as_ref().and_then(|f| f.death_cause.as_deref()) != Some("head-collision") {
            continue;
        }
        count += 1;
        *per.entry(a.label()).or_insert(0) += 1;
        *per.entry(b.label()).or_insert(0) += 1;
        let mut key = (a.label(), b.label());
        if key.0 > key.1 {
            key = (key.1, key.0);
        }
        *pairs.entry(key).or_insert(0) += 1;
    }
    if count == 0 {
        return section;
    }
    let share = if duels == 0 {
        0.0
    } else {
        count as f64 / duels as f64
    };
    let mut top_snakes: Vec<(&String, &i64)> = per.iter().collect();
    top_snakes.sort_by(|a, b| b.1.cmp(a.1));
    let mut top_pairs: Vec<(&(String, String), &i64)> = pairs.iter().collect();
    top_pairs.sort_by(|a, b| b.1.cmp(a.1));
    section.summary = format!(
        "{count} this week ({} of duels). Most frequent: {}. Pairs: {}.",
        pct(share),
        top_snakes
            .iter()
            .take(4)
            .map(|(name, c)| format!("{name} {c}"))
            .collect::<Vec<_>>()
            .join(", "),
        top_pairs
            .iter()
            .take(3)
            .map(|((a, b), c)| format!("{a} vs {b} {c}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    for (name, c) in top_snakes.iter().take(4) {
        section.items.push(SectionItem {
            game_id: None,
            subject: Some((*name).clone()),
            value: Some(**c as f64),
            text: format!("{name} {c}"),
        });
    }
    for ((a, b), c) in top_pairs.iter().take(3) {
        section.items.push(SectionItem {
            game_id: None,
            subject: Some(format!("{a} vs {b}")),
            value: Some(**c as f64),
            text: format!("{a} vs {b} {c}"),
        });
    }
    section
}

// ---------------------------------------------------------------- fumes

fn fumes_section(ladder: &Ladder, games: &[&WindowGame]) -> Section {
    let mut section = Section::empty(SectionKind::Fumes, Some(ladder.clone()));
    // (health asc, final_turn desc), like the prototype's sort key.
    let mut rows: Vec<(i32, i32, usize, usize)> = Vec::new();
    for (game_index, game) in games.iter().enumerate() {
        let Some(winner) = game.unique_winner() else {
            continue;
        };
        let Some(frame) = winner.frame.as_ref() else {
            continue;
        };
        if frame.death_cause.is_some() {
            continue;
        }
        let Some(final_turn) = game.final_turn else {
            continue;
        };
        if frame.health <= 3 && final_turn >= 50 {
            let Some(w_index) = winner_index(game) else {
                continue;
            };
            rows.push((frame.health, final_turn, game_index, w_index));
        }
    }
    rows.sort_by(|a, b| (a.0, b.1).cmp(&(b.0, a.1)));
    if rows.is_empty() {
        return section;
    }
    section.summary = "winner finished at ≤3 health in a game of ≥50 turns".to_string();
    for (health, final_turn, game_index, w_index) in rows.iter().take(2) {
        let game = games[*game_index];
        let winner = &game.snakes[*w_index];
        section.items.push(SectionItem {
            game_id: Some(game.game_id),
            subject: Some(winner.snake_name.clone()),
            value: Some(*health as f64),
            text: format!(
                "**{}** survived turn {final_turn} with {health} health",
                winner.label()
            ),
        });
    }
    section
}

// ---------------------------------------------------------------- hat tricks

fn hat_tricks_section(ladder: &Ladder, games: &[&WindowGame]) -> Section {
    let mut section = Section::empty(SectionKind::HatTricks, Some(ladder.clone()));
    let mut per: BTreeMap<String, i64> = BTreeMap::new();
    let mut longest: Option<(i32, Uuid, String)> = None;
    for game in games.iter() {
        // Prototype gate: 4 participants AND the winner has a frame.
        if game.snakes.len() != 4 {
            continue;
        }
        let Some(winner) = game.unique_winner() else {
            continue;
        };
        let Some(winner_frame) = winner.frame.as_ref() else {
            continue;
        };
        let victims: Vec<&WindowSnake> = game
            .snakes
            .iter()
            .filter(|s| s.entry_id != winner.entry_id)
            .collect();
        if victims.iter().any(|v| v.house) {
            continue;
        }
        if !victims.iter().all(|v| {
            v.frame
                .as_ref()
                .and_then(|f| f.eliminated_by)
                .is_some_and(|by| by == winner_frame.game_battlesnake_id)
        }) {
            continue;
        }
        *per.entry(winner.label()).or_insert(0) += 1;
        if !winner.house {
            let turns = game.final_turn.unwrap_or(0);
            if longest.as_ref().is_none_or(|(t, _, _)| turns > *t) {
                longest = Some((turns, game.game_id, winner.label()));
            }
        }
    }
    if per.is_empty() {
        return section;
    }
    let mut top: Vec<(&String, &i64)> = per.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    section.summary = format!(
        "4-snake wins where the winner eliminated all three: {}",
        top.iter()
            .take(3)
            .map(|(name, c)| format!("{name} {c}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (name, c) in top.iter().take(3) {
        section.items.push(SectionItem {
            game_id: None,
            subject: Some((*name).clone()),
            value: Some(**c as f64),
            text: format!("{name} {c}"),
        });
    }
    if let Some((turns, game_id, label)) = longest {
        section.items.push(SectionItem {
            game_id: Some(game_id),
            subject: Some(label.clone()),
            value: Some(turns as f64),
            text: format!("Longest: {label}, {turns} turns"),
        });
    }
    section
}

// ---------------------------------------------------------------- marathons

fn marathons_section(ladder: &Ladder, games: &[&WindowGame]) -> Section {
    let mut section = Section::empty(SectionKind::Marathons, Some(ladder.clone()));
    // Strictly greater wins, so the first max-turn game (in window order)
    // is kept, matching Python's stable desc sort.
    let mut best: Option<(i32, usize)> = None;
    for (game_index, game) in games.iter().enumerate() {
        if let Some(final_turn) = game.final_turn
            && best.is_none_or(|(turn, _)| final_turn > turn)
        {
            best = Some((final_turn, game_index));
        }
    }
    if let Some((final_turn, game_index)) = best {
        let game = games[game_index];
        let winners: Vec<String> = game.winners().iter().map(|w| w.label()).collect();
        let also_ran: Vec<String> = game
            .snakes
            .iter()
            .filter(|s| s.placement != 1)
            .map(|s| s.label())
            .collect();
        section.summary = "the longest game this week".to_string();
        section.items.push(SectionItem {
            game_id: Some(game.game_id),
            subject: None,
            value: Some(final_turn as f64),
            text: format!(
                "{final_turn} turns, won by {}{}",
                winners.join(" & "),
                if also_ran.is_empty() {
                    String::new()
                } else {
                    format!(" ({} also ran)", also_ran.join(", "))
                }
            ),
        });
    }
    section
}

// ---------------------------------------------------------------- streaks

fn streaks_section(ladder: &Ladder, games: &[&WindowGame], throne: &ThroneOutcome) -> Section {
    let mut streak: BTreeMap<Uuid, i32> = BTreeMap::new();
    let mut beaten_mu: BTreeMap<Uuid, Vec<f64>> = BTreeMap::new();
    let mut beaten_label: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
    // entry -> (length, through, best scalp label); first run of a given
    // max length wins (strict >, like the prototype).
    let mut best: BTreeMap<Uuid, (i32, DateTime<Utc>, String)> = BTreeMap::new();
    for game in games {
        let winner = game.unique_winner();
        for snake in &game.snakes {
            match winner.filter(|w| w.entry_id == snake.entry_id) {
                Some(_) => {
                    *streak.entry(snake.entry_id).or_insert(0) += 1;
                    let mus = beaten_mu.entry(snake.entry_id).or_default();
                    let labels = beaten_label.entry(snake.entry_id).or_default();
                    for other in &game.snakes {
                        if other.entry_id != snake.entry_id {
                            mus.push(other.mu_before);
                            labels.push(other.label());
                        }
                    }
                    let length = streak[&snake.entry_id];
                    if length > best.get(&snake.entry_id).map_or(0, |b| b.0) {
                        let scalp = mus
                            .iter()
                            .zip(labels.iter())
                            .max_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap())
                            .map(|(_, label)| (*label).clone())
                            .unwrap_or_else(|| "?".to_string());
                        best.insert(snake.entry_id, (length, game.created_at, scalp));
                    }
                }
                None => {
                    streak.insert(snake.entry_id, 0);
                    beaten_mu.insert(snake.entry_id, Vec::new());
                    beaten_label.insert(snake.entry_id, Vec::new());
                }
            }
        }
    }
    let mut section = Section::empty(SectionKind::Streaks, Some(ladder.clone()));
    let mut rows: Vec<(i32, DateTime<Utc>, String, Uuid)> = best
        .into_iter()
        .filter(|(entry, _)| !throne.house.get(entry).copied().unwrap_or(false))
        .map(|(entry, run)| (run.0, run.1, run.2, entry))
        .collect();
    // (length, through, scalp) desc, like Python's reverse tuple sort.
    rows.sort_by_key(|row| std::cmp::Reverse((row.0, row.1, row.2.clone())));
    rows.truncate(2);
    if rows.is_empty() {
        return section;
    }
    section.summary = "longest run of outright wins in the window".to_string();
    for (length, through, scalp, entry) in rows {
        let name = throne
            .names
            .get(&entry)
            .cloned()
            .unwrap_or_else(|| "?".into());
        section.items.push(SectionItem {
            game_id: None,
            subject: Some(name.clone()),
            value: Some(length as f64),
            text: format!(
                "{name} {length} straight, best scalp {scalp} (through {})",
                short_time(through)
            ),
        });
    }
    section
}

// ---------------------------------------------------------------- rivalries

fn rivalries_section(ladder: &Ladder, games: &[&WindowGame], throne: &ThroneOutcome) -> Section {
    let mut section = Section::empty(SectionKind::Rivalries, Some(ladder.clone()));
    let top8: BTreeSet<Uuid> = throne
        .rank1
        .iter()
        .filter(|(_, rank)| **rank <= 8)
        .map(|(entry, _)| *entry)
        .collect();
    // (a, b) keyed by entry id; [a wins, b wins, draws].
    let mut h2h: BTreeMap<(Uuid, Uuid), [i64; 3]> = BTreeMap::new();
    for game in games.iter().filter(|g| g.snakes.len() == 2) {
        let (x, y) = (&game.snakes[0], &game.snakes[1]);
        let (a, b) = if x.entry_id < y.entry_id {
            (x, y)
        } else {
            (y, x)
        };
        if !top8.contains(&a.entry_id) || !top8.contains(&b.entry_id) {
            continue;
        }
        let row = h2h.entry((a.entry_id, b.entry_id)).or_insert([0, 0, 0]);
        if a.placement == b.placement {
            row[2] += 1;
        } else if a.placement == 1 {
            row[0] += 1;
        } else {
            row[1] += 1;
        }
    }
    let name = |entry: &Uuid| -> String {
        throne
            .names
            .get(entry)
            .cloned()
            .unwrap_or_else(|| "?".into())
    };
    // (closeness asc, -n, a, b) — closeness |wa-wb|/n, tie → more games,
    // then entry id.
    let mut rows: Vec<(f64, i64, Uuid, Uuid, i64, i64, i64)> = Vec::new();
    for ((a, b), [wa, wb, d]) in &h2h {
        let n = wa + wb + d;
        if n >= 10 {
            rows.push(((wa - wb).abs() as f64 / n as f64, -n, *a, *b, *wa, *wb, *d));
        }
    }
    rows.sort_by(|x, y| {
        x.0.partial_cmp(&y.0)
            .unwrap()
            .then_with(|| x.1.cmp(&y.1))
            .then_with(|| x.2.cmp(&y.2))
            .then_with(|| x.3.cmp(&y.3))
    });
    // Kryptonite: lower-ranked snake with a winning record over a
    // higher-ranked one, ≥30 decided games; top 3 by win share.
    let mut kryptonite: Vec<(f64, Uuid, Uuid, i64, i64)> = Vec::new();
    for ((a, b), [wa, wb, _d]) in &h2h {
        let rank_a = throne.rank1.get(a).copied().unwrap_or(i32::MAX);
        let rank_b = throne.rank1.get(b).copied().unwrap_or(i32::MAX);
        let (lo, hi, wl, wh) = if rank_a > rank_b {
            (*a, *b, *wa, *wb)
        } else {
            (*b, *a, *wb, *wa)
        };
        if wl + wh >= 30 && wl > wh {
            kryptonite.push((wl as f64 / (wl + wh) as f64, lo, hi, wl, wh));
        }
    }
    kryptonite.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap()
            .then_with(|| x.1.cmp(&y.1))
            .then_with(|| x.2.cmp(&y.2))
    });
    if rows.is_empty() && kryptonite.is_empty() {
        return section;
    }
    section.summary = "Duels head-to-heads among the current top 8".to_string();
    for (_, _, a, b, wa, wb, d) in rows.iter().take(3) {
        section.items.push(SectionItem {
            game_id: None,
            subject: Some(format!("{} vs {}", name(a), name(b))),
            value: None,
            text: format!("{} {}–{} {} ({} draws)", name(a), wa, wb, name(b), d),
        });
    }
    let mut lopsided = rows.clone();
    lopsided.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap()
            .then_with(|| x.1.cmp(&y.1))
            .then_with(|| x.2.cmp(&y.2))
    });
    for (_, _, a, b, wa, wb, d) in lopsided.iter().take(3) {
        let (hi, lo, h, l) = if wa >= wb {
            (a, b, wa, wb)
        } else {
            (b, a, wb, wa)
        };
        section.items.push(SectionItem {
            game_id: None,
            subject: Some(format!("{} vs {}", name(hi), name(lo))),
            value: None,
            text: format!("{} {}–{} {} ({} draws)", name(hi), h, l, name(lo), d),
        });
    }
    for (_, lo, hi, wl, wh) in kryptonite.iter().take(3) {
        section.items.push(SectionItem {
            game_id: None,
            subject: Some(format!("{} over {}", name(lo), name(hi))),
            value: Some(*wl as f64),
            text: format!(
                "{} (#{}) {}–{} {} (#{})",
                name(lo),
                throne.rank1.get(lo).copied().unwrap_or(0),
                wl,
                wh,
                name(hi),
                throne.rank1.get(hi).copied().unwrap_or(0),
            ),
        });
    }
    section
}

// ---------------------------------------------------------------- ops footnote

fn ops_footnote_section(ladder: &Ladder, games: &[&WindowGame]) -> Section {
    let mut section = Section::empty(SectionKind::OpsFootnote, Some(ladder.clone()));
    let mut per: BTreeMap<String, i64> = BTreeMap::new();
    for game in games.iter().filter(|g| g.snakes.len() == 2) {
        for snake in &game.snakes {
            if let Some(frame) = snake.frame.as_ref()
                && frame.death_cause.as_deref() == Some("wall-collision")
                && frame.death_turn.is_some_and(|t| t <= 5)
            {
                *per.entry(snake.label()).or_insert(0) += 1;
            }
        }
    }
    if per.is_empty() {
        return section;
    }
    let total: i64 = per.values().sum();
    let mut top: Vec<(&String, &i64)> = per.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    section.summary = format!(
        "duel deaths by wall-collision on turns 1-5: {total} this week. Most: {}.",
        top.iter()
            .take(6)
            .map(|(name, c)| format!("{name} {c}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (name, c) in top.iter().take(6) {
        section.items.push(SectionItem {
            game_id: None,
            subject: Some((*name).clone()),
            value: Some(**c as f64),
            text: format!("{name} {c}"),
        });
    }
    section
}

// ---------------------------------------------------------------- calibration

fn calibration_section(duel_k: f64, four_snake_k: f64) -> Section {
    let mut section = Section::empty(SectionKind::Calibration, None);
    section.summary = format!(
        "Upset odds are P(first place) from each snake's pre-game rating \
         (Weng-Lin mu/sigma, beta {BETA:.3}, Plackett-Luce), sharpened by a \
         factor k fit to this week's results: k = {duel_k:.2} for duels, \
         {four_snake_k:.2} for 4-snake games."
    );
    section.items.push(SectionItem {
        game_id: None,
        subject: Some("duels".to_string()),
        value: Some(duel_k),
        text: format!("Fitted k for duels: {duel_k:.2}"),
    });
    section.items.push(SectionItem {
        game_id: None,
        subject: Some("4-snake".to_string()),
        value: Some(four_snake_k),
        text: format!("Fitted k for 4-snake games: {four_snake_k:.2}"),
    });
    section
}

// ---------------------------------------------------------------- assembly

/// Games whose story depends on whether someone timed out (prototype
/// `--candidates`): upsets top 40 per ladder + photo finishes top 4 per
/// ladder, unioned across ladders.
pub fn candidate_game_ids(data: &WindowData, duel_k: f64, four_snake_k: f64) -> BTreeSet<Uuid> {
    let mut ids = BTreeSet::new();
    for (ladder_index, ladder) in data.ladders.iter().enumerate() {
        let games = data.games_on_ladder(ladder_index);
        let k = if ladder.is_duels() {
            duel_k
        } else {
            four_snake_k
        };
        for row in upset_rows(&games, k, 40) {
            ids.insert(games[row.game_index].game_id);
        }
        for row in photo_finish_rows(&games, 4) {
            ids.insert(games[row.game_index].game_id);
        }
    }
    ids
}

/// Run every detector over the window and assemble the canonical section
/// set (display order) plus the Discord picks. Pure. `DuelsOnly` kinds are
/// omitted (with a warn) when no public ladder has `match_size == 2`.
pub fn compute_sections(
    data: &WindowData,
    w0: DateTime<Utc>,
    w1: DateTime<Utc>,
    duel_k: f64,
    four_snake_k: f64,
) -> (Vec<Section>, DiscordPicks) {
    let ladder_games: Vec<Vec<&WindowGame>> = (0..data.ladders.len())
        .map(|index| data.games_on_ladder(index))
        .collect();
    let thrones: Vec<ThroneOutcome> = (0..data.ladders.len())
        .map(|index| replay_ladder(data, index, w0, w1))
        .collect();
    let duels_index = data.duels_ladder_index();
    if duels_index.is_none() {
        tracing::warn!("no public ladder with match_size = 2; skipping Duels-only sections");
    }

    let mut picks = DiscordPicks::default();
    let mut top_upset: Option<(Pick, f64)> = None;
    let mut sections: Vec<Section> = Vec::new();

    for kind in SECTION_DISPLAY_ORDER {
        let per_kind: Vec<Section> = match kind.scope() {
            SectionScope::PerLadder => data
                .ladders
                .iter()
                .enumerate()
                .map(|(index, ladder)| {
                    let games = &ladder_games[index];
                    match kind {
                        SectionKind::Throne => {
                            let (section, change) = throne_section(ladder, &thrones[index]);
                            if change.changes > 0 {
                                picks.throne_changes.push(change);
                            }
                            section
                        }
                        SectionKind::Movers => movers_section(ladder, games, &thrones[index]),
                        SectionKind::Upsets => {
                            let k = if ladder.is_duels() {
                                duel_k
                            } else {
                                four_snake_k
                            };
                            let (section, pick) = upsets_section(ladder, games, k);
                            // Keep the lowest calibrated p across ladders.
                            if let Some((_, p)) = &pick
                                && top_upset.as_ref().is_none_or(|(_, best)| *p < *best)
                            {
                                top_upset = pick;
                            }
                            section
                        }
                        SectionKind::PhotoFinishes => {
                            let (section, pick) = photo_finishes_section(ladder, games);
                            if ladder.is_duels() {
                                picks.duel_photo_finish = pick;
                            }
                            section
                        }
                        SectionKind::MutualDestruction => mutual_destruction_section(ladder, games),
                        SectionKind::Fumes => fumes_section(ladder, games),
                        SectionKind::Marathons => marathons_section(ladder, games),
                        SectionKind::Streaks => streaks_section(ladder, games, &thrones[index]),
                        _ => unreachable!("scope mismatch"),
                    }
                })
                .collect(),
            SectionScope::PerLadderNonDuel => data
                .ladders
                .iter()
                .enumerate()
                .filter(|(_, ladder)| !ladder.is_duels())
                .map(|(index, ladder)| hat_tricks_section(ladder, &ladder_games[index]))
                .collect(),
            SectionScope::DuelsOnly => match duels_index {
                Some(index) => {
                    let ladder = &data.ladders[index];
                    let games = &ladder_games[index];
                    vec![match kind {
                        SectionKind::Chicken => chicken_section(ladder, games),
                        SectionKind::Rivalries => rivalries_section(ladder, games, &thrones[index]),
                        SectionKind::OpsFootnote => ops_footnote_section(ladder, games),
                        _ => unreachable!("scope mismatch"),
                    }]
                }
                None => Vec::new(),
            },
            SectionScope::CrossLadder => vec![match kind {
                SectionKind::Newcomers => newcomers_section(data, &thrones),
                SectionKind::Calibration => calibration_section(duel_k, four_snake_k),
                _ => unreachable!("scope mismatch"),
            }],
        };
        sections.extend(per_kind);
    }
    picks.top_upset = top_upset.map(|(pick, _)| pick);
    (sections, picks)
}

/// Assemble the final [`ComputedDigest`] (counts, sections, picks) from
/// pre-fitted calibration k's (fit once in phase 1; refitting here would
/// double the grid cost over the window's games).
pub fn assemble_digest(
    data: &WindowData,
    w0: DateTime<Utc>,
    w1: DateTime<Utc>,
    duel_k: f64,
    four_snake_k: f64,
) -> ComputedDigest {
    let (sections, picks) = compute_sections(data, w0, w1, duel_k, four_snake_k);
    let per_ladder_counts = data
        .ladders
        .iter()
        .enumerate()
        .map(|(index, ladder)| {
            (
                ladder.name.clone(),
                data.games
                    .iter()
                    .filter(|g| g.ladder_index == index)
                    .count() as i64,
            )
        })
        .collect();
    ComputedDigest {
        window_start: w0,
        window_end: w1,
        games_count: data.games.len() as i64,
        games_without_frames: data.games.iter().filter(|g| g.final_turn.is_none()).count() as i64,
        per_ladder_counts,
        duel_k,
        four_snake_k,
        sections,
        picks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlights::types::{
        EntryInfo, FinalFrame, Ladder, WindowData, WindowGame, WindowSnake,
    };
    use chrono::TimeZone;

    // ------------------------------------------------------------ fixtures

    /// Deterministic UUID from a small integer (fixtures only).
    fn uuid(seed: u8) -> Uuid {
        Uuid::from_u64_pair(seed as u64, 0)
    }

    fn ladder(name: &str, match_size: i32, seed: u8) -> Ladder {
        Ladder {
            leaderboard_id: uuid(seed),
            name: name.to_string(),
            match_size,
        }
    }

    fn duels_ladder() -> Ladder {
        ladder("Duels 11x11", 2, 1)
    }

    #[allow(clippy::too_many_arguments)]
    fn snake(entry: u8, name: &str, placement: i32, mu: f64) -> WindowSnake {
        WindowSnake {
            entry_id: uuid(entry),
            snake_name: name.to_string(),
            owner_login: Some("someone".to_string()),
            house: false,
            placement,
            mu_before: mu,
            sigma_before: 25.0 / 6.0,
            mu_after: mu + 0.5,
            sigma_after: 25.0 / 6.0,
            frame: None,
        }
    }

    fn frame_for(gbid_seed: u8, length: i32, health: i32) -> FinalFrame {
        FinalFrame {
            game_battlesnake_id: uuid(gbid_seed),
            length,
            health,
            death_cause: None,
            death_turn: None,
            eliminated_by: None,
            fault_turns: BTreeSet::new(),
        }
    }

    fn game(ladder_index: usize, hour: u32, snakes: Vec<WindowSnake>) -> WindowGame {
        let created = Utc.with_ymd_and_hms(2026, 10, 26, hour, 0, 0).unwrap();
        WindowGame {
            game_id: Uuid::new_v4(),
            ladder_index,
            created_at: created,
            snakes,
            final_turn: None,
        }
    }

    /// Attach head-collision deaths: every snake with `Some((turn, by_seed))`.
    fn die_head(snake: &mut WindowSnake, turn: i32, by_seed: u8) {
        let gbid = snake
            .frame
            .as_ref()
            .map(|f| f.game_battlesnake_id)
            .unwrap_or_else(|| uuid(snake.entry_id.as_u64_pair().0 as u8));
        snake.frame = Some(FinalFrame {
            game_battlesnake_id: gbid,
            death_cause: Some("head-collision".to_string()),
            death_turn: Some(turn),
            eliminated_by: Some(uuid(by_seed)),
            ..snake
                .frame
                .clone()
                .unwrap_or_else(|| frame_for(snake.entry_id.as_u64_pair().0 as u8, 10, 50))
        });
    }

    fn fixture_data(ladders: Vec<Ladder>, games: Vec<WindowGame>) -> WindowData {
        WindowData {
            ladders,
            games,
            entries: BTreeMap::new(),
            start_ratings: BTreeMap::new(),
        }
    }

    fn w0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap()
    }

    fn w1() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 31, 12, 0, 0).unwrap()
    }

    // ------------------------------------------------------------ fit_k

    #[test]
    fn fit_k_tie_rule_picks_highest_k() {
        // Duplicated max ll: the argmax must resolve to the highest k.
        let series = vec![(1.0, 0.5), (2.0, 1.0), (2.0, 2.0), (1.5, 4.0)];
        assert_eq!(argmax_ties_highest(&series), 2.0);
    }

    #[test]
    fn fit_k_stronger_always_wins_sharpens() {
        // A snake with a large mu edge that always wins → k > 1.
        let mut games = Vec::new();
        for hour in 0..8 {
            let mut strong = snake(1, "Strong", 1, 35.0);
            let weak = snake(2, "Weak", 2, 15.0);
            strong.frame = Some(frame_for(1, 12, 50));
            games.push(game(0, hour, vec![strong, weak]));
        }
        let refs: Vec<&WindowGame> = games.iter().collect();
        assert!(fit_k(&refs, true) > 1.0);
    }

    #[test]
    fn fit_k_uniform_mu_near_one() {
        let mut games = Vec::new();
        for hour in 0..8 {
            let a = snake(1, "A", 1, 25.0);
            let b = snake(2, "B", 2, 25.0);
            games.push(game(0, hour, vec![a, b]));
        }
        let refs: Vec<&WindowGame> = games.iter().collect();
        // With identical mus every k gives the same ll (0.5 ln each... all
        // probs are 1/2 regardless of k) → all ll equal → highest k wins.
        assert_eq!(fit_k(&refs, true), 4.0);
    }

    #[test]
    fn fit_k_grid_boundaries() {
        assert_eq!((50..=400).step_by(2).next(), Some(50));
        assert_eq!((50..=400).step_by(2).last(), Some(400));
    }

    #[test]
    fn fit_k_empty_window_returns_highest_grid_k() {
        assert_eq!(fit_k(&[], true), 4.0);
    }

    // ------------------------------------------------------------ win_probs

    #[test]
    fn win_probs_sum_to_one_and_favor_higher_mu() {
        let a = snake(1, "A", 1, 30.0);
        let b = snake(2, "B", 2, 20.0);
        let g = game(0, 0, vec![a, b]);
        let probs = g.win_probs(1.0);
        let sum: f64 = probs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9);
        assert!(probs[0] > probs[1]);
    }

    #[test]
    fn win_probs_sharpening_lowers_long_shot() {
        let a = snake(1, "A", 1, 30.0);
        let b = snake(2, "B", 2, 20.0);
        let g = game(0, 0, vec![a, b]);
        assert!(g.win_probs(2.0)[1] < g.win_probs(1.0)[1]);
    }

    // ------------------------------------------------------------ upsets

    fn upset_fixture() -> Vec<WindowGame> {
        let mut winner = snake(1, "Underdog", 1, 10.0);
        let mut loser = snake(2, "Favourite", 2, 40.0);
        winner.frame = Some(frame_for(1, 11, 50));
        let mut g = game(0, 0, vec![winner, loser.clone()]);
        g.final_turn = Some(20);
        die_head(&mut loser, 20, 1);
        g.snakes[1] = loser;
        vec![g]
    }

    #[test]
    fn upset_detected_without_faults() {
        let games = upset_fixture();
        let refs: Vec<&WindowGame> = games.iter().collect();
        let rows = upset_rows(&refs, 1.0, 3);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].calibrated_p < 0.5);
    }

    #[test]
    fn upset_excluded_when_loser_faulted_near_death() {
        for offset in [0, -1, -2] {
            let mut games = upset_fixture();
            let frame = games[0].snakes[1].frame.as_mut().unwrap();
            frame.fault_turns.insert(frame.death_turn.unwrap() + offset);
            let refs: Vec<&WindowGame> = games.iter().collect();
            assert!(
                upset_rows(&refs, 1.0, 3).is_empty(),
                "fault at death_turn {offset:+} must exclude"
            );
        }
    }

    #[test]
    fn upset_kept_when_fault_three_turns_before_death() {
        let mut games = upset_fixture();
        let frame = games[0].snakes[1].frame.as_mut().unwrap();
        frame.fault_turns.insert(frame.death_turn.unwrap() - 3);
        let refs: Vec<&WindowGame> = games.iter().collect();
        assert_eq!(upset_rows(&refs, 1.0, 3).len(), 1);
    }

    #[test]
    fn upset_loser_without_death_turn_has_no_fault_filter() {
        let mut games = upset_fixture();
        let frame = games[0].snakes[1].frame.as_mut().unwrap();
        frame.death_turn = None;
        frame.fault_turns.insert(5); // cannot be compared → ignored
        let refs: Vec<&WindowGame> = games.iter().collect();
        assert_eq!(upset_rows(&refs, 1.0, 3).len(), 1);
    }

    #[test]
    fn upset_favourite_house_excluded() {
        let mut games = upset_fixture();
        games[0].snakes[1].house = true; // favourite is house
        let refs: Vec<&WindowGame> = games.iter().collect();
        assert!(upset_rows(&refs, 1.0, 3).is_empty());
    }

    #[test]
    fn upset_winner_house_excluded() {
        let mut games = upset_fixture();
        games[0].snakes[0].house = true;
        let refs: Vec<&WindowGame> = games.iter().collect();
        assert!(upset_rows(&refs, 1.0, 3).is_empty());
    }

    #[test]
    fn upset_favourite_is_winner_excluded() {
        let winner = snake(1, "Top", 1, 40.0);
        let loser = snake(2, "Bottom", 2, 10.0);
        let g = game(0, 0, vec![winner, loser]);
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        assert!(upset_rows(&refs, 1.0, 3).is_empty());
    }

    // ------------------------------------------------------------ shared first place

    #[test]
    fn shared_first_place_is_not_a_unique_winner() {
        let a = snake(1, "A", 1, 30.0);
        let b = snake(2, "B", 1, 30.0);
        let g = game(0, 0, vec![a.clone(), b.clone()]);
        assert!(g.unique_winner().is_none());
        // Excluded from upsets and fumes; eligible for mutual destruction.
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        assert!(upset_rows(&refs, 1.0, 3).is_empty());
        let section = fumes_section(&duels_ladder(), &refs);
        assert!(section.items.is_empty());
    }

    #[test]
    fn mutual_destruction_shared_first_both_die_final_turn() {
        let mut a = snake(1, "A", 1, 30.0);
        let mut b = snake(2, "B", 1, 30.0);
        a.frame = Some(frame_for(1, 10, 0));
        b.frame = Some(frame_for(2, 12, 0));
        die_head(&mut a, 42, 2);
        die_head(&mut b, 42, 1);
        let mut g = game(0, 0, vec![a, b]);
        g.final_turn = Some(42);
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        let section = mutual_destruction_section(&duels_ladder(), &refs);
        assert_eq!(section.items.len(), 1);
        assert_eq!(section.items[0].value, Some(42.0));
    }

    #[test]
    fn mutual_destruction_negative_earlier_death_turn() {
        let mut a = snake(1, "A", 1, 30.0);
        let mut b = snake(2, "B", 1, 30.0);
        a.frame = Some(frame_for(1, 10, 0));
        b.frame = Some(frame_for(2, 12, 0));
        die_head(&mut a, 40, 2); // died before the final turn
        die_head(&mut b, 42, 1);
        let mut g = game(0, 0, vec![a, b]);
        g.final_turn = Some(42);
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        let section = mutual_destruction_section(&duels_ladder(), &refs);
        assert!(section.items.is_empty());
    }

    // ------------------------------------------------------------ photo finishes

    #[test]
    fn photo_finish_detected() {
        let mut winner = snake(1, "Long", 1, 30.0);
        let mut loser = snake(2, "Short", 2, 28.0);
        winner.frame = Some(frame_for(1, 53, 40));
        loser.frame = Some(frame_for(2, 52, 0));
        die_head(&mut loser, 778, 1);
        let mut g = game(0, 0, vec![winner, loser]);
        g.final_turn = Some(778);
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        let rows = photo_finish_rows(&refs, 2);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn photo_finish_length_diff_two_not_detected() {
        let mut winner = snake(1, "Long", 1, 30.0);
        let mut loser = snake(2, "Short", 2, 28.0);
        winner.frame = Some(frame_for(1, 54, 40));
        loser.frame = Some(frame_for(2, 52, 0));
        die_head(&mut loser, 778, 1);
        let mut g = game(0, 0, vec![winner, loser]);
        g.final_turn = Some(778);
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        assert!(photo_finish_rows(&refs, 2).is_empty());
    }

    #[test]
    fn photo_finish_earlier_death_not_detected() {
        let mut winner = snake(1, "Long", 1, 30.0);
        let mut loser = snake(2, "Short", 2, 28.0);
        winner.frame = Some(frame_for(1, 53, 40));
        loser.frame = Some(frame_for(2, 52, 0));
        die_head(&mut loser, 700, 1); // died before the final turn
        let mut g = game(0, 0, vec![winner, loser]);
        g.final_turn = Some(778);
        let refs: Vec<&WindowGame> = [&g].into_iter().collect();
        assert!(photo_finish_rows(&refs, 2).is_empty());
    }

    // ------------------------------------------------------------ chicken

    fn chicken_game(turn: i32, placements_equal: bool, cause: Option<&str>) -> WindowGame {
        let mut a = snake(1, "A", 1, 25.0);
        let mut b = snake(2, "B", if placements_equal { 1 } else { 2 }, 25.0);
        a.frame = Some(frame_for(1, 5, 30));
        b.frame = Some(frame_for(2, 5, 30));
        match cause {
            Some("head") => die_head(&mut a, turn, 2),
            Some("wall") => {
                let f = a.frame.as_mut().unwrap();
                f.death_cause = Some("wall-collision".to_string());
                f.death_turn = Some(turn);
            }
            _ => {}
        }
        let mut g = game(0, 0, vec![a, b]);
        g.final_turn = Some(turn);
        g
    }

    #[test]
    fn chicken_boundary_turn_5_in_6_out() {
        let data = fixture_data(
            vec![duels_ladder()],
            vec![chicken_game(5, true, Some("head"))],
        );
        let refs = data.games_on_ladder(0);
        let section = chicken_section(&data.ladders[0], &refs);
        assert!(section.summary.starts_with("1 this week"));

        let data = fixture_data(
            vec![duels_ladder()],
            vec![chicken_game(6, true, Some("head"))],
        );
        let refs = data.games_on_ladder(0);
        let section = chicken_section(&data.ladders[0], &refs);
        assert!(section.is_empty());
    }

    #[test]
    fn chicken_only_first_participant_frame_consulted() {
        // Equal placements, snakes[0] head-collision, snakes[1] NO frame.
        let mut a = snake(1, "A", 1, 25.0);
        let b = snake(2, "B", 1, 25.0);
        a.frame = Some(frame_for(1, 5, 30));
        die_head(&mut a, 4, 2);
        let mut g = game(0, 0, vec![a, b]);
        g.final_turn = Some(4);
        let data = fixture_data(vec![duels_ladder()], vec![g]);
        let refs = data.games_on_ladder(0);
        let section = chicken_section(&data.ladders[0], &refs);
        assert!(section.summary.starts_with("1 this week"));
    }

    #[test]
    fn chicken_wall_death_out() {
        let data = fixture_data(
            vec![duels_ladder()],
            vec![chicken_game(4, true, Some("wall"))],
        );
        let refs = data.games_on_ladder(0);
        let section = chicken_section(&data.ladders[0], &refs);
        assert!(section.is_empty());
    }

    #[test]
    fn chicken_share_zero_when_no_duels() {
        let data = fixture_data(vec![duels_ladder()], vec![]);
        let refs = data.games_on_ladder(0);
        let section = chicken_section(&data.ladders[0], &refs);
        assert!(section.is_empty()); // no divide-by-zero
    }

    // ------------------------------------------------------------ fumes

    fn fumes_game(health: i32, turns: i32, dead: bool) -> WindowGame {
        let mut winner = snake(1, "Winner", 1, 30.0);
        let loser = snake(2, "Loser", 2, 28.0);
        winner.frame = Some(frame_for(1, 30, health));
        if dead {
            let f = winner.frame.as_mut().unwrap();
            f.death_cause = Some("self-collision".to_string());
        }
        let mut g = game(0, 0, vec![winner, loser]);
        g.final_turn = Some(turns);
        g
    }

    #[test]
    fn fumes_in_and_out() {
        for (health, turns, dead, expect) in [
            (3, 50, false, true),
            (1, 100, false, true),
            (4, 50, false, false),
            (3, 49, false, false),
            (3, 50, true, false),
        ] {
            let data = fixture_data(vec![duels_ladder()], vec![fumes_game(health, turns, dead)]);
            let refs = data.games_on_ladder(0);
            let section = fumes_section(&data.ladders[0], &refs);
            assert_eq!(
                !section.items.is_empty(),
                expect,
                "health={health} turns={turns} dead={dead}"
            );
        }
    }

    // ------------------------------------------------------------ hat tricks

    fn hat_trick_game(
        victim_killer: bool,
        house_victim: bool,
        four: bool,
        winner_frame: bool,
    ) -> WindowGame {
        let mut winner = snake(1, "Winner", 1, 30.0);
        let mk = |seed: u8, name: &str, house: bool| {
            let mut v = snake(seed, name, 2, 25.0);
            v.house = house;
            v.frame = Some(FinalFrame {
                game_battlesnake_id: uuid(seed),
                length: 8,
                health: 40,
                death_cause: Some("snake-collision".to_string()),
                death_turn: Some(30),
                eliminated_by: victim_killer.then_some(uuid(1)),
                fault_turns: BTreeSet::new(),
            });
            v
        };
        if winner_frame {
            winner.frame = Some(frame_for(1, 40, 90));
        }
        let victims = vec![
            mk(2, "V1", house_victim),
            mk(3, "V2", false),
            mk(4, "V3", false),
        ];
        let snakes = if four {
            let mut all = vec![winner];
            all.extend(victims);
            all
        } else {
            let mut all = vec![winner];
            all.extend(victims.into_iter().take(2));
            all
        };
        let mut g = game(0, 0, snakes);
        g.final_turn = Some(100);
        g
    }

    fn hat_section(g: WindowGame) -> Section {
        let data = fixture_data(vec![duels_ladder()], vec![g]);
        let refs = data.games_on_ladder(0);
        hat_tricks_section(&data.ladders[0], &refs)
    }

    #[test]
    fn hat_trick_positive() {
        let section = hat_section(hat_trick_game(true, false, true, true));
        assert!(section.summary.contains("Winner 1"));
    }

    #[test]
    fn hat_trick_three_participants_never_qualify() {
        let section = hat_section(hat_trick_game(true, false, false, true));
        assert!(section.is_empty());
    }

    #[test]
    fn hat_trick_winner_without_frame_not() {
        let section = hat_section(hat_trick_game(true, false, true, false));
        assert!(section.is_empty());
    }

    #[test]
    fn hat_trick_victim_without_killer_not() {
        let section = hat_section(hat_trick_game(false, false, true, true));
        assert!(section.is_empty());
    }

    #[test]
    fn hat_trick_house_victim_not() {
        let section = hat_section(hat_trick_game(true, true, true, true));
        assert!(section.is_empty());
    }

    #[test]
    fn hat_trick_house_winner_still_counted() {
        let mut g = hat_trick_game(true, false, true, true);
        g.snakes[0].house = true;
        let section = hat_section(g);
        assert!(section.summary.contains("(house) 1"));
        // Longest only tracks non-house winners → no Longest row.
        assert!(!section.items.iter().any(|i| i.text.contains("Longest")));
    }

    // ------------------------------------------------------------ marathons

    #[test]
    fn marathons_longest_game() {
        let mut short = game(0, 0, vec![snake(1, "A", 1, 25.0), snake(2, "B", 2, 25.0)]);
        short.final_turn = Some(10);
        let mut long = game(0, 1, vec![snake(3, "C", 1, 25.0), snake(4, "D", 2, 25.0)]);
        long.final_turn = Some(1155);
        let data = fixture_data(vec![duels_ladder()], vec![short, long]);
        let refs = data.games_on_ladder(0);
        let section = marathons_section(&data.ladders[0], &refs);
        assert_eq!(section.items.len(), 1);
        assert!(section.items[0].text.contains("1155"));
    }

    #[test]
    fn marathons_no_frames_no_items() {
        let g = game(0, 0, vec![snake(1, "A", 1, 25.0), snake(2, "B", 2, 25.0)]);
        let data = fixture_data(vec![duels_ladder()], vec![g]);
        let refs = data.games_on_ladder(0);
        assert!(marathons_section(&data.ladders[0], &refs).is_empty());
    }

    // ------------------------------------------------------------ streaks

    fn streak_data() -> WindowData {
        let ladders = vec![duels_ladder()];
        let mut games = vec![
            {
                let mut g = game(0, 1, vec![snake(1, "A", 1, 20.0), snake(2, "B", 2, 30.0)]);
                g.final_turn = Some(9);
                g
            },
            {
                let mut g = game(0, 2, vec![snake(1, "A", 1, 20.0), snake(3, "C", 2, 28.0)]);
                g.final_turn = Some(9);
                g
            },
            // Draw: both placement 1 → everyone's streak resets.
            {
                let mut g = game(0, 3, vec![snake(1, "A", 1, 20.0), snake(2, "B", 1, 30.0)]);
                g.final_turn = Some(9);
                g
            },
            // B wins after the reset.
            {
                let mut g = game(0, 4, vec![snake(1, "A", 2, 20.0), snake(2, "B", 1, 30.0)]);
                g.final_turn = Some(9);
                g
            },
        ];
        games.sort_by_key(|g| g.created_at);
        fixture_data(ladders, games)
    }

    #[test]
    fn streaks_best_run_and_scalp() {
        let data = streak_data();
        let refs = data.games_on_ladder(0);
        let throne = replay_ladder(&data, 0, w0(), w1());
        let section = streaks_section(&data.ladders[0], &refs, &throne);
        // A: 2 straight (beat B mu 30, C mu 28) — best scalp B.
        // B: 1 straight after the reset.
        assert_eq!(section.items.len(), 2);
        let a = section
            .items
            .iter()
            .find(|i| i.subject.as_deref() == Some("A"))
            .expect("A's streak");
        assert!(a.text.contains("2 straight"));
        assert!(a.text.contains("best scalp B"));
    }

    #[test]
    fn streaks_draw_resets() {
        // After the draw at hour 3, A's streak ends: B's hour-4 win makes
        // B 1-straight and A 0. First-run-of-max-length: A's best stays 2.
        let data = streak_data();
        let refs = data.games_on_ladder(0);
        let throne = replay_ladder(&data, 0, w0(), w1());
        let section = streaks_section(&data.ladders[0], &refs, &throne);
        let b = section
            .items
            .iter()
            .find(|i| i.subject.as_deref() == Some("B"))
            .expect("B's streak");
        assert!(b.text.contains("1 straight"));
    }

    #[test]
    fn streaks_first_run_of_equal_max_length_wins() {
        // A wins twice (hours 1,2), draws, then wins twice again (hours 4,5).
        // Both runs are length 2; the first (beat mu 30) must be the best.
        let mut games = Vec::new();
        for (hour, b_mu) in [(1, 30.0), (2, 10.0), (3, 25.0), (4, 10.0), (5, 10.0)] {
            let placements = if hour == 3 { (1, 1) } else { (1, 2) };
            let a = snake(1, "A", placements.0, 20.0);
            let other = snake(2, "B", placements.1, b_mu);
            let mut g = game(0, hour, vec![a, other]);
            g.final_turn = Some(9);
            games.push(g);
        }
        games.sort_by_key(|g| g.created_at);
        let data = fixture_data(vec![duels_ladder()], games);
        let refs = data.games_on_ladder(0);
        let throne = replay_ladder(&data, 0, w0(), w1());
        let section = streaks_section(&data.ladders[0], &refs, &throne);
        let a = section
            .items
            .iter()
            .find(|i| i.text.contains("A "))
            .expect("A");
        assert!(a.text.contains("best scalp B"), "{}", a.text);
    }

    // ------------------------------------------------------------ throne

    fn throne_data() -> WindowData {
        let ladders = vec![duels_ladder()];
        let mut games = Vec::new();
        for (hour, winner_seed) in [(1, 1), (2, 1), (3, 2)] {
            let a = snake(1, "Alpha", if winner_seed == 1 { 1 } else { 2 }, 30.0);
            let b = snake(2, "Beta", if winner_seed == 2 { 1 } else { 2 }, 28.0);
            let mut g = game(0, hour, vec![a, b]);
            g.final_turn = Some(9);
            games.push(g);
        }
        games.sort_by_key(|g| g.created_at);
        let mut entries = BTreeMap::new();
        for seed in [1u8, 2] {
            entries.insert(
                uuid(seed),
                EntryInfo {
                    leaderboard_id: uuid(1),
                    snake_name: if seed == 1 {
                        "Alpha".into()
                    } else {
                        "Beta".into()
                    },
                    owner_login: Some("someone".into()),
                    disabled_at: None,
                    house: false,
                },
            );
        }
        let mut start_ratings = BTreeMap::new();
        start_ratings.insert(uuid(2), (40.0, 1.0)); // Beta leads at w0
        start_ratings.insert(uuid(1), (20.0, 1.0));
        WindowData {
            ladders,
            games,
            entries,
            start_ratings,
        }
    }

    #[test]
    fn throne_change_credits_held_time_and_crowning() {
        let data = throne_data();
        let outcome = replay_ladder(&data, 0, w0(), w1());
        assert!(outcome.changes >= 1);
        let (new_name, old_name, _, _) = outcome.crowning.expect("crowning exists");
        assert_eq!(new_name, "Alpha");
        assert_eq!(old_name.as_deref(), Some("Beta"));
        // Alpha held #1 from Oct 26 01:00 to w1 (Oct 31 12:00) = 131 h.
        let (_, _, secs) = outcome
            .held
            .iter()
            .find(|(_, name, _)| name == "Alpha")
            .expect("Alpha held");
        assert!(*secs > 125.0 * 3600.0 && *secs < 135.0 * 3600.0, "{secs}");
    }

    #[test]
    fn throne_first_ever_leader_counts_as_change() {
        // No start ratings → leader starts None → the first game crowns.
        let mut data = throne_data();
        data.start_ratings.clear();
        let outcome = replay_ladder(&data, 0, w0(), w1());
        assert!(outcome.changes >= 1);
        let (new_name, old_name, _, _) = outcome.crowning.expect("crowning exists");
        assert_eq!(new_name, "Alpha");
        assert_eq!(old_name, None);
    }

    #[test]
    fn throne_no_change_no_crowning_item() {
        let mut data = throne_data();
        // Beta (seeded #1, 40-3=37) plays one game; its post-game score
        // (28.5-12.5=16) still beats Alpha's (10.5-12.5=-2) → no change.
        data.games.clear();
        let mut g = game(
            0,
            1,
            vec![snake(1, "Alpha", 1, 10.0), snake(2, "Beta", 2, 28.0)],
        );
        g.final_turn = Some(9);
        data.games.push(g);
        let outcome = replay_ladder(&data, 0, w0(), w1());
        assert_eq!(outcome.changes, 0);
        assert!(outcome.crowning.is_none());
        let (section, change) = throne_section(&data.ladders[0], &outcome);
        assert!(section.items.is_empty());
        assert_eq!(change.changes, 0);
    }

    // ------------------------------------------------------------ house labels

    #[test]
    fn house_label_renders_tagged() {
        let s = snake(1, "Bombastic Bob", 1, 25.0);
        let house = WindowSnake { house: true, ..s };
        assert_eq!(house.label(), "Bombastic Bob (house)");
        assert_eq!(house.snake_name, "Bombastic Bob");
    }

    // ------------------------------------------------------------ empty window

    #[test]
    fn empty_window_emits_canonical_section_set() {
        let data = fixture_data(
            vec![
                duels_ladder(),
                ladder("Standard 11x11", 4, 2),
                ladder("Royale 11x11", 4, 3),
                ladder("Constrictor 11x11", 4, 4),
            ],
            vec![],
        );
        let digest = assemble_digest(&data, w0(), w1(), 1.0, 1.0);
        // 8 PerLadder kinds × 4 ladders + hat_tricks × 3 + 3 DuelsOnly + 2
        // CrossLadder = 32 + 3 + 3 + 2 = 40.
        assert_eq!(digest.sections.len(), 40, "canonical section set");
        assert!(
            digest
                .sections
                .iter()
                .all(|s| s.summary == "none this week" || s.kind == SectionKind::Calibration)
        );
        assert_eq!(digest.games_count, 0);
        // Calibration always renders its fitted k's.
        let calibration = digest
            .sections
            .iter()
            .find(|s| s.kind == SectionKind::Calibration)
            .expect("calibration section");
        assert!(!calibration.items.is_empty());
    }

    #[test]
    fn empty_window_chicken_share_is_zero_not_nan() {
        let data = fixture_data(vec![duels_ladder()], vec![]);
        let refs = data.games_on_ladder(0);
        let section = chicken_section(&data.ladders[0], &refs);
        assert!(section.is_empty());
    }

    // ------------------------------------------------------------ ops footnote

    #[test]
    fn ops_footnote_wall_deaths_early_turns() {
        let mut a = snake(1, "A", 1, 25.0);
        let mut b = snake(2, "B", 2, 25.0);
        a.frame = Some(frame_for(1, 5, 30));
        b.frame = Some(frame_for(2, 5, 30));
        // a died by wall on turn 3 (in), b on turn 6 (out).
        let fa = a.frame.as_mut().unwrap();
        fa.death_cause = Some("wall-collision".to_string());
        fa.death_turn = Some(3);
        let fb = b.frame.as_mut().unwrap();
        fb.death_cause = Some("wall-collision".to_string());
        fb.death_turn = Some(6);
        let mut g = game(0, 0, vec![a, b]);
        g.final_turn = Some(10);
        let data = fixture_data(vec![duels_ladder()], vec![g]);
        let refs = data.games_on_ladder(0);
        let section = ops_footnote_section(&data.ladders[0], &refs);
        assert!(section.summary.contains("1 this week"));
        assert!(section.summary.contains("A 1"));

        // Negative: no wall deaths.
        let data = fixture_data(vec![duels_ladder()], vec![]);
        let refs = data.games_on_ladder(0);
        assert!(ops_footnote_section(&data.ladders[0], &refs).is_empty());
    }
}
