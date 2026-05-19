//! Docs range / document style formatting.
//!
//! Why: Most agent edits want at minimum bold, italic, font size, heading
//! style — exposing two narrowly-scoped tools keeps the JSON schemas small.
//! What: `format_document_range` mutates text styles within a range;
//! `set_document_style` sets document-level defaults (e.g., page size).
//! Test: Live only.

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::DOCS_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: Inline text styling (bold/italic/etc) needs a single ergonomic entry point.
/// What: POSTs `updateTextStyle` batchUpdate over `[start, end)` with the requested fields.
/// Test: Live API.
pub async fn format_document_range(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let start = args
        .get("start_index")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing start_index"))?;
    let end = args
        .get("end_index")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow::anyhow!("missing end_index"))?;

    let mut text_style = json!({});
    let mut fields = Vec::<&str>::new();
    if let Some(b) = args.get("bold").and_then(|v| v.as_bool()) {
        text_style["bold"] = json!(b);
        fields.push("bold");
    }
    if let Some(i) = args.get("italic").and_then(|v| v.as_bool()) {
        text_style["italic"] = json!(i);
        fields.push("italic");
    }
    if let Some(u) = args.get("underline").and_then(|v| v.as_bool()) {
        text_style["underline"] = json!(u);
        fields.push("underline");
    }
    if let Some(size) = args.get("font_size").and_then(|v| v.as_f64()) {
        text_style["fontSize"] = json!({ "magnitude": size, "unit": "PT" });
        fields.push("fontSize");
    }

    let mut requests = Vec::<Value>::new();
    if !fields.is_empty() {
        requests.push(json!({
            "updateTextStyle": {
                "range": { "startIndex": start, "endIndex": end },
                "textStyle": text_style,
                "fields": fields.join(","),
            }
        }));
    }
    if let Some(style) = opt_str(&args, "named_style") {
        requests.push(json!({
            "updateParagraphStyle": {
                "range": { "startIndex": start, "endIndex": end },
                "paragraphStyle": { "namedStyleType": style },
                "fields": "namedStyleType",
            }
        }));
    }
    let body = json!({ "requests": requests });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}

/// Why: Page-level style (margins, page size) is a different Docs API verb than text style.
/// What: POSTs `updateDocumentStyle` with the requested style object and field mask.
/// Test: Live API.
pub async fn set_document_style(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let style = args.get("style").cloned().unwrap_or_else(|| json!({}));
    let fields = opt_str(&args, "fields").unwrap_or("*");
    let body = json!({
        "requests": [{
            "updateDocumentStyle": {
                "documentStyle": style,
                "fields": fields,
            }
        }]
    });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}
