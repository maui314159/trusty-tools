//! redb-backed durable chunk corpus (issue #28).
//!
//! Why: prior to this module the chunk corpus was persisted as a single
//! `chunks.json` file rewritten in full after every committed batch. On a
//! 200k-chunk corpus that JSON blob is ~400 MB; serializing it on every batch
//! commit (a reindex emits one commit per 128 files) caused the
//! memory-explosion documented in `PersistState` and forced a full re-read of
//! the entire file into a `HashMap` on every daemon restart. redb gives us:
//!   * crash-safe, atomic per-batch commits (no half-written file window),
//!   * O(batch) incremental writes instead of O(corpus) full rewrites,
//!   * the option to stream chunks back at startup without holding two copies
//!     (the JSON `Vec<RawChunk>` plus the live `HashMap`) in RAM at once.
//!
//! What: [`CorpusStore`] wraps a `redb::Database` with two tables — one keyed
//! by `chunk_id` holding the serialized [`RawChunk`], one keyed by file path
//! holding the serialized per-file [`RawEntity`] list. Values are serialized
//! with `serde_json` (already a workspace dependency; no new crate, and the
//! human-readable form keeps `redb` dumps debuggable).
//!
//! Test: see the `tests` submodule — `roundtrip` writes chunks + entities and
//! reads them back into a fresh store; `missing_db_is_empty` covers the
//! first-run / post-upgrade fallback; `delete_removes_chunk` covers eviction.

use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::core::chunker::RawChunk;
use crate::core::entity::RawEntity;

/// redb table holding the serialized chunk corpus, keyed by `chunk_id`.
///
/// Why: `chunk_id` (`"{path}:{start}:{end}"`) is the corpus's natural primary
/// key — it is collision-safe and is exactly what the in-memory `HashMap` is
/// keyed by, so a redb row maps 1:1 onto a `HashMap` entry.
/// What: `&str → &[u8]` where the value is `serde_json`-encoded [`RawChunk`].
const CHUNKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("chunks");

/// redb table holding the per-file entity lists, keyed by file path.
///
/// Why: `entities` are needed to rebuild the symbol graph on warm-boot and are
/// derived per file, so the file path is the natural key.
/// What: `&str → &[u8]` where the value is `serde_json`-encoded
/// `Vec<RawEntity>`.
const ENTITIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("entities");

/// Durable, redb-backed store for an index's chunk corpus + entity lists.
///
/// Why: see module docs — replaces the full-rewrite `chunks.json` snapshot
/// with an embedded transactional KV store so per-batch commits are O(batch)
/// and crash-safe.
/// What: owns a `redb::Database`; exposes batched upsert, full enumeration,
/// per-id/per-file deletion, and a count. Every mutating call is its own redb
/// write transaction, so a crash between calls never leaves a torn corpus.
/// Test: covered by the `tests` submodule.
pub struct CorpusStore {
    db: Database,
    /// Filesystem path the `db` was opened at. Retained so the atomic
    /// `--force` reindex swap (issue #28, Phase 4) knows which file to rename
    /// without the caller having to pass the path back in.
    path: std::path::PathBuf,
}

impl CorpusStore {
    /// Open (creating if absent) the redb database at `path`.
    ///
    /// Why: the daemon resolves one `index.redb` per index under its data dir;
    /// opening here is the single entry point so table-creation and the
    /// create-if-missing semantics live in one place.
    /// What: opens the database, then runs a no-op write transaction that
    /// `open_table`s both tables so they exist before any reader runs (redb
    /// requires a table to have been created in a committed write txn before
    /// it can be opened read-only).
    /// Test: `roundtrip` and `missing_db_is_empty` both exercise `open`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent of {}", path.display()))?;
        }
        let db = Database::create(path)
            .with_context(|| format!("open redb corpus at {}", path.display()))?;
        // Materialize both tables in a committed write txn so later read-only
        // transactions can `open_table` them even on a brand-new database.
        {
            let txn = db.begin_write().context("begin corpus init txn")?;
            {
                txn.open_table(CHUNKS_TABLE).context("init chunks table")?;
                txn.open_table(ENTITIES_TABLE)
                    .context("init entities table")?;
            }
            txn.commit().context("commit corpus init txn")?;
        }
        Ok(Self {
            db,
            path: path.to_path_buf(),
        })
    }

    /// Open a fresh (truncated) redb corpus at `path`, discarding any existing
    /// file first.
    ///
    /// Why: the `--force` reindex (issue #28, Phase 4) stages the rebuilt
    /// corpus in `index.redb.tmp`. A stale `.tmp` left behind by a previously
    /// aborted reindex must not contribute pre-existing rows to the new staged
    /// corpus — the staged file must reflect *only* this reindex's output so
    /// the post-reindex atomic rename produces a corpus identical to a clean
    /// rebuild.
    /// What: best-effort removes any file already at `path`, then delegates to
    /// [`Self::open`]. A `NotFound` removal error is ignored (nothing to
    /// clear); any other removal error is surfaced.
    /// Test: `tests::test_force_reindex_atomic_corpus_swap`.
    pub fn open_fresh(path: &Path) -> Result<Self> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("clear stale staging corpus at {}", path.display()))
            }
        }
        Self::open(path)
    }

    /// Filesystem path this store's database was opened at.
    ///
    /// Why: the atomic `--force` reindex swap needs to know the staging file's
    /// path to rename it over the live `index.redb`, and the caller would
    /// otherwise have to thread the path alongside every `Arc<CorpusStore>`.
    /// What: returns the stored `PathBuf`.
    /// Test: `tests::test_force_reindex_atomic_corpus_swap` asserts the path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Upsert a batch of chunks in a single redb write transaction.
    ///
    /// Why: a batch commit (`commit_parsed_batch`) lands up to a few hundred
    /// chunks at once. One transaction per batch keeps the write amplification
    /// proportional to the batch size, not the whole corpus, and makes the
    /// batch atomic — a crash mid-commit rolls the whole batch back.
    /// What: serializes each [`RawChunk`] with `serde_json` and inserts it
    /// under its `id`. Existing ids are overwritten (upsert semantics).
    /// Test: `roundtrip` writes then reads; `delete_removes_chunk` re-upserts.
    pub fn upsert_chunks(&self, chunks: &[RawChunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().context("begin chunk upsert txn")?;
        {
            let mut table = txn.open_table(CHUNKS_TABLE)?;
            for chunk in chunks {
                let bytes = serde_json::to_vec(chunk)
                    .with_context(|| format!("serialize chunk {}", chunk.id))?;
                table
                    .insert(chunk.id.as_str(), bytes.as_slice())
                    .with_context(|| format!("insert chunk {}", chunk.id))?;
            }
        }
        txn.commit().context("commit chunk upsert txn")?;
        Ok(())
    }

    /// Upsert a batch of per-file entity lists in a single write transaction.
    ///
    /// Why: entity lists are committed alongside chunks; sharing the same
    /// one-txn-per-batch discipline keeps both tables consistent on a crash.
    /// What: serializes each `Vec<RawEntity>` and inserts it under its file
    /// path key.
    /// Test: `roundtrip` exercises this alongside `upsert_chunks`.
    pub fn upsert_entities(&self, entities: &[(String, Vec<RawEntity>)]) -> Result<()> {
        if entities.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().context("begin entity upsert txn")?;
        {
            let mut table = txn.open_table(ENTITIES_TABLE)?;
            for (file, ents) in entities {
                let bytes = serde_json::to_vec(ents)
                    .with_context(|| format!("serialize entities for {file}"))?;
                table
                    .insert(file.as_str(), bytes.as_slice())
                    .with_context(|| format!("insert entities for {file}"))?;
            }
        }
        txn.commit().context("commit entity upsert txn")?;
        Ok(())
    }

    /// Delete a set of chunk ids in one write transaction.
    ///
    /// Why: `remove_file` / `remove_chunk` must evict from the durable store
    /// too, or a restart would resurrect deleted chunks.
    /// What: removes each id from `CHUNKS_TABLE`; unknown ids are a silent
    /// no-op (idempotent delete), matching the in-memory `HashMap::remove`.
    /// Test: `delete_removes_chunk`.
    pub fn delete_chunks(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().context("begin chunk delete txn")?;
        {
            let mut table = txn.open_table(CHUNKS_TABLE)?;
            for id in ids {
                table
                    .remove(id.as_str())
                    .with_context(|| format!("delete chunk {id}"))?;
            }
        }
        txn.commit().context("commit chunk delete txn")?;
        Ok(())
    }

    /// Delete a per-file entity list. Idempotent.
    ///
    /// Why: `remove_file` drops the file's entities; the durable store must
    /// follow or the symbol graph would rebuild stale symbols on restart.
    /// What: removes the file key from `ENTITIES_TABLE`.
    /// Test: covered indirectly by `delete_removes_chunk` (same txn shape).
    pub fn delete_entities(&self, file: &str) -> Result<()> {
        let txn = self.db.begin_write().context("begin entity delete txn")?;
        {
            let mut table = txn.open_table(ENTITIES_TABLE)?;
            table
                .remove(file)
                .with_context(|| format!("delete entities for {file}"))?;
        }
        txn.commit().context("commit entity delete txn")?;
        Ok(())
    }

    /// Load every chunk in the corpus into a `Vec`.
    ///
    /// Why: the warm-boot path rehydrates the in-memory `HashMap` (and rebuilds
    /// BM25 + the symbol graph) from this. A streaming iterator would avoid the
    /// transient `Vec`, but the caller already needs an owned `RawChunk` per
    /// entry to insert into the map, so the `Vec` is not extra peak RAM beyond
    /// the map itself.
    /// What: opens a read transaction, walks `CHUNKS_TABLE`, and deserializes
    /// each value. A single corrupt row is skipped with a `warn` rather than
    /// failing the whole load — one bad chunk must not brick the daemon.
    /// Test: `roundtrip`.
    pub fn load_all_chunks(&self) -> Result<Vec<RawChunk>> {
        let txn = self.db.begin_read().context("begin chunk read txn")?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter().context("iterate chunks table")? {
            let (key, value) = entry.context("read chunk row")?;
            match serde_json::from_slice::<RawChunk>(value.value()) {
                Ok(chunk) => out.push(chunk),
                Err(e) => {
                    tracing::warn!("corpus: skipping corrupt chunk row '{}' ({e})", key.value())
                }
            }
        }
        Ok(out)
    }

    /// Batch point-read a set of chunks by `chunk_id`.
    ///
    /// Why: issue #28 deferred item — the search hot path used to materialize
    /// top-k results by joining fused `(id, score)` pairs against an in-memory
    /// `HashMap<String, RawChunk>` that held *every* chunk's text resident in
    /// the heap permanently (~45 GB RSS on a large monorepo). Reading the
    /// top-k chunk text straight out of redb at materialization time lets the
    /// daemon drop that HashMap from the query path entirely: redb's values are
    /// mmap-backed, so a point lookup is served from the OS page cache rather
    /// than process heap, cutting steady-state RSS to <10 GB. A typical
    /// `top_k=20` query does 20 point reads inside one read transaction —
    /// each is an O(log n) B-tree descent over an mmap'd file, well within the
    /// sub-10 ms query budget.
    /// What: opens a single redb read transaction and fetches each requested
    /// id. Missing ids are skipped (not an error) — a fused id with no redb row
    /// is almost always a benign race against a concurrent removal, and one
    /// missing chunk must not fail the whole query. A corrupt row is likewise
    /// skipped with a `warn`. The returned `Vec` preserves the input `ids`
    /// order for the ids that were found.
    /// Test: `get_chunks_batch_reads_subset` round-trips a corpus and asserts
    /// only the requested ids come back, in order, with missing ids skipped.
    pub fn get_chunks(&self, ids: &[&str]) -> Result<Vec<RawChunk>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read().context("begin chunk point-read txn")?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(value) = table
                .get(*id)
                .with_context(|| format!("point-read chunk {id}"))?
            else {
                tracing::warn!("corpus: chunk '{id}' not found in redb — skipping");
                continue;
            };
            match serde_json::from_slice::<RawChunk>(value.value()) {
                Ok(chunk) => out.push(chunk),
                Err(e) => {
                    tracing::warn!("corpus: skipping corrupt chunk row '{id}' ({e})")
                }
            }
        }
        Ok(out)
    }

    /// Load every per-file entity list.
    ///
    /// Why: counterpart of [`Self::load_all_chunks`] for the entities table;
    /// the warm-boot path needs both to rebuild the symbol graph.
    /// What: walks `ENTITIES_TABLE`, deserializing each `Vec<RawEntity>`. A
    /// corrupt row is skipped with a `warn`.
    /// Test: `roundtrip`.
    pub fn load_all_entities(&self) -> Result<Vec<(String, Vec<RawEntity>)>> {
        let txn = self.db.begin_read().context("begin entity read txn")?;
        let table = txn.open_table(ENTITIES_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter().context("iterate entities table")? {
            let (key, value) = entry.context("read entity row")?;
            let file = key.value().to_string();
            match serde_json::from_slice::<Vec<RawEntity>>(value.value()) {
                Ok(ents) => out.push((file, ents)),
                Err(e) => {
                    tracing::warn!("corpus: skipping corrupt entity row '{file}' ({e})")
                }
            }
        }
        Ok(out)
    }

    /// Number of chunks currently stored.
    ///
    /// Why: lets the warm-boot path log a count and lets callers cheaply check
    /// "is the durable corpus empty?" (first-run / post-upgrade case) without
    /// materializing every row.
    /// What: returns `CHUNKS_TABLE.len()`.
    /// Test: `roundtrip` asserts the count after upsert.
    pub fn chunk_count(&self) -> Result<usize> {
        let txn = self.db.begin_read().context("begin count txn")?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        Ok(table.len().context("count chunks")? as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chunker::{ChunkType, RawChunk};

    /// Build a minimal `RawChunk` for tests.
    fn raw(id: &str, content: &str) -> RawChunk {
        RawChunk {
            id: id.to_string(),
            file: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            content: content.to_string(),
            function_name: None,
            language: Some("rust".to_string()),
            chunk_type: ChunkType::Code,
            calls: Vec::new(),
            inherits_from: Vec::new(),
            chunk_depth: 0,
            parent_chunk_id: None,
            child_chunk_ids: Vec::new(),
            nlp_keywords: Vec::new(),
            nlp_code_refs: Vec::new(),
            virtual_terms: Vec::new(),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();

        let chunks = vec![raw("a:1:1", "fn a() {}"), raw("b:1:1", "fn b() {}")];
        store.upsert_chunks(&chunks).unwrap();
        store
            .upsert_entities(&[("src/lib.rs".to_string(), Vec::new())])
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);

        // Reopen to simulate a daemon restart.
        drop(store);
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        let mut loaded = store.load_all_chunks().unwrap();
        loaded.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "a:1:1");
        assert_eq!(loaded[0].content, "fn a() {}");

        let entities = store.load_all_entities().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].0, "src/lib.rs");
    }

    #[test]
    fn get_chunks_batch_reads_subset() {
        // Issue #28 deferred item: the query hot path materializes top-k
        // results via `get_chunks`. It must return only the requested ids, in
        // input order, and silently skip ids absent from the corpus.
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        store
            .upsert_chunks(&[
                raw("a:1:1", "fn a() {}"),
                raw("b:1:1", "fn b() {}"),
                raw("c:1:1", "fn c() {}"),
            ])
            .unwrap();

        // Request a subset out of corpus order, with one unknown id mixed in.
        let got = store
            .get_chunks(&["c:1:1", "missing:0:0", "a:1:1"])
            .unwrap();
        assert_eq!(got.len(), 2, "unknown id must be skipped, not error");
        assert_eq!(got[0].id, "c:1:1", "input order must be preserved");
        assert_eq!(got[0].content, "fn c() {}");
        assert_eq!(got[1].id, "a:1:1");

        // Empty input is a no-op.
        assert!(store.get_chunks(&[]).unwrap().is_empty());

        // All-missing input yields an empty vec, never an error.
        assert!(store.get_chunks(&["nope:0:0"]).unwrap().is_empty());
    }

    #[test]
    fn missing_db_is_empty() {
        // A brand-new database (post-upgrade / first-run) must open cleanly
        // and report an empty corpus rather than erroring.
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("fresh.redb")).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0);
        assert!(store.load_all_chunks().unwrap().is_empty());
        assert!(store.load_all_entities().unwrap().is_empty());
    }

    #[test]
    fn delete_removes_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        store
            .upsert_chunks(&[raw("a:1:1", "x"), raw("b:1:1", "y")])
            .unwrap();
        store.delete_chunks(&["a:1:1".to_string()]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
        let loaded = store.load_all_chunks().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "b:1:1");
        // Deleting an unknown id is a silent no-op.
        store.delete_chunks(&["nope:0:0".to_string()]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
    }

    #[test]
    fn empty_batches_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        store.upsert_chunks(&[]).unwrap();
        store.upsert_entities(&[]).unwrap();
        store.delete_chunks(&[]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0);
    }

    #[test]
    fn delete_entities_removes_file_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        store
            .upsert_entities(&[
                ("src/a.rs".to_string(), Vec::new()),
                ("src/b.rs".to_string(), Vec::new()),
            ])
            .unwrap();
        assert_eq!(store.load_all_entities().unwrap().len(), 2);
        store.delete_entities("src/a.rs").unwrap();
        let remaining = store.load_all_entities().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, "src/b.rs");
        // Deleting an unknown file is a silent no-op.
        store.delete_entities("src/never.rs").unwrap();
        assert_eq!(store.load_all_entities().unwrap().len(), 1);
    }

    #[test]
    fn path_accessor_returns_open_path() {
        // Issue #28 Phase 4: the atomic-swap path reads `path()` to know which
        // file to rename. It must echo back exactly what `open` was given.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.redb");
        let store = CorpusStore::open(&p).unwrap();
        assert_eq!(store.path(), p.as_path());
    }

    #[test]
    fn open_fresh_truncates_stale_staging_file() {
        // Issue #28 Phase 4: a stale `index.redb.tmp` left by an aborted
        // reindex must not contribute pre-existing rows to the next staged
        // corpus — `open_fresh` discards the old file first.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.redb.tmp");

        // Populate, then drop so the file is closed and persisted on disk.
        {
            let store = CorpusStore::open(&p).unwrap();
            store.upsert_chunks(&[raw("stale:1:1", "old")]).unwrap();
            assert_eq!(store.chunk_count().unwrap(), 1);
        }
        assert!(p.exists());

        // `open_fresh` must yield an empty corpus despite the existing file.
        let fresh = CorpusStore::open_fresh(&p).unwrap();
        assert_eq!(fresh.chunk_count().unwrap(), 0);
        assert_eq!(fresh.path(), p.as_path());

        // And `open_fresh` on a path that does not exist is also fine.
        let fresh2 = CorpusStore::open_fresh(&dir.path().join("never.redb.tmp")).unwrap();
        assert_eq!(fresh2.chunk_count().unwrap(), 0);
    }
}
