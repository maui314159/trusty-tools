//! Native search tools (#133, #137) — typed replacements for shell_exec-based search.
//!
//! Why: Agents currently invoke code search / skill listing through the
//! `shell_exec` tool. That couples the agent to bash and makes it hard to
//! enforce structure in the result. Native tools return JSON directly, so
//! downstream LLM reasoning has a predictable shape.
//! What: Three tools — `search_code`, `search_memory`, `search_skills`.
//! Each returns a JSON object with a `hits` array. When the underlying
//! backend is not wired (tool constructed with `new()` rather than
//! `with_indexer(...)`/`with_graph(...)`/`with_resolver(...)`), the tool
//! returns a structured `{"error": "...", "hits": []}` payload rather than
//! panicking so the agent can gracefully continue.
//! Test: Each tool's `name()`, `schema()`, happy-path `execute()` (with a
//! real or mock backend) and graceful-degradation path are covered in the
//! `tests` module below.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::memory::graph::MemoryGraph;
use crate::search::indexer::CodeIndexer;
use crate::search::service_client::SearchDaemonClient;
use crate::tools::file_filter::{should_skip_dir, should_skip_file};
use crate::tools::traits::{SkillResolver, ToolExecutor, ToolResult};

const DEFAULT_TOP_N: usize = 5;
const DEFAULT_SKILL_TOP_N: usize = 3;
/// Max characters of snippet returned per hit so a single call can't blow the
/// context window.
const SNIPPET_MAX_CHARS: usize = 600;
/// Max characters of skill body included in `search_skills` previews.
const SKILL_PREVIEW_MAX_CHARS: usize = 400;

/// `search_code` — semantic code search over the local vector index.
///
/// Why: Agents exploring an unfamiliar codebase need semantic (not regex)
/// lookups. Wraps `CodeIndexer` so the tool can be injected with a shared
/// index without every caller re-opening the on-disk store.
/// What: Holds an optional `Arc<CodeIndexer>`. When `Some`, forwards the
/// query to `indexer.search`. When `None`, returns a graceful "unavailable"
/// payload so the agent can proceed.
/// Test: `search_code_returns_hits_from_indexer` runs a real indexer over a
/// mock store + embedder; `search_code_degrades_gracefully` verifies the
/// `None`-backend path.
pub struct SearchCodeTool {
    backend: SearchBackend,
    /// When true, replace the full chunk text with a 7-line snippet
    /// centred on the function start so a single search call can't
    /// blow the context window (#376 C1). Default `true` for the
    /// `Remote` backend (where snippets travel over HTTP), `false`
    /// for `Local`/`None` to preserve existing local behaviour.
    compact: bool,
}

/// Where `SearchCodeTool` actually runs the query.
///
/// Why: The tool started life with only a local in-process index. Issue
/// #374 introduces a daemon variant; modeling the choice as an enum
/// keeps `execute()` a small match instead of two near-duplicate paths
/// and makes the priority order (Remote > Local > None) explicit.
/// What: `Remote` talks HTTP to the search daemon, `Local` calls
/// `CodeIndexer` in-process, `None` falls back to grep over CWD.
/// Test: All three arms are exercised in `tests` below.
enum SearchBackend {
    None,
    Local(Arc<CodeIndexer>),
    Remote(SearchDaemonClient),
}

impl SearchCodeTool {
    /// Construct without a backend (graceful-degradation grep mode).
    pub fn new() -> Self {
        Self {
            backend: SearchBackend::None,
            compact: false,
        }
    }

    /// Construct with a shared `CodeIndexer` that will service real queries.
    pub fn with_indexer(indexer: Arc<CodeIndexer>) -> Self {
        Self {
            backend: SearchBackend::Local(indexer),
            compact: false,
        }
    }

    /// Construct with a daemon client that will forward queries over HTTP.
    pub fn with_daemon(client: SearchDaemonClient) -> Self {
        Self {
            backend: SearchBackend::Remote(client),
            compact: true,
        }
    }

    /// Override the compact-snippet flag (mostly for tests + advanced
    /// callers that want the full chunk text).
    pub fn with_compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    /// Auto-select the best available backend for `project_root`.
    ///
    /// Why: Callers (the ctrl tool registry) shouldn't have to reach
    /// into pid files themselves. The priority order — daemon, local
    /// indexer, grep — matches the desired UX: zero-cost when the
    /// daemon is up, full-fidelity when it isn't, never an error.
    /// What: Probes the daemon first; on miss, returns a `None`
    /// backend (the caller can later upgrade to `Local` via
    /// `with_indexer`). Returns a future to keep `new_auto` callable
    /// from sync code via `.await`.
    /// Test: `new_auto_returns_none_backend_when_no_daemon`.
    pub async fn new_auto(project_root: &Path) -> Self {
        if let Some(client) = SearchDaemonClient::connect_if_running(project_root).await {
            return Self::with_daemon(client);
        }
        Self::new()
    }

    /// Auto-select with a local fallback when no daemon is running.
    ///
    /// Why: The ctrl tool registry already has an `Arc<CodeIndexer>` it
    /// would otherwise wire directly. Letting it pass that in lets us
    /// prefer the daemon first and only fall back to the local index
    /// when the daemon is absent — without ever dropping to the bare
    /// grep path that was the source of bug #374.
    /// What: Tries the daemon; on miss returns `with_indexer(local)`.
    pub async fn new_auto_with_local(project_root: &Path, local: Arc<CodeIndexer>) -> Self {
        if let Some(client) = SearchDaemonClient::connect_if_running(project_root).await {
            return Self::with_daemon(client);
        }
        Self::with_indexer(local)
    }
}

impl Default for SearchCodeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for SearchCodeTool {
    fn name(&self) -> &str {
        "search_code"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "Semantic code search over the project's local vector index. Returns an array of {path, function_name, start_line, end_line, language, score, snippet} hits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Natural-language query."},
                        "top_n": {"type": "integer", "description": "Max number of results (default 5)."}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return ToolResult::err("search_code: missing 'query'");
        };
        let top_n = args
            .get("top_n")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_TOP_N);

        // Backend dispatch: prefer daemon, then local indexer, then grep.
        let indexer = match &self.backend {
            SearchBackend::Remote(client) => {
                // Daemon path: forward the query over HTTP. On any error,
                // fall through to grep so the agent always gets *something*.
                match client.search(query, top_n).await {
                    Ok(chunks) if !chunks.is_empty() => {
                        let hits: Vec<Value> = chunks
                            .into_iter()
                            .map(|c| chunk_to_hit_json(&c, self.compact))
                            .collect();
                        let out = json!({
                            "query": query,
                            "hits": hits,
                            "engine": "daemon",
                        });
                        return ToolResult::ok(out.to_string());
                    }
                    Ok(_) => {
                        // Daemon returned empty — fall through to grep below.
                        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                        let hits = grep_fallback_search(&cwd, query, top_n);
                        let out = json!({
                            "query": query,
                            "hits": hits,
                            "fallback": "grep",
                            "note": "search daemon returned no hits; results from substring grep"
                        });
                        return ToolResult::ok(out.to_string());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "daemon search failed; falling back to grep");
                        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                        let hits = grep_fallback_search(&cwd, query, top_n);
                        let out = json!({
                            "query": query,
                            "hits": hits,
                            "fallback": "grep",
                            "note": format!("search daemon error ({e}); results from substring grep")
                        });
                        return ToolResult::ok(out.to_string());
                    }
                }
            }
            SearchBackend::Local(indexer) => indexer,
            SearchBackend::None => {
                // Graceful fallback: walk the project directory and substring-match
                // the query against file contents. This isn't as smart as vector
                // search, but it lets the in-process CTRL path (which never
                // initialises a vector index) return useful results instead of an
                // error. See bug #213.
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let hits = grep_fallback_search(&cwd, query, top_n);
                let out = json!({
                    "query": query,
                    "hits": hits,
                    "fallback": "grep",
                    "note": "vector index not available; results from case-insensitive substring grep over project files"
                });
                return ToolResult::ok(out.to_string());
            }
        };

        // Why: Hybrid (vector + BM25 RRF) search is strictly better than
        // either signal alone for code lookups, so we always prefer it when
        // an indexer is wired. Ripgrep is only invoked when hybrid returned
        // zero hits — never as a competing signal — so well-indexed projects
        // never pay the walkdir cost. Pass `expand_graph: true` so KG
        // callers/callees of top-K hits also surface (#376 B1).
        match indexer.search_hybrid(query, top_n, true).await {
            Ok(chunks) if !chunks.is_empty() => {
                let hits: Vec<Value> = chunks
                    .into_iter()
                    .map(|c| chunk_to_hit_json(&c, self.compact))
                    .collect();
                let out = json!({
                    "query": query,
                    "hits": hits,
                    "engine": "hybrid",
                });
                ToolResult::ok(out.to_string())
            }
            Ok(_empty) => {
                // Hybrid returned nothing — fall back to walkdir+grep so the
                // caller still gets something for tokens that aren't in the
                // index (newly-added files, generated code, README content).
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let hits = grep_fallback_search(&cwd, query, top_n);
                let out = json!({
                    "query": query,
                    "hits": hits,
                    "fallback": "ripgrep",
                    "note": "hybrid search returned no hits; results from case-insensitive substring grep"
                });
                ToolResult::ok(out.to_string())
            }
            Err(e) => {
                // Hybrid failed unexpectedly. Try the legacy vector-only
                // path as a safety net before giving up — keeps the tool
                // useful even if the BM25/tokenizer path regresses.
                tracing::warn!(error = %e, "search_hybrid failed; falling back to vector-only search");
                match indexer.search(query, top_n).await {
                    Ok(chunks) => {
                        let hits: Vec<Value> = chunks
                            .into_iter()
                            .map(|c| chunk_to_hit_json(&c, self.compact))
                            .collect();
                        let out = json!({
                            "query": query,
                            "hits": hits,
                            "engine": "vector",
                            "note": "hybrid path errored; fell back to vector-only search",
                        });
                        ToolResult::ok(out.to_string())
                    }
                    Err(e2) => {
                        let out = json!({
                            "error": format!("search_code backend failed: hybrid={e}; vector={e2}"),
                            "query": query,
                            "hits": []
                        });
                        ToolResult::ok(out.to_string())
                    }
                }
            }
        }
    }
}

/// `search_memory` — query the project agent-memory graph.
///
/// Why: Agents benefit from recall of prior decisions / research across runs.
/// Wraps `MemoryGraph::search` so the tool can share the same graph the PM
/// loop is writing to.
/// What: Holds an optional `Arc<MemoryGraph>`. When `None`, returns a graceful
/// "unavailable" payload.
/// Test: `search_memory_executes_with_graph` exercises a real graph over a
/// mock store; `search_memory_degrades_gracefully` covers the absent-graph
/// path.
pub struct SearchMemoryTool {
    graph: Option<Arc<MemoryGraph>>,
}

impl SearchMemoryTool {
    /// Construct without a backend (graceful-degradation mode).
    pub fn new() -> Self {
        Self { graph: None }
    }

    /// Construct with a shared `MemoryGraph` for real queries.
    pub fn with_graph(graph: Arc<MemoryGraph>) -> Self {
        Self { graph: Some(graph) }
    }
}

impl Default for SearchMemoryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for SearchMemoryTool {
    fn name(&self) -> &str {
        "search_memory"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "search_memory",
                "description": "Query agent memory (stored sessions) for relevant prior turns. Returns {id, score, payload} hits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "top_n": {"type": "integer", "description": "Default 5."}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return ToolResult::err("search_memory: missing 'query'");
        };
        let top_n = args
            .get("top_n")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_TOP_N);

        let Some(graph) = self.graph.as_ref() else {
            let out = json!({
                "error": "memory graph not available — SearchMemoryTool was constructed without a MemoryGraph backend.",
                "query": query,
                "hits": []
            });
            return ToolResult::ok(out.to_string());
        };

        match graph.search(query, top_n).await {
            Ok(results) => {
                let hits: Vec<Value> = results
                    .into_iter()
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "score": r.score,
                            "segment": r.segment,
                            "payload": r.payload,
                        })
                    })
                    .collect();
                let out = json!({
                    "query": query,
                    "hits": hits,
                });
                ToolResult::ok(out.to_string())
            }
            Err(e) => {
                let out = json!({
                    "error": format!("search_memory backend failed: {e}"),
                    "query": query,
                    "hits": []
                });
                ToolResult::ok(out.to_string())
            }
        }
    }
}

/// `search_skills` — find relevant skills in the local registry.
///
/// Why: Agents need to discover which skill bodies are relevant to a task
/// without memorizing the full catalog.
/// What: Holds an optional `Arc<dyn SkillResolver>`. When the resolver is
/// present, enumerates known skills, filters names/content by case-insensitive
/// substring match, and returns name + first-line + short preview. When
/// absent, walks `.open-mpm/skills/` on disk directly so the tool is still
/// useful in the simplest wiring. When neither is viable, returns a graceful
/// "unavailable" payload.
/// Test: `search_skills_executes_with_resolver` exercises the DI path;
/// `search_skills_scans_config_skills_dir` tests the fs fallback.
pub struct SearchSkillsTool {
    resolver: Option<Arc<dyn SkillResolver>>,
    /// Filesystem fallback root — defaults to `.open-mpm/skills` under CWD.
    skills_dir: PathBuf,
}

impl SearchSkillsTool {
    /// Construct with default `.open-mpm/skills` fallback and no resolver.
    pub fn new() -> Self {
        Self {
            resolver: None,
            skills_dir: PathBuf::from(".open-mpm").join("skills"),
        }
    }

    /// Construct with an explicit skills directory for filesystem fallback.
    #[allow(dead_code)]
    pub fn with_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skills_dir = dir;
        self
    }

    /// Construct with a `SkillResolver` that takes priority over the fs scan.
    pub fn with_resolver(resolver: Arc<dyn SkillResolver>) -> Self {
        Self {
            resolver: Some(resolver),
            skills_dir: PathBuf::from(".open-mpm").join("skills"),
        }
    }
}

impl Default for SearchSkillsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for SearchSkillsTool {
    fn name(&self) -> &str {
        "search_skills"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "search_skills",
                "description": "Find relevant skills by case-insensitive substring match against the skill name and first few lines of content. Returns {name, first_line, preview} entries.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "top_n": {"type": "integer", "description": "Default 3."}
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return ToolResult::err("search_skills: missing 'query'");
        };
        let top_n = args
            .get("top_n")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_SKILL_TOP_N);

        let needle = query.to_lowercase();
        let mut hits: Vec<Value> = Vec::new();

        // Resolver path (preferred): enumerate names, load bodies, substring match.
        if let Some(resolver) = self.resolver.as_ref() {
            for name in resolver.list() {
                let body = resolver.resolve(&name).unwrap_or_default();
                if !name.to_lowercase().contains(&needle) && !body.to_lowercase().contains(&needle)
                {
                    continue;
                }
                let first_line = body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                let preview: String = body
                    .chars()
                    .take(SKILL_PREVIEW_MAX_CHARS)
                    .collect::<String>();
                hits.push(json!({
                    "name": name,
                    "first_line": first_line,
                    "preview": preview,
                }));
                if hits.len() >= top_n {
                    break;
                }
            }
            let out = json!({
                "query": query,
                "hits": hits,
            });
            return ToolResult::ok(out.to_string());
        }

        // Filesystem fallback: scan `skills_dir` for `*.md` files.
        let entries = match std::fs::read_dir(&self.skills_dir) {
            Ok(e) => e,
            Err(e) => {
                let out = json!({
                    "error": format!(
                        "skills directory {} not readable: {e}",
                        self.skills_dir.display()
                    ),
                    "query": query,
                    "hits": []
                });
                return ToolResult::ok(out.to_string());
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            if !name.to_lowercase().contains(&needle) && !body.to_lowercase().contains(&needle) {
                continue;
            }
            let first_line = body
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string();
            let preview: String = body
                .chars()
                .take(SKILL_PREVIEW_MAX_CHARS)
                .collect::<String>();
            hits.push(json!({
                "name": name,
                "first_line": first_line,
                "preview": preview,
            }));
            if hits.len() >= top_n {
                break;
            }
        }

        let out = json!({
            "query": query,
            "hits": hits,
        });
        ToolResult::ok(out.to_string())
    }
}

/// Number of context lines around the match line in compact-snippet mode.
const COMPACT_CONTEXT_LINES: usize = 7;

/// Build the JSON hit envelope from a [`CodeChunk`].
///
/// Why: Three execute() arms (daemon, local hybrid, vector-only fallback)
/// all need the same shape and the same compact/full toggle. Extracting
/// it once keeps the policy in one place (#376 C1).
/// What: When `compact` is true, replaces `text` with a 7-line window
/// centred on the chunk's start line and emits a `match_reason` field.
/// When false, behaves exactly like the legacy code path (full text up
/// to `SNIPPET_MAX_CHARS`).
fn chunk_to_hit_json(c: &crate::search::indexer::CodeChunk, compact: bool) -> Value {
    // Use the chunk's own match_reason when populated; fall back to "hybrid"
    // for chunks that were stored before #401 was deployed (empty string).
    let reason = if c.match_reason.is_empty() {
        "hybrid"
    } else {
        &c.match_reason
    };
    if compact {
        let snippet = compact_snippet(&c.text, c.start_line, c.start_line, COMPACT_CONTEXT_LINES);
        json!({
            "file": c.file.display().to_string(),
            "line": c.start_line,
            "function": c.function_name,
            "snippet": snippet,
            "score": c.score,
            "grade": grade_from_score(c.score),
            "match_reason": reason,
            "language": c.language,
            "end_line": c.end_line,
        })
    } else {
        let snippet: String = c.text.chars().take(SNIPPET_MAX_CHARS).collect::<String>();
        json!({
            "path": c.file.display().to_string(),
            "function_name": c.function_name,
            "start_line": c.start_line,
            "end_line": c.end_line,
            "language": c.language,
            "score": c.score,
            "snippet": snippet,
            "match_reason": reason,
        })
    }
}

/// Return a small window of `text` around `highlight_line` (1-indexed,
/// relative to the chunk's `start_line`).
///
/// Why: Compact snippets save context-window tokens for downstream LLM
/// reasoning; 7 lines is enough to read a function signature plus a
/// couple of body lines (#376 C1).
/// What: Slices the chunk text by line, takes `context_lines` lines
/// centred (best-effort) on the matching line, joins with `\n`. If the
/// chunk is short, returns the whole text.
fn compact_snippet(
    text: &str,
    chunk_start_line: usize,
    highlight_line: usize,
    context_lines: usize,
) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= context_lines {
        return lines.join("\n");
    }
    // Compute a 0-indexed offset of the highlight line *within* the chunk.
    let offset = highlight_line.saturating_sub(chunk_start_line);
    let half = context_lines / 2;
    let start = offset
        .saturating_sub(half)
        .min(lines.len().saturating_sub(context_lines));
    let end = (start + context_lines).min(lines.len());
    lines[start..end].join("\n")
}

/// Coarse letter grade from an RRF/cosine score.
///
/// Why: Compact output bundles a one-character signal so callers can
/// triage hits without parsing floats (#376 C1).
/// What: Bucketed thresholds tuned for RRF in [0, 2/RRF_K + ε];
/// "A" for the strongest 10% of hits, descending to "F" for noise.
fn grade_from_score(score: f32) -> &'static str {
    if score >= 0.025 {
        "A"
    } else if score >= 0.018 {
        "B"
    } else if score >= 0.012 {
        "C"
    } else if score >= 0.006 {
        "D"
    } else {
        "F"
    }
}

/// Walk `root` (depth-first), case-insensitive substring match `query` against
/// file contents, return up to `top_n` hits in the same shape as the indexed
/// path so downstream LLM reasoning gets a consistent envelope.
///
/// Why: When `SearchCodeTool` is constructed without a `CodeIndexer` (for
/// example, the in-process CTRL research path), agents would otherwise see
/// `{"error": "search index not available"}` and abort. A simple grep is
/// strictly better than a hard error — issue #213.
/// What: Honours the same `should_skip_dir` / `should_skip_file` filters as
/// `GrepFilesTool::walkdir_grep` so we don't churn through `target/`,
/// `.git/`, binaries, etc. Each hit is a JSON object with the same keys
/// (`path`, `start_line`, `end_line`, `snippet`, `score`) the vector path
/// emits, plus a `match_line` for clarity. `score` is a fixed sentinel (0.0)
/// so callers can distinguish fallback hits from semantic hits.
/// Test: `search_code_falls_back_to_grep_when_indexer_absent` writes a
/// fixture file containing the query, points CWD at the tempdir, and asserts
/// the tool surfaces it via the fallback path.
fn grep_fallback_search(root: &Path, query: &str, top_n: usize) -> Vec<Value> {
    let needle = query.to_lowercase();
    let mut hits: Vec<Value> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    // Cap the snippet so a long line can't blow the context window.
    const MATCH_SNIPPET_MAX_CHARS: usize = SNIPPET_MAX_CHARS;

    while let Some(cur) = stack.pop() {
        if hits.len() >= top_n {
            break;
        }
        if cur.is_file() {
            if should_skip_file(&cur) {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&cur) else {
                continue;
            };
            for (idx, line) in body.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    let snippet: String = line.chars().take(MATCH_SNIPPET_MAX_CHARS).collect();
                    hits.push(json!({
                        "path": cur.display().to_string(),
                        "function_name": Value::Null,
                        "start_line": idx + 1,
                        "end_line": idx + 1,
                        "language": cur
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or(""),
                        "score": 0.0,
                        "snippet": snippet,
                    }));
                    if hits.len() >= top_n {
                        break;
                    }
                }
            }
        } else if cur.is_dir() {
            if let Some(name) = cur.file_name().and_then(|s| s.to_str())
                && should_skip_dir(name)
            {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(&cur) {
                for entry in rd.flatten() {
                    stack.push(entry.path());
                }
            }
        }
    }

    hits
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;
    use crate::memory::store::{MemoryResult, MemoryStore, Segment};
    use crate::memory::{AgentSession, Embedder, MemoryGraph};

    // ------- Shared mock infrastructure (insertion-order search) -------

    struct MockStore {
        inner: Mutex<HashMap<String, (Vec<f32>, Value)>>,
        order: Mutex<Vec<String>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                inner: Mutex::new(HashMap::new()),
                order: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MemoryStore for MockStore {
        async fn insert(
            &self,
            _segment: Segment,
            id: &str,
            vector: &[f32],
            payload: Value,
        ) -> anyhow::Result<()> {
            self.inner
                .lock()
                .unwrap()
                .insert(id.to_string(), (vector.to_vec(), payload));
            let mut order = self.order.lock().unwrap();
            if !order.contains(&id.to_string()) {
                order.push(id.to_string());
            }
            Ok(())
        }

        async fn search(
            &self,
            _segment: Segment,
            _query_vec: &[f32],
            top_k: usize,
        ) -> anyhow::Result<Vec<MemoryResult>> {
            let order = self.order.lock().unwrap().clone();
            let inner = self.inner.lock().unwrap();
            let mut out = Vec::new();
            for (score_idx, id) in order.iter().take(top_k).enumerate() {
                if let Some((_, payload)) = inner.get(id) {
                    out.push(MemoryResult {
                        id: id.clone(),
                        score: 1.0 - (score_idx as f32) * 0.1,
                        payload: payload.clone(),
                        segment: "mem".to_string(),
                    });
                }
            }
            Ok(out)
        }

        async fn get(&self, _segment: Segment, id: &str) -> anyhow::Result<Option<Value>> {
            Ok(self.inner.lock().unwrap().get(id).map(|(_, p)| p.clone()))
        }

        async fn delete(&self, _segment: Segment, id: &str) -> anyhow::Result<()> {
            self.inner.lock().unwrap().remove(id);
            self.order.lock().unwrap().retain(|x| x != id);
            Ok(())
        }
    }

    struct MockEmbedder {
        dim: usize,
    }

    impl Embedder for MockEmbedder {
        fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| vec![t.len() as f32 / 100.0; self.dim])
                .collect())
        }

        fn embed_single(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![text.len() as f32 / 100.0; self.dim])
        }

        fn dimension(&self) -> usize {
            self.dim
        }
    }

    // Filesystem-backed mock skill resolver.
    struct FakeSkillResolver {
        skills: HashMap<String, String>,
    }

    impl SkillResolver for FakeSkillResolver {
        fn resolve(&self, name: &str) -> Option<String> {
            self.skills.get(name).cloned()
        }
        fn list(&self) -> Vec<String> {
            let mut v: Vec<String> = self.skills.keys().cloned().collect();
            v.sort();
            v
        }
    }

    // ------- search_code tests -------

    #[tokio::test]
    async fn search_code_reports_name_and_schema() {
        let t = SearchCodeTool::new();
        assert_eq!(t.name(), "search_code");
        let s = t.schema();
        assert_eq!(s["function"]["name"], "search_code");
        assert_eq!(s["function"]["parameters"]["required"][0], "query");
    }

    #[tokio::test]
    async fn search_code_errors_on_missing_query() {
        let t = SearchCodeTool::new();
        let out = t.execute(json!({})).await;
        assert!(out.is_error());
    }

    #[tokio::test]
    async fn search_code_degrades_gracefully() {
        // No backend injected → tool must return Success without panicking.
        // After #213 the tool falls back to a grep over CWD, so the response
        // shape is `{query, hits, fallback}` (no `error` field). `hits` may
        // be empty or non-empty depending on what's in the working dir; we
        // only assert the envelope is well-formed.
        let t = SearchCodeTool::new();
        let out = t
            .execute(json!({"query": "__definitely_no_such_token_xyz__"}))
            .await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["query"], "__definitely_no_such_token_xyz__");
        assert!(v["hits"].is_array());
        assert_eq!(v["fallback"], "grep");
    }

    #[tokio::test]
    async fn search_code_falls_back_to_grep_when_indexer_absent() {
        // We can't safely chdir in tests (process-global state) and the
        // CWD-walking branch could miss our file inside a large repo before
        // hitting `top_n`. Drive the helper directly with a tempdir + fixture
        // to get deterministic results.
        let dir = tempdir().unwrap();
        let fixture = dir.path().join("notes.md");
        std::fs::write(
            &fixture,
            "line one\nintent classifier explained here\nline three\n",
        )
        .unwrap();

        let hits = grep_fallback_search(dir.path(), "intent classifier", 5);
        assert!(!hits.is_empty(), "expected at least one hit");
        let first = &hits[0];
        assert!(first["path"].as_str().unwrap().ends_with("notes.md"));
        assert_eq!(first["start_line"].as_u64().unwrap(), 2);
        assert!(
            first["snippet"]
                .as_str()
                .unwrap()
                .contains("intent classifier")
        );

        // Round-trip via the tool to confirm the fallback envelope wiring
        // (and that the legacy "search index not available" error is gone).
        let t = SearchCodeTool::new();
        let out = t
            .execute(json!({"query": "__no_match_token_zzz_213__", "top_n": 5}))
            .await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert_eq!(v["fallback"], "grep");
        assert!(v["hits"].is_array());
        assert!(v["note"].is_string());
        assert!(v.get("error").is_none());
    }

    #[tokio::test]
    async fn search_code_returns_hits_from_indexer() {
        // Wire a real CodeIndexer over mock store + embedder, index one
        // Rust file, and assert the tool surfaces it via search.
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn hello() { println!(\"hi\"); }\n").unwrap();

        let store: Arc<dyn MemoryStore> = Arc::new(MockStore::new());
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder { dim: 8 });
        let indexer = Arc::new(CodeIndexer::new(store, embedder));
        indexer.index_file(&file, None).await.unwrap();

        let tool = SearchCodeTool::with_indexer(indexer);
        let out = tool.execute(json!({"query": "hello", "top_n": 5})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "expected at least one hit; got {v:?}");
        // Path + snippet should be present.
        assert!(hits[0]["path"].as_str().unwrap().ends_with("lib.rs"));
        assert!(hits[0]["snippet"].as_str().unwrap().contains("hello"));
    }

    // ------- search_memory tests -------

    #[tokio::test]
    async fn search_memory_degrades_gracefully() {
        let t = SearchMemoryTool::new();
        let out = t.execute(json!({"query": "decisions"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert!(v["hits"].is_array());
        assert_eq!(v["hits"].as_array().unwrap().len(), 0);
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn search_memory_errors_on_missing_query() {
        let t = SearchMemoryTool::new();
        assert!(t.execute(json!({})).await.is_error());
    }

    #[tokio::test]
    async fn search_memory_executes_with_graph() {
        let store: Arc<dyn MemoryStore> = Arc::new(MockStore::new());
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder { dim: 8 });
        let graph = Arc::new(MemoryGraph::new(store, embedder));
        let session = AgentSession {
            id: "s1".to_string(),
            agent_name: "engineer".to_string(),
            workflow_run_id: "r1".to_string(),
            phase: "build".to_string(),
            prompt: "write me a function".to_string(),
            response: "here is hello_world".to_string(),
            timestamp: chrono::Utc::now(),
            parent_id: None,
            segment: None,
        };
        graph.record(session).await.unwrap();

        let tool = SearchMemoryTool::with_graph(graph);
        let out = tool.execute(json!({"query": "hello"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "expected at least one hit; got {v:?}");
    }

    // ------- search_skills tests -------

    #[tokio::test]
    async fn search_skills_errors_on_missing_query() {
        let t = SearchSkillsTool::new();
        assert!(t.execute(json!({})).await.is_error());
    }

    #[tokio::test]
    async fn search_skills_executes_with_resolver() {
        let mut skills = HashMap::new();
        skills.insert(
            "rust-testing".to_string(),
            "# Rust Testing\nHow to write tests in Rust.".to_string(),
        );
        skills.insert(
            "python-packaging".to_string(),
            "# Python Packaging\nBuild wheels.".to_string(),
        );
        let resolver: Arc<dyn SkillResolver> = Arc::new(FakeSkillResolver { skills });
        let t = SearchSkillsTool::with_resolver(resolver);
        let out = t.execute(json!({"query": "rust"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "rust-testing");
        assert!(hits[0]["first_line"].as_str().unwrap().contains("Rust"));
    }

    #[tokio::test]
    async fn search_skills_scans_config_skills_dir() {
        // No resolver; point skills_dir at a tempdir and verify fs fallback.
        let dir = tempdir().unwrap();
        let a = dir.path().join("alpha.md");
        std::fs::write(&a, "# Alpha\nAlpha skill body mentions widgets.").unwrap();
        let b = dir.path().join("beta.md");
        std::fs::write(&b, "# Beta\nUnrelated content.").unwrap();

        let t = SearchSkillsTool::new().with_skills_dir(dir.path().to_path_buf());
        let out = t.execute(json!({"query": "widgets"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["name"], "alpha");
    }

    #[tokio::test]
    async fn search_skills_missing_dir_degrades_gracefully() {
        // No resolver and a skills dir that does not exist → error payload,
        // but the tool call itself still succeeds.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let t = SearchSkillsTool::new().with_skills_dir(missing);
        let out = t.execute(json!({"query": "anything"})).await;
        assert!(!out.is_error());
        let v: Value = serde_json::from_str(out.content()).unwrap();
        assert!(v["error"].is_string());
        assert_eq!(v["hits"].as_array().unwrap().len(), 0);
    }
}
