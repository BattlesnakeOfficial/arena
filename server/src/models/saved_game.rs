use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

/// A game a user saved to their public profile.
#[derive(Debug, Clone)]
pub struct SavedGame {
    pub saved_game_id: Uuid,
    pub user_id: Uuid,
    pub game_id: Uuid,
    pub title: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A saved game joined with the game it points at, shaped for profile
/// rendering: link target, title (possibly empty), and enough game metadata
/// to build a fallback title and show a date.
#[derive(Debug, Clone)]
pub struct SavedGameListing {
    pub saved_game_id: Uuid,
    pub game_id: Uuid,
    pub title: String,
    pub game_type: String,
    pub board_size: String,
    pub game_created_at: chrono::DateTime<chrono::Utc>,
}

impl SavedGameListing {
    /// Title shown on the profile: the user's title, or a description of the
    /// game ("Standard on 11x11") when they didn't provide one.
    pub fn display_title(&self) -> String {
        if self.title.is_empty() {
            format!("{} on {}", self.game_type, self.board_size)
        } else {
            self.title.clone()
        }
    }
}

/// Save a game to a user's profile. Upserts on (user_id, game_id): saving a
/// game the user already saved just updates the title.
pub async fn save_game(
    pool: &PgPool,
    user_id: Uuid,
    game_id: Uuid,
    title: &str,
) -> cja::Result<SavedGame> {
    let saved = sqlx::query_as!(
        SavedGame,
        r#"
        INSERT INTO saved_games (user_id, game_id, title)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id, game_id)
        DO UPDATE SET title = EXCLUDED.title, updated_at = NOW()
        RETURNING
            saved_game_id,
            user_id,
            game_id,
            title,
            created_at,
            updated_at
        "#,
        user_id,
        game_id,
        title
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to save game")?;

    Ok(saved)
}

/// Get a saved game by its id.
pub async fn get_saved_game_by_id(
    pool: &PgPool,
    saved_game_id: Uuid,
) -> cja::Result<Option<SavedGame>> {
    let saved = sqlx::query_as!(
        SavedGame,
        r#"
        SELECT
            saved_game_id,
            user_id,
            game_id,
            title,
            created_at,
            updated_at
        FROM saved_games
        WHERE saved_game_id = $1
        "#,
        saved_game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch saved game")?;

    Ok(saved)
}

/// Get the viewer's saved-game row for a specific game, if any. Used by the
/// game page to pre-fill the save form with the existing title.
pub async fn get_saved_game_for_user_and_game(
    pool: &PgPool,
    user_id: Uuid,
    game_id: Uuid,
) -> cja::Result<Option<SavedGame>> {
    let saved = sqlx::query_as!(
        SavedGame,
        r#"
        SELECT
            saved_game_id,
            user_id,
            game_id,
            title,
            created_at,
            updated_at
        FROM saved_games
        WHERE user_id = $1 AND game_id = $2
        "#,
        user_id,
        game_id
    )
    .fetch_optional(pool)
    .await
    .wrap_err("Failed to fetch saved game for user and game")?;

    Ok(saved)
}

/// Delete a saved game, scoped by owner. Returns true if a row was deleted;
/// false means the saved game doesn't exist or belongs to someone else.
pub async fn delete_saved_game(
    pool: &PgPool,
    saved_game_id: Uuid,
    user_id: Uuid,
) -> cja::Result<bool> {
    let result = sqlx::query!(
        "DELETE FROM saved_games WHERE saved_game_id = $1 AND user_id = $2",
        saved_game_id,
        user_id
    )
    .execute(pool)
    .await
    .wrap_err("Failed to delete saved game")?;

    Ok(result.rows_affected() > 0)
}

/// List a user's saved games for their profile, newest save first, joined
/// with the games table for fallback-title metadata.
pub async fn list_saved_games_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> cja::Result<Vec<SavedGameListing>> {
    let listings = sqlx::query_as!(
        SavedGameListing,
        r#"
        SELECT
            sg.saved_game_id,
            sg.game_id,
            sg.title,
            g.game_type,
            g.board_size,
            g.created_at AS game_created_at
        FROM saved_games sg
        JOIN games g ON g.game_id = sg.game_id
        WHERE sg.user_id = $1
        ORDER BY sg.created_at DESC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list saved games for user")?;

    Ok(listings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::game::{CreateGame, GameBoardSize, GameType, create_game};

    async fn create_user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
             RETURNING user_id",
            github_id,
            format!("gh-user-{github_id}"),
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    async fn create_test_game(pool: &PgPool) -> cja::Result<Uuid> {
        let game = create_game(
            pool,
            CreateGame {
                board_size: GameBoardSize::Medium,
                game_type: GameType::Standard,
            },
        )
        .await?;
        Ok(game.game_id)
    }

    #[test]
    fn display_title_falls_back_to_game_description() {
        let listing = SavedGameListing {
            saved_game_id: Uuid::nil(),
            game_id: Uuid::nil(),
            title: String::new(),
            game_type: "Standard".to_string(),
            board_size: "11x11".to_string(),
            game_created_at: chrono::Utc::now(),
        };
        assert_eq!(listing.display_title(), "Standard on 11x11");

        let titled = SavedGameListing {
            title: "Epic comeback".to_string(),
            ..listing
        };
        assert_eq!(titled.display_title(), "Epic comeback");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn save_game_upserts_on_user_and_game(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9101).await?;
        let game_id = create_test_game(&pool).await?;

        let first = save_game(&pool, user_id, game_id, "First title").await?;
        assert_eq!(first.title, "First title");

        // Re-saving the same game updates the title in place.
        let second = save_game(&pool, user_id, game_id, "Second title").await?;
        assert_eq!(second.saved_game_id, first.saved_game_id);
        assert_eq!(second.title, "Second title");

        let listings = list_saved_games_for_user(&pool, user_id).await?;
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].title, "Second title");
        assert_eq!(listings[0].game_type, "Standard");
        assert_eq!(listings[0].board_size, "11x11");

        // A different user saving the same game gets their own row.
        let other_user_id = create_user(&pool, 9102).await?;
        let other = save_game(&pool, other_user_id, game_id, "").await?;
        assert_ne!(other.saved_game_id, first.saved_game_id);

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn delete_saved_game_is_owner_scoped(pool: PgPool) -> cja::Result<()> {
        let owner_id = create_user(&pool, 9103).await?;
        let stranger_id = create_user(&pool, 9104).await?;
        let game_id = create_test_game(&pool).await?;

        let saved = save_game(&pool, owner_id, game_id, "Mine").await?;

        // A different user can't delete it.
        assert!(!delete_saved_game(&pool, saved.saved_game_id, stranger_id).await?);
        assert!(
            get_saved_game_by_id(&pool, saved.saved_game_id)
                .await?
                .is_some()
        );

        // The owner can.
        assert!(delete_saved_game(&pool, saved.saved_game_id, owner_id).await?);
        assert!(
            get_saved_game_by_id(&pool, saved.saved_game_id)
                .await?
                .is_none()
        );

        Ok(())
    }
}
