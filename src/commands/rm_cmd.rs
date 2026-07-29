//! `zot rm` -- move items to the Zotero trash via the web API (restorable in
//! the Zotero UI; never a permanent delete).

use anyhow::Result;
use serde_json::json;

use crate::api::WebApiClient;
use crate::output::{RmOutput, format_output};

pub fn run_rm(keys: &[String], json: bool) -> Result<()> {
    let web = WebApiClient::from_config()?;

    let mut trashed = Vec::new();
    for key in keys {
        web.patch_item(key, &json!({ "deleted": true }))?;
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
