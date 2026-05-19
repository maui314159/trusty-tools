//! Google Tasks service.
//!
//! Why: Tasks API has two resources (lists and tasks-within-a-list) with
//! identical CRUD shapes — we expose them as two tools.
//! What: `manage_task_lists` covers list-level CRUD; `manage_tasks` covers
//! per-task CRUD plus "complete" and "move".
//! Test: Live only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::TASKS_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// Convenience wrapper: list tasks from the default tasklist (`@default`).
///
/// Why: The CTO bot and other agents need a single-shot "what's on my
/// list?" tool without learning the action-style `manage_tasks` dispatcher.
/// What: GETs `lists/{tasklist}/tasks?maxResults={n}&showCompleted={b}` —
/// defaults to the user's default list, `max_results=20`,
/// `show_completed=false`. Projects each item to the small shape
/// `{id, title, due, status, notes}` so agents get predictable fields.
/// Test: Live API only; tool-shape covered by `tool_list_response()` test.
pub async fn list_tasks(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let tasklist = opt_str(&args, "tasklist_id").unwrap_or("@default");
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(20);
    let show_completed = args
        .get("show_completed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let url = format!(
        "{TASKS_API_BASE}/lists/{tasklist}/tasks?maxResults={max_results}&showCompleted={show_completed}"
    );
    let raw = client.get(&url, account).await?;

    let items = raw
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tasks: Vec<Value> = items
        .into_iter()
        .map(|t| {
            json!({
                "id":     t.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "title":  t.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "due":    t.get("due").and_then(|v| v.as_str()),
                "status": t.get("status").and_then(|v| v.as_str()).unwrap_or("needsAction"),
                "notes":  t.get("notes").and_then(|v| v.as_str()),
            })
        })
        .collect();
    Ok(json!({ "tasks": tasks }))
}

/// Convenience wrapper: mark a single task complete.
///
/// Why: Agents frequently want to tick exactly one task without learning
/// the full `manage_tasks` action enum.
/// What: PATCHes `lists/{tasklist}/tasks/{id}` with
/// `{"status": "completed"}`. Defaults to the `@default` tasklist.
/// Test: Live API only.
pub async fn complete_task(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let tasklist = opt_str(&args, "tasklist_id").unwrap_or("@default");
    let task_id = require_str(&args, "task_id")?;
    let url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks/{task_id}");
    client
        .patch(&url, json!({ "status": "completed" }), account)
        .await
}

/// Why: Task list CRUD is small enough to share one tool action enum.
/// What: Routes `list|create|delete|update` to `users/@me/lists` on the Tasks API.
/// Test: Live API.
pub async fn manage_task_lists(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    match action {
        "list" => {
            let url = format!("{TASKS_API_BASE}/users/@me/lists");
            client.get(&url, account).await
        }
        "create" => {
            let title = require_str(&args, "title")?;
            let url = format!("{TASKS_API_BASE}/users/@me/lists");
            client.post(&url, json!({ "title": title }), account).await
        }
        "update" => {
            let id = require_str(&args, "tasklist_id")?;
            let url = format!("{TASKS_API_BASE}/users/@me/lists/{id}");
            let body = args.get("updates").cloned().unwrap_or_else(|| json!({}));
            client.patch(&url, body, account).await
        }
        "delete" => {
            let id = require_str(&args, "tasklist_id")?;
            let url = format!("{TASKS_API_BASE}/users/@me/lists/{id}");
            client.delete(&url, account).await
        }
        other => Err(anyhow!("unknown action for manage_task_lists: {other}")),
    }
}

/// Why: Task CRUD inside a list is the bulk of the Tasks API surface.
/// What: Routes `list|get|create|update|delete|complete` to `lists/{id}/tasks`.
/// Test: Live API.
pub async fn manage_tasks(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    let tasklist = opt_str(&args, "tasklist_id").unwrap_or("@default");
    match action {
        "list" => {
            let url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks");
            client.get(&url, account).await
        }
        "create" => {
            let body = args
                .get("task")
                .cloned()
                .ok_or_else(|| anyhow!("missing 'task' object"))?;
            let url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks");
            client.post(&url, body, account).await
        }
        "update" => {
            let id = require_str(&args, "task_id")?;
            let body = args.get("updates").cloned().unwrap_or_else(|| json!({}));
            let url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks/{id}");
            client.patch(&url, body, account).await
        }
        "delete" => {
            let id = require_str(&args, "task_id")?;
            let url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks/{id}");
            client.delete(&url, account).await
        }
        "complete" => {
            let id = require_str(&args, "task_id")?;
            let body = json!({ "status": "completed" });
            let url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks/{id}");
            client.patch(&url, body, account).await
        }
        "move" => {
            let id = require_str(&args, "task_id")?;
            let mut url = format!("{TASKS_API_BASE}/lists/{tasklist}/tasks/{id}/move");
            let mut params = Vec::<String>::new();
            if let Some(parent) = opt_str(&args, "parent") {
                params.push(format!("parent={parent}"));
            }
            if let Some(prev) = opt_str(&args, "previous") {
                params.push(format!("previous={prev}"));
            }
            if !params.is_empty() {
                url = format!("{url}?{}", params.join("&"));
            }
            client.post(&url, json!({}), account).await
        }
        other => Err(anyhow!("unknown action for manage_tasks: {other}")),
    }
}
