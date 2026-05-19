//! Drive file permission / sharing management.
//!
//! Why: Sharing changes are sensitive operations; one tool routes the four
//! actions (list/create/update/delete) explicitly.
//! What: Wraps `/files/{fileId}/permissions` endpoints.
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::DRIVE_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Sharing is a security-sensitive op kept on its own tool for visibility.
/// What: Routes `list|create|update|delete` to the Drive v3 `/permissions` sub-resource.
/// Test: Live API.
pub async fn manage_file_permissions(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    let file_id = require_str(&args, "file_id")?;
    match action {
        "list" => {
            let url = format!(
                "{DRIVE_API_BASE}/files/{file_id}/permissions?supportsAllDrives=true&fields=permissions(id,emailAddress,role,type,displayName)"
            );
            client.get(&url, account).await
        }
        "create" => {
            let role = opt_str(&args, "role").unwrap_or("reader");
            let type_ = opt_str(&args, "type").unwrap_or("user");
            let mut body = json!({ "role": role, "type": type_ });
            if let Some(e) = opt_str(&args, "email_address") {
                body["emailAddress"] = json!(e);
            }
            if let Some(d) = opt_str(&args, "domain") {
                body["domain"] = json!(d);
            }
            let url = format!(
                "{DRIVE_API_BASE}/files/{file_id}/permissions?supportsAllDrives=true&sendNotificationEmail={}",
                opt_str(&args, "send_notification_email").unwrap_or("false")
            );
            client.post(&url, body, account).await
        }
        "update" => {
            let perm_id = require_str(&args, "permission_id")?;
            let role = require_str(&args, "role")?;
            let body = json!({ "role": role });
            let url = format!(
                "{DRIVE_API_BASE}/files/{file_id}/permissions/{perm_id}?supportsAllDrives=true"
            );
            client.patch(&url, body, account).await
        }
        "delete" => {
            let perm_id = require_str(&args, "permission_id")?;
            let url = format!(
                "{DRIVE_API_BASE}/files/{file_id}/permissions/{perm_id}?supportsAllDrives=true"
            );
            client.delete(&url, account).await
        }
        other => Err(anyhow!(
            "unknown action for manage_file_permissions: {other}"
        )),
    }
}
