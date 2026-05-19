//! TM (Tmux Manager) tools — let the LLM query and control tmux sessions.
//!
//! Why: Without these tools, the ctrl/PM persona can only *describe* what
//! tmux/TM could do — it cannot actually inspect or manipulate live sessions.
//! Wiring TM through the tool registry means utterances like "what sessions
//! are running?", "pause the frontend session", or "what is api-work doing?"
//! flow through real `TmManager` calls rather than hallucination.
//! What: Nine `ToolExecutor` implementations covering session lifecycle
//! (`tm_new_session`, `tm_kill_session`), control (`tm_pause_session`,
//! `tm_resume_session`, `tm_send_message`), inspection (`tm_list_sessions`,
//! `tm_list_projects`, `tm_capture_pane`), and reconciliation
//! (`tm_reconcile`). Each tool holds a shared `Arc<Mutex<TmManager>>` so the
//! REPL's single TM instance is the source of truth across tools.
//! Test: Unit tests register the tools into a `ToolRegistry` and assert that
//! `register_tm_tools` adds all nine names. Behavioral tests are gated by a
//! live tmux server and live in `tests/`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::tm::manager::TmManager;
use crate::tm::project::AdapterType;
use crate::tools::ToolRegistry;
use crate::tools::traits::{ToolExecutor, ToolResult};

// ============================================================================
// Helpers
// ============================================================================

/// Pull a required string field from a JSON `Value`, returning a tool-friendly
/// error when missing or empty.
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolResult> {
    match args.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(ToolResult::err(format!("missing param: {key}"))),
    }
}

// ============================================================================
// tm_list_sessions
// ============================================================================

/// List all TM-managed tmux sessions with project/adapter/status info.
pub struct TmListSessionsTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmListSessionsTool {
    fn name(&self) -> &str {
        "tm_list_sessions"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_list_sessions",
                "description": "List all tmux sessions managed by TM, with adapter type, status, project path, and last-active time. Use this to answer questions like 'what sessions are running?' or 'what is X working on?'.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        let mgr = self.tm.lock().await;
        match mgr.list_sessions().await {
            Ok(sessions) => {
                let rows: Vec<Value> = sessions
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "tmux_session_name": s.tmux_session_name,
                            "adapter": s.adapter_type.as_str(),
                            "status": s.status.to_string(),
                            "project_path": s.project_path.to_string_lossy(),
                            "project_name": s.project_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("?"),
                            "last_active": s.last_active_ago(),
                        })
                    })
                    .collect();
                ToolResult::ok(serde_json::to_string_pretty(&rows).unwrap_or_default())
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_list_projects
// ============================================================================

/// List all TM projects with framework and session count.
pub struct TmListProjectsTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmListProjectsTool {
    fn name(&self) -> &str {
        "tm_list_projects"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_list_projects",
                "description": "List all TM projects with detected language/framework and number of associated tmux sessions.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        let mgr = self.tm.lock().await;
        match mgr.list_projects().await {
            Ok(projects) => {
                let rows: Vec<Value> = projects
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.name,
                            "path": p.path.to_string_lossy(),
                            "framework": p.framework.display(),
                            "session_count": p.session_ids.len(),
                        })
                    })
                    .collect();
                ToolResult::ok(serde_json::to_string_pretty(&rows).unwrap_or_default())
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_new_session
// ============================================================================

/// Create a new tmux session for a project.
pub struct TmNewSessionTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmNewSessionTool {
    fn name(&self) -> &str {
        "tm_new_session"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_new_session",
                "description": "Create a new tmux session for a project directory. Optionally specify which AI harness adapter to use.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_name": {
                            "type": "string",
                            "description": "Friendly session name (will be uniquified if it collides with existing tmux sessions)."
                        },
                        "project_path": {
                            "type": "string",
                            "description": "Absolute path to the project root directory."
                        },
                        "adapter": {
                            "type": "string",
                            "description": "Optional adapter type: claude-mpm, claude-code, codex, augment, gemini, shell. Defaults to shell."
                        }
                    },
                    "required": ["session_name", "project_path"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let session_name = match required_str(&args, "session_name") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let project_path = match required_str(&args, "project_path") {
            Ok(s) => PathBuf::from(s),
            Err(e) => return e,
        };
        let adapter = args
            .get("adapter")
            .and_then(Value::as_str)
            .map(AdapterType::from_id);

        let mgr = self.tm.lock().await;
        match mgr.new_session(&session_name, &project_path, adapter).await {
            Ok(session) => ToolResult::ok(
                serde_json::to_string_pretty(&json!({
                    "name": session.name,
                    "tmux_session_name": session.tmux_session_name,
                    "adapter": session.adapter_type.as_str(),
                    "status": session.status.to_string(),
                    "project_path": session.project_path.to_string_lossy(),
                    "id": session.id,
                }))
                .unwrap_or_default(),
            ),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_pause_session
// ============================================================================

/// Pause a tmux session (sends adapter pause command).
pub struct TmPauseSessionTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmPauseSessionTool {
    fn name(&self) -> &str {
        "tm_pause_session"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_pause_session",
                "description": "Pause a tmux session by sending the adapter's pause command (e.g., /mpm-session-pause for claude-mpm). Marks the session as Paused.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_name": {
                            "type": "string",
                            "description": "The session name (or id) to pause."
                        }
                    },
                    "required": ["session_name"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let session_name = match required_str(&args, "session_name") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let mgr = self.tm.lock().await;
        match mgr.pause_session(&session_name).await {
            Ok(()) => ToolResult::ok(format!("Paused session '{session_name}'.")),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_resume_session
// ============================================================================

/// Resume a paused tmux session.
pub struct TmResumeSessionTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmResumeSessionTool {
    fn name(&self) -> &str {
        "tm_resume_session"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_resume_session",
                "description": "Resume a previously-paused tmux session by sending the adapter's resume command. Marks the session as Running.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_name": {
                            "type": "string",
                            "description": "The session name (or id) to resume."
                        }
                    },
                    "required": ["session_name"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let session_name = match required_str(&args, "session_name") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let mgr = self.tm.lock().await;
        match mgr.resume_session(&session_name).await {
            Ok(()) => ToolResult::ok(format!("Resumed session '{session_name}'.")),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_send_message
// ============================================================================

/// Send a message to a session's harness (e.g., a prompt to claude-mpm).
pub struct TmSendMessageTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmSendMessageTool {
    fn name(&self) -> &str {
        "tm_send_message"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_send_message",
                "description": "Send a message (prompt or command) to the AI harness running in a tmux session. The message is formatted by the session's adapter before being typed into the pane.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_name": {
                            "type": "string",
                            "description": "The session name (or id) to receive the message."
                        },
                        "message": {
                            "type": "string",
                            "description": "The message text to send."
                        }
                    },
                    "required": ["session_name", "message"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let session_name = match required_str(&args, "session_name") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let message = match required_str(&args, "message") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let mgr = self.tm.lock().await;
        match mgr.send_message(&session_name, &message).await {
            Ok(()) => ToolResult::ok(format!(
                "Sent {} chars to session '{session_name}'.",
                message.len()
            )),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_capture_pane
// ============================================================================

/// Capture pane output from a session.
pub struct TmCapturePaneTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmCapturePaneTool {
    fn name(&self) -> &str {
        "tm_capture_pane"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_capture_pane",
                "description": "Capture the most recent lines of pane output from a tmux session. Use this to check what a session is currently doing or showing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_name": {
                            "type": "string",
                            "description": "The session name (or id) whose pane to capture."
                        },
                        "lines": {
                            "type": "number",
                            "description": "Number of trailing lines to capture (default 50)."
                        }
                    },
                    "required": ["session_name"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let session_name = match required_str(&args, "session_name") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let lines = args
            .get("lines")
            .and_then(Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32)
            .unwrap_or(50);
        let mgr = self.tm.lock().await;
        match mgr.capture_pane(&session_name, lines).await {
            Ok(output) => ToolResult::ok(output),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_reconcile
// ============================================================================

/// Reconcile registry with live tmux sessions (discovers new, marks orphaned).
pub struct TmReconcileTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmReconcileTool {
    fn name(&self) -> &str {
        "tm_reconcile"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_reconcile",
                "description": "Reconcile TM's registry with live tmux. Discovers new tmux sessions not yet tracked, and marks vanished registry sessions as Orphaned. Returns counts of added and orphaned sessions.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> ToolResult {
        let mgr = self.tm.lock().await;
        match mgr.reconcile().await {
            Ok(report) => {
                let added: Vec<&str> = report.added.iter().map(|s| s.name.as_str()).collect();
                let payload = json!({
                    "added": added,
                    "orphaned": report.orphaned,
                    "added_count": report.added.len(),
                    "orphaned_count": report.orphaned.len(),
                });
                ToolResult::ok(serde_json::to_string_pretty(&payload).unwrap_or_default())
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// tm_kill_session
// ============================================================================

/// Kill a tmux session.
pub struct TmKillSessionTool {
    pub tm: Arc<Mutex<TmManager>>,
}

#[async_trait]
impl ToolExecutor for TmKillSessionTool {
    fn name(&self) -> &str {
        "tm_kill_session"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "tm_kill_session",
                "description": "Kill a tmux session and mark it Stopped. This is destructive — confirm with the user before invoking unless the user explicitly asked.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_name": {
                            "type": "string",
                            "description": "The session name (or id) to kill."
                        }
                    },
                    "required": ["session_name"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let session_name = match required_str(&args, "session_name") {
            Ok(s) => s.to_string(),
            Err(e) => return e,
        };
        let mgr = self.tm.lock().await;
        match mgr.kill_session(&session_name).await {
            Ok(()) => ToolResult::ok(format!("Killed session '{session_name}'.")),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ============================================================================
// Registration helpers
// ============================================================================

/// Register all TM tools into a tool registry, sharing one `TmManager` instance.
///
/// Why: The REPL owns the canonical `TmManager`; sharing it through the
/// registry means every `tm_*` call mutates the same in-memory state and
/// idle-monitor view.
/// What: Inserts all nine `Tm*Tool` structs, each with a clone of `tm`.
/// Test: `register_tm_tools_adds_all_nine` below.
pub fn register_tm_tools(registry: &mut ToolRegistry, tm: Arc<Mutex<TmManager>>) {
    registry.register(Arc::new(TmListSessionsTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmListProjectsTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmNewSessionTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmPauseSessionTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmResumeSessionTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmSendMessageTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmCapturePaneTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmReconcileTool {
        tm: Arc::clone(&tm),
    }));
    registry.register(Arc::new(TmKillSessionTool { tm }));
}

/// Try to register TM tools by constructing a fresh `TmManager` rooted at
/// `state_dir`. Used by call sites that don't already hold a shared instance
/// (e.g., the free `run_pm_task_with_history`).
///
/// Why: Without a shared `Arc<Mutex<TmManager>>` threaded through every
/// dispatch path, the simplest way to give the LLM TM access is to construct
/// a manager on-demand. JSON registry I/O is cheap enough that doing so per
/// turn is acceptable; multiple managers across a single tmux server agree
/// because they all read/write the same `tm_sessions.json`.
/// What: Tries `TmManager::new(state_dir)`. On success, registers all nine
/// tools. On failure (typically: tmux binary missing), logs at debug and
/// returns without modifying the registry.
/// Test: `register_tm_tools_for_state_dir_skips_when_tmux_missing` —
/// constructing a TmManager without tmux fails fast and the registry is
/// unchanged.
#[allow(dead_code)]
pub fn register_tm_tools_for_state_dir(registry: &mut ToolRegistry, state_dir: &std::path::Path) {
    // Best-effort: ensure the directory exists so registry I/O won't fail
    // simply because no one has opened TM yet.
    let _ = std::fs::create_dir_all(state_dir);
    match TmManager::new(state_dir) {
        Ok(mgr) => {
            let arc = Arc::new(Mutex::new(mgr));
            register_tm_tools(registry, arc);
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                state_dir = %state_dir.display(),
                "register_tm_tools_for_state_dir: TmManager construction failed (tmux likely unavailable); skipping TM tool registration"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::TmuxOrchestrator;

    fn requires_tmux_or_skip() -> bool {
        TmuxOrchestrator::is_available()
    }

    #[test]
    fn register_tm_tools_adds_all_nine() {
        if !requires_tmux_or_skip() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let mgr = TmManager::new(dir.path()).unwrap();
        let arc = Arc::new(Mutex::new(mgr));
        let mut registry = ToolRegistry::new();
        register_tm_tools(&mut registry, arc);

        for name in [
            "tm_list_sessions",
            "tm_list_projects",
            "tm_new_session",
            "tm_pause_session",
            "tm_resume_session",
            "tm_send_message",
            "tm_capture_pane",
            "tm_reconcile",
            "tm_kill_session",
        ] {
            assert!(registry.contains(name), "missing TM tool: {name}");
        }
    }

    #[tokio::test]
    async fn missing_session_name_is_recoverable_error() {
        if !requires_tmux_or_skip() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let mgr = TmManager::new(dir.path()).unwrap();
        let arc = Arc::new(Mutex::new(mgr));

        let tool = TmPauseSessionTool { tm: arc };
        let r = tool.execute(json!({})).await;
        assert!(r.is_error());
        assert!(r.content().contains("session_name"));
    }

    #[tokio::test]
    async fn capture_pane_default_lines_when_omitted() {
        if !requires_tmux_or_skip() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let mgr = TmManager::new(dir.path()).unwrap();
        let arc = Arc::new(Mutex::new(mgr));

        let tool = TmCapturePaneTool { tm: arc };
        // Session doesn't exist → adapter call errors. We just want to
        // confirm the args parser doesn't panic on missing `lines`.
        let r = tool
            .execute(json!({"session_name": "does-not-exist"}))
            .await;
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn list_sessions_no_params_required() {
        if !requires_tmux_or_skip() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let mgr = TmManager::new(dir.path()).unwrap();
        let arc = Arc::new(Mutex::new(mgr));

        let tool = TmListSessionsTool { tm: arc };
        let r = tool.execute(json!({})).await;
        // Empty registry → empty array, success.
        assert!(!r.is_error(), "unexpected error: {}", r.content());
    }

    #[test]
    fn schemas_declare_required_params() {
        // No tmux required — schema is static.
        let dir = tempfile::TempDir::new().unwrap();
        if !requires_tmux_or_skip() {
            // Still skip if we can't construct TmManager.
            return;
        }
        let mgr = TmManager::new(dir.path()).unwrap();
        let arc = Arc::new(Mutex::new(mgr));

        let pause = TmPauseSessionTool {
            tm: Arc::clone(&arc),
        };
        let schema = pause.schema();
        assert_eq!(schema["function"]["name"], "tm_pause_session");
        let required = schema["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "session_name");

        let send = TmSendMessageTool {
            tm: Arc::clone(&arc),
        };
        let schema = send.schema();
        let required = schema["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|v| v == "session_name"));
        assert!(required.iter().any(|v| v == "message"));
    }
}
