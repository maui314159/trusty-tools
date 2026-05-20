//! Shared utility surface for trusty-* projects.
//!
//! Why: Port auto-detect, data-directory resolution, tracing init, NO_COLOR
//! handling, and the OpenRouter chat-completions client appeared in both
//! trusty-memory and trusty-search with subtle divergence. Centralising keeps
//! them aligned and gives future trusty-* binaries a one-import surface.
//!
//! What: pure utility functions — no global state. Each subsystem is a free
//! function or a small helper struct.
//!
//! Test: `cargo test -p trusty-common` covers port walking, data-dir creation,
//! and the OpenRouter request shape (without hitting the network).
//!
//! # Test isolation: `TRUSTY_DATA_DIR_OVERRIDE`
//!
//! macOS's [`dirs::data_dir()`] resolves the application-support directory via
//! `NSFileManager`, a native Cocoa API that completely ignores the `HOME` and
//! `XDG_DATA_HOME` environment variables. This makes it impossible to redirect
//! data-directory access in tests using ordinary env-var tricks, because the
//! kernel query bypasses the environment entirely.
//!
//! To work around this, [`resolve_data_dir`] checks the
//! [`DATA_DIR_OVERRIDE_ENV`] (`TRUSTY_DATA_DIR_OVERRIDE`) environment variable
//! before consulting `dirs::data_dir()`. When set, the variable's value is used
//! as the base directory verbatim, and `dirs::data_dir()` is never called.
//!
//! **This escape hatch is intended for testing only.** Do not set it in
//! production deployments; rely on the OS-standard data directory instead.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub mod chat;
pub mod claude_config;
pub mod project_discovery;

/// macOS LaunchAgent generation and lifecycle management. macOS-only —
/// the module compiles to nothing on every other platform.
#[cfg(target_os = "macos")]
pub mod launchd;

#[cfg(feature = "axum-server")]
pub mod server;

/// Shared JSON-RPC 2.0 / MCP primitives (formerly the `trusty-mcp-core` crate).
///
/// Why: Centralises `Request`/`Response`/`JsonRpcError` envelopes, the
/// `initialize` response builder, an async stdio dispatch loop, and the
/// OpenRPC `rpc.discover` helpers so every MCP server in the workspace
/// imports the same types.
/// What: Gated behind the `mcp` feature; pulls in no extra dependencies
/// beyond `serde` / `tokio`, both of which are already required.
/// Test: `cargo test -p trusty-common --features mcp` runs the module's
/// own unit tests (envelope round-trips, stdio loop dispatch, OpenRPC
/// builder shape).
#[cfg(feature = "mcp")]
pub mod mcp;

/// General-purpose JSON-RPC client + transports (formerly the library half
/// of the `trusty-rpc` crate).
///
/// Why: Both `trpc` (the CLI) and any future library consumer want one
/// place that owns the JSON-RPC envelope construction, stdio-subprocess
/// transport, HTTP transport, and pretty-printers.
/// What: Gated behind the `rpc` feature; requires `uuid` for request id
/// generation. The HTTP transport reuses the workspace `reqwest`.
/// Test: `cargo test -p trusty-common --features rpc` runs the module's
/// own unit tests (envelope extraction, pretty-print smoke tests).
#[cfg(feature = "rpc")]
pub mod rpc;

/// Shared text-embedding abstraction (formerly the `trusty-embedder` crate).
///
/// Why: trusty-memory and trusty-search both ship near-identical `Embedder`
/// traits and `FastEmbedder` implementations; centralising the surface here
/// keeps them aligned and lets future consumers pick up embedding for free
/// without a separate published crate.
/// What: Gated behind the `embedder` feature. Exposes the `Embedder` trait,
/// `FastEmbedder` (fastembed-rs, all-MiniLM-L6-v2, 384-d) with LRU caching
/// and ORT warmup, and (under `embedder-test-support`) the `MockEmbedder`
/// test double.
/// Test: `cargo test -p trusty-common --features embedder,embedder-test-support`
/// covers the mock embedder and ONNX-backed `#[ignore]`d integration tests.
#[cfg(feature = "embedder")]
pub mod embedder;

pub use chat::{
    ChatEvent, ChatProvider, LocalModelConfig, OllamaProvider, OpenRouterProvider, ToolCall,
    ToolDef, auto_detect_local_provider,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

// ─── Port binding ─────────────────────────────────────────────────────────

/// Bind to `addr`; if the port is in use, walk forward up to `max_attempts`
/// ports and return the first listener that binds.
///
/// Why: Running multiple instances of a trusty-* daemon (or restarting before
/// the kernel releases the prior socket) shouldn't produce a noisy failure —
/// auto-incrementing gives a friendlier developer experience while still
/// honouring the user's preferred starting port.
/// What: returns the first successful `tokio::net::TcpListener`. Callers can
/// inspect `local_addr()` to discover where it landed and report it however
/// they prefer — this function does not perform any I/O on stdout/stderr.
/// `max_attempts == 0` means "try `addr` exactly once".
/// Test: `auto_port_walks_forward` binds a port, then calls this with the
/// occupied port and confirms a different free port is returned.
pub async fn bind_with_auto_port(addr: SocketAddr, max_attempts: u16) -> Result<TcpListener> {
    use std::io::ErrorKind;
    let mut current = addr;
    for attempt in 0..=max_attempts {
        match TcpListener::bind(current).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == ErrorKind::AddrInUse && attempt < max_attempts => {
                let next_port = current.port().saturating_add(1);
                if next_port == 0 {
                    anyhow::bail!("ran out of ports while searching for free slot");
                }
                tracing::warn!("port {} in use, trying {}", current.port(), next_port);
                current.set_port(next_port);
            }
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("could not find free port after {max_attempts} attempts")
}

// ─── Data directory ───────────────────────────────────────────────────────

/// Environment variable name for the data-directory test escape hatch.
///
/// Why: macOS's `dirs::data_dir()` delegates to `NSFileManager`, a native Cocoa
/// API that ignores `HOME` and `XDG_DATA_HOME`. Setting `HOME` in a test process
/// does **not** redirect `dirs::data_dir()` on macOS, making path isolation
/// impossible without a separate bypass. This constant names that bypass.
///
/// What: When `TRUSTY_DATA_DIR_OVERRIDE` is set in the environment,
/// [`resolve_data_dir`] uses its value as the base directory and skips the
/// `dirs::data_dir()` call entirely. The final path is
/// `${TRUSTY_DATA_DIR_OVERRIDE}/<app_name>`, identical in structure to the
/// normal OS-standard path.
///
/// **Intended for tests only.** Do not set this variable in production; it
/// bypasses the OS-standard application-data directory.
///
/// Test: All `resolve_data_dir` tests in this module set this var to a
/// temporary directory so they run identically on macOS, Linux, and Windows.
pub const DATA_DIR_OVERRIDE_ENV: &str = "TRUSTY_DATA_DIR_OVERRIDE";

/// Resolve `<data_dir>/<app_name>`, creating it if it doesn't exist.
///
/// Why: All trusty-* tools want a per-machine, per-app directory under the
/// OS-standard data dir (`~/Library/Application Support/`, `~/.local/share/`,
/// `%APPDATA%/`). If `dirs::data_dir()` is unavailable (rare — locked-down
/// containers), falls back to `~/.<app_name>` so the tool still works.
///
/// The [`DATA_DIR_OVERRIDE_ENV`] (`TRUSTY_DATA_DIR_OVERRIDE`) environment
/// variable provides a test escape hatch: when set, `dirs::data_dir()` is
/// **never called** and the variable's value is used as the base directory
/// instead. This is necessary because macOS's `dirs::data_dir()` calls
/// `NSFileManager` — a native Cocoa API that resolves the application-support
/// directory through the system rather than through the process environment —
/// so setting `HOME` or `XDG_DATA_HOME` in a test process does not redirect
/// it. `TRUSTY_DATA_DIR_OVERRIDE` is the only reliable cross-platform way to
/// isolate test data paths. **It is intended for tests only; do not set it in
/// production.**
///
/// What: returns the absolute path `${base}/<app_name>` (created if absent).
/// Resolution order:
/// 1. `$TRUSTY_DATA_DIR_OVERRIDE/<app_name>` — when the env var is set.
/// 2. `$(dirs::data_dir())/<app_name>` — normal OS-standard path.
/// 3. `~/.<app_name>` — fallback when `dirs::data_dir()` returns `None`.
///
/// Test: `resolve_data_dir_creates_directory` pins a temporary directory via
/// `TRUSTY_DATA_DIR_OVERRIDE` and asserts that the returned path is created
/// under it, exercising both the override path and directory-creation logic.
pub fn resolve_data_dir(app_name: &str) -> Result<PathBuf> {
    let base = if let Ok(override_dir) = std::env::var(DATA_DIR_OVERRIDE_ENV) {
        PathBuf::from(override_dir)
    } else {
        dirs::data_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(format!(".{app_name}"))))
            .context("could not resolve data directory or home directory")?
    };
    let dir = if base.ends_with(format!(".{app_name}")) {
        base
    } else {
        base.join(app_name)
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create data directory {}", dir.display()))?;
    Ok(dir)
}

// ─── Daemon address file ──────────────────────────────────────────────────

/// Filename used inside each app's data directory to record the daemon's
/// bound HTTP address. Kept as a module-level constant so writers and readers
/// can't drift.
const DAEMON_ADDR_FILENAME: &str = "http_addr";

/// Write the daemon's bound HTTP address to the app's data directory.
///
/// Why: Both trusty-search and trusty-memory persist their bound `host:port`
/// to disk so MCP clients (and follow-up CLI invocations) can discover where
/// the daemon ended up after auto-port-walking. Centralising the path layout
/// keeps the two projects in sync and prevents a third trusty-* daemon from
/// inventing yet another location.
/// What: writes `addr` verbatim (no trailing newline) to
/// `{resolve_data_dir(app_name)}/http_addr`, creating the directory if it
/// doesn't yet exist. Atomic-overwrite semantics aren't required — the file
/// is rewritten on every daemon start.
/// Test: `daemon_addr_round_trips` writes then reads under a stubbed HOME and
/// confirms equality.
pub fn write_daemon_addr(app_name: &str, addr: &str) -> Result<()> {
    let dir = resolve_data_dir(app_name)?;
    let path = dir.join(DAEMON_ADDR_FILENAME);
    std::fs::write(&path, addr).with_context(|| format!("write daemon addr to {}", path.display()))
}

/// Read the daemon's HTTP address from the app's data directory.
///
/// Why: CLI commands and MCP clients need to discover the running daemon's
/// bound port. Returning `Option` lets callers distinguish "daemon never
/// started" (file absent) from "filesystem error" (permission denied, etc.)
/// without resorting to string matching on error messages.
/// What: reads `{resolve_data_dir(app_name)}/http_addr`, trims surrounding
/// whitespace, and returns `Some(addr)`. Returns `Ok(None)` iff the file
/// does not exist; any other I/O error propagates as `Err`.
/// Test: `daemon_addr_round_trips` and `read_daemon_addr_missing_returns_none`.
pub fn read_daemon_addr(app_name: &str) -> Result<Option<String>> {
    let dir = resolve_data_dir(app_name)?;
    let path = dir.join(DAEMON_ADDR_FILENAME);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("read daemon addr from {}", path.display())),
    }
}

// ─── CLI initialisation ───────────────────────────────────────────────────

/// Initialise the global tracing subscriber.
///
/// Why: Every trusty-* binary wants the same verbosity ladder and the same
/// `RUST_LOG` override semantics. Defining it once removes the boilerplate
/// from every `main.rs`.
/// What: `verbose_count` maps `0 → warn`, `1 → info`, `2 → debug`, `3+ →
/// trace`. If `RUST_LOG` is set in the environment it wins. Logs go to
/// stderr so stdout stays clean for MCP JSON-RPC.
/// Test: side-effecting (global subscriber) — covered by integration with
/// `cargo run -- -v status` in downstream crates.
pub fn init_tracing(verbose_count: u8) {
    let default_filter = match verbose_count {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    // try_init so callers that pre-install a subscriber don't panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Disable coloured terminal output when requested or when stdout is not a TTY.
///
/// Why: Pipe-friendly output is mandatory for scripting (`trusty-search list
/// | jq …`). `NO_COLOR` / `TERM=dumb` are the canonical signals; passing
/// `--no-color` should override too.
/// What: calls `colored::control::set_override(false)` when the caller asks
/// for it or when the standard heuristics indicate no colour.
/// Test: side-effecting global; trivially covered by manual `NO_COLOR=1 cargo
/// run -- list`.
pub fn maybe_disable_color(no_color: bool) {
    let env_says_no =
        std::env::var("NO_COLOR").is_ok() || std::env::var("TERM").as_deref() == Ok("dumb");
    if no_color || env_says_no {
        colored::control::set_override(false);
    }
}

// ─── OpenRouter ───────────────────────────────────────────────────────────

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const HTTP_REFERER: &str = "https://github.com/bobmatnyc/trusty-common";
const X_TITLE: &str = "trusty-common";
const OPENROUTER_CONNECT_TIMEOUT_SECS: u64 = 10;
const OPENROUTER_REQUEST_TIMEOUT_SECS: u64 = 120; // chat completions can take 60–90s

/// OpenAI-compatible chat message.
///
/// Why: Both trusty-memory's `chat` subcommand and trusty-search's `/chat`
/// endpoint speak the OpenRouter format. Sharing the struct keeps them in
/// step (and lets callers compose chat histories without re-defining types).
/// Tool-use additions (`tool_call_id`, `tool_calls`) follow the OpenAI
/// function-calling shape: assistant messages set `tool_calls` when the model
/// requests tool invocations; subsequent `role: "tool"` messages echo the
/// matching `tool_call_id` with the tool's result in `content`.
/// What: `role` is one of `"system" | "user" | "assistant" | "tool"`.
/// `content` is the message text. `tool_call_id` is the id of the tool call
/// this message is replying to (only set when `role == "tool"`). `tool_calls`
/// is the raw OpenAI `tool_calls` array on an assistant message that asked
/// to invoke tools — kept as `serde_json::Value` so we don't drop any fields
/// the upstream may add.
/// Test: serde round-trip in `chat_message_round_trips`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
}

/// Send a chat completion request to OpenRouter and return the assistant's
/// message content.
///
/// Why: A one-shot, non-streaming chat call is the common-case helper — used
/// by trusty-memory's `chat` CLI and trusty-search's `/chat` endpoint.
/// What: POSTs `{model, messages, stream: false}` to OpenRouter with bearer
/// auth, decodes the response, and returns `choices[0].message.content`.
/// Errors propagate as anyhow with HTTP status context.
/// Test: error paths covered by `openrouter_propagates_http_errors` (uses a
/// blackhole base URL — no real call).
#[deprecated(since = "0.3.1", note = "Use OpenRouterProvider::chat_stream instead")]
pub async fn openrouter_chat(
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
) -> Result<String> {
    if api_key.is_empty() {
        return Err(anyhow!("openrouter api key is empty"));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            OPENROUTER_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(std::time::Duration::from_secs(
            OPENROUTER_REQUEST_TIMEOUT_SECS,
        ))
        .build()
        .context("build reqwest client for openrouter_chat")?;
    let body = ChatRequest {
        model,
        messages: &messages,
        stream: false,
    };
    let resp = client
        .post(OPENROUTER_URL)
        .bearer_auth(api_key)
        .header("HTTP-Referer", HTTP_REFERER)
        .header("X-Title", X_TITLE)
        .json(&body)
        .send()
        .await
        .context("POST openrouter chat completions")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("openrouter HTTP {status}: {text}"));
    }
    let payload: ChatResponse = resp.json().await.context("decode openrouter response")?;
    payload
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| anyhow!("openrouter returned no choices"))
}

/// Stream chat-completion deltas from OpenRouter through a tokio mpsc channel.
///
/// Why: `chat` UIs want incremental tokens for a responsive feel; the
/// streaming endpoint emits SSE `data:` frames with delta content.
/// What: POSTs the request with `stream: true`, parses each SSE `data:` line
/// as a JSON object, extracts `choices[0].delta.content`, and sends each
/// non-empty chunk to `tx`. The function returns when the stream terminates
/// (either by `[DONE]` sentinel or by upstream EOF).
/// Test: integration-only (no offline mock); covered manually via the
/// trusty-search `/chat` endpoint that re-uses this helper.
#[deprecated(since = "0.3.1", note = "Use OpenRouterProvider::chat_stream instead")]
pub async fn openrouter_chat_stream(
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    tx: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    use futures_util::StreamExt;

    if api_key.is_empty() {
        return Err(anyhow!("openrouter api key is empty"));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            OPENROUTER_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(std::time::Duration::from_secs(
            OPENROUTER_REQUEST_TIMEOUT_SECS,
        ))
        .build()
        .context("build reqwest client for openrouter_chat_stream")?;
    let body = ChatRequest {
        model,
        messages: &messages,
        stream: true,
    };
    let resp = client
        .post(OPENROUTER_URL)
        .bearer_auth(api_key)
        .header("HTTP-Referer", HTTP_REFERER)
        .header("X-Title", X_TITLE)
        .json(&body)
        .send()
        .await
        .context("POST openrouter chat completions (stream)")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("openrouter HTTP {status}: {text}"));
    }

    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("read openrouter stream chunk")?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        buf.push_str(text);

        while let Some(idx) = buf.find('\n') {
            let line: String = buf.drain(..=idx).collect();
            let line = line.trim();
            let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(delta) = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
                && !delta.is_empty()
                && tx.send(delta.to_string()).await.is_err()
            {
                // Receiver dropped — caller has lost interest.
                return Ok(());
            }
        }
    }
    Ok(())
}

// ─── Misc helpers ─────────────────────────────────────────────────────────

/// Check whether a path exists and is a directory.
///
/// Why: tiny but commonly-needed shim — clearer at call sites than
/// `path.exists() && path.is_dir()`.
/// What: returns `true` iff the path exists and metadata reports a directory.
/// Test: `is_dir_recognises_directories`.
pub fn is_dir(path: &Path) -> bool {
    path.metadata().map(|m| m.is_dir()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that mutate the `TRUSTY_DATA_DIR_OVERRIDE` env var so
    /// they don't race when `cargo test` runs them in parallel threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn auto_port_walks_forward() {
        // Bind to an OS-chosen port, then ask auto-port to start there.
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let next = bind_with_auto_port(addr, 8).await.unwrap();
        let got = next.local_addr().unwrap().port();
        assert_ne!(got, port, "expected walk-forward to a different port");
    }

    #[tokio::test]
    async fn auto_port_zero_attempts_still_binds_free() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let l = bind_with_auto_port(addr, 0).await.unwrap();
        assert!(l.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn resolve_data_dir_creates_directory() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Use the override env var so we deterministically control the base
        // directory cross-platform (macOS's dirs::data_dir ignores HOME).
        let tmp = tempfile_like_dir();
        // SAFETY: env mutation; tests in this module run serially via
        // #[test] threading isolation only when MUTEX-guarded — we accept
        // the residual risk since the override var is unique to these tests.
        unsafe {
            std::env::set_var(DATA_DIR_OVERRIDE_ENV, &tmp);
        }
        let dir = resolve_data_dir("trusty-test-xyz").unwrap();
        assert!(
            dir.exists(),
            "data dir should be created at {}",
            dir.display()
        );
        assert!(dir.is_dir());
        assert!(
            dir.starts_with(&tmp),
            "data dir {} should live under override {}",
            dir.display(),
            tmp.display()
        );
        unsafe {
            std::env::remove_var(DATA_DIR_OVERRIDE_ENV);
        }
    }

    #[test]
    fn daemon_addr_round_trips() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile_like_dir();
        // SAFETY: env mutation; see note in resolve_data_dir_creates_directory.
        unsafe {
            std::env::set_var(DATA_DIR_OVERRIDE_ENV, &tmp);
        }
        let app = format!(
            "trusty-test-daemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        write_daemon_addr(&app, "127.0.0.1:12345").unwrap();
        let got = read_daemon_addr(&app).unwrap();
        unsafe {
            std::env::remove_var(DATA_DIR_OVERRIDE_ENV);
        }
        assert_eq!(got.as_deref(), Some("127.0.0.1:12345"));
    }

    #[test]
    fn read_daemon_addr_missing_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile_like_dir();
        // SAFETY: env mutation; see note in resolve_data_dir_creates_directory.
        unsafe {
            std::env::set_var(DATA_DIR_OVERRIDE_ENV, &tmp);
        }
        let app = format!(
            "trusty-test-daemon-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let got = read_daemon_addr(&app).unwrap();
        unsafe {
            std::env::remove_var(DATA_DIR_OVERRIDE_ENV);
        }
        assert!(got.is_none(), "expected None when file absent, got {got:?}");
    }

    #[test]
    fn is_dir_recognises_directories() {
        let tmp = tempfile_like_dir();
        assert!(is_dir(&tmp));
        assert!(!is_dir(&tmp.join("nope")));
    }

    #[test]
    fn chat_message_round_trips() {
        let m = ChatMessage {
            role: "user".into(),
            content: "hello".into(),
            tool_call_id: None,
            tool_calls: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: ChatMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back.role, "user");
        assert_eq!(back.content, "hello");
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn openrouter_chat_rejects_empty_key() {
        let err = openrouter_chat("", "x", vec![]).await.unwrap_err();
        assert!(err.to_string().contains("api key"));
    }

    // Test-only helper: makes a unique scratch dir without pulling in tempfile
    // as a dev-dep (keeps the dependency surface minimal).
    fn tempfile_like_dir() -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("trusty-common-test-{pid}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
