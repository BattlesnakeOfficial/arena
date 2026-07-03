use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{Executor, PgPool, Postgres, Type};
use std::str::FromStr;
use uuid::Uuid;

use crate::models::game::{GameBoardSize, GameType};

// Lifecycle of a tournament. Transitions are enforced by `can_transition_to`.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type, Default)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TournamentStatus {
    #[default]
    Created,
    Registration,
    InProgress,
    Completed,
    Canceled,
}

impl TournamentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TournamentStatus::Created => "created",
            TournamentStatus::Registration => "registration",
            TournamentStatus::InProgress => "in_progress",
            TournamentStatus::Completed => "completed",
            TournamentStatus::Canceled => "canceled",
        }
    }

    // Canceled is a terminal state reachable from anywhere; everything else
    // moves strictly forward. Reset (in_progress -> registration) is allowed
    // so owners can regenerate a bracket before results matter.
    pub fn can_transition_to(&self, next: TournamentStatus) -> bool {
        use TournamentStatus::{Canceled, Completed, Created, InProgress, Registration};
        match (self, next) {
            (Created | Registration | InProgress | Completed, Canceled) => true,
            (Created, Registration) => true,
            (Registration, InProgress) => true,
            (InProgress, Completed) => true,
            (InProgress, Registration) => true, // reset
            _ => false,
        }
    }
}

impl FromStr for TournamentStatus {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "created" => Ok(TournamentStatus::Created),
            "registration" => Ok(TournamentStatus::Registration),
            "in_progress" => Ok(TournamentStatus::InProgress),
            "completed" => Ok(TournamentStatus::Completed),
            "canceled" => Ok(TournamentStatus::Canceled),
            _ => Err(color_eyre::eyre::eyre!("Invalid tournament status: {}", s)),
        }
    }
}

// Who is allowed to register snakes while the tournament is open.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type, Default)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStatus {
    #[default]
    Open,
    Closed,
    OwnerOnly,
}

impl RegistrationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RegistrationStatus::Open => "open",
            RegistrationStatus::Closed => "closed",
            RegistrationStatus::OwnerOnly => "owner_only",
        }
    }
}

impl FromStr for RegistrationStatus {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "open" => Ok(RegistrationStatus::Open),
            "closed" => Ok(RegistrationStatus::Closed),
            "owner_only" => Ok(RegistrationStatus::OwnerOnly),
            _ => Err(color_eyre::eyre::eyre!(
                "Invalid registration status: {}",
                s
            )),
        }
    }
}

// Tournament visibility differs from battlesnake `Visibility`: the private
// variant is participants-only rather than owner-only.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type, Default)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TournamentVisibility {
    #[default]
    Public,
    ParticipantsOnly,
}

impl TournamentVisibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            TournamentVisibility::Public => "public",
            TournamentVisibility::ParticipantsOnly => "participants_only",
        }
    }
}

impl FromStr for TournamentVisibility {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "public" => Ok(TournamentVisibility::Public),
            "participants_only" => Ok(TournamentVisibility::ParticipantsOnly),
            _ => Err(color_eyre::eyre::eyre!(
                "Invalid tournament visibility: {}",
                s
            )),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type, Default)]
#[sqlx(type_name = "text")]
pub enum MatchStyle {
    #[default]
    #[sqlx(rename = "single_game")]
    #[serde(rename = "single_game")]
    SingleGame,
    #[sqlx(rename = "best_of_3")]
    #[serde(rename = "best_of_3")]
    BestOf3,
    #[sqlx(rename = "first_to_3")]
    #[serde(rename = "first_to_3")]
    FirstTo3,
}

impl MatchStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchStyle::SingleGame => "single_game",
            MatchStyle::BestOf3 => "best_of_3",
            MatchStyle::FirstTo3 => "first_to_3",
        }
    }

    /// Wins required to take the match.
    pub fn wins_needed(&self) -> i32 {
        match self {
            MatchStyle::SingleGame => 1,
            MatchStyle::BestOf3 => 2,
            MatchStyle::FirstTo3 => 3,
        }
    }

    /// Games played if neither side ever ties (ties force extra games).
    pub fn max_games_without_ties(&self) -> i32 {
        match self {
            MatchStyle::SingleGame => 1,
            MatchStyle::BestOf3 => 3,
            MatchStyle::FirstTo3 => 5,
        }
    }
}

impl FromStr for MatchStyle {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "single_game" => Ok(MatchStyle::SingleGame),
            "best_of_3" => Ok(MatchStyle::BestOf3),
            "first_to_3" => Ok(MatchStyle::FirstTo3),
            _ => Err(color_eyre::eyre::eyre!("Invalid match style: {}", s)),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type, Default)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    #[default]
    Scheduled,
    InProgress,
    Completed,
    Canceled,
}

impl MatchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchStatus::Scheduled => "scheduled",
            MatchStatus::InProgress => "in_progress",
            MatchStatus::Completed => "completed",
            MatchStatus::Canceled => "canceled",
        }
    }
}

impl FromStr for MatchStatus {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "scheduled" => Ok(MatchStatus::Scheduled),
            "in_progress" => Ok(MatchStatus::InProgress),
            "completed" => Ok(MatchStatus::Completed),
            "canceled" => Ok(MatchStatus::Canceled),
            _ => Err(color_eyre::eyre::eyre!("Invalid match status: {}", s)),
        }
    }
}

// How a participant slot gets filled: seeded in round 1, advanced as a
// winner/loser of a feeder match, or placed manually as a wildcard.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Type, Default)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ParticipantType {
    #[default]
    Seed,
    Winner,
    Loser,
    Wildcard,
}

impl ParticipantType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ParticipantType::Seed => "seed",
            ParticipantType::Winner => "winner",
            ParticipantType::Loser => "loser",
            ParticipantType::Wildcard => "wildcard",
        }
    }
}

impl FromStr for ParticipantType {
    type Err = color_eyre::eyre::Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "seed" => Ok(ParticipantType::Seed),
            "winner" => Ok(ParticipantType::Winner),
            "loser" => Ok(ParticipantType::Loser),
            "wildcard" => Ok(ParticipantType::Wildcard),
            _ => Err(color_eyre::eyre::eyre!("Invalid participant type: {}", s)),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Tournament {
    pub tournament_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub user_id: Uuid,
    pub game_type: GameType,
    pub board_size: GameBoardSize,
    pub registration_status: RegistrationStatus,
    pub visibility: TournamentVisibility,
    pub status: TournamentStatus,
    pub match_style: MatchStyle,
    pub max_snakes_per_user: i32,
    pub required_participants: i32,
    pub current_round: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TournamentRegistration {
    pub registration_id: Uuid,
    pub tournament_id: Uuid,
    pub battlesnake_id: Uuid,
    pub user_id: Uuid,
    pub seed: i32,
    pub registered_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TournamentMatch {
    pub match_id: Uuid,
    pub tournament_id: Uuid,
    pub round: i32,
    pub position: i32,
    pub status: MatchStatus,
    pub next_match_id: Option<Uuid>,
    pub winner_id: Option<Uuid>,
    pub visual_column: i32,
    pub visual_row: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MatchParticipant {
    pub match_participant_id: Uuid,
    pub match_id: Uuid,
    /// Which side of the match this participant occupies (1 or 2).
    pub slot: i16,
    /// None until the feeder match completes and the participant is known.
    pub battlesnake_id: Option<Uuid>,
    pub source_match_id: Option<Uuid>,
    pub participant_type: ParticipantType,
    pub seed_position: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MatchGame {
    pub match_game_id: Uuid,
    pub match_id: Uuid,
    pub game_id: Uuid,
    pub game_number: i32,
    /// None while the game is running, or for a tie.
    pub winner_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateTournament {
    pub name: String,
    pub description: Option<String>,
    pub game_type: GameType,
    pub board_size: GameBoardSize,
    pub registration_status: RegistrationStatus,
    pub visibility: TournamentVisibility,
    pub match_style: MatchStyle,
    pub max_snakes_per_user: i32,
    pub required_participants: i32,
}

// Database functions

// Intermediate row: game_type and board_size have catch-all variants
// (Other/Custom) so they can't derive sqlx::Type — they map via FromStr,
// matching the pattern in models/game.rs.
struct TournamentRow {
    tournament_id: Uuid,
    name: String,
    description: Option<String>,
    user_id: Uuid,
    game_type: String,
    board_size: String,
    registration_status: RegistrationStatus,
    visibility: TournamentVisibility,
    status: TournamentStatus,
    match_style: MatchStyle,
    max_snakes_per_user: i32,
    required_participants: i32,
    current_round: i32,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl TryFrom<TournamentRow> for Tournament {
    type Error = color_eyre::eyre::Report;

    fn try_from(row: TournamentRow) -> Result<Self, Self::Error> {
        let game_type = GameType::from_str(&row.game_type)
            .wrap_err_with(|| format!("Invalid game type: {}", row.game_type))?;
        let board_size = GameBoardSize::from_str(&row.board_size)
            .wrap_err_with(|| format!("Invalid board size: {}", row.board_size))?;

        Ok(Tournament {
            tournament_id: row.tournament_id,
            name: row.name,
            description: row.description,
            user_id: row.user_id,
            game_type,
            board_size,
            registration_status: row.registration_status,
            visibility: row.visibility,
            status: row.status,
            match_style: row.match_style,
            max_snakes_per_user: row.max_snakes_per_user,
            required_participants: row.required_participants,
            current_round: row.current_round,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

pub async fn create_tournament<'e, E>(
    executor: E,
    user_id: Uuid,
    data: CreateTournament,
) -> cja::Result<Tournament>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as!(
        TournamentRow,
        r#"
        INSERT INTO tournaments (
            name, description, user_id, game_type, board_size,
            registration_status, visibility, match_style,
            max_snakes_per_user, required_participants
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING
            tournament_id,
            name,
            description,
            user_id,
            game_type,
            board_size,
            registration_status as "registration_status: RegistrationStatus",
            visibility as "visibility: TournamentVisibility",
            status as "status: TournamentStatus",
            match_style as "match_style: MatchStyle",
            max_snakes_per_user,
            required_participants,
            current_round,
            created_at,
            updated_at
        "#,
        data.name,
        data.description,
        user_id,
        data.game_type.as_str(),
        data.board_size.as_str(),
        data.registration_status.as_str(),
        data.visibility.as_str(),
        data.match_style.as_str(),
        data.max_snakes_per_user,
        data.required_participants,
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create tournament")?;

    row.try_into()
}

pub async fn get_tournament_by_id<'e, E>(
    executor: E,
    tournament_id: Uuid,
) -> cja::Result<Option<Tournament>>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query_as!(
        TournamentRow,
        r#"
        SELECT
            tournament_id,
            name,
            description,
            user_id,
            game_type,
            board_size,
            registration_status as "registration_status: RegistrationStatus",
            visibility as "visibility: TournamentVisibility",
            status as "status: TournamentStatus",
            match_style as "match_style: MatchStyle",
            max_snakes_per_user,
            required_participants,
            current_round,
            created_at,
            updated_at
        FROM tournaments
        WHERE tournament_id = $1
        "#,
        tournament_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch tournament")?;

    row.map(Tournament::try_from).transpose()
}

/// Update status without transition validation — callers are responsible for
/// checking `TournamentStatus::can_transition_to` first (route/job layer owns
/// the error message).
///
/// Compare-and-swap: the update only applies if the tournament is currently in
/// `expected`, which protects against concurrent status changes racing between
/// the caller's transition check and this write.
pub async fn set_tournament_status<'e, E>(
    executor: E,
    tournament_id: Uuid,
    status: TournamentStatus,
    expected: TournamentStatus,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    let updated = try_set_tournament_status(executor, tournament_id, status, expected).await?;

    if !updated {
        return Err(color_eyre::eyre::eyre!(
            "Failed to update tournament {} status to {}: tournament not found or status is no longer {}",
            tournament_id,
            status.as_str(),
            expected.as_str(),
        ));
    }

    Ok(())
}

/// CAS variant of [`set_tournament_status`] that reports whether the swap
/// applied instead of erroring: returns `false` when the tournament is missing
/// or its status is no longer `expected`. Use this when losing the race is
/// benign — e.g. a tournament completion racing a concurrent cancel must not
/// resurrect the canceled tournament, and must not surface as a job failure.
pub async fn try_set_tournament_status<'e, E>(
    executor: E,
    tournament_id: Uuid,
    status: TournamentStatus,
    expected: TournamentStatus,
) -> cja::Result<bool>
where
    E: Executor<'e, Database = Postgres>,
{
    let result = sqlx::query!(
        "UPDATE tournaments SET status = $2 WHERE tournament_id = $1 AND status = $3",
        tournament_id,
        status.as_str(),
        expected.as_str(),
    )
    .execute(executor)
    .await
    .wrap_err("Failed to update tournament status")?;

    Ok(result.rows_affected() == 1)
}

pub async fn create_registration<'e, E>(
    executor: E,
    tournament_id: Uuid,
    battlesnake_id: Uuid,
    user_id: Uuid,
    seed: i32,
) -> cja::Result<TournamentRegistration>
where
    E: Executor<'e, Database = Postgres>,
{
    let registration = sqlx::query_as!(
        TournamentRegistration,
        r#"
        INSERT INTO tournament_registrations (tournament_id, battlesnake_id, user_id, seed)
        VALUES ($1, $2, $3, $4)
        RETURNING
            registration_id,
            tournament_id,
            battlesnake_id,
            user_id,
            seed,
            registered_at
        "#,
        tournament_id,
        battlesnake_id,
        user_id,
        seed,
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create tournament registration")?;

    Ok(registration)
}

pub async fn get_registrations_for_tournament(
    pool: &PgPool,
    tournament_id: Uuid,
) -> cja::Result<Vec<TournamentRegistration>> {
    let registrations = sqlx::query_as!(
        TournamentRegistration,
        r#"
        SELECT
            registration_id,
            tournament_id,
            battlesnake_id,
            user_id,
            seed,
            registered_at
        FROM tournament_registrations
        WHERE tournament_id = $1
        ORDER BY seed ASC
        "#,
        tournament_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch tournament registrations")?;

    Ok(registrations)
}

/// Count registrations a battlesnake holds in tournaments that are still
/// active (open for registration or in progress). Used to refuse deleting a
/// snake out from under a live tournament — the FK cascade would silently
/// remove its registrations and match participations.
pub async fn count_active_tournament_registrations<'e, E>(
    executor: E,
    battlesnake_id: Uuid,
) -> cja::Result<i64>
where
    E: Executor<'e, Database = Postgres>,
{
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!"
        FROM tournament_registrations tr
        JOIN tournaments t ON t.tournament_id = tr.tournament_id
        WHERE tr.battlesnake_id = $1
          AND t.status IN ('registration', 'in_progress')
        "#,
        battlesnake_id
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to count active tournament registrations")?;

    Ok(count)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateTournamentMatch {
    pub tournament_id: Uuid,
    pub round: i32,
    pub position: i32,
    pub next_match_id: Option<Uuid>,
    pub visual_column: i32,
    pub visual_row: i32,
}

pub async fn create_match<'e, E>(
    executor: E,
    data: CreateTournamentMatch,
) -> cja::Result<TournamentMatch>
where
    E: Executor<'e, Database = Postgres>,
{
    let tournament_match = sqlx::query_as!(
        TournamentMatch,
        r#"
        INSERT INTO tournament_matches (
            tournament_id, round, position, next_match_id, visual_column, visual_row
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
            match_id,
            tournament_id,
            round,
            position,
            status as "status: MatchStatus",
            next_match_id,
            winner_id,
            visual_column,
            visual_row,
            created_at,
            updated_at
        "#,
        data.tournament_id,
        data.round,
        data.position,
        data.next_match_id,
        data.visual_column,
        data.visual_row,
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create tournament match")?;

    Ok(tournament_match)
}

pub async fn get_matches_for_tournament(
    pool: &PgPool,
    tournament_id: Uuid,
) -> cja::Result<Vec<TournamentMatch>> {
    let matches = sqlx::query_as!(
        TournamentMatch,
        r#"
        SELECT
            match_id,
            tournament_id,
            round,
            position,
            status as "status: MatchStatus",
            next_match_id,
            winner_id,
            visual_column,
            visual_row,
            created_at,
            updated_at
        FROM tournament_matches
        WHERE tournament_id = $1
        ORDER BY round ASC, position ASC
        "#,
        tournament_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch tournament matches")?;

    Ok(matches)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateMatchParticipant {
    pub match_id: Uuid,
    /// Which side of the match this participant occupies (1 or 2).
    pub slot: i16,
    pub battlesnake_id: Option<Uuid>,
    pub source_match_id: Option<Uuid>,
    pub participant_type: ParticipantType,
    pub seed_position: Option<i32>,
}

pub async fn create_match_participant<'e, E>(
    executor: E,
    data: CreateMatchParticipant,
) -> cja::Result<MatchParticipant>
where
    E: Executor<'e, Database = Postgres>,
{
    let participant = sqlx::query_as!(
        MatchParticipant,
        r#"
        INSERT INTO match_participants (
            match_id, slot, battlesnake_id, source_match_id, participant_type, seed_position
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
            match_participant_id,
            match_id,
            slot,
            battlesnake_id,
            source_match_id,
            participant_type as "participant_type: ParticipantType",
            seed_position,
            created_at
        "#,
        data.match_id,
        data.slot,
        data.battlesnake_id,
        data.source_match_id,
        data.participant_type.as_str(),
        data.seed_position,
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create match participant")?;

    Ok(participant)
}

pub async fn get_participants_for_match(
    pool: &PgPool,
    match_id: Uuid,
) -> cja::Result<Vec<MatchParticipant>> {
    let participants = sqlx::query_as!(
        MatchParticipant,
        r#"
        SELECT
            match_participant_id,
            match_id,
            slot,
            battlesnake_id,
            source_match_id,
            participant_type as "participant_type: ParticipantType",
            seed_position,
            created_at
        FROM match_participants
        WHERE match_id = $1
        ORDER BY slot ASC
        "#,
        match_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch match participants")?;

    Ok(participants)
}

pub async fn create_match_game<'e, E>(
    executor: E,
    match_id: Uuid,
    game_id: Uuid,
    game_number: i32,
) -> cja::Result<MatchGame>
where
    E: Executor<'e, Database = Postgres>,
{
    let match_game = sqlx::query_as!(
        MatchGame,
        r#"
        INSERT INTO match_games (match_id, game_id, game_number)
        VALUES ($1, $2, $3)
        RETURNING
            match_game_id,
            match_id,
            game_id,
            game_number,
            winner_id,
            created_at
        "#,
        match_id,
        game_id,
        game_number,
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to create match game")?;

    Ok(match_game)
}

pub async fn get_match_games_for_match(
    pool: &PgPool,
    match_id: Uuid,
) -> cja::Result<Vec<MatchGame>> {
    let match_games = sqlx::query_as!(
        MatchGame,
        r#"
        SELECT
            match_game_id,
            match_id,
            game_id,
            game_number,
            winner_id,
            created_at
        FROM match_games
        WHERE match_id = $1
        ORDER BY game_number ASC
        "#,
        match_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch match games")?;

    Ok(match_games)
}

pub async fn get_match_by_id<'e, E>(
    executor: E,
    match_id: Uuid,
) -> cja::Result<Option<TournamentMatch>>
where
    E: Executor<'e, Database = Postgres>,
{
    let tournament_match = sqlx::query_as!(
        TournamentMatch,
        r#"
        SELECT
            match_id,
            tournament_id,
            round,
            position,
            status as "status: MatchStatus",
            next_match_id,
            winner_id,
            visual_column,
            visual_row,
            created_at,
            updated_at
        FROM tournament_matches
        WHERE match_id = $1
        "#,
        match_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch tournament match")?;

    Ok(tournament_match)
}

pub async fn get_matches_for_round(
    pool: &PgPool,
    tournament_id: Uuid,
    round: i32,
) -> cja::Result<Vec<TournamentMatch>> {
    let matches = sqlx::query_as!(
        TournamentMatch,
        r#"
        SELECT
            match_id,
            tournament_id,
            round,
            position,
            status as "status: MatchStatus",
            next_match_id,
            winner_id,
            visual_column,
            visual_row,
            created_at,
            updated_at
        FROM tournament_matches
        WHERE tournament_id = $1 AND round = $2
        ORDER BY position ASC
        "#,
        tournament_id,
        round,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch matches for round")?;

    Ok(matches)
}

pub async fn set_match_status<'e, E>(
    executor: E,
    match_id: Uuid,
    status: MatchStatus,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        "UPDATE tournament_matches SET status = $2 WHERE match_id = $1",
        match_id,
        status.as_str(),
    )
    .execute(executor)
    .await
    .wrap_err("Failed to update match status")?;

    Ok(())
}

pub async fn count_unfinished_matches_in_round(
    pool: &PgPool,
    tournament_id: Uuid,
    round: i32,
) -> cja::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!"
        FROM tournament_matches
        WHERE tournament_id = $1
          AND round = $2
          AND status NOT IN ('completed', 'canceled')
        "#,
        tournament_id,
        round,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count unfinished matches")?;

    Ok(count)
}

pub async fn round_exists(pool: &PgPool, tournament_id: Uuid, round: i32) -> cja::Result<bool> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM tournament_matches
            WHERE tournament_id = $1 AND round = $2
        ) as "exists!"
        "#,
        tournament_id,
        round,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check round existence")?;

    Ok(exists)
}

pub async fn set_tournament_current_round<'e, E>(
    executor: E,
    tournament_id: Uuid,
    current_round: i32,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        "UPDATE tournaments SET current_round = $2 WHERE tournament_id = $1",
        tournament_id,
        current_round,
    )
    .execute(executor)
    .await
    .wrap_err("Failed to update tournament current round")?;

    Ok(())
}

pub async fn find_match_game_by_game_id(
    pool: &PgPool,
    game_id: Uuid,
) -> cja::Result<Option<MatchGame>> {
    let match_game = sqlx::query_as!(
        MatchGame,
        r#"
        SELECT
            match_game_id,
            match_id,
            game_id,
            game_number,
            winner_id,
            created_at
        FROM match_games
        WHERE game_id = $1
        "#,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to look up match game by game id")?;

    Ok(match_game)
}

/// Record a game's winner on its match_games row. `None` records a tie.
///
/// Executor-generic so the game runner can write the result inside the same
/// transaction that marks the game finished.
pub async fn set_match_game_winner<'e, E>(
    executor: E,
    match_game_id: Uuid,
    winner_id: Option<Uuid>,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        "UPDATE match_games SET winner_id = $2 WHERE match_game_id = $1",
        match_game_id,
        winner_id,
    )
    .execute(executor)
    .await
    .wrap_err("Failed to set match game winner")?;

    Ok(())
}

/// Matches that look stuck: `in_progress` in an `in_progress` tournament with
/// no update since `cutoff`. The stuck-match sweeper cron re-enqueues
/// evaluation for these; `run_match` is idempotent, so false positives (e.g. a
/// long match whose current game is still healthy) are harmless no-ops.
///
/// The tournament-status filter keeps the sweeper from repeatedly poking
/// matches of canceled or otherwise ended tournaments.
pub async fn find_stale_in_progress_matches(
    pool: &PgPool,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> cja::Result<Vec<Uuid>> {
    let match_ids = sqlx::query_scalar!(
        r#"
        SELECT m.match_id
        FROM tournament_matches m
        JOIN tournaments t ON t.tournament_id = m.tournament_id
        WHERE m.status = 'in_progress'
          AND t.status = 'in_progress'
          AND m.updated_at < $1
        ORDER BY m.updated_at ASC
        "#,
        cutoff,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to find stale in-progress matches")?;

    Ok(match_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tournament_status_round_trips_through_strings() {
        for status in [
            TournamentStatus::Created,
            TournamentStatus::Registration,
            TournamentStatus::InProgress,
            TournamentStatus::Completed,
            TournamentStatus::Canceled,
        ] {
            assert_eq!(TournamentStatus::from_str(status.as_str()).unwrap(), status);
        }
        assert!(TournamentStatus::from_str("bogus").is_err());
    }

    #[test]
    fn tournament_status_transitions() {
        use TournamentStatus::{Canceled, Completed, Created, InProgress, Registration};

        assert!(Created.can_transition_to(Registration));
        assert!(Registration.can_transition_to(InProgress));
        assert!(InProgress.can_transition_to(Completed));
        assert!(InProgress.can_transition_to(Registration)); // reset

        // Canceled is reachable from any other state, including Completed
        assert!(Created.can_transition_to(Canceled));
        assert!(Registration.can_transition_to(Canceled));
        assert!(InProgress.can_transition_to(Canceled));
        assert!(Completed.can_transition_to(Canceled));

        // No skipping ahead or moving backward
        assert!(!Created.can_transition_to(InProgress));
        assert!(!Created.can_transition_to(Completed));
        assert!(!Registration.can_transition_to(Created));
        assert!(!Registration.can_transition_to(Completed));
        assert!(!Completed.can_transition_to(InProgress));

        // Canceled is terminal
        assert!(!Canceled.can_transition_to(Created));
        assert!(!Canceled.can_transition_to(Registration));
        assert!(!Canceled.can_transition_to(InProgress));
        assert!(!Canceled.can_transition_to(Completed));
    }

    #[test]
    fn registration_status_round_trips_through_strings() {
        for status in [
            RegistrationStatus::Open,
            RegistrationStatus::Closed,
            RegistrationStatus::OwnerOnly,
        ] {
            assert_eq!(
                RegistrationStatus::from_str(status.as_str()).unwrap(),
                status
            );
        }
        assert!(RegistrationStatus::from_str("bogus").is_err());
    }

    #[test]
    fn tournament_visibility_round_trips_through_strings() {
        for visibility in [
            TournamentVisibility::Public,
            TournamentVisibility::ParticipantsOnly,
        ] {
            assert_eq!(
                TournamentVisibility::from_str(visibility.as_str()).unwrap(),
                visibility
            );
        }
        assert!(TournamentVisibility::from_str("bogus").is_err());
    }

    #[test]
    fn match_style_round_trips_through_strings() {
        for style in [
            MatchStyle::SingleGame,
            MatchStyle::BestOf3,
            MatchStyle::FirstTo3,
        ] {
            assert_eq!(MatchStyle::from_str(style.as_str()).unwrap(), style);
        }
        assert!(MatchStyle::from_str("bogus").is_err());
    }

    #[test]
    fn match_style_win_thresholds() {
        assert_eq!(MatchStyle::SingleGame.wins_needed(), 1);
        assert_eq!(MatchStyle::BestOf3.wins_needed(), 2);
        assert_eq!(MatchStyle::FirstTo3.wins_needed(), 3);

        assert_eq!(MatchStyle::SingleGame.max_games_without_ties(), 1);
        assert_eq!(MatchStyle::BestOf3.max_games_without_ties(), 3);
        assert_eq!(MatchStyle::FirstTo3.max_games_without_ties(), 5);
    }

    #[test]
    fn match_status_round_trips_through_strings() {
        for status in [
            MatchStatus::Scheduled,
            MatchStatus::InProgress,
            MatchStatus::Completed,
            MatchStatus::Canceled,
        ] {
            assert_eq!(MatchStatus::from_str(status.as_str()).unwrap(), status);
        }
        assert!(MatchStatus::from_str("bogus").is_err());
    }

    #[test]
    fn participant_type_round_trips_through_strings() {
        for participant_type in [
            ParticipantType::Seed,
            ParticipantType::Winner,
            ParticipantType::Loser,
            ParticipantType::Wildcard,
        ] {
            assert_eq!(
                ParticipantType::from_str(participant_type.as_str()).unwrap(),
                participant_type
            );
        }
        assert!(ParticipantType::from_str("bogus").is_err());
    }

    // Insert a user + battlesnake pair with raw (non-macro) queries so the
    // fixtures don't need entries in the sqlx offline cache.
    async fn fixture_user_and_snake(pool: &PgPool) -> cja::Result<(Uuid, Uuid)> {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, $3) RETURNING user_id",
        )
        .bind(424_242_i64)
        .bind("test-user")
        .bind("test-token")
        .fetch_one(pool)
        .await?;

        let battlesnake_id: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, $2, $3) RETURNING battlesnake_id",
        )
        .bind(user_id)
        .bind("test-snake")
        .bind("http://example.com")
        .fetch_one(pool)
        .await?;

        Ok((user_id, battlesnake_id))
    }

    fn fixture_tournament() -> CreateTournament {
        CreateTournament {
            name: "Test Tournament".to_string(),
            description: None,
            game_type: GameType::Standard,
            board_size: GameBoardSize::Medium,
            registration_status: RegistrationStatus::Open,
            visibility: TournamentVisibility::Public,
            match_style: MatchStyle::SingleGame,
            max_snakes_per_user: 1,
            required_participants: 2,
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn counts_only_registrations_in_active_tournaments(pool: PgPool) -> cja::Result<()> {
        let (user_id, battlesnake_id) = fixture_user_and_snake(&pool).await?;

        // No registrations yet.
        let count = count_active_tournament_registrations(&pool, battlesnake_id).await?;
        assert_eq!(count, 0);

        let tournament = create_tournament(&pool, user_id, fixture_tournament()).await?;
        create_registration(&pool, tournament.tournament_id, battlesnake_id, user_id, 1).await?;

        // Registered, but the tournament is still 'created' — not active.
        let count = count_active_tournament_registrations(&pool, battlesnake_id).await?;
        assert_eq!(count, 0);

        set_tournament_status(
            &pool,
            tournament.tournament_id,
            TournamentStatus::Registration,
            TournamentStatus::Created,
        )
        .await?;
        let count = count_active_tournament_registrations(&pool, battlesnake_id).await?;
        assert_eq!(count, 1);

        set_tournament_status(
            &pool,
            tournament.tournament_id,
            TournamentStatus::InProgress,
            TournamentStatus::Registration,
        )
        .await?;
        let count = count_active_tournament_registrations(&pool, battlesnake_id).await?;
        assert_eq!(count, 1);

        set_tournament_status(
            &pool,
            tournament.tournament_id,
            TournamentStatus::Completed,
            TournamentStatus::InProgress,
        )
        .await?;
        let count = count_active_tournament_registrations(&pool, battlesnake_id).await?;
        assert_eq!(count, 0);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn set_tournament_status_is_a_compare_and_swap(pool: PgPool) -> cja::Result<()> {
        let (user_id, _) = fixture_user_and_snake(&pool).await?;
        let tournament = create_tournament(&pool, user_id, fixture_tournament()).await?;

        // Wrong expected status: refused, status unchanged.
        let result = set_tournament_status(
            &pool,
            tournament.tournament_id,
            TournamentStatus::InProgress,
            TournamentStatus::Registration,
        )
        .await;
        assert!(result.is_err());
        let reloaded = get_tournament_by_id(&pool, tournament.tournament_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, TournamentStatus::Created);

        // Matching expected status: applied.
        set_tournament_status(
            &pool,
            tournament.tournament_id,
            TournamentStatus::Registration,
            TournamentStatus::Created,
        )
        .await?;
        let reloaded = get_tournament_by_id(&pool, tournament.tournament_id)
            .await?
            .unwrap();
        assert_eq!(reloaded.status, TournamentStatus::Registration);

        // Unknown tournament: refused.
        let result = set_tournament_status(
            &pool,
            Uuid::new_v4(),
            TournamentStatus::Registration,
            TournamentStatus::Created,
        )
        .await;
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn match_style_serde_uses_explicit_names() {
        assert_eq!(
            serde_json::to_string(&MatchStyle::BestOf3).unwrap(),
            r#""best_of_3""#
        );
        assert_eq!(
            serde_json::to_string(&MatchStyle::FirstTo3).unwrap(),
            r#""first_to_3""#
        );
        assert_eq!(
            serde_json::from_str::<MatchStyle>(r#""best_of_3""#).unwrap(),
            MatchStyle::BestOf3
        );
    }
}
