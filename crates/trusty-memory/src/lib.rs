//! MCP server (stdio + HTTP/SSE) for trusty-memory.
//!
//! Why: Claude Code and other MCP-aware clients integrate with trusty-memory
//! through the standardized Model Context Protocol; we expose memory + KG
//! tools so they can be called by name.
//! What: Provides `run_stdio` (JSON-RPC 2.0 over stdin/stdout) and `run_http`
//! (axum HTTP/SSE stub), plus an `AppState` that carries the shared
//! `PalaceRegistry`, on-disk data root, and a lazily-initialized embedder.
//! Test: `cargo test -p trusty-memory-mcp` validates handshake + dispatch.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, OnceCell};
use tracing::info;
use trusty_common::mcp::{error_codes, initialize_response, Request, Response};
use trusty_common::memory_core::embed::FastEmbedder;
use trusty_common::memory_core::store::ChatSessionStore;
use trusty_common::memory_core::PalaceRegistry;
use trusty_common::ChatProvider;

pub mod commands;
pub mod openrpc;
pub mod service;
pub mod tools;
pub mod web;

pub use service::MemoryMcpService;
pub use tools::MemoryMcpServer;

/// Live daemon events broadcast to connected SSE subscribers.
///
/// Why: The dashboard needs push-driven updates so palace creation, drawer
/// add/delete, dream cycles, and aggregate status changes are visible without
/// polling. A single broadcast channel fans out to every connected browser.
/// What: Tagged enum serialized as `{"type": "...", ...fields}` over SSE.
/// Test: `web::tests::sse_stream_emits_events` subscribes, triggers a
/// mutation, and asserts the frame arrives.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonEvent {
    PalaceCreated {
        id: String,
        name: String,
    },
    DrawerAdded {
        palace_id: String,
        drawer_count: usize,
    },
    DrawerDeleted {
        palace_id: String,
        drawer_count: usize,
    },
    DreamCompleted {
        palace_id: Option<String>,
        merged: usize,
        pruned: usize,
        compacted: usize,
        closets_updated: usize,
        duration_ms: u64,
    },
    StatusChanged {
        total_drawers: usize,
        total_vectors: usize,
        total_kg_triples: usize,
    },
}

/// Shared application state passed to every request handler.
///
/// Why: The stdio loop and HTTP server need the same handles to the registry,
/// data root, and embedder so MCP tools can perform real reads/writes against
/// the live trusty-memory core. The embedder is heavy (loads ONNX weights) so
/// we hold it behind a `OnceCell` and initialize lazily on first use.
/// What: `Clone`-able via `Arc` fields. The registry / data root are eager;
/// `embedder` is `Arc<OnceCell<Arc<FastEmbedder>>>` so concurrent first-use
/// races resolve to a single shared instance.
/// Test: `app_state_default_constructs` confirms construction without panic.
#[derive(Clone)]
pub struct AppState {
    pub version: String,
    pub registry: Arc<PalaceRegistry>,
    pub data_root: PathBuf,
    pub embedder: Arc<OnceCell<Arc<FastEmbedder>>>,
    /// Optional default palace applied to MCP tool calls when the caller
    /// omits the `palace` argument. Set via `trusty-memory serve --palace`.
    pub default_palace: Option<String>,
    /// Active chat provider selected at startup. `None` means no upstream is
    /// configured (no Ollama detected and no OpenRouter key) — callers must
    /// degrade gracefully (chat endpoint returns 412).
    pub chat_provider: Arc<OnceCell<Option<Arc<dyn ChatProvider>>>>,
    /// Per-palace chat-session stores, opened lazily so cold-start cost is
    /// paid only when chat-history endpoints are hit.
    pub session_stores: Arc<dashmap::DashMap<String, Arc<ChatSessionStore>>>,
    /// Broadcast sender for live `DaemonEvent` pushes to SSE subscribers.
    ///
    /// Why: Lets mutating handlers emit events that any connected dashboard
    /// receives instantly. Cap of 128 buffers transient slow readers; if a
    /// receiver lags it gets `RecvError::Lagged` and we emit a `lag` frame.
    pub events: Arc<broadcast::Sender<DaemonEvent>>,
    /// Instant the daemon started, used to compute `uptime_secs` on `/health`.
    ///
    /// Why (issue #35): `GET /health` reports how long the daemon has been
    /// up. Capturing a monotonic `Instant` at `AppState` construction lets the
    /// handler compute the elapsed seconds cheaply and without a clock-skew
    /// hazard.
    /// What: a wall-monotonic `Instant`; `AppState::new` stamps it at startup.
    /// Test: `health_endpoint_includes_resource_fields`.
    pub started_at: std::time::Instant,
    /// In-memory ring buffer of recent tracing log lines (issue #35).
    ///
    /// Why: the `GET /api/v1/logs/tail` endpoint serves the last N log lines
    /// so operators can inspect a running daemon without tailing a file. The
    /// buffer is shared between the tracing `LogBufferLayer` (writer) and the
    /// HTTP handler (reader).
    /// What: a cheap `Arc`-backed clone of the buffer the subscriber writes
    /// to. Defaults to an empty buffer for states that never install the
    /// layer (tests, the stdio path).
    /// Test: `logs_tail_returns_recent_lines`.
    pub log_buffer: trusty_common::log_buffer::LogBuffer,
    /// Most recent on-disk footprint of `data_root`, in bytes (issue #35).
    ///
    /// Why: `GET /health` reports `disk_bytes`. Walking the data directory on
    /// every health request would make a frequent health poll do unbounded
    /// I/O; a background task recomputes it every 10 s and stores it here so
    /// the handler reads it lock-free.
    /// What: an `AtomicU64` updated by the ticker spawned in `run_http_on`.
    /// `0` until the first walk completes.
    /// Test: `health_endpoint_includes_resource_fields`.
    pub disk_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Per-process RSS + CPU sampler, refreshed on each `/health` request
    /// (issue #35).
    ///
    /// Why: CPU usage is a delta between two `sysinfo` refreshes, so the
    /// sampler must persist between requests — hence the shared `Mutex`.
    /// What: a `tokio::sync::Mutex<SysMetrics>` so the async health handler
    /// can sample without blocking the runtime.
    /// Test: `health_endpoint_includes_resource_fields`.
    pub sys_metrics: Arc<tokio::sync::Mutex<trusty_common::sys_metrics::SysMetrics>>,
}

impl AppState {
    /// Construct an `AppState` rooted at the given on-disk data directory.
    ///
    /// Why: The CLI (`serve`) and integration tests need to point the MCP
    /// server at different roots — production at `dirs::data_dir`, tests at a
    /// `tempfile::tempdir()`.
    /// What: Builds an empty `PalaceRegistry`, captures the version, and
    /// allocates an empty `OnceCell` for the embedder. `default_palace` is
    /// `None`; use `with_default_palace` to set it.
    /// Test: `tools::tests::dispatch_palace_create_persists` constructs an
    /// AppState pointed at a tempdir and round-trips a palace through it.
    pub fn new(data_root: PathBuf) -> Self {
        let (events_tx, _) = broadcast::channel::<DaemonEvent>(128);
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            registry: Arc::new(PalaceRegistry::new()),
            data_root,
            embedder: Arc::new(OnceCell::new()),
            default_palace: None,
            chat_provider: Arc::new(OnceCell::new()),
            session_stores: Arc::new(dashmap::DashMap::new()),
            events: Arc::new(events_tx),
            started_at: std::time::Instant::now(),
            // Default to an empty buffer — `with_log_buffer` overrides this
            // when the daemon installs the `LogBufferLayer` (HTTP mode).
            log_buffer: trusty_common::log_buffer::LogBuffer::new(
                trusty_common::log_buffer::DEFAULT_LOG_CAPACITY,
            ),
            disk_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sys_metrics: Arc::new(tokio::sync::Mutex::new(
                trusty_common::sys_metrics::SysMetrics::new(),
            )),
        }
    }

    /// Builder-style: attach the daemon's shared [`LogBuffer`] so the
    /// `GET /api/v1/logs/tail` endpoint serves the same lines the tracing
    /// subscriber captures (issue #35).
    ///
    /// Why: `main` builds the buffer (via `init_tracing_with_buffer`) before
    /// constructing the `AppState`, then hands a clone here so the HTTP
    /// handler and the tracing layer observe the same ring.
    /// What: replaces the empty default buffer with the supplied one.
    /// Test: `logs_tail_returns_recent_lines`.
    #[must_use]
    pub fn with_log_buffer(mut self, buffer: trusty_common::log_buffer::LogBuffer) -> Self {
        self.log_buffer = buffer;
        self
    }

    /// Send a `DaemonEvent` to all connected SSE subscribers.
    ///
    /// Why: Mutating handlers call this after a successful write so the
    /// dashboard can update without polling. The send is best-effort —
    /// `broadcast::Sender::send` returns `Err` only when there are no live
    /// receivers, which is fine (no listeners == no work to do).
    /// What: Drops the result, so callers don't need to care whether anyone
    /// is listening.
    /// Test: `web::tests::sse_stream_receives_palace_created` confirms a
    /// subscriber observes the emitted event.
    pub fn emit(&self, event: DaemonEvent) {
        let _ = self.events.send(event);
    }

    /// Open (or return cached) the chat-session store for a palace.
    ///
    /// Why: Chat session persistence lives in a dedicated SQLite file under
    /// the palace's data dir (`chat_sessions.db`) so it doesn't intermingle
    /// with the KG's transactional load. The store is cheap to clone via
    /// `Arc` but the underlying r2d2 pool should be reused, so cache by id.
    /// What: Creates the palace data dir if missing, opens (or reuses) a
    /// `ChatSessionStore` and stashes an `Arc` in the DashMap.
    /// Test: Indirectly via the session HTTP handlers in `web::tests`.
    pub fn session_store(&self, palace_id: &str) -> Result<Arc<ChatSessionStore>> {
        if let Some(entry) = self.session_stores.get(palace_id) {
            return Ok(entry.clone());
        }
        let dir = self.data_root.join(palace_id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("create palace dir {}: {e}", dir.display()))?;
        let store = Arc::new(ChatSessionStore::open(&dir.join("chat_sessions.db"))?);
        self.session_stores
            .insert(palace_id.to_string(), store.clone());
        Ok(store)
    }

    /// Builder-style setter for the default palace name.
    ///
    /// Why: `serve --palace <name>` wants to bind every tool call to a
    /// project-scoped namespace without forcing every MCP request to repeat
    /// the palace argument.
    /// What: Returns `self` with `default_palace = Some(name)`.
    /// Test: `default_palace_used_when_arg_omitted` covers the resolution
    /// path; this setter is exercised there.
    pub fn with_default_palace(mut self, name: Option<String>) -> Self {
        self.default_palace = name;
        self
    }

    /// Resolve (or initialize) the shared embedder.
    ///
    /// Why: FastEmbedder load is expensive — we share one instance across all
    /// tool calls; the `OnceCell` ensures concurrent first-use races collapse
    /// to a single load.
    /// What: Returns `Arc<FastEmbedder>` on success. Errors propagate from the
    /// underlying ONNX load.
    /// Test: Indirectly via `dispatch_remember_then_recall`.
    /// Resolve the active chat provider, auto-detecting on first call.
    ///
    /// Why: Provider selection depends on filesystem-loaded config plus a
    /// network probe (Ollama liveness), so it must be lazily initialised at
    /// runtime. Caching the choice in a `OnceCell` keeps it stable across
    /// concurrent requests without re-probing on every chat call.
    /// What: On first use loads `~/.trusty-memory/config.toml`, prefers an
    /// auto-detected Ollama instance (when `local_model.enabled`), and falls
    /// back to OpenRouter when an API key is set. Returns `Ok(None)` when
    /// neither is available so the caller can emit a 412.
    /// Test: `web::tests::providers_endpoint_returns_payload` covers the
    /// detection path indirectly through `/api/v1/chat/providers`.
    pub async fn chat_provider(&self) -> Option<Arc<dyn ChatProvider>> {
        self.chat_provider
            .get_or_init(|| async {
                let cfg = crate::web::load_user_config().unwrap_or_default();
                if cfg.local_model.enabled {
                    if let Some(mut p) =
                        trusty_common::auto_detect_local_provider(&cfg.local_model.base_url).await
                    {
                        // auto_detect returns an empty model id; callers must
                        // set the configured model name themselves.
                        p.model = cfg.local_model.model.clone();
                        return Some(Arc::new(p) as Arc<dyn ChatProvider>);
                    }
                }
                if !cfg.openrouter_api_key.is_empty() {
                    return Some(Arc::new(trusty_common::OpenRouterProvider::new(
                        cfg.openrouter_api_key,
                        cfg.openrouter_model,
                    )) as Arc<dyn ChatProvider>);
                }
                None
            })
            .await
            .clone()
    }

    pub async fn embedder(&self) -> Result<Arc<FastEmbedder>> {
        let cell = self.embedder.clone();
        let embedder = cell
            .get_or_try_init(|| async {
                let e = FastEmbedder::new().await?;
                Ok::<Arc<FastEmbedder>, anyhow::Error>(Arc::new(e))
            })
            .await?
            .clone();
        Ok(embedder)
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("version", &self.version)
            .field("data_root", &self.data_root)
            .field("registry_len", &self.registry.len())
            .finish()
    }
}

/// Handle a single MCP JSON-RPC message and produce its response.
///
/// Why: Pulled out of the stdio loop so unit tests can drive every method
/// without touching real stdin/stdout.
/// What: Routes `initialize`, `tools/list`, `tools/call`, `ping`, and the
/// `notifications/initialized` notification (which returns `Value::Null`).
/// Test: See unit tests below — initialize/list/call all return expected
/// JSON-RPC envelopes; notifications return `Null` (no response written).
pub async fn handle_message(state: &AppState, msg: Value) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "initialize" => {
            let extra = state
                .default_palace
                .as_ref()
                .map(|dp| json!({ "default_palace": dp }));
            let result = initialize_response("trusty-memory", &state.version, extra);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            })
        }
        // Notifications must NOT receive a response.
        "notifications/initialized" | "notifications/cancelled" => Value::Null,
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": tools::tool_definitions_with(state.default_palace.is_some())
        }),
        // OpenRPC 1.3.2 discovery — see `openrpc.rs`. Returns the full
        // service description so orchestrators (open-mpm, etc.) can
        // introspect every tool and its required `memory.read`/`memory.write`
        // scope without bespoke per-server adapters.
        "rpc.discover" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": openrpc::build_discover_response(
                &state.version,
                state.default_palace.is_some(),
            ),
        }),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or_default();
            let tool_name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = params.get("arguments").cloned().unwrap_or_default();
            match tools::dispatch_tool(state, &tool_name, args).await {
                Ok(content) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": content.to_string()}]
                    }
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": e.to_string()}
                }),
            }
        }
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {method}")
            }
        }),
    }
}

/// Run the MCP stdio JSON-RPC 2.0 server loop.
///
/// Why: Claude Code launches MCP servers as child processes and speaks
/// JSON-RPC over stdin/stdout — this is the primary integration path.
/// What: Delegates to `trusty_mcp_core::run_stdio_loop`, adapting each
/// shared `Request` back into the JSON `Value` shape `handle_message`
/// expects, and translating the returned `Value` into a `Response`.
/// Notifications (where `handle_message` returns `Value::Null`) become
/// suppressed responses so the loop emits nothing on the wire.
/// Test: `handle_message` covers protocol behaviour in unit tests.
pub async fn run_stdio(state: AppState) -> Result<()> {
    info!("trusty-memory MCP stdio server starting");
    let state = Arc::new(state);
    trusty_common::mcp::run_stdio_loop(move |req: Request| {
        let state = state.clone();
        async move {
            // Re-serialise the Request into the JSON shape handle_message expects.
            // (handle_message predates the shared types and reads loose Values.)
            let msg = json!({
                "jsonrpc": req.jsonrpc.unwrap_or_else(|| "2.0".to_string()),
                "id": req.id.clone().unwrap_or(Value::Null),
                "method": req.method,
                "params": req.params.unwrap_or(Value::Null),
            });
            let resp_value = handle_message(&state, msg).await;
            // handle_message returns Value::Null for notifications.
            if resp_value.is_null() {
                return Response::suppressed();
            }
            // Otherwise it returns the full JSON-RPC envelope as a Value;
            // re-encode into the shared Response struct so the loop can serialise.
            let id = resp_value.get("id").cloned();
            if let Some(result) = resp_value.get("result").cloned() {
                Response::ok(id, result)
            } else if let Some(err) = resp_value.get("error") {
                let code =
                    err.get("code")
                        .and_then(|c| c.as_i64())
                        .unwrap_or(error_codes::INTERNAL_ERROR as i64) as i32;
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("internal error")
                    .to_string();
                Response::err(id, code, message)
            } else {
                Response::err(
                    id,
                    error_codes::INTERNAL_ERROR,
                    "malformed handler response",
                )
            }
        }
    })
    .await
}

/// Run the optional HTTP/SSE + web admin server.
///
/// Why: A long-running daemon mode lets non-stdio clients (browsers, curl,
/// future remote agents) hit `/health`, the `/api/v1/*` REST surface, and the
/// embedded admin SPA.
/// What: axum router built from `web::router()` plus a `/sse` stub for the
/// existing MCP-over-SSE clients. Caller provides a pre-bound listener so
/// port auto-detection lives at the call site.
/// Test: `cargo test -p trusty-memory-mcp web::tests` exercises the router
/// shape; manual: `curl http://127.0.0.1:<port>/health` returns `ok`.
pub async fn run_http_on(state: AppState, listener: tokio::net::TcpListener) -> Result<()> {
    use axum::routing::get;

    // Issue #35: recompute the `data_root` disk footprint every 10 s on a
    // background task so `GET /health` reports `disk_bytes` without doing a
    // recursive directory walk on the request path.
    spawn_disk_size_ticker(state.clone());

    let app = web::router()
        .route("/sse", get(sse_handler))
        .with_state(state);

    let local = listener.local_addr().ok();
    if let Some(a) = local {
        info!("HTTP server listening on http://{a}");
        eprintln!("HTTP server listening on http://{a}");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

/// Convenience: bind `addr` and serve via [`run_http_on`].
pub async fn run_http(state: AppState, addr: std::net::SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    run_http_on(state, listener).await
}

/// Spawn a background ticker that recomputes the `data_root` disk footprint
/// every 10 seconds and stores it in `state.disk_bytes` (issue #35).
///
/// Why: `GET /health` reports `disk_bytes`. Walking the data directory on
/// every health request would turn a frequent health poll into unbounded
/// recursive I/O. Computing it off the request path on a fixed cadence keeps
/// `/health` cheap and bounds the staleness to ~10 s — fine for an
/// at-a-glance footprint figure.
/// What: spawns a detached tokio task. `AppState` is cheap to `Clone` (all
/// `Arc` fields), so the task holds a full clone; the daemon process lives
/// for the lifetime of the server anyway, so no `Weak` downgrade is needed.
/// Each tick runs the blocking directory walk on `spawn_blocking` so it never
/// stalls the async runtime, then stores the byte total atomically.
/// Test: `health_endpoint_includes_resource_fields` asserts the field shape;
/// the ticker cadence is not unit-tested (timing-dependent).
fn spawn_disk_size_ticker(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            let dir = state.data_root.clone();
            // The directory walk is blocking filesystem I/O — run it on the
            // blocking pool so it never parks an async worker thread.
            let bytes = tokio::task::spawn_blocking(move || {
                trusty_common::sys_metrics::dir_size_bytes(&dir)
            })
            .await
            .unwrap_or(0);
            state
                .disk_bytes
                .store(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

/// Live SSE event stream — pushes `DaemonEvent` frames to dashboard clients.
///
/// Why: The dashboard subscribes once and reacts to live pushes (palace
/// created, drawer added/deleted, dream completed, status changed) instead of
/// polling `/api/v1/*` endpoints.
/// What: Subscribes to `state.events`, emits an initial `connected` frame,
/// then forwards every `DaemonEvent` as `data: <json>\n\n`. Lagged
/// subscribers receive a `lag` frame indicating skipped events; channel
/// closure ends the stream.
/// Test: `web::tests::sse_stream_emits_palace_created` (covers subscribe +
/// emit + receive); manual: `curl -N http://.../sse`.
pub(crate) async fn sse_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl axum::response::IntoResponse {
    use futures::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    let rx = state.events.subscribe();
    let initial = futures::stream::once(async {
        Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::from(
            "data: {\"type\":\"connected\"}\n\n",
        ))
    });
    let events = BroadcastStream::new(rx).map(|res| {
        let frame = match res {
            Ok(event) => match serde_json::to_string(&event) {
                Ok(json) => format!("data: {json}\n\n"),
                Err(e) => format!("data: {{\"type\":\"error\",\"message\":\"{e}\"}}\n\n"),
            },
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                format!("data: {{\"type\":\"lag\",\"skipped\":{n}}}\n\n")
            }
        };
        Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::from(frame))
    });
    let stream = initial.chain(events);

    axum::response::Response::builder()
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(axum::body::Body::from_stream(stream))
        .expect("valid SSE response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        // Leak the tempdir so it lives for the test process; tests are short.
        std::mem::forget(tmp);
        AppState::new(root)
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version_and_capabilities() {
        let state = test_state();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        });
        let resp = handle_message(&state, req).await;
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "trusty-memory");
    }

    #[tokio::test]
    async fn initialized_notification_returns_null() {
        let state = test_state();
        let req = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let resp = handle_message(&state, req).await;
        assert!(resp.is_null());
    }

    #[tokio::test]
    async fn tools_list_returns_all_tools() {
        let state = test_state();
        let req = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let resp = handle_message(&state, req).await;
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 12);
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let state = test_state();
        let req = json!({"jsonrpc": "2.0", "id": 4, "method": "wat"});
        let resp = handle_message(&state, req).await;
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn ping_returns_empty_result() {
        let state = test_state();
        let req = json!({"jsonrpc": "2.0", "id": 5, "method": "ping"});
        let resp = handle_message(&state, req).await;
        assert!(resp["result"].is_object());
    }

    #[tokio::test]
    async fn app_state_default_constructs() {
        let s = test_state();
        assert!(!s.version.is_empty());
        assert!(s.registry.is_empty());
        assert!(s.default_palace.is_none());
    }

    /// Why: Issue #26 — when `serve --palace <name>` is set, the MCP server
    /// must (a) report the default in the `initialize` `serverInfo`, (b)
    /// drop `palace` from the required schema in `tools/list`, and (c) let
    /// `tools/call` use the default when the caller omits `palace`.
    /// Test: Construct an AppState with a default palace, create that palace
    /// on disk via the registry, then call `memory_remember` without a
    /// `palace` argument and confirm it resolves to the default.
    #[tokio::test]
    async fn default_palace_used_when_arg_omitted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        // Pre-create the default palace so remember has somewhere to land.
        let registry = trusty_common::memory_core::PalaceRegistry::new();
        let palace = trusty_common::memory_core::Palace {
            id: trusty_common::memory_core::PalaceId::new("default-pal"),
            name: "default-pal".to_string(),
            description: None,
            created_at: chrono::Utc::now(),
            data_dir: root.join("default-pal"),
        };
        registry
            .create_palace(&root, palace)
            .expect("create_palace");

        let state = AppState::new(root).with_default_palace(Some("default-pal".to_string()));

        // (a) initialize advertises the default.
        let init = handle_message(
            &state,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        )
        .await;
        assert_eq!(
            init["result"]["serverInfo"]["default_palace"], "default-pal",
            "initialize must echo default_palace in serverInfo"
        );

        // (b) tools/list drops `palace` from required when default is set.
        let list = handle_message(
            &state,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        let tools = list["result"]["tools"].as_array().expect("tools array");
        let remember = tools
            .iter()
            .find(|t| t["name"] == "memory_remember")
            .expect("memory_remember tool");
        let required: Vec<&str> = remember["inputSchema"]["required"]
            .as_array()
            .expect("required array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !required.contains(&"palace"),
            "palace must not be required when default is configured; got {required:?}"
        );
        assert!(required.contains(&"text"));

        // (c) tools/call resolves the default when arg is omitted.
        let call = handle_message(
            &state,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "memory_remember",
                    "arguments": {"text": "default-palace test memory"},
                },
            }),
        )
        .await;
        // Successful dispatch returns `result.content[0].text` JSON.
        let text = call["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected success result, got {call}"));
        let parsed: Value = serde_json::from_str(text).expect("parse content json");
        assert_eq!(parsed["palace"], "default-pal");
        assert_eq!(parsed["status"], "stored");
        assert!(parsed["drawer_id"].as_str().is_some());
    }

    /// Why: When no default is set, `tools/call` for a palace-bound tool
    /// without a `palace` argument should error helpfully rather than panic.
    #[tokio::test]
    async fn missing_palace_without_default_errors() {
        let state = test_state();
        let resp = handle_message(
            &state,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "memory_recall",
                    "arguments": {"query": "anything"},
                },
            }),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32603);
        let msg = resp["error"]["message"].as_str().unwrap_or("");
        assert!(
            msg.contains("missing 'palace'"),
            "expected helpful error, got: {msg}"
        );
    }

    /// Why: initialize without a default palace must omit `default_palace`
    /// from `serverInfo` so clients can detect the unbound mode.
    #[tokio::test]
    async fn initialize_without_default_palace_omits_field() {
        let state = test_state();
        let init = handle_message(
            &state,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        )
        .await;
        assert!(init["result"]["serverInfo"]["default_palace"].is_null());
    }
}
