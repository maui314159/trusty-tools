//! Gmail message search, retrieval, attachments, compose, modify.
//!
//! Why: The bread-and-butter Gmail surface for an MCP server: search inbox,
//! read messages, download attachments, compose drafts/sends, label.
//! What: Helpers for RFC 2822 MIME composition + base64url encoding so we
//! can POST to `/users/me/messages/send` with the wire-format Google
//! expects.
//! Test: Live only.

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::GMAIL_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Search is the canonical entry to Gmail for any agent flow.
/// What: Forwards the Gmail-DSL `query` string to `users/me/messages` and returns hits.
/// Test: Live API.
pub async fn search_gmail_messages(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let query = opt_str(&args, "query").unwrap_or("");
    let max = args
        .get("max_results")
        .and_then(|v| v.as_i64())
        .unwrap_or(10);
    let url = format!(
        "{GMAIL_API_BASE}/users/me/messages?q={}&maxResults={max}",
        urlencode(query)
    );
    client.get(&url, account).await
}

/// Why: After search, fetching the parsed body is the next step.
/// What: GETs `messages/{id}?format=full`, decodes parts, returns headers + plain/HTML body.
/// Test: Live API.
pub async fn get_gmail_message_content(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "message_id")?;
    let url = format!("{GMAIL_API_BASE}/users/me/messages/{id}?format=full");
    client.get(&url, account).await
}

/// Why: Attachments live behind a separate Gmail endpoint and need base64 handling.
/// What: GETs `messages/{id}/attachments/{aid}`, returns decoded bytes (b64 in JSON).
/// Test: Live API.
pub async fn download_gmail_attachment(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let msg = require_str(&args, "message_id")?;
    let att = require_str(&args, "attachment_id")?;
    let url = format!("{GMAIL_API_BASE}/users/me/messages/{msg}/attachments/{att}");
    let resp = client.get(&url, account).await?;

    let return_content = args
        .get("return_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let save_path = opt_str(&args, "save_path");

    if let Some(path) = save_path
        && let Some(b64) = resp.get("data").and_then(|v| v.as_str())
    {
        let bytes = URL_SAFE_NO_PAD
            .decode(b64.trim_end_matches('='))
            .map_err(|e| anyhow!("base64 decode attachment: {e}"))?;
        std::fs::write(path, bytes)?;
        return Ok(json!({ "saved": path, "size": resp.get("size") }));
    }
    if return_content {
        return Ok(resp);
    }
    Ok(json!({
        "size": resp.get("size"),
        "attachmentId": att,
    }))
}

/// Why: Discovery: enumerate filename/MIME for every attachment before downloading.
/// What: Walks the message payload tree and emits `{filename, mime_type, attachment_id}`.
/// Test: Live API.
pub async fn list_message_attachments(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "message_id")?;
    let url = format!("{GMAIL_API_BASE}/users/me/messages/{id}?format=full");
    let msg = client.get(&url, account).await?;

    let mut atts = Vec::<Value>::new();
    if let Some(payload) = msg.get("payload") {
        collect_attachments(payload, &mut atts);
    }
    Ok(json!({ "attachments": atts }))
}

fn collect_attachments(part: &Value, out: &mut Vec<Value>) {
    if let Some(body) = part.get("body")
        && let Some(att_id) = body.get("attachmentId").and_then(|v| v.as_str())
    {
        out.push(json!({
            "attachmentId": att_id,
            "filename": part.get("filename"),
            "mimeType": part.get("mimeType"),
            "size": body.get("size"),
        }));
    }
    if let Some(parts) = part.get("parts").and_then(|v| v.as_array()) {
        for p in parts {
            collect_attachments(p, out);
        }
    }
}

/// Compose an email: send, draft, or send an existing draft.
///
/// Why: Single tool covers the three common write paths in Gmail; we build
/// the RFC 2822 MIME envelope here so callers pass logical fields.
/// What: Builds a `From/To/Subject/Body` message, base64url-encodes it, and
/// POSTs to either `/messages/send` or `/drafts`.
/// Test: Live only (real Gmail send).
pub async fn compose_email(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let action = opt_str(&args, "action").unwrap_or("send");

    if action == "send_draft" {
        let draft_id = require_str(&args, "draft_id")?;
        let body = json!({ "id": draft_id });
        let url = format!("{GMAIL_API_BASE}/users/me/drafts/send");
        return client.post(&url, body, account).await;
    }

    let to = require_str(&args, "to")?;
    let subject = opt_str(&args, "subject").unwrap_or("");
    let body_text = opt_str(&args, "body").unwrap_or("");
    let cc = opt_str(&args, "cc");
    let bcc = opt_str(&args, "bcc");
    let html = args.get("html").and_then(|v| v.as_bool()).unwrap_or(false);

    let mime = build_mime_message(to, cc, bcc, subject, body_text, html);
    let encoded = URL_SAFE_NO_PAD.encode(mime.as_bytes());

    let payload = json!({ "raw": encoded });
    match action {
        "send" => {
            let url = format!("{GMAIL_API_BASE}/users/me/messages/send");
            client.post(&url, payload, account).await
        }
        "draft" => {
            let body = json!({ "message": { "raw": encoded } });
            let url = format!("{GMAIL_API_BASE}/users/me/drafts");
            client.post(&url, body, account).await
        }
        other => Err(anyhow!("unknown action for compose_email: {other}")),
    }
}

fn build_mime_message(
    to: &str,
    cc: Option<&str>,
    bcc: Option<&str>,
    subject: &str,
    body: &str,
    html: bool,
) -> String {
    let content_type = if html {
        "text/html; charset=UTF-8"
    } else {
        "text/plain; charset=UTF-8"
    };
    let mut headers = vec![
        format!("To: {to}"),
        format!("Subject: {subject}"),
        format!("Content-Type: {content_type}"),
        "MIME-Version: 1.0".to_string(),
    ];
    if let Some(c) = cc {
        headers.push(format!("Cc: {c}"));
    }
    if let Some(b) = bcc {
        headers.push(format!("Bcc: {b}"));
    }
    format!("{}\r\n\r\n{}", headers.join("\r\n"), body)
}

/// Why: Bulk label add/remove (incl. archive/trash) is one Gmail batchModify call.
/// What: POSTs `add_label_ids` and `remove_label_ids` against `messages/batchModify`.
/// Test: Live API.
pub async fn modify_gmail_messages(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let ids: Vec<String> = args
        .get("message_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Err(anyhow!("message_ids must be a non-empty array"));
    }
    let add: Vec<String> = args
        .get("add_label_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let remove: Vec<String> = args
        .get("remove_label_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let body = json!({
        "ids": ids,
        "addLabelIds": add,
        "removeLabelIds": remove,
    });
    let url = format!("{GMAIL_API_BASE}/users/me/messages/batchModify");
    client.post(&url, body, account).await
}

fn urlencode(s: &str) -> String {
    // Minimal URL encoder: replace spaces and non-alphanumerics.
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
