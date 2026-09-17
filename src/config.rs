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

/// The controlled tag vocabulary: the facets a tag may belong to, plus any
/// bare tags allowed outside them.
///
/// A facet is either bare (its values are whole tags, like the states
/// `to-read`/`reading`/`read`) or prefixed (`method:rag`). A prefixed facet
/// with no `values` is open, so `concept:` can grow without a config edit
/// while `method:` cannot. `cardinality` is what makes "exactly one state"
/// and "at most one kind" checkable per item rather than per tag name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vocabulary {
    /// Bare tags allowed outside any facet, with no cardinality rule.
    #[serde(default)]
    pub allowed_tags: Vec<String>,
    /// The facets themselves.
    #[serde(default)]
    pub facets: Vec<Facet>,
}

/// How many tags of one facet a single item may carry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cardinality {
    /// Zero or more, the default: `domain:`, `method:`, `concept:`, flags.
    #[default]
    Any,
    /// Zero or one: `kind:`, `course:`.
    AtMostOne,
    /// Exactly one: state.
    ExactlyOne,
}

impl Cardinality {
    /// Whether a second tag of this facet on one item is a defect.
    pub fn allows_many(self) -> bool {
        self == Cardinality::Any
    }

    fn is_any(&self) -> bool {
        *self == Cardinality::Any
    }
}

/// One facet of the vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facet {
    /// Name used when reporting, e.g. `state`. Defaults to the prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The prefix its tags carry, with or without the colon (`kind` and
    /// `kind:` are the same facet). Absent for a bare facet, whose `values`
    /// are whole tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// The values this facet allows. For a prefixed facet, empty means open:
    /// any non-empty value is accepted, which is what `concept:` needs. A bare
    /// facet with no values matches nothing.
    #[serde(default)]
    pub values: Vec<String>,
    /// How many of this facet's tags one item may carry.
    #[serde(default, skip_serializing_if = "Cardinality::is_any")]
    pub cardinality: Cardinality,
}

impl Facet {
    /// The prefix as it appears at the front of a tag, colon included, or
    /// `None` for a bare facet.
    pub fn prefix_with_colon(&self) -> Option<String> {
        let prefix = self.prefix.as_ref()?.trim().trim_end_matches(':');
        if prefix.is_empty() {
            return None;
        }
        Some(format!("{prefix}:"))
    }

    /// What this facet is called in output.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        self.prefix_with_colon()
            .unwrap_or_else(|| "unnamed facet".to_string())
    }

    /// Whether a prefixed facet accepts any value (no closed list configured).
    /// Always false for a bare facet, which is its value list.
    pub fn is_open(&self) -> bool {
        self.prefix_with_colon().is_some() && self.values.is_empty()
    }
}

impl Default for Vocabulary {
    /// The vocabulary of the filing convention, shipped so `--check` is useful
    /// on a machine that has never run `zot config`.
    ///
    /// state and flags are bare; everything else is namespaced. `concept:` and
    /// `course:` are open because their values are coined as the library
    /// grows.
    fn default() -> Self {
        let strings = |values: &[&str]| values.iter().map(|v| (*v).to_string()).collect();
        let bare = |name: &str, values: &[&str], cardinality: Cardinality| Facet {
            name: Some(name.to_string()),
            prefix: None,
            values: strings(values),
            cardinality,
        };
        let prefixed = |name: &str, values: &[&str], cardinality: Cardinality| Facet {
            name: Some(name.to_string()),
            prefix: Some(format!("{name}:")),
            values: strings(values),
            cardinality,
        };
        Vocabulary {
            allowed_tags: Vec::new(),
            facets: vec![
                bare(
                    "state",
                    &["to-read", "reading", "read"],
                    Cardinality::ExactlyOne,
                ),
                bare("flag", &["urgent", "graded"], Cardinality::Any),
                prefixed(
                    "kind",
                    &["survey", "benchmark", "dataset", "position", "thesis"],
                    Cardinality::AtMostOne,
                ),
                prefixed(
                    "domain",
                    &["aec", "bim", "construction-mgmt", "facility-mgmt"],
                    Cardinality::Any,
                ),
                prefixed(
                    "method",
                    &["llm", "vlm", "agent", "rag", "kg", "cv"],
                    Cardinality::Any,
                ),
                prefixed("concept", &[], Cardinality::Any),
                prefixed("course", &[], Cardinality::AtMostOne),
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
