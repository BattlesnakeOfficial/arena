use axum::{
    Form,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use std::collections::HashMap;
use std::str::FromStr as _;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    customizations::chip_color,
    errors::{ServerResult, WithStatus},
    models::{
        battlesnake,
        game::{GameBoardSize, GameType},
        leaderboard, session,
        tournament::{
            self, BracketParticipant, CreateTournament, MatchGame, MatchStatus, MatchStyle,
            RegistrationStatus, Tournament, TournamentMatch, TournamentStatus,
            TournamentVisibility, UpdateTournamentSettings,
        },
        user,
    },
    routes::auth::{CurrentUser, CurrentUserWithSession, OptionalUser},
    state::AppState,
    tournament_bracket::persist_bracket,
};

/// Cap for the leaderboard import qualifier flow.
const MAX_IMPORT_COUNT: i64 = 32;

/// Hard cap on total registrations per tournament. Bracket generation is
/// property-tested up to 128 participants, so both register and import refuse
/// to push a tournament past this. Keep in sync with
/// `MAX_REQUIRED_PARTICIPANTS`.
const MAX_TOTAL_REGISTRATIONS: i64 = 128;

/// Input limits for tournament settings (server-side; the form mirrors them).
const MAX_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 4000;
const MAX_SNAKES_PER_USER_LIMIT: i32 = 32;
const MAX_REQUIRED_PARTICIPANTS: i32 = 128;

// --- Pure business rules (unit tested below) ---

/// Registrations can only be added/removed/reseeded before the bracket exists.
fn registrations_editable(status: TournamentStatus) -> bool {
    matches!(
        status,
        TournamentStatus::Created | TournamentStatus::Registration
    )
}

/// Registration permission matrix: the tournament must be in a pre-start
/// status, and the registration_status must allow the caller.
///
/// For participants-only tournaments, only the owner may register snakes.
/// Otherwise an outsider could self-register, become a "participant", and
/// defeat the visibility 404 that hides the tournament from them.
fn can_register(tournament: &Tournament, is_owner: bool) -> bool {
    if !registrations_editable(tournament.status) {
        return false;
    }
    if tournament.visibility == TournamentVisibility::ParticipantsOnly && !is_owner {
        return false;
    }
    match tournament.registration_status {
        RegistrationStatus::Open => true,
        RegistrationStatus::OwnerOnly => is_owner,
        RegistrationStatus::Closed => false,
    }
}

/// Who can view a tournament page. `participants_only` tournaments are only
/// visible to the owner and users with a registered snake.
fn can_view(
    tournament: &Tournament,
    viewer_user_id: Option<Uuid>,
    participant_user_ids: &[Uuid],
) -> bool {
    match tournament.visibility {
        TournamentVisibility::Public => true,
        TournamentVisibility::ParticipantsOnly => viewer_user_id
            .is_some_and(|id| id == tournament.user_id || participant_user_ids.contains(&id)),
    }
}

/// Shared validation for create + settings update.
fn validate_tournament_params(
    name: &str,
    description: &str,
    required_participants: i32,
    max_snakes_per_user: i32,
) -> Result<(), String> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err("Tournament name cannot be empty".to_string());
    }
    if trimmed_name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "Tournament name must be at most {MAX_NAME_CHARS} characters"
        ));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "Description must be at most {MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    if !(2..=MAX_REQUIRED_PARTICIPANTS).contains(&required_participants) {
        return Err(format!(
            "Required participants must be between 2 and {MAX_REQUIRED_PARTICIPANTS}"
        ));
    }
    if !(1..=MAX_SNAKES_PER_USER_LIMIT).contains(&max_snakes_per_user) {
        return Err(format!(
            "Max snakes per user must be between 1 and {MAX_SNAKES_PER_USER_LIMIT}"
        ));
    }
    Ok(())
}

/// Settings-change rules: only editable before start, and game_type/board_size
/// are frozen once any snake is registered.
fn validate_settings_update(
    tournament: &Tournament,
    has_registrations: bool,
    new_game_type: &GameType,
    new_board_size: &GameBoardSize,
) -> Result<(), String> {
    if !registrations_editable(tournament.status) {
        return Err("Tournament settings can only be edited before the tournament starts".into());
    }
    if has_registrations
        && (*new_game_type != tournament.game_type || *new_board_size != tournament.board_size)
    {
        return Err(
            "Game type and board size cannot be changed after snakes have registered".into(),
        );
    }
    Ok(())
}

/// Parse a game type from a form value, rejecting anything outside the
/// supported dropdown options (GameType::from_str is a catch-all).
fn parse_game_type(s: &str) -> Result<GameType, String> {
    match GameType::from_str(s) {
        Ok(GameType::Other(_)) | Err(_) => Err(format!("Invalid game type: {s}")),
        Ok(game_type) => Ok(game_type),
    }
}

/// Parse a board size from a form value, rejecting custom sizes.
fn parse_board_size(s: &str) -> Result<GameBoardSize, String> {
    match GameBoardSize::from_str(s) {
        Ok(GameBoardSize::Custom(_)) | Err(_) => Err(format!("Invalid board size: {s}")),
        Ok(board_size) => Ok(board_size),
    }
}

/// Select snakes to import from a leaderboard, walking down the rankings:
/// skip snakes already registered, skip owners at the per-user cap, stop at
/// the requested count, and never push total registrations past
/// `MAX_TOTAL_REGISTRATIONS`. Returns `(battlesnake_id, user_id)` pairs in
/// rank order. Pure — callers pass the in-transaction registration snapshot.
fn select_import_candidates(
    ranked: &[leaderboard::RankedEntry],
    existing: &[tournament::TournamentRegistration],
    max_snakes_per_user: i32,
    requested: i64,
) -> Vec<(Uuid, Uuid)> {
    let mut registered_snakes: Vec<Uuid> = existing.iter().map(|r| r.battlesnake_id).collect();
    let mut per_user_counts: HashMap<Uuid, i64> = HashMap::new();
    for reg in existing {
        *per_user_counts.entry(reg.user_id).or_insert(0) += 1;
    }

    let remaining_capacity = MAX_TOTAL_REGISTRATIONS - existing.len() as i64;
    let target = requested.min(remaining_capacity).max(0);

    let mut candidates: Vec<(Uuid, Uuid)> = Vec::new();
    for entry in ranked {
        if candidates.len() as i64 >= target {
            break;
        }
        if registered_snakes.contains(&entry.battlesnake_id) {
            continue;
        }
        let owner_count = per_user_counts.entry(entry.user_id).or_insert(0);
        if *owner_count >= i64::from(max_snakes_per_user) {
            continue;
        }
        *owner_count += 1;
        registered_snakes.push(entry.battlesnake_id);
        candidates.push((entry.battlesnake_id, entry.user_id));
    }
    candidates
}

// --- In-transaction helpers ---
//
// Every mutating handler follows the same shape: open one transaction, lock
// the tournament row via `get_tournament_for_update`, run ALL validation
// against the locked row, mutate, then commit. Validating on the pool and
// opening a transaction afterwards is a TOCTOU bug — these helpers only make
// sense inside that locked transaction.

/// Whether this tournament must be concealed (404) from the requester,
/// mirroring `show_tournament`'s `can_view` rule: participants_only
/// tournaments don't exist for anyone but the owner and registered
/// participants. Mutating handlers pass their locked transaction so the
/// participant check reads in-transaction state.
async fn is_hidden_from<'e, E>(executor: E, t: &Tournament, user_id: Uuid) -> cja::Result<bool>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    if t.visibility != TournamentVisibility::ParticipantsOnly || t.user_id == user_id {
        return Ok(false);
    }
    let registrations =
        tournament::count_registrations_for_user(executor, t.tournament_id, user_id)
            .await
            .wrap_err("Failed to count requester registrations")?;
    Ok(registrations == 0)
}

/// Registration checks + insert against the locked tournament row: duplicate
/// snake, per-user cap, and the total-registrations cap are all evaluated on
/// in-transaction counts. Returns `Err(message)` in the inner result for
/// user-facing refusals (flash + redirect at the route layer).
async fn register_snake_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    t: &Tournament,
    owner_user_id: Uuid,
    battlesnake_id: Uuid,
    snake_name: &str,
) -> cja::Result<Result<tournament::TournamentRegistration, String>> {
    if tournament::is_battlesnake_registered(&mut **tx, t.tournament_id, battlesnake_id)
        .await
        .wrap_err("Failed to check existing registration")?
    {
        return Ok(Err(format!(
            "{snake_name} is already registered in this tournament"
        )));
    }

    let user_reg_count =
        tournament::count_registrations_for_user(&mut **tx, t.tournament_id, owner_user_id)
            .await
            .wrap_err("Failed to count user registrations")?;
    if user_reg_count >= i64::from(t.max_snakes_per_user) {
        return Ok(Err(format!(
            "You have reached the limit of {} snake(s) for this tournament",
            t.max_snakes_per_user
        )));
    }

    let total = tournament::count_registrations(&mut **tx, t.tournament_id)
        .await
        .wrap_err("Failed to count registrations")?;
    if total >= MAX_TOTAL_REGISTRATIONS {
        return Ok(Err(format!(
            "This tournament is full ({MAX_TOTAL_REGISTRATIONS} snakes)"
        )));
    }

    let registration = tournament::register_snake_with_next_seed(
        tx,
        t.tournament_id,
        battlesnake_id,
        owner_user_id,
    )
    .await
    .wrap_err("Failed to register snake")?;

    Ok(Ok(registration))
}

/// Start rules (BS-022): the tournament must be in `registration`, and enough
/// snakes must be registered — at least `required_participants`, and never
/// fewer than 2 (a bracket needs two sides even if the owner set a lower bar).
fn validate_start(tournament: &Tournament, registration_count: i64) -> Result<(), String> {
    if !tournament
        .status
        .can_transition_to(TournamentStatus::InProgress)
    {
        return Err(format!(
            "Tournament cannot start from status '{}'",
            tournament.status.as_str()
        ));
    }
    if registration_count < 2 {
        return Err("At least 2 registered snakes are needed to start a tournament".to_string());
    }
    if registration_count < i64::from(tournament.required_participants) {
        return Err(format!(
            "Tournament requires {} participants but only {} are registered",
            tournament.required_participants, registration_count
        ));
    }
    Ok(())
}

/// Header label for a bracket round: the last round is the Final.
fn round_label(round: i32, total_rounds: i32) -> String {
    if round == total_rounds {
        "Final".to_string()
    } else {
        format!("Round {round}")
    }
}

/// Wins for a participant, derived from per-game winners (`None` = tie or
/// still running — counts for nobody).
fn win_count(game_winners: &[Option<Uuid>], battlesnake_id: Uuid) -> usize {
    game_winners
        .iter()
        .flatten()
        .filter(|winner| **winner == battlesnake_id)
        .count()
}

/// Whether a match game with no recorded winner finished as a tie (vs still
/// being in flight). A later game only exists once this one finished, and a
/// completed match has no games in flight.
fn is_tie_game(winner_id: Option<Uuid>, has_later_game: bool, match_status: MatchStatus) -> bool {
    winner_id.is_none() && (has_later_game || match_status == MatchStatus::Completed)
}

// --- Shared rendering helpers ---

fn status_badge(status: TournamentStatus) -> Markup {
    let (class, label) = match status {
        TournamentStatus::Created => ("badge", "Draft"),
        TournamentStatus::Registration => ("badge ok", "Registration open"),
        TournamentStatus::InProgress => ("badge live", "In progress"),
        TournamentStatus::Completed => ("badge", "Completed"),
        TournamentStatus::Canceled => ("badge", "Canceled"),
    };
    html! { span class=(class) { (label) } }
}

/// Everything needed to render one match card in the bracket.
struct BracketMatchView<'a> {
    tournament_match: &'a TournamentMatch,
    participants: &'a [BracketParticipant],
    games: &'a [MatchGame],
}

/// One team row inside a match card: color chip, name, score cell.
fn bracket_team_row(
    p: &BracketParticipant,
    m: &TournamentMatch,
    game_winners: &[Option<Uuid>],
    show_wins: bool,
) -> Markup {
    let is_winner = p.battlesnake_id.is_some() && p.battlesnake_id == m.winner_id;
    let is_loser = m.winner_id.is_some() && p.battlesnake_id.is_some() && !is_winner;
    let is_tbd = p.battlesnake_id.is_none();

    html! {
        div .team .w[is_winner] .l[is_loser] .tbd[is_tbd] {
            @if let Some(color) = p.snake_color.as_deref().filter(|c| !c.is_empty()) {
                span class="chip" style={"background:"(chip_color(color))} {}
            } @else {
                span class="chip tbd" {}
            }
            span class="name" {
                @if let Some(name) = &p.snake_name { (name) } @else { "TBD" }
            }
            span class="score" {
                @if show_wins {
                    @if let Some(battlesnake_id) = p.battlesnake_id {
                        (win_count(game_winners, battlesnake_id))
                    } @else { "—" }
                } @else if is_winner { "✓" }
                @else { "—" }
            }
        }
    }
}

/// One match card, placed on the bracket grid by its visual coordinates.
/// Round columns are the odd grid columns (connectors live between); each
/// match spans two grid rows so later rounds center between their feeders.
fn bracket_match_card(
    view: &BracketMatchView,
    match_style: MatchStyle,
    total_rounds: i32,
) -> Markup {
    let m = view.tournament_match;
    let game_winners: Vec<Option<Uuid>> = view.games.iter().map(|g| g.winner_id).collect();
    let show_wins = match_style != MatchStyle::SingleGame;
    // Byes only have their single seeded participant persisted.
    let is_bye = m.round == 1 && view.participants.len() == 1;
    let is_live = m.status == MatchStatus::InProgress;

    let col = m.visual_column * 2 + 1;
    let row = m.visual_row + 1;

    let tag_label = if m.round == total_rounds {
        "Final".to_string()
    } else {
        format!("R{} · M{}", m.round, m.position + 1)
    };
    let status_label = match m.status {
        MatchStatus::Scheduled => {
            if is_bye {
                "bye"
            } else {
                "scheduled"
            }
        }
        MatchStatus::InProgress => "live",
        MatchStatus::Completed => "done",
        MatchStatus::Canceled => "canceled",
    };
    // The game currently being played, for the theater strip.
    let live_game = if is_live { view.games.last() } else { None };

    html! {
        div .match .live[is_live]
            style={"grid-column: "(col)"; grid-row: "(row)" / "(row + 2)";"} {
            span class="tag" {
                @if is_live { span class="live-dot" {} }
                (tag_label) " · " (status_label)
                @if let Some(game) = live_game { " · game " (game.game_number) }
            }
            @for p in view.participants {
                (bracket_team_row(p, m, &game_winners, show_wins))
            }
            @if is_bye {
                div class="team tbd" {
                    span class="chip tbd" {}
                    span class="name" { "Bye" }
                    span class="score" { "—" }
                }
            }
            @if let Some(game) = live_game {
                a class="theater-strip" href={"/games/"(game.game_id)} {
                    span class="play" {
                        svg viewBox="0 0 8 8" fill="#1A0710" aria-hidden="true" {
                            path d="M0.5 0l7 4-7 4z" {}
                        }
                    }
                    span class="label" { "THEATER" }
                    span class="go" { "watch live →" }
                }
            }
            @if !view.games.is_empty() {
                div class="mgames" {
                    @for (i, game) in view.games.iter().enumerate() {
                        a href={"/games/"(game.game_id)} {
                            "Game " (game.game_number)
                            @if is_tie_game(game.winner_id, i + 1 < view.games.len(), m.status) {
                                " (tie)"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The bracket: rounds as columns on a shared CSS grid (odd columns hold
/// matches, even columns hold the elbow connectors), a champion slot after
/// the final, and a horizontal scroll container for small screens. Grid
/// placement comes from each match's `visual_column`/`visual_row`.
fn bracket_section(
    t: &Tournament,
    matches: &[TournamentMatch],
    participants_by_match: &HashMap<Uuid, Vec<BracketParticipant>>,
    games_by_match: &HashMap<Uuid, Vec<MatchGame>>,
) -> Markup {
    let total_rounds = matches.iter().map(|m| m.round).max().unwrap_or(0);
    // No matches means no grid: `repeat(0, ...)` is invalid CSS.
    if total_rounds == 0 {
        return html! {};
    }

    // Champion: the winner of the final (highest round), named via that
    // match's participant rows. If the winning snake was deleted since (its
    // participant row cascades away), still show the slot with a neutral
    // placeholder rather than silently dropping it.
    let final_match = matches.iter().find(|m| m.round == total_rounds);
    let champion = (t.status == TournamentStatus::Completed)
        .then(|| {
            let final_match = final_match?;
            let winner_id = final_match.winner_id?;
            participants_by_match
                .get(&final_match.match_id)
                .and_then(|participants| {
                    participants
                        .iter()
                        .find(|p| p.battlesnake_id == Some(winner_id))
                })
                .map(|p| {
                    (
                        p.snake_name
                            .clone()
                            .unwrap_or_else(|| "(deleted snake)".to_string()),
                        p.snake_color.clone().filter(|c| !c.is_empty()),
                    )
                })
                .or(Some(("(deleted snake)".to_string(), None)))
        })
        .flatten();

    // Grid geometry. Matches sit in odd columns (1, 3, ...); the elbow
    // connectors between rounds sit in the even columns; the champion slot
    // takes the last odd column. Rows: a bracket of size 2^R needs 2^R rows,
    // each match spanning two.
    let grid_rows = 1_i32 << total_rounds;
    let champ_col = total_rounds * 2 + 1;
    let mut grid_cols = String::new();
    for _ in 1..=total_rounds {
        grid_cols.push_str("minmax(210px, 1fr) 28px ");
    }
    grid_cols.push_str("minmax(170px, 1fr)");
    let min_width = total_rounds * 238 + 170;

    // Feeder rows for connector placement: an elbow spans from the vertical
    // center of one feeder match to the center of its sibling.
    let row_of: HashMap<(i32, i32), i32> = matches
        .iter()
        .map(|m| ((m.round, m.position), m.visual_row))
        .collect();

    static EMPTY_PARTICIPANTS: Vec<BracketParticipant> = Vec::new();
    static EMPTY_GAMES: Vec<MatchGame> = Vec::new();

    html! {
        h2 class="vh" { "Bracket" }
        div class="bracket-note" {
            (matches.iter().filter(|m| m.status == MatchStatus::Completed).count())
            " of " (matches.len()) " matches played · "
            (total_rounds) @if total_rounds == 1 { " round" } @else { " rounds" }
        }
        div class="bracket-scroll" tabindex="0" role="region" aria-label="Tournament bracket" {
            div style={"min-width: "(min_width)"px;"} {
                div class="round-labels" style={"grid-template-columns: "(grid_cols)";"} {
                    @for round in 1..=total_rounds {
                        span style={"grid-column: "(round * 2 - 1)";"} {
                            (round_label(round, total_rounds))
                        }
                    }
                    span style={"grid-column: "(champ_col)";"} { "Champion" }
                }
                div class="bracket"
                    style={
                        "grid-template-columns: "(grid_cols)"; "
                        "grid-template-rows: repeat("(grid_rows)", minmax(44px, auto));"
                    } {
                    @for m in matches {
                        @let view = BracketMatchView {
                            tournament_match: m,
                            participants: participants_by_match
                                .get(&m.match_id)
                                .unwrap_or(&EMPTY_PARTICIPANTS),
                            games: games_by_match.get(&m.match_id).unwrap_or(&EMPTY_GAMES),
                        };
                        (bracket_match_card(&view, t.match_style, total_rounds))

                        // Elbow connector from this match's two feeders.
                        @if m.round > 1 {
                            @let top = row_of.get(&(m.round - 1, m.position * 2));
                            @let bottom = row_of.get(&(m.round - 1, m.position * 2 + 1));
                            @if let (Some(top), Some(bottom)) = (top, bottom) {
                                div class="conn"
                                    style={
                                        "grid-column: "(m.round * 2 - 2)"; "
                                        "grid-row: "(top + 2)" / "(bottom + 2)";"
                                    } {}
                            }
                        }
                    }

                    // Champion slot, fed by the final.
                    @if let Some(final_match) = final_match {
                        @let frow = final_match.visual_row + 1;
                        div class="conn-h"
                            style={"grid-column: "(champ_col - 1)"; grid-row: "(frow)" / "(frow + 2)";"} {}
                        div .champ .crowned[champion.is_some()]
                            style={"grid-column: "(champ_col)"; grid-row: "(frow)" / "(frow + 2)";"} {
                            div class="glyph" aria-hidden="true" { "🏆" }
                            div class="label" { "Champion" }
                            div class="who" {
                                @if let Some((name, color)) = &champion {
                                    @if let Some(color) = color {
                                        span class="chip champ-chip" style={"background:"(chip_color(color))} {}
                                    }
                                    (name)
                                } @else {
                                    "To be decided"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Form fields shared by the create and edit pages. When `current` is Some,
/// fields are pre-filled with the tournament's existing values.
#[allow(clippy::too_many_lines)]
fn tournament_form_fields(current: Option<&Tournament>) -> Markup {
    let name = current.map(|t| t.name.clone()).unwrap_or_default();
    let description = current
        .and_then(|t| t.description.clone())
        .unwrap_or_default();
    let game_type = current.map_or(GameType::Standard, |t| t.game_type.clone());
    let board_size = current.map_or(GameBoardSize::Medium, |t| t.board_size.clone());
    let match_style = current.map_or(MatchStyle::SingleGame, |t| t.match_style);
    let registration_status = current.map_or(RegistrationStatus::Open, |t| t.registration_status);
    let visibility = current.map_or(TournamentVisibility::Public, |t| t.visibility);
    let max_snakes_per_user = current.map_or(1, |t| t.max_snakes_per_user);
    let required_participants = current.map_or(2, |t| t.required_participants);

    html! {
        div class="field" {
            label for="name" { "Name" }
            input type="text" id="name" name="name" required value=(name) {}
        }

        div class="field" {
            label for="description" { "Description" }
            textarea id="description" name="description" rows="3" { (description) }
        }

        div class="field" {
            label for="game_type" { "Game Type" }
            select id="game_type" name="game_type" required {
                option value="Standard" selected[game_type == GameType::Standard] { "Standard" }
                option value="Royale" selected[game_type == GameType::Royale] { "Royale" }
                option value="Constrictor" selected[game_type == GameType::Constrictor] { "Constrictor" }
                option value="Snail Mode" selected[game_type == GameType::SnailMode] { "Snail Mode" }
            }
            div class="hint" { "Cannot be changed once snakes have registered" }
        }

        div class="field" {
            label for="board_size" { "Board Size" }
            select id="board_size" name="board_size" required {
                option value="7x7" selected[board_size == GameBoardSize::Small] { "7x7 (Small)" }
                option value="11x11" selected[board_size == GameBoardSize::Medium] { "11x11 (Medium)" }
                option value="19x19" selected[board_size == GameBoardSize::Large] { "19x19 (Large)" }
            }
            div class="hint" { "Cannot be changed once snakes have registered" }
        }

        div class="field" {
            label for="match_style" { "Match Style" }
            select id="match_style" name="match_style" required {
                option value="single_game" selected[match_style == MatchStyle::SingleGame] { "Single Game" }
                option value="best_of_3" selected[match_style == MatchStyle::BestOf3] { "Best of 3" }
                option value="first_to_3" selected[match_style == MatchStyle::FirstTo3] { "First to 3" }
            }
        }

        div class="field" {
            label for="registration_status" { "Registration" }
            select id="registration_status" name="registration_status" required {
                option value="open" selected[registration_status == RegistrationStatus::Open] { "Open (anyone can register)" }
                option value="closed" selected[registration_status == RegistrationStatus::Closed] { "Closed (no registrations)" }
                option value="owner_only" selected[registration_status == RegistrationStatus::OwnerOnly] { "Owner Only" }
            }
        }

        div class="field" {
            label for="visibility" { "Visibility" }
            select id="visibility" name="visibility" required {
                option value="public" selected[visibility == TournamentVisibility::Public] { "Public" }
                option value="participants_only" selected[visibility == TournamentVisibility::ParticipantsOnly] { "Participants Only" }
            }
        }

        div class="field" {
            label for="max_snakes_per_user" { "Max Snakes per User" }
            input type="number" id="max_snakes_per_user" name="max_snakes_per_user"
                min="1" required value=(max_snakes_per_user) {}
        }

        div class="field" {
            label for="required_participants" { "Required Participants" }
            input type="number" id="required_participants" name="required_participants"
                min="2" required value=(required_participants) {}
        }
    }
}

/// Set a flash message and redirect. Shared tail for the POST handlers.
async fn flash_redirect(
    state: &AppState,
    session_id: Uuid,
    message: String,
    flash_type: &str,
    to: &str,
) -> ServerResult<Response, StatusCode> {
    session::set_flash_message(&state.db, session_id, message, flash_type)
        .await
        .wrap_err("Failed to set flash message")?;
    Ok(Redirect::to(to).into_response())
}

// --- Form payloads ---

/// Shared by POST /tournaments (create) and POST /tournaments/{id}/settings.
/// game_type/board_size arrive as strings and are validated via
/// parse_game_type/parse_board_size since those enums have catch-all variants.
#[derive(Debug, Deserialize)]
pub struct TournamentSettingsForm {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub game_type: String,
    pub board_size: String,
    pub match_style: MatchStyle,
    pub registration_status: RegistrationStatus,
    pub visibility: TournamentVisibility,
    pub max_snakes_per_user: i32,
    pub required_participants: i32,
}

impl TournamentSettingsForm {
    fn description_opt(&self) -> Option<String> {
        let trimmed = self.description.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterForm {
    pub battlesnake_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct UnregisterForm {
    pub registration_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct SeedForm {
    pub registration_id: Uuid,
    pub new_seed: i32,
}

#[derive(Debug, Deserialize)]
pub struct StatusForm {
    pub action: String,
}

#[derive(Debug, Deserialize)]
pub struct ImportLeaderboardForm {
    pub leaderboard_id: Uuid,
    pub count: i64,
}

// --- Handlers ---

/// GET /tournaments — public tournaments plus the viewer's own.
pub async fn list_tournaments(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let viewer_id = viewer.as_ref().map(|u| u.user_id);
    let tournaments = tournament::list_visible_tournaments(&state.db, viewer_id)
        .await
        .wrap_err("Failed to list tournaments")?;

    Ok(page_factory.create_page(
        "Tournaments".to_string(),
        Box::new(html! {
            div class="page-head" {
                div {
                    h1 { "Tournaments" }
                    div class="sub" {
                        "Single-elimination brackets. Register your snakes, "
                        "seed the field, and play it out round by round."
                    }
                }
                div class="spacer" {}
                @if viewer.is_some() {
                    a href="/tournaments/new" class="btn solid head-cta" { "Create Tournament" }
                }
            }

            @if tournaments.is_empty() {
                p class="empty" { "No tournaments yet." }
            } @else {
                div class="section" {
                    table class="data" {
                        thead {
                            tr {
                                th { "Tournament" }
                                th { "Status" }
                                th class="r" { "Snakes" }
                                th class="r hide-sm" { "Game" }
                                th class="r hide-sm" { "Created" }
                            }
                        }
                        tbody {
                            @for t in &tournaments {
                                tr {
                                    td {
                                        div class="snake-cell" {
                                            span {
                                                a class="name" href={"/tournaments/"(t.tournament_id)} { (t.name) }
                                                span class="owner" { "by " (t.owner_login) }
                                            }
                                        }
                                    }
                                    td { (status_badge(t.status)) }
                                    td class="r num" { (t.registration_count) }
                                    td class="r num hide-sm" { (t.game_type.as_str()) }
                                    td class="r num hide-sm" { (t.created_at.format("%b %-d, %Y")) }
                                }
                            }
                        }
                    }
                }
            }
        }),
    ))
}

/// GET /tournaments/new — creation form (auth required).
pub async fn new_tournament(
    CurrentUser(_): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page(
        "Create Tournament".to_string(),
        Box::new(html! {
            div class="crumb" {
                a href="/tournaments" { "Tournaments" }
                " / New"
            }
            div class="page-head" {
                div {
                    h1 { "Create Tournament" }
                    div class="sub" {
                        "Pick the game rules and registration policy — you can "
                        "tweak everything until the bracket is generated."
                    }
                }
            }

            form class="tform" action="/tournaments" method="post" {
                (tournament_form_fields(None))

                div class="actions" {
                    button type="submit" class="btn solid" { "Create Tournament" }
                    a href="/tournaments" class="btn" { "Cancel" }
                }
            }
        }),
    ))
}

/// POST /tournaments — create (auth required).
pub async fn create_tournament(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Form(form): Form<TournamentSettingsForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let parsed = validate_tournament_params(
        &form.name,
        &form.description,
        form.required_participants,
        form.max_snakes_per_user,
    )
    .and_then(|()| {
        Ok((
            parse_game_type(&form.game_type)?,
            parse_board_size(&form.board_size)?,
        ))
    });

    let (game_type, board_size) = match parsed {
        Ok(values) => values,
        Err(message) => {
            return flash_redirect(
                &state,
                session.session_id,
                message,
                session::FLASH_TYPE_ERROR,
                "/tournaments/new",
            )
            .await;
        }
    };

    let created = tournament::create_tournament(
        &state.db,
        user.user_id,
        CreateTournament {
            name: form.name.trim().to_string(),
            description: form.description_opt(),
            game_type,
            board_size,
            registration_status: form.registration_status,
            visibility: form.visibility,
            match_style: form.match_style,
            max_snakes_per_user: form.max_snakes_per_user,
            required_participants: form.required_participants,
        },
    )
    .await
    .wrap_err("Failed to create tournament")?;

    flash_redirect(
        &state,
        session.session_id,
        "Tournament created successfully!".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &format!("/tournaments/{}", created.tournament_id),
    )
    .await
}

/// GET /tournaments/{id} — detail page.
#[allow(clippy::too_many_lines)]
pub async fn show_tournament(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path(tournament_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    let registrations = tournament::get_registrations_with_details(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch registrations")?;

    let viewer_id = viewer.as_ref().map(|u| u.user_id);
    let participant_user_ids: Vec<Uuid> = registrations.iter().map(|r| r.user_id).collect();

    // participants_only tournaments 404 for outsiders (don't reveal existence)
    if !can_view(&t, viewer_id, &participant_user_ids) {
        return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }

    let owner = user::get_user_by_id(&state.db, t.user_id)
        .await
        .wrap_err("Failed to fetch tournament owner")?;
    let owner_login = owner
        .as_ref()
        .map(|o| o.github_login.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let is_owner = viewer_id == Some(t.user_id);

    // Snakes the viewer could register: their own, not yet in this tournament,
    // and only while they are under the per-user cap.
    let registerable_snakes = if let Some(u) = viewer.as_ref() {
        if can_register(&t, is_owner) {
            let user_reg_count = registrations
                .iter()
                .filter(|r| r.user_id == u.user_id)
                .count() as i32;
            if user_reg_count < t.max_snakes_per_user {
                let registered_ids: Vec<Uuid> =
                    registrations.iter().map(|r| r.battlesnake_id).collect();
                battlesnake::get_battlesnakes_by_user_id(&state.db, u.user_id)
                    .await
                    .wrap_err("Failed to fetch viewer's battlesnakes")?
                    .into_iter()
                    .filter(|s| !registered_ids.contains(&s.battlesnake_id))
                    .collect()
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // Leaderboards for the owner's import form
    let leaderboards = if is_owner && registrations_editable(t.status) {
        leaderboard::get_active_leaderboards(&state.db)
            .await
            .wrap_err("Failed to fetch leaderboards")?
    } else {
        vec![]
    };

    // Bracket data, only once a bracket exists (in_progress or completed).
    let bracket_data = if matches!(
        t.status,
        TournamentStatus::InProgress | TournamentStatus::Completed
    ) {
        let matches = tournament::get_matches_for_tournament(&state.db, tournament_id)
            .await
            .wrap_err("Failed to fetch tournament matches")?;
        let participants =
            tournament::get_participants_with_names_for_tournament(&state.db, tournament_id)
                .await
                .wrap_err("Failed to fetch bracket participants")?;
        let match_games = tournament::get_match_games_for_tournament(&state.db, tournament_id)
            .await
            .wrap_err("Failed to fetch tournament match games")?;

        let mut participants_by_match: HashMap<Uuid, Vec<BracketParticipant>> = HashMap::new();
        for participant in participants {
            participants_by_match
                .entry(participant.match_id)
                .or_default()
                .push(participant);
        }
        let mut games_by_match: HashMap<Uuid, Vec<MatchGame>> = HashMap::new();
        for match_game in match_games {
            games_by_match
                .entry(match_game.match_id)
                .or_default()
                .push(match_game);
        }

        Some((matches, participants_by_match, games_by_match))
    } else {
        None
    };

    let can_edit_registrations = registrations_editable(t.status);
    let max_seed = registrations.len();

    // Stat band numbers, cheap from data already fetched.
    let (total_rounds, total_matches, completed_matches) = bracket_data
        .as_ref()
        .map(|(matches, _, _)| {
            (
                matches.iter().map(|m| m.round).max().unwrap_or(0),
                matches.len(),
                matches
                    .iter()
                    .filter(|m| m.status == MatchStatus::Completed)
                    .count(),
            )
        })
        .unwrap_or((0, 0, 0));

    let style_label = match t.match_style {
        MatchStyle::SingleGame => "Single game",
        MatchStyle::BestOf3 => "Best of 3",
        MatchStyle::FirstTo3 => "First to 3",
    };

    Ok(page_factory.create_page(
        format!("Tournament: {}", t.name),
        Box::new(html! {
            div class="crumb" {
                a href="/tournaments" { "Tournaments" }
                " / " (t.name)
            }
            div class="page-head" {
                div {
                    h1 { (t.name) }
                    div class="sub" {
                        "by " (owner_login)
                        " · " (t.game_type.as_str())
                        " · " (t.board_size.as_str())
                        " · " (style_label)
                    }
                }
                div class="spacer" {}
                @if t.status == TournamentStatus::InProgress {
                    span class="round-pill" {
                        span class="live-dot" {}
                        "LIVE · ROUND " (t.current_round) " OF " (total_rounds)
                    }
                } @else {
                    span class="head-status" { (status_badge(t.status)) }
                }
            }
            @if let Some(ref description) = t.description {
                p class="tourney-desc" { (description) }
            }

            div class="stats" style="margin-top: 34px;" {
                div class="stat" {
                    div class="label" { "Registered snakes" }
                    div class="value" {
                        (registrations.len())
                        @if can_edit_registrations {
                            small { "of " (t.required_participants) " needed" }
                        }
                    }
                }
                div class="stat" {
                    div class="label" { "Rounds" }
                    div class="value" {
                        @if total_rounds > 0 { (total_rounds) } @else { "—" }
                    }
                }
                div class="stat" {
                    div class="label" { "Matches played" }
                    div class="value" {
                        @if total_matches > 0 {
                            @if t.status == TournamentStatus::InProgress {
                                span class="live" { (completed_matches) }
                            } @else {
                                (completed_matches)
                            }
                            small { "of " (total_matches) }
                        } @else { "—" }
                    }
                }
                div class="stat" {
                    div class="label" { "Format" }
                    div class="value sm" {
                        (style_label) " · " (t.board_size.as_str()) " " (t.game_type.as_str())
                    }
                }
            }

            // Bracket (in_progress / completed only)
            @if let Some((matches, participants_by_match, games_by_match)) = &bracket_data {
                (bracket_section(&t, matches, participants_by_match, games_by_match))
            }

            div class="grid" {
                div {
                    div class="section" style="margin-top: 0;" {
                        h2 { "Registered Snakes" }
                        @if registrations.is_empty() {
                            p class="empty" { "No snakes registered yet." }
                        } @else {
                            table class="data" {
                                thead {
                                    tr {
                                        th { "Seed" }
                                        th { "Battlesnake" }
                                        @if can_edit_registrations && viewer.is_some() {
                                            th class="r" { "Actions" }
                                        }
                                    }
                                }
                                tbody {
                                    @for reg in &registrations {
                                        tr {
                                            td class="rank" { (format!("{:02}", reg.seed)) }
                                            td {
                                                div class="snake-cell" {
                                                    span class="chip" style={"background:"(chip_color(&reg.snake_color))} {}
                                                    span {
                                                        a class="name" href={"/battlesnakes/"(reg.battlesnake_id)"/profile"} { (reg.snake_name) }
                                                        span class="owner" { "by " (reg.owner_login) }
                                                    }
                                                }
                                            }
                                            @if can_edit_registrations && viewer.is_some() {
                                                td {
                                                    div class="reg-actions" {
                                                        @if is_owner {
                                                            form class="seed-form" action={"/tournaments/"(t.tournament_id)"/seed"} method="post" {
                                                                input type="hidden" name="registration_id" value=(reg.registration_id);
                                                                input type="number" name="new_seed" aria-label="New seed"
                                                                    min="1" max=(max_seed) value=(reg.seed) {}
                                                                button type="submit" class="btn sm" { "Move" }
                                                            }
                                                        }
                                                        @if is_owner || viewer_id == Some(reg.user_id) {
                                                            form action={"/tournaments/"(t.tournament_id)"/unregister"} method="post" {
                                                                input type="hidden" name="registration_id" value=(reg.registration_id);
                                                                button type="submit" class="btn sm danger"
                                                                    onclick="return confirm('Remove this snake from the tournament?');" { "Unregister" }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                aside class="rail" {
                    @if is_owner {
                        div class="block" {
                            h3 { "Owner Controls" }
                            div class="owner-actions" {
                                @if t.status == TournamentStatus::Created {
                                    form action={"/tournaments/"(t.tournament_id)"/status"} method="post" {
                                        input type="hidden" name="action" value="open_registration";
                                        button type="submit" class="btn sm solid" { "Open Registration" }
                                    }
                                }
                                @if t.status == TournamentStatus::Registration {
                                    form action={"/tournaments/"(t.tournament_id)"/start"} method="post" {
                                        button type="submit" class="btn sm solid" { "Start Tournament" }
                                    }
                                }
                                @if t.status == TournamentStatus::InProgress {
                                    form action={"/tournaments/"(t.tournament_id)"/run-round"} method="post" {
                                        button type="submit" class="btn sm solid" { "Run Round " (t.current_round) }
                                    }
                                    form action={"/tournaments/"(t.tournament_id)"/reset"} method="post" {
                                        button type="submit" class="btn sm"
                                            onclick="return confirm('Reset this tournament? The bracket and all match results will be deleted. Registrations are kept. Any games still running will finish on their own but won\'t count.');" { "Reset Tournament" }
                                    }
                                }
                                @if can_edit_registrations {
                                    a href={"/tournaments/"(t.tournament_id)"/edit"} class="btn sm" { "Edit Settings" }
                                }
                                @if t.status.can_transition_to(TournamentStatus::Canceled) {
                                    form action={"/tournaments/"(t.tournament_id)"/status"} method="post" {
                                        input type="hidden" name="action" value="cancel";
                                        button type="submit" class="btn sm danger"
                                            onclick="return confirm('Are you sure you want to cancel this tournament?');" { "Cancel Tournament" }
                                    }
                                }
                            }
                        }

                        @if !leaderboards.is_empty() {
                            div class="block" {
                                h3 { "Import from Leaderboard" }
                                p class="railp" {
                                    "Register the top-ranked snakes from a leaderboard, seeded by rank."
                                }
                                form class="rail-form" action={"/tournaments/"(t.tournament_id)"/import-leaderboard"} method="post" {
                                    select name="leaderboard_id" aria-label="Leaderboard" {
                                        @for lb in &leaderboards {
                                            option value=(lb.leaderboard_id) { (lb.name) }
                                        }
                                    }
                                    label class="lbl" for="import_count" { "Top" }
                                    input type="number" id="import_count" name="count"
                                        min="1" max=(MAX_IMPORT_COUNT) value="8" {}
                                    button type="submit" class="btn sm" { "Import" }
                                }
                            }
                        }
                    }

                    @if !registerable_snakes.is_empty() {
                        div class="block" {
                            h3 { "Register a Snake" }
                            form class="rail-form" action={"/tournaments/"(t.tournament_id)"/register"} method="post" {
                                select name="battlesnake_id" aria-label="Battlesnake" {
                                    @for snake in &registerable_snakes {
                                        option value=(snake.battlesnake_id) { (snake.name) }
                                    }
                                }
                                button type="submit" class="btn sm solid" { "Register" }
                            }
                        }
                    }

                    div class="block" {
                        h3 { "Format" }
                        dl class="meta-list" {
                            div { dt { "Game" } dd { (t.game_type.as_str()) } }
                            div { dt { "Board" } dd { (t.board_size.as_str()) } }
                            div { dt { "Series" } dd { (style_label) } }
                            div { dt { "Registration" } dd { (t.registration_status.as_str()) } }
                            div { dt { "Visibility" } dd { (t.visibility.as_str()) } }
                            div { dt { "Max snakes / user" } dd { (t.max_snakes_per_user) } }
                            div { dt { "Required to start" } dd { (t.required_participants) } }
                            div { dt { "Created" } dd { (t.created_at.format("%b %-d, %Y")) } }
                        }
                    }
                }
            }
        }),
    ))
}

/// GET /tournaments/{id}/edit — settings form (owner only).
pub async fn edit_tournament(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(tournament_id): Path<Uuid>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&state.db, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("You don't have permission to edit this tournament".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let registration_count = tournament::count_registrations(&state.db, tournament_id)
        .await
        .wrap_err("Failed to count registrations")?;

    Ok(page_factory.create_page(
        format!("Edit Tournament: {}", t.name),
        Box::new(html! {
            div class="crumb" {
                a href="/tournaments" { "Tournaments" }
                " / "
                a href={"/tournaments/"(tournament_id)} { (t.name) }
                " / Edit"
            }
            div class="page-head" {
                div {
                    h1 { "Edit Tournament" }
                    div class="sub" { (t.name) }
                }
            }

            @if registration_count > 0 {
                p class="empty" {
                    (registration_count) " snake(s) are registered — game type and board size can no longer be changed."
                }
            }

            form class="tform" action={"/tournaments/"(tournament_id)"/settings"} method="post" {
                (tournament_form_fields(Some(&t)))

                div class="actions" {
                    button type="submit" class="btn solid" { "Update Tournament" }
                    a href={"/tournaments/"(tournament_id)} class="btn" { "Cancel" }
                }
            }
        }),
    ))
}

/// POST /tournaments/{id}/settings — update settings (owner only).
pub async fn update_settings(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<TournamentSettingsForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // One transaction for the whole handler: lock the tournament row, then
    // validate (ownership, status, the registration-based settings freeze)
    // against the locked row before writing.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin settings transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&mut *tx, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("You don't have permission to edit this tournament".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let edit_url = format!("/tournaments/{tournament_id}/edit");

    let registration_count = tournament::count_registrations(&mut *tx, tournament_id)
        .await
        .wrap_err("Failed to count registrations")?;

    let parsed = validate_tournament_params(
        &form.name,
        &form.description,
        form.required_participants,
        form.max_snakes_per_user,
    )
    .and_then(|()| {
        Ok((
            parse_game_type(&form.game_type)?,
            parse_board_size(&form.board_size)?,
        ))
    })
    .and_then(|(game_type, board_size)| {
        validate_settings_update(&t, registration_count > 0, &game_type, &board_size)?;
        Ok((game_type, board_size))
    });

    let (game_type, board_size) = match parsed {
        Ok(values) => values,
        Err(message) => {
            return flash_redirect(
                &state,
                session.session_id,
                message,
                session::FLASH_TYPE_ERROR,
                &edit_url,
            )
            .await;
        }
    };

    tournament::update_tournament_settings(
        &mut *tx,
        tournament_id,
        UpdateTournamentSettings {
            name: form.name.trim().to_string(),
            description: form.description_opt(),
            game_type,
            board_size,
            match_style: form.match_style,
            registration_status: form.registration_status,
            visibility: form.visibility,
            max_snakes_per_user: form.max_snakes_per_user,
            required_participants: form.required_participants,
        },
    )
    .await
    .wrap_err("Failed to update tournament settings")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit settings transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        "Tournament settings updated!".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &format!("/tournaments/{tournament_id}"),
    )
    .await
}

/// POST /tournaments/{id}/register — register one of the caller's snakes.
pub async fn register_snake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<RegisterForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let detail_url = format!("/tournaments/{tournament_id}");

    // One transaction for the whole handler: lock the tournament row, then
    // run every check (visibility, registration matrix, dupe, per-user and
    // total caps) against the locked row before inserting.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin registration transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Hidden tournaments 404 for outsiders, exactly like the detail page.
    if is_hidden_from(&mut *tx, &t, user.user_id).await? {
        return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }

    let is_owner = t.user_id == user.user_id;

    if !can_register(&t, is_owner) {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration is not open for this tournament".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let snake = battlesnake::get_battlesnake_by_id(&state.db, form.battlesnake_id)
        .await
        .wrap_err("Failed to fetch battlesnake")?;

    let Some(snake) = snake.filter(|s| s.user_id == user.user_id) else {
        return flash_redirect(
            &state,
            session.session_id,
            "You can only register your own battlesnakes".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    };

    let registration =
        match register_snake_in_tx(&mut tx, &t, user.user_id, snake.battlesnake_id, &snake.name)
            .await?
        {
            Ok(registration) => registration,
            Err(message) => {
                // Dropping the transaction rolls it back.
                return flash_redirect(
                    &state,
                    session.session_id,
                    message,
                    session::FLASH_TYPE_ERROR,
                    &detail_url,
                )
                .await;
            }
        };

    tx.commit()
        .await
        .wrap_err("Failed to commit registration transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        format!("Registered {} (seed {})", snake.name, registration.seed),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/unregister — remove a registration (snake owner or
/// tournament owner) and renumber remaining seeds.
pub async fn unregister_snake(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<UnregisterForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let detail_url = format!("/tournaments/{tournament_id}");

    // One transaction for the whole handler: lock the tournament row so the
    // status check, registration lookup, and seed renumbering can't race
    // other mutations.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin unregister transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    // Hidden tournaments 404 for outsiders, exactly like the detail page.
    if is_hidden_from(&mut *tx, &t, user.user_id).await? {
        return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
    }

    if !registrations_editable(t.status) {
        return flash_redirect(
            &state,
            session.session_id,
            "Snakes can no longer be unregistered from this tournament".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let registration = tournament::get_registration_by_id(&mut *tx, form.registration_id)
        .await
        .wrap_err("Failed to fetch registration")?
        .filter(|r| r.tournament_id == tournament_id);

    let Some(registration) = registration else {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration not found".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    };

    let is_tournament_owner = t.user_id == user.user_id;
    let is_snake_owner = registration.user_id == user.user_id;
    if !is_tournament_owner && !is_snake_owner {
        return flash_redirect(
            &state,
            session.session_id,
            "You don't have permission to remove this registration".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    tournament::delete_registration_and_renumber(
        &mut tx,
        tournament_id,
        registration.registration_id,
    )
    .await
    .wrap_err("Failed to unregister snake")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit unregister transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        "Snake unregistered".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/seed — move a registration to a new seed (owner only).
pub async fn move_seed(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<SeedForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // One transaction for the whole handler: lock the tournament row so seed
    // shuffles serialize with registrations, unregistrations, and each other.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin seed move transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&mut *tx, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("Only the tournament owner can change seeds".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    if !registrations_editable(t.status) {
        return flash_redirect(
            &state,
            session.session_id,
            "Seeds can only be changed before the tournament starts".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    // Single in-transaction fetch: move_registration_seed reports a missing
    // registration as `false` and we surface it as a flash error.
    let moved = tournament::move_registration_seed(
        &mut tx,
        tournament_id,
        form.registration_id,
        form.new_seed,
    )
    .await
    .wrap_err("Failed to move seed")?;

    if !moved {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration not found".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    tx.commit()
        .await
        .wrap_err("Failed to commit seed move transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        "Seed updated".to_string(),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/status — lifecycle transitions (owner only).
///
/// NOTE: `start` (registration -> in_progress) is intentionally not
/// implemented here — bracket generation lands in a separate PR.
pub async fn update_status(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<StatusForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // One transaction for the whole handler: lock the tournament row so the
    // transition check and the status write can't race another change. The
    // compare-and-swap in set_tournament_status is a second line of defense.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin status transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&mut *tx, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("Only the tournament owner can change its status".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    let next_status = match form.action.as_str() {
        "open_registration" => TournamentStatus::Registration,
        "cancel" => TournamentStatus::Canceled,
        other => {
            return flash_redirect(
                &state,
                session.session_id,
                format!("Unknown action: {other}"),
                session::FLASH_TYPE_ERROR,
                &detail_url,
            )
            .await;
        }
    };

    if !t.status.can_transition_to(next_status) {
        return flash_redirect(
            &state,
            session.session_id,
            format!(
                "Cannot move tournament from {} to {}",
                t.status.as_str(),
                next_status.as_str()
            ),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    // Compare-and-swap on the status we validated the transition from; the
    // row lock means `t.status` can't have changed underneath us.
    tournament::set_tournament_status(&mut *tx, tournament_id, next_status, t.status)
        .await
        .wrap_err("Failed to update tournament status")?;

    tx.commit()
        .await
        .wrap_err("Failed to commit status transaction")?;

    let message = match next_status {
        TournamentStatus::Registration => "Registration is now open!".to_string(),
        TournamentStatus::Canceled => "Tournament canceled".to_string(),
        _ => format!("Tournament moved to {}", next_status.as_str()),
    };

    flash_redirect(
        &state,
        session.session_id,
        message,
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// The start flow against the locked tournament row: read the registrations
/// in-transaction, validate, persist the bracket, and CAS the tournament into
/// `in_progress` at round 1. Returns `Ok(Err(message))` for user-facing
/// refusals (the caller flashes and drops the transaction to roll back).
///
/// `t` must come from `get_tournament_for_update` on this transaction:
/// register/unregister take the same row lock, so the registration set read
/// here can't change between validation and the bracket write — that's what
/// keeps the bracket consistent with its seeds.
///
/// The row lock also serializes concurrent starts (the loser re-reads
/// `in_progress` and is refused by `validate_start`), but as a second line of
/// defense a `(tournament_id, round, position)` unique violation from a
/// bracket that already exists is surfaced as a friendly refusal instead of
/// a 500.
async fn start_tournament_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    t: &Tournament,
) -> cja::Result<Result<usize, String>> {
    let registrations = tournament::get_registrations_for_tournament(&mut **tx, t.tournament_id)
        .await
        .wrap_err("Failed to fetch registrations")?;

    if let Err(message) = validate_start(t, registrations.len() as i64) {
        return Ok(Err(message));
    }

    if let Err(err) = persist_bracket(tx, t.tournament_id, &registrations).await {
        if crate::tournament_match::is_unique_violation(
            &err,
            "tournament_matches_tournament_id_round_position_key",
        ) {
            return Ok(Err("Tournament already started".to_string()));
        }
        return Err(err).wrap_err("Failed to generate bracket");
    }

    // CAS on the locked row's status (validate_start guarantees it's
    // `registration` — that's the only status that can transition to
    // `in_progress`).
    tournament::set_tournament_status(
        &mut **tx,
        t.tournament_id,
        TournamentStatus::InProgress,
        t.status,
    )
    .await
    .wrap_err("Failed to set tournament in progress")?;
    tournament::set_tournament_current_round(&mut **tx, t.tournament_id, 1)
        .await
        .wrap_err("Failed to set current round")?;

    Ok(Ok(registrations.len()))
}

/// POST /tournaments/{id}/start — generate the bracket and begin round 1
/// (owner only, BS-022).
pub async fn start_tournament(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // One transaction for the whole handler, with the tournament row locked
    // FIRST: owner/status checks, the registration read, bracket
    // persistence, and the status/round writes all see the same state, and
    // register/unregister serialize on the same lock.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin start transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&mut *tx, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("Only the tournament owner can start it".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    match start_tournament_in_tx(&mut tx, &t).await? {
        Err(message) => {
            // Dropping the transaction rolls it back.
            drop(tx);
            flash_redirect(
                &state,
                session.session_id,
                message,
                session::FLASH_TYPE_ERROR,
                &detail_url,
            )
            .await
        }
        Ok(snake_count) => {
            tx.commit()
                .await
                .wrap_err("Failed to commit start transaction")?;

            flash_redirect(
                &state,
                session.session_id,
                format!(
                    "Tournament started with {snake_count} snakes! Use \"Run Round\" to play each round."
                ),
                session::FLASH_TYPE_SUCCESS,
                &detail_url,
            )
            .await
        }
    }
}

/// POST /tournaments/{id}/run-round — kick off the current round's matches
/// (owner only, BS-023). The actual work happens in RunTournamentRoundJob;
/// the job is enqueued outside any transaction (matchmaker pattern).
pub async fn run_round(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let t = tournament::get_tournament_by_id(&state.db, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        return Err("Only the tournament owner can run rounds".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    if t.status != TournamentStatus::InProgress {
        return flash_redirect(
            &state,
            session.session_id,
            "Rounds can only be run while the tournament is in progress".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    // Every game a round creates is charged to the owner's game-creation
    // budget (see tournament_match); this gate is where that budget is
    // enforced, since the match jobs themselves must never fail mid-flight.
    // A round can overshoot the limit by its own games (they're charged as
    // they're created, not up front), but the next click gets stopped here —
    // which is what turns "reset and re-run forever" from unlimited games
    // into the same throughput cap everyone else has.
    let limit = state.config.game_creation_rate_limit;
    let window_minutes = state.config.game_creation_rate_limit_window_minutes;
    let attempts = crate::models::rate_limit::count_recent_game_creation_attempts(
        &state.db,
        user.user_id,
        window_minutes,
    )
    .await
    .wrap_err("Failed to count game creation attempts")?;
    if attempts >= limit {
        tracing::warn!(
            event_type = "game_creation_rate_limited",
            user_id = %user.user_id,
            attempts = attempts,
            limit = limit,
            source = "tournament",
            "tournament round blocked by game creation rate limit"
        );
        return flash_redirect(
            &state,
            session.session_id,
            format!(
                "You're over the game-creation limit ({limit} games per {window_minutes} \
                 minutes, tournament games included). Wait a bit before running the next round."
            ),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    // Pin the job to the round we just validated and are about to tell the
    // owner about: if the tournament moves on (or is reset and restarted)
    // before the job runs, the stale payload no-ops instead of auto-running
    // a round the owner never clicked.
    cja::jobs::Job::enqueue(
        crate::jobs::RunTournamentRoundJob {
            tournament_id,
            round: t.current_round,
        },
        state.clone(),
        format!(
            "Owner ran round {} of tournament {tournament_id}",
            t.current_round
        ),
    )
    .await
    .wrap_err("Failed to enqueue round job")?;

    flash_redirect(
        &state,
        session.session_id,
        format!("Round {} started", t.current_round),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/reset — delete the bracket and reopen registration
/// (owner only, BS-024). Matches (and their participants/games via FK
/// cascade) are deleted; registrations are preserved.
///
/// Reset policy for in-flight games: we deliberately do NOT cancel them.
/// Games are hard-bounded by `MAX_TURNS` so they always terminate on their
/// own, and every downstream consumer tolerates the deleted match data:
/// - the game-completion hook (`resolve_finished_match_game`) no-ops for
///   games whose `match_games` row was cascaded away,
/// - stranded `RunMatchJob`s warn and no-op when their match is gone,
/// - the stuck-match sweeper only re-enqueues matches that still exist in
///   `in_progress`, and
/// - a stranded `RunTournamentRoundJob` carries the round it was enqueued
///   for and no-ops when it no longer matches `current_round`.
///
/// So a reset leaves running games to finish harmlessly; their results just
/// don't count toward anything.
pub async fn reset_tournament(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    // One transaction with the tournament row locked first, like every other
    // mutating handler: the status guard and the bracket delete can't race a
    // concurrent status change (e.g. a double-clicked reset).
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin reset transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&mut *tx, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("Only the tournament owner can reset it".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    let detail_url = format!("/tournaments/{tournament_id}");

    // Completed tournaments are intentionally not resettable: once a
    // champion is decided the result is final (cancel is the only way out).
    if t.status != TournamentStatus::InProgress {
        return flash_redirect(
            &state,
            session.session_id,
            "Only an in-progress tournament can be reset".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    tournament::delete_matches_for_tournament(&mut *tx, tournament_id)
        .await
        .wrap_err("Failed to delete tournament matches")?;
    tournament::set_tournament_current_round(&mut *tx, tournament_id, 0)
        .await
        .wrap_err("Failed to reset current round")?;
    tournament::set_tournament_status(
        &mut *tx,
        tournament_id,
        TournamentStatus::Registration,
        TournamentStatus::InProgress,
    )
    .await
    .wrap_err("Failed to reopen registration")?;
    tx.commit()
        .await
        .wrap_err("Failed to commit reset transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        "Tournament reset — the bracket was cleared and registration is open again. \
         Any games still running will finish on their own but won't count."
            .to_string(),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

/// POST /tournaments/{id}/import-leaderboard — the "leaderboards feed
/// tournaments" qualifier flow. Registers the top N ranked snakes that aren't
/// already registered, respecting max_snakes_per_user, seeded in rank order
/// after existing registrations.
#[allow(clippy::too_many_lines)]
pub async fn import_leaderboard(
    State(state): State<AppState>,
    CurrentUserWithSession { user, session }: CurrentUserWithSession,
    Path(tournament_id): Path<Uuid>,
    Form(form): Form<ImportLeaderboardForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let detail_url = format!("/tournaments/{tournament_id}");

    // One transaction for the whole handler: lock the tournament row, then
    // snapshot registrations, select candidates, and insert — all against the
    // locked row so a racing self-registration can't collide with the import.
    let mut tx = state
        .db
        .begin()
        .await
        .wrap_err("Failed to begin import transaction")?;

    let t = tournament::get_tournament_for_update(&mut tx, tournament_id)
        .await
        .wrap_err("Failed to fetch tournament")?
        .ok_or_else(|| "Tournament not found".to_string())
        .with_status(StatusCode::NOT_FOUND)?;

    if t.user_id != user.user_id {
        // Hidden tournaments 404 for outsiders — a 403 here would confirm the
        // tournament exists, distinguishing valid hidden UUIDs from noise.
        if is_hidden_from(&mut *tx, &t, user.user_id).await? {
            return Err("Tournament not found".to_string()).with_status(StatusCode::NOT_FOUND);
        }
        return Err("Only the tournament owner can import from a leaderboard".to_string())
            .with_status(StatusCode::FORBIDDEN);
    }

    // Import obeys the same registration matrix as manual registration by the
    // owner: pre-start status only, and never when registration is closed.
    if !registrations_editable(t.status) {
        return flash_redirect(
            &state,
            session.session_id,
            "Snakes can only be imported before the tournament starts".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    if t.registration_status == RegistrationStatus::Closed {
        return flash_redirect(
            &state,
            session.session_id,
            "Registration is closed for this tournament".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let Some(lb) = leaderboard::get_leaderboard_by_id(&state.db, form.leaderboard_id)
        .await
        .wrap_err("Failed to fetch leaderboard")?
    else {
        return flash_redirect(
            &state,
            session.session_id,
            "Leaderboard not found".to_string(),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    };

    if lb.disabled_at.is_some() {
        return flash_redirect(
            &state,
            session.session_id,
            format!("{} is disabled and cannot be imported from", lb.name),
            session::FLASH_TYPE_ERROR,
            &detail_url,
        )
        .await;
    }

    let count = form.count.clamp(1, MAX_IMPORT_COUNT);

    let ranked = leaderboard::get_ranked_entries(
        &state.db,
        lb.leaderboard_id,
        leaderboard::LeaderboardSort::Rating,
    )
    .await
    .wrap_err("Failed to fetch ranked leaderboard entries")?;

    // Snapshot the registrations inside the locked transaction so duplicate
    // skipping and the per-user/total caps are enforced against reality.
    let existing = tournament::get_registrations_for_tournament(&mut *tx, tournament_id)
        .await
        .wrap_err("Failed to fetch existing registrations")?;

    let candidates = select_import_candidates(&ranked, &existing, t.max_snakes_per_user, count);

    if candidates.is_empty() {
        return flash_redirect(
            &state,
            session.session_id,
            format!("No eligible snakes to import from {}", lb.name),
            session::FLASH_TYPE_INFO,
            &detail_url,
        )
        .await;
    }

    // Register all selected snakes, appended after the existing registrations
    // in rating-rank order.
    let imported = candidates.len();
    let mut seed = tournament::next_seed(&mut *tx, tournament_id)
        .await
        .wrap_err("Failed to compute seed during import")?;
    for (battlesnake_id, owner_user_id) in candidates {
        tournament::create_registration(
            &mut *tx,
            tournament_id,
            battlesnake_id,
            owner_user_id,
            seed,
        )
        .await
        .wrap_err("Failed to register imported snake")?;
        seed += 1;
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit import transaction")?;

    flash_redirect(
        &state,
        session.session_id,
        format!("Imported {imported} snake(s) from {}", lb.name),
        session::FLASH_TYPE_SUCCESS,
        &detail_url,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::tournament::TournamentRegistration;
    use sqlx::PgPool;

    fn test_tournament(
        status: TournamentStatus,
        registration_status: RegistrationStatus,
        visibility: TournamentVisibility,
    ) -> Tournament {
        Tournament {
            tournament_id: Uuid::new_v4(),
            name: "Test Tournament".to_string(),
            description: None,
            user_id: Uuid::new_v4(),
            game_type: GameType::Standard,
            board_size: GameBoardSize::Medium,
            registration_status,
            visibility,
            status,
            match_style: MatchStyle::SingleGame,
            max_snakes_per_user: 1,
            required_participants: 2,
            current_round: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn registrations_editable_only_before_start() {
        assert!(registrations_editable(TournamentStatus::Created));
        assert!(registrations_editable(TournamentStatus::Registration));
        assert!(!registrations_editable(TournamentStatus::InProgress));
        assert!(!registrations_editable(TournamentStatus::Completed));
        assert!(!registrations_editable(TournamentStatus::Canceled));
    }

    #[test]
    fn can_register_permission_matrix() {
        use RegistrationStatus::{Closed, Open, OwnerOnly};
        use TournamentStatus::{Canceled, Completed, Created, InProgress, Registration};
        use TournamentVisibility::{ParticipantsOnly, Public};

        // (status, registration_status, visibility, is_owner, expected)
        let cases = [
            // Open + public: anyone, but only while created/registration
            (Created, Open, Public, false, true),
            (Created, Open, Public, true, true),
            (Registration, Open, Public, false, true),
            (Registration, Open, Public, true, true),
            (InProgress, Open, Public, false, false),
            (InProgress, Open, Public, true, false),
            (Completed, Open, Public, true, false),
            (Canceled, Open, Public, true, false),
            // OwnerOnly: only the owner, still gated by status
            (Created, OwnerOnly, Public, false, false),
            (Created, OwnerOnly, Public, true, true),
            (Registration, OwnerOnly, Public, false, false),
            (Registration, OwnerOnly, Public, true, true),
            (InProgress, OwnerOnly, Public, true, false),
            (Canceled, OwnerOnly, Public, true, false),
            // Closed: nobody, not even the owner
            (Created, Closed, Public, false, false),
            (Created, Closed, Public, true, false),
            (Registration, Closed, Public, false, false),
            (Registration, Closed, Public, true, false),
            (InProgress, Closed, Public, true, false),
            // ParticipantsOnly: only the OWNER may register snakes — an
            // outsider self-registering would become a "participant" and
            // defeat the visibility 404.
            (Created, Open, ParticipantsOnly, false, false),
            (Created, Open, ParticipantsOnly, true, true),
            (Registration, Open, ParticipantsOnly, false, false),
            (Registration, Open, ParticipantsOnly, true, true),
            (Registration, OwnerOnly, ParticipantsOnly, false, false),
            (Registration, OwnerOnly, ParticipantsOnly, true, true),
            (Registration, Closed, ParticipantsOnly, false, false),
            (Registration, Closed, ParticipantsOnly, true, false),
            (InProgress, Open, ParticipantsOnly, true, false),
            (Canceled, Open, ParticipantsOnly, true, false),
        ];

        for (status, registration_status, visibility, is_owner, expected) in cases {
            let t = test_tournament(status, registration_status, visibility);
            assert_eq!(
                can_register(&t, is_owner),
                expected,
                "status={status:?} registration_status={registration_status:?} visibility={visibility:?} is_owner={is_owner}"
            );
        }
    }

    #[test]
    fn can_view_public_tournaments() {
        let t = test_tournament(
            TournamentStatus::Created,
            RegistrationStatus::Open,
            TournamentVisibility::Public,
        );
        assert!(can_view(&t, None, &[]));
        assert!(can_view(&t, Some(Uuid::new_v4()), &[]));
    }

    #[test]
    fn can_view_participants_only_tournaments() {
        let t = test_tournament(
            TournamentStatus::Created,
            RegistrationStatus::Open,
            TournamentVisibility::ParticipantsOnly,
        );
        let participant = Uuid::new_v4();
        let stranger = Uuid::new_v4();

        // Anonymous and non-participants are denied
        assert!(!can_view(&t, None, &[participant]));
        assert!(!can_view(&t, Some(stranger), &[participant]));

        // The owner and registered participants can view
        assert!(can_view(&t, Some(t.user_id), &[participant]));
        assert!(can_view(&t, Some(participant), &[participant]));
    }

    #[test]
    fn validate_tournament_params_rules() {
        assert!(validate_tournament_params("Snake Cup", "", 2, 1).is_ok());
        assert!(validate_tournament_params("Snake Cup", "A fine cup", 8, 3).is_ok());

        assert!(validate_tournament_params("", "", 2, 1).is_err());
        assert!(validate_tournament_params("   ", "", 2, 1).is_err());
        assert!(validate_tournament_params("Snake Cup", "", 1, 1).is_err());
        assert!(validate_tournament_params("Snake Cup", "", 0, 1).is_err());
        assert!(validate_tournament_params("Snake Cup", "", 2, 0).is_err());
        assert!(validate_tournament_params("Snake Cup", "", 2, -1).is_err());
    }

    #[test]
    fn validate_tournament_params_upper_limits() {
        let max_name = "n".repeat(MAX_NAME_CHARS);
        let max_description = "d".repeat(MAX_DESCRIPTION_CHARS);

        // At the limits: fine.
        assert!(
            validate_tournament_params(
                &max_name,
                &max_description,
                MAX_REQUIRED_PARTICIPANTS,
                MAX_SNAKES_PER_USER_LIMIT
            )
            .is_ok()
        );
        // Name is measured after trimming.
        assert!(validate_tournament_params(&format!("  {max_name}  "), "", 2, 1).is_ok());

        // One past each limit: refused.
        let long_name = "n".repeat(MAX_NAME_CHARS + 1);
        assert!(validate_tournament_params(&long_name, "", 2, 1).is_err());

        let long_description = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert!(validate_tournament_params("Snake Cup", &long_description, 2, 1).is_err());

        assert!(
            validate_tournament_params("Snake Cup", "", MAX_REQUIRED_PARTICIPANTS + 1, 1).is_err()
        );
        assert!(
            validate_tournament_params("Snake Cup", "", 2, MAX_SNAKES_PER_USER_LIMIT + 1).is_err()
        );
    }

    fn ranked_entry(battlesnake_id: Uuid, user_id: Uuid) -> leaderboard::RankedEntry {
        leaderboard::RankedEntry {
            leaderboard_entry_id: Uuid::new_v4(),
            battlesnake_id,
            user_id,
            display_score: 25.0,
            games_played: 50,
            first_place_finishes: 10,
            non_first_finishes: 40,
            mu: 25.0,
            sigma: 8.333,
            snake_name: "ranked-snake".to_string(),
            snake_color: "#888888".to_string(),
            owner_login: "ranked-owner".to_string(),
        }
    }

    fn registration(battlesnake_id: Uuid, user_id: Uuid, seed: i32) -> TournamentRegistration {
        TournamentRegistration {
            registration_id: Uuid::new_v4(),
            tournament_id: Uuid::new_v4(),
            battlesnake_id,
            user_id,
            seed,
            registered_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn import_candidates_skip_duplicates_and_capped_owners() {
        let user_a = Uuid::new_v4();
        let user_b = Uuid::new_v4();
        let already_registered = Uuid::new_v4();
        let a_snake_2 = Uuid::new_v4();
        let b_snake = Uuid::new_v4();

        let existing = vec![registration(already_registered, user_a, 1)];
        let ranked = vec![
            ranked_entry(already_registered, user_a), // dupe: skipped
            ranked_entry(a_snake_2, user_a),          // user_a at cap 1: skipped
            ranked_entry(b_snake, user_b),            // selected
        ];

        let candidates = select_import_candidates(&ranked, &existing, 1, 10);
        assert_eq!(candidates, vec![(b_snake, user_b)]);
    }

    #[test]
    fn import_candidates_stop_at_requested_count() {
        let ranked: Vec<_> = (0..10)
            .map(|_| ranked_entry(Uuid::new_v4(), Uuid::new_v4()))
            .collect();

        let candidates = select_import_candidates(&ranked, &[], 1, 3);
        assert_eq!(candidates.len(), 3);
        // Rank order preserved.
        assert_eq!(candidates[0].0, ranked[0].battlesnake_id);
        assert_eq!(candidates[2].0, ranked[2].battlesnake_id);
    }

    #[test]
    fn import_candidates_never_exceed_total_registration_cap() {
        // 126 existing registrations: only 2 slots left no matter the ask.
        let existing: Vec<_> = (0..126)
            .map(|i| registration(Uuid::new_v4(), Uuid::new_v4(), i + 1))
            .collect();
        let ranked: Vec<_> = (0..10)
            .map(|_| ranked_entry(Uuid::new_v4(), Uuid::new_v4()))
            .collect();

        let candidates = select_import_candidates(&ranked, &existing, 1, 10);
        assert_eq!(candidates.len(), 2);

        // Already full: nothing to import.
        let full: Vec<_> = (0..128)
            .map(|i| registration(Uuid::new_v4(), Uuid::new_v4(), i + 1))
            .collect();
        let candidates = select_import_candidates(&ranked, &full, 1, 10);
        assert!(candidates.is_empty());
    }

    #[test]
    fn settings_update_blocked_after_start() {
        for status in [
            TournamentStatus::InProgress,
            TournamentStatus::Completed,
            TournamentStatus::Canceled,
        ] {
            let t = test_tournament(
                status,
                RegistrationStatus::Open,
                TournamentVisibility::Public,
            );
            assert!(
                validate_settings_update(&t, false, &t.game_type.clone(), &t.board_size.clone())
                    .is_err(),
                "settings should be locked in status {status:?}"
            );
        }
    }

    #[test]
    fn settings_update_freezes_game_config_once_registered() {
        let t = test_tournament(
            TournamentStatus::Registration,
            RegistrationStatus::Open,
            TournamentVisibility::Public,
        );

        // No registrations: everything editable
        assert!(
            validate_settings_update(&t, false, &GameType::Royale, &GameBoardSize::Large).is_ok()
        );

        // With registrations: game_type/board_size are frozen
        assert!(
            validate_settings_update(&t, true, &GameType::Royale, &GameBoardSize::Medium).is_err()
        );
        assert!(
            validate_settings_update(&t, true, &GameType::Standard, &GameBoardSize::Large).is_err()
        );

        // With registrations but unchanged game config: fine
        assert!(
            validate_settings_update(&t, true, &GameType::Standard, &GameBoardSize::Medium).is_ok()
        );
    }

    #[test]
    fn parse_game_type_accepts_dropdown_values_only() {
        assert_eq!(parse_game_type("Standard").unwrap(), GameType::Standard);
        assert_eq!(parse_game_type("Royale").unwrap(), GameType::Royale);
        assert_eq!(
            parse_game_type("Constrictor").unwrap(),
            GameType::Constrictor
        );
        assert_eq!(parse_game_type("Snail Mode").unwrap(), GameType::SnailMode);
        assert!(parse_game_type("Wrapped").is_err());
        assert!(parse_game_type("").is_err());
    }

    #[test]
    fn parse_board_size_accepts_dropdown_values_only() {
        assert_eq!(parse_board_size("7x7").unwrap(), GameBoardSize::Small);
        assert_eq!(parse_board_size("11x11").unwrap(), GameBoardSize::Medium);
        assert_eq!(parse_board_size("19x19").unwrap(), GameBoardSize::Large);
        assert!(parse_board_size("25x25").is_err());
        assert!(parse_board_size("").is_err());
    }

    // --- DB tests: the registration caps and visibility concealment are
    // enforced against in-transaction state under the tournament row lock. ---

    // Raw (non-macro) queries so the fixtures don't need entries in the sqlx
    // offline cache.
    async fn fixture_user(pool: &PgPool, github_id: i64, login: &str) -> cja::Result<Uuid> {
        let user_id = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, $3) RETURNING user_id",
        )
        .bind(github_id)
        .bind(login)
        .bind("test-token")
        .fetch_one(pool)
        .await?;
        Ok(user_id)
    }

    async fn fixture_snake(pool: &PgPool, user_id: Uuid, name: &str) -> cja::Result<Uuid> {
        let battlesnake_id = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, $3) RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind(name)
        .bind("http://example.com")
        .fetch_one(pool)
        .await?;
        Ok(battlesnake_id)
    }

    fn create_params(max_snakes_per_user: i32) -> CreateTournament {
        CreateTournament {
            name: "Cap Test".to_string(),
            description: None,
            game_type: GameType::Standard,
            board_size: GameBoardSize::Medium,
            registration_status: RegistrationStatus::Open,
            visibility: TournamentVisibility::Public,
            match_style: MatchStyle::SingleGame,
            max_snakes_per_user,
            required_participants: 2,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn per_user_cap_and_dupes_enforced_inside_locked_transaction(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = fixture_user(&pool, 1, "capped-user").await?;
        let snake_1 = fixture_snake(&pool, user_id, "snake-1").await?;
        let snake_2 = fixture_snake(&pool, user_id, "snake-2").await?;
        let t = tournament::create_tournament(&pool, user_id, create_params(1)).await?;

        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");

        let first = register_snake_in_tx(&mut tx, &locked, user_id, snake_1, "snake-1").await?;
        assert!(first.is_ok());

        // Same snake again: duplicate, refused on in-tx state (not committed).
        let dupe = register_snake_in_tx(&mut tx, &locked, user_id, snake_1, "snake-1").await?;
        let message = dupe.expect_err("duplicate registration should be refused");
        assert!(
            message.contains("already registered"),
            "unexpected refusal message: {message}"
        );

        // Second snake for the same user: over the per-user cap of 1, and the
        // count it trips on is the in-transaction one.
        let second = register_snake_in_tx(&mut tx, &locked, user_id, snake_2, "snake-2").await?;
        let message = second.expect_err("second registration should hit the per-user cap");
        assert!(
            message.contains("limit"),
            "unexpected refusal message: {message}"
        );

        tx.commit().await?;

        let total = tournament::count_registrations(&pool, t.tournament_id).await?;
        assert_eq!(total, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn total_registration_cap_refuses_the_129th_snake(pool: PgPool) -> cja::Result<()> {
        let owner = fixture_user(&pool, 999, "owner").await?;
        let t = tournament::create_tournament(&pool, owner, create_params(32)).await?;

        // Fill the tournament to the 128-snake cap: 4 users x 32 snakes.
        for u in 0..4i64 {
            let filler = fixture_user(&pool, u, &format!("filler-{u}")).await?;
            sqlx::query(
                "INSERT INTO battlesnakes (user_id, name, url)
                 SELECT $1, 'snake-' || g, 'http://example.com' FROM generate_series(1, 32) g",
            )
            .bind(filler)
            .execute(&pool)
            .await?;
        }
        sqlx::query(
            "INSERT INTO tournament_registrations (tournament_id, battlesnake_id, user_id, seed)
             SELECT $1, battlesnake_id, user_id,
                    (ROW_NUMBER() OVER (ORDER BY battlesnake_id))::int
             FROM battlesnakes WHERE user_id <> $2",
        )
        .bind(t.tournament_id)
        .bind(owner)
        .execute(&pool)
        .await?;
        assert_eq!(
            tournament::count_registrations(&pool, t.tournament_id).await?,
            MAX_TOTAL_REGISTRATIONS
        );

        let late_snake = fixture_snake(&pool, owner, "late-snake").await?;
        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");
        let result =
            register_snake_in_tx(&mut tx, &locked, owner, late_snake, "late-snake").await?;
        let message = result.expect_err("the 129th registration should be refused");
        assert!(
            message.contains("full"),
            "unexpected refusal message: {message}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn participants_only_tournaments_hidden_from_outsiders(pool: PgPool) -> cja::Result<()> {
        let owner = fixture_user(&pool, 1, "owner").await?;
        let participant = fixture_user(&pool, 2, "participant").await?;
        let outsider = fixture_user(&pool, 3, "outsider").await?;
        let snake = fixture_snake(&pool, participant, "participant-snake").await?;

        let mut params = create_params(1);
        params.visibility = TournamentVisibility::ParticipantsOnly;
        let t = tournament::create_tournament(&pool, owner, params).await?;
        tournament::create_registration(&pool, t.tournament_id, snake, participant, 1).await?;

        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");

        assert!(!is_hidden_from(&mut *tx, &locked, owner).await?);
        assert!(!is_hidden_from(&mut *tx, &locked, participant).await?);
        assert!(is_hidden_from(&mut *tx, &locked, outsider).await?);
        Ok(())
    }

    // --- DB tests: the start flow validates against the locked row and
    // in-transaction registrations, and refuses cleanly instead of 500ing. ---

    #[sqlx::test(migrations = "../migrations")]
    async fn start_with_one_registration_is_refused_under_the_lock(
        pool: PgPool,
    ) -> cja::Result<()> {
        let user_id = fixture_user(&pool, 10, "starter").await?;
        let snake = fixture_snake(&pool, user_id, "lonely-snake").await?;
        let t = tournament::create_tournament(&pool, user_id, create_params(2)).await?;
        tournament::set_tournament_status(
            &pool,
            t.tournament_id,
            TournamentStatus::Registration,
            TournamentStatus::Created,
        )
        .await?;
        tournament::create_registration(&pool, t.tournament_id, snake, user_id, 1).await?;

        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");
        let message = start_tournament_in_tx(&mut tx, &locked)
            .await?
            .expect_err("one registration should not be enough to start");
        assert!(
            message.contains("At least 2"),
            "unexpected refusal message: {message}"
        );
        drop(tx); // roll back

        // Nothing was persisted: still in registration, no bracket.
        let reloaded = tournament::get_tournament_by_id(&pool, t.tournament_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, TournamentStatus::Registration);
        assert!(
            tournament::get_matches_for_tournament(&pool, t.tournament_id)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn double_start_gets_a_friendly_refusal_not_a_raw_error(pool: PgPool) -> cja::Result<()> {
        let user_id = fixture_user(&pool, 11, "double-starter").await?;
        let t = tournament::create_tournament(&pool, user_id, create_params(2)).await?;
        tournament::set_tournament_status(
            &pool,
            t.tournament_id,
            TournamentStatus::Registration,
            TournamentStatus::Created,
        )
        .await?;
        for seed in 1..=2i32 {
            let snake = fixture_snake(&pool, user_id, &format!("snake-{seed}")).await?;
            tournament::create_registration(&pool, t.tournament_id, snake, user_id, seed).await?;
        }

        // First start succeeds and commits.
        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");
        let snake_count = start_tournament_in_tx(&mut tx, &locked)
            .await?
            .expect("first start should succeed");
        assert_eq!(snake_count, 2);
        tx.commit().await?;

        let reloaded = tournament::get_tournament_by_id(&pool, t.tournament_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, TournamentStatus::InProgress);
        assert_eq!(reloaded.current_round, 1);

        // Second start: the re-read under the lock sees in_progress and is
        // refused by validation (Ok(Err(..)), not Err(..)).
        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");
        let message = start_tournament_in_tx(&mut tx, &locked)
            .await?
            .expect_err("second start should be refused");
        assert!(
            message.contains("cannot start from status"),
            "unexpected refusal message: {message}"
        );
        drop(tx);

        // Belt-and-suspenders: if a bracket already exists anyway (simulated
        // by forcing the status back without clearing the matches), the
        // unique violation surfaces as the friendly message, not a raw error.
        tournament::set_tournament_status(
            &pool,
            t.tournament_id,
            TournamentStatus::Registration,
            TournamentStatus::InProgress,
        )
        .await?;
        let mut tx = pool.begin().await?;
        let locked = tournament::get_tournament_for_update(&mut tx, t.tournament_id)
            .await?
            .expect("tournament exists");
        let message = start_tournament_in_tx(&mut tx, &locked)
            .await?
            .expect_err("starting over an existing bracket should be refused");
        assert_eq!(message, "Tournament already started");
        Ok(())
    }

    #[test]
    fn validate_start_requires_registration_status() {
        for status in [
            TournamentStatus::Created,
            TournamentStatus::InProgress,
            TournamentStatus::Completed,
            TournamentStatus::Canceled,
        ] {
            let t = test_tournament(
                status,
                RegistrationStatus::Open,
                TournamentVisibility::Public,
            );
            assert!(
                validate_start(&t, 8).is_err(),
                "start should be rejected from status {status:?}"
            );
        }

        let t = test_tournament(
            TournamentStatus::Registration,
            RegistrationStatus::Open,
            TournamentVisibility::Public,
        );
        assert!(validate_start(&t, 8).is_ok());
    }

    #[test]
    fn validate_start_requires_enough_participants() {
        let mut t = test_tournament(
            TournamentStatus::Registration,
            RegistrationStatus::Open,
            TournamentVisibility::Public,
        );

        // required_participants = 2 (default)
        assert!(validate_start(&t, 0).is_err());
        assert!(validate_start(&t, 1).is_err());
        assert!(validate_start(&t, 2).is_ok());
        assert!(validate_start(&t, 3).is_ok());

        // Higher bar: must meet required_participants
        t.required_participants = 8;
        assert!(validate_start(&t, 7).is_err());
        assert!(validate_start(&t, 8).is_ok());
        assert!(validate_start(&t, 9).is_ok());

        // Degenerate config below 2 still needs 2 snakes for a bracket
        t.required_participants = 0;
        assert!(validate_start(&t, 1).is_err());
        assert!(validate_start(&t, 2).is_ok());
    }

    #[test]
    fn round_labels_name_the_final() {
        assert_eq!(round_label(1, 1), "Final"); // 2-snake tournament
        assert_eq!(round_label(1, 3), "Round 1");
        assert_eq!(round_label(2, 3), "Round 2");
        assert_eq!(round_label(3, 3), "Final");
    }

    #[test]
    fn win_count_counts_only_the_participants_wins() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let winners = [Some(a), None, Some(b), Some(a)];

        assert_eq!(win_count(&winners, a), 2);
        assert_eq!(win_count(&winners, b), 1);
        assert_eq!(win_count(&winners, Uuid::new_v4()), 0);
        assert_eq!(win_count(&[], a), 0);
        assert_eq!(win_count(&[None, None], a), 0);
    }

    #[test]
    fn tie_games_are_distinguished_from_in_flight_games() {
        let winner = Some(Uuid::new_v4());

        // A decided game is never a tie.
        assert!(!is_tie_game(winner, false, MatchStatus::InProgress));
        assert!(!is_tie_game(winner, true, MatchStatus::Completed));

        // No winner + a later game exists: this game finished as a tie.
        assert!(is_tie_game(None, true, MatchStatus::InProgress));

        // No winner on the last game: tie only if the match is over.
        assert!(!is_tie_game(None, false, MatchStatus::InProgress));
        assert!(!is_tie_game(None, false, MatchStatus::Scheduled));
        assert!(is_tie_game(None, false, MatchStatus::Completed));
    }
}
