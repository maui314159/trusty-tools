//! Docs table operations: insert, find, and structural edits.
//!
//! Why: Tables are common in agent-authored docs; this module wraps the
//! insertTable/insertTableRow/insertTableColumn batchUpdate requests.
//! What: 3 tools — insert, find, manage (insert/delete row/column).
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::DOCS_API_BASE;
use crate::api::services::{account_of, require_str};

/// Why: Tables are not raw text; creating them requires a typed Docs API request.
/// What: POSTs `insertTable` batchUpdate with `{rows, columns, location}` and returns the doc.
/// Test: Live API.
pub async fn insert_table_in_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let rows = args.get("rows").and_then(|v| v.as_i64()).unwrap_or(2);
    let columns = args.get("columns").and_then(|v| v.as_i64()).unwrap_or(2);
    let index = args
        .get("index")
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        .max(1);
    let body = json!({
        "requests": [{
            "insertTable": {
                "rows": rows,
                "columns": columns,
                "location": { "index": index },
            }
        }]
    });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}

/// Why: Discovering existing tables is needed before any structural edit.
/// What: Fetches the doc and walks its body, returning index + dimensions for every table.
/// Test: Live API.
pub async fn find_tables_in_document(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let url = format!("{DOCS_API_BASE}/documents/{id}");
    let doc = client.get(&url, account).await?;
    let mut tables = Vec::<Value>::new();
    if let Some(arr) = doc
        .get("body")
        .and_then(|b| b.get("content"))
        .and_then(|c| c.as_array())
    {
        for el in arr {
            if let Some(t) = el.get("table") {
                tables.push(json!({
                    "startIndex": el.get("startIndex"),
                    "endIndex": el.get("endIndex"),
                    "rows": t.get("rows"),
                    "columns": t.get("columns"),
                }));
            }
        }
    }
    Ok(json!({ "tables": tables }))
}

/// Why: Row/column ops share enough request shape to live behind one action enum.
/// What: Dispatches `insert_row|insert_column|delete_row|delete_column` to Docs batchUpdate.
/// Test: Live API.
pub async fn manage_table_structure(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    let id = require_str(&args, "document_id")?;
    let table_index = args
        .get("table_start_index")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("missing table_start_index"))?;
    let row = args.get("row").and_then(|v| v.as_i64()).unwrap_or(0);
    let column = args.get("column").and_then(|v| v.as_i64()).unwrap_or(0);

    let request = match action {
        "insert_row" => json!({
            "insertTableRow": {
                "tableCellLocation": {
                    "tableStartLocation": { "index": table_index },
                    "rowIndex": row,
                    "columnIndex": column,
                },
                "insertBelow": args.get("below").and_then(|v| v.as_bool()).unwrap_or(true),
            }
        }),
        "insert_column" => json!({
            "insertTableColumn": {
                "tableCellLocation": {
                    "tableStartLocation": { "index": table_index },
                    "rowIndex": row,
                    "columnIndex": column,
                },
                "insertRight": args.get("right").and_then(|v| v.as_bool()).unwrap_or(true),
            }
        }),
        "delete_row" => json!({
            "deleteTableRow": {
                "tableCellLocation": {
                    "tableStartLocation": { "index": table_index },
                    "rowIndex": row,
                    "columnIndex": column,
                }
            }
        }),
        "delete_column" => json!({
            "deleteTableColumn": {
                "tableCellLocation": {
                    "tableStartLocation": { "index": table_index },
                    "rowIndex": row,
                    "columnIndex": column,
                }
            }
        }),
        other => {
            return Err(anyhow!(
                "unknown action for manage_table_structure: {other}"
            ));
        }
    };
    let body = json!({ "requests": [request] });
    let url = format!("{DOCS_API_BASE}/documents/{id}:batchUpdate");
    client.post(&url, body, account).await
}
