use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context as _, eyre};
use std::time::Duration;

// Include the cli module from the library
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
    },
    /// List all active API tokens
    List,
    /// Revoke an API token
    Revoke {
        /// Token ID to revoke
        id: String,
    },
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
        TokenCommands::Create { name } => {
            let name = name.unwrap_or_else(|| {
                hostname::get()
                    .ok()
                    .and_then(|h| h.into_string().ok())
                    .unwrap_or_else(|| "CLI Token".to_string())
            });

            let response = client
                .post(format!("{}/api/tokens", base_url))
                .bearer_auth(token)
                .json(&serde_json::json!({ "name": name }))
                .send()
                .await
                .wrap_err("Failed to create token")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(eyre!("Failed to create token: {} - {}", status, body));
            }

            let result: serde_json::Value = response.json().await?;
            println!("Token created successfully!");
            println!("ID: {}", result["id"]);
            println!("Name: {}", result["name"]);
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
                println!("{:<38} {:<20} {:<20}", "ID", "NAME", "LAST USED");
                println!("{}", "-".repeat(78));
                for token in tokens {
                    let last_used = token["last_used_at"].as_str().unwrap_or("Never");
                    println!(
                        "{:<38} {:<20} {:<20}",
                        token["id"].as_str().unwrap_or(""),
                        token["name"].as_str().unwrap_or(""),
                        last_used
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

    // Validate the token by trying to list tokens
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/api/tokens", base_url))
        .bearer_auth(&token)
        .send()
        .await
        .wrap_err("Failed to validate token")?;

    if !response.status().is_success() {
        return Err(eyre!("Invalid token"));
    }

    // Save the token
    let mut config = config;
    config.auth = Some(AuthConfig { token: Some(token) });
    config.save()?;

    println!("Login successful! Token saved.");
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
