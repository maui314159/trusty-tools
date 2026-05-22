//! Temporal knowledge graph — public `KnowledgeGraph` API.
//!
//! Why: Some facts are relational and time-bounded ("Alice worked at Acme from
//! 2020 to 2023"). Vector search alone can't represent that; a triple store
//! with `valid_from`/`valid_to` intervals can. As of issue #44 the backing
//! store is redb (pure-Rust, embedded, transactional) — see `kg_redb.rs` for
//! the storage engine. The legacy SQLite implementation is preserved under
//! `#[cfg(feature = "sqlite-kg")]` for issue #45's migration tool; issue #47
//! will remove it.
//! What: `Triple` record + `KnowledgeGraph` handle. Every method delegates to
//! `KgStoreRedb`; async methods run blocking redb work on `tokio::task::
//! spawn_blocking` so the async reactor isn't stalled.
//! Test: Asserting (s,p,o) twice closes the first interval and opens a new
//! one; `query_active` returns only the latest. Tests in this file exercise
//! the public API; storage-engine tests live in `kg_redb.rs`.

use crate::memory_core::palace::Drawer;
use crate::memory_core::store::kg_redb::KgStoreRedb;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use petgraph::algo::dijkstra;
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// In-memory edge payload mirroring a knowledge-graph triple.
///
/// Why: The redb TRIPLES table is optimised for transactional persistence and
/// point/range lookups; it is not a graph. For multi-hop reasoning (issue #48,
/// blocking #7 and #10) we maintain a parallel `petgraph::StableGraph` in
/// memory so neighbour scans and shortest-path queries run without touching
/// disk. `KgEdge` is the per-edge payload that travels with each graph edge —
/// it carries the same temporal / confidence / provenance metadata the
/// underlying `Triple` does so callers can rank or filter edges in-flight.
/// What: A plain data struct with the subset of `Triple` fields that vary per
/// edge (subject and object live on the graph endpoints).
/// Test: Indirect — every `kg_graph_tests.rs` test asserts on `KgEdge` values
/// returned by `KnowledgeGraph::neighbors`.
#[derive(Debug, Clone)]
pub struct KgEdge {
    pub predicate: String,
    pub confidence: f32,
    pub provenance: Option<String>,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
}

/// In-memory adjacency cache backing the public graph API.
///
/// Why: Mutating the graph and its `node_index` lookup must happen atomically;
/// holding them in a single struct lets a single `RwLock` guard cover both.
/// What: `StableGraph` so removing an edge does not invalidate other
/// `NodeIndex` values, plus the `String -> NodeIndex` lookup so callers can
/// resolve an entity name to its node in O(1).
/// Test: Indirect — exercised by every adjacency-related test.
#[derive(Default)]
struct Adjacency {
    graph: StableGraph<String, KgEdge>,
    node_index: HashMap<String, NodeIndex<u32>>,
}

impl Adjacency {
    /// Why: Adding the same entity twice would create duplicate nodes; this
    /// helper returns the existing node when the entity is already mapped.
    /// What: Looks up `entity` in `node_index`; on miss adds a node and
    /// records the new mapping.
    /// Test: Indirect via `hydration_populates_graph` and `assert_adds_edge`.
    fn ensure_node(&mut self, entity: &str) -> NodeIndex<u32> {
        if let Some(idx) = self.node_index.get(entity) {
            return *idx;
        }
        let idx = self.graph.add_node(entity.to_string());
        self.node_index.insert(entity.to_string(), idx);
        idx
    }

    /// Why: Building a `KgEdge` from a `Triple` is needed both during
    /// hydration and on every `assert`; centralise the conversion.
    /// What: Copies the temporal / scoring metadata into a new `KgEdge`.
    /// Test: Indirect via `hydration_populates_graph`.
    fn edge_from_triple(t: &Triple) -> KgEdge {
        KgEdge {
            predicate: t.predicate.clone(),
            confidence: t.confidence,
            provenance: t.provenance.clone(),
            valid_from: t.valid_from,
            valid_to: t.valid_to,
        }
    }

    /// Why: `assert` must keep the graph in sync with the store; doing it
    /// here keeps the lock-management in one place.
    /// What: Removes any prior edge for `(subject, predicate)` between the
    /// existing subject and object nodes, then inserts the new edge using
    /// the provided triple's metadata. Nodes are created if absent.
    /// Test: `assert_adds_edge`, `retract_removes_edge`.
    fn upsert_edge(&mut self, triple: &Triple) {
        let s_idx = self.ensure_node(&triple.subject);
        let o_idx = self.ensure_node(&triple.object);
        // Remove any existing edge with the same predicate between the
        // existing subject and any object (matches the temporal invariant
        // "at most one active edge per (subject, predicate)").
        let to_remove: Vec<_> = self
            .graph
            .edges(s_idx)
            .filter(|e| e.weight().predicate == triple.predicate)
            .map(|e| e.id())
            .collect();
        for eid in to_remove {
            self.graph.remove_edge(eid);
        }
        self.graph
            .add_edge(s_idx, o_idx, Self::edge_from_triple(triple));
    }

    /// Why: `retract` closes the active interval at `(subject, predicate)`;
    /// the in-memory graph should drop the corresponding edge so subsequent
    /// `neighbors` calls do not see stale links. Nodes are intentionally
    /// preserved because StableGraph indices stay stable and the entity may
    /// be referenced by other edges.
    /// What: Removes every edge from the subject's node whose predicate
    /// matches `predicate`. Returns the number of edges dropped.
    /// Test: `retract_removes_edge`.
    fn remove_edges(&mut self, subject: &str, predicate: &str) -> usize {
        let Some(&s_idx) = self.node_index.get(subject) else {
            return 0;
        };
        let to_remove: Vec<_> = self
            .graph
            .edges(s_idx)
            .filter(|e| e.weight().predicate == predicate)
            .map(|e| e.id())
            .collect();
        let n = to_remove.len();
        for eid in to_remove {
            self.graph.remove_edge(eid);
        }
        n
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Confidence in [0.0, 1.0] from the asserter.
    pub confidence: f32,
    /// Free-form provenance string (drawer id, source URL, agent name, ...).
    pub provenance: Option<String>,
}

/// Public KG handle. Internally backed by [`KgStoreRedb`].
///
/// Why: Callers should not see whether storage is SQLite or redb; the type
/// owns that choice and presents the same surface as before.
/// What: Thin wrapper around `KgStoreRedb` that runs blocking redb ops on the
/// tokio blocking pool for async methods.
/// Test: See submodule tests in this file plus engine tests in
/// `kg_redb::tests`.
#[derive(Clone)]
pub struct KnowledgeGraph {
    store: KgStoreRedb,
    /// In-memory adjacency view of the active triples, hydrated on `open`
    /// and kept in sync by `assert` / `retract`. See [`Adjacency`].
    adj: Arc<RwLock<Adjacency>>,
}

/// Why: Callers historically pass `data_dir.join("kg.db")` (SQLite filename).
/// To keep the public API stable while moving to redb storage, derive a
/// redb file path adjacent to the SQLite file (`kg.redb` in the same
/// directory). When the input already ends in `.redb`, use it directly.
/// What: Returns the redb file path that corresponds to the given input.
/// Test: Indirect — `open_creates_schema` opens via the SQLite-style path
/// and reading/writing succeeds against the redb file.
fn redb_path_for(input: &Path) -> std::path::PathBuf {
    match input.extension().and_then(|s| s.to_str()) {
        Some("redb") => input.to_path_buf(),
        _ => input.with_extension("redb"),
    }
}

/// One-shot SQLite → redb migration on palace open.
///
/// Why: Pre-#44 palaces persist all knowledge-graph state in
/// `<data_dir>/kg.db` (SQLite). The redb migration (issue #44) silently
/// creates a fresh `kg.redb` on first open — without this hook, every legacy
/// triple and drawer would be invisible after upgrade. Running automatically
/// on `KnowledgeGraph::open` means users do nothing; renaming `kg.db` to
/// `kg.db.migrated` afterwards guarantees the migration runs exactly once
/// per palace even across restarts.
/// What: When `<data_dir>/kg.db` exists and `<data_dir>/kg.db.migrated` does
/// not, open the legacy file read-only via `KgStoreSqlite`, dump every
/// triple (active + historical) plus every drawer, write them into the redb
/// store in a single transaction (`import_all`), then rename the legacy file
/// to `kg.db.migrated`. Gated behind the `sqlite-kg` feature so non-migration
/// builds drop the rusqlite dependency entirely; when the feature is off
/// this function is a no-op.
/// Test: `crates/trusty-common/tests/kg_migration_tests.rs` builds a real
/// legacy `kg.db` with rusqlite, opens `KnowledgeGraph`, and asserts the
/// active triples + drawers survive and the file is renamed.
#[cfg(feature = "sqlite-kg")]
fn migrate_from_sqlite_if_needed(data_dir: &Path, redb_store: &KgStoreRedb) -> Result<()> {
    use crate::memory_core::store::kg_sqlite::KnowledgeGraphSqlite;

    let legacy = data_dir.join("kg.db");
    let migrated_marker = data_dir.join("kg.db.migrated");

    if !legacy.exists() {
        return Ok(());
    }
    if migrated_marker.exists() {
        // Migration already done — defensive: if both somehow exist, prefer
        // the marker and leave the legacy file alone.
        return Ok(());
    }

    let sqlite = KnowledgeGraphSqlite::open_readonly(&legacy)
        .with_context(|| format!("open legacy sqlite kg at {}", legacy.display()))?;
    let triples = sqlite
        .dump_all_triples()
        .context("dump triples from legacy sqlite kg")?;
    let drawers = sqlite
        .load_drawers()
        .context("load drawers from legacy sqlite kg")?;

    let n_triples = triples.len();
    let n_drawers = drawers.len();
    redb_store
        .import_all(triples, drawers)
        .context("import legacy sqlite data into redb")?;

    // Drop the SQLite handle before renaming so no open file handles linger.
    drop(sqlite);

    std::fs::rename(&legacy, &migrated_marker).with_context(|| {
        format!(
            "rename {} to {}",
            legacy.display(),
            migrated_marker.display()
        )
    })?;

    tracing::info!(
        "Migrated {} triples and {} drawers from SQLite to redb at {}",
        n_triples,
        n_drawers,
        data_dir.display()
    );
    Ok(())
}

/// No-op stub used when the `sqlite-kg` feature is disabled.
///
/// Why: Issue #45's migration only compiles with rusqlite available. Keeping
/// the call site in `open()` unconditional avoids `#[cfg]` noise there; this
/// stub satisfies the type signature when the feature is off.
/// What: Immediately returns `Ok(())`.
/// Test: Compiles in default builds (no feature flag) — verified by
/// `cargo test -p trusty-common --features memory-core`.
#[cfg(not(feature = "sqlite-kg"))]
fn migrate_from_sqlite_if_needed(_data_dir: &Path, _redb_store: &KgStoreRedb) -> Result<()> {
    Ok(())
}

/// Build the in-memory adjacency cache from every active triple in the store.
///
/// Why: On `open` the in-memory graph must reflect every triple already in
/// redb so the first `neighbors` / `shortest_path` query is correct without
/// any prior I/O. For typical palaces (≤10K triples) this completes in well
/// under 50ms — `list_active` is a single redb table scan with no random
/// disk seeks.
/// What: Pulls every active triple via `KgStoreRedb::list_active` and
/// inserts each as an edge in a fresh `Adjacency`.
/// Test: `hydration_populates_graph` (and indirectly every neighbors test
/// after reopening a palace).
fn hydrate_adjacency(store: &KgStoreRedb) -> Result<Adjacency> {
    let mut adj = Adjacency::default();
    let triples = store
        .list_active(usize::MAX, 0)
        .context("list active triples for adjacency hydration")?;
    for t in &triples {
        adj.upsert_edge(t);
    }
    Ok(adj)
}

impl KnowledgeGraph {
    /// Open or create the redb-backed KG at the path derived from `path`.
    ///
    /// Why: Callers continue to pass the legacy `<data_dir>/kg.db` path. We
    /// translate that to `<data_dir>/kg.redb` and open the redb file there.
    /// Test: `open_creates_schema`.
    pub fn open(path: &Path) -> Result<Self> {
        let redb_path = redb_path_for(path);
        let store = KgStoreRedb::open(&redb_path)
            .with_context(|| format!("open KG redb at {}", redb_path.display()))?;
        if let Some(data_dir) = redb_path.parent() {
            migrate_from_sqlite_if_needed(data_dir, &store)
                .with_context(|| format!("migrate legacy SQLite KG at {}", data_dir.display()))?;
        }
        let adj = hydrate_adjacency(&store)
            .with_context(|| format!("hydrate KG adjacency from {}", redb_path.display()))?;
        Ok(Self {
            store,
            adj: Arc::new(RwLock::new(adj)),
        })
    }

    /// Assert a fact, closing any prior active interval for the same
    /// (subject, predicate). See [`KgStoreRedb::assert`] for semantics.
    ///
    /// Why: Temporal model — new assertion supersedes the prior active row
    /// instead of overwriting it, preserving history.
    /// What: Delegates to `KgStoreRedb::assert` on the blocking pool.
    /// Test: `assert_then_query_active_returns_fact`,
    /// `second_assert_closes_prior_interval`.
    pub async fn assert(&self, triple: Triple) -> Result<()> {
        let store = self.store.clone();
        let triple_for_store = triple.clone();
        tokio::task::spawn_blocking(move || store.assert(&triple_for_store))
            .await
            .context("assert spawn_blocking join error")??;
        // Sync the in-memory adjacency only after redb commit succeeds so a
        // failed write does not leave the cache ahead of the store.
        {
            let mut adj = self
                .adj
                .write()
                .map_err(|_| anyhow::anyhow!("kg adjacency lock poisoned"))?;
            // Closed-on-arrival triples (assert with valid_to=Some) should
            // not contribute an active edge — drop any existing edge for
            // (subject, predicate) and return.
            if triple.valid_to.is_some() {
                adj.remove_edges(&triple.subject, &triple.predicate);
            } else {
                adj.upsert_edge(&triple);
            }
        }
        Ok(())
    }

    /// Close the active triple for (subject, predicate) without replacement.
    /// Returns the number of rows closed (0 or 1).
    ///
    /// Why: `assert` always closes-and-replaces; retract supports the
    /// prompt-facts surface (`remove_prompt_fact`) where there is no
    /// successor.
    /// What: Delegates to `KgStoreRedb::retract` on the blocking pool.
    /// Test: `retract_closes_active_interval`.
    pub async fn retract(&self, subject: &str, predicate: &str) -> Result<usize> {
        let store = self.store.clone();
        let subject_owned = subject.to_string();
        let predicate_owned = predicate.to_string();
        let s_for_store = subject_owned.clone();
        let p_for_store = predicate_owned.clone();
        let closed = tokio::task::spawn_blocking(move || store.retract(&s_for_store, &p_for_store))
            .await
            .context("retract spawn_blocking join error")??;
        if closed > 0 {
            let mut adj = self
                .adj
                .write()
                .map_err(|_| anyhow::anyhow!("kg adjacency lock poisoned"))?;
            adj.remove_edges(&subject_owned, &predicate_owned);
        }
        Ok(closed)
    }

    /// Return every entity directly connected to `entity` plus the edge
    /// payload that links them.
    ///
    /// Why: Fast single-hop traversal without redb I/O. Used by graph-aware
    /// retrieval and reasoning paths (issues #7, #10) that need to expand
    /// a seed set of entities by one hop without paying for a disk scan.
    /// What: Acquires a read lock on the in-memory adjacency, collects
    /// every outgoing *and* incoming edge incident to `entity`'s node, and
    /// returns `(other_entity, edge)` pairs. Returns an empty vec when the
    /// entity is unknown.
    /// Test: `neighbors_returns_connected`.
    pub fn neighbors(&self, entity: &str) -> Result<Vec<(String, KgEdge)>> {
        let adj = self
            .adj
            .read()
            .map_err(|_| anyhow::anyhow!("kg adjacency lock poisoned"))?;
        let Some(&idx) = adj.node_index.get(entity) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        // Outgoing edges (entity -> other).
        for e in adj.graph.edges(idx) {
            let other = adj
                .graph
                .node_weight(e.target())
                .cloned()
                .unwrap_or_default();
            out.push((other, e.weight().clone()));
        }
        // Incoming edges (other -> entity).
        for e in adj.graph.edges_directed(idx, petgraph::Direction::Incoming) {
            let other = adj
                .graph
                .node_weight(e.source())
                .cloned()
                .unwrap_or_default();
            out.push((other, e.weight().clone()));
        }
        Ok(out)
    }

    /// Return the shortest path of entity names from `from` to `to`, if any.
    ///
    /// Why: Multi-hop reasoning needs a "is there a route, and what is it?"
    /// primitive for paths like "alice -knows-> bob -manages-> carol".
    /// Computing this from the live in-memory graph avoids the per-hop
    /// query latency of repeated redb scans.
    /// What: Runs `petgraph::algo::dijkstra` with unit edge weights on the
    /// outgoing-edge graph (edges follow subject→object direction). When a
    /// finite distance to `to` exists, reconstructs the path by greedy
    /// predecessor walk: at each step pick a neighbour whose distance is
    /// exactly one less than the current node. Returns `None` when either
    /// endpoint is unknown or no path exists.
    /// Test: `shortest_path_finds_route`.
    pub fn shortest_path(&self, from: &str, to: &str) -> Result<Option<Vec<String>>> {
        let adj = self
            .adj
            .read()
            .map_err(|_| anyhow::anyhow!("kg adjacency lock poisoned"))?;
        let Some(&from_idx) = adj.node_index.get(from) else {
            return Ok(None);
        };
        let Some(&to_idx) = adj.node_index.get(to) else {
            return Ok(None);
        };
        if from_idx == to_idx {
            return Ok(Some(vec![from.to_string()]));
        }

        let distances = dijkstra(&adj.graph, from_idx, Some(to_idx), |_| 1usize);
        let Some(&total) = distances.get(&to_idx) else {
            return Ok(None);
        };

        // Reconstruct path: walk from `to` back to `from`, at each hop
        // pick any neighbour with distance == current - 1. Use undirected
        // adjacency for reconstruction so we can step backwards along the
        // directed edges found by Dijkstra.
        let mut path_rev = vec![to_idx];
        let mut current = to_idx;
        let mut current_dist = total;
        while current_dist > 0 {
            let mut next: Option<NodeIndex<u32>> = None;
            for e in adj
                .graph
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                let src = e.source();
                if let Some(&d) = distances.get(&src)
                    && d + 1 == current_dist
                {
                    next = Some(src);
                    break;
                }
            }
            let Some(prev) = next else {
                // No predecessor found — graph mutated between dijkstra
                // and reconstruction, or Dijkstra returned a distance for
                // an unreachable node (defensive guard).
                return Ok(None);
            };
            path_rev.push(prev);
            current = prev;
            current_dist -= 1;
        }
        path_rev.reverse();
        let path: Vec<String> = path_rev
            .into_iter()
            .filter_map(|i| adj.graph.node_weight(i).cloned())
            .collect();
        Ok(Some(path))
    }

    /// Return all currently active triples (`valid_to is None`) for `subject`.
    ///
    /// Why: Most queries want "what is true *now*".
    /// What: Delegates to `KgStoreRedb::query_active` on the blocking pool.
    /// Test: `assert_then_query_active_returns_fact`.
    pub async fn query_active(&self, subject: &str) -> Result<Vec<Triple>> {
        let store = self.store.clone();
        let subject = subject.to_string();
        let triples = tokio::task::spawn_blocking(move || store.query_active(&subject))
            .await
            .context("query_active spawn_blocking join error")??;
        Ok(triples)
    }

    /// List up to `limit` distinct subjects with at least one active triple.
    ///
    /// Why: KG Explorer UI browses subjects without knowing one upfront.
    /// What: Delegates to `KgStoreRedb::list_subjects` synchronously.
    /// Test: `list_subjects_returns_distinct_active_subjects`.
    pub fn list_subjects(&self, limit: usize) -> Result<Vec<String>> {
        self.store.list_subjects(limit)
    }

    /// List up to `limit` `(subject, active_count)` rows.
    ///
    /// Why: KG Explorer UI shows a triple-count badge next to each subject.
    /// What: Delegates to `KgStoreRedb::list_subjects_with_counts`.
    /// Test: `list_subjects_with_counts_returns_grouped_counts`.
    pub fn list_subjects_with_counts(&self, limit: usize) -> Result<Vec<(String, u64)>> {
        self.store.list_subjects_with_counts(limit)
    }

    /// List up to `limit` active triples ordered by `valid_from` desc.
    ///
    /// Why: KG Explorer "All" mode pages through every active triple.
    /// What: Delegates to `KgStoreRedb::list_active` on the blocking pool.
    /// Test: `list_active_returns_ordered_window`.
    pub async fn list_active(&self, limit: usize, offset: usize) -> Result<Vec<Triple>> {
        let store = self.store.clone();
        let triples = tokio::task::spawn_blocking(move || store.list_active(limit, offset))
            .await
            .context("list_active spawn_blocking join error")??;
        Ok(triples)
    }

    /// Count currently active triples.
    ///
    /// Why: Dashboard tally of live facts. Returns 0 on internal error so it
    /// stays diagnostic-grade (matches prior behavior).
    /// What: Delegates to `KgStoreRedb::count_active_triples` and clamps the
    /// u64 to `usize` for backward compatibility with existing callers.
    /// Test: `count_active_triples_returns_live_only`.
    pub fn count_active_triples(&self) -> usize {
        let n = self.store.count_active_triples();
        usize::try_from(n).unwrap_or(usize::MAX)
    }

    /// Compatibility shim for the old WAL checkpoint API.
    ///
    /// Why: The Dreamer cycle called this to bound SQLite's WAL. redb manages
    /// its own write log internally, so there is nothing to do; we return
    /// `(0, 0)` to preserve the tuple shape callers expect.
    /// What: Delegates to `KgStoreRedb::checkpoint` (a no-op) and returns the
    /// (wal_pages, checkpointed_pages) tuple as `(0, 0)`.
    /// Test: `wal_checkpoint_returns_pages`.
    pub fn checkpoint(&self) -> Result<(i64, i64)> {
        self.store.checkpoint()?;
        Ok((0, 0))
    }

    /// Persist a drawer's metadata. See [`KgStoreRedb::upsert_drawer`].
    ///
    /// Why: HNSW only stores vectors; without the metadata persisted alongside
    /// drawers cannot be reconstructed after restart.
    /// What: Delegates to `KgStoreRedb::upsert_drawer`.
    /// Test: `upsert_drawer_then_load_drawers_round_trips`.
    pub fn upsert_drawer(&self, drawer: &Drawer) -> Result<()> {
        self.store.upsert_drawer(drawer)
    }

    /// Remove a drawer's metadata by ID.
    ///
    /// Why: Forgetting must clear both the vector index and the persistent
    /// metadata row.
    /// What: Delegates to `KgStoreRedb::delete_drawer`.
    /// Test: `delete_drawer_removes_row`.
    pub fn delete_drawer(&self, id: Uuid) -> Result<()> {
        self.store.delete_drawer(id)
    }

    /// Load the set of drawer IDs currently stored.
    ///
    /// Why: Compaction only needs "is this UUID a live drawer?".
    /// What: Delegates to `KgStoreRedb::load_drawer_ids`.
    /// Test: `load_drawer_ids_matches_load_drawers`.
    pub fn load_drawer_ids(&self) -> Result<std::collections::HashSet<Uuid>> {
        self.store.load_drawer_ids()
    }

    /// Load all drawer metadata.
    ///
    /// Why: Cold-start retrieval needs the full drawer table to map every
    /// HNSW vector hit back to metadata.
    /// What: Delegates to `KgStoreRedb::load_drawers`.
    /// Test: `upsert_drawer_then_load_drawers_round_trips`.
    pub fn load_drawers(&self) -> Result<Vec<Drawer>> {
        self.store.load_drawers()
    }

    /// Dump every triple including closed history rows.
    ///
    /// Why: Issue #45's SQLite → redb migration walks the entire SQLite table.
    /// This complementary helper exposes the redb side for downstream
    /// consistency checks.
    /// What: Delegates to `KgStoreRedb::dump_all_triples`.
    /// Test: Covered indirectly by `kg_redb::tests::assert_supersedes_prior`.
    pub fn dump_all_triples(&self) -> Result<Vec<Triple>> {
        self.store.dump_all_triples()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[tokio::test]
    async fn open_creates_schema() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let result = kg.query_active("nonexistent").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn assert_then_query_active_returns_fact() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let triple = Triple {
            subject: "alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Acme Corp".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(triple).await.unwrap();
        let active = kg.query_active("alice").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].object, "Acme Corp");
    }

    /// Why: `retract` is the prompt-facts surface's way to remove an alias
    /// without inserting a replacement. The active interval must be closed
    /// (`valid_to` set, `query_active` empty afterwards) and the returned
    /// count must reflect rows touched (1 on success, 0 when there was no
    /// active row).
    #[tokio::test]
    async fn retract_closes_active_interval() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let t = Triple {
            subject: "tga".to_string(),
            predicate: "is_alias_for".to_string(),
            object: "trusty-git-analytics".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(t).await.unwrap();
        assert_eq!(kg.query_active("tga").await.unwrap().len(), 1);

        let closed = kg.retract("tga", "is_alias_for").await.unwrap();
        assert_eq!(closed, 1, "should close exactly one active row");
        assert!(
            kg.query_active("tga").await.unwrap().is_empty(),
            "retract must drop the active triple"
        );

        // Second retract is a no-op (no active row).
        let again = kg.retract("tga", "is_alias_for").await.unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn second_assert_closes_prior_interval() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let t1 = Triple {
            subject: "alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Acme Corp".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(t1).await.unwrap();

        let t2 = Triple {
            subject: "alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Beta Inc".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(t2).await.unwrap();

        let active = kg.query_active("alice").await.unwrap();
        assert_eq!(active.len(), 1, "should have exactly 1 active triple");
        assert_eq!(active[0].object, "Beta Inc");
    }

    #[test]
    fn upsert_drawer_then_load_drawers_round_trips() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let room_id = Uuid::new_v4();
        let mut d = Drawer::new(room_id, "the cold-start drawer");
        d.importance = 0.83;
        d.tags = vec!["alpha".into(), "beta".into()];
        d.source_file = Some(PathBuf::from("/tmp/source.md"));
        kg.upsert_drawer(&d).unwrap();

        let loaded = kg.load_drawers().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, d.id);
        assert_eq!(loaded[0].room_id, room_id);
        assert_eq!(loaded[0].content, "the cold-start drawer");
        assert!((loaded[0].importance - 0.83).abs() < 1e-5);
        assert_eq!(loaded[0].tags, vec!["alpha".to_string(), "beta".into()]);
        assert_eq!(loaded[0].source_file, Some(PathBuf::from("/tmp/source.md")));
    }

    /// Why: Issue #49 — compaction needs a cheap "is this UUID a live drawer?"
    /// check; `load_drawer_ids` returns the set of all stored IDs without the
    /// overhead of materializing full `Drawer` rows.
    #[test]
    fn load_drawer_ids_matches_load_drawers() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let room = Uuid::new_v4();
        let d1 = Drawer::new(room, "one");
        let d2 = Drawer::new(room, "two");
        kg.upsert_drawer(&d1).unwrap();
        kg.upsert_drawer(&d2).unwrap();

        let ids = kg.load_drawer_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&d1.id));
        assert!(ids.contains(&d2.id));
    }

    #[test]
    fn delete_drawer_removes_row() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let d = Drawer::new(Uuid::new_v4(), "to be deleted");
        kg.upsert_drawer(&d).unwrap();
        kg.delete_drawer(d.id).unwrap();
        let loaded = kg.load_drawers().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn upsert_drawer_replaces_existing_row() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let mut d = Drawer::new(Uuid::new_v4(), "original");
        kg.upsert_drawer(&d).unwrap();
        d.content = "updated".into();
        d.importance = 0.95;
        kg.upsert_drawer(&d).unwrap();
        let loaded = kg.load_drawers().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "updated");
        assert!((loaded[0].importance - 0.95).abs() < 1e-5);
    }

    /// Why: The dashboard's KG triple count must reflect only live facts
    /// (`valid_to IS NULL`); closed intervals are history and must not be
    /// counted.
    #[tokio::test]
    async fn count_active_triples_returns_live_only() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        assert_eq!(kg.count_active_triples(), 0);

        kg.assert(Triple {
            subject: "alice".into(),
            predicate: "works_at".into(),
            object: "Acme".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        assert_eq!(kg.count_active_triples(), 1);

        // Superseding triple closes the prior interval — count stays at 1.
        kg.assert(Triple {
            subject: "alice".into(),
            predicate: "works_at".into(),
            object: "Beta".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        assert_eq!(kg.count_active_triples(), 1);
    }

    /// Why: The Dreamer cycle calls `checkpoint()` to keep the WAL bounded;
    /// the method must return a `(wal_pages, checkpointed_pages)` tuple
    /// without erroring. Under redb this is a no-op returning `(0, 0)`.
    #[tokio::test]
    async fn wal_checkpoint_returns_pages() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        kg.assert(Triple {
            subject: "s".into(),
            predicate: "p".into(),
            object: "o".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        let (wal, done) = kg.checkpoint().expect("checkpoint should succeed");
        assert!(wal >= 0);
        assert!(done >= 0);
    }

    /// Why: KG Explorer UI calls `list_subjects` to populate the left panel.
    #[tokio::test]
    async fn list_subjects_returns_distinct_active_subjects() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        assert!(kg.list_subjects(50).unwrap().is_empty());

        kg.assert(Triple {
            subject: "bob".into(),
            predicate: "knows".into(),
            object: "alice".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        kg.assert(Triple {
            subject: "alice".into(),
            predicate: "knows".into(),
            object: "bob".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        // Second assertion on same (subject, predicate) closes the first —
        // still leaves one active row for "alice", so distinct count stays 2.
        kg.assert(Triple {
            subject: "alice".into(),
            predicate: "knows".into(),
            object: "carol".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();

        let subjects = kg.list_subjects(50).unwrap();
        assert_eq!(subjects, vec!["alice".to_string(), "bob".to_string()]);
    }

    /// Why: KG Explorer UI shows a triple-count badge next to each subject.
    #[tokio::test]
    async fn list_subjects_with_counts_returns_grouped_counts() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        assert!(kg.list_subjects_with_counts(50).unwrap().is_empty());

        for (subj, pred) in [
            ("alice", "knows"),
            ("alice", "likes"),
            ("alice", "owns"),
            ("bob", "knows"),
        ] {
            kg.assert(Triple {
                subject: subj.into(),
                predicate: pred.into(),
                object: "thing".into(),
                valid_from: Utc::now(),
                valid_to: None,
                confidence: 1.0,
                provenance: None,
            })
            .await
            .unwrap();
        }

        let rows = kg.list_subjects_with_counts(50).unwrap();
        assert_eq!(rows, vec![("alice".to_string(), 3), ("bob".to_string(), 1)]);
    }

    /// Why: KG Explorer's "All" mode pages through every active triple in
    /// `valid_from DESC` order.
    #[tokio::test]
    async fn list_active_returns_ordered_window() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();

        for i in 0..3 {
            kg.assert(Triple {
                subject: format!("subj-{i}"),
                predicate: "rel".into(),
                object: format!("obj-{i}"),
                valid_from: Utc::now() + chrono::Duration::milliseconds(i * 10),
                valid_to: None,
                confidence: 1.0,
                provenance: None,
            })
            .await
            .unwrap();
        }

        let all = kg.list_active(10, 0).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].subject, "subj-2");
        assert_eq!(all[2].subject, "subj-0");

        let window = kg.list_active(2, 1).await.unwrap();
        assert_eq!(window.len(), 2);
        assert_eq!(window[0].subject, "subj-1");
        assert_eq!(window[1].subject, "subj-0");
    }
}
