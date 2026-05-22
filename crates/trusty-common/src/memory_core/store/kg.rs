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
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

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
        Ok(Self { store })
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
        tokio::task::spawn_blocking(move || store.assert(&triple))
            .await
            .context("assert spawn_blocking join error")??;
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
        let subject = subject.to_string();
        let predicate = predicate.to_string();
        let closed = tokio::task::spawn_blocking(move || store.retract(&subject, &predicate))
            .await
            .context("retract spawn_blocking join error")??;
        Ok(closed)
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
