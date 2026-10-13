//! Private leaderboards: unlisted ladders that only admins create. Anyone
//! holding the direct link can view one, and it plays exactly like a public
//! ladder (matchmaking, ratings, health sweeps). Joining takes the invite
//! code unless the user already has a snake on it.

use std::ops::RangeInclusive;

use chrono::{DateTime, Utc};
use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{
    battlesnake::Visibility,
    customization_unlock_code::{ValidationError, generate_code},
    leaderboard::Leaderboard,
};

/// Game types a leaderboard can run (mirrors the `leaderboards.game_type` CHECK).
pub const GAME_TYPES: &[&str] = &["Standard", "Royale", "Constrictor", "Snail Mode"];
/// Board sizes a leaderboard can run (mirrors the `leaderboards.board_size` CHECK).
pub const BOARD_SIZES: &[&str] = &["7x7", "11x11", "19x19"];
/// Snakes per game (mirrors the `leaderboards.match_size` CHECK).
pub const MATCH_SIZES: RangeInclusive<i32> = 2..=4;
const MAX_NAME_CHARS: usize = 100;

pub struct CreatePrivateLeaderboard {
    pub name: String,
    pub game_type: String,
    pub board_size: String,
    pub match_size: i32,
}

/// One row of the admin leaderboard list. Covers public leaderboards too;
/// `invite_code` is only set for private ones.
#[derive(Debug)]
pub struct AdminLeaderboardListing {
    pub leaderboard_id: Uuid,
    pub name: String,
    pub game_type: String,
    pub board_size: String,
    pub match_size: i32,
    pub visibility: Visibility,
    pub invite_code: Option<String>,
    pub active_entries: i64,
    pub disabled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Invite codes are typed by hand, so accept stray whitespace and lowercase.
fn normalize_code(submitted: &str) -> String {
    submitted.trim().to_ascii_uppercase()
}

/// A user with a (non-deleted) snake on the leaderboard, active or paused, is
/// a member — see the `leaderboard_member` SQL function, which profile
/// listings share. The leaderboard page and the join check both use this, so
/// the page never hides the invite-code field from someone the join would refuse.
pub async fn is_member(pool: &PgPool, leaderboard_id: Uuid, user_id: Uuid) -> cja::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT leaderboard_member($1, $2) AS "member!""#,
        leaderboard_id,
        user_id
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check leaderboard membership")
}

/// Whether `user_id` may put a snake on `lb`. Public leaderboards are open to
/// everyone. Private ones admit members without a code — that's what lets
/// Resume and adding another snake work — and everyone else only with the
/// current invite code. Every join path (web form and API) goes through here.
pub async fn join_allowed(
    pool: &PgPool,
    lb: &Leaderboard,
    user_id: Uuid,
    invite_code: Option<&str>,
) -> cja::Result<bool> {
    if lb.visibility == Visibility::Public || is_member(pool, lb.leaderboard_id, user_id).await? {
        return Ok(true);
    }
    let Some(code) = invite_code.map(normalize_code).filter(|c| !c.is_empty()) else {
        return Ok(false);
    };
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
            SELECT 1 FROM leaderboards WHERE leaderboard_id = $1 AND invite_code = $2
        ) AS "valid!""#,
        lb.leaderboard_id,
        code
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to check leaderboard invite code")
}

/// Create a private leaderboard with a fresh invite code; returns its id.
pub async fn create(pool: &PgPool, input: &CreatePrivateLeaderboard) -> cja::Result<Uuid> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ValidationError("Name is required").into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ValidationError("Name must be 100 characters or fewer").into());
    }
    if !GAME_TYPES.contains(&input.game_type.as_str()) {
        return Err(ValidationError("Choose a valid game type").into());
    }
    if !BOARD_SIZES.contains(&input.board_size.as_str()) {
        return Err(ValidationError("Choose a valid board size").into());
    }
    if !MATCH_SIZES.contains(&input.match_size) {
        return Err(ValidationError("Snakes per game must be between 2 and 4").into());
    }

    sqlx::query_scalar!(
        "INSERT INTO leaderboards (name, game_type, board_size, match_size, visibility, invite_code)
         VALUES ($1, $2, $3, $4, 'private', $5)
         RETURNING leaderboard_id",
        name,
        input.game_type,
        input.board_size,
        input.match_size,
        generate_code(),
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to create private leaderboard")
}

/// Replace a private leaderboard's invite code, so a leaked code stops
/// admitting new members. Existing members keep their place. Returns `false`
/// when no private leaderboard has that id.
pub async fn regenerate_invite_code(pool: &PgPool, leaderboard_id: Uuid) -> cja::Result<bool> {
    let result = sqlx::query!(
        "UPDATE leaderboards SET invite_code = $2
         WHERE leaderboard_id = $1 AND visibility = 'private'",
        leaderboard_id,
        generate_code(),
    )
    .execute(pool)
    .await
    .wrap_err("Failed to regenerate invite code")?;
    Ok(result.rows_affected() == 1)
}

/// Every leaderboard, public and private, for the admin list.
pub async fn list_for_admin(pool: &PgPool) -> cja::Result<Vec<AdminLeaderboardListing>> {
    sqlx::query_as!(
        AdminLeaderboardListing,
        r#"SELECT l.leaderboard_id, l.name, l.game_type, l.board_size, l.match_size,
                  l.visibility as "visibility: Visibility",
                  l.invite_code, l.disabled_at, l.created_at,
                  (SELECT COUNT(*) FROM leaderboard_entries le
                    WHERE le.leaderboard_id = l.leaderboard_id
                      AND le.disabled_at IS NULL) AS "active_entries!"
           FROM leaderboards l
           ORDER BY l.visibility = 'private', l.created_at ASC, l.name ASC"#
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list leaderboards for admin")
}
