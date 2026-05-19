//! MCP tool surface for trusty-memory.
//!
//! Why: Concentrates the public tool contract in one file so changes are
//! auditable and the MCP schema stays in sync with the implementation.
//! What: Defines `MemoryMcpServer`, `tool_definitions()` (the MCP
//! `tools/list` payload), and the in-process tool dispatcher wired to the
//! real `PalaceRegistry` + retrieval / KG APIs.
//! Test: `cargo test -p trusty-memory-mcp` validates the schema and dispatch.
//!
//! Tools exposed:
//! - `memory_remember(palace, text, room?, tags?)` -> drawer_id
//! - `memory_recall(palace, query, top_k?)`        -> Vec<Drawer> (L0+L1+L2)
//! - `memory_recall_deep(palace, query, top_k?)`   -> Vec<Drawer> (L3 deep)
//! - `memory_list(palace, room?, tag?, limit?)`    -> Vec<Drawer>
//! - `memory_forget(palace, drawer_id)`            -> ()
//! - `palace_create(name, description?)`           -> PalaceId
//! - `palace_list()`                                -> Vec<PalaceId>
//! - `palace_info(palace)`                          -> palace metadata + stats
//! - `kg_assert(palace, subject, predicate, object, confidence?, provenance?)` -> ()
//! - `kg_query(palace, subject)`                    -> Vec<Triple>

use crate::AppState;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use trusty_memory_core::palace::{Palace, PalaceId, RoomType};
use trusty_memory_core::retrieval::{recall, recall_across_palaces, recall_deep};
use trusty_memory_core::store::kg::Triple;
use uuid::Uuid;

/// Marker server type. Reserved for future stateful MCP server impls.
///
/// Why: Keep a stable type name while the protocol-loop is implemented at
/// module level, so external callers can still depend on a server symbol.
/// What: Zero-sized struct with `new` / `Default`.
/// Test: `MemoryMcpServer::default()` constructs without panic.
pub struct MemoryMcpServer;

impl MemoryMcpServer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MemoryMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// MCP `tools/list` response payload.
///
/// Why: Claude Code calls `tools/list` once on connect and uses the schema
/// to drive the tool picker; the schema is the source of truth for arg names.
/// `palace` is required only when the server has no `--palace` default
/// configured — when a default is set, the schema omits `palace` from
/// `required` so clients can drop it.
/// What: Returns a JSON object `{ "tools": [...] }` with all 10 tool defs.
/// Test: `tool_definitions_lists_all_tools`,
/// `tool_definitions_drops_palace_required_when_default_set`.
pub fn tool_definitions() -> Value {
    tool_definitions_with(false)
}

/// Variant of `tool_definitions` aware of whether a default palace is
/// configured. When `has_default` is true, the `palace` argument is moved
/// out of the `required` list for every tool that takes it.
///
/// Why: Lets `handle_message` emit a schema that matches the running
/// server's actual contract — clients reading the schema should see exactly
/// what they need to send.
/// What: Builds the same shape as `tool_definitions` but with conditional
/// `required` arrays.
/// Test: `tool_definitions_drops_palace_required_when_default_set`.
pub fn tool_definitions_with(has_default: bool) -> Value {
    let memory_remember_required: Vec<&str> = if has_default {
        vec!["text"]
    } else {
        vec!["palace", "text"]
    };
    let memory_recall_required: Vec<&str> = if has_default {
        vec!["query"]
    } else {
        vec!["palace", "query"]
    };
    let kg_assert_required: Vec<&str> = if has_default {
        vec!["subject", "predicate", "object"]
    } else {
        vec!["palace", "subject", "predicate", "object"]
    };
    let kg_query_required: Vec<&str> = if has_default {
        vec!["subject"]
    } else {
        vec!["palace", "subject"]
    };
    let memory_list_required: Vec<&str> = if has_default { vec![] } else { vec!["palace"] };
    let memory_forget_required: Vec<&str> = if has_default {
        vec!["drawer_id"]
    } else {
        vec!["palace", "drawer_id"]
    };
    let palace_info_required: Vec<&str> = if has_default { vec![] } else { vec!["palace"] };
    let palace_compact_required: Vec<&str> = if has_default { vec![] } else { vec!["palace"] };

    json!({
        "tools": [
            {
                "name": "memory_remember",
                "description": "Store a memory (drawer) in a palace room.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace": {"type": "string", "description": "Palace ID (optional if server started with --palace)"},
                        "text":   {"type": "string", "description": "Memory content"},
                        "room":   {"type": "string", "description": "Room type (optional)"},
                        "tags":   {"type": "array", "items": {"type": "string"}}
                    },
                    "required": memory_remember_required,
                }
            },
            {
                "name": "memory_recall",
                "description": "Recall memories using L0+L1+L2 progressive retrieval.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace": {"type": "string"},
                        "query":  {"type": "string"},
                        "top_k":  {"type": "integer", "default": 10}
                    },
                    "required": memory_recall_required,
                }
            },
            {
                "name": "memory_recall_deep",
                "description": "Deep recall using L3 full HNSW search.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace": {"type": "string"},
                        "query":  {"type": "string"},
                        "top_k":  {"type": "integer", "default": 10}
                    },
                    "required": memory_recall_required,
                }
            },
            {
                "name": "palace_create",
                "description": "Create a new memory palace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name":        {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "palace_list",
                "description": "List all palaces on this machine.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "kg_assert",
                "description": "Assert a fact in the temporal knowledge graph.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace":     {"type": "string"},
                        "subject":    {"type": "string"},
                        "predicate":  {"type": "string"},
                        "object":     {"type": "string"},
                        "confidence": {"type": "number", "default": 1.0},
                        "provenance": {"type": "string"}
                    },
                    "required": kg_assert_required,
                }
            },
            {
                "name": "kg_query",
                "description": "Query active knowledge-graph triples for a subject.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace":  {"type": "string"},
                        "subject": {"type": "string"}
                    },
                    "required": kg_query_required,
                }
            },
            {
                "name": "memory_list",
                "description": "List drawers in a palace, optionally filtered by room type or tag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace": {"type": "string"},
                        "room":   {"type": "string", "description": "Filter by room type (Frontend, Backend, Testing, Planning, Documentation, Research, Configuration, Meetings, General, or custom)"},
                        "tag":    {"type": "string", "description": "Filter by tag"},
                        "limit":  {"type": "integer", "description": "Max results (default 50)"}
                    },
                    "required": memory_list_required,
                }
            },
            {
                "name": "memory_forget",
                "description": "Delete a drawer from a palace by its UUID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace":    {"type": "string"},
                        "drawer_id": {"type": "string", "description": "UUID of the drawer to delete"}
                    },
                    "required": memory_forget_required,
                }
            },
            {
                "name": "palace_info",
                "description": "Get metadata and stats for a single palace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace": {"type": "string"}
                    },
                    "required": palace_info_required,
                }
            },
            {
                "name": "palace_compact",
                "description": "Remove orphaned vector index entries (vectors with no matching drawer row). See issue #49.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "palace": {"type": "string"}
                    },
                    "required": palace_compact_required,
                }
            },
            {
                "name": "memory_recall_all",
                "description": "Semantic search across ALL palaces simultaneously. Returns the top-k most relevant drawers ranked by similarity, regardless of which palace they belong to. Each result includes a `palace_id` field identifying its source.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "q":     {"type": "string", "description": "Free-text query"},
                        "top_k": {"type": "integer", "default": 10},
                        "deep":  {"type": "boolean", "default": false}
                    },
                    "required": ["q"],
                }
            }
        ]
    })
}

/// Parse a `RoomType` from an optional string (`"Backend"`, `"Frontend"`,
/// etc.) — falls back to `RoomType::General` when unset or unknown.
///
/// Why: MCP arguments are JSON; we accept the friendly enum-name forms so
/// callers don't have to learn an internal serialization.
/// What: Match-on-string returning the corresponding `RoomType`.
/// Test: Indirectly via `dispatch_remember_then_recall`.
fn parse_room(s: Option<&str>) -> RoomType {
    match s.unwrap_or("General") {
        "Frontend" => RoomType::Frontend,
        "Backend" => RoomType::Backend,
        "Testing" => RoomType::Testing,
        "Planning" => RoomType::Planning,
        "Documentation" => RoomType::Documentation,
        "Research" => RoomType::Research,
        "Configuration" => RoomType::Configuration,
        "Meetings" => RoomType::Meetings,
        "General" => RoomType::General,
        other => RoomType::Custom(other.to_string()),
    }
}

/// Resolve (or lazily open) the palace handle for a tool call.
fn open_palace_handle(
    state: &AppState,
    palace_id: &str,
) -> Result<std::sync::Arc<trusty_memory_core::PalaceHandle>> {
    let pid = PalaceId::new(palace_id);
    state
        .registry
        .open_palace(&state.data_root, &pid)
        .with_context(|| format!("open palace {palace_id}"))
}

/// Resolve a palace argument, falling back to `state.default_palace` when
/// the caller omitted `palace`.
///
/// Why: `serve --palace <name>` lets the operator bind a process to a single
/// project namespace; tool calls then no longer need to repeat the palace
/// every time. This helper centralises the precedence rule (explicit arg
/// wins over default) and produces a uniform error when neither is set.
/// What: Returns the explicit `args["palace"]` string if present, otherwise
/// `state.default_palace`. Errors with a helpful message if both are absent.
/// Test: `default_palace_used_when_arg_omitted` and
/// `dispatch_unknown_tool_errors`.
fn resolve_palace<'a>(state: &'a AppState, args: &'a Value, tool: &str) -> Result<String> {
    if let Some(p) = args.get("palace").and_then(|v| v.as_str()) {
        return Ok(p.to_string());
    }
    state
        .default_palace
        .clone()
        .ok_or_else(|| anyhow!("{tool}: missing 'palace' (no --palace default configured)"))
}

/// Dispatch a tool call by name to its real handler.
///
/// Why: Centralises the name → handler mapping; every handler now performs a
/// real read/write against the live `PalaceRegistry` instead of returning a
/// stub.
/// What: Returns `Ok(Value)` on success, `Err` on unknown tool / bad args /
/// underlying failure.
/// Test: `dispatch_palace_create_persists`, `dispatch_remember_then_recall`,
/// `dispatch_kg_assert_then_query`, `dispatch_unknown_tool_errors`.
pub async fn dispatch_tool(state: &AppState, name: &str, args: Value) -> Result<Value> {
    match name {
        "memory_remember" => {
            let palace = resolve_palace(state, &args, "memory_remember")?;
            let palace = palace.as_str();
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("memory_remember: missing 'text'"))?
                .to_string();
            let room = parse_room(args.get("room").and_then(|v| v.as_str()));
            let tags: Vec<String> = args
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let handle = open_palace_handle(state, palace)?;
            let drawer_id = handle
                .remember(text, room, tags, 0.5)
                .await
                .context("PalaceHandle::remember")?;
            Ok(json!({
                "drawer_id": drawer_id.to_string(),
                "palace": palace,
                "status": "stored",
            }))
        }
        "memory_recall" => {
            let palace = resolve_palace(state, &args, "memory_recall")?;
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("memory_recall: missing 'query'"))?;
            let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

            let handle = open_palace_handle(state, &palace)?;
            let embedder = state.embedder().await?;
            let results = recall(&handle, embedder.as_ref(), query, top_k)
                .await
                .context("recall")?;
            Ok(serialize_recall(&palace, query, results))
        }
        "memory_recall_deep" => {
            let palace = resolve_palace(state, &args, "memory_recall_deep")?;
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("memory_recall_deep: missing 'query'"))?;
            let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

            let handle = open_palace_handle(state, &palace)?;
            let embedder = state.embedder().await?;
            let results = recall_deep(&handle, embedder.as_ref(), query, top_k)
                .await
                .context("recall_deep")?;
            Ok(serialize_recall(&palace, query, results))
        }
        "palace_create" => {
            let palace_name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("palace_create: missing 'name'"))?;
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let palace = Palace {
                id: PalaceId::new(palace_name),
                name: palace_name.to_string(),
                description,
                created_at: chrono::Utc::now(),
                data_dir: state.data_root.join(palace_name),
            };
            let _handle = state
                .registry
                .create_palace(&state.data_root, palace)
                .context("create_palace")?;
            Ok(json!({"palace_id": palace_name, "status": "created"}))
        }
        "palace_list" => {
            let root = state.data_root.clone();
            let palaces = tokio::task::spawn_blocking(move || {
                trusty_memory_core::PalaceRegistry::list_palaces(&root)
            })
            .await
            .context("join list_palaces")??;
            let ids: Vec<String> = palaces.iter().map(|p| p.id.as_str().to_string()).collect();
            Ok(json!({"palaces": ids}))
        }
        "kg_assert" => {
            let palace = resolve_palace(state, &args, "kg_assert")?;
            let palace = palace.as_str();
            let subject = args
                .get("subject")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("kg_assert: missing 'subject'"))?
                .to_string();
            let predicate = args
                .get("predicate")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("kg_assert: missing 'predicate'"))?
                .to_string();
            let object = args
                .get("object")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("kg_assert: missing 'object'"))?
                .to_string();
            let confidence = args
                .get("confidence")
                .and_then(|v| v.as_f64())
                .map(|c| (c as f32).clamp(0.0, 1.0))
                .unwrap_or(1.0);
            let provenance = args
                .get("provenance")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let handle = open_palace_handle(state, palace)?;
            let triple = Triple {
                subject,
                predicate,
                object,
                valid_from: chrono::Utc::now(),
                valid_to: None,
                confidence,
                provenance,
            };
            handle.kg.assert(triple).await.context("kg.assert")?;
            Ok(json!({"status": "asserted"}))
        }
        "kg_query" => {
            let palace = resolve_palace(state, &args, "kg_query")?;
            let subject = args
                .get("subject")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("kg_query: missing 'subject'"))?;
            let handle = open_palace_handle(state, &palace)?;
            let triples = handle
                .kg
                .query_active(subject)
                .await
                .context("kg.query_active")?;
            let payload: Vec<Value> = triples
                .iter()
                .map(|t| {
                    json!({
                        "subject": t.subject,
                        "predicate": t.predicate,
                        "object": t.object,
                        "valid_from": t.valid_from.to_rfc3339(),
                        "valid_to": t.valid_to.as_ref().map(|d| d.to_rfc3339()),
                        "confidence": t.confidence,
                        "provenance": t.provenance,
                    })
                })
                .collect();
            Ok(json!({"subject": subject, "triples": payload}))
        }
        "memory_list" => {
            let palace = resolve_palace(state, &args, "memory_list")?;
            let handle = open_palace_handle(state, &palace)?;
            let room = args
                .get("room")
                .and_then(|v| v.as_str())
                .map(|s| parse_room(Some(s)));
            let tag = args
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let drawers = handle.list_drawers(room, tag, limit);
            let payload: Vec<Value> = drawers
                .iter()
                .map(|d| {
                    json!({
                        "drawer_id": d.id.to_string(),
                        "content": d.content,
                        "importance": d.importance,
                        "tags": d.tags,
                        "created_at": d.created_at.to_rfc3339(),
                    })
                })
                .collect();
            Ok(json!({"palace": palace, "drawers": payload}))
        }
        "memory_forget" => {
            let palace = resolve_palace(state, &args, "memory_forget")?;
            let drawer_id_str = args
                .get("drawer_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("memory_forget: missing 'drawer_id'"))?;
            let drawer_id = Uuid::parse_str(drawer_id_str)
                .map_err(|e| anyhow!("memory_forget: invalid drawer_id UUID: {e}"))?;
            let handle = open_palace_handle(state, &palace)?;
            handle.forget(drawer_id).await.context("forget")?;
            Ok(json!({"status": "deleted", "drawer_id": drawer_id_str, "palace": palace}))
        }
        "palace_info" => {
            let palace = resolve_palace(state, &args, "palace_info")?;
            let handle = open_palace_handle(state, &palace)?;
            let drawer_count = handle.list_drawers(None, None, usize::MAX).len();
            let data_dir = handle
                .data_dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string());
            Ok(json!({
                "id": handle.id.as_str(),
                "name": handle.id.as_str(),
                "drawer_count": drawer_count,
                "data_dir": data_dir,
            }))
        }
        "palace_compact" => {
            let palace = resolve_palace(state, &args, "palace_compact")?;
            let handle = open_palace_handle(state, &palace)?;
            // Use the live drawer table (sourced from SQLite at palace open) as
            // the authoritative valid-id set, then run the vector store's
            // synchronous compaction on a blocking thread.
            let valid_ids: std::collections::HashSet<Uuid> =
                handle.drawers.read().iter().map(|d| d.id).collect();
            let vector_store = handle.vector_store.clone();
            let res = tokio::task::spawn_blocking(move || vector_store.compact_orphans(&valid_ids))
                .await
                .context("join palace_compact")??;
            Ok(json!({
                "palace": palace,
                "total_checked": res.total_checked,
                "orphans_removed": res.orphans_removed,
                "index_size_before": res.index_size_before,
                "index_size_after": res.index_size_after,
            }))
        }
        "memory_recall_all" => {
            let query = args
                .get("q")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("memory_recall_all: missing 'q'"))?;
            let top_k = args.get("top_k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let deep = args.get("deep").and_then(|v| v.as_bool()).unwrap_or(false);

            // List every palace on disk and open a handle for each. Palaces
            // that fail to open are skipped with a warning so a single bad
            // namespace cannot fail the whole fan-out.
            let root = state.data_root.clone();
            let palaces = tokio::task::spawn_blocking(move || {
                trusty_memory_core::PalaceRegistry::list_palaces(&root)
            })
            .await
            .context("join list_palaces")??;

            let mut handles = Vec::with_capacity(palaces.len());
            for p in &palaces {
                match state.registry.open_palace(&state.data_root, &p.id) {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        tracing::warn!(palace = %p.id, "memory_recall_all: open failed: {e:#}")
                    }
                }
            }

            let embedder = state.embedder().await?;
            let erased: std::sync::Arc<dyn trusty_memory_core::embed::Embedder + Send + Sync> =
                embedder;
            let results = recall_across_palaces(&handles, &erased, query, top_k, deep)
                .await
                .context("recall_across_palaces")?;

            let payload: Vec<Value> = results
                .iter()
                .map(|r| {
                    json!({
                        "palace_id":  r.palace_id,
                        "drawer_id":  r.result.drawer.id.to_string(),
                        "content":    r.result.drawer.content,
                        "importance": r.result.drawer.importance,
                        "tags":       r.result.drawer.tags,
                        "score":      r.result.score,
                        "layer":      r.result.layer,
                    })
                })
                .collect();
            Ok(json!({ "query": query, "results": payload }))
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

/// Serialize `recall` results into a JSON shape the MCP client can render.
fn serialize_recall(
    palace: &str,
    query: &str,
    results: Vec<trusty_memory_core::retrieval::RecallResult>,
) -> Value {
    let payload: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "drawer_id": r.drawer.id.to_string(),
                "content":   r.drawer.content,
                "score":     r.score,
                "layer":     r.layer,
                "tags":      r.drawer.tags,
                "importance": r.drawer.importance,
            })
        })
        .collect();
    json!({
        "palace": palace,
        "query": query,
        "results": payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;

    fn test_state() -> AppState {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AppState::new(root)
    }

    /// Why: Issue #26 — when the server is started with `--palace`, the
    /// `tools/list` schema must drop `palace` from the `required` array for
    /// every tool that accepts it, so MCP clients know it's optional.
    /// Test: Build the schema both ways and check the required arrays.
    #[test]
    fn tool_definitions_drops_palace_required_when_default_set() {
        let with_default = tool_definitions_with(true);
        let without_default = tool_definitions_with(false);
        for (name, palace_required_when_no_default) in [
            ("memory_remember", true),
            ("memory_recall", true),
            ("memory_recall_deep", true),
            ("memory_list", true),
            ("memory_forget", true),
            ("palace_info", true),
            ("palace_compact", true),
            ("kg_assert", true),
            ("kg_query", true),
        ] {
            for (defs, has_default) in [(&with_default, true), (&without_default, false)] {
                let tools = defs["tools"].as_array().unwrap();
                let tool = tools.iter().find(|t| t["name"] == name).unwrap();
                let required: Vec<&str> = tool["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();
                let palace_required = required.contains(&"palace");
                let expected = palace_required_when_no_default && !has_default;
                assert_eq!(
                    palace_required, expected,
                    "tool={name} has_default={has_default} required={required:?}"
                );
            }
        }
    }

    #[test]
    fn tool_definitions_lists_all_tools() {
        let defs = tool_definitions();
        let tools = defs
            .get("tools")
            .and_then(|t| t.as_array())
            .expect("tools array");
        assert_eq!(tools.len(), 12);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        for expected in [
            "memory_remember",
            "memory_recall",
            "memory_recall_deep",
            "memory_list",
            "memory_forget",
            "palace_create",
            "palace_list",
            "palace_info",
            "palace_compact",
            "kg_assert",
            "kg_query",
            "memory_recall_all",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    /// Why: Confirm `palace_create` actually persists a palace under the
    /// configured data root and `palace_list` then sees it.
    #[tokio::test]
    async fn dispatch_palace_create_persists() {
        let state = test_state();
        let created = dispatch_tool(&state, "palace_create", json!({"name": "alpha"}))
            .await
            .expect("palace_create");
        assert_eq!(created["palace_id"], "alpha");

        let listed = dispatch_tool(&state, "palace_list", json!({}))
            .await
            .expect("palace_list");
        let ids = listed["palaces"].as_array().expect("palaces array");
        assert!(ids.iter().any(|v| v.as_str() == Some("alpha")));
    }

    /// Why: End-to-end confirmation that a remembered drawer is recallable
    /// through the MCP tool surface using the real embedder + retrieval path.
    #[tokio::test]
    async fn dispatch_remember_then_recall() {
        let state = test_state();
        let _ = dispatch_tool(&state, "palace_create", json!({"name": "beta"}))
            .await
            .expect("palace_create");

        let remembered = dispatch_tool(
            &state,
            "memory_remember",
            json!({
                "palace": "beta",
                "text": "Quokkas are the happiest marsupials in Australia",
                "room": "General",
                "tags": ["wildlife"],
            }),
        )
        .await
        .expect("memory_remember");
        assert!(remembered["drawer_id"].as_str().is_some());

        let recalled = dispatch_tool(
            &state,
            "memory_recall",
            json!({"palace": "beta", "query": "Quokkas marsupials Australia", "top_k": 5}),
        )
        .await
        .expect("memory_recall");
        let results = recalled["results"].as_array().expect("results");
        assert!(
            results
                .iter()
                .any(|r| r["content"].as_str().unwrap_or("").contains("Quokkas")),
            "expected to recall the Quokkas drawer; got {results:?}"
        );
    }

    /// Why: Confirm `kg_assert` writes a triple and `kg_query` returns it
    /// through the MCP tool surface.
    #[tokio::test]
    async fn dispatch_kg_assert_then_query() {
        let state = test_state();
        let _ = dispatch_tool(&state, "palace_create", json!({"name": "gamma"}))
            .await
            .expect("palace_create");

        let _ = dispatch_tool(
            &state,
            "kg_assert",
            json!({
                "palace": "gamma",
                "subject": "alice",
                "predicate": "works_at",
                "object": "Acme",
                "confidence": 0.9,
                "provenance": "test",
            }),
        )
        .await
        .expect("kg_assert");

        let queried = dispatch_tool(
            &state,
            "kg_query",
            json!({"palace": "gamma", "subject": "alice"}),
        )
        .await
        .expect("kg_query");
        let triples = queried["triples"].as_array().expect("triples array");
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0]["object"], "Acme");
        assert_eq!(triples[0]["predicate"], "works_at");
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_errors() {
        let state = test_state();
        let err = dispatch_tool(&state, "does_not_exist", json!({}))
            .await
            .expect_err("should error");
        assert!(err.to_string().contains("unknown tool"));
    }
}
