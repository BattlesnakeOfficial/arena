use color_eyre::eyre::Context as _;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CONFIG_DIR: &str = "arena";
const CONFIG_FILE: &str = "config.toml";
const DEFAULT_API_URL: &str = "https://arena.battlesnake.com";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub api_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub token: Option<String>,
}

impl CliConfig {
    /// Get the config directory path (~/.config/arena on Linux/macOS)
    pub fn config_dir() -> color_eyre::Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| color_eyre::eyre::eyre!("Could not find config directory"))?
            .join(CONFIG_DIR);
        Ok(config_dir)
    }

    /// Get the config file path
    pub fn config_path() -> color_eyre::Result<PathBuf> {
        Ok(Self::config_dir()?.join(CONFIG_FILE))
    }

    /// Load config from file, creating defaults if it doesn't exist
    pub fn load() -> color_eyre::Result<Self> {
        let path = Self::config_path()?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Self = toml::from_str(&contents)
            .wrap_err_with(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    /// Save config to file
    pub fn save(&self) -> color_eyre::Result<()> {
        let dir = Self::config_dir()?;
        let path = Self::config_path()?;

        // Ensure config directory exists
        std::fs::create_dir_all(&dir)
            .wrap_err_with(|| format!("Failed to create config directory: {}", dir.display()))?;

        let contents = toml::to_string_pretty(self).wrap_err("Failed to serialize config")?;

        std::fs::write(&path, contents)
            .wrap_err_with(|| format!("Failed to write config file: {}", path.display()))?;

        Ok(())
    }

    /// Get the API base URL
    pub fn api_url(&self) -> &str {
        self.api_url.as_deref().unwrap_or(DEFAULT_API_URL)
    }
}
