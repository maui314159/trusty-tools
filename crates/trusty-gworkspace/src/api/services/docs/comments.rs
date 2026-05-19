//! Docs comment operations (via Drive API).
//!
//! Why: Doc comments live under Drive's `/files/{fileId}/comments` despite
//! conceptually belonging to Docs. We expose them here so callers see one
//! coherent "docs" surface.
//! What: list/create/reply/resolve/delete on Drive comments.
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::DRIVE_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Comments are a separate Drive API surface; one tool keeps the action enum local.
/// What: Routes `list|create|reply|resolve|delete` against the Drive v3 `comments` resource.
/// Test: Live API.
pub async fn manage_document_comments(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    let file_id = require_str(&args, "document_id")?;
    match action {
        "list" => {
            let url = format!("{DRIVE_API_BASE}/files/{file_id}/comments?fields=*");
            client.get(&url, account).await
        }
        "create" => {
            let content = require_str(&args, "content")?;
            let body = json!({ "content": content });
            let url = format!("{DRIVE_API_BASE}/files/{file_id}/comments?fields=*");
            client.post(&url, body, account).await
        }
        "reply" => {
            let comment_id = require_str(&args, "comment_id")?;
            let content = require_str(&args, "content")?;
            let body = json!({ "content": content });
            let url =
                format!("{DRIVE_API_BASE}/files/{file_id}/comments/{comment_id}/replies?fields=*");
            client.post(&url, body, account).await
        }
        "resolve" => {
            let comment_id = require_str(&args, "comment_id")?;
            let body = json!({ "resolved": true });
            let url = format!("{DRIVE_API_BASE}/files/{file_id}/comments/{comment_id}?fields=*");
            client.patch(&url, body, account).await
        }
        "delete" => {
            let comment_id = require_str(&args, "comment_id")?;
            let url = format!("{DRIVE_API_BASE}/files/{file_id}/comments/{comment_id}");
            client.delete(&url, account).await
        }
        other => Err(anyhow!(
            "unknown action for manage_document_comments: {other}"
        )),
    }
}

// Suppress dead-code warning by re-using opt_str if/when needed.
#[allow(dead_code)]
fn _force_opt_str() {
    let _: Option<&str> = opt_str(&json!({}), "x");
}
