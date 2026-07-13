use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::battlesnake::{self, Battlesnake};
use crate::models::game::{self, CreateGameWithSnakes, GameBoardSize, GameType};
use crate::state::AppState;

// Flow model for the game creation process
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GameCreationFlow {
    pub flow_id: Uuid,
    pub board_size: GameBoardSize,
    pub game_type: GameType,
    /// Selected battlesnake IDs - duplicates are allowed (same snake can appear multiple times)
    pub selected_battlesnake_ids: Vec<Uuid>,
    pub search_query: Option<String>,
    pub user_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl GameCreationFlow {
    // Create a new flow for a user
    pub async fn create_for_user(pool: &PgPool, user_id: Uuid) -> cja::Result<Self> {
        // Insert a new flow with default values
        let flow = sqlx::query_as!(
            GameCreationFlowRaw,
            r#"
            INSERT INTO game_flows (
                user_id,
                board_size,
                game_type,
                selected_battlesnakes,
                search_query
            )
            VALUES ($1, $2, $3, $4, $5)
            RETURNING
                flow_id,
                board_size,
                game_type,
                selected_battlesnakes,
                search_query,
                user_id,
                created_at,
                updated_at
            "#,
            user_id,
            GameBoardSize::Medium.as_str(),
            GameType::Standard.as_str(),
            &Vec::<Uuid>::new(),
            None::<String>
        )
        .fetch_one(pool)
        .await
        .wrap_err("Failed to create game flow")?;

        Ok(flow.into())
    }

    // Get a flow by ID, ensuring it belongs to the user
    pub async fn get_by_id(
        pool: &PgPool,
        flow_id: Uuid,
        user_id: Uuid,
    ) -> cja::Result<Option<Self>> {
        let flow = sqlx::query_as!(
            GameCreationFlowRaw,
            r#"
            SELECT
                flow_id,
                board_size,
                game_type,
                selected_battlesnakes,
                search_query,
                user_id,
                created_at,
                updated_at
            FROM game_flows
            WHERE flow_id = $1 AND user_id = $2
            "#,
            flow_id,
            user_id
        )
        .fetch_optional(pool)
        .await
        .wrap_err("Failed to get game flow")?;

        Ok(flow.map(|f| f.into()))
    }

    // Update the flow with new values
    pub async fn update(&self, pool: &PgPool) -> cja::Result<Self> {
        let flow = sqlx::query_as!(
            GameCreationFlowRaw,
            r#"
            UPDATE game_flows
            SET
                board_size = $1,
                game_type = $2,
                selected_battlesnakes = $3,
                search_query = $4
            WHERE flow_id = $5 AND user_id = $6
            RETURNING
                flow_id,
                board_size,
                game_type,
                selected_battlesnakes,
                search_query,
                user_id,
                created_at,
                updated_at
            "#,
            self.board_size.as_str(),
            self.game_type.as_str(),
            &self.selected_battlesnake_ids,
            self.search_query.as_deref(),
            self.flow_id,
            self.user_id
        )
        .fetch_one(pool)
        .await
        .wrap_err("Failed to update game flow")?;

        Ok(flow.into())
    }

    // Delete a flow
    pub async fn delete(pool: &PgPool, flow_id: Uuid, user_id: Uuid) -> cja::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM game_flows
            WHERE flow_id = $1 AND user_id = $2
            "#,
            flow_id,
            user_id
        )
        .execute(pool)
        .await
        .wrap_err("Failed to delete game flow")?;

        Ok(())
    }

    // Add a battlesnake to the selection (duplicates allowed)
    pub fn add_battlesnake(&mut self, battlesnake_id: Uuid) -> bool {
        // Only add if we have fewer than 4 snakes selected
        if self.selected_battlesnake_ids.len() < 4 {
            self.selected_battlesnake_ids.push(battlesnake_id);
            true
        } else {
            false // Already have 4 snakes
        }
    }

    // Remove the last occurrence of a battlesnake from the selection
    pub fn remove_battlesnake(&mut self, battlesnake_id: Uuid) -> bool {
        // Find and remove the last occurrence (allows removing duplicates one at a time)
        if let Some(pos) = self
            .selected_battlesnake_ids
            .iter()
            .rposition(|&id| id == battlesnake_id)
        {
            self.selected_battlesnake_ids.remove(pos);
            true
        } else {
            false
        }
    }

    // Check if a battlesnake is selected (at least once)
    pub fn is_battlesnake_selected(&self, battlesnake_id: &Uuid) -> bool {
        self.selected_battlesnake_ids.contains(battlesnake_id)
    }

    // Count how many times a battlesnake is selected
    pub fn battlesnake_count(&self, battlesnake_id: &Uuid) -> usize {
        self.selected_battlesnake_ids
            .iter()
            .filter(|&id| id == battlesnake_id)
            .count()
    }

    // Get count of selected snakes
    pub fn selected_count(&self) -> usize {
        self.selected_battlesnake_ids.len()
    }

    // Validate the flow state before creating a game
    pub fn validate(&self) -> cja::Result<()> {
        if self.selected_battlesnake_ids.is_empty() {
            return Err(cja::color_eyre::eyre::eyre!(
                "At least one battlesnake is required"
            ));
        }

        if self.selected_battlesnake_ids.len() > 4 {
            return Err(cja::color_eyre::eyre::eyre!(
                "Maximum of 4 battlesnakes allowed"
            ));
        }

        Ok(())
    }

    // Convert the flow to a CreateGameWithSnakes request
    pub fn to_create_game_request(&self) -> cja::Result<CreateGameWithSnakes> {
        self.validate()?;

        Ok(CreateGameWithSnakes {
            board_size: self.board_size.clone(),
            game_type: self.game_type.clone(),
            battlesnake_ids: self.selected_battlesnake_ids.clone(),
        })
    }

    // Create the game from the flow and enqueue a job to run it
    pub async fn create_game_and_enqueue(&self, app_state: AppState) -> cja::Result<Uuid> {
        let create_request = self.to_create_game_request()?;

        let game = game::create_game_with_snakes(&app_state.db, create_request)
            .await
            .wrap_err("Failed to create game")?;

        // Set enqueued_at timestamp before enqueueing the job
        game::set_game_enqueued_at(&app_state.db, game.game_id, chrono::Utc::now())
            .await
            .wrap_err("Failed to set enqueued_at")?;

        // Enqueue a job to run the game asynchronously
        let job = crate::jobs::GameRunnerJob {
            game_id: game.game_id,
        };
        cja::jobs::Job::enqueue(
            job,
            app_state,
            format!("Game {} created via flow", game.game_id),
        )
        .await
        .wrap_err("Failed to enqueue game runner job")?;

        Ok(game.game_id)
    }

    // Get all battlesnakes for the current user
    pub async fn get_user_battlesnakes(&self, pool: &PgPool) -> cja::Result<Vec<Battlesnake>> {
        battlesnake::get_battlesnakes_by_user_id(pool, self.user_id)
            .await
            .wrap_err("Failed to get user's battlesnakes")
    }

    // Search for public battlesnakes
    pub async fn search_public_battlesnakes(&self, pool: &PgPool) -> cja::Result<Vec<Battlesnake>> {
        if let Some(query) = &self.search_query {
            if query.is_empty() {
                return Ok(Vec::new());
            }

            // Search for public battlesnakes by name (case-insensitive)
            // This SQL query finds public battlesnakes that match the search query
            // and are not owned by the current user
            let battlesnakes = sqlx::query_as!(
                Battlesnake,
                r#"
                SELECT
                    battlesnake_id,
                    user_id,
                    name,
                    url,
                    visibility as "visibility: _",
                    color,
                    head,
                    tail,
                    created_at,
                    updated_at
                FROM battlesnakes
                WHERE
                    visibility = 'public'
                    AND user_id != $1
                    AND name ILIKE $2
                ORDER BY name ASC
                LIMIT 10
                "#,
                self.user_id,
                format!("%{}%", query)
            )
            .fetch_all(pool)
            .await
            .wrap_err("Failed to search public battlesnakes")?;

            Ok(battlesnakes)
        } else {
            Ok(Vec::new())
        }
    }

    // Get details of the selected battlesnakes
    pub async fn get_selected_battlesnakes(&self, pool: &PgPool) -> cja::Result<Vec<Battlesnake>> {
        if self.selected_battlesnake_ids.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<Uuid> = self.selected_battlesnake_ids.to_vec();

        let battlesnakes = sqlx::query_as!(
            Battlesnake,
            r#"
            SELECT
                battlesnake_id,
                user_id,
                name,
                url,
                visibility as "visibility: _",
                color,
                head,
                tail,
                created_at,
                updated_at
            FROM battlesnakes
            WHERE battlesnake_id = ANY($1)
            ORDER BY name ASC
            "#,
            &ids
        )
        .fetch_all(pool)
        .await
        .wrap_err("Failed to get selected battlesnakes")?;

        Ok(battlesnakes)
    }
}

// Raw database model
#[derive(Debug)]
struct GameCreationFlowRaw {
    pub flow_id: Uuid,
    pub board_size: String,
    pub game_type: String,
    pub selected_battlesnakes: Vec<Uuid>,
    pub search_query: Option<String>,
    pub user_id: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// Convert raw database model to domain model
impl From<GameCreationFlowRaw> for GameCreationFlow {
    fn from(raw: GameCreationFlowRaw) -> Self {
        let board_size =
            std::str::FromStr::from_str(&raw.board_size).unwrap_or(GameBoardSize::Medium);

        let game_type = std::str::FromStr::from_str(&raw.game_type).unwrap_or(GameType::Standard);

        Self {
            flow_id: raw.flow_id,
            board_size,
            game_type,
            selected_battlesnake_ids: raw.selected_battlesnakes,
            search_query: raw.search_query,
            user_id: raw.user_id,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_flow() -> GameCreationFlow {
        GameCreationFlow {
            flow_id: Uuid::new_v4(),
            board_size: GameBoardSize::Medium,
            game_type: GameType::Standard,
            selected_battlesnake_ids: Vec::new(),
            search_query: None,
            user_id: Uuid::new_v4(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_add_battlesnake_allows_duplicates() {
        let mut flow = create_test_flow();
        let snake_id = Uuid::new_v4();

        // Add the same snake twice
        assert!(flow.add_battlesnake(snake_id));
        assert!(flow.add_battlesnake(snake_id));

        assert_eq!(flow.selected_count(), 2);
        assert_eq!(flow.battlesnake_count(&snake_id), 2);
    }

    #[test]
    fn test_add_battlesnake_respects_max_4() {
        let mut flow = create_test_flow();
        let snake_id = Uuid::new_v4();

        // Add the same snake 4 times
        assert!(flow.add_battlesnake(snake_id));
        assert!(flow.add_battlesnake(snake_id));
        assert!(flow.add_battlesnake(snake_id));
        assert!(flow.add_battlesnake(snake_id));

        // 5th should fail
        assert!(!flow.add_battlesnake(snake_id));
        assert_eq!(flow.selected_count(), 4);
    }

    #[test]
    fn test_remove_battlesnake_removes_one_at_a_time() {
        let mut flow = create_test_flow();
        let snake_id = Uuid::new_v4();

        // Add the same snake 3 times
        flow.add_battlesnake(snake_id);
        flow.add_battlesnake(snake_id);
        flow.add_battlesnake(snake_id);
        assert_eq!(flow.battlesnake_count(&snake_id), 3);

        // Remove one
        assert!(flow.remove_battlesnake(snake_id));
        assert_eq!(flow.battlesnake_count(&snake_id), 2);

        // Remove another
        assert!(flow.remove_battlesnake(snake_id));
        assert_eq!(flow.battlesnake_count(&snake_id), 1);

        // Remove last one
        assert!(flow.remove_battlesnake(snake_id));
        assert_eq!(flow.battlesnake_count(&snake_id), 0);

        // Can't remove what's not there
        assert!(!flow.remove_battlesnake(snake_id));
    }

    #[test]
    fn test_battlesnake_count_with_mixed_snakes() {
        let mut flow = create_test_flow();
        let snake_a = Uuid::new_v4();
        let snake_b = Uuid::new_v4();

        flow.add_battlesnake(snake_a);
        flow.add_battlesnake(snake_b);
        flow.add_battlesnake(snake_a);

        assert_eq!(flow.battlesnake_count(&snake_a), 2);
        assert_eq!(flow.battlesnake_count(&snake_b), 1);
        assert_eq!(flow.selected_count(), 3);
    }

    #[test]
    fn test_is_battlesnake_selected() {
        let mut flow = create_test_flow();
        let snake_id = Uuid::new_v4();
        let other_snake = Uuid::new_v4();

        assert!(!flow.is_battlesnake_selected(&snake_id));

        flow.add_battlesnake(snake_id);
        assert!(flow.is_battlesnake_selected(&snake_id));
        assert!(!flow.is_battlesnake_selected(&other_snake));
    }

    #[test]
    fn test_validate_requires_at_least_one_snake() {
        let flow = create_test_flow();
        assert!(flow.validate().is_err());

        let mut flow_with_snake = create_test_flow();
        flow_with_snake.add_battlesnake(Uuid::new_v4());
        assert!(flow_with_snake.validate().is_ok());
    }

    #[test]
    fn test_to_create_game_request_preserves_duplicates() {
        let mut flow = create_test_flow();
        let snake_id = Uuid::new_v4();

        flow.add_battlesnake(snake_id);
        flow.add_battlesnake(snake_id);
        flow.add_battlesnake(snake_id);

        let request = flow.to_create_game_request().unwrap();
        assert_eq!(request.battlesnake_ids.len(), 3);
        assert!(request.battlesnake_ids.iter().all(|&id| id == snake_id));
    }
}
