//! Docs core: create, append/insert/delete text, fetch document or its structure.
//!
//! Why: Most agent workflows touch Docs through create-then-write, which
//! requires the `batchUpdate` insertText request shape.
//! What: Thin wrappers around `/documents` + `/documents/{id}:batchUpdate`.
//! Test: Live only.

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::DOCS_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Why: New-document creation is a one-shot Docs API call worth exposing as its own tool.
/// What: POSTs `{title}` to `/documents` and returns the created `documentId`.
/// Test: Live API.
pub async fn create_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let title = opt_str(&args, "title").unwrap_or("Untitled");
    let url = format!("{DOCS_API_BASE}/documents");
    client.post(&url, json!({ "title": title }), account).await
}

/// Why: Full document fetch is common; expose verbatim Docs payload.
/// What: GETs `/documents/{id}` and returns the raw JSON tree.
/// Test: Live API.
pub async fn get_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let url = format!("{DOCS_API_BASE}/documents/{id}");
    client.get(&url, account).await
}

/// Why: Full doc payloads are large; models often only need the skeleton to navigate.
/// What: Returns only structural elements — headings, paragraphs (without runs), tables.
/// Test: Live API.
pub async fn get_document_structure(client: &BaseClient, args: Value) -> Result<Value> {
    let full = get_document(client, args).await?;
    let mut out = Vec::<Value>::new();
    if let Some(body) = full.get("body").and_then(|b| b.get("content"))
        && let Some(arr) = body.as_array()
    {
        for el in arr {
            if let Some(p) = el.get("paragraph") {
                let style = p
                    .get("paragraphStyle")
                    .and_then(|s| s.get("namedStyleType"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("NORMAL_TEXT");
                let text: String = p
                    .get("elements")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|e| {
                                e.get("textRun")
                                    .and_then(|t| t.get("content"))
                                    .and_then(|c| c.as_str())
                            })
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                out.push(json!({ "type": "paragraph", "style": style, "text": text }));
            } else if el.get("table").is_some() {
                out.push(json!({ "type": "table" }));
            } else if el.get("sectionBreak").is_some() {
                out.push(json!({ "type": "section_break" }));
            }
        }
    }
    Ok(json!({
        "title": full.get("title"),
        "documentId": full.get("documentId"),
        "elements": out,
    }))
}

/// Why: Appending text to the end of a doc is the most common authoring operation.
/// What: Computes the end index then POSTs an `insertText` batchUpdate request.
/// Test: Live API.
pub async fn append_to_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let text = require_str(&args, "text")?;

    // Fetch document to find the end index
    let meta_url = format!("{DOCS_API_BASE}/documents/{id}");
    let doc = client.get(&meta_url, account).await?;
    let end_index = doc
        .get("body")
        .and_then(|b| b.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.last())
        .and_then(|el| el.get("endIndex"))
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        .saturating_sub(1)
        .max(1);

    let body = json!({
        "requests": [{
            "insertText": {
                "location": { "index": end_index },
                "text": text,
            }
        }]
    });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}

/// Why: Targeted text insertion at a known index is the building block for refactors.
/// What: POSTs a single `insertText` batchUpdate request with `{location.index, text}`.
/// Test: Live API.
pub async fn insert_text_in_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let text = require_str(&args, "text")?;
    let index = args
        .get("index")
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        .max(1);
    let body = json!({
        "requests": [{
            "insertText": {
                "location": { "index": index },
                "text": text,
            }
        }]
    });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}

/// Why: Range deletion is the inverse of insert and required for any rewrite flow.
/// What: POSTs a `deleteContentRange` batchUpdate request bounded by `start`/`end`.
/// Test: Live API.
pub async fn delete_range_in_document(client: &BaseClient, args: Value) -> Result<Value> {
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
    let body = json!({
        "requests": [{
            "deleteContentRange": {
                "range": { "startIndex": start, "endIndex": end },
            }
        }]
    });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}

/// Why: Search-and-replace is a high-leverage authoring primitive worth a dedicated tool.
/// What: POSTs `replaceAllText` with the find/replace strings; optionally case-sensitive.
/// Test: Live API.
pub async fn replace_text_in_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let find = require_str(&args, "find")?;
    let replace = require_str(&args, "replace")?;
    let body = json!({
        "requests": [{
            "replaceAllText": {
                "containsText": { "text": find, "matchCase": true },
                "replaceText": replace,
            }
        }]
    });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}
