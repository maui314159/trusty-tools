//! Native ticketing tools (#132/#133).
//!
//! Why: Expose `TicketingClient` operations as strongly-typed LLM tools so
//! agents don't have to shell out to `gh` / JIRA CLI / Linear CLI. Each tool
//! wraps an `Arc<dyn TicketingClient>` so the provider can be swapped by
//! config without changing tool wiring.
//! What: Five tools — `create_ticket`, `get_ticket`, `close_ticket`,
//! `list_tickets`, `add_comment`. All implement `ToolExecutor`.
//! Test: Construction and minimal error-path tests in `tests` below, using a
//! mock `TicketingClient` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::ticketing::actions::ActionsClient;
use crate::ticketing::{
    CreateTicketReq, Priority, TicketFilter, TicketStatus, TicketingClient, UpdateTicketReq,
};
use crate::tools::traits::{ToolExecutor, ToolResult};

/// Tool: `create_ticket`.
pub struct CreateTicketTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for CreateTicketTool {
    fn name(&self) -> &str {
        "create_ticket"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "create_ticket",
                "description": "Create a new ticket in the configured ticketing provider (GitHub / JIRA / Linear).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "body": {"type": "string"},
                        "labels": {"type": "array", "items": {"type": "string"}},
                        "priority": {"type": "string", "enum": ["low", "medium", "high", "critical"]},
                        "assignee": {"type": "string"}
                    },
                    "required": ["title", "body"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(title) = args.get("title").and_then(Value::as_str) else {
            return ToolResult::err("create_ticket: missing 'title'");
        };
        let Some(body) = args.get("body").and_then(Value::as_str) else {
            return ToolResult::err("create_ticket: missing 'body'");
        };
        let labels: Vec<String> = args
            .get("labels")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let priority = args
            .get("priority")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "low" => Some(Priority::Low),
                "medium" => Some(Priority::Medium),
                "high" => Some(Priority::High),
                "critical" => Some(Priority::Critical),
                _ => None,
            });
        let assignee = args
            .get("assignee")
            .and_then(Value::as_str)
            .map(String::from);

        let req = CreateTicketReq {
            title: title.to_string(),
            body: body.to_string(),
            labels,
            priority,
            assignee,
        };
        match self.0.create_ticket(req).await {
            Ok(t) => {
                let out = json!({
                    "id": t.id,
                    "title": t.title,
                    "url": t.url,
                });
                ToolResult::ok(out.to_string())
            }
            Err(e) => ToolResult::err(format!("create_ticket failed: {e:#}")),
        }
    }
}

/// Tool: `get_ticket`.
pub struct GetTicketTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for GetTicketTool {
    fn name(&self) -> &str {
        "get_ticket"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "get_ticket",
                "description": "Fetch a ticket by its provider-native id.",
                "parameters": {
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("get_ticket: missing 'id'");
        };
        match self.0.get_ticket(id).await {
            Ok(t) => match serde_json::to_string(&t) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("get_ticket: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("get_ticket failed: {e:#}")),
        }
    }
}

/// Tool: `close_ticket`.
pub struct CloseTicketTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for CloseTicketTool {
    fn name(&self) -> &str {
        "close_ticket"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "close_ticket",
                "description": "Close / complete a ticket. Optional comment is posted before close.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "comment": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("close_ticket: missing 'id'");
        };
        if let Some(comment) = args.get("comment").and_then(Value::as_str)
            && !comment.is_empty()
            && let Err(e) = self.0.add_comment(id, comment).await
        {
            return ToolResult::err(format!("close_ticket: comment failed: {e:#}"));
        }
        match self.0.close_ticket(id).await {
            Ok(()) => ToolResult::ok(json!({"closed": true, "id": id}).to_string()),
            Err(e) => ToolResult::err(format!("close_ticket failed: {e:#}")),
        }
    }
}

/// Tool: `list_tickets`.
pub struct ListTicketsTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for ListTicketsTool {
    fn name(&self) -> &str {
        "list_tickets"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "list_tickets",
                "description": "List tickets, optionally filtered by status/labels/limit.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "enum": ["open", "in_progress", "done", "closed"]},
                        "labels": {"type": "array", "items": {"type": "string"}},
                        "limit": {"type": "integer"}
                    },
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let status = args
            .get("status")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "open" => Some(TicketStatus::Open),
                "in_progress" => Some(TicketStatus::InProgress),
                "done" => Some(TicketStatus::Done),
                "closed" => Some(TicketStatus::Closed),
                _ => None,
            });
        let labels: Vec<String> = args
            .get("labels")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);

        let filter = TicketFilter {
            status,
            labels,
            assignee: None,
            limit,
        };
        match self.0.list_tickets(filter).await {
            Ok(ts) => match serde_json::to_string(&ts) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("list_tickets: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("list_tickets failed: {e:#}")),
        }
    }
}

/// Tool: `add_comment`.
pub struct AddCommentTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for AddCommentTool {
    fn name(&self) -> &str {
        "add_comment"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "add_comment",
                "description": "Add a comment to an existing ticket.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "body": {"type": "string"}
                    },
                    "required": ["id", "body"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("add_comment: missing 'id'");
        };
        let Some(body) = args.get("body").and_then(Value::as_str) else {
            return ToolResult::err("add_comment: missing 'body'");
        };
        match self.0.add_comment(id, body).await {
            Ok(()) => ToolResult::ok(json!({"added": true}).to_string()),
            Err(e) => ToolResult::err(format!("add_comment failed: {e:#}")),
        }
    }
}

/// Tool: `update_ticket` (#243).
///
/// Why: Closing the loop — agents often need to amend a ticket's title/body
/// or set status mid-workflow without going through close+recreate.
/// What: Accepts `id` plus any subset of `title`/`body`/`status`/`labels`/
/// `assignee` and forwards a partial `UpdateTicketReq` to the client.
/// Test: `update_ticket_tool_schema_has_required_id`,
/// `update_ticket_happy_path`.
pub struct UpdateTicketTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for UpdateTicketTool {
    fn name(&self) -> &str {
        "update_ticket"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "update_ticket",
                "description": "Update an existing ticket. Pass any subset of title/body/status/labels/assignee; unspecified fields are left unchanged.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "body": {"type": "string"},
                        "status": {"type": "string", "enum": ["open", "in_progress", "done", "closed"]},
                        "labels": {"type": "array", "items": {"type": "string"}},
                        "assignee": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("update_ticket: missing 'id'");
        };
        let title = args.get("title").and_then(Value::as_str).map(String::from);
        let body = args.get("body").and_then(Value::as_str).map(String::from);
        let status = args
            .get("status")
            .and_then(Value::as_str)
            .and_then(|s| match s {
                "open" => Some(TicketStatus::Open),
                "in_progress" => Some(TicketStatus::InProgress),
                "done" => Some(TicketStatus::Done),
                "closed" => Some(TicketStatus::Closed),
                _ => None,
            });
        let labels = args.get("labels").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
        let assignee = args
            .get("assignee")
            .and_then(Value::as_str)
            .map(String::from);

        let req = UpdateTicketReq {
            title,
            body,
            status,
            labels,
            assignee,
            ..Default::default()
        };
        match self.0.update_ticket(id, req).await {
            Ok(t) => match serde_json::to_string(&t) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("update_ticket: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("update_ticket failed: {e:#}")),
        }
    }
}

/// Tool: `actions_trigger` (#243).
///
/// Why: Lets the ticketing agent kick off a `workflow_dispatch` event so a
/// chat turn can rerun lint/CI/release pipelines without shelling out to
/// `gh`.
/// What: Wraps `GitHubActionsClient::trigger_workflow`. Accepts a workflow
/// filename or numeric ID, optional ref (default "main"), and optional
/// inputs object.
/// Test: `actions_trigger_tool_schema_is_valid`.
pub struct ActionsTriggerTool {
    pub client: Arc<dyn ActionsClient>,
}

#[async_trait]
impl ToolExecutor for ActionsTriggerTool {
    fn name(&self) -> &str {
        "actions_trigger"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "actions_trigger",
                "description": "Trigger a GitHub Actions workflow_dispatch event for the configured repo.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "workflow": {
                            "type": "string",
                            "description": "Workflow filename (e.g. 'ci.yml') or numeric ID."
                        },
                        "ref": {
                            "type": "string",
                            "description": "Branch or tag to run on (default 'main').",
                            "default": "main"
                        },
                        "inputs": {
                            "type": "object",
                            "description": "Optional workflow_dispatch inputs.",
                            "default": {}
                        }
                    },
                    "required": ["workflow"]
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(workflow) = args.get("workflow").and_then(Value::as_str) else {
            return ToolResult::err("actions_trigger: missing 'workflow'");
        };
        let git_ref = args
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string();
        let inputs = args.get("inputs").cloned().unwrap_or_else(|| json!({}));
        match self
            .client
            .trigger_workflow(workflow, &git_ref, inputs)
            .await
        {
            Ok(()) => ToolResult::ok(
                json!({
                    "triggered": true,
                    "workflow": workflow,
                    "ref": git_ref,
                })
                .to_string(),
            ),
            Err(e) => ToolResult::err(format!("actions_trigger failed: {e:#}")),
        }
    }
}

/// Tool: `actions_status` (#243).
///
/// Why: Pairs with `actions_trigger` — after kicking off a workflow the
/// agent often needs to summarize whether it succeeded, is still running,
/// or failed.
/// What: Wraps `GitHubActionsClient::list_runs` (default limit 5). Returns
/// the parsed runs as JSON for the LLM to summarize.
/// Test: `actions_status_tool_schema_is_valid`.
pub struct ActionsStatusTool {
    pub client: Arc<dyn ActionsClient>,
}

#[async_trait]
impl ToolExecutor for ActionsStatusTool {
    fn name(&self) -> &str {
        "actions_status"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "actions_status",
                "description": "Get recent GitHub Actions workflow run status.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "workflow": {
                            "type": "string",
                            "description": "Workflow filename or numeric ID."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Number of recent runs to return.",
                            "default": 5
                        }
                    },
                    "required": ["workflow"]
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(workflow) = args.get("workflow").and_then(Value::as_str) else {
            return ToolResult::err("actions_status: missing 'workflow'");
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .unwrap_or(5)
            .clamp(1, 100);
        match self.client.list_runs(workflow, limit).await {
            Ok(runs) => match serde_json::to_string(&runs) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("actions_status: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("actions_status failed: {e:#}")),
        }
    }
}

/// Tool: `ticket_tag` (#246).
///
/// Why: Lets agents add or remove labels without overwriting the rest of
/// a ticket's label set (the prior `update_ticket` semantic). Tags drive
/// triage workflows ("needs-review", "blocked", "good-first-issue").
/// What: Accepts an `id` plus optional `add` / `remove` arrays. Calls
/// `add_tags` then `remove_tags` (in that order) on the underlying
/// client and returns the updated ticket summary.
/// Test: `ticket_tag_tool_schema_requires_id`,
/// `ticket_tag_happy_path`.
pub struct TicketTagTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for TicketTagTool {
    fn name(&self) -> &str {
        "ticket_tag"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "ticket_tag",
                "description": "Add or remove tags/labels on a ticket without replacing the existing set.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "add": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Labels to add to the ticket."
                        },
                        "remove": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Labels to remove from the ticket."
                        }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("ticket_tag: missing 'id'");
        };
        let add: Vec<String> = args
            .get("add")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let remove: Vec<String> = args
            .get("remove")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if add.is_empty() && remove.is_empty() {
            return ToolResult::err("ticket_tag: must provide at least one of 'add' or 'remove'");
        }
        if !add.is_empty()
            && let Err(e) = self.0.add_tags(id, &add).await
        {
            return ToolResult::err(format!("ticket_tag add failed: {e:#}"));
        }
        if !remove.is_empty()
            && let Err(e) = self.0.remove_tags(id, &remove).await
        {
            return ToolResult::err(format!("ticket_tag remove failed: {e:#}"));
        }
        match self.0.get_ticket(id).await {
            Ok(t) => ToolResult::ok(
                json!({
                    "id": t.id,
                    "labels": t.labels,
                    "url": t.url,
                })
                .to_string(),
            ),
            Err(e) => ToolResult::err(format!("ticket_tag: refetch failed: {e:#}")),
        }
    }
}

/// Tool: `ticket_assign` (#246).
///
/// Why: Ownership transfer is a common workflow event; pass `""` to
/// `unassign` rather than introducing a separate tool.
/// What: Accepts `id` and `assignee`. Empty string => `unassign`,
/// otherwise => `assign`.
/// Test: `ticket_assign_tool_schema_requires_id_and_assignee`.
pub struct TicketAssignTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for TicketAssignTool {
    fn name(&self) -> &str {
        "ticket_assign"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "ticket_assign",
                "description": "Assign or unassign a ticket. Pass empty string for assignee to unassign.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "assignee": {
                            "type": "string",
                            "description": "GitHub username, or empty string to unassign"
                        }
                    },
                    "required": ["id", "assignee"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("ticket_assign: missing 'id'");
        };
        let Some(assignee) = args.get("assignee").and_then(Value::as_str) else {
            return ToolResult::err("ticket_assign: missing 'assignee'");
        };
        let result = if assignee.is_empty() {
            self.0.unassign(id).await
        } else {
            self.0.assign(id, assignee).await
        };
        match result {
            Ok(t) => match serde_json::to_string(&t) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("ticket_assign: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("ticket_assign failed: {e:#}")),
        }
    }
}

/// Tool: `ticket_transition` (#246).
///
/// Why: Surface the canonical workflow transition (open / in_progress /
/// in_review / done / closed / blocked / cancelled) so the agent doesn't
/// have to fall back to provider-specific status strings.
/// What: Accepts `id` and `status`. Maps the string to a `TicketStatus`
/// and calls `transition`.
/// Test: `ticket_transition_tool_schema_has_status_enum`.
pub struct TicketTransitionTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for TicketTransitionTool {
    fn name(&self) -> &str {
        "ticket_transition"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "ticket_transition",
                "description": "Move a ticket to a new status (canonical workflow state).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "status": {
                            "type": "string",
                            "enum": [
                                "open", "in_progress", "in_review",
                                "done", "closed", "blocked", "cancelled"
                            ]
                        }
                    },
                    "required": ["id", "status"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return ToolResult::err("ticket_transition: missing 'id'");
        };
        let Some(status_str) = args.get("status").and_then(Value::as_str) else {
            return ToolResult::err("ticket_transition: missing 'status'");
        };
        let status = match status_str {
            "open" => TicketStatus::Open,
            "in_progress" => TicketStatus::InProgress,
            "in_review" => TicketStatus::InReview,
            "done" => TicketStatus::Done,
            "closed" => TicketStatus::Closed,
            "blocked" => TicketStatus::Blocked,
            "cancelled" => TicketStatus::Cancelled,
            other => {
                return ToolResult::err(format!("ticket_transition: unknown status '{other}'"));
            }
        };
        match self.0.transition(id, status).await {
            Ok(t) => match serde_json::to_string(&t) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("ticket_transition: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("ticket_transition failed: {e:#}")),
        }
    }
}

/// Tool: `ticket_search` (#246).
///
/// Why: `list_tickets` only filters by structured fields; `search` lets
/// the agent find tickets by free-text phrase ("cors bug").
/// What: Accepts `query`, optional `state` (open/closed/all, default
/// open) and optional `limit` (default 10).
/// Test: `ticket_search_tool_schema_requires_query`.
pub struct TicketSearchTool(pub Arc<dyn TicketingClient>);

#[async_trait]
impl ToolExecutor for TicketSearchTool {
    fn name(&self) -> &str {
        "ticket_search"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "ticket_search",
                "description": "Full-text search across tickets in the configured provider.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "state": {
                            "type": "string",
                            "enum": ["open", "closed", "all"],
                            "default": "open"
                        },
                        "limit": {"type": "integer", "default": 10}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return ToolResult::err("ticket_search: missing 'query'");
        };
        let state = args.get("state").and_then(Value::as_str).unwrap_or("open");
        let status = match state {
            "closed" => Some(TicketStatus::Closed),
            "all" => None,
            _ => Some(TicketStatus::Open),
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(10);
        let filter = TicketFilter {
            status,
            labels: vec![],
            assignee: None,
            limit: Some(limit),
        };
        match self.0.search(query, filter).await {
            Ok(ts) => match serde_json::to_string(&ts) {
                Ok(s) => ToolResult::ok(s),
                Err(e) => ToolResult::err(format!("ticket_search: serialize failed: {e}")),
            },
            Err(e) => ToolResult::err(format!("ticket_search failed: {e:#}")),
        }
    }
}

/// Build the full set of ticketing + actions tools (#243).
///
/// Why: Centralizes the "what tools does the ticketing agent get" decision
/// so callers (ctrl, PM, ticketing-agent runner) all get the same set
/// without copy/pasting eight `Arc::new`s.
/// What: Returns 10 ticketing tools (always — the 6 originals plus
/// `ticket_tag`, `ticket_assign`, `ticket_transition`, `ticket_search`
/// from #246) plus 2 actions tools when an `actions` client is provided.
/// When `actions` is `None`, only the 10 ticketing tools are returned.
/// Test: `ticketing_tools_count`, `ticketing_tools_count_is_12`.
pub fn ticketing_tools(
    client: Arc<dyn TicketingClient>,
    actions: Option<Arc<dyn ActionsClient>>,
) -> Vec<Arc<dyn ToolExecutor>> {
    let mut out: Vec<Arc<dyn ToolExecutor>> = vec![
        Arc::new(CreateTicketTool(client.clone())),
        Arc::new(GetTicketTool(client.clone())),
        Arc::new(UpdateTicketTool(client.clone())),
        Arc::new(CloseTicketTool(client.clone())),
        Arc::new(ListTicketsTool(client.clone())),
        Arc::new(AddCommentTool(client.clone())),
        Arc::new(TicketTagTool(client.clone())),
        Arc::new(TicketAssignTool(client.clone())),
        Arc::new(TicketTransitionTool(client.clone())),
        Arc::new(TicketSearchTool(client)),
    ];
    if let Some(a) = actions {
        out.push(Arc::new(ActionsTriggerTool { client: a.clone() }));
        out.push(Arc::new(ActionsStatusTool { client: a }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticketing::types::{Ticket, TicketStatus, UpdateTicketReq};
    use anyhow::{Result, anyhow};

    /// Mock client that records calls and can be configured to fail.
    struct MockClient {
        fail: bool,
    }

    #[async_trait]
    impl TicketingClient for MockClient {
        fn provider_name(&self) -> &str {
            "mock"
        }

        async fn create_ticket(&self, req: CreateTicketReq) -> Result<Ticket> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            Ok(Ticket {
                id: "1".into(),
                title: req.title,
                body: req.body,
                status: TicketStatus::Open,
                priority: None,
                labels: req.labels,
                assignee: None,
                created_at: None,
                updated_at: None,
                url: Some("http://x/1".into()),
            })
        }

        async fn get_ticket(&self, id: &str) -> Result<Ticket> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            Ok(Ticket {
                id: id.into(),
                title: "T".into(),
                body: "B".into(),
                status: TicketStatus::Open,
                priority: None,
                labels: vec![],
                assignee: None,
                created_at: None,
                updated_at: None,
                url: None,
            })
        }

        async fn update_ticket(&self, id: &str, _req: UpdateTicketReq) -> Result<Ticket> {
            self.get_ticket(id).await
        }

        async fn close_ticket(&self, _id: &str) -> Result<()> {
            if self.fail {
                Err(anyhow!("boom"))
            } else {
                Ok(())
            }
        }

        async fn list_tickets(&self, _f: TicketFilter) -> Result<Vec<Ticket>> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            Ok(vec![])
        }

        async fn add_comment(&self, _id: &str, _body: &str) -> Result<()> {
            if self.fail {
                Err(anyhow!("boom"))
            } else {
                Ok(())
            }
        }

        async fn add_tags(&self, id: &str, _tags: &[String]) -> Result<Ticket> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            self.get_ticket(id).await
        }

        async fn remove_tags(&self, id: &str, _tags: &[String]) -> Result<Ticket> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            self.get_ticket(id).await
        }

        async fn assign(&self, id: &str, _assignee: &str) -> Result<Ticket> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            self.get_ticket(id).await
        }

        async fn unassign(&self, id: &str) -> Result<Ticket> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            self.get_ticket(id).await
        }

        async fn search(&self, _query: &str, _filter: TicketFilter) -> Result<Vec<Ticket>> {
            if self.fail {
                return Err(anyhow!("boom"));
            }
            Ok(vec![])
        }
    }

    fn ok_client() -> Arc<dyn TicketingClient> {
        Arc::new(MockClient { fail: false })
    }

    fn fail_client() -> Arc<dyn TicketingClient> {
        Arc::new(MockClient { fail: true })
    }

    #[tokio::test]
    async fn create_ticket_happy_path() {
        let tool = CreateTicketTool(ok_client());
        assert_eq!(tool.name(), "create_ticket");
        let out = tool
            .execute(json!({"title": "T", "body": "B", "labels": ["bug"]}))
            .await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["id"], "1");
    }

    #[tokio::test]
    async fn create_ticket_missing_title() {
        let tool = CreateTicketTool(ok_client());
        assert!(tool.execute(json!({"body": "b"})).await.is_error());
    }

    #[tokio::test]
    async fn create_ticket_propagates_error() {
        let tool = CreateTicketTool(fail_client());
        let out = tool.execute(json!({"title": "T", "body": "B"})).await;
        assert!(out.is_error());
    }

    #[tokio::test]
    async fn get_ticket_happy_path() {
        let tool = GetTicketTool(ok_client());
        let out = tool.execute(json!({"id": "42"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["id"], "42");
    }

    #[tokio::test]
    async fn get_ticket_missing_id() {
        let tool = GetTicketTool(ok_client());
        assert!(tool.execute(json!({})).await.is_error());
    }

    #[tokio::test]
    async fn close_ticket_happy_path() {
        let tool = CloseTicketTool(ok_client());
        let out = tool.execute(json!({"id": "1"})).await;
        assert!(!out.is_error());
    }

    #[tokio::test]
    async fn close_ticket_with_comment() {
        let tool = CloseTicketTool(ok_client());
        let out = tool.execute(json!({"id": "1", "comment": "fixed"})).await;
        assert!(!out.is_error());
    }

    #[tokio::test]
    async fn list_tickets_happy_path() {
        let tool = ListTicketsTool(ok_client());
        let out = tool.execute(json!({})).await;
        assert!(!out.is_error());
    }

    #[tokio::test]
    async fn list_tickets_with_filters() {
        let tool = ListTicketsTool(ok_client());
        let out = tool.execute(json!({"status": "open", "limit": 10})).await;
        assert!(!out.is_error());
    }

    #[tokio::test]
    async fn add_comment_happy_path() {
        let tool = AddCommentTool(ok_client());
        let out = tool.execute(json!({"id": "1", "body": "hi"})).await;
        assert!(!out.is_error());
    }

    #[tokio::test]
    async fn add_comment_missing_body() {
        let tool = AddCommentTool(ok_client());
        assert!(tool.execute(json!({"id": "1"})).await.is_error());
    }

    // ----- #243: UpdateTicketTool + Actions tools + ticketing_tools() -----

    #[test]
    fn update_ticket_tool_schema_has_required_id() {
        let tool = UpdateTicketTool(ok_client());
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "update_ticket");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert!(required.iter().any(|v| v == "id"));
        // Status enum is locked to canonical values.
        let status_enum = &s["function"]["parameters"]["properties"]["status"]["enum"];
        assert_eq!(status_enum[0], "open");
        assert_eq!(status_enum[1], "in_progress");
    }

    #[tokio::test]
    async fn update_ticket_happy_path() {
        let tool = UpdateTicketTool(ok_client());
        let out = tool
            .execute(json!({"id": "5", "title": "new", "status": "in_progress"}))
            .await;
        assert!(!out.is_error(), "got error: {}", out.content());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["id"], "5");
    }

    #[tokio::test]
    async fn update_ticket_missing_id() {
        let tool = UpdateTicketTool(ok_client());
        assert!(tool.execute(json!({"title": "x"})).await.is_error());
    }

    fn fake_actions_client() -> Arc<dyn ActionsClient> {
        Arc::new(
            crate::ticketing::actions::GitHubActionsClient::new("fake-token", "owner/repo")
                .expect("fake actions client"),
        )
    }

    #[test]
    fn actions_trigger_tool_schema_is_valid() {
        let tool = ActionsTriggerTool {
            client: fake_actions_client(),
        };
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "actions_trigger");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert!(required.iter().any(|v| v == "workflow"));
        // 'ref' is optional with a default.
        assert_eq!(
            s["function"]["parameters"]["properties"]["ref"]["default"],
            "main"
        );
    }

    #[test]
    fn actions_status_tool_schema_is_valid() {
        let tool = ActionsStatusTool {
            client: fake_actions_client(),
        };
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "actions_status");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert!(required.iter().any(|v| v == "workflow"));
        assert_eq!(
            s["function"]["parameters"]["properties"]["limit"]["default"],
            5
        );
    }

    #[tokio::test]
    async fn actions_trigger_missing_workflow_errors() {
        let tool = ActionsTriggerTool {
            client: fake_actions_client(),
        };
        // No 'workflow' arg — must fail before any network call.
        assert!(tool.execute(json!({})).await.is_error());
    }

    #[tokio::test]
    async fn actions_status_missing_workflow_errors() {
        let tool = ActionsStatusTool {
            client: fake_actions_client(),
        };
        assert!(tool.execute(json!({})).await.is_error());
    }

    #[test]
    fn ticketing_tools_count() {
        // Without actions: 10 tools (6 originals + ticket_tag/assign/transition/search).
        let tools = ticketing_tools(ok_client(), None);
        assert_eq!(tools.len(), 10);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"create_ticket"));
        assert!(names.contains(&"get_ticket"));
        assert!(names.contains(&"update_ticket"));
        assert!(names.contains(&"close_ticket"));
        assert!(names.contains(&"list_tickets"));
        assert!(names.contains(&"add_comment"));
        assert!(names.contains(&"ticket_tag"));
        assert!(names.contains(&"ticket_assign"));
        assert!(names.contains(&"ticket_transition"));
        assert!(names.contains(&"ticket_search"));

        // With actions: 10 + 2 = 12.
        let tools_full = ticketing_tools(ok_client(), Some(fake_actions_client()));
        assert_eq!(tools_full.len(), 12);
        let names_full: Vec<&str> = tools_full.iter().map(|t| t.name()).collect();
        assert!(names_full.contains(&"actions_trigger"));
        assert!(names_full.contains(&"actions_status"));
    }

    // ----- #246: ticket_tag / ticket_assign / ticket_transition / ticket_search -----

    #[test]
    fn ticket_tag_tool_schema_requires_id() {
        let tool = TicketTagTool(ok_client());
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "ticket_tag");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "id");
        // Both add and remove arrays of strings.
        assert_eq!(
            s["function"]["parameters"]["properties"]["add"]["type"],
            "array"
        );
        assert_eq!(
            s["function"]["parameters"]["properties"]["remove"]["type"],
            "array"
        );
    }

    #[tokio::test]
    async fn ticket_tag_requires_at_least_one_action() {
        let tool = TicketTagTool(ok_client());
        let out = tool.execute(json!({"id": "1"})).await;
        assert!(out.is_error());
    }

    #[tokio::test]
    async fn ticket_tag_happy_path_add() {
        let tool = TicketTagTool(ok_client());
        let out = tool.execute(json!({"id": "1", "add": ["bug"]})).await;
        assert!(!out.is_error(), "got error: {}", out.content());
    }

    #[test]
    fn ticket_assign_tool_schema_requires_id_and_assignee() {
        let tool = TicketAssignTool(ok_client());
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "ticket_assign");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert!(required.iter().any(|v| v == "id"));
        assert!(required.iter().any(|v| v == "assignee"));
        assert_eq!(required.len(), 2);
    }

    #[tokio::test]
    async fn ticket_assign_with_user() {
        let tool = TicketAssignTool(ok_client());
        let out = tool.execute(json!({"id": "1", "assignee": "alice"})).await;
        assert!(!out.is_error(), "got error: {}", out.content());
    }

    #[tokio::test]
    async fn ticket_assign_empty_string_unassigns() {
        let tool = TicketAssignTool(ok_client());
        let out = tool.execute(json!({"id": "1", "assignee": ""})).await;
        assert!(!out.is_error(), "got error: {}", out.content());
    }

    #[test]
    fn ticket_transition_tool_schema_has_status_enum() {
        let tool = TicketTransitionTool(ok_client());
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "ticket_transition");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert!(required.iter().any(|v| v == "id"));
        assert!(required.iter().any(|v| v == "status"));
        let status_enum = s["function"]["parameters"]["properties"]["status"]["enum"]
            .as_array()
            .expect("status enum is array");
        let names: Vec<&str> = status_enum.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"open"));
        assert!(names.contains(&"in_progress"));
        assert!(names.contains(&"in_review"));
        assert!(names.contains(&"done"));
        assert!(names.contains(&"closed"));
        assert!(names.contains(&"blocked"));
        assert!(names.contains(&"cancelled"));
    }

    #[tokio::test]
    async fn ticket_transition_unknown_status_errors() {
        let tool = TicketTransitionTool(ok_client());
        let out = tool.execute(json!({"id": "1", "status": "garbage"})).await;
        assert!(out.is_error());
    }

    #[test]
    fn ticket_search_tool_schema_requires_query() {
        let tool = TicketSearchTool(ok_client());
        let s = tool.schema();
        assert_eq!(s["function"]["name"], "ticket_search");
        let required = s["function"]["parameters"]["required"]
            .as_array()
            .expect("required is array");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "query");
        // state has a default of "open".
        assert_eq!(
            s["function"]["parameters"]["properties"]["state"]["default"],
            "open"
        );
        // limit has default 10.
        assert_eq!(
            s["function"]["parameters"]["properties"]["limit"]["default"],
            10
        );
    }

    #[tokio::test]
    async fn ticket_search_missing_query_errors() {
        let tool = TicketSearchTool(ok_client());
        let out = tool.execute(json!({})).await;
        assert!(out.is_error());
    }

    #[tokio::test]
    async fn ticket_search_happy_path() {
        let tool = TicketSearchTool(ok_client());
        let out = tool.execute(json!({"query": "cors"})).await;
        assert!(!out.is_error(), "got error: {}", out.content());
    }

    #[test]
    fn ticketing_tools_count_is_12() {
        // Total 12 tools when actions client is provided (10 ticketing + 2 actions).
        let tools = ticketing_tools(ok_client(), Some(fake_actions_client()));
        assert_eq!(tools.len(), 12);
    }
}
