//! API token scope vocabulary: the permission names and snake-access modes.
//!
//! Lives in the lib crate so the server and `arena-cli` share one list.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A permission held by an API token. The exact dotted names are the public
/// API surface (DB CHECK constraint values, wire format, 403 bodies) — byte
/// for byte.
#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type,
)]
#[sqlx(type_name = "text")]
pub enum Permission {
    /// `GET /api/me`
    #[serde(rename = "profile.read")]
    #[sqlx(rename = "profile.read")]
    ProfileRead,
    /// adds `email` to `/api/me`
    #[serde(rename = "email.read")]
    #[sqlx(rename = "email.read")]
    EmailRead,
    /// list and get all of the user's snakes
    #[serde(rename = "snakes.read")]
    #[sqlx(rename = "snakes.read")]
    SnakesRead,
    /// create snakes
    #[serde(rename = "snakes.create")]
    #[sqlx(rename = "snakes.create")]
    SnakesCreate,
    /// read games
    #[serde(rename = "games.read")]
    #[sqlx(rename = "games.read")]
    GamesRead,
    /// create games with any of the user's snakes
    #[serde(rename = "games.create")]
    #[sqlx(rename = "games.create")]
    GamesCreate,
    /// list, create, and revoke API tokens
    #[serde(rename = "tokens.manage")]
    #[sqlx(rename = "tokens.manage")]
    TokensManage,
    /// the admin API routes (admin users only)
    #[serde(rename = "admin")]
    #[sqlx(rename = "admin")]
    Admin,
    /// see a snake's `url` (per snake, in the token's access)
    #[serde(rename = "snake.url.read")]
    #[sqlx(rename = "snake.url.read")]
    SnakeUrlRead,
    /// change a snake's `url`
    #[serde(rename = "snake.url.write")]
    #[sqlx(rename = "snake.url.write")]
    SnakeUrlWrite,
    /// change a snake's editable fields except `url`
    #[serde(rename = "snake.edit")]
    #[sqlx(rename = "snake.edit")]
    SnakeEdit,
    /// delete a snake
    #[serde(rename = "snake.delete")]
    #[sqlx(rename = "snake.delete")]
    SnakeDelete,
    /// join and leave leaderboards
    #[serde(rename = "snake.compete")]
    #[sqlx(rename = "snake.compete")]
    SnakeCompete,
}

impl Permission {
    /// All 13 permissions in canonical order. `as_str()` output must equal the
    /// migration's CHECK-constraint names byte for byte.
    pub const ALL: [Permission; 13] = [
        Permission::ProfileRead,
        Permission::EmailRead,
        Permission::SnakesRead,
        Permission::SnakesCreate,
        Permission::GamesRead,
        Permission::GamesCreate,
        Permission::TokensManage,
        Permission::Admin,
        Permission::SnakeUrlRead,
        Permission::SnakeUrlWrite,
        Permission::SnakeEdit,
        Permission::SnakeDelete,
        Permission::SnakeCompete,
    ];

    /// Permissions an OAuth client may request. `tokens.manage` and `admin`
    /// are personal-only forever: requesting either is `invalid_scope`. Keep
    /// the CHECK list in the oauth_authorization_code_permissions migration
    /// in sync when permissions are added.
    pub const OAUTH_REQUESTABLE: [Permission; 11] = [
        Permission::ProfileRead,
        Permission::EmailRead,
        Permission::SnakesRead,
        Permission::SnakesCreate,
        Permission::GamesRead,
        Permission::GamesCreate,
        Permission::SnakeUrlRead,
        Permission::SnakeUrlWrite,
        Permission::SnakeEdit,
        Permission::SnakeDelete,
        Permission::SnakeCompete,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Permission::ProfileRead => "profile.read",
            Permission::EmailRead => "email.read",
            Permission::SnakesRead => "snakes.read",
            Permission::SnakesCreate => "snakes.create",
            Permission::GamesRead => "games.read",
            Permission::GamesCreate => "games.create",
            Permission::TokensManage => "tokens.manage",
            Permission::Admin => "admin",
            Permission::SnakeUrlRead => "snake.url.read",
            Permission::SnakeUrlWrite => "snake.url.write",
            Permission::SnakeEdit => "snake.edit",
            Permission::SnakeDelete => "snake.delete",
            Permission::SnakeCompete => "snake.compete",
        }
    }

    /// One plain-words sentence describing what this permission lets an app
    /// do, honest about account-wide vs per-snake scope (the snake selection
    /// only gates per-snake actions). Shown on the OAuth approval screen and
    /// mirrored in docs/oauth.md.
    pub const fn description(self) -> &'static str {
        match self {
            Permission::ProfileRead => "See your public profile.",
            Permission::EmailRead => "See your email address.",
            Permission::SnakesRead => {
                "See the names and settings of all your battlesnakes (not their URLs)."
            }
            Permission::SnakesCreate => "Create battlesnakes for you.",
            Permission::GamesRead => "See your games.",
            Permission::GamesCreate => "Start games using any of your battlesnakes.",
            Permission::TokensManage => "Manage your API tokens.",
            Permission::Admin => "Act as an administrator.",
            Permission::SnakeUrlRead => {
                "See where a battlesnake runs (its URL), for the snakes you pick below."
            }
            Permission::SnakeUrlWrite => {
                "Change where a battlesnake runs (its URL), for the \
                snakes you pick below. This lets the app take control of where your snake runs."
            }
            Permission::SnakeEdit => {
                "Edit a battlesnake's name, visibility, and region, for the snakes you pick below."
            }
            Permission::SnakeDelete => "Delete battlesnakes this app created for you.",
            Permission::SnakeCompete => {
                "Enter and leave leaderboards with your battlesnakes, for the snakes you pick below."
            }
        }
    }
}

impl FromStr for Permission {
    type Err = color_eyre::eyre::Report;

    /// Strict: no normalization, unknown strings are errors (the 400 path
    /// in `POST /api/tokens` depends on it).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|permission| permission.as_str() == s)
            .ok_or_else(|| color_eyre::eyre::eyre!("Unknown permission: {s}"))
    }
}

/// Which of the user's snakes a token may touch. `All` covers every snake the
/// user owns, now and later; `Selected` covers an explicit list (snakes the
/// token creates join it).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SnakeAccess {
    All,
    Selected,
}

impl SnakeAccess {
    pub const fn as_str(self) -> &'static str {
        match self {
            SnakeAccess::All => "all",
            SnakeAccess::Selected => "selected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_names_round_trip_and_are_exact() {
        // as_str output must match FromStr, serde, and the migration CHECK names.
        for permission in Permission::ALL {
            let name = permission.as_str();
            assert_eq!(name.parse::<Permission>().unwrap(), permission);
            assert_eq!(
                serde_json::to_value(permission).unwrap(),
                serde_json::json!(name)
            );
        }
        assert_eq!(
            Permission::ALL
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>(),
            vec![
                "profile.read",
                "email.read",
                "snakes.read",
                "snakes.create",
                "games.read",
                "games.create",
                "tokens.manage",
                "admin",
                "snake.url.read",
                "snake.url.write",
                "snake.edit",
                "snake.delete",
                "snake.compete",
            ]
        );
        // Strictness: no normalization, no unknown strings accepted.
        assert!("Profile.Read".parse::<Permission>().is_err());
        assert!("PROFILE.READ".parse::<Permission>().is_err());
        assert!("nope".parse::<Permission>().is_err());
        assert!("admin ".parse::<Permission>().is_err());
    }

    #[test]
    fn snake_access_serializes_to_wire_names() {
        for access in [SnakeAccess::All, SnakeAccess::Selected] {
            assert_eq!(
                serde_json::to_value(access).unwrap(),
                serde_json::json!(access.as_str())
            );
        }
    }

    #[test]
    fn every_permission_has_a_description() {
        for permission in Permission::ALL {
            let description = permission.description();
            assert!(
                !description.is_empty(),
                "{} needs a description",
                permission.as_str()
            );
        }
    }

    #[test]
    fn descriptions_are_honest_about_scope() {
        // Account-wide permissions must not claim the snake selection limits
        // them; per-snake ones must say which snakes; snake.delete says whose
        // snakes it covers; games.create says "any".
        let snakes_read = Permission::SnakesRead.description();
        assert!(snakes_read.contains("all your battlesnakes"));
        let games_create = Permission::GamesCreate.description();
        assert!(games_create.to_lowercase().contains("any"));
        let url_write = Permission::SnakeUrlWrite.description();
        assert!(url_write.contains("take control"));
        assert!(url_write.contains("URL"));
        assert_ne!(
            Permission::SnakeUrlRead.description(),
            Permission::SnakeUrlWrite.description(),
            "url read/write must be distinct lines"
        );
        assert!(
            Permission::SnakeDelete
                .description()
                .contains("this app created")
        );
        for permission in [
            Permission::SnakeUrlRead,
            Permission::SnakeUrlWrite,
            Permission::SnakeEdit,
            Permission::SnakeCompete,
        ] {
            assert!(
                permission.description().contains("snakes you pick"),
                "{} must name the selection",
                permission.as_str()
            );
        }
    }

    #[test]
    fn oauth_requestable_exactly_excludes_personal_only() {
        let requestable: std::collections::BTreeSet<Permission> =
            Permission::OAUTH_REQUESTABLE.into_iter().collect();
        assert!(!requestable.contains(&Permission::TokensManage));
        assert!(!requestable.contains(&Permission::Admin));
        let all: std::collections::BTreeSet<Permission> = Permission::ALL.into_iter().collect();
        let personal_only: std::collections::BTreeSet<Permission> =
            [Permission::TokensManage, Permission::Admin]
                .into_iter()
                .collect();
        assert_eq!(
            requestable,
            all.difference(&personal_only).copied().collect(),
            "OAUTH_REQUESTABLE = ALL minus tokens.manage/admin, exactly"
        );
        // And every name parses — mirrors the migration CHECK list.
        for permission in Permission::OAUTH_REQUESTABLE {
            assert_eq!(
                permission.as_str().parse::<Permission>().unwrap(),
                permission
            );
        }
    }
}
