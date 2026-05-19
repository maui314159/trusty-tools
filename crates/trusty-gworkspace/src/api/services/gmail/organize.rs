//! Gmail filter CRUD.
//!
//! Why: Filters automate label/forward/archive based on incoming messages.
//! What: list/create/delete for `/users/me/settings/filters`.
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::api::client::BaseClient;
use crate::api::constants::GMAIL_API_BASE;
use crate::api::services::{account_of, require_str};

/// Why: Filters are how power users automate inbox routing.
/// What: Routes `list|create|delete` to `users/me/settings/filters`.
/// Test: Live API.
pub async fn manage_gmail_filters(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    match action {
        "list" => {
            let url = format!("{GMAIL_API_BASE}/users/me/settings/filters");
            client.get(&url, account).await
        }
        "create" => {
            let body = args
                .get("filter")
                .cloned()
                .ok_or_else(|| anyhow!("missing 'filter' object"))?;
            let url = format!("{GMAIL_API_BASE}/users/me/settings/filters");
            client.post(&url, body, account).await
        }
        "delete" => {
            let id = require_str(&args, "filter_id")?;
            let url = format!("{GMAIL_API_BASE}/users/me/settings/filters/{id}");
            client.delete(&url, account).await
        }
        other => Err(anyhow!("unknown action for manage_gmail_filters: {other}")),
    }
}
