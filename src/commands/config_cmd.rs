//! `zot config` -- one-time setup of the Zotero web API key used by write
//! commands (`edit`, `attach`, `rm`). Stored in the platform config dir;
//! re-run `set-key` any time to rotate the key.

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::output::{ConfigOutput, format_output};

pub fn run_set_key(key: &str, json: bool) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API key is empty");
    }

    let mut cfg = Config::load()?;
    cfg.api_key = Some(key.to_string());
    cfg.user_id = None; // Re-derive for the new key.
    cfg.save()?;

    // Validate immediately: resolves and caches the user ID via /keys/current.
    let _ = crate::api::WebApiClient::from_config()
        .context("Key stored, but validation against api.zotero.org failed")?;
    let cfg = Config::load()?;

    print_config(&cfg, Some("API key validated and stored."), json)
}

pub fn run_show(json: bool) -> Result<()> {
    let cfg = Config::load()?;
    print_config(&cfg, None, json)
}

#[allow(
    clippy::string_slice,
    reason = "Zotero API keys are ASCII alphanumeric, so byte and char indices coincide"
)]
fn print_config(cfg: &Config, note: Option<&str>, json: bool) -> Result<()> {
    let masked = cfg.api_key.as_deref().map(|k| {
        if k.len() > 6 {
            format!("{}...{}", &k[..3], &k[k.len() - 3..])
        } else {
            "***".to_string()
        }
    });
    let env_override = std::env::var("ZOTERO_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .is_some();

    let output = ConfigOutput {
        path: Config::config_path()?.display().to_string(),
        api_key: masked,
        user_id: cfg.user_id,
        env_override,
        note: note.map(String::from),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}
