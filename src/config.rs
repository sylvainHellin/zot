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
    /// Controlled tag vocabulary `zot tags --check` judges the library against.
    /// Absent means the built-in default, so `--check` works before anything
    /// is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocabulary: Option<Vocabulary>,
}

/// The controlled tag vocabulary: a list of allowed bare tags plus the facet
/// prefixes everything else has to carry.
///
/// Two shapes, because tags split in two: a handful of bare words that are
/// states or flags (`to-read`, `urgent`), and namespaced values under a
/// `prefix:` (`method:rag`). A facet with no `values` is open, so
/// `concept:` can grow without a config edit while `method:` cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vocabulary {
    /// Tags allowed with no facet prefix.
    #[serde(default)]
    pub allowed_tags: Vec<String>,
    /// Allowed facet prefixes, each open or closed.
    #[serde(default)]
    pub facets: Vec<Facet>,
}

/// One facet of the vocabulary: a prefix and, optionally, its closed value set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facet {
    /// The prefix, written with or without its colon (`kind` and `kind:` are
    /// the same facet).
    pub prefix: String,
    /// The values this facet allows. Empty means open: any value is accepted,
    /// which is what `concept:` and `course:` need.
    #[serde(default)]
    pub values: Vec<String>,
}

impl Facet {
    /// The prefix as it appears at the front of a tag, colon included.
    pub fn prefix_with_colon(&self) -> String {
        format!("{}:", self.prefix.trim().trim_end_matches(':'))
    }

    /// Whether this facet accepts any value (no closed list configured).
    pub fn is_open(&self) -> bool {
        self.values.is_empty()
    }
}

impl Default for Vocabulary {
    /// The vocabulary of the filing convention, shipped so `--check` is useful
    /// on a machine that has never run `zot config`.
    ///
    /// state (`to-read`/`reading`/`read`) and flags (`urgent`, `graded`) are
    /// bare; everything else is namespaced. `concept:` and `course:` are open
    /// because their values are coined as the library grows.
    fn default() -> Self {
        let facet = |prefix: &str, values: &[&str]| Facet {
            prefix: format!("{prefix}:"),
            values: values.iter().map(|v| (*v).to_string()).collect(),
        };
        Vocabulary {
            allowed_tags: ["to-read", "reading", "read", "urgent", "graded"]
                .iter()
                .map(|t| (*t).to_string())
                .collect(),
            facets: vec![
                facet("kind", &["survey", "benchmark", "dataset", "position", "thesis"]),
                facet("domain", &["aec", "bim", "construction-mgmt", "facility-mgmt"]),
                facet("method", &["llm", "vlm", "agent", "rag", "kg", "cv"]),
                facet("concept", &[]),
                facet("course", &[]),
            ],
        }
    }
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

    /// The configured vocabulary, or the built-in default when none is set.
    pub fn vocabulary(&self) -> Vocabulary {
        self.vocabulary.clone().unwrap_or_default()
    }

    /// Whether the vocabulary came from the config file rather than the
    /// built-in default, which `--check` reports so a surprising verdict can be
    /// traced to its source.
    pub fn vocabulary_is_configured(&self) -> bool {
        self.vocabulary.is_some()
    }

    /// Resolve the API key: env var wins, then stored config.
    pub fn resolve_api_key(&self) -> Option<String> {
        std::env::var("ZOTERO_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| self.api_key.clone())
    }
}
