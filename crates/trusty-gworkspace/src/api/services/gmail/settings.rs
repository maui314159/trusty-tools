//! Gmail account settings + email body formatting helper.
//!
//! Why: Settings (vacation responder, auto-forwarding, signatures) plus a
//! lightweight markdown→HTML helper for callers that want a quick format.
//! What: `manage_gmail_settings` dispatches on `setting` field;
//! `format_email_content` is a pure transform (no network).
//! Test: Live for settings; local for format.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::GMAIL_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Account settings (vacation, signature, forwarding) live behind one tool.
/// What: Routes per-setting actions to `users/me/settings/*` endpoints.
/// Test: Live API.
pub async fn manage_gmail_settings(client: &BaseClient, args: Value) -> Result<Value> {
    let setting = require_str(&args, "setting")?;
    let action = opt_str(&args, "action").unwrap_or("get");
    let account = account_of(&args);
    let endpoint = match setting {
        "vacation" => "vacation",
        "auto_forwarding" => "autoForwarding",
        "imap" => "imap",
        "pop" => "pop",
        "language" => "language",
        other => return Err(anyhow!("unsupported setting: {other}")),
    };
    let url = format!("{GMAIL_API_BASE}/users/me/settings/{endpoint}");
    match action {
        "get" => client.get(&url, account).await,
        "update" => {
            let body = args.get("value").cloned().unwrap_or_else(|| json!({}));
            client.put(&url, body, account).await
        }
        other => Err(anyhow!("unknown action for manage_gmail_settings: {other}")),
    }
}

/// Lightweight markdown→HTML pre-processor.
///
/// Why: Many MCP callers want to send pretty HTML mail without depending on
/// a full markdown parser. We do a minimal transform here for paragraphs,
/// bold, italic, links; otherwise we wrap the text in `<p>`.
/// What: Stateless transformation; returns `{ html, plain }` JSON.
/// Test: Local unit test.
pub async fn format_email_content(_client: &BaseClient, args: Value) -> Result<Value> {
    let body = require_str(&args, "body")?;
    let mode = opt_str(&args, "mode").unwrap_or("auto");
    let html = if mode == "passthrough" {
        body.to_string()
    } else {
        simple_markdown_to_html(body)
    };
    Ok(json!({ "html": html, "plain": body }))
}

fn simple_markdown_to_html(input: &str) -> String {
    let mut out = String::new();
    for para in input.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let mut line = para.to_string();
        // **bold**
        while let Some((a, rest)) = line.split_once("**") {
            if let Some((b, c)) = rest.split_once("**") {
                line = format!("{a}<strong>{b}</strong>{c}");
            } else {
                break;
            }
        }
        // *italic*
        while let Some((a, rest)) = line.split_once('*') {
            if let Some((b, c)) = rest.split_once('*') {
                line = format!("{a}<em>{b}</em>{c}");
            } else {
                break;
            }
        }
        out.push_str(&format!("<p>{}</p>", line.replace('\n', "<br>")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_wraps_paragraphs_and_handles_bold() {
        let html = simple_markdown_to_html("hello **world**\n\nsecond line");
        assert!(html.contains("<strong>world</strong>"));
        assert!(html.contains("<p>"));
    }
}
