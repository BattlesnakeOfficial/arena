use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::{Executor, PgPool, Postgres, Transaction, Type};
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

/// Fetch a tournament with `FOR UPDATE`, locking its row for the rest of the
/// transaction. Every mutating handler locks the tournament row first so that
/// all validation (status, registration caps, dupe checks, settings freeze)
/// and the subsequent writes are serialized per tournament — this is the
/// TOCTOU guard for concurrent registrations, seed moves, imports, and
/// settings/status changes.
pub async fn get_tournament_for_update(
    tx: &mut Transaction<'_, Postgres>,
    tournament_id: Uuid,
) -> cja::Result<Option<Tournament>> {
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
        FOR UPDATE
        "#,
        tournament_id
    )
    .fetch_optional(&mut **tx)
    .await
    .wrap_err("Failed to fetch tournament for update")?;

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
    let result = sqlx::query!(
        "UPDATE tournaments SET status = $2 WHERE tournament_id = $1 AND status = $3",
        tournament_id,
        status.as_str(),
        expected.as_str(),
    )
    .execute(executor)
    .await
    .wrap_err("Failed to update tournament status")?;

    if result.rows_affected() == 0 {
        return Err(color_eyre::eyre::eyre!(
            "Failed to update tournament {} status to {}: tournament not found or status is no longer {}",
            tournament_id,
            status.as_str(),
            expected.as_str(),
        ));
    }

    Ok(())
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

pub async fn get_registrations_for_tournament<'e, E>(
    executor: E,
    tournament_id: Uuid,
) -> cja::Result<Vec<TournamentRegistration>>
where
    E: Executor<'e, Database = Postgres>,
{
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
    .fetch_all(executor)
    .await
    .wrap_err("Failed to fetch tournament registrations")?;

    Ok(registrations)
}

// --- Tournament list / detail / registration queries (BS-017 + BS-018) ---

/// A tournament row enriched with owner login and registration count for the
/// list page.
#[derive(Debug, Clone)]
pub struct TournamentListItem {
    pub tournament_id: Uuid,
    pub name: String,
    pub owner_login: String,
    pub status: TournamentStatus,
    pub game_type: GameType,
    pub registration_count: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// List tournaments visible to a viewer: all public tournaments, plus the
/// viewer's own non-public ones when logged in.
pub async fn list_visible_tournaments(
    pool: &PgPool,
    viewer_user_id: Option<Uuid>,
) -> cja::Result<Vec<TournamentListItem>> {
    struct Row {
        tournament_id: Uuid,
        name: String,
        owner_login: String,
        status: TournamentStatus,
        game_type: String,
        registration_count: i64,
        created_at: chrono::DateTime<chrono::Utc>,
    }

    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT
            t.tournament_id,
            t.name,
            u.github_login as owner_login,
            t.status as "status: TournamentStatus",
            t.game_type,
            COUNT(r.registration_id) as "registration_count!",
            t.created_at
        FROM tournaments t
        JOIN users u ON t.user_id = u.user_id
        LEFT JOIN tournament_registrations r ON r.tournament_id = t.tournament_id
        WHERE t.visibility = 'public' OR t.user_id = $1
        GROUP BY t.tournament_id, u.github_login
        ORDER BY t.created_at DESC
        "#,
        viewer_user_id,
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list visible tournaments")?;

    rows.into_iter()
        .map(|row| {
            let game_type = GameType::from_str(&row.game_type)
                .wrap_err_with(|| format!("Invalid game type: {}", row.game_type))?;
            Ok(TournamentListItem {
                tournament_id: row.tournament_id,
                name: row.name,
                owner_login: row.owner_login,
                status: row.status,
                game_type,
                registration_count: row.registration_count,
                created_at: row.created_at,
            })
        })
        .collect()
}

/// A registration enriched with snake name and owner login for display.
#[derive(Debug, Clone)]
pub struct RegistrationWithDetails {
    pub registration_id: Uuid,
    pub battlesnake_id: Uuid,
    pub user_id: Uuid,
    pub seed: i32,
    pub snake_name: String,
    pub owner_login: String,
}

pub async fn get_registrations_with_details(
    pool: &PgPool,
    tournament_id: Uuid,
) -> cja::Result<Vec<RegistrationWithDetails>> {
    let registrations = sqlx::query_as!(
        RegistrationWithDetails,
        r#"
        SELECT
            r.registration_id,
            r.battlesnake_id,
            r.user_id,
            r.seed,
            b.name as snake_name,
            u.github_login as owner_login
        FROM tournament_registrations r
        JOIN battlesnakes b ON r.battlesnake_id = b.battlesnake_id
        JOIN users u ON r.user_id = u.user_id
        WHERE r.tournament_id = $1
        ORDER BY r.seed ASC
        "#,
        tournament_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to fetch registrations with details")?;

    Ok(registrations)
}

pub async fn get_registration_by_id<'e, E>(
    executor: E,
    registration_id: Uuid,
) -> cja::Result<Option<TournamentRegistration>>
where
    E: Executor<'e, Database = Postgres>,
{
    let registration = sqlx::query_as!(
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
        WHERE registration_id = $1
        "#,
        registration_id
    )
    .fetch_optional(executor)
    .await
    .wrap_err("Failed to fetch registration by id")?;

    Ok(registration)
}

pub async fn count_registrations<'e, E>(executor: E, tournament_id: Uuid) -> cja::Result<i64>
where
    E: Executor<'e, Database = Postgres>,
{
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!" FROM tournament_registrations WHERE tournament_id = $1"#,
        tournament_id
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to count registrations")?;

    Ok(count)
}

pub async fn count_registrations_for_user<'e, E>(
    executor: E,
    tournament_id: Uuid,
    user_id: Uuid,
) -> cja::Result<i64>
where
    E: Executor<'e, Database = Postgres>,
{
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) as "count!"
        FROM tournament_registrations
        WHERE tournament_id = $1 AND user_id = $2
        "#,
        tournament_id,
        user_id
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to count registrations for user")?;

    Ok(count)
}

pub async fn is_battlesnake_registered<'e, E>(
    executor: E,
    tournament_id: Uuid,
    battlesnake_id: Uuid,
) -> cja::Result<bool>
where
    E: Executor<'e, Database = Postgres>,
{
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM tournament_registrations
            WHERE tournament_id = $1 AND battlesnake_id = $2
        ) as "exists!"
        "#,
        tournament_id,
        battlesnake_id
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to check if battlesnake is registered")?;

    Ok(exists)
}

/// Next free seed for a tournament (max seed + 1). Call inside the same
/// transaction as the registration insert to keep assignment consistent.
pub async fn next_seed<'e, E>(executor: E, tournament_id: Uuid) -> cja::Result<i32>
where
    E: Executor<'e, Database = Postgres>,
{
    let next = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(MAX(seed), 0) + 1 as "next_seed!"
        FROM tournament_registrations
        WHERE tournament_id = $1
        "#,
        tournament_id
    )
    .fetch_one(executor)
    .await
    .wrap_err("Failed to compute next seed")?;

    Ok(next)
}

/// Register a snake with the next free seed. Runs inside the caller's
/// transaction — the caller must have locked the tournament row (via
/// `get_tournament_for_update`) so the seed assignment can't race.
pub async fn register_snake_with_next_seed(
    tx: &mut Transaction<'_, Postgres>,
    tournament_id: Uuid,
    battlesnake_id: Uuid,
    user_id: Uuid,
) -> cja::Result<TournamentRegistration> {
    let seed = next_seed(&mut **tx, tournament_id).await?;
    let registration =
        create_registration(&mut **tx, tournament_id, battlesnake_id, user_id, seed).await?;

    Ok(registration)
}

/// Defer the (tournament_id, seed) unique constraint for the rest of the
/// current transaction. Must be run inside any transaction that transiently
/// violates seed uniqueness (block shifts, swaps, renumbering); the constraint
/// is DEFERRABLE INITIALLY IMMEDIATE, so it is re-checked at commit.
async fn defer_seed_constraint(tx: &mut Transaction<'_, Postgres>) -> cja::Result<()> {
    sqlx::query!("SET CONSTRAINTS tournament_registrations_tournament_id_seed_key DEFERRED")
        .execute(&mut **tx)
        .await
        .wrap_err("Failed to defer seed uniqueness constraint")?;

    Ok(())
}

/// Renumber seeds to 1..N (ordered by current seed) to close any gaps.
/// Callers must have deferred the seed uniqueness constraint first.
async fn renumber_seeds<'e, E>(executor: E, tournament_id: Uuid) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        r#"
        UPDATE tournament_registrations tr
        SET seed = ranked.new_seed
        FROM (
            SELECT registration_id, (ROW_NUMBER() OVER (ORDER BY seed ASC))::int AS new_seed
            FROM tournament_registrations
            WHERE tournament_id = $1
        ) ranked
        WHERE tr.registration_id = ranked.registration_id
          AND tr.seed <> ranked.new_seed
        "#,
        tournament_id
    )
    .execute(executor)
    .await
    .wrap_err("Failed to renumber seeds")?;

    Ok(())
}

/// Delete a registration and renumber remaining seeds to close the gap. Runs
/// inside the caller's transaction — the caller must have locked the
/// tournament row first. Returns whether a row was deleted.
pub async fn delete_registration_and_renumber(
    tx: &mut Transaction<'_, Postgres>,
    tournament_id: Uuid,
    registration_id: Uuid,
) -> cja::Result<bool> {
    defer_seed_constraint(tx).await?;

    let result = sqlx::query!(
        "DELETE FROM tournament_registrations WHERE registration_id = $1 AND tournament_id = $2",
        registration_id,
        tournament_id
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to delete registration")?;

    renumber_seeds(&mut **tx, tournament_id).await?;

    Ok(result.rows_affected() > 0)
}

/// Move a registration to a new seed, shifting the others to make room.
/// The requested seed is clamped to the valid range [1, max seed].
///
/// Runs inside the caller's transaction — the caller must have locked the
/// tournament row first so seed mutations serialize. Returns `false` when the
/// registration doesn't exist in this tournament (callers surface that as a
/// user-facing error rather than a 500).
pub async fn move_registration_seed(
    tx: &mut Transaction<'_, Postgres>,
    tournament_id: Uuid,
    registration_id: Uuid,
    new_seed: i32,
) -> cja::Result<bool> {
    let Some(current_seed) = sqlx::query_scalar!(
        r#"
        SELECT seed FROM tournament_registrations
        WHERE registration_id = $1 AND tournament_id = $2
        "#,
        registration_id,
        tournament_id
    )
    .fetch_optional(&mut **tx)
    .await
    .wrap_err("Failed to fetch registration seed")?
    else {
        return Ok(false);
    };

    let max_seed = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(MAX(seed), 0) as "max_seed!"
        FROM tournament_registrations
        WHERE tournament_id = $1
        "#,
        tournament_id
    )
    .fetch_one(&mut **tx)
    .await
    .wrap_err("Failed to fetch max seed")?;

    let target_seed = new_seed.clamp(1, max_seed);

    if target_seed == current_seed {
        return Ok(true);
    }

    // The block shift below transiently collides with the moved row's seed.
    defer_seed_constraint(tx).await?;

    if target_seed < current_seed {
        // Moving up: shift the block [target, current) down by one.
        sqlx::query!(
            r#"
            UPDATE tournament_registrations
            SET seed = seed + 1
            WHERE tournament_id = $1 AND seed >= $2 AND seed < $3
            "#,
            tournament_id,
            target_seed,
            current_seed
        )
        .execute(&mut **tx)
        .await
        .wrap_err("Failed to shift seeds up")?;
    } else {
        // Moving down: shift the block (current, target] up by one.
        sqlx::query!(
            r#"
            UPDATE tournament_registrations
            SET seed = seed - 1
            WHERE tournament_id = $1 AND seed > $2 AND seed <= $3
            "#,
            tournament_id,
            current_seed,
            target_seed
        )
        .execute(&mut **tx)
        .await
        .wrap_err("Failed to shift seeds down")?;
    }

    sqlx::query!(
        "UPDATE tournament_registrations SET seed = $2 WHERE registration_id = $1",
        registration_id,
        target_seed
    )
    .execute(&mut **tx)
    .await
    .wrap_err("Failed to set new seed")?;

    Ok(true)
}

/// Editable tournament settings (BS-017). Callers are responsible for
/// validating that game_type/board_size changes are allowed (they are frozen
/// once registrations exist).
#[derive(Debug, Clone)]
pub struct UpdateTournamentSettings {
    pub name: String,
    pub description: Option<String>,
    pub game_type: GameType,
    pub board_size: GameBoardSize,
    pub match_style: MatchStyle,
    pub registration_status: RegistrationStatus,
    pub visibility: TournamentVisibility,
    pub max_snakes_per_user: i32,
    pub required_participants: i32,
}

pub async fn update_tournament_settings<'e, E>(
    executor: E,
    tournament_id: Uuid,
    data: UpdateTournamentSettings,
) -> cja::Result<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query!(
        r#"
        UPDATE tournaments
        SET name = $2,
            description = $3,
            game_type = $4,
            board_size = $5,
            match_style = $6,
            registration_status = $7,
            visibility = $8,
            max_snakes_per_user = $9,
            required_participants = $10
        WHERE tournament_id = $1
        "#,
        tournament_id,
        data.name,
        data.description,
        data.game_type.as_str(),
        data.board_size.as_str(),
        data.match_style.as_str(),
        data.registration_status.as_str(),
        data.visibility.as_str(),
        data.max_snakes_per_user,
        data.required_participants,
    )
    .execute(executor)
    .await
    .wrap_err("Failed to update tournament settings")?;

    Ok(())
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

    #[sqlx::test(migrations = "../migrations")]
    async fn seed_moves_and_renumbering_defer_the_seed_constraint(pool: PgPool) -> cja::Result<()> {
        let (user_id, snake_1) = fixture_user_and_snake(&pool).await?;
        let snake_2: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'snake-2', 'http://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;
        let snake_3: Uuid = sqlx::query_scalar(
            "INSERT INTO battlesnakes (user_id, name, url)
             VALUES ($1, 'snake-3', 'http://example.com') RETURNING battlesnake_id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await?;

        let mut params = fixture_tournament();
        params.max_snakes_per_user = 3;
        let tournament = create_tournament(&pool, user_id, params).await?;
        let tid = tournament.tournament_id;

        let reg_1 = create_registration(&pool, tid, snake_1, user_id, 1).await?;
        let reg_2 = create_registration(&pool, tid, snake_2, user_id, 2).await?;
        let reg_3 = create_registration(&pool, tid, snake_3, user_id, 3).await?;

        let seeds_of = |regs: Vec<TournamentRegistration>| {
            regs.into_iter()
                .map(|r| (r.registration_id, r.seed))
                .collect::<Vec<_>>()
        };

        // Move seed 3 -> 1: the block shift transiently collides with the
        // moved row, so this only works if the unique constraint is deferred.
        let mut tx = pool.begin().await?;
        let moved = move_registration_seed(&mut tx, tid, reg_3.registration_id, 1).await?;
        assert!(moved);
        tx.commit().await?;

        let regs = get_registrations_for_tournament(&pool, tid).await?;
        assert_eq!(
            seeds_of(regs),
            vec![
                (reg_3.registration_id, 1),
                (reg_1.registration_id, 2),
                (reg_2.registration_id, 3),
            ]
        );

        // Unknown registration: reported as not-found, not an error.
        let mut tx = pool.begin().await?;
        let moved = move_registration_seed(&mut tx, tid, Uuid::new_v4(), 1).await?;
        assert!(!moved);
        drop(tx);

        // Unregister the middle seed: remaining seeds renumber to 1..N.
        let mut tx = pool.begin().await?;
        let deleted = delete_registration_and_renumber(&mut tx, tid, reg_1.registration_id).await?;
        assert!(deleted);
        tx.commit().await?;

        let regs = get_registrations_for_tournament(&pool, tid).await?;
        assert_eq!(
            seeds_of(regs),
            vec![(reg_3.registration_id, 1), (reg_2.registration_id, 2)]
        );

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
