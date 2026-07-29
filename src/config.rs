//! Persistent CLI configuration (Zotero web API credentials).
//!
//! Stored once via `zot config set-key` at the platform config dir
//! (Linux: `~/.config/zot/config.json`) so write commands never need the key
//! passed per invocation. The `ZOTERO_API_KEY` env var overrides the stored
//! key when set (useful for one-off runs and CI).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Zotero web API key (write-enabled) for api.zotero.org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Numeric Zotero user ID, cached from `/keys/current` on first use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<u64>,
}

impl Config {
    pub fn config_path() -> Result<PathBuf> {
        let proj_dirs = directories::ProjectDirs::from("", "", "zot")
            .context("Could not determine config directory")?;
        Ok(proj_dirs.config_dir().join("config.json"))
    }

    pub fn load() -> Result<Config> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config at {}", path.display()))?;
        let cfg: Config = serde_json::from_str(&raw)
            .with_context(|| format!("Invalid config file at {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, raw)
            .with_context(|| format!("Failed to write config at {}", path.display()))?;
        Ok(())
    }

    /// Resolve the API key: env var wins, then stored config.
    pub fn resolve_api_key(&self) -> Option<String> {
        std::env::var("ZOTERO_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| self.api_key.clone())
    }
}
