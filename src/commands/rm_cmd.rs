//! `zot rm` -- move items to the Zotero trash via the web API (restorable in
//! the Zotero UI; never a permanent delete).

use anyhow::{Context, Result};
use serde_json::json;

use crate::api::WebApiClient;
use crate::output::{RmOutput, format_output};

pub fn run_rm(keys: &[String], json: bool) -> Result<()> {
    let web = WebApiClient::from_config()?;

    let mut trashed = Vec::new();
    for key in keys {
        // Trashing does not merge anything into the item's state, but the PATCH
        // still needs a version to guard it; read it here rather than inside the
        // client, so the precondition is always the caller's snapshot.
        let version = web
            .get_item(key)?
            .get("version")
            .and_then(|v| v.as_u64())
            .context("No version on item")?;
        web.patch_item(key, &json!({ "deleted": true }), version)?;
        trashed.push(key.clone());
    }

    let output = RmOutput {
        trashed,
        note: "Moved to trash via web API (restorable in the Zotero UI); \
               syncs to the local library on the next Zotero sync."
            .to_string(),
    };
    println!("{}", format_output(&output, json));
    Ok(())
}
