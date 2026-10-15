use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context as _, eyre};
use std::time::Duration;

// Include the cli module from the library
use arena::api_scope::Permission;
use arena::cli::config::{AuthConfig, CliConfig};
use arena::cli::output::{
    OutputFormat, format_timestamp, print_field, print_success, print_table, status_colored,
};

#[derive(Parser)]
#[command(name = "arena")]
#[command(about = "Battlesnake CLI", long_about = None)]
struct Cli {
    /// Output format: 'json' or 'human'. Default: human for TTY, json for pipes.
    #[arg(long, global = true)]
    format: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authentication commands
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Show who you are logged in as
    Whoami,
    /// Battlesnake management commands
    Snakes {
        #[command(subcommand)]
        command: SnakesCommands,
    },
    /// Game management commands
    Games {
        #[command(subcommand)]
        command: GamesCommands,
    },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Login via GitHub OAuth and store API token
    Login,
    /// Logout and clear stored token
    Logout,
    /// API token management
    Token {
        #[command(subcommand)]
        command: TokenCommands,
    },
}

#[derive(Subcommand)]
enum TokenCommands {
    /// Create a new API token
    Create {
        /// Name for the token (e.g., "My laptop", "CI")
        #[arg(short, long)]
        name: Option<String>,
        /// Permission to grant (repeatable). Combines with --all-permissions as a union.
        #[arg(long = "permission", value_name = "PERM")]
        permission: Vec<String>,
        /// Grant every non-admin permission (add --permission admin on top if you're an admin)
        #[arg(long)]
        all_permissions: bool,
        /// Restrict the token to these snake IDs (repeatable; omitted means all snakes)
        #[arg(long = "snake", value_name = "ID")]
        snake: Vec<String>,
    },
    /// List all active API tokens
    List,
    /// Revoke an API token
    Revoke {
        /// Token ID to revoke
        id: String,
    },
}

/// Build the `POST /api/tokens` body for `token create`. Snake IDs are
/// checked client-side; permission names go through as given, so the
/// server's 400 (which names the bad permission) is the single authority and
/// an older CLI can still request permissions a newer server added.
fn build_create_token_body(
    name: &str,
    permission: &[String],
    all_permissions: bool,
    snake: &[String],
) -> color_eyre::Result<serde_json::Value> {
    if permission.is_empty() && !all_permissions {
        return Err(eyre!(
            "pass --permission <p> (repeatable), --all-permissions, or both"
        ));
    }
    let mut permissions: Vec<String> = Vec::new();
    if all_permissions {
        permissions.extend(
            Permission::ALL
                .into_iter()
                .filter(|p| *p != Permission::Admin)
                .map(|p| p.as_str().to_string()),
        );
    }
    for permission_name in permission {
        // Union, de-duplicated (order-preserving).
        if !permissions.contains(permission_name) {
            permissions.push(permission_name.clone());
        }
    }
    if snake.is_empty() {
        Ok(serde_json::json!({
            "name": name,
            "permissions": permissions,
            "snake_access": "all",
        }))
    } else {
        let snake_ids: Vec<String> = snake
            .iter()
            .map(|id| {
                uuid::Uuid::parse_str(id)
                    .map(|id| id.to_string())
                    .map_err(|_| eyre!("invalid snake id: {id}"))
            })
            .collect::<color_eyre::Result<_>>()?;
        Ok(serde_json::json!({
            "name": name,
            "permissions": permissions,
            "snake_access": "selected",
            "snake_ids": snake_ids,
        }))
    }
}

/// Failure of a token-create request: either the server answered no
/// (status + body) or the request never landed.
#[derive(Debug)]
enum CreateTokenError {
    Status(reqwest::StatusCode, String),
    Transport(color_eyre::Report),
}

impl CreateTokenError {
    fn into_report(self) -> color_eyre::Report {
        match self {
            // The subset-rule 403: tokens minted before scoped permissions
            // lack email.read, so `--all-permissions` from one of them hits it.
            CreateTokenError::Status(status, body)
                if status == reqwest::StatusCode::FORBIDDEN
                    && body.contains("exceed this token's") =>
            {
                eyre!(
                    "Failed to create token: {status} - {body}\n\
                     A token can only create tokens within its own permissions and snake \
                     access. If you logged in before token permissions existed, run \
                     'arena auth login' for a token with every permission."
                )
            }
            CreateTokenError::Status(status, body) => {
                eyre!("Failed to create token: {status} - {body}")
            }
            CreateTokenError::Transport(report) => report,
        }
    }
}

/// Send the token-create request. Split from `handle_token_command` so tests
/// can target a wiremock server (`CliConfig::load()` reads the real home
/// config, which is not injectable).
async fn post_create_token(
    base_url: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, CreateTokenError> {
    let response = reqwest::Client::new()
        .post(format!("{base_url}/api/tokens"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| CreateTokenError::Transport(eyre!("Failed to create token: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(CreateTokenError::Status(status, body));
    }
    response
        .json()
        .await
        .map_err(|e| CreateTokenError::Transport(eyre!("Failed to parse token response: {e}")))
}

#[derive(Subcommand)]
enum SnakesCommands {
    /// List all your snakes
    List,
    /// Create a new snake
    Create {
        /// Name for the snake
        name: String,
        /// URL for the snake server
        url: String,
        /// Make the snake public (visible to other users)
        #[arg(long)]
        public: bool,
    },
    /// Show details of a snake
    Show {
        /// Snake ID
        id: String,
    },
    /// Edit an existing snake
    Edit {
        /// Snake ID
        id: String,
        /// New name for the snake
        #[arg(long)]
        name: Option<String>,
        /// New URL for the snake server
        #[arg(long)]
        url: Option<String>,
        /// Make the snake public
        #[arg(long, conflicts_with = "private")]
        public: bool,
        /// Make the snake private
        #[arg(long, conflicts_with = "public")]
        private: bool,
    },
    /// Delete a snake
    Delete {
        /// Snake ID
        id: String,
    },
}

#[derive(Subcommand)]
enum GamesCommands {
    /// List your games
    List {
        /// Filter by snake ID
        #[arg(long)]
        snake: Option<String>,
        /// Maximum number of games to return
        #[arg(long, default_value = "20")]
        limit: u32,
    },
    /// Create a new game
    Create {
        /// Comma-separated snake IDs (required)
        #[arg(long)]
        snakes: String,
        /// Board size (7x7, 11x11, 19x19)
        #[arg(long, default_value = "11x11")]
        board: String,
        /// Game type (standard, royale, constrictor, snail)
        #[arg(long = "type", default_value = "standard")]
        game_type: String,
    },
    /// Show game details
    Show {
        /// Game ID
        id: String,
    },
    /// Watch a game
    Watch {
        /// Game ID
        id: String,
        /// Open in browser instead of polling
        #[arg(long)]
        web: bool,
    },
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    // Determine output format based on flag and TTY detection
    let output_format =
        OutputFormat::from_flag(cli.format.as_deref()).map_err(|e| eyre!("{}", e))?;

    match cli.command {
        Commands::Auth { command } => handle_auth_command(command).await?,
        Commands::Whoami => handle_whoami(output_format).await?,
        Commands::Snakes { command } => handle_snakes_command(command, output_format).await?,
        Commands::Games { command } => handle_games_command(command).await?,
    }

    Ok(())
}

async fn handle_auth_command(command: AuthCommands) -> color_eyre::Result<()> {
    match command {
        AuthCommands::Login => {
            login().await?;
        }
        AuthCommands::Logout => {
            logout()?;
        }
        AuthCommands::Token { command } => {
            handle_token_command(command).await?;
        }
    }
    Ok(())
}

async fn handle_token_command(command: TokenCommands) -> color_eyre::Result<()> {
    let config = CliConfig::load()?;
    let token = config
        .auth
        .as_ref()
        .and_then(|a| a.token.as_ref())
        .ok_or_else(|| eyre!("Not logged in. Run 'arena auth login' first."))?;

    let client = reqwest::Client::new();
    let base_url = config.api_url();

    match command {
        TokenCommands::Create {
            name,
            permission,
            all_permissions,
            snake,
        } => {
            let name = name.unwrap_or_else(|| {
                hostname::get()
                    .ok()
                    .and_then(|h| h.into_string().ok())
                    .unwrap_or_else(|| "CLI Token".to_string())
            });

            let body = build_create_token_body(&name, &permission, all_permissions, &snake)?;
            let result = post_create_token(base_url, token, &body)
                .await
                .map_err(CreateTokenError::into_report)?;

            println!("Token created successfully!");
            println!("ID: {}", result["id"]);
            println!("Name: {}", result["name"]);
            let permissions = result["permissions"]
                .as_array()
                .map(|p| {
                    p.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            println!("Permissions: {permissions}");
            let access = result["snake_access"].as_str().unwrap_or("all");
            let ids = result["snake_ids"].as_array().map(|a| a.len()).unwrap_or(0);
            if access == "selected" {
                println!(
                    "Access: selected ({ids} snake{})",
                    if ids == 1 { "" } else { "s" }
                );
            } else {
                println!("Access: all");
            }
            println!("\nSecret (save this - it won't be shown again):");
            println!("{}", result["secret"]);
        }
        TokenCommands::List => {
            let response = client
                .get(format!("{}/api/tokens", base_url))
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to list tokens")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to list tokens: {} - {}", status, body));
            }

            let tokens: Vec<serde_json::Value> = response.json().await?;

            if tokens.is_empty() {
                println!("No active tokens found.");
            } else {
                // PERMISSIONS is last and unpadded: a full token's list runs
                // ~150 characters, so no fixed width fits it.
                println!(
                    "{:<38} {:<20} {:<20} {:<14} PERMISSIONS",
                    "ID", "NAME", "LAST USED", "ACCESS"
                );
                println!("{}", "-".repeat(106));
                for token in tokens {
                    let last_used = token["last_used_at"].as_str().unwrap_or("Never");
                    let permissions = token["permissions"]
                        .as_array()
                        .map(|p| {
                            p.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    let access = match token["snake_access"].as_str() {
                        Some("selected") => format!(
                            "selected ({})",
                            token["snake_ids"].as_array().map(|a| a.len()).unwrap_or(0)
                        ),
                        _ => "all".to_string(),
                    };
                    println!(
                        "{:<38} {:<20} {:<20} {:<14} {}",
                        token["id"].as_str().unwrap_or(""),
                        token["name"].as_str().unwrap_or(""),
                        last_used,
                        access,
                        permissions
                    );
                }
            }
        }
        TokenCommands::Revoke { id } => {
            let response = client
                .delete(format!("{}/api/tokens/{}", base_url, id))
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to revoke token")?;

            if response.status() == reqwest::StatusCode::NO_CONTENT {
                println!("Token revoked successfully.");
            } else if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(eyre!("Token not found or already revoked."));
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to revoke token: {} - {}", status, body));
            }
        }
    }

    Ok(())
}

async fn handle_snakes_command(
    command: SnakesCommands,
    output_format: OutputFormat,
) -> color_eyre::Result<()> {
    let config = CliConfig::load()?;
    let token = config
        .auth
        .as_ref()
        .and_then(|a| a.token.as_ref())
        .ok_or_else(|| eyre!("Not logged in. Run 'arena auth login' first."))?;

    let client = reqwest::Client::new();
    let base_url = config.api_url();

    match command {
        SnakesCommands::List => {
            let response = client
                .get(format!("{}/api/snakes", base_url))
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to list snakes")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to list snakes: {} - {}", status, body));
            }

            let snakes: Vec<serde_json::Value> = response.json().await?;

            match output_format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&snakes)?);
                }
                OutputFormat::Human => {
                    if snakes.is_empty() {
                        println!("No snakes found. Create one with 'arena snakes create'.");
                    } else {
                        println!("Your snakes:\n");
                        let rows: Vec<Vec<String>> = snakes
                            .iter()
                            .map(|snake| {
                                let visibility = if snake["is_public"].as_bool().unwrap_or(false) {
                                    "public"
                                } else {
                                    "private"
                                };
                                vec![
                                    snake["name"].as_str().unwrap_or("").to_string(),
                                    snake["url"].as_str().unwrap_or("").to_string(),
                                    status_colored(visibility),
                                ]
                            })
                            .collect();
                        print_table(vec!["NAME", "URL", "VISIBILITY"], rows);
                    }
                }
            }
        }
        SnakesCommands::Create { name, url, public } => {
            let response = client
                .post(format!("{}/api/snakes", base_url))
                .bearer_auth(token)
                .json(&serde_json::json!({
                    "name": name,
                    "url": url,
                    "is_public": public
                }))
                .send()
                .await
                .wrap_err("Failed to create snake")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to create snake: {} - {}", status, body));
            }

            let snake: serde_json::Value = response.json().await?;

            match output_format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&snake)?);
                }
                OutputFormat::Human => {
                    print_success("Snake created successfully!\n");
                    print_snake_details(&snake);
                }
            }
        }
        SnakesCommands::Show { id } => {
            let response = client
                .get(format!("{}/api/snakes/{}", base_url, id))
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to get snake")?;

            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(eyre!("Snake not found."));
            } else if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to get snake: {} - {}", status, body));
            }

            let snake: serde_json::Value = response.json().await?;

            match output_format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&snake)?);
                }
                OutputFormat::Human => {
                    print_snake_details(&snake);
                }
            }
        }
        SnakesCommands::Edit {
            id,
            name,
            url,
            public,
            private,
        } => {
            // Build the update payload with only provided fields
            let mut update: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            if let Some(name) = name {
                update.insert("name".to_string(), serde_json::Value::String(name));
            }
            if let Some(url) = url {
                update.insert("url".to_string(), serde_json::Value::String(url));
            }
            if public {
                update.insert("is_public".to_string(), serde_json::Value::Bool(true));
            } else if private {
                update.insert("is_public".to_string(), serde_json::Value::Bool(false));
            }

            let response = client
                .put(format!("{}/api/snakes/{}", base_url, id))
                .bearer_auth(token)
                .json(&update)
                .send()
                .await
                .wrap_err("Failed to update snake")?;

            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(eyre!("Snake not found."));
            } else if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to update snake: {} - {}", status, body));
            }

            let snake: serde_json::Value = response.json().await?;

            match output_format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&snake)?);
                }
                OutputFormat::Human => {
                    print_success("Snake updated successfully!\n");
                    print_snake_details(&snake);
                }
            }
        }
        SnakesCommands::Delete { id } => {
            let response = client
                .delete(format!("{}/api/snakes/{}", base_url, id))
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to delete snake")?;

            if response.status() == reqwest::StatusCode::NO_CONTENT {
                match output_format {
                    OutputFormat::Json => {
                        println!(
                            "{}",
                            serde_json::json!({
                                "status": "deleted",
                                "id": id
                            })
                        );
                    }
                    OutputFormat::Human => {
                        print_success("Snake deleted successfully.");
                    }
                }
            } else if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(eyre!("Snake not found."));
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to delete snake: {} - {}", status, body));
            }
        }
    }

    Ok(())
}

/// Print snake details in human-readable format.
fn print_snake_details(snake: &serde_json::Value) {
    print_field("Name", snake["name"].as_str().unwrap_or(""));
    print_field("ID", snake["id"].as_str().unwrap_or(""));
    print_field("URL", snake["url"].as_str().unwrap_or(""));

    let visibility = if snake["is_public"].as_bool().unwrap_or(false) {
        "public"
    } else {
        "private"
    };
    print_field("Visibility", &status_colored(visibility));

    if let Some(created) = snake["created_at"].as_str()
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(created)
    {
        print_field("Created", &format_timestamp(dt.with_timezone(&chrono::Utc)));
    }

    if let Some(updated) = snake["updated_at"].as_str()
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(updated)
    {
        print_field("Updated", &format_timestamp(dt.with_timezone(&chrono::Utc)));
    }
}

/// Error from `fetch_me`: distinguishes "the server answered no" (invalid or
/// expired token, server error) from "we never got an answer" (transport).
#[derive(Debug)]
enum FetchMeError {
    /// Non-2xx HTTP response, with the body for context.
    Status(reqwest::StatusCode, String),
    /// Transport or parse failure; the eyre context chain is preserved.
    Request(color_eyre::Report),
}

/// GET /api/me with the given token; returns the parsed response body.
async fn fetch_me(base_url: &str, token: &str) -> Result<serde_json::Value, FetchMeError> {
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{base_url}/api/me"))
        .bearer_auth(token)
        .send()
        .await
        .wrap_err("Failed to contact arena")
        .map_err(FetchMeError::Request)?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(FetchMeError::Status(status, body));
    }

    response
        .json()
        .await
        .wrap_err("Failed to parse response")
        .map_err(FetchMeError::Request)
}

/// The label/value rows `whoami` prints.
fn whoami_fields(me: &serde_json::Value) -> Vec<(&'static str, String)> {
    vec![
        (
            "Login",
            me["github_login"].as_str().unwrap_or("").to_string(),
        ),
        (
            "Display name",
            me["display_name"].as_str().unwrap_or("").to_string(),
        ),
        ("ID", me["id"].as_str().unwrap_or("").to_string()),
    ]
}

async fn handle_whoami(output_format: OutputFormat) -> color_eyre::Result<()> {
    let config = CliConfig::load()?;
    let token = config
        .auth
        .as_ref()
        .and_then(|a| a.token.as_ref())
        .ok_or_else(|| eyre!("Not logged in. Run 'arena auth login' first."))?;

    let me = match fetch_me(config.api_url(), token).await {
        Ok(me) => me,
        // whoami's whole job is reporting auth state: a 401 IS the answer, so
        // say it in the same friendly terms as the not-logged-in path rather
        // than dumping a raw HTTP error like the other authed commands do.
        Err(FetchMeError::Status(reqwest::StatusCode::UNAUTHORIZED, _)) => {
            return Err(eyre!(
                "Token rejected (revoked or expired). Run 'arena auth login' first."
            ));
        }
        Err(FetchMeError::Status(status, body)) => {
            return Err(eyre!("Failed to fetch current user: {status} - {body}"));
        }
        Err(FetchMeError::Request(report)) => return Err(report),
    };

    match output_format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&me)?),
        OutputFormat::Human => {
            for (label, value) in whoami_fields(&me) {
                print_field(label, &value);
            }
        }
    }
    Ok(())
}

async fn login() -> color_eyre::Result<()> {
    let config = CliConfig::load()?;
    let base_url = config.api_url();

    println!("Opening browser for GitHub authentication...");
    println!(
        "If the browser doesn't open, visit: {}/auth/github?cli=true",
        base_url
    );

    // Try to open browser
    let _ = open::that(format!("{}/auth/github?cli=true", base_url));

    // For now, prompt user to enter the token manually
    println!("\nAfter authenticating, you'll receive an API token.");
    println!("Enter your API token:");

    let mut token = String::new();
    std::io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        return Err(eyre!("No token provided"));
    }

    // Validate the token by fetching our own profile (GET /api/me)
    let me = match fetch_me(base_url, &token).await {
        Ok(me) => me,
        // The server answered: any non-2xx means the token is no good.
        Err(FetchMeError::Status(_, _)) => return Err(eyre!("Invalid token")),
        // Transport/parse failures are not the token's fault; preserve today's
        // wording ("Failed to validate token: <cause>") so an unreachable server
        // is never misreported as a bad token.
        Err(FetchMeError::Request(report)) => {
            return Err(report.wrap_err("Failed to validate token"));
        }
    };

    // Save the token
    let mut config = config;
    config.auth = Some(AuthConfig { token: Some(token) });
    config.save()?;

    let login = me["github_login"].as_str().unwrap_or("unknown");
    println!("Logged in as {login}");
    println!("Token saved.");
    Ok(())
}

fn logout() -> color_eyre::Result<()> {
    let mut config = CliConfig::load()?;
    config.auth = None;
    config.save()?;
    println!("Logged out successfully.");
    Ok(())
}

async fn handle_games_command(command: GamesCommands) -> color_eyre::Result<()> {
    let config = CliConfig::load()?;
    let token = config
        .auth
        .as_ref()
        .and_then(|a| a.token.as_ref())
        .ok_or_else(|| eyre!("Not logged in. Run 'arena auth login' first."))?;

    let client = reqwest::Client::new();
    let base_url = config.api_url();

    match command {
        GamesCommands::List { snake, limit } => {
            let mut url = format!("{}/api/games?limit={}", base_url, limit);
            if let Some(snake_id) = snake {
                url.push_str(&format!("&snake_id={}", snake_id));
            }

            let response = client
                .get(&url)
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to list games")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to list games: {} - {}", status, body));
            }

            let games: Vec<serde_json::Value> = response.json().await?;
            println!("{}", serde_json::to_string_pretty(&games)?);
        }
        GamesCommands::Create {
            snakes,
            board,
            game_type,
        } => {
            // Parse comma-separated snake IDs
            let snake_ids: Vec<&str> = snakes.split(',').map(|s| s.trim()).collect();

            let response = client
                .post(format!("{}/api/games", base_url))
                .bearer_auth(token)
                .json(&serde_json::json!({
                    "snakes": snake_ids,
                    "board": board,
                    "game_type": game_type
                }))
                .send()
                .await
                .wrap_err("Failed to create game")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to create game: {} - {}", status, body));
            }

            let game: serde_json::Value = response.json().await?;
            println!("{}", serde_json::to_string_pretty(&game)?);
        }
        GamesCommands::Show { id } => {
            let response = client
                .get(format!("{}/api/games/{}/details", base_url, id))
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Failed to get game")?;

            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(eyre!("Game not found."));
            } else if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to get game: {} - {}", status, body));
            }

            let game: serde_json::Value = response.json().await?;
            println!("{}", serde_json::to_string_pretty(&game)?);
        }
        GamesCommands::Watch { id, web } => {
            if web {
                // Open in browser
                let url = format!("{}/games/{}", base_url, id);
                println!("Opening game in browser...");
                open::that(&url).wrap_err("Failed to open browser")?;
            } else {
                // Poll loop
                loop {
                    let response = client
                        .get(format!("{}/api/games/{}/details", base_url, id))
                        .bearer_auth(token)
                        .send()
                        .await
                        .wrap_err("Failed to get game")?;

                    if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Err(eyre!("Game not found."));
                    } else if !response.status().is_success() {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        return Err(eyre!("Failed to get game: {} - {}", status, body));
                    }

                    let game: serde_json::Value = response.json().await?;

                    // Clear screen and print current state
                    print!("\x1B[2J\x1B[1;1H");
                    println!("{}", serde_json::to_string_pretty(&game)?);

                    // Check if game is finished
                    if game["status"] == "finished" {
                        println!("\nGame finished!");
                        break;
                    }

                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{FetchMeError, fetch_me, whoami_fields};

    fn me_body() -> serde_json::Value {
        serde_json::json!({
            "id": "4f0a3c2e-1111-4222-8333-444455556666",
            "github_login": "gh-me",
            "display_name": "Display Person",
            "avatar_url": null,
            "pronouns": null,
            "country": null,
            "profile_url": "http://localhost:3000/users/gh-me",
            "created_at": "2026-10-14T00:00:00Z",
        })
    }

    #[test]
    fn whoami_fields_lists_login_display_name_and_id() {
        let fields = whoami_fields(&me_body());
        assert_eq!(
            fields,
            vec![
                ("Login", "gh-me".to_string()),
                ("Display name", "Display Person".to_string()),
                ("ID", "4f0a3c2e-1111-4222-8333-444455556666".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn fetch_me_sends_bearer_token_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
            .mount(&server)
            .await;

        let me = fetch_me(&server.uri(), "test-token").await.unwrap();
        assert_eq!(me["github_login"], "gh-me");
        assert_eq!(me["display_name"], "Display Person");
        assert_eq!(me["id"], "4f0a3c2e-1111-4222-8333-444455556666");
    }

    #[tokio::test]
    async fn fetch_me_distinguishes_status_from_transport_failure() {
        // A 401 is a Status error — the server answered no.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        match fetch_me(&server.uri(), "bad-token").await {
            Err(FetchMeError::Status(StatusCode::UNAUTHORIZED, body)) => {
                assert_eq!(body, "unauthorized");
            }
            other => panic!("expected Status(UNAUTHORIZED), got {other:?}"),
        }

        // So is a 500.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        match fetch_me(&server.uri(), "any-token").await {
            Err(FetchMeError::Status(StatusCode::INTERNAL_SERVER_ERROR, _)) => {}
            other => panic!("expected Status(INTERNAL_SERVER_ERROR), got {other:?}"),
        }

        // An unreachable server is a Request error, not a bad token.
        match fetch_me("http://127.0.0.1:1", "any-token").await {
            Err(FetchMeError::Request(_)) => {}
            other => panic!("expected Request, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod token_create_tests {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use arena::api_scope::Permission;

    use super::{CreateTokenError, build_create_token_body, post_create_token};

    fn token_response() -> serde_json::Value {
        serde_json::json!({
            "id": "4f0a3c2e-1111-4222-8333-444455556666",
            "name": "CLI Token",
            "secret": "deadbeef",
            "permissions": ["profile.read"],
            "snake_access": "all",
            "snake_ids": [],
            "created_at": "2026-10-14T00:00:00Z",
        })
    }

    fn perms(
        name: &str,
        all_permissions: bool,
        permission: &[&str],
        snake: &[&str],
    ) -> serde_json::Value {
        let permission: Vec<String> = permission.iter().map(|s| s.to_string()).collect();
        let snake: Vec<String> = snake.iter().map(|s| s.to_string()).collect();
        build_create_token_body(name, &permission, all_permissions, &snake).unwrap()
    }

    #[test]
    fn neither_flag_is_a_client_side_error() {
        let err = build_create_token_body("x", &[], false, &[]).unwrap_err();
        assert!(err.to_string().contains("--all-permissions"), "{err}");
    }

    #[test]
    fn unknown_permission_is_left_to_the_server() {
        let body = perms("x", false, &["not.a.permission"], &[]);
        assert_eq!(body["permissions"], serde_json::json!(["not.a.permission"]));
    }

    #[test]
    fn malformed_snake_id_is_a_client_side_error() {
        let err = build_create_token_body(
            "x",
            &["profile.read".to_string()],
            false,
            &["nope".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid snake id"));
    }

    fn sent(body: &serde_json::Value) -> Vec<&str> {
        body["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect()
    }

    #[test]
    fn all_permissions_is_every_permission_but_admin() {
        let body = perms("x", true, &[], &[]);
        let expected: Vec<&str> = Permission::ALL
            .into_iter()
            .filter(|p| *p != Permission::Admin)
            .map(Permission::as_str)
            .collect();
        assert_eq!(sent(&body), expected);
        assert!(expected.contains(&"email.read"));
        assert_eq!(expected.len(), 12);
    }

    #[test]
    fn all_permissions_union_admin_sends_all_thirteen() {
        let body = perms("x", true, &["admin"], &[]);
        assert_eq!(sent(&body).len(), 13);
    }

    #[test]
    fn all_permissions_union_existing_still_deduplicates() {
        let body = perms("x", true, &["snakes.read"], &[]);
        assert_eq!(sent(&body).len(), 12, "{body}");
    }

    #[test]
    fn scope_403_suggests_logging_in_again() {
        let report = CreateTokenError::Status(
            reqwest::StatusCode::FORBIDDEN,
            "requested permissions or snake access exceed this token's".to_string(),
        )
        .into_report()
        .to_string();
        assert!(report.contains("exceed this token's"), "{report}");
        assert!(report.contains("arena auth login"), "{report}");

        // Other 403s are reported as-is.
        let report = CreateTokenError::Status(
            reqwest::StatusCode::FORBIDDEN,
            "missing permission: tokens.manage".to_string(),
        )
        .into_report()
        .to_string();
        assert!(!report.contains("arena auth login"), "{report}");
    }

    #[test]
    fn omitted_snake_sends_all_access_without_snake_ids() {
        let body = perms("x", true, &[], &[]);
        assert_eq!(body["snake_access"], "all");
        assert!(body.get("snake_ids").is_none());
    }

    #[test]
    fn snake_flags_send_selected_with_ids() {
        let body = perms(
            "x",
            false,
            &["profile.read"],
            &["4f0a3c2e-1111-4222-8333-444455556666"],
        );
        assert_eq!(body["snake_access"], "selected");
        assert_eq!(
            body["snake_ids"],
            serde_json::json!(["4f0a3c2e-1111-4222-8333-444455556666"])
        );
    }

    #[tokio::test]
    async fn token_create_sends_bearer_and_scope_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/tokens"))
            .and(header("Authorization", "Bearer test-token"))
            .and(body_partial_json(serde_json::json!({
                "permissions": ["profile.read"],
                "snake_access": "all",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(token_response()))
            .expect(1)
            .mount(&server)
            .await;

        let body = perms("CLI Token", false, &["profile.read"], &[]);
        let created = post_create_token(&server.uri(), "test-token", &body)
            .await
            .unwrap();
        assert_eq!(created["name"], "CLI Token");
    }

    #[tokio::test]
    async fn token_create_surfaces_status_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/tokens"))
            .respond_with(
                ResponseTemplate::new(403).set_body_string("cannot grant admin permission"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let body = perms("x", true, &["admin"], &[]);
        match post_create_token(&server.uri(), "t", &body).await {
            Err(super::CreateTokenError::Status(reqwest::StatusCode::FORBIDDEN, body)) => {
                assert_eq!(body, "cannot grant admin permission");
            }
            other => panic!("expected FORBIDDEN, got {other:?}"),
        }
    }
}
