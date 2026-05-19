//! Gmail label CRUD.
//!
//! Why: Labels are how Gmail organises messages; tools need create/delete/list.
//! What: Single tool dispatched on `action`.
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::GMAIL_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Labels back Gmail organisation; one tool covers the small CRUD surface.
/// What: Routes `list|create|update|delete` to `users/me/labels` on the Gmail API.
/// Test: Live API.
pub async fn manage_gmail_labels(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    match action {
        "list" => {
            let url = format!("{GMAIL_API_BASE}/users/me/labels");
            client.get(&url, account).await
        }
        "create" => {
            let name = require_str(&args, "name")?;
            let body = json!({
                "name": name,
                "labelListVisibility": opt_str(&args, "label_list_visibility").unwrap_or("labelShow"),
                "messageListVisibility": opt_str(&args, "message_list_visibility").unwrap_or("show"),
            });
            let url = format!("{GMAIL_API_BASE}/users/me/labels");
            client.post(&url, body, account).await
        }
        "update" => {
            let id = require_str(&args, "label_id")?;
            let body = args.get("updates").cloned().unwrap_or_else(|| json!({}));
            let url = format!("{GMAIL_API_BASE}/users/me/labels/{id}");
            client.patch(&url, body, account).await
        }
        "delete" => {
            let id = require_str(&args, "label_id")?;
            let url = format!("{GMAIL_API_BASE}/users/me/labels/{id}");
            client.delete(&url, account).await
        }
        other => Err(anyhow!("unknown action for manage_gmail_labels: {other}")),
    }
}
