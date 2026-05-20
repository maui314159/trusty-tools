//! Optional HTTP server exposing the symbol registry on port 7700 (#351).
//!
//! Why: External tools and other agents need a stable, language-neutral way
//! to query/modify the symbol substrate without linking against the Rust
//! library directly. An axum HTTP surface keeps that boundary clean.
//! What: `Routes` builds an `axum::Router` over a shared, mutex-guarded
//! `SymbolRegistry`. `serve` binds the router to `0.0.0.0:7700` and runs
//! until the future is dropped.
//! Test: `tests/server_tests.rs` exercises `/health` end-to-end.
//!
//! Note: The library can be used without this feature; the heavy axum +
//! tokio tree only links when `features = ["server"]` is set.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::symgraph::editor::{Patch, replace_symbol};
use crate::symgraph::emitter::{LayoutRules, apply_emit, emit};
use crate::symgraph::graph::SymbolGraph;
use crate::symgraph::parser::parse_directory;
use crate::symgraph::registry::{SymbolEntry, SymbolId, SymbolRegistry};
use crate::symgraph::strategy::ModulePathStrategy;

/// Default bind port.
pub const DEFAULT_PORT: u16 = 7700;

/// Shared application state — one registry per server.
#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Mutex<SymbolRegistry>>,
}

impl AppState {
    // INTENT: Wrap a registry in shared, async-safe state for the server.
    pub fn new(registry: SymbolRegistry) -> Self {
        Self {
            registry: Arc::new(Mutex::new(registry)),
        }
    }
}

// INTENT: Build the full route table without binding a port, enabling test and composition use.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/parse", post(parse_handler))
        .route("/symbols", get(list_symbols_handler))
        .route(
            "/symbol/{id}",
            get(get_symbol_handler).put(put_symbol_handler),
        )
        .route("/emit", post(emit_handler))
        .route("/verify", post(verify_handler))
        .route("/graph", get(graph_handler))
        .with_state(state)
}

// INTENT: Bind the router to a TCP port and serve until cancelled.
pub async fn serve(state: AppState, port: u16) -> anyhow::Result<()> {
    let app = router(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    symbols: usize,
}

// INTENT: Return server liveness and current symbol count.
async fn health(State(s): State<AppState>) -> impl IntoResponse {
    let reg = s.registry.lock().await;
    Json(HealthResponse {
        status: "ok",
        symbols: reg.len(),
    })
}

#[derive(Deserialize)]
struct ParseReq {
    directory: String,
    project_root: Option<String>,
}

#[derive(Serialize)]
struct ParseResp {
    parsed: usize,
}

// INTENT: Parse a directory for symbols and merge them into the shared registry.
async fn parse_handler(
    State(s): State<AppState>,
    Json(req): Json<ParseReq>,
) -> Result<Json<ParseResp>, ApiError> {
    let dir = std::path::PathBuf::from(&req.directory);
    let root = req
        .project_root
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dir.clone());
    let new_reg = parse_directory(&dir, &root).map_err(ApiError::from)?;
    let mut reg = s.registry.lock().await;
    let parsed = new_reg.len();
    for (_, entry) in new_reg.iter() {
        reg.insert(entry.clone());
    }
    Ok(Json(ParseResp { parsed }))
}

// INTENT: List all symbols currently in the registry.
async fn list_symbols_handler(State(s): State<AppState>) -> Json<Vec<SymbolEntry>> {
    let reg = s.registry.lock().await;
    Json(reg.iter().map(|(_, e)| e.clone()).collect())
}

// INTENT: Look up a single symbol by its qualified ID.
async fn get_symbol_handler(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SymbolEntry>, ApiError> {
    let reg = s.registry.lock().await;
    reg.get(&SymbolId(id))
        .cloned()
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
struct PutSymbolReq {
    file: String,
    new_source: String,
}

// INTENT: Replace a symbol's source text in-place via the editor.
async fn put_symbol_handler(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PutSymbolReq>,
) -> Result<Json<Patch>, ApiError> {
    let p = std::path::PathBuf::from(&req.file);
    let patch = replace_symbol(&p, &id, &req.new_source).map_err(ApiError::from)?;
    Ok(Json(patch))
}

#[derive(Deserialize, Default)]
struct EmitReq {
    output_dir: Option<String>,
    src_root: Option<String>,
}

#[derive(Serialize)]
struct EmitResp {
    written: Vec<String>,
}

// INTENT: Emit the registry to source files using the default module-path strategy.
async fn emit_handler(
    State(s): State<AppState>,
    Json(req): Json<EmitReq>,
) -> Result<Json<EmitResp>, ApiError> {
    let reg = s.registry.lock().await;
    let mut rules = LayoutRules::default();
    if let Some(root) = req.src_root {
        rules.src_root = root;
    }
    let strategy = ModulePathStrategy::default();
    let outputs = emit(&reg, &rules, &strategy).map_err(ApiError::from)?;
    let written = if let Some(dir) = req.output_dir {
        let p = std::path::PathBuf::from(dir);
        apply_emit(&outputs, &p)
            .map_err(ApiError::from)?
            .into_iter()
            .map(|p| p.display().to_string())
            .collect()
    } else {
        outputs.keys().map(|p| p.display().to_string()).collect()
    };
    Ok(Json(EmitResp { written }))
}

#[derive(Serialize)]
struct VerifyResp {
    stale: Vec<String>,
}

// INTENT: Check which symbols have stale content hashes.
async fn verify_handler(State(s): State<AppState>) -> Json<VerifyResp> {
    let reg = s.registry.lock().await;
    Json(VerifyResp {
        stale: reg.verify_hashes().into_iter().map(|i| i.0).collect(),
    })
}

#[derive(Deserialize)]
struct GraphReq {
    file: Option<String>,
}

// INTENT: Build and return the dependency graph for a given source file.
async fn graph_handler(
    axum::extract::Query(q): axum::extract::Query<GraphReq>,
) -> Result<Json<SymbolGraph>, ApiError> {
    let file = q
        .file
        .ok_or_else(|| ApiError::Bad("missing ?file= query param".into()))?;
    let g =
        SymbolGraph::build_from_file(&std::path::PathBuf::from(&file)).map_err(ApiError::from)?;
    Ok(Json(g))
}

/// Lightweight error type that maps to HTTP status codes.
pub enum ApiError {
    NotFound,
    Bad(String),
    Internal(String),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Bad(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, msg).into_response()
    }
}
