//! Agent memory graph — persist IPC round-trips as a searchable, traversable graph.
//!
//! Why: Every agent prompt/response is a potentially-useful artifact. Persisting
//! them with embeddings enables semantic recall across sessions; persisting
//! temporal and delegation edges enables replaying or tracing workflows. The
//! graph lives on top of the existing `MemoryStore` + `Embedder` layers so we
//! don't need a separate graph database.
//! What: Defines `AgentSession` (node payload) and `MemoryGraph` (facade) with
//! `record`, `search`, `get_session`, `get_run`, and `get_children`. Edges are
//! encoded as specially-keyed entries in `Segment::AgentMemory` so we can reuse
//! the same redb/usearch backend; zero-vector edge rows are filtered out of
//! semantic search results.
//! Test: See unit tests — round-trip record+search, temporal ordering within a
//! run, parent→children delegation traversal, and edge-row exclusion on search.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::code_store::CodeStore;
use super::embed::Embedder;
use super::session_store::SessionStore;
use super::store::{MemoryResult, MemoryStore, Segment};

/// One agent IPC round-trip captured as a node in the memory graph.
///
/// Why: Bundles everything callers need to reason about a past turn — who ran
/// (agent_name), when (timestamp), what was asked (prompt), what was returned
/// (response), and how this sits in a larger workflow (workflow_run_id, phase,
/// parent_id). Serialized as JSON into the store payload.
/// What: Plain serde struct; `id` is a fresh UUIDv4 assigned by the caller.
/// Test: `record_and_search_round_trip` verifies serialization survives a
/// store insert + retrieval via search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub agent_name: String,
    pub workflow_run_id: String,
    pub phase: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: DateTime<Utc>,
    pub parent_id: Option<String>,
    /// Optional graph-native segment tier this session belongs to.
    ///
    /// Why: With Context/Brief/History segments now first-class, callers can
    /// route a session to a specific tier (e.g., a decision-recording phase
    /// writes to `History`). When unset (the default), `record()` falls back
    /// to `Segment::AgentMemory` so existing call sites remain unchanged.
    /// What: `None` means legacy AgentMemory routing; `Some(seg)` overrides.
    /// Test: Existing tests construct sessions without setting `segment` and
    /// continue to pass — backward compatibility verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment: Option<Segment>,
}

/// Facade that records `AgentSession`s and exposes graph-ish lookups.
///
/// Why: Keeps the store + embedder wiring in one place so callers
/// (`SubprocessAgentRunner`, future CLI inspection tools) can depend on a
/// small, ergonomic interface. The internals decide when to embed, when to
/// write edge rows, and how to reconstruct traversals.
/// What: Holds `Arc<dyn MemoryStore>` + `Arc<dyn Embedder>` so both are
/// trivially cloneable and shareable across tokio tasks.
/// Test: All behaviors covered by the `tests` module using mock store/embedder.
pub struct MemoryGraph {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
}

/// Prefix for edge payload rows. Used for filtering out of search results.
const EDGE_PREFIX: &str = "edge:";
/// Prefix for "last session in a run" tracking rows.
const RUN_LAST_PREFIX: &str = "run:";
/// Prefix for "ordered list of session ids in a run" rows.
const RUN_SESSIONS_PREFIX: &str = "run-sessions:";
/// Prefix for "children ids of a parent session" rows.
const CHILDREN_PREFIX: &str = "children:";

impl MemoryGraph {
    /// Construct a new graph over the given store and embedder.
    pub fn new(store: Arc<dyn MemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    /// Construct a graph wired to the per-session agent-memory store.
    ///
    /// Why: In the multi-session world the caller holds both a `SessionStore`
    /// (for agent memory) and a `CodeStore` (for the shared code index); this
    /// helper makes the intent explicit at the call site. The graph currently
    /// only writes into `Segment::AgentMemory` so the `CodeStore` parameter is
    /// retained for symmetry / future use (e.g., cross-linking sessions to
    /// code chunks) — it's taken now so the signature doesn't have to change
    /// later.
    /// What: Discards `code_store` today and wraps `session_store` as the
    /// backing store. Returns a standard `MemoryGraph`.
    /// Test: Used by `src/subprocess.rs` wiring; exercised indirectly by any
    /// integration that constructs a session-scoped graph.
    pub fn new_session(
        session_store: Arc<SessionStore>,
        _code_store: Arc<CodeStore>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        let store: Arc<dyn MemoryStore> = session_store;
        Self { store, embedder }
    }

    /// Merge semantic-search results across every session under `sessions_dir`.
    ///
    /// Why: When a user asks "have I done this before?" they rarely remember
    /// which run it happened in. This helper opens each session's store,
    /// searches it, filters out edge/auxiliary rows, and returns the global
    /// top-k by similarity.
    /// What: Enumerates child directories of `sessions_dir` (skipping the
    /// `index.redb` file), opens each as a `SessionStore`, calls `search` on
    /// the query embedding with `top_k * 2` (headroom for filtering), then
    /// merges and sorts descending by score and truncates to `top_k`.
    /// Test: `search_all_sessions_merges_across_run_ids`.
    pub async fn search_all_sessions(
        sessions_dir: &Path,
        embedder: &dyn Embedder,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<MemoryResult>> {
        let query_vec = embedder
            .embed_single(query)
            .context("failed to embed cross-session search query")?;
        let fetch_k = top_k.saturating_mul(2).max(top_k);
        let vector_dim = embedder.dimension();

        let mut all: Vec<MemoryResult> = Vec::new();
        if !sessions_dir.exists() {
            return Ok(all);
        }

        for entry in std::fs::read_dir(sessions_dir)
            .with_context(|| format!("reading sessions dir {}", sessions_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let run_id = match path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Each subdirectory is a session; open and search it. Errors on
            // a single session don't abort the cross-session query — we log
            // and skip so one bad dir doesn't blind the user to others.
            let store = match SessionStore::open(sessions_dir, &run_id, vector_dim) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(run_id = %run_id, error = %e, "failed to open session store; skipping");
                    continue;
                }
            };
            let hits = match store
                .search(Segment::AgentMemory, &query_vec, fetch_k)
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(run_id = %run_id, error = %e, "search failed; skipping");
                    continue;
                }
            };
            for h in hits {
                if !Self::is_auxiliary_key(&h.id) {
                    all.push(h);
                }
            }
        }

        // Stable sort by score desc, then truncate.
        all.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all.truncate(top_k);
        Ok(all)
    }

    /// Record a session: insert the node, embed prompt+response, and maintain
    /// temporal / delegation / run-membership edges.
    ///
    /// Why: Persisting metadata alongside the vector makes the node immediately
    /// searchable AND traversable. Edge maintenance (`FOLLOWED_BY`,
    /// `DELEGATED_TO`, run-membership) happens inline so callers don't have to
    /// remember to wire it up — one `record` call captures the full context.
    /// What: Writes the session payload, then writes up to four auxiliary
    /// rows: a `FOLLOWED_BY` edge (if another session preceded it in the same
    /// run), a `DELEGATED_TO` edge (if `parent_id` is set), an append to the
    /// run's session list, and an append to the parent's children list.
    /// Test: Covered by every test in this module — all insert at least once.
    pub async fn record(&self, session: AgentSession) -> Result<()> {
        let payload = serde_json::to_value(&session).context("failed to serialize AgentSession")?;

        let embed_text = format!("{}\n{}", session.prompt, session.response);
        let vec = self
            .embedder
            .embed_single(&embed_text)
            .context("failed to embed session prompt+response")?;

        // Route the node to its declared segment tier; fall back to the legacy
        // AgentMemory bucket so callers that don't opt in stay backward compat.
        // Edge / marker / list rows below stay on AgentMemory regardless — they
        // are graph plumbing, not tier-routed knowledge.
        let node_segment = session.segment.unwrap_or(Segment::AgentMemory);

        self.store
            .insert(node_segment, &session.id, &vec, payload)
            .await
            .context("failed to insert session payload")?;

        // FOLLOWED_BY: link the previous session in this run to this one.
        if !session.workflow_run_id.is_empty() {
            let last_key = format!("{RUN_LAST_PREFIX}{}:last", session.workflow_run_id);
            if let Some(prev) = self
                .store
                .get(Segment::AgentMemory, &last_key)
                .await
                .context("failed to read last-session marker")?
                && let Some(prev_id) = prev.as_str()
            {
                let edge_key = format!("{EDGE_PREFIX}followed_by:{}:{}", prev_id, session.id);
                let zero_vec = self.zero_vec();
                let edge_payload = serde_json::json!({
                    "kind": "followed_by",
                    "from": prev_id,
                    "to": session.id,
                });
                self.store
                    .insert(Segment::AgentMemory, &edge_key, &zero_vec, edge_payload)
                    .await
                    .context("failed to insert followed_by edge")?;
            }

            // Update "last session in run" marker.
            let zero_vec = self.zero_vec();
            let marker = serde_json::Value::String(session.id.clone());
            self.store
                .insert(Segment::AgentMemory, &last_key, &zero_vec, marker)
                .await
                .context("failed to update last-session marker")?;

            // Append to run's session-id list.
            self.append_id_list(
                &format!("{RUN_SESSIONS_PREFIX}{}", session.workflow_run_id),
                &session.id,
            )
            .await
            .context("failed to append session to run list")?;
        }

        // DELEGATED_TO: parent -> this session.
        if let Some(pid) = &session.parent_id {
            let edge_key = format!("{EDGE_PREFIX}delegated_to:{}:{}", pid, session.id);
            let zero_vec = self.zero_vec();
            let edge_payload = serde_json::json!({
                "kind": "delegated_to",
                "from": pid,
                "to": session.id,
            });
            self.store
                .insert(Segment::AgentMemory, &edge_key, &zero_vec, edge_payload)
                .await
                .context("failed to insert delegated_to edge")?;

            self.append_id_list(&format!("{CHILDREN_PREFIX}{pid}"), &session.id)
                .await
                .context("failed to append child to parent children list")?;
        }

        Ok(())
    }

    /// Semantic search over stored sessions.
    ///
    /// Why: Callers want to recall "what have I done like this before?" — a
    /// vector-similarity query over prompt+response answers that.
    /// What: Embeds the query, calls `store.search`, then filters out edge /
    /// marker / list rows (they share the same segment but aren't sessions).
    /// Test: `search_excludes_edge_entries` verifies filtering; other tests
    /// confirm sessions surface correctly.
    pub async fn search(&self, query: &str, top_k: usize) -> Result<Vec<MemoryResult>> {
        let vec = self
            .embedder
            .embed_single(query)
            .context("failed to embed search query")?;
        // Over-fetch a bit so filtering doesn't starve top_k when many edge
        // rows are in the neighborhood. 4x is arbitrary but generous.
        let fetch_k = top_k.saturating_mul(4).max(top_k);
        let raw = self
            .store
            .search(Segment::AgentMemory, &vec, fetch_k)
            .await?;
        let filtered: Vec<MemoryResult> = raw
            .into_iter()
            .filter(|r| !Self::is_auxiliary_key(&r.id))
            .take(top_k)
            .collect();
        Ok(filtered)
    }

    /// Fetch a single session by id.
    ///
    /// Why: Traversal results (children, run lists) give back ids; callers
    /// then want to hydrate each into a full `AgentSession`.
    /// What: Reads the payload and `serde_json::from_value`s it.
    /// Test: `get_run_returns_sessions_in_temporal_order` and
    /// `get_children_returns_delegated_sessions` exercise this path.
    pub async fn get_session(&self, session_id: &str) -> Result<Option<AgentSession>> {
        let raw = self.store.get(Segment::AgentMemory, session_id).await?;
        let Some(v) = raw else {
            return Ok(None);
        };
        let session: AgentSession =
            serde_json::from_value(v).context("failed to deserialize AgentSession payload")?;
        Ok(Some(session))
    }

    /// All sessions in a workflow run, sorted by timestamp ascending.
    ///
    /// Why: Workflow replay / debugging needs the full ordered sequence.
    /// What: Reads the `run-sessions:{run_id}` id-list payload, hydrates each
    /// id, sorts by `timestamp`. Missing ids (e.g. evicted) are silently
    /// skipped rather than producing a partial error.
    /// Test: `get_run_returns_sessions_in_temporal_order`.
    pub async fn get_run(&self, workflow_run_id: &str) -> Result<Vec<AgentSession>> {
        let key = format!("{RUN_SESSIONS_PREFIX}{workflow_run_id}");
        let ids = self.read_id_list(&key).await?;
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(s) = self.get_session(&id).await? {
                sessions.push(s);
            }
        }
        sessions.sort_by_key(|s| s.timestamp);
        Ok(sessions)
    }

    /// All sessions delegated from `session_id` (direct children only).
    ///
    /// Why: Drilling into a PM→sub-agent call tree — e.g. "what did the PM
    /// hand off on this turn?"
    /// What: Reads the `children:{session_id}` id-list and hydrates each entry.
    /// Test: `get_children_returns_delegated_sessions`.
    pub async fn get_children(&self, session_id: &str) -> Result<Vec<AgentSession>> {
        let key = format!("{CHILDREN_PREFIX}{session_id}");
        let ids = self.read_id_list(&key).await?;
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(s) = self.get_session(&id).await? {
                sessions.push(s);
            }
        }
        sessions.sort_by_key(|s| s.timestamp);
        Ok(sessions)
    }

    // --- helpers --------------------------------------------------------

    /// Return true if a store id belongs to an edge/marker/list row rather
    /// than an actual session node.
    fn is_auxiliary_key(id: &str) -> bool {
        id.starts_with(EDGE_PREFIX)
            || id.starts_with(RUN_LAST_PREFIX)
            || id.starts_with(RUN_SESSIONS_PREFIX)
            || id.starts_with(CHILDREN_PREFIX)
    }

    /// Build a zero-filled vector of the embedder's dimension.
    ///
    /// Why: Edge / marker / list rows aren't meant to be searched semantically
    /// but the store requires a vector per row. Zero vectors cluster together
    /// in cosine space, are cheap, and — crucially — are filtered out of
    /// search results by id prefix anyway.
    fn zero_vec(&self) -> Vec<f32> {
        vec![0.0f32; self.embedder.dimension()]
    }

    /// Read a JSON-array-of-strings payload, returning empty vec if absent.
    async fn read_id_list(&self, key: &str) -> Result<Vec<String>> {
        let raw = self.store.get(Segment::AgentMemory, key).await?;
        let Some(v) = raw else {
            return Ok(Vec::new());
        };
        let list: Vec<String> =
            serde_json::from_value(v).context("failed to deserialize id list payload")?;
        Ok(list)
    }

    /// Append `id` to the JSON-array payload at `key`, creating if missing.
    async fn append_id_list(&self, key: &str, id: &str) -> Result<()> {
        let mut list = self.read_id_list(key).await?;
        list.push(id.to_string());
        let payload = serde_json::to_value(&list).context("failed to serialize updated id list")?;
        let zero_vec = self.zero_vec();
        self.store
            .insert(Segment::AgentMemory, key, &zero_vec, payload)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::collections::HashMap;
    use tokio::sync::Mutex as TokioMutex;

    // --- Mock Embedder -------------------------------------------------

    /// Deterministic embedder that returns fixed 384-dim vectors.
    ///
    /// Why: Avoid downloading the real fastembed model in unit tests. The
    /// graph code cares that embedding produces a vector of the right size;
    /// similarity quality is tested elsewhere.
    struct MockEmbedder {
        dim: usize,
    }

    impl Embedder for MockEmbedder {
        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1f32; self.dim]).collect())
        }
        fn embed_single(&self, _text: &str) -> Result<Vec<f32>> {
            Ok(vec![0.1f32; self.dim])
        }
        fn dimension(&self) -> usize {
            self.dim
        }
    }

    // --- Mock Store ----------------------------------------------------

    /// In-memory `MemoryStore` impl keyed by (segment prefix, id).
    ///
    /// Why: Unit tests need a store they can construct synchronously without
    /// touching disk. Search returns all matching-segment entries in insertion
    /// order truncated to `top_k` — good enough because the graph tests don't
    /// assert nearest-neighbor ordering, only that specific ids appear.
    #[derive(Default)]
    struct MockStore {
        inner: TokioMutex<MockInner>,
    }

    #[derive(Default)]
    struct MockInner {
        // Insertion order tracker so "search" is deterministic.
        order: Vec<(String, String)>, // (segment_prefix, id)
        data: HashMap<(String, String), (Vec<f32>, serde_json::Value)>,
    }

    #[async_trait]
    impl MemoryStore for MockStore {
        async fn insert(
            &self,
            segment: Segment,
            id: &str,
            vector: &[f32],
            payload: serde_json::Value,
        ) -> Result<()> {
            let mut g = self.inner.lock().await;
            let key = (segment.prefix().to_string(), id.to_string());
            if !g.data.contains_key(&key) {
                g.order.push(key.clone());
            }
            g.data.insert(key, (vector.to_vec(), payload));
            Ok(())
        }
        async fn search(
            &self,
            segment: Segment,
            _query_vec: &[f32],
            top_k: usize,
        ) -> Result<Vec<MemoryResult>> {
            let g = self.inner.lock().await;
            let prefix = segment.prefix().to_string();
            let mut out = Vec::new();
            for (seg, id) in &g.order {
                if seg != &prefix {
                    continue;
                }
                let (_v, payload) = g.data.get(&(seg.clone(), id.clone())).unwrap();
                out.push(MemoryResult {
                    id: id.clone(),
                    score: 1.0,
                    payload: payload.clone(),
                    segment: seg.clone(),
                });
                if out.len() >= top_k {
                    break;
                }
            }
            Ok(out)
        }
        async fn get(&self, segment: Segment, id: &str) -> Result<Option<serde_json::Value>> {
            let g = self.inner.lock().await;
            Ok(g.data
                .get(&(segment.prefix().to_string(), id.to_string()))
                .map(|(_v, p)| p.clone()))
        }
        async fn delete(&self, segment: Segment, id: &str) -> Result<()> {
            let mut g = self.inner.lock().await;
            let key = (segment.prefix().to_string(), id.to_string());
            g.data.remove(&key);
            g.order.retain(|k| k != &key);
            Ok(())
        }
    }

    fn make_graph() -> MemoryGraph {
        let store: Arc<dyn MemoryStore> = Arc::new(MockStore::default());
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder { dim: 384 });
        MemoryGraph::new(store, embedder)
    }

    fn session(
        id: &str,
        run_id: &str,
        parent: Option<&str>,
        ts_secs: i64,
        prompt: &str,
        response: &str,
    ) -> AgentSession {
        AgentSession {
            id: id.to_string(),
            agent_name: "test-agent".to_string(),
            workflow_run_id: run_id.to_string(),
            phase: "test".to_string(),
            prompt: prompt.to_string(),
            response: response.to_string(),
            timestamp: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            parent_id: parent.map(str::to_string),
            segment: None,
        }
    }

    #[tokio::test]
    async fn record_and_search_round_trip() {
        let graph = make_graph();
        let s = session(
            "sess-1",
            "run-1",
            None,
            1000,
            "write a markdown table",
            "| a | b |\n|---|---|",
        );
        graph.record(s.clone()).await.unwrap();

        let hits = graph.search("write a markdown table", 10).await.unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(
            ids.contains(&"sess-1"),
            "expected sess-1 in search results, got: {ids:?}"
        );
    }

    #[tokio::test]
    async fn get_run_returns_sessions_in_temporal_order() {
        let graph = make_graph();
        // Insert out of order to verify sorting.
        graph
            .record(session("b", "run-x", None, 2000, "q2", "r2"))
            .await
            .unwrap();
        graph
            .record(session("a", "run-x", None, 1000, "q1", "r1"))
            .await
            .unwrap();
        graph
            .record(session("c", "run-x", None, 3000, "q3", "r3"))
            .await
            .unwrap();

        let sessions = graph.get_run("run-x").await.unwrap();
        let ids: Vec<_> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "expected chronological order");
    }

    #[tokio::test]
    async fn get_children_returns_delegated_sessions() {
        let graph = make_graph();
        graph
            .record(session("parent", "run-p", None, 1000, "pq", "pr"))
            .await
            .unwrap();
        graph
            .record(session(
                "child1",
                "run-p",
                Some("parent"),
                1100,
                "c1q",
                "c1r",
            ))
            .await
            .unwrap();
        graph
            .record(session(
                "child2",
                "run-p",
                Some("parent"),
                1200,
                "c2q",
                "c2r",
            ))
            .await
            .unwrap();

        let kids = graph.get_children("parent").await.unwrap();
        let ids: Vec<_> = kids.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"child1"));
        assert!(ids.contains(&"child2"));
    }

    #[tokio::test]
    async fn search_excludes_edge_entries() {
        let graph = make_graph();
        // Recording two sessions in the same run creates a `followed_by` edge
        // plus `run:` marker + `run-sessions:` list rows — all should be
        // filtered out of search results.
        graph
            .record(session(
                "s1",
                "run-e",
                None,
                1000,
                "first prompt",
                "first response",
            ))
            .await
            .unwrap();
        graph
            .record(session(
                "s2",
                "run-e",
                Some("s1"),
                2000,
                "second prompt",
                "second response",
            ))
            .await
            .unwrap();

        let hits = graph.search("first prompt", 50).await.unwrap();
        for h in &hits {
            assert!(
                !MemoryGraph::is_auxiliary_key(&h.id),
                "search returned auxiliary row: {}",
                h.id
            );
        }
        let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(ids.contains(&"s1"));
        assert!(ids.contains(&"s2"));
    }

    #[tokio::test]
    async fn get_session_returns_none_when_missing() {
        let graph = make_graph();
        let got = graph.get_session("nope").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn search_all_sessions_merges_across_run_ids() {
        // Uses real SessionStores backed by tempdir to exercise the actual
        // directory walk. The MockEmbedder avoids downloading fastembed.
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let sessions_dir = dir.path().join("sessions");

        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder { dim: 384 });

        // Session A: record one session.
        {
            let store = SessionStore::new_arc(&sessions_dir, "run-a", 384).unwrap();
            let graph = MemoryGraph::new(store.clone(), embedder.clone());
            graph
                .record(session("sa1", "run-a", None, 1000, "query a", "response a"))
                .await
                .unwrap();
        }
        // Session B: record one session.
        {
            let store = SessionStore::new_arc(&sessions_dir, "run-b", 384).unwrap();
            let graph = MemoryGraph::new(store.clone(), embedder.clone());
            graph
                .record(session("sb1", "run-b", None, 2000, "query b", "response b"))
                .await
                .unwrap();
        }

        let hits =
            MemoryGraph::search_all_sessions(&sessions_dir, embedder.as_ref(), "anything", 10)
                .await
                .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert!(ids.contains(&"sa1"), "expected sa1 in: {ids:?}");
        assert!(ids.contains(&"sb1"), "expected sb1 in: {ids:?}");
        for h in &hits {
            assert!(
                !MemoryGraph::is_auxiliary_key(&h.id),
                "auxiliary id leaked: {}",
                h.id
            );
        }
    }
}
