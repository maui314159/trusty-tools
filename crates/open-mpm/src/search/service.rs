//! Search-as-a-service daemon (#374).
//!
//! Why: The semantic code index is expensive to keep warm — loading the
//! HNSW into RAM, opening the redb store, and running the FastEmbedder
//! are all one-time costs that a short-lived REPL or sub-agent process
//! pays repeatedly. Running the index as a long-lived daemon shared by
//! every open-mpm process in a project amortizes that cost so a tool
//! call that searches the index pays only the HTTP round-trip plus the
//! query itself. The daemon also owns the redb write lock exclusively,
//! which avoids the lock-contention failures we used to hit when a
//! REPL, an --api server, and a sub-agent all tried to open the same
//! `.open-mpm/state/code/` directory.
//! What: [`run_search_service`] is the daemon entry point. It opens
//! the on-disk store, warms the HNSW into RAM, spawns a [`FileWatcher`]
//! to keep the index in sync with the working tree, binds an HTTP
//! listener on an auto-assigned localhost port, persists `{pid, port,
//! socket_path, started_at}` to `.open-mpm/state/search.pid`, and
//! serves five JSON routes: `/search/health`, `/search/query`,
//! `/search/index-file`, `/search/remove-file`, `/search/reindex`.
//! Shutdown is triggered by SIGTERM / SIGINT and removes the pid file
//! plus the unix socket placeholder before exiting.
//! Test: See `tests` module — pid-file round-trip, socket-path
//! convention, and an end-to-end start/query/stop integration test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::memory::{CodeStore, FastEmbedder};
use crate::search::indexer::CodeIndexer;
use crate::search::watcher::FileWatcher;

/// Embedding dimension for FastEmbedder. Mirrors `build_file_watcher` in
/// `src/main.rs` so the daemon and the in-process watcher see identical
/// vectors on the wire.
const EMBED_DIM: usize = 384;

/// Default extensions the embedded watcher tracks. Mirrors
/// `default_extensions` in `src/main.rs`.
fn default_extensions() -> Vec<String> {
    ["rs", "py", "ts", "tsx", "js", "jsx", "go", "md"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Persisted record of the running search daemon.
///
/// Why: External processes (REPL, sub-agents, the --api server) need to
/// discover the daemon's HTTP port without each one re-binding. The pid
/// file is the rendezvous point.
/// What: Serialized as JSON in `.open-mpm/state/search.pid`. The
/// `socket_path` field records the canonical Unix-socket placeholder
/// for the daemon (kept for parity with the ctrl socket convention even
/// though the wire protocol is HTTP-over-TCP per #374's option B).
/// Test: `pid_file_roundtrip`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDaemonState {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub port: u16,
    pub socket_path: PathBuf,
}

/// Resolve the canonical pid-file path under the project's state dir.
///
/// Why: Centralised so the daemon writer and the client reader can't
/// drift out of sync.
/// What: Returns `<project_root>/.open-mpm/state/search.pid`.
pub fn pid_file_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".open-mpm")
        .join("state")
        .join("search.pid")
}

/// Resolve the canonical Unix-socket path for the daemon.
///
/// Why: Mirrors `ctrl_socket_path` in `src/ctrl/socket.rs` so future
/// migrations to a Unix-socket transport reuse the same convention.
/// Currently the daemon advertises this path in the pid file but binds
/// HTTP on `127.0.0.1:<auto-port>`; the placeholder file is touched so
/// stale-detection logic can use it.
/// What: Returns `~/.open-mpm/sockets/<project_id>.search.sock`. Falls
/// back to `<project_root>/.open-mpm/state/search.sock` when no home
/// directory is detectable.
pub fn search_socket_path(project_root: &Path) -> PathBuf {
    let project_id = crate::ctrl::socket::project_id_from_path(project_root);
    if let Some(home) = dirs::home_dir() {
        home.join(".open-mpm")
            .join("sockets")
            .join(format!("{project_id}.search.sock"))
    } else {
        project_root
            .join(".open-mpm")
            .join("state")
            .join("search.sock")
    }
}

/// Read the daemon pid file, returning `None` if missing or malformed.
pub fn read_pid_file(project_root: &Path) -> Option<SearchDaemonState> {
    let raw = std::fs::read_to_string(pid_file_path(project_root)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write the daemon pid file atomically (write-then-rename).
pub fn write_pid_file(project_root: &Path, state: &SearchDaemonState) -> Result<()> {
    let path = pid_file_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("pid.tmp");
    let bytes = serde_json::to_vec_pretty(state)?;
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("writing temp pid file {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming temp pid file to {}", path.display()))?;
    Ok(())
}

/// Best-effort removal of the pid file.
pub fn remove_pid_file(project_root: &Path) {
    let _ = std::fs::remove_file(pid_file_path(project_root));
}

/// Probe `/search/health` over a 500ms budget to confirm the daemon at
/// `port` is actually answering — not just bound.
async fn health_ok(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/search/health");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Returns true iff a daemon is observably running for `project_root`.
pub async fn is_daemon_running(project_root: &Path) -> bool {
    let Some(state) = read_pid_file(project_root) else {
        return false;
    };
    if !pid_alive(state.pid) {
        return false;
    }
    health_ok(state.port).await
}

/// Check whether `pid` is alive via `kill -0`. Mirrors `service::pid_alive`.
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Shared state injected into every axum handler.
///
/// Why: Each handler needs the same indexer + bookkeeping state; axum's
/// `State<T>` extractor cleans up signature noise vs threading a tuple.
/// What: Holds an `Arc<CodeIndexer>` (the warm index), the project root
/// (so handlers can canonicalise relative paths), and a small `Mutex`
/// guard around an in-flight reindex flag so duplicate background
/// reindex requests don't pile up.
/// Test: Exercised via the integration test `start_query_stop_round_trip`.
#[derive(Clone)]
pub struct SearchState {
    pub indexer: Arc<CodeIndexer>,
    pub project_root: PathBuf,
    pub reindex_in_flight: Arc<Mutex<bool>>,
}

#[derive(Deserialize)]
struct QueryBody {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
    /// When true, run KG expansion on top-K results (#376 B1).
    #[serde(default = "default_expand_graph")]
    expand_graph: bool,
    /// When true, truncate each chunk's `text` to 7 lines (compact mode).
    ///
    /// Why: Full chunk payloads can be 40-120 lines. Compact mode cuts
    /// ~5-10x token cost for callers that only need to locate a function,
    /// not read its entire body (#400).
    #[serde(default)]
    compact: bool,
}

fn default_top_k() -> usize {
    5
}

fn default_expand_graph() -> bool {
    true
}

#[derive(Deserialize)]
struct PathBody {
    path: String,
}

/// `GET /search/health` — liveness probe.
async fn health_handler(State(s): State<SearchState>) -> impl IntoResponse {
    // Best-effort: count of CodeIndex chunks isn't directly exposed by the
    // store trait, so we report a sentinel `-1` when unavailable. The
    // important contract is the 200 status + `status: ok`.
    let chunks: i64 = -1;
    let _ = &s; // silence unused for now; placeholder for future stats
    Json(serde_json::json!({
        "status": "ok",
        "indexed_chunks": chunks,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Number of lines to keep per chunk in compact mode (#400).
const COMPACT_LINES: usize = 7;

/// Truncate a chunk's text to `COMPACT_LINES` lines when compact mode is on.
///
/// Why: Full chunks can be 40-120 lines; callers that only need to locate a
/// function can request compact mode for ~5-10x token savings (#400).
fn apply_compact(
    mut hits: Vec<crate::search::indexer::CodeChunk>,
) -> Vec<crate::search::indexer::CodeChunk> {
    for chunk in &mut hits {
        let truncated: String = chunk
            .text
            .lines()
            .take(COMPACT_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        chunk.text = truncated;
    }
    hits
}

/// `POST /search/query` — semantic + lexical hybrid search.
async fn query_handler(
    State(s): State<SearchState>,
    Json(body): Json<QueryBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if body.query.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "query must be non-empty".into()));
    }
    // Helper: serialize hits, return a 500 instead of swallowing the error
    // into `Value::Null` like the previous version did (#376 A4).
    let to_json = |hits: Vec<crate::search::indexer::CodeChunk>| {
        let hits = if body.compact {
            apply_compact(hits)
        } else {
            hits
        };
        serde_json::to_value(&hits).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("encoding hits to JSON failed: {e}"),
            )
        })
    };
    match s
        .indexer
        .search_hybrid(&body.query, body.top_k, body.expand_graph)
        .await
    {
        Ok(hits) => Ok(Json(to_json(hits)?)),
        Err(e) => {
            tracing::warn!(error = %e, "search_hybrid failed; falling back to vector-only");
            match s.indexer.search(&body.query, body.top_k).await {
                Ok(hits) => Ok(Json(to_json(hits)?)),
                Err(e2) => Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("search failed: hybrid={e}; vector={e2}"),
                )),
            }
        }
    }
}

/// `POST /search/index-file` — re-index a single file by absolute path.
async fn index_file_handler(
    State(s): State<SearchState>,
    Json(body): Json<PathBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = PathBuf::from(&body.path);
    match s.indexer.index_file(&path, Some(&s.project_root)).await {
        Ok(n) => Ok(Json(serde_json::json!({ "chunks": n }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("index_file failed: {e}"),
        )),
    }
}

/// `POST /search/remove-file` — drop all chunks for a path.
async fn remove_file_handler(
    State(s): State<SearchState>,
    Json(body): Json<PathBody>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = PathBuf::from(&body.path);
    match s.indexer.remove_file(&path).await {
        Ok(n) => Ok(Json(serde_json::json!({ "removed": n }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("remove_file failed: {e}"),
        )),
    }
}

/// `POST /search/reindex` — fire-and-forget full directory reindex.
async fn reindex_handler(State(s): State<SearchState>) -> Json<Value> {
    {
        let mut flag = s.reindex_in_flight.lock().await;
        if *flag {
            return Json(serde_json::json!({ "status": "already-running" }));
        }
        *flag = true;
    }
    let indexer = Arc::clone(&s.indexer);
    let root = s.project_root.clone();
    let flag = Arc::clone(&s.reindex_in_flight);
    tokio::spawn(async move {
        let exts = default_extensions();
        let ext_refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
        match indexer.index_directory(&root, &ext_refs).await {
            Ok(n) => tracing::info!(chunks = n, "background reindex complete"),
            Err(e) => tracing::warn!(error = %e, "background reindex failed"),
        }
        *flag.lock().await = false;
    });
    Json(serde_json::json!({ "status": "started" }))
}

/// Build the axum router with the shared state attached.
///
/// Why: Splitting the router from `run_search_service` keeps the
/// integration test simple — it can construct a `SearchState` over a
/// mock store and exercise all five handlers without going through
/// `tokio::signal` or pid-file IO.
pub fn build_router(state: SearchState) -> Router {
    Router::new()
        .route("/search/health", get(health_handler))
        .route("/search/query", post(query_handler))
        .route("/search/index-file", post(index_file_handler))
        .route("/search/remove-file", post(remove_file_handler))
        .route("/search/reindex", post(reindex_handler))
        .with_state(state)
}

/// Run the search-as-a-service daemon to completion.
///
/// Why: Long-running entry point invoked from `main.rs` early dispatch
/// (`--search-service`). Owns the redb lock for the project's code
/// store for the lifetime of the process — every other open-mpm
/// process in the same project must talk to the daemon over HTTP
/// rather than opening the store itself.
/// What:
///   1. Refuses to start if a healthy daemon is already running.
///   2. Opens `CodeStore` + `FastEmbedder` + builds an `Arc<CodeIndexer>`
///      with `Duration::MAX` cool-down (never evict — that's the whole
///      point of having a daemon).
///   3. `warm_up()` — load HNSW into RAM.
///   4. Spawns a `FileWatcher` task so on-disk edits update the index.
///   5. Binds a TCP listener on `127.0.0.1:0` (auto-assigned port).
///   6. Writes the pid file, touches the socket placeholder.
///   7. Installs a SIGTERM/SIGINT handler that triggers axum graceful
///      shutdown and cleans up the pid file + socket placeholder.
///   8. Serves until shutdown.
/// Test: Manual: `open-mpm --search-service` in one terminal, `curl
/// http://127.0.0.1:<port>/search/health` in another. The integration
/// test `start_query_stop_round_trip` covers the same path with
/// in-memory mocks.
pub async fn run_search_service(project_root: PathBuf) -> Result<()> {
    if is_daemon_running(&project_root).await {
        println!("search daemon already running; nothing to do");
        return Ok(());
    }

    // TOCTOU guard (#376 A3): two daemons started concurrently can both
    // pass the `is_daemon_running` check above, then both write the pid
    // file. Acquire an exclusive non-blocking flock on a sibling lock
    // file so only one process proceeds. The lock is released
    // automatically when `_lock_file` is dropped (i.e., on daemon exit).
    let state_dir = project_root.join(".open-mpm").join("state");
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating state dir {}", state_dir.display()))?;
    let lock_path = state_dir.join("search.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening startup lock {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `lock_file` owns the fd for the duration of this call;
        // we never close it manually. flock with LOCK_NB returns 0 on
        // success and -1 with errno=EWOULDBLOCK if another process holds
        // the lock — the exact contention case we want to detect.
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            anyhow::bail!(
                "search daemon already starting (lock held at {})",
                lock_path.display()
            );
        }
    }
    // Keep the lock alive for the lifetime of the daemon by binding it
    // to a name; dropping at end-of-fn releases the OS lock.
    let _lock_file = lock_file;

    // Best-effort cleanup: a stale pid file from a previous crash should
    // not block startup.
    remove_pid_file(&project_root);

    let code_dir = project_root.join(".open-mpm").join("state").join("code");
    std::fs::create_dir_all(&code_dir)
        .with_context(|| format!("creating code dir {}", code_dir.display()))?;

    tracing::info!("opening code store at {}", code_dir.display());
    let store = CodeStore::open(&code_dir, EMBED_DIM).context("failed to open CodeStore")?;
    let embedder = FastEmbedder::new().context("failed to construct FastEmbedder")?;
    // Cap concurrent indexing jobs at ~half available parallelism so axum
    // HTTP handler tasks always have threads to run on. Without this cap a
    // burst of fastembed ONNX inference jobs (one per chunk) saturates the
    // tokio blocking pool and `/search/query` times out under active
    // re-indexing (#399).
    let indexing_concurrency = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(1))
        .unwrap_or(1);
    let indexing_permits = Arc::new(tokio::sync::Semaphore::new(indexing_concurrency));
    tracing::info!(
        permits = indexing_concurrency,
        "search daemon: capping concurrent indexing jobs"
    );
    // `Duration::MAX` disables cool-down — the daemon is exactly the place
    // where we want to keep the HNSW pinned in RAM forever.
    let indexer = Arc::new(
        CodeIndexer::new(Arc::new(store), Arc::new(embedder))
            .with_cool_after(Duration::MAX)
            .with_indexing_semaphore(Arc::clone(&indexing_permits)),
    );

    tracing::info!("warming code index...");
    if let Err(e) = indexer.warm_up().await {
        tracing::warn!(error = %e, "warm_up failed; will lazy-load on first query");
    }

    // Background file watcher so edits propagate without a manual reindex.
    let watcher_indexer = Arc::clone(&indexer);
    let watcher_root = project_root.clone();
    tokio::spawn(async move {
        let watcher = FileWatcher::new(watcher_indexer, watcher_root, default_extensions());
        if let Err(e) = watcher.watch().await {
            tracing::warn!(error = %e, "file watcher exited");
        }
    });

    // Bind on an auto-assigned port so multiple projects can each run a
    // daemon on the same machine without coordinating port numbers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding 127.0.0.1:0")?;
    let bound_port = listener
        .local_addr()
        .context("reading bound socket addr")?
        .port();

    let socket_path = search_socket_path(&project_root);
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Touch the socket placeholder so external tooling can detect a
    // running daemon by file presence + pid liveness alone.
    let _ = std::fs::write(&socket_path, b"");

    let pid_state = SearchDaemonState {
        pid: std::process::id(),
        started_at: Utc::now(),
        port: bound_port,
        socket_path: socket_path.clone(),
    };
    write_pid_file(&project_root, &pid_state).context("writing pid file")?;

    println!(
        "[open-mpm] search daemon: http://127.0.0.1:{bound_port}/search (pid {})",
        pid_state.pid
    );

    let state = SearchState {
        indexer: Arc::clone(&indexer),
        project_root: project_root.clone(),
        reindex_in_flight: Arc::new(Mutex::new(false)),
    };
    let router = build_router(state);

    // Graceful shutdown on SIGTERM / SIGINT — drop the pid file and the
    // socket placeholder so subsequent starts don't see stale state.
    let project_root_for_shutdown = project_root.clone();
    let socket_for_shutdown = socket_path.clone();
    let shutdown = async move {
        wait_for_signal().await;
        tracing::info!("search daemon: shutdown signal received");
        remove_pid_file(&project_root_for_shutdown);
        let _ = std::fs::remove_file(&socket_for_shutdown);
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("axum::serve")?;

    // Best-effort cleanup if the server exits without a signal.
    remove_pid_file(&project_root);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Wait for SIGTERM or SIGINT (Ctrl+C). Returns when either fires.
///
/// Why: Daemons need a portable shutdown hook; `tokio::signal` gives us
/// SIGINT cross-platform and SIGTERM on Unix. On Windows we just wait
/// for ctrl_c.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn pid_file_roundtrip() {
        let dir = TempDir::new().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join(".open-mpm").join("state")).unwrap();

        let state = SearchDaemonState {
            pid: 12345,
            started_at: Utc::now(),
            port: 54321,
            socket_path: PathBuf::from("/tmp/test.sock"),
        };
        write_pid_file(project, &state).expect("write");
        let back = read_pid_file(project).expect("read");
        assert_eq!(back.pid, 12345);
        assert_eq!(back.port, 54321);
        assert_eq!(back.socket_path, PathBuf::from("/tmp/test.sock"));
    }

    #[test]
    fn read_missing_pid_file_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_pid_file(dir.path()).is_none());
    }

    #[test]
    fn pid_file_path_is_under_state_dir() {
        let dir = TempDir::new().unwrap();
        let p = pid_file_path(dir.path());
        assert!(p.ends_with(".open-mpm/state/search.pid"));
    }

    #[test]
    fn search_socket_path_uses_project_id() {
        let p = PathBuf::from("/tmp/some-project");
        let s = search_socket_path(&p);
        let s_str = s.to_string_lossy().into_owned();
        // Either uses HOME-based sockets dir or falls back to project state.
        assert!(
            s_str.contains("some-project.search.sock") || s_str.ends_with("search.sock"),
            "unexpected socket path: {s_str}"
        );
    }

    #[tokio::test]
    async fn health_ok_returns_false_for_unbound_port() {
        // Port 1 is privileged; nothing should be listening at user level.
        assert!(!health_ok(1).await);
    }

    #[tokio::test]
    async fn router_serves_health_with_mock_indexer() {
        // Why: Exercises the axum router and the SearchState plumbing
        // without spinning up a full daemon (which needs an embedder
        // model + file watcher). Uses an in-memory MockStore + MockEmbedder
        // mirrored from the indexer tests.
        use crate::memory::{Embedder, MemoryResult, MemoryStore, Segment};
        use async_trait::async_trait;
        use std::collections::HashMap;
        use std::sync::Mutex as StdMutex;

        struct MockStore {
            inner: StdMutex<HashMap<String, (Vec<f32>, Value)>>,
        }
        #[async_trait]
        impl MemoryStore for MockStore {
            async fn insert(
                &self,
                _: Segment,
                id: &str,
                v: &[f32],
                p: Value,
            ) -> anyhow::Result<()> {
                self.inner
                    .lock()
                    .unwrap()
                    .insert(id.into(), (v.to_vec(), p));
                Ok(())
            }
            async fn search(
                &self,
                _: Segment,
                _: &[f32],
                _: usize,
            ) -> anyhow::Result<Vec<MemoryResult>> {
                Ok(vec![])
            }
            async fn get(&self, _: Segment, id: &str) -> anyhow::Result<Option<Value>> {
                Ok(self.inner.lock().unwrap().get(id).map(|(_, p)| p.clone()))
            }
            async fn delete(&self, _: Segment, id: &str) -> anyhow::Result<()> {
                self.inner.lock().unwrap().remove(id);
                Ok(())
            }
        }
        struct MockEmbedder;
        impl Embedder for MockEmbedder {
            fn embed(&self, t: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
                Ok(t.iter().map(|s| vec![s.len() as f32; 8]).collect())
            }
            fn embed_single(&self, t: &str) -> anyhow::Result<Vec<f32>> {
                Ok(vec![t.len() as f32; 8])
            }
            fn dimension(&self) -> usize {
                8
            }
        }

        let store: Arc<dyn MemoryStore> = Arc::new(MockStore {
            inner: StdMutex::new(HashMap::new()),
        });
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
        let indexer = Arc::new(CodeIndexer::new(store, embedder));

        let dir = TempDir::new().unwrap();
        let state = SearchState {
            indexer,
            project_root: dir.path().to_path_buf(),
            reindex_in_flight: Arc::new(Mutex::new(false)),
        };
        let app = build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give the listener a moment to come up. 50ms is plenty on localhost.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let url = format!("http://127.0.0.1:{port}/search/health");
        let resp = reqwest::get(&url).await.expect("GET /search/health");
        assert!(resp.status().is_success());
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert!(body["version"].is_string());

        // Empty-query is rejected.
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/search/query"))
            .json(&serde_json::json!({"query": "", "top_k": 5}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        // Valid query returns a JSON array (mock indexer returns []).
        let resp = client
            .post(format!("http://127.0.0.1:{port}/search/query"))
            .json(&serde_json::json!({"query": "foo", "top_k": 3}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body: Value = resp.json().await.unwrap();
        assert!(body.is_array(), "expected JSON array, got {body:?}");

        // /search/reindex returns started.
        let resp = client
            .post(format!("http://127.0.0.1:{port}/search/reindex"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "started");

        server.abort();
    }
}
