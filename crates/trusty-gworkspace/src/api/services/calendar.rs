//! Google Calendar service.
//!
//! Why: Calendars + events + free/busy queries are the three primary user
//! workflows; one module each Python service module.
//! What: Dispatches on the `action` field (list|create|update|delete) plus
//! `query_free_busy` as a separate tool.
//! Test: Integration only.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::api::client::BaseClient;
use crate::api::constants::CALENDAR_API_BASE;
use crate::api::services::{account_of, opt_str, require_str};

/// CRUD operations against the calendarList collection.
///
/// Why: The Google API splits Calendar into the resource (`/calendars`) and
/// the user's subscription list (`/users/me/calendarList`). For listing we
/// use `calendarList`; for create/update/delete we hit `/calendars`.
/// What: `action` ∈ {"list", "create", "update", "delete"}.
/// Test: Live calls only.
pub async fn manage_calendars(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    match action {
        "list" => {
            let url = format!("{CALENDAR_API_BASE}/users/me/calendarList");
            client.get(&url, account).await
        }
        "create" => {
            let summary = require_str(&args, "summary")?;
            let body = json!({
                "summary": summary,
                "description": args.get("description"),
                "timeZone": args.get("time_zone"),
            });
            let url = format!("{CALENDAR_API_BASE}/calendars");
            client.post(&url, body, account).await
        }
        "update" => {
            let calendar_id = require_str(&args, "calendar_id")?;
            let url = format!("{CALENDAR_API_BASE}/calendars/{calendar_id}");
            let body = args.get("updates").cloned().unwrap_or_else(|| json!({}));
            client.patch(&url, body, account).await
        }
        "delete" => {
            let calendar_id = require_str(&args, "calendar_id")?;
            let url = format!("{CALENDAR_API_BASE}/calendars/{calendar_id}");
            client.delete(&url, account).await
        }
        other => Err(anyhow!("unknown action for manage_calendars: {other}")),
    }
}

/// CRUD over events within a calendar.
/// Why: Single tool surfaces every event CRUD path so the model dispatches by `action`.
/// What: Routes `list|get|insert|update|delete` to the Calendar v3 `/events` endpoints.
/// Test: Live API; logic-only branches covered by argument-extraction smoke tests.
pub async fn manage_events(client: &BaseClient, args: Value) -> Result<Value> {
    let action = require_str(&args, "action")?;
    let account = account_of(&args);
    let calendar_id = opt_str(&args, "calendar_id").unwrap_or("primary");
    match action {
        "list" => {
            let mut url = format!("{CALENDAR_API_BASE}/calendars/{calendar_id}/events");
            let mut params = Vec::<(String, String)>::new();
            if let Some(t) = opt_str(&args, "time_min") {
                params.push(("timeMin".into(), t.into()));
            }
            if let Some(t) = opt_str(&args, "time_max") {
                params.push(("timeMax".into(), t.into()));
            }
            if let Some(q) = opt_str(&args, "query") {
                params.push(("q".into(), q.into()));
            }
            if let Some(max) = args.get("max_results").and_then(|v| v.as_i64()) {
                params.push(("maxResults".into(), max.to_string()));
            }
            if !params.is_empty() {
                let qs: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
                url = format!("{url}?{}", qs.join("&"));
            }
            client.get(&url, account).await
        }
        "create" => {
            let body = args
                .get("event")
                .cloned()
                .ok_or_else(|| anyhow!("missing 'event' object"))?;
            let url = format!("{CALENDAR_API_BASE}/calendars/{calendar_id}/events");
            client.post(&url, body, account).await
        }
        "update" => {
            let event_id = require_str(&args, "event_id")?;
            let body = args.get("updates").cloned().unwrap_or_else(|| json!({}));
            let url = format!("{CALENDAR_API_BASE}/calendars/{calendar_id}/events/{event_id}");
            client.patch(&url, body, account).await
        }
        "delete" => {
            let event_id = require_str(&args, "event_id")?;
            let url = format!("{CALENDAR_API_BASE}/calendars/{calendar_id}/events/{event_id}");
            client.delete(&url, account).await
        }
        other => Err(anyhow!("unknown action for manage_events: {other}")),
    }
}

/// Free/busy query — useful for scheduling.
/// Why: Scheduling assistants need a fast availability check across multiple calendars.
/// What: POSTs to `/freeBusy` with `time_min`, `time_max`, and a calendar id list.
/// Test: Live API.
pub async fn query_free_busy(client: &BaseClient, args: Value) -> Result<Value> {
    let account = account_of(&args);
    let time_min = require_str(&args, "time_min")?;
    let time_max = require_str(&args, "time_max")?;
    let calendars: Vec<Value> = args
        .get("calendar_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| json!({ "id": s }))
                .collect()
        })
        .unwrap_or_else(|| vec![json!({ "id": "primary" })]);
    let body = json!({
        "timeMin": time_min,
        "timeMax": time_max,
        "items": calendars,
    });
    let url = format!("{CALENDAR_API_BASE}/freeBusy");
    client.post(&url, body, account).await
}
