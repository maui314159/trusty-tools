//! Drive file listing, search, content fetch, shared drives, file mgmt.
//!
//! Why: Drive is the most-used MCP surface — listing folders, searching by
//! name, downloading file content (especially Docs as text/HTML).
//! What: Each tool is a thin wrapper over the v3 REST endpoints.
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::DRIVE_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Folder listing is the entry point for any Drive navigation tool.
/// What: Queries `/files` filtered by parent id, returning name/mimeType/id for each child.
/// Test: Live API.
pub async fn list_drive_contents(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let folder_id = opt_str(&args, "folder_id").unwrap_or("root");
    let q = format!("'{folder_id}' in parents and trashed = false");
    let max = args
        .get("max_results")
        .and_then(|v| v.as_i64())
        .unwrap_or(100);
    let url = format!(
        "{DRIVE_API_BASE}/files?q={}&pageSize={max}&fields=files(id,name,mimeType,modifiedTime,size,parents)&supportsAllDrives=true&includeItemsFromAllDrives=true",
        encode(&q)
    );
    client.get(&url, account).await
}

/// Why: Full-text/metadata search across Drive needs the Drive `q` query DSL.
/// What: Forwards the user-supplied `query` to `/files?q=...` and returns matched files.
/// Test: Live API.
pub async fn search_drive_files(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let query = require_str(&args, "query")?;
    let max = args
        .get("max_results")
        .and_then(|v| v.as_i64())
        .unwrap_or(20);
    let url = format!(
        "{DRIVE_API_BASE}/files?q={}&pageSize={max}&fields=files(id,name,mimeType,modifiedTime,owners,parents)&supportsAllDrives=true&includeItemsFromAllDrives=true",
        encode(query)
    );
    client.get(&url, account).await
}

/// Why: Fetching file body (export for native Google types) needs MIME-aware handling.
/// What: For Google MIME types calls `/export`; for others calls `/files/{id}?alt=media`.
/// Test: Live API.
pub async fn get_drive_file_content(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "file_id")?;
    let export_mime = opt_str(&args, "export_mime_type");

    // Get metadata first to know mime type
    let meta_url = format!("{DRIVE_API_BASE}/files/{id}?fields=id,name,mimeType");
    let meta = client.get(&meta_url, account).await?;
    let mime = meta.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");

    let content_url = if mime.starts_with("application/vnd.google-apps") {
        // Google native doc — must use export
        let target = export_mime.unwrap_or("text/plain");
        format!(
            "{DRIVE_API_BASE}/files/{id}/export?mimeType={}",
            encode(target)
        )
    } else {
        format!("{DRIVE_API_BASE}/files/{id}?alt=media")
    };

    let token = client.get_access_token(account).await?;
    let raw = reqwest::Client::new()
        .get(&content_url)
        .bearer_auth(token)
        .send()
        .await?;
    let status = raw.status();
    let text = raw.text().await.unwrap_or_default();
    if !status.is_success() {
        return Ok(json!({ "error": text, "status": status.as_u16() }));
    }
    Ok(json!({
        "id": id,
        "name": meta.get("name"),
        "mimeType": mime,
        "content": text,
    }))
}

/// Why: Shared drives are a separate Drive endpoint from regular `/files`.
/// What: GETs `/drives` and returns name+id for each shared drive the user can access.
/// Test: Live API.
pub async fn list_shared_drives(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let url = format!("{DRIVE_API_BASE}/drives?pageSize=100");
    client.get(&url, account).await
}

/// Why: File-level mutations (create folder, rename, trash) share one Drive API surface.
/// What: Dispatches `create_folder|rename|trash|untrash|delete` to the Drive `/files` API.
/// Test: Live API.
pub async fn manage_drive_file(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    match action {
        "create_folder" => {
            let name = require_str(&args, "name")?;
            let parent = opt_str(&args, "parent_id");
            let mut body = json!({
                "name": name,
                "mimeType": "application/vnd.google-apps.folder",
            });
            if let Some(p) = parent {
                body["parents"] = json!([p]);
            }
            let url = format!("{DRIVE_API_BASE}/files?supportsAllDrives=true");
            client.post(&url, body, account).await
        }
        "rename" => {
            let id = require_str(&args, "file_id")?;
            let name = require_str(&args, "name")?;
            let body = json!({ "name": name });
            let url = format!("{DRIVE_API_BASE}/files/{id}?supportsAllDrives=true");
            client.patch(&url, body, account).await
        }
        "trash" => {
            let id = require_str(&args, "file_id")?;
            let body = json!({ "trashed": true });
            let url = format!("{DRIVE_API_BASE}/files/{id}?supportsAllDrives=true");
            client.patch(&url, body, account).await
        }
        "delete" => {
            let id = require_str(&args, "file_id")?;
            let url = format!("{DRIVE_API_BASE}/files/{id}?supportsAllDrives=true");
            client.delete(&url, account).await
        }
        "copy" => {
            let id = require_str(&args, "file_id")?;
            let body = json!({
                "name": opt_str(&args, "name").unwrap_or("Copy"),
            });
            let url = format!("{DRIVE_API_BASE}/files/{id}/copy?supportsAllDrives=true");
            client.post(&url, body, account).await
        }
        "move" => {
            let id = require_str(&args, "file_id")?;
            let parent = require_str(&args, "parent_id")?;
            let url =
                format!("{DRIVE_API_BASE}/files/{id}?addParents={parent}&supportsAllDrives=true");
            client.patch(&url, json!({}), account).await
        }
        other => Err(anyhow!("unknown action for manage_drive_file: {other}")),
    }
}

pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            for b in c.to_string().bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}
